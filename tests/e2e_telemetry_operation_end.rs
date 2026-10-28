#![cfg(all(feature = "e2e", unix))]

mod common;
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::EventType;
use dot_agent_deck::telemetry::tracker::MAX_OPEN_OPERATIONS;
use serde_json::json;
use spec::spec;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver, attribute, nanos};

fn launch(receiver: &Receiver) -> (TuiDeck, AgentRecord) {
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "operation-end-standin",
            "sh -c 'echo OPERATION_END_READY; exec cat'",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("OPERATION_END_READY");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    (deck, agent)
}

fn hook(deck: &TuiDeck, agent: &AgentRecord, kind: &str, call: &str, command: &str) {
    let payload = json!({
        "session_id": "operation-end-synthetic", "hook_event_name": kind,
        "tool_name": "Bash", "tool_use_id": call, "tool_input": {"command": command},
        "cwd": deck.workdir()
    });
    let mut child = Command::new(common::contained_deck_binary())
        .args(["hook", "--agent", "claude-code"])
        .env_clear()
        .env("HOME", deck.home_dir())
        .env("PATH", "/usr/bin:/bin")
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            agent.pane_id_env.as_deref().unwrap(),
        )
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

fn close(deck: &TuiDeck, agent: &AgentRecord, receiver: &Receiver) -> serde_json::Value {
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(response.ok, "stop pane: {response:?}");
    let pane = agent.pane_id_env.as_deref().unwrap();
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.run(pane).is_some()),
        "run never exported"
    );
    receiver.run(pane).unwrap()
}

fn wait_operations(receiver: &Receiver, pane: &str, count: usize) -> bool {
    common::wait_until(EXPORT_WAIT, || receiver.operations(pane).len() == count)
}

/// Scenario: Send matching Bash start and end hooks through the real CLI to an attached stand-in pane. The exported operation must complete before its run and omit the deck's forced-closure attribute, including after the pane closes.
#[spec("telemetry/otlp/021")]
#[test]
fn telemetry_otlp_021_normal_operation_has_no_forced_end() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    let pane = agent.pane_id_env.as_deref().unwrap();
    hook(&deck, &agent, "PreToolUse", "success-control", "pwd");
    deck.wait_for_string("pwd");
    hook(&deck, &agent, "PostToolUse", "success-control", "pwd");
    assert!(
        wait_operations(&receiver, pane, 1),
        "successful PostToolUse control must export before run end"
    );
    deck.wait_for_string("Working");
    assert!(!deck.snapshot_grid().contains("Error"));
    let operation = receiver.operations(pane).remove(0);
    assert_eq!(attribute(&operation, "deck.operation.program"), Some("pwd"));
    assert!(
        operation["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["key"] != "deck.operation.end"),
        "agent-ended operation must omit deck.operation.end: {operation}"
    );
    assert!(
        receiver.run(pane).is_none(),
        "operation must finish while run is open"
    );
    let run = close(&deck, &agent, &receiver);
    assert!(nanos(&operation, "endTimeUnixNano") < nanos(&run, "endTimeUnixNano"));
    assert_eq!(receiver.operations(pane), vec![operation]);
}

/// Scenario: Leave a Bash operation open in an attached stand-in pane and close the pane through the daemon. Its exported operation must end with the run and identify the deck's forced closure as run_end.
#[spec("telemetry/otlp/022")]
#[test]
fn telemetry_otlp_022_open_operation_marks_run_end() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    let pane = agent.pane_id_env.as_deref().unwrap();
    hook(&deck, &agent, "PreToolUse", "left-open", "sleep 60");
    deck.wait_for_string("sleep 60");
    let run = close(&deck, &agent, &receiver);
    assert!(
        wait_operations(&receiver, pane, 1),
        "open operation never exported"
    );
    let operation = receiver.operations(pane).remove(0);
    assert_eq!(
        attribute(&operation, "deck.operation.end"),
        Some("run_end"),
        "operation closed by run end must say why: {operation}"
    );
    assert_eq!(
        nanos(&operation, "endTimeUnixNano"),
        nanos(&run, "endTimeUnixNano")
    );
}

/// Scenario: Send one more concurrent Bash start than the open-operation limit to an attached stand-in pane. The oldest operation must export before the run closes with cap as its closure reason, while a late end for that evicted call leaves the other calls open.
#[spec("telemetry/otlp/023")]
#[test]
fn telemetry_otlp_023_open_operation_marks_cap() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    let pane = agent.pane_id_env.as_deref().unwrap();
    let events = deck.subscribe_events();
    for index in 0..=MAX_OPEN_OPERATIONS {
        hook(
            &deck,
            &agent,
            "PreToolUse",
            &format!("cap-{index}"),
            if index == 0 { "pwd" } else { "true" },
        );
    }
    events.wait_for(
        |e| {
            e.event_type == EventType::ToolStart
                && e.metadata.get("tool_use_id") == Some(&format!("cap-{MAX_OPEN_OPERATIONS}"))
        },
        Duration::from_secs(10),
    );
    assert!(
        wait_operations(&receiver, pane, 1),
        "cap must export oldest operation while run is open"
    );
    let operation = receiver.operations(pane).remove(0);
    assert_eq!(attribute(&operation, "deck.operation.program"), Some("pwd"));
    assert_eq!(
        attribute(&operation, "deck.operation.end"),
        Some("cap"),
        "operation evicted by cap must say why: {operation}"
    );
    assert!(receiver.run(pane).is_none());
    hook(&deck, &agent, "PostToolUse", "cap-0", "pwd");
    events.wait_for(
        |e| {
            e.event_type == EventType::ToolEnd
                && e.metadata.get("tool_use_id").map(String::as_str) == Some("cap-0")
        },
        Duration::from_secs(10),
    );
    let _ = close(&deck, &agent, &receiver);
    assert!(wait_operations(&receiver, pane, MAX_OPEN_OPERATIONS + 1));
    assert_eq!(
        receiver
            .operations(pane)
            .iter()
            .filter(|s| attribute(s, "deck.operation.end") == Some("run_end"))
            .count(),
        MAX_OPEN_OPERATIONS
    );
}
