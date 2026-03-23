use std::process::Command;

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
    fn create_pane(&self, command: Option<&str>) -> Result<String, PaneError>;
    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError>;
    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError>;
    fn resize_pane(
        &self,
        pane_id: &str,
        direction: PaneDirection,
        amount: u16,
    ) -> Result<(), PaneError>;
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
        self.run_zellij(&["action", "focus-terminal-pane", "--pane-id", pane_id])?;
        Ok(())
    }

    fn create_pane(&self, command: Option<&str>) -> Result<String, PaneError> {
        let before = self.list_panes()?;

        match command {
            Some(cmd) => self.run_zellij(&["action", "new-pane", "--", cmd])?,
            None => self.run_zellij(&["action", "new-pane"])?,
        };

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
        let output = self.run_zellij(&["action", "list-panes"])?;
        parse_list_panes(&output)
    }

    fn resize_pane(
        &self,
        pane_id: &str,
        direction: PaneDirection,
        _amount: u16,
    ) -> Result<(), PaneError> {
        self.run_zellij(&[
            "action",
            "resize",
            direction.as_str(),
            "--pane-id",
            pane_id,
        ])?;
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
    fn create_pane(&self, _: Option<&str>) -> Result<String, PaneError> {
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

/// Parse `zellij action list-panes` output.
///
/// Each line is tab-separated with fields:
/// `tab_name\tpane_id\tpane_title\tis_focused\tcommand\t...`
///
/// We extract the fields we need and skip unparseable lines.
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
    fn parse_list_panes_typical_output() {
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
    fn parse_list_panes_empty_output() {
        let panes = parse_list_panes("").unwrap();
        assert!(panes.is_empty());
    }

    #[test]
    fn parse_list_panes_skips_short_lines() {
        let output = "incomplete\tdata\n\
                       Tab #1\t42\tshell\ttrue\tbash\n";
        let panes = parse_list_panes(output).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "42");
    }

    #[test]
    fn parse_list_panes_no_command_field() {
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
        assert!(ctrl.create_pane(None).is_err());
        assert!(ctrl.close_pane("1").is_err());
        assert!(ctrl.list_panes().is_err());
        assert!(ctrl
            .resize_pane("1", PaneDirection::Up, 1)
            .is_err());
    }

    #[test]
    fn detect_multiplexer_without_zellij() {
        // In test environment, ZELLIJ is not set
        let ctrl = detect_multiplexer();
        assert_eq!(ctrl.name(), "none");
        assert!(!ctrl.is_available());
    }
}
