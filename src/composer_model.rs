//! PRD #1666 part 3, round 5-wake (R6-4), Q32: what an agent's input box
//! holds, as far as the bytes written into it say.
//!
//! The doorbell types only into an empty composer, and the screen cannot
//! always prove one. Claude Code 2.1.310 draws nothing for a space typed into
//! its input box; it only moves the cursor (measured on a real session,
//! 2026-10-26). So spaces with the cursor moved back to the start, or a
//! spaces-only line with the cursor on another one, draw exactly what the
//! empty composer draws. Codex draws them the same way, without its
//! placeholder. And the placeholder, the one drawing an empty box makes, is
//! not drawn after a delivered task, so it cannot be the proof either.
//!
//! [`ComposerModel`] follows the box from the bytes the pane's PTY took, in
//! the order it took them, the deck's own included: the same stream the draft
//! bit is fed (`crate::agent_pty`'s `PaneInputStream`). It follows them once
//! per measured agent ([`Semantics`]), since the agents edit differently, and
//! the doorbell asks the one its screen profile was measured on
//! (`crate::doorbell_screen::ScreenProfile::input`). Each knows the text and
//! the cursor while every key since the last proof of empty is one it
//! follows below, as measured on that agent, and nothing otherwise
//! ([`InputProof::Unknown`]): a key it does not follow is never guessed at.
//! The doorbell asks it alongside the screen (`crate::doorbell_txn::Frame`):
//! a blank composer is empty only when the bytes say so too. Measured on
//! Claude Code 2.1.310 and Codex 0.160.0 and 0.164.0 (2026-10-26), which edit
//! alike except at a line's edge and for `Enter` on blanks; the tests replay
//! every measured sequence per agent, with what the real box held as the
//! oracle.
//!
//! **Followed**, as measured: printable ASCII typed, or pasted in a paste of
//! at most [`PASTE_FOLLOWED_MAX`] characters and three lines (inserted at the
//! cursor; a space too); `Ctrl+J` and a newline inside such a paste (a line
//! break); `Backspace` (deletes before the cursor, joining lines at a line's
//! start); `Delete`, `ESC[3~` (deletes at the cursor, joining the next line
//! at a line break, nothing at the box's end); `Left` and `Right`, `ESC[D`,
//! `ESC[C` and their `ESC O` forms (one character, across line breaks);
//! `Ctrl+A` and the Home key, `ESC[H`, `ESC[1~`, `ESC O H` (start of line);
//! `Ctrl+E` and the End key, `ESC[F`, `ESC[4~`, `ESC O F` (end of line).
//! Pressed where the cursor already is at a line's start or end, Claude
//! Code's Home and End keys move on to the line above's start or the line
//! below's end, and its `Ctrl+A` and `Ctrl+E` stay; Codex's `Ctrl+A` and
//! `Ctrl+E` move on, and its Home and End keys stay. `Ctrl+U` deletes from
//! the start of the line to the cursor; at the start of a later line it
//! deletes the line break, joining the line to the one above; at the start
//! of the first line, nothing. `Ctrl+C` while no turn is under way empties
//! the whole box, whatever it held. `Enter` on text that has something other
//! than blanks in it submits it, which empties the box; on a box of blanks
//! (spaces and line breaks) it submits nothing and keeps them, with the
//! cursor where it was on Claude Code and at the start of the box on Codex.
//! Terminal reports (mouse, focus, cursor position, a numbered OSC or DCS
//! reply and the like) and the paste markers edit nothing.
//!
//! **Not followed**, so the box becomes [`InputProof::Unknown`]: `Up` and
//! `Down` (history, or a move between lines whose column the model does not
//! track), `Tab` (completion on Claude Code, a submit on Codex), non-ASCII
//! text, any other control key, an `Alt` or modified key, `Shift+Enter`,
//! `Alt+Enter`, an `Enter` after a backslash, a longer paste or anything
//! else inside a paste, `Ctrl+C` during a turn with something in the box (it
//! may only interrupt the turn), and an escape sequence that is not one of
//! the above. `Tab`, `Alt+Enter` and a keyboard-protocol key may also have
//! submitted the box, so a report they caused is theirs, not a later
//! `Enter`'s. While a sequence is still arriving
//! the box is unknown too, except after a lone `ESC`, which swallows
//! nothing: the next byte reads it as `Alt`. An `Enter` whose box may have
//! had a menu open on it (text with an `@`, which offers files to complete,
//! or a box of blanks after a leading `!`, `#`, `&`, `/` or `?`) is an
//! `Enter` on a box the model does not know: it may have completed or
//! changed mode instead of submitting.
//!
//! **The ways back.** From an unknown box, three things make it known again:
//! `Ctrl+C` while no turn is under way, which empties it whatever it held
//! (measured on both agents over text, blanks and line breaks); `End` then
//! `Ctrl+U` with nothing between them on a box known to be one line, which
//! empties that line (on a box that may have more, it does not: on an empty
//! line it joins the line above instead, with the cursor after that line's
//! text; on Claude Code it then proves only the cursor's line empty,
//! [`InputProof::LineEmpty`], which a composer drawn as one blank row with
//! the cursor at its start completes); and an `Enter` the agent's own
//! prompt hook then reports as a prompt it took
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
//! and if it did not, what the box held is still there around it (after it
//! on Codex, whose cursor went to the start). So a key that would reach past
//! what was typed after the `Enter` — `Left`, `Backspace`, `Home` or `Ctrl+U`
//! at its start, `Right`, `Delete` or `End` at its end, an `Enter` at the
//! start of text typed after it — gives up on the `Enter`: the box is
//! unknown again, its lines counted from both. `Enter` on text typed after it
//! submits the whole box, whatever the earlier `Enter` left; a second `Enter`
//! with nothing typed since awaits the same report, since if either
//! submitted the box is empty.
//!
//! **What it cannot see**, and accepts: text the agent puts into its own box
//! (a restored prompt) is invisible to it; a key that reaches a dialog or
//! menu rather than the box, other than the `Enter`s above, is read as an
//! edit of the box; and a prompt the agent submits by itself, not from its
//! box, is read as a prompt it took from the box. The first can make the box
//! look empty when it holds text the agent added, which the screen then
//! shows unless it is blanks. The second mostly makes the box look fuller
//! than it is, but an `Enter` on text, or a `Ctrl+C`, that a dialog took
//! instead reads as emptying the box. The third needs an `Enter` on a box of
//! blanks the model did not know, still owed its report when the agent's own
//! prompt arrives; so does a report of an earlier submit whose text does not
//! carry what the model followed, should the agent rewrite a prompt before
//! its hook sees it. In each, a box of blanks can read as empty. The
//! doorbell's other checks (a Clean ready frame, no dialog) still apply on
//! top. The other way round, a submit that never reports (a command the
//! agent handles without its prompt hook), or an earlier report its own
//! `Enter` was not recorded in time for, can leave an `Enter` unconfirmed:
//! the box stays unknown, held as a possible draft, until `Ctrl+C`, or a
//! prompt typed and sent.

use std::collections::VecDeque;

/// What the bytes say about the box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputProof {
    /// The box is empty.
    #[default]
    Empty,
    /// The box holds something: text, blanks or a line break.
    Draft,
    /// The line the cursor is on is empty and the cursor at its start, and
    /// other lines may hold anything: on Claude Code, after `End` then
    /// `Ctrl+U` on a box the model did not know (Q32). Claude Code draws
    /// every line as a row, so a composer drawn as one blank row with the
    /// cursor at its start, and nothing hidden, is then empty.
    LineEmpty,
    /// A key the model does not follow reached the box since it was last
    /// known: only the screen's own proof of empty (the agent's
    /// placeholder) can say it is empty.
    Unknown,
}

impl InputProof {
    /// What two followings of one box say together: it holds something if
    /// either says so, and it is empty only if both say so.
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Draft, _) | (_, Self::Draft) => Self::Draft,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Empty, Self::Empty) => Self::Empty,
            _ => Self::LineEmpty,
        }
    }
}

/// How an agent's input box edits, as measured on its releases. See the
/// module documentation for what each follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantics {
    /// Claude Code 2.1.310.
    ClaudeCode,
    /// Codex 0.160.0 and 0.164.0.
    Codex,
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

/// The most characters of one paste followed. Both agents put a long paste
/// into the box as one placeholder that a single key deletes, so past this
/// the box is [`InputProof::Unknown`]. Measured on 2026-10-26: pastes of 256
/// and 800 characters went in as text on all three releases, one of 1,200
/// became a placeholder.
pub(crate) const PASTE_FOLLOWED_MAX: usize = 256;

