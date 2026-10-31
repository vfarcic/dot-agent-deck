//! PRD #1666 part 3 (Q35-16): the planner's pure parts
//! (`src/composer_probe/planner.rs`): where a person's op is cut, how its
//! native candidates are replayed over what the PTY took, and the physical
//! boundary with its ground wake, and the measured input grammar it reads a
//! Claude Code 2.1.310/2.1.312 generation's input with (Q83).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use dot_agent_deck::composer_probe::framing::Framing;
use dot_agent_deck::composer_probe::planner::{
    Boundary, InputGrammar, Natives, Physical, PhysicalTracker, WakeAt, first_cut, is_candidate,
    next_cut,
};
use spec::spec;

fn poll_once<F: std::future::Future>(fut: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    fut.poll(&mut Context::from_waker(Waker::noop()))
}

/// The tokens of `bytes` that are candidates, by `is_candidate`.
fn candidates(bytes: &[u8], solicited: bool) -> usize {
    let mut framing = Framing::default();
    let mut n = 0;
    for &b in bytes {
        framing.feed(b, |t| n += usize::from(is_candidate(&t, solicited)));
    }
    n
}

/// Scenario: Read a colour reply ended by BEL, one ended by ST, a lone BEL and a BEL inside a DCS string, as an op classified a solicited whole reply and as one that isn't. Only the solicited reply's ending BEL stops being a candidate; every other form stays one either way.
#[spec("inbox/probe/010")]
#[test]
fn probe_010_only_a_solicited_whole_replys_bel_is_no_candidate() {
    let bel_reply = b"\x1b]10;rgb:1/22/333\x07";
    assert_eq!(candidates(bel_reply, false), 1);
    assert_eq!(candidates(bel_reply, true), 0);
    for other in [
        b"\x1b]10;rgb:1/22/333\x1b\\".as_slice(),
        b"\x07",
        b"\x1bPq\x07\x1b\\",
        b"\x1b[103;5u",
    ] {
        let n = usize::from(other != b"\x1b]10;rgb:1/22/333\x1b\\");
        assert_eq!(candidates(other, false), n, "{other:?}");
        assert_eq!(candidates(other, true), n, "{other:?}");
    }
}

/// Scenario: Cut a frame of text and three candidates once, then keep reading its rest from each cut's saved state, as releases do; cut a CSI-u Ctrl+G with a BEL inside it; and cut a solicited whole reply and a frame with no candidate. The cuts land after each candidate in turn, the CSI the BEL's cut split still completes as a Ctrl+G in the rest, and the solicited reply and the plain frame have no cut.
#[spec("inbox/probe/010")]
#[test]
fn probe_010_the_rest_of_a_cut_frame_is_cut_lazily_from_the_saved_state() {
    let bytes = b"a\x07\x1b[103;5ub\x1b\x07c";
    let mut cuts = Vec::new();
    let mut at = 0;
    let mut cut = first_cut(&Framing::default(), bytes, false);
    while let Some(c) = cut {
        at += c.at;
        cuts.push(at);
        cut = next_cut(&c.resume, &bytes[at..]);
    }
    assert_eq!(cuts, [2, 10, 13]);
    // A BEL inside a CSI cuts there, and the cut's saved state carries the
    // unfinished CSI into the rest, where it completes as a Ctrl+G.
    let first = first_cut(&Framing::default(), b"\x1b[103;\x075u", false).unwrap();
    assert_eq!(first.at, 7);
    let rest = next_cut(&first.resume, b"5u").unwrap();
    assert_eq!(rest.at, 2);
    assert!(next_cut(&Framing::default(), b"plain text").is_none());
    assert!(
        first_cut(&Framing::default(), b"\x1b]10;rgb:1/22/333\x07", true).is_none(),
        "a solicited whole reply has no cut"
    );
}

