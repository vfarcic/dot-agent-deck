//! PRD #1666 part 3 (Q35-06, amendments 1–4 of Q35-core): the D3′ query
//! ledger (`inbox/probe/045`), the native hazard FIFO (`052`) and `B`
//! (`060`), pure, through the output hook and a generation's input.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::epoch::{Enqueued, GenerationInput, OutputNote, Writer};
use dot_agent_deck::composer_probe::paste_mode::OutputHook;
use dot_agent_deck::composer_probe::query_ledger::{
    Backlog, CprWaitEnd, EntryView, HAZARD_SLOTS, HazardFifo, HazardPop, LEDGER_SLOTS, Pop,
    QueryHistory, QueryKind, QueryLedger, T_TRIGGER,
};
use spec::spec;

const DA1: QueryKind = QueryKind::Da1;
const CPR: QueryKind = QueryKind::Cpr;
const BG: QueryKind = QueryKind::OscColour { slot: 11 };
const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
const CPR_REPLY: &[u8] = b"\x1b[5;1R";

fn noted(input: &GenerationInput, kinds: &[QueryKind]) {
    let notes: Vec<_> = kinds.iter().map(|&k| OutputNote::Query(k)).collect();
    input.on_output(&notes, Instant::now());
}

fn moved(e: Enqueued) -> bool {
    matches!(e, Enqueued::Moved(_))
}

// ---- inbox/probe/045: the exemption ledger ----

/// Scenario: Note eight queries, CPR only and then a mix of DA1, OSC 11 and CPR, then a ninth. The ninth saturates the ledger: its history turns `Unknown` for good, the query is dropped and counted, and no reply is exempt or eligible to pass ahead from then on, though a reply to a kept query is still recognised as solicited and still settles it.
#[spec("inbox/probe/045")]
#[test]
fn probe_045_the_ninth_query_saturates_the_ledger() {
    let mixes: [&[QueryKind]; 2] = [&[CPR; LEDGER_SLOTS], &[DA1, BG, CPR, DA1, BG, CPR, DA1, BG]];
    for mix in mixes {
        let mut ledger = QueryLedger::default();
        for &k in mix {
            ledger.note(k);
        }
        assert_eq!(
            (ledger.outstanding(), ledger.saturated()),
            (LEDGER_SLOTS, false)
        );
        assert_eq!(ledger.history(), QueryHistory::Exact);
        assert_eq!(ledger.exempts(DA1), mix.contains(&DA1), "the control");
        ledger.note(DA1);
        assert!(ledger.saturated());
        assert_eq!((ledger.outstanding(), ledger.dropped()), (LEDGER_SLOTS, 1));
        assert_eq!(ledger.history(), QueryHistory::Unknown);
        assert!(!ledger.exempts(DA1) && !ledger.exempts(BG) && !ledger.exempts(CPR));
        assert_eq!(
            ledger.solicited(DA1),
            mix.contains(&DA1),
            "kept entries still match"
        );
        if mix.contains(&DA1) {
            assert!(ledger.settle(DA1));
            assert_eq!(ledger.outstanding(), LEDGER_SLOTS - 1);
        }
        ledger.note(BG);
        assert!(ledger.saturated(), "a freed slot doesn't unsaturate it");
        assert_eq!(ledger.history(), QueryHistory::Unknown);
        assert_eq!(ledger.dropped(), 2);
    }
}

/// Scenario: Through a generation's input, saturate its ledger with a DA1 among the kept queries, then write a whole DA1 reply. It touches the box, is no longer eligible to pass ahead, and still answers the kept query, so a second reply finds nothing to answer.
#[spec("inbox/probe/045")]
#[test]
fn probe_045_after_saturation_every_reply_touches() {
    let input = GenerationInput::default();
    noted(&input, &[DA1]);
    assert_eq!(input.reply_class(DA1), (true, true), "the control");
    noted(&input, &[CPR; LEDGER_SLOTS]);
    assert_eq!(input.history(), QueryHistory::Unknown);
    assert_eq!(input.reply_class(DA1), (true, false));
    assert!(moved(input.enqueue(Writer::Human, DA1_REPLY)));
    assert_eq!(
        input.reply_class(DA1),
        (false, false),
        "it answered the kept query"
    );
}

