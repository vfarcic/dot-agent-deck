//! Telling a person's input apart from what a client's terminal sends on its
//! own.
//!
//! Everything an attached client writes into a pane arrives on one path, and
//! not all of it was typed: a terminal answers the agent's queries (cursor
//! position, device attributes, colours) and reports focus changes without
//! anyone touching a key. Those must not count as an intervention. Anything
//! else is — keys, pastes, mouse clicks and scrolls.
//!
//! The bytes arrive as the writes the PTY accepted, not as whole reports: one
//! reply can be split across two writes. So the filter is a small streaming
//! parser, one per run ([`InputFilter`]), that holds an incomplete escape
//! sequence until the write that finishes it says what it was. What it holds is
//! bounded, and a sequence that grows past the bound is read as a person's.
//!
//! **A lone `ESC` is the one ambiguity.** It is the Escape key, which people
//! press to interrupt an agent, and it is also the first byte of every report.
//! The filter holds it like any other incomplete sequence; whoever feeds the
//! filter settles it as a person's with [`InputFilter::settle_lone_escape`]
//! once [`LONE_ESCAPE_WINDOW`] has passed with nothing after it, or when the
//! run ends. When a write follows sooner, [`InputFilter::decide_held`] says
//! what it made of the `ESC`, with the rest of that key's bytes and apart from
//! what follows the key, so the key can be counted once, when the `ESC` was
//! written.
//!
//! **What the input was is told apart the same way** ([`InterventionKind`]),
//! from the bytes alone and never by keeping them: a key that interrupts, a
//! bracketed paste, text, or a key that only moves around. Everything between
//! a bracketed paste's `ESC[200~` and `ESC[201~` is the paste, whatever bytes
//! it holds and however long it is — a pasted `0x03` is not Ctrl+C. A paste
//! whose `ESC[201~` never comes is ended by whoever feeds the filter, with
//! [`InputFilter::end_paste`], once [`PASTE_IDLE_WINDOW`] has passed with no
//! input at all.

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Bytes of a CSI sequence's parameters held while it is incomplete. Every
/// report the filter recognises is far shorter.
pub const MAX_CSI_PARAMS: usize = 32;
/// Bytes of an OSC reply held while it is incomplete.
pub const MAX_OSC_BODY: usize = 1024;
/// How long a lone `ESC` waits for the rest of a report before it is read as
/// the Escape key. A report split across writes arrives in pieces
/// microseconds to milliseconds apart; 100 ms is the escape timeout terminal
/// programs commonly use for the same question (Vim's `ttimeoutlen`), and short
/// enough that the key is counted at once. A report whose remainder arrives
/// later than this is counted as a person's input.
pub const LONE_ESCAPE_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);
/// How long a bracketed paste whose `ESC[201~` has not arrived waits with no
/// input at all before it is read as ended, so a client that never sends the
/// end marker — one that died mid-paste, say — cannot hold every later key,
/// Ctrl+C included, as a paste. A terminal sends a paste as fast as the pane
/// accepts it, so a gap inside one is the agent not reading its input; this
/// is far longer than an agent usually stalls under load, and a person who
/// keeps typing after a lost end marker is read correctly again from the
/// first key after a pause this long. Its length bounds no memory: the filter holds no paste bytes.
pub const PASTE_IDLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// What a person's input was, as `deck.intervention.kind` names it. Ordered
/// by precedence: the strongest kind seen in a burst of input is the burst's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InterventionKind {
    /// Only cursor keys, mouse reports, Alt chords and other shortcuts.
    Navigate,
    /// Printable characters, Enter, Backspace, Tab, Delete.
    Text,
    /// A bracketed paste.
    Paste,
    /// Input while the agent waits on a question or permission prompt, or an
    /// answer given in the deck's own question UI. Never from the bytes alone.
    Answer,
    /// Ctrl+C, or the Escape key.
    Interrupt,
}

impl InterventionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Text => "text",
            Self::Paste => "paste",
            Self::Answer => "answer",
            Self::Interrupt => "interrupt",
        }
    }
}

