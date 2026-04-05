use std::collections::HashMap;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use crate::config::socket_path;
use crate::event::{AgentEvent, AgentType, EventType};

#[derive(Debug, Deserialize)]
struct ClaudeCodeHookInput {
    session_id: String,
    hook_event_name: String,
    cwd: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
    prompt: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeHookInput {
    session_id: String,
    event: String,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    status: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

pub fn handle_hook(agent: &str) -> ExitCode {
    let input = match read_stdin() {
        Some(s) if !s.is_empty() => s,
        _ => return ExitCode::SUCCESS,
    };

    let event = match agent {
        "opencode" => {
            let hook_input: OpenCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_opencode_event(hook_input)
        }
        _ => {
            let hook_input: ClaudeCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_event(hook_input)
        }
    };

    let event = match event {
        Some(e) => e,
        None => return ExitCode::SUCCESS,
    };

    let is_claude_permission = event.event_type == EventType::PermissionRequest
        && matches!(event.agent_type, AgentType::ClaudeCode);

    let json = match serde_json::to_string(&event) {
        Ok(j) => j,
        Err(_) => return ExitCode::SUCCESS,
    };

    if is_claude_permission {
        match send_and_wait_for_response(&json) {
            Some(decision) => {
                let output = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": {
                            "behavior": decision
                        }
                    }
                });
                println!("{output}");
                ExitCode::SUCCESS
            }
            None => ExitCode::FAILURE,
        }
    } else {
        let _ = send_to_socket(&json);
        ExitCode::SUCCESS
    }
}

fn read_stdin() -> Option<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    Some(buf)
}

fn map_event_type(hook_event_name: &str) -> Option<EventType> {
    match hook_event_name {
        "SessionStart" => Some(EventType::SessionStart),
        "SessionEnd" => Some(EventType::SessionEnd),
        "UserPromptSubmit" => Some(EventType::Thinking),
        "PreToolUse" => Some(EventType::ToolStart),
        "PostToolUse" => Some(EventType::ToolEnd),
        "Notification" => Some(EventType::WaitingForInput),
        "PermissionRequest" => Some(EventType::PermissionRequest),
        "Stop" => Some(EventType::Idle),
        "StopFailure" => Some(EventType::Error),
        "PreCompact" => Some(EventType::Compacting),
        "PostCompact" => Some(EventType::Thinking),
        "SubagentStart" => Some(EventType::SubagentStart),
        "SubagentStop" => Some(EventType::SubagentStop),
        _ => None,
    }
}

