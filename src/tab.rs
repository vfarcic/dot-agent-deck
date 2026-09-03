use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::agent_pty::TabMembership;
use crate::event::{AgentType, EventType};
use crate::mode_manager::{ModeManager, ModeManagerError};
use crate::pane::{AgentSpawnOptions, CloseTabOutcome, PaneController, close_panes_concurrently};
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
        /// PRD #313: whether this tab's focused pane takes the whole frame,
        /// with the card sidebar and the non-focused panes not drawn. Ephemeral
        /// and per-tab, exactly like [`Tab::Orchestration::zoomed`] — the two
        /// are deliberately separate values rather than one global, so zooming
        /// the Dashboard does not silently zoom an orchestration tab you were
        /// supervising, or the reverse.
        zoomed: bool,
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
        /// Edge-trigger state for the all-clear focus move — whether any role
        /// pane on this tab was `WaitingForInput` as of the last
        /// [`TabManager::observe_waiting_panes`] call. That observer runs once
        /// per frame while the deck is locked, outside the auto-focus chain, so
        /// this is always current for the frame — which is what lets the
        /// all-clear move fire exactly once, on the frame where this flips from
        /// `true` to `false`, rather than every frame nothing is waiting.
        /// Starts `false`: a freshly-opened tab has no "was waiting" history to
        /// edge-trigger off of.
        had_waiting_pane: bool,
        /// The latched `true` → `false` transition of `had_waiting_pane`, set
        /// by [`TabManager::observe_waiting_panes`] and consumed by
        /// [`TabManager::auto_focus_all_clear`]. Splitting the observation from
        /// the focus move is what makes the edge survive: the move only runs
        /// when the chain reaches its branch, while the observation must happen
        /// every frame regardless of which branch wins. Recording the edge in
        /// the mover instead would mean a waiting episode whose first frame was
        /// consumed by `auto_focus_waiting_pane` was never remembered, and the
        /// all-clear move for it never fired.
        all_clear_pending: bool,
        /// PRD #336: whether the sidebar/pane-column split is toggled to the
        /// narrower-sidebar 25/75 ratio. `false` = the 34/66 default.
        ///
        /// The split is GLOBAL, not per-tab: sidebar width is a reading
        /// preference, not a property of which orchestration is open. This
        /// field is therefore a per-tab *mirror* of
        /// [`TabManager::orchestration_split_narrow`], which is the single
        /// source of truth. Only two writers exist, both inside `TabManager`
        /// and both in this file — the construction sites (which seed it from
        /// the global) and [`TabManager::toggle_orchestration_split`] (which
        /// rewrites the global and every open orchestration tab in one pass).
        /// Nothing outside `TabManager` may write it, so the invariant "every
        /// `Tab::Orchestration::split_narrow` equals the global" holds at every
        /// point a reader could observe it, with no call-ordering requirement.
        ///
        /// Deliberately not persisted across launches — a fresh process starts
        /// at the 34/66 default (PRD #336 keeps persistence out of scope).
        split_narrow: bool,
        /// PRD #313: whether this tab's focused role pane is zoomed to the
        /// whole frame, with the sidebar and the non-focused panes not drawn.
        /// `false` = the normal supervisory view.
        ///
        /// Unlike [`Self::Orchestration::split_narrow`] this is **per-tab and
        /// is itself the source of truth** — there is deliberately no
        /// `TabManager`-level global mirroring it. tmux zooms a *window*, not a
        /// session, and the two states answer different questions: the split is
        /// a standing reading preference, while zoom says "I have stopped
        /// supervising and am working in *this* agent". A tab the user never
        /// zoomed must not silently lose its sidebar, which a global would do —
        /// and keeping it per-tab means no cross-tab broadcast loop like
        /// [`TabManager::toggle_orchestration_split`]'s is needed at all.
        ///
        /// Ephemeral in the same way, and more so: not persisted across
        /// launches *and* not written to the saved session, so a detach/reattach
        /// always returns the full supervisory view. It is pure presentation —
        /// nothing about it reaches the daemon.
        zoomed: bool,
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
    /// PRD #336: the orchestration sidebar/pane-column split, as a GLOBAL
    /// reading preference — `true` = the narrower-sidebar 25/75 ratio,
    /// `false` = the 34/66 default. Single source of truth; see
    /// [`Tab::Orchestration::split_narrow`] for why it lives here rather than
    /// on the tab, and [`toggle_orchestration_split`](Self::toggle_orchestration_split)
    /// for the one place it changes.
    orchestration_split_narrow: bool,
}

impl TabManager {
    pub fn new(pane_controller: Arc<dyn PaneController>) -> Self {
        Self {
            tabs: vec![Tab::Dashboard {
                selected_session_id: None,
                // PRD #313: a fresh deck is never zoomed; zoom is ephemeral and
                // is not restored from a saved session.
                zoomed: false,
            }],
            active_index: 0,
            next_id: 1,
            pane_controller,
            orchestration_split_narrow: false,
        }
    }

    /// PRD #336: the current GLOBAL orchestration split. Read by the spawn
    /// path so a role pane's PTY opens at the width it will actually be
    /// rendered at, rather than at the default the tab no longer starts from.
    pub fn orchestration_split_narrow(&self) -> bool {
        self.orchestration_split_narrow
    }

