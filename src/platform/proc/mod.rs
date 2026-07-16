//! Process lifecycle: agent teardown + daemon-stop termination + orphan
//! watchdog (PRD #42 M1, lifted from `agent_pty.rs`, `build_version_handshake.rs`,
//! and `daemon.rs`).
//!
//! Unix uses POSIX signals: `killpg(SIGTERM/SIGKILL)` to tear down an agent's
//! whole process group, `kill(SIGTERM/SIGKILL)` to stop the daemon by PID, and
//! `getppid` for the (test-only) orphan watchdog. Windows uses Job Objects +
//! `TerminateProcess`/`CTRL_BREAK_EVENT` (compiling stubs at M1 — real behavior
//! is PRD #163).
//!
//! Note: peer-credential PID *discovery* (`SO_PEERCRED`) stays in
//! `daemon_attach` until M2; this module only relocates the kill/teardown
//! helpers.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// Result of delivering the daemon-stop graceful signal to a PID via
/// [`terminate_pid`].
///
/// The distinction matters to
/// [`crate::build_version_handshake::terminate_daemon_graceful`]: `Delivered`
/// means the signal reached a live process that may still be shutting down, so
/// the caller must poll for it to disappear (and possibly escalate to
/// [`force_kill_pid`]); `AlreadyGone` means the target PID no longer existed
/// (`ESRCH` on Unix), so there is nothing to wait for and the caller can report
/// `Stopped` immediately. This mirrors `main`, where an `ESRCH` from the
/// `SIGTERM` `kill(2)` short-circuited straight to
/// `Ok(TerminateOutcome::Stopped)` rather than entering the poll/escalate loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateSignal {
    /// The signal was delivered to a live process; it may still be dying, so
    /// the caller must poll for the process to disappear.
    Delivered,
    /// The target process was already gone when signalled (`ESRCH`) — an
    /// already-gone success that short-circuits the poll/escalate loop.
    AlreadyGone,
}

#[cfg(unix)]
pub use unix::{
    current_ppid, force_kill_child_and_wait, force_kill_pid, send_sigterm_to_child_group,
    terminate_child_with_grace_and_wait, terminate_pid,
};
#[cfg(windows)]
pub use windows::{
    current_ppid, force_kill_child_and_wait, force_kill_pid, send_sigterm_to_child_group,
    terminate_child_with_grace_and_wait, terminate_pid,
};
