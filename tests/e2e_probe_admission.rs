#![cfg(feature = "e2e")]

//! Scripted writer sections over the public probe interfaces. No daemon,
//! terminal or agent is spawned: these are contracts for S-20's integration.

use dot_agent_deck::composer_probe::Permission;
use dot_agent_deck::composer_probe::admission::{self, Attempt, Fence, Outside, Refusal};
use dot_agent_deck::composer_probe::epoch::{GenerationInput, OutputNote, Plan, Writer};
use dot_agent_deck::composer_probe::observe::{Page, Seam};
use dot_agent_deck::composer_probe::planner::{Boundary, Physical, PhysicalTracker};
use dot_agent_deck::composer_probe::probe;
use dot_agent_deck::composer_probe::query_ledger::{
    CprWaitEnd, HazardFifo, HazardPop, LEDGER_SLOTS, QueryKind, T_TRIGGER,
};
use dot_agent_deck::composer_probe::record::{
    Attribution, Cause, Channel, Completion, Decision, EcPolicy, Hold, NotOwned, Observation,
    RecordKey, Records, Report, Route, ScopeState, Tree,
};
use dot_agent_deck::doorbell_screen::CODEX_PROFILE;
use dot_agent_deck::doorbell_txn::{JobClaim, Start, StartCtx};
use spec::spec;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const G: u64 = 19;
const KEY: RecordKey = RecordKey { pid: 59, start: 1 };

#[derive(Clone, Default)]
struct PageLog(Arc<Mutex<Vec<(u32, u64)>>>);

impl PageLog {
    fn seen(&self) -> Vec<(u32, u64)> {
        self.0.lock().unwrap().clone()
    }
}

impl Page for PageLog {
    fn publish(&mut self, count: u32) {
        let mut log = self.0.lock().unwrap();
        let seg = log.last().map_or(0, |&(_, seg)| seg);
        log.push((count, seg));
    }

    fn publish_probe(&mut self, count: u32, seg: u64) {
        self.0.lock().unwrap().push((count, seg));
    }
}

fn fresh(now: Instant) -> (Seam<PageLog>, PageLog) {
    fresh_generation(now, G)
}

fn fresh_generation(now: Instant, generation: u64) -> (Seam<PageLog>, PageLog) {
    let input = Arc::new(GenerationInput::new(CprWaitEnd::Counted));
    assert!(input.attach_records(Records::new(generation, EcPolicy::Hold, now)));
    let log = PageLog::default();
    let mut seam = Seam::new(generation, input, log.clone());
    assert!(seam.set_permission(Permission::Armable));
    (seam, log)
}

fn outside(attempt: Attempt) -> Outside {
    Outside {
        attempt,
        permission: Permission::Armable,
        fence: if matches!(attempt, Attempt::Lapse { .. }) {
            Fence::Lapsed
        } else {
            Fence::Open
        },
        human_ticket: false,
        boundary: Boundary::Ground,
        keymap: Ok(()),
        surface: true,
    }
}

fn ctx(now: Instant) -> StartCtx {
    StartCtx {
        boundary: Boundary::Ground,
        retired: false,
        now,
    }
}

// This sink models the caller's write after Start::Started. Its assertions
// establish the section contract, not that the live writer uses the section.
fn write_probe<P: Page>(
    seam: &mut Seam<P>,
    outside: Outside,
    ctx: StartCtx,
) -> (Start<Refusal>, Vec<u8>) {
    let started = probe::start(
        &JobClaim::default(),
        &ctx,
        |ctx| {
            if ctx.retired {
                return Err(Refusal::Retired);
            }
            seam.arm(
                19,
                &Outside {
                    boundary: ctx.boundary,
                    ..outside
                },
                ctx.now,
            )
            .map(drop)
        },
        || panic!("no withdrawal was injected"),
    );
    let bytes = if started == Start::Started {
        vec![0x07]
    } else {
        Vec::new()
    };
    (started, bytes)
}

fn observation(attribution: Attribution, decision: Option<Decision>, now: Instant) -> Observation {
    Observation {
        generation: G,
        key: KEY,
        channel: if decision.is_some() {
            Channel::Ipc
        } else {
            Channel::Scan
        },
        attribution,
        claim: None,
        decision,
        seg_seen: None,
        native_seen: None,
        birth_floor: now,
    }
}

