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

    fn replace(&mut self, old_id: &str, new_id: String) {
        if let Some(pos) = self.pane_ids.iter().position(|id| id == old_id) {
            self.pane_ids[pos] = new_id;
        }
    }
}

struct PendingCommand {
    pane_id: String,
    init_command: Option<String>,
    command: String,
}

struct ActiveMode {
    name: String,
    has_init: bool,
    compiled_rules: Vec<CompiledRule>,
    persistent_pane_ids: Vec<String>,
    reactive_pool: ReactivePool,
    pending_commands: Vec<PendingCommand>,
}

/// Result of routing a command to a reactive pane.
#[derive(Debug, PartialEq)]
pub struct PaneChange {
    /// Pane that was closed (if recreated).
    pub closed: Option<String>,
    /// Pane that was created (if recreated).
    pub created: Option<String>,
}

// ---------------------------------------------------------------------------
// ModeManager
// ---------------------------------------------------------------------------

pub struct ModeManager {
    pane_controller: Arc<dyn PaneController>,
    active_mode: Option<ActiveMode>,
    cwd: Option<String>,
}

impl ModeManager {
    pub fn new(pane_controller: Arc<dyn PaneController>) -> Self {
        Self {
            pane_controller,
            active_mode: None,
            cwd: None,
        }
    }

    pub fn activate_mode(
        &mut self,
        config: &ModeConfig,
        cwd: Option<&str>,
    ) -> Result<(), ModeManagerError> {
        // Deactivate any existing mode first
        if self.active_mode.is_some() {
            self.deactivate_mode()?;
        }

        self.cwd = cwd.map(|s| s.to_string());

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

        // Phase 1: Create all panes as empty shells. Commands are NOT sent yet —
        // the caller must resize panes to correct dimensions, then call
        // start_mode_commands() to send commands at the right PTY size.
        // Track all created panes so we can clean up on partial failure.
        let mut created_pane_ids: Vec<String> = Vec::new();

        let result =
            (|| -> Result<(Vec<String>, Vec<PendingCommand>, ReactivePool), ModeManagerError> {
                let mut persistent_ids = Vec::with_capacity(config.panes.len());
                let mut pending = Vec::new();

                for pane_cfg in &config.panes {
                    let effective_cmd = if pane_cfg.watch {
                        let exe = std::env::current_exe()
                            .unwrap_or_else(|_| std::path::PathBuf::from("dot-agent-deck"));
                        format!(
                            "{} watch --interval 10 {:?}",
                            exe.display(),
                            pane_cfg.command
                        )
                    } else {
                        pane_cfg.command.clone()
                    };

                    let pane_id = self.pane_controller.create_pane(None, cwd)?;
                    created_pane_ids.push(pane_id.clone());
                    let display_name = pane_cfg.name.as_deref().unwrap_or(&pane_cfg.command);
                    self.pane_controller.rename_pane(&pane_id, display_name)?;

                    pending.push(PendingCommand {
                        pane_id: pane_id.clone(),
                        init_command: config.init_command.clone(),
                        command: effective_cmd,
                    });

                    persistent_ids.push(pane_id);
                }

                let mut pool = ReactivePool::new();
                for i in 0..config.reactive_panes {
                    let pane_id = self.pane_controller.create_pane(None, cwd)?;
                    created_pane_ids.push(pane_id.clone());
                    self.pane_controller
                        .rename_pane(&pane_id, &format!("reactive-{i}"))?;

                    // Reactive panes only need init_command (no command until a rule matches)
                    if config.init_command.is_some() {
                        pending.push(PendingCommand {
                            pane_id: pane_id.clone(),
                            init_command: config.init_command.clone(),
                            command: String::new(),
                        });
                    }

                    pool.add(pane_id);
                }

                Ok((persistent_ids, pending, pool))
            })();

        let (persistent_pane_ids, pending_commands, reactive_pool) = match result {
            Ok(v) => v,
            Err(e) => {
                // Clean up any panes created before the failure.
                for id in &created_pane_ids {
                    let _ = self.pane_controller.close_pane(id);
                }
                return Err(e);
            }
        };

        self.active_mode = Some(ActiveMode {
            name: config.name.clone(),
            has_init: config.init_command.is_some(),
            compiled_rules,
            persistent_pane_ids,
            reactive_pool,
            pending_commands,
        });

        Ok(())
    }

