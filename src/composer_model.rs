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
//! position, a numbered OSC or DCS reply and the like) and the paste markers
//! edit nothing.
//!
//! **Not followed**, so the box becomes [`InputProof::Unknown`]: `Up` and
//! `Down` (history, or a move between lines whose column the model does not
//! track), `Tab` (completion), non-ASCII text, any other control key, an
//! `Alt` or modified key, `Shift+Enter`, `Alt+Enter`, an `Enter` after a
//! backslash, anything else inside a paste, and an escape sequence that is
//! not one of the above. While a sequence is still arriving the box is
//! unknown too, except after a lone `ESC`, which swallows nothing: the next
//! byte reads it as `Alt`.
//!
//! **The ways back.** From an unknown box, three things make it known again:
//! `Ctrl+C`; `End` then `Ctrl+U` with nothing between them, which empties
//! the current line (whether other lines are left is what the screen shows,
//! since the doorbell admits only a composer drawn on a single row); and an
//! `Enter` the agent's own prompt hook then reports as a prompt it took
//! ([`ComposerModel::prompt_reported`]). An `Enter` on a box of blanks
//! submits nothing and starts nothing, so a turn starting is not that
//! evidence: a turn already under way, one that started for another reason,
//! or a late report of an earlier one would confirm an `Enter` that left
//! blanks in the box. A report counts for the `Enter` only when no earlier
//! submit can own it: the boxes the model knew were submitted, and the
//! unknown `Enter`s it gave up on, each owe the agent's report until one
//! arrives (a known box by its text), so a report goes to the oldest that
//! can own it, and only a report none of them can own confirms.
//!
//! Until that report, what is typed after the `Enter` is followed on its own,
//! as if typed into an empty box: if the `Enter` submitted that is the box,
//! and if it did not, what the box held is still there around it. So a key
//! that would reach past what was typed after the `Enter` — `Left`,
//! `Backspace`, `Home` or `Ctrl+U` at its start, `Right`, `Delete` or `End`
//! at its end, an `Enter` at the start of text typed after it — gives up on
//! the `Enter`: the box is unknown again, its lines counted from both.
//! `Enter` on text typed after it submits the whole box, whatever the earlier
//! `Enter` left; a second `Enter` with nothing typed since awaits the same
//! report, since if either submitted the box is empty.
//!
//! **What it cannot see**, and accepts: text the agent puts into its own box
//! (a restored prompt) is invisible to it; a key that reaches a dialog or
//! menu rather than the box is read as an edit of the box; and a prompt the
//! agent submits by itself, not from its box, is read as a prompt it took
//! from the box. The first can make the box look empty when it holds text the
//! agent added, which the screen then shows unless it is blanks. The second
//! mostly makes the box look fuller than it is, but an `Enter` on text, or a
//! `Ctrl+C`, that a dialog took instead reads as emptying the box. The third
//! needs an `Enter` on a box of blanks the model did not know, still owed
//! its report when the agent's own prompt arrives; so does a report of an
//! earlier submit whose text does not carry what the model followed, should
//! the agent rewrite a prompt before its hook sees it. In each, a box of blanks
//! can read as empty. The doorbell's other checks (a Clean ready frame, no
//! dialog) still apply on top. The other way round, a submit that never
//! reports (a command the agent handles without its prompt hook), or an
//! earlier report its own `Enter` was not recorded in time for, can leave an
//! `Enter` unconfirmed: the box stays unknown, held as a possible draft,
//! until `End` then `Ctrl+U`, `Ctrl+C`, or a prompt typed and sent.

use std::collections::VecDeque;

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

/// What the pane's own agent reported about a prompt it took, from its prompt
/// hook (Claude Code's `UserPromptSubmit`): see
/// [`ComposerModel::prompt_reported`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptReport<'a> {
    /// The prompt's text as the hook reported it, which may be cut short
    /// (`crate::submitted_prompt::USER_PROMPT_MAX_LEN`).
    User(&'a str),
    /// A prompt the deck recognised as its own line.
    Deck,
}

/// The most characters followed: past it the box is [`InputProof::Unknown`]
/// rather than a growing copy of someone's prompt.
const MAX_CHARS: usize = 4096;

