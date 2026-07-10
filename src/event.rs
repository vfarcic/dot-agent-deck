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
    Pi,
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
    /// (`claude` → `ClaudeCode`, `opencode` → `OpenCode`, `pi` → `Pi`);
    /// unknown commands and `None` input return `None` so the daemon stores
    /// "type not known yet" rather than misclassifying. Whitespace
    /// before the binary name is ignored to match shell-style invocations.
    pub fn from_command(cmd: Option<&str>) -> Option<Self> {
        let cmd = cmd?;
        let bin = cmd.split_whitespace().next()?;
        let basename = std::path::Path::new(bin).file_name()?.to_str()?;
        match basename {
            "claude" => Some(AgentType::ClaudeCode),
            "opencode" => Some(AgentType::OpenCode),
            "pi" => Some(AgentType::Pi),
            _ => None,
        }
    }
}

/// `AgentEvent.metadata` key carrying a human-friendly card title (PRD #127
/// finding #2). The daemon's live-surface path (`surface_spawned_pane`) sets
/// this to the schedule's task name so an ALREADY-ATTACHED TUI titles the
/// live card with the friendly name — matching what a disconnect/reconnect
/// already renders from the daemon registry's `display_name`. Real agent hooks
/// don't emit it; consumers treat its absence as "no friendly name known".
pub const DISPLAY_NAME_METADATA_KEY: &str = "display_name";

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
    /// PRD #92 F9 followup-7: daemon-side registry id of the agent
    /// that produced this hook event. Populated by the agent's hook
    /// script from the `DOT_AGENT_DECK_AGENT_ID` env var the daemon
    /// injects at spawn time (same pattern as
    /// [`crate::agent_pty::DOT_AGENT_DECK_PANE_ID`]). Lets the
    /// post-respawn dispatch task scope its `SessionStart` wait to
    /// the NEW agent's id, so a late `SessionStart` from the OLD
    /// agent — emitted in the subscribe→kill window — can't be
    /// mis-accepted as the NEW agent's readiness signal. Optional
    /// because hook payloads from external agents (or test forgers)
    /// may omit it; events with `None` simply won't match
    /// agent-id-scoped filters.
    #[serde(default)]
    pub agent_id: Option<String>,
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

/// Daemon → attached-TUI broadcast (PRD #76 M2.17). The daemon publishes
/// one of these per ingested hook event; subscribers receive them as
/// `KIND_EVENT` frames on the attach socket.
///
/// PRD #93 round-5: the `Delegate` / `WorkDone` variants used to ride this
/// channel too, because the daemon couldn't validate or dispatch them
/// locally in external-daemon mode (the role map lived on the TUI side).
/// The daemon now owns the role map and the PTY registry, so it dispatches
/// those signals directly into the target pane's PTY — no broadcast hop,
/// no replay buffer, no salvage. Only hook events keep using this channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum BroadcastMsg {
    /// A hook event (existing M2.17 wire shape, now wrapped).
    #[serde(rename = "event")]
    Event(AgentEvent),
    /// PRD #120: a daemon-spawned ORCHESTRATION (the issue-dispatch path),
    /// pushed to already-attached TUIs so they can build the orchestration tab
    /// LIVE — mid-session, with no reconnect. The single-agent live-surface
    /// path (a synthetic [`EventType::SessionStart`] painted as a flat
    /// dashboard card by [`crate::state::AppState::apply_event`]) cannot
    /// reconstruct a multi-role tab, and orchestration tabs were previously
    /// rebuilt ONLY at TUI hydration (startup / reconnect). This variant
    /// carries the structural membership the TUI's
    /// `open_orchestration_tab_with_existing_role_panes` machinery needs to
    /// build the tab on the fly.
    ///
    /// Adding this variant changes the `KIND_EVENT` payload schema (an older
    /// peer would mis-parse the new `kind` tag), so it bumps
    /// [`crate::daemon_protocol::PROTOCOL_VERSION`].
    #[serde(rename = "orchestration_surface")]
    OrchestrationSurface(OrchestrationSurface),
}

