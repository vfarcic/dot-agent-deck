//! PRD #1666 part 3 (Q35-14b; core §2.4 and §5.4, amendments 1–6 of
//! Q35-core): the daemon's side of an installed generation's editor helper
//! (`src/composer_probe/observe.rs`), pure over scripted connections: the
//! seam that applies an attested invocation, scan or marker to the real
//! invocation records through a generation's input lock and publishes the
//! count to a page; what the helper is told; the epoch's external-edit move;
//! E2's clock; the budgets; the drain's reschedule; and the `e2e` build's
//! control verb. The real processes, helper and arm page are
//! `inbox/probe/021`, `022` and `039`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::epoch::GenerationInput;
use dot_agent_deck::composer_probe::observe::{
    ControlOp, ControlRefusal, ControlStep, E2_AFTER, Evidence, Page, Pool, READ_DEADLINE, Seam,
    Slot, TestLiability, drain_ready, e2_due, marker_observation, outcome, report,
    scan_observation,
};
use dot_agent_deck::composer_probe::record::{
    Attribution, Cause, Decision, EcPolicy, Marker, NotConsumed, NotOwned, RecordKey, Recorded,
    Records, Refusal, Report, ScopeState, Settlement, Tree,
};
use dot_agent_deck::composer_probe::{InertCause, Permission};
use dot_agent_deck::event::{
    ComposerClaim, ComposerDecision, ComposerInvocationRequest, ComposerOutcome, EditorReport,
    EditorTree, ExportRead,
};
use spec::spec;

const G: u64 = 41;

fn key(pid: u32) -> RecordKey {
    RecordKey {
        pid,
        start: 9_000 + u64::from(pid),
    }
}

/// A page that keeps every count published to it.
#[derive(Debug, Default)]
struct Book(Vec<u32>);

impl Page for Book {
    fn publish(&mut self, count: u32) {
        self.0.push(count);
    }
}

/// A seam over a fresh generation with its records attached, under `Hold`.
fn seam(permission: Permission) -> Seam<Book> {
    let input = Arc::new(GenerationInput::default());
    assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
    let mut seam = Seam::new(G, input, Book::default());
    seam.set_permission(permission);
    seam
}

fn request(decision: ComposerDecision, claim: Option<ComposerClaim>) -> ComposerInvocationRequest {
    ComposerInvocationRequest {
        pane_id: "p".into(),
        token: None,
        generation: G,
        decision,
        claim,
        seg_seen: Some(0),
        native_seen: Some(0),
        argv: vec!["/tmp/claude-1000/claude-prompt-1.md".into()],
    }
}

fn evidence(pid: u32, attribution: Attribution, read: Option<ExportRead>) -> Evidence {
    Evidence {
        key: key(pid),
        birth_floor: Instant::now(),
        attribution,
        read,
    }
}

fn armed() -> ComposerInvocationRequest {
    request(ComposerDecision::Armed, Some(ComposerClaim::Owned))
}

fn count(seam: &Seam<Book>) -> u32 {
    seam.input().liability_count().unwrap()
}

