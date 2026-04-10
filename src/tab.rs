use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::event::EventType;
use crate::mode_manager::{ModeManager, ModeManagerError};
use crate::pane::PaneController;
use crate::project_config::ModeConfig;
use crate::state::SessionState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub type TabId = u32;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TabError {
    #[error("Cannot close the dashboard tab")]
    CannotCloseDashboard,
    #[error("Tab index {0} out of bounds")]
    IndexOutOfBounds(usize),
    #[error("Mode error: {0}")]
    ModeManager(#[from] ModeManagerError),
}

// ---------------------------------------------------------------------------
// Tab enum
// ---------------------------------------------------------------------------

pub enum Tab {
    Dashboard,
    Mode {
        id: TabId,
        name: String,
        agent_pane_id: String,
        mode_manager: Box<ModeManager>,
        last_routed_timestamp: HashMap<String, DateTime<Utc>>,
        cwd: String,
        /// Which side pane has visual focus in Normal mode. `None` = agent pane.
        focused_side_pane_index: Option<usize>,
    },
}

impl Tab {
    fn label(&self) -> &str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Mode { name, .. } => name,
        }
    }
}

// ---------------------------------------------------------------------------
// TabManager
// ---------------------------------------------------------------------------

pub struct TabManager {
    tabs: Vec<Tab>,
    active_index: usize,
    next_id: TabId,
    pane_controller: Arc<dyn PaneController>,
}

