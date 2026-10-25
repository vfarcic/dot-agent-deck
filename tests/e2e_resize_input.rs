#![cfg(feature = "e2e")]

//! L2 test: keys typed right after a terminal resize reach the deck without
//! any further input.
//!
//! crossterm 0.29.0's mio event source returned the `Resize` from a readiness
//! batch as soon as it met the SIGWINCH token, leaving the terminal-input token
//! of the same batch unread. mio's readiness is edge-triggered, so those bytes
//! raised no new event and the keys sat in the terminal until the user typed
//! something else (`vendor/crossterm/VENDORED.md` has the fix). The window is
//! narrow: the SIGWINCH and the keys have to be waiting together when the event
//! loop next polls, which is what a resize landing while the deck is still
//! drawing the previous one produces. So each keystroke here follows two
//! resizes, the second sent once the reader has recorded a screen clear after
//! the first, normally the one that starts the deck's redraw for it. The redraw
//! can finish before the second resize lands, and a clear from the previous
//! pair that the reader records late can release the second resize early, so
//! the overlap is likely rather than certain.
//!
//! What it watches is filter-mode text, which only grows, so each key has a
//! check of its own that no earlier key can satisfy.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Each block is typed into a fresh filter, one key per resize pair. No two
/// blocks share a character, so text left on the grid by an earlier block
/// cannot satisfy a later block's check.
const BLOCKS: [&str; 4] = [
    "abcdefghijklmno",
    "pqrstuvwxyzABCD",
    "EFGHIJKLMNOPQRS",
    "TUVWXYZ01234567",
];

/// How long a key may take to show on the grid before it counts as stranded.
/// A stranded key never shows on its own, so this only bounds a failing run.
const KEY_DEADLINE: Duration = Duration::from_secs(10);

/// Scenario: Launch the deck on an empty dashboard and open the filter with
/// `/`. Then, sixty times, resize the terminal, resize it again once the deck
/// has sent a screen clear after that resize, and type one character straight
/// after the second resize; every character must appear in the filter prompt
/// without the test sending any further byte. The filter is closed with Esc and
/// reopened every fifteen characters.
#[spec("resize/sigwinch/004")]
#[test]
fn sigwinch_004_keys_right_after_a_resize_arrive_without_more_input() {
    let mut deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    let mut attempt: u64 = 0;
    for block in BLOCKS {
        deck.send_keys(b"/");
        deck.wait_for_string("/ ");
        let mut typed = String::from("/ ");
        for key in block.chars() {
            let (cols, rows) = if attempt.is_multiple_of(2) {
                (100, 30)
            } else {
                (110, 32)
            };
            deck.resize_during_redraw((cols - 4, rows - 2), (cols, rows));
            deck.send_keys(key.to_string().as_bytes());
            typed.push(key);
            if !deck.wait_for_grid_string_within(&typed, KEY_DEADLINE) {
                let before = deck.snapshot_grid();
                // One harmless byte (Ctrl+Space) gives the terminal new input.
                // If the key then shows, it was stranded rather than lost.
                deck.send_keys(b"\x00");
                let released = deck.wait_for_grid_string_within(&typed, Duration::from_secs(2));
                panic!(
                    "attempt {attempt}: {key:?} typed right after a resize to {cols}x{rows} did \
                     not show as {typed:?} within {KEY_DEADLINE:?}; after one more byte it \
                     showed: {released}.\nGrid before:\n{before}"
                );
            }
            attempt += 1;
        }
        deck.send_keys(b"\x1b"); // Esc clears and closes the filter
        deck.wait_for_string("[New Agent");
    }
}
