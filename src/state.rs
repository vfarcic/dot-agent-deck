use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, broadcast};
use tracing::warn;

use crate::agent_pty::AgentPtyRegistry;
use crate::config_validation::sanitize_role_name;
use crate::event::{
    AgentEvent, AgentType, BroadcastMsg, DelegateSignal, EventType, WorkDoneSignal,
};
use crate::project_config::{OrchestrationRoleConfig, load_project_config};

const MAX_RECENT_EVENTS: usize = 50;
const MAX_FIRST_PROMPTS: usize = 3;

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
const SESSION_START_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Thinking,
    Working,
    Compacting,
    WaitingForInput,
    Idle,
    Error,
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
}

#[derive(Debug, Clone)]
pub struct ActiveTool {
    pub name: String,
    pub detail: Option<String>,
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
}

pub type SharedState = Arc<RwLock<AppState>>;

/// Compose the prompt that the daemon writes into a worker pane on
/// delegation: the caller-supplied `task_body` (typically a one-liner
/// pointing at `.dot-agent-deck/worker-task-{role}.md`), a blank line,
/// then the work-done footer that reminds the worker to signal
/// completion via `dot-agent-deck work-done` once they finish.
///
/// The footer used to be appended per-role by the TUI's
/// `OrchestrationConfig.roles[*].prompt_template` wrapping. PRD #93
/// round-5 moved dispatch into the daemon but left
/// `OrchestrationConfig` out of scope, so without this helper every
/// delegated task lost the footer and workers stopped signaling back.
pub fn compose_delegate_prompt(task_body: &str) -> String {
    format!(
        "{task_body}\n\n## When done\n\n\
         Signal completion by running this command via Bash:\n\
         ```bash\n\
         dot-agent-deck work-done --task \"Brief summary of what you accomplished. Include file paths and outcomes.\"\n\
         ```"
    )
}

