use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::agent_pty::TabMembership;
use crate::event::{AgentType, EventType};
use crate::mode_manager::{ModeManager, ModeManagerError};
use crate::pane::{AgentSpawnOptions, CloseTabOutcome, PaneController};
use crate::project_config::{ModeConfig, OrchestrationConfig, resolve_orchestration_name};
use crate::state::SessionState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub type TabId = u32;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TabError {
    #[error("Cannot close the dashboard tab")]
    CannotCloseDashboard,
    #[error("Tab index {0} out of bounds")]
    IndexOutOfBounds(usize),
    #[error("Mode error: {0}")]
    ModeManager(#[from] ModeManagerError),
    /// Hydration-time API mismatch (PRD #76 M2.12 fixup auditor #3):
    /// the caller passed a `role_pane_ids` vec whose length did not
    /// match `config.roles.len()`. Reported as an error rather than
    /// panicking so a malformed daemon record + a future-caller bug
    /// can't crash the TUI from a hydration-only API.
    #[error(
        "open_orchestration_tab_with_existing_role_panes: role_pane_ids length {got} does not match config.roles.len() {expected}"
    )]
    MismatchedRoleCount { expected: usize, got: usize },
}

// ---------------------------------------------------------------------------
// Orchestration status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestrationStatus {
    WaitingForOrchestrator,
    Delegated,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestrationRoleStatus {
    Waiting,
    Working,
    Done,
    /// PRD #76 M2.12: the role's agent pane was not present in the daemon
    /// on reconnect — either the agent died before the TUI reattached or
    /// hydration couldn't locate it. The slot is preserved on the
    /// orchestration tab as a dead placeholder rather than silently
    /// respawned (design decision 4), so the user can decide whether to
    /// re-run the orchestration.
    Failed,
}

// ---------------------------------------------------------------------------
// Tab enum
// ---------------------------------------------------------------------------

pub enum Tab {
    Dashboard,
    Mode {
        id: TabId,
        name: String,
        agent_pane_id: String,
        mode_manager: Box<ModeManager>,
        last_routed_timestamp: HashMap<String, DateTime<Utc>>,
        cwd: String,
        /// Which side pane has visual focus in Normal mode. `None` = agent pane.
        focused_side_pane_index: Option<usize>,
    },
    Orchestration {
        id: TabId,
        name: String,
        /// Pane IDs for each role, in the same order as config roles.
        role_pane_ids: Vec<String>,
        /// Per-role status for the orchestration sidebar.
        role_statuses: Vec<OrchestrationRoleStatus>,
        cwd: String,
        /// Index into `role_pane_ids` for the start (orchestrator) role.
        start_role_index: usize,
        /// Pre-built prompt to inject into the start role once it is ready.
        orchestrator_prompt: Option<String>,
        /// Full orchestration config, kept for dispatch (M5) access to
        /// role prompt_template, clear flag, and command.
        config: OrchestrationConfig,
        /// Tracks whether the orchestration is waiting, delegated, or completed.
        status: OrchestrationStatus,
    },
}

impl Tab {
    fn label(&self) -> &str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Mode { name, .. } => name,
            Tab::Orchestration { name, .. } => name,
        }
    }
}

// ---------------------------------------------------------------------------
// TabManager
// ---------------------------------------------------------------------------

pub struct TabManager {
    tabs: Vec<Tab>,
    active_index: usize,
    next_id: TabId,
    pane_controller: Arc<dyn PaneController>,
}

