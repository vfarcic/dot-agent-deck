//! PRD #103 Phase 3 — `dot-agent-deck daemon stop` / `daemon restart`.
//!
//! Documented, non-`kill -9` way to recycle the local daemon. Three
//! load-bearing properties:
//!
//! 1. **PID discovery via `peer_pid()`** ([`crate::platform::peercred::peer_pid`])
//!    — `SO_PEERCRED` / `LOCAL_PEERPID` on the connected attach socket.
//!    No protocol surface required, so this works against *any* daemon
//!    version including the v0.24.x daemon that motivated this PRD. It is also
//!    why this whole module takes a
//!    [`LocalEndpoint`](crate::daemon_client::LocalEndpoint) and not a path
//!    (PRD #741 M2): a peer credential names a process on *this* machine, so
//!    over a forwarded socket it would name the tunnel rather than the daemon.
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
use std::time::Duration;

use tracing::{debug, warn};

use crate::agent_pty::{AgentPtyRegistry, AgentRecord};
use crate::build_version_handshake::{HandshakeError, TerminateOutcome, terminate_daemon_graceful};
use crate::daemon_client::{LocalEndpoint, issue_command};
use crate::daemon_protocol::{
    AttachRequest, CAP_STOP_DAEMON, StopDaemonRefusal, StopRefusalReason,
};
use crate::platform::ipc::IpcStream;
use crate::platform::peercred::peer_pid;
use crate::state::{OrchestrationRoleRecord, SharedState};

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
    /// Issue #1049, wire path only: the daemon refused, and said why in a shape
    /// this side did not have to reconstruct.
    ///
    /// Distinct from [`Self::LiveAgents`] / [`Self::LiveOrchestrations`] rather
    /// than folded into them, because the two say different things about where
    /// the verdict came from. Those two are *this* process deciding, from a
    /// `ListAgents` reply it read itself; this one is the daemon's own verdict,
    /// arriving already rendered. Folding them would mean re-deriving a message
    /// the daemon already sent, and a remote client cannot check the daemon's
    /// reasoning anyway — it has no view of the pane list except this reply.
    Refused(StopDaemonRefusal),
    /// Issue #1049, wire path only: the daemon answered `ok = false` with no
    /// structured refusal. Either a genuine unrelated error, or — the case worth
    /// naming — a daemon predating [`AttachRequest::StopDaemon`], whose serde
    /// decode fails with `unknown variant`.
    ///
    /// There is no fallback to offer a remote caller here: the PID path needs a
    /// [`LocalEndpoint`], which is exactly what a remote caller does not have.
    /// Read [`CAP_STOP_DAEMON`] off a `Hello` first if you want to tell the two
    /// apart before spending a round trip on it.
    WireRejected(String),
    /// Issue #1049, wire path only: the daemon accepted the connection but never
    /// answered within [`WIRE_STOP_REQUEST_TIMEOUT`].
    ///
    /// Its own variant because the recovery differs from every other error here:
    /// this side does **not** know whether the daemon saw the request, so the
    /// honest report is "unknown", not "failed". Retrying is safe — if the first
    /// attempt landed and the daemon stopped, the retry answers
    /// [`WireStopOutcome::NoDaemonRunning`]; if it was refused, the retry is
    /// refused the same way.
    ///
    /// Reachable specifically over a forwarded socket, which is the transport
    /// this verb exists for: `ssh -L` accepts locally whether or not anything
    /// upstream is healthy, so a stalled remote produces a connection that is
    /// open and permanently silent. Without this bound the call would block
    /// forever and the confirmation budget below would never be reached
    /// (Greptile P1 on PR #1113).
    WireTimedOut,
}

/// Does `capabilities`, as advertised on a `Hello` reply, include the wire stop?
///
/// `None` means the daemon withheld the field entirely, which per
/// [`crate::daemon_protocol::AttachResponse::capabilities`] a client reads as
/// "withhold", not as "everything". So a daemon too old to advertise anything
/// answers `false` here, which is the safe direction: the caller reports that
/// the deck cannot be stopped over the wire instead of sending a frame that
/// will come back as `unknown variant`.
pub fn supports_wire_stop(capabilities: Option<&[String]>) -> bool {
    capabilities.is_some_and(|caps| caps.iter().any(|c| c == CAP_STOP_DAEMON))
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
            // The daemon already rendered this; re-wording it here would give
            // the operator a second, subtly different account of one refusal.
            Self::Refused(refusal) => write!(f, "{}", refusal.summary),
            Self::WireRejected(msg) => write!(f, "daemon refused stop-daemon: {msg}"),
            Self::WireTimedOut => write!(
                f,
                "daemon accepted the connection but did not answer stop-daemon within {}s;                  whether it saw the request is unknown — retrying is safe",
                WIRE_STOP_REQUEST_TIMEOUT.as_secs()
            ),
        }
    }
}

