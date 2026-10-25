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
/// for a later sample; one never answered goes first, then the one whose
/// last lookup is oldest.
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
    /// outstanding, up to the bound, and forget the directories not in it
    /// whose lookup has finished. Then wait, until `until` at the latest,
    /// for the directories in `wanted` that have a lookup outstanding and no
    /// answer yet. A directory of `wanted` that could not be queued is not
    /// waited for.
    pub fn update(&mut self, cwds: &BTreeSet<PathBuf>, wanted: &BTreeSet<PathBuf>, until: Instant) {
        self.drain();
        self.schedule(cwds);
        loop {
            let waiting = wanted.iter().any(|cwd| {
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

    fn schedule(&mut self, cwds: &BTreeSet<PathBuf>) {
        // A directory no longer in use is forgotten, unless its lookup is
        // still outstanding: that lookup keeps its place until it finishes.
        self.state
            .retain(|cwd, state| cwds.contains(cwd) || state.outstanding.is_some());
        let mut due: Vec<(&PathBuf, bool, Option<Instant>)> = Vec::new();
        for cwd in cwds {
            let state = self.state.entry(cwd.clone()).or_default();
            if state.outstanding.is_none() {
                due.push((cwd, state.answer.is_some(), state.queued));
            }
        }
        // Never answered first, then the oldest lookup.
        due.sort_by_key(|(_, answered, queued)| (*answered, *queued));
        let room = self.max_outstanding.saturating_sub(self.outstanding());
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (cwd, _, _) in due.into_iter().take(room) {
            self.generation += 1;
            let state = self.state.get_mut(cwd).expect("entered above");
            state.outstanding = Some(self.generation);
            state.queued = Some(Instant::now());
            queue.push_back(Job {
                cwd: cwd.clone(),
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