/// Scenario: Note a DA1 query and answer it with a whole reply, a duplicate reply, a reply split across two writes, a reply inside a bracketed paste, and a reply to no query; and note a query whose reply never comes (cancelled). Only the first whole solicited reply is exempt; the split one still answers its query; the pasted one answers nothing; and the cancelled query stays outstanding until a later reply answers it.
#[spec("inbox/probe/045")]
#[test]
fn probe_045_split_duplicate_unsolicited_and_cancelled_replies() {
    let input = GenerationInput::default();
    assert!(
        moved(input.enqueue(Writer::Human, DA1_REPLY)),
        "nothing asked"
    );
    noted(&input, &[DA1]);
    assert_eq!(
        input.enqueue(Writer::Human, DA1_REPLY),
        Enqueued::Exempt(dot_agent_deck::composer_probe::framing::Report::Da1)
    );
    assert!(moved(input.enqueue(Writer::Human, DA1_REPLY)), "duplicate");

    noted(&input, &[DA1]);
    assert!(moved(input.enqueue(Writer::Human, b"\x1b[?62")));
    assert!(moved(input.enqueue(Writer::Human, b";22c")));
    assert_eq!(
        input.reply_class(DA1),
        (false, false),
        "the split reply answered it"
    );

    noted(&input, &[DA1]);
    input.enqueue(Writer::Human, b"\x1b[200~");
    assert!(moved(input.enqueue(Writer::Human, DA1_REPLY)));
    input.enqueue(Writer::Human, b"\x1b[201~");
    assert_eq!(
        input.reply_class(DA1),
        (true, true),
        "a pasted reply answers nothing"
    );

    // The reply to that query is never written: it stays outstanding, and the
    // next whole reply answers it.
    assert!(matches!(
        input.enqueue(Writer::Human, DA1_REPLY),
        Enqueued::Exempt(_)
    ));
    assert_eq!(input.reply_class(DA1), (false, false));

    noted(&input, &[BG]);
    for reply in [
        &b"\x1b]11;rgb:ffff/ffff/ffff\x07"[..],
        b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\",
    ] {
        noted(&input, &[BG]);
        assert!(
            matches!(input.enqueue(Writer::Human, reply), Enqueued::Exempt(_)),
            "BEL and ST"
        );
    }
    assert!(matches!(
        input.enqueue(Writer::Human, b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
        Enqueued::Exempt(_)
    ));
    assert_eq!(input.reply_class(BG), (false, false));
}

/// Scenario: Note a CPR query, then write its whole reply. The reply settles the query but is never exempt, so it touches the box.
#[spec("inbox/probe/045")]
#[test]
fn probe_045_a_cpr_reply_settles_and_touches() {
    let input = GenerationInput::default();
    noted(&input, &[CPR]);
    assert_eq!(input.reply_class(CPR), (true, false));
    assert!(moved(input.enqueue(Writer::Human, CPR_REPLY)));
    assert_eq!(input.reply_class(CPR), (false, false));
    assert!(!input.backlog());
}

/// Scenario: With a CPR query outstanding and no answering client, accept a native Ctrl+G, let time pass, then write the CPR's reply. The candidate is latched behind the unanswered backlog, the FIFO stays non-clear across more than two quiet periods, and the reply ends `B` without popping it.
#[spec("inbox/probe/045")]
#[test]
fn probe_045_a_native_ctrl_g_behind_an_unanswered_backlog() {
    let t0 = Instant::now();
    let input = GenerationInput::default();
    noted(&input, &[CPR]);
    assert!(input.backlog());
    assert_eq!(input.accept_native(t0), Some(1));
    assert_eq!(input.hazards(t0).0, [EntryView::Latched { seq: 1 }]);
    let later = t0 + T_TRIGGER * 3;
    assert!(!input.hazards_clear(later));
    input.enqueue(Writer::Human, CPR_REPLY);
    assert!(!input.backlog());
    assert!(
        !input.hazards_clear(later + T_TRIGGER),
        "a reply never pops"
    );
}

/// Scenario: Feed the output hook a DA1 query, an OSC 10 and an OSC 11 query, a private and a plain CPR query, split across two pushes at every offset, and lookalikes: a DA1 with another parameter or a private marker, a DSR other than 6, a CPR with another parameter or an intermediate, an OSC 12 query, an OSC 10 that sets a colour, and query text inside an OSC and a DCS payload, with and without a C1 CSI byte. Only the real queries are noted.
#[spec("inbox/probe/045")]
#[test]
fn probe_045_the_output_hook_notes_whole_queries_only() {
    let input = Arc::new(GenerationInput::default());
    let mut hook = OutputHook::new(Arc::clone(&input));
    let stream: &[u8] = b"\x1b[c\x1b[0c\x1b]10;?\x1b\\\x1b]11;?\x07\x1b[?6n\x1b[6n";
    for at in 0..stream.len() {
        let input = Arc::new(GenerationInput::default());
        let mut hook = OutputHook::new(Arc::clone(&input));
        hook.on_output(&stream[..at]);
        hook.on_output(&stream[at..]);
        for kind in [DA1, QueryKind::OscColour { slot: 10 }, BG, CPR] {
            assert!(input.reply_class(kind).0, "split at {at}: {kind:?}");
        }
        assert!(input.backlog(), "split at {at}");
    }
    hook.on_output(
        b"\x1b[1c\x1b[>c\x1b[5n\x1b[6;1n\x1b[6 n\x1b]12;?\x07\x1b]10;rgb:0/0/0\x07\
          \x1b]0;[6n[c\x9b6n\x07\x1bP]11;?[6n\x1b\\",
    );
    for kind in [DA1, QueryKind::OscColour { slot: 10 }, BG, CPR] {
        assert_eq!(input.reply_class(kind), (false, false), "{kind:?}");
    }
    assert!(!input.backlog());
    hook.on_output(b"\x1b[c");
    assert_eq!(input.reply_class(DA1), (true, true), "the control");
}

// ---- inbox/probe/060: B ----

/// Scenario: On a profile whose CPR waits end at the next frame, emit a CPR query then `?2026h`; a `?2026h` then a CPR query; each inside an OSC and a DCS string; and each split across pushes. A frame after the query ends `B`, a frame before it doesn't, and neither counts inside a string.
#[spec("inbox/probe/060")]
#[test]
fn probe_060_the_sync_begin_rule() {
    let run = |pushes: &[&[u8]]| {
        let input = Arc::new(GenerationInput::new(CprWaitEnd::SyncBegin));
        let mut hook = OutputHook::new(Arc::clone(&input));
        for p in pushes {
            hook.on_output(p);
        }
        input.backlog()
    };
    assert!(run(&[b"\x1b[6n"]));
    assert!(!run(&[b"\x1b[6n", b"\x1b[?2026h"]));
    assert!(!run(&[b"\x1b[?6n\x1b[?1;2026h"]), "2026 among other modes");
    assert!(run(&[b"\x1b[?2026h\x1b[6n"]), "a frame before the query");
    assert!(run(&[b"\x1b[6n\x1b[?2026l"]), "a reset is no frame");
    assert!(
        run(&[b"\x1b[6n\x1b]0;[?2026h\x9b?2026h\x07"]),
        "a frame inside an OSC"
    );
    assert!(run(&[b"\x1b[6n\x1bP[?2026h\x1b\\"]), "a frame inside a DCS");
    assert!(!run(&[b"\x1b]0;[6n\x9b6n\x07"]), "a query inside an OSC");
    assert!(!run(&[b"\x1bP[6n\x1b\\"]), "a query inside a DCS");
    assert!(
        run(&[b"\x1b]0;title\x1b[6n"]),
        "an ESC ends the string: the control"
    );
    let whole: &[u8] = b"\x1b[6n\x1b[?2026h";
    for at in 0..=whole.len() {
        assert!(!run(&[&whole[..at], &whole[at..]]), "split at {at}");
    }
    for at in 1..4 {
        assert!(
            run(&[&b"\x1b[6n"[..at], &b"\x1b[6n"[at..]]),
            "query split at {at}"
        );
    }
}

/// Scenario: Feed Codex's start-up probe prefix (a CPR, a DA1 and the OSC 10/11 colour queries, none answered), then its first frame. `B` holds after the prefix and is false after the frame, though the queries stay outstanding in the ledger.
#[spec("inbox/probe/060")]
#[test]
fn probe_060_the_startup_shape_leaves_b_false() {
    let input = Arc::new(GenerationInput::new(CprWaitEnd::SyncBegin));
    let mut hook = OutputHook::new(Arc::clone(&input));
    hook.on_output(b"\x1b[?6n\x1b[c\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    assert!(input.backlog());
    hook.on_output(b"\x1b[?2026h\x1b[1;1H\x1b[Jhello\x1b[?2026l");
    assert!(!input.backlog());
    assert_eq!(input.reply_class(CPR), (true, false), "still outstanding");
}

/// Scenario: On a profile that declares nothing, note three CPR queries and write two whole CPR replies, then three more, one of them split and one typed with a byte before it. `B` holds while queries outnumber whole replies, counts a split reply once it is whole, ignores a reply that isn't whole, and is floored at zero.
#[spec("inbox/probe/060")]
#[test]
fn probe_060_the_counter_rule() {
    let input = GenerationInput::new(CprWaitEnd::Counted);
    noted(&input, &[CPR, CPR, CPR]);
    input.enqueue(Writer::Human, CPR_REPLY);
    input.enqueue(Writer::Answer, CPR_REPLY);
    assert!(input.backlog(), "one outstanding");
    input.enqueue(Writer::Human, b"\x1b[5;");
    assert!(input.backlog(), "half a reply");
    input.enqueue(Writer::Human, b"1R");
    assert!(!input.backlog(), "the split reply, once whole");
    input.enqueue(Writer::Human, CPR_REPLY);
    assert!(!input.backlog(), "floored at zero");
    noted(&input, &[CPR]);
    input.enqueue(Writer::Human, b"\x1b[200~\x1b[5;1R\x1b[201~");
    assert!(input.backlog(), "a pasted reply isn't one");
    input.enqueue(Writer::UnnamedDeck, CPR_REPLY);
    assert!(!input.backlog(), "whoever writes it");

    let mut b = Backlog::new(CprWaitEnd::Counted);
    b.sync_begin();
    b.cpr_query();
    b.sync_begin();
    assert!(b.holds(), "a frame doesn't end a counted wait");
    let mut b = Backlog::new(CprWaitEnd::SyncBegin);
    b.cpr_query();
    b.cpr_reply();
    assert!(b.holds(), "a reply doesn't end a frame-ended wait");
}

/// Scenario: On each profile rule, saturate the query ledger with nine non-CPR queries, and on the counted rule with nine CPR queries answered one by one. Ledger saturation leaves `B` as it was: false with no CPR outstanding, and following its own count with them.
#[spec("inbox/probe/060")]
#[test]
fn probe_060_ledger_saturation_leaves_b_unchanged() {
    for end in [CprWaitEnd::SyncBegin, CprWaitEnd::Counted] {
        let input = GenerationInput::new(end);
        noted(&input, &[DA1; LEDGER_SLOTS + 1]);
        assert_eq!(input.history(), QueryHistory::Unknown);
        assert!(!input.backlog(), "{end:?}");
    }
    let input = GenerationInput::new(CprWaitEnd::Counted);
    noted(&input, &[CPR; LEDGER_SLOTS + 1]);
    for _ in 0..LEDGER_SLOTS {
        input.enqueue(Writer::Human, CPR_REPLY);
    }
    assert!(
        input.backlog(),
        "the ninth, dropped by the ledger, is still counted"
    );
    input.enqueue(Writer::Human, CPR_REPLY);
    assert!(!input.backlog());
}

// ---- inbox/probe/052: the hazard FIFO ----

fn fifo_at(t0: Instant) -> HazardFifo {
    let mut f = HazardFifo::default();
    assert!(f.clear(t0));
    f
}

fn views(f: &mut HazardFifo, now: Instant) -> Vec<EntryView> {
    f.entries(now)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Scenario: Accept a native Ctrl+G before a CPR query is emitted, one while it is outstanding, one after its reply, and let each quiet period run. The first latches when the query lands within its quiet period, the second enters latched, the third enters quiet and leaves once it has run past `T_TRIGGER` but not at exactly `T_TRIGGER`.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_candidates_around_a_cpr_wait() {
    let t0 = Instant::now();
    let input = GenerationInput::default();
    assert_eq!(input.accept_native(t0), Some(1));
    assert_eq!(
        input.hazards(t0).0,
        [EntryView::Quiet {
            seq: 1,
            paused: false
        }]
    );
    input.on_output(&[OutputNote::Query(CPR)], t0 + ms(5));
    assert_eq!(input.hazards(t0 + ms(5)).0, [EntryView::Latched { seq: 1 }]);
    assert_eq!(input.accept_native(t0 + ms(6)), Some(2));
    input.enqueue(Writer::Human, CPR_REPLY);
    let t1 = t0 + ms(10);
    assert_eq!(input.accept_native(t1), Some(3));
    assert_eq!(
        input.hazards(t1).0,
        [
            EntryView::Latched { seq: 1 },
            EntryView::Latched { seq: 2 },
            EntryView::Quiet {
                seq: 3,
                paused: false
            }
        ]
    );
    assert_eq!(
        input.hazards(t1 + T_TRIGGER).0.len(),
        3,
        "exactly T_TRIGGER is still quiet"
    );
    assert_eq!(
        input.hazards(t1 + T_TRIGGER + ms(1)).0.len(),
        2,
        "past it, D5"
    );

    let late = GenerationInput::default();
    late.accept_native(t0);
    late.on_output(&[OutputNote::Query(CPR)], t0 + T_TRIGGER + ms(1));
    assert!(
        late.hazards_clear(t0 + T_TRIGGER + ms(1)),
        "a query after the quiet period latches nothing"
    );
}

/// Scenario: Accept a quiet native Ctrl+G and, at the very instant its quiet period ends, note a CPR query and check admission, in both orders; then the same with the check a millisecond later and no query. The query at the boundary latches it whichever comes first; without it the entry leaves just after.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_expiry_racing_a_cpr_latches() {
    let t0 = Instant::now();
    let edge = t0 + T_TRIGGER;
    for query_first in [true, false] {
        let input = GenerationInput::default();
        input.accept_native(t0);
        if query_first {
            input.on_output(&[OutputNote::Query(CPR)], edge);
            assert!(!input.hazards_clear(edge));
        } else {
            assert!(!input.hazards_clear(edge));
            input.on_output(&[OutputNote::Query(CPR)], edge);
        }
        assert_eq!(
            input.hazards(edge + T_TRIGGER).0,
            [EntryView::Latched { seq: 1 }]
        );
    }
    let input = GenerationInput::default();
    input.accept_native(t0);
    assert!(!input.hazards_clear(edge));
    assert!(input.hazards_clear(edge + ms(1)));
}

/// Scenario: Accept a native Ctrl+G, saturate the query ledger with DA1 queries while it is quiet, and accept another after. Ledger saturation latches nothing and enters nothing latched.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_ledger_saturation_latches_nothing() {
    let t0 = Instant::now();
    let input = GenerationInput::default();
    input.accept_native(t0);
    noted(&input, &[DA1; LEDGER_SLOTS + 1]);
    input.accept_native(t0 + ms(1));
    assert_eq!(
        input.hazards(t0 + ms(1)).0,
        [
            EntryView::Quiet {
                seq: 1,
                paused: false
            },
            EntryView::Quiet {
                seq: 2,
                paused: false
            }
        ]
    );
}

/// Scenario: Latch three candidates and pop with invocations reporting `native_seen`: below the oldest, at the second, and again with a spent receipt. Pops take the oldest live entry numbered at most `native_seen`, one per record, never an entry above it; two candidates and one invocation leave one; an ignored candidate stays latched through any time.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_exact_pops_once_oldest_first() {
    let t0 = Instant::now();
    let mut f = fifo_at(t0);
    for _ in 0..3 {
        f.accept(t0, true);
    }
    let mut empty = HazardPop::Unspent;
    assert_eq!(
        fifo_at(t0).pop_reported(&mut empty, u64::MAX, t0),
        Pop::Nothing,
        "an empty FIFO"
    );
    let mut empty = HazardPop::Unspent;
    assert_eq!(
        fifo_at(t0).pop_at_drain(&mut empty, t0 + T_TRIGGER, t0),
        Pop::Nothing
    );
    let mut early = HazardPop::Unspent;
    assert_eq!(f.pop_reported(&mut early, 0, t0), Pop::Nothing);
    assert_eq!(
        early,
        HazardPop::Spent(Pop::Nothing),
        "spent though nothing qualified"
    );
    let mut record = HazardPop::Unspent;
    assert_eq!(f.pop_reported(&mut record, 2, t0), Pop::Entry(1));
    assert_eq!(
        f.pop_reported(&mut record, 3, t0),
        Pop::Nothing,
        "once per record"
    );
    assert_eq!(
        f.pop_at_drain(&mut record, t0 + T_TRIGGER, t0),
        Pop::Nothing,
        "on any channel"
    );
    let mut other = HazardPop::Unspent;
    assert_eq!(f.pop_reported(&mut other, 2, t0), Pop::Entry(2));
    let ignored = t0 + T_TRIGGER * 100;
    assert_eq!(views(&mut f, ignored), [EntryView::Latched { seq: 3 }]);
    assert!(!f.clear(ignored));
    let mut last = HazardPop::Unspent;
    assert_eq!(
        f.pop_reported(&mut last, 2, ignored),
        Pop::Nothing,
        "above native_seen"
    );
    let mut last = HazardPop::Unspent;
    assert_eq!(f.pop_reported(&mut last, 3, ignored), Pop::Entry(3));
    assert!(f.clear(ignored));
}

