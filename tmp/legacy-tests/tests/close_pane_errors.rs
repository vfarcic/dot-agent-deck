//! PRD #92 F4 — `close_pane` error handling.
//!
//! Before F4, `TabManager::close_tab` returned `Result<Vec<String>, _>`
//! with every `close_pane` error silently dropped via `let _ =`, and
//! `ModeManager::deactivate_mode` did the same for its persistent +
//! reactive panes. The Ctrl+W UI handler then unconditionally removed
//! every dashboard card whose pane id appeared in the returned vector,
//! so a failed `StopAgent` RPC left the underlying agent alive in the
//! daemon registry while the user's card vanished from the dashboard.
//!
//! F4 widens both signatures to return [`CloseTabOutcome`] — `closed`
//! lists pane IDs whose `close_pane` returned `Ok(())`, `failed` carries
//! `(pane_id, rendered_error)` pairs for the rest. These tests exercise
//! the layer the UI handler now consumes: a stub `PaneController` that
//! returns `Err` for explicitly nominated pane IDs, attached to either
//! a `TabManager` (mode tab and orchestration tab) or a `ModeManager`,
//! then assert the resulting outcome split.
//!
//! The Ctrl+W dispatcher itself is verified by code-read (see
//! `src/ui.rs`'s Ctrl+w arm) — it builds a `HashSet<&str>` from
//! `outcome.closed` and only removes sessions whose `pane_id` is in
//! that set, leaving failed-close sessions present so the user can
//! retry.

use std::any::Any;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use dot_agent_deck::mode_manager::ModeManager;
use dot_agent_deck::pane::{PaneController, PaneDirection, PaneError, PaneInfo, RenameOutcome};
use dot_agent_deck::project_config::{
    ModeConfig, ModePersistentPane, ModeRule, OrchestrationConfig, OrchestrationRoleConfig,
};
use dot_agent_deck::tab::TabManager;

/// Stub `PaneController` whose `close_pane` returns `Err` for any pane
/// id in `should_fail`, and `Ok(())` otherwise. Records every closed id
/// (success or failure) so tests can assert call ordering.
struct FailingPaneController {
    next_id: Mutex<u64>,
    /// Pane IDs for which `close_pane` returns `Err` instead of `Ok`.
    should_fail: Mutex<HashSet<String>>,
    /// Every pane id ever passed to `close_pane`, in call order.
    close_log: Mutex<Vec<String>>,
}

impl FailingPaneController {
    fn new() -> Self {
        Self {
            next_id: Mutex::new(1),
            should_fail: Mutex::new(HashSet::new()),
            close_log: Mutex::new(Vec::new()),
        }
    }

    fn fail_on(&self, pane_id: &str) {
        self.should_fail.lock().unwrap().insert(pane_id.to_string());
    }
}

impl PaneController for FailingPaneController {
    fn create_pane(&self, _cmd: Option<&str>, _cwd: Option<&str>) -> Result<String, PaneError> {
        let mut id = self.next_id.lock().unwrap();
        let pane_id = format!("mock-{id}");
        *id += 1;
        Ok(pane_id)
    }

    fn write_to_pane(&self, _pane_id: &str, _text: &str) -> Result<(), PaneError> {
        Ok(())
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        self.close_log.lock().unwrap().push(pane_id.to_string());
        if self.should_fail.lock().unwrap().contains(pane_id) {
            Err(PaneError::CommandFailed(format!(
                "F4 test stub: simulated StopAgent failure for {pane_id}"
            )))
        } else {
            Ok(())
        }
    }

    fn rename_pane(&self, _pane_id: &str, name: &str) -> Result<RenameOutcome, PaneError> {
        Ok(RenameOutcome::applied(name))
    }