/// A CSI sequence longer than this is not a key the model follows.
const MAX_CSI_LEN: usize = 16;

/// An OSC or DCS reply longer than this is not a reply.
const MAX_REPLY_LEN: u16 = 512;

/// The most submits kept owing a report. Past it the oldest is forgotten, so
/// its report, if it came that late, could confirm an unknown `Enter`; a
/// known box's entry goes as soon as a later report matches, so this many
/// stay only after as many submits the agent never reported.
const MAX_OWED: usize = 64;

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

impl Content {
    /// Whether the box may hold more than one line.
    fn multiline(&self) -> bool {
        match self {
            Self::Known { text, others, .. } => *others || text.contains(&'\n'),
            Self::Unknown { multiline } => *multiline,
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
    /// `ESC ]` or `ESC P`, a reply ended by `BEL` (OSC) or `ESC \`: a number
    /// first (`lead`), then `;` (OSC) or `$`, `+` (DCS), or a DCS's `>` or
    /// `!`, then printable bytes. `esc` once an `ESC` arrived inside it;
    /// `len` the bytes so far.
    Text {
        osc: bool,
        lead: bool,
        esc: bool,
        len: u16,
    },
    /// The three bytes after a legacy X10 mouse report's `ESC [ M`.
    X10 { remaining: u8 },
    /// The continuation bytes of a UTF-8 character still to come.
    Utf8 { remaining: u8, paste: bool },
}

/// What a submit the model counted owes: the agent's report of it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owed {
    /// A box the model knew held text was submitted: that text, and whether
    /// the deck typed it.
    Prompt { text: String, deck: bool },
    /// An `Enter` on a box the model did not know, given up on before its
    /// report came: it may have submitted. Forgotten once the agent has
    /// ended `expires` turns in all, by when a report it caused has come.
    Maybe { expires: u64 },
}

/// An `Enter` on a box the model did not know, not yet reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unconfirmed {
    /// What the box held may be more than one line.
    multiline: bool,
    /// [`Owed::Maybe::expires`], should the model give up on it.
    expires: u64,
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
    /// not reported it: if it submitted, the box was emptied and `content`
    /// is what was typed after it; if it did not (a box of blanks keeps
    /// them), what the box held is still there around `content`.
    unconfirmed: Option<Unconfirmed>,
    /// The submits still owed the agent's report, oldest first.
    owed: VecDeque<Owed>,
    /// The agent reported a turn starting and has not reported it ended.
    busy: bool,
    /// How many turns the agent reported ending.
    turns: u64,
    decode: Decode,
}

impl ComposerModel {
    /// What the bytes so far say about the box.
    pub(crate) fn proof(&self) -> InputProof {
        match &self.content {
            Content::Known { text, .. } if !text.is_empty() => InputProof::Draft,
            // A sequence still arriving may be swallowing keys.
            _ if !matches!(self.decode, Decode::Ground | Decode::Esc { paste: false }) => {
                InputProof::Unknown
            }
            Content::Known { .. } if self.unconfirmed.is_none() => InputProof::Empty,
            Content::Known { .. } | Content::Unknown { .. } => InputProof::Unknown,
        }
    }

    /// The pane's own agent reported a turn starting (or going on).
    pub(crate) fn turn_started(&mut self) {
        self.busy = true;
    }

    /// The pane's own agent reported Idle: a turn it reported starting has
    /// ended, and a given-up `Enter` whose report would have come by now
    /// owes nothing.
    pub(crate) fn turn_ended(&mut self) {
        if !std::mem::replace(&mut self.busy, false) {
            return;
        }
        self.turns += 1;
        let turns = self.turns;
        self.owed
            .retain(|owed| !matches!(owed, Owed::Maybe { expires } if turns >= *expires));
    }

