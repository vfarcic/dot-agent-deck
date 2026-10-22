#![cfg(all(feature = "e2e", unix))]

//! PTY-attached coverage for input that reaches a confirmation dialog before
//! the dialog's first frame.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch a synthetic `cat` card at 80×24 and send navigation and confirmation keys in the same write as keyboard or mouse openings of ConfigGen, Quit and Stop. Each prompt keeps its initial choice and the agent keeps running; after a visible frame ConfigGen navigates and cancels, and Quit opens Stop (controls). A `y` sent once Stop is on screen stops the deck (control).
#[spec("prompt/quit/006")]
#[test]
fn quit_006_input_queued_before_a_confirmation_is_drawn_chooses_nothing() {
    let mut deck = TuiDeck::builder()
        .with_pty_size(80, 24)
        .with_continue_session("stop-target", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("[Back to Pane d/Ctrl+D]");

    let click_burst = |label: &str, tail: &str| {
        let (col, row) = deck.wait_for_in_grid(label);
        let sgr_col = col + 2;
        let sgr_row = row + 1;
        let burst = format!("\x1b[<0;{sgr_col};{sgr_row}M\x1b[<0;{sgr_col};{sgr_row}m{tail}");
        deck.send_bytes(burst.as_bytes());
    };
    for mouse in [false, true] {
        if mouse {
            click_burst("Generate", "y\x1b[B\r");
        } else {
            deck.send_bytes(b"g\x1b[B\ry");
        }
        deck.wait_until_grid_then_hold(
            "ConfigGen open on Yes despite queued input",
            Duration::from_secs(1),
            |grid| grid.contains("Generate .dot-agent-deck.toml") && grid.contains("> Yes"),
        );
        deck.send_keys(b"\x1b[B");
        deck.wait_until_grid("control: drawn ConfigGen navigates to No", |grid| {
            grid.contains("Generate .dot-agent-deck.toml") && grid.contains("> No")
        });
        deck.send_keys(b"\r");
        deck.wait_until_grid("control: drawn ConfigGen No closes", |grid| {
            !grid.contains("Generate .dot-agent-deck.toml") && grid.contains("[Generate")
        });
    }

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

    deck.send_keys(b"\x1b[B");
    deck.wait_until_grid("drawn Quit selects Stop", |grid| grid.contains("> Stop"));
    deck.send_bytes(b"\ry");
    deck.wait_until_grid_then_hold(
        "keyboard-opened Stop stays on No despite queued y",
        Duration::from_secs(1),
        |grid| grid.contains("will be terminated") && grid.contains("> No"),
    );
    deck.send_keys(b"\x1b");
    deck.wait_until_grid("Stop Escape returns to Quit", |grid| {
        grid.contains("Quit dot-agent-deck?") && !grid.contains("will be terminated")
    });

    click_burst("[Stop]", "y");
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

    // The footer advertises the same Ctrl+C gesture. Its mouse opening must
    // show Quit before any buffered keys can choose Detach or Stop.
    let mut clicked = TuiDeck::builder()
        .with_pty_size(80, 24)
        .with_continue_session("mouse-quit-target", "cat")
        .launch_with_fixture("minimal");
    clicked.wait_for_string("[Command Mode Ctrl+D]");
    clicked.send_keys(b"\x04");
    clicked.wait_for_string("[Back to Pane d/Ctrl+D]");
    let (col, row) = clicked.wait_for_in_grid("Quit Ctrl+C");
    let burst = format!(
        "\x1b[<0;{};{}M\x1b[<0;{};{}my",
        col + 2,
        row + 1,
        col + 2,
        row + 1
    );
    clicked.send_bytes(burst.as_bytes());
    assert!(
        clicked
            .wait_for_exit_within(Duration::from_secs(1))
            .is_none(),
        "clicking [Quit Ctrl+C] exited without drawing the Quit confirmation"
    );
    clicked.wait_until_grid_then_hold(
        "mouse-opened Quit stays on Detach despite queued y",
        Duration::from_secs(1),
        |grid| grid.contains("Quit dot-agent-deck?") && grid.contains("> Detach"),
    );
}
