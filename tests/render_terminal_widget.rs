//! PRD #84 M1 — L1/unit reproducers for the `TerminalWidget` render path.
//!
//! These pin the render half of the PRD #84 contract (`prds/done/84-rendering-
//! layer-rework.md`): the widget must render the vt100 screen 1:1 against its
//! inner area — no `min(area, screen)` col clamp, no cursor-anchored row
//! window (the two heuristics removed in M5, `src/terminal_widget.rs:94-117`).
//!
//! Both tests drive the production `TerminalWidget` (a public
//! `ratatui::widgets::Widget`) directly against an in-process
//! `ratatui::buffer::Buffer` — no PTY, no subprocess, fully deterministic.
//! `TerminalWidget` already renders correctly when the vt100 screen size
//! matches the inner area (the contract-holding case M4 guarantees upstream),
//! so each fixture intentionally feeds a *mismatched* screen to exercise the
//! heuristic under test. That mismatch is exactly the upstream state the
//! current architecture cannot prevent and that M4 fixes; the assertions
//! encode the M5 widget behaviour.

use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use spec::spec;

use dot_agent_deck::terminal_widget::TerminalWidget;

/// Render `parser`'s screen through a `TerminalWidget` into a fresh buffer
/// sized to `area`, returning the painted buffer. `focused` drives only the
/// border/cursor styling, not layout.
fn render_widget(parser: vt100::Parser, area: Rect, focused: bool) -> Buffer {
    let parser = Arc::new(Mutex::new(parser));
    let widget = TerminalWidget::new(parser, "pane".to_string(), focused);
    let mut buf = Buffer::empty(area);
    widget.render(area, &mut buf);
    buf
}

