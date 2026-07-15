use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, broadcast};
use tracing::warn;

use crate::agent_pty::AgentPtyRegistry;
use crate::config_validation::sanitize_role_name;
use crate::event::{
    AgentEvent, AgentType, BroadcastMsg, DISPLAY_NAME_METADATA_KEY, DelegateSignal, EventType,
    LiveTarget, OrchestrationSurface, WorkDoneSignal, Writable,
};
use crate::project_config::{OrchestrationRoleConfig, load_project_config};

const MAX_RECENT_EVENTS: usize = 50;
/// PRD #120 L1: cap on [`AppState::pending_orchestration_surfaces`]. The render
/// loop drains the queue one surface per frame, so a daemon flooding surface
/// events faster than it drains can't grow the Vec unbounded — beyond this the
/// OLDEST queued surface is dropped (the newer dispatch is the more relevant one
/// to build). Sized well above any realistic concurrent-dispatch burst (a fire's
/// `max_per_run` issue dispatches is single/low-double digits).
const MAX_PENDING_ORCHESTRATION_SURFACES: usize = 64;
/// Maximum number of first-prompt entries retained per session. The live-side
/// cap in `apply_event` and the wire-boundary clamp in
/// [`crate::daemon_client`] (which re-clamps a hostile/oversized daemon
/// snapshot) share this single source of truth.
pub(crate) const MAX_FIRST_PROMPTS: usize = 3;

/// PRD #92 F9 followup-6: how long the post-respawn dispatch task
/// waits for the freshly-spawned agent to emit a `SessionStart` hook
/// event before falling back to writing the prompt anyway.
///
/// Restores the pre-daemon baseline (`2fc39c3:src/ui.rs::process_pending_dispatches`)
/// which deferred the task-prompt write until `SessionStart` arrived
/// (10 s timeout fallback). The F9 fixed-delay shortcut
/// (`RESPAWN_READY_DELAY = 250 ms`) was empirically too short for
/// Claude Code's TUI boot sequence — bytes landed mid-init and got
/// dropped on the floor.
///
/// Agents that never emit `SessionStart` (e.g. `cat -u` in tests, or
/// agent runtimes without dot-agent-deck's hooks installed) still get
/// their prompt — just delayed by `SESSION_START_WAIT_TIMEOUT`.
pub(crate) const SESSION_START_WAIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionStatus {
    Thinking,
    Working,
    Compacting,
    WaitingForInput,
    Idle,
    Error,
    /// PRD #162 forward-compat catch-all: a future/unknown `status` string on
    /// the wire deserializes here instead of failing the whole `AgentRecord`
    /// decode. Deserialize-only — `#[serde(other)]` variants are never
    /// serialized, and the daemon's `live_snapshot()` only ever produces the
    /// six real variants, so `Unknown` only ever originates from an
    /// unrecognized wire value on a newer daemon. Rendered neutrally (like
    /// `Idle`) so it never masquerades as an active state.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardStats {
    pub active: usize,
    pub working: usize,
    pub thinking: usize,
    pub waiting: usize,
    pub errors: usize,
    pub idle: usize,
    pub compacting: usize,
    pub total_tools: u64,
    /// PRD #20 finding #10: per-agent-type active counts, in registry
    /// (`agent_registry::ALL`) order, including only real agent types that have
    /// at least one active session. The stats bar renders a compact breakdown
    /// (`1 ClaudeCode │ 1 Codex`) from this ONLY when more than one distinct
    /// agent type is active, so a single-agent dashboard is unchanged. Defaults
    /// to empty (a hand-built `DashboardStats` carries no breakdown).
    pub by_agent_type: Vec<(AgentType, usize)>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActiveTool {
    pub name: String,
    pub detail: Option<String>,
}

/// PRD #162: a serializable snapshot of the daemon's live, event-derived
/// session state, attached to each `AgentRecord` in the `ListAgents` response
/// so a reconnecting TUI restores the real status / agent type / active tool /
/// tool count / prompt context instead of minting a bare `Idle` / "No agent"
/// placeholder.
///
/// Carried as an additive optional (`AgentRecord.live: Option<SessionSnapshot>`):
/// an older daemon, the test/dummy-state attach path, or an agent that never
/// emitted an event all yield `None`, and the TUI falls back to today's
/// placeholder behavior. No `PROTOCOL_VERSION` bump — every field follows the
/// M2.11–M2.13 `#[serde(default, skip_serializing_if = ...)]` reconnect-field
/// convention.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionSnapshot {
    /// The live `SessionStatus` (`Working` / `Thinking` / `WaitingForInput` /
    /// `Idle` / `Compacting` / `Error`) as `apply_event` last computed it.
    pub status: SessionStatus,
    /// The event-derived agent type — this is the "No agent" fix: a spawn-time
    /// `AgentRecord.agent_type = None` is overridden by the `Some(..)` carried
    /// here once the session has emitted at least one event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<AgentType>,
    /// The active tool (name + detail) if the session is mid-tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tool: Option<ActiveTool>,
    /// Running tool tally so the card's tool count survives the reconnect.
    pub tool_count: u32,
    /// First-prompt context preserved across the reconnect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub first_prompts: Vec<String>,
    /// The most recent user prompt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_prompt: Option<String>,
    /// PRD #20 blocker-4: the session's durable live-target descriptor, so a
    /// history-only / view-only card keeps its input-refusal across a
    /// detach/reconnect instead of falling back to the legacy live default.
    /// Additive optional (`#[serde(default)]` + `skip_serializing_if`): an
    /// older daemon or a native PTY pane that never declared one yields `None`,
    /// which the TUI reads as `Live`. Restored by
    /// [`AppState::seed_hydrated_session`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_target: Option<LiveTarget>,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: String,
    pub agent_type: AgentType,
    pub cwd: Option<String>,
    pub status: SessionStatus,
    pub active_tool: Option<ActiveTool>,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub recent_events: VecDeque<AgentEvent>,
    pub tool_count: u32,
    pub last_user_prompt: Option<String>,
    pub first_prompts: Vec<String>,
    pub pane_id: Option<String>,
    /// PRD #110: the daemon-side registry id of the agent process that
    /// produced this session. Lets the same-pane reuse guard in
    /// `apply_event` distinguish "same agent restarting in place"
    /// (opencode crash/reload — reuse) from "different agent entirely"
    /// (PRD #92 F9 clear=true respawn — new session card).
    pub agent_id: Option<String>,
    /// PRD #127 finding #2: a human-friendly card title carried on the
    /// live-surface `SessionStart` (the schedule's task name, via
    /// [`crate::event::DISPLAY_NAME_METADATA_KEY`]). The dashboard prefers
    /// `ui.display_names` (populated by hydration/rename) and falls back to
    /// this when the attached TUI has no display-name entry for the pane —
    /// the live scheduler-spawn case, where the name would otherwise degrade
    /// to the truncated pane id. `None` for ordinary hook-driven sessions.
    pub display_name: Option<String>,
}

impl SessionState {
    /// PRD #162: build the wire [`SessionSnapshot`] from this live session.
    /// The snapshot's `agent_type` is the EVENT-DERIVED value, so a
    /// reconnecting TUI can override a `None` spawn-time
    /// `AgentRecord.agent_type` with what the agent actually is — but
    /// `AgentType::None` (the agent has emitted events yet never identified
    /// itself) maps to `Option::None`, NOT `Some(AgentType::None)`. A
    /// `Some(None-the-type)` would shadow the spawn-time fallback in
    /// [`AppState::seed_hydrated_session`] and regress a real, known
    /// spawn-time type to "No agent"; emitting `None` here keeps that
    /// fallback reachable.
    pub fn live_snapshot(&self) -> SessionSnapshot {
        let agent_type = match self.agent_type {
            AgentType::None => None,
            ref other => Some(other.clone()),
        };
        SessionSnapshot {
            status: self.status.clone(),
            agent_type,
            active_tool: self.active_tool.clone(),
            tool_count: self.tool_count,
            first_prompts: self.first_prompts.clone(),
            last_user_prompt: self.last_user_prompt.clone(),
            // PRD #20 blocker-4: carry the durable live-target so a reconnect
            // restores the card's write-semantics (history-only / view-only).
            live_target: self.live_target(),
        }
    }

