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
//!
//! The third (`render/layout/006`, PRD #313 M5) is the L1 snapshot rule 4 asks
//! for on a user-facing layout change: it renders a whole orchestration tab
//! unzoomed and zoomed through `render_orchestration_frame_to_buffer` and pins
//! what zoom removes (the sidebar, the non-focused roles) against what it must
//! KEEP (the focused pane's border, now carrying a `[Z]` indicator). It is also
//! the ONLY tier that can see the indicator's STYLE: the L2 vt100 grid carries
//! characters and no attributes, so there a display name spelling `[Z]` is
//! indistinguishable from the real marker, while here the `Buffer` keeps the
//! cells and the marker's own `zoom_marker_style` span is visible.

use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    render_button_bar_with_bindings_to_buffer, render_new_pane_form_to_buffer,
    render_orchestration_frame_to_buffer,
};
use ratatui::style::Modifier;
use spec::spec;

mod common;
use common::{joined_rows, nonblank_rows, role_pane_border_title, role_pane_left_edge};

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

/// Scenario: Render a two-role orchestration tab (`orchestrator` focused,
/// `worker` not) into a 100x30 `TestBackend` twice — once unzoomed and once
/// zoomed — and compare what is on screen. Unzoomed, the sidebar occupies the
/// left 34% and the `worker` role card is visible beside the focused pane;
/// zoomed, the sidebar and the non-focused role are gone, the focused pane's box
/// starts at column 0, and its border title reads exactly `orchestrator [Z]`
/// with the marker's cells drawn REVERSED — the styled channel a spoofed,
/// agent-supplied display name spelling `[Z]` cannot occupy.
#[spec("render/layout/006")]
#[test]
fn layout_006_zoom_hides_the_sidebar_and_keeps_the_marked_border() {
    const ROLES: [&str; 2] = ["orchestrator", "worker"];

    // --- Unzoomed: sidebar on the left, pane column starting at 34%. ---
    let unzoomed_buffer = render_orchestration_frame_to_buffer(&ROLES, 0, false, false, 100, 30);
    let unzoomed = joined_rows(&unzoomed_buffer);
    assert_eq!(
        role_pane_left_edge(&unzoomed, "orchestrator"),
        Some(34),
        "unzoomed: the focused role pane's box must start at the 34%-width \
         sidebar boundary\n{unzoomed}"
    );
    assert!(
        unzoomed.contains("worker"),
        "unzoomed: the non-focused `worker` role must be visible in the \
         sidebar\n{unzoomed}"
    );
    assert!(
        !unzoomed.contains("[Z]"),
        "unzoomed: no zoom indicator may be drawn\n{unzoomed}"
    );
    // The whole-grid check above is the strong form of the NEGATIVE (nothing
    // anywhere), so it needs no positional anchor; this pins the same row the
    // positive assertion below reads, so a regression that moved the marker out
    // of the title rather than removing it cannot pass both.
    assert_eq!(
        role_pane_border_title(&unzoomed, "orchestrator").as_deref(),
        Some("orchestrator"),
        "unzoomed: the focused pane's border title must be the bare role name, \
         with no zoom marker fused onto it\n{unzoomed}"
    );

    // --- Zoomed: no sidebar, border kept, `[Z]` in the title. ---
    let zoomed_buffer = render_orchestration_frame_to_buffer(&ROLES, 0, false, true, 100, 30);
    let zoomed = joined_rows(&zoomed_buffer);
    // The border is what carries the title, the focus/status colour (PRD #155
    // M3) and the command-mode weight (`9345a74`) — PRD #313 Open Question 2
    // decides to KEEP it, so finding the box's corner glyph fused to the role
    // name at column 0 asserts BOTH halves at once: the sidebar is gone AND the
    // border was not dropped with it.
    assert_eq!(
        role_pane_left_edge(&zoomed, "orchestrator"),
        Some(0),
        "zoomed: the focused role pane must keep its BORDER and start at column \
         0 — no sidebar\n{zoomed}"
    );
    assert!(
        !zoomed.contains("worker"),
        "zoomed: the non-focused `worker` role must not be drawn anywhere — \
         neither a sidebar card nor a pane\n{zoomed}"
    );
    // Positional, NOT `zoomed.contains("[Z]")`: a pane title is a display name,
    // display names reach the deck over the hook socket, and
    // `sanitize_display_name` strips control characters and bidi overrides but
    // not brackets — so an agent calling itself `worker [Z]` puts that token on
    // the grid with nothing zoomed. Reading the border title of the box the
    // geometry actually expanded is what a text-only grid can still prove.
    assert_eq!(
        role_pane_border_title(&zoomed, "orchestrator").as_deref(),
        Some("orchestrator [Z]"),
        "zoomed: the focused pane's border title must carry the `[Z]` zoom \
         indicator, mirroring tmux's status-line Z — without it a user \
         concludes their other agents disappeared (PRD #313 M3)\n{zoomed}"
    );

    // The style half, which only an L1 test can see: the L2 vt100 grid carries
    // characters and no attributes, so there the spoof above is
    // indistinguishable from the real marker. Here the `Buffer` keeps the cells'
    // style, and the product draws the marker as its OWN span
    // (`terminal_widget::zoom_marker_style`) precisely so a display name that
    // merely SPELLS `[Z]` renders as ordinary title text beside it. Asserting
    // REVERSED plus "differs from the name's own cells" pins that channel
    // without pinning the exact colour, which is presentation.
    let marker_column = zoomed
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            line.find("[Z]")
                .map(|byte_index| (row as u16, line[..byte_index].chars().count() as u16))
        })
        .expect("the zoomed border title carries `[Z]` (asserted above)");
    let (marker_row, marker_start) = marker_column;
    // Column 0 is the box corner, so the title's own text starts at column 1 —
    // the assertion above pinned that edge at 0.
    let title_style = zoomed_buffer[(1, marker_row)].style();
    for offset in 0..3u16 {
        let cell_style = zoomed_buffer[(marker_start + offset, marker_row)].style();
        assert!(
            cell_style.add_modifier.contains(Modifier::REVERSED),
            "zoomed: the `[Z]` marker must be drawn REVERSED so it cannot be \
             imitated by plain title text (an agent-supplied display name), got \
             {cell_style:?} at column {}\n{zoomed}",
            marker_start + offset
        );
        assert_ne!(
            cell_style, title_style,
            "zoomed: the `[Z]` marker's cells must not share the style of the \
             role name beside them — that shared style is exactly what a \
             spoofed display name would render with\n{zoomed}"
        );
    }

    // Snapshots last: the assertions above are the load-bearing part (a wrong
    // rendering fails them before any snapshot can be blessed), while these
    // record the whole frame for review and for browsing the diff.
    insta::assert_snapshot!("layout_006_orchestration_unzoomed", unzoomed);
    insta::assert_snapshot!("layout_006_orchestration_zoomed", zoomed);
}
