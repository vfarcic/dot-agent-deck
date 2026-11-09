//! A [`portable_pty::Child`] stand-in whose *reap* is under a test's control
//! while its *kill* is real — the fixture for every "a child that never
//! finishes exiting" regression (issues #581, #1118 and #1677).
//!
//! No real process can be coaxed into the shape those issues are about — a
//! child wedged in uninterruptible kernel I/O (D state), which SIGKILL cannot
//! dislodge until the I/O completes — because a real child always returns from
//! `wait` once SIGKILL lands. So the wedge is modelled here and only here:
//! everything the teardown paths *do* (the signals they send, the pids they
//! send them to) stays production code, and on Unix the fixture can carry a
//! real pid so the production `killpg` lands on a real process group.
//!
//! **This module exists because the fixture had to leave `agent_pty.rs`'s own
//! `#[cfg(test)] mod tests` to be reused** — the same decision `test_pty_wait`
//! records (issue #1132). Issue #1677's regression tests live in
//! `daemon_protocol` (the `StopAgent` sequence) and `close_agents` (the `close`
//! verb), which drive the same wedged shape through those layers and could not
//! reach a fixture private to `agent_pty`'s test module. The rejected
//! alternative — copying the child into each test module — would leave three
//! divergent copies of a fixture whose `wait`-never-returns flag is exactly the
//! subtlety the tests turn on.
//!
//! Test-only and never part of the shipped library: `lib.rs` declares it under
//! `#[cfg(test)]`, so it is compiled for the lib target's own unit tests and
//! nowhere else.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A latch the test flips to let a deliberately-wedged reap finally complete,
/// so a wedged shutdown thread can always be joined — or, for issue #1677, so
/// a test can prove the stop path gave up *before* the child ever answered.
#[derive(Debug, Default)]
pub(crate) struct WedgeGate {
    released: Mutex<bool>,
    wake: std::sync::Condvar,
}

impl WedgeGate {
    pub(crate) fn is_released(&self) -> bool {
        *self.released.lock().unwrap()
    }

    pub(crate) fn block_until_released(&self) {
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }

    pub(crate) fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

/// A [`portable_pty::Child`] whose *reap* is under the test's control while
/// its *kill* is real.
///
/// Until [`WedgeGate::release`], `try_wait` answers `Ok(None)` ("still
/// running") forever and `wait` blocks — issue #1677's shape, the child that
/// never finishes exiting. With `wait_never_returns` set, the blocking `wait`
/// never returns *at all*, however the gate stands — the unbounded-`wait` half
/// of the same wedge, which only a `try_wait`-based reap can get past.
#[derive(Debug)]
pub(crate) struct WedgedChild {
    /// Reported to the teardown path as this child's pid. `Some` makes the
    /// production `killpg(SIGKILL)` land on a real process group; `None`
    /// drives the documented pid-unavailable fallback, which is
    /// `Child::kill` on both backends and therefore observable on Windows
    /// too.
    pub(crate) pid: Option<u32>,
    /// How many times the teardown path issued a kill through that
    /// fallback. A count, not a flag: a SIGTERM *ask* reaches the same
    /// fallback, so only a kill *beyond* [`PHASE_ONE_FALLBACK_KILLS`] is a
    /// later phase's. (The first draft of the #581 test asserted a flag and
    /// passed on the unfixed code — phase 1 had already set it for every
    /// agent.)
    pub(crate) kills: Arc<AtomicUsize>,
    /// Set when the child was actually reaped (by either `wait` or a
    /// `try_wait` that reported an exit). A fix must not buy signal
    /// independence by dropping the reap.
    pub(crate) reaped: Arc<AtomicBool>,
    /// Until released, `wait` blocks forever and `try_wait` keeps saying
    /// "still running".
    pub(crate) gate: Arc<WedgeGate>,
    /// When set, the blocking `wait` never returns *at all*, however the
    /// gate stands — the unbounded-`wait` half of the same wedge, which
    /// only a `try_wait`-based reap can get past.
    pub(crate) wait_never_returns: bool,
}

impl WedgedChild {
    pub(crate) fn new(pid: Option<u32>, gate: Arc<WedgeGate>) -> Self {
        Self {
            pid,
            kills: Arc::new(AtomicUsize::new(0)),
            reaped: Arc::new(AtomicBool::new(false)),
            gate,
            wait_never_returns: false,
        }
    }
}

impl portable_pty::ChildKiller for WedgedChild {
    fn kill(&mut self) -> std::io::Result<()> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        unreachable!("no teardown path clones a killer")
    }
}

impl portable_pty::Child for WedgedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        if !self.gate.is_released() {
            return Ok(None);
        }
        self.reaped.store(true, Ordering::SeqCst);
        Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        if self.wait_never_returns {
            // `park` may wake spuriously, so loop: this call must never
            // return, which is the whole point of the flag.
            loop {
                std::thread::park();
            }
        }
        self.gate.block_until_released();
        self.reaped.store(true, Ordering::SeqCst);
        Ok(portable_pty::ExitStatus::with_exit_code(0))
    }

    fn process_id(&self) -> Option<u32> {
        self.pid
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

/// Poll `done` until it holds or `budget` elapses. Returns what it last saw.
pub(crate) fn holds_within(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// How long a phase-3 kill is given to reach every agent before a test calls
/// it starved, and the slack issue #1677's bounds are asserted with. Generous
/// — it is only ever paid on failure.
pub(crate) const WEDGE_TEST_BUDGET: Duration = Duration::from_secs(10);

/// Kills that phase 1's SIGTERM *ask* contributes per agent for a pid-less
/// child, before phase 3 is reached at all: on Unix `killpg` needs a pid and
/// so takes the `Child::kill` fallback, while on Windows
/// `GenerateConsoleCtrlEvent` skips a pid-less child outright. Phase 3's
/// force-kill is the one *on top of* this baseline, so it is the only thing
/// a strictly-greater count can be.
pub(crate) const PHASE_ONE_FALLBACK_KILLS: usize = if cfg!(unix) { 1 } else { 0 };
