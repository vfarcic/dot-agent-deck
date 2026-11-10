//! PRD #385 M2/M3 — L1 coverage of pane text selection: which pane a press
//! selects in, where the highlight is drawn, how long it stays, and that it
//! never shows text other than what was copied.
//!
//! Driven through `PaneSelectionSeam`, which tiles inert panes over a
//! `TestBackend` frame and feeds mouse reports and keys to the functions the
//! live event loop calls. Copies are collected by the seam, never delivered,
//! so no test here reaches a clipboard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use dot_agent_deck::ui::PaneSelectionSeam;
use spec::spec;

const LEFT_PANE: &[u8] = b"alpha-left-text";
const RIGHT_PANE: &[u8] = b"bravo-right-text";

/// Two panes, `a` and `b`, tiled over a 40x12 frame, with `focused` focused.
fn two_panes(focused: &str) -> PaneSelectionSeam {
    let mut seam =
        PaneSelectionSeam::tiled(40, 12, &[("a", LEFT_PANE), ("b", RIGHT_PANE)], focused);
    seam.render();
    seam
}

fn press(seam: &mut PaneSelectionSeam, col: u16, row: u16) {
    seam.mouse(MouseEventKind::Down(MouseButton::Left), col, row);
}

fn drag(seam: &mut PaneSelectionSeam, col: u16, row: u16) {
    seam.mouse(MouseEventKind::Drag(MouseButton::Left), col, row);
}

fn release(seam: &mut PaneSelectionSeam, col: u16, row: u16) {
    seam.mouse(MouseEventKind::Up(MouseButton::Left), col, row);
}

/// The first content cell of a pane drawn in `rect`.
fn first_cell(rect: Rect) -> (u16, u16) {
    (rect.x + 1, rect.y + 1)
}

/// Drag across the first five cells of `b`'s first row (`bravo`) and release.
fn select_bravo(seam: &mut PaneSelectionSeam) {
    let (x, y) = first_cell(seam.pane_rect("b"));
    press(seam, x, y);
    drag(seam, x + 4, y);
    release(seam, x + 4, y);
}

fn is_highlighted(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .is_some_and(|cell| cell.bg == Color::LightCyan)
}

/// Every highlighted cell, row by row.
fn highlighted(buf: &Buffer) -> Vec<(u16, u16)> {
    let area = buf.area;
    (area.y..area.y + area.height)
        .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
        .filter(|&(x, y)| is_highlighted(buf, x, y))
        .collect()
}

