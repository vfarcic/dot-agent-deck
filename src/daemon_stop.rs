//! PRD #103 Phase 3 — `dot-agent-deck daemon stop` / `daemon restart`.
//!
//! Documented, non-`kill -9` way to recycle the local daemon. Three
//! load-bearing properties:
//!
//! 1. **PID discovery via `peer_pid()`** ([`crate::daemon_attach::peer_pid`])
//!    — `SO_PEERCRED` / `LOCAL_PEERPID` on the connected attach socket.
//!    No protocol surface required, so this works against *any* daemon
//!    version including the v0.24.x daemon that motivated this PRD.
//! 2. **Agent-liveness check via existing `ListAgents`** — predates
//!    every change in this PRD, so a stale daemon answers normally.
//!    Refuse without `--force` when ≥1 agent is alive (data-loss
//!    guard).
//! 3. **SIGTERM + poll + optional SIGKILL escalation** —
//!    [`crate::build_version_handshake::terminate_daemon_graceful`]
//!    handles both stages; this module just decides whether to pass
//!    `force_kill_after = Some(...)` based on the `--force` flag.
//!
//! `restart` is implemented as a thin wrapper: it runs `stop` and
//! returns. The next TUI invocation lazy-spawns a fresh daemon per
//! PRD #93.

use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;
use tracing::debug;

use crate::build_version_handshake::{HandshakeError, TerminateOutcome, terminate_daemon_graceful};
use crate::daemon_attach::peer_pid;
use crate::daemon_client::issue_command;
use crate::daemon_protocol::AttachRequest;

/// SIGTERM grace before reporting "daemon did not exit cleanly". PRD #103
/// M3.2: 5 s.
pub const STOP_GRACE_TIMEOUT: Duration = Duration::from_secs(5);

/// SIGKILL grace after SIGTERM timed out (only used with `--force`).
/// PRD #103 M3.2: ~1 s.
pub const STOP_FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(1);

/// Successful outcomes. `Stopped` is the normal case; `ForceKilled` only
/// reachable with `--force` after SIGTERM timed out;
/// `NoDaemonRunning` is the idempotent missing-socket case (exit 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    NoDaemonRunning,
    Stopped { pid: u32 },
    ForceKilled { pid: u32 },
}

#[derive(Debug)]
pub enum StopError {
    /// `UnixStream::connect` failed in a non-idempotent way (i.e. not
    /// ECONNREFUSED / ENOENT — those are folded into
    /// [`StopOutcome::NoDaemonRunning`]).
    ConnectFailed(io::Error),
    /// `peer_pid` syscall failed. macOS/Linux both support it, so this
    /// is exceptional.
    PeerPid(io::Error),
    /// `ListAgents` round-trip failed (transport or daemon-level error).
    ListAgents(String),
    /// Daemon is hosting `ids` and `--force` was not passed.
    LiveAgents { ids: Vec<String> },
    /// SIGTERM (and SIGKILL if `--force`) failed to take the daemon
    /// down within the configured timeouts.
    TimedOut { pid: u32 },
    /// `libc::kill` itself failed (typically ESRCH if the daemon already
    /// exited between probe and signal).
    KillFailed(io::Error),
}

impl std::fmt::Display for StopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectFailed(e) => write!(f, "failed to connect to daemon: {e}"),
            Self::PeerPid(e) => write!(f, "failed to read daemon's peer PID: {e}"),
            Self::ListAgents(msg) => write!(f, "list-agents failed: {msg}"),
            Self::LiveAgents { ids } => {
                write!(
                    f,
                    "daemon has {n} managed agent(s) running; pass --force to terminate them",
                    n = ids.len()
                )
            }
            Self::TimedOut { pid } => {
                write!(
                    f,
                    "daemon (pid {pid}) did not exit cleanly within {}s; re-run with --force to SIGKILL",
                    STOP_GRACE_TIMEOUT.as_secs()
                )
            }
            Self::KillFailed(e) => write!(f, "kill syscall failed: {e}"),
        }
    }
}

impl std::error::Error for StopError {}

