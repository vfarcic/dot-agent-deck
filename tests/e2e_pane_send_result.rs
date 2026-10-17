#![cfg(all(feature = "e2e", unix))]

//! Synthetic L2 coverage for history-only input delivery and visible feedback.

mod common;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_route.rs"]
mod route;

use std::time::Duration;

use common::TuiDeck;
use serde_json::json;
use spec::spec;

/// Scenario: Launch a real dashboard with a long-lived synthetic pane, then
/// identify its Codex session as history-only through a synthetic hook event.
/// Pressing the card's key must visibly explain the history-only target instead
/// of entering PaneInput.
#[spec("prompt/pane-input/004")]
#[test]
fn pane_input_004_history_only_send_reports_result_and_feedback() {
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
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

    deck.send_keys(b"1");
    let feedback = "History-only session cannot accept live input";
    let feedback_visible = deck.wait_for_grid_string_within(feedback, Duration::from_secs(2));
    let grid = deck.snapshot_grid();

    assert!(
        feedback_visible,
        "the dashboard must surface `{feedback}` instead of entering PaneInput or silently dropping the send\nFinal grid:\n{grid}"
    );
    assert!(
        grid.contains("Codex"),
        "a rejected history-only send must not remove the dashboard card\nFinal grid:\n{grid}"
    );
}

/// Scenario: Keep a real dashboard focused in PaneInput while its attached
/// session transitions from live to history-only, then send a key and a paste
/// in separate runs. Each rejection must show feedback and leave PaneInput.
#[spec("prompt/pane-input/008")]
#[test]
fn pane_input_008_stream_rejection_surfaces_feedback_and_exits_input_mode() {
    let mut observations = Vec::new();
    for (input_kind, input) in [
        ("key", b"rejected-key".as_slice()),
        ("paste", b"\x1b[200~rejected-paste\x1b[201~".as_slice()),
    ] {
        let deck = TuiDeck::builder()
            .impersonating_pane_signals()
            .with_continue_session(format!("stream-rejection-{input_kind}"), "cat")
            .launch_with_fixture("minimal");
        deck.wait_for_string("[Command Mode Ctrl+D]");

        let record = common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .next()
            .expect("restored synthetic pane must have a daemon record");
        let pane_id = record
            .pane_id_env
            .expect("restored synthetic pane must have a daemon pane id");
        let event = json!({
            "session_id": format!("stream-rejection-{input_kind}-session"),
            "agent_type": "codex",
            "event_type": "session_start",
            "timestamp": "2026-07-15T12:00:00Z",
            "pane_id": pane_id,
            "agent_id": record.id,
            "live_target": {
                "kind": "process",
                "writable": "history-only"
            }
        });
        common::write_hook_line(deck.hook_socket_path(), &event.to_string())
            .expect("make focused synthetic session history-only");
        deck.wait_for_absence("No agent");

        deck.send_keys(input);
        let feedback = deck.wait_for_grid_string_within(
            "History-only session cannot accept live input",
            Duration::from_secs(2),
        );
        let grid = deck.snapshot_grid();
        observations.push((
            input_kind,
            feedback,
            !grid.contains("[Command Mode Ctrl+D]"),
            grid,
        ));
    }

    assert!(
        observations
            .iter()
            .all(|(_, feedback, exited, _)| *feedback && *exited),
        "key and paste rejection must both show feedback and exit PaneInput; observations={observations:#?}"
    );
}

/// Scenario: Open an orchestration through the real TUI and report its start role history-only. The remit remains Queued without a typed pointer, then the same role becoming live fetches and visibly renders it.
#[spec("prompt/pane-input/007")]
#[test]
#[cfg(unix)]
fn pane_input_007_orchestrator_prompt_retries_after_non_applied_result() {
    route::client_orchestration(false);
}
