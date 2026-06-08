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

/// Scenario: Launch the deck in the `schedule-mode` fixture, open the new-pane
/// form (Ctrl+n → Space confirms the dir), cycle the Mode field to the built-in
/// `schedule` authoring option, and submit it. Assert the authoring session
/// lands as a single-agent DASHBOARD CARD — the dashboard's
/// `dot-agent-deck — N session(s)` title renders (it shows only on the Dashboard
/// tab) and no `×` tab-close glyph appears — NOT as a 50/50 mode tab, which would
/// open a second tab whose strip carries a `×` and hide the dashboard title. RED
/// today: the `schedule` option opens via `render_mode_tab` as a mode tab, so a
/// `×` appears and the dashboard title is absent.
#[spec("prompt/new-pane/008")]
#[test]
fn new_pane_008_schedule_authoring_opens_as_dashboard_card() {
    let deck = TuiDeck::launch_with_fixture("schedule-mode");
    deck.wait_for_string("No active sessions");

    // Open the new-pane form and cycle the Mode field to the built-in
    // `schedule` authoring option (the cycler caps at the last option) —
    // mirroring `new_pane_007`'s drive.
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // Mode field is up (cycler at "No mode")
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C"); // Right ×8
    deck.wait_for_string("schedule mode"); // selection landed on the schedule mode

    // Submit via the [Submit] button (deterministic — the schedule mode still
    // shows a Command field, so an Enter-count would be fragile). That field is
    // empty for the built-in option, which the schedule authoring mode defaults
    // to `claude`; the card-vs-mode-tab layout renders independent of which
    // command is spawned, so this assertion holds regardless of the agent.
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the new-pane form should render a [Submit] button");
    deck.click(scol, srow);

    // Submitting closes the form; wait for the resulting layout to settle into
    // one of the two observable end-states: a single-agent dashboard card (the
    // dashboard's session-count title renders only on the Dashboard tab) or a
    // 50/50 mode tab (a second tab whose strip carries a `×` close glyph).
    deck.wait_for_absence("[Submit]"); // form closed
    deck.wait_until_grid("schedule submit settles into a card or a mode tab", |g| {
        g.contains("dot-agent-deck \u{2014}") || g.contains("×")
    });

    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("dot-agent-deck \u{2014}"),
        "the `schedule` authoring session must open as a single-agent DASHBOARD CARD: the \
         dashboard's `dot-agent-deck — N session(s)` title renders only on the Dashboard \
         tab, so its presence is what proves the authoring session stayed a card.\nGrid:\n{grid}"
    );
    assert!(
        !grid.contains("×"),
        "the `schedule` authoring session must NOT open as a 50/50 mode tab: a mode tab \
         creates a second tab whose strip carries a `×` close glyph. A `×` on screen means \
         the authoring agent was (wrongly) routed through `render_mode_tab` instead of \
         landing as a dashboard card.\nGrid:\n{grid}"
    );
}
