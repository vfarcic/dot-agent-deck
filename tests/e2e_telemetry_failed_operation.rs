#![cfg(all(feature = "e2e", unix))]

//! Issue #1835: a Claude Code tool call that fails ends with
//! `PostToolUseFailure`, not `PostToolUse`. Its `agent.operation` span must end
//! when the call does, not when the run does. The real-agent half is
//! `tests/e2e_telemetry_failed_operation_live.rs`.

mod common;
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::json;
use spec::spec;
use std::io::Write;
use std::process::{Command, Stdio};
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver, attribute, nanos};

/// Scenario: Send a Bash start hook for `false` and then the `PostToolUseFailure` Claude Code fires when that call fails, both through the real hook CLI, to an attached stand-in pane. The exported operation must complete before its run, without the deck's forced-closure attribute, while the card stays Working rather than Error.
#[spec("telemetry/otlp/045")]
#[test]
fn telemetry_otlp_045_failed_claude_call_ends_its_operation() {
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "failed-operation-standin",
            "sh -c 'echo FAILED_OPERATION_READY; exec cat'",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("FAILED_OPERATION_READY");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    let pane = agent.pane_id_env.as_deref().unwrap();

    let hook = |kind: &str, extra: serde_json::Value| {
        let mut payload = json!({
            "session_id": "failed-operation-synthetic", "hook_event_name": kind,
            "tool_name": "Bash", "tool_use_id": "failed-call",
            "tool_input": {"command": "false"}, "cwd": deck.workdir()
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut child = Command::new(common::contained_deck_binary())
            .args(["hook", "--agent", "claude-code"])
            .env_clear()
            .env("HOME", deck.home_dir())
            .env("PATH", "/usr/bin:/bin")
            .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
            .env("DOT_AGENT_DECK_PANE_ID", pane)
            .env("DOT_AGENT_DECK_AGENT_ID", &agent.id)
            .env("OTEL_SDK_DISABLED", "true")
            .current_dir(deck.workdir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{kind} CLI failed: {output:?}");
    };

    hook("PreToolUse", json!({}));
    deck.wait_for_string("Working");
    // The payload shape Claude Code 2.1.314 sends for a Bash `false`.
    hook(
        "PostToolUseFailure",
        json!({"error": "Exit code 1", "is_interrupt": false, "duration_ms": 104}),
    );
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.operations(pane).len() == 1),
        "a failed call's operation must export when the call ends, not at run end (issue #1835)"
    );
    assert!(
        receiver.run(pane).is_none(),
        "operation must finish while run is open"
    );
    let operation = receiver.operations(pane).remove(0);
    assert_eq!(
        attribute(&operation, "deck.operation.program"),
        Some("false")
    );
    assert!(
        operation["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["key"] != "deck.operation.end"),
        "a call the agent ended must omit deck.operation.end: {operation}"
    );
    deck.wait_for_string("Working");
    assert!(
        !deck.snapshot_grid().contains("Error"),
        "a failed command is ordinary agent work, not an Error card:\n{}",
        deck.snapshot_grid()
    );

    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(response.ok, "stop pane: {response:?}");
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.run(pane).is_some()),
        "run never exported"
    );
    let run = receiver.run(pane).unwrap();
    assert!(nanos(&operation, "endTimeUnixNano") < nanos(&run, "endTimeUnixNano"));
    assert_eq!(receiver.operations(pane), vec![operation]);
}
