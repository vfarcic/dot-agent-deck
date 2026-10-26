//! PRD #1666 part 3, round 5-wake (R6-4): what an agent's input box holds, as
//! far as the bytes written into it say.
//!
//! The doorbell types only into an empty composer, and the screen cannot
//! always prove one. Claude Code 2.1.310 draws nothing for a space typed into
//! its input box; it only moves the cursor (measured on a real session,
//! 2026-10-26). So spaces with the cursor moved back to the start, or a
//! spaces-only line with the cursor on another one, draw exactly what the
//! empty composer draws. And the placeholder, the one drawing an empty box
//! makes, is not drawn after a delivered task, so it cannot be the proof
//! either.
//!
//! [`ComposerModel`] follows the box from the bytes the pane's PTY took, in
//! the order it took them, the deck's own included: the same stream the draft
//! bit is fed (`crate::agent_pty`'s `PaneInputStream`). It knows the text and
//! the cursor while every key since the last proof of empty is one whose
//! effect was measured on Claude Code 2.1.310, and nothing otherwise
//! ([`InputProof::Unknown`]): a key it does not follow is never guessed at.
//! The doorbell asks it alongside the screen (`crate::doorbell_txn::Frame`):
//! a blank composer is empty only when the bytes say so too.
//!
//! **Followed**, as measured: printable ASCII typed or pasted (inserted at the
//! cursor; a space too); `Ctrl+J` and a newline inside a paste (a line break);
//! `Backspace` (deletes before the cursor, joining lines at a line's start);
//! `Delete`, `ESC[3~` (deletes at the cursor); `Left` and `Right`, `ESC[D`,
//! `ESC[C` and their `ESC O` forms (one character, across line breaks);
//! `Ctrl+A`, `ESC[H`, `ESC[1~`, `ESC O H` (start of line); `Ctrl+E`,
//! `ESC[F`, `ESC[4~`, `ESC O F` (end of line); `Ctrl+U` (deletes from the
//! start of the line to the cursor, and nothing at a line's start);
//! `Ctrl+C` (empties the whole box); `Enter` (submits text that has
//! something other than blanks in it, which empties the box, and leaves a
//! box of blanks as it was). Terminal reports (mouse, focus, cursor
//! position and the like) and the paste markers edit nothing.
//!
//! **Not followed**, so the box becomes [`InputProof::Unknown`]: `Up` and
//! `Down` (history, or a move between lines whose column the model does not
//! track), `Tab` (completion), non-ASCII text, any other control key, an
//! `Alt` or modified key, `Shift+Enter`, `Alt+Enter`, an `Enter` after a
//! backslash, and anything else inside a paste. From there three things make
//! it known again: `Ctrl+C`; `End` then `Ctrl+U` with nothing between them,
//! which empties the current line (whether other lines are left is what the
//! screen shows, since the doorbell admits only a composer drawn on a single
//! row); and an `Enter` followed by the agent starting a turn, which says the
//! `Enter` submitted the box and emptied it (an `Enter` on a box of blanks
//! submits nothing, so the turn is the proof). What is typed after that
//! `Enter` is followed as typed into an empty box, and counts once the turn
//! starts.
//!
//! **What it cannot see**, and accepts: text the agent puts into its own box
//! (a restored prompt) is invisible to it; a key that reaches a dialog or
//! menu rather than the box is read as an edit of the box; and a turn the
//! agent starts for another reason confirms an unconfirmed `Enter` all the
//! same. The first can make the box look empty when it holds text the agent
//! added, which the screen then shows unless it is blanks. The second
//! mostly makes the box look fuller than it is, but an `Enter` on text, or a
//! `Ctrl+C`, that a dialog took instead reads as emptying the box. The third
//! needs an `Enter` on a box of blanks the model did not know. In each, a
//! box of blanks can read as empty. The doorbell's other checks (a Clean
//! ready frame, no dialog) still apply on top.

