//! PRD #1666 part 3 (Q35-07; core §2.2–§2.4 and §2.6, amendments 1–6 of
//! Q35-core): the invocation record, its settlement, retention and caps
//! (`inbox/probe/008`), and the writer scope and completion truth table
//! (`inbox/probe/058`'s record half), pure over scripted observations and
//! times, and through a generation's input lock.

use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::epoch::{GenerationInput, OutputNote};
use dot_agent_deck::composer_probe::query_ledger::{
    CprWaitEnd, EntryView, HazardFifo, HazardPop, Pop, QueryKind, T_TRIGGER,
};
use dot_agent_deck::composer_probe::record::{
    Attribution, Cause, Channel, Claim, Completion, Decision, EC_POLICY, EPISODE_CAP, EcPolicy,
    Frontier, FrontierOutcome, HISTORY_CAP, Hold, NotConsumed, NotOwned, Observation, RECORD_CAP,
    Recorded, Records, Refusal, Report, Reported, Resolution, Route, ScopeState, Settlement,
    StopSource, Tree, birth_floor_ns, born_at_or_after, parse_marker,
};
use dot_agent_deck::composer_probe::record::{Marker, RecordKey};
use spec::spec;

const G: u64 = 7;
const HOLD: EcPolicy = EcPolicy::Hold;
const AUTH: EcPolicy = EcPolicy::Authority;
const OWNED: Attribution = Attribution::Owned;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn key(pid: u32) -> RecordKey {
    RecordKey {
        pid,
        start: 1_000 + u64::from(pid),
    }
}

struct T {
    r: Records,
    f: HazardFifo,
    t: Instant,
}

impl T {
    fn new(policy: EcPolicy) -> T {
        let t = Instant::now();
        T {
            r: Records::new(G, policy, t),
            f: HazardFifo::default(),
            t,
        }
    }

    fn at(&self, n: u64) -> Instant {
        self.t + ms(n)
    }

    fn obs(
        &mut self,
        k: RecordKey,
        channel: Channel,
        attribution: Attribution,
        decision: Option<Decision>,
        at: u64,
    ) -> Recorded {
        self.full(
            Observation {
                generation: G,
                key: k,
                channel,
                attribution,
                claim: None,
                decision,
                seg_seen: None,
                native_seen: None,
                birth_floor: self.at(10),
            },
            at,
        )
    }

    fn full(&mut self, o: Observation, at: u64) -> Recorded {
        let now = self.at(at);
        self.r.observe(&mut self.f, &o, now).expect("observe")
    }

    fn raise(&mut self, at: u64) -> u32 {
        let now = self.at(at);
        self.r.raise(G, now).unwrap()
    }

    fn exit(&mut self, k: RecordKey, at: u64) {
        let now = self.at(at);
        self.r.exit(&mut self.f, G, k, now, now).unwrap();
    }

    fn drain(&mut self, k: RecordKey, at: u64) -> (Settlement, bool) {
        let now = self.at(at);
        let d = self.r.drained(&mut self.f, G, k, now).unwrap();
        (d.settlement, d.rescan)
    }

    fn report(&mut self, k: RecordKey, report: Report, at: u64) -> Reported {
        let now = self.at(at);
        self.r.report(&mut self.f, G, k, report, now).unwrap()
    }

    fn scope(&self) -> ScopeState {
        self.r.scope().state()
    }

    fn settlement(&self, k: RecordKey) -> Settlement {
        self.r.get(k).unwrap().settlement
    }
}

/// An `Owned` scan sighting of `k`, with no decision, born at `floor`.
fn scan(t: &T, k: RecordKey, floor: u64) -> Observation {
    Observation {
        generation: G,
        key: k,
        channel: Channel::Scan,
        attribution: OWNED,
        claim: None,
        decision: None,
        seg_seen: None,
        native_seen: None,
        birth_floor: t.at(floor),
    }
}

fn kept(r: Recorded) -> (Settlement, bool) {
    match r {
        Recorded::Kept { settlement, rescan } => (settlement, rescan),
        Recorded::Dropped => panic!("dropped"),
    }
}

// ---- inbox/probe/008: the record and its settlement ----

/// Scenario: Publish one liability and settle one `Armed` helper per row of core §2.4's table, by IPC and by marker: each attribution, each claim, the count at 1 and at 0, and each other decision. Only an `Owned` helper whose claim is `Owned` or absent consumes, the count going down by one; every other row leaves the count where it was.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_a_decision_settles_by_the_table() {
    let not = Settlement::NotConsumed;
    // (attribution, claim, decision, count, settlement)
    type Row = (Attribution, Option<Claim>, Decision, u32, Settlement);
    let rows: &[Row] = &[
        (OWNED, None, Decision::Armed, 1, Settlement::Consumed),
        (
            OWNED,
            Some(Claim::Owned),
            Decision::Armed,
            1,
            Settlement::Consumed,
        ),
        (
            OWNED,
            None,
            Decision::Armed,
            0,
            not(NotConsumed::AlreadyZero),
        ),
        (
            OWNED,
            Some(Claim::Unknown),
            Decision::Armed,
            1,
            not(NotConsumed::Unattributed),
        ),
        (
            OWNED,
            Some(Claim::Tool),
            Decision::Armed,
            1,
            not(NotConsumed::Unattributed),
        ),
        (
            Attribution::NotOwned(NotOwned::Unknown),
            Some(Claim::Owned),
            Decision::Armed,
            1,
            not(NotConsumed::Unattributed),
        ),
        (
            Attribution::NotOwned(NotOwned::Tool),
            None,
            Decision::Armed,
            1,
            not(NotConsumed::Unattributed),
        ),
        (
            Attribution::NotOwned(NotOwned::Shape),
            None,
            Decision::Armed,
            1,
            not(NotConsumed::Unattributed),
        ),
        (
            Attribution::Unattributable,
            None,
            Decision::Armed,
            1,
            not(NotConsumed::Unattributed),
        ),
        (
            Attribution::Pending,
            None,
            Decision::Armed,
            1,
            not(NotConsumed::Unattributed),
        ),
        (OWNED, None, Decision::Unarmed, 1, not(NotConsumed::Unarmed)),
        (OWNED, None, Decision::Tool, 1, not(NotConsumed::Unarmed)),
        (
            OWNED,
            None,
            Decision::Unknown,
            1,
            not(NotConsumed::UnknownDecision),
        ),
    ];
    for &(attribution, claim, decision, count, want) in rows {
        for channel in [Channel::Ipc, Channel::Marker] {
            let mut t = T::new(HOLD);
            for i in 0..count {
                t.raise(u64::from(i));
            }
            let k = key(1);
            let o = Observation {
                generation: G,
                key: k,
                channel,
                attribution,
                claim,
                decision: Some(decision),
                seg_seen: Some(1),
                native_seen: None,
                birth_floor: t.at(10),
            };
            let (got, _) = kept(t.full(o, 20));
            let row = format!("{attribution:?} {claim:?} {decision:?} count {count} {channel:?}");
            assert_eq!(got, want, "{row}");
            let after = if want == Settlement::Consumed {
                count - 1
            } else {
                count
            };
            assert_eq!(t.r.count(), after, "{row}");
        }
    }
}

/// Scenario: With one liability published, an armed nested agent's helper (`NotOwned(Unknown)`) and then an armed orphaned tool's helper (`NotOwned(Tool)`) arrive before `A`'s own helper, each seen by IPC only, by marker only, or by its exit only. Neither consumes, the count stays 1 so `A`'s helper still reads the arm, and `A`'s own `Armed` helper then consumes it: one decrement in all.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_unrelated_helpers_leave_the_liability() {
    let unrelated = [
        Attribution::NotOwned(NotOwned::Unknown),
        Attribution::NotOwned(NotOwned::Tool),
    ];
    for route in ["ipc", "marker", "exit"] {
        let mut t = T::new(HOLD);
        t.raise(0);
        for (n, &attribution) in unrelated.iter().enumerate() {
            let k = key(10 + n as u32);
            let at = 20 + 10 * n as u64;
            match route {
                "ipc" => {
                    t.obs(k, Channel::Ipc, attribution, Some(Decision::Armed), at);
                }
                "marker" => {
                    t.obs(k, Channel::Marker, attribution, Some(Decision::Armed), at);
                }
                _ => {
                    t.obs(k, Channel::Scan, attribution, None, at);
                    t.exit(k, at + 1);
                    assert_eq!(
                        t.drain(k, at + 2).0,
                        Settlement::NotConsumed(NotConsumed::Unproven)
                    );
                }
            }
            assert_eq!(t.r.count(), 1, "{route}: {attribution:?} consumed");
            assert_eq!(t.r.get(k).unwrap().completion, Completion::NotRequired);
        }
        let a = key(1);
        let (s, _) = kept(t.obs(a, Channel::Ipc, OWNED, Some(Decision::Armed), 60));
        assert_eq!((s, t.r.count()), (Settlement::Consumed, 0), "{route}");
    }
}

/// Scenario: Settle a record, then send it everything again: the same decision on another channel, a conflicting decision, repeated scans, and an IPC request after its exit was drained. The settlement and the count don't change, and the first decision stays the record's.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_a_record_settles_once() {
    let mut t = T::new(HOLD);
    t.raise(0);
    t.raise(1);
    let k = key(1);
    assert_eq!(
        kept(t.obs(k, Channel::Ipc, OWNED, Some(Decision::Armed), 20)).0,
        Settlement::Consumed
    );
    for (channel, decision) in [
        (Channel::Marker, Some(Decision::Armed)),
        (Channel::Ipc, Some(Decision::Armed)),
        (Channel::Scan, None),
        (Channel::Scan, None),
        (Channel::Marker, Some(Decision::Unarmed)),
    ] {
        let (s, _) = kept(t.obs(k, channel, OWNED, decision, 30));
        assert_eq!((s, t.r.count()), (Settlement::Consumed, 1));
    }
    assert_eq!(t.r.get(k).unwrap().decision, Some(Decision::Armed));
    t.exit(k, 40);
    assert_eq!(t.drain(k, 41).0, Settlement::Consumed);
    let (s, _) = kept(t.obs(k, Channel::Ipc, OWNED, Some(Decision::Armed), 50));
    assert_eq!((s, t.r.count()), (Settlement::Consumed, 1), "a late IPC");

    // An exit settlement isn't redone by a late decision either.
    let mut t = T::new(HOLD);
    t.raise(0);
    let k = key(2);
    t.obs(k, Channel::Scan, OWNED, None, 20);
    t.exit(k, 30);
    assert_eq!(t.drain(k, 31).0, Settlement::Consumed);
    for d in [Decision::Armed, Decision::Unarmed] {
        let (s, _) = kept(t.obs(k, Channel::Ipc, OWNED, Some(d), 40));
        assert_eq!((s, t.r.count()), (Settlement::Consumed, 0));
    }
    assert_eq!(t.r.get(k).unwrap().decision, None);
}

