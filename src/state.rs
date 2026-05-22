use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::warn;

use crate::agent_pty::AgentPtyRegistry;
use crate::config_validation::sanitize_role_name;
use crate::event::{AgentEvent, AgentType, DelegateSignal, EventType, WorkDoneSignal};
use crate::project_config::{OrchestrationRoleConfig, load_project_config};

const MAX_RECENT_EVENTS: usize = 50;
const MAX_FIRST_PROMPTS: usize = 3;

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
    /// Maps pane_id → orchestration name. Lets the daemon's dispatch
    /// (`handle_delegate` / `handle_work_done`) scope target lookups to
    /// panes in the *same* orchestration tab when several tabs run in
    /// parallel (PRD #93 round-5).
    pub pane_orchestration_map: HashMap<String, String>,
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
    pub fn insert_placeholder_session(
        &mut self,
        pane_id: String,
        cwd: Option<String>,
        agent_type: Option<AgentType>,
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
    pub async fn handle_delegate(&self, signal: DelegateSignal, registry: &AgentPtyRegistry) {
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

        let orchestration = self.pane_orchestration_map.get(&signal.pane_id);

        for target_role in &signal.to {
            // Find the worker pane(s) in the same orchestration with the
            // matching role name. Skip the orchestrator itself (a role that
            // names itself is almost certainly a misconfiguration; we
            // don't want the orchestrator's pane to be fed its own
            // delegate prompt).
            let target_panes: Vec<String> = self
                .pane_role_map
                .iter()
                .filter(|(pane_id, role)| {
                    role.as_str() == target_role.as_str()
                        && !self.orchestrator_pane_ids.contains(pane_id.as_str())
                        && self.pane_orchestration_map.get(pane_id.as_str()) == orchestration
                })
                .map(|(pane_id, _)| pane_id.clone())
                .collect();

            if target_panes.is_empty() {
                warn!(role = %target_role, "delegate: no worker pane found for role");
                continue;
            }

            let safe_name = sanitize_role_name(target_role);
            // CodeRabbit (PRD #93 round-9): the task file lands in the
            // *worker's* cwd, not the orchestrator's. Earlier rounds
            // captured `pane_cwd_map[&signal.pane_id]` once outside the
            // loop and reused it for every worker — fine when every
            // worker shared the orchestrator's directory, broken the
            // moment two role panes were started in different cwds.
            // Per-target lookup also means the per-worker file write
            // happens once per pane and not once per role-name + reused.
            for pane_id in target_panes {
                let cwd = self.pane_cwd_map.get(&pane_id).cloned();
                // CodeRabbit (PRD #93 round-9): look the role config up
                // by `(worker cwd, orchestration name, target role)` so
                // we can apply the per-role `prompt_template` wrapping
                // that Round 5 lost. Approach (b) from the brief: load
                // the project config from the worker's cwd rather than
                // threading prompt_template/clear through
                // `TabMembership` — no wire-format change, and a config
                // edit between sessions takes effect on the next
                // delegate without needing a pane respawn. `None` from
                // the lookup means "no template, fall back to raw task"
                // which matches the pre-round-9 behavior.
                let role_config = match (cwd.as_deref(), orchestration) {
                    (Some(c), Some(orch_name)) => {
                        lookup_orchestration_role(c, orch_name, target_role)
                    }
                    _ => None,
                };
                let prompt_template = role_config
                    .as_ref()
                    .and_then(|r| r.prompt_template.as_deref());
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
                    let file_content = compose_worker_task_file(prompt_template, &signal.task);
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
                    compose_worker_task_file(prompt_template, &signal.task)
                };
                // CodeRabbit (PRD #93 round-9): the per-role `clear`
                // flag (pre-Round-5: kill the worker pane and respawn
                // the role's command before injecting the new prompt)
                // is not yet wired through on the daemon side. Restart
                // requires the daemon to know the role's spawn command,
                // re-issue the StartAgent / close+spawn dance from
                // inside the hook loop, and defer the prompt-write
                // until the fresh agent is ready — substantial new
                // machinery that's deliberately out of scope for this
                // commit (see commit message). Log the deferral here
                // so a `clear = true` regression test or operator
                // running with a `clear`-bearing config sees a clear
                // signal rather than silent drift.
                if let Some(role) = role_config.as_ref()
                    && role.clear
                {
                    tracing::debug!(
                        role = %target_role,
                        pane_id = %pane_id,
                        "delegate: role.clear=true is not yet implemented on the daemon side; \
                         injecting prompt into the existing pane without restart"
                    );
                }
                let one_liner = compose_delegate_prompt(&task_body);
                if let Err(e) = registry.write_to_pane(&pane_id, &one_liner).await {
                    warn!(
                        pane_id = %pane_id,
                        role = %target_role,
                        error = %e,
                        "delegate: failed to write task prompt into target pane"
                    );
                }
            }
        }
    }

    /// Handle a worker's work-done signal: write the per-role summary file
    /// and inject a one-liner pointing the orchestrator pane at it.
    ///
    /// PRD #93 round-5: the file write was already daemon-side (now that
    /// the daemon owns `pane_cwd_map`); the new piece is that the daemon
    /// also picks the orchestrator pane for the same orchestration and
    /// writes the "Worker {role} has completed..." feedback directly into
    /// its PTY via [`AgentPtyRegistry::write_to_pane`]. No broadcast hop —
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
        if let Err(e) = registry.write_to_pane(&orch_pane_id, &feedback).await {
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
        if let Some(ref pane_id) = event.pane_id
            && let Some(existing_id) = self.sessions.iter().find_map(|(id, session)| {
                (session.pane_id.as_ref().is_some_and(|p| p == pane_id) && id != &event.session_id)
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
            let pane_id_and_cwd = self.sessions.get(&event.session_id).and_then(|session| {
                session.pane_id.as_ref().map(|pid| {
                    self.pane_started_at.insert(pid.clone(), session.started_at);
                    (pid.clone(), session.cwd.clone())
                })
            });
            self.sessions.remove(&event.session_id);
            // Restore a placeholder card so the pane remains visible on the dashboard.
            if let Some((pane_id, cwd)) = pane_id_and_cwd
                && self.managed_pane_ids.contains(&pane_id)
            {
                // M2.13: a SessionEnd restoration creates a fresh
                // placeholder; `agent_type` is unknown post-end and gets
                // re-populated when the next `SessionStart` hook arrives
                // for this pane. Same default behavior as before M2.13.
                self.insert_placeholder_session(pane_id, cwd, None);
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
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None);

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
        state.insert_placeholder_session("99".to_string(), Some("/tmp".to_string()), None);
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
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None);

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

    #[test]
    fn placeholder_restored_after_session_end() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None);

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
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None);

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
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None);

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
        state.insert_placeholder_session("42".to_string(), Some("/tmp".to_string()), None);

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
        // write_to_pane call. The async runtime here is just a vehicle.
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
}