    /// PRD #20 M3/blocker-2: the current live-target descriptor of this session,
    /// or `None` when no event ever declared one.
    ///
    /// The value is DURABLE, not a property that disappears when the declaring
    /// event ages out of the bounded `recent_events` journal: `apply_event`
    /// forward-stamps the last-declared `live_target` onto every subsequent
    /// event that omits one (see [`AppState::apply_event`]), and
    /// [`AppState::seed_hydrated_session`] restamps it from the reconnect
    /// snapshot. So reading the newest declaration back out of `recent_events`
    /// always reflects the explicit session state, even after >`MAX_RECENT_EVENTS`
    /// undeclared events have evicted the original declaration. A
    /// `SessionState` carries no dedicated field for it because uneditable
    /// fixtures construct the struct by exhaustive literal.
    pub fn live_target(&self) -> Option<LiveTarget> {
        self.recent_events.iter().rev().find_map(|e| e.live_target)
    }

    /// PRD #20 M3: the write-semantics of this session's live target. A session
    /// that never declared a live_target (every native Claude/OpenCode/Pi PTY
    /// pane, and any directly-constructed fixture) is treated as
    /// [`Writable::Live`]: the historical default where the pane the dashboard
    /// shows is the pane it writes to. A wrapped Codex session that declared
    /// `history-only` (see [`crate::wrap`]) reports non-live here durably.
    pub fn writable(&self) -> Writable {
        self.live_target()
            .map(|lt| lt.writable)
            .unwrap_or(Writable::Live)
    }
}

#[derive(Debug, Default, Clone)]
pub struct AppState {
    pub sessions: HashMap<String, SessionState>,
    /// Remembers started_at per pane so a `/clear` restart keeps its position.
    pane_started_at: HashMap<String, DateTime<Utc>>,
    /// Set by the background version-check task when a newer release exists.
    pub update_available: Option<String>,
    /// Pane IDs created by our app — events from unknown panes are rejected.
    pub managed_pane_ids: HashSet<String>,
    /// Maps pane_id → orchestration role name (set when orchestration tab opens).
    pub pane_role_map: HashMap<String, String>,
    /// Maps pane_id → working directory for orchestration panes.
    pub pane_cwd_map: HashMap<String, String>,
    /// Pane IDs that are orchestrator (start=true) roles — only these can delegate.
    pub orchestrator_pane_ids: HashSet<String>,
    /// Maps pane_id → (orchestration name, orchestration cwd). Lets the
    /// daemon's dispatch (`handle_delegate` / `handle_work_done`) scope
    /// target lookups to panes in the *same* orchestration tab when
    /// several tabs run in parallel (PRD #93 round-5).
    ///
    /// Round-11 auditor #C: the identity is a `(name, cwd)` tuple, not
    /// just name. Two unnamed orchestrations whose `name`s both fall
    /// back to the same cwd-basename — e.g. `~/project-a/foo` and
    /// `~/project-b/foo` — would otherwise collide here and a
    /// `Delegate` from A's orchestrator could cross-route to B's
    /// coder. The cwd disambiguator is the cwd the TUI passed at
    /// StartAgent time; in practice all role panes in one orchestration
    /// share that cwd, so within-orchestration scoping still finds all
    /// the right panes.
    pub pane_orchestration_map: HashMap<String, (String, String)>,
    /// PRD #120: orchestrations the daemon spawned WHILE this TUI is attached
    /// (the issue-dispatch path), queued for the TUI event loop to build into
    /// live tabs. The daemon publishes a
    /// [`BroadcastMsg::OrchestrationSurface`]; the event subscriber records it
    /// here (it has no access to the `TabManager` / pane controller), and the
    /// render loop drains ONE entry per frame (M2/S3: each build does bounded
    /// per-role attach round-trips, so one-per-frame keeps a burst from freezing
    /// the UI), attaches each role's PTY, and builds the orchestration tab via
    /// the existing `open_orchestration_tab_with_existing_role_panes` machinery.
    /// Empty in the common case; bounded by `MAX_PENDING_ORCHESTRATION_SURFACES`
    /// (L1) so a flood can't grow it unbounded.
    pub pending_orchestration_surfaces: Vec<OrchestrationSurface>,
    /// PRD #20 R20-003 (finding #4): the DAEMON-AUTHORITATIVE hook session id
    /// (the "generation") currently bound to each pane, keyed by `pane_id`.
    /// Captured from every event's ORIGINAL `session_id` BEFORE the same-agent
    /// reuse guard in [`Self::apply_event`] remaps that id onto the stable card
    /// id. Without this separate track, a same-agent `/clear` / thread restart
    /// (which mints a NEW hook session under the SAME `agent_id`) is remapped
    /// back onto the OLD card id, so the card's `session_id` — and thus
    /// [`Self::pane_session_id`] — keeps reporting the OLD generation, and an old
    /// queued prompt bound to it is wrongly accepted in the NEW conversation.
    /// The atomic write-and-submit guard compares the caller's expected session
    /// against [`Self::pane_hook_session_id`] (this map) instead, so a stale
    /// generation is refused with no bytes. Cleared on `SessionEnd`.
    pane_hook_session: HashMap<String, String>,
}

pub type SharedState = Arc<RwLock<AppState>>;

const WORK_DONE_FOOTER: &str = "## When done\n\n\
Signal completion by running this command via Bash:\n\
```bash\n\
dot-agent-deck work-done --task \"Brief summary of what you accomplished. Include file paths and outcomes.\"\n\
```";