const CTRL_C: u8 = 0x03;
/// What ends a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";
/// The code points the kitty keyboard protocol gives keys that have no
/// character — F13 and up, the keypad, media and modifier keys — in a CSI-u
/// report: Unicode's Private Use Area.
const KITTY_FUNCTIONAL_KEYS: std::ops::RangeInclusive<u32> = 57344..=63743;
/// The keypad's Delete key in the kitty keyboard protocol.
const KITTY_KEYPAD_DELETE: u32 = 57426;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum State {
    /// Between sequences.
    #[default]
    Ground,
    /// After `ESC`.
    Esc,
    /// After `ESC [`, with the parameter bytes so far.
    Csi(Vec<u8>),
    /// Inside `ESC ]`, with its length so far.
    Osc(usize),
    /// Inside an OSC, after an `ESC` that may begin its `ESC \` terminator.
    OscEsc(usize),
    /// After `ESC O`, already counted: a cursor key in application mode
    /// (`ESC O A`) ends with the next byte.
    Ss3,
    /// Inside a bracketed paste: how many of [`PASTE_END`] its last bytes
    /// match.
    Paste(usize),
    /// After `ESC` and the first byte of a multibyte UTF-8 character: Alt
    /// with that character, this many bytes still to come.
    AltChar(u8),
    /// After an X10 mouse report's `ESC [ M`: this many of its three raw
    /// payload bytes still to come.
    X10Mouse(u8),
}

/// The per-run streaming filter. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct InputFilter {
    state: State,
}

impl InputFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one accepted write: the strongest kind of what it contains, or
    /// completes, that a person did, `None` when it is only terminal reports.
    /// An incomplete sequence at the end of `bytes` is held and decided by the
    /// next write.
    pub fn classify(&mut self, bytes: &[u8]) -> Option<InterventionKind> {
        let mut kind = None;
        for &b in bytes {
            kind = kind.max(self.step(b));
        }
        kind
    }

    /// [`Self::classify`], as whether anything a person did is in it.
    #[cfg(test)]
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        self.classify(bytes).is_some()
    }

    /// Whether a sequence is being held.
    #[cfg(test)]
    pub fn is_holding(&self) -> bool {
        self.state != State::Ground
    }

    /// Whether what is held is a lone `ESC`: the last byte fed was an `ESC`
    /// that begins no sequence yet.
    pub fn holds_lone_escape(&self) -> bool {
        self.state == State::Esc
    }

    /// Whether the filter is inside a bracketed paste whose end marker has
    /// not arrived.
    pub fn in_paste(&self) -> bool {
        matches!(self.state, State::Paste(_))
    }

    /// Read a bracketed paste whose end marker has not arrived as ended:
    /// what comes next is classified on its own. Nothing changes when the
    /// filter is not inside one.
    pub fn end_paste(&mut self) {
        if self.in_paste() {
            self.state = State::Ground;
        }
    }

    /// Read a held lone `ESC` as the Escape key — an
    /// [`InterventionKind::Interrupt`]: `true`, and the filter is back between
    /// sequences. `false`, and nothing changes, when something
    /// else (or nothing) is held.
    pub fn settle_lone_escape(&mut self) -> bool {
        if self.holds_lone_escape() {
            self.state = State::Ground;
            true
        } else {
            false
        }
    }

    /// Feed the start of one accepted write, as [`Self::classify`] would,
    /// only until the sequence held from an earlier write is decided and
    /// ended: what it turned out to be — the Escape key before another `ESC`,
    /// Alt with a key, a key's sequence — and how many bytes of `bytes` that
    /// took. A decided key's own remaining bytes are taken with it — an SS3
    /// key's final byte, the rest of an Alt chord's character, an X10 mouse
    /// report's payload, a bracketed paste up to its end marker — so the key
    /// is one input, not one plus its tail; what follows the key is not taken.
    /// The `ESC` after an Escape key is not the Escape key's: it starts a
    /// sequence of its own, held again. A key whose bytes `bytes` runs out
    /// inside stays held, and the write that completes it classifies the rest
    /// as its own. `None` when it was a terminal report, when it is still
    /// undecided once `bytes` run out, or when nothing was held, which takes
    /// no bytes.
    pub fn decide_held(&mut self, bytes: &[u8]) -> (Option<InterventionKind>, usize) {
        let mut taken = 0;
        let mut kind = None;
        while self.state != State::Ground
            && let Some(&b) = bytes.get(taken)
            && (kind.is_none() || self.continues_with(b))
        {
            taken += 1;
            kind = kind.max(self.step(b));
        }
        (kind, taken)
    }

    /// Whether `b` belongs to the key already decided and still in progress,
    /// rather than starting what comes after it.
    fn continues_with(&self, b: u8) -> bool {
        match self.state {
            State::Ss3 => (0x40..=0x7e).contains(&b),
            State::AltChar(_) => (0x80..=0xbf).contains(&b),
            State::X10Mouse(_) | State::Paste(_) => true,
            // Not after a decision, or (an `ESC`) a new sequence.
            State::Ground | State::Esc | State::Csi(_) | State::Osc(_) | State::OscEsc(_) => false,
        }
    }

    fn step(&mut self, b: u8) -> Option<InterventionKind> {
        use InterventionKind::{Interrupt, Navigate, Paste, Text};
        match std::mem::take(&mut self.state) {
            State::Ground => ground_kind(b).or_else(|| {
                self.state = State::Esc;
                None
            }),
            State::Esc => match b {
                b'[' => {
                    self.state = State::Csi(Vec::new());
                    None
                }
                b']' => {
                    self.state = State::Osc(0);
                    None
                }
                b'O' => {
                    self.state = State::Ss3;
                    Some(Navigate)
                }
                // A second ESC: the first was the Escape key; the second may
                // start a report.
                ESC => {
                    self.state = State::Esc;
                    Some(Interrupt)
                }
                // Alt+Enter: a newline.
                b'\r' | b'\n' => Some(Text),
                // Alt with a character beyond ASCII: one key, whatever its
                // length, a shortcut like Alt+x.
                0xc2..=0xf4 => {
                    self.state = State::AltChar(utf8_continuations(b));
                    Some(Navigate)
                }
                // Alt+key and the like.
                _ => Some(Navigate),
            },
            State::Csi(mut params) => {
                if (0x40..=0x7e).contains(&b) {
                    if is_csi_report(&params, b) {
                        None
                    } else {
                        Some(match (params.as_slice(), b) {
                            (b"200", b'~') => {
                                self.state = State::Paste(0);
                                Paste
                            }
                            // An X10 mouse report: three raw bytes follow.
                            (b"", b'M') => {
                                self.state = State::X10Mouse(3);
                                Navigate
                            }
                            // Delete.
                            (b"3", b'~') => Text,
                            (_, b'u') => csi_u_kind(&params),
                            _ => Navigate,
                        })
                    }
                } else if params.len() >= MAX_CSI_PARAMS {
                    Some(Navigate)
                } else {
                    params.push(b);
                    self.state = State::Csi(params);
                    None
                }
            }
            State::Osc(len) => {
                if b == BEL {
                    None
                } else if len >= MAX_OSC_BODY {
                    Some(Navigate)
                } else {
                    self.state = if b == ESC {
                        State::OscEsc(len + 1)
                    } else {
                        State::Osc(len + 1)
                    };
                    None
                }
            }
            State::OscEsc(len) => {
                if b == b'\\' {
                    None
                } else if len >= MAX_OSC_BODY {
                    Some(Navigate)
                } else {
                    self.state = State::Osc(len + 1);
                    None
                }
            }
            State::Ss3 => {
                if (0x40..=0x7e).contains(&b) {
                    Some(Navigate)
                } else {
                    self.step(b)
                }
            }
            State::Paste(matched) => {
                let matched = if b == PASTE_END[matched] {
                    matched + 1
                } else if b == PASTE_END[0] {
                    1
                } else {
                    0
                };
                // Ended: what follows is not the paste.
                if matched < PASTE_END.len() {
                    self.state = State::Paste(matched);
                }
                Some(Paste)
            }
            State::AltChar(left) => {
                if (0x80..=0xbf).contains(&b) {
                    if left > 1 {
                        self.state = State::AltChar(left - 1);
                    }
                    Some(Navigate)
                } else {
                    // Not UTF-8 after all: the character ended before it.
                    self.step(b)
                }
            }
            State::X10Mouse(left) => {
                if left > 1 {
                    self.state = State::X10Mouse(left - 1);
                }
                Some(Navigate)
            }
        }
    }
}

