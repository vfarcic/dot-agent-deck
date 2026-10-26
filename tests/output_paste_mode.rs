//! PRD #1666 part 3 (Q35-02): the bracketed-paste tracker over an agent's
//! output (`src/composer_probe/paste_mode.rs`) and the output grammar of the
//! framing tokenizer it reads through, pure. The reference is the deck's own
//! `vt100` parser, fed the same bytes.

use std::num::NonZeroU16;

use dot_agent_deck::composer_probe::framing::{BelAt, Event, Framing, MAX_SEQUENCE_BYTES};
use dot_agent_deck::composer_probe::paste_mode::{OutputHook, PasteModeTracker};
use spec::spec;

fn vt100_parser() -> vt100::Parser {
    vt100::Parser::new(
        NonZeroU16::new(24).unwrap(),
        NonZeroU16::new(80).unwrap(),
        0,
    )
}

/// Mode 2004 after `bytes`, by the tracker and by `vt100`.
fn both(bytes: &[u8]) -> (bool, bool) {
    let mut tracker = PasteModeTracker::default();
    tracker.feed(bytes);
    let mut parser = vt100_parser();
    parser.process(bytes);
    (tracker.on(), parser.screen().bracketed_paste())
}

/// `n` empty parameters' separators.
fn semis(n: usize) -> Vec<u8> {
    vec![b';'; n]
}

/// Scenario: Feed an agent's output that turns bracketed paste on and off, combined with other modes, with sub-parameters, other private markers and near numbers, and with RIS; then the same streams cut into two pushes at every offset and one byte per push. The tracker reports the mode `vt100` reports after each, however the bytes were split.
#[spec("inbox/probe/004")]
#[test]
fn probe_004_set_reset_and_ris_however_split() {
    let cases: Vec<(Vec<u8>, bool)> = vec![
        (b"\x1b[?2004h".to_vec(), true),
        (b"\x1b[?2004h\x1b[?2004l".to_vec(), false),
        (b"\x1b[?1049h\x1b[?2004h\x1b[?1049l".to_vec(), true),
        (b"\x1b[?1;2004h".to_vec(), true),
        (b"\x1b[?2004;1049h".to_vec(), true),
        (b"\x1b[?2004h\x1b[?1049;2004l".to_vec(), false),
        (b"\x1b[?2004;2004h".to_vec(), true),
        (b"\x1b[?02004h".to_vec(), true),
        (b"\x1b[?12004h".to_vec(), false),
        (b"\x1b[?200h".to_vec(), false),
        (b"\x1b[?2004:1h".to_vec(), false),
        (b"\x1b[?:2004h".to_vec(), false),
        (b"\x1b[2004h".to_vec(), false),
        (b"\x1b[>2004h".to_vec(), false),
        (b"\x1b[=2004h".to_vec(), false),
        (b"\x1b[?2004m".to_vec(), false),
        (b"\x1b[?2004h\x1bc".to_vec(), false),
        (b"\x1b[?2004h\x1bctext\x1b[?2004h".to_vec(), true),
        (b"\x1b[?2004h\x1b[?2004s".to_vec(), true),
        (b"\x1b[?2004h\x1b[!p".to_vec(), true),
        // vte keeps 32 parameter values: the 32nd counts, the 33rd doesn't,
        // and a parameter cut short at the 32nd keeps what it got.
        ([&b"\x1b[?"[..], &semis(31), b"2004h"].concat(), true),
        ([&b"\x1b[?"[..], &semis(32), b"2004h"].concat(), false),
        ([&b"\x1b[?"[..], &semis(31), b"2004:1h"].concat(), true),
        ([&b"\x1b[?"[..], &semis(30), b"2004:1h"].concat(), false),
        (
            [&b"\x1b[?"[..], &b"1:"[..].repeat(15), b"1;2004h"].concat(),
            true,
        ),
    ];
    for (bytes, on) in &cases {
        assert_eq!(
            both(bytes),
            (*on, *on),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );

        for cut in 0..=bytes.len() {
            let mut tracker = PasteModeTracker::default();
            tracker.feed(&bytes[..cut]);
            tracker.feed(&bytes[cut..]);
            assert_eq!(tracker.on(), *on, "{bytes:?} cut at {cut}");
        }
        let mut tracker = PasteModeTracker::default();
        for byte in bytes {
            tracker.feed(std::slice::from_ref(byte));
        }
        assert_eq!(tracker.on(), *on, "{bytes:?} a byte at a time");
    }
}