impl std::error::Error for StopError {}

/// Drive the `daemon stop` flow against `endpoint`. Reusable from
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
/// 4. `terminate_daemon_graceful(pid, endpoint, 5s, force.then(|| 1s))`:
///    - SIGTERM, poll up to 5 s for the daemon to stop accepting connects.
///    - On timeout with `force`: SIGKILL, poll up to 1 s.
///    - On timeout without `force`: surface as `TimedOut`.
pub async fn run_daemon_stop(
    endpoint: &LocalEndpoint,
    force: bool,
) -> Result<StopOutcome, StopError> {
    let attach_path = endpoint.path();
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
    let outcome = terminate_daemon_graceful(pid, endpoint, STOP_GRACE_TIMEOUT, force_window).await;
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
        let _ = writeln!(out, "  {}", orchestration_role_line(role));
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

/// Render ONE orchestration-role registration as a human-readable line, shared
/// by [`format_live_orchestrations_refusal`] and
/// [`format_teardown_inventory`] (issue #1109).
///
/// Shared rather than spelled twice because the two describe the SAME
/// registration from opposite sides of one decision — the refusal says what a
/// guarded stop would destroy, the inventory says what an unguarded one just
/// did — and an operator correlating a refusal they read on Monday with a log
/// line they grep on Friday should not have to notice that the two renderings
/// drifted. The same reasoning already shapes `wire_stop_refusal`, which
/// delegates to the formatters rather than re-deriving a second wording.
///
/// No leading indent: the refusal adds its own two spaces, the inventory packs
/// these into a bracketed list on one line.
fn orchestration_role_line(role: &OrchestrationRoleRecord) -> String {
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
    format!("{} {}{marker}{orchestration}", role.pane_id, role.role)
}

/// Render ONE live agent as a human-readable entry for
/// [`format_teardown_inventory`].
///
/// `id` is the registry's own agent id — the handle every other daemon log line
/// about this agent carries — and `pane` is the key the role maps are indexed
/// by, so the two together let a reader join this line to both. `label` and
/// `cwd` are what say *whose work* was running; `cwd` in particular is the only
/// field here that distinguishes two panes of different dispatched units when
/// neither holds an orchestration role.
fn teardown_agent_line(agent: &AgentRecord) -> String {
    let dash = |v: Option<&str>| v.filter(|s| !s.is_empty()).unwrap_or("-").to_string();
    format!(
        "{} pane={} label={} cwd={}",
        agent.id,
        dash(agent.pane_id_env.as_deref()),
        dash(agent.display_name.as_deref()),
        dash(agent.cwd.as_deref()),
    )
}

/// Issue #1109: render what an UNGUARDED teardown is destroying, for the
/// daemon's own log. `None` when there is nothing to name.
///
/// # Why this exists beside a refusal rather than as one
///
/// Three paths tear this daemon down. `dot-agent-deck daemon stop` and the
/// [`crate::daemon_protocol::AttachRequest::StopDaemon`] wire verb are guarded
/// — they run [`stop_refusal`] and refuse without `--force`. The other two are
/// not: a termination SIGNAL (issue #1109) and the header-only
/// [`crate::daemon_protocol::KIND_SHUTDOWN`] frame both drain the registry
/// unconditionally.
///
/// Issue #1109 asked whether those two should refuse as well, and the answer
/// recorded in `docs/develop/daemon-teardown-paths.md` is no: a SIGTERM is how
/// a service manager, a container runtime or a session logout asks a daemon to
/// stop, and a daemon that argues back is escalated to SIGKILL on the
/// supervisor's clock — which loses the graceful drain AND the disclosure, so it
/// is strictly worse than obeying. What those paths can do instead is SAY what
/// they are taking down, which is the half the guarded path was really
/// providing: #428's occurrence #5 needed log archaeology to establish that a
/// stray `pkill -f "daemon serve"` had stopped nine panes across three
/// dispatched units, because the shutdown line named none of them.
///
/// # Shape
///
/// One line, because it is read by `grep` after the fact rather than by a human
/// watching a terminal — the guarded path's refusal is the multi-line one, and
/// it is read live. The list is bounded by the number of live panes one deck
/// holds, and is deliberately uncapped for the same reason
/// [`format_live_orchestrations_refusal`] is: a truncated forensic list is the
/// one thing worse than no list.
pub fn format_teardown_inventory(
    roles: &[OrchestrationRoleRecord],
    agents: &[AgentRecord],
) -> Option<String> {
    if roles.is_empty() && agents.is_empty() {
        return None;
    }
    use std::fmt::Write;
    let mut out = String::new();
    let _ = write!(
        out,
        "terminating {a} managed agent(s) and destroying {r} orchestration role registration(s)",
        a = agents.len(),
        r = roles.len()
    );
    if !agents.is_empty() {
        let list: Vec<String> = agents.iter().map(teardown_agent_line).collect();
        let _ = write!(out, "; agents: [{}]", list.join(", "));
    }
    if !roles.is_empty() {
        let list: Vec<String> = roles.iter().map(orchestration_role_line).collect();
        let _ = write!(out, "; roles: [{}]", list.join(", "));
        // The same permanence sentence `format_live_orchestrations_refusal`
        // ends on. It is the part that is NOT obvious from the agent list: the
        // processes are merely stopped, while these registrations have no
        // persistence path of any kind and are gone with this process.
        let _ = write!(
            out,
            "; these role registrations are held in memory only, so any agent that survives \
             this teardown keeps running but can never delegate again"
        );
    }
    Some(out)
}

/// The budget [`log_teardown_inventory`] gives the state lock before giving up
/// on naming the orchestration roles.
///
/// The disclosure is diagnostic, so it must not be able to delay the teardown it
/// describes. Two clocks bound it, and the tighter one is ours rather than the
/// operating system's:
///
/// - **An external sender's.** On the signal path the sender is frequently a
///   service manager running its own `TimeoutStopSec`, and spending that budget
///   waiting for a lock trades a graceful drain for a `SIGKILL`. Those windows
///   are measured in tens of seconds, so they are not what sets this number.
/// - **Our own stop clients'**, which is what does. `daemon stop` sends the very
///   SIGTERM this fires on and then polls
///   [`crate::agent_pty::DAEMON_STOP_POLL_BUDGET`] for the daemon to go away,
///   and the teardown that follows this call can already spend
///   [`crate::agent_pty::AGENT_TERMINATE_GRACE`] plus
///   [`crate::agent_pty::FORCE_REAP_DEADLINE`] of it. This is charged to the
///   same window, ahead of both, so it comes out of the headroom that pays for
///   unwinding, dropping the registry, exiting, and a client that samples only
///   every 100 ms.
///
/// 200 ms is twice that sampling interval and far above any contention this
/// lock sees in practice — every other holder takes it for one snapshot — while
/// leaving the great majority of the headroom alone. The assertion below pins
/// the relationship rather than leaving it to this comment, exactly as
/// `FORCE_REAP_DEADLINE`'s own assertions do.
const TEARDOWN_INVENTORY_LOCK_BUDGET: Duration = Duration::from_millis(200);

// The arithmetic above is load-bearing, not stylistic: a disclosure that pushes
// the teardown past what `daemon stop` waits for turns a clean stop into a
// `TimedOut` (or, with `--force`, a SIGKILL mid-teardown) — and it would do it
// on the exact path the disclosure exists to serve.
const _: () = assert!(
    TEARDOWN_INVENTORY_LOCK_BUDGET.as_millis()
        + crate::agent_pty::AGENT_TERMINATE_GRACE.as_millis()
        + crate::agent_pty::FORCE_REAP_DEADLINE.as_millis()
        < crate::agent_pty::DAEMON_STOP_POLL_BUDGET.as_millis(),
    "the teardown disclosure plus the SIGTERM grace plus the reap must finish strictly inside \
     the window a stop client polls for, or naming what was destroyed is what destroyed the \
     clean stop"
);

/// Issue #1109: log what this teardown is destroying, on the paths that do not
/// refuse.
///
/// Call this BEFORE draining the registry. `AgentPtyRegistry::agent_records`
/// filters to live agents and `shutdown_all_graceful` drains the map, so after
/// the drain both halves of this inventory read empty and the disclosure is a
/// line saying nothing was lost.
///
/// `path` names which teardown produced the line (`"signal"`,
/// `"shutdown-frame"`) so a reader does not have to correlate it against the
/// line above by timestamp.
///
/// Best-effort by construction: a state lock it cannot take inside
/// [`TEARDOWN_INVENTORY_LOCK_BUDGET`] costs the ROLE half of the inventory and
/// is itself logged, so an absent role list arrives with a line saying why
/// rather than reading as "no roles were at stake". The agent half comes off the
/// registry's own mutex and does not depend on that read.
pub async fn log_teardown_inventory(
    state: &SharedState,
    registry: &AgentPtyRegistry,
    path: &'static str,
) {
    let agents = registry.agent_records();
    let roles = match tokio::time::timeout(TEARDOWN_INVENTORY_LOCK_BUDGET, state.read()).await {
        Ok(guard) => guard.live_orchestration_roles(registry),
        Err(_) => {
            warn!(
                path,
                budget_ms = TEARDOWN_INVENTORY_LOCK_BUDGET.as_millis() as u64,
                "could not read daemon state within the budget; this teardown's orchestration \
                 roles are UNLISTED, which is not the same as none being at stake"
            );
            Vec::new()
        }
    };
    if let Some(inventory) = format_teardown_inventory(&roles, &agents) {
        warn!(
            path,
            agent_count = agents.len(),
            role_count = roles.len(),
            "{inventory}"
        );
    }
}

/// Issue #1049: the wire form of [`stop_refusal`], for
/// [`crate::daemon_protocol::AttachRequest::StopDaemon`].
///
/// A thin adapter on purpose. The *policy* is `stop_refusal` and the *wording*
/// is [`format_live_orchestrations_refusal`] / [`format_live_agents_refusal`],
/// both unchanged and both still what the CLI uses — so the wire stop and the
/// CLI stop cannot reach different verdicts or describe the same verdict
/// differently. That mattered enough to shape the function: the obvious
/// alternative, deciding the refusal again from the two slices, is how the two
/// paths would have drifted the first time either policy was touched.
///
/// `None` means nothing stands in the way of the stop.
pub fn wire_stop_refusal(
    orchestration_roles: &[OrchestrationRoleRecord],
    agent_ids: &[String],
    force: bool,
) -> Option<StopDaemonRefusal> {
    let err = stop_refusal(orchestration_roles, agent_ids, force)?;
    let summary = err.to_string();
    let (reason, message) = match &err {
        StopError::LiveOrchestrations { roles } => (
            StopRefusalReason::LiveOrchestrations,
            format_live_orchestrations_refusal(roles),
        ),
        StopError::LiveAgents { ids } => (
            StopRefusalReason::LiveAgents,
            format_live_agents_refusal(ids),
        ),
        // `stop_refusal` returns only those two, and its signature is the whole
        // reason this is unreachable rather than defensive. If a third guard is
        // ever added there, this arm is what stops it reaching the wire as a
        // silent "nothing in the way" — the daemon refuses, generically, rather
        // than stopping a deck whose new guard this function did not understand.
        other => (StopRefusalReason::Unknown, format!("{other}\n")),
    };
    Some(StopDaemonRefusal {
        reason,
        roles: orchestration_roles.to_vec(),
        agent_ids: agent_ids.to_vec(),
        message,
        summary,
    })
}

/// How long to wait for a wire-stopped daemon to actually stop answering.
///
/// The same 5 s the PID path gives SIGTERM ([`STOP_GRACE_TIMEOUT`]), for the
/// same reason: the daemon's own drain gives each agent
/// `AGENT_TERMINATE_GRACE` before escalating, and the teardown follows.
const WIRE_STOP_CONFIRM_TIMEOUT: Duration = STOP_GRACE_TIMEOUT;

/// Gap between confirmation probes. Matches the PID path's poll cadence.
const WIRE_STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bound on the `StopDaemon` round trip itself.
///
/// The daemon answers this verb *before* it drains anything — a refusal is
/// immediate and an accept is written ahead of the teardown — so a healthy peer
/// replies in well under a second plus the transport's round trip. Ten seconds
/// is slack for a loaded host or a slow tunnel, not a budget anything is
/// expected to use. Exceeding it yields [`StopError::WireTimedOut`].
const WIRE_STOP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on one confirmation probe.
///
/// Tighter than the request bound because a probe is only ever a `Hello`, and
/// because several of them have to fit inside
/// [`WIRE_STOP_CONFIRM_TIMEOUT`].
const WIRE_STOP_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many CONSECUTIVE unreachable probes confirm the daemon is gone.
///
/// Not one. A single failed probe used to be treated as proof the daemon had
/// exited, which over a forwarded socket meant a momentary tunnel hiccup could
/// return [`WireStopOutcome::Stopped`] for a daemon that was still running
/// (Greptile P1 on PR #1113) — a **false success**, and false successes on the
/// stop path are the specific defect issue #1049 was filed about: over `ssh -L`
/// the old PID path reported "Daemon stopped gracefully (pid N)" having killed
/// the tunnel. Requiring the absence to persist across three probes costs
/// ~200 ms on the ordinary path and removes that class of report.
const WIRE_STOP_GONE_CONFIRMATIONS: u32 = 3;

/// Issue #1049: what a wire stop can end as.
///
/// Deliberately NOT [`StopOutcome`], whose two success variants both carry a
/// pid. A wire caller has no pid — that is the entire point of the verb — and
/// inventing one to reuse the type would put a number in front of an operator
/// that names nothing they can act on. `ForceKilled` has no meaning here
/// either: escalating past a graceful stop needs a signal, and a caller on
/// another machine has nothing to signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireStopOutcome {
    /// Nothing was listening. Idempotent, exactly like
    /// [`StopOutcome::NoDaemonRunning`].
    NoDaemonRunning,
    /// The daemon accepted the stop AND stopped answering within
    /// [`WIRE_STOP_CONFIRM_TIMEOUT`].
    Stopped,
    /// The daemon accepted the stop and had not demonstrably gone when the
    /// confirmation budget ran out.
    ///
    /// Not an error, and reported separately rather than as one: the daemon
    /// said yes and began its drain, which can legitimately outlast the budget
    /// with slow-exiting agents, and over a tunnel every probe carries the
    /// round trip too. What the caller must not do is *report* it as stopped —
    /// PRD #741's own cautionary case is a stop path that printed success for a
    /// daemon it had not touched.
    AcceptedNotConfirmed,
}

