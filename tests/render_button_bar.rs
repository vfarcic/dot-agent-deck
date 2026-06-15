//! PRD #80 M2 — L1 widget tests for the persistent global button bar.
//!
//! Per PRD #77 Decision 2 these are in-process tests driving the
//! production bottom-bar renderer through `render_button_bar_to_buffer`
//! (a `TestBackend` wrapper, mirroring `render_card_to_buffer`). No
//! subprocess, no PTY. File-layout-mirrors-catalog (Decision 7): catalog
//! IDs `mouse/buttonbar/NNN` land here with function names
//! `<sub-area>_<NNN>_<short_suffix>` (Decision 17).
//!
//! The bar exposes the five global commands, each carrying its keyboard
//! shortcut inline so the bar doubles as a legend:
//!   New Pane        → Ctrl+N   `[New Pane Ctrl+N]`
//!   Close           → Ctrl+W   `[Close Ctrl+W]`
//!   Toggle Layout   → Ctrl+T   `[Toggle Layout Ctrl+T]`
//!   Help            → ?        `[Help ?]`
//!   Quit            → Ctrl+C   `[Quit Ctrl+C]`
//! The shortcut strings are derived from the keyboard handlers in
//! `src/ui.rs` (`global_ctrl_action` for Ctrl+N/W/T, `Char('?')` → Help,
//! Ctrl+C → Quit). M2 must wire the buttons to those same bindings.

use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{render_button_bar_to_buffer, render_button_bar_with_bindings_to_buffer};
use spec::spec;

mod common;
use common::{joined_rows, nonblank_rows};

/// Collapse the rendered single-row buffer into one string of cell
/// symbols, so content assertions read like the on-screen bar.
fn row_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    (0..area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>()
}

/// Scenario: Render the global button bar into a 120-column (comfortable
/// width) `TestBackend` buffer. The single bottom row must contain a
/// clickable button for every global command WITH its inline shortcut —
/// `[New Pane Ctrl+N]`, `[Close Ctrl+W]`, `[Toggle Layout Ctrl+T]`,
/// `[Help ?]`, and `[Quit Ctrl+C]` — so click and keyboard expose the same
/// five actions and the bar doubles as a legend. PRD #144 no-wrap guard:
/// rendering the FULL dashboard bar (global + context, ~133 cells) at a
/// comfortably wide 200 cols into a multi-row area must leave it on a SINGLE
/// row (no wrap) with its full labels intact — wrapping kicks in only when the
/// bar does not fit one row.
#[spec("mouse/buttonbar/001")]
#[test]
fn buttonbar_001_full_bar_has_button_per_command_with_shortcut() {
    let buffer = render_button_bar_to_buffer(120);
    let bar = row_text(&buffer);

    for expected in [
        "[New Pane Ctrl+N]",
        "[Close Ctrl+W]",
        "[Toggle Layout Ctrl+T]",
        "[Help ?]",
        "[Quit Ctrl+C]",
    ] {
        assert!(
            bar.contains(expected),
            "button bar at 120 cols must render the {expected:?} button inline-shortcut label, got {bar:?}"
        );
    }

    // PRD #144 no-wrap guard: at a comfortable width the full dashboard bar
    // (global + context buttons) fits on one row, so even when handed a
    // multi-row area it must NOT wrap — every row but the first stays blank …
    let wide = render_button_bar_with_bindings_to_buffer(&KeybindingConfig::default(), 200, 3);
    assert_eq!(
        nonblank_rows(&wide),
        1,
        "at a comfortable 200-col width the full button bar fits one row and \
         must NOT wrap, got rows:\n{}",
        joined_rows(&wide)
    );
    // … and that single row still carries the full inline-shortcut labels.
    assert!(
        joined_rows(&wide).contains("[New Pane Ctrl+N]"),
        "the comfortable-width single-row bar must still render the full \
         `[New Pane Ctrl+N]` label, got:\n{}",
        joined_rows(&wide)
    );
}

