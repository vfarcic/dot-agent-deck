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
    let mut parser = dot_agent_deck::vt_grid::parser(8, 20, 0);
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
    let mut parser = dot_agent_deck::vt_grid::parser(3, 6, 0);
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
    let mut parser = dot_agent_deck::vt_grid::parser(ROWS, PTY_RESIZE_DIM_MAX, 0);
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

/// Scenario: A vt100 screen holds `你好AB` on its first row: two two-column
/// glyphs, then two letters. Render it through `TerminalWidget`. Each glyph
/// takes the two columns it takes on the screen and nothing more, so `A` is
/// drawn in the inner area's fifth column, as it is on the screen, and the
/// row reads `你好AB` with no gap after either glyph.
#[spec("render/widget/004")]
#[test]
fn widget_004_a_wide_glyph_takes_its_own_two_columns_and_no_more() {
    let mut parser = dot_agent_deck::vt_grid::parser(3, 12, 0);
    parser.process("你好AB".as_bytes());
    assert_eq!(parser.screen().cell(0, 4).unwrap().contents(), "A");
    let area = Rect::new(0, 0, 14, 5);
    let buf = render_widget(parser, area, false);
    let inner = inner_of(area);
    assert_eq!(
        buf[(inner.x + 4, inner.y)].symbol(),
        "A",
        "`A` is drawn in the column it has on the screen:\n{}",
        inner_dump(&buf, inner)
    );
    assert_eq!(buf[(inner.x + 5, inner.y)].symbol(), "B");
    assert_eq!(buf[(inner.x, inner.y)].symbol(), "你");
    assert_eq!(buf[(inner.x + 2, inner.y)].symbol(), "好");
}

/// Scenario: Vt100 screens hold a zero-width-joiner emoji (`👩‍💻AB`, which the screen keeps as two two-column cells) and a regional-indicator flag (`🇺🇸AB`, two one-column cells), each followed by two letters. Render each through `TerminalWidget`. Every screen cell is drawn in its own column with that cell's own text, so the letters sit where the screen has them and a selection made in screen columns shows the text it copies.
#[spec("render/widget/004")]
#[test]
fn widget_004_every_screen_cell_is_drawn_in_its_own_column() {
    for text in ["👩\u{200d}💻AB", "🇺🇸AB", "你好AB"] {
        let mut parser = dot_agent_deck::vt_grid::parser(3, 12, 0);
        parser.process(text.as_bytes());
        let cells: Vec<(u16, String)> = (0..12)
            .filter_map(|col| {
                let cell = parser.screen().cell(0, col)?;
                (!cell.is_wide_continuation() && !cell.contents().is_empty())
                    .then(|| (col, cell.contents().to_string()))
            })
            .collect();
        let area = Rect::new(0, 0, 14, 5);
        let buf = render_widget(parser, area, false);
        let inner = inner_of(area);
        for (col, contents) in cells {
            assert_eq!(
                buf[(inner.x + col, inner.y)].symbol(),
                contents,
                "{text:?}: screen column {col} is drawn in its own column:\n{}",
                inner_dump(&buf, inner)
            );
        }
    }
}

/// Render `parser`'s screen through a `TerminalWidget` into a fresh buffer
/// sized to `area`, keeping the parser so the next frame reads the same screen.
fn render_shared(parser: &Arc<Mutex<vt100::Parser>>, area: Rect) -> Buffer {
    let widget = TerminalWidget::new(parser.clone(), "pane".to_string(), false);
    let mut buf = Buffer::empty(area);
    widget.render(area, &mut buf);
    buf
}

/// The bytes the deck's backend (`WideGlyphBackend` over ratatui's
/// `CrosstermBackend`) writes to bring a terminal showing `prev` to `next`,
/// through the same `Buffer::diff` the deck's frames use.
fn emitted_bytes(prev: &Buffer, next: &Buffer) -> Vec<u8> {
    use dot_agent_deck::wide_glyph_backend::WideGlyphBackend;
    use ratatui::backend::{Backend, CrosstermBackend};
    let mut bytes = Vec::new();
    {
        let mut backend = WideGlyphBackend::new(CrosstermBackend::new(&mut bytes));
        backend
            .draw(prev.diff(next).into_iter())
            .expect("drawing into a Vec cannot fail");
        Backend::flush(&mut backend).expect("flushing a Vec cannot fail");
    }
    bytes
}

