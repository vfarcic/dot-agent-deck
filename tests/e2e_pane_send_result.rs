#![cfg(feature = "e2e")]

//! Synthetic L2 coverage for history-only input delivery and visible feedback.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::json;
use spec::spec;

#[cfg(unix)]
fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write send-result recorder");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod send-result recorder");
}

/// Scenario: Launch a real dashboard with a long-lived synthetic pane, then
/// identify its Codex session as history-only through a synthetic hook event.
/// An atomic send must report `history-only`, and attempting to enter that card
/// from the dashboard must retain the card and visibly explain why input was not sent.
#[spec("prompt/pane-input/004")]
#[test]
fn pane_input_004_history_only_send_reports_result_and_feedback() {
    let deck = TuiDeck::builder()
        .with_continue_session("history-codex", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");

    let records = common::agent_records_on(deck.attach_socket_path());
    let record = records
        .iter()
        .find(|record| record.display_name.as_deref() == Some("history-codex"))
        .or_else(|| records.first())
        .expect("restored synthetic pane must have a daemon record");
    let pane_id = record
        .pane_id_env
        .clone()
        .expect("restored synthetic pane must have a daemon pane id");
    let agent_id = record.id.clone();
    let event = json!({
        "session_id": "history-codex-session",
        "agent_type": "codex",
        "event_type": "session_start",
        "timestamp": "2026-07-15T12:00:00Z",
        "pane_id": pane_id,
        "agent_id": agent_id,
        "live_target": {
            "kind": "process",
            "writable": "history-only"
        }
    });
    common::write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("inject history-only Codex SessionStart");
    deck.wait_for_absence("No agent");

    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::WriteAndSubmit {
            pane_id: pane_id.clone(),
            text: "this must not reach a history-only target".to_string(),
        },
    )
    .expect("send input request");
    let response_json = serde_json::to_value(response).expect("serialize send response");
    let send_result = response_json.get("send_result").cloned();

    deck.send_keys(b"1");
    let feedback = "History-only session cannot accept live input";
    let feedback_visible = deck.wait_for_grid_string_within(feedback, Duration::from_secs(2));
    let grid = deck.snapshot_grid();

    assert_eq!(
        send_result,
        Some(json!("history-only")),
        "the daemon must return the honest history-only SendResult; feedback_visible={feedback_visible}\nFinal grid:\n{grid}"
    );
    assert!(
        feedback_visible,
        "the dashboard must surface `{feedback}` instead of entering PaneInput or silently dropping the send\nFinal grid:\n{grid}"
    );
    assert!(
        grid.contains("Codex"),
        "a rejected history-only send must not remove the dashboard card\nFinal grid:\n{grid}"
    );
}

/// Scenario: Open an orchestration whose start role declares itself
/// history-only, let the real spawn-time prompt action receive that non-applied
/// result, then transition the same role to live. The UI must show feedback,
/// avoid marking the role Working, retain the prompt, and deliver it after live.
#[spec("prompt/pane-input/007")]
#[test]
#[cfg(unix)]
fn pane_input_007_orchestrator_prompt_retries_after_non_applied_result() {
    const MARKER: &str = "ORCHESTRATORRESULTMARKER20";
    let deck = TuiDeck::launch_with_fixture("send-result-orchestration");
    deck.wait_for_string("No active sessions");
    let script = deck.workdir().join("orchestrator-send-result.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
emit_target() {
    WRITABLE="$1" python3 - <<'PY'
import datetime
import json
import os
import socket

pane = os.environ["DOT_AGENT_DECK_PANE_ID"]
payload = {
    "session_id": "orchestrator-send-result-session",
    "agent_type": "codex",
    "event_type": "session_start",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "pane_id": pane,
    "agent_id": os.environ.get("DOT_AGENT_DECK_AGENT_ID"),
    "live_target": {
        "kind": "pty" if os.environ["WRITABLE"] == "live" else "process",
        "writable": os.environ["WRITABLE"],
    },
}
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.environ["DOT_AGENT_DECK_SOCKET"])
s.sendall((json.dumps(payload) + "\n").encode())
s.close()
PY
}

emit_target history-only
while [ ! -f allow-live-target ]; do sleep 0.05; done
emit_target live
while IFS= read -r line; do printf '%s\n' "$line" >> orchestrator-prompt.log; done
"#,
    );

    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");

    let feedback = deck.wait_for_grid_string_within(
        "History-only session cannot accept live input",
        Duration::from_secs(5),
    );
    let marked_working = deck.snapshot_grid().contains("Working");
    std::fs::write(deck.workdir().join("allow-live-target"), "")
        .expect("allow synthetic role to become live");
    let delivered = common::wait_for_file_substr_count(
        &deck.workdir().join("orchestrator-prompt.log"),
        MARKER,
        1,
        Duration::from_secs(10),
    );
    let grid = deck.snapshot_grid();

    assert!(
        feedback,
        "the orchestrator prompt's HistoryOnly result must surface visible feedback\nFinal grid:\n{grid}"
    );
    assert!(
        !marked_working,
        "a role whose prompt was not delivered must not be marked Working\nFinal grid:\n{grid}"
    );
    assert!(
        delivered,
        "the non-delivered orchestrator prompt must be retained and retried after the role becomes live\nFinal grid:\n{grid}"
    );
}
