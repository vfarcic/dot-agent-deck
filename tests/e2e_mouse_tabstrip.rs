#![cfg(feature = "e2e")]

//! PRD #80 M3 — L2 synthetic tests for tab-strip mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, opens a
//! second tab (a Mode tab) so the tab strip renders, then drives the mouse
//! via SGR reports through the `TuiDeck::click` / `find_in_grid` helpers:
//!   - mouse/tabstrip/001 — clicking a non-active tab header switches to it
//!     (same outcome as Tab / Ctrl+PageDown).
//!   - mouse/tabstrip/002 (click→close half) — clicking a Mode tab's `[×]`
//!     closes it (same outcome as Ctrl+W). The presence/absence of `[×]`
//!     across tab kinds is pinned by the L1 spec in `render_tab_strip.rs`.
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use common::TuiDeck;
use spec::spec;

/// Open a second tab (a Mode tab) so the deck has ≥2 tabs and the tab strip
/// renders. Drives Ctrl+N → directory picker → select current dir →
/// new-pane form → pick the fixture's `demo` mode → submit. Synchronizes on
/// observable screen state at each step. The tab strip only renders when
/// ≥2 tabs exist, so waiting for the `Dashboard` header to appear confirms
/// the second tab was created (the strip is hidden with a lone Dashboard).
fn open_second_tab(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    deck.send_bytes(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" "); // Space: choose current dir → new-pane form
    deck.wait_until_quiescent();
    deck.send_bytes(b"\x1b[C"); // Right: move Mode selection off "No mode" to `demo`
    deck.send_bytes(b"\r"); // Enter: Mode → Name field
    deck.send_bytes(b"\r"); // Enter: submit (Command field hidden for a mode pane)
    deck.wait_for_string("Dashboard"); // tab strip appears only with ≥2 tabs
}

/// Scenario: With a Dashboard tab and a Mode tab open (the Mode tab is
/// active after creation), click the inactive `Dashboard` tab header in the
/// top strip. The deck must switch to the Dashboard view — the same outcome
/// as pressing Tab / Ctrl+PageUp — so the empty-dashboard `No active
/// sessions` state is shown again, proving click-to-switch funnels through
/// the shared tab-switch action.
#[spec("mouse/tabstrip/001")]
#[test]
fn tabstrip_001_click_header_switches_tab() {
    let deck = TuiDeck::launch_with_fixture("modes");
    open_second_tab(&deck);

    // The Dashboard header sits in the top strip and is currently inactive
    // (the freshly-opened Mode tab is active). Click it.
    let (col, row) = deck
        .find_in_grid("Dashboard")
        .expect("tab strip should render a Dashboard header");
    deck.click(col + 1, row);

    // Switching to the Dashboard tab shows the empty-dashboard state.
    deck.wait_for_string("No active sessions");
}

/// Scenario: With a Dashboard tab and a Mode tab open, click the `[×]`
/// close glyph on the Mode tab's header. The tab must close — the same
/// outcome as Ctrl+W on that tab — leaving only the Dashboard, so the tab
/// strip collapses (it is hidden for a lone Dashboard) and no `×` glyph
/// remains on screen.
#[spec("mouse/tabstrip/002")]
#[test]
fn tabstrip_002_click_close_glyph_closes_tab() {
    let deck = TuiDeck::launch_with_fixture("modes");
    open_second_tab(&deck);

    // Click the Mode tab's close affordance.
    let (col, row) = deck
        .find_in_grid("×")
        .expect("Mode tab header should render a [×] close affordance");
    deck.click(col, row);

    // Tab closed → back to a lone Dashboard → strip hidden, no × remains.
    deck.wait_until_quiescent();
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains('×'),
        "closing the Mode tab should remove its [×]; grid still shows one:\n{grid}"
    );
}
