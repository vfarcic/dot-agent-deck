#![cfg(all(feature = "e2e", unix))]

//! Issue #1835: a Claude Code tool call that fails ends with
//! `PostToolUseFailure`, not `PostToolUse`. Its `agent.operation` span must end
//! when the call does, not when the run does. Issue #2093: a call the agent
//! never reports the end of — one interrupted with Escape — ends when the
//! agent's turn does, at the interrupt key. The real-agent halves are in
//! `tests/e2e_telemetry_failed_operation_live.rs`.

dot_agent_deck_test_harness::link!();
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::json;
use spec::spec;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
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
        send_hook(&deck, &agent, "failed-call", "false", kind, extra)
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
            keep_worktree: false,
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

/// Run the real `dot-agent-deck hook --agent claude-code` CLI with a Claude
/// Code-shaped payload for Bash call `call` running `command`, as `agent`'s
/// pane.
fn send_hook(
    deck: &TuiDeck,
    agent: &AgentRecord,
    call: &str,
    command: &str,
    kind: &str,
    extra: serde_json::Value,
) {
    let pane = agent.pane_id_env.as_deref().unwrap();
    let mut payload = json!({
        "session_id": "failed-operation-synthetic", "hook_event_name": kind,
        "cwd": deck.workdir()
    });
    if kind.contains("ToolUse") {
        payload.as_object_mut().unwrap().extend(
            json!({"tool_name": "Bash", "tool_use_id": call, "tool_input": {"command": command}})
                .as_object()
                .unwrap()
                .clone(),
        );
    }
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
}

fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// Scenario: Send a Bash start hook to an attached stand-in pane, press Escape in the TUI as a person interrupting the call would, then send the next turn's `UserPromptSubmit`, a short call that starts and ends, and its `Stop` — the hooks Claude Code fires when Escape interrupts a running Bash call, which fires no end of its own. The operation must export when that turn ends, while the run is still open, marked `interrupted` and ending at the Escape rather than at the `Stop`; a second call left open with no Escape ends at its turn's `Stop`, marked `turn_end`.
#[spec("telemetry/otlp/049")]
#[test]
fn telemetry_otlp_049_interrupted_call_ends_with_its_turn() {
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        // Raw input, so the stand-in records the Escape it is sent rather than
        // a line discipline eating it.
        .with_continue_session(
            "interrupted-operation-standin",
            "/bin/sh -c 'stty raw -echo; echo INTERRUPTED_OPERATION_READY; exec cat > interrupt-input.bin'",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("INTERRUPTED_OPERATION_READY");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    let pane = agent.pane_id_env.as_deref().unwrap().to_string();
    let end_of = |program: &str| {
        receiver
            .operations(&pane)
            .into_iter()
            .find(|op| attribute(op, "deck.operation.program") == Some(program))
    };

    send_hook(
        &deck,
        &agent,
        "interrupted-call",
        "make",
        "PreToolUse",
        json!({}),
    );
    let started = unix_nanos_now();
    deck.send_bytes(b"\x1b");
    let input = deck.workdir().join("interrupt-input.bin");
    assert!(
        common::wait_until(EXPORT_WAIT, || std::fs::read(&input)
            .is_ok_and(|got| got.contains(&0x1b))),
        "the stand-in never received the Escape; grid:\n{}",
        deck.snapshot_grid()
    );
    let pressed = unix_nanos_now();
    // The interrupted turn fires nothing more. The next prompt alone does not
    // prove the call over — Claude Code fires it while a command still runs.
    send_hook(
        &deck,
        &agent,
        "",
        "",
        "UserPromptSubmit",
        json!({"prompt": "go on"}),
    );
    // A barrier rather than a wait: a call (`ls`) that starts and ends after the
    // prompt exports after anything the prompt had ended, since spans leave
    // in the order they end, so once it is out the `make` call would be too.
    send_hook(&deck, &agent, "barrier-call", "ls", "PreToolUse", json!({}));
    send_hook(
        &deck,
        &agent,
        "barrier-call",
        "ls",
        "PostToolUse",
        json!({}),
    );
    assert!(
        common::wait_until(EXPORT_WAIT, || end_of("ls").is_some()),
        "the barrier call never exported: {:?}",
        receiver.operations(&pane)
    );
    assert!(
        end_of("make").is_none(),
        "a prompt must not end a call: {:?}",
        receiver.operations(&pane)
    );
    let before_stop = unix_nanos_now();
    send_hook(&deck, &agent, "", "", "Stop", json!({}));
    assert!(
        common::wait_until(EXPORT_WAIT, || end_of("make").is_some()),
        "the interrupted call must export when the next turn ends, not at run end (issue \
         #2093); exported: {:?}",
        receiver.operations(&pane)
    );
    assert!(receiver.run(&pane).is_none(), "the run must still be open");
    let interrupted = end_of("make").unwrap();
    assert_eq!(
        attribute(&interrupted, "deck.operation.end"),
        Some("interrupted"),
        "{interrupted}"
    );
    let end = nanos(&interrupted, "endTimeUnixNano");
    assert!(
        end >= started && end <= pressed && end < before_stop,
        "the span must end at the Escape, not at the Stop: {interrupted}"
    );

    // No Escape this time: the turn's end ends the call.
    send_hook(&deck, &agent, "open-call", "cargo", "PreToolUse", json!({}));
    let before_turn_end = unix_nanos_now();
    send_hook(&deck, &agent, "", "", "Stop", json!({}));
    assert!(
        common::wait_until(EXPORT_WAIT, || end_of("cargo").is_some()),
        "a call left open must end with its turn; exported: {:?}",
        receiver.operations(&pane)
    );
    let left_open = end_of("cargo").unwrap();
    assert_eq!(
        attribute(&left_open, "deck.operation.end"),
        Some("turn_end"),
        "{left_open}"
    );
    assert!(nanos(&left_open, "endTimeUnixNano") >= before_turn_end);
    assert!(receiver.run(&pane).is_none(), "the run must still be open");
}
