use std::process::Command;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PaneError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Command failed: {0}")]
    CommandFailed(String),
    #[error("Parse error: {0}")]
    ParseError(String),
    #[error("Pane control not available")]
    NotAvailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneDirection {
    Up,
    Down,
    Left,
    Right,
}

impl PaneDirection {
    fn as_str(&self) -> &'static str {
        match self {
            PaneDirection::Up => "up",
            PaneDirection::Down => "down",
            PaneDirection::Left => "left",
            PaneDirection::Right => "right",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub title: String,
    pub is_focused: bool,
    pub command: Option<String>,
}

pub trait PaneController: Send + Sync {
    fn focus_pane(&self, pane_id: &str) -> Result<(), PaneError>;
    fn create_pane(&self, command: Option<&str>, cwd: Option<&str>) -> Result<String, PaneError>;
    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError>;
    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError>;
    fn resize_pane(
        &self,
        pane_id: &str,
        direction: PaneDirection,
        amount: u16,
    ) -> Result<(), PaneError>;
    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<(), PaneError>;
    fn toggle_layout(&self) -> Result<(), PaneError>;
    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError>;
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
}

// ---------------------------------------------------------------------------
// Zellij implementation
// ---------------------------------------------------------------------------

pub struct ZellijController {
    zellij_bin: String,
}

impl Default for ZellijController {
    fn default() -> Self {
        Self {
            zellij_bin: "zellij".to_string(),
        }
    }
}

impl ZellijController {
    pub fn new() -> Self {
        Self::default()
    }

