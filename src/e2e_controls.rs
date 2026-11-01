//! PRD #1666 part 3 (Q105b.2): controls the `e2e` build's tests reach through
//! [`crate::daemon_protocol::AttachRequest::E2eComposerControl`], and the
//! library's own tests reach directly. Compiled only into a test or `e2e`
//! build: a release build has none of them, and nothing here grants a write,
//! an arm or a release. Each one only withholds, fails or reorders what the
//! production path already does.
//!
//! - [`WriterE2e`]: a generation's PTY writer thread. A **hold** parks the
//!   thread before it starts its next job, so accepted bytes stay queued and
//!   undelivered until a go; a **fault plan** makes the PTY accept a chosen
//!   number of bytes and then fail every write, after letting a chosen number
//!   of matching jobs through untouched (Q17-1, the U2-06 race).
//! - [`AttachControl`]: one attach client's connections, by the `client_id`
//!   its `attach-stream` carried. **Degraded** makes the input loop's peer
//!   watch never resolve, as on a platform with no half-close; **end output**
//!   ends the connection's output task as a broken socket would, with no
//!   retirement and no close by the client (Q15-1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// Which writer jobs a fault plan applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultJobs {
    /// Every job that writes.
    Any,
    /// Only the keys of a question's answer (`PtyOp::Answer`).
    Answer,
}

/// A writer fault plan: after `skip` matching jobs are written untouched,
/// the PTY accepts `accept` more bytes of matching jobs in all and then fails
/// each matching write with `EIO`, until the plan is cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultPlan {
    pub jobs: FaultJobs,
    pub skip: usize,
    pub accept: usize,
}

impl FaultPlan {
    /// Parse `jobs=<any|answer>,skip=<K>,accept=<N>`; each key is optional
    /// (`any`, 0 and 0 by default), and an unknown key or value is `None`.
    pub fn parse(spec: &str) -> Option<Self> {
        let mut plan = FaultPlan {
            jobs: FaultJobs::Any,
            skip: 0,
            accept: 0,
        };
        for part in spec.split(',').filter(|p| !p.is_empty()) {
            let (key, value) = part.split_once('=')?;
            match key {
                "jobs" => {
                    plan.jobs = match value {
                        "any" => FaultJobs::Any,
                        "answer" => FaultJobs::Answer,
                        _ => return None,
                    }
                }
                "skip" => plan.skip = value.parse().ok()?,
                "accept" => plan.accept = value.parse().ok()?,
                _ => return None,
            }
        }
        Some(plan)
    }
}

#[derive(Debug, Default)]
struct Faults {
    plan: Option<FaultPlan>,
    /// Matching jobs still to let through untouched.
    skip: usize,
    /// Bytes the PTY still accepts once the skip ran out.
    budget: usize,
    /// The job the thread is writing now matches and the skip ran out.
    applies: bool,
}

/// A generation's writer-thread controls. See the module documentation.
#[derive(Debug, Default)]
pub struct WriterE2e {
    held: Mutex<bool>,
    wake: Condvar,
    /// The thread is parked at the hold with a job dequeued.
    parked: AtomicBool,
    /// Jobs the thread has dequeued, held or not.
    dequeued: AtomicU64,
    faults: Mutex<Faults>,
    /// Writes the fault plan failed.
    failed_writes: AtomicUsize,
}

/// How often a parked thread looks again at its generation's retirement.
const HOLD_POLL: Duration = Duration::from_millis(20);