/// Scenario: Seen by scan before its IPC, by IPC before its scan, and with a marker as well, at a count of 1 and of 2, and two `Owned` helpers at a count of 2. Each helper consumes exactly one liability whichever channel comes first, and two consume two.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_channel_orders_and_two_helpers() {
    for count in [1, 2] {
        for order in [
            [Channel::Scan, Channel::Ipc, Channel::Marker],
            [Channel::Ipc, Channel::Scan, Channel::Marker],
            [Channel::Marker, Channel::Ipc, Channel::Scan],
        ] {
            let mut t = T::new(HOLD);
            for i in 0..count {
                t.raise(i);
            }
            let k = key(1);
            for (n, channel) in order.into_iter().enumerate() {
                let decision = (channel != Channel::Scan).then_some(Decision::Armed);
                t.obs(k, channel, OWNED, decision, 20 + n as u64);
            }
            assert_eq!(t.settlement(k), Settlement::Consumed, "{order:?}");
            assert_eq!(t.r.count(), count as u32 - 1, "{order:?}");
        }
    }
    let mut t = T::new(HOLD);
    t.raise(0);
    t.raise(1);
    for pid in [1, 2] {
        let (s, _) = kept(t.obs(key(pid), Channel::Ipc, OWNED, Some(Decision::Armed), 20));
        assert_eq!(s, Settlement::Consumed);
    }
    assert_eq!(t.r.count(), 0);
}

/// Scenario: A pid reused with another start time is a second record: the first one's settlement doesn't answer the second's decision, and each consumes its own liability.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_a_reused_pid_is_another_record() {
    let mut t = T::new(HOLD);
    t.raise(0);
    t.raise(1);
    let first = RecordKey { pid: 9, start: 100 };
    let second = RecordKey { pid: 9, start: 200 };
    t.obs(first, Channel::Ipc, OWNED, Some(Decision::Armed), 20);
    t.obs(second, Channel::Scan, OWNED, None, 30);
    assert_eq!(t.settlement(second), Settlement::Open);
    t.obs(second, Channel::Ipc, OWNED, Some(Decision::Armed), 31);
    assert_eq!(t.settlement(second), Settlement::Consumed);
    assert_eq!((t.r.len(), t.r.count()), (2, 0));
}

/// Scenario: Settle a decisionless `Owned` helper at its exit drain over histories of the count: one at 1 throughout; one that went to 0 and back inside its lifetime; one at 0 at its birth floor; a zero stamped exactly at its birth floor (also with the raise after it stamped there too) and exactly at its exit; an exit before the floor; and the same helper claiming `Unknown`, or `Unattributable`. Only the first consumes.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_the_exit_settles_by_the_interval_proof() {
    // (raise/lower script as (at, +1|-1), birth floor, exit, attribution, claim, consumed)
    type Script = &'static [(u64, i8)];
    type Row = (
        &'static str,
        Script,
        u64,
        u64,
        Attribution,
        Option<Claim>,
        bool,
    );
    let rows: &[Row] = &[
        ("held", &[(5, 1)], 10, 50, OWNED, None, true),
        (
            "held, raised twice",
            &[(5, 1), (20, 1)],
            10,
            50,
            OWNED,
            None,
            true,
        ),
        (
            "a zero inside",
            &[(5, 1), (20, -1), (30, 1)],
            10,
            50,
            OWNED,
            None,
            false,
        ),
        ("zero at birth", &[(15, 1)], 10, 50, OWNED, None, false),
        (
            "a zero at the floor",
            &[(5, 1), (10, -1), (12, 1)],
            10,
            50,
            OWNED,
            None,
            false,
        ),
        (
            "a zero and a raise both at the floor",
            &[(5, 1), (10, -1), (10, 1)],
            10,
            50,
            OWNED,
            None,
            false,
        ),
        (
            "a zero at the exit",
            &[(5, 1), (50, -1), (52, 1)],
            10,
            50,
            OWNED,
            None,
            false,
        ),
        (
            "a zero just after the exit",
            &[(5, 1), (51, -1), (53, 1)],
            10,
            50,
            OWNED,
            None,
            true,
        ),
        ("exit before floor", &[(5, 1)], 40, 30, OWNED, None, false),
        (
            "claim Unknown",
            &[(5, 1)],
            10,
            50,
            OWNED,
            Some(Claim::Unknown),
            false,
        ),
        (
            "unattributable",
            &[(5, 1)],
            10,
            50,
            Attribution::Unattributable,
            None,
            false,
        ),
    ];
    for &(name, script, floor, exit, attribution, claim, consumed) in rows {
        let mut t = T::new(HOLD);
        let k = key(1);
        // The record exists before the history is written, so nothing it
        // could read is compacted away.
        t.full(
            Observation {
                generation: G,
                key: k,
                channel: Channel::Scan,
                attribution,
                claim,
                decision: None,
                seg_seen: None,
                native_seen: None,
                birth_floor: t.at(floor),
            },
            1,
        );
        for &(at, step) in script {
            if step > 0 {
                t.raise(at);
            } else {
                t.r.lower(G, t.at(at)).unwrap();
            }
        }
        let before = t.r.count();
        t.exit(k, exit);
        let (s, _) = t.drain(k, 60);
        if consumed {
            assert_eq!(
                (s, t.r.count()),
                (Settlement::Consumed, before - 1),
                "{name}"
            );
        } else {
            assert_eq!(
                (s, t.r.count()),
                (Settlement::NotConsumed(NotConsumed::Unproven), before),
                "{name}"
            );
        }
    }
}

/// Scenario: A decisionless `Owned` helper exits with the count held at 1 throughout its life, and before its drain another helper's `Armed` decision consumes that liability. The drain consumes nothing: the count is already 0.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_an_exit_proved_after_the_count_reached_zero_consumes_nothing() {
    let mut t = T::new(HOLD);
    t.raise(0);
    let (k, other) = (key(1), key(2));
    t.obs(k, Channel::Scan, OWNED, None, 15);
    t.exit(k, 20);
    assert!(t.r.history().proves(t.at(10), t.at(20)));
    t.obs(other, Channel::Ipc, OWNED, Some(Decision::Armed), 25);
    assert_eq!(t.r.count(), 0);
    assert_eq!(
        t.drain(k, 30).0,
        Settlement::NotConsumed(NotConsumed::Unproven)
    );
    assert_eq!(t.r.count(), 0);
}

/// Scenario: Raise the count with no record open, so the history is compacted, then observe one helper born before the compaction and one born after it. The earlier one's exit can't be proved; the later one's can.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_a_floor_before_the_kept_history_refuses() {
    let mut t = T::new(HOLD);
    t.raise(20);
    assert!(t.r.history().is_empty(), "compacted with no record open");
    assert!(t.r.is_empty());
    let base = Observation {
        generation: G,
        key: key(1),
        channel: Channel::Scan,
        attribution: OWNED,
        claim: None,
        decision: None,
        seg_seen: None,
        native_seen: None,
        birth_floor: t.at(25),
    };
    let (late, early) = (key(1), key(2));
    t.full(base, 30);
    t.full(
        Observation {
            key: early,
            birth_floor: t.at(19),
            ..base
        },
        30,
    );
    t.raise(31);
    assert!(!t.r.history().is_empty(), "kept for the open records");
    assert!(!t.r.is_empty());
    t.exit(early, 40);
    assert_eq!(
        t.drain(early, 41).0,
        Settlement::NotConsumed(NotConsumed::Unproven),
        "born before the kept history"
    );
    t.exit(late, 42);
    assert_eq!(t.drain(late, 43).0, Settlement::Consumed);
    assert_eq!(t.r.count(), 1);
}

/// Scenario: With no record open, raise, lower and raise the count all at one instant, so compaction folds the zero into a positive baseline, and only then observe an `Owned` helper with no decision born at that instant. Its drained exit consumes nothing and the count stays where it was; a helper born just after that instant consumes, and so does one born at a compacted instant whose zero came strictly before it.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_a_zero_compacted_at_the_floor_refuses() {
    // (script, floor, consumed)
    type Row = (&'static str, &'static [(u64, i8)], u64, bool);
    let rows: &[Row] = &[
        (
            "a zero and a raise at the floor",
            &[(10, 1), (10, -1), (10, 1)],
            10,
            false,
        ),
        (
            "born just after them",
            &[(10, 1), (10, -1), (10, 1)],
            11,
            true,
        ),
        (
            "a zero before the floor",
            &[(5, 1), (6, -1), (10, 1)],
            10,
            true,
        ),
        ("only raises at the floor", &[(10, 1), (10, 1)], 10, true),
    ];
    for &(name, script, floor, consumed) in rows {
        let mut t = T::new(HOLD);
        for &(at, step) in script {
            if step > 0 {
                t.raise(at);
            } else {
                t.r.lower(G, t.at(at)).unwrap();
            }
        }
        assert!(t.r.history().is_empty() && t.r.is_empty(), "{name}");
        assert_eq!(
            t.r.history().proves(t.at(floor), t.at(30)),
            consumed,
            "{name}"
        );
        let before = t.r.count();
        let k = key(1);
        let o = scan(&t, k, floor);
        t.full(o, 20);
        t.exit(k, 30);
        let want = if consumed {
            (Settlement::Consumed, before - 1)
        } else {
            (Settlement::NotConsumed(NotConsumed::Unproven), before)
        };
        assert_eq!((t.drain(k, 31).0, t.r.count()), want, "{name}");
    }
}

/// Scenario: With one record open from before, write a zero and a raise at one instant and then enough raises and lowers to push the history one past its cap, so the zero is evicted and the raise beside it is kept; only then observe an `Owned` helper with no decision born at that instant. Its drained exit consumes nothing and the count stays; one born just after it consumes.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_a_zero_evicted_at_the_floor_refuses() {
    let mut t = T::new(HOLD);
    t.raise(1);
    let keeper = scan(&t, key(1), 2);
    t.full(keeper, 3);
    t.r.lower(G, t.at(10)).unwrap();
    t.raise(10);
    // Never 0 again: 2, 1, 2, 1, ...
    for i in 0..HISTORY_CAP as u64 - 2 {
        if i % 2 == 0 {
            t.raise(20 + i);
        } else {
            t.r.lower(G, t.at(20 + i)).unwrap();
        }
    }
    assert_eq!(t.r.history().len(), HISTORY_CAP);
    t.raise(20 + HISTORY_CAP as u64);
    assert_eq!(t.r.history().len(), HISTORY_CAP, "the zero was evicted");
    let end = 30 + HISTORY_CAP as u64;
    assert!(!t.r.history().proves(t.at(10), t.at(end)));
    assert!(t.r.history().proves(t.at(11), t.at(end)));
    let (at, after) = (key(2), key(3));
    let o = scan(&t, at, 10);
    t.full(o, end);
    let o = scan(&t, after, 11);
    t.full(o, end);
    let before = t.r.count();
    t.exit(at, end + 1);
    assert_eq!(
        (t.drain(at, end + 2).0, t.r.count()),
        (Settlement::NotConsumed(NotConsumed::Unproven), before)
    );
    t.exit(after, end + 3);
    assert_eq!(
        (t.drain(after, end + 4).0, t.r.count()),
        (Settlement::Consumed, before - 1)
    );
}

