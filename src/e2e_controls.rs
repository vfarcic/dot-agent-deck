//! PRD #1666 part 3 (Q105b.2): controls the `e2e` build's tests reach through
//! [`crate::daemon_protocol::AttachRequest::E2eComposerControl`], and the
//! library's own tests reach directly. Compiled only into a test or `e2e`
//! build: a release build has none of them. None of the controls here writes,
//! arms or releases anything itself; each withholds, fails or reorders what
//! the production path already does. The records override the daemon serves
//! beside them is the exception to "only withholds":
//! `records-policy:authority` selects an EC policy production does not run,
//! which completes and releases more than production's `Hold` does, so what a
//! generation does under it is not production behaviour
//! ([`crate::composer_probe::record::Records::configure_for_e2e`]).
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
    /// Only a question answer's jobs (`PtyOp::Answer`). An answer is
    /// written as one job per entry of [`crate::question::answer_keys`], one
    /// at a time: a form's one digit per question, a menu's one key
    /// sequence. So `skip` counts those jobs, not bytes or answers.
    Answer,
}

/// A writer fault plan: after `skip` matching jobs are written untouched,
/// the PTY takes `accept` more bytes of matching jobs in all, counted by what
/// it actually took (R-C-U2-01), and then fails each matching write with
/// `EIO`, until the plan is cleared.
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

    /// One `write(2)` offered `len` bytes: `Ok(n)` offers the PTY `n` of
    /// them (`len` when no fault applies), `Err` fails it with nothing
    /// taken. Spends nothing: [`Self::took`] charges what the PTY took.
    fn offer(&self, len: usize) -> std::io::Result<usize> {
        let faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        if !faults.applies || len == 0 {
            return Ok(len);
        }
        if faults.budget == 0 {
            drop(faults);
            self.failed_writes.fetch_add(1, Ordering::SeqCst);
            return Err(std::io::Error::from_raw_os_error(EIO));
        }
        Ok(len.min(faults.budget))
    }

    /// The PTY took `n` bytes of what [`Self::offer`] offered (R-C-U2-01):
    /// the budget is charged by what reached the PTY, so `accept=N` means N
    /// bytes reached it, whatever the PTY took per call or failed with.
    fn took(&self, n: usize) {
        let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        if faults.applies {
            faults.budget = faults.budget.saturating_sub(n);
        }
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
        let offered = self.controls.offer(buf.len())?;
        // An error, `Interrupted` included, took nothing and spends nothing.
        let took = self.inner.write(&buf[..offered])?;
        self.controls.took(took);
        Ok(took)
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

/// The attach controls by client id, for this process. Entries are never
/// removed: a test or `e2e` daemon keeps one small entry per client id a
/// connection or a control named, for as long as it runs.
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

    /// An inner writer scripted call by call: `Ok(n)` takes at most `n` of
    /// the offered bytes, `Err(kind)` takes none.
    struct Scripted {
        taken: Arc<Mutex<Vec<u8>>>,
        script: std::collections::VecDeque<std::io::Result<usize>>,
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = match self.script.pop_front() {
                Some(Ok(n)) => n.min(buf.len()),
                Some(Err(e)) => return Err(e),
                None => buf.len(),
            };
            self.taken.lock().unwrap().extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn scripted(
        script: Vec<std::io::Result<usize>>,
    ) -> (FaultableWriter, Arc<WriterE2e>, Arc<Mutex<Vec<u8>>>) {
        let taken = Arc::new(Mutex::new(Vec::new()));
        let controls = Arc::new(WriterE2e::default());
        let w = FaultableWriter {
            inner: Box::new(Scripted {
                taken: Arc::clone(&taken),
                script: script.into(),
            }),
            controls: Arc::clone(&controls),
        };
        (w, controls, taken)
    }

    /// R-C-U2-01: `accept=N` means N bytes reached the PTY. An inner write
    /// that takes less than it was offered spends only what it took, so the
    /// budget's remaining bytes are still offered.
    #[test]
    fn a_short_inner_write_spends_only_what_it_took() {
        let (mut w, controls, taken) = scripted(vec![Ok(1), Ok(1)]);
        controls.set_faults(FaultPlan::parse("jobs=any,accept=3"));
        controls.begin_job(false);
        assert_eq!(w.write(b"abc").unwrap(), 1, "the PTY took one byte");
        assert_eq!(w.write(b"bc").unwrap(), 1, "the budget still offers two");
        assert_eq!(w.write(b"c").unwrap(), 1, "and then the last one");
        assert!(w.write(b"d").is_err(), "the fourth byte is past the budget");
        assert_eq!(&*taken.lock().unwrap(), b"abc");
        assert_eq!(controls.failed_writes(), 1);
    }

    /// R-C-U2-01: an inner write that fails, `Interrupted` included, takes
    /// nothing and spends nothing, so the caller's retry gets the whole
    /// budget.
    #[test]
    fn a_failed_inner_write_spends_nothing() {
        let (mut w, controls, taken) = scripted(vec![
            Err(std::io::ErrorKind::Interrupted.into()),
            Err(std::io::ErrorKind::WouldBlock.into()),
        ]);
        controls.set_faults(FaultPlan::parse("jobs=any,accept=2"));
        controls.begin_job(false);
        assert_eq!(
            w.write(b"ab").unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
        assert_eq!(
            w.write(b"ab").unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(w.write(b"ab").unwrap(), 2, "the retry takes the budget");
        assert!(w.write(b"c").is_err());
        assert_eq!(&*taken.lock().unwrap(), b"ab");
        assert_eq!(controls.failed_writes(), 1, "only the plan's own failure");
    }

    mod records {
        use crate::composer_probe::query_ledger::HazardFifo;
        use crate::composer_probe::record::{
            Attribution, Channel, Decision, EcPolicy, NotOwned, Observation, RecordKey, Records,
        };
        use std::time::{Duration, Instant};

        const G: u64 = 7;

        fn observation(at: Instant, decision: Option<Decision>) -> Observation {
            Observation {
                generation: G,
                key: RecordKey { pid: 41, start: 9 },
                channel: Channel::Ipc,
                attribution: Attribution::NotOwned(NotOwned::Tool),
                claim: None,
                decision,
                seg_seen: None,
                native_seen: None,
                birth_floor: at,
            }
        }

        /// R-C-U2-02: a helper observed with its decision in the same call,
        /// then exited, drained and pruned, leaves no record, episode or
        /// pending clock behind, and the override is still refused: once
        /// anything was observed, for good.
        #[test]
        fn configuration_is_refused_after_an_observation_that_was_pruned() {
            let t = Instant::now();
            let mut r = Records::new(G, EcPolicy::Hold, t);
            let mut fifo = HazardFifo::default();
            let obs = observation(t, Some(Decision::Armed));
            r.observe(&mut fifo, &obs, t).unwrap();
            let later = t + Duration::from_millis(5);
            r.exit(&mut fifo, G, obs.key, later, later).unwrap();
            r.drained(&mut fifo, G, obs.key, later).unwrap();
            assert!(
                r.prune(&mut fifo, G, obs.key, true, later)
                    .unwrap()
                    .is_some(),
                "the record goes"
            );
            assert!(r.is_empty(), "nothing retained");
            assert!(
                r.configure_for_e2e(Some(EcPolicy::Authority), None)
                    .is_err(),
                "the policy after a pruned observation"
            );
            assert!(
                r.configure_for_e2e(None, Some(1)).is_err(),
                "the cap after a pruned observation"
            );
            assert_eq!(r.policy(), EcPolicy::Hold);
        }

        /// R-C-U2-02: an observation the identity refusal dropped makes no
        /// record, and still counts as the first observation.
        #[test]
        fn configuration_is_refused_after_a_refused_observation() {
            let t = Instant::now();
            let mut r = Records::new(G, EcPolicy::Hold, t);
            r.refused_identity(G, Channel::Ipc, Some(Decision::Armed), None, t)
                .unwrap();
            assert!(r.is_empty());
            assert!(r.configure_for_e2e(None, Some(1)).is_err());
        }

        /// R-C-U2-02: one dropped at the cap the override set does too.
        #[test]
        fn configuration_is_refused_after_one_dropped_at_the_cap() {
            let t = Instant::now();
            let mut r = Records::new(G, EcPolicy::Hold, t);
            let mut fifo = HazardFifo::default();
            r.configure_for_e2e(Some(EcPolicy::Authority), Some(1))
                .unwrap();
            let first = observation(t, Some(Decision::Armed));
            r.observe(&mut fifo, &first, t).unwrap();
            let mut second = observation(t, Some(Decision::Armed));
            second.key.pid = 42;
            r.observe(&mut fifo, &second, t).unwrap();
            assert_eq!(r.len(), 1, "the second was dropped at the cap of 1");
            assert!(r.configure_for_e2e(None, Some(2)).is_err());
            assert_eq!(r.policy(), EcPolicy::Authority);
        }

        /// The Q35-20c fix review's U3-2: each observation the record cap
        /// drops counts once in its decision's bucket, and an observation of
        /// a record already kept counts in none.
        #[test]
        fn each_drop_at_the_cap_counts_in_its_decisions_bucket() {
            let t = Instant::now();
            let mut r = Records::new(G, EcPolicy::Hold, t);
            let mut fifo = HazardFifo::default();
            r.configure_for_e2e(None, Some(1)).unwrap();
            let kept = observation(t, Some(Decision::Armed));
            r.observe(&mut fifo, &kept, t).unwrap();
            let decisions = [
                None,
                Some(Decision::Armed),
                Some(Decision::Armed),
                Some(Decision::Unarmed),
                Some(Decision::Tool),
                Some(Decision::Unknown),
            ];
            for (pid, decision) in (100u32..).zip(decisions) {
                let mut dropped = observation(t, decision);
                dropped.key.pid = pid;
                r.observe(&mut fifo, &dropped, t).unwrap();
            }
            r.observe(&mut fifo, &kept, t).unwrap();
            assert_eq!(r.len(), 1);
            let w = r.witness();
            assert_eq!(
                (
                    w.cap_dropped_undecided,
                    w.cap_dropped_armed,
                    w.cap_dropped_unarmed,
                    w.cap_dropped_tool,
                    w.cap_dropped_unknown
                ),
                (1, 2, 1, 1, 1)
            );
        }

        /// The override before any observation takes effect, within its
        /// bounds: a cap of 0 or past `RECORD_CAP` is refused, as is a
        /// retired generation.
        #[test]
        fn configuration_before_any_observation_takes_effect_within_bounds() {
            use crate::composer_probe::record::RECORD_CAP;
            let t = Instant::now();
            let mut r = Records::new(G, EcPolicy::Hold, t);
            assert!(r.configure_for_e2e(None, Some(0)).is_err());
            assert!(r.configure_for_e2e(None, Some(RECORD_CAP + 1)).is_err());
            r.configure_for_e2e(Some(EcPolicy::Authority), Some(2))
                .unwrap();
            assert_eq!(r.policy(), EcPolicy::Authority);
            assert_eq!(r.cap(), 2);
            r.retire(t);
            assert_eq!(
                r.configure_for_e2e(None, Some(1)),
                Err("the generation retired")
            );
            assert_eq!(r.cap(), 2, "a refused override changes nothing");
        }

        /// The S-20e follow-up review's U2-1: the witness names each record's
        /// exit drain, once, with its ending: an `Unarmed` helper that sent
        /// no report drains as `WaitUnknown`, an `Armed` one does not, one
        /// given up is not complete, a second drain of the same record adds
        /// nothing, and only the latest `DRAINS_KEPT` are kept, oldest first.
        #[test]
        fn the_witness_names_each_drain_and_its_ending() {
            use crate::composer_probe::record::{DRAINS_KEPT, Report};
            let t = Instant::now();
            let mut r = Records::new(G, EcPolicy::Hold, t);
            let mut fifo = HazardFifo::default();
            let helper = |pid: u32, decision: Decision| Observation {
                key: RecordKey { pid, start: 9 },
                attribution: Attribution::Owned,
                ..observation(t, Some(decision))
            };
            let unknown = helper(1, Decision::Unarmed);
            r.observe(&mut fifo, &unknown, t).unwrap();
            assert_eq!(r.witness().drains, 0, "observed, not drained");
            r.exit(&mut fifo, G, unknown.key, t, t).unwrap();
            assert_eq!(r.witness().drains, 0, "exited, not drained");
            r.drained(&mut fifo, G, unknown.key, t).unwrap();
            r.drained(&mut fifo, G, unknown.key, t).unwrap();
            let w = r.witness();
            assert_eq!(w.drains, 1, "a second drain adds nothing");
            let first = w.drained().next().unwrap();
            assert_eq!(first.key, unknown.key);
            assert!(first.complete && first.wait_unknown, "{first:?}");
            assert_eq!(
                (first.decision, first.report),
                (Some(Decision::Unarmed), Report::None)
            );

            let armed = helper(2, Decision::Armed);
            r.observe(&mut fifo, &armed, t).unwrap();
            r.exit(&mut fifo, G, armed.key, t, t).unwrap();
            r.drained_uncertain(&mut fifo, G, armed.key, t).unwrap();
            let last = r.witness().drained().last().unwrap();
            assert_eq!(last.key, armed.key);
            assert!(!last.complete && !last.wait_unknown, "{last:?}");

            r.configure_for_e2e(None, None).unwrap_err();
            for pid in 3..3 + DRAINS_KEPT as u32 {
                let h = helper(pid, Decision::Unarmed);
                r.observe(&mut fifo, &h, t).unwrap();
                r.exit(&mut fifo, G, h.key, t, t).unwrap();
                r.drained(&mut fifo, G, h.key, t).unwrap();
            }
            let w = r.witness();
            assert_eq!(w.drains, 2 + DRAINS_KEPT as u64);
            let pids: Vec<u32> = w.drained().map(|d| d.key.pid).collect();
            assert_eq!(
                pids,
                (3..3 + DRAINS_KEPT as u32).collect::<Vec<_>>(),
                "the latest, oldest first"
            );
        }
    }

    /// Wait, at most `bound`, for `done`: whether it came.
    fn within(bound: Duration, done: impl Fn() -> bool) -> bool {
        let until = std::time::Instant::now() + bound;
        while !done() {
            if std::time::Instant::now() >= until {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    const BOUND: Duration = Duration::from_secs(10);

    #[test]
    fn a_hold_parks_until_go_or_retirement() {
        let controls = Arc::new(WriterE2e::default());
        let retired = Arc::new(AtomicBool::new(false));
        controls.hold();
        let (c, r) = (Arc::clone(&controls), Arc::clone(&retired));
        let thread = std::thread::spawn(move || c.at_dequeue(&r));
        assert!(within(BOUND, || controls.parked()), "parks at the hold");
        controls.go();
        assert!(within(BOUND, || thread.is_finished()), "leaves at the go");
        thread.join().unwrap();
        assert!(!controls.parked());
        controls.hold();
        let (c, r) = (Arc::clone(&controls), Arc::clone(&retired));
        let thread = std::thread::spawn(move || c.at_dequeue(&r));
        assert!(within(BOUND, || controls.parked()), "parks again");
        retired.store(true, Ordering::SeqCst);
        assert!(
            within(BOUND, || thread.is_finished()),
            "leaves at retirement, within its poll"
        );
        thread.join().unwrap();
        assert_eq!(controls.dequeued(), 2);
    }
}