/// Scenario: A record seen only by a scan pops nothing until its exit drain, where the birth-floor fallback pops the oldest entry accepted strictly before the floor; a floor equal to the acceptance pops nothing; a record whose exact report arrives first spends its receipt so the drain pops nothing more; and a candidate no record was ever seen for stays.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_the_drain_fallback() {
    let t0 = Instant::now();
    let mut f = fifo_at(t0);
    f.accept(t0, true);
    f.accept(t0 + ms(10), true);
    let mut tie = HazardPop::Unspent;
    assert_eq!(
        f.pop_at_drain(&mut tie, t0, t0 + ms(20)),
        Pop::Nothing,
        "a tie pops nothing"
    );
    let mut scanned = HazardPop::Unspent;
    assert_eq!(
        f.pop_at_drain(&mut scanned, t0 + ms(1), t0 + ms(20)),
        Pop::Entry(1)
    );
    let mut exact_first = HazardPop::Unspent;
    assert_eq!(
        f.pop_reported(&mut exact_first, 2, t0 + ms(20)),
        Pop::Entry(2)
    );
    assert_eq!(
        f.pop_at_drain(&mut exact_first, t0 + ms(30), t0 + ms(30)),
        Pop::Nothing
    );
    f.accept(t0 + ms(40), true);
    assert!(!f.clear(t0 + T_TRIGGER * 10), "never seen, never popped");
}