/// Scenario: Fill the table with 64 records and observe a 65th helper; then, with one record open, push the count history to 256 entries and one more. The 65th makes no record and consumes nothing, at either cap the records refuse admission, and once the cap drops the oldest entry, a proof that needed it fails.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_the_record_and_history_caps() {
    let mut t = T::new(HOLD);
    t.raise(0);
    for pid in 0..RECORD_CAP as u32 {
        t.obs(
            key(100 + pid),
            Channel::Ipc,
            OWNED,
            Some(Decision::Unarmed),
            5,
        );
    }
    assert_eq!(t.r.len(), RECORD_CAP);
    let r = t.obs(key(1), Channel::Ipc, OWNED, Some(Decision::Armed), 20);
    assert_eq!(
        (r, t.r.count(), t.r.len()),
        (Recorded::Dropped, 1, RECORD_CAP)
    );
    assert_eq!(t.r.admits(), Err(Hold::RecordCap));

    // The record cap on its own: helpers that settle and need nothing.
    let mut t = T::new(HOLD);
    for pid in 0..RECORD_CAP as u32 {
        t.obs(
            key(100 + pid),
            Channel::Ipc,
            Attribution::NotOwned(NotOwned::Tool),
            Some(Decision::Armed),
            5,
        );
    }
    assert_eq!(t.r.admits(), Err(Hold::RecordCap));

    // The history cap: an open record keeps every entry since its floor.
    let mut t = T::new(HOLD);
    t.raise(5);
    let k = key(1);
    t.obs(k, Channel::Scan, OWNED, None, 6);
    let floor = t.at(10);
    assert!(t.r.history().proves(floor, floor));
    for i in 0..HISTORY_CAP as u64 {
        if i % 2 == 0 {
            t.r.lower(G, t.at(11 + i)).unwrap();
        } else {
            t.r.raise(G, t.at(11 + i)).unwrap();
        }
    }
    assert_eq!(t.r.history().len(), HISTORY_CAP);
    assert_eq!(t.r.admits(), Err(Hold::HistoryCap));
    assert!(t.r.history().proves(floor, floor), "kept from the floor");
    t.r.raise(G, t.at(11 + HISTORY_CAP as u64)).unwrap();
    assert_eq!(
        t.r.history().len(),
        HISTORY_CAP,
        "at the cap, the oldest goes"
    );
    assert!(
        !t.r.history().proves(floor, floor),
        "and what it held proves nothing"
    );
}

/// Scenario: Take a record through settlement, exit, drain and completion, trying to delete it at each step, once with its marker deletion failing. It is deleted only when all four hold and its markers are gone, and the facts handed out at its drain are the ones its deletion returns.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_retention() {
    let mut t = T::new(HOLD);
    t.raise(0);
    let k = key(1);
    let prune = |t: &mut T, ok: bool| {
        let now = t.at(100);
        t.r.prune(&mut t.f, G, k, ok, now).unwrap()
    };
    t.obs(k, Channel::Scan, OWNED, None, 20);
    assert_eq!(prune(&mut t, true), None, "open");
    t.obs(k, Channel::Ipc, OWNED, Some(Decision::Armed), 21);
    t.r.match_segment(G, k, 3).unwrap();
    assert_eq!(prune(&mut t, true), None, "not exited");
    t.exit(k, 30);
    assert_eq!(prune(&mut t, true), None, "not drained");
    let now = t.at(31);
    let fact = t.r.drained(&mut t.f, G, k, now).unwrap().fact;
    assert_eq!(fact.exit_observed, t.at(30));
    assert_eq!(fact.first_armed_seen, Some(t.at(21)));
    assert_eq!(
        (fact.matched, fact.decision),
        (Some(3), Some(Decision::Armed))
    );
    assert_eq!(prune(&mut t, true), None, "completion pending");
    let now = t.at(40);
    t.r.complete(&mut t.f, G, k, Route::HelperOnly, now, now)
        .unwrap();
    assert_eq!(
        prune(&mut t, false),
        None,
        "a failed marker deletion keeps it"
    );
    assert_eq!(t.r.len(), 1);
    assert_eq!(prune(&mut t, true), Some(fact));
    assert!(t.r.get(k).is_none());

    // Settled and needing no completion, but not drained: kept.
    let n = key(2);
    t.obs(
        n,
        Channel::Ipc,
        Attribution::NotOwned(NotOwned::Tool),
        Some(Decision::Armed),
        50,
    );
    t.exit(n, 51);
    let now = t.at(100);
    assert_eq!(t.r.prune(&mut t.f, G, n, true, now), Ok(None));
    t.drain(n, 52);
    assert!(t.r.prune(&mut t.f, G, n, true, now).unwrap().is_some());
}

/// Scenario: Under `Authority`, an `Unarmed` helper reports its editor clean and its exit is drained; its frontier is taken and its segment matched only afterwards, then it completes and is deleted. The facts read after the drain carry the later frontier and match, and its deletion hands over the same; before its drain there are none.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_the_exit_facts_follow_the_record_until_it_goes() {
    let mut t = T::new(AUTH);
    let k = key(1);
    t.obs(k, Channel::Ipc, OWNED, Some(Decision::Unarmed), 10);
    t.report(k, Report::Done(Tree::Clean), 11);
    t.exit(k, 12);
    assert_eq!(t.r.fact(k), None, "not drained");
    let now = t.at(13);
    let at_drain = t.r.drained(&mut t.f, G, k, now).unwrap().fact;
    assert_eq!(t.r.fact(k), Some(at_drain));
    assert_eq!(
        (at_drain.frontier, at_drain.matched),
        (Frontier::Requested, None)
    );
    t.r.frontier(G, k, FrontierOutcome::Taken { offset: 9 })
        .unwrap();
    t.r.match_segment(G, k, 4).unwrap();
    let later = t.r.fact(k).unwrap();
    assert_eq!(
        (later.frontier, later.matched),
        (Frontier::Taken { offset: 9 }, Some(4))
    );
    let now = t.at(14);
    t.r.complete(&mut t.f, G, k, Route::EditorBearing, now, now)
        .unwrap();
    assert_eq!(t.r.prune(&mut t.f, G, k, true, now), Ok(Some(later)));
    assert_eq!(t.r.fact(k), None, "gone");
}

/// Scenario: Observe `Armed` on an IPC request, again on its marker and again on a second request. The first-`Armed` time is the first observation's and none of the later ones moves it; a helper that never declared `Armed` has none.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_first_armed_seen_is_set_once() {
    let mut t = T::new(HOLD);
    let k = key(1);
    t.obs(k, Channel::Scan, OWNED, None, 10);
    assert_eq!(t.r.get(k).unwrap().first_armed_seen, None);
    t.obs(k, Channel::Ipc, OWNED, Some(Decision::Armed), 20);
    t.obs(k, Channel::Marker, OWNED, Some(Decision::Armed), 30);
    t.obs(k, Channel::Ipc, OWNED, Some(Decision::Armed), 40);
    assert_eq!(t.r.get(k).unwrap().first_armed_seen, Some(t.at(20)));
    let u = key(2);
    t.obs(u, Channel::Ipc, OWNED, Some(Decision::Unarmed), 20);
    assert_eq!(t.r.get(u).unwrap().first_armed_seen, None);
}

/// Scenario: Convert start ticks to a birth floor at 100 ticks a second and at a rate that doesn't divide a second, then ask the birth frontier whether a helper born in the same tick as a segment's write, after it, follows it. The floor is the tick's start, so the same tick refuses and only a later tick matches; a rate of 0 or an overflow gives no floor.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_the_birth_floor_refuses_within_a_tick() {
    assert_eq!(birth_floor_ns(0, 100), Some(0));
    assert_eq!(birth_floor_ns(123, 100), Some(1_230_000_000));
    assert_eq!(birth_floor_ns(1, 3), Some(333_333_333), "floored");
    assert_eq!(birth_floor_ns(1, 0), None);
    assert_eq!(birth_floor_ns(u64::MAX, 1), None);
    // A segment written 4 ms into tick 123; a helper born 6 ms into it.
    let written = 1_230_000_000 + 4_000_000;
    let floor = birth_floor_ns(123, 100).unwrap();
    assert!(!born_at_or_after(floor, written), "same tick refuses");
    assert!(born_at_or_after(birth_floor_ns(124, 100).unwrap(), written));
    assert!(born_at_or_after(written, written), "an equal floor follows");
}

/// Scenario: Parse the marker names the helper writes, with and without the numbers it read and with the daemon's `.seen` suffix, and a list of malformed names: other words, signs, spaces, empty or overlong fields, a pid past `u32`, extra or missing parts, a misplaced suffix. Only the helper's forms parse, completely.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_marker_names() {
    let k = RecordKey {
        pid: 41,
        start: 5_000,
    };
    assert_eq!(
        parse_marker("41-5000-armed-3-9"),
        Some(Marker {
            key: k,
            decision: Decision::Armed,
            seen: Some((3, 9)),
            renamed: false
        })
    );
    assert_eq!(
        parse_marker("41-5000-unarmed.seen"),
        Some(Marker {
            key: k,
            decision: Decision::Unarmed,
            seen: None,
            renamed: true
        })
    );
    assert_eq!(
        parse_marker("4294967295-18446744073709551615-tool-18446744073709551615-0.seen")
            .map(|m| (m.key.pid, m.decision)),
        Some((u32::MAX, Decision::Tool))
    );
    #[cfg(target_os = "linux")]
    {
        use dot_agent_deck::composer_probe::helper::{Seen, marker_name};
        use dot_agent_deck::event::ComposerDecision;
        for (d, want) in [
            (ComposerDecision::Armed, Decision::Armed),
            (ComposerDecision::Unarmed, Decision::Unarmed),
            (ComposerDecision::Tool, Decision::Tool),
        ] {
            let name = marker_name(41, 5_000, d, Some(Seen { seg: 3, native: 9 }));
            assert_eq!(
                parse_marker(&name).map(|m| (m.decision, m.seen)),
                Some((want, Some((3, 9))))
            );
            let name = marker_name(41, 5_000, d, None);
            assert_eq!(parse_marker(&name).map(|m| m.key), Some(k));
        }
    }
    for bad in [
        "",
        "41-5000",
        "41-5000-done-1-2",
        "41-5000-Armed",
        "41-5000-armed-1",
        "41-5000-armed-1-2-3",
        "41-5000-armed-1-2.seen.seen",
        "41-5000-armed.seen-1-2",
        "+41-5000-armed",
        "41-+5000-armed",
        "41-5000-armed- 1-2",
        "41--5000-armed",
        "4294967296-5000-armed",
        "12345678901-5000-armed",
        "41-123456789012345678901-armed",
        "41-18446744073709551616-armed",
        "41-5000-armed-1-18446744073709551616",
        "41-5000-armed.tmp",
        "../41-5000-armed",
        "41-5000-armed-1-2\n",
    ] {
        assert_eq!(parse_marker(bad), None, "{bad:?}");
    }
}

