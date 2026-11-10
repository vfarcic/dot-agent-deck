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
use unicode_width::UnicodeWidthStr;

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

/// Scenario: Copy mode is open on a pane when that pane closes. The very next frame is drawn in command mode, its bottom bar without the COPY chip, with nothing copied.
#[spec("copymode/keys/002")]
#[test]
fn copymode_keys_002_copy_mode_closes_with_its_pane() {
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    chars(&mut seam, "vkv");
    assert!(bar(&seam.render()).starts_with(" COPY"));
    seam.close_pane();
    let buf = seam.render();
    assert!(
        bar(&buf).starts_with(" COMMAND"),
        "the first frame after the pane closed must not show the COPY chip: {}",
        bar(&buf)
    );
    assert!(seam.copies().is_empty());
}

/// Thirty lines, each wider than the 28-column pane they are drawn in.
fn wide_lines() -> Vec<u8> {
    (0..30)
        .map(|n| format!("r{n:02}-abcdefghijklmnopqrstuvwxyz0123456789"))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

/// Scenario: The pane is drawn 28 columns by 9 rows while its terminal screen is 50 by 20, so it shows the screen's top-left corner and the screen's cursor is below the drawn rows. Copy mode opens with its cursor on a drawn cell, cannot move it past the pane's right edge, and a selection over two lines yanks exactly the text the highlight is drawn over.
#[spec("copymode/keys/003")]
#[test]
fn copymode_keys_003_a_screen_larger_than_its_pane_selects_what_is_drawn() {
    let mut seam = CopyModeSeam::with_screen(WIDTH, HEIGHT, &wide_lines(), 20, 50);
    seam.render();
    chars(&mut seam, "v");
    let buf = seam.render();
    let rect = seam.pane_rect();
    let cursor = cells(&buf, is_cursor);
    assert_eq!(
        cursor.len(),
        1,
        "copy mode must open with its cursor on a drawn cell of the pane:\n{}",
        pane_text(&buf, rect)
    );

    // Up one line, to column 4, start the selection, down one line and as
    // far right as the cursor goes.
    chars(&mut seam, "k");
    press(&mut seam, KeyCode::Char('h'), 60);
    press(&mut seam, KeyCode::Char('l'), 4);
    chars(&mut seam, "vj");
    press(&mut seam, KeyCode::Char('l'), 60);
    let buf = seam.render();
    let shown = marked_text(&buf);
    let cursor = cells(&buf, is_cursor);
    assert!(
        cursor.iter().all(|&(x, y)| x > rect.x
            && x < rect.x + rect.width - 1
            && y < rect.y + rect.height - 1),
        "the cursor must stay inside the pane, got {cursor:?}"
    );
    chars(&mut seam, "y");
    assert_eq!(
        seam.copies().last().map(String::as_str),
        Some(shown.as_str()),
        "the yank must copy exactly what the highlight shows"
    );
    assert_eq!(
        shown, "abcdefghijklmnopqrstuvwx\nr18-abcdefghijklmnopqrstuvwx",
        "the selection runs over the two drawn lines it was made on"
    );
}

/// The text of the marked (selected or cursor) cells a terminal shows, one
/// line per row: a cell hidden under the wide glyph before it is skipped, as
/// the terminal does not draw it.
fn visible_marked_text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut lines = Vec::new();
    for y in area.y..area.y + area.height {
        let mut line = String::new();
        let mut x = area.x;
        while x < area.x + area.width {
            let symbol = buf.cell((x, y)).map_or(" ", |c| c.symbol());
            if is_selected(buf, x, y) || is_cursor(buf, x, y) {
                line.push_str(symbol);
            }
            x += symbol.width().max(1) as u16;
        }
        if !line.is_empty() {
            lines.push(line.trim_end().to_string());
        }
    }
    lines.join("\n")
}

