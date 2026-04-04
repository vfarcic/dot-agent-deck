use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use std::any::Any;

use crate::pane::{PaneController, PaneDirection, PaneError, PaneInfo};

/// State for a single embedded terminal pane.
struct Pane {
    /// Writer to send input to the PTY.
    writer: Box<dyn std::io::Write + Send>,
    /// Parsed terminal screen (vt100).
    screen: Arc<Mutex<vt100::Parser>>,
    /// The child process handle.
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Master PTY handle (kept alive for resize).
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Display name for this pane.
    name: String,
    /// Whether this pane is currently focused.
    is_focused: bool,
    /// The command that was used to create this pane.
    command: Option<String>,
}

/// Thread-safe pane registry.
type PaneRegistry = Arc<Mutex<HashMap<String, Pane>>>;

/// Embedded terminal pane controller using portable-pty + vt100.
///
/// Replaces `ZellijController` by spawning PTY processes directly and parsing
/// their output with a VT100 terminal emulator.
pub struct EmbeddedPaneController {
    panes: PaneRegistry,
    next_id: Arc<Mutex<u64>>,
}

impl Default for EmbeddedPaneController {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddedPaneController {
    pub fn new() -> Self {
        Self {
            panes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Access the vt100 screen for a pane (used by the terminal widget for rendering).
    pub fn get_screen(&self, pane_id: &str) -> Option<Arc<Mutex<vt100::Parser>>> {
        let panes = self.panes.lock().unwrap();
        panes.get(pane_id).map(|p| Arc::clone(&p.screen))
    }

    /// Return all pane IDs in insertion order (by numeric ID).
    pub fn pane_ids(&self) -> Vec<String> {
        let panes = self.panes.lock().unwrap();
        let mut ids: Vec<String> = panes.keys().cloned().collect();
        ids.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
        ids
    }

    /// Get the currently focused pane ID, if any.
    pub fn focused_pane_id(&self) -> Option<String> {
        let panes = self.panes.lock().unwrap();
        panes
            .iter()
            .find(|(_, p)| p.is_focused)
            .map(|(id, _)| id.clone())
    }

    /// Write raw bytes directly to a pane's PTY stdin without appending CR.
    /// Used for interactive keyboard input forwarding.
    pub fn write_raw_bytes(&self, pane_id: &str, bytes: &[u8]) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            pane.writer.write_all(bytes).map_err(PaneError::Io)?;
            pane.writer.flush().map_err(PaneError::Io)?;
            Ok(())
        } else {
            Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )))
        }
    }

    /// Scroll a pane's view by `delta` lines (positive = scroll up into history).
    /// vt100 0.16 clamps the offset to the actual scrollback buffer size.
    pub fn scroll_pane(&self, pane_id: &str, delta: isize) {
        let panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get(pane_id)
            && let Ok(mut parser) = pane.screen.lock()
        {
            let current = parser.screen().scrollback();
            let new_offset = if delta > 0 {
                current.saturating_add(delta as usize)
            } else {
                current.saturating_sub((-delta) as usize)
            };
            parser.screen_mut().set_scrollback(new_offset);
        }
    }

    /// Reset a pane's scrollback offset to 0 (show latest output).
    pub fn reset_scrollback(&self, pane_id: &str) {
        let panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get(pane_id)
            && let Ok(mut parser) = pane.screen.lock()
        {
            parser.screen_mut().set_scrollback(0);
        }
    }

    /// Resize a pane's PTY and VT100 parser to the given dimensions.
    pub fn resize_pane_pty(&self, pane_id: &str, rows: u16, cols: u16) -> Result<(), PaneError> {
        let panes = self.panes.lock().unwrap();
        let pane = panes
            .get(pane_id)
            .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
        pane.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PaneError::CommandFailed(format!("PTY resize failed: {e}")))?;
        if let Ok(mut parser) = pane.screen.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    fn allocate_id(&self) -> String {
        let mut id = self.next_id.lock().unwrap();
        let current = *id;
        *id += 1;
        current.to_string()
    }
}

