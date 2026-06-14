//! PRD #144 — L1 layout test for the dashboard button bar's height budget.
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

use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::render_button_bar_with_bindings_to_buffer;
use spec::spec;

/// Count the rows of `buffer` that carry any non-blank cell — i.e. how many
/// rows the rendered bar occupies. This is the height the dashboard layout must
/// subtract from its budget for the bottom bar.
fn nonblank_rows(buffer: &ratatui::buffer::Buffer) -> usize {
    let area = buffer.area();
    (0..area.height)
        .filter(|&y| (0..area.width).any(|x| !buffer[(x, y)].symbol().trim().is_empty()))
        .count()
}

/// Join every row of the bar buffer into one `\n`-separated string, for a
/// readable failure message.
fn joined_rows(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

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