/// What the bytes say about the box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputProof {
    /// The box is empty: the current line holds nothing, and no other line
    /// can be left unless the screen shows one.
    #[default]
    Empty,
    /// The box holds something: text, blanks or a line break.
    Draft,
    /// A key the model does not follow reached the box since it was last
    /// known: only the screen's own proof of empty (the agent's
    /// placeholder) can say it is empty.
    Unknown,
}

/// The most characters followed: past it the box is [`InputProof::Unknown`]
/// rather than a growing copy of someone's prompt.
const MAX_CHARS: usize = 4096;

/// A CSI sequence longer than this is not a key the model follows.
const MAX_CSI_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    /// The box's text, with `\n` for a line break, and where the cursor is
    /// in it (`None`: somewhere the model lost track of). `others`: lines
    /// outside `text` may exist, after `End` then `Ctrl+U` emptied one line
    /// of a box whose other lines were not known.
    Known {
        text: Vec<char>,
        cursor: Option<usize>,
        others: bool,
    },
    /// Not known; `multiline` when it may hold more than one line.
    Unknown { multiline: bool },
}

impl Default for Content {
    fn default() -> Self {
        Self::Known {
            text: Vec::new(),
            cursor: Some(0),
            others: false,
        }
    }
}

/// One key, as far as the box is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Insert(char),
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    KillBefore,
    ClearAll,
    Submit,
    /// A key the model does not follow; `newline` when it may have broken a
    /// line.
    Unfollowed {
        newline: bool,
    },
}

/// Where the decoder is inside an escape sequence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Decode {
    #[default]
    Ground,
    /// An `ESC`; `paste` when it arrived inside a bracketed paste.
    Esc { paste: bool },
    /// `ESC [`: the parameter and intermediate bytes so far.
    Csi { params: Vec<u8>, paste: bool },
    /// `ESC O`.
    Ss3,
    /// `ESC ]` or `ESC P`, a reply ended by `BEL` (OSC) or `ESC \`;
    /// `esc` once an `ESC` arrived inside it.
    Text { osc: bool, esc: bool },
    /// The three bytes after a legacy X10 mouse report's `ESC [ M`.
    X10 { remaining: u8 },
    /// The continuation bytes of a UTF-8 character still to come.
    Utf8 { remaining: u8, paste: bool },
}

/// One pane's input box, followed from its PTY input. See the module
/// documentation.
#[derive(Debug, Default)]
pub(crate) struct ComposerModel {
    content: Content,
    /// The last key was `End`, so a `Ctrl+U` now empties the current line
    /// wherever the cursor was.
    at_line_end: bool,
    /// An `Enter` reached a box the model did not know, and the agent has
    /// not started a turn since: if that `Enter` submitted, the box was
    /// emptied and `content` is what was typed after it; if it did not (a
    /// box of blanks keeps them), what the box held is still there, on one
    /// line or (`Some(true)`) maybe more.
    unconfirmed: Option<bool>,
    decode: Decode,
}

impl ComposerModel {
    /// What the bytes so far say about the box.
    pub(crate) fn proof(&self) -> InputProof {
        match &self.content {
            Content::Known { text, .. } if !text.is_empty() => InputProof::Draft,
            Content::Known { .. } if self.unconfirmed.is_none() => InputProof::Empty,
            Content::Known { .. } | Content::Unknown { .. } => InputProof::Unknown,
        }
    }

    /// The pane's own agent reported a turn starting: an `Enter` it had not
    /// confirmed submitted the box, and emptied it.
    pub(crate) fn turn_started(&mut self) {
        self.unconfirmed = None;
    }

    /// The doorbell's own line was proven submitted with no human operation
    /// since its admission (plan §1.4): the agent drew it alone in its
    /// composer and took it, so the box is empty now.
    pub(crate) fn reconcile_deck_submit(&mut self) {
        self.content = Content::default();
        self.at_line_end = false;
        self.unconfirmed = None;
    }