/// Scenario: Publish one liability on an armable generation and hand the seam a sequence of scripted helper connections: an `Owned` `Armed` invocation with an empty export consumes it once, is told `Mine` and the page goes to 0; the same invocation again, a second `Owned` helper at a count of 0, a tool's, an unattributable one, one whose claim disagrees, one the identity refusal dropped and one naming another generation are each told `Uncertain` and consume nothing; an `Owned` helper whose export couldn't be read consumes and is told `Unreadable`, never `Empty`.
#[spec("inbox/probe/069")]
#[test]
fn probe_069_the_handler_settles_only_an_attested_armed_invocation() {
    let mut s = seam(Permission::Armable);
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    let now = Instant::now();
    let owned = evidence(1, Attribution::Owned, Some(ExportRead::Empty));
    assert_eq!(
        s.invocation(&armed(), Some(owned), now),
        ComposerOutcome::Mine {
            read: ExportRead::Empty
        }
    );
    assert_eq!(count(&s), 0);
    assert_eq!(s.page().0, [1, 0], "the page follows the records' count");
    // The settled record's outcome stands; nothing more is consumed.
    assert_eq!(
        s.invocation(&armed(), Some(owned), now),
        ComposerOutcome::Mine {
            read: ExportRead::Empty
        }
    );
    assert_eq!(s.page().0, [1, 0]);
    assert_eq!(
        s.input().record(key(1)).unwrap().settlement,
        Settlement::Consumed
    );
    // Another `Owned` helper with no liability left.
    assert_eq!(
        s.invocation(
            &armed(),
            Some(evidence(2, Attribution::Owned, Some(ExportRead::Empty))),
            now
        ),
        ComposerOutcome::Uncertain
    );
    assert_eq!(
        s.input().record(key(2)).unwrap().settlement,
        Settlement::NotConsumed(NotConsumed::AlreadyZero)
    );

    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    for (pid, attribution, claim) in [
        (
            3,
            Attribution::NotOwned(NotOwned::Tool),
            ComposerClaim::Tool,
        ),
        (
            4,
            Attribution::NotOwned(NotOwned::Unknown),
            ComposerClaim::Unknown,
        ),
        (5, Attribution::Unattributable, ComposerClaim::Owned),
        (
            6,
            Attribution::NotOwned(NotOwned::Shape),
            ComposerClaim::Owned,
        ),
        // The daemon reads `Owned` but the helper's own class disagreed.
        (7, Attribution::Owned, ComposerClaim::Unknown),
    ] {
        let got = s.invocation(
            &request(ComposerDecision::Armed, Some(claim)),
            Some(evidence(pid, attribution, None)),
            now,
        );
        assert_eq!(got, ComposerOutcome::Uncertain, "{attribution:?} {claim:?}");
        assert_eq!(count(&s), 1, "{attribution:?}: the liability stays");
    }
    assert_eq!(
        s.invocation(&armed(), None, now),
        ComposerOutcome::Uncertain,
        "an identity refusal makes no record"
    );
    let mut other = armed();
    other.generation = G + 1;
    assert_eq!(
        s.invocation(&other, Some(evidence(8, Attribution::Owned, None)), now),
        ComposerOutcome::Uncertain
    );
    assert!(s.input().record(key(8)).is_none(), "another generation's");
    assert_eq!(count(&s), 1);
    assert_eq!(s.page().0, [1, 0, 1]);

    // An `Owned` helper whose export didn't read: consumed, unreadable.
    let unreadable = evidence(9, Attribution::Owned, Some(ExportRead::Unreadable));
    assert_eq!(
        s.invocation(&armed(), Some(unreadable), now),
        ComposerOutcome::Mine {
            read: ExportRead::Unreadable
        }
    );
    let no_read = evidence(10, Attribution::Owned, None);
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    assert_eq!(
        s.invocation(&armed(), Some(no_read), now),
        ComposerOutcome::Mine {
            read: ExportRead::Unreadable
        },
        "an `Owned` record with no read is never `Empty`"
    );
    assert_eq!(count(&s), 0);
}

/// Scenario: Hand the seam an `Unarmed`, a `Tool` and an unknown decision, each from its own helper, and an `Armed` one. Each non-`Armed` one moves the generation's input epoch once as an external edit before its outcome comes back, and the writer scope is already open by then, so the reply can't go out first; the `Armed` one moves nothing.
#[spec("inbox/probe/069")]
#[test]
fn probe_069_an_invocation_that_is_not_the_decks_moves_the_epoch_before_its_reply() {
    let mut s = seam(Permission::Armable);
    let now = Instant::now();
    for (pid, decision, expected) in [
        (1, ComposerDecision::Unarmed, ComposerOutcome::Unarmed),
        (2, ComposerDecision::Tool, ComposerOutcome::Unarmed),
        (3, ComposerDecision::Unknown, ComposerOutcome::Uncertain),
    ] {
        let before = s.input().epoch();
        let got = s.invocation(
            &request(decision, Some(ComposerClaim::Owned)),
            Some(evidence(pid, Attribution::Owned, None)),
            now,
        );
        let after = s.input().epoch();
        assert_eq!(got, expected, "{decision:?}");
        assert_eq!(after.epoch(), before.epoch() + 1, "{decision:?}: one move");
        assert_eq!(after.last_foreign(), after.epoch(), "foreign: no deck job");
        assert!(after.touched());
        assert!(
            matches!(
                s.input().writer_scope().unwrap().state(),
                ScopeState::Open(Cause::EditorReturnUnchecked)
            ),
            "{decision:?}: open before the reply"
        );
    }
    let before = s.input().epoch();
    s.invocation(
        &armed(),
        Some(evidence(4, Attribution::Owned, Some(ExportRead::Empty))),
        now,
    );
    assert_eq!(
        s.input().epoch(),
        before,
        "an `Armed` invocation moves nothing"
    );
}

