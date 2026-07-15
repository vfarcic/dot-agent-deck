#![cfg(unix)]

//! Fast subprocess coverage for Codex's Claude-compatible native hook payloads.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dot_agent_deck::event::{AgentEvent, AgentType, EventType};

fn invoke_codex_hook(payload: &serde_json::Value) -> AgentEvent {
    let temp = tempfile::tempdir().expect("create Codex hook socket directory");
    let socket = temp.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).expect("bind Codex hook socket");
    listener
        .set_nonblocking(true)
        .expect("make Codex hook listener nonblocking");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hook", "--agent", "codex"])
        .env("DOT_AGENT_DECK_SOCKET", &socket)
        .env("DOT_AGENT_DECK_PANE_ID", "codex-hook-pane")
        .env("DOT_AGENT_DECK_AGENT_ID", "codex-hook-agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Codex hook ingestion command");
    child
        .stdin
        .take()
        .expect("Codex hook stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write Codex hook payload");
    let output = child
        .wait_with_output()
        .expect("wait for Codex hook command");
    assert!(
        output.status.success(),
        "`hook --agent codex` rejected a native Codex hook payload: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut line = String::new();
                stream
                    .read_to_string(&mut line)
                    .expect("read emitted Codex AgentEvent");
                return serde_json::from_str(line.trim()).expect("parse emitted Codex AgentEvent");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "`hook --agent codex` exited successfully but emitted no AgentEvent for {payload}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept emitted Codex AgentEvent: {error}"),
        }
    }
}

/// Scenario: Send Codex-native SessionStart, prompt, shell/apply_patch tool,
/// Stop, and permission hook JSON through the real `hook --agent codex` CLI.
/// Every emitted event must retain Codex identity and expose the corresponding
/// lifecycle type, prompt text, tool name, and useful tool detail.
#[test]
fn codex_hooks_001_native_payloads_emit_rich_codex_events() {
    let cases = [
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "SessionStart",
                "cwd": "/tmp/codex-hooks",
                "source": "startup"
            }),
            EventType::SessionStart,
            None,
            None,
            None,
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "UserPromptSubmit",
                "cwd": "/tmp/codex-hooks",
                "prompt": "create the hook sentinel"
            }),
            EventType::Thinking,
            None,
            None,
            Some("create the hook sentinel"),
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "PreToolUse",
                "cwd": "/tmp/codex-hooks",
                "tool_name": "shell",
                "tool_use_id": "shell-1",
                "tool_input": {"command": ["/bin/sh", "-lc", "touch codex_hook_sentinel.txt"]}
            }),
            EventType::ToolStart,
            Some("shell"),
            Some("touch codex_hook_sentinel.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "PostToolUse",
                "cwd": "/tmp/codex-hooks",
                "tool_name": "shell",
                "tool_use_id": "shell-1",
                "tool_input": {"command": ["/bin/sh", "-lc", "touch codex_hook_sentinel.txt"]},
                "tool_response": {"exit_code": 0}
            }),
            EventType::ToolEnd,
            Some("shell"),
            Some("touch codex_hook_sentinel.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "PreToolUse",
                "cwd": "/tmp/codex-hooks",
                "tool_name": "apply_patch",
                "tool_use_id": "patch-1",
                "tool_input": {"patch": "*** Add File: codex_hook_patch.txt\n+hook parity"}
            }),
            EventType::ToolStart,
            Some("apply_patch"),
            Some("codex_hook_patch.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "PostToolUse",
                "cwd": "/tmp/codex-hooks",
                "tool_name": "apply_patch",
                "tool_use_id": "patch-1",
                "tool_input": {"patch": "*** Add File: codex_hook_patch.txt\n+hook parity"},
                "tool_response": {"status": "completed"}
            }),
            EventType::ToolEnd,
            Some("apply_patch"),
            Some("codex_hook_patch.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "Stop",
                "cwd": "/tmp/codex-hooks",
                "last_assistant_message": "done"
            }),
            EventType::Idle,
            None,
            None,
            None,
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "PermissionRequest",
                "cwd": "/tmp/codex-hooks",
                "tool_name": "shell",
                "tool_input": {"command": ["rm", "codex_hook_sentinel.txt"]}
            }),
            EventType::PermissionRequest,
            Some("shell"),
            Some("codex_hook_sentinel.txt"),
            None,
        ),
    ];

    for (payload, event_type, tool_name, detail_fragment, prompt) in cases {
        let event = invoke_codex_hook(&payload);
        assert_eq!(event.agent_type, AgentType::Codex, "payload={payload}");
        assert_eq!(event.event_type, event_type, "payload={payload}");
        assert_eq!(event.tool_name.as_deref(), tool_name, "payload={payload}");
        if let Some(fragment) = detail_fragment {
            assert!(
                event
                    .tool_detail
                    .as_deref()
                    .is_some_and(|d| d.contains(fragment)),
                "Codex tool detail must contain {fragment:?}; event={event:?} payload={payload}"
            );
        }
        assert_eq!(event.user_prompt.as_deref(), prompt, "payload={payload}");
        assert_eq!(event.pane_id.as_deref(), Some("codex-hook-pane"));
        assert_eq!(event.agent_id.as_deref(), Some("codex-hook-agent"));
    }
}
