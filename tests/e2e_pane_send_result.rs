#![cfg(feature = "e2e")]

//! Synthetic L2 coverage for history-only input delivery and visible feedback.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::json;
use spec::spec;

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