/// Scenario: Observe a helper's `Armed` decision 30 s after the generation started, then a duplicate 3 s later. E2 isn't due 4.9 s after the first observation and is due at 5 s, whatever the helper's birth or the duplicate; a helper seen only by a scan, one that decided `Unarmed`, and an `Armed` one whose exit was observed are never due.
#[spec("inbox/probe/069")]
#[test]
fn probe_069_e2_runs_from_the_first_armed_observation() {
    let t = Instant::now();
    let mut s = seam(Permission::Armable);
    let first = t + Duration::from_secs(30);
    let born_early = Evidence {
        birth_floor: t,
        ..evidence(1, Attribution::Owned, Some(ExportRead::Empty))
    };
    s.invocation(&armed(), Some(born_early), first);
    s.invocation(&armed(), Some(born_early), first + Duration::from_secs(3));
    let r = s.input().record(key(1)).unwrap();
    assert_eq!(
        r.first_armed_seen,
        Some(first),
        "a duplicate doesn't move it"
    );
    assert!(!e2_due(&r, first + E2_AFTER - Duration::from_millis(100)));
    assert!(e2_due(&r, first + E2_AFTER));
    assert!(!e2_due(&r, first), "not at its decision");

    let obs = scan_observation(G, key(2), Attribution::Owned, t);
    s.observe(&obs, first).unwrap();
    s.invocation(
        &request(ComposerDecision::Unarmed, Some(ComposerClaim::Owned)),
        Some(evidence(3, Attribution::Owned, None)),
        first,
    );
    let late = first + Duration::from_secs(60);
    for pid in [2, 3] {
        assert!(!e2_due(&s.input().record(key(pid)).unwrap(), late), "{pid}");
    }
    s.exit(
        key(1),
        first + Duration::from_secs(1),
        first + Duration::from_secs(1),
    )
    .unwrap();
    assert!(!e2_due(&s.input().record(key(1)).unwrap(), late), "exited");
}

/// Scenario: Take units from a generation's pool of two and a total of three: the third from one generation is refused, a second generation gets one and is then refused by the full total (giving its own unit back), and dropping a slot returns a unit to both, which never go below zero.
#[spec("inbox/probe/070")]
#[test]
fn probe_070_each_budget_holds_per_generation_and_in_total() {
    let total: &'static Pool = Box::leak(Box::new(Pool::new(3)));
    let a = Arc::new(Pool::new(2));
    let b = Arc::new(Pool::new(2));
    let one = Slot::take(&a, total).unwrap();
    let two = Slot::take(&a, total).unwrap();
    assert!(Slot::take(&a, total).is_none(), "the generation's bound");
    assert_eq!((a.used(), total.used()), (2, 2));
    let three = Slot::take(&b, total).unwrap();
    assert!(Slot::take(&b, total).is_none(), "the total's bound");
    assert_eq!((b.used(), total.used()), (1, 3), "b's unit given back");
    drop(one);
    assert_eq!((a.used(), total.used()), (1, 2));
    let four = Slot::take(&b, total).unwrap();
    drop((two, three, four));
    assert_eq!((a.used(), b.used(), total.used()), (0, 0, 0));
    total.give();
    a.give();
    assert_eq!((a.used(), total.used()), (0, 0), "never below zero");
    assert!(Pool::new(0).used() == 0 && !Pool::new(0).take());
}

