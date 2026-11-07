//! The deck's editor helper's e2e pause seam, driven from its trace
//! (PRD #1666 part 3, `inbox/probe/040`): a thread lets every pause go at
//! once except the one a trial arms, which it holds for the trial's span and
//! then lets go, or whose helper it SIGKILLs by captured pid. Bounded waits
//! live here, never in an `e2e_*.rs` body (Decision 21).
#![allow(dead_code)]

use crate::common;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The helper's e2e pause seam, driven from its trace: every pause is
/// released at once, except the armed target's, which is held for its
/// trial's span and then released, or its helper SIGKILLed by captured pid.
pub struct Pauser {
    dir: tempfile::TempDir,
    state: Arc<Mutex<PauseState>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug)]
struct Target {
    point: &'static str,
    hold: Duration,
    kill: bool,
    /// Only a pause after this many pause lines can be the target.
    after: usize,
}

#[derive(Default)]
struct PauseState {
    target: Option<Target>,
    /// When the target's pause was seen, and when it was let go.
    hit: Option<Instant>,
    done: Option<Instant>,
    killed: Option<u32>,
    home: Option<PathBuf>,
}

impl Pauser {
    pub fn new() -> Self {
        let dir = common::harness_tempdir().unwrap();
        let fifo = dir.path().join("pause");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        std::fs::write(dir.path().join("trace"), "").unwrap();
        Self {
            dir,
            state: Arc::new(Mutex::new(PauseState::default())),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    fn fifo(&self) -> PathBuf {
        self.dir.path().join("pause")
    }

    fn trace_path(&self) -> PathBuf {
        self.dir.path().join("trace")
    }

    pub fn trace(&self) -> String {
        std::fs::read_to_string(self.trace_path()).unwrap_or_default()
    }

    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "DOT_AGENT_DECK_E2E_HELPER_PAUSE".into(),
                self.fifo().display().to_string(),
            ),
            (
                "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT".into(),
                "start,ready".into(),
            ),
            (
                "DOT_AGENT_DECK_E2E_HELPER_TRACE".into(),
                self.trace_path().display().to_string(),
            ),
        ]
    }

    pub fn pauses(trace: &str) -> Vec<String> {
        trace
            .lines()
            .filter_map(|l| {
                let mut words = l.split_whitespace().skip(1);
                (words.next() == Some("pause")).then(|| words.next().unwrap_or("").to_string())
            })
            .collect()
    }

    pub fn start(&mut self, home: &Path) {
        self.state.lock().unwrap().home = Some(home.to_path_buf());
        let (state, stop) = (Arc::clone(&self.state), Arc::clone(&self.stop));
        let (fifo, trace) = (self.fifo(), self.trace_path());
        self.thread = Some(std::thread::spawn(move || {
            let mut handled = 0;
            while !stop.load(Ordering::Relaxed) {
                let pauses = Self::pauses(&std::fs::read_to_string(&trace).unwrap_or_default());
                while handled < pauses.len() && !stop.load(Ordering::Relaxed) {
                    let target = {
                        let mut st = state.lock().unwrap();
                        let hit = st.target.filter(|t| {
                            st.hit.is_none() && handled >= t.after && pauses[handled] == t.point
                        });
                        if hit.is_some() {
                            st.hit = Some(Instant::now());
                        }
                        hit
                    };
                    match target {
                        Some(t) => {
                            let at = Instant::now();
                            while at.elapsed() < t.hold && !stop.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            let killed = if t.kill {
                                let home = state.lock().unwrap().home.clone().unwrap();
                                let pids = helpers(&home);
                                assert_eq!(pids.len(), 1, "one paused helper: {pids:?}");
                                // SAFETY: kill(2) of the helper pid captured above.
                                assert_eq!(unsafe { libc::kill(pids[0] as i32, libc::SIGKILL) }, 0);
                                Some(pids[0])
                            } else {
                                release(&fifo, &stop);
                                None
                            };
                            let mut st = state.lock().unwrap();
                            st.done = Some(Instant::now());
                            st.killed = killed;
                        }
                        None => release(&fifo, &stop),
                    }
                    handled += 1;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }));
    }

    /// The next `point` pause after now is held for `hold`, then let go or
    /// its helper killed.
    pub fn arm(&self, point: &'static str, hold: Duration, kill: bool) {
        let after = Self::pauses(&self.trace()).len();
        let mut st = self.state.lock().unwrap();
        st.target = Some(Target {
            point,
            hold,
            kill,
            after,
        });
        st.hit = None;
        st.done = None;
        st.killed = None;
    }

    pub fn hit(&self) -> Option<Instant> {
        self.state.lock().unwrap().hit
    }

    pub fn done(&self) -> bool {
        self.state.lock().unwrap().done.is_some()
    }

    pub fn disarm(&self) -> Option<u32> {
        let mut st = self.state.lock().unwrap();
        st.target = None;
        st.killed
    }
}

impl Drop for Pauser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One byte to the pause FIFO, once a reader has it open.
fn release(fifo: &Path, stop: &AtomicBool) {
    let start = Instant::now();
    while !stop.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(30) {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(mut f) => {
                let _ = f.write_all(b"r");
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// The deck's helper processes running under `home`.
pub fn helpers(home: &Path) -> Vec<u32> {
    let helper = dot_agent_deck::composer_probe::instance::HELPER_NAME;
    let home = format!("HOME={}", home.display());
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cmd = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let argv0 = cmd.split(|c| *c == 0).next().unwrap_or_default();
        if !String::from_utf8_lossy(argv0).ends_with(helper) {
            continue;
        }
        let env = std::fs::read(entry.path().join("environ")).unwrap_or_default();
        if env.split(|c| *c == 0).any(|v| v == home.as_bytes()) {
            pids.push(pid);
        }
    }
    pids
}

/// Return at `at`, to within a millisecond: a trial's keystroke schedule.
pub fn until(at: Instant) {
    while Instant::now() < at {
        std::thread::sleep(Duration::from_millis(1));
    }
}
