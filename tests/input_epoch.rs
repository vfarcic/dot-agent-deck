//! PRD #1666 part 3 (Q35-06): the input epoch's success test and its
//! negatives (R3 §3.4), through a generation's [`GenerationInput`].

use std::sync::Arc;
use std::time::Instant;

use dot_agent_deck::composer_probe::epoch::{
    DeckJobId, Enqueued, GenerationInput, Observation, OutputNote, Writer,
};
use dot_agent_deck::composer_probe::framing::Report;
use dot_agent_deck::composer_probe::paste_mode::OutputHook;
use dot_agent_deck::composer_probe::query_ledger::QueryKind;
use spec::spec;

const PROBE: DeckJobId = DeckJobId(7);

/// A human byte, then a probe's admission: `E0`.
fn touched_then_admitted(input: &GenerationInput) -> u64 {
    assert!(matches!(
        input.enqueue(Writer::Human, b"x"),
        Enqueued::Moved(1)
    ));
    assert!(input.touched());
    match input.enqueue(Writer::Deck(PROBE), b"\x07") {
        Enqueued::Moved(e) => e - 1,
        other => panic!("a probe's byte moves the epoch: {other:?}"),
    }
}

fn observation(admitted_at: u64) -> Observation {
    Observation {
        probe: PROBE,
        admitted_at,
        read_empty: true,
        retired: false,
    }
}

/// Scenario: Type one byte, admit a probe whose byte is the next thing written, and settle it with an empty read and nothing else in between. The box is no longer touched; a later byte touches it again.
#[spec("inbox/probe/002")]
#[test]
fn probe_002_one_touch_one_empty_probe_is_empty() {
    let input = GenerationInput::default();
    let e0 = touched_then_admitted(&input);
    assert_eq!(e0, 1);
    let epoch = input.epoch();
    assert_eq!(
        (epoch.epoch(), epoch.owner(), epoch.last_foreign()),
        (2, Some(PROBE), 1)
    );
    assert!(input.apply(&observation(e0)));
    assert!(!input.touched());
    assert_eq!(input.epoch().observed(), 2);
    input.enqueue(Writer::Human, b"y");
    assert!(input.touched());
    let epoch = input.epoch();
    assert_eq!(
        (epoch.epoch(), epoch.owner(), epoch.last_foreign()),
        (3, None, 3)
    );
}

/// Scenario: Between a probe's admission and its settlement, write a voice answer, a human byte, a deck write no job names, another deck job's byte, an external edit, and reports shaped like exempt ones that aren't (a focus report with focus reporting off, a DA1 reply nobody asked for, a CPR reply to an outstanding query), each in its own run. None applies the observation and the box stays touched.
#[spec("inbox/probe/002")]
#[test]
fn probe_002_anything_after_the_probe_refuses_it() {
    type Between = fn(&GenerationInput);
    let cases: [(&str, Between); 8] = [
        ("voice answer", |i| {
            i.enqueue(Writer::Answer, b"1\r");
        }),
        ("human byte", |i| {
            i.enqueue(Writer::Human, b"z");
        }),
        ("unnamed deck write", |i| {
            i.enqueue(Writer::UnnamedDeck, b"msg");
        }),
        ("another deck job", |i| {
            i.enqueue(Writer::Deck(DeckJobId(8)), b"msg");
        }),
        ("external edit", |i| {
            i.external_edit();
        }),
        ("focus report, mode off", |i| {
            assert!(matches!(
                i.enqueue(Writer::Human, b"\x1b[I"),
                Enqueued::Moved(_)
            ));
        }),
        ("unsolicited DA1 reply", |i| {
            assert!(matches!(
                i.enqueue(Writer::Human, b"\x1b[?62;22c"),
                Enqueued::Moved(_)
            ));
        }),
        ("CPR reply", |i| {
            i.on_output(&[OutputNote::Query(QueryKind::Cpr)], Instant::now());
            assert!(matches!(
                i.enqueue(Writer::Human, b"\x1b[3;1R"),
                Enqueued::Moved(_)
            ));
        }),
    ];
    for (label, between) in cases {
        let input = GenerationInput::default();
        let e0 = touched_then_admitted(&input);
        between(&input);
        assert!(!input.apply(&observation(e0)), "{label}");
        assert!(input.touched(), "{label}");
    }
}

