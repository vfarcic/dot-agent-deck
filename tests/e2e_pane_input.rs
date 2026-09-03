#![cfg(feature = "e2e")]

//! PTY-attached coverage for Ctrl+W inside real interactive panes.
//!
//! Deterministic (lane 1) half — no agent credential needed. The credentialed
//! `prompt/pane-input/022` lives in the sibling `e2e_pane_input_live.rs`
//! (issue #502); the two shared no helpers and no constants, so splitting them
//! duplicated nothing.

mod common;

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