fn attempts(input: &GenerationInput) -> [Attempt; 3] {
    input.enqueue(Writer::UnnamedDeck, b"\x07");
    [
        Attempt::First,
        Attempt::Retry,
        Attempt::Lapse {
            lapsed_e0: input.epoch().epoch() - 1,
        },
    ]
}

/// Scenario: Schedule a probe while the composer is eligible, then observe an undecided helper or a tool editor before its writer section runs. First probes, scheduled retries and lapse retries produce no probe byte or arm publication after the scope becomes pending or open; a fresh eligible generation writes one probe, and disarming its liability publishes zero.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_scope_changed_after_scheduling_refuses_each_write_path() {
    let now = Instant::now();
    for decision in [None, Some(Decision::Tool)] {
        for n in 0..3 {
            let (mut seam, log) = fresh(now);
            let attempt = attempts(seam.input())[n];
            let request = outside(attempt);
            assert_eq!(seam.admits(&request, now), Ok(()));
            seam.observe(
                &observation(Attribution::NotOwned(NotOwned::Tool), decision, now),
                now,
            )
            .unwrap();
            let hold = if decision.is_some() {
                Hold::Scope(ScopeState::Open(Cause::EditorReturnUnchecked))
            } else {
                Hold::Scope(ScopeState::Pending)
            };
            let (result, bytes) = write_probe(&mut seam, request, ctx(now));
            assert_eq!(
                result,
                Start::Refused(Refusal::Records(hold)),
                "{attempt:?}"
            );
            assert!(bytes.is_empty(), "refused {attempt:?} wrote {bytes:?}");
            assert!(
                log.seen().is_empty(),
                "refused {attempt:?} published {:?}",
                log.seen()
            );
        }
    }
    let (mut seam, log) = fresh(now);
    assert_eq!(
        write_probe(&mut seam, outside(Attempt::First), ctx(now)),
        (Start::Started, vec![7])
    );
    assert_eq!(log.seen(), [(1, 19)]);
    assert_eq!(seam.disarm(), Ok(0));
    assert_eq!(log.seen(), [(1, 19), (0, 19)]);
}

/// Scenario: Observe an armed owned helper whose required invocation is incomplete after a probe was scheduled. Each probe path refuses without a byte or publication; once the helper exits and its matched helper-only completion is supplied, the next probe writes once.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_required_invocation_blocks_until_matched_completion() {
    let now = Instant::now();
    for n in 0..3 {
        let (mut seam, log) = fresh(now);
        let attempt = attempts(seam.input())[n];
        let request = outside(attempt);
        assert_eq!(seam.admits(&request, now), Ok(()));
        seam.observe(
            &observation(Attribution::Owned, Some(Decision::Armed), now),
            now,
        )
        .unwrap();
        let (result, bytes) = write_probe(&mut seam, request, ctx(now));
        assert_eq!(
            result,
            Start::Refused(Refusal::Records(Hold::Incomplete(KEY)))
        );
        assert!(bytes.is_empty());
        assert!(log.seen().is_empty());
        seam.input().record_match(G, KEY, 18).unwrap();
        seam.exit(KEY, now, now).unwrap();
        seam.input()
            .record_complete(G, KEY, Route::HelperOnly, now, now)
            .unwrap();
        assert_eq!(
            write_probe(&mut seam, request, ctx(now)),
            (Start::Started, vec![7])
        );
        assert_eq!(log.seen(), [(1, 19)]);
    }
}