/// Compose the prompt that the daemon writes into a worker pane on
/// delegation. In the normal file-backed path this is intentionally only
/// the one-line pointer to `.dot-agent-deck/worker-task-{role}.md`.
/// Keeping every injected PTY prompt single-line avoids bracketed paste
/// and lets the synthetic CR follow the same reliable path as ordinary
/// typed prompts.
///
/// The footer used to be appended per-role by the TUI's
/// `OrchestrationConfig.roles[*].prompt_template` wrapping. PRD #93
/// round-5 moved dispatch into the daemon; the durable worker context now
/// lives in the task file instead of the injected pane prompt.
pub fn compose_delegate_prompt(task_body: &str) -> String {
    task_body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// CodeRabbit (PRD #93 round-9): build the file contents written to
/// `.dot-agent-deck/worker-task-{role}.md` for a delegation. When the
/// role config supplies a `prompt_template`, wrap the task under a
/// `## Task` header beneath the template — mirrors the pre-Round-5 TUI
/// dispatch path that Round 5 lost when it moved orchestration onto
/// the daemon side without bringing the per-role template wrapping
/// along. The work-done footer is appended to the file rather than the
/// PTY-injected pointer so workers still get completion instructions
/// without forcing a multi-line bracketed-paste write into the agent TUI.
pub fn compose_worker_task_file(prompt_template: Option<&str>, task: &str) -> String {
    let body = match prompt_template {
        Some(tpl) if !tpl.trim().is_empty() => format!("{tpl}\n\n## Task\n\n{task}"),
        _ => task.to_string(),
    };
    format!("{}\n\n{}", body.trim_end(), WORK_DONE_FOOTER)
}

/// Look up the role config for `role_name` inside the orchestration
/// named `orchestration_name`, by parsing the project config file at
/// `cwd`. Returns `None` when any layer is missing (no project config,
/// no matching orchestration, no matching role) — the caller treats
/// "no config" as "no template, no clear" and falls through to the
/// default behavior. Centralizing the lookup here keeps
/// `handle_delegate` from juggling three layers of `Option` inline.
fn lookup_orchestration_role(
    cwd: &str,
    orchestration_name: &str,
    role_name: &str,
) -> Option<OrchestrationRoleConfig> {
    let cfg = load_project_config(std::path::Path::new(cwd))
        .ok()
        .flatten()?;
    let orch = cfg
        .orchestrations
        .into_iter()
        .find(|o| o.name == orchestration_name)?;
    orch.roles.into_iter().find(|r| r.name == role_name)
}

/// PRD #92 F9 followup-6: block until the daemon's hook broadcast
/// surfaces a `SessionStart` event for `pane_id`, or `timeout`
/// elapses. The caller is expected to have called `event_tx.subscribe()`
/// **before** spawning the new process — otherwise a fast-booting
/// agent's `SessionStart` could land on the broadcast channel and be
/// missed by a receiver that attached too late.
///
/// PRD #92 F9 followup-7: also filter on `agent_id` — the daemon-side
/// registry id of the freshly-spawned agent. The followup-6 filter
/// matched on `pane_id` alone, which is reused verbatim across a
/// clear=true respawn — so a late `SessionStart` from the OLD agent
/// firing within the subscribe→kill window (e.g. its initial boot
/// was slow) would have unblocked the wait and let the dispatch task
/// write the prompt while the NEW agent was still booting. With the
/// `agent_id` discriminator, OLD-agent events carry the OLD id and
/// are rejected; the NEW agent's first `SessionStart` carries the
/// NEW id (injected via `DOT_AGENT_DECK_AGENT_ID` on spawn and
/// forwarded by the agent's hook script) and matches.
///
/// `Lagged` is treated as "keep polling" rather than fatal: a slow
/// dispatch task that fell behind the daemon's event volume still
/// wakes up on the next event in the ring, and a SessionStart that
/// happened to fall off the back of the ring is functionally
/// equivalent to "we missed it" — the timeout path covers that.
/// `Closed` only fires when the daemon-wide sender is dropped (i.e.
/// the daemon itself is shutting down), in which case there's nothing
/// to wait for.
///
/// Returns `true` when SessionStart was observed, `false` on timeout
/// or sender closure. The boolean isn't currently consulted at the
/// call site — the dispatch path writes the prompt regardless, matching
/// the baseline `process_pending_dispatches` semantics — but it's
/// returned so future telemetry / tracing can distinguish "fast path"
/// from "fallback".
///
/// PRD #127: also reused by the scheduler spawn primitive
/// ([`crate::spawn::spawn`]) to gate a freshly-spawned scheduled card's
/// prompt delivery on the same readiness signal — hence `pub(crate)`.
pub(crate) async fn wait_for_session_start(
    rx: &mut broadcast::Receiver<BroadcastMsg>,
    pane_id: &str,
    agent_id: &str,
    timeout: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            return false;
        };
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(BroadcastMsg::Event(event))) => {
                if event.event_type == EventType::SessionStart
                    && event.pane_id.as_deref() == Some(pane_id)
                    && event.agent_id.as_deref() == Some(agent_id)
                {
                    return true;
                }
            }
            // PRD #120: not a hook event — keep waiting for the SessionStart.
            Ok(Ok(BroadcastMsg::OrchestrationSurface(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => return false,
            Err(_) => return false,
        }
    }
}

