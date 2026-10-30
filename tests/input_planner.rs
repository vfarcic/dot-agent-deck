//! PRD #1666 part 3 (Q35-16): the planner's pure parts
//! (`src/composer_probe/planner.rs`): where a person's op is cut, how its
//! native candidates are replayed over what the PTY took, and the physical
//! boundary with its ground wake, and the measured input grammar it reads a
//! Claude Code 2.1.310/2.1.312 generation's input with (Q83).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::framing::Framing;
use dot_agent_deck::composer_probe::planner::{
    Boundary, FLUSH_MARGIN, FLUSH_QUIET, FLUSH_RESPONSE, InputGrammar, Natives, Physical,
    PhysicalTracker, WakeAt, first_cut, is_candidate, next_cut,
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

/// Scenario: Feed the writer's tracker, with Claude Code 2.1.310/2.1.312's measured input grammar, the user's `ESC [` then Ctrl+U, and separately `ESC [` then Enter: each C0 aborts the CSI, so the stream is in ground at once with no wait. The strict grammar keeps the same bytes inside the CSI.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_the_measured_grammar_aborts_a_csi_on_a_c0() {
    let t0 = Instant::now();
    for c0 in [0x15u8, b'\r', 0x07] {
        let physical = Physical::default();
        let mut tracker = PhysicalTracker::default();
        tracker.written_with(b"\x1b[", &physical, InputGrammar::ClaudeMeasured, t0);
        assert_eq!(physical.boundary(), Boundary::Inside);
        tracker.written_with(&[c0], &physical, InputGrammar::ClaudeMeasured, t0);
        assert_eq!(physical.boundary(), Boundary::Ground, "{c0:#x} aborts it");
        assert!(
            !physical.flushed(t0 + Duration::from_secs(10)),
            "no flush pending"
        );

        let physical = Physical::default();
        let mut tracker = PhysicalTracker::default();
        tracker.written(b"\x1b[", &physical);
        tracker.written(&[c0], &physical);
        assert_eq!(physical.boundary(), Boundary::Inside, "{c0:#x}, strict");
    }
}

/// Scenario: With the measured grammar, leave an OSC unfinished (`ESC ]` then Ctrl+U, which stays inside it): no deck byte may start inside the agent's 50 ms quiet window or the tracker's margin after it, so the stream counts as flushed only from 50 ms plus the margin after the last write, and then the writer's settle reads ground. A response-like prefix (`ESC ] 11;`) waits 2 s plus the margin; a lone ESC, an open paste and the strict grammar never flush.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_no_deck_byte_inside_the_agents_flush_window() {
    let measured = InputGrammar::ClaudeMeasured;
    let t0 = Instant::now();
    let physical = Physical::default();
    let mut tracker = PhysicalTracker::default();
    tracker.written_with(b"\x1b]", &physical, measured, t0);
    tracker.written_with(b"\x15", &physical, measured, t0 + Duration::from_millis(30));
    let last = t0 + Duration::from_millis(30);
    for inside in [0, 1, 49, 50, 449] {
        let at = last + Duration::from_millis(inside);
        assert!(!physical.flushed(at), "+{inside} ms is still inside");
        assert_eq!(
            tracker.settle(&physical, at),
            Boundary::Inside,
            "+{inside} ms"
        );
    }
    let due = last + FLUSH_QUIET + FLUSH_MARGIN;
    assert!(physical.flushed(due));
    assert_eq!(tracker.settle(&physical, due), Boundary::Ground);

    let physical = Physical::default();
    let mut tracker = PhysicalTracker::default();
    tracker.written_with(b"\x1b]11;", &physical, measured, t0);
    assert!(
        !physical.flushed(t0 + FLUSH_QUIET + FLUSH_MARGIN),
        "a response-like prefix waits longer"
    );
    assert!(physical.flushed(t0 + FLUSH_RESPONSE + FLUSH_MARGIN));

    for (what, bytes, grammar) in [
        ("lone ESC", b"\x1b".as_slice(), measured),
        ("paste", b"\x1b[200~abc", measured),
        ("strict", b"\x1b]", InputGrammar::Strict),
    ] {
        let physical = Physical::default();
        let mut tracker = PhysicalTracker::default();
        tracker.written_with(bytes, &physical, grammar, t0);
        assert!(!physical.flushed(t0 + Duration::from_secs(60)), "{what}");
    }
}

/// Scenario: Only a latched Claude Code 2.1.310 or 2.1.312 reads with the measured grammar; another Claude release, and Codex, read strictly.
#[spec("inbox/probe/076")]
#[test]
fn probe_076_the_grammar_follows_the_latched_release() {
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
}

/// Scenario: With the measured grammar, leave an OSC unfinished, then let the agent's flush come due with no write in between. A key the user types after it is read from ground, not as more of the dead string, so a plain key leaves the stream in ground; and a waiter for a deck line, registered while the OSC was open, resolves at the flush with no further write.
#[spec("inbox/probe/076")]
#[tokio::test(flavor = "current_thread")]
async fn probe_076_the_flush_ends_the_sequence_for_writes_and_waiters() {
    let measured = InputGrammar::ClaudeMeasured;
    let t0 = Instant::now();
    let physical = Physical::default();
    let mut tracker = PhysicalTracker::default();
    tracker.written_with(b"\x1b]", &physical, measured, t0);
    let due = t0 + FLUSH_QUIET + FLUSH_MARGIN;
    tracker.written_with(b"x", &physical, measured, due + Duration::from_millis(1));
    assert_eq!(
        tracker.boundary(),
        Boundary::Ground,
        "the key after the flush"
    );

    let physical = Arc::new(Physical::default());
    let mut tracker = PhysicalTracker::default();
    tracker.written_with(b"\x1b]", &physical, measured, Instant::now());
    let waiter = tokio::spawn({
        let physical = Arc::clone(&physical);
        async move { physical.until_line().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "inside the window");
    tokio::time::sleep(FLUSH_QUIET + FLUSH_MARGIN + Duration::from_millis(5)).await;
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the flush wakes the waiter")
        .unwrap();
}
