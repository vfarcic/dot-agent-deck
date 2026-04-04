use std::any::Any;

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

pub fn detect_multiplexer() -> Box<dyn PaneController> {
    Box::new(crate::embedded_pane::EmbeddedPaneController::new())
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
