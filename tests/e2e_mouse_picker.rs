#![cfg(feature = "e2e")]

//! PRD #80 M7 — L2 synthetic tests for directory-picker mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, opens the
//! directory picker via Ctrl+N, and drives the mouse via SGR reports through
//! `TuiDeck::click` / `find_in_grid` / `send_bytes`. Each interaction is
//! asserted to equal the corresponding keystroke. Decision 6: gated behind
//! the `e2e` feature so `cargo test-fast` never compiles it.
//!
//! Deterministic layout: the `picker` fixture is copied into the per-test
//! tempdir (the launch cwd, where the picker opens) and contains a known
//! nested subdir `childdir/grandkid`, alongside the harness-created `home`
//! sibling dir. So `childdir` is a clickable row, double-clicking it reveals
//! `grandkid`, and going back up reveals `home` again.

mod common;

use common::TuiDeck;
use spec::spec;

/// Open the directory picker (Ctrl+N) from the dashboard and wait for it.
fn open_picker(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    deck.send_bytes(b"\x0e"); // Ctrl+N → New Pane → directory picker
    deck.wait_for_string("Select Directory");
    deck.wait_for_string("childdir"); // fixture's known subdir row is listed
}

/// Click the button/affordance whose label text is `needle`.
fn click_target(deck: &TuiDeck, needle: &str) {
    let (col, row) = deck
        .find_in_grid(needle)
        .unwrap_or_else(|| panic!("expected a clickable {needle} affordance in the picker"));
    deck.click(col, row);
}

/// Scenario: Open the picker and single-click the `childdir` row. That row
/// must become the highlighted/selected row — the `> ` selection marker
/// moves onto it (`> childdir/`), the same row a j/k highlight would land on.
/// RED until M7 adds clickable row hit-testing (today a click doesn't move
/// the picker's selection, which starts on `..`).
#[spec("mouse/picker/001")]
#[test]
fn picker_001_single_click_selects_row() {
    let deck = TuiDeck::launch_with_fixture("picker");
    open_picker(&deck);

    let (col, row) = deck
        .find_in_grid("childdir")
        .expect("childdir row should be listed");
    deck.click(col, row);
    // Deterministic wait IS the assertion: the childdir row gains the "> "
    // selection marker.
    deck.wait_until_grid("childdir row selected (> marker)", |g| {
        g.lines().any(|l| l.contains("> childdir"))
    });
}

/// Scenario: Open the picker and double-click the `childdir` row. It must
/// descend into that directory — the same outcome as Enter / l — so the
/// listing changes to childdir's contents, revealing its `grandkid` subdir.
/// RED until M7 adds clickable row descend (double-click).
#[spec("mouse/picker/001")]
#[test]
fn picker_001_double_click_enters_dir() {
    let deck = TuiDeck::launch_with_fixture("picker");
    open_picker(&deck);

    let (col, row) = deck
        .find_in_grid("childdir")
        .expect("childdir row should be listed");
    deck.click(col, row);
    deck.click(col, row); // second click within the double-click window

    // Descended into childdir → its child `grandkid` is now listed.
    deck.wait_for_string("grandkid");
}

/// Scenario: Descend into `childdir` via the keyboard (deterministic setup),
/// then single-click the `..` parent affordance. It must go up one directory
/// — the same outcome as h / Backspace / Left — so the listing returns to
/// the parent, where the sibling `home` directory is visible again. RED until
/// M7 makes the parent affordance clickable. (Assumption: the `..` entry /
/// breadcrumb is the parent affordance and a click on it navigates up.)
#[spec("mouse/picker/001")]
#[test]
fn picker_001_click_parent_goes_up() {
    let deck = TuiDeck::launch_with_fixture("picker");
    open_picker(&deck);

    // Keyboard: move highlight from `..` (index 0) to `childdir` (index 1)
    // and enter it, so we are deterministically inside childdir.
    deck.send_bytes(b"j");
    deck.send_bytes(b"l");
    deck.wait_for_string("grandkid");

    // Click the parent affordance to go back up.
    click_target(&deck, "..");

    // Back at the parent dir → the sibling `home` directory is listed again.
    deck.wait_for_string("home");
}

/// Scenario: Open the picker and click its `[Cancel]` button. The picker must
/// close — the same outcome as q / Esc — returning to the dashboard. RED
/// until M7 renders the `[Cancel]` button. ([Confirm] descent to the new-pane
/// form is covered separately; row double-click covers directory descent.)
#[spec("mouse/picker/001")]
#[test]
fn picker_001_click_cancel_closes() {
    let deck = TuiDeck::launch_with_fixture("picker");
    open_picker(&deck);

    click_target(&deck, "[Cancel]");

    // Picker closed → dashboard returns.
    deck.wait_for_string("No active sessions");
}

/// Scenario: Open the picker and click its `[Confirm]` button. It must
/// confirm the current directory — the same outcome as Space — advancing to
/// the new-pane form (the `Name:` field appears). RED until M7 renders the
/// `[Confirm]` button.
#[spec("mouse/picker/001")]
#[test]
fn picker_001_click_confirm_opens_form() {
    let deck = TuiDeck::launch_with_fixture("picker");
    open_picker(&deck);

    click_target(&deck, "[Confirm]");

    // Confirmed the directory → the new-pane form opens.
    deck.wait_for_string("Name:");
}

/// Scenario: Open the picker and click its `[Filter]` affordance. It must
/// open the picker's filter input — the same outcome as `/` — so the
/// filtering footer (`Enter: accept filter`) appears. RED until M7 renders
/// the clickable filter affordance.
#[spec("mouse/picker/001")]
#[test]
fn picker_001_click_filter_opens_filter() {
    let deck = TuiDeck::launch_with_fixture("picker");
    open_picker(&deck);

    click_target(&deck, "[Filter]");

    // Filter mode active → the filtering footer hint is shown.
    deck.wait_for_string("Enter: accept filter");
}