/// The most line breaks of one paste followed: Claude Code 2.1.310 put a
/// paste of three lines in as text and one of six as a placeholder
/// (measured on 2026-10-26).
const PASTE_BREAKS_FOLLOWED: usize = 2;

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
    /// in it. `others`: lines before and after `text` may exist, which a key
    /// reaching past `text` would meet ([`InputProof::LineEmpty`]).
    Known {
        text: Vec<char>,
        cursor: usize,
        others: bool,
    },
    /// Not known; `multiline` when it may hold more than one line.
    Unknown { multiline: bool },
}

impl Default for Content {
    fn default() -> Self {
        Self::Known {
            text: Vec::new(),
            cursor: 0,
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
    Home(Edge),
    End(Edge),
    KillBefore,
    ClearAll,
    Submit,
    /// A key the model does not follow; `newline` when it may have broken a
    /// line.
    Unfollowed {
        newline: bool,
    },
    /// A key the model does not follow that may have submitted the box, so
    /// its report may come: `Tab`, which submits on Codex (measured),
    /// `Alt+Enter`, and a keyboard-protocol key.
    MaybeSubmit,
}

/// How a start-of-line or end-of-line key came: the agents move to the
/// adjacent line for one when the cursor is already at the line's start or
/// end, and stay for the other (measured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// `Ctrl+A`, `Ctrl+E`.
    Ctrl,
    /// The `Home` and `End` keys: `ESC[H`, `ESC[1~`, `ESC O H`, `ESC[F`,
    /// `ESC[4~`, `ESC O F`.
    Key,
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

/// One pane's input box, followed from its PTY input once per measured
/// agent. See the module documentation.
#[derive(Debug)]
pub(crate) struct ComposerModel {
    claude: Model,
    codex: Model,
}

impl Default for ComposerModel {
    fn default() -> Self {
        Self {
            claude: Model::new(Semantics::ClaudeCode),
            codex: Model::new(Semantics::Codex),
        }
    }
}

impl ComposerModel {
    fn models(&mut self) -> [&mut Model; 2] {
        [&mut self.claude, &mut self.codex]
    }

    fn model(&self, semantics: Semantics) -> &Model {
        match semantics {
            Semantics::ClaudeCode => &self.claude,
            Semantics::Codex => &self.codex,
        }
    }

    /// What the bytes so far say about the box, edited as `semantics` edits;
    /// for a pane with no measured agent, what every measured agent's
    /// following says together ([`InputProof::join`]).
    pub(crate) fn proof(&self, semantics: Option<Semantics>) -> InputProof {
        match semantics {
            Some(semantics) => self.model(semantics).proof(),
            None => self.claude.proof().join(self.codex.proof()),
        }
    }

    /// What the box holds as `semantics` edits it, when known: its text,
    /// with `\n` for a line break.
    #[cfg(test)]
    pub(crate) fn text(&self, semantics: Semantics) -> Option<String> {
        match &self.model(semantics).content {
            Content::Known { text, .. } => Some(text.iter().collect()),
            Content::Unknown { .. } => None,
        }
    }

    /// The pane's own agent reported a turn starting (or going on).
    pub(crate) fn turn_started(&mut self) {
        for model in self.models() {
            model.busy = true;
        }
    }

    /// The pane's own agent reported Idle: a turn it reported starting has
    /// ended, and a given-up `Enter` whose report would have come by now
    /// owes nothing.
    pub(crate) fn turn_ended(&mut self) {
        for model in self.models() {
            model.turn_ended();
        }
    }

    /// The pane's own agent reported, from its prompt hook, a prompt it
    /// took. See [`Model::prompt_reported`].
    pub(crate) fn prompt_reported(&mut self, report: PromptReport<'_>) {
        for model in self.models() {
            model.prompt_reported(report);
        }
    }

    /// The doorbell's own line was proven submitted with no human operation
    /// since its admission (plan §1.4): the agent drew it alone in its
    /// composer and took it, so the box is empty now.
    pub(crate) fn reconcile_deck_submit(&mut self) {
        for model in self.models() {
            model.reconcile_deck_submit();
        }
    }

    /// Feed one byte the PTY took. `submits` and `in_paste` are the caller's
    /// stream state (paste framing, the keypress behind the byte), as the
    /// draft bit is fed; `deck` when the deck wrote it.
    pub(crate) fn feed_byte(&mut self, byte: u8, submits: bool, in_paste: bool, deck: bool) {
        for model in self.models() {
            model.feed_byte(byte, submits, in_paste, deck);
        }
    }
}

/// The box as one agent edits it.
#[derive(Debug)]
struct Model {
    semantics: Semantics,
    content: Content,
    /// The last key was `End`, so a `Ctrl+U` now empties the current line
    /// wherever the cursor was, or joins it to the line above if it was
    /// empty.
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
    /// The characters, and the line breaks among them, of the current paste
    /// so far.
    pasted: usize,
    paste_breaks: usize,
}

impl Model {
    fn new(semantics: Semantics) -> Self {
        Self {
            semantics,
            content: Content::default(),
            at_line_end: false,
            unconfirmed: None,
            owed: VecDeque::new(),
            busy: false,
            turns: 0,
            decode: Decode::default(),
            pasted: 0,
            paste_breaks: 0,
        }
    }

    /// What the bytes so far say about the box.
    fn proof(&self) -> InputProof {
        match &self.content {
            Content::Known { text, .. } if !text.is_empty() => InputProof::Draft,
            // A sequence still arriving may be swallowing keys.
            _ if !matches!(self.decode, Decode::Ground | Decode::Esc { paste: false }) => {
                InputProof::Unknown
            }
            Content::Known { others: true, .. } if self.unconfirmed.is_none() => {
                InputProof::LineEmpty
            }
            Content::Known { .. } if self.unconfirmed.is_none() => InputProof::Empty,
            Content::Known { .. } | Content::Unknown { .. } => InputProof::Unknown,
        }
    }

    /// See [`ComposerModel::turn_ended`].
    fn turn_ended(&mut self) {
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
    fn prompt_reported(&mut self, report: PromptReport<'_>) {
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

    /// See [`ComposerModel::reconcile_deck_submit`].
    fn reconcile_deck_submit(&mut self) {
        self.settle();
        self.content = Content::default();
        self.at_line_end = false;
    }

    /// See [`ComposerModel::feed_byte`].
    fn feed_byte(&mut self, byte: u8, submits: bool, in_paste: bool, deck: bool) {
        if !in_paste {
            self.pasted = 0;
            self.paste_breaks = 0;
        }
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
                // `Alt+Enter`.
                b'\r' => Some(Key::MaybeSubmit),
                // `Alt+<key>`, `Alt+Backspace`.
                _ => Some(Key::Unfollowed { newline: false }),
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
                b'H' => Some(Key::Home(Edge::Key)),
                b'F' => Some(Key::End(Edge::Key)),
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
                // A long paste may be folded into one placeholder.
                b'\r' | b'\n' | 0x20..=0x7e if self.pasted == PASTE_FOLLOWED_MAX => {
                    Some(Key::Unfollowed { newline: true })
                }
                b'\r' | b'\n' if self.paste_breaks == PASTE_BREAKS_FOLLOWED => {
                    Some(Key::Unfollowed { newline: true })
                }
                b'\r' | b'\n' => {
                    self.pasted += 1;
                    self.paste_breaks += 1;
                    Some(Key::Newline)
                }
                0x20..=0x7e => {
                    self.pasted += 1;
                    Some(Key::Insert(char::from(byte)))
                }
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
            0x01 => Some(Key::Home(Edge::Ctrl)),
            0x05 => Some(Key::End(Edge::Ctrl)),
            0x15 => Some(Key::KillBefore),
            0x03 => Some(Key::ClearAll),
            0xc0..=0xf7 => {
                self.decode = Decode::Utf8 {
                    remaining: utf8_continuations(byte),
                    paste: false,
                };
                None
            }
            // `Tab` submits the box on Codex (measured), and completes on
            // Claude Code.
            b'\t' => Some(Key::MaybeSubmit),
            // A CR that does not submit follows an ESC (`Alt+Enter`), which
            // the ESC arm reads; any other control key is not followed.
            _ => Some(Key::Unfollowed {
                newline: byte == b'\r',
            }),
        }
    }

    fn apply(&mut self, key: Key, deck: bool) {
        if key == Key::MaybeSubmit {
            // It may also break a line; whatever the box was, it is not
            // known now, and a report it caused belongs to it.
            self.apply(Key::Unfollowed { newline: true }, deck);
            self.owe(Owed::Maybe {
                expires: self.report_by(),
            });
            return;
        }
        let after_end = std::mem::replace(&mut self.at_line_end, matches!(key, Key::End(_)));
        if key == Key::ClearAll {
            if self.busy && self.proof() != InputProof::Empty {
                // During a turn it may interrupt the turn and leave the box
                // as it was (not measured).
                self.content = Content::Unknown {
                    multiline: self.content.multiline(),
                };
                self.give_up();
                return;
            }
            self.settle();
            self.content = Content::default();
            return;
        }
        if self.unconfirmed.is_some() && self.reaches_past(key) {
            self.give_up();
        }
        let content = std::mem::take(&mut self.content);
        self.content = match content {
            // What the other lines hold decides what Enter does.
            Content::Known { others: true, .. } if key == Key::Submit => {
                self.await_report(true);
                Content::default()
            }
            Content::Known {
                text,
                cursor,
                others,
            } if key == Key::Submit && !others => self.submit(text, cursor, deck),
            Content::Known {
                text,
                cursor,
                others,
            } => known(self.semantics, text, cursor, others, key),
            Content::Unknown { multiline } => match key {
                // Whatever the box held went with it, unless it was only
                // blanks: confirmed by the agent's report of it.
                Key::Submit => {
                    self.await_report(multiline);
                    Content::default()
                }
                // One line: `End` reached its end and `Ctrl+U` emptied it.
                // On more, an empty line would join the one above instead,
                // and the cursor would end after that line's text: on Claude
                // Code the screen tells the two apart.
                Key::KillBefore if after_end && !multiline => Content::default(),
                Key::KillBefore if after_end && self.semantics == Semantics::ClaudeCode => {
                    Content::Known {
                        text: Vec::new(),
                        cursor: 0,
                        others: true,
                    }
                }
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
    fn submit(&mut self, text: Vec<char>, cursor: usize, deck: bool) -> Content {
        let multiline = text.contains(&'\n');
        // A backslash before the cursor makes Enter a line break on Claude
        // Code; Codex submits it.
        if cursor > 0 && text[cursor - 1] == '\\' {
            return Content::Unknown { multiline: true };
        }
        // Blanks alone are kept (measured): Claude Code's cursor stays,
        // Codex's goes to the start of the box, wherever it was. After an
        // unconfirmed Enter that kept blanks, Codex's cursor went to their
        // start already, so what was typed since is at the box's start.
        if text.iter().all(|c| c.is_whitespace()) {
            let cursor = match self.semantics {
                Semantics::ClaudeCode => cursor,
                Semantics::Codex => 0,
            };
            return Content::Known {
                text,
                cursor,
                others: false,
            };
        }
        // A menu may have taken it: a file to complete, or a mode a leading
        // character switched to, with nothing but blanks to submit.
        let mode = text
            .first()
            .is_some_and(|c| matches!(c, '!' | '#' | '&' | '/' | '?'))
            && text[1..].iter().all(|c| c.is_whitespace());
        if mode || text.contains(&'@') {
            self.await_report(multiline);
            return Content::default();
        }
        // The whole box went, whatever an earlier Enter left in it.
        self.settle();
        self.owe(Owed::Prompt {
            text: text.into_iter().collect(),
            deck,
        });
        Content::default()
    }

    /// An `Enter` on a box the model does not know: if it submitted, the box
    /// is what is typed after it; if not, what it held is still there. An
    /// earlier one still awaiting its report is settled, and its lines kept.
    fn await_report(&mut self, multiline: bool) {
        let multiline = multiline || self.unconfirmed.is_some_and(|u| u.multiline);
        self.settle();
        self.unconfirmed = Some(Unconfirmed {
            multiline,
            expires: self.report_by(),
        });
    }

    /// Whether `key`, after an unconfirmed `Enter`, reaches past what was
    /// typed after it, into what that `Enter` may have left.
    fn reaches_past(&self, key: Key) -> bool {
        let Content::Known {
            text, cursor: at, ..
        } = &self.content
        else {
            return false;
        };
        let at = *at;
        let above = text[..at].iter().filter(|&&c| c == '\n').count();
        let below = text[at..].iter().filter(|&&c| c == '\n').count();
        match key {
            Key::Left | Key::Backspace => at == 0,
            // What is before the cursor decides whether Enter breaks the
            // line. With nothing typed since, this Enter and the earlier one
            // empty the box together if either submits, so they await one
            // report.
            Key::Submit => at == 0 && !text.is_empty(),
            Key::KillBefore => above == 0,
            // From a line's start or end, some move on to the line beyond.
            Key::Home(_) => above <= 1,
            Key::Right | Key::Delete => at == text.len(),
            Key::End(_) => below <= 1,
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

/// Whether `semantics` moves a start-of-line or end-of-line key that came as
/// `edge` on to the adjacent line's start or end when the cursor is already
/// at its own line's (measured): Claude Code does for the `Home` and `End`
/// keys, Codex for `Ctrl+A` and `Ctrl+E`; each keeps the cursor for the
/// other.
fn moves_on(semantics: Semantics, edge: Edge) -> bool {
    matches!(
        (semantics, edge),
        (Semantics::ClaudeCode, Edge::Key) | (Semantics::Codex, Edge::Ctrl)
    )
}

/// The box after `key`, from known `text` and `cursor`, as `semantics`
/// edits it. `Enter` and `Ctrl+C` are [`Model::apply`]'s.
fn known(semantics: Semantics, mut text: Vec<char>, at: usize, others: bool, key: Key) -> Content {
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
    // A key that reaches past `text` meets the other lines. `text` began at
    // the start of an empty line, so its first line starts that line and
    // its last line ends it.
    let first = !text[..at].contains(&'\n');
    let last = !text[at..].contains(&'\n');
    let crosses = match key {
        Key::Backspace | Key::Left | Key::KillBefore => at == 0,
        Key::Delete | Key::Right => at == text.len(),
        Key::Home(edge) => first && at == 0 && moves_on(semantics, edge),
        Key::End(edge) => last && at == text.len() && moves_on(semantics, edge),
        _ => false,
    };
    if others && crosses {
        return unknown(true);
    }
    let at = match key {
        Key::Insert(c) => {
            text.insert(at, c);
            at + 1
        }
        Key::Newline => {
            text.insert(at, '\n');
            at + 1
        }
        Key::Backspace if at == 0 => 0,
        // At a line's start, it joins the line to the one above (measured).
        Key::Backspace => {
            text.remove(at - 1);
            at - 1
        }
        Key::Delete if at == text.len() => at,
        // On a line break, it joins the next line onto this one (measured).
        Key::Delete => {
            text.remove(at);
            at
        }
        Key::Left => at.saturating_sub(1),
        Key::Right => (at + 1).min(text.len()),
        Key::Home(edge) => match line_start(at, &text) {
            start if start < at || at == 0 => start,
            // Already at a later line's start.
            _ if moves_on(semantics, edge) => line_start(at - 1, &text),
            _ => at,
        },
        Key::End(edge) => match line_end(at, &text) {
            end if end > at || at == text.len() => end,
            // Already at an earlier line's end.
            _ if moves_on(semantics, edge) => line_end(at + 1, &text),
            _ => at,
        },
        // At the start of a later line, it deletes the line break, joining
        // the line to the one above; at the first line's start, nothing
        // (measured on both agents).
        Key::KillBefore => match line_start(at, &text) {
            0 if at == 0 => 0,
            start if start == at => {
                text.remove(at - 1);
                at - 1
            }
            start => {
                text.drain(start..at);
                start
            }
        },
        Key::ClearAll | Key::Submit | Key::MaybeSubmit | Key::Unfollowed { .. } => {
            unreachable!("Model::apply handles it")
        }
    };
    Content::Known {
        text,
        cursor: at,
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
        (b"" | b"1", b'H') | (b"1", b'~') => Some(Key::Home(Edge::Key)),
        (b"" | b"1", b'F') | (b"4", b'~') => Some(Key::End(Edge::Key)),
        (b"3", b'~') => Some(Key::Delete),
        // `Up` and `Down` can recall a prompt of several lines.
        (b"", b'A' | b'B') => Some(Key::Unfollowed { newline: true }),
        // `Shift+Enter` (`ESC[13;2u`) and other keyboard-protocol keys may
        // break a line, or submit.
        (_, b'u') => Some(Key::MaybeSubmit),
        _ => Some(Key::Unfollowed { newline: false }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use spec::spec;

    use super::InputProof::{self, Draft, Empty, LineEmpty, Unknown};
    use super::PromptReport::{Deck, User};
    use super::Semantics::{self, ClaudeCode, Codex};
    use crate::agent_pty::DraftTestStream;
    use crate::draft_deferral::ByteOrigin;

    const AGENTS: [Semantics; 2] = [ClaudeCode, Codex];

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

        /// The proof both agents' followings give, which must agree.
        fn proof(&self) -> InputProof {
            let [claude, codex] = AGENTS.map(|agent| self.proof_in(agent));
            assert_eq!(claude, codex, "Claude Code and Codex disagree");
            claude
        }

        fn proof_in(&self, agent: Semantics) -> InputProof {
            self.0.composer().proof(Some(agent))
        }

        fn text_in(&self, agent: Semantics) -> Option<String> {
            self.0.composer().text(agent)
        }
    }

    fn after(bytes: &[u8]) -> InputProof {
        Pane::default().user(bytes).proof()
    }

    /// The proofs as Claude Code and as Codex edit the box.
    fn each(bytes: &[u8]) -> (InputProof, InputProof) {
        let mut pane = Pane::default();
        pane.user(bytes);
        (pane.proof_in(ClaudeCode), pane.proof_in(Codex))
    }

    /// End then Ctrl+U on a box the model did not know, of several lines:
    /// the cursor's line on Claude Code, nothing on Codex.
    const LINE_EMPTY: (InputProof, InputProof) = (LineEmpty, Unknown);

    const H: &str = "\x1b[H";
    const E: &str = "\x1b[F";
    const L: &str = "\x1b[D";
    const R: &str = "\x1b[C";
    const B: &str = "\x7f";
    const D: &str = "\x1b[3~";
    const U: &str = "\x15";
    const J: &str = "\n";
    const CR: &str = "\r";

    /// The tester's compound sequences (`tester-p3-keyseq-report.md`,
    /// 2026-10-26), each from an empty box, and what each real agent's box
    /// held afterwards, revealed with markers at both ends and at every
    /// line's end: Claude Code 2.1.310, and Codex 0.164.0 and 0.160.0, which
    /// held the same in every row.
    const MEASURED: &[(&str, &[&str], &str, &str)] = &[
        (
            "reviewer",
            &["ab", J, "  ", H, U, H, B, D, D, U],
            "  ",
            "  ",
        ),
        (
            "join_text_bs_delete",
            &["ab", J, "cd", H, U, B, D],
            "ad",
            "ad",
        ),
        (
            "join_blanks_bs_delete",
            &["  ", J, "  ", H, U, B, D],
            "  ",
            "  ",
        ),
        ("join_text_then_clear", &["ab", J, "cd", H, U, E, U], "", ""),
        (
            "join_blanks_then_clear",
            &["  ", J, "  ", H, U, E, U],
            "",
            "",
        ),
        (
            "home_end_left_right_kill",
            &["ab", J, "cd", H, L, H, E, R, E, U],
            "ab\n",
            "ab\n",
        ),
        (
            "ctrl_a_e_left_right_kill",
            &["ab", J, "cd", "\x01", L, "\x01", "\x05", R, "\x05", U],
            "ab\n",
            "ab\n",
        ),
        (
            "three_lines_join_kill",
            &["ab", J, "  ", J, "cd", H, L, H, U, E, U],
            "\ncd",
            "\ncd",
        ),
        (
            "blanks_partial_deletes",
            &["  ", J, "  ", H, L, B, R, D],
            " \n ",
            " \n ",
        ),
        (
            "blanks_home_kill_partial",
            &["  ", J, "  ", H, U, H, D, U],
            "   ",
            "   ",
        ),
        ("linebreaks_partial_delete", &[J, J, H, U, H, D, U], "", ""),
        ("blanks_enter_type_kill", &["   ", CR, "x", U], "", "   "),
        (
            "breaks_enter_type_kill",
            &[J, J, CR, "x", U],
            "\n\n",
            "\n\n",
        ),
        (
            "mixed_blanks_enter_type_kill",
            &["  ", J, "  ", CR, "x", U],
            "  \n",
            "  \n  ",
        ),
        ("blank_space_home", &["   ", H], "   ", "   "),
        ("blank_break_home", &[J, H], "\n", "\n"),
        ("space_delete_to_empty", &[" ", H, D], "", ""),
        ("join_only_break", &[J, U], "", ""),
        ("text_two_kills", &["ab", J, "cd", U, U], "ab", "ab"),
    ];

    const A: &str = "\x01";
    const V: &str = "\x05";
    const X: &str = "x";

    /// Line-boundary probes (2026-10-26, `runs/probe-*` and `runs/probe2-*`
    /// beside the tester's), each from an empty box and ending with an `x`
    /// typed where the cursor was, and what each real agent's box held:
    /// Claude Code 2.1.310, and Codex 0.164.0 and 0.160.0, which held the
    /// same in every row.
    const MEASURED_EDGES: &[(&str, &[&str], &str, &str)] = &[
        (
            "end_on_empty_first_line",
            &[J, J, L, L, E, X],
            "\nx\n",
            "x\n\n",
        ),
        (
            "kill_on_empty_first_line",
            &[J, J, L, L, U, X],
            "x\n\n",
            "x\n\n",
        ),
        (
            "end_kill_on_empty_first_line",
            &[J, J, L, L, E, U, X],
            "x\n",
            "x\n\n",
        ),
        (
            "end_twice_at_line_end",
            &["ab", J, "cd", L, L, L, E, E, X],
            "ab\ncdx",
            "abx\ncd",
        ),
        (
            "ctrl_e_twice_at_line_end",
            &["ab", J, "cd", L, L, L, V, V, X],
            "abx\ncd",
            "ab\ncdx",
        ),
        (
            "home_twice_at_line_start",
            &["ab", J, "cd", H, H, X],
            "xab\ncd",
            "ab\nxcd",
        ),
        (
            "ctrl_a_twice_at_line_start",
            &["ab", J, "cd", A, A, X],
            "ab\nxcd",
            "xab\ncd",
        ),
        (
            "home_on_empty_last_line",
            &["ab", J, H, X],
            "xab\n",
            "ab\nx",
        ),
        (
            "end_on_empty_middle_line",
            &["ab", J, J, "cd", H, L, E, X],
            "ab\n\ncdx",
            "ab\nx\ncd",
        ),
        (
            "kill_at_box_start_text_below",
            &["ab", J, "cd", H, L, L, L, U, X],
            "xab\ncd",
            "xab\ncd",
        ),
        (
            "kill_on_empty_first_line_text_below",
            &[J, "cd", H, L, U, X],
            "x\ncd",
            "x\ncd",
        ),
        (
            "delete_on_line_break",
            &["ab", J, "cd", H, L, D, X],
            "abxcd",
            "abxcd",
        ),
        (
            "end_kill_last_empty_line",
            &["ab", J, E, U, X],
            "abx",
            "abx",
        ),
        (
            "end_kill_middle_empty_line",
            &["ab", J, J, "cd", H, L, E, U, X],
            "ab\n\nx",
            "abx\ncd",
        ),
        (
            "blank_enter_cursor_inside",
            &["   ", L, CR, X],
            "  x ",
            "x   ",
        ),
        (
            "blank_enter_cursor_inside_multi",
            &["  ", J, "  ", H, CR, X],
            "  \nx  ",
            "x  \n  ",
        ),
        (
            "right_at_line_end_then_x",
            &["ab", J, "cd", H, L, R, X],
            "ab\nxcd",
            "ab\nxcd",
        ),
        (
            "left_at_line_start_then_x",
            &["ab", J, "cd", H, L, X],
            "abx\ncd",
            "abx\ncd",
        ),
        (
            "home_twice_third_line",
            &["ab", J, "cd", J, "ef", H, H, X],
            "ab\nxcd\nef",
            "ab\ncd\nxef",
        ),
        (
            "ctrl_a_twice_third_line",
            &["ab", J, "cd", J, "ef", A, A, X],
            "ab\ncd\nxef",
            "ab\nxcd\nef",
        ),
        (
            "end_twice_first_of_three",
            &["ab", J, "cd", J, "ef", L, L, L, L, L, L, E, E, X],
            "ab\ncd\nefx",
            "abx\ncd\nef",
        ),
        (
            "ctrl_e_twice_first_of_three",
            &["ab", J, "cd", J, "ef", L, L, L, L, L, L, V, V, X],
            "abx\ncd\nef",
            "ab\ncd\nefx",
        ),
        (
            "home_1tilde_twice",
            &["ab", J, "cd", "\x1b[1~", "\x1b[1~", X],
            "xab\ncd",
            "ab\nxcd",
        ),
        (
            "home_ss3_twice",
            &["ab", J, "cd", "\x1bOH", "\x1bOH", X],
            "xab\ncd",
            "ab\nxcd",
        ),
        (
            "end_4tilde_twice",
            &["ab", J, "cd", L, L, L, "\x1b[4~", "\x1b[4~", X],
            "ab\ncdx",
            "abx\ncd",
        ),
        (
            "end_ss3_twice",
            &["ab", J, "cd", L, L, L, "\x1bOF", "\x1bOF", X],
            "ab\ncdx",
            "abx\ncd",
        ),
        ("ctrl_a_on_first_line_start", &["ab", A, A, X], "xab", "xab"),
        (
            "ctrl_e_on_last_line_end",
            &["ab", J, "cd", V, V, X],
            "ab\ncdx",
            "ab\ncdx",
        ),
        (
            "home_on_first_line_start",
            &["ab", J, "cd", L, L, L, H, H, X],
            "xab\ncd",
            "xab\ncd",
        ),
        (
            "end_on_last_line_end",
            &["ab", J, "cd", E, E, X],
            "ab\ncdx",
            "ab\ncdx",
        ),
    ];

    /// The remedy measurements (2026-10-26, `runs/remedy-*`): each box, then
    /// `End` and `Ctrl+U`, and what each real agent's box held. A letter and
    /// `Ctrl+C` emptied every one of them on all three, and `Ctrl+C` alone
    /// every one that held anything; on an empty box Claude Code asked for a
    /// second `Ctrl+C` to exit, and Codex exited.
    const MEASURED_END_KILL: &[(&str, &[&str], &str, &str)] = &[
        ("text_multi_line2start", &["ab", J, "cd", H], "ab\n", "ab\n"),
        (
            "text_multi_line1end",
            &["ab", J, "cd", L, L, L],
            "ab\n",
            "\ncd",
        ),
        ("blanks_one", &["   "], "", ""),
        ("blanks_one_home", &["   ", H], "", ""),
        ("blanks_multi_home", &["  ", J, "  ", H], "  \n", "  \n"),
        ("breaks_only", &[J, J], "\n", "\n"),
        ("breaks_only_start", &[J, J, L, L], "\n", "\n\n"),
        ("blanks_entered", &["   ", CR], "", ""),
        ("mixed_entered", &["  ", J, "  ", CR], "  \n", "\n  "),
        ("empty", &[], "", ""),
    ];

    /// Scenario: Replay the line-boundary probes and the End-then-Ctrl+U remedy cases measured on real Claude Code 2.1.310 and real Codex 0.164.0 and 0.160.0 into a pane's input stream: Home, End, Ctrl+A and Ctrl+E pressed again at a line's start or end, in every encoding, Ctrl+U and Delete at line edges, Enter on blanks with the cursor inside them, and End then Ctrl+U on boxes of one or several lines. For every case and agent the model holds exactly what the real box held (Claude Code's Home and End keys, and Codex's Ctrl+A and Ctrl+E, move on to the adjacent line); End then Ctrl+U empties only a box of one line, and a letter then Ctrl+C empties every box.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_measured_line_edges_and_remedies_per_agent() {
        let mut rows: Vec<(String, Vec<u8>, &str, &str)> = MEASURED_EDGES
            .iter()
            .map(|(case, keys, claude, codex)| {
                (
                    case.to_string(),
                    keys.concat().into_bytes(),
                    *claude,
                    *codex,
                )
            })
            .collect();
        for (case, keys, claude, codex) in MEASURED_END_KILL {
            let mut bytes = keys.concat().into_bytes();
            bytes.extend_from_slice(b"\x1b[F\x15");
            rows.push((format!("{case} then End, Ctrl+U"), bytes, *claude, *codex));
            let mut bytes = keys.concat().into_bytes();
            bytes.extend_from_slice(b"x\x03");
            rows.push((format!("{case} then a letter, Ctrl+C"), bytes, "", ""));
        }
        for (case, bytes, claude, codex) in rows {
            let mut pane = Pane::default();
            pane.user(&bytes);
            for (agent, held) in [(ClaudeCode, claude), (Codex, codex)] {
                let what = format!("{case} on {agent:?}");
                assert_eq!(pane.text_in(agent).as_deref(), Some(held), "{what}");
                let proof = if held.is_empty() { Empty } else { Draft };
                assert_eq!(pane.proof_in(agent), proof, "{what}");
            }
        }
    }

    fn measured_case(case: &str) -> (Vec<u8>, &'static str, &'static str) {
        let (_, keys, claude, codex) = MEASURED
            .iter()
            .find(|(name, ..)| *name == case)
            .expect("a measured case");
        (keys.concat().into_bytes(), claude, codex)
    }

    /// Scenario: Replay each of the 19 compound key sequences measured on real Claude Code 2.1.310 and real Codex 0.164.0 and 0.160.0 into a pane's input stream, and read the box as each agent edits it. For every sequence and agent, the model knows exactly what the real box held (lines joined by Ctrl+U at a line's start, Codex's blanks kept after its Enter), says Empty only where the real box was empty, and Draft wherever it held anything, so it is never emptier than the box.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_measured_compound_sequences_per_agent() {
        for (case, keys, claude, codex) in MEASURED {
            let bytes = keys.concat().into_bytes();
            let mut pane = Pane::default();
            pane.user(&bytes);
            for (agent, held) in [(ClaudeCode, claude), (Codex, codex)] {
                let what = format!("{case} on {agent:?}");
                assert_eq!(pane.text_in(agent).as_deref(), Some(*held), "{what}");
                let proof = if held.is_empty() { Empty } else { Draft };
                assert_eq!(pane.proof_in(agent), proof, "{what}");
            }
        }
    }

    /// Scenario: Type the reviewer's sequence into a pane's input stream: ab, Ctrl+J, two spaces, Home, Ctrl+U, Home, Backspace, Delete, Delete, Ctrl+U. On real Claude Code and Codex the Ctrl+U at the second line's start joins it to the first, so the deletes take the letters and two spaces stay; the model must say Draft holding those two spaces on both, never Empty, and only Ctrl+C then empties it.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_reviewer_sequence_leaves_two_spaces_on_both_agents() {
        let (bytes, claude, codex) = measured_case("reviewer");
        assert_eq!(
            bytes,
            [
                0x61, 0x62, 0x0a, 0x20, 0x20, 0x1b, 0x5b, 0x48, 0x15, 0x1b, 0x5b, 0x48, 0x7f, 0x1b,
                0x5b, 0x33, 0x7e, 0x1b, 0x5b, 0x33, 0x7e, 0x15
            ],
            "the measured bytes"
        );
        let mut pane = Pane::default();
        pane.user(&bytes);
        for (agent, held) in [(ClaudeCode, claude), (Codex, codex)] {
            assert_eq!(pane.text_in(agent).as_deref(), Some(held), "{agent:?}");
            assert_eq!(pane.proof_in(agent), Draft, "{agent:?}");
        }
        // The join, alone: Ctrl+U at a later line's start deletes the break.
        assert_eq!(
            Pane::default()
                .user(b"ab\ncd\x01\x15")
                .text_in(ClaudeCode)
                .as_deref(),
            Some("abcd")
        );
        assert_eq!(after(b"\n\x15"), Empty);
        pane.user(b"\x03");
        assert_eq!(pane.proof(), Empty);
    }

    /// Scenario: Type three spaces, Enter, x and Ctrl+U into a pane's input stream. Real Codex keeps the blanks on Enter and puts its cursor at the start, so x lands before them and Ctrl+U takes only the x, leaving three spaces; real Claude Code keeps its cursor at the end, so Ctrl+U takes the whole line. The model must hold the three spaces for Codex, and say Empty only for Claude Code.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_codex_blank_enter_keeps_the_blanks_after_its_cursor() {
        let (bytes, claude, codex) = measured_case("blanks_enter_type_kill");
        assert_eq!(
            bytes,
            [0x20, 0x20, 0x20, 0x0d, 0x78, 0x15],
            "the measured bytes"
        );
        let mut pane = Pane::default();
        pane.user(&bytes);
        assert_eq!(pane.text_in(Codex).as_deref(), Some(codex));
        assert_eq!(pane.proof_in(Codex), Draft);
        assert_eq!(pane.text_in(ClaudeCode).as_deref(), Some(claude));
        assert_eq!(pane.proof_in(ClaudeCode), Empty);
        // On several lines too, and Enter on blanks submits nothing.
        let (bytes, _, codex) = measured_case("mixed_blanks_enter_type_kill");
        let mut pane = Pane::default();
        pane.user(&bytes);
        assert_eq!(pane.text_in(Codex).as_deref(), Some(codex));
        pane.0.prompt_reported(User("x"));
        assert_eq!(pane.proof_in(Codex), Draft, "no report belongs to it");
        // Wherever the cursor was in the blanks (measured).
        let mut pane = Pane::default();
        pane.user(b"   \x1b[D\rx");
        assert_eq!(pane.text_in(Codex).as_deref(), Some("x   "));
        assert_eq!(pane.text_in(ClaudeCode).as_deref(), Some("  x "));
        // With no pane agent known, the box holds what either agent holds.
        assert_eq!(
            Pane::default().user(b"   \rx\x15").0.composer().proof(None),
            Draft
        );
    }

    /// Scenario: Type into a pane's input stream the keys measured on a real Claude Code 2.1.310 session: spaces, spaces with the cursor moved back to the start by Left, Ctrl+A or Home, a word with a line-kill before its last letter, a space deleted from in front of it, blanks submitted with Enter. Each blank or partly killed box reads as a draft, the empty box stays empty after Right, Left or Backspace, and only edits that really empty the box (End then Ctrl+U on one line, Ctrl+C, Backspace over every character, Delete at the start, Enter on text) prove it empty.
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
        assert_eq!(after(b"\r"), Empty);
        assert_eq!(after(b"ab  \r"), Empty);
        assert_eq!(after(b"ab\\\r"), Unknown);
        assert_eq!(after(b"ab\\\x1b[D\x1b[C\r"), Unknown);
    }

    /// Scenario: Break lines in a pane's input with Ctrl+J and inside a paste, then edit across them: a line of nothing but a break or blanks, a blank line with text after it and the cursor back on the first line, Backspace joining lines, and Ctrl+U at a line's start joining it to the line above. A box holding only a line break or blanks on another line is a draft, Up or Down loses track of it, and deleting every character and break proves it empty again.
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
        // The second Ctrl+U joins the empty line to `ab`; a third kills it.
        assert_eq!(after(b"ab\ncd\x15\x15"), Draft);
        assert_eq!(after(b"ab\ncd\x15\x15\x15"), Empty);
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

    /// Scenario: Send a pane keys the model does not follow (Tab, Up, non-ASCII text, a word-move, Shift+Enter, Alt+x, a pasted escape) and then each of the ways back that need no report: End then Ctrl+U, and Ctrl+C. Each unfollowed key leaves the box unknown; End then Ctrl+U, adjacent, recovers a box known to be one line, while a key that may have broken a line leaves it unknown after them, since an empty line joins the one above; Ctrl+C recovers every one.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_unfollowed_keys_and_the_ways_back() {
        for (key, lines) in [
            (&b"\t"[..], true),
            (b"\x1b[A", true),
            ("\u{e9}".as_bytes(), false),
            (b"ab\x1b[1;5D", false),
            (b"\x1b[13;2u", true),
            (b"\x1bx", false),
            (b"\x1b\r", true),
            (b"\x17", false),
            (b"\x0b", false),
            (b"\x1b[200~\x1bz\x1b[201~", false),
            (b"\x1b[5~", false),
        ] {
            assert_eq!(after(key), Unknown, "{key:?}");
            let mut cleared = key.to_vec();
            cleared.extend_from_slice(b"\x05\x15");
            let one_line = if lines { LINE_EMPTY } else { (Empty, Empty) };
            assert_eq!(each(&cleared), one_line, "{key:?} then End, Ctrl+U");
            let mut killed = key.to_vec();
            killed.extend_from_slice(b"\x03");
            assert_eq!(after(&killed), Empty, "{key:?} then Ctrl+C");
            let mut apart = key.to_vec();
            apart.extend_from_slice(b"\x05\x1b[D\x15");
            assert_eq!(after(&apart), Unknown, "{key:?}: End and Ctrl+U apart");
        }
        // Other lines may be there: End then Ctrl+U proves the cursor's line
        // empty on Claude Code, and a key that reaches past it, or Enter on
        // what may be blanks, loses track again.
        assert_eq!(each(b"\x1b[A\x05\x15"), LINE_EMPTY);
        assert_eq!(each(b"\x1b[A\x05\x15\x05\x15\x05\x15"), LINE_EMPTY);
        assert_eq!(each(b"\x1b[A\x05\x15ab\x7f\x7f"), LINE_EMPTY);
        assert_eq!(each(b"\x1b[A\x05\x15ab\x01\x1b[3~"), (Draft, Unknown));
        for past in [&b"\x7f"[..], b"\x1b[D", b"\x1b[C", b"\x1b[3~", b"\x15"] {
            let mut bytes = b"\x1b[A\x05\x15".to_vec();
            bytes.extend_from_slice(past);
            assert_eq!(after(&bytes), Unknown, "{past:?}");
        }
        // Claude Code's Home and End keys move on to the line above or below.
        assert_eq!(each(b"\x1b[A\x05\x15\x1b[H"), (Unknown, Unknown));
        assert_eq!(each(b"\x1b[A\x05\x15\x01"), LINE_EMPTY);
        assert_eq!(after(b"\x1b[A\x05\x15  \r"), Unknown);
        assert_eq!(after(b"\x1b[A\x05\x15\x03"), Empty);
    }

    /// Scenario: Measured on real Claude Code 2.1.310 and both Codex releases: one Ctrl+C empties a box of text on two lines, spaces, spaces on two lines, or line breaks, wherever the cursor is, and after Enter on blanks, while End then Ctrl+U leaves a box of several lines behind. Feed those boxes, and boxes the model lost track of, then the remedy the docs name (a letter, then Ctrl+C) or Ctrl+C alone; and Ctrl+C during a turn, which may only interrupt it. The remedy empties every box when no turn is under way, and Ctrl+C during a turn proves an empty box only if it already was.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_the_measured_remedy_clears_any_box() {
        for setup in [
            &b"ab\ncd\x1b[H"[..],
            b"ab\ncd\x1b[D\x1b[D\x1b[D",
            b"   ",
            b"   \x1b[H",
            b"  \n  \x1b[H",
            b"\n\n",
            b"\n\n\x1b[D\x1b[D",
            b"   \r",
            b"  \n  \r",
            b"\x1b[A",
            b"\x1b[A\r",
            b"\t\r  \n",
            b"",
        ] {
            for remedy in [&b"x\x03"[..], b"\x03"] {
                let mut bytes = setup.to_vec();
                bytes.extend_from_slice(remedy);
                assert_eq!(after(&bytes), Empty, "{setup:?} then {remedy:?}");
            }
        }
        // During a turn, Ctrl+C may only interrupt it.
        let mut pane = Pane::default();
        pane.0.turn_started();
        pane.user(b"  \x03");
        assert_eq!(pane.proof(), Unknown, "a draft during a turn");
        pane.0.turn_ended();
        pane.user(b"\x03");
        assert_eq!(pane.proof(), Empty, "after it, Ctrl+C empties it");
        let mut pane = Pane::default();
        pane.0.turn_started();
        pane.user(b"\x03");
        assert_eq!(pane.proof(), Empty, "an empty box stays empty");
        pane.user(b"\t\x03");
        assert_eq!(pane.proof(), Unknown, "a box it lost track of");
    }

    /// Scenario: Reach the box in a context nobody measured: a paste longer than the bound or of more than three lines, an Enter on text with an @ (a file menu may complete instead) or on a mode character followed by blanks, and Tab (which submits on Codex) before a later Enter on blanks whose report then comes. Each leaves the box unknown, never Empty, until Ctrl+C, a known edit on one line, or the Enter's own report.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_unmeasured_contexts_are_unknown() {
        // A paste of three lines is followed; a fourth may fold it.
        assert_eq!(after(b"\x1b[200~a\nb\nc\x1b[201~"), Draft);
        assert_eq!(
            after(b"\x1b[200~a\nb\nc\x1b[201~\x7f\x7f\x7f\x7f\x7f"),
            Empty
        );
        assert_eq!(after(b"\x1b[200~a\nb\nc\nd\x1b[201~"), Unknown);
        assert_eq!(
            after(b"\x1b[200~a\nb\nc\nd\x1b[201~\x7f\x7f\x7f\x7f\x7f\x7f\x7f"),
            Unknown
        );
        // A paste up to the bound is followed; past it, unknown.
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend(std::iter::repeat_n(b' ', super::PASTE_FOLLOWED_MAX));
        paste.extend_from_slice(b"\x1b[201~");
        assert_eq!(after(&paste), Draft);
        let mut bytes = paste.clone();
        bytes.extend_from_slice(b"\x05\x15");
        assert_eq!(after(&bytes), Empty);
        paste.insert(6, b' ');
        assert_eq!(after(&paste), Unknown);
        paste.extend_from_slice(b"\x05\x15");
        assert_eq!(each(&paste), LINE_EMPTY, "it may have broken lines");
        // A menu may take the Enter: confirmed only by its report.
        for text in [&b"see @src"[..], b"!  ", b"/ ", b"?", b"#"] {
            let mut bytes = text.to_vec();
            bytes.push(b'\r');
            let mut pane = Pane::default();
            pane.user(&bytes);
            assert_eq!(pane.proof(), Unknown, "{text:?}");
            pane.0.prompt_reported(User("see @src/main.rs"));
            assert_eq!(pane.proof(), Empty, "{text:?}: its report");
        }
        assert_eq!(after(b"!ls\r"), Empty);
        // Tab may submit: its report is its own, not a later Enter's.
        let mut pane = Pane::default();
        pane.user(b"ab\t  \r");
        pane.0.prompt_reported(User("ab"));
        assert_eq!(pane.proof(), Unknown, "Tab's report");
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.proof(), Empty, "the Enter's own");
    }

    /// Scenario: Press Enter on a box the model lost track of, then let the agent report turns starting and ending, or report prompts from its prompt hook: an unrelated or late working report, a report of a prompt typed before that Enter, the deck's own line, a report that arrived before the Enter was recorded, and finally the Enter's own report, including after history recall with Up and inside a busy turn. Only a prompt report no earlier submit can own proves the box empty; a turn alone, or a report an earlier submit owes, leaves it unknown, held for Ctrl+C or a prompt typed and sent.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_an_unknown_enter_is_confirmed_only_by_its_own_report() {
        // A turn starting and ending is no evidence the Enter submitted.
        let mut pane = Pane::default();
        pane.user(b"\x1b[A\r");
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
        pane.user(b"\x1b[A\r");
        pane.0.prompt_reported(User("hello"));
        assert_eq!(pane.proof(), Unknown, "the earlier prompt's report");
        pane.0.prompt_reported(User("something recalled"));
        assert_eq!(pane.proof(), Empty);
        // Two prompts queued in a turn and reported as one settle both.
        let mut pane = Pane::default();
        pane.0.turn_started();
        pane.user(b"ab\rcd\r\x1b[A\r");
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
        pane.user(long.as_bytes()).user(b"\r\x1b[A\r");
        pane.0.prompt_reported(User(&format!("{}…", &long[..200])));
        assert_eq!(pane.proof(), Unknown);
        // The deck's own line reports as the deck's.
        let mut pane = Pane::default();
        pane.deck(b"(abcdef) dot-agent-deck: message m-1 waiting\r");
        pane.user(b"\x1b[A\r");
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
        assert_eq!(
            (pane.proof_in(ClaudeCode), pane.proof_in(Codex)),
            LINE_EMPTY,
            "End then Ctrl+U: maybe two lines"
        );
        pane.user(b"x\x03");
        assert_eq!(pane.proof(), Empty, "a letter, then Ctrl+C");
        let mut pane = Pane::default();
        pane.0.prompt_reported(User("raced"));
        pane.user(b"\x1b[A\rnext\r");
        assert_eq!(pane.proof(), Empty, "a prompt typed and sent");
        // Typed after the Enter: followed as typed into an empty box.
        let mut pane = Pane::default();
        pane.user(b"\x1b[A\rab\x7f\x7f");
        assert_eq!(pane.proof(), Unknown);
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.proof(), Empty);
        let mut pane = Pane::default();
        pane.user(b"\x1b[A\r  ");
        assert_eq!(pane.proof(), Draft);
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.proof(), Draft);
        // A second Enter with nothing typed since awaits the same report.
        let mut pane = Pane::default();
        pane.user(b"\x1b[A\r\r");
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.proof(), Empty);
        // Blanks typed after it and Enter: kept, before the cursor on
        // Claude Code and after it on Codex.
        let mut pane = Pane::default();
        pane.user(b"\x1b[A\r  \rx");
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.text_in(ClaudeCode).as_deref(), Some("  x"));
        assert_eq!(pane.text_in(Codex).as_deref(), Some("x  "));
    }

    /// Scenario: Press Enter on a box the model lost track of and settle it another way before its report comes: Ctrl+C, End then Ctrl+U on one line, or a prompt typed and sent; then press Enter on another such box. The settled Enter still owes the report it may have caused, so the next unowned report is its and not the new Enter's, until the agent has ended the turn that report would have come in; a prompt sent after the Enter empties the box at once and owes its own report too.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_a_settled_unknown_enter_still_owes_its_report() {
        for settle in [&b"\x03"[..], b"\x05\x15"] {
            let mut pane = Pane::default();
            pane.user("\u{e9}\r".as_bytes()).user(settle);
            assert_eq!(pane.proof(), Empty, "{settle:?}");
            pane.user(b"\x1b[A\r");
            pane.0.prompt_reported(User("first"));
            assert_eq!(pane.proof(), Unknown, "{settle:?}: the settled Enter's");
            pane.0.prompt_reported(User("second"));
            assert_eq!(pane.proof(), Empty, "{settle:?}");
            // Once a turn has ended since, the settled Enter owes nothing.
            let mut pane = Pane::default();
            pane.user("\u{e9}\r".as_bytes()).user(settle);
            pane.0.turn_started();
            pane.0.turn_ended();
            pane.user(b"\x1b[A\r");
            pane.0.prompt_reported(User("recalled"));
            assert_eq!(pane.proof(), Empty, "{settle:?}: expired");
        }
        // Inside a busy turn it waits for the turn after.
        let mut pane = Pane::default();
        pane.0.turn_started();
        pane.user("\u{e9}\r\x05\x15".as_bytes());
        pane.0.turn_ended();
        pane.user(b"\x1b[A\r");
        pane.0.prompt_reported(User("queued"));
        assert_eq!(pane.proof(), Unknown);
        // Text sent after the Enter: the whole box went.
        let mut pane = Pane::default();
        pane.user("\u{e9}\rab\r".as_bytes());
        assert_eq!(pane.proof(), Empty);
        pane.user(b"\x1b[A\r");
        pane.0.prompt_reported(User("\u{e9}ab"));
        assert_eq!(pane.proof(), Unknown, "the box sent with ab");
        pane.0.prompt_reported(User("recalled"));
        assert_eq!(pane.proof(), Empty);
    }

    /// Scenario: After an Enter on a box the model lost track of, type and edit with keys that reach past what was typed since: Left, Backspace, Home or Ctrl+U at its start, Right, Delete or End at its end; and the reviewer's composed case, a single-line box that is really empty but unknown, then Enter, a space, Ctrl+J, End, Ctrl+U, Backspace and Ctrl+A, which leaves one space at the start. Each such key gives the Enter up and the box is unknown; the lines typed after the Enter are still counted, so End then Ctrl+U, which joins an empty line to the one above, does not forget the space, and only Ctrl+C proves the box empty.
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
            let mut bytes = "\u{e9}\rab".as_bytes().to_vec();
            if matches!(keys, b"\x1b[D" | b"\x7f") {
                bytes.extend_from_slice(b"\x1b[D\x1b[D");
            }
            bytes.extend_from_slice(keys);
            let mut pane = Pane::default();
            pane.user(&bytes);
            assert_eq!(pane.proof(), Unknown, "{keys:?}");
            pane.0.prompt_reported(User("typed"));
            assert_eq!(
                pane.proof(),
                Unknown,
                "{keys:?}: given up, a report proves nothing"
            );
        }
        // Within what was typed after it, the Enter waits.
        let mut pane = Pane::default();
        pane.user("\u{e9}\rab\x1b[D\x1b[C\x7f\x7f".as_bytes());
        pane.0.prompt_reported(User("typed"));
        assert_eq!(pane.proof(), Empty);
        // Giving up counts the lines the earlier box may have held.
        assert_eq!(after(b"\x1b[A\rab\x05\x15\x7f"), Unknown);
        // End then Ctrl+U on the one line typed after it empties the box.
        assert_eq!(after("\u{e9}\rab\x05\x15".as_bytes()), Empty);
        assert_eq!(after("\u{e9}\r\x05\x15".as_bytes()), Empty);
        // The reviewer's case (F2-U2-01): the Ctrl+U after End joins the
        // empty second line to the space, so the box is not empty.
        let mut bytes = "\u{e9}".as_bytes().to_vec();
        bytes.extend_from_slice(b"\x7f\r \n\x05\x15");
        let mut pane = Pane::default();
        pane.user(&bytes);
        assert_eq!(
            (pane.proof_in(ClaudeCode), pane.proof_in(Codex)),
            LINE_EMPTY,
            "the space joined back, after which the cursor shows"
        );
        pane.user(b"\x7f\x01");
        assert_eq!(pane.proof(), Unknown);
        pane.0.turn_started();
        pane.0.turn_ended();
        assert_eq!(pane.proof(), Unknown);
        pane.user(b"\x05\x15");
        assert_eq!(
            (pane.proof_in(ClaudeCode), pane.proof_in(Codex)),
            LINE_EMPTY,
            "maybe still two lines"
        );
        pane.user(b"x\x03");
        assert_eq!(pane.proof(), Empty, "a letter, then Ctrl+C");
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
            for (start, proof) in [(&b""[..], Empty), (b"  ", Draft), (b"\x17", Unknown)] {
                let mut bytes = start.to_vec();
                bytes.extend_from_slice(report);
                assert_eq!(after(&bytes), proof, "{start:?} then {report:?}");
            }
        }
        // A report does not separate End from Ctrl+U.
        assert_eq!(after(b"\x17\x05\x1b[I\x15"), Empty);
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

    /// A small deterministic generator (xorshift64*).
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % n
        }
    }

    /// The box as an agent edits it, written from the measurements alone,
    /// apart from the model. Where nobody measured what a key does, it does
    /// something chosen at random, so the model must hold whatever it did.
    struct Reference {
        agent: Semantics,
        text: Vec<char>,
        cursor: usize,
        /// The agent reported a turn starting and has not ended it.
        busy: bool,
        /// Prompts it took and has not reported yet, in order.
        reports: VecDeque<String>,
    }

    impl Reference {
        fn line_start(&self) -> usize {
            self.text[..self.cursor]
                .iter()
                .rposition(|&c| c == '\n')
                .map_or(0, |i| i + 1)
        }

        fn line_end(&self) -> usize {
            self.text[self.cursor..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(self.text.len(), |i| self.cursor + i)
        }

        fn insert(&mut self, c: char) {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }

        fn submit(&mut self) {
            self.reports.push_back(self.text.iter().collect());
            self.text.clear();
            self.cursor = 0;
        }

        fn key(&mut self, key: &str, rng: &mut Rng) {
            match key {
                "a" | "b" | "x" | " " => self.insert(key.chars().next().unwrap()),
                "\n" => self.insert('\n'),
                "\x1b[H" | "\x01" => {
                    let start = self.line_start();
                    // At a later line's start, Claude Code's Home key and
                    // Codex's `Ctrl+A` go to the line above's.
                    let on = (key == "\x1b[H") == (self.agent == ClaudeCode);
                    self.cursor = if start == self.cursor && start > 0 && on {
                        self.cursor -= 1;
                        self.line_start()
                    } else {
                        start
                    };
                }
                "\x1b[F" | "\x05" => {
                    let end = self.line_end();
                    // At an earlier line's end, Claude Code's End key and
                    // Codex's `Ctrl+E` go to the line below's.
                    let on = (key == "\x1b[F") == (self.agent == ClaudeCode);
                    self.cursor = if end == self.cursor && end < self.text.len() && on {
                        self.cursor += 1;
                        self.line_end()
                    } else {
                        end
                    };
                }
                "\x1b[D" => self.cursor = self.cursor.saturating_sub(1),
                "\x1b[C" => self.cursor = (self.cursor + 1).min(self.text.len()),
                "\x7f" if self.cursor > 0 => {
                    self.cursor -= 1;
                    self.text.remove(self.cursor);
                }
                "\x7f" => {}
                "\x1b[3~" if self.cursor == self.text.len() => {}
                "\x1b[3~" => {
                    self.text.remove(self.cursor);
                }
                "\x15" => {
                    let start = self.line_start();
                    if start < self.cursor {
                        self.text.drain(start..self.cursor);
                        self.cursor = start;
                    } else if self.cursor > 0 {
                        self.cursor -= 1;
                        self.text.remove(self.cursor);
                    }
                }
                "\x03" => {
                    // During a turn it may only interrupt (not measured).
                    if !self.busy || rng.below(2) == 0 {
                        self.text.clear();
                        self.cursor = 0;
                    }
                }
                "\r" if self.text.iter().all(|c| c.is_whitespace()) => {
                    if self.agent == Codex {
                        self.cursor = 0;
                    }
                }
                "\r" => self.submit(),
                // Keys the model does not follow: what they do is picked.
                "\t" => match rng.below(4) {
                    0 => {}
                    1 => self.insert(' '),
                    2 if self.text.iter().any(|c| !c.is_whitespace()) => self.submit(),
                    _ => self.insert('\n'),
                },
                "\x1b[A" => match rng.below(3) {
                    0 => {}
                    1 => self.cursor = rng.below(self.text.len() + 1),
                    _ => {
                        let recalled = ["", " ", "\n", "  \n ", "hi", "a\nb"];
                        self.text = recalled[rng.below(recalled.len())].chars().collect();
                        self.cursor = self.text.len();
                    }
                },
                "\u{e9}" => self.insert('\u{e9}'),
                _ => unreachable!("{key:?}"),
            }
        }
    }

    /// Scenario: Feed a pane long random sequences of the measured keys (letters, a space, Ctrl+J, Home, End, Ctrl+A, Ctrl+E, Left, Right, Backspace, Delete, Ctrl+U, Enter, Ctrl+C), mixed with keys the model does not follow (Tab, Up, a non-ASCII letter) that do something picked at random, turns starting and ending, and the agent's delayed reports of the prompts it took, and replay each into a reference editor written from the measurements, once as Claude Code and once as Codex. After every step the model never says Empty while the reference holds anything, and whenever it says it knows the box, it holds exactly the reference's text.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_random_sequences_never_read_empty_over_a_held_box() {
        const KEYS: &[&str] = &[
            "a", "b", "x", " ", " ", "\n", "\n", "\x1b[H", "\x01", "\x1b[F", "\x05", "\x1b[D",
            "\x1b[C", "\x7f", "\x1b[3~", "\x15", "\x15", "\r", "\r", "\x03", "\t", "\x1b[A",
            "\u{e9}",
        ];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for agent in AGENTS {
            for round in 0..4000 {
                let mut pane = Pane::default();
                let mut reference = Reference {
                    agent,
                    text: Vec::new(),
                    cursor: 0,
                    busy: false,
                    reports: VecDeque::new(),
                };
                let mut log = Vec::new();
                for _ in 0..rng.below(60) + 1 {
                    match rng.below(20) {
                        0 if !reference.reports.is_empty() => {
                            let report = reference.reports.pop_front().unwrap();
                            log.push(format!("report {report:?}"));
                            pane.0.prompt_reported(User(&report));
                        }
                        1 => {
                            log.push("turn started".into());
                            reference.busy = true;
                            pane.0.turn_started();
                        }
                        // A report comes before the end of the turn it is in.
                        2 if reference.reports.is_empty() => {
                            log.push("turn ended".into());
                            reference.busy = false;
                            pane.0.turn_ended();
                        }
                        _ => {
                            let key = KEYS[rng.below(KEYS.len())];
                            log.push(format!("{key:?}"));
                            reference.key(key, &mut rng);
                            pane.user(key.as_bytes());
                        }
                    }
                    let held: String = reference.text.iter().collect();
                    let model = &pane.0.composer().model(agent);
                    let what = format!("{agent:?}, round {round}: {log:?}");
                    if model.proof() == Empty {
                        assert_eq!(held, "", "read empty over a held box: {what}");
                    }
                    // What a blank composer row with the cursor at its start
                    // then proves: Claude Code draws a line of blanks with
                    // the cursor at its start as the empty composer.
                    if model.proof() == LineEmpty {
                        assert_eq!(agent, ClaudeCode, "{what}");
                        let looks_empty = !held.contains('\n') && reference.cursor == 0;
                        assert!(
                            held.is_empty() || !looks_empty,
                            "a blank row over a held box: {what}"
                        );
                    }
                    if let super::Content::Known {
                        text,
                        cursor,
                        others: false,
                    } = &model.content
                        && model.unconfirmed.is_none()
                    {
                        assert_eq!(text.iter().collect::<String>(), held, "{what}");
                        assert_eq!(*cursor, reference.cursor, "{what}");
                    }
                }
            }
        }
    }
}
