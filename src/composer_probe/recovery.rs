//! PRD #1666 part 3 (Q35-21, OQ2): the suspension recovery. A doorbell
//! attempt that ended unresolved suspends the generation's doorbell (plan
//! §3.4′): the deck's line may still be in the box, so nothing more is typed
//! until the user has looked at it. The user's own submit or clear is that
//! look, and lifts the suspension.
//!
//! [`Recovery`] counts them over Q35-01's tokenizer
//! ([`super::framing::Framing`], the client input grammar): a CR, `Ctrl+U` or
//! `Ctrl+C` read in ground and outside a bracketed paste
//! ([`super::framing::Token::edit`]), from the bytes the PTY physically
//! accepted, in the order it took them. It is fed every byte of the pane's
//! input stream, the deck's own included, because the agent reads one stream:
//! the deck's CR ends a user's half-sent sequence, and the deck's paste
//! markers open and close a paste. Only an edit whose byte was the user's
//! counts: the deck's own Enter is no look at the box.
//!
//! **What it grants: nothing the epoch doesn't.** Lifting a suspension lets
//! the doorbell try again; it says nothing about what the box holds. The
//! next attempt still needs the epoch's word that the box is empty: the
//! user's keys moved it, so a check must read it empty first
//! (`super::epoch::InputEpoch::touched`). This module moves no epoch number.

use super::framing::{Edit, Framing};

/// Who wrote a byte of the pane's input stream, as [`Recovery`] counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A person: their keys through an attached client, or a voice answer.
    User,
    /// The deck itself.
    Deck,
}

/// One pane's count of the user's own submits and clears. See the module
/// documentation.
#[derive(Debug, Default)]
pub struct Recovery {
    framing: Framing,
    clears: u64,
}

impl Recovery {
    /// Feed `bytes` the PTY took from `origin`; whether any of them was the
    /// user's own submit or clear.
    pub fn feed(&mut self, bytes: &[u8], origin: Origin) -> bool {
        let before = self.clears;
        for &byte in bytes {
            let mut edit = None;
            self.framing.feed(byte, |token| {
                if let Some(e) = token.edit() {
                    edit = Some(e);
                }
            });
            if origin == Origin::User && matches!(edit, Some(Edit::Submit | Edit::Clear)) {
                self.clears += 1;
            }
        }
        self.clears != before
    }

    /// How many submits and clears of the user's own it has counted.
    pub fn clears(&self) -> u64 {
        self.clears
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    fn fed(steps: &[(&[u8], Origin)]) -> Vec<bool> {
        let mut recovery = Recovery::default();
        steps
            .iter()
            .map(|(bytes, origin)| recovery.feed(bytes, *origin))
            .collect()
    }

    /// Scenario: Feed one pane's input stream the user's Enter, Ctrl+U and Ctrl+C, each after some typing, and the same three keys from the deck. Each of the user's counts as their own submit or clear, and none of the deck's does.
    #[spec("inbox/probe/047")]
    #[test]
    fn probe_047_the_users_submit_and_clear_count_and_the_decks_do_not() {
        use Origin::{Deck, User};
        assert_eq!(
            fed(&[
                (b"draft", User),
                (b"\r", User),
                (b"more", User),
                (b"\x15", User),
                (b"x", User),
                (b"\x03", User),
            ]),
            [false, true, false, true, false, true]
        );
        assert_eq!(
            fed(&[
                (b"line", Deck),
                (b"\r", Deck),
                (b"\x15", Deck),
                (b"\x03", Deck)
            ]),
            [false, false, false, false]
        );
    }

    /// Scenario: Feed the user's newline keys (Ctrl+J, Alt+Enter), a CR inside a bracketed paste, and a CR that ends an escape sequence the deck's bytes are part of. None is a submit or clear of the user's: newlines keep the draft, and a pasted CR is text.
    #[spec("inbox/probe/047")]
    #[test]
    fn probe_047_newlines_and_pasted_returns_are_no_submit() {
        use Origin::{Deck, User};
        let mut recovery = Recovery::default();
        assert!(!recovery.feed(b"a\n", User), "Ctrl+J is a newline");
        assert!(!recovery.feed(b"\x1b\r", User), "Alt+Enter is a newline");
        assert!(!recovery.feed(b"\x1b[200~one\rtwo\x15", User), "pasted");
        assert!(!recovery.feed(b"\x1b[201~", User), "the paste closes");
        assert_eq!(recovery.clears(), 0);
        assert!(recovery.feed(b"\r", User), "after the paste, Enter submits");
        // The deck's paste markers frame the user's bytes as well: one
        // stream.
        assert!(!recovery.feed(b"\x1b[200~", Deck));
        assert!(!recovery.feed(b"\r", User), "inside the deck's paste");
        assert!(!recovery.feed(b"\x1b[201~", Deck));
        assert_eq!(recovery.clears(), 1);
    }

    /// Scenario: The user sends half a key sequence (`ESC [`), then the deck's notice line and its Enter go into the same stream, then the user presses Enter. The deck's first letter completes the user's sequence, so its Enter is the deck's and counts nothing; the user's Enter after it is the user's own submit (the mixed-origin case issue #544's split test covered for the deleted draft bit).
    #[spec("inbox/probe/047")]
    #[test]
    fn probe_047_a_users_half_sequence_completed_by_the_decks_line_counts_only_the_users_enter() {
        use Origin::{Deck, User};
        let mut recovery = Recovery::default();
        assert!(!recovery.feed(b"\x1b[", User));
        assert!(
            !recovery.feed(b"NOTICE\r", Deck),
            "the deck's Enter is not the user's"
        );
        assert_eq!(recovery.clears(), 0);
        assert!(
            recovery.feed(b"\r", User),
            "the user's Enter after it submits"
        );
        assert_eq!(recovery.clears(), 1);
    }

    /// Scenario: Split the user's Ctrl+U across a CSI the user began and abandoned, and feed a Ctrl+C that arrives inside an OSC string. A control byte inside a sequence or string is no edit; once the stream is back in ground the user's clear counts.
    #[spec("inbox/probe/047")]
    #[test]
    fn probe_047_only_an_edit_in_ground_counts() {
        use Origin::User;
        let mut recovery = Recovery::default();
        assert!(!recovery.feed(b"\x1b[1", User));
        assert!(!recovery.feed(b"\x15", User), "inside a CSI");
        assert!(!recovery.feed(b"\x18", User), "CAN abandons it");
        assert!(recovery.feed(b"\x15", User), "in ground again");
        assert!(!recovery.feed(b"\x1b]11;", User));
        assert!(!recovery.feed(b"\x03", User), "inside an OSC string");
        assert_eq!(recovery.clears(), 1);
    }
}