/// CodeRabbit (PRD #93 round-9): build the file contents written to
/// `.dot-agent-deck/worker-task-{role}.md` for a delegation. When the
/// role config supplies a `prompt_template`, wrap the task under a
/// `## Task` header beneath the template — mirrors the pre-Round-5 TUI
/// dispatch path that Round 5 lost when it moved orchestration onto
/// the daemon side without bringing the per-role template wrapping
/// along. With no template the file content is the raw task; the
/// PTY-injected one-liner still appends the work-done footer in both
/// shapes via [`compose_delegate_prompt`].
pub fn compose_worker_task_file(prompt_template: Option<&str>, task: &str) -> String {
    match prompt_template {
        Some(tpl) if !tpl.trim().is_empty() => format!("{tpl}\n\n## Task\n\n{task}"),
        _ => task.to_string(),
    }
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
async fn wait_for_session_start(
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
    let one_liner = compose_delegate_prompt(&task_body);
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
            }
            stats.total_tools += session.tool_count as u64;
        }
        stats
    }

    /// Register a pane ID as managed by our app.
    pub fn register_pane(&mut self, pane_id: String) {
        self.managed_pane_ids.insert(pane_id);
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
            },
        );
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
        // Only accept events from panes managed by our app.
        // Events without a pane_id (external agents) are rejected when we have
        // managed panes. Events with an unknown pane_id are rejected unless it
        // is a SessionStart (which may arrive before register_pane during startup).
        if let Some(ref pane_id) = event.pane_id {
            if !self.managed_pane_ids.contains(pane_id) {
                if event.event_type == EventType::SessionStart {
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

        if event.event_type == EventType::SessionEnd {
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
            });

        session.last_activity = event.timestamp;

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

        session.recent_events.push_back(event);
        if session.recent_events.len() > MAX_RECENT_EVENTS {
            session.recent_events.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentType, EventType};
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_event(session_id: &str, event_type: EventType) -> AgentEvent {
        AgentEvent {
            session_id: session_id.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/tmp".to_string()),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
            agent_id: None,
        }
    }

    #[test]
    fn full_session_lifecycle() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Idle);

        let mut tool_event = make_event("s1", EventType::ToolStart);
        tool_event.tool_name = Some("Read".to_string());
        tool_event.tool_detail = Some("main.rs".to_string());
        state.apply_event(tool_event);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(
            state.sessions["s1"].active_tool.as_ref().unwrap().name,
            "Read"
        );

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert!(state.sessions["s1"].active_tool.is_none());

        state.apply_event(make_event("s1", EventType::SessionEnd));
        assert!(!state.sessions.contains_key("s1"));
    }

    #[test]
    fn concurrent_sessions() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s2", EventType::SessionStart));
        assert_eq!(state.sessions.len(), 2);

        let mut tool_event = make_event("s1", EventType::ToolStart);
        tool_event.tool_name = Some("Write".to_string());
        state.apply_event(tool_event);

        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(state.sessions["s2"].status, SessionStatus::Idle);
    }

    #[test]
    fn reuse_session_for_same_pane() {
        let mut state = AppState::default();
        state.register_pane("pane-1".to_string());

        let mut first = make_event("s1", EventType::SessionStart);
        first.pane_id = Some("pane-1".to_string());
        state.apply_event(first);

        let mut restart = make_event("s2", EventType::SessionStart);
        restart.pane_id = Some("pane-1".to_string());
        state.apply_event(restart);

        assert!(state.sessions.contains_key("s1"));
        assert!(!state.sessions.contains_key("s2"));
        assert_eq!(state.sessions["s1"].pane_id.as_deref(), Some("pane-1"));
    }

    // PRD #110: the session-reuse guard now also checks agent_id so the
    // F9 clear=true respawn (which swaps the agent process behind the
    // same pane) creates a fresh session card rather than getting
    // remapped onto the dead session.
    #[test]
    fn reuse_session_when_same_pane_and_same_agent_id() {
        let mut state = AppState::default();
        state.register_pane("pane-1".to_string());

        let mut first = make_event("s1", EventType::SessionStart);
        first.pane_id = Some("pane-1".to_string());
        first.agent_id = Some("agent-A".to_string());
        state.apply_event(first);

        // Same agent process emits SessionStart again (opencode crash
        // or reload). New session_id, but agent_id matches → reuse.
        let mut restart = make_event("s2", EventType::SessionStart);
        restart.pane_id = Some("pane-1".to_string());
        restart.agent_id = Some("agent-A".to_string());
        state.apply_event(restart);

        assert!(state.sessions.contains_key("s1"));
        assert!(!state.sessions.contains_key("s2"));
        assert_eq!(state.sessions["s1"].pane_id.as_deref(), Some("pane-1"));
        assert_eq!(state.sessions["s1"].agent_id.as_deref(), Some("agent-A"));
    }

    #[test]
    fn new_session_when_same_pane_but_different_agent_id() {
        let mut state = AppState::default();
        state.register_pane("pane-1".to_string());

        let mut first = make_event("s1", EventType::SessionStart);
        first.pane_id = Some("pane-1".to_string());
        first.agent_id = Some("agent-A".to_string());
        state.apply_event(first);

        // F9 clear=true respawn — different agent process, same pane.
        // The reuse guard must skip and create a fresh session card.
        let mut respawn = make_event("s2", EventType::SessionStart);
        respawn.pane_id = Some("pane-1".to_string());
        respawn.agent_id = Some("agent-B".to_string());
        state.apply_event(respawn);

        assert!(
            state.sessions.contains_key("s2"),
            "respawn with new agent_id must create a fresh session card"
        );
        assert_eq!(state.sessions["s2"].pane_id.as_deref(), Some("pane-1"));
        assert_eq!(state.sessions["s2"].agent_id.as_deref(), Some("agent-B"));
        // The old session card is preserved (no remap), so the new
        // agent's card is additive rather than replacing the old one.
        assert!(
            state.sessions.contains_key("s1"),
            "old session card must remain when reuse is skipped"
        );
    }

    #[test]
    fn reuse_session_when_same_pane_and_both_agent_ids_absent() {
        // Backward-compat: hook scripts predating F9 followup-7 don't
        // emit `agent_id`. Both sides are None, so the guard treats
        // them as the same agent — reuse, just like before PRD #110.
        let mut state = AppState::default();
        state.register_pane("pane-1".to_string());

        let mut first = make_event("s1", EventType::SessionStart);
        first.pane_id = Some("pane-1".to_string());
        assert!(first.agent_id.is_none());
        state.apply_event(first);

        let mut restart = make_event("s2", EventType::SessionStart);
        restart.pane_id = Some("pane-1".to_string());
        assert!(restart.agent_id.is_none());
        state.apply_event(restart);

        assert!(state.sessions.contains_key("s1"));
        assert!(!state.sessions.contains_key("s2"));
        assert_eq!(state.sessions["s1"].pane_id.as_deref(), Some("pane-1"));
        assert!(state.sessions["s1"].agent_id.is_none());
    }

    #[test]
    fn auto_create_unknown_session() {
        let mut state = AppState::default();

        let mut tool_event = make_event("unknown", EventType::ToolStart);
        tool_event.tool_name = Some("Bash".to_string());
        state.apply_event(tool_event);

        assert!(state.sessions.contains_key("unknown"));
        assert_eq!(state.sessions["unknown"].status, SessionStatus::Working);
    }

    #[test]
    fn event_buffer_capping() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        for _ in 0..60 {
            state.apply_event(make_event("s1", EventType::Idle));
        }

        // 1 SessionStart + 60 Idle = 61, capped to 50
        assert_eq!(state.sessions["s1"].recent_events.len(), 50);
    }

    #[test]
    fn waiting_for_input_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
        assert!(state.sessions["s1"].active_tool.is_none());
    }

    #[test]
    fn notification_during_active_tool_shows_waiting() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // A Notification during an active tool means a permission prompt —
        // PreToolUse fires before the Notification, so active_tool is set.
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
        assert!(state.sessions["s1"].active_tool.is_some());
    }

    #[test]
    fn ask_user_question_shows_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("AskUserQuestion".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // AskUserQuestion is interactive — Notification transitions to WaitingForInput.
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn tool_count_increments_on_tool_end() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert_eq!(state.sessions["s1"].tool_count, 0);

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Read".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].tool_count, 0);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].tool_count, 1);

        let mut tool_start2 = make_event("s1", EventType::ToolStart);
        tool_start2.tool_name = Some("Write".to_string());
        state.apply_event(tool_start2);
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].tool_count, 2);
    }

    #[test]
    fn tool_end_clears_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        // Simulate: PreToolUse → PermissionRequest → tool runs → PostToolUse
        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Thinking);
    }

    #[test]
    fn toolstart_does_not_override_waiting_for_input() {
        // Regression: a concurrent subagent firing PreToolUse while a permission
        // prompt is active must not knock the status back to Working.
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        let mut subagent_tool = make_event("s1", EventType::ToolStart);
        subagent_tool.tool_name = Some("Explore".to_string());
        state.apply_event(subagent_tool);
        assert_eq!(
            state.sessions["s1"].status,
            SessionStatus::WaitingForInput,
            "ToolStart must not override WaitingForInput"
        );
        assert_eq!(
            state.sessions["s1"]
                .active_tool
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Explore"),
            "active_tool must still be updated even when status is preserved"
        );
    }

    #[test]
    fn toolstart_sets_working_when_not_waiting() {
        // Normal flow: ToolStart should still set Working when no permission prompt.
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(
            state.sessions["s1"]
                .active_tool
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Bash"),
            "active_tool must be set on normal ToolStart"
        );
    }

    #[test]
    fn tool_end_preserves_working_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // ToolEnd without permission request should keep Working→Working (not change)
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn error_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::Error));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Error);
    }

    #[test]
    fn last_user_prompt_set_and_persists() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert!(state.sessions["s1"].last_user_prompt.is_none());

        let mut prompt_event = make_event("s1", EventType::Thinking);
        prompt_event.user_prompt = Some("fix the bug".to_string());
        state.apply_event(prompt_event);
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the bug")
        );

        // Subsequent event without prompt should not clear it
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the bug")
        );

        // New prompt replaces old one
        let mut prompt_event2 = make_event("s1", EventType::Thinking);
        prompt_event2.user_prompt = Some("add tests".to_string());
        state.apply_event(prompt_event2);
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("add tests")
        );
    }

    #[test]
    fn first_prompts_captures_up_to_three() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert!(state.sessions["s1"].first_prompts.is_empty());

        let prompts = ["first", "second", "third"];
        for (i, text) in prompts.iter().enumerate() {
            let mut ev = make_event("s1", EventType::Thinking);
            ev.user_prompt = Some(text.to_string());
            state.apply_event(ev);
            assert_eq!(state.sessions["s1"].first_prompts.len(), i + 1);
            assert_eq!(state.sessions["s1"].first_prompts[i], *text);
        }
    }

    #[test]
    fn first_prompts_no_overwrite_after_cap() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        for text in &["p1", "p2", "p3", "p4", "p5"] {
            let mut ev = make_event("s1", EventType::Thinking);
            ev.user_prompt = Some(text.to_string());
            state.apply_event(ev);
        }

        assert_eq!(state.sessions["s1"].first_prompts.len(), 3);
        assert_eq!(state.sessions["s1"].first_prompts[0], "p1");
        assert_eq!(state.sessions["s1"].first_prompts[1], "p2");
        assert_eq!(state.sessions["s1"].first_prompts[2], "p3");
    }

    #[test]
    fn first_prompts_persist_across_events() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut ev = make_event("s1", EventType::Thinking);
        ev.user_prompt = Some("only prompt".to_string());
        state.apply_event(ev);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        state.apply_event(make_event("s1", EventType::Idle));
        state.apply_event(make_event("s1", EventType::Thinking));

        assert_eq!(state.sessions["s1"].first_prompts.len(), 1);
        assert_eq!(state.sessions["s1"].first_prompts[0], "only prompt");
    }

    #[test]
    fn aggregate_stats_empty() {
        let state = AppState::default();
        let stats = state.aggregate_stats();
        assert_eq!(stats, DashboardStats::default());
    }

    #[test]
    fn aggregate_stats_mixed_sessions() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        let mut tool = make_event("s1", EventType::ToolStart);
        tool.tool_name = Some("Read".to_string());
        state.apply_event(tool);
        // s1: Working

        state.apply_event(make_event("s2", EventType::SessionStart));
        state.apply_event(make_event("s2", EventType::WaitingForInput));
        // s2: WaitingForInput

        state.apply_event(make_event("s3", EventType::SessionStart));
        state.apply_event(make_event("s3", EventType::Error));
        // s3: Error

        state.apply_event(make_event("s4", EventType::SessionStart));
        state.apply_event(make_event("s4", EventType::Thinking));
        // s4: Thinking

        state.apply_event(make_event("s5", EventType::SessionStart));
        // s5: Idle

        let stats = state.aggregate_stats();
        assert_eq!(stats.active, 5);
        assert_eq!(stats.working, 1);
        assert_eq!(stats.waiting, 1);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.thinking, 1);
        assert_eq!(stats.idle, 1);
    }

    #[test]
    fn aggregate_stats_tool_count_summation() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        let mut t1 = make_event("s1", EventType::ToolStart);
        t1.tool_name = Some("Read".to_string());
        state.apply_event(t1);
        state.apply_event(make_event("s1", EventType::ToolEnd));

        state.apply_event(make_event("s2", EventType::SessionStart));
        for _ in 0..3 {
            let mut t = make_event("s2", EventType::ToolStart);
            t.tool_name = Some("Bash".to_string());
            state.apply_event(t);
            state.apply_event(make_event("s2", EventType::ToolEnd));
        }

        let stats = state.aggregate_stats();
        assert_eq!(stats.total_tools, 4);
    }

    #[test]
    fn restarted_session_preserves_started_at_via_pane() {
        let mut state = AppState::default();
        state.register_pane("pane-42".to_string());

        // Register session with a pane
        let mut ev = make_event("s1", EventType::SessionStart);
        ev.pane_id = Some("pane-42".to_string());
        state.apply_event(ev);
        let original_started = state.sessions["s1"].started_at;

        // End the session (simulates /clear)
        let mut end_ev = make_event("s1", EventType::SessionEnd);
        end_ev.pane_id = Some("pane-42".to_string());
        state.apply_event(end_ev);
        // After SessionEnd, a placeholder is restored since the pane is still managed.
        // Key is "pane-pane-42" because pane_id="pane-42" and placeholder keys use "pane-{pane_id}".
        assert!(state.sessions.contains_key("pane-pane-42"));

        // New session on the same pane reuses the placeholder key and keeps started_at.
        let mut ev2 = make_event("s2", EventType::SessionStart);
        ev2.pane_id = Some("pane-42".to_string());
        state.apply_event(ev2);
        assert_eq!(state.sessions["pane-pane-42"].started_at, original_started);
    }

    #[test]
    fn permission_request_sets_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn tool_start_preserves_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".into());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn placeholder_session_created() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None, None);

        assert!(state.sessions.contains_key("pane-42"));
        let session = &state.sessions["pane-42"];
        assert_eq!(session.agent_type, AgentType::None);
        assert_eq!(session.status, SessionStatus::Idle);
        assert_eq!(session.pane_id.as_deref(), Some("42"));
        assert_eq!(session.cwd.as_deref(), Some("/tmp"));
        assert_eq!(session.tool_count, 0);
    }

    // PRD #76 M2.13: hydration must seed the placeholder session with
    // the daemon-known agent_type so the dashboard renders the real
    // agent label on reconnect instead of "No agent" until a hook
    // fires (no hook fires on reconnect — the agent was already
    // running). Pin both the "type known" and "type unknown" forks.
    #[test]
    fn placeholder_session_carries_supplied_agent_type() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            Some(AgentType::ClaudeCode),
            None,
        );
        let session = &state.sessions["pane-42"];
        assert_eq!(
            session.agent_type,
            AgentType::ClaudeCode,
            "hydration-supplied agent_type must reach the session, not get overridden to None"
        );
        assert_eq!(
            session.status,
            SessionStatus::Idle,
            "agent_type plumbing must not perturb other placeholder fields"
        );
    }

    #[test]
    fn placeholder_session_defaults_to_none_when_agent_type_unknown() {
        let mut state = AppState::default();
        state.register_pane("99".to_string());
        state.insert_placeholder_session("99".to_string(), Some("/tmp".to_string()), None, None);
        let session = &state.sessions["pane-99"];
        assert_eq!(
            session.agent_type,
            AgentType::None,
            "local-mode (None) callers preserve the pre-M2.13 default"
        );
    }

    #[test]
    fn placeholder_transitions_to_real_session() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None, None);

        let mut start = make_event("real-uuid-123", EventType::SessionStart);
        start.pane_id = Some("42".to_string());
        start.cwd = Some("/home".to_string());
        state.apply_event(start);

        // Placeholder key is reused, real UUID key is removed
        assert!(state.sessions.contains_key("pane-42"));
        assert!(!state.sessions.contains_key("real-uuid-123"));
        let session = &state.sessions["pane-42"];
        assert_eq!(session.agent_type, AgentType::ClaudeCode);
        assert_eq!(session.cwd.as_deref(), Some("/home"));
        assert_eq!(session.pane_id.as_deref(), Some("42"));
    }

    // PRD #110 followup: brand-new pane creation in production today —
    // placeholder is born with `agent_id=None` (we don't know the daemon-
    // assigned id locally), then the agent's first SessionStart arrives
    // with `agent_id=Some(X)`. Pre-followup the strict equality guard
    // rejects reuse → two cards. This probe documents the requirement
    // that brand-new pane creation must mint the placeholder with the
    // daemon's agent_id (plumbed back from `create_pane_with_options`).
    #[test]
    fn placeholder_reused_on_first_session_start_for_brand_new_pane() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        // Simulate the post-fix flow: placeholder is born with the
        // daemon-assigned agent_id (returned from create_pane_with_options).
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            None,
            Some("agent-A".to_string()),
        );

        // First SessionStart from that agent.
        let mut start = make_event("real-uuid", EventType::SessionStart);
        start.pane_id = Some("42".to_string());
        start.agent_id = Some("agent-A".to_string());
        state.apply_event(start);

        // Exactly one card for the pane, keyed on the placeholder id.
        let cards: Vec<&str> = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some("42"))
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(
            cards.len(),
            1,
            "brand-new pane first SessionStart must adopt the placeholder; got {cards:?}"
        );
    }

    // PRD #110 followup: probe the bug from the reviewer + auditor —
    // SessionEnd→SessionStart from the SAME agent must reuse the placeholder
    // restored at SessionEnd time, not orphan it next to a fresh card.
    // Verifies the natural reload path (Claude Code /clear, opencode
    // session.deleted) emits SessionStart with a stable agent_id.
    #[test]
    fn placeholder_reused_when_same_agent_reloads_after_session_end() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());

        // First SessionStart from agent A.
        let mut start1 = make_event("real-uuid-1", EventType::SessionStart);
        start1.pane_id = Some("42".to_string());
        start1.agent_id = Some("agent-A".to_string());
        state.apply_event(start1);
        assert_eq!(
            state.sessions["real-uuid-1"].agent_id.as_deref(),
            Some("agent-A")
        );

        // SessionEnd: agent A's session ends. A placeholder is restored.
        let mut end = make_event("real-uuid-1", EventType::SessionEnd);
        end.pane_id = Some("42".to_string());
        end.agent_id = Some("agent-A".to_string());
        state.apply_event(end);
        assert!(
            state.sessions.contains_key("pane-42"),
            "SessionEnd must restore a placeholder for the same pane"
        );

        // Agent A emits a fresh SessionStart (natural reload).
        let mut start2 = make_event("real-uuid-2", EventType::SessionStart);
        start2.pane_id = Some("42".to_string());
        start2.agent_id = Some("agent-A".to_string());
        state.apply_event(start2);

        // Exactly one card for the pane, bound to agent-A.
        let cards: Vec<&str> = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some("42"))
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(
            cards.len(),
            1,
            "natural reload must produce exactly one card; got {cards:?} (sessions: {:?})",
            state.sessions.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            state.sessions[cards[0]].agent_id.as_deref(),
            Some("agent-A")
        );
    }

    // PRD #110 followup: SessionEnd from agent A, then a DIFFERENT agent
    // (F9 clear=true respawn) emits SessionStart. The placeholder must NOT
    // be remapped onto the new agent — it represents the dead agent A, and
    // adopting it would silently rebrand the new agent's card.
    #[test]
    fn placeholder_not_reused_when_different_agent_starts_after_session_end() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());

        let mut start1 = make_event("real-uuid-1", EventType::SessionStart);
        start1.pane_id = Some("42".to_string());
        start1.agent_id = Some("agent-A".to_string());
        state.apply_event(start1);

        let mut end = make_event("real-uuid-1", EventType::SessionEnd);
        end.pane_id = Some("42".to_string());
        end.agent_id = Some("agent-A".to_string());
        state.apply_event(end);

        // Different agent (B) emits SessionStart for the same pane.
        let mut start2 = make_event("real-uuid-2", EventType::SessionStart);
        start2.pane_id = Some("42".to_string());
        start2.agent_id = Some("agent-B".to_string());
        state.apply_event(start2);

        // Two cards expected: the placeholder bound to agent-A (the dead
        // session's restoration card), and a fresh card for agent-B.
        let mut cards: Vec<(&str, Option<&str>)> = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some("42"))
            .map(|s| (s.session_id.as_str(), s.agent_id.as_deref()))
            .collect();
        cards.sort();
        assert_eq!(
            cards.len(),
            2,
            "F9-style respawn must NOT remap onto the dead agent's placeholder; got {cards:?}"
        );
    }

    #[test]
    fn placeholder_restored_after_session_end() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None, None);

        // Transition to real session
        let mut start = make_event("real-uuid", EventType::SessionStart);
        start.pane_id = Some("42".to_string());
        state.apply_event(start);
        assert_eq!(state.sessions["pane-42"].agent_type, AgentType::ClaudeCode);

        // End the real session — placeholder should be restored
        let mut end = make_event("pane-42", EventType::SessionEnd);
        end.pane_id = Some("42".to_string());
        state.apply_event(end);

        assert!(state.sessions.contains_key("pane-42"));
        assert_eq!(state.sessions["pane-42"].agent_type, AgentType::None);
        assert_eq!(state.sessions["pane-42"].pane_id.as_deref(), Some("42"));
    }

    #[test]
    fn placeholder_not_restored_after_close() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None, None);

        // Transition to real session
        let mut start = make_event("real-uuid", EventType::SessionStart);
        start.pane_id = Some("42".to_string());
        state.apply_event(start);

        // Simulate Ctrl+w: remove session and unregister pane (same as ui handler)
        state.sessions.remove("pane-42");
        state.unregister_pane("42");

        assert!(state.sessions.is_empty());
        assert!(!state.managed_pane_ids.contains("42"));
    }

    #[test]
    fn placeholder_excluded_from_stats() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None, None);

        // Add a real session on a different registered pane
        state.register_pane("99".to_string());
        let mut start = make_event("s1", EventType::SessionStart);
        start.pane_id = Some("99".to_string());
        state.apply_event(start);

        let stats = state.aggregate_stats();
        assert_eq!(stats.active, 1);
        assert_eq!(stats.idle, 1);
    }

    #[test]
    fn close_placeholder_session() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None, None);

        // Simulate Ctrl+w on the placeholder
        state.sessions.remove("pane-42");
        state.unregister_pane("42");

        assert!(state.sessions.is_empty());
        assert!(!state.managed_pane_ids.contains("42"));
    }

    // PRD #93 round-5: per-pane unit tests for handle_delegate /
    // handle_work_done used to assert against `delegate_events` /
    // `work_done_events` vectors — those have been removed since the
    // daemon now writes the prompts directly into the target PTYs. The
    // remaining behavior (file write side-effect, anti-spoofing guard
    // on non-orchestrator senders) is exercised by the
    // `tests/orchestration_delegate.rs` integration tests against a
    // real daemon + PTY pair.

    #[test]
    fn compose_delegate_prompt_appends_work_done_footer() {
        let prompt =
            compose_delegate_prompt("Read .dot-agent-deck/worker-task-coder.md for your task.");
        assert!(
            prompt.starts_with("Read .dot-agent-deck/worker-task-coder.md for your task.\n\n"),
            "task body must lead, then a blank line before the footer"
        );
        assert!(
            prompt.contains("## When done"),
            "footer must include the ## When done heading"
        );
        assert!(
            prompt.contains("dot-agent-deck work-done --task"),
            "footer must instruct the worker to call dot-agent-deck work-done"
        );
    }

    #[test]
    fn work_done_writes_summary_file_in_isolation() {
        // The summary-file side effect is unit-testable without a real
        // PTY because it only touches the filesystem. Pin it here so a
        // regression in the file-write path fails the build even when the
        // PTY-write path is unreachable in a unit-test context.
        let mut state = AppState::default();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        state.pane_role_map.insert("worker".into(), "coder".into());
        state.pane_cwd_map.insert("worker".into(), cwd.clone());

        // No orchestrator registered — the PTY-write branch is skipped
        // (warn-and-return), but the file write must still land. Drive
        // through the async fn against a fresh, empty registry: the
        // lookup yields no orchestrator and we exit early before any
        // write_to_pane_and_submit call. The async runtime here is just a vehicle.
        let registry = crate::agent_pty::AgentPtyRegistry::new();
        let signal = crate::event::WorkDoneSignal {
            pane_id: "worker".into(),
            task: "Implemented login".into(),
            done: false,
            timestamp: Utc::now(),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            state.handle_work_done(signal, &registry).await;
        });

        let file = std::path::Path::new(&cwd)
            .join(".dot-agent-deck")
            .join("work-done-coder.md");
        assert!(
            file.exists(),
            "work-done summary file must be written even when no orchestrator pane is attached"
        );
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "Implemented login");
    }

    /// PRD #92 F9 followup-7: pin `wait_for_session_start`'s
    /// `(pane_id, agent_id)` filter directly. The integration tests
    /// in `tests/orchestration_delegate.rs` exercise the same
    /// contract through the full daemon path; this unit test
    /// reproduces the OLD-vs-NEW agent_id discriminator
    /// deterministically against a bare broadcast channel — no
    /// daemon, no PTY, no race window — so a regression in the
    /// filter is caught regardless of CI scheduling.
    #[tokio::test]
    async fn wait_for_session_start_rejects_old_agent_id_accepts_new() {
        let (tx, _) = tokio::sync::broadcast::channel::<BroadcastMsg>(16);

        let mk = |pane: &str, agent_id: Option<&str>| AgentEvent {
            session_id: format!("synthetic-{pane}-{}", agent_id.unwrap_or("none")),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: Some(pane.to_string()),
            agent_id: agent_id.map(str::to_string),
        };

        // Subscribe BEFORE sending so the receiver picks up every event
        // (the broadcast channel only forwards to receivers that
        // existed at send time).
        let mut rx = tx.subscribe();

        // OLD-agent SessionStart for the right pane but wrong id — must
        // be rejected and the wait must time out.
        tx.send(BroadcastMsg::Event(mk("coder-pane", Some("old-id"))))
            .unwrap();
        // Also send a SessionStart with no agent_id at all — the
        // followup-6 filter would have accepted this; followup-7's
        // filter must reject because `None != Some(new-id)`.
        tx.send(BroadcastMsg::Event(mk("coder-pane", None)))
            .unwrap();

        let observed = wait_for_session_start(
            &mut rx,
            "coder-pane",
            "new-id",
            std::time::Duration::from_millis(100),
        )
        .await;
        assert!(
            !observed,
            "wait_for_session_start must reject SessionStart with OLD agent_id or no agent_id"
        );

        // Now NEW-agent SessionStart with matching id — must unblock.
        let mut rx = tx.subscribe();
        tx.send(BroadcastMsg::Event(mk("coder-pane", Some("new-id"))))
            .unwrap();
        let observed = wait_for_session_start(
            &mut rx,
            "coder-pane",
            "new-id",
            std::time::Duration::from_secs(1),
        )
        .await;
        assert!(
            observed,
            "wait_for_session_start must accept SessionStart with matching (pane_id, agent_id)"
        );
    }
}