fn extract_tool_detail(tool_name: Option<&str>, tool_input: Option<&Value>) -> Option<String> {
    let input = tool_input?.as_object()?;
    let detail = match tool_name? {
        "Bash" => {
            let cmd = input.get("command")?.as_str()?;
            let first_line = cmd.lines().next().unwrap_or(cmd);
            truncate(first_line, 120)
        }
        "Read" | "Edit" | "Write" => input.get("file_path")?.as_str()?.to_string(),
        "Grep" | "Glob" => input.get("pattern")?.as_str()?.to_string(),
        "Agent" => input.get("description")?.as_str()?.to_string(),
        _ => {
            // First string-valued key
            let val = input.values().find_map(|v| v.as_str())?;
            truncate(val, 80)
        }
    };
    Some(detail)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn build_event(input: ClaudeCodeHookInput) -> Option<AgentEvent> {
    let ClaudeCodeHookInput {
        session_id,
        hook_event_name,
        cwd,
        tool_name,
        tool_input,
        tool_use_id,
        prompt,
        _extra: _,
    } = input;

    let event_type = map_event_type(&hook_event_name)?;
    let tool_detail = extract_tool_detail(tool_name.as_deref(), tool_input.as_ref());

    let user_prompt = prompt.map(|p| truncate(&p, 200));
    let pane_id = std::env::var("DOT_AGENT_DECK_PANE_ID").ok();

    let mut metadata = HashMap::new();
    if let Some(tool_use_id) = tool_use_id {
        metadata.insert("tool_use_id".to_string(), tool_use_id);
    }
    if matches!(event_type, EventType::PermissionRequest) {
        metadata.insert("permission_state".to_string(), "pending".to_string());
        // Claude Code doesn't include tool_use_id in PermissionRequest events,
        // so generate a synthetic one for the response channel correlation.
        metadata
            .entry("tool_use_id".to_string())
            .or_insert_with(|| format!("perm-{}-{}", session_id, Utc::now().timestamp_millis()));
    }

    // Store full bash command for reactive pane routing (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && tool_name.as_deref() == Some("Bash")
        && let Some(ref input) = tool_input
        && let Some(cmd) = input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    Some(AgentEvent {
        session_id,
        agent_type: AgentType::ClaudeCode,
        event_type,
        tool_name,
        tool_detail,
        cwd,
        timestamp: Utc::now(),
        user_prompt,
        metadata,
        pane_id,
    })
}

fn map_opencode_event_type(event: &str, status: Option<&str>) -> Option<EventType> {
    match event {
        "session.created" => Some(EventType::SessionStart),
        "session.deleted" => Some(EventType::SessionEnd),
        "session.idle" => Some(EventType::Idle),
        "session.error" => Some(EventType::Error),
        "session.prompt" => Some(EventType::Thinking),
        "session.status" | "session.status.updated" => {
            let norm = status.map(|s| s.to_ascii_lowercase());
            match norm.as_deref() {
                Some("idle") => Some(EventType::Idle),
                Some("error") => Some(EventType::Error),
                Some("waiting") => Some(EventType::WaitingForInput),
                _ => Some(EventType::Thinking),
            }
        }
        "tool.execute.before" => Some(EventType::ToolStart),
        "tool.execute.after" => Some(EventType::ToolEnd),
        "permission.asked" => Some(EventType::PermissionRequest),
        "permission.replied" => Some(EventType::Thinking),
        _ => None,
    }
}

fn build_opencode_event(input: OpenCodeHookInput) -> Option<AgentEvent> {
    let event_type = map_opencode_event_type(&input.event, input.status.as_deref())?;
    let tool_detail = extract_tool_detail(input.tool_name.as_deref(), input.tool_input.as_ref());
    let user_prompt = input.prompt.map(|p| truncate(&p, 200));
    let pane_id = std::env::var("DOT_AGENT_DECK_PANE_ID").ok();

    let mut metadata = HashMap::new();
    if matches!(event_type, EventType::PermissionRequest) {
        metadata.insert("permission_state".to_string(), "pending".to_string());
        metadata.insert(
            "tool_use_id".to_string(),
            format!(
                "perm-{}-{}",
                input.session_id,
                Utc::now().timestamp_millis()
            ),
        );
    }

    // Store full bash command for reactive pane routing (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && input.tool_name.as_deref() == Some("Bash")
        && let Some(ref tool_input) = input.tool_input
        && let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    Some(AgentEvent {
        session_id: input.session_id,
        agent_type: AgentType::OpenCode,
        event_type,
        tool_name: input.tool_name,
        tool_detail,
        cwd: input.cwd,
        timestamp: Utc::now(),
        user_prompt,
        metadata,
        pane_id,
    })
}

fn send_and_wait_for_response(json: &str) -> Option<String> {
    use std::io::BufRead;

    let path = socket_path();
    let mut stream = UnixStream::connect(path).ok()?;

    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(600)))
        .ok()?;

    let msg = format!("{json}\n");
    stream.write_all(msg.as_bytes()).ok()?;
    stream.flush().ok()?;

    let mut reader = std::io::BufReader::new(stream);
    let mut response_line = String::new();
    match reader.read_line(&mut response_line) {
        Ok(0) => None,
        Ok(_) => {
            let parsed: serde_json::Value = serde_json::from_str(response_line.trim()).ok()?;
            let decision = parsed.get("decision")?.as_str()?.to_string();
            Some(decision)
        }
        Err(_) => None,
    }
}