/// Scenario: Settle a probe that read a non-empty box, one whose generation retired, one whose admission epoch is stale, one in the name of another job, and one after the generation retired through its input. None applies.
#[spec("inbox/probe/002")]
#[test]
fn probe_002_the_observation_itself_must_hold() {
    let input = GenerationInput::default();
    let e0 = touched_then_admitted(&input);
    for (label, obs) in [
        (
            "read not empty",
            Observation {
                read_empty: false,
                ..observation(e0)
            },
        ),
        (
            "retired",
            Observation {
                retired: true,
                ..observation(e0)
            },
        ),
        ("stale admission", observation(e0 - 1)),
        (
            "other job",
            Observation {
                probe: DeckJobId(9),
                ..observation(e0)
            },
        ),
    ] {
        assert!(!input.apply(&obs), "{label}");
    }
    assert!(input.apply(&observation(e0)), "the control");
    let input = GenerationInput::default();
    let e0 = touched_then_admitted(&input);
    input.retire();
    assert!(!input.apply(&observation(e0)), "retired through the input");
}

/// Scenario: With the agent's focus reporting and SGR mouse tracking on, and a DA1 and an OSC 11 query outstanding, write each whole report between a probe's admission and its settlement. Each is exempt, so the observation still applies; the same reports split over two writes, or with a byte before them, or with the modes off, are not exempt.
#[spec("inbox/probe/002")]
#[test]
fn probe_002_exempt_reports_dont_refuse_it() {
    let notes = [
        OutputNote::Mode {
            mode: 1004,
            on: true,
        },
        OutputNote::Mode {
            mode: 1002,
            on: true,
        },
        OutputNote::Mode {
            mode: 1006,
            on: true,
        },
        OutputNote::Query(QueryKind::Da1),
        OutputNote::Query(QueryKind::OscColour { slot: 11 }),
    ];
    let input = GenerationInput::default();
    input.on_output(&notes, Instant::now());
    let e0 = touched_then_admitted(&input);
    for (bytes, report) in [
        (&b"\x1b[I"[..], Report::FocusIn),
        (b"\x1b[O", Report::FocusOut),
        (b"\x1b[<0;10;5M", Report::SgrMouse),
        (b"\x1b[?62;22c", Report::Da1),
        (
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
            Report::OscColour { slot: 11 },
        ),
    ] {
        assert_eq!(
            input.enqueue(Writer::Human, bytes),
            Enqueued::Exempt(report)
        );
    }
    assert!(input.apply(&observation(e0)), "exempt reports move nothing");
    assert!(!input.touched());

    for (label, writes) in [
        ("split", &[&b"\x1b["[..], b"I"][..]),
        ("prefixed", &[b"a\x1b[I"]),
        ("from the deck", &[]),
    ] {
        let input = GenerationInput::default();
        input.on_output(&notes, Instant::now());
        let before = input.epoch().epoch();
        for w in writes {
            assert!(
                matches!(input.enqueue(Writer::Human, w), Enqueued::Moved(_)),
                "{label}"
            );
        }
        if writes.is_empty() {
            assert!(matches!(
                input.enqueue(Writer::UnnamedDeck, b"\x1b[I"),
                Enqueued::Moved(_)
            ));
            assert!(matches!(
                input.enqueue(Writer::Answer, b"\x1b[I"),
                Enqueued::Moved(_)
            ));
        }
        assert!(input.epoch().epoch() > before, "{label}");
    }

    let input = GenerationInput::default();
    input.on_output(&notes, Instant::now());
    input.on_output(
        &[
            OutputNote::Mode {
                mode: 1004,
                on: false,
            },
            OutputNote::Mode {
                mode: 1000,
                on: false,
            },
        ],
        Instant::now(),
    );
    assert!(
        matches!(input.enqueue(Writer::Human, b"\x1b[I"), Enqueued::Moved(_)),
        "focus off"
    );
    assert!(
        matches!(
            input.enqueue(Writer::Human, b"\x1b[<0;10;5M"),
            Enqueued::Exempt(_)
        ),
        "a reset naming another tracking mode leaves 1002 on"
    );
    input.on_output(
        &[OutputNote::Mode {
            mode: 1005,
            on: true,
        }],
        Instant::now(),
    );
    assert!(
        matches!(
            input.enqueue(Writer::Human, b"\x1b[<0;10;5M"),
            Enqueued::Moved(_)
        ),
        "UTF-8 encoding replaced SGR"
    );
    input.on_output(
        &[
            OutputNote::Mode {
                mode: 1006,
                on: true,
            },
            OutputNote::Mode {
                mode: 1002,
                on: false,
            },
        ],
        Instant::now(),
    );
    assert!(
        matches!(
            input.enqueue(Writer::Human, b"\x1b[<0;10;5M"),
            Enqueued::Moved(_)
        ),
        "tracking off"
    );
    input.on_output(
        &[OutputNote::Mode { mode: 9, on: true }, OutputNote::Reset],
        Instant::now(),
    );
    assert!(
        matches!(
            input.enqueue(Writer::Human, b"\x1b[<0;10;5M"),
            Enqueued::Moved(_)
        ),
        "RIS turned the modes off"
    );
}

