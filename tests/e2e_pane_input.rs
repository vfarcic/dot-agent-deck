#![cfg(feature = "e2e")]

//! PTY-attached coverage for what a keystroke typed into a focused pane
//! reaches the pane's program as: Ctrl+W inside a real interactive shell, and
//! the editing shortcuts the desktop app's agent terminal also translates
//! (issue #1422).
//!
//! Deterministic (lane 1) half — no agent credential needed. The credentialed
//! `prompt/pane-input/022` lives in the sibling `e2e_pane_input_live.rs`
//! (issue #502); the two shared no helpers and no constants, so splitting them
//! duplicated nothing.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch a real interactive Bash/readline pane, type `echo alpha doomed`, press Ctrl+W, replace the deleted word with `survives`, and submit. The pane must visibly print `alpha survives` and remain attached in PaneInput, proving both native word deletion and non-destruction.
#[spec("prompt/pane-input/021")]
#[test]
fn pane_input_021_ctrl_w_deletes_shell_word_without_closing_pane() {
    let deck = TuiDeck::builder()
        .with_continue_session(
            "word-delete-shell",
            "env PS1='SAFE-CLOSE> ' bash --noprofile --norc -i",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.wait_for_string("SAFE-CLOSE>");

    deck.send_keys(b"echo alpha doomed");
    deck.send_keys(b"\x17");
    deck.send_keys(b"survives\r");

    deck.wait_for_string("alpha survives");
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("[Command Mode Ctrl+D]"),
        "Ctrl+W must leave the shell pane alive and attached\nFinal grid:\n{grid}"
    );
    assert!(
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .any(|record| record.display_name.as_deref() == Some("word-delete-shell")),
        "the surviving pane must still have its daemon-side agent record"
    );
}

/// One keypress: what the outer terminal writes to the deck for it, and the
/// bytes the pane's program must receive.
struct KeyCase {
    name: &'static str,
    typed: &'static [u8],
    received: &'static [u8],
}

/// Issue #1422's TUI half. The deck decodes what the outer terminal sends
/// (crossterm) and re-encodes it for the pane (`keyevent_to_bytes`), so the
/// bytes an agent receives are the deck's, not the terminal's. Each `received`
/// below is what the desktop app sends for the same shortcut
/// (`desktop/src/lib/terminalKeys.ts`), which every supported agent was
/// measured to act on (`docs/develop/desktop-gui.md`, "Keys in an agent's
/// terminal"). The `typed` side is the encoding a terminal emits: CSI u where
/// the chord only exists under the kitty keyboard protocol (Ctrl+Backspace,
/// Super chords), the xterm modifier form everywhere else.
const EDITING_KEYS: &[KeyCase] = &[
    // Translated: the deck used to drop the modifier on these.
    KeyCase {
        name: "Ctrl+Backspace (kitty protocol) deletes the previous word",
        typed: b"\x1b[127;5u",
        received: b"\x17",
    },
    KeyCase {
        name: "Ctrl+Delete deletes the next word",
        typed: b"\x1b[3;5~",
        received: b"\x1bd",
    },
    KeyCase {
        name: "Alt/Option+Delete deletes the next word",
        typed: b"\x1b[3;3~",
        received: b"\x1bd",
    },
    KeyCase {
        name: "Alt/Option+Left moves a word left",
        typed: b"\x1b[1;3D",
        received: b"\x1b[1;3D",
    },
    KeyCase {
        name: "Alt/Option+Right moves a word right",
        typed: b"\x1b[1;3C",
        received: b"\x1b[1;3C",
    },
    KeyCase {
        name: "Super/Cmd+Left (kitty protocol) goes to the start of the line",
        typed: b"\x1b[1;9D",
        received: b"\x01",
    },
    KeyCase {
        name: "Super/Cmd+Right (kitty protocol) goes to the end of the line",
        typed: b"\x1b[1;9C",
        received: b"\x05",
    },
    KeyCase {
        name: "Super/Cmd+Backspace (kitty protocol) deletes to the start of the line",
        typed: b"\x1b[127;9u",
        received: b"\x15",
    },
    // Controls: already the desktop's bytes, and must stay so.
    KeyCase {
        name: "Home",
        typed: b"\x1b[H",
        received: b"\x1b[H",
    },
    KeyCase {
        name: "End",
        typed: b"\x1b[F",
        received: b"\x1b[F",
    },
    KeyCase {
        name: "Ctrl+Left",
        typed: b"\x1b[1;5D",
        received: b"\x1b[1;5D",
    },
    KeyCase {
        name: "Ctrl+Right",
        typed: b"\x1b[1;5C",
        received: b"\x1b[1;5C",
    },
    KeyCase {
        name: "Alt/Option+Backspace",
        typed: b"\x1b\x7f",
        received: b"\x1b\x7f",
    },
    KeyCase {
        name: "Shift+Enter (kitty protocol)",
        typed: b"\x1b[13;2u",
        received: b"\x1b[13;2u",
    },
    KeyCase {
        name: "Ctrl+Enter (kitty protocol)",
        typed: b"\x1b[13;5u",
        received: b"\x1b[13;5u",
    },
    KeyCase {
        name: "Ctrl+/",
        typed: b"\x1f",
        received: b"\x1f",
    },
    // What a macOS terminal sends for Option+Left/Right by default, and what
    // a terminal configured for macOS text editing (iTerm2's Natural Text
    // Editing preset, for one) sends for Cmd+Left, Cmd+Right, Cmd+Backspace and
    // Option+Delete. They must reach the agent unchanged — in particular the
    // deck's own Ctrl chords (Ctrl+E, Ctrl+W) must not claim them while the
    // user is typing in a pane.
    KeyCase {
        name: "ESC b (a macOS terminal's Option+Left)",
        typed: b"\x1bb",
        received: b"\x1bb",
    },
    KeyCase {
        name: "ESC f (a macOS terminal's Option+Right)",
        typed: b"\x1bf",
        received: b"\x1bf",
    },
    KeyCase {
        name: "Ctrl+A (Cmd+Left in a terminal set up for it)",
        typed: b"\x01",
        received: b"\x01",
    },
    KeyCase {
        name: "Ctrl+E (Cmd+Right in a terminal set up for it)",
        typed: b"\x05",
        received: b"\x05",
    },
    KeyCase {
        name: "Ctrl+U (Cmd+Backspace in a terminal set up for it)",
        typed: b"\x15",
        received: b"\x15",
    },
    KeyCase {
        name: "Ctrl+W",
        typed: b"\x17",
        received: b"\x17",
    },
    KeyCase {
        name: "ESC d (Option+Delete in a terminal set up for it)",
        typed: b"\x1bd",
        received: b"\x1bd",
    },
    // A terminal without the kitty protocol sends Ctrl+Backspace as BS, the
    // same byte as Ctrl+H, so the deck cannot tell it is a word delete. It is
    // forwarded as it came; the user docs say how to set the terminal up.
    KeyCase {
        name: "BS (Ctrl+Backspace without the kitty protocol), unchanged",
        typed: b"\x08",
        received: b"\x08",
    },
    // The terminal pastes; the deck hands the text to the pane.
    KeyCase {
        name: "a bracketed paste",
        typed: b"\x1b[200~pasted text\x1b[201~",
        received: b"pasted text",
    },
];

