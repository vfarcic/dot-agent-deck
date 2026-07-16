//! Windows process lifecycle (PRD #42 M1 compiling stubs; real behavior is
//! PRD #163).
//!
//! The Unix `killpg` process-group teardown maps to a **Job Object** per agent
//! (`AssignProcessToJobObject` at spawn, `TerminateJobObject` to reap the agent
//! and all descendants atomically). The SIGTERM-grace window maps to a
//! best-effort `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, …)` to a
//! `CREATE_NEW_PROCESS_GROUP` child. Daemon-stop by PID maps to
//! `TerminateProcess(OpenProcess(pid), 1)`. None of that is wired yet; at M1
//! the agent-teardown helpers are best-effort single-child kills and the
//! by-PID helpers return `Unsupported`.

use std::time::Duration;

/// Best-effort single-child kill + reap. #163 replaces this with
/// `TerminateJobObject` so descendants are reaped too.
pub fn force_kill_child_and_wait(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Best-effort graceful teardown. At M1 the grace window is not honored
/// (Windows console apps honor `CTRL_BREAK_EVENT` inconsistently — #163);
/// falls straight through to a single-child kill + reap.
pub fn terminate_child_with_grace_and_wait(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    _grace: Duration,
) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Best-effort SIGTERM-equivalent for the daemon-wide shutdown phase. At M1 a
/// single-child kill; #163 routes this through `CTRL_BREAK_EVENT`.
pub fn send_sigterm_to_child_group(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    _phase: &'static str,
) {
    let _ = child.kill();
}

/// Daemon-stop graceful signal by PID — `TerminateProcess` skeleton (#163).
/// The real implementation will distinguish a delivered termination from an
/// already-exited target (the Windows analogue of `ESRCH`) via
/// [`super::TerminateSignal`]; the stub only ever errors.
pub fn terminate_pid(_pid: u32) -> std::io::Result<super::TerminateSignal> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "daemon termination by PID is not yet implemented on Windows (PRD #163)",
    ))
}

/// Daemon-stop force escalation by PID — `TerminateProcess` skeleton (#163).
pub fn force_kill_pid(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "daemon force-termination by PID is not yet implemented on Windows (PRD #163)",
    ))
}

/// The orphan watchdog has no Windows analogue (`getppid`/pid-1-reparent is
/// POSIX) and is test-only / OFF in production. Returns a sentinel so
/// `should_exit_orphaned` never triggers on the stub.
pub fn current_ppid() -> i32 {
    0
}