    fn focus_pane(&self, _pane_id: &str) -> Result<(), PaneError> {
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

    fn toggle_layout(&self) -> Result<(), PaneError> {
        Ok(())
    }

    fn name(&self) -> &str {
        "failing-mock"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn mode_config_with_two_panes() -> ModeConfig {
    ModeConfig {
        name: "f4-test".into(),
        init_command: None,
        panes: vec![
            ModePersistentPane {
                command: "echo a".into(),
                name: Some("A".into()),
                watch: false,
            },
            ModePersistentPane {
                command: "echo b".into(),
                name: Some("B".into()),
                watch: false,
            },
        ],
        rules: vec![ModeRule {
            pattern: r"k\\s+get".into(),
            watch: false,
            interval: None,
        }],
        reactive_panes: 2,
    }
}

fn orchestration_config_two_roles() -> OrchestrationConfig {
    OrchestrationConfig {
        name: "test-orch".into(),
        roles: vec![
            OrchestrationRoleConfig {
                name: "alpha".into(),
                command: "echo alpha".into(),
                start: true,
                description: None,
                prompt_template: None,
                clear: true,
            },
            OrchestrationRoleConfig {
                name: "beta".into(),
                command: "echo beta".into(),
                start: false,
                description: None,
                prompt_template: None,
                clear: true,
            },
        ],
    }
}

#[test]
fn ok_close_pane_records_in_closed_with_no_failures() {
    // Baseline: when no pane is set to fail, the outcome's `closed`
    // contains every pane id and `failed` is empty. Pins the happy
    // path so the failure tests below are clearly distinguished.
    let mock = Arc::new(FailingPaneController::new());
    let mut tm = TabManager::new(mock.clone());
    let (_idx, role_ids) = tm
        .open_orchestration_tab(&orchestration_config_two_roles(), "/tmp", None, (24, 80))
        .unwrap();
    assert_eq!(role_ids.len(), 2);

    let outcome = tm.close_tab(1).unwrap();
    assert!(
        outcome.is_clean(),
        "expected no failures, got {:?}",
        outcome.failed
    );
    assert_eq!(outcome.closed.len(), 2);
    assert!(outcome.failed.is_empty());
}

#[test]
fn err_close_pane_records_in_failed_keeps_clean_panes_in_closed() {
    // Orchestration tab teardown with one failing role pane: the
    // outcome must have the failing pane in `failed` and the healthy
    // pane in `closed`. The UI Ctrl+W handler uses this split to keep
    // the failed card present while removing the healthy one.
    let mock = Arc::new(FailingPaneController::new());
    let mut tm = TabManager::new(mock.clone());
    let (_idx, role_ids) = tm
        .open_orchestration_tab(&orchestration_config_two_roles(), "/tmp", None, (24, 80))
        .unwrap();
    let [alpha_id, beta_id]: [String; 2] = role_ids.try_into().expect("two roles");

    // Fail on the second pane only.
    mock.fail_on(&beta_id);

    let outcome = tm.close_tab(1).unwrap();
    assert!(!outcome.is_clean());
    assert_eq!(outcome.closed, vec![alpha_id.clone()]);
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].0, beta_id);
    assert!(
        outcome.failed[0].1.contains("simulated StopAgent failure"),
        "rendered error must come from the stub, got {:?}",
        outcome.failed[0].1
    );
}

#[test]
fn err_on_mode_agent_pane_keeps_failing_id_in_failed() {
    // Mode tab teardown where the *agent pane* close fails. The side
    // panes (closed by `ModeManager::deactivate_mode` inside
    // `close_tab`) should succeed and land in `closed`; the agent
    // pane id should land in `failed`. The dashboard card for the
    // agent pane must therefore be preserved by the Ctrl+W handler.
    let mock = Arc::new(FailingPaneController::new());
    let mut tm = TabManager::new(mock.clone());
    let agent_id = mock
        .create_pane(Some("echo agent"), Some("/tmp"))
        .expect("create agent pane");
    let (_idx, side_ids) = tm
        .open_mode_tab(
            &mode_config_with_two_panes(),
            "/tmp",
            agent_id.clone(),
            (24, 80),
        )
        .unwrap();
    assert!(!side_ids.is_empty(), "mode tab should have side panes");

    mock.fail_on(&agent_id);

    let outcome = tm.close_tab(1).unwrap();
    assert!(!outcome.is_clean());
    // Side panes closed OK.
    for id in &side_ids {
        assert!(
            outcome.closed.contains(id),
            "side pane {id} should be in closed; got {:?}",
            outcome.closed
        );
    }
    // Agent pane failed.
    let failed_ids: Vec<&String> = outcome.failed.iter().map(|(id, _)| id).collect();
    assert_eq!(failed_ids, vec![&agent_id]);
}

#[test]
fn mode_deactivate_with_one_failing_pane_splits_outcome() {
    // ModeManager::deactivate_mode runs independently of TabManager;
    // pin its outcome shape directly. With one persistent-pane close
    // failing, the other lands in `closed` and the failing one lands
    // in `failed`. The reactive-pool panes (spawned implicitly on
    // activate) are also closed and inspected.
    let mock = Arc::new(FailingPaneController::new());
    let mut mgr = ModeManager::new(mock.clone());
    mgr.activate_mode(&mode_config_with_two_panes(), Some("/tmp"), (24, 80))
        .unwrap();

    // Capture the persistent pane IDs so we know which one to fail on.
    let persistent_ids = mgr.managed_pane_ids();
    assert!(
        persistent_ids.len() >= 2,
        "expected at least 2 persistent panes from the test mode config, got {persistent_ids:?}"
    );
    let failing = persistent_ids[0].clone();
    mock.fail_on(&failing);

    let outcome = mgr
        .deactivate_mode()
        .expect("deactivate_mode should return Ok");
    assert!(!outcome.is_clean());
    let failed_ids: Vec<&String> = outcome.failed.iter().map(|(id, _)| id).collect();
    assert_eq!(
        failed_ids,
        vec![&failing],
        "only the nominated pane should appear in failed"
    );
    // Every other managed pane went into `closed`.
    for id in &persistent_ids[1..] {
        assert!(
            outcome.closed.contains(id),
            "pane {id} should be in closed; got {:?}",
            outcome.closed
        );
    }
}
