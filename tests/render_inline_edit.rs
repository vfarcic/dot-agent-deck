//! PRD #80 M6 — L1 widget test for the inline-edit (filter / rename) buttons.
//!
//! Per PRD #77 Decision 2 these are in-process tests driving the production
//! bottom-bar renderer through the `render_filter_bar_to_buffer` /
//! `render_rename_bar_to_buffer` seams (TestBackend wrappers mirroring
//! `render_button_bar_to_buffer`). No subprocess, no PTY. File-layout-
//! mirrors-catalog (Decision 7): catalog ID `mouse/inline/001`'s render
//! half lands here; the click→outcome half lives in
//! `tests/e2e_mouse_inline.rs`.
//!
//! M6 contract: the filter input row gains inline `[Apply]` / `[Cancel]`
//! buttons and the rename input row gains `[Save]` / `[Cancel]`, rendered at
//! the right edge of the input row — while the existing `/ ` / `Rename: `
//! prompt and the typed text stay present (buttons are additive). The
//! bracketed labels are distinct from the prompt text, so a `[Label]`
//! substring proves the button.

use dot_agent_deck::ui::{render_filter_bar_to_buffer, render_rename_bar_to_buffer};
use spec::spec;

/// Flatten the rendered one-row buffer into a string of cell symbols.
fn row_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    (0..area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>()
}

/// Scenario: Render the filter-mode bottom row (input text `proj`) and the
/// rename-mode bottom row (input text `newname`) at 80 columns. The filter
/// row must render its inline `[Apply]` and `[Cancel]` buttons while still
/// showing the `/ proj` prompt; the rename row must render `[Save]` and
/// `[Cancel]` while still showing `Rename: newname`. RED until M6 wires
/// these inline buttons (today the rows render only the prompt + text and
/// clear their button rects).
#[spec("mouse/inline/001")]
#[test]
fn inline_001_filter_and_rename_rows_render_buttons() {
    // Filter row: prompt + text still present, plus [Apply] / [Cancel].
    let filter = row_text(&render_filter_bar_to_buffer("proj", 80));
    assert!(
        filter.contains("/ proj"),
        "filter row must still render the '/ <text>' input, got {filter:?}"
    );
    for btn in ["[Apply]", "[Cancel]"] {
        assert!(
            filter.contains(btn),
            "filter row must render the {btn} button alongside the input, got {filter:?}"
        );
    }

    // Rename row: prompt + text still present, plus [Save] / [Cancel].
    let rename = row_text(&render_rename_bar_to_buffer("newname", 80));
    assert!(
        rename.contains("Rename: newname"),
        "rename row must still render the 'Rename: <text>' input, got {rename:?}"
    );
    for btn in ["[Save]", "[Cancel]"] {
        assert!(
            rename.contains(btn),
            "rename row must render the {btn} button alongside the input, got {rename:?}"
        );
    }
}
