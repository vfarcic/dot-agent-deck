#![cfg(all(feature = "e2e", unix))]

//! L2 (real-binary PTY) coverage for the dashboard's card density ladder.
//!
//! Issue #1568 added a 3-row Minimal density that the deck uses only when the
//! 5-row Compact card cannot fit every card. The L1 tests in
//! `tests/render_dashboard.rs` pin the layout decision through the
//! `render_card_grid_to_buffer` seam; this file proves the running binary makes
//! the same decision in a real terminal, with cards that arrive over the hook
//! socket. Decision 6: gated behind the `e2e` feature so `cargo test-fast`
//! never compiles it.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// Eight cards: more than a 30-row terminal holds at Compact (8 x 5 = 40 rows)
/// and fewer than it holds at Minimal (8 x 3 = 24 rows), whatever the hints bar
/// wraps to at this width.
const CARDS: usize = 8;
const COLS: u16 = 79;
const ROWS: u16 = 30;

/// Inject a synthetic Claude Code `SessionStart` hook so a dashboard card
/// exists. Mirrors `e2e_mouse_inline.rs`.
fn send_session_start(deck: &TuiDeck, session_id: &str, pane_id: &str, cwd: &str) {
    let event = serde_json::json!({
        "session_id": session_id,
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-06-07T12:00:00Z",
        "pane_id": pane_id,
        "cwd": cwd,
    });
    write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write SessionStart hook to per-test socket");
}

/// Every card's height, top to bottom, read off the corners in column 0 —
/// at 79 columns the deck draws one card column, so each card starts and ends
/// at the left edge of the screen.
fn card_heights(grid: &str) -> Vec<usize> {
    let mut heights = Vec::new();
    let mut top = None;
    for (y, line) in grid.lines().enumerate() {
        match line.chars().next() {
            Some('┌' | '┏') => top = Some(y),
            Some('└' | '┗') => {
                if let Some(t) = top.take() {
                    heights.push(y - t + 1);
                }
            }
            _ => {}
        }
    }
    heights
}

/// Scenario: Start the deck in a 79x30 terminal and send eight agents' session
/// starts over the hook socket — more than the screen holds at the 5-row
/// Compact card. Every one of the eight cards appears on screen at the 3-row
/// Minimal height, each with its `Dir:` row and `Last:` counter, and the deck
/// title shows no hidden-card marker, so nothing needs scrolling.
#[spec("dashboard/density/009")]
#[test]
fn density_009_real_binary_draws_every_card_at_minimal() {
    let deck = TuiDeck::builder()
        .with_pty_size(COLS, ROWS)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    let ids: Vec<String> = (1..=CARDS).map(|i| format!("minimal-{i:02}")).collect();
    for (i, id) in ids.iter().enumerate() {
        send_session_start(&deck, id, &format!("pane-min-{i:02}"), "/tmp/density-dir");
    }

    deck.wait_until_grid("every card drawn at the Minimal height", |grid| {
        card_heights(grid) == vec![3; CARDS] && ids.iter().all(|id| grid.contains(id.as_str()))
    });

    let grid = deck.snapshot_grid();
    for id in &ids {
        assert!(
            grid.contains(id.as_str()),
            "card {id} must be on screen without scrolling:\n{grid}"
        );
    }
    assert_eq!(
        grid.matches("Dir:  density-dir").count(),
        CARDS,
        "every Minimal card keeps its Dir row:\n{grid}"
    );
    assert_eq!(
        grid.matches("Last:").count(),
        CARDS,
        "every Minimal card keeps its bottom-border counters:\n{grid}"
    );
    assert!(
        !grid.contains('↓') && !grid.contains('↑'),
        "no card is hidden, so the deck title carries no scroll marker:\n{grid}"
    );
}