/// A row as a terminal shows it: a cell hidden under a wide glyph skipped.
fn visible_row(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
    let mut out = String::new();
    let mut x = from;
    while x < to {
        let symbol = buf.cell((x, y)).map_or(" ", |c| c.symbol());
        out.push_str(symbol);
        x += symbol.width().max(1) as u16;
    }
    out.trim_end().to_string()
}

/// The bottom bar's mode chip, from whichever row of a whole frame carries it.
fn chip_row(buf: &Buffer) -> String {
    (buf.area.y..buf.area.y + buf.area.height)
        .map(|y| row_text(buf, y))
        .find(|row| row.starts_with(" COPY") || row.starts_with(" COMMAND"))
        .unwrap_or_default()
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Scenario: Copy mode is open, with a selection, on the only pane of the dashboard when another client stops that pane's agent, so the pane closes and the dashboard has no pane to draw. The first whole frame drawn after that (the run loop's own layout) is in command mode with no COPY chip and no copy-mode cursor, and a later `y` copies nothing. A pane the daemon attaches under the closed pane's id before any frame is drawn does not inherit copy mode either.
#[spec("copymode/keys/004")]
#[test]
fn copymode_keys_004_copy_mode_closes_when_another_client_closes_the_last_pane() {
    let mut seam = CopyModeSeam::new(80, 20, &numbered_lines());
    seam.render_frame();
    chars(&mut seam, "vkv");
    let buf = seam.render_frame();
    assert!(
        chip_row(&buf).starts_with(" COPY"),
        "copy mode opens on the dashboard's pane: {:?}",
        chip_row(&buf)
    );

    seam.close_pane_externally();
    let buf = seam.render_frame();
    assert!(
        chip_row(&buf).starts_with(" COMMAND"),
        "the first frame after the pane closed is in command mode: {:?}",
        chip_row(&buf)
    );
    assert!(
        cells(&buf, is_cursor).is_empty() && cells(&buf, is_selected).is_empty(),
        "no copy-mode cursor or selection is left on screen"
    );
    chars(&mut seam, "y");
    assert!(
        seam.copies().is_empty(),
        "y after the pane closed must copy nothing, got {:?}",
        seam.copies()
    );

    // The pane id is reused before a frame is drawn.
    let mut seam = CopyModeSeam::new(80, 20, &numbered_lines());
    seam.render_frame();
    chars(&mut seam, "vkv");
    seam.close_pane_externally();
    seam.reattach_pane(b"successor-pane-text");
    chars(&mut seam, "y");
    assert!(
        seam.copies().is_empty(),
        "a pane under the reused id must not yank the closed pane's text, got {:?}",
        seam.copies()
    );
    assert_eq!(seam.mode(), "Normal");
}

/// Scenario: Copy mode is open, with a selection, on the dashboard's only pane when that pane goes away without the close handler running (it is only forgotten). The very first whole frame drawn after that is in command mode with no COPY chip and no copy-mode cursor, and a later `y` copies nothing.
#[spec("copymode/keys/004")]
#[test]
fn copymode_keys_004_the_first_frame_without_the_pane_leaves_copy_mode() {
    let mut seam = CopyModeSeam::new(80, 20, &numbered_lines());
    seam.render_frame();
    chars(&mut seam, "vkv");
    let buf = seam.render_frame();
    assert!(
        chip_row(&buf).starts_with(" COPY"),
        "copy mode opens on the dashboard's pane: {:?}",
        chip_row(&buf)
    );

    seam.close_pane();
    let buf = seam.render_frame();
    assert!(
        chip_row(&buf).starts_with(" COMMAND"),
        "the first frame without the pane is in command mode: {:?}",
        chip_row(&buf)
    );
    assert!(
        cells(&buf, is_cursor).is_empty() && cells(&buf, is_selected).is_empty(),
        "no copy-mode cursor or selection is left on screen"
    );
    assert_eq!(seam.mode(), "Normal");
    chars(&mut seam, "y");
    assert!(
        seam.copies().is_empty(),
        "y after the pane went away must copy nothing, got {:?}",
        seam.copies()
    );
}