    /// Feed one byte the PTY took. `submits` and `in_paste` are the caller's
    /// stream state (paste framing, the keypress behind the byte), as the
    /// draft bit is fed.
    pub(crate) fn feed_byte(&mut self, byte: u8, submits: bool, in_paste: bool) {
        if submits {
            self.decode = Decode::Ground;
            self.apply(Key::Submit);
            return;
        }
        if let Some(key) = self.decode_byte(byte, in_paste) {
            self.apply(key);
        }
    }

    fn decode_byte(&mut self, byte: u8, in_paste: bool) -> Option<Key> {
        const ESC: u8 = 0x1b;
        match std::mem::take(&mut self.decode) {
            Decode::Ground => self.ground(byte, in_paste),
            Decode::Esc { paste: true } => match byte {
                b'[' => {
                    self.decode = Decode::Csi {
                        params: Vec::new(),
                        paste: true,
                    };
                    None
                }
                // An ESC pasted as content.
                _ => Some(Key::Unfollowed {
                    newline: matches!(byte, b'\r' | b'\n'),
                }),
            },
            Decode::Esc { paste: false } => match byte {
                b'[' => {
                    self.decode = Decode::Csi {
                        params: Vec::new(),
                        paste: false,
                    };
                    None
                }
                b'O' => {
                    self.decode = Decode::Ss3;
                    None
                }
                b']' | b'P' => {
                    self.decode = Decode::Text {
                        osc: byte == b']',
                        esc: false,
                    };
                    None
                }
                // Escape twice: an Escape key of its own, then this one.
                ESC => {
                    self.decode = Decode::Esc { paste: false };
                    None
                }
                // `Alt+<key>`, `Alt+Enter`, `Alt+Backspace`.
                _ => Some(Key::Unfollowed {
                    newline: byte == b'\r',
                }),
            },
            Decode::Csi { mut params, paste } => match byte {
                0x20..=0x3f if params.len() < MAX_CSI_LEN => {
                    params.push(byte);
                    self.decode = Decode::Csi { params, paste };
                    None
                }
                // A legacy X10 mouse report: three raw bytes follow.
                b'M' if params.is_empty() && !paste => {
                    self.decode = Decode::X10 { remaining: 3 };
                    None
                }
                0x40..=0x7e => csi_key(&params, byte, paste),
                // Not a CSI after all.
                _ => Some(Key::Unfollowed { newline: true }),
            },
            Decode::Ss3 => match byte {
                b'D' => Some(Key::Left),
                b'C' => Some(Key::Right),
                b'H' => Some(Key::Home),
                b'F' => Some(Key::End),
                b'A' | b'B' => Some(Key::Unfollowed { newline: true }),
                _ => Some(Key::Unfollowed { newline: false }),
            },
            Decode::Text { osc, esc } => {
                match byte {
                    b'\\' if esc => {}
                    0x07 if osc => {}
                    ESC => self.decode = Decode::Text { osc, esc: true },
                    _ if esc => {
                        // Never a reply: read what followed afresh.
                        self.decode = Decode::Esc { paste: false };
                        return self.decode_byte(byte, in_paste);
                    }
                    _ => self.decode = Decode::Text { osc, esc: false },
                }
                None
            }
            Decode::X10 { remaining } => {
                if byte < 0x20 {
                    return Some(Key::Unfollowed { newline: true });
                }
                if remaining > 1 {
                    self.decode = Decode::X10 {
                        remaining: remaining - 1,
                    };
                }
                None
            }
            Decode::Utf8 { remaining, paste } => {
                if byte & 0xc0 != 0x80 {
                    return Some(Key::Unfollowed { newline: true });
                }
                if remaining > 1 {
                    self.decode = Decode::Utf8 {
                        remaining: remaining - 1,
                        paste,
                    };
                    return None;
                }
                Some(Key::Unfollowed { newline: false })
            }
        }
    }

