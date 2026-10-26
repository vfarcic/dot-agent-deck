//! PRD #1666 part 3 (Q35-01, core §4.2 and §4.4): the framing of a pane's
//! PTY byte stream, and the Ctrl+G candidates in it.
//!
//! One small tokenizer, [`Framing`], fed a byte at a time with its state
//! carried across calls, so a sequence or a paste split over any number of
//! writes is read as one. Its states are ground; ESC; CSI, with a bounded
//! buffer for its parameter and intermediate bytes; SS3; the three raw bytes
//! of a legacy X10 mouse report; the OSC, DCS, APC, PM and SOS strings; and,
//! orthogonal to all of them, bracketed paste. Each byte costs O(1) and the
//! state is a fixed size.
//!
//! Core reads the same tokenizer over different streams: the input stream in
//! acceptance order (the logical planner, Q35-16), the bytes the PTY took
//! (the physical tracker, Q35-16), and the agent's output (paste mode,
//! Q35-02; the queries the epoch's ledger waits on, Q35-06).
//!
//! **Two grammars.** [`Framing::default`] reads what a client sends an agent:
//! an ESC and the byte after it are one Alt+key, `ESC O` opens an SS3 key,
//! `CSI M` an X10 mouse report, and bracketed paste is tracked.
//! [`Framing::output`] reads what an agent writes to its terminal, following
//! the state machine of the `vte` crate that the deck's `vt100` parser is built
//! on: a C0 control or DEL after an ESC is reported without ending the escape
//! (`ESC NUL c` is still RIS), an ESC with intermediate bytes (`ESC ( B`) runs
//! to its final byte ([`Event::IntermediateEscape`]), `ESC O` and `CSI M` are
//! ordinary sequences, a byte at or above `0x80` inside an escape or a CSI is
//! skipped rather than making it malformed, and there is no paste. Two
//! differences from `vte` are kept: the 64-byte CSI cap, and a DCS that ends
//! only at ST, CAN, SUB or another ESC, not at the 8-bit ST (`0x9C`): an ESC
//! ends the DCS in both, so a sequence an ESC opens after it reads the same.
//! In both grammars, a control before a CSI's private marker leaves it the
//! private marker, and a private-marker byte after the first is malformed, as
//! `vte` ignores that CSI. `tests/output_paste_mode.rs` compares the output
//! grammar's reading of mode 2004 with `vt100`'s, and pins the difference it
//! knows of.
//!
//! **Edge cases, explicitly:**
//! - CAN (`0x18`) and SUB (`0x1A`) abandon a sequence or string and return to
//!   ground.
//! - ST (`ESC \`) ends a string. An ESC followed by anything else abandons
//!   the string, and that ESC opens the next sequence.
//! - A BEL ends an OSC string only. Inside a CSI, DCS, APC, PM or SOS it is
//!   reported and the sequence goes on.
//! - A CSI with more than [`MAX_SEQUENCE_BYTES`] parameter and intermediate
//!   bytes, a parameter byte after an intermediate, a private-marker byte
//!   after its first byte, or (on input) a byte at or above `0x80` is
//!   malformed: it returns to ground at its final byte, dispatching nothing
//!   ([`Event::MalformedCsi`]).
//! - Any other C0 control (and DEL) inside a CSI or a string is reported
//!   ([`Event::Inside`]) and doesn't end it: the tokenizer's rule, chosen
//!   because staying inside is the conservative direction for the boundary,
//!   not a claim that it reads every control as a given terminal does. On
//!   input, inside an SS3 or X10 prefix any C0 control, BEL and ESC included,
//!   ends the prefix, which was never a sequence, and is read afresh in
//!   ground.
//! - A string's length is unbounded; only the first [`MAX_SEQUENCE_BYTES`]
//!   of its payload are kept, for classifying a reply. Staying in a string is
//!   the conservative direction: the stream isn't at a boundary, so no deck
//!   line starts there (core §4.8), which costs availability only.
//!
//! **Bracketed paste** opens at `ESC [ 200 ~` and closes at `ESC [ 201 ~`,
//! exactly those bytes, as a bracketed-paste reader waiting for the literal
//! marker sees them: a marker with a control or DEL anywhere between its
//! `ESC [` and its `~` is not one, although the CSI it spells is otherwise
//! read as usual. Inside a paste every sequence is still tokenized,
//! so the closing marker is found even after an abandoned sequence, and
//! every token says it was inside a paste ([`Token::in_paste`]): the agent
//! reads a paste as text only while its own bracketed-paste mode is on, which
//! this tokenizer doesn't know.
//!
//! **Ctrl+G candidates** (core §4.4, with R3 §4.2's forms kept until M44e
//! narrows them) are found by [`Token::candidate`]. Every form is a
//! candidate wherever it appears, a paste included: the safe default, since
//! an unmatched candidate only holds bytes, while a missed one lets the
//! agent's editor read them. Whether a BEL ending an OSC was a whole solicited
//! colour reply (D3′), and so no candidate, is the caller's to decide from
//! [`whole_report`] and its query ledger.