/// Eight lines whose first eight columns are `VISIBLE<n>` and the rest a
/// suffix a narrower pane does not show.
fn visible_hidden_lines() -> Vec<u8> {
    (0..8)
        .map(|n| format!("VISIBLE{n}-HIDDEN{n}"))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

/// Scenario: The pane's terminal screen is 8 rows by 20 columns while the pane shows only 3 rows of 8 columns, `VISIBLE0` to `VISIBLE2`. Copy mode opens with its cursor on a drawn row, `G` keeps it on a drawn row, and a selection over the first two rows yanks `VISIBLE0` and `V`, without the hidden suffix. Then, on a pane that fits its screen, the frame shrinks while a selection is open: the very next frame draws the cursor inside the smaller pane, and the yank is exactly what that frame highlights.
#[spec("copymode/keys/003")]
#[test]
fn copymode_keys_003_an_oversized_screen_and_a_shrink_while_selected() {
    let mut seam = CopyModeSeam::with_screen(10, 6, &visible_hidden_lines(), 8, 20);
    seam.render();
    let rect = seam.pane_rect();
    let inner_rows = rect.y + 1..rect.y + rect.height - 1;
    assert_eq!(
        pane_text(&seam.render(), rect),
        "VISIBLE0\nVISIBLE1\nVISIBLE2"
    );
    chars(&mut seam, "v");
    let cursor = cells(&seam.render(), is_cursor);
    assert!(
        cursor.len() == 1 && inner_rows.contains(&cursor[0].1),
        "the cursor opens on a drawn row, got {cursor:?}"
    );
    chars(&mut seam, "G");
    let cursor = cells(&seam.render(), is_cursor);
    assert_eq!(
        cursor,
        [(rect.x + 1, rect.y + 3)],
        "G goes to the last drawn row"
    );
    chars(&mut seam, "gvj");
    let buf = seam.render();
    let shown = marked_text(&buf);
    chars(&mut seam, "y");
    assert_eq!(seam.copies(), ["VISIBLE0\nV"]);
    assert_eq!(shown, "VISIBLE0\nV", "{}", frame_with_marks(&buf));

    // Shrink while a selection is open.
    let lines: Vec<u8> = (0..12)
        .map(|n| format!("row{n:02}-abcdefghijklmnopqrstuvwxyz"))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes();
    let mut seam = CopyModeSeam::new(40, 14, &lines);
    seam.render();
    chars(&mut seam, "v");
    // On rows the smaller pane still draws: a selection the shrink leaves
    // entirely undrawn is dropped (`copymode/keys/010`).
    press(&mut seam, KeyCode::Char('k'), 7);
    press(&mut seam, KeyCode::Char('h'), 40);
    chars(&mut seam, "llvj");
    press(&mut seam, KeyCode::Char('l'), 40);
    seam.render();
    seam.resize(24, 8);
    let buf = seam.render();
    let rect = seam.pane_rect();
    let cursor = cells(&buf, is_cursor);
    assert!(
        cursor.len() == 1
            && cursor.iter().all(|&(x, y)| x > rect.x
                && x < rect.x + rect.width - 1
                && y > rect.y
                && y < rect.y + rect.height - 1),
        "the first frame after the shrink draws the cursor inside the pane, got {cursor:?}:\n{}",
        frame_with_marks(&buf)
    );
    let shown = marked_text(&buf);
    chars(&mut seam, "y");
    assert_eq!(
        seam.copies().last().map(String::as_str),
        Some(shown.as_str()),
        "the yank copies what the frame highlights:\n{}",
        frame_with_marks(&buf)
    );
    assert_eq!(shown, "w04-abcdefghijklmnop\nrow05-abcdefghijklmnop");
}

/// Twenty lines `L00-…` to `L19-…`, the last with no newline after it.
fn short_screen_lines() -> Vec<u8> {
    (0..20)
        .map(|n| format!("L{n:02}-text"))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

/// The pane's content rows below the first `drawn` ones: the rows a screen
/// shorter than its pane leaves blank.
fn undrawn_rows(rect: Rect, drawn: u16) -> std::ops::Range<u16> {
    rect.y + 1 + drawn..rect.y + rect.height - 1
}

/// Scenario: The pane's terminal screen is 4 rows tall while the pane has room for more, so the rows below the screen are blank, and copy mode is scrolled back into the history. A selection from a history line down to the last drawn row yanks exactly the highlighted text. A selection that runs on below the drawn rows, made by anchoring on a history line and moving up until the view follows, highlights only drawn rows, and the highlighted text is the part of the yank the pane shows.
#[spec("copymode/keys/003")]
#[test]
fn copymode_keys_003_a_screen_shorter_than_its_pane_marks_only_drawn_rows() {
    let mut seam = CopyModeSeam::with_screen(WIDTH, HEIGHT, &short_screen_lines(), 4, WIDTH);
    seam.render();
    let rect = seam.pane_rect();
    assert!(
        rect.height - 2 > 4 + 2,
        "the pane must have room for blank rows below the screen, got {rect:?}"
    );
    let marked_below = |buf: &Buffer| {
        cells(buf, |b, x, y| is_selected(b, x, y) || is_cursor(b, x, y))
            .into_iter()
            .filter(|&(_, y)| undrawn_rows(rect, 4).contains(&y))
            .collect::<Vec<_>>()
    };

    // Scrolled into the history: from `L10` down to the last drawn row.
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::Char('k'), 9);
    press(&mut seam, KeyCode::Char('h'), 40);
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::Char('j'), 3);
    press(&mut seam, KeyCode::Char('l'), 40);
    let buf = seam.render();
    let shown = marked_text(&buf);
    assert_eq!(marked_below(&buf), [], "{}", frame_with_marks(&buf));
    chars(&mut seam, "y");
    assert_eq!(
        seam.copies().last().map(String::as_str),
        Some(shown.as_str()),
        "the yank copies what the frame highlights:\n{}",
        frame_with_marks(&buf)
    );
    assert_eq!(shown, "L10-text\nL11-text\nL12-text\nL13-text");

    // Anchored on a history line, then moved up so far that the view
    // follows: the anchor's line is now below the drawn rows.
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::Char('k'), 9);
    press(&mut seam, KeyCode::Char('h'), 40);
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::Char('k'), 6);
    let buf = seam.render();
    assert_eq!(
        marked_below(&buf),
        [],
        "no cell below the drawn rows is marked:\n{}",
        frame_with_marks(&buf)
    );
    let shown = marked_text(&buf);
    chars(&mut seam, "y");
    let yanked = seam.copies().last().cloned().unwrap_or_default();
    let in_view = yanked.lines().take(4).collect::<Vec<_>>().join("\n");
    assert_eq!(
        shown,
        in_view,
        "the highlight is the part of the yank the pane draws:\n{}",
        frame_with_marks(&buf)
    );
    assert_eq!(
        yanked,
        "L04-text\nL05-text\nL06-text\nL07-text\nL08-text\nL09-text\nL"
    );
}

