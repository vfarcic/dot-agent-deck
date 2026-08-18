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
//!   branches on `cfg` and the wire format stays identical everywhere. Because
//!   that escalation names a *pid* long after the target was probed,
//!   [`pin_process`] lets the caller hold the target's identity across the whole
//!   sequence — a real handle on Windows, a documented no-op on Unix.
//!
//! Note: peer-credential PID *discovery* (`SO_PEERCRED`) lives in
//! [`crate::platform::peercred`]; this module only owns kill/teardown.
//!
//! PRD #386 M1/M2 adds a second, read-only concern alongside teardown: the
//! **descendant-process scan** the shell-activity signal is built on. Its
//! cross-platform half — [`ProcessInfo`], [`descendants`] and the structural
//! [`descendant_shell_activity`] discriminator — lives in `scan.rs` and
//! compiles everywhere; only the act of sampling the machine is platform code
//! (`ps` on Unix, an unconditional `None` on Windows, matching
//! [`foreground_pgid`]'s existing contract). It comes in two flavours:
//! [`process_table`], which blocks the calling thread, and
//! [`process_table_async`], which the daemon's poll loop uses so a wedged `ps`
//! cannot stall a Tokio worker and so a [`tokio::time::timeout`] can genuinely
//! kill it (issue #429).

mod scan;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub use scan::{
    CLAUDE_BASH_TOOL_SHAPE, MEASURED_SHELL_TOOL_SHAPES, ProcessInfo, ShellToolShape,
    descendant_shell_activity, descendants,
};

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
    AgentProcessGroup, PinnedProcess, current_ppid, force_kill_child_and_wait,
    force_kill_child_group, force_kill_pid, foreground_pgid, pin_process, process_table,
    process_table_async, send_sigterm_to_child_group, terminate_child_with_grace_and_wait,
    terminate_pid,
};
#[cfg(windows)]
pub use windows::{
    AgentProcessGroup, PinnedProcess, current_ppid, force_kill_child_and_wait,
    force_kill_child_group, force_kill_pid, foreground_pgid, pin_process, process_table,
    process_table_async, send_sigterm_to_child_group, terminate_child_with_grace_and_wait,
    terminate_pid,
};

/// A [`portable_pty::Child`] stand-in over a real [`std::process::Child`], so the
/// teardown state machines in both backends can be driven against **real**
/// processes without a PTY (PRD #163 review).
///
/// Lives here, not in a backend, because both the shared cross-platform test
/// below and the Windows backend's descendant-leak test need it. Only the three
/// methods the teardown path actually calls do anything; `clone_killer` is not on
/// that path and returns a no-op killer rather than pretending to duplicate the
/// handle.
#[cfg(test)]
pub(crate) mod test_child {
    /// Wraps a real OS child. `Debug` is required by the `portable_pty` traits.
    #[derive(Debug)]
    pub(crate) struct StdChild(pub(crate) std::process::Child);

    fn to_pty_status(status: std::process::ExitStatus) -> portable_pty::ExitStatus {
        portable_pty::ExitStatus::with_exit_code(status.code().unwrap_or(0) as u32)
    }

    /// Stand-in for the killer handle `clone_killer` is contractually required to
    /// produce. Nothing in the teardown path clones a killer, so it is never used.
    #[derive(Debug)]
    struct NoopKiller;

    impl portable_pty::ChildKiller for NoopKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(NoopKiller)
        }
    }

    impl portable_pty::ChildKiller for StdChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.0.kill()
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(NoopKiller)
        }
    }

    impl portable_pty::Child for StdChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(self.0.try_wait()?.map(to_pty_status))
        }
        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            self.0.wait().map(to_pty_status)
        }
        fn process_id(&self) -> Option<u32> {
            Some(self.0.id())
        }
        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }
}

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

    /// PRD #163 review — the teardown contract both backends owe the caller for
    /// the case `close_agent` hits whenever the user closes a pane whose agent
    /// quit on its own: a child that exits **inside** the grace window must be
    /// torn down promptly rather than costing the whole window, and must not panic
    /// on a process there is nothing left to signal.
    ///
    /// Un-gated on purpose — it runs on the Linux and the `windows-latest`
    /// `cargo nextest run` jobs, driving each backend's real state machine against
    /// a real OS process. The Windows-specific half of the same fix (the job is
    /// still terminated, so descendants of an exited child are reaped instead of
    /// leaking) needs a second process in the job and lives in that backend's
    /// tests.
    #[test]
    fn terminating_a_child_that_exits_during_the_grace_window_is_prompt() {
        // A process that exits immediately, on either platform. Deliberately NOT
        // reaped here: production reaps through the teardown call itself, so the
        // pid must still be the child's (a zombie on Unix, a live process object
        // on Windows) when the state machine runs.
        let program = if cfg!(windows) { "cmd" } else { "true" };
        let args: &[&str] = if cfg!(windows) {
            &["/C", "exit", "0"]
        } else {
            &[]
        };
        let spawned = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a short-lived helper");

        let group = AgentProcessGroup::adopt(Some(spawned.id()));
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(super::test_child::StdChild(spawned));

        let grace = std::time::Duration::from_secs(3);
        let started = std::time::Instant::now();
        terminate_child_with_grace_and_wait(&mut child, grace, &group);
        assert!(
            started.elapsed() < grace,
            "an already-exited child must not cost the full grace window (took {:?})",
            started.elapsed()
        );
    }
}