/// Scenario: Feed output where `?2004h` appears as text inside an OSC title, a DCS, an APC, a PM and a SOS, and where the string is cut short by CAN; then strings that an ESC ends with the mode change right after it. Text inside a string changes nothing; an ESC ends the string, so a mode change after it counts, as `vt100` reads both.
#[spec("inbox/probe/004")]
#[test]
fn probe_004_strings_hold_no_mode_change() {
    for (bytes, on) in [
        (&b"\x1b]0;[?2004h\x07"[..], false),
        (b"\x1b]0;?2004h\x1b\\", false),
        (b"\x1bPq[?2004h\x1b\\", false),
        (b"\x1b_[?2004h\x07\x1b\\", false),
        (b"\x1b^[?2004h\x1b\\", false),
        (b"\x1bX[?2004h\x1b\\", false),
        (b"\x1b]0;title\x18[?2004h", false),
        (b"\x1b]0;title\x1b[?2004h", true),
        (b"\x1bPq\x1b[?2004h", true),
        (b"\x1b_x\x1b[?2004h", true),
        (b"\x1b^x\x07\x1b[?2004h", true),
        (b"\x1bXx\x1b[?2004h", true),
        (b"\x1b]0;title\x07\x1b[?2004h", true),
    ] {
        assert_eq!(
            both(bytes),
            (on, on),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// Scenario: Feed output with a C0 control, DEL or a high byte between an ESC and the rest of its sequence, a control before a CSI's private marker, a private marker after the first parameter byte, ESC sequences with intermediate bytes, `ESC O` and `CSI M` followed by more output, and a CSI abandoned by CAN or a new ESC. The tracker's mode 2004 matches `vt100`'s after each, including `ESC NUL [?2004h` and `ESC [ NUL ?2004h` turning it on and `ESC NUL c` resetting it.
#[spec("inbox/probe/004")]
#[test]
fn probe_004_controls_escapes_and_private_markers_read_as_vt100_does() {
    let on = b"\x1b[?2004h";
    let cases: Vec<(Vec<u8>, bool)> = vec![
        // A control, DEL or high byte after an ESC is executed or skipped,
        // and the escape goes on.
        (b"\x1b\x00[?2004h".to_vec(), true),
        (b"\x1b\r[?2004h".to_vec(), true),
        (b"\x1b\x07[?2004h".to_vec(), true),
        (b"\x1b\x7f[?2004h".to_vec(), true),
        (b"\x1b\x80[?2004h".to_vec(), true),
        (b"\x1b\xc3[?2004h".to_vec(), true),
        ([&on[..], b"\x1b\x00c"].concat(), false),
        ([&on[..], b"\x1b\x7f\x0ac"].concat(), false),
        // Inside a CSI too, before and after its private marker.
        (b"\x1b[\x00?2004h".to_vec(), true),
        (b"\x1b[\r\n?2004h".to_vec(), true),
        (b"\x1b[?\x002004h".to_vec(), true),
        (b"\x1b[?20\x0704h".to_vec(), true),
        (b"\x1b[?2004\x7fh".to_vec(), true),
        (b"\x1b[?20\xc304h".to_vec(), true),
        (b"\x1b[?2004\x9bh".to_vec(), true),
        // A private marker after the first byte: no dispatch.
        (b"\x1b[?2004?h".to_vec(), false),
        (b"\x1b[1?2004h".to_vec(), false),
        (b"\x1b[??2004h".to_vec(), false),
        (b"\x1b[>?2004h".to_vec(), false),
        ([&on[..], b"\x1b[?2004?l"].concat(), true),
        // Intermediates: after the parameters they don't stop it; before
        // them the CSI is ignored.
        (b"\x1b[?2004$h".to_vec(), true),
        (b"\x1b[?2004 !h".to_vec(), true),
        (b"\x1b[$?2004h".to_vec(), false),
        (b"\x1b[?$2004h".to_vec(), false),
        // An ESC with intermediates runs to its final byte, which `[` is.
        (b"\x1b([?2004h".to_vec(), false),
        (b"\x1b( [?2004h".to_vec(), false),
        (b"\x1b(\x00B\x1b[?2004h".to_vec(), true),
        (b"\x1b(\x1b[?2004h".to_vec(), true),
        (b"\x1b#8\x1b[?2004h".to_vec(), true),
        ([&on[..], b"\x1b c"].concat(), true),
        ([&on[..], b"\x1b(c"].concat(), true),
        ([&on[..], b"\x1b(\x18c"].concat(), true),
        // `ESC O` and `CSI M` are ordinary sequences on output.
        (b"\x1bO[?2004h".to_vec(), false),
        (b"\x1bO\x1b[?2004h".to_vec(), true),
        (b"\x1b[Mab\x1b[?2004h".to_vec(), true),
        (b"\x1b[M\x1b[?2004h".to_vec(), true),
        // Abandoned.
        (b"\x1b[?2004\x18h".to_vec(), false),
        (b"\x1b[?2004\x1ah".to_vec(), false),
        (b"\x1b[?2004\x1bh".to_vec(), false),
        (b"\x1b\x18[?2004h".to_vec(), false),
        (b"\x1b\x1b[?2004h".to_vec(), true),
        // A C1 CSI byte is no CSI.
        (b"\x9b?2004h".to_vec(), false),
        ("\u{9b}?2004h".as_bytes().to_vec(), false),
    ];
    for (bytes, on) in &cases {
        assert_eq!(
            both(bytes),
            (*on, *on),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// Scenario: Where the tracker is known to differ from `vt100`: a mode change longer than the 64-byte sequence cap. `vt100` applies it and the tracker changes nothing, so an overlong enable leaves the tracker off where `vt100` is on, and an overlong disable after an ordinary enable leaves it on where `vt100` is off; at the cap the two agree.
#[spec("inbox/probe/004")]
#[test]
fn probe_004_the_known_overlong_difference_is_pinned() {
    let mut overlong = b"\x1b[?".to_vec();
    overlong.extend(std::iter::repeat_n(b'0', MAX_SEQUENCE_BYTES));
    overlong.extend_from_slice(b"2004h");
    assert_eq!(both(&overlong), (false, true));

    let mut stale_on = b"\x1b[?2004h\x1b[?".to_vec();
    stale_on.extend(std::iter::repeat_n(b'0', MAX_SEQUENCE_BYTES));
    stale_on.extend_from_slice(b"2004l");
    assert_eq!(both(&stale_on), (true, false));

    let mut capped = b"\x1b[?".to_vec();
    capped.extend(std::iter::repeat_n(b'0', MAX_SEQUENCE_BYTES - 4));
    capped.extend_from_slice(b"2004h");
    assert_eq!(both(&capped), (true, true));
}

/// A deterministic xorshift generator, so a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// What a generated case is built from: the pieces of mode changes, RIS and
/// strings, every kind of control, and UTF-8 and stray high bytes.
const PIECES: &[&[u8]] = &[
    b"\x1b",
    b"[",
    b"?",
    b"2004",
    b"20",
    b"04",
    b"h",
    b"l",
    b";",
    b":",
    b"1",
    b"0",
    b"\x00",
    b"\x07",
    b"\r",
    b"\x7f",
    b"\x18",
    b"\x1a",
    b"\x80",
    b"\x9b",
    b"\x9c",
    b"\xc3\xa9",
    b"\xe2",
    b"\xe2\x82",
    b" ",
    b"(",
    b"$",
    b"c",
    b"]",
    b"P",
    b"X",
    b"^",
    b"_",
    b"\\",
    b"O",
    b"M",
    b">",
    b"x",
    b"\x1b[?2004h",
    b"\x1b[?2004l",
    b"\x1bc",
];

/// Scenario: Generate 20,000 short output streams from the pieces of mode changes, RIS, strings, controls and stray high bytes, each followed by CAN, and push them in randomly cut chunks, then a byte at a time. After every push the tracker's mode 2004 is the one `vt100` reports for the same bytes.
#[spec("inbox/probe/004")]
#[test]
fn probe_004_agrees_with_vt100_on_generated_output() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut stream = Vec::new();
    for _ in 0..20_000 {
        // A case short enough that no CSI in it passes the 64-byte cap,
        // ended by CAN so none runs into the next.
        loop {
            let mut case = Vec::new();
            for _ in 0..rng.below(14) + 1 {
                case.extend_from_slice(PIECES[rng.below(PIECES.len())]);
            }
            if case.len() < MAX_SEQUENCE_BYTES - 2 {
                stream.extend_from_slice(&case);
                break;
            }
        }
        stream.push(0x18);
    }

    let mut tracker = PasteModeTracker::default();
    let mut parser = vt100_parser();
    let mut at = 0;
    let mut changes = 0usize;
    let mut last = false;
    while at < stream.len() {
        let end = (at + rng.below(24) + 1).min(stream.len());
        tracker.feed(&stream[at..end]);
        parser.process(&stream[at..end]);
        assert_eq!(
            tracker.on(),
            parser.screen().bracketed_paste(),
            "after {:?}",
            String::from_utf8_lossy(&stream[at.saturating_sub(64)..end])
        );
        if tracker.on() != last {
            changes += 1;
            last = tracker.on();
        }
        at = end;
    }
    // The stream exercised the mode, not just its default.
    assert!(changes > 1000, "{changes} changes");

    let mut tracker = PasteModeTracker::default();
    let mut parser = vt100_parser();
    for (i, byte) in stream.iter().enumerate().take(200_000) {
        tracker.feed(std::slice::from_ref(byte));
        parser.process(std::slice::from_ref(byte));
        assert_eq!(tracker.on(), parser.screen().bracketed_paste(), "byte {i}");
    }
}

/// An owned copy of an output token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Out {
    Plain(u8),
    Bel(BelAt),
    Escape(u8),
    IntermediateEscape(u8),
    Csi {
        private: Option<u8>,
        params: Vec<u8>,
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    Inside(u8),
    Abandoned,
    Other,
}

fn output_tokens(bytes: &[u8]) -> (Vec<Out>, bool) {
    let mut framing = Framing::output();
    let mut out = Vec::new();
    for &byte in bytes {
        framing.feed(byte, |token| {
            assert!(!token.in_paste, "output has no paste");
            out.push(match token.event {
                Event::Plain(b) => Out::Plain(b),
                Event::Bel(at) => Out::Bel(at),
                Event::Escape(b) => Out::Escape(b),
                Event::IntermediateEscape { final_byte } => Out::IntermediateEscape(final_byte),
                Event::Csi(csi) => Out::Csi {
                    private: csi.private,
                    params: csi.params.to_vec(),
                    intermediates: csi.intermediates.to_vec(),
                    final_byte: csi.final_byte,
                },
                Event::Inside(b) => Out::Inside(b),
                Event::Abandoned => Out::Abandoned,
                _ => Out::Other,
            });
        });
    }
    (out, framing.at_ground())
}

fn csi(private: Option<u8>, params: &[u8], final_byte: u8) -> Out {
    Out::Csi {
        private,
        params: params.to_vec(),
        intermediates: Vec::new(),
        final_byte,
    }
}

/// Scenario: Feed the output grammar an ESC with intermediates, controls and DEL after an ESC, `ESC O`, `CSI M`, paste markers, a high byte inside a CSI and a control before a private marker. Each comes out as the tokens the output grammar promises, the stream is at a boundary only once the escape reaches its final byte, and no output token is ever inside a paste.
#[spec("inbox/probe/004")]
#[test]
fn probe_004_the_output_grammar_frames_escapes_to_their_final_byte() {
    assert_eq!(output_tokens(b"\x1b("), (vec![], false));
    assert_eq!(output_tokens(b"\x1b( #"), (vec![], false));
    assert_eq!(
        output_tokens(b"\x1b(B"),
        (vec![Out::IntermediateEscape(b'B')], true)
    );
    assert_eq!(
        output_tokens(b"\x1b(\x00\x07\x7f\xc3B"),
        (
            vec![
                Out::Inside(0),
                Out::Bel(BelAt::Sequence),
                Out::Inside(0x7f),
                Out::IntermediateEscape(b'B'),
            ],
            true
        )
    );
    assert_eq!(
        output_tokens(b"\x1b(\x1b(B"),
        (vec![Out::Abandoned, Out::IntermediateEscape(b'B')], true)
    );
    assert_eq!(
        output_tokens(b"\x1b(\x18x"),
        (vec![Out::Abandoned, Out::Plain(b'x')], true)
    );
    assert_eq!(output_tokens(b"\x1b\x00"), (vec![Out::Inside(0)], false));
    assert_eq!(
        output_tokens(b"\x1b\x00\x07\x7f\x80c"),
        (
            vec![
                Out::Inside(0),
                Out::Bel(BelAt::Sequence),
                Out::Inside(0x7f),
                Out::Escape(b'c'),
            ],
            true
        )
    );
    assert_eq!(
        output_tokens(b"\x1bOP"),
        (vec![Out::Escape(b'O'), Out::Plain(b'P')], true)
    );
    assert_eq!(
        output_tokens(b"\x1b[Mab"),
        (
            vec![csi(None, b"", b'M'), Out::Plain(b'a'), Out::Plain(b'b')],
            true
        )
    );
    assert_eq!(
        output_tokens(b"\x1b[200~x\x1b[201~"),
        (
            vec![
                csi(None, b"200", b'~'),
                Out::Plain(b'x'),
                csi(None, b"201", b'~')
            ],
            true
        )
    );
    assert_eq!(
        output_tokens(b"\x1b[?20\xc304h"),
        (vec![csi(Some(b'?'), b"2004", b'h')], true)
    );
    assert_eq!(
        output_tokens(b"\x1b[\x00?2004h"),
        (vec![Out::Inside(0), csi(Some(b'?'), b"2004", b'h')], true)
    );
    assert!(matches!(
        output_tokens(b"\x1b[1?h").0.as_slice(),
        [Out::Other]
    ));
}

/// Scenario: Feed the output hook's tracker through `OutputHook::on_output` a mode change split over two calls. The hook's tracker reports it, as the tracker alone does.
#[spec("inbox/probe/004")]
#[test]
fn probe_004_the_hook_feeds_the_tracker() {
    let mut hook = OutputHook::default();
    assert!(!hook.paste_mode().on());
    hook.on_output(b"text\x1b[?20");
    assert!(!hook.paste_mode().on());
    hook.on_output(b"04hmore");
    assert!(hook.paste_mode().on());
    hook.on_output(b"\x1bc");
    assert!(!hook.paste_mode().on());
}