/// How many continuation bytes follow `lead`, the first byte of a multibyte
/// UTF-8 character.
fn utf8_continuations(lead: u8) -> u8 {
    match lead {
        0xc2..=0xdf => 1,
        0xe0..=0xef => 2,
        _ => 3,
    }
}

/// The kind of one byte between sequences; `None` for the `ESC` that starts
/// one.
fn ground_kind(b: u8) -> Option<InterventionKind> {
    match b {
        ESC => None,
        CTRL_C => Some(InterventionKind::Interrupt),
        b'\r' | b'\n' | b'\t' | 0x08 | 0x7f => Some(InterventionKind::Text),
        // Other control keys: shortcuts, not text.
        0x00..=0x1f => Some(InterventionKind::Navigate),
        // Printable, UTF-8 included.
        _ => Some(InterventionKind::Text),
    }
}

/// The kind of a key a terminal speaking the kitty keyboard protocol sent as
/// `ESC [ <code>[:…][;<modifiers>[:…]] u` — Shift+Enter's `ESC[13;2u`, for one.
fn csi_u_kind(params: &[u8]) -> InterventionKind {
    let mut fields = params.split(|&b| b == b';');
    let number = |field: Option<&[u8]>| -> Option<u32> {
        let field = field?.split(|&b| b == b':').next()?;
        std::str::from_utf8(field).ok()?.parse().ok()
    };
    let code = number(fields.next()).map(|c| keypad_as_main(c).unwrap_or(c));
    // The modifiers are sent plus one: 1 is none; bit 2 is Alt, bit 4 Ctrl.
    let modifiers = number(fields.next()).unwrap_or(1).saturating_sub(1);
    let ctrl = modifiers & 4 != 0;
    let alt = modifiers & 2 != 0;
    match code {
        // The keypad's Delete, as `ESC[3~` is.
        Some(KITTY_KEYPAD_DELETE) if modifiers == 0 => InterventionKind::Text,
        Some(c) if KITTY_FUNCTIONAL_KEYS.contains(&c) => InterventionKind::Navigate,
        Some(27) => InterventionKind::Interrupt,
        Some(c) if ctrl && (c == u32::from(b'c') || c == u32::from(b'C')) => {
            InterventionKind::Interrupt
        }
        Some(13 | 9 | 8 | 127) => InterventionKind::Text,
        Some(c) if !ctrl && !alt && char::from_u32(c).is_some_and(|c| !c.is_control()) => {
            InterventionKind::Text
        }
        _ => InterventionKind::Navigate,
    }
}