/// Scenario: The pane shows `你AB`, a two-column glyph and two letters, and the row is drawn with no gap after the glyph. In copy mode, moving right from the glyph lands on `A` and then `B`; selections forward and backward over the glyph highlight it and copy it whole; a cursor that starts, or moves up, onto the glyph's right half is put on the glyph. The same holds for a zero-width-joiner emoji and a flag, each drawn cell by cell as the screen holds it, and each time the copied text is exactly the highlighted text.
#[spec("copymode/keys/005")]
#[test]
fn copymode_keys_005_a_wide_glyph_is_highlighted_and_copied_whole() {
    let cases: [(&str, &[u8], &str, &str); 8] = [
        ("`gll`, select, yank", "你AB".as_bytes(), "gllvy", "B"),
        (
            "the letter after a joined emoji",
            "👩\u{200d}💻AB".as_bytes(),
            "glllvy",
            "B",
        ),
        (
            "across both cells of a joined emoji",
            "👩\u{200d}💻AB".as_bytes(),
            "gvlly",
            "👩\u{200d}💻A",
        ),
        (
            "a flag's second indicator",
            "🇺🇸AB".as_bytes(),
            "glvly",
            "🇸A",
        ),
        ("forward from the glyph", "你AB".as_bytes(), "gvly", "你A"),
        (
            "backward onto the glyph",
            "你AB".as_bytes(),
            "gllvhhy",
            "你AB",
        ),
        (
            "the pane's cursor on the glyph's right half",
            "你AB\x1b[1;2H".as_bytes(),
            "vly",
            "你A",
        ),
        (
            "moving up onto the glyph's right half",
            "a你b\r\nxyz".as_bytes(),
            "hkvly",
            "你b",
        ),
    ];
    for (what, bytes, keys, expected) in cases {
        let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, bytes);
        let rect = seam.pane_rect();
        let buf = seam.render();
        let first = bytes.split(|&b| b == b'\r' || b == 0x1b).next().unwrap();
        assert_eq!(
            visible_row(&buf, rect.y + 1, rect.x + 1, rect.x + rect.width - 1),
            String::from_utf8_lossy(first),
            "{what}: the row is drawn with no gap after the wide glyph"
        );
        chars(&mut seam, "v");
        let (moves, yank) = keys.split_at(keys.len() - 1);
        chars(&mut seam, moves);
        let buf = seam.render();
        let shown = visible_marked_text(&buf);
        chars(&mut seam, yank);
        assert_eq!(
            seam.copies(),
            [expected],
            "{what}: the copy\n{}",
            frame_with_marks(&buf)
        );
        assert_eq!(
            shown,
            expected,
            "{what}: the highlight\n{}",
            frame_with_marks(&buf)
        );
    }
}

