#![cfg(feature = "e2e")]

//! Issue #1705 — an agent's own clipboard copy reaches the user's terminal
//! through the TUI.
//!
//! Agents copy text by writing an OSC 52 set-clipboard request
//! (`ESC ] 52 ; c ; <base64> BEL`) to their terminal. In a TUI pane that
//! terminal is the deck's pane parser, not the user's terminal, so the request
//! has to be passed on for the copy to land on the user's clipboard. The
//! desktop app honours the same request in its own terminal; this is the TUI's
//! half (CLAUDE.md rule 22).
//!
//! The pane runs `bash`, standing in for an agent: the request is the same
//! bytes whichever program writes them, and a shell makes the copied text
//! something the test can compute without a credential. The harness's PTY IS
//! the deck's controlling terminal, so what the deck writes for the user's
//! terminal lands in its byte history, where the request is read back.

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

/// Scenario: open a pane running a shell and, in it, run a command that copies
/// a line to the clipboard with OSC 52, the way an agent copies. The deck
/// passes that copy on to the user's terminal exactly once, so the line lands
/// on the user's clipboard, and does not pass on a request to READ the
/// clipboard.
#[spec("embed/clipboard/001")]
#[test]
fn clipboard_001_an_agent_copy_in_a_pane_reaches_the_users_terminal() {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 40)
        .with_continue_session("copier", "bash --noprofile --norc")
        .launch_with_fixture("minimal");
    // --continue focuses the restored pane, so keystrokes reach bash.
    deck.wait_for_string("[Command Mode Ctrl+D]");

    // The copied text is computed by the shell, so neither it nor its base64
    // appears in what is typed: it can only reach the byte history as a copy.
    let copied = "dad-osc52-42-ok";
    let request = format!("\x1b]52;c;{}\x1b\\", base64(copied.as_bytes()));
    deck.send_bytes(
        b"printf '\\e]52;c;%s\\a' \"$(printf 'dad-osc52-%s-ok' $((6*7)) | base64 | tr -d '\\r\\n')\"; \
          printf '\\e]52;c;?\\a'; echo copied-$((1+1))\r",
    );
    deck.wait_for_string("copied-2");
    deck.wait_for_stream_string(&request);

    let stream = deck.stream_text();
    assert_eq!(
        stream.matches(&request).count(),
        1,
        "the agent's copy should reach the user's terminal exactly once"
    );
    // The read request is not passed on: the user's terminal, asked, could
    // answer it with the clipboard's contents, and the answer would reach bash.
    assert_eq!(
        stream.matches("\x1b]52;").count(),
        1,
        "the deck wrote an OSC 52 other than the agent's copy to the user's terminal"
    );
}
