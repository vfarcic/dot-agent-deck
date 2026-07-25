//! Process lifecycle: agent teardown + daemon-stop termination + orphan
//! watchdog (PRD #42 M1, lifted from `agent_pty.rs`, `build_version_handshake.rs`,
//! and `daemon.rs`).
//!
//! Unix uses POSIX signals: `killpg(SIGTERM/SIGKILL)` to tear down an agent's
//! whole process group, `kill(SIGTERM/SIGKILL)` to stop the daemon by PID, and
//! `getppid` for the (test-only) orphan watchdog. Windows (PRD #163 M3) has no
//! signal analogue and splits the two jobs apart:
//!
//! - **Agent teardown** — each agent is adopted into a **Job Object** at spawn
//!   ([`AgentProcessGroup`]) and the whole descendant tree is reaped with
//!   `TerminateJobObject`, the unconditional backstop that stands in for
//!   `killpg(SIGKILL)`. The SIGTERM grace window maps to a best-effort
//!   `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, …)` — **explicitly best-effort**
//!   per the PRD, see the windows backend for why.
//! - **Daemon stop** — the graceful half is not a signal at all but the existing,
//!   cross-platform `KIND_SHUTDOWN`/ACK protocol frame; only the force escalation
//!   is platform code (`TerminateProcess`). Which of the two a platform uses is
//!   declared by [`GRACEFUL_STOP_DELIVERY`] so the shared caller
//!   ([`crate::build_version_handshake::terminate_daemon_graceful`]) never
//!   branches on `cfg` and the wire format stays identical everywhere.
//!
//! Note: peer-credential PID *discovery* (`SO_PEERCRED`) lives in
//! [`crate::platform::peercred`]; this module only owns kill/teardown.

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
/// (`ESRCH` on Unix, `ERROR_INVALID_PARAMETER` from `OpenProcess` — or an
/// already-signalled exit code — on Windows), so there is nothing to wait for
/// and the caller can report `Stopped` immediately. This mirrors `main`, where an
/// `ESRCH` from the `SIGTERM` `kill(2)` short-circuited straight to
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

/// PRD #163 M3 — how the **graceful** half of `daemon stop` reaches the daemon
/// on this platform. The force half is always platform code
/// ([`force_kill_pid`]: `SIGKILL` / `TerminateProcess`); this enum is only about
/// the polite first ask.
///
/// It exists so [`crate::build_version_handshake::terminate_daemon_graceful`] —
/// which owns the shared *graceful → poll → force* escalation state machine, and
/// is the single path both `daemon stop` and the build-mismatch prompt go
/// through — can pick the right first step without a `cfg` branch of its own and
/// without any per-platform wire format. The platform decision lives here, at
/// the platform seam; the protocol stays identical on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GracefulStopDelivery {
    /// Unix: an out-of-band `SIGTERM` delivered by [`terminate_pid`].
    ///
    /// Load-bearing property: **zero protocol bytes are exchanged**, so
    /// `daemon stop` works against *any* daemon version — including the
    /// v0.24.x daemon that predates `KIND_SHUTDOWN` and motivated PRD #103.
    Signal,
    /// Windows: there is no `SIGTERM`. The graceful request is the existing
    /// `KIND_SHUTDOWN`/ACK frame (identical wire on every platform), sent by
    /// the shared caller *before* [`terminate_pid`], which then only classifies
    /// the target as still-alive vs already-gone. `TerminateProcess` remains the
    /// escalation when the daemon does not go away within the grace window.
    ///
    /// The trade-off this makes explicit: unlike [`Signal`](Self::Signal) the
    /// graceful step now needs a daemon that speaks `PROTOCOL_VERSION` ≥ 2.
    /// That is free on Windows — the Windows daemon is unblocked *by* #163, so
    /// no older Windows daemon exists — and a daemon that does not answer just
    /// falls through to the force escalation, exactly like a Unix daemon that
    /// ignores `SIGTERM`.
    ShutdownProtocol,
}

/// Unix delivers the graceful stop as a signal — see
/// [`GracefulStopDelivery::Signal`] for the zero-protocol-bytes property this
/// pins.
#[cfg(unix)]
pub const GRACEFUL_STOP_DELIVERY: GracefulStopDelivery = GracefulStopDelivery::Signal;

