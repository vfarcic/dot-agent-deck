use std::any::Any;
use std::sync::Arc;

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
    /// Create a pane and apply a display name in one step. The default impl
    /// composes `create_pane` + `rename_pane`, which is fine for local-PTY
    /// backends. The stream-backed `EmbeddedPaneController` overrides this
    /// so the name reaches the daemon via `StartAgent.display_name` —
    /// otherwise a disconnect or crash between `create_pane` and
    /// `rename_pane` would persist the command-based fallback name on the
    /// daemon and lose the user's chosen Name on the next reconnect
    /// (PRD #76 M2.11 reviewer P2).
    ///
    /// Returns `(pane_id, resolved_display_name)` so the caller can mirror
    /// the EXACT label the controller (and the daemon, for stream panes)
    /// stored. The resolved name is computed via
    /// [`crate::agent_pty::resolve_display_name`] — the single source of
    /// truth shared with the UI maps, preventing the divergence between
    /// `ui.pane_display_names` and `AgentRecord.display_name` that fixup-3
    /// reviewer P2 / auditor LOW called out.
    fn create_pane_with_display_name(
        &self,
        command: Option<&str>,
        cwd: Option<&str>,
        display_name: Option<&str>,
    ) -> Result<(String, String), PaneError> {
        let resolved = crate::agent_pty::resolve_display_name(display_name, command);
        let id = self.create_pane(command, cwd)?;
        self.rename_pane(&id, &resolved)?;
        Ok((id, resolved))
    }
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
    fn as_any(&self) -> &dyn Any;
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub fn detect_multiplexer() -> Arc<dyn PaneController> {
    Arc::new(crate::embedded_pane::EmbeddedPaneController::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_multiplexer_returns_embedded() {
        let ctrl = detect_multiplexer();
        assert_eq!(ctrl.name(), "embedded");
        assert!(ctrl.is_available());
    }
}