    /// The pane's own agent reported, from its prompt hook, a prompt it
    /// took. It goes to the oldest submit still owed a report that can own
    /// it: the deck's own line for the deck's, a known box whose text it
    /// carries, or else an unknown `Enter` given up on. With no such owner,
    /// it is the unconfirmed `Enter`'s, which submitted and emptied the box.
    ///
    /// The agent takes its prompts in order, so a known box's report also
    /// settles every older entry: whatever they were going to report would
    /// have come first.
    pub(crate) fn prompt_reported(&mut self, report: PromptReport<'_>) {
        let owner = self.owed.iter().position(|owed| match (owed, report) {
            (Owed::Prompt { deck, .. }, PromptReport::Deck) => *deck,
            (Owed::Prompt { text, .. }, PromptReport::User(reported)) => carries(reported, text),
            _ => false,
        });
        if let Some(owner) = owner {
            self.owed.drain(..=owner);
            return;
        }
        if report == PromptReport::Deck {
            return;
        }
        if let Some(maybe) = self
            .owed
            .iter()
            .position(|owed| matches!(owed, Owed::Maybe { .. }))
        {
            self.owed.remove(maybe);
            return;
        }
        self.unconfirmed = None;
    }

    /// The doorbell's own line was proven submitted with no human operation
    /// since its admission (plan §1.4): the agent drew it alone in its
    /// composer and took it, so the box is empty now.
    pub(crate) fn reconcile_deck_submit(&mut self) {
        self.settle();
        self.content = Content::default();
        self.at_line_end = false;
    }

