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
