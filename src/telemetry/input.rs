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

    /// Feed one accepted write. `true` when it contains, or completes,
    /// anything a person did. An incomplete sequence at the end of `bytes` is
    /// held and decided by the next write.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut human = false;
        for &b in bytes {
            human |= self.step(b);
        }
        human
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

    /// Read a held lone `ESC` as the Escape key: `true`, and the filter is
    /// back between sequences. `false`, and nothing changes, when something
    /// else (or nothing) is held.
    pub fn settle_lone_escape(&mut self) -> bool {
        if self.holds_lone_escape() {
            self.state = State::Ground;
            true
        } else {
            false
        }
    }

    fn step(&mut self, b: u8) -> bool {
        match std::mem::take(&mut self.state) {
            State::Ground => {
                if b == ESC {
                    self.state = State::Esc;
                    false
                } else {
                    true
                }
            }
            State::Esc => match b {
                b'[' => {
                    self.state = State::Csi(Vec::new());
                    false
                }
                b']' => {
                    self.state = State::Osc(0);
                    false
                }
                // A second ESC: the first was the Escape key; the second may
                // start a report.
                ESC => {
                    self.state = State::Esc;
                    true
                }
                // Alt+key and the like.
                _ => true,
            },
            State::Csi(mut params) => {
                if (0x40..=0x7e).contains(&b) {
                    !is_csi_report(&params, b)
                } else if params.len() >= MAX_CSI_PARAMS {
                    true
                } else {
                    params.push(b);
                    self.state = State::Csi(params);
                    false
                }
            }
            State::Osc(len) => {
                if b == BEL {
                    false
                } else if len >= MAX_OSC_BODY {
                    true
                } else {
                    self.state = if b == ESC {
                        State::OscEsc(len + 1)
                    } else {
                        State::Osc(len + 1)
                    };
                    false
                }
            }
            State::OscEsc(len) => {
                if b == b'\\' {
                    false
                } else if len >= MAX_OSC_BODY {
                    true
                } else {
                    self.state = State::Osc(len + 1);
                    false
                }
            }
        }
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
