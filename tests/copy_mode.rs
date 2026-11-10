//! PRD #385 M5 — L1 coverage of keyboard copy mode: entering it with `v` in
//! command mode, what it draws on the focused pane (mode chip, cursor,
//! selection), how it leaves (yank, `Esc`, `q`), and that the pane it was
//! entered on keeps its own scroll position and is not moved by live output.
//!
//! Driven through `CopyModeSeam`, which feeds keys to the live loop's key
//! handler and draws with the live render functions. Copies go to a recording
//! clipboard, so no test here reaches a real clipboard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use dot_agent_deck::clipboard::OSC52_ONLY_MESSAGE;
use dot_agent_deck::ui::CopyModeSeam;
use spec::spec;

const WIDTH: u16 = 30;
const HEIGHT: u16 = 12;

/// Thirty numbered lines, the last with no newline after it, so the pane's
/// cursor sits at the end of `line-30` and the earlier lines are history.
fn numbered_lines() -> Vec<u8> {
    (1..=30)
        .map(|n| format!("line-{n:02}"))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(seam: &mut CopyModeSeam, code: KeyCode, times: usize) {
    for _ in 0..times {
        seam.key(key(code));
    }
}

fn chars(seam: &mut CopyModeSeam, keys: &str) {
    for c in keys.chars() {
        seam.key(key(KeyCode::Char(c)));
    }
}

/// The copy-mode cursor's cell is drawn on a yellow background.
fn is_cursor(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .is_some_and(|cell| cell.bg == Color::Yellow)
}

/// A selected cell is drawn on the selection highlight's light-cyan
/// background, the one a mouse selection uses.
fn is_selected(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .is_some_and(|cell| cell.bg == Color::LightCyan)
}

fn cells(buf: &Buffer, pred: impl Fn(&Buffer, u16, u16) -> bool) -> Vec<(u16, u16)> {
    let area = buf.area;
    (area.y..area.y + area.height)
        .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
        .filter(|&(x, y)| pred(buf, x, y))
        .collect()
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (buf.area.x..buf.area.x + buf.area.width)
        .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The text of the selected and cursor cells, one line per row.
fn marked_text(buf: &Buffer) -> String {
    let marked = cells(buf, |b, x, y| is_selected(b, x, y) || is_cursor(b, x, y));
    let mut rows: Vec<u16> = marked.iter().map(|&(_, y)| y).collect();
    rows.dedup();
    rows.iter()
        .map(|&y| {
            marked
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

/// The pane's content rows, as text.
fn pane_text(buf: &Buffer, rect: Rect) -> String {
    (rect.y + 1..rect.y + rect.height - 1)
        .map(|y| {
            (rect.x + 1..rect.x + rect.width - 1)
                .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The bottom bar's row.
fn bar(buf: &Buffer) -> String {
    row_text(buf, buf.area.y + buf.area.height - 1)
}

/// The frame as text, each row followed by a marker row when it has marked
/// cells: `^` for a selected cell, `C` for the cursor.
fn frame_with_marks(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.y + area.height {
        out.push_str(&row_text(buf, y));
        out.push('\n');
        let marks: String = (area.x..area.x + area.width)
            .map(|x| {
                if is_cursor(buf, x, y) {
                    'C'
                } else if is_selected(buf, x, y) {
                    '^'
                } else {
                    ' '
                }
            })
            .collect();
        if !marks.trim().is_empty() {
            out.push_str(marks.trim_end());
            out.push('\n');
        }
    }
    out
}

/// Scenario: A pane has printed thirty lines and the deck is in command mode. Pressing `v` enters copy mode on that pane: the bottom bar's mode chip reads COPY, and the cursor starts where the pane's own cursor is. Moving up two lines and to the start, pressing `v` and moving right six cells highlights exactly `line-28`, with the cursor drawn at its last cell; `y` then copies `line-28`, reports the OSC 52 send and returns to command mode.
#[spec("copymode/render/001")]
#[test]
fn copymode_render_001_copy_mode_draws_its_chip_cursor_and_selection() {
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    let buf = seam.render();
    assert!(
        bar(&buf).starts_with(" COMMAND"),
        "starts in command mode: {:?}",
        bar(&buf)
    );

    chars(&mut seam, "v");
    let buf = seam.render();
    assert!(
        bar(&buf).starts_with(" COPY"),
        "the mode chip must say copy mode:\n{}",
        frame_with_marks(&buf)
    );
    let pane = seam.pane_rect();
    let cursor = cells(&buf, is_cursor);
    assert_eq!(
        cursor,
        [(pane.x + 1 + 7, pane.y + pane.height - 2)],
        "the cursor starts at the pane's own cursor, after `line-30`:\n{}",
        frame_with_marks(&buf)
    );

    chars(&mut seam, "kk");
    press(&mut seam, KeyCode::Left, 7);
    chars(&mut seam, "v");
    chars(&mut seam, "llllll");
    let buf = seam.render();
    assert_eq!(marked_text(&buf), "line-28", "{}", frame_with_marks(&buf));
    assert_eq!(cells(&buf, is_cursor).len(), 1, "one cursor cell");
    insta::assert_snapshot!(frame_with_marks(&buf));

    chars(&mut seam, "y");
    assert_eq!(seam.copies(), ["line-28"]);
    assert_eq!(seam.status(), Some(OSC52_ONLY_MESSAGE));
    let buf = seam.render();
    assert!(
        cells(&buf, is_cursor).is_empty() && cells(&buf, is_selected).is_empty(),
        "leaving copy mode removes its cursor and selection:\n{}",
        frame_with_marks(&buf)
    );
}

/// Scenario: In copy mode the user pages back, sets a selection and leaves with `Esc`, and again with `q` after scrolling the pane back in command mode first. Neither copies anything or sets a status message, and the pane is left at the scroll position it had before copy mode, showing what it showed before.
#[spec("copymode/keys/002")]
#[test]
fn copymode_keys_002_esc_and_q_leave_without_copying_and_restore_scroll() {
    // Esc, from a pane following its live output.
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    let pane = seam.pane_rect();
    let before = pane_text(&seam.render(), pane);
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::PageUp, 2);
    let paged = seam.render();
    assert!(bar(&paged).starts_with(" COPY"), "{}", bar(&paged));
    assert_ne!(
        pane_text(&paged, pane),
        before,
        "PageUp pages copy mode's view"
    );
    chars(&mut seam, "vj");
    press(&mut seam, KeyCode::Esc, 1);
    let buf = seam.render();
    assert!(bar(&buf).starts_with(" COMMAND"), "{}", bar(&buf));
    assert!(seam.copies().is_empty(), "Esc copies nothing");
    assert_eq!(seam.status(), None);
    assert_eq!(seam.scrollback(), 0, "the pane still follows its output");
    assert_eq!(pane_text(&buf, pane), before);

    // q, from a pane the user had scrolled back in command mode.
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    press(&mut seam, KeyCode::PageUp, 1);
    let scrolled = seam.scrollback();
    assert!(scrolled > 0, "command mode's PageUp scrolls the pane back");
    let before = pane_text(&seam.render(), pane);
    chars(&mut seam, "v");
    assert_eq!(
        pane_text(&seam.render(), pane),
        before,
        "copy mode opens on the view the pane showed"
    );
    chars(&mut seam, "gvjjG");
    chars(&mut seam, "q");
    let buf = seam.render();
    assert!(bar(&buf).starts_with(" COMMAND"), "{}", bar(&buf));
    assert!(seam.copies().is_empty(), "q copies nothing");
    assert_eq!(seam.status(), None);
    assert_eq!(
        seam.scrollback(),
        scrolled,
        "the pane's scroll position is restored"
    );
    assert_eq!(pane_text(&buf, pane), before);
}

/// Scenario: While copy mode is open, the pane's program prints forty more lines. The view copy mode shows does not move, so the text under the cursor stays put, and no new line appears in it even at the bottom of its history; after leaving copy mode the pane shows the new output.
#[spec("copymode/keys/002")]
#[test]
fn copymode_keys_002_live_output_does_not_move_the_copy_mode_view() {
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    let pane = seam.pane_rect();
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::PageUp, 1);
    let frozen = pane_text(&seam.render(), pane);
    let new_output: String = (1..=40).map(|n| format!("\r\nnew-{n:02}")).collect();
    seam.feed(new_output.as_bytes());
    assert_eq!(
        pane_text(&seam.render(), pane),
        frozen,
        "output must not move the view under the cursor"
    );
    chars(&mut seam, "G");
    let bottom = pane_text(&seam.render(), pane);
    assert!(
        bottom.contains("line-30") && !bottom.contains("new-"),
        "copy mode works on the pane as it was when copy mode started:\n{bottom}"
    );
    chars(&mut seam, "vy");
    assert_eq!(seam.copies().len(), 1);
    assert!(!seam.copies()[0].contains("new-"), "{:?}", seam.copies());
    let after = pane_text(&seam.render(), pane);
    assert!(
        after.contains("new-40"),
        "after copy mode the pane follows its output again:\n{after}"
    );
}

/// Scenario: Copy mode is entered on a pane that has printed nothing: it opens with the cursor at the pane's first cell, which `PageUp` and `g` leave there because there is no history, while `G` goes to the last row; `y` with no selection leaves copy mode open, and selecting the empty cell and yanking leaves copy mode without copying anything or setting a status. On a pane showing a full-screen program's alternate screen, copy mode sees only that screen (it keeps no history) and copies from it.
#[spec("copymode/keys/002")]
#[test]
fn copymode_keys_002_an_empty_pane_and_the_alternate_screen() {
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, b"");
    let pane = seam.pane_rect();
    let first_cell = (pane.x + 1, pane.y + 1);
    let last_row = (pane.x + 1, pane.y + pane.height - 2);
    chars(&mut seam, "v");
    let buf = seam.render();
    assert!(bar(&buf).starts_with(" COPY"), "{}", bar(&buf));
    assert_eq!(cells(&buf, is_cursor), [first_cell]);
    press(&mut seam, KeyCode::PageUp, 1);
    chars(&mut seam, "g");
    assert_eq!(
        cells(&seam.render(), is_cursor),
        [first_cell],
        "there is no history above the first row"
    );
    chars(&mut seam, "G");
    assert_eq!(cells(&seam.render(), is_cursor), [last_row]);
    chars(&mut seam, "g");
    assert!(pane_text(&seam.render(), pane).trim().is_empty());
    chars(&mut seam, "y");
    assert!(
        bar(&seam.render()).starts_with(" COPY"),
        "y with nothing selected leaves copy mode open"
    );
    chars(&mut seam, "vy");
    assert!(bar(&seam.render()).starts_with(" COMMAND"));
    assert!(
        seam.copies().is_empty(),
        "an empty selection copies nothing"
    );
    assert_eq!(seam.status(), None);

    let mut screen = numbered_lines();
    screen.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2Jfull-screen-app");
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &screen);
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::PageUp, 3);
    chars(&mut seam, "g");
    let shown = pane_text(&seam.render(), pane);
    assert!(
        shown.starts_with("full-screen-app") && !shown.contains("line-"),
        "the alternate screen has no history to page into:\n{shown}"
    );
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::Right, 14);
    press(&mut seam, KeyCode::Enter, 1);
    assert_eq!(seam.copies(), ["full-screen-app"]);
    assert!(bar(&seam.render()).starts_with(" COMMAND"));
}

/// Scenario: Copy mode is open on a pane when that pane closes. The next frame closes copy mode and the bottom bar is back in command mode, with nothing copied.
#[spec("copymode/keys/002")]
#[test]
fn copymode_keys_002_copy_mode_closes_with_its_pane() {
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    chars(&mut seam, "vkv");
    assert!(bar(&seam.render()).starts_with(" COPY"));
    seam.close_pane();
    seam.render();
    let buf = seam.render();
    assert!(bar(&buf).starts_with(" COMMAND"), "{}", bar(&buf));
    assert!(seam.copies().is_empty());
}