    fn ground(&mut self, byte: u8, in_paste: bool) -> Option<Key> {
        if in_paste {
            return match byte {
                0x1b => {
                    self.decode = Decode::Esc { paste: true };
                    None
                }
                b'\r' | b'\n' => Some(Key::Newline),
                0x20..=0x7e => Some(Key::Insert(char::from(byte))),
                0xc0..=0xf7 => {
                    self.decode = Decode::Utf8 {
                        remaining: utf8_continuations(byte),
                        paste: true,
                    };
                    None
                }
                _ => Some(Key::Unfollowed { newline: false }),
            };
        }
        match byte {
            0x1b => {
                self.decode = Decode::Esc { paste: false };
                None
            }
            0x20..=0x7e => Some(Key::Insert(char::from(byte))),
            0x7f | 0x08 => Some(Key::Backspace),
            b'\n' => Some(Key::Newline),
            0x01 => Some(Key::Home),
            0x05 => Some(Key::End),
            0x15 => Some(Key::KillBefore),
            0x03 => Some(Key::ClearAll),
            0xc0..=0xf7 => {
                self.decode = Decode::Utf8 {
                    remaining: utf8_continuations(byte),
                    paste: false,
                };
                None
            }
            // A CR that does not submit follows an ESC (`Alt+Enter`), which
            // the ESC arm reads; any other control key is not followed.
            _ => Some(Key::Unfollowed {
                newline: byte == b'\r',
            }),
        }
    }

    fn apply(&mut self, key: Key) {
        let after_end = std::mem::replace(&mut self.at_line_end, key == Key::End);
        // What an unconfirmed `Enter` may have left is on the current line
        // (or more): emptied with everything else by `Ctrl+C`, and with the
        // current line by `End` then `Ctrl+U`, which leaves only other lines
        // for the screen to show.
        if let Some(multiline) = self.unconfirmed {
            match key {
                Key::ClearAll => self.unconfirmed = None,
                Key::KillBefore if after_end => {
                    self.unconfirmed = None;
                    self.content = Content::Known {
                        text: Vec::new(),
                        cursor: Some(0),
                        others: multiline,
                    };
                    return;
                }
                _ => {}
            }
        }
        let content = std::mem::take(&mut self.content);
        self.content = match content {
            Content::Known {
                text,
                cursor,
                others,
            } => known(text, cursor, others, key),
            Content::Unknown { multiline } => match key {
                Key::ClearAll => Content::default(),
                // Whatever the box held went with it, unless it was only
                // blanks: confirmed by the agent's next turn.
                Key::Submit => {
                    self.unconfirmed = Some(multiline);
                    Content::default()
                }
                Key::KillBefore if after_end => Content::Known {
                    text: Vec::new(),
                    cursor: Some(0),
                    others: multiline,
                },
                Key::Newline | Key::Unfollowed { newline: true } => {
                    Content::Unknown { multiline: true }
                }
                _ => Content::Unknown { multiline },
            },
        };
        if let Content::Known { text, .. } = &self.content
            && text.len() > MAX_CHARS
        {
            self.content = Content::Unknown { multiline: true };
        }
    }
}