/// Scenario: RV's recovery history: `G0` is pressed during a CPR wait and ignored, the wait ends, `G1` is pressed and acted on, its invocation pops `G0` and is pending while its editor runs, completes, and `G2` is popped by its own invocation. The queue after each step is `[G0(L)]`, `[G0(L), G1(Q)]`, `[G1(Q, paused)]`, `[G1(Q, running)]`, `[]`, `[]`, and admission's hazard conjunct returns after `G1`'s quiet period.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_rv_recovery_history() {
    let t0 = Instant::now();
    let input = GenerationInput::default();
    noted(&input, &[CPR]);
    let g0 = input.accept_native(t0).unwrap();
    assert_eq!(input.hazards(t0).0, [EntryView::Latched { seq: g0 }]);
    input.enqueue(Writer::Human, CPR_REPLY);
    let g1 = input.accept_native(t0 + ms(1)).unwrap();
    assert_eq!(
        input.hazards(t0 + ms(1)).0,
        [
            EntryView::Latched { seq: g0 },
            EntryView::Quiet {
                seq: g1,
                paused: false
            }
        ]
    );
    let mut g1_record = HazardPop::Unspent;
    input.records_pending(t0 + ms(2), true);
    assert_eq!(
        input.pop_reported(&mut g1_record, g1, t0 + ms(2)),
        Pop::Entry(g0)
    );
    let editing = t0 + T_TRIGGER * 5;
    assert_eq!(
        input.hazards(editing).0,
        [EntryView::Quiet {
            seq: g1,
            paused: true
        }]
    );
    input.records_pending(editing, false);
    assert_eq!(
        input.hazards(editing).0,
        [EntryView::Quiet {
            seq: g1,
            paused: false
        }]
    );
    assert!(
        !input.hazards_clear(editing + T_TRIGGER - ms(3)),
        "G1's quiet period runs"
    );
    assert!(
        input.hazards_clear(editing + T_TRIGGER),
        "it ran 1 ms before the pause"
    );
    let g2 = input.accept_native(editing + T_TRIGGER * 2).unwrap();
    let mut g2_record = HazardPop::Unspent;
    assert_eq!(
        input.pop_reported(&mut g2_record, g2, editing + T_TRIGGER * 2),
        Pop::Entry(g2)
    );
    assert!(input.hazards_clear(editing + T_TRIGGER * 2));
}

