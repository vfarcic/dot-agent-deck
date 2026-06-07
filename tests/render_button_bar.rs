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

use dot_agent_deck::theme::ColorPalette;
use dot_agent_deck::ui::render_button_bar_to_buffer;
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
    let palette = ColorPalette::dark();
    let buffer = render_button_bar_to_buffer(120, palette);
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
    let palette = ColorPalette::dark();
    let buffer = render_button_bar_to_buffer(40, palette);
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