/// Issue #1049: stop a deck over the wire, from a client that need not be on
/// its machine.
///
/// `address` is what the caller already holds for every other request —
/// [`crate::remote_tunnel::EndpointConnection::connect_address`] — so this is
/// the same address a remote `ListAgents` goes to, and for a remote deck that is
/// the local end of an `ssh -L` tunnel.
///
/// **The whole difference from [`run_daemon_stop`] is who does the terminating.**
/// That function reads the daemon's pid off the socket with `SO_PEERCRED` and
/// signals it, which needs a kernel-level view of a peer on this machine; over a
/// tunnel the credential names the local `ssh` client, so it would signal the
/// tunnel and report the daemon stopped. Here the daemon stops itself, and
/// nothing in the path consults a pid, a credential, or a signal. That is why it
/// works remotely — and also why it is strictly weaker: there is no `--force`
/// escalation, because a caller on another machine has nothing to escalate with.
/// A wedged daemon is reported as [`WireStopOutcome::AcceptedNotConfirmed`] and
/// has to be dealt with where it runs.
///
/// **The refusal is carried, not re-derived.** The daemon runs
/// [`stop_refusal`] itself and answers with
/// [`crate::daemon_protocol::StopDaemonRefusal`], which arrives here as
/// [`StopError::Refused`] with the panes, the roles and both renderings. A
/// remote caller has no other view of that pane list, so a refusal that crossed
/// as a bare string would leave it unable to present the choice #770 exists to
/// present.
pub async fn run_daemon_stop_over_wire(
    address: &std::path::Path,
    force: bool,
) -> Result<WireStopOutcome, StopError> {
    run_daemon_stop_over_wire_with(
        address,
        force,
        WIRE_STOP_REQUEST_TIMEOUT,
        WIRE_STOP_CONFIRM_TIMEOUT,
    )
    .await
}

