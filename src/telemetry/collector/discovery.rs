//! Worktree discovery: which git checkout each working directory in use is
//! in, found by a small pool of threads rather than on the sampling thread,
//! so that a directory on a stalled filesystem holds up its own answer and
//! not every run's CPU and memory figures.
//!
//! It is bounded the way the walks are ([`super::walks`]): a directory has at
//! most one lookup outstanding (queued or running), which keeps its place
//! until it finishes, and at most [`MAX_OUTSTANDING`] are outstanding at once.
//! A lookup blocked on a stalled filesystem holds its worker and its place
//! until the filesystem answers. Each directory in use is looked up again at
//! every sample, and until the new answer arrives its last one stands.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::disk::git_toplevel;

/// Worker threads looking directories up.
pub const WORKERS: usize = 2;
/// Lookups queued or running at once, at most. A directory past it waits
/// for a place: one a sample waits for goes first, then one never answered,
/// then the one whose last lookup is oldest.
pub const MAX_OUTSTANDING: usize = 64;
/// How long a sample waits for the lookups that decide the worktree of a run
/// it sees for the first time. A lookup still outstanding then counts as no
/// worktree for that run, as a lookup that failed does.
pub const FIRST_SIGHT_WAIT: Duration = Duration::from_secs(1);

/// What looks one directory up: [`toplevel`] in the collector, a stand-in in
/// tests.
pub type LookupFn = Arc<dyn Fn(&Path) -> Option<PathBuf> + Send + Sync>;

/// The canonical git toplevel of the working directory `cwd`, or `None`
/// when it is in no checkout or the toplevel cannot be canonicalised.
pub fn toplevel(cwd: &Path) -> Option<PathBuf> {
    git_toplevel(cwd).and_then(|dir| fs::canonicalize(dir).ok())
}

struct Job {
    cwd: PathBuf,
    generation: u64,
}

struct Done {
    cwd: PathBuf,
    generation: u64,
    toplevel: Option<PathBuf>,
}

#[derive(Default)]
struct State {
    /// The last answer: `Some(None)` when it is in no worktree.
    answer: Option<Option<PathBuf>>,
    /// When its last lookup was queued.
    queued: Option<Instant>,
    /// The generation of the lookup queued or running for it.
    outstanding: Option<u64>,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    cancel: AtomicBool,
}

/// The worktree lookups of every working directory in use.
pub struct Discovery {
    state: HashMap<PathBuf, State>,
    shared: Arc<Shared>,
    done_rx: mpsc::Receiver<Done>,
    max_outstanding: usize,
    generation: u64,
}

