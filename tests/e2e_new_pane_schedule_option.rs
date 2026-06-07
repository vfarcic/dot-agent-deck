#![cfg(feature = "e2e")]

//! L2 test for PRD #127 M3.2 — the built-in "schedule" creation option in the
//! new-deck dialog.
//!
//! Re-sequenced from L1 to L2: the new-pane dialog's renderer
//! (`render_new_pane_form`) and its state (`NewPaneFormState`) are private and
//! there is no public L1 render seam (only `render_card_to_buffer` is public),
//! so the dialog is exercised by driving the REAL binary through a PTY — the
//! same Ctrl+n → dir-picker → new-pane-form flow `tabs/mode/005` uses — and
//! asserting on the rendered vt100 grid.
//!
//! ## Pinned contract (for the coder)
//! The dialog's Mode field is a cycler that shows one option at a time
//! (`◀ name ▶`). The "schedule" option is a built-in authoring mode placed at
//! the END of the cycle (after the project's workload modes), so cycling Right
//! to the cap lands on it. It is VISUALLY SEPARATED from the workload modes by
//! an authoring-session affordance — a label/section containing the word
//! `authoring` (the PRD's "throwaway authoring session" marker). RED today:
//! neither the `schedule` option nor the `authoring` separator exists.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch the deck in a fixture whose `.dot-agent-deck.toml` defines
/// one workload mode (`build`). Open the new-deck dialog (Ctrl+n → Space
/// confirms the dir → the new-pane form), then cycle the Mode field to the end
/// of its options. Assert the dialog surfaces a selectable `schedule` option
/// and that it is visually separated from the workload modes by an
/// authoring-session affordance (an `authoring` label/section) — the
/// throwaway-authoring-session marker. RED today: the dialog only cycles
/// through `No mode` / `build`, with no `schedule` option and no separator.
#[spec("prompt/new-pane/007")]
#[test]
fn new_pane_007_schedule_authoring_option_visually_separated() {
    let deck = TuiDeck::launch_with_fixture("schedule-mode");
    deck.wait_for_string("No active sessions");

    // Open the new-pane form: Ctrl+n → directory picker, Space confirms the
    // current dir (no quiescence wait — the deck repaints on a periodic tick).
    deck.send_keys(b"\x0e"); // Ctrl+n
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("No mode"); // Mode field is up (cycler at "No mode")

    // Cycle the Mode field to the end of its options (select_next_mode caps at
    // the last option). The built-in "schedule" authoring option lives there.
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C"); // Right ×8

    // Wait until selection actually LANDS on the schedule option before
    // snapshotting. The dialog title becomes "… — schedule mode" only when
    // `selected_mode()` resolves to the authoring mode, so it is a
    // selection-dependent signal. (The bare `[schedule]` chip renders at every
    // cycler index, so waiting on "schedule" alone returns immediately and
    // races the input processing.)
    deck.wait_for_string("schedule mode");

    // ...and visually separated from the workload modes by an authoring-session
    // affordance (the throwaway-authoring-session marker).
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("schedule"),
        "the new-deck dialog must surface a selectable `schedule` option.\nGrid:\n{grid}"
    );
    assert!(
        grid.to_lowercase().contains("authoring"),
        "the `schedule` option must be visually separated from workload modes by an \
         authoring-session affordance (an `authoring` label/section).\nGrid:\n{grid}"
    );
}