/// Scenario: With `[global] jump_1 = "y"` (and then `= "q"`), copy mode is open on the pane of card 1. `y` yanks and `q` leaves, and neither is then taken as the jump to card 1: the deck is in command mode, the card selection is unchanged, and nothing reached the pane's program. With `[dashboard] copy_mode = "x"`, `x` opens copy mode and `v` does not; binding `copy_mode` to a key `jump_1` already has is reported as a conflict and leaves copy mode unbound.
#[spec("copymode/keys/006")]
#[test]
fn copymode_keys_006_a_remapped_key_is_not_dispatched_twice_and_the_entry_key_remaps() {
    for (binding, keys, copies) in [("y", "vkvhhy", 1), ("q", "vkvhhq", 0)] {
        let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines()).with_card();
        // `jump_1` keeps the key: an action bound to it by default loses it.
        let warnings = seam.bind(&format!("[global]\njump_1 = \"{binding}\"\n"));
        assert!(
            warnings.iter().all(|w| !w.contains("'jump_1' unbound")),
            "{warnings:?}"
        );
        seam.render();
        let selected = seam.selected_index();
        chars(&mut seam, keys);
        assert_eq!(seam.copies().len(), copies, "`{binding}`: the copies");
        assert_eq!(
            seam.mode(),
            "Normal",
            "`{binding}` leaves copy mode for command mode, not a jump into the pane"
        );
        assert_eq!(
            seam.selected_index(),
            selected,
            "`{binding}`: the card selection is unchanged"
        );
        assert!(
            seam.forwarded().is_empty(),
            "`{binding}`: nothing reaches the pane's program"
        );
        assert!(bar(&seam.render()).starts_with(" COMMAND"));
    }

    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    let warnings = seam.bind("[dashboard]\ncopy_mode = \"x\"\n");
    assert!(warnings.is_empty(), "{warnings:?}");
    seam.render();
    chars(&mut seam, "v");
    assert!(bar(&seam.render()).starts_with(" COMMAND"), "v is unbound");
    chars(&mut seam, "x");
    assert!(
        bar(&seam.render()).starts_with(" COPY"),
        "x opens copy mode"
    );

    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    let warnings = seam.bind("[global]\njump_1 = \"x\"\n\n[dashboard]\ncopy_mode = \"x\"\n");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("conflict") && w.contains("copy_mode")),
        "{warnings:?}"
    );
    seam.render();
    chars(&mut seam, "xv");
    assert!(
        bar(&seam.render()).starts_with(" COMMAND"),
        "copy_mode lost the conflict, so neither key opens it"
    );
}