/// A CSI sequence's parameter and intermediate bytes, together, past which
/// it is malformed; also how much of a string's payload is kept (core §4.2).
pub const MAX_SEQUENCE_BYTES: usize = 64;

const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ESC: u8 = 0x1b;
const DEL: u8 = 0x7f;

/// The two Ctrl+G key codes the CSI forms carry: `g` and `G`.
const CTRL_G_KEYS: [u32; 2] = [103, 71];

/// `CSI 27 ; <mods> ; <key> ~`: xterm's modifyOtherKeys.
const MODIFY_OTHER_KEYS: u32 = 27;

/// The tokenizer. See the module documentation.
#[derive(Debug, Clone)]
pub struct Framing {
    state: State,
    /// Inside a bracketed paste, across every other state.
    paste: bool,
    /// A CSI's parameter bytes then its intermediate bytes, or a string's
    /// payload.
    buf: [u8; MAX_SEQUENCE_BYTES],
    len: usize,
    /// How many of `buf`'s bytes are a CSI's parameters; the rest are its
    /// intermediates.
    params_len: usize,
    /// A CSI's private marker (`<`, `=`, `>` or `?` as its first byte).
    private: Option<u8>,
    /// Whether the CSI being read has had any byte after `ESC [`.
    started: bool,
    /// Every byte of the CSI since `ESC [` was kept as its private marker,
    /// a parameter or an intermediate: no control or DEL came between. Only
    /// such a CSI can be a paste marker, whose bytes are exact.
    literal: bool,
    /// The CSI is malformed, or the string's payload overflowed `buf`.
    malformed: bool,
    /// The output grammar ([`Framing::output`]).
    output: bool,
}

impl Default for Framing {
    fn default() -> Self {
        Self {
            state: State::Ground,
            paste: false,
            buf: [0; MAX_SEQUENCE_BYTES],
            len: 0,
            params_len: 0,
            private: None,
            started: false,
            literal: false,
            malformed: false,
            output: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    /// On output, an ESC and one or more intermediate bytes.
    EscIntermediate,
    Csi,
    Ss3,
    X10 {
        remaining: u8,
    },
    Str {
        kind: StringKind,
        esc: bool,
    },
}

/// Which string a [`Str`] was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringKind {
    /// `ESC ]`
    Osc,
    /// `ESC P`
    Dcs,
    /// `ESC _`
    Apc,
    /// `ESC ^`
    Pm,
    /// `ESC X`
    Sos,
}

/// What ended a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// `0x07`, which ends an OSC only.
    Bel,
    /// `ESC \`
    St,
}

/// Where a BEL was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BelAt {
    /// In ground: Ctrl+G itself.
    Ground,
    /// Inside an unfinished CSI, or, on output, an unfinished escape.
    Sequence,
    /// Inside a DCS, APC, PM or SOS string.
    String,
}