/// Per-target body of [`AppState::handle_delegate`], factored out so
/// each target runs in its own `tokio::spawn`. Owns all the inputs it
/// needs (no `&self` / `&AppState` borrows) so the spawn future is
/// `'static`.
///
/// Holds the per-pane dispatch mutex across the entire respawn +
/// post-respawn prompt write, writes the worker task file to the
/// worker's cwd, optionally respawns the worker agent (per the role's
/// `clear` flag) and then writes the prompt one-liner.
///
/// On `clear = true`, this function subscribes to the daemon-wide
/// hook-event broadcast BEFORE calling
/// [`AgentPtyRegistry::respawn_agent_for_pane`] — the receiver
/// attaches to `event_tx` before the new process is forked, so a
/// fast-booting agent's `SessionStart` lands in the receiver's queue.
/// Then it waits up to [`SESSION_START_WAIT_TIMEOUT`] for that event;
/// on timeout, the prompt is written anyway (mirroring the pre-daemon
/// TUI baseline `2fc39c3:src/ui.rs::process_pending_dispatches`,
/// which fell back at 10 s for agents that don't emit
/// `SessionStart`).
///
/// The per-pane dispatch mutex (acquired unconditionally — see
/// [`AgentPtyRegistry::pane_dispatch_lock`]) closes the
/// `registry.remove` + `spawn_agent` race window inside
/// [`AgentPtyRegistry::respawn_agent_for_pane`]: two concurrent
/// connections submitting `Delegate` signals to the same worker pane
/// no longer race the respawn — they serialize behind the mutex. We
/// acquire unconditionally even when `clear = false` because it's
/// cheap and removes the subtler "concurrent clear=true vs
/// clear=false" interleave.
///
/// Errors are logged and dropped; the caller spawns each target
/// independently so a single pane's failure (a missing role config,
/// a respawn that couldn't exec the command, a write that hit a
/// closed PTY) doesn't poison the other panes' dispatches.
#[allow(clippy::too_many_arguments)]
async fn dispatch_one_owned(
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    orchestration: Option<(String, String)>,
    orchestrator_pane_id: String,
    target_role: String,
    pane_id: String,
    task: String,
    cwd: Option<String>,
) {
    let dispatch_mutex = registry.pane_dispatch_lock(&pane_id);
    let _dispatch_guard = dispatch_mutex.lock().await;

    // Look the role config up by `(worker cwd, orchestration name,
    // target role)` so the per-role `prompt_template` wrapping is
    // applied to the task body. Loading the config from disk on
    // every delegate means a config edit between sessions takes
    // effect on the next delegate without a pane respawn. `None`
    // means "no template, fall back to the raw task".
    let role_config = match (cwd.as_deref(), orchestration.as_ref()) {
        (Some(c), Some((orch_name, _orch_cwd))) => {
            lookup_orchestration_role(c, orch_name, &target_role)
        }
        _ => None,
    };
    // When we have an orchestration context (cwd + orchestration
    // name) but the role lookup returned None, the operator's
    // intended `clear = true` is silently dropped — the role
    // config no longer exists, almost always because the user
    // edited `.dot-agent-deck.toml` mid-session and the role name
    // diverged. Emit a warn so the cause is at least discoverable
    // in the daemon log; the fall-through to the no-respawn path
    // is preserved because we have no `command` to spawn anyway.
    if role_config.is_none() && cwd.is_some() && orchestration.is_some() {
        warn!(
            role = %target_role,
            pane_id = %pane_id,
            "delegate: role_config not found for role; \
             clear=true respawn intent dropped — \
             did the role name change in .dot-agent-deck.toml?"
        );
    }
    let prompt_template = role_config
        .as_ref()
        .and_then(|r| r.prompt_template.as_deref());
    let safe_name = sanitize_role_name(&target_role);
    // The task file lands in the *worker's* cwd, not the
    // orchestrator's — earlier rounds reused a single cwd capture
    // across every worker and broke the moment two role panes
    // were started in different cwds.
    let task_body = if let Some(cwd) = cwd.as_deref() {
        let dir = std::path::Path::new(cwd).join(".dot-agent-deck");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(
                dir = %dir.display(),
                role = %target_role,
                pane_id = %pane_id,
                error = %e,
                "delegate: failed to create task directory"
            );
        }
        let file_path = dir.join(format!("worker-task-{safe_name}.md"));
        let file_content = compose_worker_task_file(prompt_template, &task);
        if let Err(e) = std::fs::write(&file_path, &file_content) {
            warn!(
                path = %file_path.display(),
                role = %target_role,
                pane_id = %pane_id,
                error = %e,
                "delegate: failed to write worker task file"
            );
        }
        format!("Read .dot-agent-deck/worker-task-{safe_name}.md for your task.")
    } else {
        // Defensive: the daemon's StartAgent handler always
        // records `pane_cwd_map` for orchestration panes (see
        // `daemon_protocol.rs`), so this branch should be
        // unreachable in production. Log and fall back to
        // inlining the task body so the worker still gets
        // *something* useful rather than a dangling reference.
        warn!(
            role = %target_role,
            pane_id = %pane_id,
            "delegate: no cwd recorded for worker pane — inlining task body"
        );
        compose_worker_task_file(prompt_template, &task)
    };
    // The single-line pointer the worker receives ("Read
    // .dot-agent-deck/worker-task-<role>.md for your task."). Computed here so
    // the PRD #201 pi-native path below can stash it as the pane's seed before
    // the respawned pi boots.
    let one_liner = compose_delegate_prompt(&task_body);

    // PRD #201 native prompt delivery: a pi WORKER whose role is `clear = true`
    // (respawn → a fresh `session_start`) receives its task NATIVELY — the
    // daemon stashes the pointer as the pane's seed and pi's extension pulls it
    // via `get-seed` → `pi.sendUserMessage`, no PTY keystroke injection. This
    // ALSO dissolves the pi-specific fragility the old path had: pi never emits
    // `EventType::SessionStart`, so `wait_for_session_start` always burned the
    // full ~10s timeout before injecting into a maybe-not-yet-ready pane. A
    // `clear = false` pi worker (no respawn → no `session_start`) keeps the
    // legacy injection — the native pull needs a fresh session to fire on, so
    // mid-session re-delegation is a documented further enhancement.
    let is_pi_native = role_config
        .as_ref()
        .map(|r| r.clear && AgentType::from_command(Some(&r.command)) == Some(AgentType::Pi))
        .unwrap_or(false);

    // Honor the per-role `clear` flag from `.dot-agent-deck.toml`.
    // `clear = true` terminates the existing worker child (SIGTERM
    // with grace, then SIGKILL via
    // `terminate_child_with_grace_and_wait`) and spawns a fresh
    // one with the same `pane_id_env` and identity — the dashboard
    // card stays put, the PID rolls over, and the agent's
    // conversation history is gone. `clear = false` preserves the
    // agent across delegations — no respawn, just the prompt
    // write below. Missing role config defaults to no respawn:
    // we have no `command` to spawn even if `clear` were `true`.
    if let Some(role) = role_config.as_ref()
        && role.clear
    {
        // CRITICAL race-avoidance (PRD #92 F9 followup-6): subscribe
        // BEFORE the new process is forked. `broadcast::Receiver`
        // attaches to future sends; creating it after `respawn_agent_for_pane`
        // returns would race a fast-booting agent that emits
        // `SessionStart` before our `subscribe()` call lands. With
        // the order below the receiver is guaranteed to see every
        // event sent after `event_tx.subscribe()` — including the
        // new agent's first `SessionStart`.
        let mut event_rx = event_tx.subscribe();
        match registry
            .respawn_agent_for_pane(&pane_id, &role.command)
            .await
        {
            Ok(new_agent_id) => {
                if is_pi_native {
                    // PRD #201: NATIVE delivery — stash the pointer as the
                    // respawned pi's seed and arm the PTY-injection safety net.
                    // Skip the `SessionStart` wait (pi never emits
                    // `EventType::SessionStart`, so it would just burn the full
                    // timeout) and skip the inline injection below (`return`):
                    // pi's extension pulls the seed on `session_start` via
                    // `get-seed` → `sendUserMessage`.
                    tracing::debug!(
                        role = %target_role,
                        pane_id = %pane_id,
                        new_agent_id = %new_agent_id,
                        "delegate: pi worker respawned for clear=true; \
                         stashing seed for native get-seed pull (no injection)"
                    );
                    registry.set_pending_seed(&pane_id, &one_liner);
                    crate::agent_pty::arm_seed_fallback(
                        registry.clone(),
                        pane_id.clone(),
                        crate::agent_pty::seed_fallback_grace(),
                    );
                    return;
                }
                tracing::debug!(
                    role = %target_role,
                    pane_id = %pane_id,
                    new_agent_id = %new_agent_id,
                    timeout_secs = SESSION_START_WAIT_TIMEOUT.as_secs(),
                    "delegate: respawned worker agent for clear=true; \
                     waiting for SessionStart on hook broadcast"
                );
                // PRD #92 F9 followup-7: scope the wait to the NEW
                // agent's id so a late `SessionStart` from the OLD
                // agent (which carried the OLD id, injected via
                // `DOT_AGENT_DECK_AGENT_ID` at its own spawn time)
                // can't be mis-accepted as the NEW agent's
                // readiness signal.
                let observed = wait_for_session_start(
                    &mut event_rx,
                    &pane_id,
                    &new_agent_id,
                    SESSION_START_WAIT_TIMEOUT,
                )
                .await;
                if !observed {
                    tracing::debug!(
                        role = %target_role,
                        pane_id = %pane_id,
                        timeout_secs = SESSION_START_WAIT_TIMEOUT.as_secs(),
                        "delegate: SessionStart wait timed out; \
                         writing prompt via fallback path"
                    );
                }
            }
            Err(e) => {
                // The respawn failed AFTER the terminate phase
                // already disposed of the previous child.
                // Without surfacing the error to the operator,
                // the worker pane is left with no live agent,
                // the subsequent prompt write also fails
                // with `NotFound`, and the user sees nothing in
                // the TUI — just two log lines somewhere
                // off-screen. The full error stays in the
                // daemon log via the `tracing::warn!` below;
                // the notice written into the orchestrator
                // pane's scrollback is a high-level message so
                // a stray filesystem path (or other detail
                // from `AgentPtyError::Spawn`) doesn't leak
                // into the orchestrator LLM's view. Using
                // `write_to_pane_notice` (no SUBMIT_DELAY, LF
                // tail instead of CR) means the notice forms a
                // visible line in scrollback without an Enter
                // — the orchestrator's LLM sees it as
                // scrollback noise, not a user prompt to
                // respond to.
                warn!(
                    pane_id = %pane_id,
                    role = %target_role,
                    error = %e,
                    "delegate: respawn for clear=true failed; \
                     surfacing high-level notice in orchestrator \
                     pane and skipping the subsequent prompt write"
                );
                let notice = format!(
                    "⚠ respawn failed for role '{target_role}' on pane \
                     {pane_id} (see daemon log for details)"
                );
                if let Err(write_err) = registry
                    .write_to_pane_notice(&orchestrator_pane_id, &notice)
                    .await
                {
                    warn!(
                        pane_id = %orchestrator_pane_id,
                        role = %target_role,
                        error = %write_err,
                        "delegate: failed to surface respawn error in \
                         orchestrator pane scrollback"
                    );
                }
                // Skip the post-respawn prompt write — there is
                // no live worker agent on this pane to receive
                // it, and the submit-write would just log a
                // second `NotFound`.
                return;
            }
        }
    }
    // Legacy PTY injection for every non-pi-native path: claude / opencode
    // workers, and `clear = false` pi workers (which get no fresh
    // `session_start` for the extension to pull on). The pi-native `clear =
    // true` path returned early above after stashing the seed.
    if let Err(e) = registry
        .write_to_pane_and_submit(&pane_id, &one_liner)
        .await
    {
        warn!(
            pane_id = %pane_id,
            role = %target_role,
            error = %e,
            "delegate: failed to write task prompt into target pane"
        );
    }
}

/// PRD #20 blocker-4: build an inert [`AgentEvent`] that carries only a
/// `live_target`, used to re-seed a reconnected session's durable write-
/// semantics into `recent_events`. Its `Idle` type and empty tool/prompt fields
/// mean the card's activity renderers (`collect_recent_prompts`,
/// `recent_tool_lines`) ignore it; only [`SessionState::live_target`] reads it.
fn live_target_carrier_event(session: &SessionState, live_target: LiveTarget) -> AgentEvent {
    AgentEvent {
        session_id: session.session_id.clone(),
        agent_type: session.agent_type.clone(),
        event_type: EventType::Idle,
        tool_name: None,
        tool_detail: None,
        cwd: session.cwd.clone(),
        timestamp: session.last_activity,
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: session.pane_id.clone(),
        agent_id: session.agent_id.clone(),
        agent_version: None,
        schema_version: None,
        live_target: Some(live_target),
    }
}