/// Scenario: Copy mode yanks what was written to the cells it highlights and drops only the blank cells nothing was written to. A no-break space selected alone is copied; spaces the program wrote are kept on the row the selection ends on, even past the last letter; a row the selection runs through to its end loses its trailing spaces; never-written cells after the text are not copied.
#[spec("copymode/keys/007")]
#[test]
fn copymode_keys_007_only_unwritten_padding_is_trimmed() {
    let screen = "X\u{a0}Y\r\nab   \r\ncd".as_bytes();
    let cases: [(&str, &str, &str); 4] = [
        ("a no-break space alone", "glvy", "\u{a0}"),
        (
            "written spaces at the selection's end",
            "gjvllllllly",
            "ab   ",
        ),
        ("a row run through to its end", "gjvjy", "ab\nc"),
        ("a selection into unwritten cells", "gjjvlllllly", "cd"),
    ];
    for (what, keys, expected) in cases {
        let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, screen);
        seam.render();
        chars(&mut seam, "v");
        let (moves, yank) = keys.split_at(keys.len() - 1);
        chars(&mut seam, moves);
        seam.render();
        chars(&mut seam, yank);
        assert_eq!(seam.copies(), [expected], "{what}");
    }
}

/// Scenario: Copy mode is open on a pane. Unbound printable keys, digits, `Ctrl+D`, `Ctrl+W`, the movement and selection keys, `PageUp` and a paste from the terminal all stay in the deck: the pane's program receives nothing and copy mode stays open. `Ctrl+C` then closes copy mode and opens the quit dialog, still sending nothing to the program and copying nothing.
#[spec("copymode/keys/008")]
#[test]
fn copymode_keys_008_nothing_typed_in_copy_mode_reaches_the_pane() {
    let mut seam = CopyModeSeam::new(WIDTH, HEIGHT, &numbered_lines());
    seam.render();
    chars(&mut seam, "v");
    assert!(bar(&seam.render()).starts_with(" COPY"));
    assert!(seam.forwarded().is_empty());

    chars(&mut seam, "zZ!1x");
    seam.key(ctrl('d'));
    seam.key(ctrl('w'));
    chars(&mut seam, "hjklv ");
    press(&mut seam, KeyCode::Left, 1);
    press(&mut seam, KeyCode::Up, 1);
    press(&mut seam, KeyCode::PageUp, 1);
    press(&mut seam, KeyCode::Tab, 1);
    press(&mut seam, KeyCode::Backspace, 1);
    seam.paste("pasted-in-copy-mode");
    assert!(
        bar(&seam.render()).starts_with(" COPY"),
        "copy mode is still open"
    );
    assert_eq!(
        String::from_utf8_lossy(&seam.forwarded()),
        "",
        "nothing typed in copy mode reaches the pane's program"
    );

    seam.key(ctrl('c'));
    assert_eq!(seam.mode(), "QuitConfirm");
    assert!(seam.forwarded().is_empty(), "Ctrl+C is not forwarded");
    assert!(seam.copies().is_empty());
}

