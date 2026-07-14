use std::sync::Arc;

use regex::Regex;
use thiserror::Error;

use crate::pane::{AgentSpawnOptions, CloseTabOutcome, PaneController, PaneError};
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
    /// PRD #76 M2.15 fixup pass 2 G1 — latest known side-pane PTY dims
    /// (rows, cols). Used by the reactive-replacement spawn inside
    /// [`Self::handle_command`] so the new pane opens at the eventual
    /// layout size instead of the legacy 24×80 default. Refreshed from
    /// the caller's `mode_side_pane_dims(frame_area, side_count)` value
    /// on [`Self::activate_mode`] and on every
    /// [`Self::set_side_pane_dims`] call (the UI invokes the setter from
    /// the resize-mode-tab sweep just before routing reactive commands).
    /// Defaults to the conservative `(24, 80)` so tests that never call
    /// the setter still produce valid spawn options.
    side_pane_dims: (u16, u16),
}

impl ModeManager {
    pub fn new(pane_controller: Arc<dyn PaneController>) -> Self {
        Self {
            pane_controller,
            active_mode: None,
            cwd: None,
            side_pane_dims: (24, 80),
        }
    }

    /// PRD #76 M2.15 fixup pass 2 G1 — refresh the cached side-pane
    /// dims used by the reactive-replacement spawn in
    /// [`Self::handle_command`]. The caller is expected to compute
    /// `dims` via `mode_side_pane_dims(frame_area, side_count)` (the
    /// single layout-math SSOT in `ui.rs`), so the cached value tracks
    /// the same geometry the resize-mode-tab sweep applies.
    pub fn set_side_pane_dims(&mut self, dims: (u16, u16)) {
        self.side_pane_dims = dims;
    }

    pub fn activate_mode(
        &mut self,
        config: &ModeConfig,
        cwd: Option<&str>,
        // PRD #76 M2.15 fixup pass 2 G1 — initial side-pane PTY dims
        // for every persistent + reactive pane created in this mode.
        // The caller computes this via
        // `mode_side_pane_dims(frame_area, total_side_count)` so the
        // daemon-side PTY opens at the eventual viewport size, not the
        // legacy 24×80 default. Stored on `self.side_pane_dims` for
        // reactive-replacement spawns inside `handle_command`.
        side_pane_dims: (u16, u16),
    ) -> Result<(), ModeManagerError> {
        self.side_pane_dims = side_pane_dims;
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

                    // PRD #76 M2.15 fixup pass 2 G1 — route through
                    // `create_pane_with_options` with real side-pane dims so
                    // the daemon-side PTY opens at the viewport-derived size,
                    // not the legacy 24×80 default that the bare
                    // `create_pane` wrapper used to fall through to.
                    let (rows, cols) = side_pane_dims;
                    let display_name = pane_cfg.name.as_deref().unwrap_or(&pane_cfg.command);
                    let (pane_id, _) = self.pane_controller.create_pane_with_options(
                        None,
                        cwd,
                        AgentSpawnOptions {
                            display_name: Some(display_name),
                            tab_membership: None,
                            rows,
                            cols,
                            // Mode side panes run regular commands
                            // (htop, npm, etc.) not AI agents, so M2.13
                            // agent_type stays `None`.
                            agent_type: None,
                            // PRD #201: not a Pi orchestrator pane — no seed.
                            seed: None,
                        },
                    )?;
                    created_pane_ids.push(pane_id.clone());

                    pending.push(PendingCommand {
                        pane_id: pane_id.clone(),
                        init_command: config.init_command.clone(),
                        command: effective_cmd,
                    });

                    persistent_ids.push(pane_id);
                }

                let mut pool = ReactivePool::new();
                let (rows, cols) = side_pane_dims;
                for i in 0..config.reactive_panes {
                    let reactive_name = format!("reactive-{i}");
                    let (pane_id, _) = self.pane_controller.create_pane_with_options(
                        None,
                        cwd,
                        AgentSpawnOptions {
                            display_name: Some(&reactive_name),
                            tab_membership: None,
                            rows,
                            cols,
                            // Reactive panes are mode side panes, not
                            // agents — same M2.13 rationale.
                            agent_type: None,
                            // PRD #201: not a Pi orchestrator pane — no seed.
                            seed: None,
                        },
                    )?;
                    created_pane_ids.push(pane_id.clone());

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

    /// Phase 2: Send commands to panes. PRD #84 M4/M5: panes are spawned at
    /// their layout dims and reconciled to the exact inner area by the
    /// per-frame `resize_panes_to_layout` pass, so commands started here run at
    /// the correct PTY size without a manual post-spawn resize step.
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

    /// PRD #92 F4: tear down the active mode's persistent + reactive
    /// panes and return a [`CloseTabOutcome`] capturing per-pane close
    /// results. Pre-F4 this discarded every `close_pane` error with a
    /// silent `let _ =`, so a failed `StopAgent` RPC left the underlying
    /// agent alive in the daemon registry while the TUI thought it was
    /// gone. The outcome carries the failures back to the caller so
    /// `ui.status_message` can surface them and the matching dashboard
    /// cards can be preserved for retry.
    pub fn deactivate_mode(&mut self) -> Result<CloseTabOutcome, ModeManagerError> {
        let mode = self
            .active_mode
            .take()
            .ok_or(ModeManagerError::NoActiveMode)?;

        let mut outcome = CloseTabOutcome::default();

        // Close persistent panes
        for id in &mode.persistent_pane_ids {
            let result = self.pane_controller.close_pane(id);
            outcome.record(id.clone(), result);
        }

        // Close reactive panes
        for id in mode.reactive_pool.all_ids() {
            let result = self.pane_controller.close_pane(id);
            outcome.record(id.to_string(), result);
        }

        Ok(outcome)
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
            // PRD #76 M2.15 fixup pass 2 G1 — spawn the replacement at the
            // cached side-pane dims (refreshed by the UI from
            // `mode_side_pane_dims(frame_area, ...)` just before reactive
            // routing) so the daemon-side PTY opens at the viewport-derived
            // size, not the legacy 24×80 default.
            let (rows, cols) = self.side_pane_dims;
            // Passing `display_name: Some(command)` lets the production
            // controller forward the label to the daemon via
            // `StartAgent.display_name` (and the trait-default
            // `create_pane_with_options` calls `rename_pane` internally
            // for mocks), so no follow-up rename call is required.
            let (new_pane_id, _) = self.pane_controller.create_pane_with_options(
                Some(&pane_cmd),
                self.cwd.as_deref(),
                AgentSpawnOptions {
                    display_name: Some(command),
                    tab_membership: None,
                    rows,
                    cols,
                    // Reactive pane replacement: not an AI agent pane —
                    // same M2.13 rationale as the initial reactive
                    // spawn above.
                    agent_type: None,
                    // PRD #201: not a Pi orchestrator pane — no seed.
                    seed: None,
                },
            )?;
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