    /// PRD #336: flip the GLOBAL orchestration split and apply it to every
    /// open orchestration tab, returning the new value.
    ///
    /// The split is global because sidebar width is a reading preference, not
    /// a property of which orchestration is open: per-tab state meant every
    /// newly opened tab reset to 34/66, so anyone who prefers the narrow
    /// sidebar re-toggled forever.
    ///
    /// Owning it here — rather than in a thread-local or a free-floating
    /// global, as an earlier revision did — is what keeps the global scope
    /// from recreating that revision's ordering hazard. `TabManager` already
    /// owns `tabs`, so the write to the global and the writes to every tab
    /// happen together in one `&mut self` method that cannot be observed
    /// half-applied. No caller has to remember to sync anything before
    /// rendering, which is precisely the assumption the thread-local rested
    /// on.
    pub fn toggle_orchestration_split(&mut self) -> bool {
        let narrow = !self.orchestration_split_narrow;
        self.orchestration_split_narrow = narrow;
        for tab in &mut self.tabs {
            if let Tab::Orchestration { split_narrow, .. } = tab {
                *split_narrow = narrow;
            }
        }
        narrow
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
    /// (stale-id fallback). Dashboard is a no-op HERE — its selection is keyed by
    /// session id, not a pane id, and `TabManager` carries no session→pane map, so
    /// its remembered card is re-focused by `switch_tab_with_focus` (which has the
    /// live snapshot) instead. The two decks are otherwise symmetric: each keeps
    /// its remembered selection on leave and re-focuses it on return.
    /// Returns the pane id focus was restored to (if any) so callers can keep a
    /// focus baseline in sync — PRD #113 (PR #151) uses it to pre-seed
    /// `UiState.last_focused_pane_id`, so the switch-induced focus change isn't
    /// read as a reactivating user transition by the next reconcile frame.
    pub fn restore_focus_on_switch_in(&mut self) -> Option<String> {
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
        if let Some(id) = target.as_ref() {
            let _ = self.pane_controller.focus_pane(id);
        }
        target
    }

    /// Steer the active tab's focus to the lowest-`role_pane_ids`-order pane
    /// that is `WaitingForInput`, so the user always lands on the pane most
    /// likely to need their attention next. No-op for any active tab that isn't
    /// `Tab::Orchestration`, and by construction never touches another tab or
    /// switches which tab is active.
    ///
    /// Re-evaluated from scratch on every call (intended to be driven once per
    /// frame from the render loop, and only while the command-entry lock is
    /// engaged): if no pane in the active tab is waiting, `focused_role_pane_id`
    /// is left untouched and `None` is returned; otherwise the lowest-order
    /// waiting pane is computed and, only when it differs from the currently
    /// stored focus, `focused_role_pane_id` is updated and `Some(new_id)` is
    /// returned so the caller can apply the change on the pane controller
    /// (`None` when the target is already focused, to avoid flicker).
    ///
    /// Ascending `role_pane_ids` order rather than longest-waiting-first: a
    /// "longest blocked" ordering would need a new per-pane `waiting_since`
    /// timestamp and is a separate change.
    pub fn auto_focus_waiting_pane(
        &mut self,
        pane_status: &HashMap<&str, crate::state::SessionStatus>,
    ) -> Option<String> {
        let Tab::Orchestration {
            role_pane_ids,
            focused_role_pane_id,
            ..
        } = &mut self.tabs[self.active_index]
        else {
            return None;
        };
        let target = role_pane_ids.iter().find(|id| {
            matches!(
                pane_status.get(id.as_str()),
                Some(crate::state::SessionStatus::WaitingForInput)
            )
        })?;
        if focused_role_pane_id.as_deref() == Some(target.as_str()) {
            return None;
        }
        *focused_role_pane_id = Some(target.clone());
        Some(target.clone())
    }

    /// The OBSERVATION half of the all-clear edge trigger, and the sole writer
    /// of the active Orchestration tab's `had_waiting_pane`: records whether any
    /// role pane on that tab is `WaitingForInput` right now, and latches the
    /// `true` → `false` transition into `all_clear_pending` for
    /// [`Self::auto_focus_all_clear`] to consume. Moves no focus and returns
    /// nothing. No-op for any active tab that isn't `Tab::Orchestration`.
    ///
    /// **Must be called exactly once per frame while the deck is locked, before
    /// the auto-focus chain runs** — never from inside one of that chain's
    /// branches. Being outside the chain is the whole point of splitting this
    /// out: the render loop reaches `auto_focus_all_clear` only when
    /// `auto_focus_waiting_pane` returned `None`, so folding the observation in
    /// there would mean the frame a role first went `WaitingForInput` — the
    /// frame where `auto_focus_waiting_pane` steers focus onto it and therefore
    /// wins the chain — recorded nothing. A waiting episode observed in a single
    /// frame would be forgotten entirely and its all-clear move never fire.
    /// Observing outside the chain makes "current for the frame" a property of
    /// the state rather than of the branch ordering.
    ///
    /// The render loop skips this call, and the whole chain with it, on every
    /// frame the command-entry lock is disengaged, so an unlocked deck makes no
    /// focus decision that could fight the human. The compensation is
    /// [`Self::clear_waiting_pane_latch`], which the locked→unlocked toggle
    /// calls so a latch set before the unlock cannot survive across the unlocked
    /// stretch and be misread as a fresh all-clear edge on re-lock. Within a
    /// locked stretch the once-per-frame-before-the-chain contract is unchanged.
    pub fn observe_waiting_panes(
        &mut self,
        pane_status: &HashMap<&str, crate::state::SessionStatus>,
    ) {
        let Tab::Orchestration {
            role_pane_ids,
            had_waiting_pane,
            all_clear_pending,
            ..
        } = &mut self.tabs[self.active_index]
        else {
            return;
        };
        let now_waiting = role_pane_ids.iter().any(|id| {
            matches!(
                pane_status.get(id.as_str()),
                Some(crate::state::SessionStatus::WaitingForInput)
            )
        });
        if *had_waiting_pane && !now_waiting {
            *all_clear_pending = true;
        }
        *had_waiting_pane = now_waiting;
    }

    /// Reset EVERY Orchestration tab's waiting-episode edge state
    /// (`had_waiting_pane` / `all_clear_pending`) to "nothing seen yet". Tabs
    /// that aren't `Tab::Orchestration` carry no such state and are skipped.
    ///
    /// Deck-wide rather than active-tab-only because the lock it compensates for
    /// is itself deck-global — one value for every tab. Unlocking stops the
    /// observation chain for ALL Orchestration tabs at once, so every one of
    /// them can be left holding a frozen latch, not merely whichever happened to
    /// be active when the human pressed `Ctrl+E`. Clearing only the active tab
    /// leaves exactly the same stale-edge bug alive on the others: the human
    /// unlocks from tab `B`, tab `A`'s episode resolves unobserved, and on
    /// re-lock `A`'s surviving `true` is misread as a fresh all-clear that yanks
    /// focus off wherever `A` was left.
    ///
    /// Called from the locked→unlocked half of the command-entry lock toggle,
    /// and only from there. It exists because the render loop stops running
    /// [`Self::observe_waiting_panes`] while the deck is unlocked, which makes
    /// one — and only one — trace go wrong. The episode has to **straddle** the
    /// transition: a role goes `WaitingForInput` while LOCKED (so the latch is
    /// genuinely set), the human unlocks mid-episode (the chain stops, freezing
    /// the latch at `true`), the pane then resolves unobserved, and on re-lock
    /// that stale `true` meets a now-idle status and is misread as a fresh
    /// `true` → `false` edge — yanking focus to the orchestrator, away from
    /// wherever the human deliberately put it. Clearing on the transition makes
    /// re-locking start from a clean slate.
    ///
    /// An episode that both begins *and* ends inside the unlocked stretch needs
    /// no fix and never did: with the chain fully skipped, nothing ever touches
    /// the latch, so nothing goes stale. A test written against that simpler
    /// wording passes without this method and proves nothing.
    pub fn clear_waiting_pane_latch(&mut self) {
        for tab in &mut self.tabs {
            let Tab::Orchestration {
                had_waiting_pane,
                all_clear_pending,
                ..
            } = tab
            else {
                continue;
            };
            *had_waiting_pane = false;
            *all_clear_pending = false;
        }
    }

    /// Edge-triggered sibling of [`Self::auto_focus_waiting_pane`]: the instant
    /// the active Orchestration tab's last `WaitingForInput` role pane resolves,
    /// focus moves to that tab's orchestrator role
    /// (`role_pane_ids[start_role_index]`) exactly once. No-op for any active tab
    /// that isn't `Tab::Orchestration`.
    ///
    /// Deliberately edge- rather than level-triggered: the move fires only on the
    /// `true` → `false` transition [`Self::observe_waiting_panes`] latched into
    /// `all_clear_pending`, which this method consumes. A level-triggered version
    /// (fire whenever nothing is waiting) would pin focus to the orchestrator on
    /// every frame — the human could never look at another pane at all.
    ///
    /// Intended to be called once per frame from the same render-loop site as
    /// `auto_focus_waiting_pane`, and only when THAT call returned `None` for the
    /// frame (nothing left to steer toward). That gate is safe *because* the
    /// observation is not done here: on the latch frame nothing is waiting, so
    /// `auto_focus_waiting_pane` returns `None` by construction and this method
    /// is always reached. Skipping the call for a frame (the render loop does,
    /// when input is already pending) only DEFERS the move — the latch survives
    /// until it is consumed. Already-correct focus still consumes the latch, and
    /// is otherwise a no-op matching `auto_focus_waiting_pane`'s no-flicker
    /// behaviour.
    pub fn auto_focus_all_clear(&mut self) -> Option<String> {
        let Tab::Orchestration {
            role_pane_ids,
            focused_role_pane_id,
            start_role_index,
            all_clear_pending,
            ..
        } = &mut self.tabs[self.active_index]
        else {
            return None;
        };
        if !std::mem::take(all_clear_pending) {
            return None;
        }
        let orchestrator = role_pane_ids.get(*start_role_index)?;
        if focused_role_pane_id.as_deref() == Some(orchestrator.as_str()) {
            return None;
        }
        let orchestrator = orchestrator.clone();
        *focused_role_pane_id = Some(orchestrator.clone());
        Some(orchestrator)
    }

    /// The whole lock-governed focus decision for one frame, in the order the
    /// render loop applies it: steer toward the lowest-order waiting pane, and
    /// only when there is nothing left to steer toward, take the latched
    /// all-clear move back to the orchestrator. Intended to be called once per
    /// frame from `src/ui.rs`, immediately after the unconditional
    /// [`Self::observe_waiting_panes`] — which deliberately stays OUTSIDE this
    /// method, because it must run on every locked frame regardless of pending
    /// input (see its doc comment for the single-frame episode it would
    /// otherwise forget).
    ///
    /// `input_pending` is the caller's single `crossterm::event::poll(0ms)`
    /// peek for the frame, threaded in as a plain `bool` so this stays pure and
    /// unit-testable. When it is true, BOTH branches are deferred: a focus move
    /// applied on this frame lands before the event loop drains what is already
    /// queued, and that key — aimed at the pane that was focused when it was
    /// typed — would then be forwarded to the newly focused pane instead. The
    /// all-clear branch has always been guarded this way; the waiting branch
    /// needs the same guard, because a lower-role-order pane going
    /// `WaitingForInput` steals focus from the waiting pane the user is
    /// mid-answer to, and since the new pane is itself `WaitingForInput` the
    /// command-entry lock's carve-out forwards those queued keystrokes straight
    /// through to it.
    ///
    /// The early return skips the calls entirely rather than making them and
    /// discarding their results, and that distinction matters:
    /// [`Self::auto_focus_waiting_pane`] mutates `focused_role_pane_id` as a
    /// side effect before returning `Some`, so calling it while deferring would
    /// desync this `TabManager`'s bookkeeping from the pane controller's real
    /// focus, and [`Self::auto_focus_all_clear`] would consume its
    /// `all_clear_pending` latch for a move that never happened. Not calling
    /// them means the next frame recomputes cleanly — the waiting target is
    /// derived from the status snapshot every frame with no one-shot latch to
    /// lose, and the all-clear latch survives until it is genuinely consumed.
    /// Deferral, not loss, in both cases.
    pub fn auto_focus_locked(
        &mut self,
        pane_status: &HashMap<&str, crate::state::SessionStatus>,
        input_pending: bool,
    ) -> Option<String> {
        if input_pending {
            return None;
        }
        self.auto_focus_waiting_pane(pane_status)
            .or_else(|| self.auto_focus_all_clear())
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
    /// PRD #84 M4/M5: panes are spawned at their layout dims and then reconciled
    /// to the exact inner area by the per-frame `resize_panes_to_layout` pass —
    /// there is no longer a manual post-spawn resize sweep to wait on, so
    /// commands started here run at the correct PTY size.
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
        // PRD #107 / orchestration-identity fix: the user's form name (the
        // name typed or accepted in the new-pane form — usually the dir
        // basename). Routed to the tab TITLE only (`Tab::Orchestration.name`,
        // rendered by `Tab::label`); when `None`/empty the title falls back
        // to the resolved canonical name. The orchestration IDENTITY stamped
        // on every role pane's `TabMembership` is ALWAYS derived from
        // `resolve_orchestration_name(&config.name, cwd)` — never from this
        // title — so the daemon's delegate role lookup compares against the
        // canonical on-disk config name (see the in-body comment below).
        display_title: Option<&str>,
        // PRD #76 M2.15: initial PTY dims for every role pane in this
        // orchestration. The caller computes these from
        // `terminal.get_frame().area()` + the dashboard-layout helper, so
        // the daemon-side PTY opens at the viewport size instead of the
        // legacy 24×80. Callers without a real viewport (tests) pass
        // `(24, 80)`. PRD #84 M4: the per-frame `resize_panes_to_layout`
        // pass reconciles each role pane to its exact inner area (and the
        // active tab's focus state) on the first frame.
        spawn_dims: (u16, u16),
    ) -> Result<(usize, Vec<String>), TabError> {
        let mut role_pane_ids: Vec<String> = Vec::with_capacity(config.roles.len());
        let (spawn_rows, spawn_cols) = spawn_dims;

        // PRD #201 native prompt delivery: when the START (orchestrator) role is
        // a Pi pane, its first prompt is delivered NATIVELY — stashed daemon-side
        // at spawn time (`AgentSpawnOptions.seed`) and pulled by the pane's
        // extension via `get-seed` → `pi.sendUserMessage`, dissolving the last
        // keystroke-injection workaround. For a Pi start role we therefore (a)
        // attach the seed at spawn below, and (b) DROP the tab's
        // `orchestrator_prompt` so the render-loop PTY-injection site
        // (`ui.rs`) skips it — the daemon owns delivery (native pull + its own
        // PTY-injection safety net). A non-Pi start role is unchanged: no seed,
        // and the tab keeps `orchestrator_prompt` for the existing injection.
        let start_role_is_pi = config
            .roles
            .iter()
            .find(|r| r.start)
            // Issue #308: the role's RESOLVED type — its `agent = "…"`
            // declaration when it made one, else the type derived from the
            // command — so a Pi orchestrator launched through a wrapper script
            // still gets native seeding instead of silently falling back to
            // keystroke injection.
            .map(|r| r.resolved_agent_type() == Some(AgentType::Pi))
            .unwrap_or(false);

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

        // PRD #107 / orchestration-identity fix: decouple the display TITLE
        // from the orchestration IDENTITY. `resolved_name` is the canonical
        // identity (config name, or the cwd-basename fallback for an unnamed
        // orchestration — matching `load_project_config`'s normalization so
        // the daemon's `lookup_orchestration_role` resolves the role and its
        // `clear`/`prompt_template`). The `title` is purely cosmetic and may
        // be the user's form name. Pre-fix these were one field, so the
        // form/basename leaked into the identity and broke the delegate
        // respawn in every worktree whose basename != the config name.
        // Normalise the user-typed form name once: `None`/empty collapses to
        // `None` so both the local title and the persisted membership fall
        // back to the canonical `resolved_name`.
        let display_title_owned = display_title.filter(|t| !t.is_empty()).map(str::to_string);
        let title = display_title_owned
            .clone()
            .unwrap_or_else(|| resolved_name.clone());

        // PRD #140 M1.2: mint ONE instance token for the whole tab, before
        // the role loop, and stamp it on every role's membership. This is
        // what makes two tabs of the SAME orchestration in the SAME directory
        // two distinct daemon-side routing groups — `resolved_name` and `cwd`
        // are byte-identical between them, so without the token the daemon
        // cross-delivers delegate/work-done between the two tabs (issue #140).
        let orchestration_id = crate::agent_pty::mint_orchestration_id();

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
                    // PRD #107 follow-up: persist the user-typed title on
                    // every role pane so reattach restores it (see the
                    // hydration path in `open_orchestration_tab_with_existing_role_panes`).
                    display_title: display_title_owned.clone(),
                    // PRD #140 M1.2: same token on every role of this tab.
                    orchestration_id: Some(orchestration_id.clone()),
                }),
                rows: spawn_rows,
                cols: spawn_cols,
                // PRD #76 M2.13: tag each role's daemon-side registry
                // entry with the agent type inferred from its command
                // (e.g. `claude` → `ClaudeCode`). The daemon echoes this
                // back via `list_agents` on reconnect so the hydration
                // path can build the placeholder session with the right
                // type instead of "No agent".
                // Issue #308: a role may DECLARE its agent (`agent = "codex"`)
                // for a command no parser can resolve — `devbox run -- codex`,
                // `make codex`, a bespoke `run-codex.sh`. The declaration wins
                // here, so such a role badges and wraps at spawn instead of
                // reading "No agent" until its first delegated task.
                agent_type: role.resolved_agent_type(),
                // PRD #201: seed only the Pi start-role pane for native pull.
                seed: if role.start && start_role_is_pi {
                    orchestrator_prompt.clone()
                } else {
                    None
                },
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
            // Display TITLE only (see `title` above). The IDENTITY lives on
            // each role pane's `TabMembership::Orchestration.name`, which used
            // `resolved_name`.
            name: title,
            role_pane_ids: role_pane_ids.clone(),
            role_statuses: vec![OrchestrationRoleStatus::Waiting; config.roles.len()],
            cwd: cwd.to_string(),
            focused_role_pane_id: None,
            start_role_index,
            // PRD #201: a Pi start role is seeded natively at spawn, so drop the
            // prompt here — the render-loop injection site skips a `None`.
            orchestrator_prompt: if start_role_is_pi {
                None
            } else {
                orchestrator_prompt
            },
            config: config.clone(),
            status: OrchestrationStatus::WaitingForOrchestrator,
            // A brand-new tab has no waiting-episode history to edge-trigger
            // off of.
            had_waiting_pane: false,
            all_clear_pending: false,
            // PRD #336: adopt the current GLOBAL split, not the 34/66 default.
            split_narrow: self.orchestration_split_narrow,
            // PRD #313: zoom is PER-TAB, so a newly opened tab always starts
            // unzoomed regardless of what any other tab is doing.
            zoomed: false,
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
        // PRD #107 follow-up: the user-typed title the daemon echoed back on
        // each role pane's `TabMembership::Orchestration.display_title`. Used
        // for the tab TITLE so detach/reattach preserves the name the user
        // entered; `None`/empty falls back to the canonical resolved name
        // (the pre-fix behaviour). The IDENTITY still derives from
        // `resolve_orchestration_name` below — this is title-only.
        display_title: Option<&str>,
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

