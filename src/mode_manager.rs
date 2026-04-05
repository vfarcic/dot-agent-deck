use std::sync::Arc;

use regex::Regex;
use thiserror::Error;

use crate::pane::{PaneController, PaneError};
use crate::project_config::ModeConfig;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ModeManagerError {
    #[error("Invalid regex pattern '{pattern}': {source}")]
    InvalidPattern {
        pattern: String,
        source: regex::Error,
    },
    #[error("Pane error: {0}")]
    Pane(#[from] PaneError),
    #[error("No mode is currently active")]
    NoActiveMode,
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

struct CompiledRule {
    regex: Regex,
    watch: bool,
    interval: Option<u64>,
}

struct ReactivePool {
    pane_ids: Vec<String>,
    next: usize,
}

impl ReactivePool {
    fn new() -> Self {
        Self {
            pane_ids: Vec::new(),
            next: 0,
        }
    }

    fn add(&mut self, pane_id: String) {
        self.pane_ids.push(pane_id);
    }

    fn allocate(&mut self) -> Option<&str> {
        if self.pane_ids.is_empty() {
            return None;
        }
        let id = &self.pane_ids[self.next];
        self.next = (self.next + 1) % self.pane_ids.len();
        Some(id)
    }

    fn all_ids(&self) -> &[String] {
        &self.pane_ids
    }
}

struct WatchHandle {
    abort_handle: tokio::task::AbortHandle,
    pane_id: String,
}

struct ActiveMode {
    name: String,
    compiled_rules: Vec<CompiledRule>,
    persistent_pane_ids: Vec<String>,
    reactive_pool: ReactivePool,
    watch_handles: Vec<WatchHandle>,
}

// ---------------------------------------------------------------------------
// ModeManager
// ---------------------------------------------------------------------------

pub struct ModeManager {
    pane_controller: Arc<dyn PaneController>,
    active_mode: Option<ActiveMode>,
    reactive_pool_size: usize,
}

impl ModeManager {
    pub fn new(pane_controller: Arc<dyn PaneController>, reactive_pool_size: usize) -> Self {
        Self {
            pane_controller,
            active_mode: None,
            reactive_pool_size,
        }
    }

    pub fn activate_mode(&mut self, config: &ModeConfig) -> Result<(), ModeManagerError> {
        // Deactivate any existing mode first
        if self.active_mode.is_some() {
            self.deactivate_mode()?;
        }

        // Compile regex rules — fail fast on invalid patterns
        let compiled_rules = config
            .rules
            .iter()
            .map(|rule| {
                let regex = Regex::new(&rule.pattern).map_err(|source| {
                    ModeManagerError::InvalidPattern {
                        pattern: rule.pattern.clone(),
                        source,
                    }
                })?;
                Ok::<_, ModeManagerError>(CompiledRule {
                    regex,
                    watch: rule.watch,
                    interval: rule.interval,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Create persistent panes
        let mut persistent_pane_ids = Vec::with_capacity(config.panes.len());
        for pane_cfg in &config.panes {
            let pane_id = self.pane_controller.create_pane(None, None)?;

            let display_name = pane_cfg.name.as_deref().unwrap_or(&pane_cfg.command);
            self.pane_controller.rename_pane(&pane_id, display_name)?;

            if let Some(ref init_cmd) = config.shell_init {
                self.pane_controller.write_to_pane(&pane_id, init_cmd)?;
            }

            self.pane_controller
                .write_to_pane(&pane_id, &pane_cfg.command)?;

            persistent_pane_ids.push(pane_id);
        }

        // Create reactive pool panes
        let mut reactive_pool = ReactivePool::new();
        for i in 0..self.reactive_pool_size {
            let pane_id = self.pane_controller.create_pane(None, None)?;
            self.pane_controller
                .rename_pane(&pane_id, &format!("reactive-{i}"))?;

            if let Some(ref init_cmd) = config.shell_init {
                self.pane_controller.write_to_pane(&pane_id, init_cmd)?;
            }

            reactive_pool.add(pane_id);
        }

        self.active_mode = Some(ActiveMode {
            name: config.name.clone(),
            compiled_rules,
            persistent_pane_ids,
            reactive_pool,
            watch_handles: Vec::new(),
        });

        Ok(())
    }

    pub fn deactivate_mode(&mut self) -> Result<(), ModeManagerError> {
        let mode = self
            .active_mode
            .take()
            .ok_or(ModeManagerError::NoActiveMode)?;

        // Cancel all watch tasks
        for wh in &mode.watch_handles {
            wh.abort_handle.abort();
        }

        // Close persistent panes
        for id in &mode.persistent_pane_ids {
            let _ = self.pane_controller.close_pane(id);
        }

        // Close reactive panes
        for id in mode.reactive_pool.all_ids() {
            let _ = self.pane_controller.close_pane(id);
        }

        Ok(())
    }

    pub fn handle_command(&mut self, command: &str) -> Result<Option<String>, ModeManagerError> {
        let mode = self
            .active_mode
            .as_mut()
            .ok_or(ModeManagerError::NoActiveMode)?;

        // Find the first matching rule
        let matched_idx = mode
            .compiled_rules
            .iter()
            .position(|r| r.regex.is_match(command));

        let rule_idx = match matched_idx {
            Some(i) => i,
            None => return Ok(None),
        };

        // Allocate a reactive pane
        let pane_id = match mode.reactive_pool.allocate() {
            Some(id) => id.to_string(),
            None => {
                return Err(ModeManagerError::Pane(PaneError::CommandFailed(
                    "No reactive panes available".into(),
                )));
            }
        };

        // Cancel any existing watch on this pane
        mode.watch_handles.retain(|wh| {
            if wh.pane_id == pane_id {
                wh.abort_handle.abort();
                false
            } else {
                true
            }
        });

        // Send the command
        self.pane_controller.write_to_pane(&pane_id, command)?;

        // If this is a watch rule, start periodic re-execution
        let watch = mode.compiled_rules[rule_idx].watch;
        let interval = mode.compiled_rules[rule_idx].interval;
        if watch {
            let interval_secs = interval.unwrap_or(5);
            let abort_handle = Self::start_watch(
                &self.pane_controller,
                pane_id.clone(),
                command.to_string(),
                interval_secs,
            );
            mode.watch_handles.push(WatchHandle {
                abort_handle,
                pane_id: pane_id.clone(),
            });
        }

        Ok(Some(pane_id))
    }

    pub fn active_mode_name(&self) -> Option<&str> {
        self.active_mode.as_ref().map(|m| m.name.as_str())
    }

    pub fn managed_pane_ids(&self) -> Vec<String> {
        match &self.active_mode {
            Some(mode) => {
                let mut ids = mode.persistent_pane_ids.clone();
                ids.extend(mode.reactive_pool.all_ids().iter().cloned());
                ids
            }
            None => Vec::new(),
        }
    }

    fn start_watch(
        pane_controller: &Arc<dyn PaneController>,
        pane_id: String,
        command: String,
        interval_secs: u64,
    ) -> tokio::task::AbortHandle {
        let controller = Arc::clone(pane_controller);
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                // Send Ctrl-C to kill any previous execution
                let _ = controller.write_to_pane(&pane_id, "\x03");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = controller.write_to_pane(&pane_id, &command);
            }
        });
        handle.abort_handle()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{PaneDirection, PaneInfo};
    use crate::project_config::{ModePersistentPane, ModeRule};
    use std::any::Any;
    use std::sync::Mutex;

    struct MockPaneController {
        next_id: Mutex<u64>,
        written: Mutex<Vec<(String, String)>>,
        closed: Mutex<Vec<String>>,
        renamed: Mutex<Vec<(String, String)>>,
    }

    impl MockPaneController {
        fn new() -> Self {
            Self {
                next_id: Mutex::new(1),
                written: Mutex::new(Vec::new()),
                closed: Mutex::new(Vec::new()),
                renamed: Mutex::new(Vec::new()),
            }
        }
    }

    impl PaneController for MockPaneController {
        fn create_pane(&self, _cmd: Option<&str>, _cwd: Option<&str>) -> Result<String, PaneError> {
            let mut id = self.next_id.lock().unwrap();
            let pane_id = format!("mock-{id}");
            *id += 1;
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

    fn test_config() -> ModeConfig {
        ModeConfig {
            name: "kubernetes".to_string(),
            shell_init: None,
            panes: vec![ModePersistentPane {
                command: "kubectl get pods -w".to_string(),
                name: Some("Pods".to_string()),
            }],
            rules: vec![
                ModeRule {
                    pattern: r"kubectl\s+describe".to_string(),
                    watch: false,
                    interval: None,
                },
                ModeRule {
                    pattern: r"kubectl\s+get".to_string(),
                    watch: true,
                    interval: Some(2),
                },
            ],
        }
    }

    #[test]
    fn activate_creates_persistent_and_reactive_panes() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone(), 3);
        mgr.activate_mode(&test_config()).unwrap();

        // 1 persistent + 3 reactive = 4 panes
        let ids = mgr.managed_pane_ids();
        assert_eq!(ids.len(), 4);
        assert_eq!(mgr.active_mode_name(), Some("kubernetes"));
    }

    #[test]
    fn shell_init_sent_before_command() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone(), 1);

        let mut config = test_config();
        config.shell_init = Some("source .env".to_string());
        mgr.activate_mode(&config).unwrap();

        let written = mock.written.lock().unwrap();
        // Persistent pane: shell_init then command
        assert_eq!(
            written[0],
            ("mock-1".to_string(), "source .env".to_string())
        );
        assert_eq!(
            written[1],
            ("mock-1".to_string(), "kubectl get pods -w".to_string())
        );
        // Reactive pane: shell_init
        assert_eq!(
            written[2],
            ("mock-2".to_string(), "source .env".to_string())
        );
    }

    #[test]
    fn handle_command_matches_first_rule() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone(), 2);
        mgr.activate_mode(&test_config()).unwrap();

        let pane_id = mgr
            .handle_command("kubectl describe pod nginx")
            .unwrap()
            .unwrap();

        // Should match first rule (describe), dispatch to first reactive pane
        assert_eq!(pane_id, "mock-2"); // mock-1 is persistent, mock-2 is first reactive

        let written = mock.written.lock().unwrap();
        let last = written.last().unwrap();
        assert_eq!(last.1, "kubectl describe pod nginx");
    }

    #[test]
    fn reactive_pool_cycles() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone(), 2);

        let config = ModeConfig {
            name: "test".to_string(),
            shell_init: None,
            panes: vec![],
            rules: vec![ModeRule {
                pattern: r".*".to_string(),
                watch: false,
                interval: None,
            }],
        };
        mgr.activate_mode(&config).unwrap();

        let p1 = mgr.handle_command("cmd1").unwrap().unwrap();
        let p2 = mgr.handle_command("cmd2").unwrap().unwrap();
        let p3 = mgr.handle_command("cmd3").unwrap().unwrap();

        // 2 reactive panes: mock-1, mock-2 (no persistent panes)
        assert_eq!(p1, "mock-1");
        assert_eq!(p2, "mock-2");
        assert_eq!(p3, "mock-1"); // wraps around
    }

    #[test]
    fn deactivate_closes_all_panes() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone(), 2);
        mgr.activate_mode(&test_config()).unwrap();

        mgr.deactivate_mode().unwrap();

        let closed = mock.closed.lock().unwrap();
        // 1 persistent + 2 reactive = 3 panes closed
        assert_eq!(closed.len(), 3);
        assert!(mgr.active_mode_name().is_none());
        assert!(mgr.managed_pane_ids().is_empty());
    }

    #[test]
    fn invalid_regex_returns_error() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock, 1);

        let config = ModeConfig {
            name: "bad".to_string(),
            shell_init: None,
            panes: vec![],
            rules: vec![ModeRule {
                pattern: r"[invalid".to_string(),
                watch: false,
                interval: None,
            }],
        };

        let err = mgr.activate_mode(&config).unwrap_err();
        assert!(matches!(err, ModeManagerError::InvalidPattern { .. }));
    }

    #[test]
    fn handle_command_no_active_mode_errors() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock, 1);

        let err = mgr.handle_command("anything").unwrap_err();
        assert!(matches!(err, ModeManagerError::NoActiveMode));
    }

    #[test]
    fn no_match_returns_none() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock, 2);
        mgr.activate_mode(&test_config()).unwrap();

        let result = mgr.handle_command("echo hello").unwrap();
        assert!(result.is_none());
    }
}
