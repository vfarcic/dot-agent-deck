#![cfg(feature = "e2e")]

//! PRD #80 M4 — L2 synthetic tests for dashboard mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, gets
//! ≥1 session card on the dashboard, then drives the mouse via SGR reports
//! through the `TuiDeck::click` / `find_in_grid` / `send_bytes` helpers:
//!   - mouse/dashboard/001 — single-click selects a card (== j/k); double-
//!     click focuses its pane / enters PaneInput (== Enter on the selected
//!     card).
//!   - mouse/dashboard/002 — clickable Filter / Rename / Generate-config
//!     buttons enter the same modes the `/`, `r`, `g` keys do.
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.
//!
//! Card-selection note (PRD #68): single-click selection must match the
//! corrected j/k linear-cycling selection semantics — clicking card N
//! selects exactly card N, the same card a sequence of j/k would land on.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// Inject a synthetic Claude Code `SessionStart` hook so the daemon auto-
/// registers a dashboard card with the given ids and cwd. Mirrors
/// `e2e_hook_delivery.rs`; the `cwd` is included so `g` / the
/// Generate-config button has a directory target.
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

/// Return the first rendered grid line containing `needle`, if any.
fn grid_line_containing(deck: &TuiDeck, needle: &str) -> Option<String> {
    deck.snapshot_grid()
        .lines()
        .find(|l| l.contains(needle))
        .map(|l| l.to_string())
}

/// Scenario: With two dashboard cards present — a real `--continue`-spawned
/// pane (`realpane`, running `sleep`, so its pane is focusable) and a
/// synthetic hook card (`bravo`) — single-click the `bravo` card. Selection
/// (the `▸` marker) must move to `bravo`, the same card a j/k cycle would
/// select. Then double-click the `realpane` card: its pane must be focused
/// and the TUI must enter PaneInput mode — the same outcome as pressing
/// Enter on the selected card. RED until M4 adds card click hit-testing
/// (today a card click does not move selection or focus a pane).
#[spec("mouse/dashboard/001")]
#[test]
fn dashboard_001_click_selects_double_click_focuses() {
    // `realpane` runs a plain long-lived command → a real, focusable PTY
    // pane (no agent credentials needed) so the double-click→focus half can
    // actually enter PaneInput.
    let deck = TuiDeck::builder()
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    deck.wait_until_quiescent();
    // If --continue restored straight into the pane, Ctrl+D returns to the
    // dashboard so the cards are visible and clickable.
    deck.send_bytes(b"\x04"); // Ctrl+D → dashboard / Normal mode
    deck.wait_for_string("realpane");

    // Second card via a synthetic SessionStart hook.
    send_session_start(&deck, "bravo", "pane-bravo", "/tmp");
    deck.wait_for_string("bravo");

    // Single-click the bravo card title → selection marker moves to it.
    let (col, row) = deck
        .find_in_grid("bravo")
        .expect("bravo card should be on the dashboard");
    deck.click(col, row);
    deck.wait_until_quiescent();
    let bravo_line = grid_line_containing(&deck, "bravo").expect("bravo card row");
    assert!(
        bravo_line.contains('▸'),
        "single-click should select the bravo card (▸ marker), got: {bravo_line:?}"
    );

    // Double-click the realpane card → focus its pane (PaneInput mode).
    let (rcol, rrow) = deck
        .find_in_grid("realpane")
        .expect("realpane card should be on the dashboard");
    deck.click(rcol, rrow);
    deck.click(rcol, rrow); // second click within the double-click window
    deck.wait_for_string("PaneInput mode");
}

/// Scenario: With a session card on the dashboard, click each of the
/// dashboard's context buttons and assert it enters the same mode the
/// keyboard does: the Filter button → filter mode (typed text echoes in the
/// filter prompt, == `/`); the Rename button → rename mode (the `Rename:`
/// prompt, == `r` on the selected card); the Generate-config button → the
/// config-generation prompt (`Generate .dot-agent-deck.toml`, == `g`). Each
/// button is located by its label text via `find_in_grid`. RED until M4
/// renders these clickable buttons (today they do not exist, so the lookup
/// fails). Assumed placement: context buttons added to the bottom button
/// bar while in Dashboard / Normal mode, carrying their shortcuts inline
/// (e.g. `[Filter /]`, `[Rename r]`, `[Generate g]`).
#[spec("mouse/dashboard/002")]
#[test]
fn dashboard_002_filter_rename_generate_buttons() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    // Filter button → filter mode. Typed text echoes in the filter prompt,
    // which only happens once filter mode is active.
    let (c, r) = deck
        .find_in_grid("Filter")
        .expect("dashboard should render a clickable Filter button");
    deck.click(c, r);
    deck.send_bytes(b"zqx");
    deck.wait_for_string("zqx");
    deck.send_bytes(b"\x1b"); // Esc → back to Normal mode

    // Rename button (acts on the selected card) → rename mode.
    let (c, r) = deck
        .find_in_grid("Rename")
        .expect("dashboard should render a clickable Rename button");
    deck.click(c, r);
    deck.wait_for_string("Rename:");
    deck.send_bytes(b"\x1b"); // Esc → back to Normal mode

    // Generate-config button → config-generation prompt.
    let (c, r) = deck
        .find_in_grid("Generate")
        .expect("dashboard should render a clickable Generate-config button");
    deck.click(c, r);
    deck.wait_for_string("Generate .dot-agent-deck.toml");
}
