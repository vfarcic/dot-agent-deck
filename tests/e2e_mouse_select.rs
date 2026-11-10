#![cfg(feature = "e2e")]

//! PRD #385 M2 — selection follows the pointer, not the focused pane.
//!
//! Two shell panes sit side by side on the dashboard, each having printed its
//! own uniquely named sentinel. A drag across the pane that is NOT focused must
//! focus that pane and copy its text in the same gesture. The shells stand in
//! for agents: what is selected is screen text, whichever program wrote it.
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

/// The border glyph directly left of `text` on screen: the left border of the
/// pane that printed `text` at its first column. Command mode draws the focused
/// pane's border thick (`┃`) and every other pane's plain (`│`).
fn left_border_of(grid: &str, text: &str) -> Option<char> {
    grid.lines()
        .find_map(|line| line.find(text).map(|at| line[..at].chars().last()))
        .flatten()
}

fn pane_is_focused(grid: &str, text: &str) -> bool {
    left_border_of(grid, text) == Some('┃')
}

fn pane_is_unfocused(grid: &str, text: &str) -> bool {
    left_border_of(grid, text) == Some('│')
}

/// Scenario: Two shell panes are tiled on the dashboard, each having printed its own sentinel, and the one opened second is focused in command mode. A single mouse drag across the other pane's sentinel focuses that pane and copies exactly its sentinel: the OSC 52 sequence carries that pane's text, never the focused pane's, and the status line reports the OSC 52-only send.
#[spec("mouse/select/001")]
#[test]
fn select_001_a_drag_in_a_non_focused_pane_focuses_it_and_copies_its_text() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("seltarget", "bash --noprofile --norc")
        .launch_with_fixture("form");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    // The sentinels are computed by the shell, so only the output line carries
    // them; the typed command line does not.
    let target_text = "SELTGT-42-dragme";
    let focused_text = "SELFOC-56-keepout";
    deck.send_bytes(b"echo SELTGT-$((6*7))-dragme\r");
    deck.wait_for_string(target_text);

    // Second pane from the new-pane form, with no command: a shell. It is
    // focused when it opens.
    deck.send_bytes(b"\x04"); // Ctrl+D → command mode
    deck.send_bytes(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" "); // Space → confirm the directory → the form
    deck.wait_for_string("┌ New Agent");
    let initial_name = deck.capture_screen_when("initial Name value", |screen| {
        let grid = screen.contents();
        grid.lines().find_map(|line| {
            line.split_once("Name:")
                .map(|(_, v)| v.split('│').next().unwrap_or_default().trim().to_string())
        })
    });
    deck.send_bytes(b"\t"); // Mode → Name
    deck.send_bytes(&vec![0x7f; initial_name.chars().count()]);
    deck.send_bytes(b"selfocused");
    deck.send_bytes(b"\t"); // Name → Command
    let initial_command = deck.capture_screen_when("initial Command value", |screen| {
        let grid = screen.contents();
        grid.lines().find_map(|line| {
            line.split_once("Command:")
                .map(|(_, v)| v.split('│').next().unwrap_or_default().trim().to_string())
        })
    });
    deck.send_bytes(&vec![0x7f; initial_command.chars().count()]);
    deck.send_bytes(b"\r"); // submit
    deck.wait_until_grid("the form closed and the focused pane opened", |g| {
        !g.contains("┌ New Agent") && g.contains("selfocused")
    });
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"echo SELFOC-$((7*8))-keepout\r");
    deck.wait_for_string(focused_text);

    // Command mode, tiled: both panes on screen, `selfocused` wearing the thick
    // border that marks focus in command mode.
    deck.send_bytes(b"\x04"); // Ctrl+D → command mode
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    deck.send_bytes(b"\x14"); // Ctrl+T → tiled
    deck.wait_until_grid("both panes tiled, `selfocused` focused", |g| {
        g.contains(target_text)
            && g.contains(focused_text)
            && pane_is_focused(g, focused_text)
            && pane_is_unfocused(g, target_text)
    });

    // One gesture: drag across the target pane's sentinel.
    let (col, row) = deck.wait_for_in_grid(target_text);
    let last = col + target_text.len() as u16 - 1;
    deck.drag((col, row), (last, row));

    deck.wait_for_stream_string(&osc52(target_text));
    let stream = deck.stream_text();
    assert!(
        !stream.contains(&osc52(focused_text)),
        "the drag copied the focused pane's text instead of the pane under the pointer"
    );
    deck.wait_until_grid(
        "the target pane took focus and the status reports the OSC 52 send",
        |g| {
            pane_is_focused(g, target_text)
                && pane_is_unfocused(g, focused_text)
                && g.contains("Sent to terminal clipboard (OSC 52)")
        },
    );
}