impl AppState {
    pub fn aggregate_stats(&self) -> DashboardStats {
        let mut stats = DashboardStats::default();
        for session in self.sessions.values() {
            if session.agent_type == AgentType::None {
                continue;
            }
            stats.active += 1;
            match session.status {
                SessionStatus::Working => stats.working += 1,
                SessionStatus::Thinking => stats.thinking += 1,
                SessionStatus::WaitingForInput => stats.waiting += 1,
                SessionStatus::Error => stats.errors += 1,
                SessionStatus::Idle => stats.idle += 1,
                SessionStatus::Compacting => stats.compacting += 1,
                // PRD #162 forward-compat: an unknown wire status is bucketed
                // as idle so it never inflates an active-work tally.
                SessionStatus::Unknown => stats.idle += 1,
            }
            stats.total_tools += session.tool_count as u64;
        }
        // PRD #20 finding #10: per-agent-type active counts in stable registry
        // (`ALL`) order, so the rendered bar / snapshot is deterministic. Only
        // types with at least one active session are included.
        stats.by_agent_type = crate::agent_registry::ALL
            .iter()
            .filter_map(|spec| {
                let count = self
                    .sessions
                    .values()
                    .filter(|s| s.agent_type == spec.agent_type)
                    .count();
                (count > 0).then(|| (spec.agent_type.clone(), count))
            })
            .collect();
        stats
    }