/// Scenario: A tool helper reports a surviving writer, no reliable editor end, or an unarmed spawn failure, then exits and is pruned. The surviving scope still refuses the next probe even though the invocation is not required and its record is gone; a refused scan alone leaves an eligible generation able to probe.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_pruning_a_tool_record_does_not_restore_probe_permission() {
    let now = Instant::now();
    for (decision, report, cause) in [
        (
            Decision::Tool,
            Report::Done(Tree::Survivors),
            Cause::EditorLeftWriter,
        ),
        (Decision::Tool, Report::None, Cause::EditorEndUnknown),
        (
            Decision::Unarmed,
            Report::SpawnFailed {
                errno: Some(libc::ENOENT),
            },
            Cause::EditorReturnUnchecked,
        ),
    ] {
        let (mut seam, log) = fresh(now);
        seam.observe(
            &observation(Attribution::NotOwned(NotOwned::Tool), Some(decision), now),
            now,
        )
        .unwrap();
        assert_eq!(
            seam.input().record(KEY).unwrap().completion,
            Completion::NotRequired
        );
        seam.input().record_report(G, KEY, report, now).unwrap();
        seam.exit(KEY, now, now).unwrap();
        seam.drained(KEY, now).unwrap();
        assert!(seam.prune(KEY, true, now).unwrap().is_some());
        let (result, bytes) = write_probe(&mut seam, outside(Attempt::Retry), ctx(now));
        assert_eq!(
            result,
            Start::Refused(Refusal::Records(Hold::Scope(ScopeState::Open(cause))))
        );
        assert!(bytes.is_empty());
        assert!(log.seen().is_empty());
    }
    let (mut seam, log) = fresh(now);
    seam.refused(Channel::Scan, None, None, now).unwrap();
    assert_eq!(
        write_probe(&mut seam, outside(Attempt::First), ctx(now)),
        (Start::Started, vec![7])
    );
    assert_eq!(log.seen(), [(1, 19)]);
}

/// Scenario: Schedule an eligible check, then let the writer see a lone Escape, an incomplete sequence, a retired context, or a newly moved redraw estimate. Each refuses without a byte or arm publication; strict ground and reaching the latest estimate restore admission for first probes, scheduled retries and lapse retries.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_writer_context_and_latest_o5_override_scheduling() {
    let now = Instant::now();
    for n in 0..3 {
        for (boundary, retired, refusal) in [
            (
                Boundary::Escape,
                false,
                Refusal::OffGround(Boundary::Escape),
            ),
            (
                Boundary::Inside,
                false,
                Refusal::OffGround(Boundary::Inside),
            ),
            (Boundary::Ground, true, Refusal::Retired),
        ] {
            let (mut seam, log) = fresh(now);
            let attempt = attempts(seam.input())[n];
            assert_eq!(seam.admits(&outside(attempt), now), Ok(()));
            let (result, bytes) = write_probe(
                &mut seam,
                outside(attempt),
                StartCtx {
                    boundary,
                    retired,
                    now,
                },
            );
            assert_eq!(result, Start::Refused(refusal));
            assert!(bytes.is_empty());
            assert!(log.seen().is_empty());
        }
        let input = Arc::new(GenerationInput::profiled(
            CprWaitEnd::SyncBegin,
            Some(&CODEX_PROFILE.redraw),
            now,
        ));
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, now)));
        let attempt = attempts(&input)[n];
        let log = PageLog::default();
        let mut seam = Seam::new(G, input.clone(), log.clone());
        seam.set_permission(Permission::Armable);
        input.size_change(now);
        let old = input.query_wait_estimate().unwrap();
        assert_eq!(seam.admits(&outside(attempt), old), Ok(()));
        input.size_change(old);
        let moved = input.query_wait_estimate().unwrap();
        assert!(moved > old);
        assert_eq!(
            admission::o5(Some(moved), old),
            Err(Refusal::Estimate(moved))
        );
        let (result, bytes) = write_probe(&mut seam, outside(attempt), ctx(old));
        assert_eq!(result, Start::Refused(Refusal::Estimate(moved)));
        assert!(bytes.is_empty());
        assert!(log.seen().is_empty());
        assert_eq!(input.query_wait_estimate(), Some(moved));
        assert_eq!(
            write_probe(&mut seam, outside(attempt), ctx(moved)),
            (Start::Started, vec![7])
        );
        assert_eq!(log.seen(), [(1, 19)]);
    }
}