/// Scenario: Ask whether an exited helper's drain may settle: with a connection open it is rescheduled until the read deadline has passed since its exit, and settles at once with none open. Then drive a decisionless `Owned` helper through the seam: born after a liability was published, its exit observed, and not drained, it stays open and consumes nothing; drained once, its exit consumes the liability and the page follows; drained again, nothing moves.
#[spec("inbox/probe/070")]
#[test]
fn probe_070_the_drain_reschedules_while_evidence_is_undrained() {
    let exited = Instant::now();
    assert!(!drain_ready(1, exited, exited));
    assert!(!drain_ready(
        2,
        exited,
        exited + READ_DEADLINE - Duration::from_millis(1)
    ));
    assert!(drain_ready(1, exited, exited + READ_DEADLINE));
    assert!(drain_ready(0, exited, exited));

    let mut s = seam(Permission::Armable);
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    let born = Instant::now();
    let obs = scan_observation(G, key(1), Attribution::Owned, born);
    assert!(matches!(
        s.observe(&obs, born),
        Ok(Recorded::Kept {
            settlement: Settlement::Open,
            ..
        })
    ));
    assert_eq!(
        s.input().writer_scope().unwrap().state(),
        ScopeState::Pending,
        "a decisionless sighting is undecided"
    );
    let exit = born + Duration::from_millis(5);
    s.exit(key(1), exit, exit).unwrap();
    // Rescheduled: nothing settles before the drain.
    assert_eq!(
        s.input().record(key(1)).unwrap().settlement,
        Settlement::Open
    );
    assert_eq!(count(&s), 1);
    let drained = s.drained(key(1), exit + READ_DEADLINE).unwrap();
    assert_eq!(drained.settlement, Settlement::Consumed);
    assert_eq!(count(&s), 0);
    assert_eq!(s.page().0, [1, 0]);
    assert!(s.drained(key(1), exit + READ_DEADLINE).is_ok());
    assert_eq!(s.page().0, [1, 0], "a second drain changes nothing");
}

/// Scenario: Address the control verb to another generation, to a generation whose permission is pending or inert, and to an armable one: only the last publishes. Publishing twice raises the count once and withdrawing twice lowers it once and never below zero; after the liability is consumed, a withdrawal does nothing and a new publication raises the count again. Its stepping is checked on its own for each operation and count, and a retired generation refuses.
#[spec("inbox/probe/071")]
#[test]
fn probe_071_the_control_verb_is_bounded_and_idempotent() {
    for permission in [
        Permission::Pending,
        Permission::Inert(InertCause::UnmeasuredRelease),
    ] {
        let mut s = seam(permission);
        assert_eq!(
            s.control(G, ControlOp::Publish),
            Err(ControlRefusal::Permission(permission))
        );
        assert!(s.page().0.is_empty());
    }
    let mut s = seam(Permission::Armable);
    assert_eq!(
        s.control(G + 1, ControlOp::Publish),
        Err(ControlRefusal::Generation)
    );
    assert_eq!(
        s.control(G - 1, ControlOp::Withdraw),
        Err(ControlRefusal::Generation)
    );
    assert!(s.page().0.is_empty(), "a refused generation writes nothing");
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    assert_eq!(s.control(G, ControlOp::Withdraw), Ok(0));
    assert_eq!(s.control(G, ControlOp::Withdraw), Ok(0));
    assert_eq!(s.page().0, [1, 0]);
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    s.invocation(
        &armed(),
        Some(evidence(1, Attribution::Owned, Some(ExportRead::Empty))),
        Instant::now(),
    );
    assert_eq!(
        s.control(G, ControlOp::Withdraw),
        Ok(0),
        "consumed: nothing"
    );
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1), "a new one");
    assert_eq!(s.page().0, [1, 0, 1, 0, 1]);

    let mut t = TestLiability::default();
    assert_eq!(t.step(ControlOp::Withdraw, 0), ControlStep::Unchanged);
    assert_eq!(t.step(ControlOp::Publish, 0), ControlStep::Raise);
    assert_eq!(t.step(ControlOp::Publish, 1), ControlStep::Unchanged);
    assert_eq!(
        t.step(ControlOp::Publish, 0),
        ControlStep::Raise,
        "consumed"
    );
    assert_eq!(t.step(ControlOp::Withdraw, 1), ControlStep::Lower);
    assert_eq!(t.step(ControlOp::Withdraw, 1), ControlStep::Unchanged);

    s.input().retire();
    assert_eq!(
        s.control(G, ControlOp::Withdraw),
        Err(ControlRefusal::Records(Refusal::Stale))
    );
}