impl TabManager {
    pub fn new(pane_controller: Arc<dyn PaneController>) -> Self {
        Self {
            tabs: vec![Tab::Dashboard],
            active_index: 0,
            next_id: 1,
            pane_controller,
        }
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_index]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_index]
    }

    pub fn switch_to(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active_index = index;
            true
        } else {
            false
        }
    }

    pub fn show_tab_bar(&self) -> bool {
        self.tabs.len() > 1
    }

    pub fn tab_labels(&self) -> Vec<String> {
        self.tabs.iter().map(|t| t.label().to_string()).collect()
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Open a new mode tab. Returns `(tab_index, managed_pane_ids)`.
    pub fn open_mode_tab(
        &mut self,
        config: &ModeConfig,
        cwd: &str,
        agent_pane_id: String,
    ) -> Result<(usize, Vec<String>), TabError> {
        let mut mode_manager = ModeManager::new(Arc::clone(&self.pane_controller));
        mode_manager.activate_mode(config, Some(cwd))?;
        let pane_ids = mode_manager.managed_pane_ids();

        let id = self.next_id;
        self.next_id += 1;

        self.tabs.push(Tab::Mode {
            id,
            name: config.name.clone(),
            agent_pane_id,
            mode_manager: Box::new(mode_manager),
            last_routed_timestamp: HashMap::new(),
            cwd: cwd.to_string(),
            focused_side_pane_index: None,
        });

        let index = self.tabs.len() - 1;
        self.active_index = index;

        Ok((index, pane_ids))
    }

    /// Send pending commands to the active mode tab's panes.
    /// Must be called after panes are resized to correct display dimensions.
    pub fn start_mode_commands(&mut self) -> Result<(), TabError> {
        if let Some(Tab::Mode { mode_manager, .. }) = self.tabs.get_mut(self.active_index) {
            mode_manager
                .start_mode_commands()
                .map_err(TabError::ModeManager)?;
        }
        Ok(())
    }

    /// Close a mode tab by index. Returns the pane IDs that were managed.
    pub fn close_tab(&mut self, index: usize) -> Result<Vec<String>, TabError> {
        if index == 0 {
            return Err(TabError::CannotCloseDashboard);
        }
        if index >= self.tabs.len() {
            return Err(TabError::IndexOutOfBounds(index));
        }

        let tab = self.tabs.remove(index);
        let pane_ids = match tab {
            Tab::Mode {
                mut mode_manager, ..
            } => {
                let ids = mode_manager.managed_pane_ids();
                let _ = mode_manager.deactivate_mode();
                ids
            }
            Tab::Dashboard => Vec::new(),
        };

        // Adjust active_index after removal.
        if self.active_index >= self.tabs.len() {
            self.active_index = self.tabs.len() - 1;
        } else if self.active_index > index {
            self.active_index -= 1;
        } else if self.active_index == index {
            // Closed the active tab — fall back to dashboard.
            self.active_index = 0;
        }

        Ok(pane_ids)
    }

    /// Collect all managed pane IDs across all mode tabs.
    /// Returns side pane IDs managed by mode tabs (excludes agent panes,
    /// which should still render on the dashboard).
    pub fn all_managed_pane_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for tab in &self.tabs {
            if let Tab::Mode { mode_manager, .. } = tab {
                ids.extend(mode_manager.managed_pane_ids());
            }
        }
        ids
    }

    /// Find which tab index owns a given pane ID.
    pub fn tab_index_for_pane(&self, pane_id: &str) -> Option<usize> {
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Tab::Mode { mode_manager, .. } = tab
                && mode_manager
                    .managed_pane_ids()
                    .contains(&pane_id.to_string())
            {
                return Some(i);
            }
        }
        None
    }

    /// Get the active mode name (None if Dashboard is active).
    pub fn active_mode_name(&self) -> Option<&str> {
        match &self.tabs[self.active_index] {
            Tab::Dashboard => None,
            Tab::Mode { name, .. } => Some(name),
        }
    }

    /// Route reactive commands to all active mode tabs.
    /// Each tab only receives commands from its own agent session (scoped by agent_pane_id).
    /// Returns pairs of (closed_pane_id, new_pane_id) for panes that were recreated.
    pub fn route_reactive_commands(
        &mut self,
        sessions: &HashMap<String, SessionState>,
    ) -> Vec<(String, String)> {
        let mut pane_changes = Vec::new();
        for tab in &mut self.tabs {
            if let Tab::Mode {
                mode_manager,
                last_routed_timestamp,
                name,
                agent_pane_id,
                ..
            } = tab
            {
                // Only route commands from this tab's own agent session.
                let scoped: HashMap<String, SessionState> = sessions
                    .iter()
                    .filter(|(_, s)| s.pane_id.as_deref() == Some(agent_pane_id.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let new_commands = extract_new_bash_commands(&scoped, last_routed_timestamp);
                for cmd in &new_commands {
                    tracing::info!("Routing command to tab '{name}': {cmd}");
                    match mode_manager.handle_command(cmd) {
                        Ok(Some(change)) => {
                            if let (Some(old_id), Some(new_id)) = (change.closed, change.created) {
                                pane_changes.push((old_id, new_id));
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!("Reactive pane routing error in tab '{name}': {e}");
                        }
                    }
                }
            }
        }
        pane_changes
    }
}

// ---------------------------------------------------------------------------
// Reactive command extraction (moved from ui.rs)
// ---------------------------------------------------------------------------

/// Scans sessions for new Bash commands that have not been routed yet.
pub(crate) fn extract_new_bash_commands(
    sessions: &HashMap<String, SessionState>,
    last_routed: &mut HashMap<String, DateTime<Utc>>,
) -> Vec<String> {
    let mut commands = Vec::new();
    for (sid, session) in sessions {
        let cutoff = last_routed.get(sid).copied();
        for event in session.recent_events.iter() {
            if cutoff.is_some_and(|ts| event.timestamp <= ts) {
                continue;
            }
            if event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
                && let Some(cmd) = event.metadata.get("bash_command")
            {
                commands.push(cmd.clone());
            }
        }
        if let Some(last) = session.recent_events.back() {
            last_routed.insert(sid.clone(), last.timestamp);
        }
    }
    last_routed.retain(|sid, _| sessions.contains_key(sid));
    commands
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{PaneDirection, PaneError, PaneInfo};
    use crate::project_config::{ModePersistentPane, ModeRule};
    use std::any::Any;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::event::AgentEvent;
    use crate::state::SessionStatus;

    // -- Mock --

    struct MockPaneController {
        next_id: Mutex<u64>,
        closed: Mutex<Vec<String>>,
    }

    impl MockPaneController {
        fn new() -> Self {
            Self {
                next_id: Mutex::new(1),
                closed: Mutex::new(Vec::new()),
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

        fn write_to_pane(&self, _pane_id: &str, _text: &str) -> Result<(), PaneError> {
            Ok(())
        }

        fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
            self.closed.lock().unwrap().push(pane_id.to_string());
            Ok(())
        }

        fn rename_pane(&self, _pane_id: &str, _name: &str) -> Result<(), PaneError> {
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

    fn test_config(name: &str) -> ModeConfig {
        ModeConfig {
            name: name.to_string(),
            init_command: None,
            panes: vec![ModePersistentPane {
                command: "kubectl get pods -w".to_string(),
                name: Some("Pods".to_string()),
                watch: false,
            }],
            rules: vec![ModeRule {
                pattern: r"kubectl\s+describe".to_string(),
                watch: false,
                interval: None,
            }],
            reactive_panes: 3,
        }
    }

    fn make_manager() -> TabManager {
        let mock = Arc::new(MockPaneController::new());
        TabManager::new(mock)
    }

    // -- Tests --

    #[test]
    fn new_starts_with_dashboard() {
        let tm = make_manager();
        assert_eq!(tm.tab_count(), 1);
        assert_eq!(tm.active_index(), 0);
        assert!(!tm.show_tab_bar());
        assert_eq!(tm.tab_labels(), vec!["Dashboard"]);
        assert!(tm.active_mode_name().is_none());
    }

    #[test]
    fn open_mode_tab_creates_tab() {
        let mut tm = make_manager();
        let (idx, ids) = tm
            .open_mode_tab(&test_config("k8s-ops"), "/tmp", String::new())
            .unwrap();
        assert_eq!(idx, 1);
        assert!(!ids.is_empty());
        assert_eq!(tm.tab_count(), 2);
        assert_eq!(tm.active_index(), 1);
        assert!(tm.show_tab_bar());
        assert_eq!(tm.active_mode_name(), Some("k8s-ops"));
        assert_eq!(tm.tab_labels(), vec!["Dashboard", "k8s-ops"]);
    }

    #[test]
    fn open_multiple_mode_tabs() {
        let mut tm = make_manager();
        let (_, ids1) = tm
            .open_mode_tab(&test_config("k8s"), "/tmp/a", String::new())
            .unwrap();
        let (_, ids2) = tm
            .open_mode_tab(&test_config("rust-tdd"), "/tmp/b", String::new())
            .unwrap();
        assert_eq!(tm.tab_count(), 3);
        assert_eq!(tm.active_index(), 2);
        // Each tab has its own panes — IDs should not overlap.
        for id in &ids1 {
            assert!(!ids2.contains(id));
        }
        assert_eq!(tm.tab_labels(), vec!["Dashboard", "k8s", "rust-tdd"]);
    }

    #[test]
    fn close_mode_tab() {
        let mock = Arc::new(MockPaneController::new());
        let mut tm = TabManager::new(mock.clone());
        let (_, ids) = tm
            .open_mode_tab(&test_config("k8s"), "/tmp", String::new())
            .unwrap();
        assert_eq!(tm.tab_count(), 2);

        let closed_ids = tm.close_tab(1).unwrap();
        assert_eq!(closed_ids, ids);
        assert_eq!(tm.tab_count(), 1);
        assert_eq!(tm.active_index(), 0);
        // Verify panes were closed via the mock.
        let closed = mock.closed.lock().unwrap();
        assert!(!closed.is_empty());
    }

    #[test]
    fn cannot_close_dashboard() {
        let mut tm = make_manager();
        let err = tm.close_tab(0).unwrap_err();
        assert!(matches!(err, TabError::CannotCloseDashboard));
    }

    #[test]
    fn switch_to_bounds() {
        let mut tm = make_manager();
        assert!(tm.switch_to(0));
        assert!(!tm.switch_to(1));
        assert!(!tm.switch_to(99));
    }

    #[test]
    fn tab_index_for_pane_lookup() {
        let mut tm = make_manager();
        let (_, ids) = tm
            .open_mode_tab(&test_config("k8s"), "/tmp", String::new())
            .unwrap();
        for id in &ids {
            assert_eq!(tm.tab_index_for_pane(id), Some(1));
        }
        assert_eq!(tm.tab_index_for_pane("nonexistent"), None);
    }

    #[test]
    fn active_mode_name_per_tab() {
        let mut tm = make_manager();
        assert!(tm.active_mode_name().is_none());

        tm.open_mode_tab(&test_config("k8s"), "/tmp", String::new())
            .unwrap();
        assert_eq!(tm.active_mode_name(), Some("k8s"));

        tm.switch_to(0);
        assert!(tm.active_mode_name().is_none());
    }

    #[test]
    fn close_adjusts_active_index() {
        let mut tm = make_manager();
        tm.open_mode_tab(&test_config("a"), "/tmp/a", String::new())
            .unwrap();
        tm.open_mode_tab(&test_config("b"), "/tmp/b", String::new())
            .unwrap();
        tm.open_mode_tab(&test_config("c"), "/tmp/c", String::new())
            .unwrap();
        // tabs: [Dashboard, a, b, c], active = 3 (c)

        // Switch to tab "b" (index 2).
        tm.switch_to(2);
        assert_eq!(tm.active_index(), 2);

        // Close tab "a" (index 1) — active was at 2, shifts to 1.
        tm.close_tab(1).unwrap();
        assert_eq!(tm.active_index(), 1);
        assert_eq!(tm.active_mode_name(), Some("b"));
    }

    #[test]
    fn close_active_tab_falls_back_to_dashboard() {
        let mut tm = make_manager();
        tm.open_mode_tab(&test_config("k8s"), "/tmp", String::new())
            .unwrap();
        assert_eq!(tm.active_index(), 1);

        tm.close_tab(1).unwrap();
        assert_eq!(tm.active_index(), 0);
        assert!(tm.active_mode_name().is_none());
    }

    #[test]
    fn all_managed_pane_ids_across_tabs() {
        let mut tm = make_manager();
        let (_, ids1) = tm
            .open_mode_tab(&test_config("a"), "/tmp/a", String::new())
            .unwrap();
        let (_, ids2) = tm
            .open_mode_tab(&test_config("b"), "/tmp/b", String::new())
            .unwrap();

        let all = tm.all_managed_pane_ids();
        for id in &ids1 {
            assert!(all.contains(id));
        }
        for id in &ids2 {
            assert!(all.contains(id));
        }
    }

    // -- extract_new_bash_commands tests (moved from ui.rs) --

    fn make_session_with_bash(sid: &str, cmd: &str) -> (String, SessionState) {
        let mut metadata = HashMap::new();
        metadata.insert("bash_command".to_string(), cmd.to_string());
        let event = AgentEvent {
            session_id: sid.to_string(),
            agent_type: crate::event::AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            timestamp: Utc::now(),
            tool_name: Some("Bash".to_string()),
            tool_detail: None,
            cwd: None,
            user_prompt: None,
            metadata,
            pane_id: None,
        };
        let mut recent_events = VecDeque::new();
        recent_events.push_back(event);
        (
            sid.to_string(),
            SessionState {
                session_id: sid.to_string(),
                agent_type: crate::event::AgentType::ClaudeCode,
                cwd: None,
                status: SessionStatus::Working,
                active_tool: None,
                started_at: Utc::now(),
                last_activity: Utc::now(),
                recent_events,
                tool_count: 1,
                last_user_prompt: None,
                pane_id: None,
            },
        )
    }

    #[test]
    fn extract_new_bash_commands_finds_new_commands() {
        let (sid, session) = make_session_with_bash("s1", "kubectl get pods");
        let mut sessions = HashMap::new();
        sessions.insert(sid, session);
        let mut last_routed = HashMap::new();

        let cmds = extract_new_bash_commands(&sessions, &mut last_routed);
        assert_eq!(cmds, vec!["kubectl get pods"]);
    }

    #[test]
    fn extract_new_bash_commands_skips_already_seen() {
        let (sid, session) = make_session_with_bash("s1", "kubectl get pods");
        let mut sessions = HashMap::new();
        sessions.insert(sid, session);
        let mut last_routed = HashMap::new();

        // First call picks it up.
        let _ = extract_new_bash_commands(&sessions, &mut last_routed);
        // Second call should find nothing new.
        let cmds = extract_new_bash_commands(&sessions, &mut last_routed);
        assert!(cmds.is_empty());
    }

    #[test]
    fn extract_new_bash_commands_ignores_non_bash() {
        let mut sessions = HashMap::new();
        let event = AgentEvent {
            session_id: "s1".to_string(),
            agent_type: crate::event::AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            timestamp: Utc::now(),
            tool_name: Some("Read".to_string()),
            tool_detail: None,
            cwd: None,
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        };
        let mut recent_events = VecDeque::new();
        recent_events.push_back(event);
        sessions.insert(
            "s1".to_string(),
            SessionState {
                session_id: "s1".to_string(),
                agent_type: crate::event::AgentType::ClaudeCode,
                cwd: None,
                status: SessionStatus::Idle,
                active_tool: None,
                started_at: Utc::now(),
                last_activity: Utc::now(),
                recent_events,
                tool_count: 0,
                last_user_prompt: None,
                pane_id: None,
            },
        );
        let mut last_routed = HashMap::new();
        let cmds = extract_new_bash_commands(&sessions, &mut last_routed);
        assert!(cmds.is_empty());
    }

    #[test]
    fn extract_new_bash_commands_cleans_up_removed_sessions() {
        let (sid, session) = make_session_with_bash("s1", "echo hi");
        let mut sessions = HashMap::new();
        sessions.insert(sid, session);
        let mut last_routed = HashMap::new();

        let _ = extract_new_bash_commands(&sessions, &mut last_routed);
        assert!(last_routed.contains_key("s1"));

        // Remove session and call again — should clean up.
        sessions.clear();
        let _ = extract_new_bash_commands(&sessions, &mut last_routed);
        assert!(!last_routed.contains_key("s1"));
    }
}