/// Scenario: Number a run of native candidates from 5 to 7 and replay the op's bytes over what the PTY took, in pieces that split a CSI-u key. Each candidate is reported once, in order, by its number, when the byte that completes it is taken; a run whose numbering ended at 5 reports only 5; a run ending at the last number reports it and stops.
#[spec("inbox/probe/010")]
#[test]
fn probe_010_a_native_run_reports_each_candidate_as_the_pty_takes_it() {
    let op = b"a\x07b\x1b[103;5uc\x07";
    let natives = Natives {
        first: 5,
        last: 7,
        start: Framing::default(),
    };
    let mut run = natives.run();
    let mut taken = Vec::new();
    let pieces: [(&[u8], &[u64]); 4] = [
        (&op[..2], &[5]),
        (&op[2..6], &[]),
        (&op[6..9], &[]),
        (&op[9..], &[6, 7]),
    ];
    for (piece, reported) in pieces {
        let before = taken.len();
        run.written(piece, |seq| taken.push(seq));
        assert_eq!(&taken[before..], reported, "{piece:?}");
    }
    assert_eq!(taken, [5, 6, 7]);
    run.written(b"\x07", |seq| taken.push(seq));
    assert_eq!(taken, [5, 6, 7], "nothing past the last number");
    let mut short = Natives {
        first: 5,
        last: 5,
        start: Framing::default(),
    }
    .run();
    let mut taken = Vec::new();
    short.written(op, |seq| taken.push(seq));
    assert_eq!(taken, [5]);
    let mut end = Natives {
        first: u64::MAX,
        last: u64::MAX,
        start: Framing::default(),
    }
    .run();
    let mut taken = Vec::new();
    end.written(b"\x07\x07", |seq| taken.push(seq));
    assert_eq!(taken, [u64::MAX]);
}

/// Scenario: Feed the writer's tracker a person's bytes as the PTY takes them: an unfinished CSI, then its final byte; an open paste, then its close; a lone ESC, then `[`, then the CSI's final byte; nothing at all. The boundary is inside exactly while a sequence or paste is open and after a lone ESC reads as the Escape key, where a deck line may start but which isn't ground; a waiter for a line is woken by the move out of a sequence and not by staying in it or by an empty write, and a waiter for ground isn't woken by the lone ESC.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_the_tracker_reads_what_the_pty_took() {
    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let physical = Physical::default();
    let mut tracker = PhysicalTracker::default();
    assert_eq!(physical.boundary(), Boundary::Ground);
    assert_eq!(tracker.boundary(), Boundary::Ground);
    for (open, close) in [
        (b"\x1b[1".as_slice(), b"A".as_slice()),
        (b"\x1b[200~text", b"\x1b[201~"),
    ] {
        tracker.written(open, &physical);
        assert_eq!(physical.boundary(), Boundary::Inside, "{open:?}");
        assert!(!physical.boundary().line_may_start() && !physical.at_ground());
        assert_eq!(tracker.boundary(), Boundary::Inside);
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut cx = Context::from_waker(&waker);
        let mut wait = std::pin::pin!(physical.until_line());
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        tracker.written(b"", &physical);
        tracker.written(b"2", &physical);
        assert!(!flag.0.load(Ordering::SeqCst), "{open:?}: still open");
        tracker.written(close, &physical);
        assert!(physical.at_ground(), "{open:?}");
        assert!(flag.0.load(Ordering::SeqCst), "{open:?}: woken");
        assert!(wait.as_mut().poll(&mut cx).is_ready());
    }
    tracker.written(b"\x1b", &physical);
    assert_eq!(physical.boundary(), Boundary::Escape);
    assert!(physical.boundary().line_may_start() && !physical.at_ground());
    assert!(poll_once(std::pin::pin!(physical.until_line())).is_ready());
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&flag));
    let mut cx = Context::from_waker(&waker);
    let mut ground = std::pin::pin!(physical.until_ground());
    assert!(ground.as_mut().poll(&mut cx).is_pending());
    tracker.written(b"[", &physical);
    assert_eq!(physical.boundary(), Boundary::Inside, "ESC [ opens a CSI");
    assert!(!flag.0.load(Ordering::SeqCst));
    tracker.written(b"B", &physical);
    assert!(flag.0.load(Ordering::SeqCst), "the CSI ended: ground");
    assert!(ground.as_mut().poll(&mut cx).is_ready());
    // An Escape key inside a paste is the paste's.
    tracker.written(b"\x1b[200~\x1b", &physical);
    assert_eq!(physical.boundary(), Boundary::Inside);
}

