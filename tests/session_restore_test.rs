//! Regression tests for PRD #69 — mode-tab persistence across `--continue`.
//!
//! Verifies that:
//! 1. `SavedSession::snapshot` preserves `SavedPane.mode` when called against
//!    a live-pane set that includes mode-tab agent panes (the pre-teardown
//!    invariant the production exit path now upholds).
//! 2. The save → load round-trip preserves the mode field on disk.
//! 3. End-to-end: loading a session.toml that contains a mode pane is
//!    sufficient to recreate the mode tab (agent + side panes) on restore.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use dot_agent_deck::config::{SavedPane, SavedSession};
use dot_agent_deck::pane::{PaneController, PaneDirection, PaneError, PaneInfo, RenameOutcome};
use dot_agent_deck::project_config::{CONFIG_FILE_NAME, load_project_config};
use dot_agent_deck::tab::TabManager;

// ---------------------------------------------------------------------------
// Env-var serialization
// ---------------------------------------------------------------------------

/// Serializes tests in this binary that mutate `DOT_AGENT_DECK_SESSION`.
/// Cargo runs tests within a binary in parallel; without this lock they
/// would race on the shared env var.
static SESSION_ENV_LOCK: Mutex<()> = Mutex::new(());

struct SessionEnvGuard {
    _lock: MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl SessionEnvGuard {
    fn set(value: &str) -> Self {
        let lock = SESSION_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DOT_AGENT_DECK_SESSION").ok();
        // SAFETY: the lock above serializes env-var access across tests in
        // this binary; no other code reads this var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_SESSION", value);
        }
        Self { _lock: lock, prev }
    }
}

impl Drop for SessionEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see SessionEnvGuard::set.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SESSION", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SESSION"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mock pane controller (copy of the pattern in mode_integration_test.rs)
// ---------------------------------------------------------------------------

struct MockPaneController {
    next_id: Mutex<u64>,
    created: Mutex<Vec<String>>,
}

impl MockPaneController {
    fn new() -> Self {
        Self {
            next_id: Mutex::new(1),
            created: Mutex::new(Vec::new()),
        }
    }
}

impl PaneController for MockPaneController {
    fn create_pane(&self, _cmd: Option<&str>, _cwd: Option<&str>) -> Result<String, PaneError> {
        let mut id = self.next_id.lock().unwrap();
        let pane_id = format!("mock-{id}");
        *id += 1;
        self.created.lock().unwrap().push(pane_id.clone());
        Ok(pane_id)
    }

    fn write_to_pane(&self, _pane_id: &str, _text: &str) -> Result<(), PaneError> {
        Ok(())
    }

    fn close_pane(&self, _pane_id: &str) -> Result<(), PaneError> {
        Ok(())
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
        "mock"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

const MODE_CONFIG: &str = r#"
[[modes]]
name = "kubernetes-operations"

[[modes.panes]]
command = "kubectl get applications -n argocd -w"
name = "ArgoCD Apps"

[[modes.panes]]
command = "kubectl get events -A -w"
name = "Events"
"#;

fn project_dir_with_config() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(CONFIG_FILE_NAME), MODE_CONFIG).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Test 1: snapshot preserves the `mode` field when called pre-teardown.
//
// This is the direct regression for PRD #69. Before the fix, snapshot ran
// AFTER mode-tab teardown — by which point the agent pane id was no longer
// in `live_panes`, so `retain` dropped it and `mode` never reached disk.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_preserves_mode_field_when_called_pre_teardown() {
    let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
    pane_metadata.insert(
        "1".to_string(),
        SavedPane {
            dir: "/tmp/plain".to_string(),
            name: "plain".to_string(),
            command: "bash".to_string(),
            mode: None,
        },
    );
    pane_metadata.insert(
        "2".to_string(),
        SavedPane {
            dir: "/tmp/k8s".to_string(),
            name: "k8s-agent".to_string(),
            command: "claude".to_string(),
            mode: Some("kubernetes-operations".to_string()),
        },
    );

    // Pre-teardown: the live-pane set still contains every pane, including
    // the mode-tab agent pane id "2".
    let live_panes: HashSet<String> = ["1".to_string(), "2".to_string()].into_iter().collect();
    let pane_display_names: HashMap<String, String> = HashMap::new();

    let session = SavedSession::snapshot(&mut pane_metadata, &pane_display_names, &live_panes);

    assert_eq!(session.panes.len(), 2);
    let mode_pane = session
        .panes
        .iter()
        .find(|p| p.name == "k8s-agent")
        .expect("mode-tab agent pane must be in snapshot");
    assert_eq!(
        mode_pane.mode.as_deref(),
        Some("kubernetes-operations"),
        "mode field must survive snapshot — this is the PRD #69 regression"
    );
}

// ---------------------------------------------------------------------------
// Test 1b: snapshot still prunes externally-closed panes.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_prunes_externally_closed_panes() {
    let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
    pane_metadata.insert(
        "1".to_string(),
        SavedPane {
            dir: "/tmp/a".to_string(),
            name: "alive".to_string(),
            command: "bash".to_string(),
            mode: None,
        },
    );
    pane_metadata.insert(
        "2".to_string(),
        SavedPane {
            dir: "/tmp/b".to_string(),
            name: "externally-closed".to_string(),
            command: "bash".to_string(),
            mode: None,
        },
    );

    // Only pane "1" is in the live set — pane "2" was closed externally.
    let live_panes: HashSet<String> = ["1".to_string()].into_iter().collect();
    let pane_display_names: HashMap<String, String> = HashMap::new();

    let session = SavedSession::snapshot(&mut pane_metadata, &pane_display_names, &live_panes);

    assert_eq!(session.panes.len(), 1);
    assert_eq!(session.panes[0].name, "alive");
}

