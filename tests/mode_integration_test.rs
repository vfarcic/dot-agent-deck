use std::any::Any;
use std::sync::{Arc, Mutex};

use dot_agent_deck::mode_manager::ModeManager;
use dot_agent_deck::pane::{PaneController, PaneDirection, PaneError, PaneInfo};
use dot_agent_deck::project_config::{
    CONFIG_FILE_NAME, ModeConfig, ModePersistentPane, ModeRule, load_project_config,
};

// ---------------------------------------------------------------------------
// Mock pane controller (records all operations for assertion)
// ---------------------------------------------------------------------------

struct MockPaneController {
    next_id: Mutex<u64>,
    written: Mutex<Vec<(String, String)>>,
    closed: Mutex<Vec<String>>,
    renamed: Mutex<Vec<(String, String)>>,
    created: Mutex<Vec<String>>,
}

impl MockPaneController {
    fn new() -> Self {
        Self {
            next_id: Mutex::new(1),
            written: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            renamed: Mutex::new(Vec::new()),
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

    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError> {
        self.written
            .lock()
            .unwrap()
            .push((pane_id.to_string(), text.to_string()));
        Ok(())
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        self.closed.lock().unwrap().push(pane_id.to_string());
        Ok(())
    }

    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<(), PaneError> {
        self.renamed
            .lock()
            .unwrap()
            .push((pane_id.to_string(), name.to_string()));
        Ok(())
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
// Helper: create a temp dir with an embedded test config
// ---------------------------------------------------------------------------

const TEST_CONFIG: &str = r#"
[[modes]]
name = "kubernetes-operations"

[[modes.panes]]
command = "kubectl get applications -n argocd -w"
name = "ArgoCD Apps"

[[modes.panes]]
command = "kubectl get events -A -w"
name = "Events"

[[modes.rules]]
pattern = "kubectl\\s+.*(describe|explain)"
watch = false

[[modes.rules]]
pattern = "kubectl\\s+.*(get|top|logs)"
watch = true
interval = 2

[[modes.rules]]
pattern = "helm\\s+.*(status|list)"
watch = false
"#;

fn test_config_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(CONFIG_FILE_NAME), TEST_CONFIG).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Test 1: Load config and verify full structure
// ---------------------------------------------------------------------------

#[test]
fn load_real_config_and_verify_structure() {
    let dir = test_config_dir();

    let config = load_project_config(dir.path())
        .expect("should not error")
        .expect("config file exists");

    assert!(!config.modes.is_empty(), "should have at least one mode");

    let mode = &config.modes[0];
    assert_eq!(mode.name, "kubernetes-operations");
    assert_eq!(mode.panes.len(), 2);
    assert_eq!(mode.rules.len(), 3);

    // Persistent panes
    assert_eq!(
        mode.panes[0].command,
        "kubectl get applications -n argocd -w"
    );
    assert_eq!(mode.panes[0].name.as_deref(), Some("ArgoCD Apps"));
    assert_eq!(mode.panes[1].command, "kubectl get events -A -w");
    assert_eq!(mode.panes[1].name.as_deref(), Some("Events"));

    // Rules
    assert!(!mode.rules[0].watch);
    assert!(mode.rules[1].watch);
    assert_eq!(mode.rules[1].interval, Some(2));
    assert!(!mode.rules[2].watch);
}

// ---------------------------------------------------------------------------
// Test 2: Load config and activate mode — verify pane creation sequence
// ---------------------------------------------------------------------------

#[test]
fn load_real_config_and_activate_mode() {
    let dir = test_config_dir();

    let config = load_project_config(dir.path()).unwrap().unwrap();
    let mode = &config.modes[0];

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock.clone(), 3);
    mgr.activate_mode(mode, Some("/tmp/test")).unwrap();

    // 2 persistent + 3 reactive = 5 panes
    let ids = mgr.managed_pane_ids();
    assert_eq!(ids.len(), 5);
    assert_eq!(mgr.active_mode_name(), Some("kubernetes-operations"));

    // Verify all 5 panes were created
    let created = mock.created.lock().unwrap();
    assert_eq!(created.len(), 5);

    // Persistent panes now use create_pane(Some(command)), so no write_to_pane calls.
    // Reactive panes also get no writes at activation.
    let written = mock.written.lock().unwrap();
    assert!(
        written.is_empty(),
        "No write_to_pane calls expected during activation — persistent panes use direct command execution"
    );

    // Verify pane renames
    let renamed = mock.renamed.lock().unwrap();
    assert_eq!(
        renamed[0],
        ("mock-1".to_string(), "ArgoCD Apps".to_string())
    );
    assert_eq!(renamed[1], ("mock-2".to_string(), "Events".to_string()));
    assert_eq!(renamed[2], ("mock-3".to_string(), "reactive-0".to_string()));
    assert_eq!(renamed[3], ("mock-4".to_string(), "reactive-1".to_string()));
    assert_eq!(renamed[4], ("mock-5".to_string(), "reactive-2".to_string()));
}

// ---------------------------------------------------------------------------
// Test 3: End-to-end command routing with config rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_command_routing() {
    let dir = test_config_dir();

    let config = load_project_config(dir.path()).unwrap().unwrap();
    let mode = &config.modes[0];

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock.clone(), 3);
    mgr.activate_mode(mode, None).unwrap();