fn send_to_socket(json: &str) -> Option<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(path).ok()?;
    let msg = format!("{json}\n");
    stream.write_all(msg.as_bytes()).ok()?;
    stream.flush().ok()?;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_session_start() {
        assert_eq!(
            map_event_type("SessionStart"),
            Some(EventType::SessionStart)
        );
    }

    #[test]
    fn map_pre_tool_use() {
        assert_eq!(map_event_type("PreToolUse"), Some(EventType::ToolStart));
    }

    #[test]
    fn map_post_tool_use() {
        assert_eq!(map_event_type("PostToolUse"), Some(EventType::ToolEnd));
    }

    #[test]
    fn map_notification() {
        assert_eq!(
            map_event_type("Notification"),
            Some(EventType::WaitingForInput)
        );
    }

    #[test]
    fn map_permission_request() {
        assert_eq!(
            map_event_type("PermissionRequest"),
            Some(EventType::PermissionRequest)
        );
    }

    #[test]
    fn map_stop() {
        assert_eq!(map_event_type("Stop"), Some(EventType::Idle));
    }

    #[test]
    fn map_session_end() {
        assert_eq!(map_event_type("SessionEnd"), Some(EventType::SessionEnd));
    }

    #[test]
    fn map_unknown_returns_none() {
        assert_eq!(map_event_type("SomethingElse"), None);
    }

    #[test]
    fn tool_detail_bash_command() {
        let input: Value = serde_json::json!({"command": "ls -la\necho hello"});
        let detail = extract_tool_detail(Some("Bash"), Some(&input));
        assert_eq!(detail.as_deref(), Some("ls -la"));
    }

    #[test]
    fn tool_detail_bash_truncates_long_command() {
        let long_cmd = "x".repeat(200);
        let input: Value = serde_json::json!({"command": long_cmd});
        let detail = extract_tool_detail(Some("Bash"), Some(&input)).unwrap();
        assert!(detail.len() <= 124); // 120 + "…" (3 bytes)
    }

    #[test]
    fn tool_detail_read_file_path() {
        let input: Value = serde_json::json!({"file_path": "/src/main.rs"});
        let detail = extract_tool_detail(Some("Read"), Some(&input));
        assert_eq!(detail.as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn tool_detail_edit_file_path() {
        let input: Value =
            serde_json::json!({"file_path": "/src/lib.rs", "old_string": "a", "new_string": "b"});
        let detail = extract_tool_detail(Some("Edit"), Some(&input));
        assert_eq!(detail.as_deref(), Some("/src/lib.rs"));
    }

    #[test]
    fn tool_detail_grep_pattern() {
        let input: Value = serde_json::json!({"pattern": "fn main"});
        let detail = extract_tool_detail(Some("Grep"), Some(&input));
        assert_eq!(detail.as_deref(), Some("fn main"));
    }

    #[test]
    fn tool_detail_glob_pattern() {
        let input: Value = serde_json::json!({"pattern": "**/*.rs"});
        let detail = extract_tool_detail(Some("Glob"), Some(&input));
        assert_eq!(detail.as_deref(), Some("**/*.rs"));
    }

    #[test]
    fn tool_detail_agent_description() {
        let input: Value = serde_json::json!({"description": "explore codebase"});
        let detail = extract_tool_detail(Some("Agent"), Some(&input));
        assert_eq!(detail.as_deref(), Some("explore codebase"));
    }

    #[test]
    fn tool_detail_unknown_tool_uses_first_string() {
        let input: Value = serde_json::json!({"query": "SELECT 1", "timeout": 30});
        let detail = extract_tool_detail(Some("SQL"), Some(&input));
        assert_eq!(detail.as_deref(), Some("SELECT 1"));
    }

    #[test]
    fn tool_detail_none_when_no_input() {
        let detail = extract_tool_detail(Some("Bash"), None);
        assert!(detail.is_none());
    }

    #[test]
    fn tool_detail_none_when_no_tool_name() {
        let input: Value = serde_json::json!({"command": "ls"});
        let detail = extract_tool_detail(None, Some(&input));
        assert!(detail.is_none());
    }

    #[test]
    fn build_event_session_start() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "SessionStart".into(),
            cwd: Some("/tmp".into()),
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.session_id, "test-123");
        assert_eq!(event.event_type, EventType::SessionStart);
        assert_eq!(event.cwd.as_deref(), Some("/tmp"));
        assert!(event.tool_name.is_none());
        assert!(event.user_prompt.is_none());
    }

    #[test]
    fn build_event_tool_start_with_detail() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Read".into()),
            tool_input: Some(serde_json::json!({"file_path": "/src/main.rs"})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Read"));
        assert_eq!(event.tool_detail.as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn build_event_unknown_hook_returns_none() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UnknownHook".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        assert!(build_event(input).is_none());
    }

    #[test]
    fn build_event_user_prompt_submit_extracts_prompt() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UserPromptSubmit".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: Some("fix the login bug".into()),
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.event_type, EventType::Thinking);
        assert_eq!(event.user_prompt.as_deref(), Some("fix the login bug"));
    }

    #[test]
    fn build_event_prompt_truncated_to_200() {
        let long_prompt = "x".repeat(300);
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UserPromptSubmit".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: Some(long_prompt),
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        let prompt = event.user_prompt.unwrap();
        assert!(prompt.len() <= 204); // 200 + "…" (3 bytes)
        assert!(prompt.ends_with('…'));
    }

    #[test]
    fn build_event_permission_request_sets_metadata() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "PermissionRequest".into(),
            cwd: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "rm -rf /"})),
            tool_use_id: Some("use-1".into()),
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.event_type, EventType::PermissionRequest);
        assert_eq!(
            event.metadata.get("tool_use_id").map(String::as_str),
            Some("use-1")
        );
        assert_eq!(
            event.metadata.get("permission_state").map(String::as_str),
            Some("pending")
        );
    }

    #[test]
    fn send_to_missing_socket_returns_none() {
        // With no daemon running, send should silently fail
        // SAFETY: This test runs single-threaded; no other thread reads this env var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_SOCKET", "/tmp/nonexistent-test-socket.sock");
        }
        let result = send_to_socket(r#"{"test": true}"#);
        assert!(result.is_none());
    }

    #[test]
    fn deserialize_claude_code_hook_input() {
        let json = r#"{
            "session_id": "abc-123",
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "source": "claude_code"
        }"#;
        let input: ClaudeCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "abc-123");
        assert_eq!(input.hook_event_name, "PreToolUse");
        assert_eq!(input.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn deserialize_minimal_hook_input() {
        let json = r#"{
            "session_id": "abc-123",
            "hook_event_name": "SessionStart"
        }"#;
        let input: ClaudeCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "abc-123");
        assert!(input.cwd.is_none());
        assert!(input.tool_name.is_none());
        assert!(input.tool_input.is_none());
    }

    // --- OpenCode tests ---

    #[test]
    fn map_opencode_session_created() {
        assert_eq!(
            map_opencode_event_type("session.created", None),
            Some(EventType::SessionStart)
        );
    }

    #[test]
    fn map_opencode_session_deleted() {
        assert_eq!(
            map_opencode_event_type("session.deleted", None),
            Some(EventType::SessionEnd)
        );
    }

    #[test]
    fn map_opencode_session_idle() {
        assert_eq!(
            map_opencode_event_type("session.idle", None),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_opencode_session_error() {
        assert_eq!(
            map_opencode_event_type("session.error", None),
            Some(EventType::Error)
        );
    }

    #[test]
    fn map_opencode_session_status_default() {
        assert_eq!(
            map_opencode_event_type("session.status", None),
            Some(EventType::Thinking)
        );
        assert_eq!(
            map_opencode_event_type("session.status", Some("busy")),
            Some(EventType::Thinking)
        );
        assert_eq!(
            map_opencode_event_type("session.status.updated", Some("retry")),
            Some(EventType::Thinking)
        );
    }

    #[test]
    fn map_opencode_session_status_idle() {
        assert_eq!(
            map_opencode_event_type("session.status", Some("idle")),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_opencode_permission_asked() {
        assert_eq!(
            map_opencode_event_type("permission.asked", None),
            Some(EventType::PermissionRequest)
        );
    }

    #[test]
    fn map_opencode_session_status_error() {
        assert_eq!(
            map_opencode_event_type("session.status", Some("error")),
            Some(EventType::Error)
        );
    }

    #[test]
    fn map_opencode_tool_before() {
        assert_eq!(
            map_opencode_event_type("tool.execute.before", None),
            Some(EventType::ToolStart)
        );
    }

    #[test]
    fn map_opencode_tool_after() {
        assert_eq!(
            map_opencode_event_type("tool.execute.after", None),
            Some(EventType::ToolEnd)
        );
    }

    #[test]
    fn map_opencode_unknown_returns_none() {
        assert_eq!(map_opencode_event_type("unknown.event", None), None);
    }

    #[test]
    fn build_opencode_event_session_created() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "session.created".into(),
            tool_name: None,
            tool_input: None,
            status: None,
            cwd: Some("/tmp".into()),
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.session_id, "oc-123");
        assert_eq!(event.agent_type, AgentType::OpenCode);
        assert_eq!(event.event_type, EventType::SessionStart);
        assert_eq!(event.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn build_opencode_event_tool_with_detail() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "tool.execute.before".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "cargo build"})),
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_detail.as_deref(), Some("cargo build"));
    }

    #[test]
    fn build_opencode_event_unknown_returns_none() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "unknown.event".into(),
            tool_name: None,
            tool_input: None,
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        assert!(build_opencode_event(input).is_none());
    }

    #[test]
    fn deserialize_opencode_hook_input() {
        let json = r#"{
            "session_id": "oc-456",
            "event": "tool.execute.before",
            "tool_name": "Read",
            "tool_input": {"file_path": "/src/main.rs"},
            "cwd": "/home/user",
            "extra_field": "ignored"
        }"#;
        let input: OpenCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "oc-456");
        assert_eq!(input.event, "tool.execute.before");
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
        assert!(input.status.is_none());
    }

    #[test]
    fn deserialize_minimal_opencode_input() {
        let json = r#"{
            "session_id": "oc-456",
            "event": "session.created"
        }"#;
        let input: OpenCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "oc-456");
        assert!(input.tool_name.is_none());
        assert!(input.status.is_none());
        assert!(input.cwd.is_none());
    }

    #[test]
    fn pane_id_propagated_from_env_claude_code() {
        // Temporarily set the env var and restore afterwards.
        let key = "DOT_AGENT_DECK_PANE_ID";
        let prev = std::env::var(key).ok();
        // SAFETY: test is single-threaded for this env var; no other thread reads it.
        unsafe { std::env::set_var(key, "pane-42") };

        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "SessionStart".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.pane_id.as_deref(), Some("pane-42"));

        // Restore
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn pane_id_propagated_from_env_opencode() {
        let key = "DOT_AGENT_DECK_PANE_ID";
        let prev = std::env::var(key).ok();
        // SAFETY: test is single-threaded for this env var; no other thread reads it.
        unsafe { std::env::set_var(key, "pane-99") };

        let input = OpenCodeHookInput {
            session_id: "oc-1".into(),
            event: "session.created".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            prompt: None,
            status: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.pane_id.as_deref(), Some("pane-99"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn build_event_bash_tool_start_stores_full_command() {
        let full_cmd = "kubectl get pods -n production\nkubectl get svc -n production";
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": full_cmd})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd),
        );
        // tool_detail should only have the first line (truncated)
        assert_eq!(
            event.tool_detail.as_deref(),
            Some("kubectl get pods -n production"),
        );
    }

    #[test]
    fn build_event_non_bash_tool_start_no_bash_command() {
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Read".into()),
            tool_input: Some(serde_json::json!({"file_path": "/src/main.rs"})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert!(event.metadata.get("bash_command").is_none());
    }

    #[test]
    fn build_event_bash_tool_end_no_bash_command() {
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PostToolUse".into(),
            cwd: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert!(event.metadata.get("bash_command").is_none());
    }

    #[test]
    fn build_opencode_event_bash_tool_start_stores_full_command() {
        let full_cmd = "helm status my-release --namespace prod";
        let input = OpenCodeHookInput {
            session_id: "oc-1".into(),
            event: "tool.execute.before".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": full_cmd})),
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd),
        );
    }
}
