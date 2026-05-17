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
    None,
}

impl AgentType {
    /// PRD #76 M2.13: best-effort inference of agent type from the binary
    /// name in a spawn command. Used by TUI spawn sites to populate
    /// `StartAgentOptions.agent_type` so the daemon's registry can echo it
    /// back via `list_agents` and a remote reconnect can build placeholder
    /// sessions with the correct type instead of "No agent".
    ///
    /// Returns `Some(AgentType)` only for recognized agent binaries
    /// (`claude` → `ClaudeCode`, `opencode` → `OpenCode`); unknown
    /// commands and `None` input return `None` so the daemon stores
    /// "type not known yet" rather than misclassifying. Whitespace
    /// before the binary name is ignored to match shell-style invocations.
    pub fn from_command(cmd: Option<&str>) -> Option<Self> {
        let cmd = cmd?;
        let bin = cmd.split_whitespace().next()?;
        let basename = std::path::Path::new(bin).file_name()?.to_str()?;
        match basename {
            "claude" => Some(AgentType::ClaudeCode),
            "opencode" => Some(AgentType::OpenCode),
            _ => None,
        }
    }
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

/// Envelope for messages sent to the daemon over the Unix socket.
///
/// Existing hook senders transmit raw `AgentEvent` JSON (no `message_type` field).
/// New message types (e.g. `WorkDone`) include `"message_type": "work_done"` so the
/// daemon can distinguish them.  The daemon tries `DaemonMessage` first, then falls
/// back to `AgentEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type")]
pub enum DaemonMessage {
    /// Orchestrator delegates work to one or more worker roles.
    #[serde(rename = "delegate")]
    Delegate(DelegateSignal),
    /// Worker (or orchestrator with `done`) reports task completion.
    #[serde(rename = "work_done")]
    WorkDone(WorkDoneSignal),
}

/// Signal sent by the orchestrator via `dot-agent-deck delegate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateSignal {
    pub pane_id: String,
    pub task: String,
    /// Role names to delegate to (one or more).
    pub to: Vec<String>,
    pub timestamp: DateTime<Utc>,
}

/// Signal sent by a worker via `dot-agent-deck work-done`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDoneSignal {
    pub pane_id: String,
    pub task: String,
    /// When true, the orchestrator signals that the entire orchestration is complete.
    #[serde(default)]
    pub done: bool,
    pub timestamp: DateTime<Utc>,
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

    // PRD #76 M2.13: pin the AgentType::from_command inference rules.
    // Spawn-site callers (orchestration roles, new-pane form, session
    // restore) feed the daemon's `StartAgent.agent_type` through this
    // helper so the hydrated dashboard card on reconnect has the right
    // type. The mapping must be stable: a regression that flips the
    // `claude` → ClaudeCode arm would silently strand every reconnected
    // pane back at "No agent".
    #[test]
    fn agent_type_from_command_recognizes_claude() {
        assert_eq!(
            AgentType::from_command(Some("claude")),
            Some(AgentType::ClaudeCode)
        );
        // Full path also resolves via file_name().
        assert_eq!(
            AgentType::from_command(Some("/usr/local/bin/claude")),
            Some(AgentType::ClaudeCode)
        );
        // Args after the binary are ignored.
        assert_eq!(
            AgentType::from_command(Some("claude --dangerously-skip-permissions")),
            Some(AgentType::ClaudeCode)
        );
    }

    #[test]
    fn agent_type_from_command_recognizes_opencode() {
        assert_eq!(
            AgentType::from_command(Some("opencode")),
            Some(AgentType::OpenCode)
        );
        assert_eq!(
            AgentType::from_command(Some("/opt/bin/opencode --foo")),
            Some(AgentType::OpenCode)
        );
    }

    #[test]
    fn agent_type_from_command_returns_none_for_unknown_or_empty() {
        // Non-agent commands must NOT misclassify — the daemon would
        // otherwise echo a wrong type via list_agents and the dashboard
        // would mislabel non-agent panes on reconnect.
        assert!(AgentType::from_command(Some("sh")).is_none());
        assert!(AgentType::from_command(Some("/bin/bash")).is_none());
        assert!(AgentType::from_command(Some("vim")).is_none());
        assert!(AgentType::from_command(None).is_none());
        // Whitespace-only / empty input also stays None.
        assert!(AgentType::from_command(Some("")).is_none());
        assert!(AgentType::from_command(Some("   ")).is_none());
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

    #[test]
    fn serialize_deserialize_delegate_signal() {
        let signal = DelegateSignal {
            pane_id: "pane-1".into(),
            task: "Implement login".into(),
            to: vec!["coder".into()],
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let msg = DaemonMessage::Delegate(signal);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: DaemonMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonMessage::Delegate(s) => {
                assert_eq!(s.pane_id, "pane-1");
                assert_eq!(s.task, "Implement login");
                assert_eq!(s.to, vec!["coder"]);
            }
            _ => panic!("expected Delegate"),
        }
    }

    #[test]
    fn serialize_deserialize_work_done_signal() {
        let signal = WorkDoneSignal {
            pane_id: "pane-2".into(),
            task: "Implemented login".into(),
            done: false,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let msg = DaemonMessage::WorkDone(signal);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: DaemonMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonMessage::WorkDone(s) => {
                assert_eq!(s.pane_id, "pane-2");
                assert_eq!(s.task, "Implemented login");
                assert!(!s.done);
            }
            _ => panic!("expected WorkDone"),
        }
    }

    #[test]
    fn work_done_signal_defaults() {
        let json = r#"{
            "message_type": "work_done",
            "pane_id": "pane-2",
            "task": "Done",
            "timestamp": "2026-04-17T10:00:00Z"
        }"#;
        let msg: DaemonMessage = serde_json::from_str(json).unwrap();
        match msg {
            DaemonMessage::WorkDone(s) => {
                assert!(!s.done);
            }
            _ => panic!("expected WorkDone"),
        }
    }

    #[test]
    fn agent_event_not_parseable_as_daemon_message() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "idle",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        assert!(serde_json::from_str::<DaemonMessage>(json).is_err());
    }
}
