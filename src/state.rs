use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use crate::event::{AgentEvent, AgentType, EventType};

const MAX_RECENT_EVENTS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Thinking,
    Working,
    Compacting,
    WaitingForInput,
    Idle,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardStats {
    pub active: usize,
    pub working: usize,
    pub thinking: usize,
    pub waiting: usize,
    pub errors: usize,
    pub idle: usize,
    pub compacting: usize,
    pub total_tools: u64,
}

#[derive(Debug, Clone)]
pub struct ActiveTool {
    pub name: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: String,
    pub agent_type: AgentType,
    pub cwd: Option<String>,
    pub status: SessionStatus,
    pub active_tool: Option<ActiveTool>,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub recent_events: VecDeque<AgentEvent>,
    pub tool_count: u32,
    pub last_user_prompt: Option<String>,
    pub pane_id: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct AppState {
    pub sessions: HashMap<String, SessionState>,
    /// Remembers started_at per pane so a `/clear` restart keeps its position.
    pane_started_at: HashMap<String, DateTime<Utc>>,
    /// Set by the background version-check task when a newer release exists.
    pub update_available: Option<String>,
    /// Pane IDs created by our app — events from unknown panes are rejected.
    pub managed_pane_ids: HashSet<String>,
}

pub type SharedState = Arc<RwLock<AppState>>;

impl AppState {
    pub fn aggregate_stats(&self) -> DashboardStats {
        let mut stats = DashboardStats {
            active: self.sessions.len(),
            ..DashboardStats::default()
        };
        for session in self.sessions.values() {
            match session.status {
                SessionStatus::Working => stats.working += 1,
                SessionStatus::Thinking => stats.thinking += 1,
                SessionStatus::WaitingForInput => stats.waiting += 1,
                SessionStatus::Error => stats.errors += 1,
                SessionStatus::Idle => stats.idle += 1,
                SessionStatus::Compacting => stats.compacting += 1,
            }
            stats.total_tools += session.tool_count as u64;
        }
        stats
    }

    /// Register a pane ID as managed by our app.
    pub fn register_pane(&mut self, pane_id: String) {
        self.managed_pane_ids.insert(pane_id);
    }

    /// Unregister a pane ID (e.g., when closing a pane).
    pub fn unregister_pane(&mut self, pane_id: &str) {
        self.managed_pane_ids.remove(pane_id);
    }

    pub fn apply_event(&mut self, mut event: AgentEvent) {
        // Only accept events from panes managed by our app.
        // Events without a pane_id (external agents) are rejected when we have
        // managed panes. Events with an unknown pane_id are rejected unless it
        // is a SessionStart (which may arrive before register_pane during startup).
        if let Some(ref pane_id) = event.pane_id {
            if !self.managed_pane_ids.contains(pane_id) {
                if event.event_type == EventType::SessionStart {
                    // Auto-register the pane to handle the startup race where
                    // the hook fires before register_pane is called.
                    self.managed_pane_ids.insert(pane_id.clone());
                } else {
                    return;
                }
            }
        } else if !self.managed_pane_ids.is_empty() {
            return;
        }
        if let Some(ref pane_id) = event.pane_id
            && let Some(existing_id) = self.sessions.iter().find_map(|(id, session)| {
                (session.pane_id.as_ref().is_some_and(|p| p == pane_id) && id != &event.session_id)
                    .then(|| id.clone())
            })
        {
            let old_id = std::mem::replace(&mut event.session_id, existing_id);
            if old_id != event.session_id {
                self.sessions.remove(&old_id);
            }
        }

        if event.event_type == EventType::SessionEnd {
            // Preserve started_at for the pane so a restarted session keeps its position.
            if let Some(session) = self.sessions.get(&event.session_id)
                && let Some(ref pane_id) = session.pane_id
            {
                self.pane_started_at
                    .insert(pane_id.clone(), session.started_at);
            }
            self.sessions.remove(&event.session_id);
            return;
        }

        let pane_started = event
            .pane_id
            .as_ref()
            .and_then(|pid| self.pane_started_at.get(pid))
            .copied();

        let session = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionState {
                session_id: event.session_id.clone(),
                agent_type: event.agent_type.clone(),
                cwd: event.cwd.clone(),
                status: SessionStatus::Idle,
                active_tool: None,
                started_at: pane_started.unwrap_or(event.timestamp),
                last_activity: event.timestamp,
                recent_events: VecDeque::new(),
                tool_count: 0,
                last_user_prompt: None,
                pane_id: event.pane_id.clone(),
            });

