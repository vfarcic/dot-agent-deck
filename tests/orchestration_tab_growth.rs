//! Issue #868 tests for the `TabManager`-level half of "grow an already-open
//! orchestration tab" (`TabManager::add_role_to_existing_orchestration` /
//! `TabManager::orchestration_tab_index_for`), driven directly against a
//! `TabManager` with a trivial no-op `PaneController` — no daemon, no PTY,
//! no `ui.rs`/`EmbeddedPaneController` involved. This mirrors
//! `tests/pane_close.rs`'s own `DelayedCloseController` technique for
//! testing `TabManager` in isolation.
//!
//! - `pane/spawn/009`: growing a tab that already carries a DEAD SLOT for
//!   the role being spawned replaces that slot instead of appending a
//!   second entry, including the `was_new == false` half of the returned
//!   tuple and that the replaced slot's config reflects the freshly-spawned
//!   role, not the stale one.
//! - `pane/spawn/010`: `orchestration_tab_index_for` takes a per-tab
//!   `orchestration_id: Option<&str>` (added on
//!   `open_orchestration_tab_with_existing_role_panes` and
//!   `orchestration_tab_index_for` alike) so two orchestration tabs sharing
//!   the same `(cwd, name)` — told apart only by their PRD #140 `Instance`
//!   orchestration id — do not cross-wire on growth.

#![cfg(unix)]

use std::sync::Arc;

use dot_agent_deck::pane::{PaneController, PaneDirection, PaneError, PaneInfo, RenameOutcome};
use dot_agent_deck::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
use dot_agent_deck::tab::{OrchestrationRoleStatus, Tab, TabManager};
use spec::spec;

/// A `PaneController` that does nothing and never fails — `TabManager`'s own
/// tab-shape bookkeeping (the surface under test here) never calls into the
/// controller for the operations these tests exercise, so every method is
/// an unconditional success stub.
#[derive(Default)]
struct NoopPaneController;