impl TabManager {
    pub fn new(pane_controller: Arc<dyn PaneController>) -> Self {
        Self {
            tabs: vec![Tab::Dashboard],
            active_index: 0,
            next_id: 1,
            pane_controller,
        }
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_index]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_index]
    }

    pub fn switch_to(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active_index = index;
            true
        } else {
            false
        }
    }

    pub fn show_tab_bar(&self) -> bool {
        self.tabs.len() > 1
    }

    pub fn tab_labels(&self) -> Vec<String> {
        self.tabs.iter().map(|t| t.label().to_string()).collect()
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    /// Open a new mode tab. Returns `(tab_index, managed_pane_ids)`.
    ///
    /// PRD #76 M2.15 fixup pass 2 G1 — `side_pane_dims` is the
    /// initial PTY size for every persistent + reactive side pane the
    /// mode creates. Callers compute this from
    /// `terminal.get_frame().area()` via the `mode_side_pane_dims`
    /// SSOT helper in `ui.rs`, so the daemon-side PTYs open at the
    /// viewport-derived size instead of the legacy 24×80. Tests that
    /// don't care about geometry pass `(24, 80)`.
    pub fn open_mode_tab(
        &mut self,
        config: &ModeConfig,
        cwd: &str,
        agent_pane_id: String,
        side_pane_dims: (u16, u16),
    ) -> Result<(usize, Vec<String>), TabError> {
        let mut mode_manager = ModeManager::new(Arc::clone(&self.pane_controller));
        mode_manager.activate_mode(config, Some(cwd), side_pane_dims)?;
        let pane_ids = mode_manager.managed_pane_ids();

        let id = self.next_id;
        self.next_id += 1;

        self.tabs.push(Tab::Mode {
            id,
            name: config.name.clone(),
            agent_pane_id,
            mode_manager: Box::new(mode_manager),
            last_routed_timestamp: HashMap::new(),
            cwd: cwd.to_string(),
            focused_side_pane_index: None,
        });

        let index = self.tabs.len() - 1;
        self.active_index = index;

        Ok((index, pane_ids))
    }

    /// Send pending commands to the active mode tab's panes.
    /// Must be called after panes are resized to correct display dimensions.
    pub fn start_mode_commands(&mut self) -> Result<(), TabError> {
        if let Some(Tab::Mode { mode_manager, .. }) = self.tabs.get_mut(self.active_index) {
            mode_manager
                .start_mode_commands()
                .map_err(TabError::ModeManager)?;
        }
        Ok(())
    }

    /// Open a new orchestration tab. Creates one pane per role.
    /// `orchestrator_prompt` is injected into the start role once its agent is ready.
    /// Returns `(tab_index, role_pane_ids)`.
    pub fn open_orchestration_tab(
        &mut self,
        config: &OrchestrationConfig,
        cwd: &str,
        orchestrator_prompt: Option<String>,
        // PRD #76 M2.15: initial PTY dims for every role pane in this
        // orchestration. The caller computes these from
        // `terminal.get_frame().area()` + the dashboard-layout helper, so
        // the daemon-side PTY opens at the viewport size instead of the
        // legacy 24×80. Callers without a real viewport (tests) pass
        // `(24, 80)`. The post-spawn `resize_dashboard_panes` sweep
        // reconciles per-role focus state on the first frame.
        spawn_dims: (u16, u16),
    ) -> Result<(usize, Vec<String>), TabError> {
        let mut role_pane_ids: Vec<String> = Vec::with_capacity(config.roles.len());
        let (spawn_rows, spawn_cols) = spawn_dims;

        // CodeRabbit round-9 #7 / round-10 #1: `config.name` defaults
        // to an empty string when the user didn't name their
        // orchestration. We fall back to the cwd basename so the
        // daemon-side `TabMembership` carries the same resolved label
        // as the local `Tab::Orchestration` record AND the same label
        // that `load_project_config` now writes into the parsed
        // `OrchestrationConfig.name` on the daemon side. Without that
        // three-way agreement, every Orchestration `TabMembership`
        // would echo "" on reconnect (`partition_hydrated_panes` keys
        // against `("", cwd)`, collapsing parallel unnamed
        // orchestrations) AND the daemon's `handle_delegate` lookup
        // would never match the role's `prompt_template` for
        // unnamed orchestrations.
        let resolved_name = resolve_orchestration_name(&config.name, std::path::Path::new(cwd));

        // PRD #76 M2.12: tag each role pane with its orchestration tab
        // membership so the daemon-side registry can echo it back via
        // `list_agents` and the TUI rebuilds the orchestration tab on
        // reconnect instead of stranding all role panes on the dashboard.
        for (role_index, role) in config.roles.iter().enumerate() {
            let opts = AgentSpawnOptions {
                display_name: Some(role.name.as_str()),
                tab_membership: Some(TabMembership::Orchestration {
                    name: resolved_name.clone(),
                    role_index,
                    role_name: role.name.clone(),
                    is_start_role: role.start,
                    // Round-11 auditor #C: carry the orchestration's
                    // cwd (shared across every role pane in this tab)
                    // so the daemon can disambiguate two unnamed
                    // orchestrations whose basenames collide.
                    orchestration_cwd: Some(cwd.to_string()),
                }),
                rows: spawn_rows,
                cols: spawn_cols,
                // PRD #76 M2.13: tag each role's daemon-side registry
                // entry with the agent type inferred from its command
                // (e.g. `claude` → `ClaudeCode`). The daemon echoes this
                // back via `list_agents` on reconnect so the hydration
                // path can build the placeholder session with the right
                // type instead of "No agent".
                agent_type: AgentType::from_command(Some(&role.command)),
            };
            let (pane_id, _resolved) = match self.pane_controller.create_pane_with_options(
                Some(&role.command),
                Some(cwd),
                opts,
            ) {
                Ok(p) => p,
                Err(e) => {
                    // Clean up any panes already created.
                    for id in &role_pane_ids {
                        let _ = self.pane_controller.close_pane(id);
                    }
                    return Err(ModeManagerError::Pane(e).into());
                }
            };
            role_pane_ids.push(pane_id);
        }

        let id = self.next_id;
        self.next_id += 1;

        let start_role_index = config.roles.iter().position(|r| r.start).unwrap_or(0);

        self.tabs.push(Tab::Orchestration {
            id,
            name: resolved_name,
            role_pane_ids: role_pane_ids.clone(),
            role_statuses: vec![OrchestrationRoleStatus::Waiting; config.roles.len()],
            cwd: cwd.to_string(),
            start_role_index,
            orchestrator_prompt,
            config: config.clone(),
            status: OrchestrationStatus::WaitingForOrchestrator,
        });

        let index = self.tabs.len() - 1;
        self.active_index = index;

        Ok((index, role_pane_ids))
    }

    /// PRD #76 M2.12: hydration entry point for mode tabs. Same flow as
    /// [`open_mode_tab`], but documents the intent: the agent pane
    /// already exists as `agent_pane_id` (a daemon pane reattached during
    /// `hydrate_from_daemon`). Side panes still spawn fresh from
    /// `config.panes` — they're not daemon-tracked (design decision 2),
    /// so any in-flight side-pane state is intentionally lost on
    /// reconnect.
    ///
    /// Returns `(tab_index, side_pane_ids)`, matching `open_mode_tab`.
    /// Keeping the two as separate symbols (rather than overloading the
    /// user-driven entry point) makes the hydration call sites in
    /// `ui.rs` self-documenting and lets future divergence happen without
    /// touching the user-driven path.
    pub fn open_mode_tab_with_existing_agent_pane(
        &mut self,
        config: &ModeConfig,
        cwd: &str,
        agent_pane_id: String,
        // PRD #76 M2.15 fixup pass 2 G1 — initial side-pane PTY dims;
        // see `open_mode_tab` for the SSOT helper to compute this.
        side_pane_dims: (u16, u16),
    ) -> Result<(usize, Vec<String>), TabError> {
        self.open_mode_tab(config, cwd, agent_pane_id, side_pane_dims)
    }

    /// PRD #76 M2.12: hydration entry point for orchestration tabs.
    /// Unlike [`open_orchestration_tab`], does not spawn role panes —
    /// `role_pane_ids[i]` is either `Some(existing_pane_id)` (the slot
    /// is wired to that hydrated daemon pane and starts in the `Working`
    /// state) or `None` (the slot is dead: the role's agent terminated
    /// before reconnect, so it's preserved as a placeholder in
    /// `OrchestrationRoleStatus::Failed`, never silently respawned —
    /// design decision 4).
    ///
    /// `orchestrator_prompt` is always `None` because the prompt is
    /// display polish only — the orchestrator role already received it
    /// at start time and has the conversation in its scrollback (design
    /// decision 3). The wire-format `role_pane_ids` length must match
    /// `config.roles.len()`; out-of-bounds role_index entries should be
    /// dropped to the dashboard by the caller (logged as a config-drift
    /// bug per design decision 5).
    ///
    /// Returns `(tab_index, role_pane_ids_flat)` where the flat vec
    /// substitutes empty strings for `None` slots so the existing
    /// `Tab::Orchestration::role_pane_ids: Vec<String>` shape stays
    /// stable. Callers can cross-reference `role_statuses` to tell live
    /// from dead slots.
    pub fn open_orchestration_tab_with_existing_role_panes(
        &mut self,
        config: &OrchestrationConfig,
        cwd: &str,
        role_pane_ids: Vec<Option<String>>,
    ) -> Result<(usize, Vec<String>), TabError> {
        // M2.12 fixup auditor #3: this is a hydration-oriented API, so
        // mismatched lengths must surface as a `TabError` for the
        // caller to handle (log + fallback to dashboard) rather than
        // panic. The current caller constructs the vec correctly, but
        // a malformed daemon record + a future-caller bug should not
        // tear down the whole TUI.
        if role_pane_ids.len() != config.roles.len() {
            return Err(TabError::MismatchedRoleCount {
                expected: config.roles.len(),
                got: role_pane_ids.len(),
            });
        }

        // Flatten Option<String> → String. Dead slots get the empty
        // sentinel so the Vec<String> shape of `Tab::Orchestration`
        // doesn't have to change. Downstream lookups (`role_pane_ids[i]`
        // for delegation routing in `ui.rs`) will see "" and find no
        // matching pane — same observable effect as the role being
        // missing.
        // Follow-up to 0d5e651 (reviewer finding #5): synthetic
        // dead-slot ids (`__dead-slot__-…`) are seeded into otherwise
        // `None` slots BEFORE this call so the orchestration tab keeps
        // the role's card visible. They are placeholder cards, not
        // live agents — classify them as `Failed` instead of `Working`
        // so any future consumer (e.g. a "role died" badge) reads the
        // correct semantic signal.
        let role_statuses: Vec<OrchestrationRoleStatus> = role_pane_ids
            .iter()
            .map(|slot| match slot {
                Some(id) if crate::ui::is_dead_slot_pane_id(id) => OrchestrationRoleStatus::Failed,
                Some(_) => OrchestrationRoleStatus::Working,
                None => OrchestrationRoleStatus::Failed,
            })
            .collect();
        let role_pane_ids_flat: Vec<String> = role_pane_ids
            .into_iter()
            .map(|slot| slot.unwrap_or_default())
            .collect();

        let id = self.next_id;
        self.next_id += 1;

        let start_role_index = config.roles.iter().position(|r| r.start).unwrap_or(0);

        let name = resolve_orchestration_name(&config.name, std::path::Path::new(cwd));

        self.tabs.push(Tab::Orchestration {
            id,
            name,
            role_pane_ids: role_pane_ids_flat.clone(),
            role_statuses,
            cwd: cwd.to_string(),
            start_role_index,
            // Design decision 3: don't replay orchestrator_prompt on
            // reconnect. The orchestrator already received it at start
            // time and the conversation is in its scrollback.
            orchestrator_prompt: None,
            config: config.clone(),
            status: OrchestrationStatus::WaitingForOrchestrator,
        });

        let index = self.tabs.len() - 1;
        self.active_index = index;

        Ok((index, role_pane_ids_flat))
    }

    /// PRD #92 F4: close a mode or orchestration tab and return a
    /// [`CloseTabOutcome`] capturing per-pane close results. Pre-F4
    /// this returned `Vec<String>` of "managed pane IDs" with every
    /// `close_pane` error silently swallowed via `let _ =`; the
    /// resulting partial failure left agents alive in the daemon
    /// registry while their cards vanished from the dashboard.
    ///
    /// Callers now inspect `outcome.closed` to know which dashboard
    /// cards may be removed and `outcome.failed` to know which cards
    /// must be preserved (with the rendered error surfaced via
    /// `ui.status_message`). The tab itself is always removed from
    /// `self.tabs` — only the cards behave differently.
    pub fn close_tab(&mut self, index: usize) -> Result<CloseTabOutcome, TabError> {
        if index == 0 {
            return Err(TabError::CannotCloseDashboard);
        }
        if index >= self.tabs.len() {
            return Err(TabError::IndexOutOfBounds(index));
        }

        let tab = self.tabs.remove(index);
        let outcome = match tab {
            Tab::Mode {
                mut mode_manager,
                agent_pane_id,
                ..
            } => {
                // `deactivate_mode` now returns the per-pane outcome
                // for the persistent + reactive side panes. `Err` only
                // fires when there is no active mode — propagating it
                // here would lose the agent-pane close result, so we
                // fold a NoActiveMode into a fresh empty outcome and
                // let the agent-pane close drive the merge below.
                let mut outcome = mode_manager.deactivate_mode().unwrap_or_default();
                // Close the agent pane PTY so it doesn't linger on the dashboard.
                if !agent_pane_id.is_empty() {
                    let result = self.pane_controller.close_pane(&agent_pane_id);
                    outcome.record(agent_pane_id, result);
                }
                outcome
            }
            Tab::Orchestration { role_pane_ids, .. } => {
                let mut outcome = CloseTabOutcome::default();
                for id in &role_pane_ids {
                    // M2.12: skip the empty-string dead-slot sentinel
                    // inserted by `open_orchestration_tab_with_existing_role_panes`
                    // for roles that didn't survive reconnect — there's
                    // no pane to close, and leaking "" through a pane-id
                    // API confuses downstream callers.
                    // Symptom 2 fix (`.dot-agent-deck/agent-card-lifecycle-bugs.md`):
                    // also skip synthetic dead-slot pane ids
                    // (`__dead-slot__-...`) — those carry a placeholder
                    // session on the dashboard but have no backing PTY,
                    // so `close_pane` would fail with NotFound.
                    if id.is_empty() || crate::ui::is_dead_slot_pane_id(id) {
                        continue;
                    }
                    let result = self.pane_controller.close_pane(id);
                    outcome.record(id.clone(), result);
                }
                outcome
            }
            Tab::Dashboard => CloseTabOutcome::default(),
        };

        // Adjust active_index after removal.
        if self.active_index >= self.tabs.len() {
            self.active_index = self.tabs.len() - 1;
        } else if self.active_index > index {
            self.active_index -= 1;
        } else if self.active_index == index {
            // Closed the active tab — fall back to dashboard.
            self.active_index = 0;
        }

        Ok(outcome)
    }

    /// Collect all managed pane IDs across all mode tabs.
    /// Returns side pane IDs managed by mode tabs (excludes agent panes,
    /// which should still render on the dashboard).
    pub fn all_managed_pane_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for tab in &self.tabs {
            match tab {
                Tab::Mode { mode_manager, .. } => {
                    ids.extend(mode_manager.managed_pane_ids());
                }
                Tab::Orchestration { role_pane_ids, .. } => {
                    // M2.12: skip the empty-string dead-slot sentinel.
                    // Symptom 2 fix: also skip synthetic dead-slot pane
                    // ids (`__dead-slot__-...`) — those are placeholder
                    // sessions only, not real panes the embedded
                    // controller owns.
                    ids.extend(
                        role_pane_ids
                            .iter()
                            .filter(|id| !id.is_empty() && !crate::ui::is_dead_slot_pane_id(id))
                            .cloned(),
                    );
                }
                Tab::Dashboard => {}
            }
        }
        ids
    }

    /// Find which tab index owns a given pane ID.
    pub fn tab_index_for_pane(&self, pane_id: &str) -> Option<usize> {
        for (i, tab) in self.tabs.iter().enumerate() {
            match tab {
                Tab::Mode { mode_manager, .. }
                    if mode_manager
                        .managed_pane_ids()
                        .contains(&pane_id.to_string()) =>
                {
                    return Some(i);
                }
                // M2.12: an empty pane_id would falsely match the
                // dead-slot sentinel — skip the empty-string case
                // explicitly so a caller asking about pane_id="" doesn't
                // get a spurious orchestration tab match.
                // Follow-up to 0d5e651 (reviewer finding #6): also skip
                // synthetic dead-slot pane ids for consistency with
                // `close_tab`, `all_managed_pane_ids`, and
                // `resize_orchestration_role_panes_for`. No production
                // caller hits the synthetic-id branch today, but the
                // inconsistency is a footgun for any future code that
                // assumes "if `tab_index_for_pane` returns Some, the
                // pane is real."
                Tab::Orchestration { role_pane_ids, .. }
                    if !pane_id.is_empty()
                        && !crate::ui::is_dead_slot_pane_id(pane_id)
                        && role_pane_ids.contains(&pane_id.to_string()) =>
                {
                    return Some(i);
                }
                _ => {}
            }
        }
        None
    }

    /// Find the mode tab that has this pane as its agent pane.
    pub fn tab_index_for_agent_pane(&self, pane_id: &str) -> Option<usize> {
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Tab::Mode { agent_pane_id, .. } = tab
                && agent_pane_id == pane_id
            {
                return Some(i);
            }
        }
        None
    }

    /// Get the active mode name (None if Dashboard is active).
    pub fn active_mode_name(&self) -> Option<&str> {
        match &self.tabs[self.active_index] {
            Tab::Dashboard => None,
            Tab::Mode { name, .. } => Some(name),
            Tab::Orchestration { .. } => None,
        }
    }

    /// Route reactive commands to all active mode tabs.
    /// Each tab only receives commands from its own agent session (scoped by agent_pane_id).
    /// Returns pairs of (closed_pane_id, new_pane_id) for panes that were recreated.
    pub fn route_reactive_commands(
        &mut self,
        sessions: &HashMap<String, SessionState>,
    ) -> Vec<(String, String)> {
        let mut pane_changes = Vec::new();
        for tab in &mut self.tabs {
            if let Tab::Mode {
                mode_manager,
                last_routed_timestamp,
                name,
                agent_pane_id,
                ..
            } = tab
            {
                // Only route commands from this tab's own agent session.
                let scoped: HashMap<String, SessionState> = sessions
                    .iter()
                    .filter(|(_, s)| s.pane_id.as_deref() == Some(agent_pane_id.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let new_commands = extract_new_bash_commands(&scoped, last_routed_timestamp);
                for cmd in &new_commands {
                    tracing::info!("Routing command to tab '{name}': {cmd}");
                    match mode_manager.handle_command(cmd) {
                        Ok(Some(change)) => {
                            if let (Some(old_id), Some(new_id)) = (change.closed, change.created) {
                                pane_changes.push((old_id, new_id));
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!("Reactive pane routing error in tab '{name}': {e}");
                        }
                    }
                }
            }
        }
        pane_changes
    }
}

// ---------------------------------------------------------------------------
// Reactive command extraction (moved from ui.rs)
// ---------------------------------------------------------------------------

/// Scans sessions for new Bash commands that have not been routed yet.
pub(crate) fn extract_new_bash_commands(
    sessions: &HashMap<String, SessionState>,
    last_routed: &mut HashMap<String, DateTime<Utc>>,
) -> Vec<String> {
    let mut commands = Vec::new();
    for (sid, session) in sessions {
        let cutoff = last_routed.get(sid).copied();
        for event in session.recent_events.iter() {
            if cutoff.is_some_and(|ts| event.timestamp <= ts) {
                continue;
            }
            if event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
                && let Some(cmd) = event.metadata.get("bash_command")
            {
                commands.push(cmd.clone());
            }
        }
        if let Some(last) = session.recent_events.back() {
            last_routed.insert(sid.clone(), last.timestamp);
        }
    }
    last_routed.retain(|sid, _| sessions.contains_key(sid));
    commands
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