    /// PRD #20 M3: the write-semantics of the live session bound to `pane_id`.
    ///
    /// The daemon's [`crate::daemon_protocol::AttachRequest::WriteAndSubmit`]
    /// handler calls this to decide whether input should actually be delivered
    /// or reported as history-only / no-live-target. Resolves the session on the
    /// pane (newest by `last_activity` if a `/clear` restart left more than one)
    /// and reads its [`SessionState::writable`]. A pane with no live session
    /// defaults to [`Writable::Live`] so the historical PTY write path is
    /// unaffected — only a session that explicitly declared a non-live
    /// live_target (a wrapped Codex pane) reports otherwise.
    pub fn pane_writable(&self, pane_id: &str) -> Writable {
        self.sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some(pane_id))
            .max_by_key(|s| s.last_activity)
            .map(|s| s.writable())
            .unwrap_or(Writable::Live)
    }

    /// PRD #20 R20-003: the `session_id` of the newest live session bound to
    /// `pane_id` (same newest-by-`last_activity` resolution as
    /// [`Self::pane_writable`]), or `None` when the pane carries no session.
    ///
    /// The daemon's atomic write-and-submit guard compares this against the
    /// session id the prompt was queued for: if a DIFFERENT session now owns the
    /// pane (a `/clear` restart or respawn replaced it), the prompt is stale and
    /// must not be delivered to the replacement. `None` means "no session
    /// declared" — the guard treats that as a match (the legacy native-PTY
    /// default, consistent with `pane_writable` defaulting to `Live`).
    pub fn pane_session_id(&self, pane_id: &str) -> Option<String> {
        self.sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some(pane_id))
            .max_by_key(|s| s.last_activity)
            .map(|s| s.session_id.clone())
    }

    /// PRD #20 R20-003 (finding #4): the DAEMON-AUTHORITATIVE hook session id
    /// (generation) currently bound to `pane_id`, or `None` when the pane has no
    /// live hook session (only a placeholder, or the agent ended).
    ///
    /// Unlike [`Self::pane_session_id`] — which returns the *card* id that the
    /// same-agent reuse guard deliberately keeps STABLE across a `/clear` for UI
    /// continuity — this reflects the LATEST hook `session_id` the pane's agent
    /// actually reported (see [`AppState::pane_hook_session`]). The atomic
    /// write-and-submit guard compares a caller's `expected_session_id` against
    /// THIS value and requires an EXACT match: a same-agent `/clear` / thread
    /// restart rolls the generation over, so an old queued prompt is refused.
    /// A `None` here with an expected session supplied is a REJECTION, not a
    /// silent accept — the queued generation no longer exists.
    pub fn pane_hook_session_id(&self, pane_id: &str) -> Option<String> {
        self.pane_hook_session.get(pane_id).cloned()
    }

    /// Register a pane ID as managed by our app.
    pub fn register_pane(&mut self, pane_id: String) {
        self.managed_pane_ids.insert(pane_id);
    }

    /// PRD #120: record a daemon-spawned orchestration for the render loop to
    /// build into a live tab. Called from the event subscriber, which receives
    /// the [`BroadcastMsg::OrchestrationSurface`] but cannot touch the
    /// `TabManager` / pane controller (those live on the TUI render thread).
    pub fn queue_orchestration_surface(&mut self, surface: OrchestrationSurface) {
        // PRD #120 L1: bound the queue. If it's already at the cap, drop the
        // OLDEST entry to make room — a flood can't grow it unbounded, and the
        // freshest dispatch is the one most worth surfacing. Log the drop so it
        // stays observable.
        if self.pending_orchestration_surfaces.len() >= MAX_PENDING_ORCHESTRATION_SURFACES {
            let dropped = self.pending_orchestration_surfaces.remove(0);
            tracing::warn!(
                orchestration = %dropped.name,
                cap = MAX_PENDING_ORCHESTRATION_SURFACES,
                "queue_orchestration_surface: pending queue at cap; dropping oldest surface"
            );
        }
        self.pending_orchestration_surfaces.push(surface);
    }

    /// Create a placeholder session for a newly created pane so it always
    /// has a dashboard card.
    ///
    /// PRD #76 M2.13: `agent_type` lets the hydration path on remote
    /// reconnect seed the placeholder with the daemon's known agent type
    /// (carried via `AgentRecord.agent_type`) instead of defaulting to
    /// `AgentType::None` — which the dashboard renderer labels as
    /// "No agent" until a real `SessionStart` hook fires (and on
    /// reconnect, no hook fires because the agent was already running).
    /// Local-mode callers and session-end restorers pass `None`; their
    /// `agent_type` gets filled in later from the next hook event via
    /// [`AppState::apply_event`].
    ///
    /// PRD #110 followup: `agent_id` is the daemon-side registry id of
    /// the agent that owns this pane. The strict-equality reuse guard in
    /// [`AppState::apply_event`] requires the placeholder's `agent_id` to
    /// match the next `SessionStart` event's `agent_id`, otherwise a
    /// duplicate card appears beside the placeholder. Three callers know
    /// the correct id at mint time and must pass it: brand-new pane
    /// creation (daemon returns the id from `start_agent`), reconnect
    /// hydration (`HydratedPane.agent_id`), and `SessionEnd` restoration
    /// in `apply_event` (the dying session's `agent_id`). Pass `None`
    /// only for backward-compat callers / pre-F9 hook scripts that don't
    /// emit `agent_id`.
    pub fn insert_placeholder_session(
        &mut self,
        pane_id: String,
        cwd: Option<String>,
        agent_type: Option<AgentType>,
        agent_id: Option<String>,
    ) {
        let session_id = format!("pane-{}", pane_id);
        let now = Utc::now();
        let started_at = self.pane_started_at.get(&pane_id).copied().unwrap_or(now);
        self.sessions.insert(
            session_id.clone(),
            SessionState {
                session_id,
                agent_type: agent_type.unwrap_or(AgentType::None),
                cwd,
                status: SessionStatus::Idle,
                active_tool: None,
                started_at,
                last_activity: now,
                recent_events: VecDeque::new(),
                tool_count: 0,
                last_user_prompt: None,
                first_prompts: Vec::new(),
                pane_id: Some(pane_id),
                agent_id,
                display_name: None,
            },
        );
    }

    /// PRD #162: seed a hydrated pane's session from the daemon's live
    /// [`SessionSnapshot`] when one is attached, falling back to the bare
    /// [`Self::insert_placeholder_session`] placeholder when it is absent.
    ///
    /// This is the reconnect-side counterpart to the `ListAgents` snapshot
    /// join: on `dot-agent-deck connect`, each `HydratedPane` carries the
    /// agent's live state (`status` / event-derived `agent_type` /
    /// `active_tool` / `tool_count` / prompt context), and seeding from it
    /// restores the pre-disconnect card instead of resetting to `Idle` /
    /// "No agent" until the next event arrives.
    ///
    /// - `live = Some(snap)`: the card takes the snapshot's `status` /
    ///   `active_tool` / `tool_count` / `first_prompts` / `last_user_prompt`,
    ///   and its `agent_type` is the snapshot's **event-derived** value —
    ///   falling back to the spawn-time `agent_type` argument **only** when
    ///   the snapshot's is `None` (the "No agent" fix).
    /// - `live = None`: behaves identically to
    ///   [`Self::insert_placeholder_session`] (bare `Idle`, spawn-time
    ///   `agent_type`). The fallback delegates to that method so it can't
    ///   drift from the placeholder path.
    ///
    /// In BOTH branches the PRD #110 `agent_id` is minted on the seeded
    /// session exactly as `insert_placeholder_session` does, so a
    /// post-reconnect `SessionStart` from the same agent remaps onto this
    /// card via `apply_event`'s reuse guard instead of spawning a duplicate.
    pub fn seed_hydrated_session(
        &mut self,
        pane_id: String,
        cwd: Option<String>,
        agent_type: Option<AgentType>,
        agent_id: Option<String>,
        live: Option<&SessionSnapshot>,
    ) {
        // The snapshot's event-derived agent_type wins; fall back to the
        // spawn-time value only when the snapshot has none (or is absent).
        let effective_agent_type = match live {
            Some(snap) => snap.agent_type.clone().or(agent_type),
            None => agent_type,
        };
        // Mint the placeholder exactly as today (PRD #110 agent_id,
        // started_at reuse, session_id), then overlay the live snapshot
        // fields when one is present.
        self.insert_placeholder_session(pane_id.clone(), cwd, effective_agent_type, agent_id);
        if let Some(snap) = live {
            let session_id = format!("pane-{}", pane_id);
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.status = snap.status.clone();
                session.active_tool = snap.active_tool.clone();
                session.tool_count = snap.tool_count;
                session.first_prompts = snap.first_prompts.clone();
                session.last_user_prompt = snap.last_user_prompt.clone();
                // PRD #20 blocker-4: restore the durable live-target so a
                // history-only / view-only card keeps refusing input right
                // after reconnect, before any new event re-declares it. The
                // descriptor lives in `recent_events` (no dedicated field —
                // uneditable fixtures build `SessionState` by exhaustive
                // literal), so re-seed it as a single inert carrier event. It
                // sets no prompt/tool, so the card's activity renderers ignore
                // it; `apply_event`'s forward-stamping then keeps it durable.
                if let Some(live_target) = snap.live_target {
                    session
                        .recent_events
                        .push_back(live_target_carrier_event(session, live_target));
                }
            }
        }
    }

    /// Unregister a pane ID (e.g., when closing a pane).
    pub fn unregister_pane(&mut self, pane_id: &str) {
        self.managed_pane_ids.remove(pane_id);
        self.pane_role_map.remove(pane_id);
        self.pane_cwd_map.remove(pane_id);
        self.orchestrator_pane_ids.remove(pane_id);
        self.pane_orchestration_map.remove(pane_id);
    }

    /// Handle an orchestrator's delegate signal: validate the sender, look
    /// up each target role's pane, and write the task prompt into that
    /// pane's PTY directly.
    ///
    /// PRD #93 round-5: this used to enqueue into `delegate_events` for the
    /// TUI to drain. The TUI's `dispatch_delegate_events` did the role →
    /// pane resolution, built the prompt, and wrote it via the pane
    /// controller. That model required the daemon to broadcast the signal
    /// across the attach socket — a hop that lost messages whenever the
    /// deck was detached. Now the daemon owns the flow end to end: it has
    /// the role map (populated at `StartAgent` time), the cwd map, and the
    /// PTY registry, so it builds the file-backed prompt and writes the
    /// one-liner directly into the target PTY. The bytes land in the
    /// pane's scrollback like any other terminal output, surviving any
    /// number of detach/reattach cycles via the standard pane snapshot
    /// replay.
    ///
    /// The orchestrator pane that issued the delegate is identified by
    /// presence in `orchestrator_pane_ids`; non-orchestrator senders are
    /// rejected as anti-spoofing. Targets are restricted to panes in the
    /// same orchestration (via `pane_orchestration_map`) so a parallel
    /// orchestration tab's `coder` pane doesn't receive a sibling tab's
    /// task.
    pub async fn handle_delegate(
        &self,
        signal: DelegateSignal,
        registry: &Arc<AgentPtyRegistry>,
        event_tx: &broadcast::Sender<BroadcastMsg>,
    ) {
        if !self.pane_role_map.contains_key(&signal.pane_id) {
            warn!(pane_id = %signal.pane_id, "delegate from unknown pane");
            return;
        }
        if !self.orchestrator_pane_ids.contains(&signal.pane_id) {
            let role = self
                .pane_role_map
                .get(&signal.pane_id)
                .cloned()
                .unwrap_or_default();
            warn!(pane_id = %signal.pane_id, role = %role, "delegate from non-orchestrator pane");
            return;
        }

        let orchestration = self.pane_orchestration_map.get(&signal.pane_id).cloned();

        // Collect every (target_role, pane_id) the delegate fans out to.
        // Per-role filtering: same orchestration; never the orchestrator's
        // own pane (a role that names itself is almost certainly a
        // misconfiguration; we don't want the orchestrator's pane to be
        // fed its own delegate prompt).
        let mut targets: Vec<(String, String)> = Vec::new();
        for target_role in &signal.to {
            let mut role_panes: Vec<String> = self
                .pane_role_map
                .iter()
                .filter(|(pane_id, role)| {
                    role.as_str() == target_role.as_str()
                        && !self.orchestrator_pane_ids.contains(pane_id.as_str())
                        && self.pane_orchestration_map.get(pane_id.as_str()).cloned()
                            == orchestration
                })
                .map(|(pane_id, _)| pane_id.clone())
                .collect();
            if role_panes.is_empty() {
                warn!(role = %target_role, "delegate: no worker pane found for role");
                continue;
            }
            for pane_id in role_panes.drain(..) {
                targets.push((target_role.clone(), pane_id));
            }
        }

        // PRD #92 F9 followup-6: async-dispatch. Each per-target future
        // runs in its own `tokio::spawn` so `handle_delegate` (and the
        // delegate CLI on the other end of the hook socket) returns
        // immediately once the dispatches are queued. The freshly-spawned
        // agent's `SessionStart` event arrives over the daemon-wide hook
        // broadcast some time after `respawn_agent_for_pane` returns —
        // blocking the hook-loop reply on that wait was unnecessary and
        // made the CLI feel synchronous to a multi-second boot.
        //
        // Critical race-avoidance: the subscribe-before-spawn ordering
        // lives inside `dispatch_one_owned`. The receiver attaches to
        // `event_tx` *before* `respawn_agent_for_pane` forks the new
        // process, so a fast-booting agent that fires `SessionStart`
        // immediately after exec can't race the dispatch task's
        // subscription.
        //
        // Cross-pane fan-out remains concurrent (different panes' tasks
        // overlap); per-pane work still serializes against itself via
        // the per-pane dispatch mutex acquired inside the task body —
        // see [`AgentPtyRegistry::pane_dispatch_lock`].
        for (target_role, pane_id) in targets {
            let registry = Arc::clone(registry);
            let event_tx = event_tx.clone();
            let orchestration = orchestration.clone();
            let orchestrator_pane_id = signal.pane_id.clone();
            let task = signal.task.clone();
            let cwd = self.pane_cwd_map.get(&pane_id).cloned();
            tokio::spawn(async move {
                dispatch_one_owned(
                    registry,
                    event_tx,
                    orchestration,
                    orchestrator_pane_id,
                    target_role,
                    pane_id,
                    task,
                    cwd,
                )
                .await;
            });
        }
    }

    /// Handle a worker's work-done signal: write the per-role summary file
    /// and inject a one-liner pointing the orchestrator pane at it.
    ///
    /// PRD #93 round-5: the file write was already daemon-side (now that
    /// the daemon owns `pane_cwd_map`); the new piece is that the daemon
    /// also picks the orchestrator pane for the same orchestration and
    /// writes the "Worker {role} has completed..." feedback directly into
    /// its PTY via [`AgentPtyRegistry::write_to_pane_and_submit`]. No broadcast hop —
    /// the bytes sit in the orchestrator pane's scrollback, surviving any
    /// number of detach/reattach cycles.
    ///
    /// `done: true` from the orchestrator pane itself signals the whole
    /// orchestration is complete; we log and exit without writing back a
    /// "completed" prompt to the orchestrator (it just issued it).
    pub async fn handle_work_done(&self, signal: WorkDoneSignal, registry: &AgentPtyRegistry) {
        let role_name = match self.pane_role_map.get(&signal.pane_id) {
            Some(name) => name.clone(),
            None => {
                warn!(pane_id = %signal.pane_id, "work-done from unknown pane");
                return;
            }
        };

        // Orchestrator's own `--done`: completion signal, no feedback to write.
        if signal.done && self.orchestrator_pane_ids.contains(&signal.pane_id) {
            tracing::info!(
                pane_id = %signal.pane_id,
                task = %signal.task,
                "orchestration complete (orchestrator --done)"
            );
            return;
        }

        // Write summary to .dot-agent-deck/work-done-{role}.md
        let safe_name = sanitize_role_name(&role_name);
        if let Some(cwd) = self.pane_cwd_map.get(&signal.pane_id) {
            let dir = std::path::Path::new(cwd).join(".dot-agent-deck");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                warn!(dir = %dir.display(), role = %role_name, error = %e, "failed to create work-done directory");
            }
            let file_path = dir.join(format!("work-done-{safe_name}.md"));
            if let Err(e) = std::fs::write(&file_path, &signal.task) {
                warn!(path = %file_path.display(), role = %role_name, error = %e, "failed to write work-done summary");
            }
        }

        // Find the orchestrator pane in the same orchestration as the
        // worker. We scope by `pane_orchestration_map` so a parallel
        // orchestration tab's orchestrator pane doesn't receive a sibling
        // tab's worker feedback.
        let orchestration = self.pane_orchestration_map.get(&signal.pane_id);
        let orchestrator_pane_id = self
            .orchestrator_pane_ids
            .iter()
            .find(|p| self.pane_orchestration_map.get(p.as_str()) == orchestration)
            .cloned();

        let Some(orch_pane_id) = orchestrator_pane_id else {
            warn!(
                pane_id = %signal.pane_id,
                role = %role_name,
                "work-done: no orchestrator pane found for this orchestration"
            );
            return;
        };

        // If the work-done came from the orchestrator itself (without
        // --done), skip the feedback write — the orchestrator doesn't need
        // to be reminded of its own work.
        if signal.pane_id == orch_pane_id {
            return;
        }

        let feedback = format!(
            "Worker {safe_name} has completed their task. \
             Read .dot-agent-deck/work-done-{safe_name}.md for their full report."
        );
        if let Err(e) = registry
            .write_to_pane_and_submit(&orch_pane_id, &feedback)
            .await
        {
            warn!(
                pane_id = %orch_pane_id,
                role = %role_name,
                error = %e,
                "work-done: failed to write feedback into orchestrator pane"
            );
        }
    }

    pub fn apply_event(&mut self, mut event: AgentEvent) {
        // PRD #20 R20-003 (finding #4): the ORIGINAL hook `session_id` on the
        // wire, captured BEFORE the same-agent reuse guard below remaps it onto
        // the stable card id. This is the generation the daemon's send guard
        // compares against — see [`Self::pane_hook_session`].
        let incoming_session_id = event.session_id.clone();
        // Only accept events from panes managed by our app.
        // Events without a pane_id (external agents) are rejected when we have
        // managed panes. Events with an unknown pane_id are rejected unless it
        // is a SessionStart (which may arrive before register_pane during startup).
        if let Some(ref pane_id) = event.pane_id {
            if !self.managed_pane_ids.contains(pane_id) {
                if event.event_type == EventType::SessionStart {
                    // Defense in depth (auditor finding #1 follow-up):
                    // reject the synthetic dead-slot id format from the
                    // auto-register branch so a forged hook event can't
                    // bring an `__dead-slot__-…` id into existence.
                    // Production never sets a synthetic id as
                    // `DOT_AGENT_DECK_PANE_ID`, but `is_valid_pane_id_env`
                    // admits the format on its own (it only checks for
                    // `[A-Za-z0-9_-]`).
                    if crate::ui::is_dead_slot_pane_id(pane_id) {
                        return;
                    }
                    // Auto-register the pane to handle the startup race where
                    // the hook fires before register_pane is called.
                    self.managed_pane_ids.insert(pane_id.clone());
                } else {
                    return;
                }
            }
        } else if !self.managed_pane_ids.is_empty() {
            return;
        }
        // PRD #110: reuse the existing session card for the same pane
        // ONLY when the agent_id matches (or both sides are absent for
        // pre-F9 backward-compat). A different agent_id means the agent
        // process was intentionally respawned (clear=true delegate);
        // we let that event create a fresh session card instead of
        // remapping it onto the dead session.
        if let Some(ref pane_id) = event.pane_id
            && let Some(existing_id) = self.sessions.iter().find_map(|(id, session)| {
                (session.pane_id.as_ref().is_some_and(|p| p == pane_id)
                    && id != &event.session_id
                    && session.agent_id == event.agent_id)
                    .then(|| id.clone())
            })
        {
            let old_id = std::mem::replace(&mut event.session_id, existing_id);
            if old_id != event.session_id {
                self.sessions.remove(&old_id);
            }
        }

        // PRD #110 follow-up: when a `SessionStart` arrives whose
        // `agent_id` differs from an existing session on the same
        // pane, the previous agent has been replaced (F9 clear=true
        // respawn — the daemon SIGKILLs the old child so no graceful
        // `SessionEnd` ever fires). The same-agent reuse guard above
        // doesn't match, so without retiring the stale session here
        // the dashboard would end up with two cards on the same pane:
        // the dead-agent's card AND the fresh agent's card. Drop the
        // stale sibling(s) before falling through to the
        // session-create path below so the orchestration deck shows
        // exactly one card per pane after a respawn.
        //
        // Backward-compat (auditor finding #3 follow-up; reaffirmed
        // against CodeRabbit PR #118 finding #1): skip the retire
        // block entirely when the incoming event carries no
        // `agent_id`. A pre-F9 hook script (no
        // `DOT_AGENT_DECK_AGENT_ID` env var) running against an
        // upgraded daemon would otherwise wipe a tagged session it
        // doesn't know the identity of — losing its `recent_events`,
        // `tool_count`, `first_prompts`, `started_at`. Mirrors the
        // deliberately-permissive "both sides absent" branch of the
        // reuse guard above.
        //
        // Trade-off: keeping this guard means a legacy hook can
        // create a duplicate (untagged) card alongside the tagged
        // one. Removing it (CodeRabbit's wildcard suggestion on PR
        // #118) would silently drop accumulated history every time
        // an old hook fires. PRD #110 prefers the visible duplicate
        // over silent data loss; the duplicate is observable and
        // self-resolves once the legacy hook is upgraded, whereas
        // lost `recent_events` / `tool_count` / `first_prompts` are
        // not recoverable. The pinned shape lives in the regression
        // test `pre_f9_hook_with_no_agent_id_does_not_wipe_tagged_session`
        // below.
        //
        // PRD #127 finding #2: the `display_name` lives on the session, not
        // the pane, so retiring the superseded session would drop the
        // friendly title — e.g. a scheduler's synthetic live-surface
        // placeholder (`agent_id=None`, `display_name=<task name>`) replaced
        // by the agent's real `SessionStart` (a distinct `Some(agent_id)`, no
        // display_name metadata). Capture the retired session's friendly name,
        // keyed by the stable pane, so the replacement created below can
        // inherit it when the superseding event carries none.
        let mut inherited_display_name: Option<String> = None;
        if event.event_type == EventType::SessionStart
            && event.agent_id.is_some()
            && let Some(ref pane_id) = event.pane_id
        {
            let to_remove: Vec<String> = self
                .sessions
                .iter()
                .filter(|(id, session)| {
                    session.pane_id.as_ref().is_some_and(|p| p == pane_id)
                        && *id != &event.session_id
                        && session.agent_id != event.agent_id
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in to_remove {
                if let Some(removed) = self.sessions.remove(&id) {
                    // First non-empty friendly name on this pane wins.
                    if inherited_display_name.is_none() {
                        inherited_display_name = removed.display_name;
                    }
                }
            }
        }

        if event.event_type == EventType::SessionEnd {
            // PRD #20 R20-003 (finding #4): the agent ended, so drop the pane's
            // hook-session generation. A prompt queued for the now-dead session
            // then hits a `None` current-session in the send guard and is
            // refused (a `None` with an expected session is a rejection, never a
            // silent accept).
            if let Some(ref pane_id) = event.pane_id {
                self.pane_hook_session.remove(pane_id);
            }
            // Preserve started_at for the pane so a restarted session keeps its position.
            //
            // PRD #110 followup: also capture the dying session's `agent_id`
            // so the restored placeholder carries it forward. Without this,
            // a placeholder born with `agent_id=None` would not satisfy the
            // strict-equality reuse guard when the SAME agent fires its
            // next `SessionStart` (e.g. Claude `/clear`, opencode
            // `session.deleted`) — the natural reload would orphan the
            // placeholder next to a fresh card. A DIFFERENT agent
            // (F9 clear=true respawn) still produces a fresh card because
            // the agent_ids no longer match.
            let pane_id_cwd_and_agent_id =
                self.sessions.get(&event.session_id).and_then(|session| {
                    session.pane_id.as_ref().map(|pid| {
                        self.pane_started_at.insert(pid.clone(), session.started_at);
                        (pid.clone(), session.cwd.clone(), session.agent_id.clone())
                    })
                });
            self.sessions.remove(&event.session_id);
            // Restore a placeholder card so the pane remains visible on the dashboard.
            if let Some((pane_id, cwd, agent_id)) = pane_id_cwd_and_agent_id
                && self.managed_pane_ids.contains(&pane_id)
            {
                // M2.13: a SessionEnd restoration creates a fresh
                // placeholder; `agent_type` is unknown post-end and gets
                // re-populated when the next `SessionStart` hook arrives
                // for this pane. Same default behavior as before M2.13.
                self.insert_placeholder_session(pane_id, cwd, None, agent_id);
            }
            return;
        }

        // PRD #20 R20-003 (finding #4): record the LATEST hook-session generation
        // for this pane using the ORIGINAL (pre-remap) session id. A same-agent
        // `/clear` mints a new hook session under the SAME agent_id — the reuse
        // guard above remapped `event.session_id` back to the old card id for UI
        // continuity, but the generation tracked here rolls forward, so the send
        // guard refuses an old queued prompt against the new conversation.
        if let Some(ref pane_id) = event.pane_id {
            self.pane_hook_session
                .insert(pane_id.clone(), incoming_session_id.clone());
        }

        let pane_started = event
            .pane_id
            .as_ref()
            .and_then(|pid| self.pane_started_at.get(pid))
            .copied();

        let session = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionState {
                session_id: event.session_id.clone(),
                agent_type: event.agent_type.clone(),
                cwd: event.cwd.clone(),
                status: SessionStatus::Idle,
                active_tool: None,
                started_at: pane_started.unwrap_or(event.timestamp),
                last_activity: event.timestamp,
                recent_events: VecDeque::new(),
                tool_count: 0,
                last_user_prompt: None,
                first_prompts: Vec::new(),
                pane_id: event.pane_id.clone(),
                agent_id: event.agent_id.clone(),
                // PRD #127 finding #2: seed with the friendly name inherited
                // from a session this event just superseded on the same pane
                // (above). The event-metadata case is handled unconditionally
                // by the refresh block below — which takes precedence — so we
                // do NOT recompute it from metadata here (reviewer LOW-2: it
                // was a redundant duplicate of that block).
                display_name: inherited_display_name,
            });

        session.last_activity = event.timestamp;

        // PRD #127 finding #2: a later event carrying the friendly-name
        // metadata refreshes it (the synthetic live-surface `SessionStart`
        // sets it; ordinary hooks omit the key and leave it untouched). This
        // takes precedence over any name inherited from a superseded session.
        if let Some(name) = event
            .metadata
            .get(DISPLAY_NAME_METADATA_KEY)
            .filter(|n| !n.is_empty())
        {
            session.display_name = Some(name.clone());
        }

        if session.agent_type == AgentType::None && event.agent_type != AgentType::None {
            session.agent_type = event.agent_type.clone();
        }

        if event.cwd.is_some() {
            session.cwd.clone_from(&event.cwd);
        }

        if let Some(ref prompt) = event.user_prompt {
            session.last_user_prompt = Some(prompt.clone());
            if session.first_prompts.len() < MAX_FIRST_PROMPTS {
                session.first_prompts.push(prompt.clone());
            }
        }

        if event.pane_id.is_some() {
            session.pane_id.clone_from(&event.pane_id);
        }

        match event.event_type {
            EventType::SessionStart => {
                session.status = SessionStatus::Idle;
                session.active_tool = None;
            }
            EventType::Thinking => {
                session.status = SessionStatus::Thinking;
                session.active_tool = None;
            }
            EventType::ToolStart => {
                if session.status != SessionStatus::WaitingForInput {
                    session.status = SessionStatus::Working;
                }
                session.active_tool = Some(ActiveTool {
                    name: event.tool_name.clone().unwrap_or_default(),
                    detail: event.tool_detail.clone(),
                });
            }
            EventType::ToolEnd => {
                session.active_tool = None;
                session.tool_count += 1;
                if session.status == SessionStatus::WaitingForInput {
                    session.status = SessionStatus::Thinking;
                }
            }
            EventType::WaitingForInput | EventType::PermissionRequest => {
                session.status = SessionStatus::WaitingForInput;
            }
            EventType::Idle => {
                session.status = SessionStatus::Idle;
                session.active_tool = None;
            }
            EventType::Compacting => {
                session.status = SessionStatus::Compacting;
                session.active_tool = None;
            }
            EventType::SubagentStart | EventType::SubagentStop => {
                // Informational — recorded in recent_events but no status change
            }
            EventType::Error => {
                session.status = SessionStatus::Error;
            }
            EventType::SessionEnd => unreachable!(),
        }

        // PRD #20 blocker-2: keep the live-target durable across the bounded
        // journal. An event that omits `live_target` inherits the session's
        // last-declared one, so the descriptor is never lost when the original
        // declaring event ages out of `recent_events` (>MAX_RECENT_EVENTS later).
        // A new declaration on the event itself always wins.
        if event.live_target.is_none() {
            event.live_target = session
                .recent_events
                .iter()
                .rev()
                .find_map(|e| e.live_target);
        }

        session.recent_events.push_back(event);
        if session.recent_events.len() > MAX_RECENT_EVENTS {
            session.recent_events.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_delegate_prompt_is_single_line_file_pointer() {
        let prompt =
            compose_delegate_prompt("Read .dot-agent-deck/worker-task-coder.md for your task.");
        assert_eq!(
            prompt,
            "Read .dot-agent-deck/worker-task-coder.md for your task."
        );
        assert!(
            !prompt.contains('\n'),
            "pane-injected delegate prompt must stay single-line"
        );
    }

    #[test]
    fn compose_delegate_prompt_normalizes_multiline_input() {
        let prompt = compose_delegate_prompt("line one\n\nline two\r\nline three");
        assert_eq!(prompt, "line one line two line three");
        assert!(
            !prompt.contains('\n'),
            "pane-injected delegate prompt must normalize newlines"
        );
    }

    #[test]
    fn compose_worker_task_file_appends_work_done_footer() {
        let content = compose_worker_task_file(Some("You are coder."), "Implement the thing.");
        assert!(content.starts_with("You are coder.\n\n## Task\n\nImplement the thing."));
        assert!(
            content.contains("## When done"),
            "task file must include the completion heading"
        );
        assert!(
            content.contains("dot-agent-deck work-done --task"),
            "task file must instruct the worker to call dot-agent-deck work-done"
        );

        let no_template = compose_worker_task_file(None, "Implement the fallback.");
        assert!(no_template.starts_with("Implement the fallback.\n\n## When done"));
    }
}