/// Windows has no `SIGTERM`, so the graceful stop rides the shared
/// `KIND_SHUTDOWN`/ACK protocol — see [`GracefulStopDelivery::ShutdownProtocol`].
#[cfg(windows)]
pub const GRACEFUL_STOP_DELIVERY: GracefulStopDelivery = GracefulStopDelivery::ShutdownProtocol;

/// Guard a `u32` PID before naming it in a Win32 lifecycle call
/// (`OpenProcess` for the daemon-stop path, `GenerateConsoleCtrlEvent`'s
/// process-group id for the agent grace window). The Windows counterpart of the
/// Unix `checked_signal_pid` / `pid_to_pgid` guards, and it exists for the same
/// reason: **0 is not "no process", it is a wildcard.**
///
/// - `GenerateConsoleCtrlEvent(_, 0)` is documented as "signal every process
///   that shares the caller's console" — the exact `killpg(0, …)` hazard, which
///   for a console-hosted registry would mean signalling the TUI itself.
/// - `OpenProcess(_, _, 0)` names the System Idle Process, never a daemon.
///
/// Unlike the Unix guard there is no overflow arm: a Windows PID *is* a `u32`
/// (`DWORD`), so no value other than 0 changes the call's meaning.
///
/// Compiled on every platform — it is pure data, so the rule stays unit-testable
/// on Linux CI where the `#[cfg(windows)]` callers are absent (the same shape
/// [`crate::platform::lock::spawn_mutex_name`] uses).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn checked_target_pid(pid: u32) -> std::io::Result<u32> {
    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pid is 0; refusing the Win32 call (0 is a wildcard: it broadcasts to every process \
             sharing the console / names the System Idle Process)",
        ));
    }
    Ok(pid)
}

#[cfg(unix)]
pub use unix::{
    AgentProcessGroup, current_ppid, force_kill_child_and_wait, force_kill_pid,
    send_sigterm_to_child_group, terminate_child_with_grace_and_wait, terminate_pid,
};
#[cfg(windows)]
pub use windows::{
    AgentProcessGroup, current_ppid, force_kill_child_and_wait, force_kill_pid,
    send_sigterm_to_child_group, terminate_child_with_grace_and_wait, terminate_pid,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// PRD #163 M3 — the platform seam `daemon stop` is built on. On Unix the
    /// graceful step MUST stay a signal: that is the property that lets
    /// `daemon stop` kill a daemon predating every protocol surface (PRD #103's
    /// whole motivation). A regression to `ShutdownProtocol` here would silently
    /// add a `KIND_SHUTDOWN` round-trip to the Unix path and break that.
    #[test]
    fn graceful_stop_delivery_matches_the_platform_mechanism() {
        #[cfg(unix)]
        assert_eq!(GRACEFUL_STOP_DELIVERY, GracefulStopDelivery::Signal);
        #[cfg(windows)]
        assert_eq!(
            GRACEFUL_STOP_DELIVERY,
            GracefulStopDelivery::ShutdownProtocol
        );
    }

    /// The Win32 pid guard: everything but 0 passes through untouched (a Windows
    /// PID is a full `u32`, so there is no overflow arm to check).
    #[test]
    fn checked_target_pid_accepts_every_nonzero_pid() {
        assert_eq!(checked_target_pid(1).unwrap(), 1);
        assert_eq!(checked_target_pid(12345).unwrap(), 12345);
        // Above i32::MAX — legal on Windows, unlike the Unix `pid_t` guards.
        assert_eq!(checked_target_pid(u32::MAX).unwrap(), u32::MAX);
    }

    /// 0 is a wildcard on both Win32 calls this guards, so it must be refused
    /// with `InvalidInput` rather than passed through — the Windows analogue of
    /// the `kill(0, …)` / `killpg(0, …)` broadcast hazard.
    #[test]
    fn checked_target_pid_rejects_zero_pid() {
        let err = checked_target_pid(0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(msg.contains("wildcard"), "{msg:?}");
    }
}