/// Scenario: Wait for ground with the stream already at ground, with ground reached between the check and the registration, at the registration, and between the recheck and the first poll. Each completes on the first poll; none awaits a notification that already passed.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_the_ground_wake_misses_no_return_to_ground() {
    let physical = Physical::default();
    assert!(poll_once(std::pin::pin!(physical.until_ground())).is_ready());
    // Ground reached at each step, and the steps the wait then took: the
    // recheck catches the first two, the enabled waiter the third.
    for (step, taken) in [
        (WakeAt::Checked, &[WakeAt::Checked, WakeAt::Registered][..]),
        (WakeAt::Registered, &[WakeAt::Checked, WakeAt::Registered]),
        (
            WakeAt::Rechecked,
            &[WakeAt::Checked, WakeAt::Registered, WakeAt::Rechecked],
        ),
    ] {
        let physical = Physical::default();
        physical.set(Boundary::Inside);
        let mut steps = Vec::new();
        let wait = physical.until_pausing(
            |b| b == Boundary::Ground,
            |at| {
                steps.push(at);
                if at == step {
                    physical.set(Boundary::Ground);
                }
            },
        );
        assert!(
            poll_once(std::pin::pin!(wait)).is_ready(),
            "ground reached at {step:?}"
        );
        assert_eq!(steps, taken, "ground reached at {step:?}");
    }
}

/// Scenario: Wait for ground while the stream is off ground, poll once, then return it to ground and poll again; separately, return it to ground and leave it there before a second waiter starts. The first waiter is woken by the transition after its registration and completes; the second completes at once.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_a_transition_after_registration_wakes_the_waiter() {
    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let physical = Physical::default();
    physical.set(Boundary::Inside);
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&flag));
    let mut cx = Context::from_waker(&waker);
    let mut wait = std::pin::pin!(physical.until_ground());
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    physical.set(Boundary::Inside);
    assert!(
        !flag.0.load(Ordering::SeqCst),
        "staying off ground wakes nothing"
    );
    physical.set(Boundary::Ground);
    assert!(
        flag.0.load(Ordering::SeqCst),
        "the return to ground woke it"
    );
    assert!(wait.as_mut().poll(&mut cx).is_ready());
    assert!(poll_once(std::pin::pin!(physical.until_ground())).is_ready());
}

/// Where `bytes` leave the stream, written as one write with `grammar`.
fn boundary_after(bytes: &[u8], grammar: InputGrammar) -> Boundary {
    let physical = Physical::default();
    let mut tracker = PhysicalTracker::default();
    tracker.written_as(bytes, &physical, grammar);
    assert_eq!(tracker.boundary(), physical.boundary());
    physical.boundary()
}

/// Scenario: Feed the writer's tracker, with Claude Code 2.1.310/2.1.312's measured input grammar, the user's `ESC [`, and a CSI holding parameters (`ESC [ 1;2`), an intermediate (`ESC [ 1 space`) or a private marker (`ESC [ ?`), each followed by Ctrl+U, Enter or BEL, as one write and as two. Each prefix alone stays inside the CSI, and each C0 aborts it, so the stream is in ground at once. The strict grammar keeps the same bytes inside the CSI, however they are written.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_the_measured_grammar_aborts_a_csi_on_a_c0() {
    for open in [b"\x1b[".as_slice(), b"\x1b[1;2", b"\x1b[1 ", b"\x1b[?"] {
        assert_eq!(
            boundary_after(open, InputGrammar::ClaudeMeasured),
            Boundary::Inside,
            "{open:?} stays inside"
        );
        for c0 in [0x15u8, b'\r', 0x07] {
            let whole = [open, &[c0]].concat();
            for (grammar, expected) in [
                (InputGrammar::ClaudeMeasured, Boundary::Ground),
                (InputGrammar::Strict, Boundary::Inside),
            ] {
                assert_eq!(
                    boundary_after(&whole, grammar),
                    expected,
                    "{open:?} then {c0:#x}, one write, {grammar:?}"
                );
                let physical = Physical::default();
                let mut tracker = PhysicalTracker::default();
                tracker.written_as(open, &physical, grammar);
                assert_eq!(physical.boundary(), Boundary::Inside, "{open:?}");
                tracker.written_as(&[c0], &physical, grammar);
                assert_eq!(
                    physical.boundary(),
                    expected,
                    "{open:?} then {c0:#x}, two writes, {grammar:?}"
                );
            }
        }
    }
}