/// Scenario: Address a step to the table under another generation's number, then retire the table and replay every step. Each is refused as stale and changes nothing; a retired table refuses even its own generation.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_generation_isolation() {
    let mut t = T::new(HOLD);
    t.raise(0);
    let k = key(1);
    let wrong = Observation {
        generation: G + 1,
        key: k,
        channel: Channel::Ipc,
        attribution: OWNED,
        claim: None,
        decision: Some(Decision::Armed),
        seg_seen: None,
        native_seen: None,
        birth_floor: t.at(5),
    };
    let now = t.at(10);
    assert_eq!(t.r.observe(&mut t.f, &wrong, now), Err(Refusal::Stale));
    assert_eq!(t.r.raise(G - 1, now), Err(Refusal::Stale));
    assert_eq!((t.r.len(), t.r.count()), (0, 1));
    t.obs(k, Channel::Scan, OWNED, None, 11);
    t.r.retire(t.at(12));
    let ok = Observation {
        generation: G,
        ..wrong
    };
    assert_eq!(t.r.observe(&mut t.f, &ok, now), Err(Refusal::Stale));
    assert_eq!(t.r.exit(&mut t.f, G, k, now, now), Err(Refusal::Stale));
    assert_eq!(
        t.r.drained(&mut t.f, G, k, now).map(|_| ()),
        Err(Refusal::Stale)
    );
    assert_eq!(t.r.raise(G, now), Err(Refusal::Stale));
    assert_eq!(t.r.admits(), Err(Hold::Retired));
    assert_eq!((t.r.len(), t.r.count()), (0, 0));
}

/// Scenario: An `Owned` helper reports the `native_seen` it read on its IPC request, again on its marker and once more on a repeat request, and is then drained; a second helper is seen only by scan and pops at its drain by its birth floor; an `Unattributable` one with `native_seen` pops nothing. Each record pops at most once, whatever channel comes next.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_one_pop_per_record() {
    let mut t = T::new(HOLD);
    let a = t.f.accept(t.at(1), false).unwrap();
    let b = t.f.accept(t.at(2), false).unwrap();
    let c = t.f.accept(t.at(3), false).unwrap();
    let seen = |t: &T, k, ch, attribution, n| Observation {
        generation: G,
        key: k,
        channel: ch,
        attribution,
        claim: None,
        decision: Some(Decision::Unarmed),
        seg_seen: None,
        native_seen: Some(n),
        birth_floor: t.at(5),
    };
    let k = key(1);
    for ch in [Channel::Ipc, Channel::Marker, Channel::Ipc] {
        let o = seen(&t, k, ch, OWNED, c);
        t.full(o, 6);
    }
    assert_eq!(
        t.r.get(k).unwrap().hazard_pop,
        HazardPop::Spent(Pop::Entry(a))
    );
    t.exit(k, 7);
    t.drain(k, 8);
    assert_eq!(
        t.r.get(k).unwrap().hazard_pop,
        HazardPop::Spent(Pop::Entry(a))
    );
    let u = key(2);
    let o = seen(&t, u, Channel::Ipc, Attribution::Unattributable, c);
    t.full(o, 9);
    t.exit(u, 10);
    t.drain(u, 11);
    assert_eq!(t.r.get(u).unwrap().hazard_pop, HazardPop::Unspent);
    let s = key(3);
    t.obs(s, Channel::Scan, OWNED, None, 12);
    assert_eq!(
        t.r.get(s).unwrap().hazard_pop,
        HazardPop::Unspent,
        "no pop before the drain"
    );
    t.exit(s, 13);
    t.drain(s, 14);
    assert_eq!(
        t.r.get(s).unwrap().hazard_pop,
        HazardPop::Spent(Pop::Entry(b))
    );
    let left: Vec<_> =
        t.f.entries(t.at(15))
            .iter()
            .map(|e| match e {
                EntryView::Latched { seq } | EntryView::Quiet { seq, .. } => *seq,
            })
            .collect();
    assert_eq!(left, [c]);
}

/// Scenario: Through a generation's input lock, a quiet native Ctrl+G's period is checked past its end, and only afterwards a record transition stamped before that end pauses the clock (a pause delivered late); separately, a CPR stamped before the end is noted after the check (a latch delivered late). Neither restores the entry the check already let go. In the other order, the transition's pause, or the latch, keeps it: what decides is the order in which the lock saw them.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_the_quiet_clock_follows_the_lock_order() {
    let t0 = Instant::now();
    let input = || {
        let i = GenerationInput::new(CprWaitEnd::Counted);
        assert!(i.attach_records(Records::new(G, HOLD, t0)));
        i
    };
    let before_end = t0 + T_TRIGGER - ms(1);
    let past_end = t0 + T_TRIGGER + ms(1);
    let scan = Observation {
        generation: G,
        key: key(1),
        channel: Channel::Scan,
        attribution: OWNED,
        claim: None,
        decision: None,
        seg_seen: None,
        native_seen: None,
        birth_floor: t0,
    };
    // A pause delivered after the expiry candidate.
    let i = input();
    i.accept_native(t0).unwrap();
    assert!(i.hazards_clear(past_end), "the check lets it go");
    i.record_observe(&scan, before_end).unwrap();
    assert_eq!(i.hazards(past_end + ms(1)).0, []);
    // The pause first.
    let i = input();
    i.accept_native(t0).unwrap();
    i.record_observe(&scan, before_end).unwrap();
    assert!(!i.hazards_clear(past_end));
    assert!(matches!(
        i.hazards(past_end + T_TRIGGER).0[..],
        [EntryView::Quiet { paused: true, .. }]
    ));
    // A latch delivered after the expiry candidate.
    let i = input();
    i.accept_native(t0).unwrap();
    assert!(i.hazards_clear(past_end));
    i.on_output(&[OutputNote::Query(QueryKind::Cpr)], before_end);
    assert_eq!(i.hazards(past_end).0, []);
    // The latch first.
    let i = input();
    i.accept_native(t0).unwrap();
    i.on_output(&[OutputNote::Query(QueryKind::Cpr)], before_end);
    assert!(!i.hazards_clear(past_end));
    assert!(matches!(
        i.hazards(past_end).0[..],
        [EntryView::Latched { .. }]
    ));
}

/// Scenario: Through a generation's input lock, a record seen undecided pauses the quiet clock in the same step, and the step that completes it resumes the clock: a quiet entry is kept for exactly the pause's length longer.
#[spec("inbox/probe/008")]
#[test]
fn probe_008_a_record_pauses_and_resumes_in_its_own_step() {
    let t0 = Instant::now();
    let i = GenerationInput::new(CprWaitEnd::Counted);
    assert!(i.attach_records(Records::new(G, HOLD, t0)));
    i.accept_native(t0).unwrap();
    let k = key(1);
    let base = Observation {
        generation: G,
        key: k,
        channel: Channel::Ipc,
        attribution: OWNED,
        claim: None,
        decision: Some(Decision::Armed),
        seg_seen: Some(1),
        native_seen: None,
        birth_floor: t0,
    };
    let p0 = t0 + ms(1_000);
    i.record_observe(&base, p0).unwrap();
    assert!(matches!(
        i.hazards(p0).0[..],
        [EntryView::Quiet { paused: true, .. }]
    ));
    i.record_match(G, k, 1).unwrap();
    let p1 = p0 + ms(30_000);
    i.record_exit(G, k, p1, p1).unwrap();
    i.record_complete(G, k, Route::HelperOnly, p1, p1).unwrap();
    assert!(matches!(
        i.hazards(p1).0[..],
        [EntryView::Quiet { paused: false, .. }]
    ));
    // 1 s ran before the pause; the rest of the period runs after it.
    let end = p1 + T_TRIGGER - ms(1_000);
    assert!(!i.hazards_clear(end));
    assert!(i.hazards_clear(end + ms(1)));
}

// ---- inbox/probe/058: the writer scope and completion, record half ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    Recorded,
    /// Dropped at the record cap.
    Capped,
    /// Dropped by the identity refusal.
    Refused,
}

/// The oracle: the cause a single helper's life sets, written from
/// amendments 3–6 rather than from the code.
fn oracle(
    policy: EcPolicy,
    via: Via,
    channel: Channel,
    decision: Option<Decision>,
    report: Report,
) -> (Option<Cause>, Option<Cause>) {
    use Cause::*;
    let editor = |d: Decision| d != Decision::Armed;
    match via {
        Via::Capped => {
            let c = match decision {
                Some(Decision::Armed) => None,
                Some(Decision::Unarmed | Decision::Tool) => Some(EditorReturnUnchecked),
                Some(Decision::Unknown) | None => Some(EditorUnconfirmed),
            };
            (c, c)
        }
        Via::Refused => {
            let c = match (channel, decision) {
                (Channel::Scan, _) | (_, None | Some(Decision::Armed)) => None,
                (_, Some(Decision::Unarmed | Decision::Tool)) => Some(EditorReturnUnchecked),
                (_, Some(Decision::Unknown)) => Some(EditorUnconfirmed),
            };
            (c, c)
        }
        Via::Recorded => {
            let at_decision = match (policy, decision) {
                (EcPolicy::Hold, Some(Decision::Unarmed | Decision::Tool)) => {
                    Some(EditorReturnUnchecked)
                }
                (EcPolicy::Hold, Some(Decision::Unknown)) => Some(EditorUnconfirmed),
                _ => None,
            };
            let by_report = match report {
                Report::Done(Tree::Survivors) => Some(EditorLeftWriter),
                Report::Done(Tree::Unknown) => Some(EditorEndUnknown),
                _ => None,
            };
            let at_drain = match decision {
                None => Some(EditorUnconfirmed),
                Some(d) if editor(d) => match report {
                    Report::Done(Tree::Clean) | Report::SpawnFailed { .. } => None,
                    Report::Done(Tree::Survivors) => Some(EditorLeftWriter),
                    _ => Some(EditorEndUnknown),
                },
                Some(_) => None,
            };
            let before = at_decision.max(by_report);
            (before, before.max(at_drain))
        }
    }
}

