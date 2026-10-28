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
//! run ends.
//!
//! **What the input was is told apart the same way** ([`InterventionKind`]),
//! from the bytes alone and never by keeping them: a key that interrupts, a
//! bracketed paste, text, or a key that only moves around.

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
                // Alt+key and the like.
                _ => Some(Navigate),
            },
            State::Csi(mut params) => {
                if (0x40..=0x7e).contains(&b) {
                    if is_csi_report(&params, b) {
                        None
                    } else {
                        Some(match (params.as_slice(), b) {
                            (b"200", b'~') => Paste,
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
        }
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
    let code = number(fields.next());
    // The modifiers are sent plus one: 1 is none; bit 2 is Alt, bit 4 Ctrl.
    let modifiers = number(fields.next()).unwrap_or(1).saturating_sub(1);
    let ctrl = modifiers & 4 != 0;
    let alt = modifiers & 2 != 0;
    match code {
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