/// Scenario: Render the full dashboard button bar (global + context buttons,
/// ~133 cells) at a narrow/windowed 80 columns into a multi-row area. PRD #144:
/// rather than collapsing to shortcut-only chips, the bar must WRAP to multiple
/// rows while keeping the FULL `[Label Shortcut]` form of every button — so
/// `[New Pane Ctrl+N]`, `[Toggle Layout Ctrl+T]`, `[Quit Ctrl+C]` and the
/// always-shown `[Scheduled Tasks s]` all stay spelled out somewhere across the
/// rows, the shortcut-only chip `[Ctrl+N]` never appears, and the bar occupies
/// more than one row. This inverts the pre-#144 shortcut-only degradation.
#[spec("mouse/buttonbar/002")]
#[test]
fn buttonbar_002_narrow_terminal_wraps_keeping_full_labels() {
    let buffer = render_button_bar_with_bindings_to_buffer(&KeybindingConfig::default(), 80, 6);
    let bar = joined_rows(&buffer);

    // Every command keeps its FULL inline-shortcut label — wrapped, not chipped.
    for expected in [
        "[New Pane Ctrl+N]",
        "[Close Ctrl+W]",
        "[Toggle Layout Ctrl+T]",
        "[Help ?]",
        "[Quit Ctrl+C]",
        "[Scheduled Tasks s]",
    ] {
        assert!(
            bar.contains(expected),
            "narrow bar must WRAP keeping the full {expected:?} label (not a \
             shortcut-only chip), got rows:\n{bar}"
        );
    }

    // The shortcut-only chip must NOT appear — proving the bar wrapped rather
    // than degrading to chips.
    assert!(
        !bar.contains("[Ctrl+N]"),
        "narrow bar must wrap with full labels, not degrade to the `[Ctrl+N]` \
         shortcut-only chip, got rows:\n{bar}"
    );

    // And it genuinely spent vertical space: the full labels don't fit one row
    // at 80 cols, so the bar occupies more than a single row.
    assert!(
        nonblank_rows(&buffer) >= 2,
        "at 80 cols the full-label bar must wrap to multiple rows, got rows:\n{bar}"
    );
}

/// Scenario: Render the dashboard button bar at a comfortable 200-column
/// width with ZERO schedules configured — the seam drives
/// `dashboard_context_buttons` with `has_schedules = false` (an empty global
/// `schedules.toml`, no `DOT_AGENT_DECK_SCHEDULES` tasks). Even with no
/// schedules the bottom bar must show the Scheduled Tasks open button (a
/// label starting `[Scheduled`, carrying its `s` shortcut), because that
/// button opens the manager — which is itself how you CREATE the first
/// schedule (its `[Add a]` action works on an empty list). The 200-col width
/// fits the full global+context bar, so this isolates the `has_schedules`
/// gate rather than the bar's overflow / shortcut-only behavior. RED today:
/// the `if has_schedules` gate in `dashboard_context_buttons` omits the
/// button when the schedule list is empty.
#[spec("mouse/buttonbar/005")]
#[test]
fn buttonbar_005_scheduled_tasks_button_present_with_zero_schedules() {
    let buffer = render_button_bar_with_bindings_to_buffer(&KeybindingConfig::default(), 200, 1);
    let bar = row_text(&buffer);

    assert!(
        bar.contains("[Scheduled"),
        "dashboard button bar must show the Scheduled Tasks open button even with \
         zero schedules (the manager it opens is how you create the first one), got {bar:?}"
    );
}

/// Scenario: Render the FULL dashboard button set — the five global commands
/// PLUS the dashboard context buttons (Filter / Rename / Generate and the
/// always-shown Scheduled Tasks button) — at 120 columns, the harness's pinned
/// default PTY width (`DEFAULT_COLS`), into a multi-row area. The full
/// `[Label Shortcut]` set is ~133 cells, so it overflows 120 and PRD #144 has it
/// WRAP to a second row keeping EVERY button's full label rather than collapsing
/// to shortcut-only chips: the full `[New Pane Ctrl+N]` label is present and the
/// shortcut-only `[Ctrl+N]` chip is absent. Degradation is uniform — the
/// `[Scheduled Tasks s]` button is full-labelled like the rest, NOT special-
/// cased to keep its label while others chip. This inverts the pre-#144
/// collapse-to-chips behavior at the 120-col reference width.
#[spec("mouse/buttonbar/006")]
#[test]
fn buttonbar_006_full_dashboard_set_wraps_at_default_width() {
    let buffer = render_button_bar_with_bindings_to_buffer(&KeybindingConfig::default(), 120, 6);
    let bar = joined_rows(&buffer);

    // Every button keeps its full label — wrapped to a second row, not chipped.
    for expected in [
        "[New Pane Ctrl+N]",
        "[Close Ctrl+W]",
        "[Toggle Layout Ctrl+T]",
        "[Help ?]",
        "[Quit Ctrl+C]",
        "[Scheduled Tasks s]",
    ] {
        assert!(
            bar.contains(expected),
            "at the 120-col reference width the full dashboard bar must WRAP \
             keeping the full {expected:?} label (no shortcut-only chips, \
             Scheduled Tasks not special-cased), got rows:\n{bar}"
        );
    }

    // The shortcut-only chip must be absent — the bar wrapped, it did not chip.
    assert!(
        !bar.contains("[Ctrl+N]"),
        "at 120 cols the full dashboard bar must wrap with full labels, NOT \
         collapse to the `[Ctrl+N]` shortcut-only chip, got rows:\n{bar}"
    );

    // It wrapped to a second row (the full set does not fit one row at 120).
    assert!(
        nonblank_rows(&buffer) >= 2,
        "at the 120-col reference width the full-label bar must wrap to a \
         second row, got rows:\n{bar}"
    );
}