/// Scenario: Feed one generation's seam the channels' evidence in the orders the integration can see: two helpers sighted by the scan and undecided, only one then deciding by marker; a marker and then the IPC for the same `Armed` helper; an IPC before the scan; an exit observed before the IPC decision is drained; and a late IPC after the exit drain. The count drops exactly once per consumed record, the scope stays pending while one sighting is undecided and closes when its exit drains, and nothing after a settlement or a retirement moves the count.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_the_real_channels_settle_each_record_once() {
    let mut s = seam(Permission::Armable);
    let t = Instant::now();
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    let born = Instant::now();
    for pid in [1, 2] {
        s.observe(
            &scan_observation(G, key(pid), Attribution::Owned, born),
            born,
        )
        .unwrap();
    }
    assert_eq!(s.input().writer_scope().unwrap().undecided(), 2);
    let marker = Marker {
        key: key(1),
        decision: Decision::Armed,
        seen: Some((0, 0)),
        renamed: false,
    };
    let applied = s.observe(
        &marker_observation(G, &marker, Attribution::Pending, born),
        born,
    );
    assert!(matches!(
        applied,
        Ok(Recorded::Kept {
            settlement: Settlement::Consumed,
            ..
        })
    ));
    assert_eq!(count(&s), 0);
    assert_eq!(
        s.input().writer_scope().unwrap().state(),
        ScopeState::Pending,
        "the other sighting is still undecided"
    );
    // The same helper's IPC after its marker: the settled outcome, once.
    assert_eq!(
        s.invocation(
            &armed(),
            Some(evidence(1, Attribution::Owned, Some(ExportRead::Empty))),
            born
        ),
        ComposerOutcome::Mine {
            read: ExportRead::Empty
        }
    );
    assert_eq!(count(&s), 0);
    assert_eq!(s.page().0, [1, 0]);
    // The undecided one's exit drains with no proof (the count was 0).
    s.exit(key(2), born, born).unwrap();
    let drained = s.drained(key(2), born).unwrap();
    assert_eq!(
        drained.settlement,
        Settlement::NotConsumed(NotConsumed::Unproven)
    );
    assert!(drained.rescan || s.input().writer_scope().unwrap().undecided() == 0);

    // IPC before the scan, then the scan: nothing more.
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    let born3 = Instant::now();
    let three = Evidence {
        birth_floor: born3,
        ..evidence(3, Attribution::Owned, Some(ExportRead::NonEmpty))
    };
    assert_eq!(
        s.invocation(&armed(), Some(three), born3),
        ComposerOutcome::Mine {
            read: ExportRead::NonEmpty
        }
    );
    s.observe(
        &scan_observation(G, key(3), Attribution::Owned, born3),
        born3,
    )
    .unwrap();
    assert_eq!(count(&s), 0);

    // An exit observed before the decision's drain: the decision settles.
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    let born4 = Instant::now();
    s.observe(
        &scan_observation(G, key(4), Attribution::Owned, born4),
        born4,
    )
    .unwrap();
    s.exit(key(4), born4, born4).unwrap();
    let four = Evidence {
        birth_floor: born4,
        ..evidence(4, Attribution::Owned, Some(ExportRead::Empty))
    };
    assert!(matches!(
        s.invocation(&armed(), Some(four), born4),
        ComposerOutcome::Mine { .. }
    ));
    assert_eq!(count(&s), 0);
    let drained = s.drained(key(4), born4).unwrap();
    assert_eq!(drained.settlement, Settlement::Consumed, "once");
    assert_eq!(count(&s), 0, "the drain doesn't consume again");

    // A late IPC after a drained, settled exit changes nothing.
    assert_eq!(s.control(G, ControlOp::Publish), Ok(1));
    assert!(matches!(
        s.invocation(&armed(), Some(four), born4),
        ComposerOutcome::Mine { .. }
    ));
    assert_eq!(count(&s), 1, "a later liability survives late evidence");
    assert_eq!(s.page().0, [1, 0, 1, 0, 1, 0, 1]);

    // Retired: nothing applies, nothing is consumed.
    s.input().retire();
    assert_eq!(
        s.invocation(
            &armed(),
            Some(evidence(5, Attribution::Owned, Some(ExportRead::Empty))),
            t
        ),
        ComposerOutcome::Uncertain
    );
    assert!(s.input().liability_count().is_none());
}

