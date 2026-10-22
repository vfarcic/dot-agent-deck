#![cfg(all(feature = "e2e", unix))]

//! PTY-attached coverage for input that reaches a confirmation dialog before
//! the dialog's first frame.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch one live card and, from command mode, send Ctrl+C, Down, Enter and `y` in one write. The Quit dialog appears with Detach still selected, no Stop confirmation follows, and the agent keeps running. Then click Quit's `[Stop]` button with a `y` in the same write: the Stop confirmation appears with No selected and the agent keeps running. A `y` sent once that confirmation is on screen stops the deck (control).
#[spec("prompt/quit/006")]
#[test]
fn quit_006_input_queued_before_a_confirmation_is_drawn_chooses_nothing() {
    let mut deck = TuiDeck::builder()
        .with_pty_size(160, 40)
        .with_continue_session("stop-target", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("[Back to Pane d/Ctrl+D]");

    deck.send_bytes(b"\x03\x1b[B\ry");
    deck.wait_until_grid_then_hold(
        "Quit open on Detach, no Stop confirmation",
        Duration::from_secs(1),
        |grid| {
            grid.contains("Quit dot-agent-deck?")
                && grid.contains("> Detach")
                && !grid.contains("will be terminated")
        },
    );
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "stop-target",
            true,
            Duration::from_secs(1),
        ),
        "keys queued behind Ctrl+C stopped the agent"
    );

    let (col, row) = deck.wait_for_in_grid("[Stop]");
    let sgr_col = col + 2;
    let sgr_row = row + 1;
    let burst = format!("\x1b[<0;{sgr_col};{sgr_row}M\x1b[<0;{sgr_col};{sgr_row}my");
    deck.send_bytes(burst.as_bytes());
    deck.wait_until_grid_then_hold(
        "Stop confirmation open on No",
        Duration::from_secs(1),
        |grid| grid.contains("will be terminated") && grid.contains("> No"),
    );
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "stop-target",
            true,
            Duration::from_secs(1),
        ),
        "a `y` queued behind the [Stop] click stopped the agent"
    );

    deck.send_keys(b"y");
    assert!(
        deck.wait_for_exit_within(Duration::from_secs(15)).is_some(),
        "control: `y` on the drawn Stop confirmation did not stop the deck"
    );
}
