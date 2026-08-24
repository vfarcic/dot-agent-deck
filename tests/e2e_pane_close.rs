#![cfg(feature = "e2e")]

//! PTY-attached close-confirmation coverage for dashboard cards.

mod common;

use std::time::Duration;

use common::{TuiDeck, write_hook_line};
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

/// Scenario: Launch a dashboard with one live card, return from PaneInput to command mode, press Ctrl+W, then move from default Cancel to Close and confirm. The modal must appear before teardown, and the card plus daemon agent record must disappear only after confirmation.
#[spec("dashboard/pane/002")]
#[test]
fn pane_002_confirmed_ctrl_w_removes_card() {
    let deck = TuiDeck::builder()
        .with_continue_session("close-card", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("close-card");

    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "close-card",
            true,
            Duration::from_secs(1),
        ),
        "opening confirmation must not close the selected card"
    );

    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");
    deck.wait_for_string("No active sessions");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "close-card",
            false,
            Duration::from_secs(5),
        ),
        "confirming Close must remove the daemon agent record"
    );
}

/// Scenario: Launch an empty dashboard with no selected card and press Ctrl+W, then open Help as a processed-input sentinel. No close confirmation may appear and the lone Dashboard must remain rendered and usable.
#[spec("dashboard/pane/003")]
#[test]
fn pane_003_empty_dashboard_never_opens_close_confirmation() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(b"\x17");
    deck.send_keys(b"?");
    deck.wait_for_string("Create new pane");

    let grid = deck.snapshot_grid();
    assert!(grid.contains("Dashboard"), "{grid}");
    assert!(
        !grid.contains("Close selected pane?"),
        "an empty dashboard must not arm a close confirmation\n{grid}"
    );
    assert!(
        !grid.contains("Close this tab and all its panes?"),
        "an empty dashboard must not leak a tab-scoped close confirmation\n{grid}"
    );
}

/// Scenario: Launch one live dashboard card and drive Ctrl+W through the real binary twice. Enter on the Cancel-default modal must preserve the card and daemon agent, while a fresh Ctrl+W followed by Down+Enter must remove both.
#[spec("prompt/close-confirm/002")]
#[test]
fn close_confirm_002_real_cancel_preserves_and_confirm_closes() {
    let deck = TuiDeck::builder()
        .with_continue_session("confirm-target", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("confirm-target");

    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");
    deck.send_keys(b"\r"); // Enter on the default Cancel option.
    deck.wait_for_absence("Close selected pane?");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "confirm-target",
            true,
            Duration::from_secs(1),
        ),
        "Cancel must preserve the daemon agent and its card"
    );

    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");
    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");
    deck.wait_for_string("No active sessions");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "confirm-target",
            false,
            Duration::from_secs(5),
        ),
        "Down+Enter on Close must remove the daemon agent"
    );
}

/// Scenario: Launch one live card at a roomy width, click the real persistent `[Close Ctrl+W]` button, cancel, then press Ctrl+W. Both production input paths must display the same Cancel-default close modal while leaving the agent alive until explicit confirmation.
#[spec("prompt/close-confirm/003")]
#[test]
fn close_confirm_003_real_button_and_key_share_confirmation() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("button-target", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("[Back to Pane Ctrl+D]");

    let (col, row) = deck
        .find_in_grid("[Close Ctrl+W]")
        .expect("the command-mode button bar must render the Close button");
    deck.click(col + 1, row);
    deck.wait_for_string("Close selected pane?");
    let clicked_modal = deck.snapshot_grid();
    assert!(clicked_modal.contains("> Cancel"), "{clicked_modal}");
    assert!(clicked_modal.contains("  Close"), "{clicked_modal}");

    deck.send_keys(b"\r");
    deck.wait_for_absence("Close selected pane?");
    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");
    let keyed_modal = deck.snapshot_grid();
    assert!(keyed_modal.contains("> Cancel"), "{keyed_modal}");
    assert!(keyed_modal.contains("  Close"), "{keyed_modal}");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "button-target",
            true,
            Duration::from_secs(1),
        ),
        "neither input path may tear down the agent before explicit confirmation"
    );
}