/// Drive the `daemon stop` flow against `attach_path`. Reusable from
/// `cmd_daemon_stop`, `cmd_daemon_restart`, and the integration test
/// suite (`tests/daemon_stop.rs`).
///
/// Step-by-step:
/// 1. `connect`. `ECONNREFUSED` / `ENOENT` fold into
///    [`StopOutcome::NoDaemonRunning`] — covers both "socket file
///    missing" (ENOENT) and "stale socket inode after a crash"
///    (ECONNREFUSED). No separate `exists()` pre-check: that opens a
///    TOCTOU window where the daemon could exit (or be created)
///    between the file probe and the connect, and `connect` itself is
///    the authoritative liveness signal.
/// 2. `peer_pid(&stream)` — load-bearing: works against any daemon
///    version because no protocol bytes are exchanged.
/// 3. Send `ListAgents`. If ≥1 alive and `!force`, return
///    [`StopError::LiveAgents`] *without* signaling — the user must
///    detach the agents or pass `--force` consciously.
/// 4. `terminate_daemon_graceful(pid, attach_path, 5s, force.then(|| 1s))`:
///    - SIGTERM, poll up to 5 s for the daemon to stop accepting connects.
///    - On timeout with `force`: SIGKILL, poll up to 1 s.
///    - On timeout without `force`: surface as `TimedOut`.
pub async fn run_daemon_stop(attach_path: &Path, force: bool) -> Result<StopOutcome, StopError> {
    let stream = match UnixStream::connect(attach_path).await {
        Ok(s) => s,
        Err(e)
            if e.kind() == io::ErrorKind::ConnectionRefused
                || e.kind() == io::ErrorKind::NotFound =>
        {
            // ENOENT — socket file is gone; the daemon never started
            // or its cleanup unlinked the inode.
            // ECONNREFUSED — stale socket inode after a crash / kill
            // -9 / host reboot.
            // Both are "no daemon" per the PRD's recovery contract;
            // idempotent exit 0. A subsequent `daemon serve`
            // (lazy-spawn or explicit) will unlink and rebind via the
            // existing probe-remove-bind path under flock.
            debug!(
                target: "daemon_stop",
                path = %attach_path.display(),
                err = %e,
                "no daemon running (connect failed)"
            );
            return Ok(StopOutcome::NoDaemonRunning);
        }
        Err(e) => return Err(StopError::ConnectFailed(e)),
    };

    let pid = peer_pid(&stream).map_err(StopError::PeerPid)?;
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue_command(&mut rd, &mut wr, &AttachRequest::ListAgents)
        .await
        .map_err(|e| StopError::ListAgents(e.to_string()))?;
    if !resp.ok {
        return Err(StopError::ListAgents(resp.error.unwrap_or_default()));
    }
    // Prefer the typed agent_records (carries pane_id_env, display_name,
    // etc.) but fall back to the legacy `agents` array of ids for
    // forward-compat with daemons that don't emit agent_records.
    let agent_ids: Vec<String> = resp
        .agent_records
        .map(|rs| rs.into_iter().map(|r| r.id).collect::<Vec<_>>())
        .or(resp.agents)
        .unwrap_or_default();
    drop(rd);
    drop(wr);

    debug!(
        target: "daemon_stop",
        pid,
        agent_count = agent_ids.len(),
        force,
        "daemon_stop: probed daemon, deciding policy"
    );

    if !agent_ids.is_empty() && !force {
        return Err(StopError::LiveAgents { ids: agent_ids });
    }

    let force_window = if force {
        Some(STOP_FORCE_KILL_TIMEOUT)
    } else {
        None
    };
    match terminate_daemon_graceful(pid, attach_path, STOP_GRACE_TIMEOUT, force_window).await {
        Ok(TerminateOutcome::Stopped) => Ok(StopOutcome::Stopped { pid }),
        Ok(TerminateOutcome::Killed) => Ok(StopOutcome::ForceKilled { pid }),
        Err(HandshakeError::TerminateTimedOut) => Err(StopError::TimedOut { pid }),
        Err(HandshakeError::TerminateFailed(e)) => Err(StopError::KillFailed(e)),
        // The remaining HandshakeError variants are produced only by
        // the Phase 2 probe/prompt paths in build_version_handshake.rs.
        // terminate_daemon_graceful itself cannot surface them; fold
        // them into KillFailed for forward-compat if that ever changes.
        Err(other) => Err(StopError::KillFailed(io::Error::other(other.to_string()))),
    }
}

/// Render the multi-line `LiveAgents` refusal message used by both
/// `daemon stop` and `daemon restart` CLI handlers (PRD #103 M3.2/M3.3).
/// Centralised so the two CLI sites can't drift — the user-visible
/// header (`daemon has N managed agent(s) running`), the indented agent
/// list, and the recovery hint (`pass --force to terminate them`) are
/// all pinned by the M4.x integration tests via `live_agents_refusal()`.
///
/// Trailing newline included so callers can `eprint!` the result
/// directly without an extra `println!`.
pub fn format_live_agents_refusal(ids: &[String]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "daemon has {n} managed agent(s) running:",
        n = ids.len()
    );
    for id in ids {
        let _ = writeln!(out, "  {id}");
    }
    let _ = writeln!(out, "pass --force to terminate them");
    out
}

/// `daemon restart`: PRD #103 M3.3 — same logic as `daemon stop`. The
/// next TUI invocation lazy-spawns a fresh daemon (PRD #93). This is
/// intentionally a thin wrapper rather than a stop-then-spawn flow,
/// because spawning a daemon out of `daemon restart` would either
/// race the next TUI's `ensure_external_daemon_or_die` (two daemons
/// trying to bind under flock) or require duplicating the lazy-spawn
/// machinery here.
pub async fn run_daemon_restart(attach_path: &Path, force: bool) -> Result<StopOutcome, StopError> {
    run_daemon_stop(attach_path, force).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_agents_error_message_mentions_force_flag() {
        // Pin the user-visible refusal message so the M4.x tests
        // and docs can rely on it. The exact phrasing surfaces in
        // run_daemon_stop_cli's eprintln; keep this in sync.
        let err = StopError::LiveAgents {
            ids: vec!["a".into(), "b".into()],
        };
        let msg = err.to_string();
        assert!(
            msg.contains("--force"),
            "live-agents refusal must mention --force, got: {msg:?}"
        );
        assert!(
            msg.contains("2 managed agent(s) running"),
            "live-agents refusal must include the count, got: {msg:?}"
        );
    }

    #[test]
    fn timed_out_error_message_mentions_force_recovery() {
        let err = StopError::TimedOut { pid: 12345 };
        let msg = err.to_string();
        assert!(
            msg.contains("--force"),
            "TimedOut message must point at --force, got: {msg:?}"
        );
        assert!(
            msg.contains("12345"),
            "TimedOut message must include the daemon PID, got: {msg:?}"
        );
    }
}
