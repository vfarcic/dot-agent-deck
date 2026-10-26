//! PRD #1666 part 3 (Q35-01): the input framing tokenizer
//! (`src/composer_probe/framing.rs`), pure. Bytes go in, and the tests read
//! the tokens, candidates, reports and edits that come out.

use dot_agent_deck::composer_probe::framing::{
    BelAt, Candidate, CandidateForm, Edit, Event, Framing, MAX_SEQUENCE_BYTES, Report, StringKind,
    Terminator, whole_report,
};
use spec::spec;

/// An owned copy of a token, for comparing streams.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Plain(u8),
    Bel(BelAt),
    EscBel,
    Escape(u8),
    Ss3(u8),
    Csi {
        private: Option<u8>,
        params: Vec<u8>,
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    MalformedCsi(u8),
    X10Mouse,
    Str {
        kind: StringKind,
        payload: Option<Vec<u8>>,
        terminator: Terminator,
    },
    Inside(u8),
    Abandoned,
    PasteStart,
    PasteEnd,
}

/// One token as the tests compare it: the offset of the byte that
/// completed it, what it was, whether it was inside a paste, and what it is
/// as a candidate and as an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tok {
    at: usize,
    seen: Seen,
    in_paste: bool,
    candidate: Option<Candidate>,
    edit: Option<Edit>,
}

fn feed_into(framing: &mut Framing, bytes: &[u8], offset: usize, out: &mut Vec<Tok>) {
    for (i, &byte) in bytes.iter().enumerate() {
        framing.feed(byte, |token| {
            let seen = match token.event {
                Event::Plain(b) => Seen::Plain(b),
                Event::Bel(at) => Seen::Bel(at),
                Event::EscBel => Seen::EscBel,
                Event::Escape(b) => Seen::Escape(b),
                Event::Ss3(b) => Seen::Ss3(b),
                Event::Csi(csi) => Seen::Csi {
                    private: csi.private,
                    params: csi.params.to_vec(),
                    intermediates: csi.intermediates.to_vec(),
                    final_byte: csi.final_byte,
                },
                Event::MalformedCsi { final_byte } => Seen::MalformedCsi(final_byte),
                Event::X10Mouse => Seen::X10Mouse,
                Event::Str(s) => Seen::Str {
                    kind: s.kind,
                    payload: s.payload.map(<[u8]>::to_vec),
                    terminator: s.terminator,
                },
                Event::Inside(b) => Seen::Inside(b),
                Event::Abandoned => Seen::Abandoned,
                Event::PasteStart => Seen::PasteStart,
                Event::PasteEnd => Seen::PasteEnd,
            };
            out.push(Tok {
                at: offset + i,
                seen,
                in_paste: token.in_paste,
                candidate: token.candidate(),
                edit: token.edit(),
            });
        });
    }
}

fn tokens(bytes: &[u8]) -> Vec<Tok> {
    let mut out = Vec::new();
    feed_into(&mut Framing::default(), bytes, 0, &mut out);
    out
}

/// The candidates in `bytes`, each with the offset of its last byte.
fn candidates(bytes: &[u8]) -> Vec<(usize, Candidate)> {
    tokens(bytes)
        .into_iter()
        .filter_map(|t| t.candidate.map(|c| (t.at, c)))
        .collect()
}

fn edits(bytes: &[u8]) -> Vec<(usize, Edit)> {
    tokens(bytes)
        .into_iter()
        .filter_map(|t| t.edit.map(|e| (t.at, e)))
        .collect()
}

fn at_ground_after(bytes: &[u8]) -> bool {
    let mut framing = Framing::default();
    for &b in bytes {
        framing.feed(b, |_| {});
    }
    framing.at_ground()
}

fn cand(form: CandidateForm) -> Candidate {
    Candidate {
        form,
        in_paste: false,
    }
}

fn pasted(form: CandidateForm) -> Candidate {
    Candidate {
        form,
        in_paste: true,
    }
}