    // kubectl describe → matches rule 1 (watch=false)
    let result = mgr.handle_command("kubectl describe pod nginx").unwrap();
    assert!(result.is_some(), "kubectl describe should match");

    // kubectl get → matches rule 2 (watch=true, interval=2)
    let result = mgr
        .handle_command("kubectl get pods -n production")
        .unwrap();
    assert!(result.is_some(), "kubectl get should match");

    // helm status → matches rule 3 (watch=false)
    let result = mgr.handle_command("helm status myrelease").unwrap();
    assert!(result.is_some(), "helm status should match");

    // Non-matching commands
    let result = mgr.handle_command("echo hello").unwrap();
    assert!(result.is_none(), "echo should not match any rule");

    let result = mgr.handle_command("terraform plan").unwrap();
    assert!(result.is_none(), "terraform should not match any rule");

    let result = mgr.handle_command("docker build .").unwrap();
    assert!(result.is_none(), "docker should not match any rule");

    // All matched commands (watch and non-watch) use close+recreate.
    // No write_to_pane calls expected — watch rules now use `dot-agent-deck watch` subprocess.
    let written = mock.written.lock().unwrap();
    assert!(
        written.is_empty(),
        "No write_to_pane calls expected — all matched commands use close+recreate"
    );

    // All 3 matched commands should have closed old panes
    let closed = mock.closed.lock().unwrap();
    assert_eq!(
        closed.len(),
        3,
        "All matched commands (describe, get, helm) should close old reactive panes"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Reactive pool cycling with config
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reactive_pool_cycling_with_real_config() {
    let dir = test_config_dir();

    let config = load_project_config(dir.path()).unwrap().unwrap();
    let mode = &config.modes[0];

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock.clone(), 3);
    mgr.activate_mode(mode, None).unwrap();

    // Send 6 matching commands to cycle through 3 reactive panes twice
    let commands = [
        "kubectl get pods",
        "kubectl get svc",
        "kubectl get nodes",
        "kubectl get deployments",
        "kubectl get ingress",
        "kubectl get configmaps",
    ];

    // All commands should match and route to reactive panes
    for cmd in &commands {
        let change = mgr.handle_command(cmd).unwrap();
        assert!(change.is_some(), "Command '{cmd}' should match a rule");
    }
}

// ---------------------------------------------------------------------------
// Test 5: Mode deactivation cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mode_deactivation_closes_all_panes() {
    let dir = test_config_dir();

    let config = load_project_config(dir.path()).unwrap().unwrap();
    let mode = &config.modes[0];

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock.clone(), 3);
    mgr.activate_mode(mode, None).unwrap();

    // Route a command to create a watch task
    let _ = mgr.handle_command("kubectl get pods").unwrap();

    // Deactivate
    mgr.deactivate_mode().unwrap();

    // 1 close from handle_command (watch rule closes+recreates) + 5 from deactivation
    let closed = mock.closed.lock().unwrap();
    assert_eq!(
        closed.len(),
        6,
        "1 (watch close+recreate) + 5 (2 persistent + 3 reactive deactivation) = 6"
    );

    // Verify mode is fully cleared
    assert!(mgr.active_mode_name().is_none());
    assert!(mgr.managed_pane_ids().is_empty());
}