/// Scenario: A candidate is accepted and acted on within the same instant: its invocation reports its number at that instant and pops it, with no tick to wait for.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_a_same_tick_acted_on_candidate() {
    let t0 = Instant::now();
    let mut f = fifo_at(t0);
    let seq = f.accept(t0, true).unwrap();
    let mut record = HazardPop::Unspent;
    assert_eq!(f.pop_reported(&mut record, seq, t0), Pop::Entry(seq));
    assert!(f.clear(t0));
}

/// Scenario: Accept a quiet candidate, pause the clock for three quiet periods while a record is pending, then resume. The entry stays through the pause and leaves only once its running time passes `T_TRIGGER`; pausing twice or resuming twice changes nothing.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_quiet_clocks_pause_while_a_record_is_pending() {
    let t0 = Instant::now();
    let mut f = fifo_at(t0);
    f.accept(t0, false);
    f.records_pending(t0 + ms(400), true);
    f.records_pending(t0 + ms(500), true);
    let resumed = t0 + T_TRIGGER * 3;
    assert_eq!(
        views(&mut f, resumed),
        [EntryView::Quiet {
            seq: 1,
            paused: true
        }]
    );
    f.records_pending(resumed, false);
    f.records_pending(resumed + ms(100), false);
    assert!(!f.clear(resumed + T_TRIGGER - ms(400)));
    assert!(f.clear(resumed + T_TRIGGER - ms(399)));
}

