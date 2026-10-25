//! The worktree walks: a fixed pool of worker threads fed by a bounded set
//! of outstanding jobs, so neither the number of worktrees in use nor how
//! often runs come and go can grow the walks the collector has in flight.
//!
//! A worktree has at most one walk outstanding (queued or running), and that
//! walk keeps its place until it finishes, even when no run uses the
//! worktree any more: a worktree that is left and used again while its walk
//! runs is not walked twice. Each walk is tagged with a generation, and only
//! the completion of the walk a worktree is waiting for is applied to it.
//! Every walk runs under a [`Budget`]; dropping the [`Walks`] cancels what is
//! running.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::disk::{Budget, Limits, Walk};
use super::otlp::{WorktreeRef, WorktreeUsage};

/// Worker threads walking worktrees.
pub const WORKERS: usize = 2;
/// Walks queued or running at once, at most. A worktree due for a walk past
/// it waits for a later sample; the one whose last walk is oldest goes first.
pub const MAX_OUTSTANDING: usize = 16;

/// The worktrees in use, by canonical toplevel.
pub type InUse = BTreeMap<PathBuf, WorktreeRef>;

/// What walks one worktree: [`super::disk::walk`] in the collector, a
/// stand-in in tests.
pub type WalkFn = Arc<dyn Fn(&Path, &Limits<'_>) -> io::Result<Walk> + Send + Sync>;

struct Job {
    toplevel: PathBuf,
    worktree: WorktreeRef,
    generation: u64,
}

struct Done {
    toplevel: PathBuf,
    generation: u64,
    usage: Option<WorktreeUsage>,
}

#[derive(Default)]
struct State {
    last: Option<WorktreeUsage>,
    started: Option<Instant>,
    /// The generation of the walk queued or running for it.
    outstanding: Option<u64>,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    cancel: AtomicBool,
}

/// The walks of every worktree in use.
pub struct Walks {
    interval: Duration,
    state: HashMap<PathBuf, State>,
    shared: Arc<Shared>,
    done_rx: mpsc::Receiver<Done>,
    max_outstanding: usize,
    generation: u64,
    /// Ids of worktrees whose walk completed incomplete since the last
    /// [`Self::take_incomplete`].
    incomplete: Vec<String>,
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl Walks {
    /// A pool of `workers` threads walking each worktree in use at most every
    /// `interval`, with at most `max_outstanding` walks queued or running.
    /// Fails only when no worker thread could be started; fewer than asked
    /// is enough.
    pub fn new(
        interval: Duration,
        workers: usize,
        max_outstanding: usize,
        budget: Budget,
        walk: WalkFn,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let (done_tx, done_rx) = mpsc::channel();
        let mut started = 0;
        let mut last_error = None;
        for _ in 0..workers.max(1) {
            let shared = Arc::clone(&shared);
            let done = done_tx.clone();
            let walk = Arc::clone(&walk);
            let budget = budget.clone();
            match std::thread::Builder::new()
                .name("worktree-walk".into())
                .spawn(move || work(&shared, &done, &walk, &budget))
            {
                Ok(_) => started += 1,
                Err(e) => last_error = Some(e),
            }
        }
        if started == 0 {
            return Err(last_error.unwrap_or_else(|| io::Error::other("no walk worker")));
        }
        Ok(Walks {
            interval,
            state: HashMap::new(),
            shared,
            done_rx,
            max_outstanding: max_outstanding.max(1),
            generation: 0,
            incomplete: Vec::new(),
        })
    }

    /// Walks queued or running.
    pub fn outstanding(&self) -> usize {
        self.state
            .values()
            .filter(|s| s.outstanding.is_some())
            .count()
    }

    /// Ids of the worktrees whose walk stopped at a limit since the last call.
    pub fn take_incomplete(&mut self) -> Vec<String> {
        std::mem::take(&mut self.incomplete)
    }

    fn apply(&mut self, done: Done) {
        let Some(state) = self.state.get_mut(&done.toplevel) else {
            return;
        };
        // Only the walk it is waiting for settles it.
        if state.outstanding != Some(done.generation) {
            return;
        }
        state.outstanding = None;
        if let Some(usage) = done.usage {
            if !usage.complete {
                self.incomplete.push(usage.worktree.id.clone());
            }
            state.last = Some(usage);
        }
    }

    fn drain(&mut self) {
        while let Ok(done) = self.done_rx.try_recv() {
            self.apply(done);
        }
    }

    /// Queue a walk for each worktree in use that is due and has none
    /// outstanding, the longest-waiting first, up to the bound.
    fn schedule(&mut self, in_use: &InUse) {
        // A worktree no run uses is forgotten, unless its walk is still
        // outstanding: that walk keeps its place until it finishes.
        self.state
            .retain(|toplevel, state| in_use.contains_key(toplevel) || state.outstanding.is_some());
        let mut due: Vec<(&PathBuf, &WorktreeRef, Option<Instant>)> = Vec::new();
        for (toplevel, worktree) in in_use {
            let state = self.state.entry(toplevel.clone()).or_default();
            let is_due = state
                .started
                .is_none_or(|started| started.elapsed() >= self.interval);
            if state.outstanding.is_none() && is_due {
                due.push((toplevel, worktree, state.started));
            }
        }
        // Never walked first, then the oldest walk.
        due.sort_by_key(|(_, _, started)| *started);
        let room = self.max_outstanding.saturating_sub(self.outstanding());
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (toplevel, worktree, _) in due.into_iter().take(room) {
            self.generation += 1;
            let state = self.state.get_mut(toplevel).expect("entered above");
            state.outstanding = Some(self.generation);
            state.started = Some(Instant::now());
            queue.push_back(Job {
                toplevel: toplevel.clone(),
                worktree: worktree.clone(),
                generation: self.generation,
            });
        }
        drop(queue);
        self.shared.ready.notify_all();
    }

    /// Each worktree in use's most recent completed walk, named as this
    /// sample names it, after queuing the walks that are due.
    pub fn update(&mut self, in_use: &InUse) -> Vec<WorktreeUsage> {
        self.drain();
        self.schedule(in_use);
        self.usage(in_use)
    }

    /// Walk every worktree in use once, through the same pool, and wait
    /// until each has finished or `deadline` has passed. Walks still running
    /// then are cancelled and given a second to report what they counted,
    /// and the pool stops: this is `--once`'s single use of it.
    pub fn measure_all(&mut self, in_use: &InUse, deadline: Instant) -> Vec<WorktreeUsage> {
        let mut cancelled_at = None;
        loop {
            self.drain();
            if cancelled_at.is_none() {
                self.schedule(in_use);
            }
            let waiting = in_use.keys().any(|toplevel| {
                self.state
                    .get(toplevel)
                    .is_none_or(|s| s.outstanding.is_some() || s.started.is_none())
            });
            if !waiting {
                break;
            }
            let now = Instant::now();
            let until = match cancelled_at {
                Some(at) => at + Duration::from_secs(1),
                None if now >= deadline => {
                    self.shared.cancel.store(true, Ordering::Relaxed);
                    let dropped: Vec<Job> = self
                        .shared
                        .queue
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .drain(..)
                        .collect();
                    // A walk that was only queued never reports.
                    for job in dropped {
                        if let Some(state) = self.state.get_mut(&job.toplevel) {
                            state.outstanding = None;
                        }
                    }
                    cancelled_at = Some(now);
                    continue;
                }
                None => deadline,
            };
            if now >= until {
                break;
            }
            match self.done_rx.recv_timeout(until - now) {
                Ok(done) => self.apply(done),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if cancelled_at.is_some()
                && in_use
                    .keys()
                    .all(|t| self.state.get(t).is_none_or(|s| s.outstanding.is_none()))
            {
                break;
            }
        }
        self.usage(in_use)
    }

    fn usage(&self, in_use: &InUse) -> Vec<WorktreeUsage> {
        in_use
            .iter()
            .filter_map(|(toplevel, worktree)| {
                let last = self.state.get(toplevel)?.last.as_ref()?;
                // The id or path may be stated differently this sample
                // (paths switched on): send what this sample names.
                Some(WorktreeUsage {
                    worktree: worktree.clone(),
                    ..last.clone()
                })
            })
            .collect()
    }
}

impl Drop for Walks {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
    }
}

fn work(shared: &Shared, done: &mpsc::Sender<Done>, walk: &WalkFn, budget: &Budget) {
    loop {
        let job = {
            let mut queue = shared
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                if shared.cancel.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = shared
                    .ready
                    .wait(queue)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        let limits = Limits {
            budget,
            deadline: Instant::now() + budget.time,
            cancel: &shared.cancel,
        };
        let usage = walk(&job.toplevel, &limits).ok().map(|w| WorktreeUsage {
            worktree: job.worktree,
            bytes: w.bytes,
            measured_unix_nanos: now_unix_nanos(),
            complete: w.complete,
        });
        let finished = Done {
            toplevel: job.toplevel,
            generation: job.generation,
            usage,
        };
        if done.send(finished).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::collector::disk::DEFAULT_BUDGET;
    use std::sync::atomic::AtomicUsize;

    /// A stand-in walk that blocks until the test lets it go, and counts how
    /// many walks started and how many ran at once.
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
        started: AtomicUsize,
        running: AtomicUsize,
        most_running: AtomicUsize,
    }

    impl Gate {
        fn new() -> Arc<Gate> {
            Arc::new(Gate {
                open: Mutex::new(false),
                changed: Condvar::new(),
                started: AtomicUsize::new(0),
                running: AtomicUsize::new(0),
                most_running: AtomicUsize::new(0),
            })
        }

        fn walk(self: &Arc<Self>) -> WalkFn {
            let gate = Arc::clone(self);
            Arc::new(move |_root, limits| {
                gate.started.fetch_add(1, Ordering::SeqCst);
                let now = gate.running.fetch_add(1, Ordering::SeqCst) + 1;
                gate.most_running.fetch_max(now, Ordering::SeqCst);
                let mut open = gate.open.lock().unwrap();
                while !*open && !limits.cancel.load(Ordering::Relaxed) {
                    open = gate
                        .changed
                        .wait_timeout(open, Duration::from_millis(20))
                        .unwrap()
                        .0;
                }
                gate.running.fetch_sub(1, Ordering::SeqCst);
                Ok(Walk {
                    bytes: 42,
                    complete: true,
                })
            })
        }

        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }

        fn wait_started(&self, n: usize) {
            let until = Instant::now() + Duration::from_secs(10);
            while self.started.load(Ordering::SeqCst) < n {
                assert!(
                    Instant::now() < until,
                    "only {} walks started",
                    self.started.load(Ordering::SeqCst)
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn in_use(names: &[&str]) -> InUse {
        names
            .iter()
            .map(|n| {
                (
                    PathBuf::from(format!("/w/{n}")),
                    WorktreeRef {
                        id: (*n).to_string(),
                        path: None,
                    },
                )
            })
            .collect()
    }

    /// Wait for the walks to settle into `walks`' state.
    fn settle(walks: &mut Walks, in_use: &InUse) -> Vec<WorktreeUsage> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let usage = walks.update(in_use);
            if walks.outstanding() == 0 || Instant::now() > until {
                return usage;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A2: however many worktrees are in use, no more than the bound of walks
    /// is outstanding and no more than the pool runs at once.
    #[test]
    fn outstanding_walks_are_bounded() {
        let gate = Gate::new();
        let mut walks = Walks::new(Duration::ZERO, 2, 3, DEFAULT_BUDGET, gate.walk()).unwrap();
        let names: Vec<String> = (0..10).map(|i| format!("w{i}")).collect();
        let many = in_use(&names.iter().map(String::as_str).collect::<Vec<_>>());
        for _ in 0..5 {
            walks.update(&many);
            assert!(walks.outstanding() <= 3, "{}", walks.outstanding());
        }
        gate.wait_started(2);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(gate.started.load(Ordering::SeqCst), 2, "only the pool runs");
        assert_eq!(walks.outstanding(), 3);
        gate.release();
        // Every worktree gets its walk in turn, never more than 3 at once.
        let until = Instant::now() + Duration::from_secs(10);
        while gate.started.load(Ordering::SeqCst) < 10 {
            walks.update(&many);
            assert!(walks.outstanding() <= 3);
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(gate.most_running.load(Ordering::SeqCst) <= 2);
    }

    /// A2 and R5: a worktree that is left and used again while its walk runs
    /// is not walked a second time, and that walk's completion still lands.
    #[test]
    fn a_worktree_left_and_reentered_mid_walk_keeps_its_one_walk() {
        let gate = Gate::new();
        let mut walks = Walks::new(
            Duration::from_secs(3600),
            2,
            16,
            DEFAULT_BUDGET,
            gate.walk(),
        )
        .unwrap();
        let a = in_use(&["a"]);
        assert!(walks.update(&a).is_empty());
        gate.wait_started(1);
        // No run uses it: not sent, but its walk keeps its place.
        assert!(walks.update(&InUse::new()).is_empty());
        assert_eq!(walks.outstanding(), 1);
        // Used again before the walk finished: no second walk.
        assert!(walks.update(&a).is_empty());
        assert_eq!(walks.outstanding(), 1);
        gate.release();
        let usage = settle(&mut walks, &a);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].bytes, 42);
        assert!(usage[0].complete);
        assert_eq!(gate.started.load(Ordering::SeqCst), 1);
        // Left again after it finished: forgotten.
        walks.update(&InUse::new());
        assert!(walks.state.is_empty());
    }

    /// R5: a completion for a walk the worktree is not waiting for (an older
    /// generation) changes nothing.
    #[test]
    fn a_stale_completion_is_ignored() {
        let gate = Gate::new();
        let mut walks = Walks::new(
            Duration::from_secs(3600),
            1,
            16,
            DEFAULT_BUDGET,
            gate.walk(),
        )
        .unwrap();
        let a = in_use(&["a"]);
        walks.update(&a);
        let waiting = walks.state[Path::new("/w/a")].outstanding.unwrap();
        walks.apply(Done {
            toplevel: PathBuf::from("/w/a"),
            generation: waiting + 7,
            usage: None,
        });
        assert_eq!(walks.state[Path::new("/w/a")].outstanding, Some(waiting));
        gate.release();
    }

    /// `--once`: every worktree in use is walked through the pool, and a
    /// deadline cancels what has not finished.
    #[test]
    fn measure_all_walks_each_once_and_honours_its_deadline() {
        let gate = Gate::new();
        gate.release();
        let mut walks = Walks::new(Duration::MAX, 2, 3, DEFAULT_BUDGET, gate.walk()).unwrap();
        let names: Vec<String> = (0..7).map(|i| format!("w{i}")).collect();
        let many = in_use(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let usage = walks.measure_all(&many, Instant::now() + Duration::from_secs(10));
        assert_eq!(usage.len(), 7);
        assert_eq!(gate.started.load(Ordering::SeqCst), 7);

        let stuck = Gate::new();
        let mut walks = Walks::new(Duration::MAX, 2, 3, DEFAULT_BUDGET, stuck.walk()).unwrap();
        let started = Instant::now();
        let usage = walks.measure_all(&many, Instant::now() + Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(5));
        // The two running walks were cancelled and reported; the rest never ran.
        assert_eq!(usage.len(), 2, "{usage:?}");
    }
}