/// Every candidate form of core §4.4 and R3 §4.2, alone, with the form it
/// must be found as.
fn candidate_cases() -> Vec<(Vec<u8>, CandidateForm)> {
    use CandidateForm::*;
    let mut cases: Vec<(Vec<u8>, CandidateForm)> = vec![
        (b"\x07".to_vec(), Bel),
        (b"\x1b\x07".to_vec(), EscBel),
        // CSI-u, both cases, with and without modifiers, every event type,
        // and the shifted and base-layout keys.
        (b"\x1b[103u".to_vec(), CsiU),
        (b"\x1b[71u".to_vec(), CsiU),
        (b"\x1b[103;5u".to_vec(), CsiU),
        (b"\x1b[71;5u".to_vec(), CsiU),
        (b"\x1b[71;6u".to_vec(), CsiU),
        (b"\x1b[103;1u".to_vec(), CsiU),
        (b"\x1b[103;3u".to_vec(), CsiU),
        (b"\x1b[103;5:1u".to_vec(), CsiU),
        (b"\x1b[103;5:2u".to_vec(), CsiU),
        (b"\x1b[103;5:3u".to_vec(), CsiU),
        (b"\x1b[71;6:3u".to_vec(), CsiU),
        (b"\x1b[103:71;6u".to_vec(), CsiU),
        (b"\x1b[1087::103;5u".to_vec(), CsiU),
        (b"\x1b[103;5;7u".to_vec(), CsiU),
        (b"\x1b[?103;5u".to_vec(), CsiU),
        // modifyOtherKeys, both cases, any modifiers.
        (b"\x1b[27;5;103~".to_vec(), ModifyOtherKeys),
        (b"\x1b[27;5;71~".to_vec(), ModifyOtherKeys),
        (b"\x1b[27;6;71~".to_vec(), ModifyOtherKeys),
        (b"\x1b[27;1;103~".to_vec(), ModifyOtherKeys),
        (b"\x1b[27;;103~".to_vec(), ModifyOtherKeys),
        // A BEL in an ambiguous place.
        (b"\x1b[1;\x07".to_vec(), BelInSequence),
        (b"\x1b]11;rgb:0/0/0\x07".to_vec(), BelEndsOsc),
        (b"\x1b]0;a title\x07".to_vec(), BelEndsOsc),
        (b"\x1b]\x07".to_vec(), BelEndsOsc),
        (b"\x1bP1$r\x07".to_vec(), BelInString),
        (b"\x1b_x\x07".to_vec(), BelInString),
        (b"\x1b^x\x07".to_vec(), BelInString),
        (b"\x1bXx\x07".to_vec(), BelInString),
    ];
    // An overlong CSI-u and modifyOtherKeys: what was kept can't rule a
    // Ctrl+G out.
    let mut overlong = b"\x1b[103;".to_vec();
    overlong.extend(std::iter::repeat_n(b'1', MAX_SEQUENCE_BYTES));
    let mut overlong_u = overlong.clone();
    overlong_u.push(b'u');
    cases.push((overlong_u, MalformedKey));
    let mut overlong_tilde = overlong;
    overlong_tilde.push(b'~');
    cases.push((overlong_tilde, MalformedKey));
    cases
}