/// [`run_daemon_stop_over_wire`] with its two budgets spelled out.
///
/// Public because a caller with its own latency expectations has a legitimate
/// reason to choose them — a GUI that wants to surface "still stopping" sooner
/// than [`WIRE_STOP_CONFIRM_TIMEOUT`] would, say — and because the timeout and
/// confirmation behaviour has to be testable in the fast tier, where spending
/// the production 10 s and 5 s would not be acceptable. Every decision other
/// than the two budgets is shared with the wrapper, so a test through this
/// exercises the real path.
pub async fn run_daemon_stop_over_wire_with(
    address: &std::path::Path,
    force: bool,
    request_timeout: Duration,
    confirm_timeout: Duration,
) -> Result<WireStopOutcome, StopError> {
    let stream = match IpcStream::connect(address).await {
        Ok(s) => s,
        Err(e)
            if e.kind() == io::ErrorKind::ConnectionRefused
                || e.kind() == io::ErrorKind::NotFound =>
        {
            // Same idempotent reading as `run_daemon_stop`'s connect arm.
            debug!(
                target: "daemon_stop",
                path = %address.display(),
                err = %e,
                "no daemon answering (connect failed)"
            );
            return Ok(WireStopOutcome::NoDaemonRunning);
        }
        Err(e) => return Err(StopError::ConnectFailed(e)),
    };

    let (mut rd, mut wr) = stream.into_split();
    // Bounded: `issue_command` has no timeout of its own, and over a forwarded
    // socket a stalled upstream yields a connection that is open and silent
    // forever. See `StopError::WireTimedOut`.
    let resp = match tokio::time::timeout(
        request_timeout,
        issue_command(&mut rd, &mut wr, &AttachRequest::StopDaemon { force }),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(StopError::WireRejected(e.to_string())),
        Err(_) => return Err(StopError::WireTimedOut),
    };
    drop(rd);
    drop(wr);

    if !resp.ok {
        // The structured refusal when the daemon sent one; otherwise whatever it
        // said, which for a daemon predating the verb is serde's `unknown
        // variant` — see `StopError::WireRejected`.
        return Err(match resp.stop_refusal {
            Some(refusal) => StopError::Refused(refusal),
            None => StopError::WireRejected(
                resp.error
                    .unwrap_or_else(|| "stop-daemon failed with no reason given".into()),
            ),
        });
    }

    debug!(
        target: "daemon_stop",
        path = %address.display(),
        force,
        "daemon acknowledged stop-daemon; confirming it stops answering"
    );
    if poll_daemon_gone_over_wire(address, confirm_timeout).await {
        Ok(WireStopOutcome::Stopped)
    } else {
        Ok(WireStopOutcome::AcceptedNotConfirmed)
    }
}