        session.last_activity = event.timestamp;

        if event.cwd.is_some() {
            session.cwd.clone_from(&event.cwd);
        }

        if event.user_prompt.is_some() {
            session.last_user_prompt.clone_from(&event.user_prompt);
        }

        if event.pane_id.is_some() {
            session.pane_id.clone_from(&event.pane_id);
        }

        match event.event_type {
            EventType::SessionStart => {
                session.status = SessionStatus::Idle;
                session.active_tool = None;
            }
            EventType::Thinking => {
                session.status = SessionStatus::Thinking;
                session.active_tool = None;
            }
            EventType::ToolStart => {
                session.status = SessionStatus::Working;
                session.active_tool = Some(ActiveTool {
                    name: event.tool_name.clone().unwrap_or_default(),
                    detail: event.tool_detail.clone(),
                });
            }
            EventType::ToolEnd => {
                session.active_tool = None;
                session.tool_count += 1;
                if session.status == SessionStatus::WaitingForInput {
                    session.status = SessionStatus::Thinking;
                }
            }
            EventType::WaitingForInput | EventType::PermissionRequest => {
                session.status = SessionStatus::WaitingForInput;
            }
            EventType::Idle => {
                session.status = SessionStatus::Idle;
                session.active_tool = None;
            }
            EventType::Compacting => {
                session.status = SessionStatus::Compacting;
                session.active_tool = None;
            }
            EventType::SubagentStart | EventType::SubagentStop => {
                // Informational — recorded in recent_events but no status change
            }
            EventType::Error => {
                session.status = SessionStatus::Error;
            }
            EventType::SessionEnd => unreachable!(),
        }

        session.recent_events.push_back(event);
        if session.recent_events.len() > MAX_RECENT_EVENTS {
            session.recent_events.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentType, EventType};
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_event(session_id: &str, event_type: EventType) -> AgentEvent {
        AgentEvent {
            session_id: session_id.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/tmp".to_string()),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        }
    }

    #[test]
    fn full_session_lifecycle() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Idle);

        let mut tool_event = make_event("s1", EventType::ToolStart);
        tool_event.tool_name = Some("Read".to_string());
        tool_event.tool_detail = Some("main.rs".to_string());
        state.apply_event(tool_event);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(
            state.sessions["s1"].active_tool.as_ref().unwrap().name,
            "Read"
        );

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert!(state.sessions["s1"].active_tool.is_none());

        state.apply_event(make_event("s1", EventType::SessionEnd));
        assert!(!state.sessions.contains_key("s1"));
    }

    #[test]
    fn concurrent_sessions() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s2", EventType::SessionStart));
        assert_eq!(state.sessions.len(), 2);