/// Scenario: Open an OSC (`ESC ]`), a DCS (`ESC P`) and an APC (`ESC _ G`) and follow each with an ordinary C0 (Ctrl+U, Enter), a BEL, CAN, SUB, ST (`ESC \`), an ESC opening a CSI (`ESC [`, then `A` finishing it), and an ESC then a BEL, under the measured grammar and the strict one. Both read each ending as Claude Code 2.1.310/2.1.312's tokenizer does, except a BEL after a DCS or APC, which ends it only under the measured grammar; an ordinary C0 stays inside every string, and a BEL inside an open paste ends the string but not the paste.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_the_measured_grammar_ends_strings_where_the_agent_does() {
    use Boundary::{Ground, Inside};
    let measured = InputGrammar::ClaudeMeasured;
    let strict = InputGrammar::Strict;
    for opener in [b"\x1b]".as_slice(), b"\x1bP", b"\x1b_G"] {
        let osc = opener == b"\x1b]";
        // (ending, measured, strict)
        let rows: [(&[u8], Boundary, Boundary); 10] = [
            (b"\x15", Inside, Inside),
            (b"\r", Inside, Inside),
            (b"\x07", Ground, if osc { Ground } else { Inside }),
            (b"\x18", Ground, Ground),
            (b"\x1a", Ground, Ground),
            (b"\x1b\\", Ground, Ground),
            (b"\x1b[", Inside, Inside),
            (b"\x1b[A", Ground, Ground),
            (b"\x1b\x07", Ground, Ground),
            (b"\x15\r\x07", Ground, if osc { Ground } else { Inside }),
        ];
        for (ending, want_measured, want_strict) in rows {
            let bytes = [opener, ending].concat();
            assert_eq!(
                boundary_after(&bytes, measured),
                want_measured,
                "measured {bytes:?}"
            );
            assert_eq!(
                boundary_after(&bytes, strict),
                want_strict,
                "strict {bytes:?}"
            );
        }
        let pasted = [b"\x1b[200~".as_slice(), opener, b"\x07"].concat();
        assert_eq!(boundary_after(&pasted, measured), Inside, "{pasted:?}");
        let closed = [pasted.as_slice(), b"\x1b[201~"].concat();
        assert_eq!(boundary_after(&closed, measured), Ground, "{closed:?}");
    }
}

/// Scenario: Under the measured grammar, leave a CSI, an OSC, a DCS, a reply-like OSC (`ESC ] 11;`) and a reply-like CSI (`ESC [ <`) unfinished, register a waiter for a deck line and one for ground on each, and let a minute of (paused) time pass with no write: the stream stays inside and neither waiter wakes, since no grammar ends a sequence by time (Q87). The user's finishing key then wakes both.
#[spec("inbox/probe/076")]
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn probe_076_no_grammar_ends_a_sequence_by_time() {
    let measured = InputGrammar::ClaudeMeasured;
    for (open, finish) in [
        (b"\x1b[".as_slice(), b"C".as_slice()),
        (b"\x1b]", b"\x07"),
        (b"\x1bP", b"\x07"),
        (b"\x1b]11;", b"\x1b\\"),
        (b"\x1b[<", b"\x15"),
    ] {
        let physical = Arc::new(Physical::default());
        let mut tracker = PhysicalTracker::default();
        tracker.written_as(open, &physical, measured);
        let waiters = [
            tokio::spawn({
                let physical = Arc::clone(&physical);
                async move { physical.until_line().await }
            }),
            tokio::spawn({
                let physical = Arc::clone(&physical);
                async move { physical.until_ground().await }
            }),
        ];
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(physical.boundary(), Boundary::Inside, "{open:?}");
        assert!(
            waiters.iter().all(|w| !w.is_finished()),
            "{open:?}: no timer woke a waiter"
        );
        tracker.written_as(finish, &physical, measured);
        assert_eq!(physical.boundary(), Boundary::Ground, "{open:?}{finish:?}");
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("the finishing key wakes it")
                .unwrap();
        }
    }
}