/// What one confirmation probe established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// The daemon answered. It is still up.
    Answered,
    /// Nothing is there: the connect was refused, or the connection produced an
    /// EOF or transport error instead of a reply. Over `ssh -L` a dead remote
    /// daemon looks exactly like this — the tunnel accepts, then the forward
    /// collapses — which is why this and not `Stalled` is the evidence of death.
    Unreachable,
    /// The connection is open and silent: no reply, and no EOF either, within
    /// [`WIRE_STOP_PROBE_TIMEOUT`].
    ///
    /// **Deliberately not evidence of death.** A daemon that exits closes its
    /// socket, so its peer sees EOF rather than silence; silence is a peer that
    /// is stalled or a forward that is wedged. Counting it as gone is how a hung
    /// tunnel would be reported as a successful stop.
    Stalled,
}

/// Poll until `address` has demonstrably stopped answering, or `budget` elapses.
///
/// **Not** `build_version_handshake`'s `poll_daemon_gone`, and the difference is
/// the point. That one short-circuits on the endpoint's filesystem presence,
/// which its own comment scopes to a local endpoint — for a remote deck the
/// socket being polled belongs to the *tunnel*, so presence says nothing about
/// the daemon. A connect is no better for the same reason: `ssh -L` keeps
/// accepting locally after the remote daemon has gone, and only the forward
/// behind it fails.
///
/// So this spends a real round trip, and requires
/// [`WIRE_STOP_GONE_CONFIRMATIONS`] CONSECUTIVE unreachable probes before it
/// says gone. Anything else — an answer, or a stall — resets the count, so a
/// momentary hiccup cannot be reported as a completed stop.
async fn poll_daemon_gone_over_wire(address: &std::path::Path, budget: Duration) -> bool {
    let start = std::time::Instant::now();
    let mut consecutive_unreachable = 0u32;
    loop {
        match probe_daemon(address).await {
            Probe::Unreachable => {
                consecutive_unreachable += 1;
                if consecutive_unreachable >= WIRE_STOP_GONE_CONFIRMATIONS {
                    return true;
                }
            }
            // Both reset. `Answered` is proof it is up; `Stalled` proves nothing
            // either way, and treating "proves nothing" as progress toward "gone"
            // is exactly the false success this counter exists to stop.
            Probe::Answered | Probe::Stalled => consecutive_unreachable = 0,
        }
        if start.elapsed() >= budget {
            return false;
        }
        tokio::time::sleep(WIRE_STOP_POLL_INTERVAL).await;
    }
}