impl Discovery {
    /// A pool of `workers` threads with at most `max_outstanding` lookups
    /// queued or running. Fails only when no worker thread could be started;
    /// fewer than asked is enough.
    pub fn new(workers: usize, max_outstanding: usize, lookup: LookupFn) -> io::Result<Self> {
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
            let lookup = Arc::clone(&lookup);
            match std::thread::Builder::new()
                .name("worktree-lookup".into())
                .spawn(move || work(&shared, &done, &lookup))
            {
                Ok(_) => started += 1,
                Err(e) => last_error = Some(e),
            }
        }
        if started == 0 {
            return Err(last_error.unwrap_or_else(|| io::Error::other("no lookup worker")));
        }
        Ok(Discovery {
            state: HashMap::new(),
            shared,
            done_rx,
            max_outstanding: max_outstanding.max(1),
            generation: 0,
        })
    }

    /// Lookups queued or running.
    pub fn outstanding(&self) -> usize {
        self.state
            .values()
            .filter(|s| s.outstanding.is_some())
            .count()
    }

    /// The last answer for `cwd`: `None` while it has had none, `Some(None)`
    /// when it is in no worktree.
    pub fn toplevel(&self, cwd: &Path) -> Option<&Option<PathBuf>> {
        self.state.get(cwd)?.answer.as_ref()
    }

    /// Queue a lookup of each directory in `cwds` that has none
    /// outstanding, up to the bound and those in `wanted` without an answer
    /// first, and forget the directories not in it whose lookup has
    /// finished. Then wait, until `until` at the latest, for the directories
    /// in `wanted` that have no answer yet, queueing each that missed the
    /// bound once a place frees, as [`Self::look_up_all`] does.
    pub fn update(&mut self, cwds: &BTreeSet<PathBuf>, wanted: &BTreeSet<PathBuf>, until: Instant) {
        self.drain();
        self.schedule(cwds, wanted);
        loop {
            let unqueued: Vec<PathBuf> = wanted
                .iter()
                .filter(|cwd| {
                    cwds.contains(*cwd)
                        && self
                            .state
                            .get(*cwd)
                            .is_some_and(|s| s.answer.is_none() && s.outstanding.is_none())
                })
                .cloned()
                .collect();
            let room = self.max_outstanding.saturating_sub(self.outstanding());
            let left = unqueued.len().saturating_sub(room);
            self.queue(unqueued.into_iter().take(room).collect());
            let waiting = left > 0
                || wanted.iter().any(|cwd| {
                    self.state
                        .get(cwd)
                        .is_some_and(|s| s.answer.is_none() && s.outstanding.is_some())
                });
            let now = Instant::now();
            if !waiting || now >= until {
                return;
            }
            match self.done_rx.recv_timeout(until - now) {
                Ok(done) => self.apply(done),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Look every directory in `cwds` up, and wait until each has answered
    /// or `until` has passed: `--once`'s single sample, which has no later
    /// one to look the rest up in. Each directory is queued once, as room
    /// frees under the bound; one with a lookup already outstanding is
    /// waited for rather than queued again, and one already answered keeps
    /// its answer.
    pub fn look_up_all(&mut self, cwds: &BTreeSet<PathBuf>, until: Instant) {
        self.drain();
        self.forget_unused(cwds);
        let mut unqueued: VecDeque<PathBuf> = cwds
            .iter()
            .filter(|cwd| {
                self.state
                    .get(*cwd)
                    .is_none_or(|s| s.outstanding.is_none() && s.answer.is_none())
            })
            .cloned()
            .collect();
        loop {
            let room = self.max_outstanding.saturating_sub(self.outstanding());
            let take = room.min(unqueued.len());
            self.queue(unqueued.drain(..take).collect());
            let waiting = !unqueued.is_empty()
                || cwds
                    .iter()
                    .any(|cwd| self.state.get(cwd).is_some_and(|s| s.outstanding.is_some()));
            let now = Instant::now();
            if !waiting || now >= until {
                return;
            }
            match self.done_rx.recv_timeout(until - now) {
                Ok(done) => self.apply(done),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn apply(&mut self, done: Done) {
        let Some(state) = self.state.get_mut(&done.cwd) else {
            return;
        };
        // Only the lookup it is waiting for settles it.
        if state.outstanding != Some(done.generation) {
            return;
        }
        state.outstanding = None;
        state.answer = Some(done.toplevel);
    }

    fn drain(&mut self) {
        while let Ok(done) = self.done_rx.try_recv() {
            self.apply(done);
        }
    }

    /// Forget the directories not in `cwds`, unless a lookup of one is
    /// still outstanding: that lookup keeps its place until it finishes.
    fn forget_unused(&mut self, cwds: &BTreeSet<PathBuf>) {
        self.state
            .retain(|cwd, state| cwds.contains(cwd) || state.outstanding.is_some());
    }

    fn schedule(&mut self, cwds: &BTreeSet<PathBuf>, wanted: &BTreeSet<PathBuf>) {
        self.forget_unused(cwds);
        let mut due: Vec<(&PathBuf, bool, bool, Option<Instant>)> = Vec::new();
        for cwd in cwds {
            let state = self.state.entry(cwd.clone()).or_default();
            if state.outstanding.is_none() {
                let answered = state.answer.is_some();
                let awaited = !answered && wanted.contains(cwd);
                due.push((cwd, !awaited, answered, state.queued));
            }
        }
        // What the sample waits for first, then never answered, then the
        // oldest lookup.
        due.sort_by_key(|(_, not_awaited, answered, queued)| (*not_awaited, *answered, *queued));
        let room = self.max_outstanding.saturating_sub(self.outstanding());
        let due: Vec<PathBuf> = due
            .into_iter()
            .take(room)
            .map(|(cwd, _, _, _)| cwd.clone())
            .collect();
        self.queue(due);
    }

    /// Queue a lookup of each of `cwds`, which have none outstanding.
    fn queue(&mut self, cwds: Vec<PathBuf>) {
        if cwds.is_empty() {
            return;
        }
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for cwd in cwds {
            self.generation += 1;
            let state = self.state.entry(cwd.clone()).or_default();
            state.outstanding = Some(self.generation);
            state.queued = Some(Instant::now());
            queue.push_back(Job {
                cwd,
                generation: self.generation,
            });
        }
        drop(queue);
        self.shared.ready.notify_all();
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
    }
}

fn work(shared: &Shared, done: &mpsc::Sender<Done>, lookup: &LookupFn) {
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
        let toplevel = lookup(&job.cwd);
        let finished = Done {
            cwd: job.cwd,
            generation: job.generation,
            toplevel,
        };
        if done.send(finished).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in lookup that answers `/w/<name>` for `/w/<name>/...`, and
    /// blocks, until the test opens the gate, on any directory under
    /// `/stalled`: a directory on a filesystem that does not answer.
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
        /// Lookups of a directory under `/stalled` started.
        stalled: std::sync::atomic::AtomicUsize,
    }

    impl Gate {
        fn new() -> Arc<Gate> {
            Arc::new(Gate {
                open: Mutex::new(false),
                changed: Condvar::new(),
                stalled: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn lookup(self: &Arc<Self>) -> LookupFn {
            let gate = Arc::clone(self);
            Arc::new(move |cwd| {
                if cwd.starts_with("/stalled") {
                    gate.stalled.fetch_add(1, Ordering::SeqCst);
                    let mut open = gate.open.lock().unwrap();
                    while !*open {
                        open = gate.changed.wait(open).unwrap();
                    }
                }
                cwd.ancestors()
                    .find(|a| a.parent() == Some(Path::new("/w")))
                    .map(Path::to_path_buf)
            })
        }

        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// Findings 1 and 10: a lookup blocked on a stalled filesystem holds up
    /// neither the sample that queued it, past the wait it was given, nor
    /// the answers for every other directory.
    #[test]
    fn a_stalled_lookup_holds_up_only_its_own_answer() {
        let gate = Gate::new();
        let mut discovery = Discovery::new(2, 16, gate.lookup()).unwrap();
        let cwds = set(&["/stalled/x", "/w/a/src", "/w/b"]);
        let started = Instant::now();
        discovery.update(&cwds, &cwds, started + Duration::from_millis(300));
        let waited = started.elapsed();
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(
            discovery.toplevel(Path::new("/w/a/src")),
            Some(&Some(PathBuf::from("/w/a")))
        );
        assert_eq!(
            discovery.toplevel(Path::new("/w/b")),
            Some(&Some(PathBuf::from("/w/b")))
        );
        assert_eq!(discovery.toplevel(Path::new("/stalled/x")), None);

        // Later samples go on answering while it stays stalled, and it is
        // not queued a second time.
        for _ in 0..3 {
            let started = Instant::now();
            discovery.update(&cwds, &set(&[]), started + Duration::from_secs(60));
            assert!(started.elapsed() < Duration::from_secs(1));
        }
        assert_eq!(gate.stalled.load(Ordering::SeqCst), 1);
        let more = set(&["/stalled/x", "/w/c"]);
        discovery.update(&more, &more, Instant::now() + Duration::from_millis(300));
        assert_eq!(
            discovery.toplevel(Path::new("/w/c")),
            Some(&Some(PathBuf::from("/w/c")))
        );

        // Once the filesystem answers, so does the lookup.
        gate.release();
        let until = Instant::now() + Duration::from_secs(10);
        while discovery.toplevel(Path::new("/stalled/x")).is_none() {
            assert!(Instant::now() < until, "the stalled lookup never landed");
            discovery.update(&more, &set(&[]), Instant::now());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(discovery.toplevel(Path::new("/stalled/x")), Some(&None));
    }

    /// The wait covers only what is wanted: a sample that needs no new
    /// answer does not wait at all.
    #[test]
    fn a_sample_waits_only_for_what_it_wants() {
        let gate = Gate::new();
        let mut discovery = Discovery::new(1, 16, gate.lookup()).unwrap();
        let started = Instant::now();
        discovery.update(
            &set(&["/stalled/y"]),
            &set(&[]),
            started + Duration::from_secs(60),
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        gate.release();
    }

    /// Outstanding lookups are bounded, and the directory past the bound
    /// is looked up once there is room.
    #[test]
    fn outstanding_lookups_are_bounded() {
        let gate = Gate::new();
        let mut discovery = Discovery::new(1, 2, gate.lookup()).unwrap();
        let stalled = set(&["/stalled/1", "/stalled/2", "/stalled/3"]);
        discovery.update(&stalled, &set(&[]), Instant::now());
        assert_eq!(discovery.outstanding(), 2);
        discovery.update(&stalled, &set(&[]), Instant::now());
        assert_eq!(discovery.outstanding(), 2);
        gate.release();
        let until = Instant::now() + Duration::from_secs(10);
        while discovery.toplevel(Path::new("/stalled/3")).is_none() {
            assert!(Instant::now() < until);
            discovery.update(&stalled, &set(&[]), Instant::now());
            assert!(discovery.outstanding() <= 2);
            std::thread::sleep(Duration::from_millis(5));
        }
        // A directory left is forgotten once its lookup has finished.
        discovery.update(&set(&[]), &set(&[]), Instant::now());
        let until = Instant::now() + Duration::from_secs(10);
        while !discovery.state.is_empty() {
            assert!(Instant::now() < until);
            discovery.update(&set(&[]), &set(&[]), Instant::now());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// With room for one lookup, a directory never answered goes before one
    /// that has an answer and is due to be looked up again.
    #[test]
    fn a_directory_never_answered_is_looked_up_first() {
        let gate = Gate::new();
        let mut discovery = Discovery::new(1, 1, gate.lookup()).unwrap();
        let a = set(&["/w/a"]);
        discovery.update(&a, &a, Instant::now() + Duration::from_secs(10));
        assert!(discovery.toplevel(Path::new("/w/a")).is_some());
        let both = set(&["/w/a", "/w/b"]);
        let b = set(&["/w/b"]);
        discovery.update(&both, &b, Instant::now() + Duration::from_secs(10));
        assert_eq!(
            discovery.toplevel(Path::new("/w/b")),
            Some(&Some(PathBuf::from("/w/b")))
        );
    }

    /// Qodo 4371872375: a sample that sees more new directories than may be
    /// outstanding at once queues the rest as places free during its wait,
    /// so each lookup that answers at once lands within the wait, not at a
    /// later sample.
    #[test]
    fn a_first_sight_wait_refills_past_the_bound() {
        let looked_up = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&looked_up);
        let lookup: LookupFn = Arc::new(move |cwd: &Path| {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(cwd.to_path_buf())
        });
        let mut discovery = Discovery::new(2, 3, lookup).unwrap();
        let cwds: BTreeSet<PathBuf> = (0..20).map(|i| PathBuf::from(format!("/w/{i}"))).collect();
        discovery.update(&cwds, &cwds, Instant::now() + FIRST_SIGHT_WAIT);
        for cwd in &cwds {
            assert_eq!(discovery.toplevel(cwd), Some(&Some(cwd.clone())), "{cwd:?}");
        }
        assert_eq!(looked_up.load(Ordering::SeqCst), cwds.len());
        assert_eq!(discovery.outstanding(), 0);
    }

    /// Qodo 4371872375: with room for one lookup, a directory the sample
    /// waits for goes before one it does not, even one never answered.
    #[test]
    fn a_directory_a_sample_waits_for_is_looked_up_first() {
        let gate = Gate::new();
        let mut discovery = Discovery::new(1, 1, gate.lookup()).unwrap();
        let cwds = set(&["/stalled/a", "/w/z"]);
        discovery.update(
            &cwds,
            &set(&["/w/z"]),
            Instant::now() + Duration::from_secs(10),
        );
        assert_eq!(
            discovery.toplevel(Path::new("/w/z")),
            Some(&Some(PathBuf::from("/w/z")))
        );
        assert_eq!(gate.stalled.load(Ordering::SeqCst), 0);
        gate.release();
    }

    /// N1: `--once` looks up every directory, more of them than may be
    /// outstanding at once: each is queued once as room frees, and the wait
    /// lasts until the last has answered.
    #[test]
    fn looking_up_all_refills_past_the_bound() {
        let looked_up = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&looked_up);
        let lookup: LookupFn = Arc::new(move |cwd: &Path| {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(cwd.to_path_buf())
        });
        let mut discovery = Discovery::new(2, 3, lookup).unwrap();
        let cwds: BTreeSet<PathBuf> = (0..20).map(|i| PathBuf::from(format!("/w/{i}"))).collect();
        discovery.look_up_all(&cwds, Instant::now() + Duration::from_secs(60));
        for cwd in &cwds {
            assert_eq!(discovery.toplevel(cwd), Some(&Some(cwd.clone())), "{cwd:?}");
        }
        assert_eq!(looked_up.load(Ordering::SeqCst), cwds.len());
        assert_eq!(discovery.outstanding(), 0);
    }

    /// N1: a directory whose lookup is already outstanding is waited for,
    /// not queued a second time, and the wait ends at `until` while one
    /// stays stalled.
    #[test]
    fn looking_up_all_waits_for_an_outstanding_lookup_once() {
        let gate = Gate::new();
        let mut discovery = Discovery::new(1, 2, gate.lookup()).unwrap();
        let stalled = set(&["/stalled/1"]);
        discovery.update(&stalled, &set(&[]), Instant::now());
        let cwds = set(&["/stalled/1", "/w/a", "/w/b", "/w/c"]);
        let started = Instant::now();
        discovery.look_up_all(&cwds, started + Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(discovery.toplevel(Path::new("/stalled/1")), None);
        gate.release();
        discovery.look_up_all(&cwds, Instant::now() + Duration::from_secs(60));
        for cwd in ["/w/a", "/w/b", "/w/c"] {
            assert_eq!(
                discovery.toplevel(Path::new(cwd)),
                Some(&Some(PathBuf::from(cwd)))
            );
        }
        assert_eq!(discovery.toplevel(Path::new("/stalled/1")), Some(&None));
        assert_eq!(gate.stalled.load(Ordering::SeqCst), 1);
    }

    /// The collector's lookup: a working directory's canonical toplevel.
    #[test]
    fn the_collectors_lookup_canonicalises_the_toplevel() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let expected = fs::canonicalize(&repo).unwrap();
        assert_eq!(toplevel(&link.join("src")), Some(expected));
    }
}