    /// Phase 2: Send commands to panes. Must be called after panes are resized
    /// to correct display dimensions to avoid stale content artifacts.
    pub fn start_mode_commands(&mut self) -> Result<(), ModeManagerError> {
        let mode = self
            .active_mode
            .as_mut()
            .ok_or(ModeManagerError::NoActiveMode)?;

        // Collect reactive IDs so we can suppress their prompts after commands.
        let reactive_ids: Vec<String> = mode.reactive_pool.all_ids().to_vec();

        let mut failed = Vec::new();
        let pending = std::mem::take(&mut mode.pending_commands);
        for cmd in pending {
            let is_reactive = reactive_ids.contains(&cmd.pane_id);
            let ok = (|| -> Result<(), ModeManagerError> {
                if let Some(ref init) = cmd.init_command {
                    self.pane_controller.write_to_pane(&cmd.pane_id, init)?;
                }
                if !cmd.command.is_empty() {
                    self.pane_controller
                        .write_to_pane(&cmd.pane_id, &cmd.command)?;
                }
                // Hide the shell prompt in reactive panes so automated
                // command output is not cluttered by prompt strings.
                // Clear the screen afterwards so the export command itself
                // and any prior prompt output are not visible.
                if is_reactive {
                    self.pane_controller.write_to_pane(
                        &cmd.pane_id,
                        "export PS1= PS2= PROMPT= && printf '\\x1b[3J\\x1b[2J\\x1b[H'",
                    )?;
                }
                Ok(())
            })();
            if ok.is_err() {
                failed.push(cmd);
            }
        }
        mode.pending_commands = failed;

        Ok(())
    }

    pub fn deactivate_mode(&mut self) -> Result<(), ModeManagerError> {
        let mode = self
            .active_mode
            .take()
            .ok_or(ModeManagerError::NoActiveMode)?;

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

    /// Routes a command to a matching reactive pane. Returns pane change info:
    /// - `None` if no rule matched
    /// - `Some((closed_pane_id, new_pane_id))` if a pane was recreated
    /// - `Some((None, Some(pane_id)))` if the command was written to an existing pane (watch rules)
    pub fn handle_command(
        &mut self,
        command: &str,
    ) -> Result<Option<PaneChange>, ModeManagerError> {
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
        let old_pane_id = match mode.reactive_pool.allocate() {
            Some(id) => id.to_string(),
            None => {
                return Err(ModeManagerError::Pane(PaneError::CommandFailed(
                    "No reactive panes available".into(),
                )));
            }
        };

        let watch = mode.compiled_rules[rule_idx].watch;
        let interval = mode.compiled_rules[rule_idx].interval;

        let pane_cmd = if watch {
            let exe = std::env::current_exe()
                .unwrap_or_else(|_| std::path::PathBuf::from("dot-agent-deck"));
            let interval_secs = interval.unwrap_or(5);
            format!(
                "{} watch --interval {} {:?}",
                exe.display(),
                interval_secs,
                command
            )
        } else {
            command.to_string()
        };

        if mode.has_init {
            // Reuse existing shell pane to preserve init_command environment.
            // Send Ctrl+C to stop any running command, then clear scrollback + screen
            // before running the new command so old output is not visible.
            let _ = self.pane_controller.write_to_pane(&old_pane_id, "\x03");
            self.pane_controller.write_to_pane(
                &old_pane_id,
                &format!(
                    "export PS1= PS2= PROMPT= && printf '\\x1b[3J\\x1b[2J\\x1b[H' && {pane_cmd}"
                ),
            )?;
            let _ = self.pane_controller.rename_pane(&old_pane_id, command);
            Ok(Some(PaneChange {
                closed: None,
                created: None,
            }))
        } else {
            // No init_command — create replacement before closing old pane so the
            // pool never contains a dead slot if creation fails.
            let new_pane_id = self
                .pane_controller
                .create_pane(Some(&pane_cmd), self.cwd.as_deref())?;
            let _ = self.pane_controller.rename_pane(&new_pane_id, command);
            mode.reactive_pool
                .replace(&old_pane_id, new_pane_id.clone());
            let _ = self.pane_controller.close_pane(&old_pane_id);
            Ok(Some(PaneChange {
                closed: Some(old_pane_id),
                created: Some(new_pane_id),
            }))
        }
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

    /// Returns `true` if the given pane belongs to the reactive pool.
    pub fn is_reactive_pane(&self, pane_id: &str) -> bool {
        self.active_mode
            .as_ref()
            .is_some_and(|m| m.reactive_pool.all_ids().iter().any(|id| id == pane_id))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{PaneDirection, PaneInfo, RenameOutcome};
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

        fn rename_pane(&self, pane_id: &str, name: &str) -> Result<RenameOutcome, PaneError> {
            self.renamed
                .lock()
                .unwrap()
                .push((pane_id.to_string(), name.to_string()));
            Ok(RenameOutcome::Applied(name.to_string()))
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
            init_command: None,
            panes: vec![ModePersistentPane {
                command: "kubectl get pods -w".to_string(),
                name: Some("Pods".to_string()),
                watch: false,
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
            reactive_panes: 2,
        }
    }

    #[test]
    fn activate_creates_persistent_and_reactive_panes() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone());
        mgr.activate_mode(&test_config(), None).unwrap();

        // 1 persistent + 2 reactive (from config) = 3 panes
        let ids = mgr.managed_pane_ids();
        assert_eq!(ids.len(), 3);
        assert_eq!(mgr.active_mode_name(), Some("kubernetes"));
    }

    #[test]
    fn handle_command_matches_first_rule() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone());
        mgr.activate_mode(&test_config(), None).unwrap();

        let change = mgr
            .handle_command("kubectl describe pod nginx")
            .unwrap()
            .unwrap();

        // Non-watch rule: old pane closed, new pane created
        assert_eq!(change.closed.as_deref(), Some("mock-2")); // first reactive pane
        assert!(change.created.is_some()); // new pane created with the command

        // Old pane was closed
        let closed = mock.closed.lock().unwrap();
        assert!(closed.contains(&"mock-2".to_string()));
    }

