#![cfg(unix)]

//! Fast subprocess coverage for Devin's Claude-compatible native hook payloads.
//!
//! Devin reuses the whole Claude ingestion path and only stamps a different
//! [`AgentType`], so the risk is not that the parser breaks — it is that the
//! `"devin"` arm silently falls through to the generic one and mislabels every
//! event, or that Devin's two divergences from Claude's lifecycle regress. Those
//! divergences are the point of this file:
//!
//! - `PostCompaction` — Devin's spelling of the post-compaction event, and the
//!   only compaction signal a Devin session produces (it has no `PreCompact`).
//! - `exec` — Devin's shell tool, whose plain-string `command` the tool-detail
//!   extractor sharpens, falling back to the generic first-string branch when
//!   the shape is not what we guessed.

// Issue #322. Fast-tier, does NOT link `tests/common/mod.rs`; the ~40-line
// crate-internal resolver is `#[path]`-included instead, at two extra test
// executions rather than the harness's ~530. Every scratch dir here binds a
// `hook.sock`, which is exactly the shape that kept `tests/daemon_protocol.rs`
// out of the OS temp dir. See `docs/develop/e2e-temp-dirs.md`.
#[path = "../src/test_temp.rs"]
mod test_temp;

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dot_agent_deck::event::{AgentEvent, AgentType, EventType};

fn invoke_devin_hook(payload: &serde_json::Value) -> AgentEvent {
    let temp = test_temp::tempdir().expect("create Devin hook socket directory");
    let socket = temp.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).expect("bind Devin hook socket");
    listener
        .set_nonblocking(true)
        .expect("make Devin hook listener nonblocking");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hook", "--agent", "devin"])
        .env("DOT_AGENT_DECK_SOCKET", &socket)
        .env("DOT_AGENT_DECK_PANE_ID", "devin-hook-pane")
        .env("DOT_AGENT_DECK_AGENT_ID", "devin-hook-agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Devin hook ingestion command");
    child
        .stdin
        .take()
        .expect("Devin hook stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write Devin hook payload");
    let output = child
        .wait_with_output()
        .expect("wait for Devin hook command");
    assert!(
        output.status.success(),
        "`hook --agent devin` rejected a native Devin hook payload: status={} stderr={}",
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
                    .expect("read emitted Devin AgentEvent");
                return serde_json::from_str(line.trim()).expect("parse emitted Devin AgentEvent");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "`hook --agent devin` exited successfully but emitted no AgentEvent for {payload}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept emitted Devin AgentEvent: {error}"),
        }
    }
}

/// Scenario: Send every hook event the deck installs into Devin's config —
/// SessionStart, UserPromptSubmit, an `exec` tool pair, PermissionRequest, Stop,
/// PostCompaction, and SessionEnd — through the real `hook --agent devin` CLI.
/// Each emitted event must be stamped with Devin identity and carry the matching
/// lifecycle type, prompt text, tool name, and tool detail.
#[test]
fn devin_hooks_001_native_payloads_emit_rich_devin_events() {
    let cases = [
        (
            serde_json::json!({
                "session_id": "devin-hooks-session",
                "hook_event_name": "SessionStart",
                "cwd": "/tmp/devin-hooks",
                "source": "startup"
            }),
            EventType::SessionStart,
            None,
            None,
            None,
        ),
        (
            serde_json::json!({
                "session_id": "devin-hooks-session",
                "hook_event_name": "UserPromptSubmit",
                "cwd": "/tmp/devin-hooks",
                "prompt": "create the hook sentinel"
            }),
            EventType::Thinking,
            None,
            None,
            Some("create the hook sentinel"),
        ),
        (
            serde_json::json!({
                "session_id": "devin-hooks-session",
                "hook_event_name": "PreToolUse",
                "cwd": "/tmp/devin-hooks",
                "tool_name": "exec",
                "tool_use_id": "exec-1",
                "tool_input": {"command": "touch devin_hook_sentinel.txt"}
            }),
            EventType::ToolStart,
            Some("exec"),
            Some("touch devin_hook_sentinel.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "devin-hooks-session",
                "hook_event_name": "PostToolUse",
                "cwd": "/tmp/devin-hooks",
                "tool_name": "exec",
                "tool_use_id": "exec-1",
                "tool_input": {"command": "touch devin_hook_sentinel.txt"},
                "tool_response": ""
            }),
            EventType::ToolEnd,
            Some("exec"),
            Some("touch devin_hook_sentinel.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "devin-hooks-session",
                "hook_event_name": "PermissionRequest",
                "cwd": "/tmp/devin-hooks",
                "tool_name": "exec",
                "tool_input": {"command": "rm devin_hook_sentinel.txt"}
            }),
            EventType::PermissionRequest,
            Some("exec"),
            Some("rm devin_hook_sentinel.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "devin-hooks-session",
                "hook_event_name": "Stop",
                "cwd": "/tmp/devin-hooks",
                "last_assistant_message": "done"
            }),
            EventType::Idle,
            None,
            None,
            None,
        ),
        (
            serde_json::json!({
                "session_id": "devin-hooks-session",
                "hook_event_name": "SessionEnd",
                "cwd": "/tmp/devin-hooks"
            }),
            EventType::SessionEnd,
            None,
            None,
            None,
        ),
    ];

    for (payload, event_type, tool_name, detail_fragment, prompt) in cases {
        let event = invoke_devin_hook(&payload);
        assert_eq!(event.agent_type, AgentType::Devin, "payload={payload}");
        assert_eq!(event.event_type, event_type, "payload={payload}");
        assert_eq!(event.tool_name.as_deref(), tool_name, "payload={payload}");
        if let Some(fragment) = detail_fragment {
            assert!(
                event
                    .tool_detail
                    .as_deref()
                    .is_some_and(|d| d.contains(fragment)),
                "Devin tool detail must contain {fragment:?}; event={event:?} payload={payload}"
            );
        }
        assert_eq!(event.user_prompt.as_deref(), prompt, "payload={payload}");
        assert_eq!(event.pane_id.as_deref(), Some("devin-hook-pane"));
        assert_eq!(event.agent_id.as_deref(), Some("devin-hook-agent"));
    }
}