        // Title-only: prefer the user-typed title the daemon round-tripped,
        // falling back to the canonical resolved name when absent/empty.
        let name = display_title
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| resolve_orchestration_name(&config.name, std::path::Path::new(cwd)));

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
            // Not persisted, so a rebuilt tab starts with no waiting-episode
            // history — the same clean slate a fresh one gets above.
            had_waiting_pane: false,
            all_clear_pending: false,
            // PRD #336: a hydrated/restored tab adopts the current GLOBAL
            // split, exactly like a freshly opened one — the split belongs to
            // the session, not to the tab. The global itself is not persisted
            // across launches, so a restore during startup lands on the 34/66
            // default; a restore mid-session picks up whatever is in effect.
            split_narrow: self.orchestration_split_narrow,
            // PRD #313: zoom is ephemeral view state and is never persisted, so
            // a hydrated/restored tab comes back with the full supervisory view.
            zoomed: false,
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
    /// Callers inspect `outcome.closed` to know which dashboard cards
    /// may be removed and `outcome.failed` to know which cards must be
    /// preserved (with the rendered error surfaced via
    /// `ui.status_message`).
    ///
    /// **PRD #241 — the tab is removed LAST, and only if every pane
    /// closed.** Until this PRD the tab was removed from `self.tabs`
    /// *before* the per-pane closes ran, so a genuine `stop-agent`
    /// failure produced an incoherent result: the tab was gone while
    /// the failed pane's card was deliberately retained "so the user
    /// can retry" — with nothing left to press `Ctrl+W` on. Now:
    ///
    /// * every pane closes first (concurrently — see
    ///   [`close_panes_concurrently`]);
    /// * `outcome.is_clean()` removes the tab, exactly as before;
    /// * any failure keeps the tab, and the panes that DID close are
    ///   forgotten from it via [`Self::forget_closed_panes`].
    ///
    /// Closing is deliberately **not transactional**: panes that
    /// already closed stay closed. The retained tab therefore holds
    /// only what could not be stopped, which is both the honest state
    /// and the one where a retry re-attempts exactly the failures.
    pub fn close_tab(&mut self, index: usize) -> Result<CloseTabOutcome, TabError> {
        if index == 0 {
            return Err(TabError::CannotCloseDashboard);
        }
        if index >= self.tabs.len() {
            return Err(TabError::IndexOutOfBounds(index));
        }

        // Enumerate the tab's panes WITHOUT mutating it — the tab has to
        // survive intact in case a close fails. `managed_pane_ids` lists the
        // same persistent-then-reactive side panes `deactivate_mode` used to
        // close here, in the same order, and the agent pane follows them.
        let pane_ids: Vec<String> = match &self.tabs[index] {
            Tab::Mode {
                mode_manager,
                agent_pane_id,
                ..
            } => {
                let mut ids = mode_manager.managed_pane_ids();
                // An empty id is the "no agent pane" marker, not a pane.
                if !agent_pane_id.is_empty() {
                    ids.push(agent_pane_id.clone());
                }
                ids
            }
            Tab::Orchestration { role_pane_ids, .. } => role_pane_ids
                .iter()
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
                .filter(|id| !id.is_empty() && !crate::ui::is_dead_slot_pane_id(id))
                .cloned()
                .collect(),
            Tab::Dashboard { .. } => Vec::new(),
        };

        let outcome = close_panes_concurrently(self.pane_controller.as_ref(), &pane_ids);

        if !outcome.is_clean() {
            // At least one agent is still alive on the daemon. Keep the tab so
            // the user has something to retry against, minus whatever really
            // did close.
            self.forget_closed_panes(index, &outcome.closed);
            return Ok(outcome);
        }

        self.tabs.remove(index);

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

    /// PRD #241: strike the panes that successfully closed off a tab that is
    /// being KEPT because a sibling pane's close failed.
    ///
    /// Successfully-closed panes are already out of the controller's registry,
    /// so leaving their ids on the tab would make the next `Ctrl+W` re-close
    /// them, collect "Pane N not found" errors, and keep the tab forever — the
    /// same permanently-unclosable state issue #218 reported for a single card.
    /// Closed side panes are dropped from the mode's pools; a closed agent pane
    /// or orchestration role slot becomes the pre-existing empty-string dead
    /// slot, which every pane-id consumer (`close_tab`, `all_managed_pane_ids`,
    /// `tab_index_for_pane`, the resize pass) already skips. Focus pointing at
    /// a pane that no longer exists is reset to the tab's default.
    fn forget_closed_panes(&mut self, index: usize, closed: &[String]) {
        if closed.is_empty() {
            return;
        }
        let gone: HashSet<&str> = closed.iter().map(String::as_str).collect();
        match &mut self.tabs[index] {
            Tab::Mode {
                mode_manager,
                agent_pane_id,
                focused_pane_id,
                ..
            } => {
                mode_manager.forget_panes(&gone);
                if gone.contains(agent_pane_id.as_str()) {
                    agent_pane_id.clear();
                }
                if focused_pane_id
                    .as_deref()
                    .is_some_and(|id| gone.contains(id))
                {
                    *focused_pane_id = None;
                }
            }
            Tab::Orchestration {
                role_pane_ids,
                role_statuses,
                focused_role_pane_id,
                ..
            } => {
                for (slot, id) in role_pane_ids.iter_mut().enumerate() {
                    if gone.contains(id.as_str()) {
                        id.clear();
                        // Matches the convention `open_orchestration_tab_with_existing_role_panes`
                        // already uses for an absent role pane.
                        if let Some(status) = role_statuses.get_mut(slot) {
                            *status = OrchestrationRoleStatus::Failed;
                        }
                    }
                }
                if focused_role_pane_id
                    .as_deref()
                    .is_some_and(|id| gone.contains(id))
                {
                    *focused_role_pane_id = None;
                }
            }
            Tab::Dashboard { .. } => {}
        }
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
                // `close_tab` and `all_managed_pane_ids`. No production
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
            agent: None,
            name: name.to_string(),
            init_command: None,
            seed_prompt: None,
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
            default: false,
            name: name.to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    agent: None,
                    name: "orchestrator".to_string(),
                    command: "echo orch".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    agent: None,
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

    /// Scenario: Create an orchestration with a user-typed name ("My Custom
    /// Run") and confirm the tab shows it; then simulate detach/reattach via
    /// the hydration entry point and confirm the user-typed title is
    /// restored (PRD #107 follow-up — the daemon round-trips it on
    /// `TabMembership::Orchestration.display_title`). Finally, reattach with
    /// no persisted title and confirm the tab falls back to the canonical
    /// config name, matching daemon-spawned/older-client behaviour.
    #[test]
    fn orchestration_010_reattach_preserves_user_title() {
        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());

        // Create: the user-typed form name becomes the tab title.
        let (created_idx, _) = tm
            .open_orchestration_tab(
                &orch_config("orch"),
                "/work",
                None,
                Some("My Custom Run"),
                (24, 80),
            )
            .expect("create orchestration");
        assert_eq!(tm.tab_labels()[created_idx], "My Custom Run");

        // Reattach with the persisted title (the value the daemon
        // round-trips via `TabMembership::Orchestration.display_title`): the
        // tab keeps the user-typed name instead of the canonical config name.
        let (reattach_idx, _) = tm
            .open_orchestration_tab_with_existing_role_panes(
                &orch_config("orch"),
                "/work",
                vec![Some("p0".into()), Some("p1".into())],
                Some("My Custom Run"),
            )
            .expect("reattach with title");
        assert_eq!(tm.tab_labels()[reattach_idx], "My Custom Run");

        // Reattach without a persisted title falls back to the canonical
        // resolved name (the config name) — the pre-fix behaviour preserved
        // for daemon-spawned/older orchestrations.
        let (fallback_idx, _) = tm
            .open_orchestration_tab_with_existing_role_panes(
                &orch_config("orch"),
                "/work",
                vec![Some("p0".into()), Some("p1".into())],
                None,
            )
            .expect("reattach without title");
        assert_eq!(tm.tab_labels()[fallback_idx], "orch");
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
            .open_orchestration_tab(&orch_config("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");

        // Stamp a distinct remembered id onto each tab variant.
        if let Tab::Dashboard {
            selected_session_id,
            ..
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
            Tab::Dashboard { selected_session_id: Some(s), .. } if s == "sess-dashboard"
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
            zoomed: false,
        };
        // No focused pane: index derives purely from the remembered id.
        let idx = crate::ui::sync_and_derive_selection(&mut dash, None, filtered, None);
        assert_eq!(idx, Some(1));

        // A focused pane that maps to a visible card adopts that card.
        let idx = crate::ui::sync_and_derive_selection(&mut dash, Some("p3"), filtered, None);
        assert_eq!(idx, Some(2));
        assert!(matches!(
            &dash,
            Tab::Dashboard { selected_session_id: Some(s), .. } if s == "s3"
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
        let idx = crate::ui::sync_and_derive_selection(&mut mode, Some("p1"), filtered, None);
        assert_eq!(idx, None);
        assert!(matches!(
            &dash,
            Tab::Dashboard { selected_session_id: Some(s), .. } if s == "s3"
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
            zoomed: false,
        };
        let idx = crate::ui::sync_and_derive_selection(&mut dash, None, filtered, None);
        assert_eq!(idx, Some(0));
        assert!(matches!(
            &dash,
            Tab::Dashboard {
                selected_session_id: None,
                ..
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
            had_waiting_pane: false,
            all_clear_pending: false,
            split_narrow: false,
            zoomed: false,
        };
        let idx = crate::ui::sync_and_derive_selection(&mut orch, None, filtered, None);
        assert_eq!(idx, Some(0));
        assert!(matches!(
            &orch,
            Tab::Orchestration {
                focused_role_pane_id: None,
                ..
            }
        ));

        // Two visible cards on ONE role pane: the highlight must be able to
        // rest on either. The Orchestration arm keys on pane id, and
        // `position` returns the FIRST match, so re-deriving every frame used
        // to pin the highlight to index 0 and make the second card impossible
        // to select — the observable symptom of a duplicated Pi card sitting
        // at deck indices 3 and 4 with only the first selectable.
        let dup: &[(&str, Option<&str>)] = &[("s-a", Some("p1")), ("s-b", Some("p1"))];
        let mut dup_tab = Tab::Orchestration {
            id: 3,
            name: "orch".to_string(),
            role_pane_ids: vec!["p1".to_string()],
            role_statuses: vec![OrchestrationRoleStatus::Working],
            cwd: "/work".to_string(),
            focused_role_pane_id: Some("p1".to_string()),
            start_role_index: 0,
            orchestrator_prompt: None,
            config: orch_config("orch"),
            status: OrchestrationStatus::WaitingForOrchestrator,
            had_waiting_pane: false,
            all_clear_pending: false,
            split_narrow: false,
            zoomed: false,
        };
        assert_eq!(
            crate::ui::sync_and_derive_selection(&mut dup_tab, None, dup, Some(1)),
            Some(1),
            "the highlight must hold on the second card of a same-pane group, not snap to the first"
        );
        // With no current highlight, the first member is still the right answer.
        assert_eq!(
            crate::ui::sync_and_derive_selection(&mut dup_tab, None, dup, None),
            Some(0)
        );
        // An out-of-range or mismatched current index must not be trusted.
        assert_eq!(
            crate::ui::sync_and_derive_selection(&mut dup_tab, None, dup, Some(9)),
            Some(0)
        );

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
            .open_orchestration_tab(&orch_config("bg-orch"), "/work", None, None, (24, 80))
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
            .open_orchestration_tab(&orch_config("orch"), "/work", None, None, (24, 80))
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

    /// Four roles (`orchestrator` start=true, `alpha`, `beta`, `gamma`) in
    /// spawn order, for the locked-half ordering test, which needs THREE
    /// non-orchestrator roles waiting at once to observe focus advancing
    /// through them in ascending order rather than merely picking one.
    fn orch_config_4(name: &str) -> OrchestrationConfig {
        OrchestrationConfig {
            default: false,
            name: name.to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    agent: None,
                    name: "orchestrator".to_string(),
                    command: "echo orch".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    agent: None,
                    name: "alpha".to_string(),
                    command: "echo alpha".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    agent: None,
                    name: "beta".to_string(),
                    command: "echo beta".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    agent: None,
                    name: "gamma".to_string(),
                    command: "echo gamma".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
            ],
        }
    }

    /// Three roles (`orchestrator` start=true, `alpha`, `beta`) in spawn
    /// order, for auto-focus tests that need to distinguish lowest- vs
    /// higher-order waiting panes among more than two roles.
    fn orch_config_3(name: &str) -> OrchestrationConfig {
        OrchestrationConfig {
            default: false,
            name: name.to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    agent: None,
                    name: "orchestrator".to_string(),
                    command: "echo orch".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    agent: None,
                    name: "alpha".to_string(),
                    command: "echo alpha".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    agent: None,
                    name: "beta".to_string(),
                    command: "echo beta".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
            ],
        }
    }

    /// Scenario: Within a single active orchestration tab (roles
    /// `orchestrator` < `alpha` < `beta` in spawn order), drive a synthetic
    /// `SessionStatus` map through `TabManager::auto_focus_waiting_pane` to pin
    /// the exact resolution rule: no waiting panes leaves manual focus alone, a
    /// newly-waiting pane steals focus, ties resolve to the lowest-order waiting
    /// pane (even stealing focus mid-input from a higher-order pane that is
    /// itself still waiting), an already-lowest focused pane is a no-op, and
    /// resolving the focused pane advances to the next-lowest still-waiting
    /// pane. A second orchestration tab then proves a background tab's
    /// newly-waiting pane has zero effect and never flips the active tab.
    #[spec("orchestration/focus/001")]
    #[test]
    fn focus_001_auto_focus_follows_lowest_order_waiting_pane() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (orch_idx, role_ids) = tm
            .open_orchestration_tab(&orch_config_3("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");
        assert_eq!(tm.active_index(), orch_idx);
        let orchestrator = role_ids[0].clone();
        let alpha = role_ids[1].clone();
        let beta = role_ids[2].clone();

        // No pane is WaitingForInput: the resolver is a no-op and
        // `focused_role_pane_id` starts (and stays) at its default `None`.
        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        status.insert(alpha.as_str(), SessionStatus::Working);
        status.insert(beta.as_str(), SessionStatus::Idle);
        assert_eq!(tm.auto_focus_waiting_pane(&status), None);
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration {
                focused_role_pane_id: None,
                ..
            }
        ));

        // Simulate the user manually focusing a pane. With still nothing
        // waiting, the manual choice must be left exactly as set.
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = &mut tm.tabs[orch_idx]
        {
            *focused_role_pane_id = Some(orchestrator.clone());
        }
        assert_eq!(tm.auto_focus_waiting_pane(&status), None);
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == orchestrator
        ));

        // `beta` (non-focused, highest order) transitions to WaitingForInput:
        // it steals focus from the manually-set `orchestrator`.
        status.insert(beta.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            tm.auto_focus_waiting_pane(&status).as_deref(),
            Some(beta.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
        ));

        // `alpha` also becomes WaitingForInput: with two panes concurrently
        // waiting, focus resolves to the LOWER-order one (`alpha`), not the
        // currently-focused `beta`.
        status.insert(alpha.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            tm.auto_focus_waiting_pane(&status).as_deref(),
            Some(alpha.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
        ));

        // `alpha` is already the lowest-order waiting pane and already focused:
        // a repeat call must be a no-op (no flicker).
        assert_eq!(tm.auto_focus_waiting_pane(&status), None);
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
        ));

        // `orchestrator` (lower order than the currently-focused `alpha`) newly
        // transitions to WaitingForInput while `alpha` is itself still waiting:
        // focus jumps to `orchestrator` anyway — the deliberate "steal focus
        // mid-input" tradeoff.
        status.insert(orchestrator.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            tm.auto_focus_waiting_pane(&status).as_deref(),
            Some(orchestrator.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == orchestrator
        ));

        // Resolving the focused pane (`orchestrator` -> Idle) advances focus to
        // the next-lowest STILL-waiting pane (`alpha`), even though the
        // higher-order `beta` is also still waiting.
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        assert_eq!(
            tm.auto_focus_waiting_pane(&status).as_deref(),
            Some(alpha.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
        ));

        // Open a second orchestration tab, which becomes active and leaves the
        // first as a BACKGROUND tab.
        let (orch2_idx, _role_ids2) = tm
            .open_orchestration_tab(&orch_config_3("orch-2"), "/work", None, None, (24, 80))
            .expect("open second orchestration tab");
        assert_eq!(tm.active_index(), orch2_idx);

        // `orchestrator` newly transitions to WaitingForInput again on the
        // now-BACKGROUND first tab — the lowest-order role, which would steal
        // focus if that tab were active.
        status.insert(orchestrator.as_str(), SessionStatus::WaitingForInput);
        let result = tm.auto_focus_waiting_pane(&status);

        // The active tab (`orch-2`) has none of its own panes represented in
        // `status`, so there is nothing to focus there.
        assert_eq!(result, None);
        // Auto-focus never changes which TAB is active.
        assert_eq!(
            tm.active_index(),
            orch2_idx,
            "auto-focus must never switch the active tab"
        );
        // The background tab's stored focus is untouched — still `alpha`.
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
        ));
    }

    /// Scenario: Within a single active orchestration tab (roles
    /// `orchestrator` < `alpha` < `beta`), drive synthetic `SessionStatus` maps
    /// through BOTH `auto_focus_waiting_pane` and `auto_focus_all_clear` per
    /// frame, gated exactly the way the real `src/ui.rs` render-loop site gates
    /// them (all-clear only runs when waiting-pane found nothing to steer
    /// toward). Proves the full coexistence story end to end: a manual focus is
    /// left alone while nothing is waiting; a newly-waiting pane steals focus;
    /// once it resolves, the all-clear move snaps focus back to the orchestrator
    /// role exactly once — not on every subsequent frame, and not again for a
    /// manual focus change until a NEW pane starts and resolves waiting. A
    /// second (background) orchestration tab proves the all-clear move, like its
    /// sibling, never touches an inactive tab or switches which tab is active.
    #[spec("orchestration/focus/002")]
    #[test]
    fn focus_002_all_clear_focus_move_is_edge_triggered() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (orch_idx, role_ids) = tm
            .open_orchestration_tab(&orch_config_3("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");
        assert_eq!(tm.active_index(), orch_idx);
        let orchestrator = role_ids[0].clone();
        let alpha = role_ids[1].clone();
        let beta = role_ids[2].clone();

        // Mirrors the real per-frame call site in `src/ui.rs`: the
        // waiting-history observation runs first, then `auto_focus_all_clear`
        // only runs when `auto_focus_waiting_pane` found nothing to steer
        // toward.
        fn frame(tm: &mut TabManager, status: &HashMap<&str, SessionStatus>) -> Option<String> {
            tm.observe_waiting_panes(status);
            tm.auto_focus_waiting_pane(status)
                .or_else(|| tm.auto_focus_all_clear())
        }

        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        status.insert(alpha.as_str(), SessionStatus::Idle);
        status.insert(beta.as_str(), SessionStatus::Idle);

        // Nothing waiting, nothing was waiting before: no move at all.
        assert_eq!(frame(&mut tm, &status), None);

        // Manual focus (simulating the user navigating) must be left alone
        // while there's no waiting history to edge-trigger off of.
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = &mut tm.tabs[orch_idx]
        {
            *focused_role_pane_id = Some(alpha.clone());
        }
        assert_eq!(frame(&mut tm, &status), None);
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
        ));

        // `beta` becomes WaitingForInput: `auto_focus_waiting_pane` steals focus
        // to it exactly as `focus_001` pins.
        status.insert(beta.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(frame(&mut tm, &status).as_deref(), Some(beta.as_str()));

        // Next frame: `beta` still waiting and already focused, so
        // `auto_focus_waiting_pane` no-ops and `auto_focus_all_clear` runs (per
        // the gate) — it must not move focus while something is still waiting.
        assert_eq!(frame(&mut tm, &status), None);
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
        ));

        // `beta` resolves: the all-clear edge fires — focus snaps to the
        // orchestrator role exactly once.
        status.insert(beta.as_str(), SessionStatus::Idle);
        assert_eq!(
            frame(&mut tm, &status).as_deref(),
            Some(orchestrator.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == orchestrator
        ));

        // Repeated frames with nothing waiting and already on the orchestrator
        // must NOT fire again — this is the edge- vs level-triggered
        // distinction.
        assert_eq!(frame(&mut tm, &status), None);
        assert_eq!(frame(&mut tm, &status), None);
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == orchestrator
        ));

        // The human manually looks at another pane after the all-clear already
        // fired for `beta`'s waiting episode. Nothing is waiting and nothing new
        // has started waiting since the last fire, so the all-clear must leave
        // this manual choice alone — it must not snap back on every frame.
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = &mut tm.tabs[orch_idx]
        {
            *focused_role_pane_id = Some(alpha.clone());
        }
        assert_eq!(frame(&mut tm, &status), None);
        assert!(
            matches!(
                &tm.tabs[orch_idx],
                Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
            ),
            "edge-triggered: must not repeatedly snap back once already fired for this episode"
        );

        // A NEW waiting episode (`alpha`, already focused, starts waiting) arms
        // the edge again; its resolution fires the all-clear a second time,
        // proving this isn't a one-shot-forever flag.
        status.insert(alpha.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(frame(&mut tm, &status), None);
        status.insert(alpha.as_str(), SessionStatus::Idle);
        assert_eq!(
            frame(&mut tm, &status).as_deref(),
            Some(orchestrator.as_str())
        );

        // Open a second orchestration tab, which becomes active and leaves the
        // first as a BACKGROUND tab.
        let (orch2_idx, _role_ids2) = tm
            .open_orchestration_tab(&orch_config_3("orch-2"), "/work", None, None, (24, 80))
            .expect("open second orchestration tab");
        assert_eq!(tm.active_index(), orch2_idx);

        // Drive the first (now background) tab's roles through a full
        // waiting-then-resolved episode. Both methods only ever touch
        // `self.tabs[self.active_index]`, so this must have zero effect on the
        // background tab and must never switch which tab is active.
        status.insert(alpha.as_str(), SessionStatus::WaitingForInput);
        let result = frame(&mut tm, &status);
        assert_eq!(result, None);
        status.insert(alpha.as_str(), SessionStatus::Idle);
        let result = frame(&mut tm, &status);
        assert_eq!(result, None);
        assert_eq!(
            tm.active_index(),
            orch2_idx,
            "the all-clear move must never switch the active tab"
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == orchestrator
        ));
    }

    /// Scenario: The shortest possible waiting episode — a role goes
    /// `WaitingForInput` on one frame and is resolved by the next, with no
    /// intervening frame in which it was both still waiting and already focused.
    /// Drives the real per-frame sequence (`observe_waiting_panes`, then
    /// `auto_focus_waiting_pane` → `auto_focus_all_clear`): the first frame
    /// steers focus onto the waiting role and the second must still fire the
    /// all-clear move back to the orchestrator. This is why the observation must
    /// live OUTSIDE the chain — `focus_002` always has a still-waiting frame in
    /// between, which is exactly what lets a dropped edge hide.
    #[spec("orchestration/focus/003")]
    #[test]
    fn focus_003_all_clear_survives_a_single_frame_waiting_episode() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (orch_idx, role_ids) = tm
            .open_orchestration_tab(&orch_config_3("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");
        assert_eq!(tm.active_index(), orch_idx);
        let orchestrator = role_ids[0].clone();
        let alpha = role_ids[1].clone();
        let beta = role_ids[2].clone();

        // Mirrors the real per-frame call site in `src/ui.rs`: the observation
        // runs first, outside the chain, then the chain.
        fn frame(tm: &mut TabManager, status: &HashMap<&str, SessionStatus>) -> Option<String> {
            tm.observe_waiting_panes(status);
            tm.auto_focus_waiting_pane(status)
                .or_else(|| tm.auto_focus_all_clear())
        }

        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        status.insert(alpha.as_str(), SessionStatus::Idle);
        status.insert(beta.as_str(), SessionStatus::Idle);

        // The human is looking at `alpha`, and nothing has ever waited on this
        // tab yet, so a quiet frame moves nothing.
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = &mut tm.tabs[orch_idx]
        {
            *focused_role_pane_id = Some(alpha.clone());
        }
        assert_eq!(frame(&mut tm, &status), None);

        // Frame 1: `beta` starts waiting. `auto_focus_waiting_pane` steers focus
        // onto it and therefore WINS the chain, so `auto_focus_all_clear` never
        // runs on this frame — the only frame in which `beta` is observed
        // waiting.
        status.insert(beta.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(frame(&mut tm, &status).as_deref(), Some(beta.as_str()));

        // Frame 2: `beta` resolves. The all-clear move must still fire — the
        // waiting history has to be recorded by the observation outside the
        // chain, not by whichever branch of the chain happened to run. Recording
        // it inside `auto_focus_all_clear` loses this edge entirely and leaves
        // focus stranded on the resolved `beta`.
        status.insert(beta.as_str(), SessionStatus::Idle);
        assert_eq!(
            frame(&mut tm, &status).as_deref(),
            Some(orchestrator.as_str()),
            "a waiting episode observed in a SINGLE frame must still \
             edge-trigger the all-clear focus move — that frame is exactly \
             the one `auto_focus_waiting_pane` consumes"
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == orchestrator
        ));

        // Still edge-triggered: the next quiet frame must not fire again.
        assert_eq!(frame(&mut tm, &status), None);
    }

    /// Scenario: The LOCKED half of "focus follows the lock". Four roles
    /// (`orchestrator` < `alpha` < `beta` < `gamma`); all three non-orchestrator
    /// roles go `WaitingForInput` together, and the real per-frame sequence must
    /// steer focus to them in ascending `role_pane_ids` order, advancing to the
    /// next-lowest still-waiting role each time the currently-focused one
    /// resolves, and finally return to the orchestrator on the all-clear edge
    /// once all three have resolved. Pins the ordering promise explicitly so a
    /// later change to the chain has to stay compatible with it.
    #[spec("orchestration/focus/004")]
    #[test]
    fn focus_004_locked_focus_visits_all_waiting_roles_in_order() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (orch_idx, role_ids) = tm
            .open_orchestration_tab(&orch_config_4("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");
        assert_eq!(tm.active_index(), orch_idx);
        let orchestrator = role_ids[0].clone();
        let alpha = role_ids[1].clone();
        let beta = role_ids[2].clone();
        let gamma = role_ids[3].clone();

        // Mirrors the real per-frame call site in `src/ui.rs`.
        fn frame(tm: &mut TabManager, status: &HashMap<&str, SessionStatus>) -> Option<String> {
            tm.observe_waiting_panes(status);
            tm.auto_focus_waiting_pane(status)
                .or_else(|| tm.auto_focus_all_clear())
        }

        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        status.insert(alpha.as_str(), SessionStatus::Idle);
        status.insert(beta.as_str(), SessionStatus::Idle);
        status.insert(gamma.as_str(), SessionStatus::Idle);

        // Nothing waiting yet: no move.
        assert_eq!(frame(&mut tm, &status), None);

        // All three non-orchestrator roles go WaitingForInput together. Focus
        // must land on the LOWEST-order one first (`alpha`).
        status.insert(alpha.as_str(), SessionStatus::WaitingForInput);
        status.insert(beta.as_str(), SessionStatus::WaitingForInput);
        status.insert(gamma.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(frame(&mut tm, &status).as_deref(), Some(alpha.as_str()));
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
        ));

        // `alpha` resolves; `beta` and `gamma` are still waiting — focus
        // advances to the next-lowest still-waiting role (`beta`).
        status.insert(alpha.as_str(), SessionStatus::Idle);
        assert_eq!(frame(&mut tm, &status).as_deref(), Some(beta.as_str()));
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
        ));

        // `beta` resolves; `gamma` is still waiting — focus advances to it.
        status.insert(beta.as_str(), SessionStatus::Idle);
        assert_eq!(frame(&mut tm, &status).as_deref(), Some(gamma.as_str()));
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == gamma
        ));

        // `gamma` resolves — nothing left waiting: the all-clear edge fires and
        // returns focus to the orchestrator role.
        status.insert(gamma.as_str(), SessionStatus::Idle);
        assert_eq!(
            frame(&mut tm, &status).as_deref(),
            Some(orchestrator.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == orchestrator
        ));

        // Steady state: a further quiet frame does not move focus again.
        assert_eq!(frame(&mut tm, &status), None);
    }

    /// Scenario: The UNLOCKED half — while the deck is unlocked, no auto-focus
    /// branch may fire at all: a waiting pane already in flight must not steal
    /// focus, and its later resolution must not fire an all-clear move either,
    /// so a manual focus choice survives the whole stretch untouched. Re-locking
    /// must then (a) NOT fire a stale all-clear move for the episode the human
    /// already handled while unlocked — THE STALE-LATCH ASSERTION below, marked
    /// explicitly — and (b) resume normal waiting-pane steering / all-clear
    /// pinning for a fresh episode. The local `frame` helper's `locked` flag
    /// models the real per-frame call site in `src/ui.rs`, which gates the whole
    /// chain — `observe_waiting_panes` included — on `ui.command_entry_locked`.
    /// It also exercises `TabManager::clear_waiting_pane_latch`, the setter the
    /// locked→unlocked toggle handler must call.
    ///
    /// The episode deliberately STRADDLES the transition (waiting starts while
    /// locked → unlock mid-episode → resolves while unlocked and unobserved →
    /// re-lock), because that is the only shape in which the bug is reachable:
    /// an episode that both begins and ends inside the unlocked stretch never
    /// touches the latch at all, so a test written against that simpler wording
    /// would pass with no fix and prove nothing. Manual focus is parked on
    /// `beta` (the pane that already stole it during the locked stretch), not
    /// `alpha`: the final re-locked episode moves focus onto `alpha`, and
    /// parking on `alpha` instead would make that a same-pane no-op under
    /// `auto_focus_waiting_pane`'s no-flicker early return rather than an
    /// observable move.
    #[spec("orchestration/focus/005")]
    #[test]
    fn focus_005_unlock_suspends_auto_focus_and_clears_stale_latch() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (orch_idx, role_ids) = tm
            .open_orchestration_tab(&orch_config_3("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");
        assert_eq!(tm.active_index(), orch_idx);
        let orchestrator = role_ids[0].clone();
        let alpha = role_ids[1].clone();
        let beta = role_ids[2].clone();

        // Mirrors the real per-frame call site in `src/ui.rs`: while unlocked,
        // nothing below runs at all — not even `observe_waiting_panes`.
        fn frame(
            tm: &mut TabManager,
            status: &HashMap<&str, SessionStatus>,
            locked: bool,
        ) -> Option<String> {
            if !locked {
                return None;
            }
            tm.observe_waiting_panes(status);
            tm.auto_focus_waiting_pane(status)
                .or_else(|| tm.auto_focus_all_clear())
        }

        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        status.insert(alpha.as_str(), SessionStatus::Idle);
        status.insert(beta.as_str(), SessionStatus::Idle);

        // Locked start: `beta` goes WaitingForInput and steals focus, exactly as
        // `focus_001` pins.
        status.insert(beta.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            frame(&mut tm, &status, true).as_deref(),
            Some(beta.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
        ));

        // Unlock MID-EPISODE — `beta` is still WaitingForInput. The
        // locked→unlocked transition must clear the edge latch
        // (`had_waiting_pane` / `all_clear_pending`) right here, so a stretch
        // spent unlocked cannot later be misread as a fresh episode. This is the
        // setter the toggle handler must call at this exact point.
        tm.clear_waiting_pane_latch();

        // The human takes manual control of a non-orchestrator role while
        // unlocked — re-affirming `beta`, which already has focus from the steal
        // above.
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = &mut tm.tabs[orch_idx]
        {
            *focused_role_pane_id = Some(beta.clone());
        }

        // While unlocked, `beta` (still nominally "waiting" per `status`) does
        // not steal focus back.
        assert_eq!(
            frame(&mut tm, &status, false),
            None,
            "unlocked: a waiting pane already in flight must not steal focus"
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
        ));

        // `beta` resolves while unlocked — e.g. the human answered it directly,
        // unmediated by the lock. No all-clear move fires either: the chain does
        // not run at all while unlocked.
        status.insert(beta.as_str(), SessionStatus::Idle);
        assert_eq!(
            frame(&mut tm, &status, false),
            None,
            "unlocked: resolving a waiting pane must not fire an all-clear move either"
        );
        assert!(
            matches!(
                &tm.tabs[orch_idx],
                Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
            ),
            "manual focus on beta must survive the entire unlocked stretch"
        );

        // *** THE STALE-LATCH ASSERTION ***
        // Re-lock. Without `clear_waiting_pane_latch` having actually cleared
        // the latch above, `observe_waiting_panes` would compare its OLD
        // `had_waiting_pane == true` (frozen from before the unlock, when `beta`
        // was still waiting) against the CURRENT idle status and misread that as
        // a fresh true->false edge — firing a spurious all-clear move that yanks
        // focus off `beta`, the pane the human deliberately left it on. With the
        // latch cleared, this must be a no-op.
        assert_eq!(
            frame(&mut tm, &status, true),
            None,
            "re-locking must NOT fire a stale all-clear move for an episode \
             the human already dealt with while unlocked"
        );
        assert!(
            matches!(
                &tm.tabs[orch_idx],
                Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
            ),
            "focus must still be exactly where the human left it after re-lock"
        );

        // Re-locking resumes normal pinning: a NEW waiting episode steers focus
        // and its resolution snaps focus back to the orchestrator, exactly as
        // `focus_001`/`focus_002` pin for the always-locked case.
        status.insert(alpha.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            frame(&mut tm, &status, true).as_deref(),
            Some(alpha.as_str()),
            "re-locking must resume waiting-pane steering"
        );
        status.insert(alpha.as_str(), SessionStatus::Idle);
        assert_eq!(
            frame(&mut tm, &status, true).as_deref(),
            Some(orchestrator.as_str()),
            "re-locking must resume all-clear pinning back to the orchestrator"
        );
    }

    /// Scenario: The lock is deck-global, so the latch clearing must be too.
    /// Two Orchestration tabs (`A`, `B`): `A`'s `alpha` role goes
    /// `WaitingForInput` while `A` is active and locked, latching `A`'s
    /// `had_waiting_pane = true` and stealing focus onto `alpha`. The user then
    /// switches to `B` and unlocks — the deck-global toggle's latch-clearing
    /// call fires with `B`, not `A`, active. While unlocked, `A`'s worker
    /// resolves unobserved (the chain never runs against a background tab, and
    /// wouldn't run at all while unlocked even if `A` were active). The user
    /// re-locks and returns to `A`. `A`'s first locked frame back must treat the
    /// already-resolved `alpha` as old news, not a fresh `true` → `false` edge,
    /// so focus stays on `alpha` rather than being yanked to the orchestrator
    /// role. This is `focus_005`'s bug reappearing across tabs whenever the
    /// clearing call is scoped to the active tab instead of the deck-global lock
    /// it compensates for. Pins the outcome — every Orchestration tab's edge
    /// state is reset on the locked→unlocked transition — not the mechanism.
    #[spec("orchestration/focus/006")]
    #[test]
    fn focus_006_lock_toggle_clears_latch_on_every_tab_not_just_active() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());

        let (idx_a, roles_a) = tm
            .open_orchestration_tab(&orch_config_3("orchA"), "/work", None, None, (24, 80))
            .expect("open orchestration tab A");
        let orchestrator_a = roles_a[0].clone();
        let alpha_a = roles_a[1].clone();

        let (idx_b, _roles_b) = tm
            .open_orchestration_tab(&orch_config_3("orchB"), "/work", None, None, (24, 80))
            .expect("open orchestration tab B");
        assert_eq!(tm.active_index(), idx_b);

        // Mirrors the real per-frame call site: the whole chain is skipped while
        // unlocked, exactly as `focus_005` models.
        fn frame(
            tm: &mut TabManager,
            status: &HashMap<&str, SessionStatus>,
            locked: bool,
        ) -> Option<String> {
            if !locked {
                return None;
            }
            tm.observe_waiting_panes(status);
            tm.auto_focus_waiting_pane(status)
                .or_else(|| tm.auto_focus_all_clear())
        }

        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator_a.as_str(), SessionStatus::Idle);
        status.insert(alpha_a.as_str(), SessionStatus::Idle);

        // 1. Tab A is active and locked; `alpha` goes WaitingForInput, latching
        // `had_waiting_pane = true` on A and stealing focus, as `focus_001`
        // pins.
        assert!(tm.switch_to(idx_a));
        status.insert(alpha_a.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            frame(&mut tm, &status, true).as_deref(),
            Some(alpha_a.as_str())
        );
        assert!(matches!(
            &tm.tabs[idx_a],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha_a
        ));

        // 2. User switches to tab B and unlocks. The deck-global lock toggle's
        // latch-clearing call fires here — against whichever tab is active at
        // the moment, which is now B, not A.
        assert!(tm.switch_to(idx_b));
        tm.clear_waiting_pane_latch();

        // 3. While unlocked, A's worker resolves — unobserved, since the chain
        // doesn't run for a background tab, and wouldn't run at all while
        // unlocked even if A were active.
        status.insert(alpha_a.as_str(), SessionStatus::Idle);

        // 4. User re-locks and returns to A.
        assert!(tm.switch_to(idx_a));

        // 5. A's first locked frame back: if A's latch was actually cleared in
        // step 2 (deck-global, not active-tab-only), this is a no-op — nothing
        // new resolved from A's perspective, so there is no edge to fire. If the
        // latch survived (a `clear_waiting_pane_latch` that only touched the
        // then-active tab B), this reads as a stale `true` -> `false` edge and
        // yanks focus to the orchestrator, overriding where the user left it.
        assert_eq!(
            frame(&mut tm, &status, true),
            None,
            "the locked->unlocked toggle must clear EVERY orchestration \
             tab's waiting-episode latch, not just the tab active at the \
             moment of the toggle — a background tab's already-resolved \
             episode must not be replayed as a fresh all-clear edge on \
             return"
        );
        assert!(
            matches!(
                &tm.tabs[idx_a],
                Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha_a
            ),
            "focus must stay on alpha, not be yanked to the orchestrator by \
             a stale latch surviving on a background tab"
        );
    }

    /// Scenario: Turning the `experimental` flag OFF mid-session must clear the
    /// waiting-episode latch, exactly as the `Ctrl+E` unlock does. The flag is
    /// live-reloaded from `.dot-agent-deck.toml`, so it is a SECOND way to stop
    /// observing — and the latch is edge-triggered, so freezing it while a pane
    /// is waiting would replay a stale all-clear the next time the flag goes
    /// on. Here a worker latches while the flag is on, the flag goes off, the
    /// worker resolves unobserved, and the first frame back on must steal no
    /// focus.
    #[spec("orchestration/focus/009")]
    #[test]
    fn focus_009_flag_off_clears_latch_so_re_enabling_replays_no_stale_edge() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (_idx, roles) = tm
            .open_orchestration_tab(&orch_config_3("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");
        let orchestrator = roles[0].clone();
        let alpha = roles[1].clone();

        // Mirrors the real per-frame call site AFTER the experimental gate: the
        // flag's `false` arm CLEARS the latch rather than merely skipping
        // observation, which is the whole point of this test.
        fn frame(
            tm: &mut TabManager,
            status: &HashMap<&str, SessionStatus>,
            flag_on: bool,
            locked: bool,
        ) -> Option<String> {
            if !flag_on {
                tm.clear_waiting_pane_latch();
                return None;
            }
            if !locked {
                return None;
            }
            tm.observe_waiting_panes(status);
            tm.auto_focus_waiting_pane(status)
                .or_else(|| tm.auto_focus_all_clear())
        }

        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        status.insert(alpha.as_str(), SessionStatus::Idle);

        // 1. Flag on and locked: `alpha` waits, latching the episode and
        // stealing focus (`orchestration/focus/001`).
        status.insert(alpha.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            frame(&mut tm, &status, true, true).as_deref(),
            Some(alpha.as_str())
        );

        // 2. The user edits `.dot-agent-deck.toml`; the watcher picks the flag
        // up as OFF. One frame passes in that state.
        assert_eq!(frame(&mut tm, &status, false, true), None);

        // 3. While the surface is off, `alpha` resolves — unobserved.
        status.insert(alpha.as_str(), SessionStatus::Idle);

        // 4. The flag goes back on. Nothing new has happened from the deck's
        // perspective, so there is no edge to fire. A latch left standing in
        // step 2 would read here as a stale `true` -> `false` all-clear and
        // yank focus to the orchestrator for an episode already dealt with.
        assert_eq!(
            frame(&mut tm, &status, true, true),
            None,
            "turning the experimental flag off must CLEAR the waiting-episode \
             latch, not freeze it — otherwise re-enabling the flag replays an \
             already-resolved episode as a fresh all-clear edge and steals \
             focus to the orchestrator"
        );
    }

    /// Scenario: The waiting-focus branch must not steal focus while a
    /// keystroke is still queued for the pane that currently has it. Within a
    /// locked orchestration tab (roles `orchestrator` < `alpha` < `beta`),
    /// `beta` (higher role order) is already focused and `WaitingForInput` —
    /// the human is mid-answer, so a keystroke is still queued for it. `alpha`
    /// (LOWER role order) then also goes `WaitingForInput`, which would
    /// normally steal focus per `orchestration/focus/001`'s lowest-order rule.
    /// On the frame where `input_pending` is true, that steal must be DEFERRED,
    /// not applied, so the queued keystroke still lands on `beta` rather than
    /// being misrouted to `alpha` — where, because `alpha` is itself
    /// `WaitingForInput`, the lock's carve-out would let it straight through to
    /// answer a prompt the user never saw. Once `input_pending` clears on a
    /// later frame, the deferred steer to `alpha` must still fire, proving the
    /// guard defers the move rather than dropping it, mirroring
    /// `TabManager::auto_focus_all_clear`'s existing "no one-shot latch"
    /// contract. Drives `TabManager::auto_focus_locked`, which folds both
    /// `auto_focus_waiting_pane` and `auto_focus_all_clear` behind ONE
    /// `input_pending` guard shared by both branches, mirroring the real
    /// per-frame call site's shape.
    #[spec("orchestration/focus/008")]
    #[test]
    fn focus_008_waiting_focus_defers_while_input_pending() {
        use crate::state::SessionStatus;

        let pc = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let (orch_idx, role_ids) = tm
            .open_orchestration_tab(&orch_config_3("orch"), "/work", None, None, (24, 80))
            .expect("open orchestration tab");
        assert_eq!(tm.active_index(), orch_idx);
        let orchestrator = role_ids[0].clone();
        let alpha = role_ids[1].clone();
        let beta = role_ids[2].clone();

        // Mirrors the real per-frame call site in `src/ui.rs`: the observation
        // runs unconditionally and outside the chain, and both branches are
        // gated on the SAME `input_pending` guard, computed once per frame in
        // production from `crossterm::event::poll(Duration::from_millis(0))`.
        fn frame(
            tm: &mut TabManager,
            status: &HashMap<&str, SessionStatus>,
            input_pending: bool,
        ) -> Option<String> {
            tm.observe_waiting_panes(status);
            tm.auto_focus_locked(status, input_pending)
        }

        let mut status: HashMap<&str, SessionStatus> = HashMap::new();
        status.insert(orchestrator.as_str(), SessionStatus::Idle);
        status.insert(alpha.as_str(), SessionStatus::Idle);
        status.insert(beta.as_str(), SessionStatus::Idle);

        // `beta` (higher role order) goes WaitingForInput and steals focus — no
        // input pending yet, so the move applies immediately, exactly as
        // `orchestration/focus/001` pins.
        status.insert(beta.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            frame(&mut tm, &status, false).as_deref(),
            Some(beta.as_str())
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
        ));

        // The human is mid-answer to `beta` — a keystroke is queued. On THIS
        // frame, `alpha` (LOWER role order than `beta`) ALSO goes
        // WaitingForInput, which would normally steal focus per
        // `orchestration/focus/001`'s lowest-order rule. Because input is
        // pending, the steal must be DEFERRED: focus stays on `beta` so the
        // queued keystroke is not misrouted to `alpha`.
        status.insert(alpha.as_str(), SessionStatus::WaitingForInput);
        assert_eq!(
            frame(&mut tm, &status, true),
            None,
            "a waiting-focus steal must be deferred, not applied, while a \
             keystroke is still queued for the currently-focused waiting pane"
        );
        assert!(
            matches!(
                &tm.tabs[orch_idx],
                Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == beta
            ),
            "focus must still be on beta — the queued keystroke's target — \
             while input is pending, not yanked to alpha"
        );

        // The queued keystroke has now been drained (input no longer pending).
        // `alpha` is still WaitingForInput and still lower-order than `beta`,
        // so the deferred steer must fire NOW — proving the guard defers the
        // move rather than dropping it, exactly as `auto_focus_all_clear`'s
        // existing pending-input guard already behaves for its own branch.
        assert_eq!(
            frame(&mut tm, &status, false).as_deref(),
            Some(alpha.as_str()),
            "the deferred steer to alpha must still fire once input is no \
             longer pending — deferred, not lost"
        );
        assert!(matches!(
            &tm.tabs[orch_idx],
            Tab::Orchestration { focused_role_pane_id: Some(p), .. } if *p == alpha
        ));
    }
}