        let mut tool_event = make_event("s1", EventType::ToolStart);
        tool_event.tool_name = Some("Write".to_string());
        state.apply_event(tool_event);

        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(state.sessions["s2"].status, SessionStatus::Idle);
    }

    #[test]
    fn reuse_session_for_same_pane() {
        let mut state = AppState::default();
        state.register_pane("pane-1".to_string());

        let mut first = make_event("s1", EventType::SessionStart);
        first.pane_id = Some("pane-1".to_string());
        state.apply_event(first);

        let mut restart = make_event("s2", EventType::SessionStart);
        restart.pane_id = Some("pane-1".to_string());
        state.apply_event(restart);

        assert!(state.sessions.contains_key("s1"));
        assert!(!state.sessions.contains_key("s2"));
        assert_eq!(state.sessions["s1"].pane_id.as_deref(), Some("pane-1"));
    }

    #[test]
    fn auto_create_unknown_session() {
        let mut state = AppState::default();

        let mut tool_event = make_event("unknown", EventType::ToolStart);
        tool_event.tool_name = Some("Bash".to_string());
        state.apply_event(tool_event);

        assert!(state.sessions.contains_key("unknown"));
        assert_eq!(state.sessions["unknown"].status, SessionStatus::Working);
    }

    #[test]
    fn event_buffer_capping() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        for _ in 0..60 {
            state.apply_event(make_event("s1", EventType::Idle));
        }

        // 1 SessionStart + 60 Idle = 61, capped to 50
        assert_eq!(state.sessions["s1"].recent_events.len(), 50);
    }

    #[test]
    fn waiting_for_input_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
        assert!(state.sessions["s1"].active_tool.is_none());
    }

    #[test]
    fn notification_during_active_tool_shows_waiting() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // A Notification during an active tool means a permission prompt —
        // PreToolUse fires before the Notification, so active_tool is set.
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
        assert!(state.sessions["s1"].active_tool.is_some());
    }

    #[test]
    fn ask_user_question_shows_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("AskUserQuestion".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // AskUserQuestion is interactive — Notification transitions to WaitingForInput.
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn tool_count_increments_on_tool_end() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert_eq!(state.sessions["s1"].tool_count, 0);

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Read".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].tool_count, 0);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].tool_count, 1);

        let mut tool_start2 = make_event("s1", EventType::ToolStart);
        tool_start2.tool_name = Some("Write".to_string());
        state.apply_event(tool_start2);
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].tool_count, 2);
    }

    #[test]
    fn tool_end_clears_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        // Simulate: PreToolUse → PermissionRequest → tool runs → PostToolUse
        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Thinking);
    }

    #[test]
    fn tool_end_preserves_working_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // ToolEnd without permission request should keep Working→Working (not change)
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn error_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::Error));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Error);
    }

    #[test]
    fn last_user_prompt_set_and_persists() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert!(state.sessions["s1"].last_user_prompt.is_none());

        let mut prompt_event = make_event("s1", EventType::Thinking);
        prompt_event.user_prompt = Some("fix the bug".to_string());
        state.apply_event(prompt_event);
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the bug")
        );

        // Subsequent event without prompt should not clear it
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the bug")
        );

        // New prompt replaces old one
        let mut prompt_event2 = make_event("s1", EventType::Thinking);
        prompt_event2.user_prompt = Some("add tests".to_string());
        state.apply_event(prompt_event2);
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("add tests")
        );
    }

    #[test]
    fn aggregate_stats_empty() {
        let state = AppState::default();
        let stats = state.aggregate_stats();
        assert_eq!(stats, DashboardStats::default());
    }

    #[test]
    fn aggregate_stats_mixed_sessions() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        let mut tool = make_event("s1", EventType::ToolStart);
        tool.tool_name = Some("Read".to_string());
        state.apply_event(tool);
        // s1: Working

        state.apply_event(make_event("s2", EventType::SessionStart));
        state.apply_event(make_event("s2", EventType::WaitingForInput));
        // s2: WaitingForInput

        state.apply_event(make_event("s3", EventType::SessionStart));
        state.apply_event(make_event("s3", EventType::Error));
        // s3: Error

        state.apply_event(make_event("s4", EventType::SessionStart));
        state.apply_event(make_event("s4", EventType::Thinking));
        // s4: Thinking

        state.apply_event(make_event("s5", EventType::SessionStart));
        // s5: Idle

        let stats = state.aggregate_stats();
        assert_eq!(stats.active, 5);
        assert_eq!(stats.working, 1);
        assert_eq!(stats.waiting, 1);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.thinking, 1);
        assert_eq!(stats.idle, 1);
    }

    #[test]
    fn aggregate_stats_tool_count_summation() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        let mut t1 = make_event("s1", EventType::ToolStart);
        t1.tool_name = Some("Read".to_string());
        state.apply_event(t1);
        state.apply_event(make_event("s1", EventType::ToolEnd));

        state.apply_event(make_event("s2", EventType::SessionStart));
        for _ in 0..3 {
            let mut t = make_event("s2", EventType::ToolStart);
            t.tool_name = Some("Bash".to_string());
            state.apply_event(t);
            state.apply_event(make_event("s2", EventType::ToolEnd));
        }

        let stats = state.aggregate_stats();
        assert_eq!(stats.total_tools, 4);
    }

    #[test]
    fn restarted_session_preserves_started_at_via_pane() {
        let mut state = AppState::default();
        state.register_pane("pane-42".to_string());

        // Register session with a pane
        let mut ev = make_event("s1", EventType::SessionStart);
        ev.pane_id = Some("pane-42".to_string());
        state.apply_event(ev);
        let original_started = state.sessions["s1"].started_at;

        // End the session (simulates /clear)
        let mut end_ev = make_event("s1", EventType::SessionEnd);
        end_ev.pane_id = Some("pane-42".to_string());
        state.apply_event(end_ev);
        assert!(!state.sessions.contains_key("s1"));

        // New session on the same pane should keep the original started_at
        let mut ev2 = make_event("s2", EventType::SessionStart);
        ev2.pane_id = Some("pane-42".to_string());
        state.apply_event(ev2);
        assert_eq!(state.sessions["s2"].started_at, original_started);
    }

    #[test]
    fn permission_request_sets_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn tool_start_clears_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".into());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }
}