/// Scenario: A3's delayed-reader schedule, with A4's editor-eaten press beside it: `G1`'s invocation is pending when `G2` is written (parsed into the agent's queue, or eaten by the user's editor) with `B` false, and again with a CPR outstanding. `G2`'s entry is paused, never discarded; after the return it runs its quiet period, or its invocation pops it; latched, it stays until an acted-on Ctrl+G pops it. An editor-eaten entry older than the acted-on one is the one the acted-on invocation pops, and the acted-on entry then leaves under D5.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_delayed_reader_and_editor_eaten_entries() {
    let t0 = Instant::now();
    // The delayed reader: G2 written while G1's helper is alive.
    let input = GenerationInput::default();
    let g1 = input.accept_native(t0).unwrap();
    let mut g1_record = HazardPop::Unspent;
    input.records_pending(t0 + ms(1), true);
    assert_eq!(
        input.pop_reported(&mut g1_record, g1, t0 + ms(1)),
        Pop::Entry(g1)
    );
    let g2 = input.accept_native(t0 + ms(2)).unwrap();
    let back = t0 + T_TRIGGER * 4;
    assert_eq!(
        input.hazards(back).0,
        [EntryView::Quiet {
            seq: g2,
            paused: true
        }]
    );
    input.records_pending(back, false);
    let mut g2_record = HazardPop::Unspent;
    input.records_pending(back + ms(1), true);
    assert_eq!(
        input.pop_reported(&mut g2_record, g2, back + ms(1)),
        Pop::Entry(g2)
    );
    assert!(
        input.hazards_clear(back + ms(1)),
        "the record's completion is Q35-19's conjunct"
    );

    // Editor-eaten, B false: G_e costs one quiet period after the return.
    let input = GenerationInput::default();
    let g1 = input.accept_native(t0).unwrap();
    let mut r1 = HazardPop::Unspent;
    input.records_pending(t0 + ms(1), true);
    input.pop_reported(&mut r1, g1, t0 + ms(1));
    let eaten = input.accept_native(t0 + ms(2)).unwrap();
    input.records_pending(back, false);
    assert!(!input.hazards_clear(back + T_TRIGGER));
    assert!(
        input.hazards_clear(back + T_TRIGGER + ms(1)),
        "D5, counted from the return"
    );
    let _ = eaten;

    // Editor-eaten, B true: latched until an acted-on Ctrl+G pops it; the
    // acted-on entry, older-first, is left to D5.
    let input = GenerationInput::default();
    noted(&input, &[CPR]);
    let eaten = input.accept_native(t0).unwrap();
    input.enqueue(Writer::Human, CPR_REPLY);
    let later = t0 + T_TRIGGER * 50;
    assert_eq!(input.hazards(later).0, [EntryView::Latched { seq: eaten }]);
    let acted = input.accept_native(later).unwrap();
    let mut acted_record = HazardPop::Unspent;
    input.records_pending(later + ms(1), true);
    assert_eq!(
        input.pop_reported(&mut acted_record, acted, later + ms(1)),
        Pop::Entry(eaten)
    );
    input.records_pending(later + ms(2), false);
    assert_eq!(
        input.hazards(later + ms(2)).0,
        [EntryView::Quiet {
            seq: acted,
            paused: false
        }]
    );
    assert!(input.hazards_clear(later + ms(2) + T_TRIGGER));
}

