#![cfg(feature = "e2e")]

//! PRD #80 M2 — L2 synthetic test for the global button bar.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, finds
//! the `New Pane` button in the persistent bottom button bar, and clicks
//! it via an SGR mouse report. The click must produce the SAME outcome as
//! pressing Ctrl+N — the directory picker (`Select Directory`) opens —
//! proving click and keyboard funnel into the one shared `Action`.
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch the deck against the `minimal` fixture, wait for the
/// empty dashboard, locate the `[New Pane Ctrl+N]` button in the bottom
/// button bar, and left-click it. The same directory picker that Ctrl+N
/// opens (titled `Select Directory`) must appear — demonstrating
/// click→action parity through the shared dispatch funnel.
#[spec("mouse/buttonbar/003")]
#[test]
fn buttonbar_003_click_new_pane_opens_picker() {
    let deck = TuiDeck::launch_with_fixture("minimal");

    // Empty dashboard rendered → the bottom button bar is on screen.
    deck.wait_for_string("No active sessions");

    // Find the New Pane button by its on-screen label and click inside it.
    let (col, row) = deck
        .find_in_grid("[New Pane")
        .expect("button bar should render a New Pane button");
    deck.click(col + 1, row);

    // Ctrl+N's outcome: the directory picker opens. Same action, via click.
    deck.wait_for_string("Select Directory");
}