/// Scenario: Through the real output hook, the agent turns on focus reporting and SGR mouse tracking in one sequence, then resets the terminal, then turns them on again and off one by one. A whole focus or mouse report is exempt exactly while the output has its mode on.
#[spec("inbox/probe/002")]
#[test]
fn probe_002_the_output_hook_sets_the_report_modes() {
    let input = Arc::new(GenerationInput::default());
    let mut hook = OutputHook::new(Arc::clone(&input));
    let exempt = |bytes: &[u8]| matches!(input.enqueue(Writer::Human, bytes), Enqueued::Exempt(_));
    let (focus, mouse) = (&b"\x1b[I"[..], &b"\x1b[<0;3;4m"[..]);
    assert!(!exempt(focus) && !exempt(mouse));
    hook.on_output(b"\x1b[?1004;1003;1006h");
    assert!(exempt(focus) && exempt(mouse));
    hook.on_output(b"\x1bc");
    assert!(!exempt(focus) && !exempt(mouse), "RIS");
    hook.on_output(b"\x1b[?1004h\x1b[?1000h\x1b[?1006h");
    assert!(exempt(focus) && exempt(mouse));
    hook.on_output(b"\x1b[?1004l");
    assert!(!exempt(focus) && exempt(mouse));
    hook.on_output(b"\x1b[?1006l");
    assert!(!exempt(mouse), "the encoding reset");
    hook.on_output(b"\x1b[?1006h\x1b[?1000l");
    assert!(!exempt(mouse), "the tracking reset");
}

/// Scenario: Open a bracketed paste in the input, then write a whole focus report with focus reporting on; close the paste and write it again. Inside the paste it isn't a report and moves the epoch; after the paste it is exempt.
#[spec("inbox/probe/002")]
#[test]
fn probe_002_a_report_inside_a_paste_touches() {
    let input = GenerationInput::default();
    input.on_output(
        &[OutputNote::Mode {
            mode: 1004,
            on: true,
        }],
        Instant::now(),
    );
    input.enqueue(Writer::Human, b"\x1b[200~text");
    assert!(matches!(
        input.enqueue(Writer::Human, b"\x1b[I"),
        Enqueued::Moved(_)
    ));
    input.enqueue(Writer::Human, b"\x1b[201~");
    assert!(matches!(
        input.enqueue(Writer::Human, b"\x1b[I"),
        Enqueued::Exempt(_)
    ));
}
