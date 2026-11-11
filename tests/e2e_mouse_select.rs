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

dot_agent_deck_test_harness::link!();

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

/// The screen row of the first content row of the pane whose left border is
/// in column `border_col`, found from the row `below` by walking up to the
/// pane's top-left corner.
fn first_content_row(screen: &vt100::Screen, border_col: u16, below: u16) -> Option<u16> {
    (0..below).rev().find_map(|row| {
        let cell = screen.cell(row, border_col)?;
        matches!(cell.contents(), "┌" | "┏" | "╭" | "╔").then_some(row + 1)
    })
}

/// Scenario: One shell pane has printed a sentinel on its bottom row and the deck is in command mode. A drag that starts on the sentinel and is released over the bottom bar's New Agent button copies exactly the sentinel and opens no directory picker; a second drag released over the pane's dashboard card copies the pane text between the card's row and the sentinel and leaves the deck in command mode.
#[spec("mouse/select/005")]
#[test]
fn select_005_a_drag_released_over_chrome_still_copies_the_pane_text() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("selrelease", "bash --noprofile --norc")
        .launch_with_fixture("form");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    // Blank the pane, then leave the sentinel on its bottom row with the
    // shell held by `sleep`, so every row above the sentinel is empty.
    let sentinel = "SELREL-42-overchrome";
    deck.send_bytes(
        b"clear; for i in $(seq 80); do echo; done; printf 'SELREL-%d-overchrome' $((6*7)); sleep 600\r",
    );
    deck.wait_for_string(sentinel);
    deck.send_bytes(b"\x04"); // Ctrl+D → command mode
    deck.wait_for_string("[New Agent n/Ctrl+N]");

    let (col, row) = deck.wait_for_in_grid(sentinel);
    let last = col + sentinel.len() as u16 - 1;
    let (button_col, button_row) = deck.wait_for_in_grid("[New Agent n/Ctrl+N]");
    assert!(
        button_col + 1 < col && button_row > row,
        "the test needs the New Agent button below and left of the pane: button at ({button_col}, {button_row}), sentinel at ({col}, {row})"
    );

    // Released over the button: the drag's end clamps to the pane's bottom
    // row and first column, so the selection is exactly the sentinel.
    deck.drag((last, row), (button_col + 1, button_row));
    deck.wait_for_stream_string(&osc52(sentinel));
    deck.wait_for_string("Sent to terminal clipboard (OSC 52)");
    assert!(
        !deck.snapshot_grid().contains("Select Directory"),
        "releasing a pane drag over the New Agent button must not open the directory picker"
    );

    // Released over the pane's dashboard card, above and left of the pane.
    let (card_col, card_row) = deck.wait_for_in_grid("Launch an agent");
    let inner_y = deck.capture_screen_when("the pane's top border", |screen| {
        first_content_row(screen, col - 1, row)
    });
    assert!(
        card_col + 1 < col && card_row < row,
        "the test needs the card above and left of the pane: card at ({card_col}, {card_row}), sentinel at ({col}, {row})"
    );
    let blank_rows = row - card_row.max(inner_y);
    let expected = format!("{}{sentinel}", "\n".repeat(usize::from(blank_rows)));
    // A plain click on a blank pane cell first, so the next press is not
    // counted as a second click on the cell the first drag pressed (which
    // would select a word).
    deck.click(col + 10, row - 5);
    deck.drag((last, row), (card_col + 1, card_row));
    deck.wait_for_stream_string(&osc52(&expected));
    deck.wait_until_grid("the deck is still in command mode", |g| {
        g.contains("[New Agent n/Ctrl+N]") && !g.contains("Select Directory")
    });
}

/// The OSC 52 set-clipboard prefix, which every copy the deck writes starts with.
const OSC52_PREFIX: &str = "\x1b]52;c;";

/// How long a test waits for a copy that must not come.
const NO_COPY_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Scenario: One shell pane shows a sentinel and the deck is in command mode. The user presses on the sentinel and drags across it, and with the button still held presses `?`; once the Help overlay is drawn, the release lands over it. Nothing is copied (no OSC 52, no copy status), and after Help closes a click on the New Agent button opens the directory picker as usual. A drag held while the terminal loses focus copies nothing on its release either.
#[spec("mouse/select/011")]
#[test]
fn select_011_an_overlay_or_focus_loss_mid_drag_cancels_the_pane_selection() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("seloverlay", "bash --noprofile --norc")
        .launch_with_fixture("form");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    let sentinel = "SELOVL-42-heldover";
    deck.send_bytes(b"printf 'SELOVL-%d-heldover\\n' $((6*7))\r");
    deck.wait_for_string(sentinel);
    deck.send_bytes(b"\x04"); // Ctrl+D → command mode
    deck.wait_for_string("[New Agent n/Ctrl+N]");

    // Press on the sentinel and drag across it, the button still held.
    let (col, row) = deck.wait_for_in_grid(sentinel);
    let last = col + sentinel.len() as u16 - 1;
    deck.send_bytes(
        format!(
            "\x1b[<0;{};{}M\x1b[<32;{};{}M",
            col + 1,
            row + 1,
            last + 1,
            row + 1
        )
        .as_bytes(),
    );
    // `?` opens Help while the drag is held; the release lands over it.
    deck.send_bytes(b"?");
    deck.wait_for_string("Press ? or Esc to close");
    let (help_col, help_row) = deck.wait_for_in_grid("Panes auto-saved continuously.");
    deck.send_bytes(format!("\x1b[<0;{};{}m", help_col + 3, help_row + 1).as_bytes());
    assert!(
        !deck.wait_for_stream_string_within(OSC52_PREFIX, NO_COPY_WAIT),
        "a release over the Help overlay copied the pane selection the overlay interrupted"
    );
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("Sent to terminal clipboard") && !grid.contains("Copied to clipboard"),
        "no copy status may show for a cancelled selection:\n{grid}"
    );
    assert!(
        grid.contains("Press ? or Esc to close"),
        "the release over Help leaves Help open:\n{grid}"
    );

    // Help closes, and the next click on chrome works as usual.
    deck.send_bytes(b"\x1b"); // Esc → close Help
    deck.wait_until_grid("Help closed", |g| !g.contains("Press ? or Esc to close"));
    let (button_col, button_row) = deck.wait_for_in_grid("[New Agent n/Ctrl+N]");
    deck.click(button_col + 1, button_row);
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b"q"); // close the picker
    deck.wait_until_grid("the picker closed", |g| !g.contains("Select Directory"));

    // A drag held while the terminal loses focus copies nothing on release.
    let (col, row) = deck.wait_for_in_grid(sentinel);
    let last = col + sentinel.len() as u16 - 1;
    deck.send_bytes(
        format!(
            "\x1b[<0;{};{}M\x1b[<32;{};{}M\x1b[O\x1b[<0;{};{}m",
            col + 1,
            row + 1,
            last + 1,
            row + 1,
            last + 1,
            row + 1
        )
        .as_bytes(),
    );
    assert!(
        !deck.wait_for_stream_string_within(OSC52_PREFIX, NO_COPY_WAIT),
        "a release after the terminal lost focus copied the pane selection"
    );
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("Sent to terminal clipboard") && !grid.contains("Copied to clipboard"),
        "no copy status may show after a focus loss mid-drag:\n{grid}"
    );
}
