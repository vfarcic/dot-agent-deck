#![cfg(feature = "e2e")]

//! PRD #80 M3 — L2 synthetic tests for tab-strip mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, opens a
//! second tab (a Mode tab) so the tab strip renders, then drives the mouse
//! via SGR reports through the `TuiDeck::click` / `find_in_grid` helpers:
//!   - mouse/tabstrip/001 — clicking a non-active tab header switches to it
//!     (same outcome as Tab / Ctrl+PageDown).
//!   - mouse/tabstrip/002 (click→close half) — clicking a Mode tab's `[×]`
//!     opens the shared confirmation, whose Close choice closes it (same outcome
//!     as Ctrl+W). The presence/absence of `[×]`
//!     across tab kinds is pinned by the L1 spec in `render_tab_strip.rs`.
//!
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
fn open_mode_tab(deck: &TuiDeck, right_presses: usize, selected_mode: &str) {
    deck.send_bytes(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" "); // Space: choose current dir → new-pane form
    deck.wait_for_string("Mode:"); // form ready (Mode field present with modes)
    for _ in 0..right_presses {
        deck.send_bytes(b"\x1b[C");
    }
    deck.wait_for_string(selected_mode); // selection reflected in the title
    // Submit via the [Submit] button (deterministic — a mode pane still shows
    // the Command field, so an Enter-count would be fragile).
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render a [Submit] button");
    deck.click(scol, srow);
    deck.wait_for_string("Dashboard"); // tab strip appears only with ≥2 tabs
}

fn open_second_tab(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    open_mode_tab(deck, 1, "demo mode");
}

/// Scenario: With a Dashboard tab and a Mode tab open (the Mode tab is
/// active after creation), click the inactive `Dashboard` tab header in the
/// top strip. The deck must switch to the Dashboard view — the same outcome
/// as pressing Tab / Ctrl+PageUp — so the dashboard's session-count title
/// (`dot-agent-deck — N session(s)`, shown only on the Dashboard tab, not on
/// a Mode tab) appears, proving click-to-switch funnels through the shared
/// tab-switch action.
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

    // Switching to the Dashboard tab shows the dashboard's session-count title
    // (the Mode tab view does not render it).
    deck.wait_for_string("dot-agent-deck \u{2014}");
}

/// Scenario: With a Dashboard tab and a Mode tab open, click the `[×]`
/// close glyph on the Mode tab's header. The tab must remain until the shared
/// confirmation appears and Close is explicitly chosen, then close — the same
/// outcome as Ctrl+W on that tab — leaving only the Dashboard, so the tab strip
/// collapses and no `×` glyph remains on screen.
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

    // The glyph is a request, not a one-click teardown: the tab and its close
    // affordance remain behind the shared Cancel-default modal.
    deck.wait_for_string("Close selected pane?");
    assert!(deck.snapshot_grid().contains('×'));
    deck.send_bytes(b"\x1b[B"); // Down → select Close
    deck.send_bytes(b"\r"); // Enter → confirm

    // Tab closed → back to a lone Dashboard → strip hidden, no × remains.
    deck.wait_for_absence("×");
}

/// Scenario: Open visibly distinct `alpha` and `beta` tabs so `beta` is active, then click the inactive `alpha` tab's real `×` and try both Ctrl+PageUp and Ctrl+PageDown while its confirmation is open. The modal must suppress navigation, and confirming must close the clicked/armed `alpha` tab while the active `beta` tab remains.
#[spec("mouse/tabstrip/003")]
#[test]
fn tabstrip_003_inactive_close_binds_target_and_modal_suppresses_navigation() {
    const TAB_NEXT: &[u8] = b"\x1b[6;5~";
    const TAB_PREV: &[u8] = b"\x1b[5;5~";

    let deck = TuiDeck::launch_with_fixture("tab-close-targets");
    deck.wait_for_string("No active sessions");
    open_mode_tab(&deck, 1, "alpha mode");
    deck.wait_for_string("ALPHA_TAB_SENTINEL");
    open_mode_tab(&deck, 2, "beta mode");
    deck.wait_for_string("BETA_TAB_SENTINEL");

    // `find_in_grid` returns the first ×: alpha's, while beta remains active.
    let (col, row) = deck
        .find_in_grid("×")
        .expect("two Mode tabs must render close affordances");
    deck.click(col, row);
    deck.wait_for_string("Close selected pane?");
    assert!(deck.snapshot_grid().contains("BETA_TAB_SENTINEL"));

    // Each navigation chord is followed by a modal-selection move that makes
    // processing observable. The background must stay on beta throughout.
    deck.send_bytes(TAB_PREV);
    deck.send_bytes(b"\x1b[B");
    deck.wait_for_string("> Close");
    assert!(
        deck.snapshot_grid().contains("BETA_TAB_SENTINEL"),
        "Ctrl+PageUp must not switch the tab behind an open close modal"
    );
    deck.send_bytes(TAB_NEXT);
    deck.send_bytes(b"\x1b[A");
    deck.wait_for_string("> Cancel");
    assert!(
        deck.snapshot_grid().contains("BETA_TAB_SENTINEL"),
        "Ctrl+PageDown must not switch the tab behind an open close modal"
    );

    deck.send_bytes(b"\x1b[B");
    deck.send_bytes(b"\r");
    deck.wait_until_grid("clicked alpha tab closed while beta survives", |grid| {
        grid.contains("BETA_TAB_SENTINEL")
            && !grid.contains("ALPHA_TAB_SENTINEL")
            && grid.matches('×').count() == 1
    });
}