/// Scenario: Schedule a probe, retire its generation before the writer section, then try the old job. It refuses without publishing or writing, while a successor generation starts its own probe once.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_retirement_before_arm_leaves_the_page_untouched() {
    let now = Instant::now();
    let (mut seam, log) = fresh(now);
    assert_eq!(seam.admits(&outside(Attempt::First), now), Ok(()));
    seam.input().retire();
    assert_eq!(
        write_probe(&mut seam, outside(Attempt::First), ctx(now)),
        (Start::Refused(Refusal::Retired), vec![])
    );
    assert!(log.seen().is_empty());
    let (mut next, next_log) = fresh_generation(now, G + 1);
    assert_eq!(
        write_probe(&mut next, outside(Attempt::First), ctx(now)),
        (Start::Started, vec![7])
    );
    assert_eq!(next_log.seen(), [(1, 19)]);
    assert!(log.seen().is_empty());
}

struct RetirementPage {
    log: PageLog,
    publish: mpsc::Sender<()>,
    retired: mpsc::Receiver<()>,
}

impl Page for RetirementPage {
    fn publish(&mut self, count: u32) {
        self.log.publish(count);
    }

    fn publish_probe(&mut self, count: u32, seg: u64) {
        self.publish.send(()).unwrap();
        // Old code lets retirement complete after the count read and before
        // the page store and history raise.
        // With the arm ordered against retirement, retirement waits for this
        // callback to return. Bound the wait so that implementation can pass.
        match self.retired.recv_timeout(Duration::from_secs(1)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("retirement worker disconnected: {error}"),
        }
        self.log.publish_probe(count, seg);
    }
}

/// Scenario: Start retirement from another thread after admission, at the page callback before its arm store. The arm transaction must finish successfully before retirement; it cannot refuse after leaving a raised word on the page, and a successor still publishes only its own probe.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_retirement_during_publication_cannot_turn_a_published_arm_into_refusal() {
    let now = Instant::now();
    let input = Arc::new(GenerationInput::new(CprWaitEnd::Counted));
    assert!(input.attach_records(Records::new(G, EcPolicy::Hold, now)));
    let (publish, publishing) = mpsc::channel();
    let (retired, retired_rx) = mpsc::channel();
    let retiring = input.clone();
    let worker = std::thread::spawn(move || {
        publishing.recv_timeout(Duration::from_secs(5)).unwrap();
        retiring.retire();
        retired.send(()).unwrap();
    });
    let log = PageLog::default();
    let mut seam = Seam::new(
        G,
        input.clone(),
        RetirementPage {
            log: log.clone(),
            publish,
            retired: retired_rx,
        },
    );
    seam.set_permission(Permission::Armable);
    assert_eq!(seam.admits(&outside(Attempt::First), now), Ok(()));
    let (result, bytes) = write_probe(&mut seam, outside(Attempt::First), ctx(now));
    worker.join().unwrap();
    assert_eq!(
        result,
        Start::Started,
        "retirement interleaving refused after page publication {:?}; scripted bytes {bytes:?}",
        log.seen()
    );
    assert_eq!(log.seen(), [(1, 19)]);
    assert_eq!(bytes, [7]);
    let (mut next, next_log) = fresh_generation(now, G + 1);
    assert_eq!(
        write_probe(&mut next, outside(Attempt::First), ctx(now)),
        (Start::Started, vec![7])
    );
    assert_eq!(next_log.seen(), [(1, 19)]);
    assert_eq!(log.seen(), [(1, 19)]);
}