/// The code the same key has off the keypad, for a kitty keyboard protocol
/// keypad code that crossterm reads as a character or Enter: keypad 1 is `1`,
/// keypad Enter is Enter. `None` for every other code.
fn keypad_as_main(code: u32) -> Option<u32> {
    let key = match code {
        57399..=57408 => char::from_digit(code - 57399, 10)?,
        57409 => '.',
        57410 => '/',
        57411 => '*',
        57412 => '-',
        57413 => '+',
        57414 => '\r',
        57415 => '=',
        57416 => ',',
        _ => return None,
    };
    Some(u32::from(key))
}

/// Whether `ESC [ <params> <fin>` is a report a terminal sends unasked: focus
/// in/out, cursor-position report, device attributes, device status, a DECRQM
/// mode report.
fn is_csi_report(params: &[u8], fin: u8) -> bool {
    match fin {
        b'I' | b'O' => params.is_empty(),
        b'R' => !params.is_empty() && params.iter().all(|b| b.is_ascii_digit() || *b == b';'),
        b'c' => matches!(params.first(), Some(b'?' | b'>' | b'=')),
        b'n' => params.iter().all(u8::is_ascii_digit),
        b'y' => params.first() == Some(&b'?') && params.last() == Some(&b'$'),
        _ => false,
    }
}