const KEY_LOG: &str = "keys.log";

fn render_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| match b {
            0x1b => "ESC".to_string(),
            0x20..=0x7e => (*b as char).to_string(),
            other => format!("<{other:02x}>"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Scenario: Start the deck with one pane whose program records every byte it receives, then type each platform editing shortcut the way a terminal sends it — Ctrl+Backspace, Ctrl+Delete, Option+Delete, Option+Left/Right, Cmd+Left/Right/Backspace — plus Home, End, Ctrl+Left/Right, the modified Enters and a paste. Each must reach the pane as the bytes the desktop app sends for that shortcut, which every supported agent acts on, and none of them may be taken by the deck's own shortcuts.
#[spec("embed/key-forwarding/003")]
#[test]
fn key_forwarding_003_editing_shortcuts_reach_the_pane_as_the_desktop_sends_them() {
    let deck = TuiDeck::builder()
        .with_continue_session(
            "key-recorder",
            format!("sh -c 'stty raw -echo; printf KEYREC-READY; exec cat -u > {KEY_LOG}'"),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.wait_for_string("KEYREC-READY");

    // One keypress per write, each followed by a separator the recorder
    // receives as itself, so the log splits back into one entry per key.
    for case in EDITING_KEYS {
        deck.send_keys(case.typed);
        deck.send_keys(b"|");
    }
    deck.send_keys(b"END");

    let log_path = deck.workdir().join(KEY_LOG);
    let finished = common::wait_until(Duration::from_secs(10), || {
        std::fs::read(&log_path).is_ok_and(|log| log.ends_with(b"END"))
    });
    let log = std::fs::read(&log_path).unwrap_or_default();
    assert!(
        finished,
        "the recorder never received the closing marker\nlog: {}\nFinal grid:\n{}",
        render_bytes(&log),
        deck.snapshot_grid()
    );

    let received: Vec<&[u8]> = log.split(|b| *b == b'|').collect();
    assert_eq!(
        received.len(),
        EDITING_KEYS.len() + 1,
        "one entry per key plus the closing marker\nlog: {}",
        render_bytes(&log)
    );
    let wrong: Vec<String> = EDITING_KEYS
        .iter()
        .zip(&received)
        .filter(|(case, got)| case.received != **got)
        .map(|(case, got)| {
            format!(
                "{}: expected {}, received {}",
                case.name,
                render_bytes(case.received),
                render_bytes(got)
            )
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {} keys reached the pane as the wrong bytes:\n{}",
        wrong.len(),
        EDITING_KEYS.len(),
        wrong.join("\n")
    );
    assert!(
        deck.snapshot_grid().contains("[Command Mode Ctrl+D]"),
        "no key above may take the deck out of the pane\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}