impl WriterE2e {
    /// Park the writer thread before its next job.
    pub fn hold(&self) {
        *self.held.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// Let it go on.
    pub fn go(&self) {
        *self.held.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.wake.notify_all();
    }

    pub fn is_held(&self) -> bool {
        *self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The thread is parked at the hold with a dequeued job.
    pub fn parked(&self) -> bool {
        self.parked.load(Ordering::SeqCst)
    }

    pub fn dequeued(&self) -> u64 {
        self.dequeued.load(Ordering::SeqCst)
    }

    pub fn failed_writes(&self) -> usize {
        self.failed_writes.load(Ordering::SeqCst)
    }

    /// Install `plan`, or clear it with `None`.
    pub fn set_faults(&self, plan: Option<FaultPlan>) {
        let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        *faults = Faults {
            plan,
            skip: plan.map_or(0, |p| p.skip),
            budget: plan.map_or(0, |p| p.accept),
            applies: false,
        };
    }

    /// The writer thread dequeued a job: count it, and park while held, until
    /// a go or `retired` reads true. Called with no lock of the writer's held.
    pub fn at_dequeue(&self, retired: &AtomicBool) {
        self.dequeued.fetch_add(1, Ordering::SeqCst);
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while *held && !retired.load(Ordering::SeqCst) {
            self.parked.store(true, Ordering::SeqCst);
            held = self
                .wake
                .wait_timeout(held, HOLD_POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        self.parked.store(false, Ordering::SeqCst);
    }

    /// The thread starts writing a job; `answer` says whether it is an
    /// answer's keys. Decides whether the fault plan applies to its writes.
    pub fn begin_job(&self, answer: bool) {
        let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        let matches = faults
            .plan
            .is_some_and(|p| p.jobs == FaultJobs::Any || answer);
        faults.applies = false;
        if !matches {
            return;
        }
        if faults.skip > 0 {
            faults.skip -= 1;
        } else {
            faults.applies = true;
        }
    }

    /// One `write(2)` of `len` bytes: `Ok(n)` lets the PTY take `n` of them
    /// (`len` when no fault applies), `Err` fails it with nothing taken.
    fn admit(&self, len: usize) -> std::io::Result<usize> {
        let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        if !faults.applies || len == 0 {
            return Ok(len);
        }
        if faults.budget == 0 {
            drop(faults);
            self.failed_writes.fetch_add(1, Ordering::SeqCst);
            return Err(std::io::Error::from_raw_os_error(EIO));
        }
        let n = len.min(faults.budget);
        faults.budget -= n;
        Ok(n)
    }
}

/// `EIO`, what a PTY whose other end went away fails a write with (5 on
/// Linux and macOS).
const EIO: i32 = 5;

/// The PTY writer as the thread sees it in a test or `e2e` build: `inner`
/// with the generation's fault plan in front of it.
pub struct FaultableWriter {
    pub inner: Box<dyn std::io::Write + Send>,
    pub controls: Arc<WriterE2e>,
}

impl std::io::Write for FaultableWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.controls.admit(buf.len())?;
        self.inner.write(&buf[..n])
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// One attach client's controls (Q15-1). See the module documentation.
#[derive(Debug)]
pub struct AttachControl {
    degraded: AtomicBool,
    end_output: tokio::sync::watch::Sender<bool>,
}

impl Default for AttachControl {
    fn default() -> Self {
        Self {
            degraded: AtomicBool::new(false),
            end_output: tokio::sync::watch::channel(false).0,
        }
    }
}

impl AttachControl {
    /// The client's connections watch their peer no more: a frame that
    /// starts waiting for room after this is stopped only by its output
    /// task's end.
    pub fn degrade_peer_watch(&self) {
        self.degraded.store(true, Ordering::SeqCst);
    }

    pub fn peer_watch_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    /// End the output task of every connection of this client, now and
    /// later.
    pub fn end_output(&self) {
        self.end_output.send_replace(true);
    }

    /// Resolves once [`Self::end_output`] was called.
    pub async fn output_ended(&self) {
        let mut rx = self.end_output.subscribe();
        let _ = rx.wait_for(|ended| *ended).await;
    }
}

/// The attach controls by client id, for this process.
pub fn attach_control(client_id: &str) -> Arc<AttachControl> {
    static CONTROLS: OnceLock<Mutex<HashMap<String, Arc<AttachControl>>>> = OnceLock::new();
    let mut controls = CONTROLS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Arc::clone(controls.entry(client_id.to_string()).or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_fault_plan_parses_and_refuses_unknown_keys() {
        assert_eq!(
            FaultPlan::parse("jobs=answer,skip=1,accept=3"),
            Some(FaultPlan {
                jobs: FaultJobs::Answer,
                skip: 1,
                accept: 3
            })
        );
        assert_eq!(
            FaultPlan::parse(""),
            Some(FaultPlan {
                jobs: FaultJobs::Any,
                skip: 0,
                accept: 0
            })
        );
        assert_eq!(FaultPlan::parse("jobs=probe"), None);
        assert_eq!(FaultPlan::parse("bytes=1"), None);
    }

    #[test]
    fn the_plan_skips_matching_jobs_then_takes_its_budget_then_fails() {
        let taken = Arc::new(Mutex::new(Vec::new()));
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let controls = Arc::new(WriterE2e::default());
        let mut w = FaultableWriter {
            inner: Box::new(Sink(Arc::clone(&taken))),
            controls: Arc::clone(&controls),
        };
        controls.set_faults(FaultPlan::parse("jobs=answer,skip=1,accept=2"));
        controls.begin_job(false);
        assert_eq!(w.write(b"user").unwrap(), 4, "not an answer");
        controls.begin_job(true);
        assert_eq!(w.write(b"a").unwrap(), 1, "the skipped first key");
        controls.begin_job(true);
        assert_eq!(w.write(b"bcd").unwrap(), 2, "the budget's prefix");
        assert!(w.write(b"d").is_err());
        controls.begin_job(true);
        assert!(w.write(b"e").is_err(), "sticky until cleared");
        assert_eq!(controls.failed_writes(), 2);
        controls.set_faults(None);
        controls.begin_job(true);
        assert_eq!(w.write(b"f").unwrap(), 1);
        assert_eq!(&*taken.lock().unwrap(), b"userabcf");
    }

    #[test]
    fn a_hold_parks_until_go_or_retirement() {
        let controls = Arc::new(WriterE2e::default());
        let retired = Arc::new(AtomicBool::new(false));
        controls.hold();
        let (c, r) = (Arc::clone(&controls), Arc::clone(&retired));
        let thread = std::thread::spawn(move || c.at_dequeue(&r));
        while !controls.parked() {
            std::thread::yield_now();
        }
        controls.go();
        thread.join().unwrap();
        assert!(!controls.parked());
        controls.hold();
        let (c, r) = (Arc::clone(&controls), Arc::clone(&retired));
        let thread = std::thread::spawn(move || c.at_dequeue(&r));
        while !controls.parked() {
            std::thread::yield_now();
        }
        retired.store(true, Ordering::SeqCst);
        thread.join().unwrap();
        assert_eq!(controls.dequeued(), 2);
    }
}