/// The text the highlighted cells show, one line per highlighted row.
fn highlighted_text(buf: &Buffer) -> String {
    let cells = highlighted(buf);
    let mut rows: Vec<u16> = cells.iter().map(|&(_, y)| y).collect();
    rows.dedup();
    rows.iter()
        .map(|&y| {
            cells
                .iter()
                .filter(|&&(_, cy)| cy == y)
                .map(|&(x, _)| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn inside_content(rect: Rect, (x, y): (u16, u16)) -> bool {
    x > rect.x && x < rect.x + rect.width - 1 && y > rect.y && y < rect.y + rect.height - 1
}

/// The frame as text, each row followed by a marker row under its highlighted
/// cells when it has any.
fn frame_with_highlight(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.y + area.height {
        let row: String = (area.x..area.x + area.width)
            .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect();
        out.push_str(row.trim_end());
        out.push('\n');
        if (area.x..area.x + area.width).any(|x| is_highlighted(buf, x, y)) {
            let marks: String = (area.x..area.x + area.width)
                .map(|x| if is_highlighted(buf, x, y) { '^' } else { ' ' })
                .collect();
            out.push_str(marks.trim_end());
            out.push('\n');
        }
    }
    out
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Scenario: Two panes are tiled and the first is focused. A left press in the second pane focuses it and starts a selection there in the same gesture, and dragging along its first row highlights exactly those cells of that pane, never the previously focused one.
#[spec("mouse/select/002")]
#[test]
fn select_002_a_press_in_a_non_focused_pane_focuses_it_and_selects_there() {
    let mut seam = two_panes("a");
    let a = seam.pane_rect("a");
    let b = seam.pane_rect("b");
    let (x, y) = first_cell(b);

    press(&mut seam, x, y);
    assert_eq!(
        seam.focused_pane_id().as_deref(),
        Some("b"),
        "a press in a pane that is not focused must focus it"
    );
    drag(&mut seam, x + 4, y);
    let buf = seam.render();

    let cells = highlighted(&buf);
    assert!(!cells.is_empty(), "the drag must show a highlight");
    assert!(
        cells.iter().all(|&cell| inside_content(b, cell)),
        "the highlight must be on the pane the drag was made in ({b:?}), got {cells:?}"
    );
    assert!(
        !cells.iter().any(|&cell| inside_content(a, cell)),
        "the highlight must not be on the previously focused pane"
    );
    assert_eq!(highlighted_text(&buf), "bravo");
    insta::assert_snapshot!(frame_with_highlight(&buf));
}

/// Scenario: A drag across the second pane's first word copies `bravo` and the highlight stays on screen after the release. Each of these then removes it: the next mouse press, a key typed into that pane, a tab switch, and that pane closing. Esc typed into the pane goes to its program and leaves the highlight in place.
#[spec("mouse/select/003")]
#[test]
fn select_003_a_drag_selection_stays_until_something_clears_it() {
    // Survives its own release.
    let mut seam = two_panes("b");
    select_bravo(&mut seam);
    assert_eq!(seam.copies(), ["bravo"], "the release copies the selection");
    assert_eq!(
        highlighted_text(&seam.render()),
        "bravo",
        "the highlight must stay after the drag is released"
    );

    // The next mouse press clears it, wherever it lands.
    let (ax, ay) = first_cell(seam.pane_rect("a"));
    let b = seam.pane_rect("b");
    press(&mut seam, ax, ay);
    let buf = seam.render();
    assert!(
        !highlighted(&buf)
            .iter()
            .any(|&cell| inside_content(b, cell)),
        "the next press must clear the selection:\n{}",
        frame_with_highlight(&buf)
    );
    release(&mut seam, ax, ay);
    assert!(
        highlighted(&seam.render()).is_empty(),
        "a plain click leaves no highlight"
    );

    // Esc belongs to the pane's program and does not clear; typing does.
    let mut seam = two_panes("b");
    select_bravo(&mut seam);
    seam.key(key(KeyCode::Esc));
    assert_eq!(
        highlighted_text(&seam.render()),
        "bravo",
        "Esc typed into the pane must not clear the selection"
    );
    seam.key(key(KeyCode::Char('x')));
    assert!(
        highlighted(&seam.render()).is_empty(),
        "typing into the pane that owns the selection must clear it"
    );

    // A tab switch clears it: coming back does not bring it back.
    let mut seam = two_panes("b");
    select_bravo(&mut seam);
    assert_eq!(highlighted_text(&seam.render()), "bravo");
    seam.switch_tab();
    seam.render();
    seam.switch_tab();
    assert!(
        highlighted(&seam.render()).is_empty(),
        "a tab switch must clear the selection"
    );

    // The owning pane closing clears it, and it is not drawn on another pane.
    let mut seam = two_panes("b");
    select_bravo(&mut seam);
    assert_eq!(highlighted_text(&seam.render()), "bravo");
    seam.close_pane("b");
    assert!(
        highlighted(&seam.render()).is_empty(),
        "closing the pane that owns the selection must clear it"
    );
}

/// Scenario: After a drag copies `bravo` from the second pane, new output arrives in both panes. Output that leaves the selected text where it is keeps the highlight; output that scrolls, overwrites or clears that text never leaves a highlight showing anything other than `bravo`.
#[spec("mouse/select/004")]
#[test]
fn select_004_a_kept_highlight_never_shows_text_other_than_what_was_copied() {
    let assert_shows_only_the_copy = |seam: &mut PaneSelectionSeam, what: &str| {
        let buf = seam.render();
        let shown = highlighted_text(&buf);
        assert!(
            shown.is_empty() || shown == "bravo",
            "after {what} the highlight shows {shown:?}, but `bravo` is what was copied:\n{}",
            frame_with_highlight(&buf)
        );
    };

    // Output that does not move the selected text keeps the highlight.
    let mut seam = two_panes("b");
    select_bravo(&mut seam);
    assert_eq!(seam.copies(), ["bravo"]);
    assert_eq!(
        highlighted_text(&seam.render()),
        "bravo",
        "the highlight must stay after the drag is released"
    );
    seam.feed("a", b"\r\nmore output in a");
    seam.feed("b", b"\x1b[3;1Hlater line");
    assert_eq!(
        highlighted_text(&seam.render()),
        "bravo",
        "output that leaves the selected text in place must keep the highlight"
    );

    // Output that moves or replaces the selected text.
    let moves: [(&str, &[u8]); 3] = [
        (
            "output that scrolls the pane",
            b"\x1b[99;1H\r\nnew 1\r\nnew 2\r\nnew 3",
        ),
        (
            "output that overwrites the selected text",
            b"\x1b[1;1HBRAVO",
        ),
        ("output that clears the screen", b"\x1b[2J\x1b[1;1H"),
    ];
    for (what, bytes) in moves {
        let mut seam = two_panes("b");
        select_bravo(&mut seam);
        assert_eq!(
            highlighted_text(&seam.render()),
            "bravo",
            "the highlight must stay after the drag is released"
        );
        seam.feed("b", bytes);
        assert_shows_only_the_copy(&mut seam, what);
    }
}

/// Twenty lines, each wider than the 28-column panes these tests draw them
/// in, the last with no newline after it so the cursor sits on the screen's
/// bottom row.
fn wide_lines(rows: usize) -> Vec<u8> {
    (0..rows)
        .map(|n| format!("r{n:02}-abcdefghijklmnopqrstuvwxyz0123456789"))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

/// The drawn width of a pane in a 30-column frame.
const SHOWN: usize = 28;

/// The first `SHOWN` columns of line `n` of [`wide_lines`]: what a pane in a
/// 30-column frame shows of it.
fn shown_line(n: usize) -> String {
    format!("r{n:02}-abcdefghijklmnopqrstuvwxyz0123456789")[..SHOWN].to_string()
}

/// The last copy, and the highlighted text, are both `expected`.
fn assert_highlight_is_the_copy(seam: &mut PaneSelectionSeam, expected: &str, what: &str) {
    assert_eq!(
        seam.copies().last().map(String::as_str),
        Some(expected),
        "{what}: the copy is not what the pane shows there"
    );
    let buf = seam.render();
    assert_eq!(
        highlighted_text(&buf),
        expected,
        "{what}: the highlight does not show what was copied:\n{}",
        frame_with_highlight(&buf)
    );
}

fn double_click(seam: &mut PaneSelectionSeam, col: u16, row: u16) {
    for _ in 0..2 {
        press(seam, col, row);
        release(seam, col, row);
    }
}

fn triple_click(seam: &mut PaneSelectionSeam, col: u16, row: u16) {
    for _ in 0..3 {
        press(seam, col, row);
        release(seam, col, row);
    }
}

/// Scenario: One pane is drawn 28 columns by 6 rows while its terminal screen is larger, as it is while another attached client decides the size or a resize is in flight, so the pane shows the screen's top-left corner. A drag on the first drawn row, a drag across three drawn rows, a double-click on a word that runs past the pane's right edge and a triple-click on a paragraph each copy exactly the text the highlight is drawn over, and nothing the pane does not show.
#[spec("mouse/select/007")]
#[test]
fn select_007_a_screen_larger_than_its_pane_copies_what_is_drawn() {
    // Taller and wider: the cursor is on row 19, far below the drawn rows.
    let tall = wide_lines(20);
    let mut seam = PaneSelectionSeam::tiled_with_screen(30, 8, &[("a", &tall)], "a", 20, 50);
    seam.render();
    let (x, y) = first_cell(seam.pane_rect("a"));
    press(&mut seam, x, y);
    drag(&mut seam, x + 5, y);
    release(&mut seam, x + 5, y);
    assert_highlight_is_the_copy(&mut seam, "r00-ab", "a drag on the first drawn row");

    press(&mut seam, x + 4, y);
    drag(&mut seam, x + 3, y + 2);
    release(&mut seam, x + 3, y + 2);
    let expected = format!("{}\n{}\nr02-", &shown_line(0)[4..], shown_line(1));
    assert_highlight_is_the_copy(&mut seam, &expected, "a drag across three drawn rows");

    // As tall as the pane, only wider: the rows already line up, so this is
    // the columns alone.
    let six = wide_lines(6);
    let mut seam = PaneSelectionSeam::tiled_with_screen(30, 8, &[("a", &six)], "a", 6, 50);
    seam.render();
    let (x, y) = first_cell(seam.pane_rect("a"));
    press(&mut seam, x + 4, y);
    drag(&mut seam, x + 3, y + 2);
    release(&mut seam, x + 3, y + 2);
    assert_highlight_is_the_copy(
        &mut seam,
        &expected,
        "a drag across three rows of a screen wider than its pane",
    );

    double_click(&mut seam, x + 6, y + 1);
    assert_highlight_is_the_copy(
        &mut seam,
        &shown_line(1),
        "a double-click on a word running past the pane's edge",
    );

    triple_click(&mut seam, x + 6, y + 1);
    let paragraph = (0..6).map(shown_line).collect::<Vec<_>>().join("\n");
    assert_highlight_is_the_copy(
        &mut seam,
        &paragraph,
        "a triple-click on a paragraph wider than the pane",
    );

    let mut seam = PaneSelectionSeam::tiled_with_screen(30, 8, &[("a", &tall)], "a", 20, 50);
    seam.render();
    triple_click(&mut seam, x + 6, y + 1);
    assert_highlight_is_the_copy(
        &mut seam,
        &paragraph,
        "a triple-click on a screen taller and wider than its pane",
    );
}

/// Seven lines that fill a 42 x 7 pane's content area.
fn seven_lines() -> Vec<String> {
    (1..=7)
        .map(|n| format!("line-{n} alpha bravo charlie delta echo"))
        .collect()
}

/// Scenario: In command mode the COMMAND MODE banner is drawn over the focused pane, and a pane whose scroll was refused shows its cannot-scroll notice. A drag over the whole pane, through the banner or the notice, highlights the pane's own text, both while dragging and after the release, and the release copies exactly that text.
#[spec("mouse/select/008")]
#[test]
fn select_008_a_selection_over_the_command_banner_or_a_notice_shows_the_pane_text() {
    let lines = seven_lines();
    let bytes = lines.join("\r\n").into_bytes();
    let expected = lines.join("\n");

    for overlay in ["the COMMAND MODE banner", "the cannot-scroll notice"] {
        let mut seam = PaneSelectionSeam::tiled(44, 9, &[("a", &bytes)], "a");
        if overlay == "the COMMAND MODE banner" {
            seam.command_mode(true);
        } else {
            seam.arm_scroll_notice("a");
        }
        let before = seam.render();
        let frame = frame_with_highlight(&before);
        assert!(
            lines.iter().any(|line| !frame.contains(line.as_str())),
            "{overlay} must cover some of the pane's text before the drag:\n{frame}"
        );

        let rect = seam.pane_rect("a");
        let (x, y) = first_cell(rect);
        let (last_x, last_y) = (rect.x + rect.width - 2, rect.y + rect.height - 2);
        press(&mut seam, x, y);
        drag(&mut seam, last_x, last_y);
        let buf = seam.render();
        assert_eq!(
            highlighted_text(&buf),
            expected,
            "while dragging through {overlay} the highlight must show the pane's text:\n{}",
            frame_with_highlight(&buf)
        );
        release(&mut seam, last_x, last_y);
        assert_highlight_is_the_copy(&mut seam, &expected, &format!("a drag through {overlay}"));
    }
}

/// Scenario: In the stacked layout, the one drawn pane's screen is larger than the pane, and a drag on its first drawn row copies exactly the highlighted text. Separately, a frame resize after a copy (the pane's screen not resized yet) never leaves a highlight showing anything other than what was copied.
#[spec("mouse/select/009")]
#[test]
fn select_009_stacked_and_resized_panes_copy_and_show_the_same_text() {
    let tall = wide_lines(20);
    let mut seam =
        PaneSelectionSeam::tiled_with_screen(30, 12, &[("a", b"other"), ("b", &tall)], "b", 20, 50);
    seam.stack();
    seam.render();
    let (x, y) = first_cell(seam.pane_rect("b"));
    press(&mut seam, x, y);
    drag(&mut seam, x + 5, y);
    release(&mut seam, x + 5, y);
    assert_highlight_is_the_copy(&mut seam, "r00-ab", "a drag in a stacked pane");

    for (width, height) in [(50, 14), (30, 8), (40, 12)] {
        let mut seam = two_panes("b");
        select_bravo(&mut seam);
        assert_eq!(highlighted_text(&seam.render()), "bravo");
        seam.resize(width, height);
        let buf = seam.render();
        let shown = highlighted_text(&buf);
        assert!(
            shown.is_empty() || shown == "bravo",
            "after a resize to {width}x{height} the highlight shows {shown:?}, but `bravo` is what was copied:\n{}",
            frame_with_highlight(&buf)
        );
    }
}