/// Scenario: Accept 16 candidates and then more, popping some kept ones in between. The seventeenth and every later one are counted, not kept, even once a kept slot frees; a counted one pops only when no kept entry is live and `native_seen` reaches the newest counted; after the count reaches zero, candidates are kept again.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_overflow_into_extra() {
    let t0 = Instant::now();
    let mut f = fifo_at(t0);
    for _ in 0..HAZARD_SLOTS {
        f.accept(t0, false);
    }
    let first_extra = f.accept(t0 + ms(1), false).unwrap();
    assert_eq!((f.extra(), first_extra), (1, HAZARD_SLOTS as u64 + 1));
    let mut r = HazardPop::Unspent;
    assert_eq!(f.pop_reported(&mut r, 1, t0 + ms(2)), Pop::Entry(1));
    let second_extra = f.accept(t0 + ms(3), false).unwrap();
    assert_eq!(f.extra(), 2, "no slot reuse while counting");
    assert_eq!(views(&mut f, t0 + ms(3)).len(), HAZARD_SLOTS - 1);
    // Quiet kept entries leave under D5; counted ones are latched and don't.
    let quiet_gone = t0 + T_TRIGGER + ms(4);
    assert!(views(&mut f, quiet_gone).is_empty());
    let mut r = HazardPop::Unspent;
    assert_eq!(
        f.pop_reported(&mut r, second_extra - 1, quiet_gone),
        Pop::Nothing
    );
    let mut r = HazardPop::Unspent;
    assert_eq!(f.pop_reported(&mut r, second_extra, quiet_gone), Pop::Extra);
    assert!(!f.clear(quiet_gone));
    let mut r = HazardPop::Unspent;
    assert_eq!(
        f.pop_at_drain(&mut r, t0 + ms(3), quiet_gone),
        Pop::Nothing,
        "tie with the newest"
    );
    let mut r = HazardPop::Unspent;
    assert_eq!(f.pop_at_drain(&mut r, t0 + ms(4), quiet_gone), Pop::Extra);
    assert!(f.clear(quiet_gone));
    let kept = f.accept(quiet_gone, true).unwrap();
    assert_eq!(
        views(&mut f, quiet_gone),
        [EntryView::Latched { seq: kept }]
    );
    assert_eq!(f.extra(), 0);
}

/// Scenario: With a latched candidate, write replies (solicited, duplicate, split and pasted), a CR, a deck write, an external edit, and emit an unmatched `?2004l`…`?2004h` pair and a frame, then let an hour pass. None of them pops it.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_nothing_but_an_invocation_pops() {
    let t0 = Instant::now();
    let input = Arc::new(GenerationInput::default());
    let mut hook = OutputHook::new(Arc::clone(&input));
    hook.on_output(b"\x1b[6n\x1b[c");
    input.accept_native(t0).unwrap();
    for bytes in [
        CPR_REPLY,
        DA1_REPLY,
        DA1_REPLY,
        b"\x1b[5",
        b";1R",
        b"\x1b[200~\x07\x1b[201~",
        b"\r",
    ] {
        input.enqueue(Writer::Human, bytes);
    }
    input.enqueue(Writer::UnnamedDeck, b"msg\r");
    input.external_edit();
    hook.on_output(b"\x1b[?2004l\x1b[?2004h\x1b[?2026hframe");
    assert_eq!(
        input.hazards(t0 + Duration::from_secs(3600)).0,
        [EntryView::Latched { seq: 1 }]
    );
}
