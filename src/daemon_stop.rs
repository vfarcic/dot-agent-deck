//! PRD #103 Phase 3 — `dot-agent-deck daemon stop` / `daemon restart`.
//!
//! Documented, non-`kill -9` way to recycle the local daemon. Three
//! load-bearing properties:
//!
//! 1. **PID discovery via `peer_pid()`** ([`crate::platform::peercred::peer_pid`])
//!    — `SO_PEERCRED` / `LOCAL_PEERPID` on the connected attach socket.
//!    No protocol surface required, so this works against *any* daemon
//!    version including the v0.24.x daemon that motivated this PRD.
//! 2. **Agent-liveness check via existing `ListAgents`** — predates
//!    every change in this PRD, so a stale daemon answers normally.
//!    Refuse without `--force` when ≥1 agent is alive (data-loss
//!    guard). Issue #770 added a SECOND guard on the same reply: refuse
//!    when the daemon holds live orchestration ROLE registrations, which
//!    are in-memory-only state a stop destroys for good. Both ride
//!    `ListAgents`, so an older daemon that cannot report roles still
//!    answers the agent half exactly as before.
//! 3. **Graceful + poll + optional force escalation** —
//!    [`crate::build_version_handshake::terminate_daemon_graceful`]
//!    handles both stages; this module just decides whether to pass
//!    `force_kill_after = Some(...)` based on the `--force` flag. Which
//!    mechanism each stage uses is the platform's business (PRD #163 M3,
//!    [`crate::platform::proc::GRACEFUL_STOP_DELIVERY`]): `SIGTERM` then
//!    `SIGKILL` on Unix; the shared `KIND_SHUTDOWN`/ACK frame then
//!    `TerminateProcess` on Windows, which has no signals.
//!
//! `restart` is implemented as a thin wrapper: it runs `stop` and
//! returns. The next TUI invocation lazy-spawns a fresh daemon per
//! PRD #93.

use std::io;
use std::path::Path;
use std::time::Duration;

use tracing::debug;