impl PaneController for EmbeddedPaneController {
    fn focus_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if !panes.contains_key(pane_id) {
            return Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )));
        }
        for (id, pane) in panes.iter_mut() {
            pane.is_focused = id == pane_id;
        }
        Ok(())
    }

    fn create_pane(&self, command: Option<&str>, cwd: Option<&str>) -> Result<String, PaneError> {
        let pty_system = NativePtySystem::default();

        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PaneError::CommandFailed(format!("Failed to open PTY: {e}")))?;

        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

        let mut cmd = match command {
            Some(c) if c.contains(' ') => {
                let mut cmd = CommandBuilder::new(&default_shell);
                cmd.arg("-c");
                cmd.arg(c);
                cmd
            }
            Some(c) => CommandBuilder::new(c),
            None => CommandBuilder::new(&default_shell),
        };

        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }

        let pane_id = self.allocate_id();
        // Tag the spawned process so hooks can identify which pane it belongs to.
        cmd.env("DOT_AGENT_DECK_PANE_ID", &pane_id);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PaneError::CommandFailed(format!("Failed to spawn command: {e}")))?;

        // Drop the slave — we interact through the master side only.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PaneError::CommandFailed(format!("Failed to get PTY writer: {e}")))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PaneError::CommandFailed(format!("Failed to get PTY reader: {e}")))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 10_000)));

        // Spawn a background thread to read PTY output and feed it to the vt100 parser.
        let parser_clone = Arc::clone(&parser);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser_clone.lock() {
                            p.process(&buf[..n]);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let pane = Pane {
            writer,
            screen: parser,
            child,
            master: pair.master,
            name: command.unwrap_or("shell").to_string(),
            is_focused: false,
            command: command.map(|c| c.to_string()),
        };

        self.panes.lock().unwrap().insert(pane_id.clone(), pane);

        Ok(pane_id)
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let mut pane = {
            let mut panes = self.panes.lock().unwrap();
            match panes.remove(pane_id) {
                Some(p) => p,
                None => {
                    return Err(PaneError::CommandFailed(format!(
                        "Pane {pane_id} not found"
                    )));
                }
            }
        };
        // Kill the child process and wait for it to exit after releasing the
        // lock so we don't hold the mutex during blocking I/O.
        let _ = pane.child.kill();
        let _ = pane.child.wait();
        Ok(())
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError> {
        let panes = self.panes.lock().unwrap();
        let mut list: Vec<(u64, PaneInfo)> = panes
            .iter()
            .map(|(id, p)| {
                (
                    id.parse::<u64>().unwrap_or(0),
                    PaneInfo {
                        pane_id: id.clone(),
                        title: p.name.clone(),
                        is_focused: p.is_focused,
                        command: p.command.clone(),
                    },
                )
            })
            .collect();
        list.sort_by_key(|(num, _)| *num);
        Ok(list.into_iter().map(|(_, info)| info).collect())
    }

    fn resize_pane(
        &self,
        _pane_id: &str,
        _direction: PaneDirection,
        _amount: u16,
    ) -> Result<(), PaneError> {
        // Resize is handled by the layout engine in future milestones.
        // For now, this is a no-op.
        Ok(())
    }

    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            pane.name = name.to_string();
            Ok(())
        } else {
            Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )))
        }
    }

    fn toggle_layout(&self) -> Result<(), PaneError> {
        // Layout toggling will be implemented in the layout engine milestone.
        Ok(())
    }

    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            pane.writer
                .write_all(text.as_bytes())
                .map_err(PaneError::Io)?;
            // Send CR (Enter)
            pane.writer.write_all(b"\r").map_err(PaneError::Io)?;
            pane.writer.flush().map_err(PaneError::Io)?;
            Ok(())
        } else {
            Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )))
        }
    }

    fn name(&self) -> &str {
        "embedded"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_list_panes() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.list_panes().unwrap().is_empty());

        let id = ctrl.create_pane(None, None).unwrap();
        assert!(!id.is_empty());

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, id);

        ctrl.close_pane(&id).unwrap();
        assert!(ctrl.list_panes().unwrap().is_empty());
    }

    #[test]
    fn focus_pane_updates_state() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();

        ctrl.focus_pane(&id1).unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert!(panes.iter().find(|p| p.pane_id == id1).unwrap().is_focused);
        assert!(!panes.iter().find(|p| p.pane_id == id2).unwrap().is_focused);

        ctrl.focus_pane(&id2).unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert!(!panes.iter().find(|p| p.pane_id == id1).unwrap().is_focused);
        assert!(panes.iter().find(|p| p.pane_id == id2).unwrap().is_focused);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
    }

    #[test]
    fn rename_pane_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        ctrl.rename_pane(&id, "my-agent").unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "my-agent");

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn close_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.close_pane("999").is_err());
    }

    #[test]
    fn focus_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.focus_pane("999").is_err());
    }

    #[test]
    fn write_to_pane_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        // Should not error — just sends bytes to PTY stdin
        ctrl.write_to_pane(&id, "echo hello").unwrap();

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn controller_metadata() {
        let ctrl = EmbeddedPaneController::new();
        assert_eq!(ctrl.name(), "embedded");
        assert!(ctrl.is_available());
    }

    #[test]
    fn screen_access_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(Some("echo hello"), None).unwrap();

        // Give the PTY a moment to produce output
        std::thread::sleep(std::time::Duration::from_millis(200));

        let screen = ctrl.get_screen(&id).expect("screen should exist");
        let parser = screen.lock().unwrap();
        let contents = parser.screen().contents();
        // The screen should have some content (at minimum the echoed text or shell prompt)
        assert!(!contents.trim().is_empty());

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn pane_ids_are_sequential() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();
        let id3 = ctrl.create_pane(None, None).unwrap();

        let n1: u64 = id1.parse().unwrap();
        let n2: u64 = id2.parse().unwrap();
        let n3: u64 = id3.parse().unwrap();
        assert_eq!(n2, n1 + 1);
        assert_eq!(n3, n2 + 1);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
        ctrl.close_pane(&id3).unwrap();
    }

    #[test]
    fn pane_ids_sorted_in_list() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();
        let id3 = ctrl.create_pane(None, None).unwrap();

        let ids = ctrl.pane_ids();
        assert_eq!(ids, vec![id1.clone(), id2.clone(), id3.clone()]);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
        ctrl.close_pane(&id3).unwrap();
    }

    #[test]
    fn focused_pane_id_tracks_focus() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.focused_pane_id().is_none());

        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();

        ctrl.focus_pane(&id1).unwrap();
        assert_eq!(ctrl.focused_pane_id().as_deref(), Some(id1.as_str()));

        ctrl.focus_pane(&id2).unwrap();
        assert_eq!(ctrl.focused_pane_id().as_deref(), Some(id2.as_str()));

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
    }

    #[test]
    fn write_raw_bytes_no_cr_appended() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        // write_raw_bytes should succeed without error
        ctrl.write_raw_bytes(&id, b"hello").unwrap();

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn write_raw_bytes_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.write_raw_bytes("999", b"hello").is_err());
    }

    #[test]
    fn rename_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.rename_pane("999", "name").is_err());
    }

    #[test]
    fn create_pane_with_command() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(Some("echo test"), None).unwrap();

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "echo test");
        assert_eq!(panes[0].command.as_deref(), Some("echo test"));

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn create_pane_default_name_is_shell() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "shell");
        assert!(panes[0].command.is_none());

        ctrl.close_pane(&id).unwrap();
    }
}
