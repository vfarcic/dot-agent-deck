#![cfg(feature = "e2e")]

//! PTY-attached close-confirmation coverage for dashboard cards.

mod common;

use std::time::Duration;

use common::TuiDeck;
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
}
