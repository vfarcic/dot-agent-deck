use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    ToolStart,
    ToolEnd,
    Thinking,
    Compacting,
    SubagentStart,
    SubagentStop,
    WaitingForInput,
    PermissionRequest,
    Idle,
    Error,
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    ClaudeCode,
    OpenCode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub session_id: String,
    pub agent_type: AgentType,
    pub event_type: EventType,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_detail: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub user_prompt: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub pane_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_event() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "tool_start",
            "tool_name": "Read",
            "tool_detail": "src/main.rs",
            "cwd": "/home/user/project",
            "timestamp": "2026-03-22T10:00:00Z",
            "metadata": {"key": "value"}
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.session_id, "abc-123");
        assert_eq!(event.agent_type, AgentType::ClaudeCode);
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Read"));
    }

    #[test]
    fn parse_minimal_event() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "idle",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert!(event.tool_name.is_none());
        assert!(event.tool_detail.is_none());
        assert!(event.cwd.is_none());
        assert!(event.metadata.is_empty());
    }

    #[test]
    fn parse_event_with_user_prompt() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "thinking",
            "user_prompt": "fix the login bug",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.user_prompt.as_deref(), Some("fix the login bug"));
    }

    #[test]
    fn parse_event_without_user_prompt() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "tool_start",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert!(event.user_prompt.is_none());
    }

    #[test]
    fn reject_invalid_event_type() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "unknown_type",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        assert!(serde_json::from_str::<AgentEvent>(json).is_err());
    }

    #[test]
    fn parse_open_code_event() {
        let json = r#"{
            "session_id": "oc-456",
            "agent_type": "open_code",
            "event_type": "session_start",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.agent_type, AgentType::OpenCode);
        assert_eq!(event.event_type, EventType::SessionStart);
    }
}