/// Scenario: Feed every Ctrl+G form the deck must treat as a candidate (BEL, Alt+Ctrl+G, every CSI-u and modifyOtherKeys spelling of Ctrl+G with any modifiers and event type, a BEL inside a sequence or string, and a malformed key sequence) into a fresh tokenizer. Each is found exactly once, as its own form, at its last byte; afterwards the stream is back at a boundary, except after a BEL inside an unfinished sequence or string, which goes on.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_every_candidate_form_is_found_once_at_its_last_byte() {
    for (bytes, form) in candidate_cases() {
        let found = candidates(&bytes);
        assert_eq!(
            found,
            vec![(bytes.len() - 1, cand(form))],
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
        // A BEL inside a sequence leaves the sequence going on.
        let ground = form != CandidateForm::BelInSequence && form != CandidateForm::BelInString;
        assert_eq!(
            at_ground_after(&bytes),
            ground,
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

/// Scenario: Feed keys and reports that look like Ctrl+G but are not (other keys in the same encodings, `g` typed plainly, Alt+g, an OSC ended by ST, a CSI-u whose key is not g). None of them is a candidate.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_lookalikes_are_not_candidates() {
    for bytes in [
        &b"g"[..],
        b"G",
        b"\x1bg",
        b"\x1bG",
        b"\x1b[104;5u",
        b"\x1b[5;103u",
        b"\x1b[27;5;104~",
        b"\x1b[103~",
        b"\x1b[103;5~",
        b"\x1b[28;5;103~",
        b"\x1b[27;5;103u",
        b"\x1b]11;rgb:0/0/0\x1b\\",
        b"\x1bP1$r\x1b\\",
        b"\x1b[A",
        b"\x1bOP",
        b"\x1b[200~text\x1b[201~",
        b"\x1b[<0;10;5M",
        b"\x1b[I",
        b"\r\n\x15\x03\x7f\t",
    ] {
        assert_eq!(
            candidates(bytes),
            vec![],
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// Scenario: Feed each Ctrl+G form between ordinary typing, then the same stream split into two writes at every offset and one byte per write, carrying the tokenizer state across. The same candidates come out at the same offsets however the stream is split.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_splits_across_writes_change_nothing() {
    let mut stream = b"hello ".to_vec();
    for (bytes, _) in candidate_cases() {
        stream.extend_from_slice(&bytes);
        // Close whatever a BEL inside a sequence or string left open.
        stream.extend_from_slice(b"\x18 typed ");
    }
    stream.extend_from_slice(b"\x1b[200~pasted \x07 and \x1b[103;5u\x1b[201~ after");
    let whole = tokens(&stream);
    assert!(whole.iter().filter(|t| t.candidate.is_some()).count() >= candidate_cases().len());

    for cut in 0..=stream.len() {
        let mut framing = Framing::default();
        let mut out = Vec::new();
        feed_into(&mut framing, &stream[..cut], 0, &mut out);
        // The planner carries a cloned state from one write to the next.
        let mut next = framing.clone();
        feed_into(&mut next, &stream[cut..], cut, &mut out);
        assert_eq!(out, whole, "cut at {cut}");
    }

    let mut framing = Framing::default();
    let mut out = Vec::new();
    for (i, byte) in stream.iter().enumerate() {
        feed_into(&mut framing, std::slice::from_ref(byte), i, &mut out);
    }
    assert_eq!(out, whole);
}

/// Scenario: Open a bracketed paste and put each Ctrl+G form inside it, including after an OSC and a CSI left unfinished in the paste. Every form is still a candidate, marked as inside the paste, and the paste closes at the exact closing marker, while an opening marker or a private-marked, longer or extra-parameter closing marker inside it does not close it.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_candidates_inside_a_paste_are_found_and_marked() {
    for (bytes, form) in candidate_cases() {
        let mut stream = b"\x1b[200~".to_vec();
        stream.extend_from_slice(&bytes);
        stream.extend_from_slice(b"\x1b[201~");
        let found = candidates(&stream);
        assert_eq!(
            found,
            vec![(6 + bytes.len() - 1, pasted(form))],
            "{:?}",
            String::from_utf8_lossy(&stream)
        );
        assert!(at_ground_after(&stream), "{stream:?}");
    }

    // The closing marker closes the paste after an unfinished OSC or CSI.
    for unfinished in [
        &b"\x1b]0;title"[..],
        b"\x1b[1;",
        b"\x1b",
        b"\x1bO",
        b"\x1bP1$",
    ] {
        let mut stream = b"\x1b[200~".to_vec();
        stream.extend_from_slice(unfinished);
        stream.extend_from_slice(b"\x1b[201~\x07");
        let found = tokens(&stream);
        assert!(found.iter().any(|t| t.seen == Seen::PasteEnd), "{stream:?}");
        assert_eq!(
            candidates(&stream),
            vec![(stream.len() - 1, cand(CandidateForm::Bel))]
        );
    }

    // Inside a paste an opening marker is text, and only the exact closing
    // marker closes it.
    let stream = b"\x1b[200~a\x1b[200~b\x1b[?201~c\x1b[2010~d\x1b[201;1~e\x07";
    let found = tokens(stream);
    assert_eq!(
        found.iter().filter(|t| t.seen == Seen::PasteStart).count(),
        1
    );
    assert!(!found.iter().any(|t| t.seen == Seen::PasteEnd));
    assert_eq!(
        candidates(stream),
        vec![(stream.len() - 1, pasted(CandidateForm::Bel))]
    );
    assert!(!at_ground_after(stream));
    // A closing marker outside a paste is an ordinary sequence.
    assert!(matches!(
        tokens(b"\x1b[201~").as_slice(),
        [Tok {
            seen: Seen::Csi { .. },
            in_paste: false,
            ..
        }]
    ));
}

/// The tokens of `stream` fed in two writes cut at every offset, the state
/// cloned at the cut, each equal to the tokens of one write.
fn assert_split_invariant(stream: &[u8]) {
    let whole = tokens(stream);
    for cut in 0..=stream.len() {
        let mut framing = Framing::default();
        let mut out = Vec::new();
        feed_into(&mut framing, &stream[..cut], 0, &mut out);
        let mut next = framing.clone();
        feed_into(&mut next, &stream[cut..], cut, &mut out);
        assert_eq!(out, whole, "{stream:?} cut at {cut}");
    }
}

fn state_after(bytes: &[u8]) -> Framing {
    let mut framing = Framing::default();
    for &b in bytes {
        framing.feed(b, |_| {});
    }
    framing
}

/// `marker` with `controls` inserted before its byte at `at`: every
/// spelling of a marker with one control or DEL anywhere in it, and before
/// and after it.
fn polluted_markers(marker: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for control in (0x00..=0x1fu8).chain([0x7f]) {
        for at in 0..=marker.len() {
            let mut polluted = marker[..at].to_vec();
            polluted.push(control);
            polluted.extend_from_slice(&marker[at..]);
            out.push(polluted);
        }
    }
    // Two controls at once, and the two reviewers' spellings.
    out.push(b"\x1b[2\x000\x7f1~".to_vec());
    out.push(b"\x1b[\r\n201~".to_vec());
    out.push(b"\x1b[20\r1~".to_vec());
    out.push(b"\x1b[20\x001~".to_vec());
    out.push(b"\x1b[2\x000\x7f0~".to_vec());
    out.push(b"\x1b[20\r0~".to_vec());
    out.push(b"\x1b[20\x000~".to_vec());
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Scenario: Open a paste, then send its closing marker with a control byte or DEL inserted at every position, each of the 32 C0 controls and DEL in turn; then an opening marker spelled the same ways. A spelling that no longer contains the exact marker bytes neither closes nor opens a paste: Enter, Ctrl+U, a BEL and a focus report after it are still read as pasted text, and only a later exact marker changes that. Each stream is also fed split at every offset with the state cloned, with the same result.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_a_marker_with_a_control_in_it_is_no_marker() {
    const OPEN: &[u8] = b"\x1b[200~";
    const CLOSE: &[u8] = b"\x1b[201~";
    // After the marker: Enter, Ctrl+U, Ctrl+G and a focus report.
    const AFTER: &[u8] = b"\r\x15\x07";
    const FOCUS: &[u8] = b"\x1b[I";

    for polluted in polluted_markers(CLOSE) {
        let head = [OPEN, b"text", &polluted].concat();
        // The oracle: a bracketed-paste reader closes the paste at the
        // literal marker bytes, and nowhere else.
        let closes = contains(&polluted, CLOSE);
        let state = state_after(&head);
        if !closes {
            assert!(!state.at_ground(), "{polluted:?}");
            assert_eq!(whole_report(&state, FOCUS), None, "{polluted:?}");
        }

        let mut stream = [&head[..], AFTER, FOCUS].concat();
        let after = head.len();
        stream.extend_from_slice(CLOSE);
        let close_at = stream.len() - 1;
        stream.push(b'\r');
        let found = tokens(&stream);

        let ends: Vec<usize> = found
            .iter()
            .filter(|t| t.seen == Seen::PasteEnd)
            .map(|t| t.at)
            .collect();
        if closes {
            assert_eq!(ends.len(), 1, "{polluted:?}");
            assert!(ends[0] < after, "{polluted:?}");
        } else {
            assert_eq!(ends, vec![close_at], "{polluted:?}");
            // Until the exact marker, everything after the polluted one is
            // pasted: no edit, and the BEL a pasted candidate.
            let inside: Vec<&Tok> = found.iter().filter(|t| t.at >= after).collect();
            assert!(inside.iter().all(|t| t.at > close_at || t.in_paste));
            assert!(
                inside
                    .iter()
                    .filter(|t| t.at < close_at)
                    .all(|t| t.edit.is_none()),
                "{polluted:?}"
            );
            assert!(
                inside
                    .iter()
                    .any(|t| t.at == after + 2 && t.candidate == Some(pasted(CandidateForm::Bel)))
            );
        }
        // The Enter after the exact closing marker submits, from ground.
        assert_eq!(found.last().unwrap().edit, Some(Edit::Submit));
        assert!(at_ground_after(&stream), "{polluted:?}");
        assert_split_invariant(&stream);
    }

    for polluted in polluted_markers(OPEN) {
        let head = [&b"typed"[..], &polluted].concat();
        let opens = contains(&polluted, OPEN);
        let mut stream = [&head[..], b"\r"].concat();
        let enter = stream.len() - 1;
        let found = tokens(&stream);
        let starts = found.iter().filter(|t| t.seen == Seen::PasteStart).count();
        assert_eq!(starts, usize::from(opens), "{polluted:?}");
        let edit = found.iter().find(|t| t.at == enter).unwrap().edit;
        if opens {
            assert_eq!(edit, None, "{polluted:?}");
            assert!(!at_ground_after(&stream));
        } else {
            assert_eq!(edit, Some(Edit::Submit), "{polluted:?}");
            assert!(at_ground_after(&stream), "{polluted:?}");
            assert_eq!(
                whole_report(&state_after(&head), FOCUS),
                Some(Report::FocusIn)
            );
        }
        stream.extend_from_slice(OPEN);
        stream.extend_from_slice(b"x\x07");
        stream.extend_from_slice(CLOSE);
        assert!(at_ground_after(&stream), "{polluted:?}");
        assert_split_invariant(&stream);
    }
}

/// Scenario: Start a sequence or string and abandon it with CAN, SUB, a new ESC or a control byte, with a Ctrl+G form around it; then send a sequence longer than the parameter cap. Nothing abandoned dispatches as a key, the bytes that follow are read afresh from ground, and the overlong sequence returns to ground at its final byte.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_cancel_and_overlong_sequences_return_to_ground() {
    use CandidateForm::*;
    // CAN or SUB inside a CSI-u: no key; the `u` after it is text.
    for cancel in [0x18u8, 0x1a] {
        let stream = [&b"\x1b[103;5"[..], &[cancel][..], &b"u"[..]].concat();
        let found = tokens(&stream);
        assert_eq!(found[0].seen, Seen::Abandoned);
        assert_eq!(found[1].seen, Seen::Plain(b'u'));
        assert_eq!(candidates(&stream), vec![]);
        assert!(at_ground_after(&stream));
        // Inside an OSC: the BEL after it is Ctrl+G in ground, not the end
        // of the string.
        let stream = [&b"\x1b]11;"[..], &[cancel][..], &b"\x07"[..]].concat();
        assert_eq!(candidates(&stream), vec![(stream.len() - 1, cand(Bel))]);
        // A lone ESC cancelled.
        let stream = [&b"\x1b"[..], &[cancel][..], &b"\x07"[..]].concat();
        assert_eq!(candidates(&stream), vec![(2, cand(Bel))]);
    }

    // An ESC inside an OSC that doesn't begin ST abandons the string and
    // opens the next sequence.
    assert_eq!(
        candidates(b"\x1b]0;title\x1b[103;5u"),
        vec![(16, cand(CsiU))]
    );
    assert_eq!(
        candidates(b"\x1b]0;title\x1b\x07"),
        vec![(10, cand(EscBel))]
    );
    assert_eq!(
        tokens(b"\x1b]0;t\x1b\x07")[0].seen,
        Seen::Abandoned,
        "the string is abandoned before Alt+Ctrl+G"
    );
    // A new ESC abandons a CSI; ESC ESC is the Escape key, then a sequence.
    assert_eq!(candidates(b"\x1b[1;\x1b\x07"), vec![(5, cand(EscBel))]);
    assert_eq!(candidates(b"\x1b\x1b\x07"), vec![(2, cand(EscBel))]);
    // A control byte ends an SS3 prefix and is read afresh.
    assert_eq!(candidates(b"\x1bO\x07"), vec![(2, cand(Bel))]);
    assert_eq!(edits(b"\x1bO\x15"), vec![(2, Edit::Clear)]);
    // An X10 prefix cut short is no report, and the byte is read afresh.
    assert_eq!(candidates(b"\x1b[M \x07"), vec![(4, cand(Bel))]);
    assert_eq!(tokens(b"\x1b[M !\"")[0].seen, Seen::X10Mouse);

    // Overlong: past the cap the CSI is malformed, stays a sequence until
    // its final byte, and then the stream is at ground again.
    let mut stream = b"\x1b[".to_vec();
    stream.extend(std::iter::repeat_n(b'1', MAX_SEQUENCE_BYTES + 10));
    assert!(!at_ground_after(&stream));
    stream.push(b'A');
    assert!(at_ground_after(&stream));
    assert_eq!(
        tokens(&stream),
        vec![Tok {
            at: stream.len() - 1,
            seen: Seen::MalformedCsi(b'A'),
            in_paste: false,
            candidate: None,
            edit: None,
        }]
    );
    stream.push(0x07);
    assert_eq!(candidates(&stream), vec![(stream.len() - 1, cand(Bel))]);
    // Exactly at the cap it is still well formed.
    let mut stream = b"\x1b[".to_vec();
    stream.extend(std::iter::repeat_n(b'1', MAX_SEQUENCE_BYTES));
    stream.push(b'A');
    assert!(matches!(tokens(&stream)[0].seen, Seen::Csi { .. }));
    // A BEL inside an overlong sequence is still a candidate, and so is the
    // overlong sequence's own `u`.
    let mut stream = b"\x1b[".to_vec();
    stream.extend(std::iter::repeat_n(b'1', MAX_SEQUENCE_BYTES + 1));
    stream.push(0x07);
    stream.push(b'u');
    assert_eq!(
        candidates(&stream),
        vec![
            (stream.len() - 2, cand(BelInSequence)),
            (stream.len() - 1, cand(MalformedKey)),
        ]
    );
    // A parameter after an intermediate, or a byte no CSI carries, is
    // malformed too.
    assert_eq!(tokens(b"\x1b[ 1u")[0].seen, Seen::MalformedCsi(b'u'));
    assert_eq!(
        tokens("\x1b[1żu".as_bytes())[0].seen,
        Seen::MalformedCsi(b'u')
    );
}

/// Scenario: Feed partial sequences, strings and pastes, one at a time, and ask after each whether the stream is at a boundary. Only a stream that has finished every sequence and closed every paste is at ground.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_the_boundary_is_ground_outside_every_sequence_and_paste() {
    for open in [
        &b"\x1b"[..],
        b"\x1b[",
        b"\x1b[1;5",
        b"\x1b[1;\x07",
        b"\x1b[1;\r",
        b"\x1bO",
        b"\x1b[M",
        b"\x1b[M ",
        b"\x1b]0;title",
        b"\x1b]0;title\x1b",
        b"\x1bPq",
        b"\x1b_x",
        b"\x1b^x",
        b"\x1bXx",
        b"\x1b[200~",
        b"\x1b[200~text",
        b"\x1b[200~\x1b[201",
    ] {
        assert!(
            !at_ground_after(open),
            "{:?}",
            String::from_utf8_lossy(open)
        );
    }
    for closed in [
        &b""[..],
        b"text",
        b"\x07",
        b"\x1b\x07",
        b"\x1b[A",
        b"\x1bOP",
        b"\x1b[M !\"",
        b"\x1b]0;title\x07",
        b"\x1b]0;title\x1b\\",
        b"\x1bPq\x1b\\",
        b"\x1b[200~text\x1b[201~",
        b"\x1bx",
        b"\x1b\r",
    ] {
        assert!(
            at_ground_after(closed),
            "{:?}",
            String::from_utf8_lossy(closed)
        );
    }
}

/// Scenario: Classify ops that are exactly one terminal report (focus, SGR mouse, CPR, DA1, OSC 10/11 colour replies ended by BEL or ST), then the same reports split, concatenated, prefixed, inside a paste, out of range or in a near shape. Only a whole, in-range report read from a boundary is classified.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_only_one_whole_report_from_a_boundary_is_classified() {
    let ground = Framing::default();
    for (op, report) in [
        (&b"\x1b[I"[..], Report::FocusIn),
        (b"\x1b[O", Report::FocusOut),
        (b"\x1b[<0;10;5M", Report::SgrMouse),
        (b"\x1b[<65;1;1m", Report::SgrMouse),
        (b"\x1b[12;40R", Report::Cpr { row: 12, col: 40 }),
        (b"\x1b[65535;1R", Report::Cpr { row: 65535, col: 1 }),
        (b"\x1b[?64;1;2;6;22c", Report::Da1),
        (b"\x1b[?1;2c", Report::Da1),
        (
            b"\x1b]10;rgb:ffff/0000/8080\x07",
            Report::OscColour { slot: 10 },
        ),
        (b"\x1b]11;rgb:f/0/80\x1b\\", Report::OscColour { slot: 11 }),
        (
            b"\x1b]11;rgb:1a2b/3C4d/5e6F\x07",
            Report::OscColour { slot: 11 },
        ),
    ] {
        assert_eq!(
            whole_report(&ground, op),
            Some(report),
            "{:?}",
            String::from_utf8_lossy(op)
        );
    }
    for op in [
        &b""[..],
        b"\x1b[I\x1b[O",
        b"\x1b[I\x1b",
        b"\x1b[I\x1b[",
        b"x\x1b[I",
        b"\x1b[Ix",
        b"\x1b[12;40R\x1b[12;40R",
        b"\x1b[12;40",
        b"\x1b[0;40R",
        b"\x1b[12;0R",
        b"\x1b[65536;1R",
        b"\x1b[12R",
        b"\x1b[12;40;1R",
        b"\x1b[?12;40R",
        b"\x1b[12:1;40R",
        b"\x1b[12;\r40R",
        b"\x1b[12;\x7f40R",
        b"\x1b[64;1c",
        b"\x1b[?c",
        b"\x1b[?64;;1c",
        b"\x1b[?64 c",
        b"\x1b[<0;10M",
        b"\x1b[<0;0;5M",
        b"\x1b[<0;5;0M",
        b"\x1b[1I",
        b"\x1b[>I",
        b"\x1b]12;rgb:0/0/0\x07",
        b"\x1b]11;rgb:0/0\x07",
        b"\x1b]11;rgb:0/0/0/0\x07",
        b"\x1b]11;rgb:00000/0/0\x07",
        b"\x1b]11;rgb:g/0/0\x07",
        b"\x1b]11;rgb:/0/0\x07",
        b"\x1b]11;?\x07",
        b"\x1bP1$r0m\x1b\\",
        b"\x1b[200~\x1b[I\x1b[201~",
        b"\x1b[103;5u",
        b"\x07",
    ] {
        assert_eq!(
            whole_report(&ground, op),
            None,
            "{:?}",
            String::from_utf8_lossy(op)
        );
    }
    // A report split across two ops is classified in neither: the second
    // starts from a carried prefix.
    let mut carried = Framing::default();
    for &b in b"\x1b[12;" {
        carried.feed(b, |_| {});
    }
    assert_eq!(whole_report(&carried, b"40R"), None);
    assert_eq!(whole_report(&carried, b"\x1b[12;40R"), None);
    // Inside an open paste nothing is a report.
    let mut in_paste = Framing::default();
    for &b in b"\x1b[200~" {
        in_paste.feed(b, |_| {});
    }
    assert_eq!(whole_report(&in_paste, b"\x1b[I"), None);
    // Classifying leaves the caller's state as it was.
    assert!(ground.at_ground());
}

/// Scenario: Type Enter, Ctrl+U and Ctrl+C, then Alt+Enter, Ctrl+J, a kitty-encoded Enter, those keys inside a paste and a CR inside an unfinished sequence. Only the plain Enter submits and only the plain Ctrl+U and Ctrl+C clear.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_submit_and_clear_are_the_plain_keys_outside_a_paste() {
    assert_eq!(edits(b"draft\r"), vec![(5, Edit::Submit)]);
    assert_eq!(
        edits(b"a\x15b\x03"),
        vec![(1, Edit::Clear), (3, Edit::Clear)]
    );
    for none in [
        &b"\x1b\r"[..],
        b"\n",
        b"\x1b[13u",
        b"\x1b[13;2u",
        b"\x1b\x15",
        b"\x1b\x03",
        b"\x1b[200~a\rb\x15c\x03\x1b[201~",
        b"\x1b[1;\r",
        b"\x1b]0;\r",
    ] {
        assert_eq!(edits(none), vec![], "{:?}", String::from_utf8_lossy(none));
    }
    // After a paste closes, Enter submits again.
    assert_eq!(edits(b"\x1b[200~a\rb\x1b[201~\r"), vec![(15, Edit::Submit)]);
}

