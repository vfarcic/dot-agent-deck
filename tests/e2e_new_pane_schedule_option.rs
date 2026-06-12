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
//! an authoring-session affordance — a hint line marked with `↳` reading
//! exactly `↳ authoring (one-off)` (the PRD's "throwaway authoring session"
//! marker), short enough to stay fully CONTAINED within the new-pane modal
//! border. RED today: the hint renders the long
//! `↳ authoring session — writes a schedule, then done`, whose tail overflows
//! the modal's inner width and is clipped at the right border.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch the deck in a fixture whose `.dot-agent-deck.toml` defines
/// one workload mode (`build`). Open the new-deck dialog (Ctrl+n → Space
/// confirms the dir → the new-pane form), then cycle the Mode field to the end
/// of its options so the built-in `schedule` authoring option is selected.
/// Assert the authoring-session affordance — the `↳`-marked hint that visually
/// separates `schedule` from the workload modes — renders its FULL text as
/// exactly `↳ authoring (one-off)` AND is fully contained within the new-pane
/// modal border (its tail is followed by padding before the right `│`, not
/// clipped by it). RED today: the hint is the long
/// `↳ authoring session — writes a schedule, then done`, which is both the
/// wrong text and clipped at the modal's right border.
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
    // The authoring hint renders on its own line only while the schedule mode
    // is selected; the `↳` marker is unique to that line. Wait for it so the
    // assertions never race the repaint.
    deck.wait_for_string("\u{21b3}");

    let grid = deck.snapshot_grid();

    // Isolate the authoring hint's row — the only line carrying the `↳` marker.
    let hint_row = grid
        .lines()
        .find(|line| line.contains('\u{21b3}'))
        .unwrap_or_else(|| {
            panic!(
                "the new-pane form must render an authoring hint line (marked `↳`) when \
                 the `schedule` mode is selected.\nGrid:\n{grid}"
            )
        });

    // The hint must live BETWEEN the modal's vertical borders (`│`, U+2502).
    // Slice the inner span between the left and right border so the assertions
    // see exactly what rendered INSIDE the box — anything pushed past the inner
    // width is clipped at the border and absent from this slice.
    let bar = '\u{2502}';
    let left = hint_row.find(bar).unwrap_or_else(|| {
        panic!(
            "the authoring hint row should carry the modal's left border `│`.\nRow: {hint_row:?}"
        )
    });
    let right = hint_row.rfind(bar).unwrap_or_else(|| {
        panic!(
            "the authoring hint row should carry the modal's right border `│`.\nRow: {hint_row:?}"
        )
    });
    assert!(
        right > left,
        "the authoring hint row should have distinct left/right modal borders.\nRow: {hint_row:?}"
    );
    let inner = &hint_row[left + bar.len_utf8()..right];

    // (1) FULL TEXT: the complete hint (normalized for the grid's left/right
    // padding) must be exactly the SHORTENED authoring marker — not the long,
    // overflowing wording.
    assert_eq!(
        inner.trim(),
        "\u{21b3} authoring (one-off)",
        "the authoring affordance must render the SHORTENED hint `↳ authoring (one-off)`, \
         not the long `↳ authoring session — writes a schedule, then done`.\nGrid:\n{grid}"
    );

    // (2) CONTAINED WITHIN THE MODAL: the hint must not be pressed against (and
    // clipped by) the right border — there must be padding between its tail and
    // the `│`. An overflowing hint runs straight into the border, so `inner`
    // would not end in a space.
    assert!(
        inner.ends_with(' '),
        "the authoring hint must be fully CONTAINED within the new-pane modal border: its \
         tail should be followed by padding before the right `│`, but it runs straight into \
         (is clipped by) the border.\nGrid:\n{grid}"
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

/// Scenario: Launch the deck in a fixture whose `.dot-agent-deck.toml` defines a
/// workload mode (`build`) PLUS an orchestration (`ci-deployment`), so the
/// new-deck dialog's Mode field renders a long chip row
/// (`  Mode: [No mode] [build] [Orch: ci-deployment] [schedule]`) that is wider
/// than the capped modal. Open the new-pane form (Ctrl+n → Space confirms the
/// dir), then cycle the Mode field to the end so the built-in `[schedule]` chip
/// is the selected option, and assert that the `[schedule]` chip renders FULLY
/// within the modal's vertical borders — it is not clipped off the right edge.
/// RED today: the chip row overflows the modal's inner width, so the renderer
/// breaks the chip loop before drawing the trailing `[schedule]` chip, leaving
/// it absent from every between-the-borders span. The assertion is
/// approach-agnostic: it passes whether the coder later WRAPS the chip row to a
/// second line or WINDOWS/scrolls the cycler, as long as the selected
/// `[schedule]` chip ends up visible inside the modal.
#[spec("prompt/new-pane/009")]
#[test]
fn new_pane_009_schedule_chip_contained_when_row_overflows() {
    let deck = TuiDeck::launch_with_fixture("mode-chip-overflow");
    deck.wait_for_string("No active sessions");

    // Open the new-pane form: Ctrl+n → directory picker, Space confirms the
    // current dir → the new-pane form.
    deck.send_keys(b"\x0e"); // Ctrl+n
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("No mode"); // Mode field is up (cycler at "No mode")

    // Cycle the Mode field to the end of its options so the built-in `schedule`
    // authoring option is selected (`select_next_mode` caps at the last option,
    // so an over-count of Rights is safe).
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C"); // Right ×8

    // Wait until selection actually LANDS on the schedule option before
    // snapshotting. The dialog title becomes "… — schedule mode" only when
    // `selected_mode()` resolves to the authoring mode, so it is a
    // selection-dependent signal that renders in the same frame as the chips.
    // (Waiting on the bare `[schedule]` chip would be circular — the chip is
    // exactly what is clipped/absent in the RED state.)
    deck.wait_for_string("schedule mode");

    let grid = deck.snapshot_grid();

    // The selected `[schedule]` chip must render FULLY between the modal's
    // vertical borders (`│`, U+2502). Scan every row bounded by a left AND a
    // right `│` and check whether the bracketed chip lands inside that inner
    // span — a wrap fix puts the chip on a second content line, a window/scroll
    // fix keeps it on the Mode line, and both still live between the borders.
    // The top-border title row carries corner glyphs (`┌ … ┐`), not `│ … │`, so
    // the title's "schedule mode" (no brackets) is excluded; only the bracketed
    // chip counts.
    let bar = '\u{2502}';
    let contained = grid
        .lines()
        .any(|line| match (line.find(bar), line.rfind(bar)) {
            (Some(l), Some(r)) if r > l => line[l + bar.len_utf8()..r].contains("[schedule]"),
            _ => false,
        });

    assert!(
        contained,
        "the selected `[schedule]` chip must render FULLY within the new-pane modal border, \
         but it is clipped off the right edge. The Mode chip row \
         (`  Mode: [No mode] [build] [Orch: ci-deployment] [schedule]`) overflows the modal's \
         inner width, so the renderer drops the trailing `[schedule]` chip and it never appears \
         between any row's `│ … │` borders.\nGrid:\n{grid}"
    );
}