/// Every cell of a vt100 `display` as one line per row, with the colours and
/// attributes a reader would see, so two displays compare cell for cell.
fn display_dump(display: &vt100::Parser) -> String {
    let (rows, cols) = dot_agent_deck::vt_grid::size(display.screen());
    (0..rows)
        .map(|row| {
            let cells: Vec<String> = (0..cols)
                .map(|col| {
                    let cell = display.screen().cell(row, col).expect("in bounds");
                    let contents = match cell.contents() {
                        "" => " ",
                        other => other,
                    };
                    format!(
                        "{contents}{}{}{}{}{}",
                        if cell.is_wide_continuation() { "~" } else { "" },
                        if cell.inverse() { "!" } else { "" },
                        if cell.underline() { "_" } else { "" },
                        match cell.bgcolor() {
                            vt100::Color::Default => String::new(),
                            other => format!("bg{other:?}"),
                        },
                        match cell.fgcolor() {
                            vt100::Color::Default => String::new(),
                            other => format!("fg{other:?}"),
                        },
                    )
                })
                .collect();
            format!("  [{row}] {}", cells.join("|"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a host terminal draws an emoji-presentation symbol (a symbol followed by
/// VS16, U+FE0F, such as `☀️`): two columns wide, as vt100 and the pane's own
/// screen do, or one column wide, as xterm.js with its Unicode 6 or 11 width
/// tables does.
#[derive(Clone, Copy, Debug)]
enum Host {
    TwoColumnVs16,
    OneColumnVs16,
}

const HOSTS: [Host; 2] = [Host::TwoColumnVs16, Host::OneColumnVs16];

/// A host terminal the deck's output is drawn on. The one-column host is
/// vt100 reading the output without its VS16 characters, so the symbol before
/// each takes the one column its text presentation takes.
struct HostDisplay {
    host: Host,
    screen: vt100::Parser,
}

impl HostDisplay {
    fn new(host: Host, area: Rect) -> Self {
        Self {
            host,
            screen: dot_agent_deck::vt_grid::parser(area.height, area.width, 0),
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        match self.host {
            Host::TwoColumnVs16 => self.screen.process(bytes),
            Host::OneColumnVs16 => {
                let vs16 = "\u{fe0f}".as_bytes();
                let mut kept = Vec::with_capacity(bytes.len());
                let mut rest = bytes;
                while !rest.is_empty() {
                    if rest.starts_with(vs16) {
                        rest = &rest[vs16.len()..];
                    } else {
                        kept.push(rest[0]);
                        rest = &rest[1..];
                    }
                }
                self.screen.process(&kept);
            }
        }
    }

    /// The display after a fresh render of `frame`.
    fn fresh(host: Host, frame: &Buffer) -> Self {
        let mut display = Self::new(host, frame.area);
        display.process(&emitted_bytes(&Buffer::empty(frame.area), frame));
        display
    }

    fn dump(&self) -> String {
        display_dump(&self.screen)
    }
}

/// Draw `before` on a pane, then rewrite the pane's screen as `after`, and
/// return what `host` shows after the incremental redraw and what it shows
/// after a fresh render of the same second frame.
fn incremental_and_fresh(host: Host, before: &str, after: &str) -> (String, String) {
    let area = Rect::new(0, 0, 18, 5);
    let pane = Arc::new(Mutex::new(dot_agent_deck::vt_grid::parser(3, 16, 0)));
    pane.lock().unwrap().process(before.as_bytes());
    let first = render_shared(&pane, area);
    let mut incremental = HostDisplay::fresh(host, &first);

    pane.lock()
        .unwrap()
        .process(format!("\x1b[0m\x1b[H\x1b[2J{after}").as_bytes());
    let second = render_shared(&pane, area);
    incremental.process(&emitted_bytes(&first, &second));
    let fresh = HostDisplay::fresh(host, &second);
    (incremental.dump(), fresh.dump())
}

/// Scenario: A pane shows `☀︎AB` (a sun in text presentation, one column), then its program rewrites the row as `☀️AB` (emoji presentation, two columns); separately, `xZAB` is rewritten as `☀️AB`, `❤️AB` and `1️⃣AB`. Draw both frames through `TerminalWidget`, ratatui's buffer diff and the deck's backend onto a host that draws such a symbol two columns wide and onto one that draws it one column wide. On each host the incremental redraw shows the same row as a fresh render, with `A` and `B` in place and nothing left over in the symbol's second column.
#[spec("render/widget/005")]
#[test]
fn widget_005_a_glyph_that_turns_wide_does_not_shift_the_rest_of_its_row() {
    for host in HOSTS {
        for (before, after) in [
            ("☀\u{fe0e}AB", "☀\u{fe0f}AB"),
            ("xZAB", "☀\u{fe0f}AB"),
            ("xZAB", "❤\u{fe0f}AB"),
            ("xZAB", "1\u{fe0f}\u{20e3}AB"),
        ] {
            let (incremental, fresh) = incremental_and_fresh(host, before, after);
            assert_eq!(
                incremental, fresh,
                "{host:?}, {before:?} -> {after:?}: after an incremental redraw the terminal \
                 shows what a fresh render shows\nincremental:\n{incremental}\nfresh:\n{fresh}"
            );
        }
    }
}

/// Scenario: For each pair of pane contents — wide glyphs replaced by narrow text, a styled wide glyph replaced by narrow ones, a wide glyph replaced by a sequence whose width the screen and ratatui measure differently, and the sun turning narrow again — draw the first and then the second through `TerminalWidget`, ratatui's buffer diff and the deck's backend onto a host that draws an emoji-presentation symbol two columns wide and onto one that draws it one column wide. On each host every incremental redraw shows what a fresh render of the second frame shows.
#[spec("render/widget/005")]
#[test]
fn widget_005_every_width_transition_redraws_as_a_fresh_render_does() {
    for (before, after) in [
        ("你好AB", "xyAB"),
        ("👩\u{200d}💻AB", "xyAB"),
        ("☀\u{fe0f}AB", "xyAB"),
        ("☀\u{fe0f}AB", "☀\u{fe0e}AB"),
        ("你好AB", "☀\u{fe0f}AB"),
        ("☀\u{fe0f}AB", "👩\u{200d}💻AB"),
        ("👩\u{200d}💻AB", "☀\u{fe0f}AB"),
        ("\u{1b}[4;7;44m☀\u{fe0f}AB", "x "),
        ("\u{1b}[4;7;44m👩\u{200d}💻AB", "x "),
        ("\u{1b}[4;7;44m你 ", "☀\u{fe0f} "),
        ("\u{1b}[4;7;44m你", "ｶﾞ"),
        ("\u{1b}[4;7;44m你", "⌚\u{fe0e}"),
        ("\u{1b}[4;7;44m你", "🙂\u{fe0e}"),
        ("\u{1b}[4;7;44m🙂\u{fe0e}", "x"),
    ] {
        for host in HOSTS {
            let (incremental, fresh) = incremental_and_fresh(host, before, after);
            assert_eq!(
                incremental, fresh,
                "{host:?}, {before:?} -> {after:?}: after an incremental redraw the terminal \
                 shows what a fresh render shows\nincremental:\n{incremental}\nfresh:\n{fresh}"
            );
        }
    }
}

/// The text a host display shows in the cells drawn with the selection's
/// highlight, one line per row that has any.
fn display_highlighted_text(display: &HostDisplay) -> String {
    // The selection's light cyan background, as the crossterm backend sends it.
    let highlight = vt100::Color::Idx(14);
    let (rows, cols) = dot_agent_deck::vt_grid::size(display.screen.screen());
    (0..rows)
        .filter_map(|row| {
            let text: String = (0..cols)
                .filter_map(|col| display.screen.screen().cell(row, col))
                .filter(|cell| cell.bgcolor() == highlight && !cell.is_wide_continuation())
                .map(|cell| match cell.contents() {
                    "" => " ".to_string(),
                    other => other.to_string(),
                })
                .collect();
            (!text.is_empty()).then(|| text.trim_end().to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Scenario: A pane shows `xZAB`; the user drags over `xZ` and releases, then the pane's program rewrites the row as `☀️AB`, the user clicks a blank cell on another row and drags over `A` and `B`. Every frame of that sequence is drawn through the deck's backend onto a host that draws `☀️` two columns wide and onto one that draws it one column wide. On each host the highlighted text is `AB`, the last copy, and the display matches a fresh render of the last frame.
#[spec("render/widget/006")]
#[test]
fn widget_006_a_highlight_over_a_row_that_turned_wide_shows_what_was_copied_on_any_host() {
    use crossterm::event::{MouseButton, MouseEventKind};
    use dot_agent_deck::ui::PaneSelectionSeam;

    for host in HOSTS {
        let mut seam = PaneSelectionSeam::tiled(20, 6, &[("a", b"xZAB")], "a");
        let mut frames = vec![seam.render()];
        let rect = seam.pane_rect("a");
        let (x, y) = (rect.x + 1, rect.y + 1);
        let drag = |seam: &mut PaneSelectionSeam, from: u16, to: u16, row: u16| {
            seam.mouse(MouseEventKind::Down(MouseButton::Left), from, row);
            if to != from {
                seam.mouse(MouseEventKind::Drag(MouseButton::Left), to, row);
            }
            seam.mouse(MouseEventKind::Up(MouseButton::Left), to, row);
        };

        drag(&mut seam, x, x + 1, y);
        frames.push(seam.render());
        assert_eq!(seam.copies().last().map(String::as_str), Some("xZ"));

        seam.feed("a", "\x1b[0m\x1b[H\x1b[2J☀\u{fe0f}AB".as_bytes());
        frames.push(seam.render());
        // A click on a blank row, so the next press is not a double-click.
        drag(&mut seam, x, x, y + 2);
        frames.push(seam.render());
        drag(&mut seam, x + 2, x + 3, y);
        frames.push(seam.render());
        assert_eq!(seam.copies().last().map(String::as_str), Some("AB"));

        let mut display = HostDisplay::fresh(host, &frames[0]);
        for pair in frames.windows(2) {
            display.process(&emitted_bytes(&pair[0], &pair[1]));
        }
        let last = frames.last().expect("frames were drawn");
        let fresh = HostDisplay::fresh(host, last);
        assert_eq!(
            display_highlighted_text(&display),
            "AB",
            "{host:?}: the highlight shows other text than the last copy\n{}",
            display.dump()
        );
        assert_eq!(
            display.dump(),
            fresh.dump(),
            "{host:?}: after the incremental redraws the terminal shows what a fresh render shows"
        );
    }
}