const REPORTS: [Report; 6] = [
    Report::None,
    Report::Done(Tree::Clean),
    Report::Done(Tree::Survivors),
    Report::Done(Tree::Unknown),
    Report::SpawnFailed { errno: Some(2) },
    Report::Unrecognised,
];

const DECISIONS: [Option<Decision>; 5] = [
    None,
    Some(Decision::Armed),
    Some(Decision::Unarmed),
    Some(Decision::Tool),
    Some(Decision::Unknown),
];

const ATTRIBUTIONS: [Attribution; 6] = [
    Attribution::Pending,
    Attribution::Owned,
    Attribution::NotOwned(NotOwned::Unknown),
    Attribution::NotOwned(NotOwned::Tool),
    Attribution::NotOwned(NotOwned::Shape),
    Attribution::Unattributable,
];

fn state(cause: Option<Cause>) -> ScopeState {
    cause.map_or(ScopeState::Closed, ScopeState::Open)
}

/// Scenario: For every policy, attribution, channel, decision and report, a helper is recorded, or dropped at the record cap, or dropped by the identity refusal, then reports and its exit is drained. The writer scope after the decision and after the drain is the oracle's, written from amendments 3–6, for every attribution alike: under `Hold` any decision but `Armed` opens it, under both policies an ending not shown clean does, a decisionless exit opens it as unconfirmed, and a dropped observation opens it unless it proves the helper ran no editor.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_the_scope_truth_table() {
    let mut rows = 0;
    for policy in [HOLD, AUTH] {
        for attribution in ATTRIBUTIONS {
            for channel in [Channel::Ipc, Channel::Marker, Channel::Scan] {
                for decision in DECISIONS {
                    if channel == Channel::Scan && decision.is_some() {
                        continue;
                    }
                    for report in REPORTS {
                        for via in [Via::Recorded, Via::Capped, Via::Refused] {
                            let row = format!(
                                "{policy:?} {attribution:?} {channel:?} {decision:?} {report:?} {via:?}"
                            );
                            let (want_decided, want_drained) =
                                oracle(policy, via, channel, decision, report);
                            let mut t = T::new(policy);
                            if via == Via::Capped {
                                for pid in 0..RECORD_CAP as u32 {
                                    t.obs(
                                        key(100 + pid),
                                        Channel::Ipc,
                                        Attribution::NotOwned(NotOwned::Tool),
                                        Some(Decision::Armed),
                                        1,
                                    );
                                }
                            }
                            let k = key(1);
                            match via {
                                Via::Refused => {
                                    t.r.refused_identity(G, channel, decision, None, t.at(20))
                                        .unwrap();
                                    assert_eq!(t.scope(), state(want_decided), "{row}");
                                    continue;
                                }
                                Via::Capped => {
                                    let r = t.obs(k, channel, attribution, decision, 20);
                                    assert_eq!(r, Recorded::Dropped, "{row}");
                                    assert_eq!(t.scope(), state(want_decided), "{row}");
                                    continue;
                                }
                                Via::Recorded => {}
                            }
                            t.obs(k, channel, attribution, decision, 20);
                            if report != Report::None {
                                t.report(k, report, 25);
                            }
                            let decided = match (decision, want_decided) {
                                (None, None) => ScopeState::Pending,
                                (_, c) => state(c),
                            };
                            assert_eq!(t.scope(), decided, "{row}: after the decision");
                            assert!(t.r.admits().is_err() || decided == ScopeState::Closed);
                            t.exit(k, 30);
                            t.drain(k, 31);
                            assert_eq!(t.scope(), state(want_drained), "{row}: after the drain");
                            assert_eq!(t.r.scope().undecided(), 0, "{row}");
                            rows += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(rows, 2 * 6 * (2 * 5 + 1) * 6, "every recorded row ran");
    assert_eq!(EC_POLICY, HOLD, "production holds");
}

/// Scenario: Two helpers are seen by scan with no decision; one declares `Armed`, and the other's exit is then drained with no decision. The scope stays pending after the first decision, never reads closed, and ends open as unconfirmed with no re-evaluation; in the variant where the second declares `Armed` too, the scope closes at the second decision and asks for re-evaluation exactly once.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_two_undecided_records_only_one_deciding() {
    for second_decides in [false, true] {
        let mut t = T::new(HOLD);
        let (a, b) = (key(1), key(2));
        t.obs(a, Channel::Scan, OWNED, None, 10);
        t.obs(b, Channel::Scan, OWNED, None, 11);
        assert_eq!(
            (t.scope(), t.r.scope().undecided()),
            (ScopeState::Pending, 2)
        );
        let (_, rescan) = kept(t.obs(a, Channel::Ipc, OWNED, Some(Decision::Armed), 20));
        assert!(!rescan);
        assert_eq!(
            (t.scope(), t.r.scope().undecided()),
            (ScopeState::Pending, 1)
        );
        let mut rescans = 0;
        if second_decides {
            let (_, rescan) = kept(t.obs(b, Channel::Marker, OWNED, Some(Decision::Armed), 30));
            rescans += u32::from(rescan);
            assert_eq!(t.scope(), ScopeState::Closed);
            let (_, rescan) = kept(t.obs(b, Channel::Ipc, OWNED, Some(Decision::Armed), 31));
            rescans += u32::from(rescan);
        }
        t.exit(b, 40);
        rescans += u32::from(t.drain(b, 41).1);
        t.exit(a, 42);
        rescans += u32::from(t.drain(a, 43).1);
        if second_decides {
            assert_eq!((t.scope(), rescans), (ScopeState::Closed, 1));
        } else {
            assert_eq!(
                (t.scope(), rescans),
                (ScopeState::Open(Cause::EditorUnconfirmed), 0)
            );
        }
    }
}

/// Scenario: The same helper's evidence arrives duplicated and in each order: scan, IPC and marker, the marker twice, the IPC before the scan, and a marker naming `Unarmed` after an IPC naming `Armed`. `undecided` goes down once, the record settles once on its first decision, and every editor-bearing decision observed still opens the scope under `Hold`.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_duplicate_and_reordered_evidence() {
    let armed = Some(Decision::Armed);
    let orders: [&[(Channel, Option<Decision>)]; 4] = [
        &[
            (Channel::Scan, None),
            (Channel::Ipc, armed),
            (Channel::Marker, armed),
        ],
        &[
            (Channel::Marker, armed),
            (Channel::Marker, armed),
            (Channel::Scan, None),
        ],
        &[
            (Channel::Ipc, armed),
            (Channel::Scan, None),
            (Channel::Scan, None),
        ],
        &[
            (Channel::Scan, None),
            (Channel::Marker, armed),
            (Channel::Ipc, armed),
        ],
    ];
    for order in orders {
        let mut t = T::new(HOLD);
        t.raise(0);
        let k = key(1);
        let mut rescans = 0;
        for (n, &(ch, d)) in order.iter().enumerate() {
            rescans += u32::from(kept(t.obs(k, ch, OWNED, d, 20 + n as u64)).1);
        }
        let opened_pending = order[0].1.is_none();
        assert_eq!(rescans, u32::from(opened_pending), "{order:?}");
        assert_eq!(
            (t.scope(), t.settlement(k), t.r.count()),
            (ScopeState::Closed, Settlement::Consumed, 0),
            "{order:?}"
        );
    }
    let mut t = T::new(HOLD);
    t.raise(0);
    let k = key(1);
    t.obs(k, Channel::Ipc, OWNED, armed, 20);
    t.obs(k, Channel::Marker, OWNED, Some(Decision::Unarmed), 21);
    assert_eq!(t.settlement(k), Settlement::Consumed);
    assert_eq!(t.scope(), ScopeState::Open(Cause::EditorReturnUnchecked));
    assert_eq!(t.r.scope().undecided(), 0);
}

/// Scenario: The only undecided helper declares `Unarmed` under `Hold`. In that one step the scope's cause is set and `undecided` reaches zero, so the scope goes from pending straight to open: it is never closed in between, no re-evaluation is asked for, admission is refused, and the episode ends resolved by `Unarmed`.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_the_last_undecided_unarmed_opens_with_the_decrement() {
    let t0 = Instant::now();
    let i = GenerationInput::new(CprWaitEnd::Counted);
    assert!(i.attach_records(Records::new(G, HOLD, t0)));
    let k = key(1);
    let mut o = Observation {
        generation: G,
        key: k,
        channel: Channel::Scan,
        attribution: OWNED,
        claim: None,
        decision: None,
        seg_seen: None,
        native_seen: None,
        birth_floor: t0,
    };
    i.record_observe(&o, t0 + ms(10)).unwrap();
    assert_eq!(i.writer_scope().unwrap().state(), ScopeState::Pending);
    o.channel = Channel::Ipc;
    o.decision = Some(Decision::Unarmed);
    let step = i.record_observe(&o, t0 + ms(20)).unwrap();
    assert_eq!(
        step,
        Recorded::Kept {
            settlement: Settlement::NotConsumed(NotConsumed::Unarmed),
            rescan: false
        }
    );
    let scope = i.writer_scope().unwrap();
    assert_eq!(
        (scope.state(), scope.undecided()),
        (ScopeState::Open(Cause::EditorReturnUnchecked), 0)
    );
    assert!(i.records_admit().is_err());
    let stop = i.first_stop(t0 + ms(30)).unwrap();
    assert_eq!(
        stop.stopped.map(|s| (s.cause, s.after)),
        Some((Cause::EditorReturnUnchecked, ms(20)))
    );
    assert_eq!(stop.episodes.len(), 1);
    assert_eq!(stop.episodes[0].duration, ms(10));
    assert_eq!(
        stop.episodes[0].resolved,
        Resolution::Decided(Decision::Unarmed)
    );
}

/// Scenario: A helper's decision and its exit drain race. Decision first: `Armed`, then the drain, and the scope closes once. Drain first: the decisionless drain opens the scope as unconfirmed, and a late `Armed` or `Unarmed` changes neither the record nor the scope, and doesn't count down `undecided` again.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_the_decision_against_the_exit() {
    let mut t = T::new(HOLD);
    let k = key(1);
    t.obs(k, Channel::Scan, OWNED, None, 10);
    assert!(kept(t.obs(k, Channel::Ipc, OWNED, Some(Decision::Armed), 20)).1);
    t.exit(k, 21);
    assert_eq!(
        t.drain(k, 22),
        (Settlement::NotConsumed(NotConsumed::AlreadyZero), false)
    );
    assert_eq!(t.scope(), ScopeState::Closed);

    for late in [Decision::Armed, Decision::Unarmed] {
        let mut t = T::new(HOLD);
        t.raise(0);
        let k = key(1);
        t.obs(
            k,
            Channel::Scan,
            Attribution::NotOwned(NotOwned::Unknown),
            None,
            10,
        );
        t.exit(k, 20);
        assert_eq!(
            t.drain(k, 21),
            (Settlement::NotConsumed(NotConsumed::Unproven), false)
        );
        assert_eq!(t.scope(), ScopeState::Open(Cause::EditorUnconfirmed));
        let (s, rescan) = kept(t.obs(k, Channel::Ipc, OWNED, Some(late), 30));
        assert_eq!(
            (s, rescan),
            (Settlement::NotConsumed(NotConsumed::Unproven), false)
        );
        let r = t.r.get(k).unwrap();
        assert_eq!((r.decision, r.first_armed_seen), (None, None), "{late:?}");
        assert_eq!(
            t.scope(),
            ScopeState::Open(Cause::EditorUnconfirmed),
            "{late:?}"
        );
        assert_eq!((t.r.scope().undecided(), t.r.count()), (0, 1));
    }
}

/// Scenario: A decisionless helper's exit is drained twice, and observed exiting twice. The second drain settles nothing more, consumes nothing more, books no second episode and asks for no re-evaluation; the first exit time is kept.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_a_decisionless_exit_drains_once() {
    let mut t = T::new(AUTH);
    t.raise(0);
    let k = key(1);
    t.obs(k, Channel::Scan, OWNED, None, 15);
    t.exit(k, 20);
    t.exit(k, 25);
    assert_eq!(t.drain(k, 30), (Settlement::Consumed, false));
    assert_eq!(t.drain(k, 31), (Settlement::Consumed, false));
    assert_eq!(t.r.count(), 0);
    assert_eq!(t.r.get(k).unwrap().exit_observed, Some(t.at(20)));
    assert_eq!(t.scope(), ScopeState::Open(Cause::EditorUnconfirmed));
    let stop = t.r.first_stop(t.at(40));
    assert_eq!(stop.episodes.len(), 1);
    assert_eq!(stop.episodes[0].resolved, Resolution::ExitWithoutDecision);
    assert_eq!(t.r.scope().undecided(), 0);
}

