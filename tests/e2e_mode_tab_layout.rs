#![cfg(feature = "e2e")]

//! PRD #311 — regression guard for the Mode tab's side-pane column.
//!
//! PRD #311 removes `PaneLayout::Stacked`'s collapsed-frame arm on the
//! Orchestration/Dashboard pane column, but `pane_layout` is one global field
//! (`src/ui.rs:1531`) read by all three `render_terminal_panes` call sites
//! (Open Question 2). A Mode tab's agent + side panes render through their
//! own `render_mode_tab` call sites, which today hardcode `PaneLayout::Stacked`
//! (agent, single pane) and `PaneLayout::Tiled` (side panes) rather than
//! reading the global — so a Mode tab is not collapsing its side panes today.
//! This test pins that fact so a future refactor of the Stacked arm (or of
//! `render_mode_tab`) cannot accidentally wire the global `pane_layout` into
//! the side-pane column, which — since the global defaults to `Stacked` —
//! would immediately collapse every side pane but one to a 1-row title bar.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch the deck against the `mode-two-side-panes` fixture (one
/// mode with TWO persistent side panes, each printing a unique sentinel and
/// idling) and open the Mode tab. With the deck's default `pane_layout`
/// (`Stacked`), assert BOTH side panes' sentinels are visible in the grid AT
/// THE SAME TIME — proving the side-pane column renders every side pane
/// simultaneously rather than collapsing all but one to a titled 1-row frame,
/// which is what a Stacked-style collapse would do to the second pane.
#[spec("tabs/mode/001")]
#[test]
fn mode_001_side_panes_render_simultaneously_under_stacked_global() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 32)
        .launch_with_fixture("mode-two-side-panes");
    deck.wait_for_string("No active sessions");

    // Open the deck's single `demo` mode (Ctrl+N -> directory picker -> current
    // dir -> new-pane form -> Right to select `demo` -> Submit).
    deck.send_bytes(b"\x0e"); // Ctrl+N
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" ");
    deck.wait_for_string("Mode:");
    deck.send_bytes(b"\x1b[C"); // Right: "No mode" -> "demo"
    deck.wait_for_string("demo mode");
    let (scol, srow) = deck.wait_for_in_grid("[Submit]");
    deck.click(scol, srow);
    deck.wait_for_string("Dashboard"); // tab strip appears only with >=2 tabs

    // Both persistent side panes must be alive and visible at once — neither
    // collapsed to a titled frame with no content.
    deck.wait_for_string("SIDE_ALPHA_SENTINEL");
    deck.wait_for_string("SIDE_BETA_SENTINEL");
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("SIDE_ALPHA_SENTINEL") && grid.contains("SIDE_BETA_SENTINEL"),
        "both Mode-tab side panes must render simultaneously under the default \
         (Stacked) global pane_layout, not one collapsed to a title-only row:\n{grid}"
    );
}