/// Scenario: Send Devin's `PostCompaction` hook — its own spelling of the
/// post-compaction event, which Claude never sends — through the real
/// `hook --agent devin` CLI. It must map to Thinking rather than being dropped
/// as an unrecognized event name, because it is the only compaction signal a
/// Devin session produces.
#[test]
fn devin_hooks_002_post_compaction_is_the_sole_compaction_signal() {
    let event = invoke_devin_hook(&serde_json::json!({
        "session_id": "devin-hooks-session",
        "hook_event_name": "PostCompaction",
        "cwd": "/tmp/devin-hooks"
    }));

    assert_eq!(event.agent_type, AgentType::Devin);
    assert_eq!(
        event.event_type,
        EventType::Thinking,
        "Devin's PostCompaction must resume the card rather than be dropped; event={event:?}"
    );
}

/// Scenario: Send a Devin `exec` tool payload whose `tool_input` has no
/// `command` key, through the real `hook --agent devin` CLI. Only the tool NAME
/// is documented, so a shape we guessed wrong must still fall through to the
/// generic first-string extraction and yield a useful detail rather than none.
#[test]
fn devin_hooks_003_exec_without_a_command_key_still_yields_a_detail() {
    let event = invoke_devin_hook(&serde_json::json!({
        "session_id": "devin-hooks-session",
        "hook_event_name": "PreToolUse",
        "cwd": "/tmp/devin-hooks",
        "tool_name": "exec",
        "tool_input": {"cmd": "ls -la /tmp/devin-hooks"}
    }));

    assert_eq!(event.agent_type, AgentType::Devin);
    assert_eq!(event.event_type, EventType::ToolStart);
    assert_eq!(event.tool_name.as_deref(), Some("exec"));
    assert_eq!(
        event.tool_detail.as_deref(),
        Some("ls -la /tmp/devin-hooks"),
        "an unguessed exec shape must degrade to the generic first-string detail; event={event:?}"
    );
}

/// Scenario: Send a Devin PostToolUse payload whose `exec` tool response reports
/// a non-zero exit while the session is still active. The hook command must emit
/// an Error event immediately instead of an ordinary ToolEnd, so the card turns
/// red the moment the command fails.
#[test]
fn devin_hooks_004_failed_tool_emits_error_before_session_exit() {
    let payload = serde_json::json!({
        "session_id": "devin-hooks-session",
        "hook_event_name": "PostToolUse",
        "cwd": "/tmp/devin-hooks",
        "tool_name": "exec",
        "tool_use_id": "exec-failed-1",
        "tool_input": {"command": "sh -c 'exit 7'"},
        "tool_response": "Exit code: 7\nWall time: 0 seconds\nOutput:\ncommand failed"
    });

    let event = invoke_devin_hook(&payload);
    assert_eq!(event.agent_type, AgentType::Devin);
    assert_eq!(
        event.event_type,
        EventType::Error,
        "a failed Devin PostToolUse must surface Error before process exit; event={event:?} payload={payload}"
    );
}