/// Scenario: Try each completion route on a record while the scope is pending, while it is open, and once it is closed, under both policies: E1/E1F on a matched `Armed` record, on an unmatched one, on a matched failed start and on an editor-bearing record; EC with and without its drain, a clean report and a taken frontier. Pending and open refuse every marker route; `Hold` refuses EC; E1/E1F needs a helper-only, exited, matched record; EC needs all three of its conditions; once the scope closes, the refused completion goes through.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_completion_routes() {
    let complete = |t: &mut T, k, route| {
        let now = t.at(90);
        t.r.complete(&mut t.f, G, k, route, now, now)
    };
    // Pending, then closed: the refused completion goes through.
    let mut t = T::new(HOLD);
    let (a, other) = (key(1), key(2));
    t.obs(a, Channel::Ipc, OWNED, Some(Decision::Armed), 10);
    t.r.match_segment(G, a, 1).unwrap();
    t.exit(a, 20);
    t.obs(other, Channel::Scan, OWNED, None, 21);
    assert_eq!(
        complete(&mut t, a, Route::HelperOnly),
        Err(Refusal::Scope(ScopeState::Pending))
    );
    assert!(kept(t.obs(other, Channel::Ipc, OWNED, Some(Decision::Armed), 30)).1);
    assert_eq!(t.r.admits(), Err(Hold::Incomplete(a)));
    assert_eq!(complete(&mut t, a, Route::HelperOnly), Ok(()));
    assert_eq!(t.r.admits(), Err(Hold::Incomplete(other)));
    assert_eq!(
        complete(&mut t, a, Route::HelperOnly),
        Err(Refusal::NotPending(Completion::Complete { at: t.at(90) }))
    );
    t.r.match_segment(G, other, 2).unwrap();
    t.exit(other, 91);
    assert_eq!(complete(&mut t, other, Route::HelperOnly), Ok(()));
    assert_eq!(t.r.admits(), Ok(()));
    // Open refuses, and stays.
    let mut t = T::new(HOLD);
    t.obs(a, Channel::Ipc, OWNED, Some(Decision::Armed), 10);
    t.r.match_segment(G, a, 1).unwrap();
    t.exit(a, 20);
    t.obs(other, Channel::Ipc, OWNED, Some(Decision::Tool), 21);
    assert_eq!(
        complete(&mut t, a, Route::HelperOnly),
        Err(Refusal::Scope(ScopeState::Open(
            Cause::EditorReturnUnchecked
        )))
    );
    // E1/E1F's own conditions, with the scope closed.
    for policy in [HOLD, AUTH] {
        let mut t = T::new(policy);
        t.obs(a, Channel::Ipc, OWNED, Some(Decision::Armed), 10);
        assert_eq!(
            complete(&mut t, a, Route::HelperOnly),
            Err(Refusal::NotExited)
        );
        t.exit(a, 20);
        assert_eq!(
            complete(&mut t, a, Route::HelperOnly),
            Err(Refusal::Unmatched),
            "an unmatched Armed record never completes by markers"
        );
        t.r.match_segment(G, a, 1).unwrap();
        assert_eq!(t.r.match_segment(G, a, 2), Err(Refusal::Matched(1)));
        assert_eq!(t.r.match_segment(G, a, 1), Ok(()), "the same segment again");
        assert_eq!(
            complete(&mut t, a, Route::EditorBearing),
            Err(match policy {
                EcPolicy::Hold => Refusal::Policy,
                EcPolicy::Authority => Refusal::NotDrained,
            })
        );
        assert_eq!(complete(&mut t, a, Route::HelperOnly), Ok(()));
    }
    // Under Authority: a failed start is helper-only; an editor-bearing
    // record isn't, and EC needs its drain, a clean report and a frontier.
    let mut t = T::new(AUTH);
    let (f, e) = (key(3), key(4));
    t.obs(f, Channel::Ipc, OWNED, Some(Decision::Unarmed), 10);
    t.report(f, Report::SpawnFailed { errno: Some(2) }, 11);
    t.r.match_segment(G, f, 1).unwrap();
    t.exit(f, 12);
    assert_eq!(complete(&mut t, f, Route::HelperOnly), Ok(()));
    assert_eq!(t.r.match_segment(G, key(9), 1), Err(Refusal::NoRecord));
    t.obs(e, Channel::Ipc, OWNED, Some(Decision::Unarmed), 20);
    t.r.match_segment(G, e, 2).unwrap();
    t.exit(e, 21);
    assert_eq!(
        complete(&mut t, e, Route::HelperOnly),
        Err(Refusal::NotHelperOnly)
    );
    assert_eq!(
        t.report(e, Report::Done(Tree::Clean), 22),
        Reported::RequestFrontier
    );
    assert_eq!(
        complete(&mut t, e, Route::EditorBearing),
        Err(Refusal::NotDrained)
    );
    t.drain(e, 23);
    assert_eq!(
        t.r.get(e).unwrap().frontier,
        Frontier::Requested,
        "a requested frontier waits"
    );
    assert_eq!(
        complete(&mut t, e, Route::EditorBearing),
        Err(Refusal::NoFrontier)
    );
    assert_eq!(
        t.r.frontier(G, e, FrontierOutcome::Taken { offset: 77 }),
        Ok(true)
    );
    assert_eq!(
        t.r.frontier(G, e, FrontierOutcome::Missing),
        Ok(false),
        "set once"
    );
    assert_eq!(t.r.get(e).unwrap().frontier, Frontier::Taken { offset: 77 });
    assert_eq!(complete(&mut t, e, Route::EditorBearing), Ok(()));
    // An unclean editor-bearing record never completes by EC.
    let mut t = T::new(AUTH);
    for (pid, report) in [(5, Report::Done(Tree::Unknown)), (6, Report::None)] {
        let k = key(pid);
        t.obs(k, Channel::Ipc, OWNED, Some(Decision::Unarmed), 10);
        if report != Report::None {
            t.report(k, report, 11);
        }
        t.exit(k, 12);
        t.drain(k, 13);
        assert_eq!(t.r.get(k).unwrap().frontier, Frontier::Missing);
    }
    assert!(matches!(
        complete(&mut t, key(5), Route::EditorBearing),
        Err(Refusal::Scope(_))
    ));
    // A record that needs no completion refuses one.
    let mut t = T::new(HOLD);
    t.obs(
        a,
        Channel::Ipc,
        Attribution::NotOwned(NotOwned::Shape),
        Some(Decision::Armed),
        10,
    );
    assert_eq!(
        complete(&mut t, a, Route::HelperOnly),
        Err(Refusal::NotPending(Completion::NotRequired))
    );
    assert_eq!(t.r.match_segment(G, a, 1), Err(Refusal::NotOwned));
}

/// Scenario: A pending record is followed by an `Owned` record born after its exit, born exactly at its exit, and born before it; the first record's editor ended (an `Armed` decision, a failed start, a `Clean` or `Survivors` report) or didn't (no report, a tree the helper couldn't account for); and the scope is open. Only a later `Owned` birth subsumes, only an ended editor can be subsumed, and an open scope doesn't stop it.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_subsumption() {
    let cases: &[(Decision, Report, bool)] = &[
        (Decision::Armed, Report::None, true),
        (Decision::Unarmed, Report::SpawnFailed { errno: None }, true),
        (Decision::Unarmed, Report::Done(Tree::Clean), true),
        (Decision::Unarmed, Report::Done(Tree::Survivors), true),
        (Decision::Unarmed, Report::Done(Tree::Unknown), false),
        (Decision::Unarmed, Report::None, false),
    ];
    for &(decision, report, ended) in cases {
        for (born, after) in [(31, true), (30, false), (29, false)] {
            for attribution in [OWNED, Attribution::Unattributable] {
                let mut t = T::new(HOLD);
                let (r1, r2) = (key(1), key(2));
                t.obs(r1, Channel::Ipc, OWNED, Some(decision), 10);
                if report != Report::None {
                    t.report(r1, report, 11);
                }
                t.exit(r1, 30);
                let o = Observation {
                    generation: G,
                    key: r2,
                    channel: Channel::Scan,
                    attribution,
                    claim: None,
                    decision: None,
                    seg_seen: None,
                    native_seen: None,
                    birth_floor: t.at(born),
                };
                t.full(o, 40);
                let want = if ended && after && attribution == OWNED {
                    Completion::Subsumed { by: r2 }
                } else {
                    Completion::Pending
                };
                assert_eq!(
                    t.r.get(r1).unwrap().completion,
                    want,
                    "{decision:?} {report:?} born {born} {attribution:?}"
                );
            }
        }
    }
    // The subsumer can arrive before the exit is seen.
    let mut t = T::new(HOLD);
    t.obs(key(1), Channel::Ipc, OWNED, Some(Decision::Armed), 10);
    let o = Observation {
        generation: G,
        key: key(2),
        channel: Channel::Ipc,
        attribution: OWNED,
        claim: None,
        decision: Some(Decision::Armed),
        seg_seen: None,
        native_seen: None,
        birth_floor: t.at(50),
    };
    t.full(o, 50);
    assert_eq!(t.r.get(key(1)).unwrap().completion, Completion::Pending);
    t.exit(key(1), 40);
    assert_eq!(
        t.r.get(key(1)).unwrap().completion,
        Completion::Subsumed { by: key(2) }
    );
}