use crate::build_version_handshake::{HandshakeError, TerminateOutcome, terminate_daemon_graceful};
use crate::daemon_client::issue_command;
use crate::daemon_protocol::AttachRequest;
use crate::platform::ipc::IpcStream;
use crate::platform::peercred::peer_pid;
use crate::state::OrchestrationRoleRecord;

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
    /// Issue #770: the daemon holds live orchestration ROLE registrations and
    /// `--force` was not passed. Distinct from [`Self::LiveAgents`] and checked
    /// first, because the consequence is different in kind: the agents guard is
    /// about processes this daemon would take down with it, while this is about
    /// state that exists NOWHERE ELSE — an agent that survives the stop keeps
    /// running and keeps posting hooks, and is simply never able to delegate
    /// again.
    LiveOrchestrations { roles: Vec<OrchestrationRoleRecord> },
    /// SIGTERM (and SIGKILL if `--force`) failed to take the daemon
    /// down within the configured timeouts.
    TimedOut { pid: u32 },
    /// The termination call itself failed — `libc::kill` on Unix (typically
    /// ESRCH if the daemon already exited between probe and signal),
    /// `OpenProcess`/`TerminateProcess` on Windows.
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
            Self::LiveOrchestrations { roles } => {
                write!(
                    f,
                    "daemon holds {n} live orchestration role(s); stopping it orphans them \
                     permanently — pass --force to stop anyway",
                    n = roles.len()
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
///    version because no protocol bytes are exchanged. Then
///    [`crate::platform::proc::pin_process`] on that pid, while the
///    authenticated connection is still open, so nothing else can
///    acquire the pid before the escalation in step 4 names it.
/// 3. Send `ListAgents`. Two refusals come off that one reply, and
///    neither signals anything — the user must resolve it or pass
///    `--force` consciously:
///    - Issue #770: the daemon reports ≥1 live orchestration ROLE →
///      [`StopError::LiveOrchestrations`]. Checked FIRST because it is
///      the more consequential of the two and its message says why: the
///      role maps exist only in this process, so a survivor of the stop
///      is orphaned permanently rather than merely killed. A daemon
///      predating the field reports `None` and this check is skipped.
///    - ≥1 managed agent alive → [`StopError::LiveAgents`], unchanged.
/// 4. `terminate_daemon_graceful(pid, attach_path, 5s, force.then(|| 1s))`:
///    - SIGTERM, poll up to 5 s for the daemon to stop accepting connects.
///    - On timeout with `force`: SIGKILL, poll up to 1 s.
///    - On timeout without `force`: surface as `TimedOut`.
pub async fn run_daemon_stop(attach_path: &Path, force: bool) -> Result<StopOutcome, StopError> {
    let stream = match IpcStream::connect(attach_path).await {
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

    // PRD #163 review (Greptile P1): from here on the daemon is identified only by
    // this number, and the pipe/socket that authenticated it is about to be
    // dropped. That is fine on Unix (nothing can be pinned there anyway — see
    // `platform::proc::pin_process`) but not on Windows, where the escalation ends
    // in `TerminateProcess(OpenProcess(pid))` *after* deliberately waiting for the
    // daemon to exit — precisely when the pid becomes available for reuse. So pin
    // the identity now, while the connection still proves whose pid this is, and
    // hold it past the last termination call below. The agent teardown path gets
    // this for free from the `Child` handle its caller keeps; `daemon stop` had no
    // such anchor.
    let pinned = match crate::platform::proc::pin_process(pid) {
        Ok(Some(pinned)) => pinned,
        // Gone between the connect and the pin. Nothing to terminate, and
        // terminating an unpinned pid is the bug this guards, so report the stop
        // as done rather than escalating blind. Same answer the escalation state
        // machine gives for its own `AlreadyGone` arm.
        Ok(None) => {
            debug!(
                target: "daemon_stop",
                pid,
                "daemon exited between connect and pid pin; reporting stopped"
            );
            return Ok(StopOutcome::Stopped { pid });
        }
        Err(e) => return Err(StopError::KillFailed(e)),
    };

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
    // Issue #770. Absent (`None`) is a daemon that predates the field, NOT a
    // daemon with no roles — `unwrap_or_default()` collapses the two on purpose,
    // because the only safe reading of "cannot answer" here is the pre-#770
    // behaviour: skip this guard and let the agent guard below decide.
    let orchestration_roles = resp.orchestration_roles.unwrap_or_default();
    drop(rd);
    drop(wr);

    debug!(
        target: "daemon_stop",
        pid,
        agent_count = agent_ids.len(),
        orchestration_role_count = orchestration_roles.len(),
        force,
        "daemon_stop: probed daemon, deciding policy"
    );

    if let Some(refusal) = stop_refusal(&orchestration_roles, &agent_ids, force) {
        return Err(refusal);
    }

    let force_window = if force {
        Some(STOP_FORCE_KILL_TIMEOUT)
    } else {
        None
    };
    let outcome =
        terminate_daemon_graceful(pid, attach_path, STOP_GRACE_TIMEOUT, force_window).await;
    // Explicit, and load-bearing: the pin must outlive the *last* by-pid call
    // inside `terminate_daemon_graceful`, so it is released here and not a line
    // earlier. (Dropping it at end of scope would be correct too; naming the drop
    // stops a future refactor from shortening its life by accident.)
    drop(pinned);
    match outcome {
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

/// The data-loss policy, as a pure decision over what the daemon just reported.
///
/// Extracted from [`run_daemon_stop`] so the force matrix can be tested without
/// a socket: every path through that function ends in `terminate_daemon_graceful`,
/// which SIGTERMs the pid on the other end of the attach socket — and for an
/// in-process test harness that pid is the test runner itself.
///
/// Order matters. Issue #770's orchestration guard is checked FIRST because,
/// when both apply, its message is the one that tells the operator something
/// they do not already know: `--force` on the agents guard means "these
/// processes die", which an operator recycling a daemon has usually accepted,
/// while `--force` here also means "any agent that SURVIVES is stranded". Two
/// refusals cannot both be printed, so the more consequential one wins.
///
/// `None` means nothing stands in the way of the stop.
pub fn stop_refusal(
    orchestration_roles: &[OrchestrationRoleRecord],
    agent_ids: &[String],
    force: bool,
) -> Option<StopError> {
    if force {
        return None;
    }
    if !orchestration_roles.is_empty() {
        return Some(StopError::LiveOrchestrations {
            roles: orchestration_roles.to_vec(),
        });
    }
    if !agent_ids.is_empty() {
        return Some(StopError::LiveAgents {
            ids: agent_ids.to_vec(),
        });
    }
    None
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

/// Issue #770: render the multi-line `LiveOrchestrations` refusal used by both
/// `daemon stop` and `daemon restart`.
///
/// Shaped like [`format_live_agents_refusal`] — header, indented list, recovery
/// hint — but the header says what is actually at stake, because the agent
/// refusal's phrasing taught the wrong lesson here. "Pass --force to terminate
/// them" reads as "these processes will be killed", which for an orchestration
/// role is the *optimistic* outcome: an agent that has detached from the PTY it
/// was born under survives the stop, keeps posting hook events, keeps looking
/// healthy on its card — and can never delegate again, because the role map it
/// was registered in lived only in the daemon that just exited.
///
/// Trailing newline included so callers can `eprint!` the result directly.
pub fn format_live_orchestrations_refusal(roles: &[OrchestrationRoleRecord]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "daemon holds {n} live orchestration role(s):",
        n = roles.len()
    );
    for role in roles {
        let marker = if role.is_orchestrator {
            " (orchestrator)"
        } else {
            ""
        };
        let orchestration = if role.orchestration.is_empty() {
            String::new()
        } else {
            format!(" [{}]", role.orchestration)
        };
        let _ = writeln!(
            out,
            "  {} {}{marker}{orchestration}",
            role.pane_id, role.role
        );
    }
    let _ = writeln!(
        out,
        "stopping the daemon deletes these registrations for good — they are held in \
         memory only, so any agent that survives the restart keeps running but can never \
         delegate again"
    );
    let _ = writeln!(out, "pass --force to stop anyway");
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

    fn role(pane_id: &str, name: &str, is_orchestrator: bool) -> OrchestrationRoleRecord {
        OrchestrationRoleRecord {
            pane_id: pane_id.to_string(),
            role: name.to_string(),
            orchestration: "issue-work".to_string(),
            is_orchestrator,
        }
    }

    /// Issue #770: the force matrix, over the pure policy so no socket (and no
    /// SIGTERM at our own pid) is involved. Orchestration roles refuse ahead of
    /// managed agents when both are present, and `--force` clears both.
    #[test]
    fn stop_refusal_covers_the_force_matrix() {
        let roles = vec![role("sched-issue-work-1-r0", "orchestrator", true)];
        let agents = vec!["7".to_string()];

        assert!(
            matches!(
                stop_refusal(&roles, &[], false),
                Some(StopError::LiveOrchestrations { .. })
            ),
            "live roles alone must refuse"
        );
        assert!(
            matches!(
                stop_refusal(&[], &agents, false),
                Some(StopError::LiveAgents { .. })
            ),
            "the pre-existing agent guard is unchanged"
        );
        assert!(
            matches!(
                stop_refusal(&roles, &agents, false),
                Some(StopError::LiveOrchestrations { .. })
            ),
            "with both present the orchestration refusal wins — it is the one \
             that says the survivors are stranded, not merely killed"
        );
        assert!(
            stop_refusal(&roles, &agents, true).is_none(),
            "--force must clear both guards"
        );
        assert!(
            stop_refusal(&[], &[], false).is_none(),
            "an idle daemon stops with no refusal"
        );
    }

    /// Issue #770: the refusal a human actually reads. It must name every role
    /// (so the operator can see WHAT is at stake, not just how many), mark the
    /// orchestrator, say that the loss is permanent rather than a kill, and
    /// point at `--force`.
    #[test]
    fn live_orchestrations_refusal_names_the_roles_and_the_permanence() {
        let roles = vec![
            role("sched-issue-work-1-r0", "orchestrator", true),
            role("sched-issue-work-1-r1", "coder", false),
        ];
        let msg = format_live_orchestrations_refusal(&roles);
        assert!(
            msg.contains("2 live orchestration role(s)"),
            "must include the count, got: {msg:?}"
        );
        for expected in [
            "sched-issue-work-1-r0",
            "orchestrator",
            "(orchestrator)",
            "sched-issue-work-1-r1",
            "coder",
            "[issue-work]",
        ] {
            assert!(
                msg.contains(expected),
                "refusal must name {expected:?}, got: {msg:?}"
            );
        }
        assert!(
            msg.contains("can never delegate again"),
            "the refusal must say the consequence is permanent loss of \
             delegation, not just termination, got: {msg:?}"
        );
        assert!(
            msg.contains("--force"),
            "must point at the override, got: {msg:?}"
        );
        assert!(
            msg.ends_with('\n'),
            "callers eprint! this directly, so it must end in a newline"
        );
        // And the single-line `Display` form used by the generic error arm.
        assert!(
            StopError::LiveOrchestrations { roles }
                .to_string()
                .contains("--force")
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