/// Scenario: Map every decision and settlement to what the helper is told, the helper's reports to the record's, and check that a scan's observation carries no decision, claim or numbers and a marker's carries its decision and numbers but no claim.
#[spec("inbox/probe/069")]
#[test]
fn probe_069_the_adapters_and_the_reply_table() {
    let consumed = Ok(Recorded::Kept {
        settlement: Settlement::Consumed,
        rescan: false,
    });
    let zero = Ok(Recorded::Kept {
        settlement: Settlement::NotConsumed(NotConsumed::AlreadyZero),
        rescan: false,
    });
    let read = Some(ExportRead::NonEmpty);
    assert_eq!(
        outcome(Decision::Armed, consumed, read),
        ComposerOutcome::Mine {
            read: ExportRead::NonEmpty
        }
    );
    assert_eq!(
        outcome(Decision::Armed, zero, read),
        ComposerOutcome::Uncertain
    );
    assert_eq!(
        outcome(Decision::Armed, Ok(Recorded::Dropped), read),
        ComposerOutcome::Uncertain
    );
    assert_eq!(
        outcome(Decision::Armed, Err(Refusal::Stale), read),
        ComposerOutcome::Uncertain
    );
    for decision in [Decision::Unarmed, Decision::Tool] {
        assert_eq!(outcome(decision, consumed, read), ComposerOutcome::Unarmed);
    }
    assert_eq!(
        outcome(Decision::Unknown, consumed, read),
        ComposerOutcome::Uncertain
    );

    for (got, want) in [
        (
            EditorReport::EditorDone {
                tree: EditorTree::Clean,
            },
            Report::Done(Tree::Clean),
        ),
        (
            EditorReport::EditorDone {
                tree: EditorTree::Survivors,
            },
            Report::Done(Tree::Survivors),
        ),
        (
            EditorReport::EditorDone {
                tree: EditorTree::Unknown,
            },
            Report::Done(Tree::Unknown),
        ),
        (
            EditorReport::SpawnFailed { errno: Some(2) },
            Report::SpawnFailed { errno: Some(2) },
        ),
        (EditorReport::Unknown, Report::Unrecognised),
    ] {
        assert_eq!(report(got), want);
    }

    let t = Instant::now();
    let scan = scan_observation(G, key(1), Attribution::Owned, t);
    assert_eq!(
        (scan.decision, scan.claim, scan.seg_seen, scan.native_seen),
        (None, None, None, None)
    );
    let marker = Marker {
        key: key(2),
        decision: Decision::Unarmed,
        seen: Some((5, 6)),
        renamed: false,
    };
    let m = marker_observation(G, &marker, Attribution::Pending, t);
    assert_eq!(
        (m.decision, m.claim, m.seg_seen, m.native_seen),
        (Some(Decision::Unarmed), None, Some(5), Some(6))
    );
}

/// Scenario: Report a clean editor tree on an `Unarmed` helper's connection: the record keeps the report and its frontier reads missing at once, since no reader takes one yet; a second report is ignored, and the scope stays open for the editor-bearing decision under `Hold`.
#[spec("inbox/probe/069")]
#[test]
fn probe_069_a_clean_report_records_no_frontier() {
    let mut s = seam(Permission::Armable);
    let now = Instant::now();
    s.invocation(
        &request(ComposerDecision::Unarmed, Some(ComposerClaim::Owned)),
        Some(evidence(1, Attribution::Owned, None)),
        now,
    );
    s.report(
        key(1),
        EditorReport::EditorDone {
            tree: EditorTree::Clean,
        },
        now,
    );
    let r = s.input().record(key(1)).unwrap();
    assert_eq!(r.report, Report::Done(Tree::Clean));
    assert_eq!(
        r.frontier,
        dot_agent_deck::composer_probe::record::Frontier::Missing
    );
    s.report(
        key(1),
        EditorReport::EditorDone {
            tree: EditorTree::Survivors,
        },
        now,
    );
    assert_eq!(
        s.input().record(key(1)).unwrap().report,
        Report::Done(Tree::Clean)
    );
    assert!(matches!(
        s.input().writer_scope().unwrap().state(),
        ScopeState::Open(_)
    ));
}