/// One `Hello` round trip, classified. `Hello` is the cheapest request every
/// daemon answers and it mutates nothing.
async fn probe_daemon(address: &std::path::Path) -> Probe {
    let Ok(stream) = IpcStream::connect(address).await else {
        return Probe::Unreachable;
    };
    let (mut rd, mut wr) = stream.into_split();
    let req = AttachRequest::Hello {
        client_version: crate::daemon_protocol::PROTOCOL_VERSION,
        client_build_version: None,
    };
    match tokio::time::timeout(
        WIRE_STOP_PROBE_TIMEOUT,
        issue_command(&mut rd, &mut wr, &req),
    )
    .await
    {
        Ok(Ok(_)) => Probe::Answered,
        // An EOF or transport error on an established connection is what a dead
        // daemon behind a live tunnel looks like.
        Ok(Err(_)) => Probe::Unreachable,
        Err(_) => Probe::Stalled,
    }
}

/// `daemon restart`: PRD #103 M3.3 — same logic as `daemon stop`. The
/// next TUI invocation lazy-spawns a fresh daemon (PRD #93). This is
/// intentionally a thin wrapper rather than a stop-then-spawn flow,
/// because spawning a daemon out of `daemon restart` would either
/// race the next TUI's `ensure_external_daemon_or_die` (two daemons
/// trying to bind under flock) or require duplicating the lazy-spawn
/// machinery here.
pub async fn run_daemon_restart(
    endpoint: &LocalEndpoint,
    force: bool,
) -> Result<StopOutcome, StopError> {
    run_daemon_stop(endpoint, force).await
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

    /// A live `AgentRecord` as [`AgentPtyRegistry::agent_records`] yields one,
    /// with only the four fields [`teardown_agent_line`] reads carrying values.
    /// Built exhaustively rather than through serde so a new field forces a
    /// deliberate look at whether the teardown inventory should name it.
    fn agent(id: &str, pane: Option<&str>, label: Option<&str>, cwd: Option<&str>) -> AgentRecord {
        AgentRecord {
            id: id.to_string(),
            pane_id_env: pane.map(str::to_string),
            display_name: label.map(str::to_string),
            cwd: cwd.map(str::to_string),
            tab_membership: None,
            agent_type: None,
            rows: 24,
            cols: 80,
            live: None,
            spawned_at_ms: None,
            cli_name: None,
            crashed: None,
        }
    }

    /// Issue #1109: the disclosure the UNGUARDED teardown paths emit. It is the
    /// whole deliverable of that issue — the signal path and `KIND_SHUTDOWN`
    /// keep destroying what they destroy, and what changed is that they now say
    /// what it was — so it must name every pane, every role, and the permanence.
    #[test]
    fn teardown_inventory_names_every_agent_every_role_and_the_permanence() {
        let roles = vec![
            role("sched-issue-work-1-r0", "orchestrator", true),
            role("sched-issue-work-1-r1", "coder", false),
        ];
        let agents = vec![
            agent(
                "12",
                Some("sched-issue-work-1-r0"),
                Some("orchestrator"),
                Some("/home/u/code/repo"),
            ),
            agent(
                "13",
                Some("sched-issue-work-1-r1"),
                Some("coder"),
                Some("/home/u/code/repo-worker"),
            ),
        ];
        let msg = format_teardown_inventory(&roles, &agents)
            .expect("a daemon with live agents and roles has something to disclose");
        for expected in [
            "terminating 2 managed agent(s)",
            "destroying 2 orchestration role registration(s)",
            "12",
            "13",
            "sched-issue-work-1-r0",
            "sched-issue-work-1-r1",
            "label=orchestrator",
            "label=coder",
            "/home/u/code/repo-worker",
            "(orchestrator)",
            "[issue-work]",
        ] {
            assert!(
                msg.contains(expected),
                "inventory must name {expected:?}, got: {msg:?}"
            );
        }
        assert!(
            msg.contains("can never delegate again"),
            "the inventory must carry the same permanence sentence the #770 \
             refusal carries — the processes are merely stopped, the role \
             registrations are gone for good; got: {msg:?}"
        );
        assert!(
            !msg.contains('\n'),
            "the inventory is grepped out of a log after the fact, so it must be \
             ONE line; got: {msg:?}"
        );
    }

    /// The inventory and the #770 refusal describe the same registration, and a
    /// reader correlating a refusal with a log line should not have to notice a
    /// drift. Pinned by rendering both from one role set and requiring the
    /// refusal's own per-role rendering to appear verbatim inside the inventory.
    #[test]
    fn teardown_inventory_renders_roles_exactly_as_the_refusal_does() {
        let roles = vec![role("sched-issue-work-1-r0", "orchestrator", true)];
        let line = orchestration_role_line(&roles[0]);
        assert!(
            format_live_orchestrations_refusal(&roles).contains(&line),
            "the refusal must render the role through the shared helper"
        );
        assert!(
            format_teardown_inventory(&roles, &[])
                .expect("roles alone are worth disclosing")
                .contains(&line),
            "the inventory must render the role through the same helper"
        );
    }

    /// An idle daemon discloses nothing: a teardown that destroys no agent and
    /// no registration has nothing to say, and a line saying so on every clean
    /// `daemon stop` is noise that trains readers to skip the one that matters.
    #[test]
    fn teardown_inventory_is_absent_when_nothing_is_at_stake() {
        assert!(format_teardown_inventory(&[], &[]).is_none());
    }

    /// The half that is reachable on its own: `live_orchestration_roles`
    /// filters to panes with a live agent, so roles imply agents — but agents
    /// do NOT imply roles. An ordinary single-agent deck must still be named,
    /// and must NOT be told its registrations are gone for good when it held
    /// none.
    #[test]
    fn teardown_inventory_without_roles_names_the_agents_and_claims_no_role_loss() {
        let agents = vec![agent("7", Some("pane-7"), None, None)];
        let msg = format_teardown_inventory(&[], &agents).expect("a live agent is worth naming");
        assert!(
            msg.contains("terminating 1 managed agent(s)")
                && msg.contains("destroying 0 orchestration role registration(s)")
                && msg.contains("pane=pane-7"),
            "got: {msg:?}"
        );
        assert!(
            !msg.contains("can never delegate again"),
            "the permanence sentence is about role registrations; with none held \
             it would be a false claim about what this teardown cost, got: {msg:?}"
        );
        assert!(
            msg.contains("label=-") && msg.contains("cwd=-"),
            "an unlabelled agent must still render a total entry rather than a \
             ragged one, got: {msg:?}"
        );
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

    /// Issue #1049: the wire refusal is the CLI refusal, not a second opinion.
    /// If these ever diverge, a deck is safe to stop from one path and not the
    /// other — the single worst outcome for a guard that exists to be trusted.
    #[test]
    fn wire_stop_refusal_agrees_with_the_cli_refusal() {
        let roles = vec![role("sched-issue-work-1-r0", "orchestrator", true)];
        let agents = vec!["7".to_string()];

        for (rs, ids) in [
            (roles.clone(), vec![]),
            (vec![], agents.clone()),
            (roles.clone(), agents.clone()),
            (vec![], vec![]),
        ] {
            for force in [false, true] {
                assert_eq!(
                    stop_refusal(&rs, &ids, force).is_some(),
                    wire_stop_refusal(&rs, &ids, force).is_some(),
                    "the wire and CLI paths must agree on WHETHER to refuse                      (roles={}, agents={}, force={force})",
                    rs.len(),
                    ids.len()
                );
            }
        }

        // …and on WHICH guard, including the ordering when both apply.
        let both = wire_stop_refusal(&roles, &agents, false).expect("both guards apply");
        assert_eq!(
            both.reason,
            StopRefusalReason::LiveOrchestrations,
            "with both present the orchestration refusal wins on the wire too — it is the one              that says the survivors are STRANDED, not merely killed"
        );
        assert_eq!(
            both.message,
            format_live_orchestrations_refusal(&roles),
            "one rendering, shared with the CLI"
        );
        assert_eq!(
            both.summary,
            StopError::LiveOrchestrations {
                roles: roles.clone()
            }
            .to_string(),
            "the one-line form is StopError's Display, so the wire and the terminal say the              same sentence"
        );
        // Both lists cross regardless of which guard tripped: a caller
        // presenting the orchestration refusal still wants to know how many
        // agents go down with it.
        assert_eq!(both.roles, roles);
        assert_eq!(both.agent_ids, agents);

        let agents_only = wire_stop_refusal(&[], &agents, false).expect("agents guard applies");
        assert_eq!(agents_only.reason, StopRefusalReason::LiveAgents);
        assert_eq!(
            agents_only.message,
            format_live_agents_refusal(&agents),
            "the agents refusal keeps its own wording"
        );
        assert!(
            agents_only.roles.is_empty(),
            "no roles registered, so none may be claimed"
        );
    }

    /// Issue #1049: a withheld capability set means "withhold", never
    /// "everything". Getting this backwards would have a remote client send a
    /// verb an old daemon answers with `unknown variant`, and report the
    /// resulting error as though the deck had refused.
    #[test]
    fn wire_stop_capability_is_read_conservatively() {
        assert!(
            !supports_wire_stop(None),
            "a daemon that advertises nothing cannot be assumed to speak this verb"
        );
        assert!(
            !supports_wire_stop(Some(&[])),
            "an empty set is an explicit no"
        );
        assert!(!supports_wire_stop(Some(&["list-projects".to_string()])));
        assert!(supports_wire_stop(Some(&[
            "list-projects".to_string(),
            CAP_STOP_DAEMON.to_string(),
        ])));
        // The advertised set must actually contain it, or the check above is
        // pinning a string no daemon sends.
        assert!(
            crate::daemon_protocol::DAEMON_CAPABILITIES.contains(&CAP_STOP_DAEMON),
            "this build must advertise the verb it answers"
        );
    }

    /// Issue #1049: the refusal survives a serde round trip with its structure
    /// intact. It crosses a socket in production, and a remote caller has no
    /// other view of the panes it names.
    #[test]
    fn wire_stop_refusal_round_trips_over_serde() {
        let roles = vec![
            role("sched-issue-work-1-r0", "orchestrator", true),
            role("sched-issue-work-1-r1", "coder", false),
        ];
        let refusal = wire_stop_refusal(&roles, &["7".to_string()], false).expect("refusal");
        let wire = serde_json::to_string(&refusal).expect("serialize");
        let back: StopDaemonRefusal = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back, refusal);

        // A newer daemon's unknown reason must not fail an older client's whole
        // decode — it still gets the message and the lists, which is enough to
        // present the choice, and loses only the ability to branch on the kind.
        let forward = wire.replace(
            "\"reason\":\"live-orchestrations\"",
            "\"reason\":\"some-future-guard\"",
        );
        assert_ne!(forward, wire, "the rename_all spelling must be what ships");
        let tolerated: StopDaemonRefusal =
            serde_json::from_str(&forward).expect("an unknown reason must not fail the decode");
        assert_eq!(tolerated.reason, StopRefusalReason::Unknown);
        assert_eq!(tolerated.roles, refusal.roles);
        assert_eq!(tolerated.message, refusal.message);
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
