#![cfg(feature = "e2e")]

//! PRD #80 M6 — L2 synthetic tests for inline-edit + PaneInput mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY and drives
//! the mouse via SGR reports through `TuiDeck::click` / `find_in_grid` /
//! `send_bytes`. Covers the filter-row `[Apply]`/`[Cancel]`, rename-row
//! `[Save]`/`[Cancel]`, click-in-field focus retention, and the PaneInput
//! `[Detach Ctrl+D]` affordance — each asserted to equal the corresponding
//! keystroke. Decision 6: gated behind the `e2e` feature so `cargo
//! test-fast` never compiles it.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// Inject a synthetic Claude Code `SessionStart` hook so a dashboard card
/// exists (filter/rename operate on cards). Mirrors `e2e_hook_delivery.rs`.
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

/// Click the button/affordance whose label text is `needle`.
fn click_button(deck: &TuiDeck, needle: &str) {
    let (col, row) = deck
        .find_in_grid(needle)
        .unwrap_or_else(|| panic!("expected a clickable {needle} affordance on screen"));
    deck.click(col, row);
}

/// Scenario: With a card present, press `/` to enter filter mode and type
/// `zq`; click inside the filter input field (it must keep input focus), type
/// `x`, then click `[Apply]`. The filter applies exactly as Enter does — the
/// app returns to Normal (the global `[New Pane Ctrl+N]` bar is back) with
/// the `zqx` filter still active, so the non-matching `alpha` card stays
/// hidden. (Click-in-field is asserted as focus-retention/typing-still-
/// captured; exact cursor column is not read from the vt100 grid.) RED until
/// M6 renders the inline `[Apply]` button.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_filter_apply_commits() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    deck.send_bytes(b"/"); // enter filter mode
    deck.send_bytes(b"zq");
    deck.wait_for_string("zq");

    // Click inside the filter input field — focus must stay in the input.
    click_button(&deck, "/ zq");
    deck.send_bytes(b"x");
    deck.wait_for_string("zqx");

    click_button(&deck, "[Apply]");

    // Applied like Enter: Normal mode (button bar back), filter still active.
    deck.wait_for_string("[New Pane Ctrl+N]");
    assert!(
        !deck.snapshot_grid().contains("alpha"),
        "applied 'zqx' filter should keep the non-matching alpha card hidden:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: With a card present, press `/` to enter filter mode and type a
/// non-matching sentinel, then click `[Cancel]`. The filter is abandoned
/// exactly as Esc does — the app returns to Normal and the `alpha` card
/// (hidden while the sentinel filter was live) is visible again. RED until
/// M6 renders the inline `[Cancel]` button.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_filter_cancel_abandons() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    deck.send_bytes(b"/");
    deck.send_bytes(b"zqx");
    deck.wait_for_string("zqx");

    click_button(&deck, "[Cancel]");

    // Abandoned like Esc: filter cleared, the alpha card returns.
    deck.wait_for_string("[New Pane Ctrl+N]");
    deck.wait_for_string("alpha");
}

/// Scenario: With a selected card, press `r` to enter rename mode, type a
/// new name, then click `[Save]`. The rename commits exactly as Enter does —
/// the new name shows on the card. RED until M6 renders the inline `[Save]`
/// button.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_rename_save_commits() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    deck.send_bytes(b"r"); // enter rename mode for the selected card
    deck.wait_for_string("Rename:");
    deck.send_bytes(b"renamed7");
    deck.wait_for_string("renamed7");

    click_button(&deck, "[Save]");

    // Committed like Enter: the new name shows on the card.
    deck.wait_for_string("renamed7");
    deck.wait_for_string("[New Pane Ctrl+N]"); // back to Normal
}

/// Scenario: With a selected card, press `r` to enter rename mode, type a
/// new name, then click `[Cancel]`. The rename is abandoned exactly as Esc
/// does — the card keeps its original `alpha` name and the typed name is not
/// applied. RED until M6 renders the inline `[Cancel]` button on the rename
/// row.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_rename_cancel_abandons() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    deck.send_bytes(b"r");
    deck.wait_for_string("Rename:");
    deck.send_bytes(b"discarded9");
    deck.wait_for_string("discarded9");

    click_button(&deck, "[Cancel]");

    // Abandoned like Esc: original name stays, typed name not applied.
    deck.wait_for_string("alpha");
    assert!(
        !deck.snapshot_grid().contains("discarded9"),
        "cancelled rename must not apply the typed name:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Focus a real, `--continue`-spawned pane (`realpane`, running a
/// long-lived command) so the TUI enters PaneInput mode, then click the
/// `[Detach Ctrl+D]` affordance shown in that mode. It must return to the
/// dashboard exactly as Ctrl+D (`Action::DetachToNormal`) does — the
/// `PaneInput mode` status is gone. RED until M6 renders the `[Detach
/// Ctrl+D]` affordance.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_pane_input_detach_returns_to_dashboard() {
    let deck = TuiDeck::builder()
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    deck.wait_until_quiescent();
    // Ensure we're on the dashboard (in case --continue auto-focused the pane).
    deck.send_bytes(b"\x04"); // Ctrl+D → dashboard / Normal
    deck.wait_for_string("realpane");

    // Enter PaneInput by focusing the selected card (== Enter).
    deck.send_bytes(b"\r");
    deck.wait_for_string("PaneInput mode");

    // Click the detach affordance shown while in PaneInput.
    click_button(&deck, "[Detach Ctrl+D]");

    // Returned to dashboard like Ctrl+D: the PaneInput status is gone.
    deck.wait_until_quiescent();
    assert!(
        !deck.snapshot_grid().contains("PaneInput mode"),
        "clicking [Detach Ctrl+D] should leave PaneInput and return to the dashboard:\n{}",
        deck.snapshot_grid()
    );
}