/// The box after `key`, from known `text` and `cursor`.
fn known(mut text: Vec<char>, cursor: Option<usize>, others: bool, key: Key) -> Content {
    let multiline = others || text.contains(&'\n');
    let unknown = |newline: bool| Content::Unknown {
        multiline: multiline || newline,
    };
    let line_start = |at: usize, text: &[char]| {
        text[..at]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1)
    };
    let line_end = |at: usize, text: &[char]| {
        text[at..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(text.len(), |i| at + i)
    };
    match key {
        Key::ClearAll => return Content::default(),
        Key::Submit => {
            // A backslash before the cursor makes Enter a line break.
            if cursor.is_some_and(|at| at > 0 && text[at - 1] == '\\') {
                return unknown(true);
            }
            // Blanks alone are kept (measured); `others` may be blanks too.
            if text.iter().any(|c| !c.is_whitespace()) {
                return Content::default();
            }
            return Content::Known {
                text,
                cursor,
                others,
            };
        }
        Key::Unfollowed { newline } => return unknown(newline),
        _ => {}
    }
    let Some(at) = cursor else {
        // Content known, cursor not: a line's start or end is found again
        // only on a single line; any edit is not followed.
        return match key {
            Key::Home if !multiline => Content::Known {
                text,
                cursor: Some(0),
                others,
            },
            Key::End if !multiline => Content::Known {
                cursor: Some(text.len()),
                text,
                others,
            },
            _ => unknown(false),
        };
    };
    let at = match key {
        Key::Insert(c) => {
            text.insert(at, c);
            at + 1
        }
        Key::Newline => {
            text.insert(at, '\n');
            at + 1
        }
        Key::Backspace if at == 0 => {
            if others {
                return unknown(true);
            }
            0
        }
        Key::Backspace => {
            text.remove(at - 1);
            at - 1
        }
        Key::Delete if at == text.len() => {
            if others {
                return unknown(true);
            }
            at
        }
        Key::Delete => {
            text.remove(at);
            at
        }
        Key::Left if at == 0 => {
            if others {
                return unknown(true);
            }
            0
        }
        Key::Left => at - 1,
        Key::Right if at == text.len() => {
            if others {
                return unknown(true);
            }
            at
        }
        Key::Right => at + 1,
        Key::Home => line_start(at, &text),
        Key::End => line_end(at, &text),
        Key::KillBefore => {
            let start = line_start(at, &text);
            text.drain(start..at);
            start
        }
        Key::ClearAll | Key::Submit | Key::Unfollowed { .. } => unreachable!("handled above"),
    };
    Content::Known {
        text,
        cursor: Some(at),
        others,
    }
}

/// How many continuation bytes follow a UTF-8 lead byte.
fn utf8_continuations(lead: u8) -> u8 {
    match lead {
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        _ => 3,
    }
}

/// The key a complete CSI sequence is, `None` for a report or a paste marker.
fn csi_key(params: &[u8], fin: u8, paste: bool) -> Option<Key> {
    // Inside a paste only the closing marker is framing (a paste does not
    // nest).
    let marker = fin == b'~' && (params == b"201" || (params == b"200" && !paste));
    if marker {
        return None;
    }
    if paste {
        // Pasted content that happens to look like a sequence.
        return Some(Key::Unfollowed { newline: false });
    }
    let private = params
        .first()
        .is_some_and(|b| matches!(b, b'<' | b'=' | b'>' | b'?'));
    let report = private
        || matches!(fin, b'M' | b'm' | b'R' | b'c' | b'n' | b't')
        || (params.contains(&b'$') && fin == b'y')
        || (params.is_empty() && matches!(fin, b'I' | b'O'));
    if report {
        return None;
    }
    match (params, fin) {
        (b"", b'D') => Some(Key::Left),
        (b"", b'C') => Some(Key::Right),
        (b"" | b"1", b'H') | (b"1", b'~') => Some(Key::Home),
        (b"" | b"1", b'F') | (b"4", b'~') => Some(Key::End),
        (b"3", b'~') => Some(Key::Delete),
        // `Up` and `Down` can recall a prompt of several lines.
        (b"", b'A' | b'B') => Some(Key::Unfollowed { newline: true }),
        // `Shift+Enter` (`ESC[13;2u`) and other keyboard-protocol keys may
        // break a line.
        (_, b'u') => Some(Key::Unfollowed { newline: true }),
        _ => Some(Key::Unfollowed { newline: false }),
    }
}

#[cfg(test)]
mod tests {
    use spec::spec;

    use super::InputProof::{self, Draft, Empty, Unknown};
    use crate::agent_pty::DraftTestStream;
    use crate::draft_deferral::ByteOrigin;

    /// A pane's input stream, fed through the real paste framing and submit
    /// scan.
    #[derive(Default)]
    struct Pane(DraftTestStream);

    impl Pane {
        fn user(&mut self, bytes: &[u8]) -> &mut Self {
            self.feed(bytes, ByteOrigin::User)
        }