// ---------------------------------------------------------------------------
// Test 6: Config not found returns None
// ---------------------------------------------------------------------------

#[test]
fn config_not_found_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let result = load_project_config(dir.path()).unwrap();
    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// Test 7: Invalid TOML returns parse error
// ---------------------------------------------------------------------------

#[test]
fn invalid_toml_returns_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(CONFIG_FILE_NAME),
        "this is { not valid toml",
    )
    .unwrap();
    let result = load_project_config(dir.path());
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Test 8: Mode activation with invalid regex fails gracefully
// ---------------------------------------------------------------------------

#[test]
fn invalid_regex_in_config_fails_activation() {
    let config = ModeConfig {
        name: "bad-regex".to_string(),
        init_command: None,
        panes: vec![],
        rules: vec![ModeRule {
            pattern: "[unclosed".to_string(),
            watch: false,
            interval: None,
        }],
        reactive_panes: 1,
    };

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock, 1);
    let result = mgr.activate_mode(&config, None);
    assert!(result.is_err(), "invalid regex should fail activation");
}

// ---------------------------------------------------------------------------
// Test 9: Mode switching (activate new mode deactivates old)
// ---------------------------------------------------------------------------

#[test]
fn mode_switching_cleans_up_previous() {
    let mode_a = ModeConfig {
        name: "mode-a".to_string(),
        init_command: None,
        panes: vec![ModePersistentPane {
            command: "echo a".to_string(),
            name: Some("A".to_string()),
            watch: false,
        }],
        rules: vec![],
        reactive_panes: 1,
    };

    let mode_b = ModeConfig {
        name: "mode-b".to_string(),
        init_command: None,
        panes: vec![ModePersistentPane {
            command: "echo b".to_string(),
            name: Some("B".to_string()),
            watch: false,
        }],
        rules: vec![],
        reactive_panes: 1,
    };

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock.clone(), 1);

    // Activate mode A
    mgr.activate_mode(&mode_a, None).unwrap();
    let ids_a = mgr.managed_pane_ids();
    assert_eq!(ids_a.len(), 2); // 1 persistent + 1 reactive
    assert_eq!(mgr.active_mode_name(), Some("mode-a"));

    // Activate mode B (should deactivate A first)
    mgr.activate_mode(&mode_b, None).unwrap();
    assert_eq!(mgr.active_mode_name(), Some("mode-b"));

    // Old panes from mode A should have been closed
    let closed = mock.closed.lock().unwrap();
    assert_eq!(closed.len(), 2, "mode-a panes should be closed on switch");
    assert!(closed.contains(&"mock-1".to_string()));
    assert!(closed.contains(&"mock-2".to_string()));
}

// ---------------------------------------------------------------------------
// Test 10: All-reactive mode (no persistent panes)
// ---------------------------------------------------------------------------

#[test]
fn all_reactive_mode_works() {
    let config = ModeConfig {
        name: "reactive-only".to_string(),
        init_command: None,
        panes: vec![],
        rules: vec![ModeRule {
            pattern: r".*".to_string(),
            watch: false,
            interval: None,
        }],
        reactive_panes: 2,
    };

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock.clone(), 2);
    mgr.activate_mode(&config, None).unwrap();

    assert_eq!(mgr.managed_pane_ids().len(), 2); // 0 persistent + 2 reactive

    let result = mgr.handle_command("any command").unwrap();
    assert!(result.is_some());
}

// ---------------------------------------------------------------------------
// Test 11: All-persistent mode (no rules)
// ---------------------------------------------------------------------------

#[test]
fn all_persistent_mode_works() {
    let config = ModeConfig {
        name: "persistent-only".to_string(),
        init_command: None,
        panes: vec![ModePersistentPane {
            command: "date".to_string(),
            name: Some("Clock".to_string()),
            watch: false,
        }],
        rules: vec![],
        reactive_panes: 2,
    };

    let mock = Arc::new(MockPaneController::new());
    let mut mgr = ModeManager::new(mock.clone(), 2);
    mgr.activate_mode(&config, None).unwrap();

    // 1 persistent + 2 reactive (pool still created even with no rules)
    assert_eq!(mgr.managed_pane_ids().len(), 3);

    // No rules → nothing matches
    let result = mgr.handle_command("echo hello").unwrap();
    assert!(result.is_none());
}