/// Scenario: Feed the sequences later commits read: private-mode sets and resets, RIS, sub-parameters, intermediates, each string kind, and a control inside a sequence or string. Each comes out as its own token with its parameters, intermediates and payload read back exactly, and a control inside is reported without ending the sequence.
#[spec("inbox/probe/003")]
#[test]
fn probe_003_sequences_read_back_their_parts() {
    let mut framing = Framing::default();
    let mut seen = Vec::new();
    for &b in b"\x1b[?1;2004h\x1b[?2004l\x1bc\x1b[103:71:1;5:3u" {
        framing.feed(b, |token| match token.event {
            Event::Csi(csi) => seen.push((
                csi.private,
                csi.params()
                    .map(|p| p.subparams().collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
                csi.final_byte,
            )),
            Event::Escape(b) => seen.push((None, vec![vec![Some(u32::from(b))]], 0)),
            other => panic!("unexpected {other:?}"),
        });
    }
    assert_eq!(
        seen,
        vec![
            (Some(b'?'), vec![vec![Some(1)], vec![Some(2004)]], b'h'),
            (Some(b'?'), vec![vec![Some(2004)]], b'l'),
            (None, vec![vec![Some(u32::from(b'c'))]], 0),
            (
                None,
                vec![vec![Some(103), Some(71), Some(1)], vec![Some(5), Some(3)]],
                b'u'
            ),
        ]
    );
    // An empty parameter list has no parameters; empty and huge values.
    let mut out = Vec::new();
    let mut framing = Framing::default();
    for &b in b"\x1b[m\x1b[;99999999999m" {
        framing.feed(b, |token| {
            if let Event::Csi(csi) = token.event {
                out.push(csi.params().map(|p| p.value()).collect::<Vec<_>>());
            }
        });
    }
    assert_eq!(out, vec![vec![], vec![None, Some(u32::MAX)]]);

    // Intermediates, a private-use byte after the first parameter byte, and
    // a control inside a CSI, which goes on and completes it.
    let s = |private: Option<u8>, params: &[u8], intermediates: &[u8], final_byte| Seen::Csi {
        private,
        params: params.to_vec(),
        intermediates: intermediates.to_vec(),
        final_byte,
    };
    let seen = |bytes: &[u8]| {
        tokens(bytes)
            .into_iter()
            .map(|t| t.seen)
            .collect::<Vec<_>>()
    };
    assert_eq!(seen(b"\x1b[2 q"), vec![s(None, b"2", b" ", b'q')]);
    assert_eq!(
        seen(b"\x1b[?2004;1$y"),
        vec![s(Some(b'?'), b"2004;1", b"$", b'y')]
    );
    assert_eq!(seen(b"\x1b[1?2h"), vec![s(None, b"1?2", b"", b'h')]);
    assert_eq!(
        seen(b"\x1b[1;\r5A"),
        vec![Seen::Inside(b'\r'), s(None, b"1;5", b"", b'A')]
    );
    assert_eq!(
        seen(b"\x1b[1;\x7f5A"),
        vec![Seen::Inside(0x7f), s(None, b"1;5", b"", b'A')]
    );

    // Each string's kind and payload; a control inside one is reported and
    // kept out of the payload.
    let st = |kind, payload: &[u8], terminator| Seen::Str {
        kind,
        payload: Some(payload.to_vec()),
        terminator,
    };
    for (bytes, kind) in [
        (&b"\x1b]0;t\x1b\\"[..], StringKind::Osc),
        (b"\x1bP0;t\x1b\\", StringKind::Dcs),
        (b"\x1b_0;t\x1b\\", StringKind::Apc),
        (b"\x1b^0;t\x1b\\", StringKind::Pm),
        (b"\x1bX0;t\x1b\\", StringKind::Sos),
    ] {
        assert_eq!(seen(bytes), vec![st(kind, b"0;t", Terminator::St)]);
    }
    assert_eq!(
        seen(b"\x1b]0;a\rb\x07"),
        vec![
            Seen::Inside(b'\r'),
            st(StringKind::Osc, b"0;ab", Terminator::Bel)
        ]
    );
    // Past the cap, the payload is not kept.
    let mut long = b"\x1b]0;".to_vec();
    long.extend(std::iter::repeat_n(b'x', MAX_SEQUENCE_BYTES));
    long.push(0x07);
    assert_eq!(
        seen(&long),
        vec![Seen::Str {
            kind: StringKind::Osc,
            payload: None,
            terminator: Terminator::Bel,
        }]
    );

    // ESC followed by CAN or SUB abandons the escape; ESC ESC is the
    // Escape key, then a sequence.
    assert_eq!(seen(b"\x1b\x18"), vec![Seen::Abandoned]);
    assert_eq!(seen(b"\x1b\x1a"), vec![Seen::Abandoned]);
    assert_eq!(
        seen(b"\x1b\x1bx"),
        vec![Seen::Abandoned, Seen::Escape(b'x')]
    );
}