// ---------------------------------------------------------------------------
// Test 2: snapshot → save → load round-trips the mode field on disk.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_save_load_round_trips_mode_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.toml");
    let _guard = SessionEnvGuard::set(path.to_str().unwrap());

    let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
    pane_metadata.insert(
        "7".to_string(),
        SavedPane {
            dir: "/tmp/k8s".to_string(),
            name: "k8s-agent".to_string(),
            command: "claude".to_string(),
            mode: Some("kubernetes-operations".to_string()),
        },
    );
    let live_panes: HashSet<String> = ["7".to_string()].into_iter().collect();
    let pane_display_names: HashMap<String, String> = HashMap::new();

    let session = SavedSession::snapshot(&mut pane_metadata, &pane_display_names, &live_panes);
    session.save().expect("save should succeed");
    assert!(path.exists(), "save must create session.toml");

    let loaded = SavedSession::load();
    assert_eq!(loaded.panes.len(), 1);
    assert_eq!(
        loaded.panes[0].mode.as_deref(),
        Some("kubernetes-operations"),
        "mode field must survive disk round-trip"
    );
    assert_eq!(loaded.panes[0].name, "k8s-agent");
    assert_eq!(loaded.panes[0].command, "claude");
}

// ---------------------------------------------------------------------------
// Test 3: end-to-end save → restore recreates side panes.
//
// Mirrors the production flow: snapshot mode-tab metadata, write to disk,
// reload, look up the ModeConfig via `load_project_config`, then drive
// `TabManager::open_mode_tab` with a fresh agent pane — verifying the side
// panes are recreated by the mode itself.
// ---------------------------------------------------------------------------

#[test]
fn save_then_restore_recreates_side_panes() {
    let project_dir = project_dir_with_config();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // ---- Save side ----
    let session_dir = tempfile::tempdir().unwrap();
    let session_path = session_dir.path().join("session.toml");
    let _guard = SessionEnvGuard::set(session_path.to_str().unwrap());

    let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
    pane_metadata.insert(
        "42".to_string(),
        SavedPane {
            dir: project_path.clone(),
            name: "my-mode-tab".to_string(),
            command: "claude".to_string(),
            mode: Some("kubernetes-operations".to_string()),
        },
    );
    let live_panes: HashSet<String> = ["42".to_string()].into_iter().collect();
    let pane_display_names: HashMap<String, String> = HashMap::new();
    let session = SavedSession::snapshot(&mut pane_metadata, &pane_display_names, &live_panes);
    session.save().unwrap();

    // ---- Restore side ----
    let loaded = SavedSession::load();
    assert_eq!(loaded.panes.len(), 1);
    let restored = &loaded.panes[0];
    let mode_name = restored
        .mode
        .as_deref()
        .expect("mode field must be present after restore — PRD #69 regression");

    let cfg = load_project_config(std::path::Path::new(&restored.dir))
        .unwrap()
        .expect("project config must load");
    let mode_cfg = cfg
        .modes
        .iter()
        .find(|m| m.name == mode_name)
        .cloned()
        .expect("mode must exist in project config");

    let mock = Arc::new(MockPaneController::new());
    let mut tab_manager = TabManager::new(mock.clone());
    let agent_pane_id = mock.create_pane(None, Some(&restored.dir)).unwrap();
    let (_idx, side_ids) = tab_manager
        .open_mode_tab(&mode_cfg, &restored.dir, agent_pane_id.clone())
        .expect("open_mode_tab must succeed on restore");

    // Two persistent + default reactive panes (matches the existing
    // `mode_integration_test.rs::load_real_config_and_activate_mode`
    // expectation for this config shape).
    assert_eq!(
        side_ids.len(),
        4,
        "mode tab restore must recreate 2 persistent + 2 reactive side panes, got {}",
        side_ids.len()
    );
    let created = mock.created.lock().unwrap();
    assert_eq!(
        created.len(),
        5,
        "1 agent shell + 2 persistent + 2 reactive = 5 panes created on restore"
    );
}

// ---------------------------------------------------------------------------
// Test 4: pre-PRD-69 session.toml (no `mode` field on any pane) still parses.
//
// Pins the backwards-compatibility contract provided by `#[serde(default)]`
// on `SavedPane.mode`. If a future change breaks that contract,
// `SavedSession::load()` would log "Invalid session at..." and return
// `Self::default()` (empty panes), failing the pane-count assertion.
// ---------------------------------------------------------------------------

#[test]
fn load_legacy_session_without_mode_field_parses_with_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.toml");
    let legacy = r#"
[[panes]]
dir = "/repo/api"
name = "api"
command = "claude"

[[panes]]
dir = "/repo/ui"
name = "ui"
command = ""

[[panes]]
dir = "/repo/docs"
name = "docs"
command = "npm run dev"
"#;
    std::fs::write(&path, legacy).unwrap();
    let _guard = SessionEnvGuard::set(path.to_str().unwrap());

    let loaded = SavedSession::load();
    assert_eq!(loaded.panes.len(), 3);
    assert!(
        loaded.panes.iter().all(|p| p.mode.is_none()),
        "pre-PRD-69 session.toml must deserialize with mode == None on every pane",
    );
    assert_eq!(loaded.panes[0].name, "api");
    assert_eq!(loaded.panes[1].command, "");
    assert_eq!(loaded.panes[2].command, "npm run dev");
}