/// Scenario: Launch one live card, send a single PTY burst containing the real `[Close Ctrl+W]` mouse click plus Down+Enter that was queued before the modal could render, and verify the burst cannot confirm it. After the modal is visibly drawn, a fresh Down+Enter must close the card.
#[spec("prompt/close-confirm/004")]
#[test]
fn close_confirm_004_pre_render_mouse_burst_cannot_confirm() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("queued-input-target", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("[Back to Pane Ctrl+D]");

    let (col, row) = deck
        .find_in_grid("[Close Ctrl+W]")
        .expect("the command-mode button bar must render the Close button");
    let click_col = col + 1;
    let sgr_col = click_col + 1;
    let sgr_row = row + 1;
    let burst = format!("\x1b[<0;{sgr_col};{sgr_row}M\x1b[<0;{sgr_col};{sgr_row}m\x1b[B\r");
    deck.send_bytes(burst.as_bytes());

    deck.wait_for_string("Close selected pane?");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "queued-input-target",
            true,
            Duration::from_secs(1),
        ),
        "Down+Enter queued behind the arming mouse event must be drained before the modal renders"
    );
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("> Cancel"),
        "the queued Down key must not move the selection\n{grid}"
    );

    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");
    deck.wait_for_string("No active sessions");
}

/// Scenario: Arm a real dashboard card's close confirmation, then genuinely replace the pane's agent — stop it and start a fresh one on the same pane id — and deliver the replacement's own SessionStart so the armed session identity is superseded before confirmation. Down+Enter must close nothing, retain the live daemon agent/card, and surface that the armed target is gone rather than retargeting the replacement.
#[spec("prompt/close-confirm/005")]
#[test]
fn close_confirm_005_vanished_armed_session_closes_nothing() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("vanish-target", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("vanish-target");

    let record = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.display_name.as_deref() == Some("vanish-target"))
        .expect("continued pane must have a daemon AgentRecord");
    let pane_id = record
        .pane_id_env
        .clone()
        .expect("continued pane must retain its stable pane id");
    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");

    // Issue #318: the generation change this test needs used to be FORGED — a
    // synthetic `SessionStart` naming an `agent_id` nobody had ever spawned.
    // That is precisely what the provenance binding now refuses: the daemon
    // derives `(pane_id, agent_id)` from the sender's capability token and
    // ignores what the payload claims, so an event can no longer assert a
    // generation it is not. The replacement is therefore a REAL one — the pane's
    // agent is stopped and a fresh one started on the same pane id, which is what
    // a `clear = true` delegate respawn does in production — and the supersession
    // is announced by that new agent's own token.
    let stopped = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: record.id.clone(),
        },
    )
    .expect("stop the armed pane's agent over the attach socket");
    assert!(
        stopped.error.is_none(),
        "StopAgent should succeed, got error: {:?}",
        stopped.error
    );
    let restarted = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StartAgent {
            command: Some("cat".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane_id.clone())],
            display_name: Some("vanish-target".into()),
            tab_membership: None,
            agent_type: None,
            seed: None,
        },
    )
    .expect("start the replacement agent on the same pane over the attach socket");
    assert!(
        restarted.error.is_none(),
        "StartAgent should succeed, got error: {:?}",
        restarted.error
    );
    let token = restarted
        .agent_token
        .clone()
        .expect("StartAgent must return the replacement agent's hook capability token");

    let replacement = serde_json::json!({
        "session_id": "replacement-generation",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-07-29T12:00:00Z",
        "pane_id": pane_id,
    });
    write_hook_line(
        deck.hook_socket_path(),
        &replacement.to_string(),
        Some(&token),
    )
    .expect("write replacement SessionStart hook");
    deck.wait_for_string("ClaudeCode");

    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");
    deck.wait_for_string("Nothing closed");
    assert!(deck.snapshot_grid().contains("vanish-target"));
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "vanish-target",
            true,
            Duration::from_secs(1),
        ),
        "confirmation for a vanished session must not retarget its replacement placeholder"
    );
}
