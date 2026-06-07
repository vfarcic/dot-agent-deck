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
    Dashboard {
        /// PRD #83: session id of the dashboard card last selected on this
        /// tab. `None` = no remembered selection (defaults to the first
        /// card). Keyed by stable session id, not a positional index, so
        /// filter/sort changes and session restarts don't move the
        /// selection to the wrong card. `UiState.selected_index` is
        /// derived from this each frame.
        selected_session_id: Option<String>,
    },
    Mode {
        id: TabId,
        name: String,
        agent_pane_id: String,
        mode_manager: Box<ModeManager>,
        last_routed_timestamp: HashMap<String, DateTime<Utc>>,
        cwd: String,
        /// PRD #83: which pane has focus in Normal mode, keyed by stable
        /// pane id. `None` = the agent pane is focused; `Some(id)` = that
        /// side pane is focused. Replaces the former positional
        /// `focused_side_pane_index: Option<usize>` so reactive pane-pool
        /// changes can't silently point focus at the wrong pane.
        focused_pane_id: Option<String>,
    },
    Orchestration {
        id: TabId,
        name: String,
        /// Pane IDs for each role, in the same order as config roles.
        role_pane_ids: Vec<String>,
        /// Per-role status for the orchestration sidebar.
        role_statuses: Vec<OrchestrationRoleStatus>,
        cwd: String,
        /// PRD #83: which role pane has focus on this tab, keyed by stable
        /// pane id. `None` = default to the start (orchestrator) role pane
        /// on switch-in.
        focused_role_pane_id: Option<String>,
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
            Tab::Dashboard { .. } => "Dashboard",
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
            tabs: vec![Tab::Dashboard {
                selected_session_id: None,
            }],
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

    /// PRD #83 M2 — capture the process-wide focused pane id into the
    /// currently active tab's per-tab selection field, just before a tab
    /// switch leaves it. Mode tabs record `None` when the agent pane is
    /// focused and `Some(side_id)` when a managed side pane is focused;
    /// Orchestration tabs record the focused role pane. A focused pane
    /// that doesn't belong to the active tab (e.g. focus moved elsewhere
    /// programmatically) leaves the field unchanged. Dashboard is a
    /// no-op: its `selected_session_id` is maintained every frame from
    /// the focused pane by the render loop, which has the session list
    /// this method lacks.
    pub fn capture_focus_on_switch_out(&mut self) {
        let focused = self.pane_controller.focused_pane_id();
        match &mut self.tabs[self.active_index] {
            Tab::Dashboard { .. } => {}
            Tab::Mode {
                agent_pane_id,
                mode_manager,
                focused_pane_id,
                ..
            } => {
                let Some(focused) = focused else { return };
                if &focused == agent_pane_id {
                    *focused_pane_id = None;
                } else if mode_manager.managed_pane_ids().contains(&focused) {
                    *focused_pane_id = Some(focused);
                }
                // Focus belongs to another tab → leave the field as-is.
            }
            Tab::Orchestration {
                role_pane_ids,
                focused_role_pane_id,
                ..
            } => {
                let Some(focused) = focused else { return };
                if role_pane_ids.iter().any(|id| id == &focused) {
                    *focused_role_pane_id = Some(focused);
                }
            }
        }
    }

    /// PRD #83 M4 — after a reactive pane-pool change, follow EVERY tab's
    /// remembered focused pane to its successor using the
    /// `(closed_id, new_id)` pairs from [`Self::route_reactive_commands`].
    ///
    /// `route_reactive_commands` iterates over ALL tabs, so a recreated
    /// reactive pane can be the remembered focus of a BACKGROUND
    /// (non-active) Mode or Orchestration tab — that tab must follow the
    /// successor on switch-in, not silently fall back to its default
    /// pane (the review finding this fixes). For every tab whose
    /// remembered focus (`Tab::Mode::focused_pane_id` /
    /// `Tab::Orchestration::focused_role_pane_id`) equals a closed id
    /// with a known successor, the field is remapped to the new id; a
    /// remembered id that has vanished from the tab's live pane set with
    /// no successor is cleared (M4 fallback → agent / start-role pane on
    /// switch-in). Keyed by stable id, this replaces the former
    /// positional-index clamp.
    ///
    /// Returns the new id for the ACTIVE tab's focused pane when it was
    /// remapped, so the caller can re-focus the live pane on the
    /// controller — background tabs need no controller focus until they
    /// become active and `restore_focus_on_switch_in` runs.
    pub fn remap_focus_after_reactive_change(
        &mut self,
        pane_changes: &[(String, String)],
    ) -> Option<String> {
        let active = self.active_index;
        let mut active_new_id: Option<String> = None;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            match tab {
                Tab::Mode {
                    focused_pane_id,
                    mode_manager,
                    ..
                } => {
                    let Some(current) = focused_pane_id.clone() else {
                        continue;
                    };
                    if let Some((_, new_id)) = pane_changes.iter().find(|(old, _)| old == &current)
                    {
                        *focused_pane_id = Some(new_id.clone());
                        if i == active {
                            active_new_id = Some(new_id.clone());
                        }
                    } else if !mode_manager.managed_pane_ids().contains(&current) {
                        *focused_pane_id = None;
                    }
                }
                Tab::Orchestration {
                    focused_role_pane_id,
                    role_pane_ids,
                    ..
                } => {
                    let Some(current) = focused_role_pane_id.clone() else {
                        continue;
                    };
                    if let Some((_, new_id)) = pane_changes.iter().find(|(old, _)| old == &current)
                    {
                        *focused_role_pane_id = Some(new_id.clone());
                        if i == active {
                            active_new_id = Some(new_id.clone());
                        }
                    } else if !role_pane_ids.contains(&current) {
                        *focused_role_pane_id = None;
                    }
                }
                Tab::Dashboard { .. } => {}
            }
        }
        active_new_id
    }

    /// PRD #83 — record that `pane_id` is now the focused pane of the
    /// active tab, updating its per-tab selection field. Used by the
    /// programmatic "jump to the tab owning this pane and focus it"
    /// paths (Enter-on-card, config-prompt focus) so the tab's remembered
    /// focus matches the pane the controller was just told to focus —
    /// otherwise the next render would highlight a stale pane. Mode tabs
    /// store `None` when the agent pane is focused. Dashboard is a no-op
    /// (its selection is keyed by session id, synced from the render loop).
    pub fn record_focus(&mut self, pane_id: &str) {
        match &mut self.tabs[self.active_index] {
            Tab::Dashboard { .. } => {}
            Tab::Mode {
                agent_pane_id,
                focused_pane_id,
                ..
            } => {
                *focused_pane_id = if pane_id == agent_pane_id {
                    None
                } else {
                    Some(pane_id.to_string())
                };
            }
            Tab::Orchestration {
                role_pane_ids,
                focused_role_pane_id,
                ..
            } => {
                if role_pane_ids.iter().any(|id| id == pane_id) {
                    *focused_role_pane_id = Some(pane_id.to_string());
                }
            }
        }
    }

    /// PRD #83 M2/M4 — restore the active tab's remembered pane focus on
    /// switch-in by calling `focus_pane` on the embedded controller.
    /// Mode tabs focus their remembered side pane (or the agent pane when
    /// `None`); Orchestration tabs focus their remembered role pane (or
    /// the start role pane). A remembered id that no longer exists in the
    /// tab's live pane set is cleared and the default is focused instead
    /// (stale-id fallback). Dashboard is a no-op — it has no fixed pane
    /// to focus and its card selection is derived from
    /// `selected_session_id` each frame.
    pub fn restore_focus_on_switch_in(&mut self) {
        let target: Option<String> = match &mut self.tabs[self.active_index] {
            Tab::Dashboard { .. } => None,
            Tab::Mode {
                agent_pane_id,
                mode_manager,
                focused_pane_id,
                ..
            } => {
                // Drop a stale side-pane id so we fall back to the agent pane.
                if let Some(id) = focused_pane_id.as_ref()
                    && !mode_manager.managed_pane_ids().contains(id)
                {
                    *focused_pane_id = None;
                }
                Some(
                    focused_pane_id
                        .clone()
                        .unwrap_or_else(|| agent_pane_id.clone()),
                )
            }
            Tab::Orchestration {
                role_pane_ids,
                focused_role_pane_id,
                start_role_index,
                ..
            } => {
                let is_live = |id: &String| {
                    !id.is_empty()
                        && !crate::ui::is_dead_slot_pane_id(id)
                        && role_pane_ids.iter().any(|p| p == id)
                };
                if let Some(id) = focused_role_pane_id.as_ref()
                    && !is_live(id)
                {
                    *focused_role_pane_id = None;
                }
                focused_role_pane_id.clone().or_else(|| {
                    // Default to the start role pane, else the first live role pane.
                    role_pane_ids
                        .get(*start_role_index)
                        .filter(|id| is_live(id))
                        .cloned()
                        .or_else(|| role_pane_ids.iter().find(|id| is_live(id)).cloned())
                })
            }
        };
        if let Some(id) = target {
            let _ = self.pane_controller.focus_pane(&id);
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
            focused_pane_id: None,
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
            focused_role_pane_id: None,
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
            focused_role_pane_id: None,
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
            Tab::Dashboard { .. } => CloseTabOutcome::default(),
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
                Tab::Dashboard { .. } => {}
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
            Tab::Dashboard { .. } => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{PaneController, PaneDirection, PaneError, PaneInfo, RenameOutcome};
    use crate::project_config::{
        ModeConfig, ModePersistentPane, OrchestrationConfig, OrchestrationRoleConfig,
    };
    use spec::spec;
    use std::sync::Mutex;

    /// Mock `PaneController` for PRD #83 tab-selection tests. It mints
    /// sequential pane ids on create, remembers the single focused pane
    /// (so `focused_pane_id` round-trips the last `focus_pane`), and
    /// records every `focus_pane` id so tests can assert which pane the
    /// switch/restore path actually focused.
    struct MockPaneController {
        next: Mutex<u32>,
        focused: Mutex<Option<String>>,
        focus_calls: Mutex<Vec<String>>,
    }

    impl MockPaneController {
        fn new() -> Self {
            Self {
                next: Mutex::new(0),
                focused: Mutex::new(None),
                focus_calls: Mutex::new(Vec::new()),
            }
        }

        fn focus_calls(&self) -> Vec<String> {
            self.focus_calls.lock().unwrap().clone()
        }

        fn last_focus(&self) -> Option<String> {
            self.focus_calls.lock().unwrap().last().cloned()
        }
    }

    impl PaneController for MockPaneController {
        fn create_pane(
            &self,
            _command: Option<&str>,
            _cwd: Option<&str>,
        ) -> Result<String, PaneError> {
            let mut n = self.next.lock().unwrap();
            let id = format!("pane-{n}");
            *n += 1;
            Ok(id)
        }
        fn focus_pane(&self, pane_id: &str) -> Result<(), PaneError> {
            *self.focused.lock().unwrap() = Some(pane_id.to_string());
            self.focus_calls.lock().unwrap().push(pane_id.to_string());
            Ok(())
        }
        fn focused_pane_id(&self) -> Option<String> {
            self.focused.lock().unwrap().clone()
        }
        fn close_pane(&self, _pane_id: &str) -> Result<(), PaneError> {
            Ok(())
        }
        fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError> {
            Ok(Vec::new())
        }
        fn resize_pane(
            &self,
            _pane_id: &str,
            _direction: PaneDirection,
            _amount: u16,
        ) -> Result<(), PaneError> {
            Ok(())
        }
        fn rename_pane(&self, _pane_id: &str, name: &str) -> Result<RenameOutcome, PaneError> {
            Ok(RenameOutcome::applied(name))
        }
        fn toggle_layout(&self) -> Result<(), PaneError> {
            Ok(())
        }
        fn write_to_pane(&self, _pane_id: &str, _text: &str) -> Result<(), PaneError> {
            Ok(())
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// A mode config with `side_pane_count` persistent (non-watch) side
    /// panes and no reactive pool, so `managed_pane_ids()` is deterministic.
    fn mode_config(name: &str, side_pane_count: usize) -> ModeConfig {
        ModeConfig {
            name: name.to_string(),
            init_command: None,
            panes: (0..side_pane_count)
                .map(|i| ModePersistentPane {
                    command: format!("echo side-{i}"),
                    name: Some(format!("side-{i}")),
                    watch: false,
                })
                .collect(),
            rules: Vec::new(),
            reactive_panes: 0,
        }
    }

    fn orch_config(name: &str) -> OrchestrationConfig {
        OrchestrationConfig {
            name: name.to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "orchestrator".to_string(),
                    command: "echo orch".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    name: "coder".to_string(),
                    command: "echo coder".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
            ],
        }
    }

    /// Scenario: Give the Dashboard, a Mode tab, and an Orchestration tab
    /// each their own stable-id selection field, switch through every tab
    /// and back, and assert each tab still holds its own remembered id —
    /// proving the selection state is per-tab, not a single global value.
    #[spec("tabs/selection/001")]
    #[test]
    fn selection_001_per_tab_field_round_trip() {
        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (mode_idx, side_ids) = tm
            .open_mode_tab(
                &mode_config("mode", 2),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open mode tab");
        let (orch_idx, role_ids) = tm
            .open_orchestration_tab(&orch_config("orch"), "/work", None, (24, 80))
            .expect("open orchestration tab");

        // Stamp a distinct remembered id onto each tab variant.
        if let Tab::Dashboard {
            selected_session_id,
        } = &mut tm.tabs[0]
        {
            *selected_session_id = Some("sess-dashboard".to_string());
        }
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[mode_idx]
        {
            *focused_pane_id = Some(side_ids[1].clone());
        }
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = &mut tm.tabs[orch_idx]
        {
            *focused_role_pane_id = Some(role_ids[1].clone());
        }

        // Walk across every tab and back; switch_to is a pure index move,
        // so each tab must keep its own id untouched.
        for idx in [0, mode_idx, orch_idx, mode_idx, 0] {
            assert!(tm.switch_to(idx));
        }

        assert!(matches!(
            &tm.tabs[0],
            Tab::Dashboard { selected_session_id: Some(s) } if s == "sess-dashboard"
        ));
        assert!(matches!(
            &tm.tabs[mode_idx],
            Tab::Mode { focused_pane_id: Some(p), .. } if *p == side_ids[1]
        ));
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == role_ids[1]
        ));
    }

    /// Scenario: On a Mode tab focus side pane #2, switch out and assert
    /// the side pane id was captured into the Mode tab's field; switch
    /// back and assert `focus_pane` fired with that exact id. Then clear
    /// the field to `None` and assert switch-in instead focuses the agent
    /// pane.
    #[spec("tabs/selection/002")]
    #[test]
    fn selection_002_switch_to_focus_restore_and_capture() {
        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (mode_idx, side_ids) = tm
            .open_mode_tab(
                &mode_config("mode", 2),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open mode tab");
        // open_mode_tab leaves the mode tab active.
        assert_eq!(tm.active_index(), mode_idx);

        // User focuses side pane #2 on the mode tab.
        let target = side_ids[1].clone();
        pc.focus_pane(&target).unwrap();

        // Switch-out capture records the focused side pane into the tab.
        tm.capture_focus_on_switch_out();
        assert!(matches!(
            &tm.tabs[mode_idx],
            Tab::Mode { focused_pane_id: Some(p), .. } if *p == target
        ));

        // Leave to the dashboard, then come back: restore must focus the
        // remembered side pane.
        assert!(tm.switch_to(0));
        assert!(tm.switch_to(mode_idx));
        tm.restore_focus_on_switch_in();
        assert_eq!(pc.last_focus().as_deref(), Some(target.as_str()));

        // With no remembered side pane, restore focuses the agent pane.
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[mode_idx]
        {
            *focused_pane_id = None;
        }
        tm.restore_focus_on_switch_in();
        assert_eq!(pc.last_focus().as_deref(), Some("agent-m"));
    }

    /// Scenario: On the Dashboard, set `selected_session_id` to the second
    /// card in a filtered list and assert `sync_and_derive_selection`
    /// derives that card's index; then assert the same sync run against a
    /// Mode tab returns `None` and never rewrites the dashboard's id —
    /// the gating that stops cross-tab selection leaks.
    #[spec("tabs/selection/003")]
    #[test]
    fn selection_003_dashboard_derived_index_and_gated_sync() {
        let filtered: &[(&str, Option<&str>)] =
            &[("s1", Some("p1")), ("s2", Some("p2")), ("s3", Some("p3"))];

        let mut dash = Tab::Dashboard {
            selected_session_id: Some("s2".to_string()),
        };
        // No focused pane: index derives purely from the remembered id.
        let idx = crate::ui::sync_and_derive_selection(&mut dash, None, filtered);
        assert_eq!(idx, Some(1));

        // A focused pane that maps to a visible card adopts that card.
        let idx = crate::ui::sync_and_derive_selection(&mut dash, Some("p3"), filtered);
        assert_eq!(idx, Some(2));
        assert!(matches!(
            &dash,
            Tab::Dashboard { selected_session_id: Some(s) } if s == "s3"
        ));

        // Gating: running the sync while a Mode tab is active returns
        // `None` (selected_index left untouched) and cannot touch the
        // dashboard's stored id.
        let mut mode = Tab::Mode {
            id: 1,
            name: "mode".to_string(),
            agent_pane_id: "agent".to_string(),
            mode_manager: Box::new(ModeManager::new(Arc::new(MockPaneController::new()))),
            last_routed_timestamp: HashMap::new(),
            cwd: "/work".to_string(),
            focused_pane_id: None,
        };
        let idx = crate::ui::sync_and_derive_selection(&mut mode, Some("p1"), filtered);
        assert_eq!(idx, None);
        assert!(matches!(
            &dash,
            Tab::Dashboard { selected_session_id: Some(s) } if s == "s3"
        ));
    }

    /// Scenario: A remembered id that's no longer in the filtered list (a
    /// gone session / removed role pane) is cleared and the selection
    /// falls back to the first card. A reactive pane recreation remaps the
    /// focused pane to its successor via the `(closed,new)` pair on BOTH
    /// the active tab (whose new id is returned for re-focus) and a
    /// background (non-active) Mode/Orchestration tab; a vanished pane
    /// with no successor clears the field on either.
    #[spec("tabs/selection/004")]
    #[test]
    fn selection_004_stale_id_fallback_and_reactive_remap() {
        // Dashboard: remembered session id no longer present → cleared + 0.
        let filtered: &[(&str, Option<&str>)] = &[("s1", Some("p1")), ("s2", Some("p2"))];
        let mut dash = Tab::Dashboard {
            selected_session_id: Some("gone".to_string()),
        };
        let idx = crate::ui::sync_and_derive_selection(&mut dash, None, filtered);
        assert_eq!(idx, Some(0));
        assert!(matches!(
            &dash,
            Tab::Dashboard {
                selected_session_id: None
            }
        ));

        // Orchestration: remembered role pane gone from the list → cleared.
        let mut orch = Tab::Orchestration {
            id: 2,
            name: "orch".to_string(),
            role_pane_ids: vec!["p1".to_string(), "p2".to_string()],
            role_statuses: vec![
                OrchestrationRoleStatus::Working,
                OrchestrationRoleStatus::Working,
            ],
            cwd: "/work".to_string(),
            focused_role_pane_id: Some("gone".to_string()),
            start_role_index: 0,
            orchestrator_prompt: None,
            config: orch_config("orch"),
            status: OrchestrationStatus::WaitingForOrchestrator,
        };
        let idx = crate::ui::sync_and_derive_selection(&mut orch, None, filtered);
        assert_eq!(idx, Some(0));
        assert!(matches!(
            &orch,
            Tab::Orchestration {
                focused_role_pane_id: None,
                ..
            }
        ));

        // Reactive remap — ACTIVE tab: the focused side pane was
        // recreated, so follow it to the successor and re-focus that id.
        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (mode_idx, side_ids) = tm
            .open_mode_tab(
                &mode_config("mode", 1),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open mode tab");
        let original = side_ids[0].clone();
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[mode_idx]
        {
            *focused_pane_id = Some(original.clone());
        }
        let remapped =
            tm.remap_focus_after_reactive_change(&[(original.clone(), "pane-new".to_string())]);
        assert_eq!(remapped.as_deref(), Some("pane-new"));
        assert!(matches!(
            &tm.tabs[mode_idx],
            Tab::Mode { focused_pane_id: Some(p), .. } if p == "pane-new"
        ));

        // ACTIVE tab vanished pane with no successor → field cleared,
        // returns None.
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[mode_idx]
        {
            *focused_pane_id = Some("ghost".to_string());
        }
        let remapped =
            tm.remap_focus_after_reactive_change(&[("other".to_string(), "x".to_string())]);
        assert_eq!(remapped, None);
        assert!(matches!(
            &tm.tabs[mode_idx],
            Tab::Mode {
                focused_pane_id: None,
                ..
            }
        ));

        // Reactive remap — BACKGROUND tabs (the review fix). Build a
        // second Mode tab and an Orchestration tab; opening them leaves
        // the LAST-opened tab active, so the earlier Mode tab is now a
        // background tab whose focused reactive pane can still be
        // recreated by `route_reactive_commands`.
        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (bg_mode, bg_sides) = tm
            .open_mode_tab(
                &mode_config("bg-mode", 1),
                "/work",
                "agent-bg".to_string(),
                (24, 80),
            )
            .expect("open background mode tab");
        let (bg_orch, bg_roles) = tm
            .open_orchestration_tab(&orch_config("bg-orch"), "/work", None, (24, 80))
            .expect("open background orchestration tab");
        let (active_mode, active_sides) = tm
            .open_mode_tab(
                &mode_config("active-mode", 1),
                "/work",
                "agent-active".to_string(),
                (24, 80),
            )
            .expect("open active mode tab");
        assert_eq!(tm.active_index(), active_mode);

        let bg_side = bg_sides[0].clone();
        let bg_role = bg_roles[0].clone();
        let active_side = active_sides[0].clone();
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[bg_mode]
        {
            *focused_pane_id = Some(bg_side.clone());
        }
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = &mut tm.tabs[bg_orch]
        {
            *focused_role_pane_id = Some(bg_role.clone());
        }
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[active_mode]
        {
            *focused_pane_id = Some(active_side.clone());
        }

        // One reactive pass recreates the focused pane of the background
        // Mode tab, the background Orchestration tab, AND the active tab.
        let remapped = tm.remap_focus_after_reactive_change(&[
            (bg_side.clone(), "bg-mode-new".to_string()),
            (bg_role.clone(), "bg-orch-new".to_string()),
            (active_side.clone(), "active-new".to_string()),
        ]);
        // Only the ACTIVE tab's new id is returned for controller re-focus.
        assert_eq!(remapped.as_deref(), Some("active-new"));
        // Background Mode tab followed its successor (NOT cleared / defaulted).
        assert!(matches!(
            &tm.tabs[bg_mode],
            Tab::Mode { focused_pane_id: Some(p), .. } if p == "bg-mode-new"
        ));
        // Background Orchestration tab followed its successor too.
        assert!(matches!(
            &tm.tabs[bg_orch],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if p == "bg-orch-new"
        ));
        // Active tab remapped as well.
        assert!(matches!(
            &tm.tabs[active_mode],
            Tab::Mode { focused_pane_id: Some(p), .. } if p == "active-new"
        ));

        // BACKGROUND tab vanished pane with no successor → field cleared,
        // while a tab whose focus is still a live managed pane is left
        // untouched. Reset the active tab to its real side pane so it
        // stays in the managed set, then point the background tab at a
        // ghost id absent from any pair and from its managed set.
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[active_mode]
        {
            *focused_pane_id = Some(active_side.clone());
        }
        if let Tab::Mode {
            focused_pane_id, ..
        } = &mut tm.tabs[bg_mode]
        {
            *focused_pane_id = Some("bg-ghost".to_string());
        }
        let remapped =
            tm.remap_focus_after_reactive_change(&[("unrelated".to_string(), "z".to_string())]);
        // No tab matched a pair, so nothing is returned for re-focus.
        assert_eq!(remapped, None);
        // Background tab's stale ghost focus was cleared (M4 fallback).
        assert!(matches!(
            &tm.tabs[bg_mode],
            Tab::Mode {
                focused_pane_id: None,
                ..
            }
        ));
        // Active tab's still-live focus was left intact.
        assert!(matches!(
            &tm.tabs[active_mode],
            Tab::Mode { focused_pane_id: Some(p), .. } if *p == active_side
        ));
    }

    /// Scenario: Drive the Problem-section walkthrough across a Dashboard,
    /// two Mode tabs, and one Orchestration tab. Focus a side pane on each
    /// Mode tab, switch through the tabs, and assert every switch-in
    /// restores that tab's own remembered pane (or its default) via a
    /// `focus_pane` call — the cross-tab focus memory the PRD requires.
    #[spec("tabs/selection/005")]
    #[test]
    fn selection_005_integration_multi_tab_walkthrough() {
        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (m1, m1_sides) = tm
            .open_mode_tab(
                &mode_config("mode-1", 2),
                "/work",
                "agent-1".to_string(),
                (24, 80),
            )
            .expect("mode 1");
        let (m2, m2_sides) = tm
            .open_mode_tab(
                &mode_config("mode-2", 2),
                "/work",
                "agent-2".to_string(),
                (24, 80),
            )
            .expect("mode 2");
        let (orch, role_ids) = tm
            .open_orchestration_tab(&orch_config("orch"), "/work", None, (24, 80))
            .expect("orch");

        // Land on mode-1 and focus its side pane #1.
        assert!(tm.switch_to(m1));
        let m1_target = m1_sides[0].clone();
        pc.focus_pane(&m1_target).unwrap();

        // Switch to mode-2 (capture m1's focus, restore m2's default agent
        // pane since it has no remembered pane yet).
        tm.capture_focus_on_switch_out();
        assert!(tm.switch_to(m2));
        tm.restore_focus_on_switch_in();
        assert_eq!(pc.last_focus().as_deref(), Some("agent-2"));

        // Focus a side pane on mode-2, then jump to the orchestration tab:
        // its default focus is the start (orchestrator) role pane.
        let m2_target = m2_sides[1].clone();
        pc.focus_pane(&m2_target).unwrap();
        tm.capture_focus_on_switch_out();
        assert!(tm.switch_to(orch));
        tm.restore_focus_on_switch_in();
        assert_eq!(pc.last_focus().as_deref(), Some(role_ids[0].as_str()));

        // Back to mode-1: restore its own remembered side pane.
        tm.capture_focus_on_switch_out();
        assert!(tm.switch_to(m1));
        tm.restore_focus_on_switch_in();
        assert_eq!(pc.last_focus().as_deref(), Some(m1_target.as_str()));

        // And to mode-2: restore the side pane focused there earlier.
        tm.capture_focus_on_switch_out();
        assert!(tm.switch_to(m2));
        tm.restore_focus_on_switch_in();
        assert_eq!(pc.last_focus().as_deref(), Some(m2_target.as_str()));

        // Sanity: every assertion above came from a real focus_pane call.
        assert!(pc.focus_calls().len() >= 6);
    }
}