/// `TerminalWidget` draws a `Borders::ALL` block, so its content area is the
/// outer `area` shrunk by one cell on every side. Mirror that here so a test
/// can address the inner rows/cols the widget actually paints into.
fn inner_of(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// Read inner content row `r` (0-based, relative to the inner area) as a
/// string of cell symbols across the full inner width — excludes the block's
/// border columns so assertions see only the rendered terminal content.
fn inner_row(buf: &Buffer, inner: Rect, r: u16) -> String {
    let y = inner.y + r;
    (inner.x..inner.x + inner.width)
        .map(|x| buf[(x, y)].symbol())
        .collect()
}

/// Dump every inner content row, one per line, for assertion failure messages.
fn inner_dump(buf: &Buffer, inner: Rect) -> String {
    (0..inner.height)
        .map(|r| format!("  [{r}] {:?}", inner_row(buf, inner, r)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Scenario: Build an 8-row × 20-col vt100 screen with a distinct marker on
/// every row — `TOP_ROW_0` on row 0, `BOTTOM_ROW_7` on row 7 — leaving the
/// cursor parked on the bottom row. Render it through `TerminalWidget` into a
/// pane whose inner area is only 4 rows tall (a screen taller than its area —
/// the upstream size mismatch the PRD #84 contract eliminates). The 1:1
/// contract maps screen cell (r, c) onto inner cell (r, c), so the inner top
/// row must show screen row 0's `TOP_ROW_0` marker. RED today: the current
/// cursor-anchored row window (`src/terminal_widget.rs:96-117`) anchors on the
/// bottom cursor and shows screen rows 4..8 instead, so `TOP_ROW_0` is absent
/// from the top row. Goes GREEN at M5 when the windowing heuristic is removed.
#[spec("render/widget/001")]
#[test]
fn widget_001_renders_screen_from_row_zero_no_cursor_window() {
    let mut parser = vt100::Parser::new(8, 20, 0);
    // Place a unique marker on each row via absolute (1-based) cursor moves;
    // the final write parks the cursor on the bottom row (row 7).
    parser.process(b"\x1b[1;1HTOP_ROW_0");
    parser.process(b"\x1b[2;1HROW_1");
    parser.process(b"\x1b[3;1HROW_2");
    parser.process(b"\x1b[4;1HROW_3");
    parser.process(b"\x1b[5;1HROW_4");
    parser.process(b"\x1b[6;1HROW_5");
    parser.process(b"\x1b[7;1HROW_6");
    parser.process(b"\x1b[8;1HBOTTOM_ROW_7");

    // Outer area 22×6 → inner 20×4 (matches the 20-col screen width, so the
    // col clamp is a no-op and this isolates the row-window heuristic). The
    // screen is 8 rows but the inner area is only 4 — the deliberate mismatch.
    let area = Rect {
        x: 0,
        y: 0,
        width: 22,
        height: 6,
    };
    let inner = inner_of(area);
    let buf = render_widget(parser, area, true);

    let top = inner_row(&buf, inner, 0);
    assert!(
        top.contains("TOP_ROW_0"),
        "1:1 render contract: inner row 0 must show vt100 screen row 0 \
         (`TOP_ROW_0`), but the cursor-anchored row window shows a lower row.\n\
         inner row 0 = {top:?}\nfull inner render:\n{}",
        inner_dump(&buf, inner)
    );
}

/// Scenario: Render a small 3-row × 6-col vt100 screen (markers `TOP00`,
/// `MID11`, `BOT22`) into a deliberately *larger* pane whose inner area is
/// 6 rows × 12 cols. The release-path contract is to fall back to drawing the
/// available PTY cells at the top-left — `min(area, pty)` — without panicking
/// or reading out of bounds: the 3×6 content lands top-left and the excess
/// rows/cols stay blank. This guards the no-panic release fallback M5 must
/// preserve (M5 adds a debug-only `debug_assert!(pty == inner)` that is a dev
/// guard, not asserted here). Passes today (current code already clamps to
/// `min` and does not panic); it pins that behaviour against regression.
#[spec("render/widget/002")]
#[test]
fn widget_002_area_larger_than_pty_falls_back_to_min_no_panic() {
    let mut parser = vt100::Parser::new(3, 6, 0);
    // 5-char markers in a 6-col screen → no autowrap, cursor parks on row 2.
    parser.process(b"\x1b[1;1HTOP00");
    parser.process(b"\x1b[2;1HMID11");
    parser.process(b"\x1b[3;1HBOT22");

    // Outer area 14×8 → inner 12×6, larger than the 3×6 screen in BOTH dims.
    let area = Rect {
        x: 0,
        y: 0,
        width: 14,
        height: 8,
    };
    let inner = inner_of(area);
    // Reaching past this call at all proves the render did not panic / index
    // out of bounds on the over-sized area.
    let buf = render_widget(parser, area, false);

    // PTY content lands at the top-left.
    let top = inner_row(&buf, inner, 0);
    assert!(
        top.contains("TOP00"),
        "min fallback: the PTY's top row should render at inner row 0, got {top:?}\n{}",
        inner_dump(&buf, inner)
    );

    // Columns beyond the 6-col PTY width stay blank (no stale cells): inner
    // row 0 is the marker then only spaces.
    assert_eq!(
        top.trim_end(),
        "TOP00",
        "columns past the PTY width must be blank in the min fallback, got {top:?}\n{}",
        inner_dump(&buf, inner)
    );

    // Rows beyond the 3-row PTY height stay blank: inner row 5 (the 6th inner
    // row) has no corresponding screen row, so it must be empty.
    let beyond = inner_row(&buf, inner, 5);
    assert!(
        beyond.trim().is_empty(),
        "rows past the PTY height must be blank in the min fallback, got {beyond:?}\n{}",
        inner_dump(&buf, inner)
    );
}

/// Scenario: Issue #747 — build a vt100 screen at exactly `PTY_RESIZE_DIM_MAX`
/// (the widest a child PTY can be given) and render it, with
/// `contract_guaranteed(true)`, into a pane whose inner area is 2 columns WIDER
/// than that. The invariant-3 `debug_assert!` is live in this debug test build
/// and must expect the CAPPED inner area, so reaching past the render call at
/// all is the assertion. The child's full width then lands at the top-left and
/// the columns past the cap stay blank, because the child has no more columns
/// to show.
#[spec("render/widget/003")]
#[test]
fn widget_003_over_cap_inner_area_does_not_trip_the_contract_guard() {
    use dot_agent_deck::agent_pty::PTY_RESIZE_DIM_MAX;

    // Short and very wide: the cap only bites on the column axis here, which
    // keeps the fixture's cell count (and the buffer below) small.
    const ROWS: u16 = 4;
    let mut parser = vt100::Parser::new(ROWS, PTY_RESIZE_DIM_MAX, 0);
    parser.process(b"\x1b[1;1HCAPPED_ROW_0");

    // Outer area 2 cols wider than the capped screen → inner width
    // PTY_RESIZE_DIM_MAX + 2, which `resize_panes_to_layout` legitimately
    // declines to size the PTY to.
    let area = Rect {
        x: 0,
        y: 0,
        width: PTY_RESIZE_DIM_MAX + 4,
        height: ROWS + 2,
    };
    let inner = inner_of(area);
    assert!(
        inner.width > PTY_RESIZE_DIM_MAX,
        "fixture must draw into an inner area wider than the cap ({} vs {PTY_RESIZE_DIM_MAX})",
        inner.width
    );

    let parser = Arc::new(Mutex::new(parser));
    let widget =
        TerminalWidget::new(parser, "wide-pane".to_string(), true).contract_guaranteed(true);
    let mut buf = Buffer::empty(area);
    // Reaching past this call in a debug build is the assertion: before #747 the
    // invariant-3 `debug_assert!` compared the parser against the RAW inner area
    // and would panic here.
    widget.render(area, &mut buf);

    let top = inner_row(&buf, inner, 0);
    assert!(
        top.starts_with("CAPPED_ROW_0"),
        "the child's capped-width content must still render from the top-left, got {:?}",
        &top[..top.len().min(40)]
    );
    assert_eq!(
        top.trim_end(),
        "CAPPED_ROW_0",
        "columns past the child's capped width must render blank — the child has no \
         more columns to show, so painting anything there would be invented content"
    );
}