/// Scenario: Through the generation's input lock, a predecessor generation's records are retired, and its callbacks keep arriving: an observation, an exit, a drain, a report, a completion. Every one is refused and touches nothing; the successor's own table is fresh, and refuses a callback that names the predecessor's generation.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_retirement_and_a_stale_predecessor_callback() {
    let t0 = Instant::now();
    let old = GenerationInput::new(CprWaitEnd::Counted);
    assert!(old.attach_records(Records::new(G, HOLD, t0)));
    let k = key(1);
    let o = Observation {
        generation: G,
        key: k,
        channel: Channel::Scan,
        attribution: OWNED,
        claim: None,
        decision: None,
        seg_seen: None,
        native_seen: None,
        birth_floor: t0,
    };
    old.record_observe(&o, t0).unwrap();
    old.liability_raise(G, t0).unwrap();
    old.retire();
    assert!(
        !old.attach_records(Records::new(G, HOLD, t0)),
        "no table after retirement"
    );
    let now = t0 + ms(10);
    let ipc = Observation {
        channel: Channel::Ipc,
        decision: Some(Decision::Armed),
        ..o
    };
    assert_eq!(old.record_observe(&ipc, now), Err(Refusal::Stale));
    assert_eq!(old.record_exit(G, k, now, now), Err(Refusal::Stale));
    assert_eq!(
        old.record_drained(G, k, now).map(|_| ()),
        Err(Refusal::Stale)
    );
    assert_eq!(
        old.record_report(G, k, Report::Done(Tree::Clean), now),
        Err(Refusal::Stale)
    );
    assert_eq!(
        old.record_complete(G, k, Route::HelperOnly, now, now),
        Err(Refusal::Stale)
    );
    assert_eq!(old.writer_scope(), None);
    assert_eq!(old.records_admit(), Err(Hold::Retired));

    let new = GenerationInput::new(CprWaitEnd::Counted);
    assert!(new.attach_records(Records::new(G + 1, HOLD, now)));
    assert!(!new.attach_records(Records::new(G + 1, HOLD, now)), "once");
    assert_eq!(new.record_observe(&ipc, now), Err(Refusal::Stale));
    assert_eq!(new.liability_raise(G, now), Err(Refusal::Stale));
    assert_eq!(new.writer_scope().unwrap().state(), ScopeState::Closed);
    assert_eq!(new.records_admit(), Ok(()));
    assert_eq!(new.record(k), None);
}

/// Scenario: Every lifecycle a record can have, each in its own table: decided by IPC, by marker, by a marker after a scan, by IPC after a scan; drained with no decision; retired while undecided; dropped at the cap. `undecided` is back at zero after each, and never goes below it.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_undecided_returns_to_zero_for_every_lifecycle() {
    let armed = Some(Decision::Armed);
    let lifecycles: [&[(Channel, Option<Decision>)]; 5] = [
        &[(Channel::Ipc, armed)],
        &[(Channel::Marker, armed)],
        &[(Channel::Scan, None), (Channel::Marker, armed)],
        &[(Channel::Scan, None), (Channel::Ipc, Some(Decision::Tool))],
        &[(Channel::Scan, None)],
    ];
    for steps in lifecycles {
        let mut t = T::new(AUTH);
        let k = key(1);
        for &(ch, d) in steps {
            t.obs(k, ch, OWNED, d, 10);
        }
        t.exit(k, 20);
        t.drain(k, 21);
        t.drain(k, 22);
        assert_eq!(t.r.scope().undecided(), 0, "{steps:?}");
    }
    let mut t = T::new(HOLD);
    t.obs(key(1), Channel::Scan, OWNED, None, 10);
    assert_eq!(t.r.scope().undecided(), 1);
    t.r.retire(t.at(20));
    assert_eq!(t.r.scope().undecided(), 0);
    let stop = t.r.first_stop(t.at(30));
    assert_eq!(stop.episodes[0].resolved, Resolution::Retired);
    assert_eq!(stop.stopped, None, "a censored generation");
    assert_eq!(stop.observed, ms(30));
}

/// Scenario: A `Clean` report asks for a frontier and the second report is ignored; a frontier outcome arriving with no request, or after one was set, changes nothing; a record whose helper didn't report clean reads `Missing` once drained, and an `Armed` one never needs a frontier.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_the_frontier_is_set_once() {
    let mut t = T::new(AUTH);
    let (a, b, c) = (key(1), key(2), key(3));
    t.obs(a, Channel::Ipc, OWNED, Some(Decision::Unarmed), 10);
    assert_eq!(
        t.r.frontier(G, a, FrontierOutcome::Missing),
        Ok(false),
        "no request"
    );
    assert_eq!(
        t.report(a, Report::Done(Tree::Clean), 11),
        Reported::RequestFrontier
    );
    assert_eq!(
        t.report(a, Report::Done(Tree::Survivors), 12),
        Reported::Duplicate
    );
    assert_eq!(
        t.scope(),
        ScopeState::Closed,
        "a duplicate report sets nothing"
    );
    assert_eq!(t.r.frontier(G, a, FrontierOutcome::Missing), Ok(true));
    assert_eq!(
        t.r.frontier(G, a, FrontierOutcome::Taken { offset: 1 }),
        Ok(false)
    );
    assert_eq!(t.r.get(a).unwrap().frontier, Frontier::Missing);
    t.obs(b, Channel::Ipc, OWNED, Some(Decision::Tool), 10);
    t.exit(b, 20);
    t.drain(b, 21);
    assert_eq!(t.r.get(b).unwrap().frontier, Frontier::Missing);
    t.obs(c, Channel::Ipc, OWNED, Some(Decision::Armed), 10);
    t.exit(c, 20);
    t.drain(c, 21);
    assert_eq!(t.r.get(c).unwrap().frontier, Frontier::NotApplicable);
}

/// Scenario: A generation's scope goes pending three times: resolved by `Armed`, by `Tool` under `Authority`, and by a drained exit with no decision, each after a known wait, then a `Survivors` report. M-FIRST's event lists the three episodes with their durations and resolutions, and the first stop's cause and time; a higher cause later raises the cause the scope shows, not the first stop. An episode ended by another record's `Survivors` report is resolved by that cause, and the 65th episode is counted rather than kept.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_the_first_stop_event() {
    let mut t = T::new(AUTH);
    t.obs(key(1), Channel::Scan, OWNED, None, 10);
    t.obs(key(1), Channel::Ipc, OWNED, Some(Decision::Armed), 15);
    t.obs(key(2), Channel::Scan, OWNED, None, 20);
    t.obs(key(2), Channel::Marker, OWNED, Some(Decision::Tool), 40);
    t.obs(key(3), Channel::Scan, OWNED, None, 50);
    t.exit(key(3), 55);
    t.drain(key(3), 60);
    t.report(key(2), Report::Done(Tree::Survivors), 70);
    let stop = t.r.first_stop(t.at(100));
    let got: Vec<_> = stop
        .episodes
        .iter()
        .map(|e| (e.duration, e.resolved))
        .collect();
    assert_eq!(
        got,
        [
            (ms(5), Resolution::Decided(Decision::Armed)),
            (ms(20), Resolution::Decided(Decision::Tool)),
            (ms(10), Resolution::ExitWithoutDecision),
        ]
    );
    let first = stop.stopped.unwrap();
    assert_eq!(
        (first.cause, first.after, first.source),
        (Cause::EditorUnconfirmed, ms(60), StopSource::Drain)
    );
    assert_eq!(
        first.record.map(|r| (r.key, r.decision)),
        Some((key(3), None))
    );
    assert_eq!(t.scope(), ScopeState::Open(Cause::EditorLeftWriter));
    assert_eq!((stop.observed, stop.episodes_uncounted), (ms(100), 0));

    // An episode a cause ends, from another record.
    let mut t = T::new(AUTH);
    t.obs(key(1), Channel::Ipc, OWNED, Some(Decision::Unarmed), 10);
    t.obs(key(2), Channel::Scan, OWNED, None, 20);
    t.report(key(1), Report::Done(Tree::Survivors), 35);
    let stop = t.r.first_stop(t.at(40));
    let got: Vec<_> = stop
        .episodes
        .iter()
        .map(|e| (e.duration, e.resolved))
        .collect();
    assert_eq!(got, [(ms(15), Resolution::Opened(Cause::EditorLeftWriter))]);

    // Episodes past the cap are counted.
    let mut t = T::new(HOLD);
    for n in 0..=EPISODE_CAP as u32 {
        let k = key(n);
        t.obs(k, Channel::Scan, OWNED, None, 1);
        t.obs(k, Channel::Ipc, OWNED, Some(Decision::Armed), 2);
        t.exit(k, 3);
        t.drain(k, 4);
        let now = t.at(5);
        t.r.prune(&mut t.f, G, k, true, now).unwrap();
    }
    let stop = t.r.first_stop(t.at(6));
    assert_eq!(
        (stop.episodes.len(), stop.episodes_uncounted),
        (EPISODE_CAP, 1)
    );
}