/// Whether `bytes`, one complete frame of client input, contains anything a
/// person did. Stateless; a frame that ends inside a sequence counts that
/// sequence as a person's.
#[cfg(test)]
pub fn is_human_input(bytes: &[u8]) -> bool {
    let mut filter = InputFilter::new();
    filter.feed(bytes) || filter.is_holding()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORTS: [&[u8]; 10] = [
        b"\x1b[I",
        b"\x1b[O",
        b"\x1b[24;80R",
        b"\x1b[?62;22c",
        b"\x1b[>1;10;0c",
        b"\x1b[0n",
        b"\x1b[?2026;2$y",
        b"\x1b]11;rgb:0000/0000/0000\x07",
        b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\",
        b"\x1b[I\x1b[12;1R",
    ];

    const HUMAN: [&[u8]; 8] = [
        b"a",
        b"\r",
        b"\x1b",
        b"\x1b[A",
        b"\x1b[200~pasted\x1b[201~",
        b"\x1b[<0;10;5M",
        b"\x1b[Ix",
        b"\x03",
    ];

    #[test]
    fn terminal_reports_alone_are_not_human() {
        for frame in REPORTS.iter().chain([&&b""[..]]) {
            assert!(!is_human_input(frame), "{frame:?}");
        }
    }

    #[test]
    fn keys_pastes_and_mouse_are_human() {
        for frame in HUMAN {
            assert!(is_human_input(frame), "{frame:?}");
        }
    }

    /// Feed `bytes` to a fresh filter as two writes split at `at`, and say
    /// whether either write was read as human.
    fn split_at(bytes: &[u8], at: usize) -> bool {
        let mut filter = InputFilter::new();
        let first = filter.feed(&bytes[..at]);
        let second = filter.feed(&bytes[at..]);
        first || second
    }

    #[test]
    fn a_report_split_at_any_byte_is_still_not_human() {
        for report in REPORTS {
            for at in 0..=report.len() {
                assert!(!split_at(report, at), "{report:?} split at {at}");
            }
            // And one byte per write.
            let mut filter = InputFilter::new();
            for b in report {
                assert!(!filter.feed(std::slice::from_ref(b)), "{report:?}");
            }
            assert!(!filter.is_holding(), "{report:?} was completed");
        }
    }

    #[test]
    fn human_bytes_next_to_a_split_report_are_human_in_the_write_that_has_them() {
        for report in REPORTS {
            let mut framed = b"x".to_vec();
            framed.extend_from_slice(report);
            framed.push(b'y');
            for at in 0..=framed.len() {
                let mut filter = InputFilter::new();
                let first = filter.feed(&framed[..at]);
                let second = filter.feed(&framed[at..]);
                assert_eq!(first, at >= 1, "{framed:?} split at {at}: first write");
                assert_eq!(
                    second,
                    at < framed.len(),
                    "{framed:?} split at {at}: second write"
                );
            }
        }
    }

    /// Whole, split at any byte, or one byte per write — one-byte sequences
    /// included — a person's sequence is human once the writes are in and a
    /// lone `ESC` left held is settled, as the run's owner does when the
    /// window passes or the run ends.
    #[test]
    fn a_human_sequence_split_at_any_byte_is_human() {
        for seq in HUMAN {
            for at in 0..=seq.len() {
                let mut filter = InputFilter::new();
                let first = filter.feed(&seq[..at]);
                let second = filter.feed(&seq[at..]);
                let settled = filter.settle_lone_escape();
                assert!(first || second || settled, "{seq:?} split at {at}");
                assert!(!filter.is_holding(), "{seq:?} split at {at}");
            }
            let mut filter = InputFilter::new();
            let mut human = false;
            for b in seq.iter() {
                human |= filter.feed(std::slice::from_ref(b));
            }
            human |= filter.settle_lone_escape();
            assert!(human, "{seq:?} one byte per write");
        }
    }

    /// The Escape key alone is held, and settling it is what counts it.
    /// Settling holds nothing else: a report still in progress stays held.
    #[test]
    fn a_lone_escape_is_held_until_settled() {
        let mut filter = InputFilter::new();
        assert!(!filter.feed(b"\x1b"));
        assert!(filter.holds_lone_escape());
        assert!(filter.settle_lone_escape());
        assert!(!filter.is_holding());
        assert!(!filter.settle_lone_escape(), "settled once");

        // ESC ESC: the first is a key at once, the second is held.
        assert!(filter.feed(b"\x1b\x1b"));
        assert!(filter.holds_lone_escape());

        let mut filter = InputFilter::new();
        assert!(!filter.feed(b"\x1b[24;"));
        assert!(!filter.holds_lone_escape());
        assert!(!filter.settle_lone_escape());
        assert!(filter.is_holding());
        assert!(!filter.feed(b"80R"), "the report completes as a report");
    }

    /// A held lone `ESC` is decided by the first bytes of the next write, and
    /// only those are taken, with the rest of the key they decide: what
    /// follows the key is classified on its own.
    #[test]
    fn a_held_escape_is_decided_by_the_bytes_that_follow_it() {
        use InterventionKind::{Interrupt, Navigate, Paste, Text};
        let cases: [(&[u8], Option<InterventionKind>, usize, bool); 9] = [
            // Another ESC: the held one was the Escape key; the new one is held.
            (b"\x1b", Some(Interrupt), 1, true),
            (b"\x1bab", Some(Interrupt), 1, true),
            (b"x", Some(Navigate), 1, false),
            (b"\r", Some(Text), 1, false),
            (b"[A", Some(Navigate), 2, false),
            (b"[Iabc", None, 2, false),
            (b"[200~pasted", Some(Paste), 11, false),
            // Still incomplete: decided by a later write.
            (b"[1;", None, 3, false),
            (b"", None, 0, true),
        ];
        for (next, kind, taken, lone) in cases {
            let mut filter = InputFilter::new();
            assert!(!filter.feed(b"\x1b"));
            assert_eq!(filter.decide_held(next), (kind, taken), "{next:?}");
            assert_eq!(filter.holds_lone_escape(), lone, "{next:?}");
        }

        let mut filter = InputFilter::new();
        assert_eq!(filter.decide_held(b"abc"), (None, 0), "nothing held");
        assert!(filter.feed(b"abc"));
    }

    /// A key a held `ESC` begins is taken whole by the write that decides it
    /// — an SS3 key's final byte, the rest of an Alt chord's character, an
    /// X10 mouse report's payload, a paste through its end marker — and
    /// nothing after it is: a key that follows is classified on its own. A
    /// key the write ends inside stays held, and the next write completes it.
    #[test]
    fn a_decided_key_takes_its_own_bytes_and_no_more() {
        use InterventionKind::{Interrupt, Navigate, Paste, Text};
        let cases: [(&[u8], InterventionKind, usize, Option<InterventionKind>); 12] = [
            (b"OA", Navigate, 2, None),
            (b"OAz", Navigate, 2, Some(Text)),
            (b"OA\x03", Navigate, 2, Some(Interrupt)),
            // Not an SS3 final byte: the key ended at `O`.
            (b"O\x03", Navigate, 1, Some(Interrupt)),
            ("\u{e9}z".as_bytes(), Navigate, 2, Some(Text)),
            ("\u{20ac}z".as_bytes(), Navigate, 3, Some(Text)),
            ("\u{1f600}z".as_bytes(), Navigate, 4, Some(Text)),
            // A lead byte with no continuation: the key ended at it.
            (b"\xc3z", Navigate, 1, Some(Text)),
            (b"[Mabc", Navigate, 5, None),
            (b"[M`\x03\x7fz", Navigate, 5, Some(Text)),
            (b"[200~a\x03\x1b[201~z", Paste, 13, Some(Text)),
            // The ESC after the paste is not the paste's: it is held anew.
            (b"[200~a\x1b[201~\x1b", Paste, 12, None),
        ];
        for (next, kind, taken, after) in cases {
            let mut filter = InputFilter::new();
            assert!(!filter.feed(b"\x1b"));
            assert_eq!(filter.decide_held(next), (Some(kind), taken), "{next:?}");
            assert_eq!(filter.classify(&next[taken..]), after, "{next:?}");
            let lone = next.ends_with(b"~\x1b");
            assert_eq!(filter.holds_lone_escape(), lone, "{next:?}");
            assert_eq!(filter.is_holding(), lone, "{next:?}");
        }

        // Split inside the key: the deciding write takes what it has, the
        // filter stays inside the key, and the next write completes it.
        for (next, kind, at) in [
            (&b"OA"[..], Navigate, 1),
            ("\u{20ac}".as_bytes(), Navigate, 2),
            (b"[Mabc", Navigate, 3),
            (b"[200~a\x1b[201~", Paste, 8),
        ] {
            let mut filter = InputFilter::new();
            assert!(!filter.feed(b"\x1b"));
            assert_eq!(
                filter.decide_held(&next[..at]),
                (Some(kind), at),
                "{next:?}"
            );
            assert!(filter.is_holding(), "{next:?} split at {at}");
            assert!(!filter.holds_lone_escape(), "{next:?} split at {at}");
            assert_eq!(filter.classify(&next[at..]), Some(kind), "{next:?}");
            assert!(!filter.is_holding(), "{next:?} completed");
        }
    }

    /// The kind of one whole frame, a lone `ESC` left held settled as the
    /// Escape key.
    fn kind_of(bytes: &[u8]) -> Option<InterventionKind> {
        let mut filter = InputFilter::new();
        let kind = filter.classify(bytes);
        if filter.settle_lone_escape() {
            return kind.max(Some(InterventionKind::Interrupt));
        }
        kind
    }

    #[test]
    fn each_frame_has_the_kind_of_what_a_person_did() {
        use InterventionKind::*;
        for (frame, expected) in [
            (&b"hello"[..], Some(Text)),
            (b"\r", Some(Text)),
            (b"\x7f", Some(Text)),
            (b"\t", Some(Text)),
            (b"\x1b[3~", Some(Text)),
            ("ü".as_bytes(), Some(Text)),
            // Shift+Enter in the kitty keyboard protocol, and Alt+Enter.
            (b"\x1b[13;2u", Some(Text)),
            (b"\x1b\r", Some(Text)),
            (b"\x1b[97u", Some(Text)),
            (b"\x1b[200~text\x1b[201~", Some(Paste)),
            (b"\x03", Some(Interrupt)),
            (b"\x1b", Some(Interrupt)),
            (b"\x1b\x1b", Some(Interrupt)),
            (b"\x1b[27u", Some(Interrupt)),
            (b"\x1b[99;5u", Some(Interrupt)),
            (b"text\x03", Some(Interrupt)),
            (b"\x1b[A", Some(Navigate)),
            (b"\x1b[1;5D", Some(Navigate)),
            (b"\x1bOA", Some(Navigate)),
            (b"\x1bx", Some(Navigate)),
            (b"\x1b[<0;10;5M", Some(Navigate)),
            (b"\x1b[5~", Some(Navigate)),
            (b"\x1b[97;3u", Some(Navigate)),
            (b"\x01", Some(Navigate)),
            (b"\x1b[Ax", Some(Text)),
            (b"\x1b[I", None),
            (b"\x1b[24;80R", None),
        ] {
            assert_eq!(kind_of(frame), expected, "{frame:?}");
        }
    }

    /// A kind is the same whether its sequence arrives whole or split.
    #[test]
    fn a_split_sequence_keeps_its_kind() {
        for frame in [
            &b"\x1b[200~x\x1b[201~"[..],
            b"\x1b[A",
            b"\x1bOB",
            b"\x1b[13;2u",
            b"\x1b[<0;10;5M",
        ] {
            let whole = kind_of(frame);
            for at in 0..=frame.len() {
                let mut filter = InputFilter::new();
                let mut kind = filter.classify(&frame[..at]);
                kind = kind.max(filter.classify(&frame[at..]));
                if filter.settle_lone_escape() {
                    kind = kind.max(Some(InterventionKind::Interrupt));
                }
                assert_eq!(kind, whole, "{frame:?} split at {at}");
            }
        }
    }

    /// Whole, split at any byte, or one byte per write, `frame` has kind
    /// `expected`, no write of it is of any other kind, and nothing is held
    /// after it.
    fn assert_kind_however_split(frame: &[u8], expected: InterventionKind) {
        let writes = (0..=frame.len())
            .map(|at| vec![&frame[..at], &frame[at..]])
            .chain([frame.chunks(1).collect()]);
        for writes in writes {
            let mut filter = InputFilter::new();
            let mut kind = None;
            for write in &writes {
                let got = filter.classify(write);
                assert!(
                    got.is_none_or(|got| got == expected),
                    "{frame:?} as {writes:?}: a write read as {got:?}"
                );
                kind = kind.max(got);
            }
            assert_eq!(kind, Some(expected), "{frame:?} as {writes:?}");
            assert!(!filter.is_holding(), "{frame:?} as {writes:?}");
        }
    }

    /// Everything between the paste markers is the paste: Ctrl+C, Escape,
    /// arrows and a near-miss of the end marker inside it included, and
    /// however the paste is split across writes.
    #[test]
    fn a_bracketed_paste_is_a_paste_whatever_it_holds() {
        for frame in [
            &b"\x1b[200~a\x03b\x1b[201~"[..],
            b"\x1b[200~\x1b\x1b[A\x1bOB\x1b[<0;1;1M\x1b[200~\x1b[201~",
            b"\x1b[200~x\x1b[201y\x03\x1b\x1b[2\x1b[201~",
            b"\x1b[200~\x1b[201~",
        ] {
            assert_kind_however_split(frame, InterventionKind::Paste);
        }

        // The markers and the payload in writes of their own.
        let mut filter = InputFilter::new();
        for write in [&b"\x1b[200~"[..], b"\x03\x1b", b"[A", b"\x1b[201~"] {
            assert_eq!(
                filter.classify(write),
                Some(InterventionKind::Paste),
                "{write:?}"
            );
        }
        // After the paste, Ctrl+C is Ctrl+C again.
        assert_eq!(filter.classify(b"\x03"), Some(InterventionKind::Interrupt));
    }

    /// A paste stays the paste until its end marker, however long it is: a
    /// Ctrl+C byte past the first MiB of it is pasted text, whole or in
    /// writes.
    #[test]
    fn a_large_paste_is_a_paste_until_its_end_marker() {
        let mut frame = b"\x1b[200~".to_vec();
        frame.extend(std::iter::repeat_n(b'a', (1 << 20) + 1));
        frame.push(0x03);
        frame.extend_from_slice(b"\x1b\x1b[A");
        frame.extend_from_slice(PASTE_END);
        assert_eq!(kind_of(&frame), Some(InterventionKind::Paste));

        let mut filter = InputFilter::new();
        for write in frame.chunks(4096) {
            assert_eq!(filter.classify(write), Some(InterventionKind::Paste));
        }
        assert!(!filter.in_paste());
        assert_eq!(filter.classify(b"\x03"), Some(InterventionKind::Interrupt));
    }

    /// A paste whose end marker has not come is ended by
    /// [`InputFilter::end_paste`]: the next key is its own. Ending nothing
    /// changes nothing.
    #[test]
    fn end_paste_ends_an_unterminated_paste() {
        let mut filter = InputFilter::new();
        assert_eq!(
            filter.classify(b"\x1b[200~ab\x1b[20"),
            Some(InterventionKind::Paste)
        );
        assert!(filter.in_paste());
        assert_eq!(filter.classify(b"\x03"), Some(InterventionKind::Paste));
        filter.end_paste();
        assert!(!filter.in_paste());
        assert!(!filter.is_holding());
        assert_eq!(filter.classify(b"\x03"), Some(InterventionKind::Interrupt));

        let mut filter = InputFilter::new();
        assert!(!filter.feed(b"\x1b[24;"));
        filter.end_paste();
        assert!(filter.is_holding(), "a report in progress is left alone");
        assert!(!filter.feed(b"80R"));
    }

    /// Keys the kitty keyboard protocol reports by a Private Use Area code —
    /// F13 and up, the keypad's arrows and Insert, media and modifier keys —
    /// only move around.
    #[test]
    fn kitty_functional_keys_are_navigation() {
        for frame in [
            &b"\x1b[57344u"[..],
            b"\x1b[57376u",
            b"\x1b[57419u",
            b"\x1b[57425u",
            b"\x1b[57427u",
            b"\x1b[57441;2u",
            b"\x1b[57426;5u",
            b"\x1b[63743u",
        ] {
            assert_kind_however_split(frame, InterventionKind::Navigate);
        }
        // Characters either side of the range (surrogates lie just below it)
        // are still characters.
        assert_eq!(kind_of(b"\x1b[55295u"), Some(InterventionKind::Text));
        assert_eq!(kind_of(b"\x1b[63744u"), Some(InterventionKind::Text));
    }

    /// The keypad's keys crossterm reads as a character, Enter or Delete are
    /// the kind the same key is off the keypad, modifiers included.
    #[test]
    fn kitty_keypad_characters_are_their_main_keys() {
        use InterventionKind::*;
        for (keypad, main, expected) in [
            (&b"\x1b[57399u"[..], &b"0"[..], Text),
            (b"\x1b[57400u", b"1", Text),
            (b"\x1b[57408u", b"9", Text),
            (b"\x1b[57409u", b".", Text),
            (b"\x1b[57413;2u", b"\x1b[43;2u", Text),
            (b"\x1b[57414u", b"\r", Text),
            (b"\x1b[57414;2u", b"\x1b[13;2u", Text),
            (b"\x1b[57416u", b",", Text),
            (b"\x1b[57426u", b"\x1b[3~", Text),
            (b"\x1b[57400;5u", b"\x1b[49;5u", Navigate),
            (b"\x1b[57400;3u", b"\x1b[49;3u", Navigate),
        ] {
            assert_eq!(kind_of(main), Some(expected), "{main:?}");
            assert_kind_however_split(keypad, expected);
        }
    }

    /// Alt with a character beyond ASCII is one key, however many bytes it
    /// takes and however they are split, and a shortcut, as Alt+x is and as
    /// the same key is in the kitty keyboard protocol.
    #[test]
    fn alt_with_a_multibyte_character_is_one_navigation_key() {
        for ch in ["é", "ü", "€", "😀"] {
            let mut frame = b"\x1b".to_vec();
            frame.extend_from_slice(ch.as_bytes());
            assert_kind_however_split(&frame, InterventionKind::Navigate);
        }
        assert_eq!(kind_of(b"\x1b[252;3u"), Some(InterventionKind::Navigate));
        // A lead byte with no continuation ends there; what follows is its
        // own.
        assert_eq!(kind_of(b"\x1b\xc3\x03"), Some(InterventionKind::Interrupt));
        assert_eq!(kind_of(b"\x1b\xc3a"), Some(InterventionKind::Text));
        let mut filter = InputFilter::new();
        assert_eq!(
            filter.classify(b"\x1b\xc3\x1b[A"),
            Some(InterventionKind::Navigate)
        );
        assert!(!filter.is_holding());
    }

    /// An X10 mouse report's three raw bytes are the report's, whatever they
    /// look like, as an SGR mouse report is.
    #[test]
    fn an_x10_mouse_report_is_navigation() {
        assert_eq!(kind_of(b"\x1b[<0;10;5M"), Some(InterventionKind::Navigate));
        for frame in [&b"\x1b[M #!"[..], b"\x1b[Mabc", b"\x1b[M`\x03\x7f"] {
            assert_kind_however_split(frame, InterventionKind::Navigate);
        }
        let mut filter = InputFilter::new();
        assert_eq!(
            filter.classify(b"\x1b[Mabc"),
            Some(InterventionKind::Navigate)
        );
        assert_eq!(filter.classify(b"x"), Some(InterventionKind::Text));
    }

    #[test]
    fn what_is_held_is_bounded() {
        let mut filter = InputFilter::new();
        assert!(!filter.feed(b"\x1b["));
        assert!(filter.feed(&[b'1'; MAX_CSI_PARAMS + 1]));
        assert!(!filter.is_holding());

        let mut filter = InputFilter::new();
        assert!(!filter.feed(b"\x1b]"));
        assert!(filter.feed(&vec![b'a'; MAX_OSC_BODY + 1]));
        assert!(!filter.is_holding());
    }
}