    /// Feed one byte the PTY took. `submits` and `in_paste` are the caller's
    /// stream state (paste framing, the keypress behind the byte), as the
    /// draft bit is fed; `deck` when the deck wrote it.
    pub(crate) fn feed_byte(&mut self, byte: u8, submits: bool, in_paste: bool, deck: bool) {
        if submits {
            // An Enter ends a sequence it arrived inside of, which may have
            // swallowed keys.
            if std::mem::take(&mut self.decode) != Decode::Ground {
                self.apply(Key::Unfollowed { newline: false }, deck);
            }
            self.apply(Key::Submit, deck);
            return;
        }
        if let Some(key) = self.decode_byte(byte, in_paste) {
            self.apply(key, deck);
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
                        lead: true,
                        esc: false,
                        len: 0,
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
                // Not a CSI after all: this byte is read afresh.
                _ => self.abandon(true, byte, in_paste),
            },
            Decode::Ss3 => match byte {
                b'D' => Some(Key::Left),
                b'C' => Some(Key::Right),
                b'H' => Some(Key::Home),
                b'F' => Some(Key::End),
                b'A' | b'B' => Some(Key::Unfollowed { newline: true }),
                _ => Some(Key::Unfollowed { newline: false }),
            },
            Decode::Text {
                osc,
                lead,
                esc,
                len,
            } => {
                if esc {
                    if byte == b'\\' {
                        return None;
                    }
                    // Never a reply: what followed is read afresh.
                    self.apply(Key::Unfollowed { newline: false }, false);
                    self.decode = Decode::Esc { paste: false };
                    return self.decode_byte(byte, in_paste);
                }
                let next = |lead| Decode::Text {
                    osc,
                    lead,
                    esc: false,
                    len: len + 1,
                };
                self.decode = match byte {
                    ESC => Decode::Text {
                        osc,
                        lead,
                        esc: true,
                        len,
                    },
                    0x07 if osc && !lead => return None,
                    _ if len >= MAX_REPLY_LEN => return self.abandon(false, byte, in_paste),
                    b'0'..=b'9' if lead => next(true),
                    b';' if lead && osc && len > 0 => next(false),
                    b'$' | b'+' if lead && !osc && len > 0 => next(false),
                    b'>' | b'!' if lead && !osc && len == 0 => next(false),
                    0x20..=0x7e if !lead => next(false),
                    _ => return self.abandon(false, byte, in_paste),
                };
                None
            }
            Decode::X10 { remaining } => {
                if byte < 0x20 {
                    return self.abandon(true, byte, in_paste);
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
                    return self.abandon(true, byte, in_paste);
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

    /// The sequence so far was no key the model follows (`newline` when it
    /// may have broken a line), and `byte` is read afresh after it.
    fn abandon(&mut self, newline: bool, byte: u8, in_paste: bool) -> Option<Key> {
        self.apply(Key::Unfollowed { newline }, false);
        self.ground(byte, in_paste)
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

    fn apply(&mut self, key: Key, deck: bool) {
        let after_end = std::mem::replace(&mut self.at_line_end, key == Key::End);
        if key == Key::ClearAll {
            self.settle();
            self.content = Content::default();
            return;
        }
        if self.unconfirmed.is_some() && self.reaches_past(key) {
            self.give_up();
        }
        let content = std::mem::take(&mut self.content);
        self.content = match content {
            Content::Known {
                text,
                cursor,
                others,
            } if key == Key::Submit => self.submit(text, cursor, others, deck),
            Content::Known {
                text,
                cursor,
                others,
            } => known(text, cursor, others, key),
            Content::Unknown { multiline } => match key {
                // Whatever the box held went with it, unless it was only
                // blanks: confirmed by the agent's report of it.
                Key::Submit => {
                    self.unconfirmed = Some(Unconfirmed {
                        multiline,
                        expires: self.report_by(),
                    });
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
        if matches!(self.content, Content::Unknown { .. }) {
            self.give_up();
        }
    }

    /// `Enter` on a box whose `text` is known.
    fn submit(
        &mut self,
        text: Vec<char>,
        cursor: Option<usize>,
        others: bool,
        deck: bool,
    ) -> Content {
        // A backslash before the cursor makes Enter a line break.
        if cursor.is_some_and(|at| at > 0 && text[at - 1] == '\\') {
            return Content::Unknown { multiline: true };
        }
        // Blanks alone are kept (measured); `others` may be blanks too.
        if !text.iter().any(|c| !c.is_whitespace()) {
            return Content::Known {
                text,
                cursor,
                others,
            };
        }
        // The whole box went, whatever an earlier Enter left in it.
        self.settle();
        self.owe(Owed::Prompt {
            text: text.into_iter().collect(),
            deck,
        });
        Content::default()
    }

    /// Whether `key`, after an unconfirmed `Enter`, reaches past what was
    /// typed after it, into what that `Enter` may have left.
    fn reaches_past(&self, key: Key) -> bool {
        let Content::Known { text, cursor, .. } = &self.content else {
            return false;
        };
        let Some(at) = *cursor else {
            return true;
        };
        let first_line = !text[..at].contains(&'\n');
        let last_line = !text[at..].contains(&'\n');
        match key {
            Key::Left | Key::Backspace => at == 0,
            // What is before the cursor decides whether Enter breaks the
            // line. With nothing typed since, this Enter and the earlier one
            // empty the box together if either submits, so they await one
            // report.
            Key::Submit => at == 0 && !text.is_empty(),
            Key::Home | Key::KillBefore => first_line,
            Key::Right | Key::Delete => at == text.len(),
            Key::End => last_line,
            _ => false,
        }
    }

    /// The `Enter` is settled some other way than by its report: the box
    /// went with `Ctrl+C`, or with a submit of text. It still owes the report
    /// it may have caused.
    fn settle(&mut self) {
        if let Some(unconfirmed) = self.unconfirmed.take() {
            self.owe(Owed::Maybe {
                expires: unconfirmed.expires,
            });
        }
    }

    /// The model gives up on the unconfirmed `Enter`: the box is unknown,
    /// on as many lines as it and what was typed since may hold.
    fn give_up(&mut self) {
        let Some(unconfirmed) = self.unconfirmed else {
            return;
        };
        self.settle();
        self.content = Content::Unknown {
            multiline: unconfirmed.multiline || self.content.multiline(),
        };
    }

    /// The turn count by which a submit now has its report: the end of the
    /// turn it starts, or of the next one when a turn or a report it would
    /// queue behind is already under way.
    fn report_by(&self) -> u64 {
        self.turns + 1 + u64::from(self.busy || !self.owed.is_empty())
    }

    fn owe(&mut self, owed: Owed) {
        if self.owed.len() == MAX_OWED {
            self.owed.pop_front();
        }
        self.owed.push_back(owed);
    }
}

/// Whether a prompt report carries `text`, the box the model knew was
/// submitted: the agent may report several prompts it queued as one, or cut
/// a long one short, so either containing the other counts.
fn carries(reported: &str, text: &str) -> bool {
    let reported = reported.trim().trim_end_matches('…').trim_end();
    let text = text.trim();
    !reported.is_empty() && !text.is_empty() && (reported.contains(text) || text.contains(reported))
}

/// The box after `key`, from known `text` and `cursor`. `Enter` and `Ctrl+C`
/// are [`ComposerModel::apply`]'s.
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
    if let Key::Unfollowed { newline } = key {
        return unknown(newline);
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
        Key::ClearAll | Key::Submit | Key::Unfollowed { .. } => {
            unreachable!("ComposerModel::apply handles it")
        }
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
    use super::PromptReport::{Deck, User};
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

    /// Scenario: Send a pane keys the model does not follow (Tab, Up, non-ASCII text, a word-move, Shift+Enter, Alt+x, a pasted escape) and then each of the ways back that need no report: End then Ctrl+U, and Ctrl+C. Each unfollowed key leaves the box unknown; End and Ctrl+U must be adjacent; after End and Ctrl+U on a box that may have had other lines, Backspace, Left or Delete at the start, or Right at the end, loses track again, and Enter on blanks there keeps them.
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

    /// Scenario: Press Enter on a box the model lost track of, then let the agent report turns starting and ending, or report prompts from its prompt hook: an unrelated or late working report, a report of a prompt typed before that Enter, the deck's own line, a report that arrived before the Enter was recorded, and finally the Enter's own report, including after history recall with Up and inside a busy turn. Only a prompt report no earlier submit can own proves the box empty; a turn alone, or a report an earlier submit owes, leaves it unknown, held for End then Ctrl+U, Ctrl+C or a prompt typed and sent.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_an_unknown_enter_is_confirmed_only_by_its_own_report() {
        // A turn starting and ending is no evidence the Enter submitted.
        let mut pane = Pane::default();
        pane.user(b"\t\r");
        assert_eq!(pane.proof(), Unknown);
        pane.0.turn_started();
        pane.0.turn_ended();
        pane.0.turn_started();
        assert_eq!(pane.proof(), Unknown, "an unrelated turn confirms nothing");
        pane.0.prompt_reported(User("the recalled prompt"));
        assert_eq!(pane.proof(), Empty, "its own report confirms it");
        // The auditor's case (A6F2-U2): spaces the model knew, a key it does
        // not follow, then Enter on what is still a box of blanks.
        let mut pane = Pane::default();
        pane.user("  \u{e9}".as_bytes()).user(b"\x7f\x01\r");
        pane.0.turn_started();
        pane.0.turn_ended();
        assert_eq!(pane.proof(), Unknown, "the blanks may still be there");
        // A report of a prompt sent before the Enter belongs to that prompt,
        // late or not.
        let mut pane = Pane::default();
        pane.user(b"hello\r");
        assert_eq!(pane.proof(), Empty);
        pane.user(b"\t\r");
        pane.0.prompt_reported(User("hello"));
        assert_eq!(pane.proof(), Unknown, "the earlier prompt's report");
        pane.0.prompt_reported(User("something recalled"));
        assert_eq!(pane.proof(), Empty);
        // Two prompts queued in a turn and reported as one settle both.
        let mut pane = Pane::default();
        pane.0.turn_started();
        pane.user(b"ab\rcd\r\t\r");
        pane.0.prompt_reported(User("ab\ncd"));
        assert_eq!(pane.proof(), Unknown);
        pane.0.prompt_reported(User("cd"));
        assert_eq!(pane.proof(), Unknown, "cd's own report, late");
        pane.0.prompt_reported(User("zz"));
        assert_eq!(
            pane.proof(),
            Empty,
            "inside a busy turn, its report confirms"
        );
        // A long prompt is reported cut short.
        let mut pane = Pane::default();
        let long = "x".repeat(300);
        pane.user(long.as_bytes()).user(b"\r\t\r");
        pane.0.prompt_reported(User(&format!("{}…", &long[..200])));
        assert_eq!(pane.proof(), Unknown);
        // The deck's own line reports as the deck's.
        let mut pane = Pane::default();
        pane.deck(b"(abcdef) dot-agent-deck: message m-1 waiting\r");
        pane.user(b"\t\r");
        pane.0.prompt_reported(Deck);
        assert_eq!(pane.proof(), Unknown);
        pane.0.prompt_reported(Deck);
        assert_eq!(
            pane.proof(),
            Unknown,
            "the deck's report is never the user's"
        );
        // A report before its Enter was recorded: held, then cleared by hand.
        let mut pane = Pane::default();
        pane.0.prompt_reported(User("raced"));
        pane.user(b"\x1b[A\r");
        assert_eq!(pane.proof(), Unknown);
        pane.user(b"\x05\x15");
        assert_eq!(pane.proof(), Empty, "End then Ctrl+U");
        let mut pane = Pane::default();
        pane.0.prompt_reported(User("raced"));
        pane.user(b"\x1b[A\rnext\r");
        assert_eq!(pane.proof(), Empty, "a prompt typed and sent");
        // Typed after the Enter: followed as typed into an empty box.
        let mut pane = Pane::default();
        pane.user(b"\t\rab\x7f\x7f");
        assert_eq!(pane.proof(), Unknown);
        pane.0.prompt_reported(User("tabbed"));
        assert_eq!(pane.proof(), Empty);
        let mut pane = Pane::default();
        pane.user(b"\t\r  ");
        assert_eq!(pane.proof(), Draft);
        pane.0.prompt_reported(User("tabbed"));
        assert_eq!(pane.proof(), Draft);
        // A second Enter with nothing typed since awaits the same report.
        let mut pane = Pane::default();
        pane.user(b"\x1b[A\r\r");
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.proof(), Empty);
    }

    /// Scenario: Press Enter on a box the model lost track of and settle it another way before its report comes: Ctrl+C, End then Ctrl+U, or a prompt typed and sent; then press Enter on another such box. The settled Enter still owes the report it may have caused, so the next unowned report is its and not the new Enter's, until the agent has ended the turn that report would have come in; a prompt sent after the Enter empties the box at once and owes its own report too.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_a_settled_unknown_enter_still_owes_its_report() {
        for settle in [&b"\x03"[..], b"\x05\x15"] {
            let mut pane = Pane::default();
            pane.user(b"\t\r").user(settle);
            assert_eq!(pane.proof(), Empty, "{settle:?}");
            pane.user(b"\x1b[A\r");
            pane.0.prompt_reported(User("first"));
            assert_eq!(pane.proof(), Unknown, "{settle:?}: the settled Enter's");
            pane.0.prompt_reported(User("second"));
            assert_eq!(pane.proof(), Empty, "{settle:?}");
            // Once a turn has ended since, the settled Enter owes nothing.
            let mut pane = Pane::default();
            pane.user(b"\t\r").user(settle);
            pane.0.turn_started();
            pane.0.turn_ended();
            pane.user(b"\x1b[A\r");
            pane.0.prompt_reported(User("recalled"));
            assert_eq!(pane.proof(), Empty, "{settle:?}: expired");
        }
        // Inside a busy turn it waits for the turn after.
        let mut pane = Pane::default();
        pane.0.turn_started();
        pane.user(b"\t\r\x03");
        pane.0.turn_ended();
        pane.user(b"\x1b[A\r");
        pane.0.prompt_reported(User("queued"));
        assert_eq!(pane.proof(), Unknown);
        // Text sent after the Enter: the whole box went.
        let mut pane = Pane::default();
        pane.user(b"\t\rab\r");
        assert_eq!(pane.proof(), Empty);
        pane.user(b"\x1b[A\r");
        pane.0.prompt_reported(User("tabbed ab"));
        assert_eq!(pane.proof(), Unknown, "the box sent with ab");
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.proof(), Empty);
    }

    /// Scenario: After an Enter on a box the model lost track of, type and edit with keys that reach past what was typed since: Left, Backspace, Home or Ctrl+U at its start, Right, Delete or End at its end; and the reviewer's composed case, a single-line box that is really empty but unknown, then Enter, a space, Ctrl+J, End, Ctrl+U, Backspace and Ctrl+A, which leaves one space at the start. Each such key gives the Enter up and the box is unknown, the line typed after the Enter is still counted, so no report or End then Ctrl+U on one line forgets it; genuinely erasing the space then proves the box empty.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_keys_past_what_followed_an_unknown_enter_give_it_up() {
        for keys in [
            &b"\x1b[D"[..],
            b"\x7f",
            b"\x01",
            b"\x15",
            b"\x1b[C",
            b"\x1b[3~",
            b"\x05",
        ] {
            let mut bytes = b"\t\rab".to_vec();
            if matches!(keys, b"\x1b[D" | b"\x7f") {
                bytes.extend_from_slice(b"\x1b[D\x1b[D");
            }
            bytes.extend_from_slice(keys);
            let mut pane = Pane::default();
            pane.user(&bytes);
            assert_eq!(pane.proof(), Unknown, "{keys:?}");
            pane.0.prompt_reported(User("tabbed"));
            assert_eq!(
                pane.proof(),
                Unknown,
                "{keys:?}: given up, a report proves nothing"
            );
        }
        // Within what was typed after it, the Enter waits.
        let mut pane = Pane::default();
        pane.user(b"\t\rab\x1b[D\x1b[C\x7f\x7f");
        pane.0.prompt_reported(User("tabbed"));
        assert_eq!(pane.proof(), Empty);
        // Giving up counts the lines the earlier box may have held.
        assert_eq!(after(b"\x1b[A\rab\x05\x15\x7f"), Unknown);
        // End then Ctrl+U on the one line typed after it empties the box.
        assert_eq!(after(b"\t\rab\x05\x15"), Empty);
        assert_eq!(after(b"\t\r\x05\x15"), Empty);
        // The reviewer's case (F2-U2-01).
        let mut bytes = "\u{e9}".as_bytes().to_vec();
        bytes.extend_from_slice(b"\x7f\r \n\x05\x15");
        let mut pane = Pane::default();
        pane.user(&bytes);
        assert_eq!(pane.proof(), Empty, "one line emptied, the other on screen");
        pane.user(b"\x7f");
        assert_eq!(pane.proof(), Unknown, "joined back: the space may be there");
        pane.user(b"\x01");
        assert_eq!(pane.proof(), Unknown);
        pane.0.turn_started();
        pane.0.turn_ended();
        assert_eq!(pane.proof(), Unknown);
        pane.user(b"\x05\x15");
        assert_eq!(pane.proof(), Empty, "the space genuinely erased");
    }

    /// Scenario: Send a pane the start of an escape sequence and stop, or a sequence that is not one the model follows: a lone ESC, ESC [, a CSI with parameters, ESC O, a UTF-8 lead byte, an X10 mouse prefix, an OSC or DCS that does not open with a number (Alt+] then typing), a reply that never ends, a reply with a control key or an Enter inside it. A lone ESC swallows nothing and keeps the box empty; every other unfinished or unrecognised sequence leaves the box unknown, and End then Ctrl+U still recovers it.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_an_unfinished_sequence_is_unknown() {
        assert_eq!(after(b"\x1b"), Empty);
        assert_eq!(after(b"  \x1b"), Draft);
        for start in [
            &b"\x1b["[..],
            b"\x1b[1;",
            b"\x1bO",
            b"\xc3",
            b"\x1b[M ",
            b"\x1b]",
            b"\x1b]11;rgb:0",
            b"\x1bP",
            b"\x1bP1$r0",
        ] {
            assert_eq!(after(start), Unknown, "{start:?}");
        }
        for typed in [
            &b"\x1b]abc"[..],
            b"\x1bPabc",
            b"\x1b]11;rgb\x15",
            b"\x1b]11;rgb\x1bx",
        ] {
            assert_eq!(after(typed), Unknown, "{typed:?}");
            let mut cleared = typed.to_vec();
            cleared.extend_from_slice(b"\x05\x15");
            assert_eq!(after(&cleared), Empty, "{typed:?} then End, Ctrl+U");
        }
        // The key that showed it was no reply is read as a key.
        assert_eq!(after(b"\x1b]11;rgb\x05\x15"), Empty);
        // Over-long, it is no reply.
        let mut long = b"\x1b]11;".to_vec();
        long.extend(std::iter::repeat_n(b'a', 600));
        assert_eq!(after(&long), Unknown);
        long.extend_from_slice(b"\x07\x05\x15");
        assert_eq!(after(&long), Empty);
        // An Enter inside one is an Enter on a box the model does not know.
        let mut pane = Pane::default();
        pane.user(b"\x1b]11;rgb\r");
        assert_eq!(pane.proof(), Unknown);
        pane.0.prompt_reported(User("whatever was typed"));
        assert_eq!(pane.proof(), Empty);
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