    #[test]
    fn reactive_pool_cycles() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone());

        let config = ModeConfig {
            name: "test".to_string(),
            init_command: None,
            panes: vec![],
            rules: vec![ModeRule {
                pattern: r".*".to_string(),
                watch: false,
                interval: None,
            }],
            reactive_panes: 2,
        };
        mgr.activate_mode(&config, None).unwrap();

        let p1 = mgr.handle_command("cmd1").unwrap().unwrap();
        let p2 = mgr.handle_command("cmd2").unwrap().unwrap();
        let p3 = mgr.handle_command("cmd3").unwrap().unwrap();

        // Each command closes old pane and creates new — IDs keep incrementing
        assert!(p1.closed.is_some());
        assert!(p1.created.is_some());
        assert!(p2.closed.is_some());
        assert!(p2.created.is_some());
        // Third command wraps around the pool and closes a recreated pane
        assert!(p3.closed.is_some());
        assert!(p3.created.is_some());
    }

    #[test]
    fn deactivate_closes_all_panes() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock.clone());
        mgr.activate_mode(&test_config(), None).unwrap();

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
        let mut mgr = ModeManager::new(mock);

        let config = ModeConfig {
            name: "bad".to_string(),
            init_command: None,
            panes: vec![],
            rules: vec![ModeRule {
                pattern: r"[invalid".to_string(),
                watch: false,
                interval: None,
            }],
            reactive_panes: 1,
        };

        let err = mgr.activate_mode(&config, None).unwrap_err();
        assert!(matches!(err, ModeManagerError::InvalidPattern { .. }));
    }

    #[test]
    fn handle_command_no_active_mode_errors() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock);

        let err = mgr.handle_command("anything").unwrap_err();
        assert!(matches!(err, ModeManagerError::NoActiveMode));
    }

    #[test]
    fn no_match_returns_none() {
        let mock = Arc::new(MockPaneController::new());
        let mut mgr = ModeManager::new(mock);
        mgr.activate_mode(&test_config(), None).unwrap();

        let result = mgr.handle_command("echo hello").unwrap();
        assert!(result.is_none());
    }
}