/// PRD #120: the structural membership of a daemon-spawned orchestration,
/// pushed to attached TUIs (via [`BroadcastMsg::OrchestrationSurface`]) so they
/// can build the orchestration tab live. Mirrors what the hydration partition
/// (`OrchestrationHydrationBucket`) reconstructs from per-pane
/// [`crate::agent_pty::TabMembership`] records at reconnect — but for a spawn
/// that happens WHILE a TUI is attached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationSurface {
    /// Canonical orchestration name — the tab IDENTITY and (absent a
    /// `display_title`) the tab-strip LABEL.
    pub name: String,
    /// Absolute orchestration cwd shared by every role pane — the tab's cwd and
    /// the hydration partition's bucket key.
    pub cwd: String,
    /// Optional user-facing tab title; `None` falls back to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    /// The spawned role panes, in role order.
    pub roles: Vec<OrchestrationSurfaceRole>,
}

/// One role pane of a live-surfaced orchestration (see [`OrchestrationSurface`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationSurfaceRole {
    /// The `DOT_AGENT_DECK_PANE_ID` the daemon tagged the pane with — reused as
    /// the TUI-side local pane id so hook events keep routing correctly. The TUI
    /// attaches to the live PTY by resolving THIS pane id through `list_agents`
    /// (see `EmbeddedPaneController::hydrate_pane`), not by a registry agent id —
    /// so no `agent_id` rides on the wire.
    pub pane_id: String,
    /// Position of this role in the orchestration config's `roles`.
    pub role_index: usize,
    /// Role name (e.g. `orchestrator`, `worker`).
    pub role_name: String,
    /// Whether this is the start (orchestrator) role.
    pub is_start_role: bool,
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

    // PRD #201 M1.1 (test-plan row 1): pin the `pi` → AgentType::Pi mapping
    // so a plain `pi` pane and a scheduled `pi` job are recognized as a
    // first-class agent type, and reassert claude/opencode as a regression
    // guard — the same detection path feeds all three. Mirrors the path/arg
    // shapes covered for claude/opencode above.
    #[test]
    fn agent_type_from_command_recognizes_pi() {
        assert_eq!(AgentType::from_command(Some("pi")), Some(AgentType::Pi));
        // Full path also resolves via file_name().
        assert_eq!(
            AgentType::from_command(Some("/usr/local/bin/pi")),
            Some(AgentType::Pi)
        );
        // Args after the binary are ignored.
        assert_eq!(
            AgentType::from_command(Some("pi --some-flag")),
            Some(AgentType::Pi)
        );
        // No regression: claude/opencode still map to their own types.
        assert_eq!(
            AgentType::from_command(Some("claude")),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            AgentType::from_command(Some("opencode")),
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

    // PRD #120: the live-orchestration-surface broadcast must round-trip
    // through the same `BroadcastMsg` wire the daemon forwards over KIND_EVENT,
    // and tag itself `orchestration_surface` so it's distinguishable from the
    // `event` variant an older peer expects (the reason PROTOCOL_VERSION bumped).
    #[test]
    fn orchestration_surface_broadcast_round_trips() {
        let msg = BroadcastMsg::OrchestrationSurface(OrchestrationSurface {
            name: "issue-work".into(),
            cwd: "/work/github-issues/.worktrees/issue-1".into(),
            display_title: None,
            roles: vec![
                OrchestrationSurfaceRole {
                    pane_id: "sched-github-issues-0-r0".into(),
                    role_index: 0,
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                },
                OrchestrationSurfaceRole {
                    pane_id: "sched-github-issues-0-r1".into(),
                    role_index: 1,
                    role_name: "worker".into(),
                    is_start_role: false,
                },
            ],
        });
        let json = serde_json::to_string(&msg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["kind"], "orchestration_surface");
        // `display_title: None` is omitted from the wire (skip_serializing_if).
        assert!(
            v.as_object().unwrap().get("display_title").is_none(),
            "None display_title must be omitted from the wire payload"
        );

        let back: BroadcastMsg = serde_json::from_str(&json).unwrap();
        let BroadcastMsg::OrchestrationSurface(s) = back else {
            panic!("expected a BroadcastMsg::OrchestrationSurface");
        };
        assert_eq!(s.name, "issue-work");
        assert_eq!(s.roles.len(), 2);
        assert_eq!(s.roles[0].role_name, "orchestrator");
        assert!(s.roles[0].is_start_role);
        assert_eq!(s.roles[1].pane_id, "sched-github-issues-0-r1");
        assert_eq!(s.roles[1].role_index, 1);
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