        fn deck(&mut self, bytes: &[u8]) -> &mut Self {
            self.feed(bytes, ByteOrigin::Deck)
        }

        fn feed(&mut self, bytes: &[u8], origin: ByteOrigin) -> &mut Self {
            for &byte in bytes {
                self.0.feed_byte(byte, origin);
            }
            self
        }

        fn proof(&self) -> InputProof {
            self.0.composer().proof()
        }
    }

    fn after(bytes: &[u8]) -> InputProof {
        Pane::default().user(bytes).proof()
    }

    /// Scenario: Type into a pane's input stream the keys measured on a real Claude Code 2.1.310 session: spaces, spaces with the cursor moved back to the start by Left, Ctrl+A or Home, a word with a line-kill before its last letter, a space deleted from in front of it, blanks submitted with Enter. Each blank or partly killed box reads as a draft, the empty box stays empty after Right, Left or Backspace, and only edits that really empty the box (End then Ctrl+U, Ctrl+C, Backspace over every character, Delete at the start, Enter on text) prove it empty.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_measured_edits_on_one_line() {
        // Spaces are content, whatever the cursor does afterwards.
        assert_eq!(after(b"   "), Draft);
        assert_eq!(after(b"   \x1b[D\x1b[D\x1b[D"), Draft);
        assert_eq!(after(b"   \x1bOD\x1bOD\x1bOD"), Draft);
        assert_eq!(after(b"  \x01"), Draft);
        assert_eq!(after(b"  \x1b[H"), Draft);
        assert_eq!(after(b"  \x1b[1~"), Draft);
        assert_eq!(after(b"  \x1bOH"), Draft);
        // Moving to the end and killing the line empties it.
        assert_eq!(after(b"   \x1b[D\x05\x15"), Empty);
        assert_eq!(after(b"   \x01\x1b[F\x15"), Empty);
        assert_eq!(after(b"   \x01\x1b[4~\x15"), Empty);
        // Ctrl+U kills only what is before the cursor.
        assert_eq!(after(b"abc\x1b[D\x15"), Draft);
        assert_eq!(after(b"   \x1b[D\x15"), Draft);
        assert_eq!(after(b"abc\x15"), Empty);
        // A delete applies where the cursor is.
        assert_eq!(after(b" \x01\x7f"), Draft);
        assert_eq!(after(b" \x01\x1b[3~"), Empty);
        assert_eq!(after(b"ab\x7f\x7f"), Empty);
        assert_eq!(after(b"ab\x08\x08"), Empty);
        assert_eq!(after(b"a \x1b[D\x1b[D\x1b[3~\x1b[3~"), Empty);
        // Keys that edit nothing in an empty box leave it empty.
        assert_eq!(after(b"\x1b[C"), Empty);
        assert_eq!(after(b"\x1b[D"), Empty);
        assert_eq!(after(b"\x7f\x7f"), Empty);
        assert_eq!(after(b"\x1b[3~"), Empty);
        assert_eq!(after(b"\x15\x01\x05"), Empty);
        assert_eq!(after(b"r\x1b[D\x1b[C\x7f"), Empty);
        // Ctrl+C empties the box; Enter submits text and keeps blanks.
        assert_eq!(after(b"  abc \x1b[D\x03"), Empty);
        assert_eq!(after(b"   \r"), Draft);
        assert_eq!(after(b"ab  \r"), Empty);
        assert_eq!(after(b"ab\\\r"), Unknown);
        assert_eq!(after(b"ab\\\x1b[D\x1b[C\r"), Unknown);
    }

    /// Scenario: Break lines in a pane's input with Ctrl+J and inside a paste, then edit across them: a line of nothing but a break or blanks, a blank line with text after it and the cursor back on the first line, and Backspace joining lines. A box holding only a line break or blanks on another line is a draft, Up or Down loses track of it, and deleting every character and break proves it empty again.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_line_breaks_are_content() {
        assert_eq!(after(b"\n"), Draft);
        assert_eq!(after(b"\n  "), Draft);
        assert_eq!(after(b"\n  \x1b[D\x1b[D\x1b[D"), Draft);
        assert_eq!(
            after(b"x\n\nabc\x01\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x01\x1b[3~"),
            Draft
        );
        assert_eq!(after(b"\n\x7f"), Empty);
        assert_eq!(after(b"ab\ncd\x15\x7f\x15"), Empty);
        assert_eq!(after(b"ab\ncd\x15\x15"), Draft);
        // Up and Down: history, or a column the model does not track.
        assert_eq!(after(b"x\n\nabc\x1b[A"), Unknown);
        assert_eq!(after(b"\x1b[A"), Unknown);
        assert_eq!(after(b"ab\x1b[B"), Unknown);
        // A paste: printable bytes and its line breaks are content; the
        // markers edit nothing, split across writes or not.
        assert_eq!(after(b"\x1b[200~  \x1b[201~"), Draft);
        assert_eq!(after(b"\x1b[200~\r\x1b[201~"), Draft);
        assert_eq!(after(b"\x1b[200~\x1b[201~"), Empty);
        let mut pane = Pane::default();
        pane.user(b"\x1b[20")
            .user(b"0~a")
            .user(b" b\x1b[2")
            .user(b"01~");
        assert_eq!(pane.proof(), Draft);
        pane.user(b"\x05\x15");
        assert_eq!(pane.proof(), Empty);
        // An Enter inside a paste is a line break, not a submit.
        assert_eq!(after(b"\x1b[200~ab\r\x1b[201~"), Draft);
    }

    /// Scenario: Send a pane keys the model does not follow (Tab, Up, non-ASCII text, a word-move, Shift+Enter, Alt+x, a pasted escape) and then each of the ways back: End then Ctrl+U, Ctrl+C, and Enter followed by the agent starting a turn. Each unfollowed key leaves the box unknown; End and Ctrl+U must be adjacent; an Enter alone stays unknown until the turn starts, what was typed after it is followed, and blanks typed after it stay a draft; after End and Ctrl+U on a box that may have had other lines, Backspace at the start loses track again.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_unfollowed_keys_and_the_ways_back() {
        for key in [
            &b"\t"[..],
            b"\x1b[A",
            "\u{e9}".as_bytes(),
            b"ab\x1b[1;5D",
            b"\x1b[13;2u",
            b"\x1bx",
            b"\x1b\r",
            b"\x17",
            b"\x0b",
            b"\x1b[200~\x1bz\x1b[201~",
            b"\x1b[5~",
        ] {
            assert_eq!(after(key), Unknown, "{key:?}");
            let mut cleared = key.to_vec();
            cleared.extend_from_slice(b"\x05\x15");
            assert_eq!(after(&cleared), Empty, "{key:?} then End, Ctrl+U");
            let mut killed = key.to_vec();
            killed.extend_from_slice(b"\x03");
            assert_eq!(after(&killed), Empty, "{key:?} then Ctrl+C");
            let mut apart = key.to_vec();
            apart.extend_from_slice(b"\x05\x1b[D\x15");
            assert_eq!(after(&apart), Unknown, "{key:?}: End and Ctrl+U apart");
        }
        // Enter on a box the model lost track of: unknown until the agent
        // starts a turn.
        let mut pane = Pane::default();
        pane.user(b"\t\r");
        assert_eq!(pane.proof(), Unknown);
        pane.0.turn_started();
        assert_eq!(pane.proof(), Empty);
        // A turn already under way confirms nothing.
        let mut pane = Pane::default();
        pane.0.turn_started();
        pane.user(b"\t\r");
        pane.0.turn_started();
        assert_eq!(pane.proof(), Unknown);
        pane.0.turn_ended();
        pane.0.turn_started();
        assert_eq!(pane.proof(), Empty);
        // Typed after the Enter: followed as typed into an empty box.
        let mut pane = Pane::default();
        pane.user(b"\t\rab\x7f\x7f");
        assert_eq!(pane.proof(), Unknown);
        pane.0.turn_started();
        assert_eq!(pane.proof(), Empty);
        let mut pane = Pane::default();
        pane.user(b"\t\r  ");
        assert_eq!(pane.proof(), Draft);
        pane.0.turn_started();
        assert_eq!(pane.proof(), Draft);
        // End then Ctrl+U after the unconfirmed Enter empties its line.
        assert_eq!(after(b"\t\r\x05\x15"), Empty);
        // Other lines may be left: joining one back loses track.
        assert_eq!(after(b"\x1b[A\x05\x15"), Empty);
        assert_eq!(after(b"\x1b[A\x05\x15\x7f"), Unknown);
        assert_eq!(after(b"\x1b[A\x05\x15\x1b[D"), Unknown);
        assert_eq!(after(b"\x1b[A\x05\x15\x1b[C"), Unknown);
        assert_eq!(after(b"\x1b[A\x05\x15\x1b[3~"), Unknown);
        assert_eq!(after(b"\x1b[A\x05\x15ab\x7f\x7f"), Empty);
        // Enter on blanks with other lines possibly left keeps them.
        assert_eq!(after(b"\x1b[A\x05\x15  \r"), Draft);
    }

    /// Scenario: Send terminal reports a client forwards with the keys (SGR and X10 mouse, focus, cursor-position and device-attribute replies, an OSC and a DCS reply) into an empty box, a box of blanks and a box the model lost track of. None of them edits the box: each keeps the proof it had.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_terminal_reports_edit_nothing() {
        let reports: &[&[u8]] = &[
            b"\x1b[<0;10;5M",
            b"\x1b[<0;10;5m",
            b"\x1b[M !!",
            b"\x1b[I",
            b"\x1b[O",
            b"\x1b[12;3R",
            b"\x1b[?62;22c",
            b"\x1b[0n",
            b"\x1b]11;rgb:0000/0000/0000\x07",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
            b"\x1bP1$r0m\x1b\\",
            b"\x1b[?2004;1$y",
            b"\x1b[8;24;80t",
        ];
        for report in reports {
            for (start, proof) in [(&b""[..], Empty), (b"  ", Draft), (b"\t", Unknown)] {
                let mut bytes = start.to_vec();
                bytes.extend_from_slice(report);
                assert_eq!(after(&bytes), proof, "{start:?} then {report:?}");
            }
        }
        // A report does not separate End from Ctrl+U.
        assert_eq!(after(b"\t\x05\x1b[I\x15"), Empty);
    }

    /// Scenario: The deck types its own token-led line into an empty box and submits it, withdraws one with deletes, or leaves one typed and unsent; the doorbell's verified submit is reconciled on a box the model lost track of; a box grows past the model's bound. The deck's submitted line leaves the box empty, its withdrawn line leaves it empty, its unsent line is a draft, the reconcile proves the box empty, and past the bound the box is unknown.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_the_decks_own_bytes_and_the_bound() {
        let line = b"(abcdef) dot-agent-deck: message m-abcdefgh-0123456789abcdef waiting, run: dot-agent-deck inbox fetch";
        let mut pane = Pane::default();
        pane.deck(line);
        assert_eq!(pane.proof(), Draft);
        pane.deck(b"\r");
        assert_eq!(pane.proof(), Empty);
        let mut pane = Pane::default();
        pane.deck(line).deck(&vec![0x7f; line.len()]);
        assert_eq!(pane.proof(), Empty);
        let mut pane = Pane::default();
        pane.user(b"\t");
        assert_eq!(pane.proof(), Unknown);
        pane.0.reconcile_deck_submit_for_test();
        assert_eq!(pane.proof(), Empty);
        let mut pane = Pane::default();
        pane.user(&vec![b'a'; super::MAX_CHARS]);
        assert_eq!(pane.proof(), Draft);
        pane.user(b"a");
        assert_eq!(pane.proof(), Unknown);
    }
}
