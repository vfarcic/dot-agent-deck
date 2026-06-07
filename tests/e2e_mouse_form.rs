#![cfg(feature = "e2e")]

//! PRD #80 M8 — L2 synthetic tests for new-pane-form mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, opens the
//! new-pane form (Ctrl+N → Select Directory → Space-to-confirm), and drives
//! the mouse via SGR reports through `TuiDeck::click` / `find_in_grid` /
//! `send_bytes`. Each interaction is asserted to equal the corresponding
//! keystroke. Decision 6: gated behind the `e2e` feature so `cargo test-fast`
//! never compiles it.
//!
//! Fixture: the `form` fixture defines two modes (`demo`, `demo2`) so the
//! form's Mode chip selector exposes ≥2 real chips (plus the implicit
//! "No mode" option), making click-to-select-chip observable. The form opens
//! focused on the Mode field (NewPaneFormState::new).

mod common;

use common::TuiDeck;
use spec::spec;

/// Open the new-pane form: Ctrl+N → directory picker → Space confirms the
/// launch cwd → the form. Synchronizes on the form's ` New Agent ` title.
fn open_form(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    deck.send_bytes(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("New Agent");
}

/// Click the button/affordance whose label text is `needle`.
fn click_target(deck: &TuiDeck, needle: &str) {
    let (col, row) = deck
        .find_in_grid(needle)
        .unwrap_or_else(|| panic!("expected a clickable {needle} affordance in the form"));
    deck.click(col, row);
}

/// Scenario: Open the form (focused on the Mode field) and click the `Name:`
/// field. Focus must move to Name — the same as Tab landing on it — so typing
/// a sentinel lands in the Name field and is shown there. RED until M8 adds
/// click-to-focus (today a click doesn't move field focus, so the sentinel —
/// typed while focus is still on the arrow-cycling Mode field — is ignored).
#[spec("mouse/form/001")]
#[test]
fn form_001_click_field_moves_focus() {
    let deck = TuiDeck::launch_with_fixture("form");
    open_form(&deck);

    let (col, row) = deck
        .find_in_grid("Name:")
        .expect("form should render a Name field");
    deck.click(col, row);

    deck.send_bytes(b"nm777");
    deck.wait_for_string("nm777");
}

/// Scenario: Open the form and click the `demo2` mode chip (not the default
/// selection). That chip must become selected — the same as Left/Right/h/l
/// cycling to it — so the form reflects the `demo2` mode (its title shows
/// `demo2 mode`). RED until M8 renders clickable mode chips (today only the
/// single selected mode is shown in a `◀ … ▶` cycler, so `demo2` isn't even
/// on screen to click).
#[spec("mouse/form/001")]
#[test]
fn form_001_click_mode_chip_selects() {
    let deck = TuiDeck::launch_with_fixture("form");
    open_form(&deck);

    click_target(&deck, "demo2");

    // Selecting the demo2 mode is reflected in the form (title → "demo2 mode").
    deck.wait_for_string("demo2 mode");
}

/// Scenario: Open the form, fill Name + Command via the keyboard, then click
/// `[Submit]`. The pane must be created with the entered values — the same as
/// Enter — so the form closes and the new pane (named `subm5`) appears. RED
/// until M8 renders the `[Submit]` button.
#[spec("mouse/form/001")]
#[test]
fn form_001_click_submit_creates_pane() {
    let deck = TuiDeck::launch_with_fixture("form");
    open_form(&deck);

    deck.send_bytes(b"\t"); // Mode → Name
    deck.send_bytes(b"subm5");
    deck.send_bytes(b"\t"); // Name → Command
    deck.send_bytes(b"sleep 600"); // plain command → real pane, no credentials

    click_target(&deck, "[Submit]");

    // Submitted like Enter: form closed, the named pane was created.
    deck.wait_until_quiescent();
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("subm5"),
        "submit should create the pane named subm5:\n{grid}"
    );
    assert!(
        !grid.contains("New Agent"),
        "submit should close the new-pane form:\n{grid}"
    );
}

/// Scenario: Open the form, type a Name, then click `[Cancel]`. The form must
/// close without creating a pane — the same as Esc — returning to the empty
/// dashboard with the typed name discarded. RED until M8 renders the
/// `[Cancel]` button.
#[spec("mouse/form/001")]
#[test]
fn form_001_click_cancel_discards() {
    let deck = TuiDeck::launch_with_fixture("form");
    open_form(&deck);

    deck.send_bytes(b"\t"); // Mode → Name
    deck.send_bytes(b"canc9");

    click_target(&deck, "[Cancel]");

    // Cancelled like Esc: back to the empty dashboard, no pane created.
    deck.wait_for_string("No active sessions");
    assert!(
        !deck.snapshot_grid().contains("canc9"),
        "cancel must not create a pane with the typed name:\n{}",
        deck.snapshot_grid()
    );
}