/// Scenario: A waiter for ground registers while a paste is open; one write closes the paste and opens an OSC, so the stream stays inside and the waiter is neither woken nor resolved; the BEL that ends the OSC then wakes it. Under the measured grammar, a waiter registered inside `ESC [` is woken by the Ctrl+U that aborts it.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_an_inside_to_inside_write_leaves_a_waiter_waiting() {
    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let measured = InputGrammar::ClaudeMeasured;
    for (open, inside, finish) in [
        (
            b"\x1b[200~abc".as_slice(),
            b"\x1b[201~\x1b]".as_slice(),
            b"\x07".as_slice(),
        ),
        (b"\x1b[", b"", b"\x15"),
    ] {
        let physical = Physical::default();
        let mut tracker = PhysicalTracker::default();
        tracker.written_as(open, &physical, measured);
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut cx = Context::from_waker(&waker);
        let mut wait = std::pin::pin!(physical.until_ground());
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        tracker.written_as(inside, &physical, measured);
        assert_eq!(physical.boundary(), Boundary::Inside, "{open:?}{inside:?}");
        assert!(!flag.0.load(Ordering::SeqCst), "{open:?}{inside:?}");
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        tracker.written_as(finish, &physical, measured);
        assert!(flag.0.load(Ordering::SeqCst), "{finish:?} woke it");
        assert!(wait.as_mut().poll(&mut cx).is_ready());
    }
}

/// Scenario: Only a Claude Code 2.1.310 or 2.1.312 release, latched for a generation spawned as Claude Code, reads with the measured grammar; another Claude release, Codex, a generation of no known agent, a conflicting header, a header not seen and a scanner dropped before one all read strictly, and the latch is asked about the generation's own agent.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_the_grammar_follows_the_latched_release() {
    use dot_agent_deck::doorbell_activation::LatchedRelease;
    use dot_agent_deck::event::AgentType;
    let v = |p| semver::Version::new(2, 1, p);
    assert_eq!(
        InputGrammar::of(&AgentType::ClaudeCode, &v(310)),
        InputGrammar::ClaudeMeasured
    );
    assert_eq!(
        InputGrammar::of(&AgentType::ClaudeCode, &v(312)),
        InputGrammar::ClaudeMeasured
    );
    assert_eq!(
        InputGrammar::of(&AgentType::ClaudeCode, &v(314)),
        InputGrammar::Strict
    );
    assert_eq!(
        InputGrammar::of(&AgentType::Codex, &semver::Version::new(0, 164, 0)),
        InputGrammar::Strict
    );
    let claude = AgentType::ClaudeCode;
    let asked = std::cell::RefCell::new(Vec::new());
    let known = |agent: &AgentType| {
        asked.borrow_mut().push(agent.clone());
        LatchedRelease::Known(v(310))
    };
    assert_eq!(
        InputGrammar::latched(Some(&claude), known),
        InputGrammar::ClaudeMeasured
    );
    assert_eq!(asked.borrow().as_slice(), [AgentType::ClaudeCode]);
    assert_eq!(
        InputGrammar::latched(Some(&AgentType::Codex), |_| LatchedRelease::Known(v(310))),
        InputGrammar::Strict,
        "a Codex generation"
    );
    assert_eq!(
        InputGrammar::latched(None, |_| LatchedRelease::Known(v(310))),
        InputGrammar::Strict,
        "no agent"
    );
    for latch in [
        LatchedRelease::Conflict,
        LatchedRelease::Unseen,
        LatchedRelease::ScannerGeometry,
        LatchedRelease::ScannerBudget,
    ] {
        assert_eq!(
            InputGrammar::latched(Some(&claude), |_| latch.clone()),
            InputGrammar::Strict,
            "{latch:?}"
        );
    }
}