/// Scenario: Feed whole and byte-split OSC 10/11 replies with ST and BEL through the real planner and physical tracker, with queries retained before saturation or dropped afterwards. Saturation alone permits a probe; a native BEL candidate refuses through the strict quiet deadline and then permits one, while whole retained replies and ST endings permit one immediately.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_report_candidates_control_admission_after_physical_writes() {
    let now = Instant::now();
    for slot in [10, 11] {
        for retained in [true, false] {
            for bel in [true, false] {
                for split in [true, false] {
                    let (mut seam, log) = fresh(now);
                    let input = seam.input().clone();
                    let query = QueryKind::OscColour { slot };
                    if retained {
                        input.on_output(&[OutputNote::Query(query)], now);
                    }
                    input.on_output(&[OutputNote::Query(QueryKind::Da1); LEDGER_SLOTS + 1], now);
                    if !retained {
                        input.on_output(&[OutputNote::Query(query)], now);
                    }
                    assert_eq!(
                        input.admit_probe(&outside(Attempt::First), now),
                        Ok(()),
                        "saturation alone"
                    );
                    let reply = format!(
                        "\x1b]{slot};rgb:1/22/333{}",
                        if bel { "\x07" } else { "\x1b\\" }
                    )
                    .into_bytes();
                    let pieces: Vec<&[u8]> = if split {
                        reply.chunks(1).collect()
                    } else {
                        vec![reply.as_slice()]
                    };
                    let physical = Physical::default();
                    let mut tracker = PhysicalTracker::default();
                    let mut captured = Vec::new();
                    for piece in pieces {
                        let planned = input.enqueue_planned(Writer::Human, piece, Plan::Open);
                        tracker.written(piece, &physical);
                        if let Some(natives) = planned.natives {
                            natives
                                .run()
                                .written(piece, |seq| input.enter_native(seq, now));
                        }
                        captured.extend_from_slice(piece);
                    }
                    assert_eq!(captured, reply);
                    let candidate = bel && (split || !retained);
                    let expected = if candidate {
                        Start::Refused(Refusal::Hazard)
                    } else {
                        Start::Started
                    };
                    let at = now + T_TRIGGER;
                    let (result, bytes) = write_probe(
                        &mut seam,
                        outside(Attempt::First),
                        StartCtx {
                            boundary: physical.boundary(),
                            ..ctx(at)
                        },
                    );
                    assert_eq!(
                        result, expected,
                        "slot={slot}, retained={retained}, BEL={bel}, split={split}"
                    );
                    if candidate {
                        assert!(bytes.is_empty());
                        assert!(log.seen().is_empty());
                        assert_eq!(
                            write_probe(
                                &mut seam,
                                outside(Attempt::Retry),
                                ctx(at + Duration::from_nanos(1))
                            ),
                            (Start::Started, vec![7])
                        );
                    } else {
                        assert_eq!(bytes, [7]);
                    }
                    assert_eq!(log.seen(), [(1, 19)]);
                }
            }
        }
    }
}

/// Scenario: Accept a native candidate during a CPR wait and later answer the CPR. Admission stays refused beyond the quiet period until that candidate is accounted for; separately, a paused quiet clock and an overflow keep the FIFO non-clear, and popping the overflow restores the empty FIFO control.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_latched_paused_and_overflow_hazards_keep_admission_closed() {
    let now = Instant::now();
    let (mut seam, log) = fresh(now);
    seam.input()
        .on_output(&[OutputNote::Query(QueryKind::Cpr)], now);
    let seq = seam.input().accept_native(now).unwrap();
    seam.input().enqueue(Writer::Human, b"\x1b[1;1R");
    let later = now + T_TRIGGER * 100;
    assert_eq!(
        write_probe(&mut seam, outside(Attempt::Retry), ctx(later)),
        (Start::Refused(Refusal::Hazard), vec![])
    );
    assert!(log.seen().is_empty());
    seam.input()
        .pop_reported(&mut HazardPop::Unspent, seq, later);
    assert_eq!(
        write_probe(&mut seam, outside(Attempt::Retry), ctx(later)),
        (Start::Started, vec![7])
    );
    let mut fifo = HazardFifo::default();
    assert!(!fifo.exhausted());
    fifo.accept(now, false).unwrap();
    fifo.records_pending(now, true);
    assert!(!fifo.clear_at(later));
    fifo.records_pending(later, false);
    assert!(!fifo.clear_at(later + T_TRIGGER));
    assert!(fifo.clear_at(later + T_TRIGGER + Duration::from_nanos(1)));
    assert!(fifo.clear(later + T_TRIGGER + Duration::from_nanos(1)));
    for _ in 0..=dot_agent_deck::composer_probe::query_ledger::HAZARD_SLOTS {
        fifo.accept(later, false).unwrap();
    }
    assert!(!fifo.clear_at(later + T_TRIGGER * 100));
    assert!(
        !fifo.exhausted(),
        "a count beyond the slots alone is not exhaustion"
    );
    for _ in 0..=dot_agent_deck::composer_probe::query_ledger::HAZARD_SLOTS {
        fifo.pop_reported(&mut HazardPop::Unspent, u64::MAX, later + T_TRIGGER * 100);
    }
    assert!(fifo.clear_at(later + T_TRIGGER * 100));
}
