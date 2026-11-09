#![cfg(unix)]

//! Fast subprocess coverage for Codex's Claude-compatible native hook payloads.

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
use dot_agent_deck::question::AnswerChannel;

fn invoke_codex_hook(payload: &serde_json::Value) -> AgentEvent {
    let temp = test_temp::tempdir().expect("create Codex hook socket directory");
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
        // A developer running this from a deck pane inherits that pane's
        // capability token; with one, the hook's inbox calls (PRD #1666 S7)
        // would reach this socket ahead of the event it reads.
        .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
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
                let mut frame: serde_json::Value =
                    serde_json::from_str(line.trim()).expect("parse emitted Codex frame");
                // Issue #1724: a keys-answerable question makes the hook ask
                // the daemon's capabilities first, on a connection of its own.
                // This stub models a daemon from before that answer — the ask
                // times out and the question is built as verified, which is
                // the fallback these tests pin — so the capabilities frame is
                // skipped and the question frame after it is the one read.
                if frame["message_type"] == "hook_capabilities" {
                    continue;
                }
                // PRD #1631 (audit A9): a frame carrying a question leaves the
                // hook as an unheld `question` message, so it goes through the
                // daemon's provenance gate; its event is what is asserted.
                if frame["message_type"] == "question" {
                    assert_eq!(frame["hold"], false, "a keys-answered question is not held");
                    frame = frame["event"].take();
                }
                return serde_json::from_value(frame).expect("parse emitted Codex AgentEvent");
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

/// [`invoke_codex_hook`], against a stub daemon that ANSWERS the hook's
/// capabilities question (issue #1724) with `release` — the release the real
/// daemon latched from the pane's start-up header. The reply has to land while
/// the hook is still asking, so the frames are read as they arrive and the
/// child is waited on only after the question frame is in hand, unlike the
/// plain helper above, whose stub never answers and so can read after the
/// exit.
fn invoke_codex_hook_with_release(
    payload: &serde_json::Value,
    release: (u64, u64, u64),
) -> AgentEvent {
    let temp = test_temp::tempdir().expect("create Codex hook socket directory");
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
        // As in `invoke_codex_hook`: a developer running this from a deck pane
        // inherits that pane's capability token; with one, the hook's inbox
        // calls (PRD #1666 S7) would reach this socket ahead of the frames
        // read here.
        .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Codex hook release command");
    child
        .stdin
        .take()
        .expect("Codex hook stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write Codex hook payload");

    let deadline = Instant::now() + Duration::from_secs(10);
    let event = loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut stream), &mut line)
                    .expect("read a Codex hook frame");
                let mut frame: serde_json::Value =
                    serde_json::from_str(line.trim()).expect("parse emitted Codex frame");
                // The hook's release question: answer it with the latched
                // release this stub plays, so the question the hook then
                // builds is bounded by it — the exchange the plain helpers
                // above leave unanswered.
                if frame["message_type"] == "hook_capabilities" {
                    let reply =
                        serde_json::json!({"agent_release": [release.0, release.1, release.2]});
                    stream
                        .write_all(format!("{reply}\n").as_bytes())
                        .expect("answer the capabilities question");
                    continue;
                }
                // PRD #1631 (audit A9): a frame carrying a question leaves the
                // hook as an unheld `question` message, so it goes through the
                // daemon's provenance gate; its event is what is asserted.
                if frame["message_type"] == "question" {
                    assert_eq!(frame["hold"], false, "a keys-answered question is not held");
                    frame = frame["event"].take();
                }
                break serde_json::from_value(frame).expect("parse emitted Codex AgentEvent");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "`hook --agent codex` emitted no AgentEvent for {payload}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept emitted Codex AgentEvent: {error}"),
        }
    };
    let output = child
        .wait_with_output()
        .expect("wait for Codex hook command");
    assert!(
        output.status.success(),
        "`hook --agent codex` rejected a native Codex hook payload: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    event
}

