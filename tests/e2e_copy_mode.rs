#![cfg(feature = "e2e")]

//! PRD #385 M5 — keyboard copy mode, end to end through the real binary.
//!
//! A shell pane prints a uniquely named sentinel and then enough lines to push
//! it into the pane's history. In command mode, `v` opens copy mode on the
//! focused pane; the test pages back to the sentinel, moves the copy-mode
//! cursor onto it, selects it and yanks. The shell stands in for an agent:
//! what is copied is screen text, whichever program wrote it.
//!
//! The harness runs the deck with the native clipboard switched off
//! (`DOT_AGENT_DECK_NATIVE_CLIPBOARD=0`) and its PTY is the deck's controlling
//! terminal, so the copy is observed as the OSC 52 sequence in the byte
//! history, and the status line says that only OSC 52 went out.

mod common;

use common::TuiDeck;
use spec::spec;

fn base64(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(CHARS[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 set-clipboard sequence the deck writes for a copy of `text`.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", base64(text.as_bytes()))
}

const PAGE_UP: &[u8] = b"\x1b[5~";

/// Where the copy-mode cursor is on screen: the one cell drawn on a yellow
/// background (indexed colour 3).
fn copy_cursor(screen: &vt100::Screen) -> Option<(u16, u16)> {
    let (rows, cols) = screen.size();
    (0..rows.get()).find_map(|row| {
        (0..cols.get()).find_map(|col| {
            screen
                .cell(row, col)
                .filter(|cell| cell.bgcolor() == vt100::Color::Idx(3))
                .map(|_| (col, row))
        })
    })
}

fn chip(grid: &str, label: &str) -> bool {
    grid.lines().any(|line| line.starts_with(label))
}

/// Move the copy-mode cursor to `target` with `h`/`j`/`k`/`l`, and wait until
/// the screen shows it there.
fn move_cursor_to(deck: &TuiDeck, target: (u16, u16)) {
    let (col, row) = deck.capture_screen_when("the copy-mode cursor", copy_cursor);
    let mut keys = Vec::new();
    let vertical = if target.1 >= row { b'j' } else { b'k' };
    keys.extend(std::iter::repeat_n(
        vertical,
        usize::from(target.1.abs_diff(row)),
    ));
    let horizontal = if target.0 >= col { b'l' } else { b'h' };
    keys.extend(std::iter::repeat_n(
        horizontal,
        usize::from(target.0.abs_diff(col)),
    ));
    deck.send_bytes(&keys);
    deck.capture_screen_when("the copy-mode cursor on its target", |screen| {
        (copy_cursor(screen) == Some(target)).then_some(())
    });
}

/// Scenario: A shell pane prints a sentinel and then 120 more lines, pushing the sentinel into the pane's history. In command mode the user presses `v`, pages up until the sentinel is on screen, moves the copy-mode cursor onto it, presses `v` to start the selection, moves across it and presses `y`. The OSC 52 sequence carries exactly the sentinel, the status line reports the OSC 52 send, and copy mode has closed.
#[spec("copymode/keys/001")]
#[test]
fn copymode_keys_001_v_pages_back_selects_and_yanks_a_sentinel() {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 40)
        .with_continue_session("copytarget", "bash --noprofile --norc")
        .launch_with_fixture("minimal");
    // --continue focuses the restored pane, so keystrokes reach bash.
    deck.wait_for_string("[Command Mode Ctrl+D]");
    // Computed by the shell, so only the output carries the sentinel, never
    // the typed command line.
    let sentinel = "COPYSNT-42-yankme";
    deck.send_bytes(b"echo COPYSNT-$((6*7))-yankme; seq 1 120; echo SEQDONE-$((2+3))\r");
    deck.wait_for_string("SEQDONE-5");
    deck.wait_until_grid("the sentinel scrolled out of view", |g| {
        !g.contains(sentinel)
    });

    deck.send_bytes(b"\x04"); // Ctrl+D → command mode
    deck.wait_until_grid("command mode", |g| chip(g, " COMMAND "));
    deck.send_bytes(b"v");
    deck.wait_until_grid("copy mode", |g| chip(g, " COPY "));
    deck.capture_screen_when("the copy-mode cursor", copy_cursor);

    for _ in 0..10 {
        let before = deck.snapshot_grid();
        if before.contains(sentinel) {
            break;
        }
        deck.send_bytes(PAGE_UP);
        deck.wait_until_grid("the view paged back", |g| g != before);
    }
    let (col, row) = deck.wait_for_in_grid(sentinel);
    move_cursor_to(&deck, (col, row));
    deck.send_bytes(b"v");
    let last = col + sentinel.len() as u16 - 1;
    move_cursor_to(&deck, (last, row));
    deck.send_bytes(b"y");

    deck.wait_for_stream_string(&osc52(sentinel));
    assert_eq!(
        deck.stream_text().matches("\x1b]52;").count(),
        1,
        "exactly one copy went out"
    );
    deck.wait_until_grid(
        "the status reports the OSC 52 send and copy mode has closed",
        |g| g.contains("Sent to terminal clipboard (OSC 52)") && chip(g, " COMMAND "),
    );
}

/// Scenario: A shell pane runs `cat -v`, which shows every byte it receives, with mouse reports and bracketed paste turned on for it. In copy mode the user types printable keys, digits, `Ctrl+D`, `Ctrl+W`, movement keys and `PageUp`, pastes text and clicks, drags and scrolls the mouse over the pane, then leaves copy mode with `q` and types `ENDMARK` into the pane. The program shows `ENDMARK` alone on its line: nothing sent while copy mode was open reached it, and nothing was copied.
#[spec("copymode/keys/009")]
#[test]
fn copymode_keys_009_keys_paste_and_mouse_in_copy_mode_reach_no_program() {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 40)
        .with_continue_session("copytarget", "bash --noprofile --norc")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(
        b"stty -icanon -echo -isig; printf '\\033[?1000h\\033[?1006h\\033[?2004h'; \
          echo CATREADY-$((4+5)); cat -v\r",
    );
    deck.wait_for_string("CATREADY-9");

    deck.send_bytes(b"\x04"); // Ctrl+D → command mode
    deck.wait_until_grid("command mode", |g| chip(g, " COMMAND "));
    deck.send_bytes(b"v");
    deck.wait_until_grid("copy mode", |g| chip(g, " COPY "));

    for bytes in [
        &b"zZ!1x"[..],
        b"\x04",
        b"\x17",
        b"hjkl",
        b"\x1b[A",
        PAGE_UP,
        b"\x1b[200~PASTED-IN-COPY\x1b[201~",
        b"\x1b[<0;40;10M",
        b"\x1b[<32;45;10M",
        b"\x1b[<0;45;10m",
        b"\x1b[<64;40;10M",
    ] {
        deck.send_bytes(bytes);
    }
    deck.send_bytes(b"q");
    deck.wait_until_grid("copy mode closed", |g| chip(g, " COMMAND "));
    deck.send_bytes(b"\x04"); // Ctrl+D → back into the pane
    deck.wait_until_grid("typing into the pane", |g| chip(g, " TYPING "));
    deck.send_bytes(b"ENDMARK");
    deck.wait_until_grid("the program shows ENDMARK", |g| g.contains("ENDMARK"));
    let grid = deck.snapshot_grid();
    // The pane's text on that row, between the pane's borders.
    let line = grid
        .lines()
        .flat_map(|line| line.split('│'))
        .map(str::trim)
        .find(|text| text.contains("ENDMARK"))
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        line, "ENDMARK",
        "the program received input sent while copy mode was open:\n{grid}"
    );
    assert!(
        !grid.contains("PASTED-IN-COPY") && !grid.contains("^["),
        "a paste or a mouse report reached the program:\n{grid}"
    );
    assert!(
        !deck.stream_text().contains("\x1b]52;"),
        "nothing was copied"
    );
}