    fn run_zellij(&self, args: &[&str]) -> Result<String, PaneError> {
        let output = Command::new(&self.zellij_bin).args(args).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PaneError::CommandFailed(stderr.to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

impl PaneController for ZellijController {
    fn focus_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        // Zellij has no direct "focus pane by ID" command.
        // Cycle through panes with focus-next-pane until the target is focused.
        let panes = self.list_panes()?;
        let total = panes.len();
        if total == 0 {
            return Err(PaneError::CommandFailed("No panes found".into()));
        }

        // Check if already focused
        if panes.iter().any(|p| p.pane_id == pane_id && p.is_focused) {
            return Ok(());
        }

        // Cycle through panes (at most total times to avoid infinite loop)
        for _ in 0..total {
            self.run_zellij(&["action", "focus-next-pane"])?;
            let current = self.list_panes()?;
            if current.iter().any(|p| p.pane_id == pane_id && p.is_focused) {
                return Ok(());
            }
        }

        Err(PaneError::CommandFailed(format!(
            "Pane {pane_id} not found after cycling all panes"
        )))
    }

    // Note: Zellij's `new-pane` doesn't return the created pane ID, so we diff
    // list_panes() before/after. This has a theoretical race if another pane is
    // created concurrently, but in practice the dashboard is the only pane creator.
    fn create_pane(&self, command: Option<&str>, cwd: Option<&str>) -> Result<String, PaneError> {
        let before = self.list_panes()?;

        // Build args for new-pane. The swap layout automatically stacks panes in the right column.
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut args = vec!["action", "new-pane"];
        let cwd_val;
        if let Some(dir) = cwd {
            args.push("--cwd");
            cwd_val = dir.to_string();
            args.push(&cwd_val);
        }
        match command {
            Some(cmd) if cmd.contains(' ') => {
                args.push("--");
                args.push(&default_shell);
                args.push("-c");
                args.push(cmd);
            }
            Some(cmd) => {
                args.push("--");
                args.push(cmd);
            }
            None => {
                args.push("--");
                args.push(&default_shell);
            }
        }
        self.run_zellij(&args)?;

        let after = self.list_panes()?;
        let before_ids: std::collections::HashSet<&str> =
            before.iter().map(|p| p.pane_id.as_str()).collect();
        let new_pane = after
            .iter()
            .find(|p| !before_ids.contains(p.pane_id.as_str()));

        match new_pane {
            Some(p) => Ok(p.pane_id.clone()),
            None => Err(PaneError::ParseError(
                "Could not determine new pane ID".to_string(),
            )),
        }
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        self.run_zellij(&["action", "close-pane", "--pane-id", pane_id])?;
        Ok(())
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError> {
        let output =
            self.run_zellij(&["action", "list-panes", "--state", "--command", "--json"])?;
        parse_list_panes_json(&output)
    }

    fn resize_pane(
        &self,
        pane_id: &str,
        direction: PaneDirection,
        _amount: u16,
    ) -> Result<(), PaneError> {
        self.run_zellij(&["action", "resize", direction.as_str(), "--pane-id", pane_id])?;
        Ok(())
    }

    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<(), PaneError> {
        self.run_zellij(&["action", "rename-pane", name, "--pane-id", pane_id])?;
        Ok(())
    }

    fn toggle_layout(&self) -> Result<(), PaneError> {
        self.run_zellij(&["action", "next-swap-layout"])?;
        Ok(())
    }

    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError> {
        let dashboard_pane = std::env::var("ZELLIJ_PANE_ID").ok();
        self.focus_pane(pane_id)?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        // Send each character as raw bytes — reliable for TUI apps in raw mode
        for byte in text.bytes() {
            self.run_zellij(&["action", "write", &byte.to_string()])?;
        }
        // Send CR (byte 13) — what terminals send for Enter
        self.run_zellij(&["action", "write", "13"])?;
        if let Some(ref dp) = dashboard_pane {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let _ = self.focus_pane(dp);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "zellij"
    }

    fn is_available(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Noop implementation (no multiplexer detected)
// ---------------------------------------------------------------------------

pub struct NoopController;

impl PaneController for NoopController {
    fn focus_pane(&self, _: &str) -> Result<(), PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn create_pane(&self, _: Option<&str>, _: Option<&str>) -> Result<String, PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn close_pane(&self, _: &str) -> Result<(), PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn resize_pane(&self, _: &str, _: PaneDirection, _: u16) -> Result<(), PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn rename_pane(&self, _: &str, _: &str) -> Result<(), PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn toggle_layout(&self) -> Result<(), PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn write_to_pane(&self, _: &str, _: &str) -> Result<(), PaneError> {
        Err(PaneError::NotAvailable)
    }
    fn name(&self) -> &str {
        "none"
    }
    fn is_available(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub fn detect_multiplexer() -> Box<dyn PaneController> {
    if std::env::var("ZELLIJ").is_ok() {
        Box::new(ZellijController::new())
    } else {
        Box::new(NoopController)
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// JSON structure returned by `zellij action list-panes --json --state --command`.
#[derive(Debug, Deserialize)]
struct ZellijPane {
    id: u32,
    title: Option<String>,
    #[serde(default)]
    is_focused: bool,
    #[serde(default)]
    command: Option<String>,
}

/// Parse JSON output from `zellij action list-panes --json --state --command`.
fn parse_list_panes_json(output: &str) -> Result<Vec<PaneInfo>, PaneError> {
    let raw: Vec<ZellijPane> = serde_json::from_str(output)
        .map_err(|e| PaneError::ParseError(format!("JSON parse error: {e}\nOutput: {output}")))?;
    Ok(raw
        .into_iter()
        .map(|p| PaneInfo {
            pane_id: p.id.to_string(),
            title: p.title.unwrap_or_default(),
            is_focused: p.is_focused,
            command: p.command,
        })
        .collect())
}

/// Parse tab-separated `zellij action list-panes` output (legacy fallback).
#[cfg(test)]
fn parse_list_panes(output: &str) -> Result<Vec<PaneInfo>, PaneError> {
    let mut panes = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            continue;
        }
        panes.push(PaneInfo {
            pane_id: fields[1].to_string(),
            title: fields[2].to_string(),
            is_focused: fields[3] == "true",
            command: fields.get(4).and_then(|s| {
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            }),
        });
    }
    Ok(panes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_panes_json_typical() {
        let json = r#"[
            {"id": 42, "title": "my-shell", "is_focused": true, "command": "bash"},
            {"id": 43, "title": "editor", "is_focused": false, "command": "vim"}
        ]"#;
        let panes = parse_list_panes_json(json).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "42");
        assert_eq!(panes[0].title, "my-shell");
        assert!(panes[0].is_focused);
        assert_eq!(panes[0].command.as_deref(), Some("bash"));
        assert_eq!(panes[1].pane_id, "43");
        assert!(!panes[1].is_focused);
    }

    #[test]
    fn parse_list_panes_json_empty() {
        let panes = parse_list_panes_json("[]").unwrap();
        assert!(panes.is_empty());
    }

    #[test]
    fn parse_list_panes_json_minimal_fields() {
        let json = r#"[{"id": 1}]"#;
        let panes = parse_list_panes_json(json).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "1");
        assert!(!panes[0].is_focused);
        assert!(panes[0].command.is_none());
    }

    #[test]
    fn parse_list_panes_tsv_typical_output() {
        let output = "Tab #1\t42\tmy-shell\ttrue\tbash\n\
                       Tab #1\t43\teditor\tfalse\tvim\n";
        let panes = parse_list_panes(output).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "42");
        assert_eq!(panes[0].title, "my-shell");
        assert!(panes[0].is_focused);
        assert_eq!(panes[0].command.as_deref(), Some("bash"));
        assert_eq!(panes[1].pane_id, "43");
        assert!(!panes[1].is_focused);
    }

    #[test]
    fn parse_list_panes_tsv_empty_output() {
        let panes = parse_list_panes("").unwrap();
        assert!(panes.is_empty());
    }

    #[test]
    fn parse_list_panes_tsv_skips_short_lines() {
        let output = "incomplete\tdata\n\
                       Tab #1\t42\tshell\ttrue\tbash\n";
        let panes = parse_list_panes(output).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "42");
    }

    #[test]
    fn parse_list_panes_tsv_no_command_field() {
        let output = "Tab #1\t42\tshell\tfalse\n";
        let panes = parse_list_panes(output).unwrap();
        assert_eq!(panes.len(), 1);
        assert!(panes[0].command.is_none());
    }

    #[test]
    fn noop_controller_returns_not_available() {
        let ctrl = NoopController;
        assert!(!ctrl.is_available());
        assert_eq!(ctrl.name(), "none");
        assert!(ctrl.focus_pane("1").is_err());
        assert!(ctrl.create_pane(None, None).is_err());
        assert!(ctrl.close_pane("1").is_err());
        assert!(ctrl.list_panes().is_err());
        assert!(ctrl.resize_pane("1", PaneDirection::Up, 1).is_err());
        assert!(ctrl.toggle_layout().is_err());
    }

    #[test]
    fn detect_multiplexer_without_zellij() {
        // Temporarily remove ZELLIJ so the detector falls back to NoopController
        let prev = std::env::var("ZELLIJ").ok();
        // SAFETY: test is single-threaded for this env var; restored immediately after.
        unsafe { std::env::remove_var("ZELLIJ") };
        let ctrl = detect_multiplexer();
        if let Some(val) = prev {
            unsafe { std::env::set_var("ZELLIJ", val) };
        }
        assert_eq!(ctrl.name(), "none");
        assert!(!ctrl.is_available());
    }
}