/// Scenario: Send Codex-native SessionStart, prompt, shell/apply_patch tool, Stop, and permission hook JSON through the real `hook --agent codex` CLI. Every emitted event must retain Codex identity and expose the corresponding lifecycle type, prompt text, tool name, and useful tool detail.
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
                "tool_name": "Bash",
                "tool_use_id": "shell-1",
                "tool_input": {"command": "touch codex_hook_sentinel.txt"}
            }),
            EventType::ToolStart,
            Some("Bash"),
            Some("touch codex_hook_sentinel.txt"),
            None,
        ),
        (
            serde_json::json!({
                "session_id": "codex-hooks-session",
                "hook_event_name": "PostToolUse",
                "cwd": "/tmp/codex-hooks",
                "tool_name": "Bash",
                "tool_use_id": "shell-1",
                "tool_input": {"command": "touch codex_hook_sentinel.txt"},
                "tool_response": ""
            }),
            EventType::ToolEnd,
            Some("Bash"),
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
                "tool_input": {"command": "*** Add File: codex_hook_patch.txt\n+hook parity"}
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
                "tool_input": {"command": "*** Add File: codex_hook_patch.txt\n+hook parity"},
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
                "tool_name": "Bash",
                "tool_input": {"command": "rm codex_hook_sentinel.txt"}
            }),
            EventType::PermissionRequest,
            Some("Bash"),
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

/// Scenario: Send a schema-accurate Codex PostToolUse payload whose string tool response reports a non-zero exit while the session remains active. The hook command must emit an Error event immediately instead of reporting an ordinary ToolEnd.
#[test]
fn codex_hooks_002_failed_tool_emits_error_before_session_exit() {
    let payload = serde_json::json!({
        "session_id": "codex-hooks-session",
        "turn_id": "codex-hooks-turn",
        "hook_event_name": "PostToolUse",
        "cwd": "/tmp/codex-hooks",
        "model": "gpt-5.1-codex-mini",
        "permission_mode": "default",
        "transcript_path": null,
        "tool_name": "Bash",
        "tool_use_id": "shell-failed-1",
        "tool_input": {"command": "sh -c 'exit 7'"},
        "tool_response": "Exit code: 7\nWall time: 0 seconds\nOutput:\ncommand failed"
    });

    let event = invoke_codex_hook(&payload);
    assert_eq!(event.agent_type, AgentType::Codex);
    assert_eq!(
        event.event_type,
        EventType::Error,
        "a failed Codex PostToolUse must surface Error before process exit; event={event:?} payload={payload}"
    );
}

/// Scenario: Send a Codex PermissionRequest through the real `hook --agent codex` CLI against a stub daemon that answers the hook's release question with a latched release. A release past the minor the Codex table was verified on (0.161 past 0.160) must come back as a question the deck refuses to answer by typing — the channel is unsupported and every option is keyboard-only — while a patch inside the verified minor (0.160.5) stays answerable by keys.
#[test]
fn codex_hooks_003_answered_release_bounds_the_emitted_question() {
    let payload = serde_json::json!({
        "session_id": "codex-hooks-session",
        "hook_event_name": "PermissionRequest",
        "cwd": "/tmp/codex-hooks",
        "tool_name": "Bash",
        "tool_input": {"command": "rm codex_hook_sentinel.txt"}
    });

    // Past the verified minor: the hook asked, the daemon answered 0.161.0,
    // and the question the hook emitted is bounded by that answer — nothing
    // is typed. If the hook never asked, or never used the answer, this
    // question arrives on `keys` and this assertion fails.
    let past = invoke_codex_hook_with_release(&payload, (0, 161, 0));
    assert_eq!(past.agent_type, AgentType::Codex);
    assert_eq!(past.event_type, EventType::PermissionRequest);
    let past = past
        .question()
        .expect("the approval still raises a question");
    assert_eq!(
        past.channel,
        AnswerChannel::Unsupported,
        "past the verified minor the deck types nothing: {past:?}"
    );
    assert!(
        past.questions[0].options.iter().all(|o| o.keyboard_only),
        "every option the deck would type is keyboard-only: {past:?}"
    );

    // The control: the same exchange answered with a patch inside the
    // verified minor keeps the question answerable by keys, so it is the
    // release in the reply that bounds the question, not the reply itself.
    let inside = invoke_codex_hook_with_release(&payload, (0, 160, 5));
    let inside = inside
        .question()
        .expect("the approval still raises a question");
    assert_eq!(
        inside.channel,
        AnswerChannel::Keys,
        "a patch inside the verified minor stays answerable by keys: {inside:?}"
    );
    assert!(
        inside.questions[0].options.iter().all(|o| !o.keyboard_only),
        "no option is keyboard-only inside the verified minor: {inside:?}"
    );
}
