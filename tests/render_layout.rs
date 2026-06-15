//! PRD #144 — L1 layout tests for the cramped-UI surfaces.
//!
//! When the full-label button bar no longer fits on one row it WRAPS to a
//! second row (PRD #144 — keep full labels, spend a row of vertical space)
//! rather than collapsing to shortcut-only chips. A 2-row bar consumes one
//! extra row of the bottom region, so the dashboard/pane area above must cede
//! exactly that row or the cards overlap / clip (PRD #144 risk row). This L1
//! test pins the height-budget side of that contract by measuring how many rows
//! the bar actually occupies, driving the production bottom-bar renderer through
//! the `render_button_bar_with_bindings_to_buffer` `TestBackend` seam (no PTY,
//! no subprocess). It complements `mouse/buttonbar/006`, which pins the *label*
//! content of the wrapped bar; this one pins its *height*.
//!
//! The second test (`render/layout/005`) pins the bounds-safety side of the
//! content-sized modals: the new-pane form modal must render without panicking
//! at a wide-but-very-short terminal, driving the production
//! `render_new_pane_form` through the `render_new_pane_form_to_buffer` seam.

use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    render_button_bar_with_bindings_to_buffer, render_new_pane_form_to_buffer,
};
use spec::spec;

mod common;
use common::{joined_rows, nonblank_rows};

/// Scenario: Render the full dashboard button bar (global + context buttons,
/// ~133 cells) into a tall `TestBackend` area at the 120-col reference width and
/// again at a roomy 200-col width, and count the rows it occupies. At 120 cols
/// the set does not fit one row, so the bar must wrap to EXACTLY two rendered
/// rows — meaning the dashboard/pane region above cedes exactly one extra row
/// (the PRD #144 height-budget contract that prevents card/pane overlap). At
/// 200 cols the same set fits one row, so the bar occupies exactly one row and
/// the dashboard cedes nothing extra. RED today: the bar collapses to a single
/// row of shortcut-only chips at 120, so it never takes the second row.
#[spec("render/layout/004")]
#[test]
fn layout_004_wrapped_bar_costs_exactly_one_extra_row() {
    // At the 120-col reference width the full button set wraps to a second row,
    // so the dashboard region above must give up exactly that one extra row.
    let reference = render_button_bar_with_bindings_to_buffer(&KeybindingConfig::default(), 120, 6);
    assert_eq!(
        nonblank_rows(&reference),
        2,
        "at the 120-col reference width the full button bar must wrap to exactly \
         two rows (so the dashboard/pane region cedes exactly one extra row of \
         its height budget), got rows:\n{}",
        joined_rows(&reference)
    );

    // At a comfortably wide width the whole set fits one row, so the bar costs a
    // single row and the dashboard cedes nothing extra.
    let roomy = render_button_bar_with_bindings_to_buffer(&KeybindingConfig::default(), 200, 6);
    assert_eq!(
        nonblank_rows(&roomy),
        1,
        "at a roomy 200-col width the full button bar fits one row, so it must \
         occupy exactly one row and take no extra row from the dashboard, got \
         rows:\n{}",
        joined_rows(&roomy)
    );
}

/// Scenario: Render the new-pane form modal (two modes) into a wide-but-very-
/// short 80×3 `TestBackend` buffer — a small-but-valid terminal where the modal,
/// clamped to ~90% of the 3-row height, has far fewer rows than the form's
/// reserved fields. The form must render WITHOUT panicking: its overlay rows (the
/// mode chips, the `[Submit]`/`[Cancel]` row, the cursor) must stay within the
/// clamped modal/buffer bounds instead of being placed by an absolute line index
/// that runs past the buffer's bottom. RED today: at 80×3 the chip row lands
/// below the buffer and `set_span` panics with an out-of-bounds write (PRD #144
/// finding A1). A TUI must not panic on a small-but-valid terminal.
#[spec("render/layout/005")]
#[test]
fn layout_005_new_pane_form_survives_short_terminal() {
    // 80 cols wide, 3 rows tall: the modal is clamped to ~2 rows, far fewer than
    // the form's reserved field rows, so any overlay positioned by an unclamped
    // absolute line index would write past the buffer bottom and panic.
    let result =
        std::panic::catch_unwind(|| render_new_pane_form_to_buffer(&["demo", "demo2"], 80, 3));

    assert!(
        result.is_ok(),
        "new-pane form must render without panicking on a wide-but-very-short \
         80x3 terminal; an overlay row (mode chips / Submit-Cancel / cursor) was \
         placed past the clamped modal/buffer bounds (PRD #144 finding A1)"
    );

    // We got a buffer back of exactly the requested size — every cell the modal
    // wrote is therefore inside the buffer (the overlays did not escape bounds).
    let buf = result.unwrap();
    assert_eq!(
        (buf.area().width, buf.area().height),
        (80, 3),
        "render seam must return an 80x3 buffer"
    );
}