/// What one byte completed. A byte completes at most two: an abandoned
/// sequence, then the byte read afresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// A byte outside every sequence: text, UTF-8, DEL, or a C0 control
    /// other than BEL and ESC.
    Plain(u8),
    /// A BEL that ends no OSC.
    Bel(BelAt),
    /// `ESC 0x07`: Alt+Ctrl+G.
    EscBel,
    /// `ESC` and one byte that opens no sequence: an Alt+key on input (`ESC
    /// CR` is Alt+Enter), an escape such as RIS (`ESC c`) on output.
    Escape(u8),
    /// On output, an ESC, its intermediate bytes and its final byte, such as
    /// a character-set designation (`ESC ( B`).
    IntermediateEscape {
        /// Its final byte.
        final_byte: u8,
    },
    /// An SS3 key: `ESC O` and its final byte.
    Ss3(u8),
    /// A complete, well-formed CSI sequence, exact paste markers excepted.
    Csi(Csi<'a>),
    /// A CSI sequence that reached its final byte malformed.
    MalformedCsi {
        /// Its final byte.
        final_byte: u8,
    },
    /// A legacy X10 mouse report: `ESC [ M` and three bytes.
    X10Mouse,
    /// A string ended by its terminator.
    Str(Str<'a>),
    /// A C0 control or DEL inside an unfinished CSI or string, or, on
    /// output, an unfinished escape, which goes on.
    Inside(u8),
    /// A sequence or string that ended unfinished: CAN or SUB, an ESC that
    /// opened another, a control that ended an SS3 or X10 prefix, or a lone
    /// ESC followed by another ESC (the Escape key). Its bytes were input.
    Abandoned,
    /// `ESC [ 200 ~`, exactly those bytes, outside a paste.
    PasteStart,
    /// `ESC [ 201 ~`, exactly those bytes, inside a paste.
    PasteEnd,
}

/// One [`Event`], and whether it was read inside a bracketed paste. The
/// closing marker counts as inside, the opening one as outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub event: Event<'a>,
    pub in_paste: bool,
}

/// A complete CSI sequence, borrowed from the tokenizer for the call that
/// reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csi<'a> {
    /// `<`, `=`, `>` or `?` when it was the first byte after `ESC [`.
    pub private: Option<u8>,
    /// The parameter bytes, without the private marker.
    pub params: &'a [u8],
    pub intermediates: &'a [u8],
    pub final_byte: u8,
}

/// One `;`-separated parameter of a [`Csi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Param<'a>(&'a [u8]);

/// A string ended by its terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Str<'a> {
    pub kind: StringKind,
    /// The payload, or `None` when it was longer than
    /// [`MAX_SEQUENCE_BYTES`].
    pub payload: Option<&'a [u8]>,
    pub terminator: Terminator,
}

/// A Ctrl+G candidate (core §4.4): a byte or sequence the agent may read as
/// its external-editor key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub form: CandidateForm,
    /// Inside a bracketed paste. A candidate either way: with the agent's
    /// bracketed-paste mode off it is a key (core §4.4), and with it on,
    /// that the agent reads it as text is premise P-PASTE, unmeasured.
    pub in_paste: bool,
}

/// Each form a Ctrl+G candidate takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateForm {
    /// `0x07`.
    Bel,
    /// `ESC 0x07`, Alt+Ctrl+G, whatever the gap between the two bytes.
    EscBel,
    /// `CSI <key> … u` where the key code, its shifted key or its base-layout
    /// key is 103 or 71, with any modifiers and event type.
    CsiU,
    /// `CSI 27 ; <mods> ; <key> ~` with key 103 or 71, any modifiers.
    ModifyOtherKeys,
    /// A BEL inside an unfinished CSI.
    BelInSequence,
    /// A BEL ending an OSC string, unless the caller classifies the string
    /// as one whole solicited reply (D3′).
    BelEndsOsc,
    /// A BEL inside a DCS, APC, PM or SOS string.
    BelInString,
    /// A malformed CSI ending in `u` or `~`: a Ctrl+G form past the bytes
    /// kept can't be ruled out.
    MalformedKey,
}

/// What a byte of the user's own did to the input box, as far as its
/// framing says (OQ2's suspension recovery): only CR, `Ctrl+U` and `Ctrl+C`,
/// in ground and outside a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// A CR: Enter. `ESC CR` (Alt+Enter) and LF (`Ctrl+J`) are newlines.
    Submit,
    /// `Ctrl+U` (`0x15`) or `Ctrl+C` (`0x03`).
    Clear,
}