fn long_rows(count: usize) -> Vec<u8> {
    (0..count)
        .map(|n| format!("row{n:02}-abcdefghijklmnopqrstuvwxyz"))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

/// Scenario: In copy mode the user selects columns 20 to 25 of one line, the terminal is narrowed so the pane draws only 10 columns, and `y` is pressed right after the one frame drawn at the new size. That frame highlights nothing, so the yank copies nothing and copy mode stays open, and a selection made afterwards copies what it highlights. A selection whose first line starts right of the new edge keeps only its drawn part, and a height shrink that moves copy mode's view yanks exactly what the first smaller frame highlights.
#[spec("copymode/keys/010")]
#[test]
fn copymode_keys_010_a_resize_never_lets_a_yank_copy_what_the_frame_did_not_highlight() {
    // A selection entirely right of the new edge.
    let mut seam = CopyModeSeam::new(40, 14, &long_rows(12));
    seam.render();
    chars(&mut seam, "vk");
    press(&mut seam, KeyCode::Char('h'), 40);
    press(&mut seam, KeyCode::Char('l'), 20);
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::Char('l'), 5);
    assert_eq!(marked_text(&seam.render()), "opqrst");
    seam.resize(12, 14);
    let buf = seam.render();
    let shown = marked_text(&buf);
    let selected = cells(&buf, is_selected);
    chars(&mut seam, "y");
    assert!(
        selected.is_empty(),
        "no part of the selection is drawn, so nothing is highlighted:\n{}",
        frame_with_marks(&buf)
    );
    assert!(
        seam.copies().is_empty(),
        "the yank copies what the frame highlighted, which is nothing (the frame marked {shown:?})"
    );
    assert_eq!(seam.status(), None);
    let buf = seam.render();
    assert!(bar(&buf).starts_with(" COPY"), "{}", bar(&buf));
    let rect = seam.pane_rect();
    let cursor = cells(&buf, is_cursor);
    assert!(
        cursor.len() == 1 && cursor[0].0 > rect.x && cursor[0].0 < rect.x + rect.width - 1,
        "the cursor is on a drawn cell, got {cursor:?}:\n{}",
        frame_with_marks(&buf)
    );
    chars(&mut seam, "vhh");
    let shown = marked_text(&seam.render());
    chars(&mut seam, "y");
    assert_eq!(seam.copies(), [shown.as_str()]);
    assert_eq!(shown, "bcd");

    // A selection whose first line starts right of the new edge.
    let mut seam = CopyModeSeam::new(40, 14, &long_rows(12));
    seam.render();
    chars(&mut seam, "vkk");
    press(&mut seam, KeyCode::Char('h'), 40);
    press(&mut seam, KeyCode::Char('l'), 20);
    chars(&mut seam, "vj");
    press(&mut seam, KeyCode::Char('h'), 40);
    press(&mut seam, KeyCode::Char('l'), 3);
    seam.render();
    seam.resize(12, 14);
    let buf = seam.render();
    let shown = marked_text(&buf);
    chars(&mut seam, "y");
    assert_eq!(
        seam.copies().last().map(String::as_str),
        Some(shown.as_str()),
        "the yank copies what the frame highlights:\n{}",
        frame_with_marks(&buf)
    );
    assert_eq!(shown, "row1", "{}", frame_with_marks(&buf));

    // A height shrink that moves copy mode's view.
    let mut seam = CopyModeSeam::new(40, 14, &long_rows(30));
    seam.render();
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::PageUp, 1);
    press(&mut seam, KeyCode::Char('h'), 40);
    chars(&mut seam, "v");
    press(&mut seam, KeyCode::Char('l'), 5);
    let before = seam.render();
    let rect = seam.pane_rect();
    assert_eq!(marked_text(&before), "row19-");
    assert_eq!(
        cells(&before, is_selected).first().map(|&(_, y)| y),
        Some(rect.y + rect.height - 2),
        "the selection is on the view's last row:\n{}",
        frame_with_marks(&before)
    );
    seam.resize(40, 8);
    let buf = seam.render();
    let shown = marked_text(&buf);
    chars(&mut seam, "y");
    assert_eq!(
        seam.copies().last().map(String::as_str),
        Some(shown.as_str()),
        "the yank copies what the frame highlights:\n{}",
        frame_with_marks(&buf)
    );
    assert_eq!(shown, "row19-", "{}", frame_with_marks(&buf));
}
