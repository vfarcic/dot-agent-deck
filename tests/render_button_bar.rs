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
/// five actions and the bar doubles as a legend. RED until M2 renders the
/// bar (today the bottom row still shows the legacy status legend).
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
}

/// Scenario: Render the global button bar at 40 columns — too narrow for
/// the full `[Label Shortcut]` set (~78 cells) but wide enough for the
/// shortcut-only fallback (~39 cells). The bar must degrade gracefully to
/// shortcut-only labels — `[Ctrl+N]`, `[Ctrl+W]`, `[Ctrl+T]`, `[?]`,
/// `[Quit Ctrl+C]`'s `[Ctrl+C]` — so every one of the five commands stays
/// represented and identifiable, and no button is clipped mid-label into
/// something unrecognizable. The full `[New Pane Ctrl+N]` label must NOT
/// appear (proving the bar degraded rather than truncated). RED until M2
/// implements the narrow-terminal fallback.
#[spec("mouse/buttonbar/002")]
#[test]
fn buttonbar_002_narrow_terminal_degrades_to_shortcut_only() {
    let buffer = render_button_bar_to_buffer(40);
    let bar = row_text(&buffer);

    // All five commands remain represented by their shortcut-only label.
    for shortcut in ["[Ctrl+N]", "[Ctrl+W]", "[Ctrl+T]", "[?]", "[Ctrl+C]"] {
        assert!(
            bar.contains(shortcut),
            "narrow bar must keep {shortcut:?} so the command stays identifiable, got {bar:?}"
        );
    }

    // The degraded bar drops the long label, so the full form is absent —
    // distinguishing graceful degradation from a mid-label truncation.
    assert!(
        !bar.contains("[New Pane Ctrl+N]"),
        "narrow bar must degrade to shortcut-only, not render the full label, got {bar:?}"
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
/// default PTY width (`DEFAULT_COLS`). The full `[Label Shortcut]` set is ~133
/// cells once Scheduled Tasks is included, so it overflows 120 and the bar must
/// degrade to shortcut-only chips: the bar contains `[Ctrl+N]` and must NOT
/// contain the full `[New Pane Ctrl+N]` label, while the Scheduled Tasks button
/// stays present and identifiable as `[Scheduled Tasks s]` (its shortcut is
/// baked into the label, so the shortcut-only fallback keeps the full name).
/// This locks in the responsive degradation that the L2 mouse specs avoid by
/// rendering at a roomy full-screen width (200 cols) — guarding against a
/// silent regression where the default-width bar stops collapsing.
#[spec("mouse/buttonbar/006")]
#[test]
fn buttonbar_006_full_dashboard_set_degrades_at_default_width() {
    let buffer = render_button_bar_with_bindings_to_buffer(&KeybindingConfig::default(), 120, 1);
    let bar = row_text(&buffer);

    // Degraded to the shortcut-only chip for New Pane …
    assert!(
        bar.contains("[Ctrl+N]"),
        "at the default 120 cols the full dashboard bar must degrade to the \
         shortcut-only `[Ctrl+N]` chip, got {bar:?}"
    );
    // … and the full label is absent (degradation, not mid-label truncation).
    assert!(
        !bar.contains("[New Pane Ctrl+N]"),
        "at 120 cols the full dashboard bar must NOT render the full \
         `[New Pane Ctrl+N]` label — it should have collapsed to chips, got {bar:?}"
    );
    // The always-shown Scheduled Tasks button stays present and identifiable
    // even in chip form (its name is baked into the label).
    assert!(
        bar.contains("[Scheduled Tasks s]"),
        "the always-shown Scheduled Tasks button must stay present and \
         identifiable as `[Scheduled Tasks s]` in chip mode, got {bar:?}"
    );
}