/// A terminal report or reply a client forwards through the input channel:
/// D3's focus and SGR mouse reports, D3′'s DA1 and OSC 10/11 colour replies,
/// and CPR, which core exempts from nothing (OQ3) but whose reply the query
/// ledger settles on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// `CSI I`
    FocusIn,
    /// `CSI O`
    FocusOut,
    /// `CSI < b ; x ; y M` or `… m`.
    SgrMouse,
    /// `CSI r ; c R`. A modified F3 (`CSI 1 ; 2 R`) has the same shape; the
    /// caller tells them apart by the cursor the query saw.
    Cpr { row: u16, col: u16 },
    /// `CSI ? … c`
    Da1,
    /// `OSC 10 ; rgb:h/h/h` or `OSC 11 ; …`, ended by BEL or ST, each
    /// channel one to four hex digits.
    OscColour {
        /// 10 (foreground) or 11 (background).
        slot: u8,
    },
}

impl Framing {
    /// A tokenizer for what an agent writes to its terminal: the output
    /// grammar (see the module documentation).
    pub fn output() -> Self {
        Self {
            output: true,
            ..Self::default()
        }
    }

    /// Read one byte, handing each [`Token`] it completes to `emit`.
    pub fn feed(&mut self, byte: u8, mut emit: impl FnMut(Token<'_>)) {
        self.step(byte, &mut emit);
    }

    /// Whether the stream is at a boundary: in ground, outside any paste,
    /// string or escape sequence (core §4.8's `physical_boundary`).
    pub fn at_ground(&self) -> bool {
        self.state == State::Ground && !self.paste
    }

    fn token<'a>(&self, event: Event<'a>) -> Token<'a> {
        Token {
            event,
            in_paste: self.paste,
        }
    }

    fn step(&mut self, byte: u8, emit: &mut dyn FnMut(Token<'_>)) {
        match self.state {
            State::Ground => self.ground(byte, emit),
            State::Esc => self.escape(byte, emit),
            State::EscIntermediate => self.escape_intermediate(byte, emit),
            State::Csi => self.csi(byte, emit),
            State::Ss3 => match byte {
                0x00..=0x1f => {
                    emit(self.token(Event::Abandoned));
                    self.state = State::Ground;
                    self.ground(byte, emit);
                }
                _ => {
                    emit(self.token(Event::Ss3(byte)));
                    self.state = State::Ground;
                }
            },
            State::X10 { remaining } => match byte {
                0x00..=0x1f => {
                    emit(self.token(Event::Abandoned));
                    self.state = State::Ground;
                    self.ground(byte, emit);
                }
                _ if remaining > 1 => {
                    self.state = State::X10 {
                        remaining: remaining - 1,
                    };
                }
                _ => {
                    emit(self.token(Event::X10Mouse));
                    self.state = State::Ground;
                }
            },
            State::Str { kind, esc } => self.string(kind, esc, byte, emit),
        }
    }

    fn ground(&mut self, byte: u8, emit: &mut dyn FnMut(Token<'_>)) {
        match byte {
            ESC => self.state = State::Esc,
            BEL => emit(self.token(Event::Bel(BelAt::Ground))),
            _ => emit(self.token(Event::Plain(byte))),
        }
    }

    fn escape(&mut self, byte: u8, emit: &mut dyn FnMut(Token<'_>)) {
        if self.output {
            match byte {
                0x20..=0x2f => {
                    self.state = State::EscIntermediate;
                    return;
                }
                b'O' => {
                    emit(self.token(Event::Escape(byte)));
                    self.state = State::Ground;
                    return;
                }
                BEL => {
                    emit(self.token(Event::Bel(BelAt::Sequence)));
                    return;
                }
                ESC | CAN | SUB => {}
                0x00..=0x1f | DEL => {
                    emit(self.token(Event::Inside(byte)));
                    return;
                }
                0x80..=0xff => return,
                _ => {}
            }
        }
        let string = |kind| State::Str { kind, esc: false };
        self.state = match byte {
            b'[' => {
                self.len = 0;
                self.params_len = 0;
                self.private = None;
                self.started = false;
                self.literal = true;
                self.malformed = false;
                State::Csi
            }
            b']' | b'P' | b'_' | b'^' | b'X' => {
                self.len = 0;
                self.malformed = false;
                string(match byte {
                    b']' => StringKind::Osc,
                    b'P' => StringKind::Dcs,
                    b'_' => StringKind::Apc,
                    b'^' => StringKind::Pm,
                    _ => StringKind::Sos,
                })
            }
            b'O' => State::Ss3,
            BEL => {
                emit(self.token(Event::EscBel));
                State::Ground
            }
            // The first ESC was the Escape key; this one opens a sequence.
            ESC => {
                emit(self.token(Event::Abandoned));
                State::Esc
            }
            CAN | SUB => {
                emit(self.token(Event::Abandoned));
                State::Ground
            }
            _ => {
                emit(self.token(Event::Escape(byte)));
                State::Ground
            }
        };
    }

    /// On output, after an ESC and an intermediate byte: more intermediates,
    /// then the final byte.
    fn escape_intermediate(&mut self, byte: u8, emit: &mut dyn FnMut(Token<'_>)) {
        match byte {
            0x30..=0x7e => {
                self.state = State::Ground;
                emit(self.token(Event::IntermediateEscape { final_byte: byte }));
            }
            BEL => emit(self.token(Event::Bel(BelAt::Sequence))),
            CAN | SUB => {
                emit(self.token(Event::Abandoned));
                self.state = State::Ground;
            }
            ESC => {
                emit(self.token(Event::Abandoned));
                self.state = State::Esc;
            }
            0x00..=0x1f | DEL => emit(self.token(Event::Inside(byte))),
            // More intermediates, or a high byte, which `vte` skips.
            _ => {}
        }
    }

    fn csi(&mut self, byte: u8, emit: &mut dyn FnMut(Token<'_>)) {
        // A control doesn't start the CSI: a private marker after one is
        // still its private marker.
        let first = !self.started;
        match byte {
            b'<' | b'=' | b'>' | b'?' if first => {
                self.started = true;
                self.private = Some(byte);
            }
            b'<' | b'=' | b'>' | b'?' => self.malformed = true,
            0x30..=0x3b => {
                self.started = true;
                if self.len > self.params_len {
                    self.malformed = true;
                }
                if self.push(byte) {
                    self.params_len = self.len;
                }
            }
            0x20..=0x2f => {
                self.started = true;
                self.push(byte);
            }
            0x40..=0x7e => {
                self.state = State::Ground;
                self.dispatch_csi(byte, emit);
            }
            BEL => {
                self.literal = false;
                emit(self.token(Event::Bel(BelAt::Sequence)));
            }
            CAN | SUB => {
                emit(self.token(Event::Abandoned));
                self.state = State::Ground;
            }
            ESC => {
                emit(self.token(Event::Abandoned));
                self.state = State::Esc;
            }
            0x00..=0x1f | DEL => {
                self.literal = false;
                emit(self.token(Event::Inside(byte)));
            }
            _ if self.output => {}
            _ => self.malformed = true,
        }
    }

    /// Keep a byte of the sequence or string; `false`, and malformed, once
    /// [`MAX_SEQUENCE_BYTES`] are kept.
    fn push(&mut self, byte: u8) -> bool {
        if self.len == MAX_SEQUENCE_BYTES {
            self.malformed = true;
            return false;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        true
    }

    /// The one place a CSI completes: its final byte has been read and the
    /// state is back in ground.
    fn dispatch_csi(&mut self, final_byte: u8, emit: &mut dyn FnMut(Token<'_>)) {
        if self.malformed {
            emit(self.token(Event::MalformedCsi { final_byte }));
            return;
        }
        let params = &self.buf[..self.params_len];
        let intermediates = &self.buf[self.params_len..self.len];
        let plain = self.private.is_none() && intermediates.is_empty();
        let marker = !self.output && plain && self.literal && final_byte == b'~';
        if marker && params == b"200" && !self.paste {
            emit(self.token(Event::PasteStart));
            self.paste = true;
        } else if marker && params == b"201" && self.paste {
            emit(self.token(Event::PasteEnd));
            self.paste = false;
        } else if !self.output && plain && final_byte == b'M' && params.is_empty() {
            self.state = State::X10 { remaining: 3 };
        } else {
            emit(Token {
                event: Event::Csi(Csi {
                    private: self.private,
                    params,
                    intermediates,
                    final_byte,
                }),
                in_paste: self.paste,
            });
        }
    }

    fn string(&mut self, kind: StringKind, esc: bool, byte: u8, emit: &mut dyn FnMut(Token<'_>)) {
        if esc {
            if byte == b'\\' {
                self.state = State::Ground;
                self.dispatch_string(kind, Terminator::St, emit);
            } else {
                emit(self.token(Event::Abandoned));
                self.state = State::Esc;
                self.escape(byte, emit);
            }
            return;
        }
        match byte {
            ESC => self.state = State::Str { kind, esc: true },
            BEL if kind == StringKind::Osc => {
                self.state = State::Ground;
                self.dispatch_string(kind, Terminator::Bel, emit);
            }
            BEL => emit(self.token(Event::Bel(BelAt::String))),
            CAN | SUB => {
                emit(self.token(Event::Abandoned));
                self.state = State::Ground;
            }
            0x00..=0x1f | DEL => emit(self.token(Event::Inside(byte))),
            _ => {
                self.push(byte);
            }
        }
    }

    fn dispatch_string(
        &mut self,
        kind: StringKind,
        terminator: Terminator,
        emit: &mut dyn FnMut(Token<'_>),
    ) {
        let payload = (!self.malformed).then_some(&self.buf[..self.len]);
        emit(Token {
            event: Event::Str(Str {
                kind,
                payload,
                terminator,
            }),
            in_paste: self.paste,
        });
    }
}

impl<'a> Csi<'a> {
    /// The `;`-separated parameters, in order. No parameter bytes is no
    /// parameters.
    pub fn params(&self) -> impl Iterator<Item = Param<'a>> + use<'a> {
        let params = self.params;
        params
            .split(|&b| b == b';')
            .take(if params.is_empty() { 0 } else { usize::MAX })
            .map(Param)
    }
}

impl<'a> Param<'a> {
    /// Its `:`-separated sub-parameters: each a decimal number (saturating
    /// at `u32::MAX`), or `None` when empty or not all digits.
    pub fn subparams(&self) -> impl Iterator<Item = Option<u32>> + use<'a> {
        self.0.split(|&b| b == b':').map(number)
    }

    /// Its first sub-parameter.
    pub fn value(&self) -> Option<u32> {
        self.subparams().next().flatten()
    }
}

fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(digits.iter().fold(0u32, |n, &d| {
        n.saturating_mul(10).saturating_add(u32::from(d - b'0'))
    }))
}

impl Token<'_> {
    /// The Ctrl+G candidate this token is, if any (core §4.4).
    pub fn candidate(&self) -> Option<Candidate> {
        let form = match self.event {
            Event::Bel(BelAt::Ground) => CandidateForm::Bel,
            Event::Bel(BelAt::Sequence) => CandidateForm::BelInSequence,
            Event::Bel(BelAt::String) => CandidateForm::BelInString,
            Event::EscBel => CandidateForm::EscBel,
            Event::Csi(csi) if csi.final_byte == b'u' && csi_u_ctrl_g(&csi) => CandidateForm::CsiU,
            Event::Csi(csi) if csi.final_byte == b'~' && modify_other_keys_ctrl_g(&csi) => {
                CandidateForm::ModifyOtherKeys
            }
            Event::MalformedCsi {
                final_byte: b'u' | b'~',
            } => CandidateForm::MalformedKey,
            Event::Str(Str {
                kind: StringKind::Osc,
                terminator: Terminator::Bel,
                ..
            }) => CandidateForm::BelEndsOsc,
            _ => return None,
        };
        Some(Candidate {
            form,
            in_paste: self.in_paste,
        })
    }

    /// What this token did to the input box, if it was a submit or a clear
    /// (OQ2). Whose byte it was is the caller's to know.
    pub fn edit(&self) -> Option<Edit> {
        if self.in_paste {
            return None;
        }
        match self.event {
            Event::Plain(b'\r') => Some(Edit::Submit),
            Event::Plain(0x15 | 0x03) => Some(Edit::Clear),
            _ => None,
        }
    }
}

/// Kitty's `CSI key[:shifted[:base]] ; mods[:event] ; text u`: any of the
/// first parameter's keys is `g` or `G`.
fn csi_u_ctrl_g(csi: &Csi<'_>) -> bool {
    csi.params().next().is_some_and(|key| {
        key.subparams()
            .any(|code| code.is_some_and(|code| CTRL_G_KEYS.contains(&code)))
    })
}

/// `CSI 27 ; mods ; key ~`, with any later parameter `g` or `G`.
fn modify_other_keys_ctrl_g(csi: &Csi<'_>) -> bool {
    let mut params = csi.params();
    params.next().and_then(|p| p.value()) == Some(MODIFY_OTHER_KEYS)
        && params.any(|p| p.value().is_some_and(|key| CTRL_G_KEYS.contains(&key)))
}

/// The report `op` is, when it is exactly one whole report read from
/// `start`: `start` at a boundary (no paste open, no sequence carried from an
/// earlier op), the op's only token completed by its last byte, and that
/// token a [`Report`] with each parameter in range. Split, concatenated or
/// prefixed reports are `None`. Whether the report's mode is on (D3) or its
/// query outstanding (D3′) is the caller's to check.
pub fn whole_report(start: &Framing, op: &[u8]) -> Option<Report> {
    if !start.at_ground() {
        return None;
    }
    let mut framing = start.clone();
    let mut tokens = 0usize;
    let mut report = None;
    for &byte in op {
        framing.feed(byte, |token| {
            tokens += 1;
            report = report_of(&token);
        });
    }
    // One token, and back at a boundary, means the op's last byte completed
    // it: from ground, every byte either completes a token or opens a
    // sequence, and every way back to ground completes one.
    if tokens == 1 && framing.at_ground() {
        report
    } else {
        None
    }
}

fn report_of(token: &Token<'_>) -> Option<Report> {
    match token.event {
        Event::Csi(csi) => csi_report(&csi),
        Event::Str(Str {
            kind: StringKind::Osc,
            payload: Some(payload),
            ..
        }) => osc_colour(payload),
        _ => None,
    }
}

fn csi_report(csi: &Csi<'_>) -> Option<Report> {
    if !csi.intermediates.is_empty() {
        return None;
    }
    let values: Option<Vec<u16>> = csi
        .params()
        .map(|p| {
            let mut subparams = p.subparams();
            let value = subparams.next().flatten()?;
            if subparams.next().is_some() {
                return None;
            }
            u16::try_from(value).ok()
        })
        .collect();
    let values = values?;
    match (csi.private, csi.final_byte, values.as_slice()) {
        (None, b'I', []) => Some(Report::FocusIn),
        (None, b'O', []) => Some(Report::FocusOut),
        (Some(b'<'), b'M' | b'm', [_, x, y]) if *x > 0 && *y > 0 => Some(Report::SgrMouse),
        (None, b'R', &[row, col]) if row > 0 && col > 0 => Some(Report::Cpr { row, col }),
        (Some(b'?'), b'c', [_, ..]) => Some(Report::Da1),
        _ => None,
    }
}

fn osc_colour(payload: &[u8]) -> Option<Report> {
    let (slot, rest) = if let Some(rest) = payload.strip_prefix(b"10;rgb:") {
        (10, rest)
    } else {
        (11, payload.strip_prefix(b"11;rgb:")?)
    };
    let mut channels = 0;
    for channel in rest.split(|&b| b == b'/') {
        channels += 1;
        if !(1..=4).contains(&channel.len()) || !channel.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
    }
    (channels == 3).then_some(Report::OscColour { slot })
}