/// Scenario: Under `Hold`, generations stop on: a `Tool` decision by a helper attributed `NotOwned(Tool)`, and an `Unarmed` one at the same time by a helper attributed `NotOwned(Shape)`; a native `Unarmed` whose editor then failed to start, and one whose editor ran, each matched to a segment after the stop; a scan dropped at the record cap, which keeps its attribution; and an identity refusal of an IPC request, which keeps the request's claim and has no attribution. Once each record is drained and deleted, every first stop still names its source, its record's attribution, its match and its report, so no two read alike.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_the_first_stop_keeps_its_provenance() {
    let gone = |t: &mut T, k: RecordKey| {
        let now = t.at(50);
        assert!(t.r.prune(&mut t.f, G, k, true, now).unwrap().is_some());
        assert!(t.r.get(k).is_none());
        t.r.first_stop(now).stopped.unwrap()
    };

    // The same cause at the same time, from different editors.
    let not_owned = |n: NotOwned, channel, decision| {
        let mut t = T::new(HOLD);
        let k = key(1);
        t.obs(k, channel, Attribution::NotOwned(n), Some(decision), 10);
        t.exit(k, 20);
        t.drain(k, 21);
        gone(&mut t, k)
    };
    let tool = not_owned(NotOwned::Tool, Channel::Ipc, Decision::Tool);
    let shape = not_owned(NotOwned::Shape, Channel::Marker, Decision::Unarmed);
    assert_eq!((tool.cause, tool.after), (shape.cause, shape.after));
    assert_eq!(
        tool.source,
        StopSource::Decision {
            channel: Channel::Ipc,
            decision: Decision::Tool,
        }
    );
    assert_eq!(
        shape.source,
        StopSource::Decision {
            channel: Channel::Marker,
            decision: Decision::Unarmed,
        }
    );
    assert_eq!(
        tool.record.map(|r| r.attribution),
        Some(Attribution::NotOwned(NotOwned::Tool))
    );
    assert_eq!(
        shape.record.map(|r| r.attribution),
        Some(Attribution::NotOwned(NotOwned::Shape))
    );

    // A native `Unarmed` whose start failed, against one whose editor ran.
    let native = |report: Report| {
        let mut t = T::new(HOLD);
        let k = key(1);
        t.obs(k, Channel::Ipc, OWNED, Some(Decision::Unarmed), 10);
        t.r.match_segment(G, k, 4).unwrap();
        t.report(k, report, 12);
        t.exit(k, 15);
        t.drain(k, 16);
        let later = scan(&t, key(2), 20);
        t.full(later, 21);
        assert!(matches!(
            t.r.get(k).unwrap().completion,
            Completion::Subsumed { .. }
        ));
        gone(&mut t, k)
    };
    let failed = native(Report::SpawnFailed { errno: Some(2) });
    let ran = native(Report::Done(Tree::Clean));
    assert_eq!(
        (failed.cause, failed.after, failed.source),
        (ran.cause, ran.after, ran.source)
    );
    let (failed, ran) = (failed.record.unwrap(), ran.record.unwrap());
    assert_eq!((failed.matched, ran.matched), (Some(4), Some(4)));
    assert_eq!(failed.report, Report::SpawnFailed { errno: Some(2) });
    assert_eq!(ran.report, Report::Done(Tree::Clean));

    // Dropped observations have no record.
    let mut t = T::new(HOLD);
    for pid in 0..RECORD_CAP as u32 {
        t.obs(
            key(100 + pid),
            Channel::Ipc,
            Attribution::NotOwned(NotOwned::Tool),
            Some(Decision::Armed),
            5,
        );
    }
    assert_eq!(
        t.obs(key(1), Channel::Scan, OWNED, None, 20),
        Recorded::Dropped
    );
    let stop = t.r.first_stop(t.at(30)).stopped.unwrap();
    assert_eq!(
        (stop.cause, stop.source, stop.record),
        (
            Cause::EditorUnconfirmed,
            StopSource::DroppedAtCap {
                channel: Channel::Scan,
                decision: None,
                attribution: OWNED,
                claim: None,
            },
            None
        )
    );
    assert_eq!(stop.attribution(), Some(OWNED));
    let mut t = T::new(HOLD);
    let now = t.at(20);
    t.r.refused_identity(
        G,
        Channel::Ipc,
        Some(Decision::Tool),
        Some(Claim::Tool),
        now,
    )
    .unwrap();
    let stop = t.r.first_stop(now).stopped.unwrap();
    assert_eq!(
        (stop.cause, stop.source, stop.record),
        (
            Cause::EditorReturnUnchecked,
            StopSource::RefusedIdentity {
                channel: Channel::Ipc,
                decision: Some(Decision::Tool),
                claim: Some(Claim::Tool),
            },
            None
        )
    );
    assert_eq!(stop.attribution(), None, "an identity refusal has none");
    assert_eq!(
        tool.attribution(),
        Some(Attribution::NotOwned(NotOwned::Tool))
    );
}

/// Scenario: Generations fill the record cap under `Hold` with the same 64 `Armed` helpers, then each sees one more helper at the same moment. Two scans with no decision and no claim, one attributed `Owned` and one `NotOwned(Shape)`, stop alike except for the attribution each first stop keeps; two `Unarmed` IPC requests with the same attribution, one claiming `Owned` and one `Tool`, stop alike except for the claim each keeps. A last generation's first stop, an identity refusal of an `Unarmed` request reported through the input lock, keeps the request's claim through retirement and has no attribution.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_a_stop_dropped_at_the_cap_keeps_its_attribution() {
    let capped = |channel: Channel,
                  attribution: Attribution,
                  claim: Option<Claim>,
                  decision: Option<Decision>| {
        let t0 = Instant::now();
        let input = GenerationInput::new(CprWaitEnd::Counted);
        assert!(input.attach_records(Records::new(G, HOLD, t0)));
        let o = |pid: u32| Observation {
            generation: G,
            key: key(pid),
            channel: Channel::Ipc,
            attribution: Attribution::NotOwned(NotOwned::Tool),
            claim: None,
            decision: Some(Decision::Armed),
            seg_seen: None,
            native_seen: None,
            birth_floor: t0,
        };
        for pid in 0..RECORD_CAP as u32 {
            input.record_observe(&o(100 + pid), t0 + ms(5)).unwrap();
        }
        let dropped = Observation {
            channel,
            attribution,
            claim,
            decision,
            ..o(1)
        };
        assert_eq!(
            input.record_observe(&dropped, t0 + ms(20)).unwrap(),
            Recorded::Dropped
        );
        input
            .retire_taking_first_stop(t0 + ms(30))
            .unwrap()
            .stopped
            .unwrap()
    };

    // Two scans, no claim in either, differing only in attribution.
    let owned = capped(Channel::Scan, OWNED, None, None);
    let shape = capped(
        Channel::Scan,
        Attribution::NotOwned(NotOwned::Shape),
        None,
        None,
    );
    assert_eq!(
        (owned.cause, owned.after, owned.record),
        (shape.cause, shape.after, shape.record)
    );
    assert_eq!(owned.cause, Cause::EditorUnconfirmed);
    assert_ne!(owned, shape);
    assert_eq!(
        owned.source,
        StopSource::DroppedAtCap {
            channel: Channel::Scan,
            decision: None,
            attribution: OWNED,
            claim: None,
        }
    );
    assert_eq!(
        shape.source,
        StopSource::DroppedAtCap {
            channel: Channel::Scan,
            decision: None,
            attribution: Attribution::NotOwned(NotOwned::Shape),
            claim: None,
        }
    );
    assert_eq!(owned.attribution(), Some(OWNED));
    assert_eq!(
        shape.attribution(),
        Some(Attribution::NotOwned(NotOwned::Shape))
    );

    // Two IPC requests, same attribution, differing only in claim.
    let unarmed = Some(Decision::Unarmed);
    let claims_owned = capped(Channel::Ipc, OWNED, Some(Claim::Owned), unarmed);
    let claims_tool = capped(Channel::Ipc, OWNED, Some(Claim::Tool), unarmed);
    assert_eq!(
        (claims_owned.cause, claims_owned.after, claims_owned.record),
        (claims_tool.cause, claims_tool.after, claims_tool.record)
    );
    assert_eq!(claims_owned.cause, Cause::EditorReturnUnchecked);
    assert_ne!(claims_owned, claims_tool);
    assert_eq!(
        claims_owned.source,
        StopSource::DroppedAtCap {
            channel: Channel::Ipc,
            decision: unarmed,
            attribution: OWNED,
            claim: Some(Claim::Owned),
        }
    );
    assert_eq!(
        claims_tool.source,
        StopSource::DroppedAtCap {
            channel: Channel::Ipc,
            decision: unarmed,
            attribution: OWNED,
            claim: Some(Claim::Tool),
        }
    );
    assert_eq!(claims_owned.attribution(), claims_tool.attribution());

    // An identity refusal keeps the request's claim through retirement.
    let t0 = Instant::now();
    let input = GenerationInput::new(CprWaitEnd::Counted);
    assert!(input.attach_records(Records::new(G, HOLD, t0)));
    input
        .record_refused(
            G,
            Channel::Ipc,
            Some(Decision::Unarmed),
            Some(Claim::Owned),
            t0 + ms(20),
        )
        .unwrap();
    let refused = input
        .retire_taking_first_stop(t0 + ms(30))
        .unwrap()
        .stopped
        .unwrap();
    assert_eq!(
        (refused.cause, refused.after, refused.record),
        (Cause::EditorReturnUnchecked, ms(20), None)
    );
    assert_eq!(
        refused.source,
        StopSource::RefusedIdentity {
            channel: Channel::Ipc,
            decision: Some(Decision::Unarmed),
            claim: Some(Claim::Owned),
        }
    );
    assert_eq!(refused.attribution(), None);
}

/// Scenario: Through the generation's input lock, a helper is seen with no decision, so the writer scope is pending, and the generation retires. Reading M-FIRST's event before retirement misses the pending episode; retiring and taking the event in one step lists it as ended by retirement, and afterwards there is no event to take.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_retirement_takes_the_pending_episode() {
    let t0 = Instant::now();
    let input = GenerationInput::new(CprWaitEnd::Counted);
    assert_eq!(input.retire_taking_first_stop(t0), None, "no records");
    let input = GenerationInput::new(CprWaitEnd::Counted);
    assert!(input.attach_records(Records::new(G, HOLD, t0)));
    let o = Observation {
        generation: G,
        key: key(1),
        channel: Channel::Scan,
        attribution: OWNED,
        claim: None,
        decision: None,
        seg_seen: None,
        native_seen: None,
        birth_floor: t0,
    };
    input.record_observe(&o, t0 + ms(10)).unwrap();
    assert!(input.first_stop(t0 + ms(20)).unwrap().episodes.is_empty());
    let stop = input.retire_taking_first_stop(t0 + ms(30)).unwrap();
    let got: Vec<_> = stop
        .episodes
        .iter()
        .map(|e| (e.duration, e.resolved))
        .collect();
    assert_eq!(got, [(ms(20), Resolution::Retired)]);
    assert_eq!((stop.stopped, stop.observed), (None, ms(30)));
    assert_eq!(input.first_stop(t0 + ms(40)), None);
    assert_eq!(input.retire_taking_first_stop(t0 + ms(40)), None, "once");
    assert_eq!(input.records_admit(), Err(Hold::Retired));
}