impl PaneController for NoopPaneController {
    fn focus_pane(&self, _pane_id: &str) -> Result<(), PaneError> {
        Ok(())
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
        "noop"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn role(name: &str, start: bool) -> OrchestrationRoleConfig {
    OrchestrationRoleConfig {
        agent: None,
        name: name.to_string(),
        command: "cat".to_string(),
        start,
        description: None,
        prompt_template: None,
        clear: false,
    }
}

fn two_role_config(orchestration_name: &str) -> OrchestrationConfig {
    OrchestrationConfig {
        default: false,
        name: orchestration_name.to_string(),
        roles: vec![role("orchestrator", true), role("reviewer", false)],
    }
}

fn new_tab_manager() -> TabManager {
    TabManager::new(Arc::new(NoopPaneController))
}

/// Scenario: open an orchestration tab whose `reviewer` role has no live
/// pane (a dead slot — the shape produced when a role's pane closed while
/// the TUI was detached and gets reattached with no daemon registration to
/// back it). Grow that tab with a genuinely NEW `reviewer` pane carrying a
/// freshly-loaded role config (distinct `command` from the tab's stale
/// copy), exactly the call `pane spawn`'s daemon broadcast handler makes.
/// The tab must end up with exactly ONE `reviewer` role entry — the dead
/// slot's pane id replaced, its status flipped from `Failed` to `Working`,
/// its config overwritten with the FRESH role config (not the stale one the
/// tab originally opened with) — not a second `reviewer` entry appended
/// alongside the dead one, and the returned `was_new` flag must report
/// `false` since this is a replace, not an append.
#[spec("pane/spawn/009")]
#[test]
fn pane_spawn_009_growing_a_dead_slot_replaces_it_instead_of_appending_a_duplicate() {
    let mut tabs = new_tab_manager();
    let config = two_role_config("dead-slot-orch");

    let (tab_index, _) = tabs
        .open_orchestration_tab_with_existing_role_panes(
            &config,
            "/work/dead-slot-cwd",
            vec![Some("orch-pane".to_string()), None],
            None,
            None,
        )
        .expect("open a tab with reviewer as a dead slot");

    let fresh_reviewer_config = OrchestrationRoleConfig {
        command: "fresh-reviewer-cmd".to_string(),
        ..role("reviewer", false)
    };

    let (grown_role_index, was_new) = tabs
        .add_role_to_existing_orchestration(
            tab_index,
            fresh_reviewer_config.clone(),
            "fresh-reviewer-pane".to_string(),
        )
        .expect("grow the tab with a freshly-spawned reviewer");
    assert!(
        !was_new,
        "replacing a dead slot must report was_new == false"
    );

    let Tab::Orchestration {
        role_pane_ids,
        role_statuses,
        config: grown_config,
        ..
    } = &tabs.tabs()[tab_index]
    else {
        panic!("expected an Orchestration tab at index {tab_index}");
    };

    let reviewer_entries = grown_config
        .roles
        .iter()
        .filter(|r| r.name == "reviewer")
        .count();
    assert_eq!(
        reviewer_entries,
        1,
        "growing a tab that already has a dead slot for `reviewer` must REPLACE that slot, \
         not append a second `reviewer` role config entry; roles = {:?}",
        grown_config
            .roles
            .iter()
            .map(|r| &r.name)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        grown_config.roles.len(),
        2,
        "the role COUNT must stay at 2 (orchestrator + reviewer), not grow to 3; roles = {:?}",
        grown_config
            .roles
            .iter()
            .map(|r| &r.name)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        role_pane_ids.len(),
        2,
        "role_pane_ids must stay at 2 entries, not grow to 3; role_pane_ids = {role_pane_ids:?}"
    );
    assert_eq!(
        role_pane_ids[1], "fresh-reviewer-pane",
        "the dead slot's pane id must be replaced by the newly-spawned pane, not left as the \
         empty dead-slot sentinel with a second entry appended; role_pane_ids = {role_pane_ids:?}"
    );
    assert_eq!(
        role_statuses[1],
        OrchestrationRoleStatus::Working,
        "the replaced slot's status must flip from the dead-slot Failed marker to Working; \
         role_statuses = {role_statuses:?}"
    );
    assert_eq!(
        grown_role_index, 1,
        "the returned role_index must name the REPLACED slot's position (1), not a freshly \
         appended index (2)"
    );
    assert_eq!(
        grown_config.roles[grown_role_index].command, fresh_reviewer_config.command,
        "the replaced slot's role config must reflect the FRESH config passed to the growth \
         call, not the stale one the tab originally opened with; roles = {:?}",
        grown_config.roles
    );
}

/// Scenario: two orchestration tabs share the exact same orchestration
/// `name` and `cwd` (a `Ctrl+N`-opened second tab of the same config in the
/// same directory, or the batch producer `surface_spawned_orchestration`
/// firing into a directory that already has a tab open), told apart only by
/// their PRD #140 `Instance` orchestration id. `pane spawn`'s tab-growth
/// path must resolve the tab belonging to the CALLING orchestration
/// instance specifically — `orchestration_tab_index_for`'s bare `(cwd,
/// name)` match cannot tell them apart, so a role meant for instance B could
/// otherwise be grown into instance A's tab instead. Build both tabs,
/// resolve the lookup for instance B's id, grow whichever tab that resolves
/// to, then assert instance A's tab is completely untouched (role count,
/// pane ids) while instance B's tab is the one that grew. Calls
/// `open_orchestration_tab_with_existing_role_panes` and
/// `orchestration_tab_index_for` with the per-tab `orchestration_id:
/// Option<&str>` argument. The role grown onto instance B (`coder`) is
/// genuinely new to that tab, so the growth call's returned `was_new` flag
/// must also report `true` — the append-leg mirror of `pane_spawn_009`'s
/// replace-leg `was_new == false` pin.
#[spec("pane/spawn/010")]
#[test]
fn pane_spawn_010_two_same_name_cwd_instances_do_not_cross_wire_on_growth() {
    let mut tabs = new_tab_manager();
    let cwd = "/work/shared-cwd";
    let name = "cross-wire-orch";
    let config = two_role_config(name);

    let (tab_a, _) = tabs
        .open_orchestration_tab_with_existing_role_panes(
            &config,
            cwd,
            vec![
                Some("a-orchestrator".to_string()),
                Some("a-reviewer".to_string()),
            ],
            None,
            Some("cross-wire-instance-a"),
        )
        .expect("open instance A's tab");
    let (tab_b, _) = tabs
        .open_orchestration_tab_with_existing_role_panes(
            &config,
            cwd,
            vec![
                Some("b-orchestrator".to_string()),
                Some("b-reviewer".to_string()),
            ],
            None,
            Some("cross-wire-instance-b"),
        )
        .expect("open instance B's tab");
    assert_ne!(tab_a, tab_b, "the two instances must be two distinct tabs");

    // The resolution instance B's own `pane spawn` growth path performs:
    // find the tab belonging to ITS orchestration_id, at this shared (cwd, name).
    let resolved_for_b = tabs
        .orchestration_tab_index_for(cwd, name, Some("cross-wire-instance-b"))
        .expect(
            "orchestration_tab_index_for must resolve SOME tab for instance B's own id, not \
             nothing",
        );
    assert_eq!(
        resolved_for_b, tab_b,
        "orchestration_tab_index_for must resolve the tab whose orchestration_id matches the \
         CALLER's (instance B), not just whichever same-(cwd,name) tab happens to be first \
         (instance A); got tab {resolved_for_b}, A = {tab_a}, B = {tab_b}"
    );

    let (grown_role_index, was_new) = tabs
        .add_role_to_existing_orchestration(
            resolved_for_b,
            role("coder", false),
            "b-coder".to_string(),
        )
        .expect("grow instance B's tab with a new coder role");
    assert_eq!(grown_role_index, 2);
    assert!(was_new, "a genuinely new role must report was_new == true");

    let Tab::Orchestration {
        role_pane_ids: a_pane_ids,
        config: a_config,
        ..
    } = &tabs.tabs()[tab_a]
    else {
        panic!("expected instance A's tab to still be an Orchestration tab");
    };
    assert_eq!(
        a_pane_ids.len(),
        2,
        "instance A's tab must be COMPLETELY untouched by a role spawned into instance B; \
         role_pane_ids = {a_pane_ids:?}"
    );
    assert_eq!(
        a_config.roles.len(),
        2,
        "instance A's role config must be untouched; roles = {:?}",
        a_config.roles.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert!(
        !a_pane_ids.contains(&"b-coder".to_string()),
        "instance B's new pane must never appear on instance A's tab; role_pane_ids = {a_pane_ids:?}"
    );

    let Tab::Orchestration {
        role_pane_ids: b_pane_ids,
        config: b_config,
        ..
    } = &tabs.tabs()[tab_b]
    else {
        panic!("expected instance B's tab to still be an Orchestration tab");
    };
    assert_eq!(
        b_pane_ids.len(),
        3,
        "instance B's tab must be the one that grew; role_pane_ids = {b_pane_ids:?}"
    );
    assert!(
        b_pane_ids.contains(&"b-coder".to_string()),
        "instance B's tab must carry the newly-spawned coder pane; role_pane_ids = {b_pane_ids:?}"
    );
    assert_eq!(b_config.roles.len(), 3);

    // A caller carrying NO orchestration_id (a pre-PRD#140 legacy surface)
    // must not accidentally match either tokened tab — the fix's stated
    // fallback behavior for the token-less case.
    let resolved_for_legacy = tabs.orchestration_tab_index_for(cwd, name, None);
    assert!(
        resolved_for_legacy.is_none(),
        "a token-less lookup must not match either tokened instance's tab; got \
         {resolved_for_legacy:?} (A = {tab_a}, B = {tab_b})"
    );
}
