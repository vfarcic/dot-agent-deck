//! PRD #1666 part 3 (Q35-08; core §4.5–§4.7, amendments 1–6 of Q35-core):
//! the segment state machine (`inbox/probe/009`), E1F's truth table over the
//! output bytes (`inbox/probe/050`), and the completion half of the writer
//! scope's truth table (`inbox/probe/058`), pure over scripted records,
//! output and times.

use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::query_ledger::{HazardFifo, T_TRIGGER};
use dot_agent_deck::composer_probe::record::{
    Attribution, Cause, Channel, Completion, Decision, EC_POLICY, EcPolicy, FrontierOutcome,
    NotOwned, Observation, RecordKey, Records, Refusal as RecordRefusal, Report, Route, ScopeState,
    Tree,
};
use dot_agent_deck::composer_probe::segments::{
    Agent, Ended, Kind, Marker, NOT_RETURNED, Outcome, Refusal, ReturnMarkers, ReturnWatch,
    SEGMENT_CAP, Seen, Segments, Stage, View,
};
use spec::spec;

const G: u64 = 7;
const HOLD: EcPolicy = EcPolicy::Hold;
const AUTH: EcPolicy = EcPolicy::Authority;
const OFF: &[u8] = b"\x1b[?2004l";
const ON: &[u8] = b"\x1b[?2004h";
const FRAME: &[u8] = b"\x1b[?2026h";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// `n` milliseconds after `t`: a free function, so a call can take it while
/// the fixture's fields are borrowed.
fn time(t: Instant, n: u64) -> Instant {
    t + ms(n)
}

fn key(pid: u32) -> RecordKey {
    RecordKey {
        pid,
        start: 1_000 + u64::from(pid),
    }
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// One generation: its records, hazard FIFO, segments and output markers,
/// on a clock in milliseconds from `t`.
struct T {
    agent: Agent,
    r: Records,
    f: HazardFifo,
    s: Segments,
    m: ReturnMarkers,
    t: Instant,
}

impl T {
    fn new(agent: Agent, policy: EcPolicy) -> T {
        T::with(agent, policy, G)
    }

    /// Segments of generation `G` beside records of `records_generation`.
    fn with(agent: Agent, policy: EcPolicy, records_generation: u64) -> T {
        let t = Instant::now();
        T {
            agent,
            r: Records::new(records_generation, policy, t),
            f: HazardFifo::default(),
            s: Segments::new(G, agent),
            m: ReturnMarkers::default(),
            t,
        }
    }

    fn probe(&mut self, at: u64) -> u64 {
        let n = self
            .s
            .open_probe(false, time(self.t, at))
            .expect("a probe opens");
        let _ = self.r.raise(G, time(self.t, at));
        n
    }

    /// The writer took segment `n`'s trigger at `at`, with the output as fed.
    fn write(&mut self, n: u64, at: u64) {
        let offset = self.m.fed();
        self.s
            .written(
                &mut self.r,
                &mut self.f,
                n,
                offset,
                time(self.t, at),
                time(self.t, at),
            )
            .expect("submitted");
    }

    fn out(&mut self, bytes: &[u8], at: u64) {
        let mut seen = Vec::new();
        self.m.feed(bytes, &mut seen);
        self.s
            .on_output(&mut self.r, &mut self.f, &seen, time(self.t, at));
    }

    fn observe(&mut self, obs: Observation, at: u64) {
        let _ = self.r.observe(&mut self.f, &obs, time(self.t, at));
        self.s.settle(&mut self.r, &mut self.f, time(self.t, at));
    }

    /// A helper `pid` on `channel`, born at `born` and observed at `at`, that
    /// read `seg`.
    fn helper(
        &mut self,
        pid: u32,
        channel: Channel,
        attribution: Attribution,
        decision: Option<Decision>,
        seg: Option<u64>,
        (born, at): (u64, u64),
    ) {
        let obs = Observation {
            generation: G,
            key: key(pid),
            channel,
            attribution,
            claim: None,
            decision,
            seg_seen: seg,
            native_seen: None,
            birth_floor: time(self.t, born),
        };
        self.observe(obs, at);
    }

    /// The common case: an `Owned` helper's IPC request with its decision
    /// and the number it read.
    fn ipc(&mut self, pid: u32, decision: Decision, seg: u64, at: u64) {
        self.helper(
            pid,
            Channel::Ipc,
            Attribution::Owned,
            Some(decision),
            Some(seg),
            (at, at),
        );
    }

    fn report(&mut self, pid: u32, report: Report, at: u64) {
        let _ = self
            .r
            .report(&mut self.f, G, key(pid), report, time(self.t, at));
        self.s.settle(&mut self.r, &mut self.f, time(self.t, at));
    }

    fn frontier(&mut self, pid: u32) {
        let offset = self.m.fed();
        let _ = self
            .r
            .frontier(G, key(pid), FrontierOutcome::Taken { offset });
        self.s.settle(&mut self.r, &mut self.f, time(self.t, 0));
    }

    /// The helper's exit observed and drained at `at`.
    fn exit(&mut self, pid: u32, at: u64) {
        let _ = self
            .r
            .exit(&mut self.f, G, key(pid), time(self.t, at), time(self.t, at));
        if let Ok(d) = self.r.drained(&mut self.f, G, key(pid), time(self.t, at)) {
            let _ = self
                .s
                .drained(&mut self.r, &mut self.f, d.fact, time(self.t, at));
        } else {
            self.s.settle(&mut self.r, &mut self.f, time(self.t, at));
        }
    }

    fn editor(&mut self, pid: u32, at: u64) -> bool {
        self.s
            .editor_running(&mut self.r, &mut self.f, key(pid), time(self.t, at))
    }

    fn view(&self, at: u64) -> Vec<View> {
        self.s.view(&self.r, time(self.t, at))
    }

    fn stages(&self, at: u64) -> Vec<Stage> {
        self.view(at).iter().map(|v| v.stage).collect()
    }

    fn ended(&mut self) -> Vec<(Option<u64>, Kind, Outcome)> {
        self.s
            .take_ended()
            .into_iter()
            .map(|e: Ended| (e.number, e.kind, e.outcome))
            .collect()
    }

    fn written(&mut self) -> Vec<Option<u64>> {
        self.ended()
            .into_iter()
            .filter(|e| e.2 == Outcome::Written)
            .map(|e| e.0)
            .collect()
    }

    /// The agent's full return for its kind.
    fn returns(&mut self, at: u64) {
        let bytes = match self.agent {
            Agent::Claude => cat(&[OFF, ON]),
            Agent::Codex => cat(&[OFF, ON, FRAME]),
        };
        self.out(&bytes, at);
    }
}

// ---- inbox/probe/009: the segment state machine ----

/// Scenario: On Codex, open a probe, write its trigger, and let its own helper decide `Armed` with the probe's number. The bytes behind it stay held through the helper's exit, `?2004l` and `?2004h` (shown `AwaitingPostFlush`), and the segment completes, releasing them, only at the first `?2026h` after that enable; a held Ctrl+G behind it is numbered only then.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_a_codex_probe_releases_at_e1f() {
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    assert_eq!(n, 1);
    t.s.open_human_g(40).unwrap();
    assert_eq!(t.stages(0), [Stage::Submitted, Stage::Waiting]);
    t.write(n, 1);
    assert_eq!(t.s.next_trigger(), None, "the probe is outstanding");
    t.ipc(1, Decision::Armed, n, 5);
    assert_eq!(
        t.stages(5)[0],
        Stage::Matched,
        "a decision releases no Codex byte"
    );
    t.out(OFF, 6);
    t.exit(1, 7);
    assert_eq!(t.stages(7)[0], Stage::Matched);
    t.out(ON, 8);
    assert_eq!(t.stages(8)[0], Stage::AwaitingPostFlush, "E1 alone holds");
    assert!(t.written().is_empty());
    t.out(FRAME, 9);
    assert_eq!(t.written(), [Some(n)], "E1F completes and releases");
    assert!(matches!(
        t.r.get(key(1)).unwrap().completion,
        Completion::Complete { .. }
    ));
    assert_eq!(t.stages(9), [Stage::Waiting]);
    assert_eq!(
        t.s.next_trigger(),
        Some(2),
        "the held Ctrl+G is written next"
    );
    assert_eq!(t.s.next_trigger(), None, "once");
}

/// Scenario: On Claude, open a probe, write it and hold a Ctrl+G behind it. Its helper's `Armed` decision with the probe's number releases what is behind it at once, but the held Ctrl+G is numbered only after the helper's exit and Claude's `?2004l` `?2004h`.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_a_claude_probe_releases_at_the_decision() {
    for decision in [Decision::Armed, Decision::Unarmed] {
        let mut t = T::new(Agent::Claude, AUTH);
        let n = t.probe(0);
        t.s.open_human_g(40).unwrap();
        t.write(n, 1);
        assert_eq!(t.stages(1)[0], Stage::Unmatched);
        t.ipc(1, decision, n, 5);
        assert_eq!(t.stages(5)[0], Stage::Released, "{decision:?}");
        assert_eq!(t.s.next_trigger(), None, "{decision:?}: not complete");
        if decision == Decision::Unarmed {
            // Under Authority an editor-bearing return completes at EC.
            t.report(1, Report::Done(Tree::Clean), 6);
            t.exit(1, 7);
            t.frontier(1);
            t.out(ON, 8);
        } else {
            t.exit(1, 7);
            t.returns(8);
        }
        assert_eq!(t.written(), [Some(n)], "{decision:?}");
        assert_eq!(t.s.next_trigger(), Some(2), "{decision:?}");
    }
    // A `Tool` or unknown decision releases nothing early.
    for decision in [Decision::Tool, Decision::Unknown] {
        let mut t = T::new(Agent::Claude, AUTH);
        let n = t.probe(0);
        t.write(n, 1);
        t.ipc(1, decision, n, 5);
        assert_eq!(t.stages(5)[0], Stage::Matched, "{decision:?}");
    }
}

/// Scenario: Write a probe, then hand it evidence that isn't its own: return markers emitted before the trigger was written, a helper that read an older number, a tool's helper that read this number, and, on records of another generation, a helper that read this number and its exit facts. None matches or completes it; each control (its own helper, its own markers, the same generation) does.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_stale_foreign_and_other_generation_evidence() {
    // Stale: a whole return before the trigger.
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.returns(0);
    t.write(n, 1);
    t.ipc(1, Decision::Armed, n, 5);
    t.exit(1, 6);
    assert_eq!(t.stages(6)[0], Stage::Matched, "stale markers");
    assert!(t.written().is_empty());
    t.returns(7);
    assert_eq!(t.written(), [Some(n)], "the control: its own markers");

    // Foreign: an older number, a tool's helper, an unattributed one.
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Armed, n - 1, 2);
    t.helper(
        2,
        Channel::Ipc,
        Attribution::NotOwned(NotOwned::Tool),
        Some(Decision::Armed),
        Some(n),
        (2, 2),
    );
    t.helper(
        3,
        Channel::Ipc,
        Attribution::Unattributable,
        Some(Decision::Armed),
        Some(n),
        (2, 2),
    );
    for pid in 1..=3 {
        t.exit(pid, 3);
    }
    assert_eq!(t.view(3)[0].record, None, "nothing foreign matches");
    t.ipc(4, Decision::Armed, n, 5);
    assert_eq!(
        t.view(5)[0].record,
        Some(key(4)),
        "the control: its own helper"
    );
    t.exit(4, 6);
    t.returns(7);
    assert_eq!(t.written(), [Some(n)]);

    // Another generation's records and facts.
    let mut t = T::with(Agent::Codex, HOLD, G + 1);
    let n = t.s.open_probe(false, time(t.t, 0)).unwrap();
    t.write(n, 1);
    let obs = Observation {
        generation: G + 1,
        key: key(1),
        channel: Channel::Ipc,
        attribution: Attribution::Owned,
        claim: None,
        decision: Some(Decision::Armed),
        seg_seen: Some(n),
        native_seen: None,
        birth_floor: time(t.t, 2),
    };
    let _ = t.r.observe(&mut t.f, &obs, time(t.t, 2));
    t.s.settle(&mut t.r, &mut t.f, time(t.t, 2));
    let _ =
        t.r.exit(&mut t.f, G + 1, key(1), time(t.t, 3), time(t.t, 3));
    let d = t.r.drained(&mut t.f, G + 1, key(1), time(t.t, 3)).unwrap();
    assert_eq!(
        t.s.drained(&mut t.r, &mut t.f, d.fact, time(t.t, 3)),
        Err(Refusal::Stale)
    );
    t.returns(4);
    assert_eq!(t.view(4)[0].record, None, "another generation's record");
    assert!(t.written().is_empty());
    assert_eq!(t.r.get(key(1)).unwrap().matched, None);
}

/// Scenario: Complete a Codex probe with its markers and its helper's drained exit in each order: the whole return before the drain, the drain before the return, a `?2026h` written before the `?2004l`, and a second `?2004l` after the pair before the drain. It completes once the drain and a pair with a frame after it are both there, never on a frame before the enable, and a later `?2004l` starts the pair over.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_markers_and_the_exit_in_either_order() {
    let run = |before_exit: &[u8], after_exit: &[u8]| {
        let mut t = T::new(Agent::Codex, HOLD);
        let n = t.probe(0);
        t.write(n, 1);
        t.ipc(1, Decision::Armed, n, 2);
        t.out(before_exit, 3);
        let early = t.written();
        t.exit(1, 4);
        let at_exit = t.written();
        t.out(after_exit, 5);
        (early, at_exit, t.written())
    };
    let some = vec![Some(1)];
    assert_eq!(
        run(&cat(&[OFF, ON, FRAME]), b""),
        (vec![], some.clone(), vec![])
    );
    assert_eq!(
        run(b"", &cat(&[OFF, ON, FRAME])),
        (vec![], vec![], some.clone())
    );
    assert_eq!(run(&cat(&[OFF, ON]), FRAME), (vec![], vec![], some.clone()));
    assert_eq!(
        run(&cat(&[FRAME, OFF, ON]), b""),
        (vec![], vec![], vec![]),
        "a frame before the disable"
    );
    assert_eq!(
        run(&cat(&[OFF, ON, FRAME, OFF]), b""),
        (vec![], vec![], vec![]),
        "a later disable starts the pair over"
    );
    assert_eq!(
        run(&cat(&[OFF, ON, FRAME, OFF]), &cat(&[ON, FRAME])),
        (vec![], vec![], some),
        "the control: the next pair and frame"
    );
}

/// Scenario: Write a probe, then let helpers with no decision and no number be seen by a scan: one born before the trigger was written, one in the same instant, and two born after it. Only a single one born at or after the written instant matches; the earlier one and the pair match nothing. A scan keeps the writer scope pending, so its markers complete nothing until the helper declares `Armed`, and then the segment completes on what already arrived.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_the_birth_frontier() {
    let scan = |t: &mut T, pid: u32, born: u64, at: u64| {
        t.helper(
            pid,
            Channel::Scan,
            Attribution::Owned,
            None,
            None,
            (born, at),
        );
    };
    // Before the written instant: no match.
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 10);
    scan(&mut t, 1, 9, 11);
    assert_eq!(t.view(11)[0].record, None, "born before the write");
    // Two after it: ambiguous.
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 10);
    scan(&mut t, 1, 10, 11);
    scan(&mut t, 2, 12, 12);
    // The first sighting matched alone before the second was seen.
    assert_eq!(t.view(12)[0].record, Some(key(1)));
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.s.open_probe(false, time(t.t, 0)).unwrap();
    // Both seen before the writer reports the trigger: neither matches.
    scan(&mut t, 1, 10, 9);
    scan(&mut t, 2, 12, 9);
    t.write(n, 10);
    assert_eq!(t.view(12)[0].record, None, "two candidates");

    // At the written instant, alone: it matches, and waits for `Armed`.
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 10);
    scan(&mut t, 1, 10, 11);
    assert_eq!(t.view(11)[0].record, Some(key(1)));
    assert_eq!(t.r.scope().state(), ScopeState::Pending);
    t.returns(12);
    assert!(t.written().is_empty(), "pending refuses E1F");
    assert_eq!(t.stages(12)[0], Stage::Matched, "not drained");
    t.ipc(1, Decision::Armed, n, 13);
    assert_eq!(t.r.scope().state(), ScopeState::Closed);
    assert!(t.written().is_empty(), "its exit isn't drained");
    t.exit(1, 14);
    assert_eq!(t.written(), [Some(n)], "re-evaluated on what had arrived");
}

/// Scenario: Match a probe by its helper's birth while it has no number, then let the same helper name another segment's number. The segment holds for good, shown `Mismatched`, through its return; the control, the helper naming this segment's number, completes it.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_a_birth_match_naming_another_number_holds() {
    for (seen, completes) in [(5, false), (1, true)] {
        let mut t = T::new(Agent::Codex, HOLD);
        let n = t.probe(0);
        t.write(n, 10);
        t.helper(1, Channel::Scan, Attribution::Owned, None, None, (10, 11));
        assert_eq!(t.view(11)[0].record, Some(key(1)));
        t.helper(
            1,
            Channel::Ipc,
            Attribution::Owned,
            Some(Decision::Armed),
            Some(seen),
            (10, 12),
        );
        t.exit(1, 13);
        t.returns(14);
        if completes {
            assert_eq!(t.written(), [Some(n)]);
        } else {
            assert_eq!(t.stages(14), [Stage::Mismatched]);
            assert!(t.written().is_empty());
        }
    }
}

/// Scenario: On Claude, write a probe that no helper answers. Exactly `T_TRIGGER` after the write it is still unmatched and no retry opens; past it, it is `Lapsed`, a retry is refused while bytes are held behind it and opens otherwise, superseding it with the next number, and a helper that read the lapsed number matches nothing. A Codex probe never lapses.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_a_claude_probe_lapses_and_admits_one_retry() {
    let mut t = T::new(Agent::Claude, HOLD);
    let n = t.probe(0);
    t.write(n, 0);
    let tie = T_TRIGGER.as_millis() as u64;
    assert_eq!(t.stages(tie), [Stage::Unmatched]);
    assert_eq!(t.s.open_probe(false, time(t.t, tie)), Err(Refusal::Busy));
    assert_eq!(t.stages(tie + 1), [Stage::Lapsed]);
    assert_eq!(
        t.s.open_probe(true, time(t.t, tie + 1)),
        Err(Refusal::Busy),
        "bytes held behind it"
    );
    let retry = t.s.open_probe(false, time(t.t, tie + 1)).unwrap();
    assert_eq!(retry, n + 1);
    assert_eq!(t.ended(), [(Some(n), Kind::Probe, Outcome::Superseded)]);
    t.write(retry, tie + 2);
    t.ipc(1, Decision::Armed, n, tie + 3);
    assert_eq!(t.view(tie + 3)[0].record, None, "the lapsed number");
    t.ipc(2, Decision::Armed, retry, tie + 4);
    assert_eq!(t.stages(tie + 4), [Stage::Released], "the control");

    // A lapsed probe with a held Ctrl+G behind it admits no retry.
    let mut t = T::new(Agent::Claude, HOLD);
    let n = t.probe(0);
    t.write(n, 0);
    t.s.open_human_g(1).unwrap();
    assert_eq!(
        t.s.open_probe(false, time(t.t, tie + 1)),
        Err(Refusal::Busy)
    );

    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 0);
    assert_eq!(t.stages(tie * 10), [Stage::Unmatched]);
    assert_eq!(
        t.s.open_probe(false, time(t.t, tie * 10)),
        Err(Refusal::Busy)
    );
}

/// Scenario: Hold a person's Ctrl+G behind a completed probe and write it alone; its helper reads its number and decides `Unarmed` (the person's editor). Nothing behind it goes until the editor child is seen running. Under `Hold` its return never completes it, so a second held Ctrl+G is never written and the view shows `AwaitingReturn` with the writer scope's cause; under `Authority` the drain frontier then the return completes it and the second is written.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_a_held_ctrl_g_releases_to_a_running_editor() {
    for agent in [Agent::Claude, Agent::Codex] {
        for policy in [HOLD, AUTH] {
            let mut t = T::new(agent, policy);
            let n = t.probe(0);
            t.s.open_human_g(1).unwrap();
            t.s.open_human_g(2).unwrap();
            t.write(n, 1);
            t.ipc(1, Decision::Armed, n, 2);
            t.exit(1, 3);
            t.returns(4);
            assert_eq!(t.written(), [Some(n)]);
            let g = t.s.next_trigger().unwrap();
            t.write(g, 5);
            t.ipc(2, Decision::Unarmed, g, 6);
            assert_eq!(t.stages(6)[0], Stage::Matched, "{agent:?} {policy:?}");
            assert!(!t.editor(9, 6), "another record's editor");
            assert!(t.editor(2, 7));
            assert_eq!(t.stages(7)[0], Stage::Released, "{agent:?} {policy:?}");
            t.report(2, Report::Done(Tree::Clean), 8);
            t.exit(2, 9);
            t.frontier(2);
            t.out(&cat(&[ON, FRAME]), 10);
            let v = t.view(10);
            if policy == HOLD {
                assert!(v[0].ec, "{agent:?}: EC is computed, not authority");
                assert!(t.written().is_empty(), "{agent:?}: Hold completes nothing");
                assert_eq!(v[0].scope, ScopeState::Open(Cause::EditorReturnUnchecked));
                assert_eq!(t.s.next_trigger(), None);
            } else {
                assert_eq!(t.written(), [Some(g)], "{agent:?}");
                assert_eq!(t.s.next_trigger(), Some(g + 1));
            }
        }
    }
}

/// Scenario: Write a held Ctrl+G whose helper decides `Unarmed` and let it exit with no report (its wait failed), and, separately, an `Unarmed` helper that never reports an editor running. The first holds as `AwaitingReturn` with `EditorEndUnknown`, through a full return, until retirement ends it; the second holds everything until its return completes it under `Authority`.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_wait_unknown_and_an_unseen_editor_hold() {
    for policy in [HOLD, AUTH] {
        let mut t = T::new(Agent::Codex, policy);
        let n = t.probe(0);
        t.s.open_human_g(1).unwrap();
        t.write(n, 1);
        t.ipc(1, Decision::Unarmed, n, 2);
        t.exit(1, 3);
        t.returns(4);
        let v = t.view(4);
        assert_eq!(v[0].stage, Stage::AwaitingReturn, "{policy:?}");
        assert_eq!(v[0].scope, ScopeState::Open(Cause::EditorEndUnknown));
        assert!(t.written().is_empty());
        t.s.retire();
        assert_eq!(
            t.ended(),
            [
                (Some(n), Kind::Probe, Outcome::Retired),
                (None, Kind::HumanG, Outcome::Retired)
            ]
        );
        assert_eq!(t.s.open_probe(false, time(t.t, 5)), Err(Refusal::Retired));
        assert_eq!(t.s.next_trigger(), None);
    }
    // An editor never seen running: nothing goes before completion.
    let mut t = T::new(Agent::Codex, AUTH);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Unarmed, n, 2);
    t.report(1, Report::Done(Tree::Clean), 3);
    t.exit(1, 4);
    assert_eq!(t.stages(4), [Stage::AwaitingReturn]);
    t.frontier(1);
    t.out(ON, 5);
    assert_eq!(t.stages(5), [Stage::AwaitingReturn], "EC needs the frame");
    t.out(FRAME, 6);
    assert_eq!(t.written(), [Some(n)]);
}

/// Scenario: Write a held Ctrl+G whose helper decides `Armed`. On Claude it passes through at the decision and completes at E1; on Codex it holds until E1F. A matched helper that decided `Unarmed` and then failed to start the editor holds until E1 on Claude and E1F on Codex under `Authority`, and for good under `Hold`.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_armed_and_spawn_failed_rows() {
    for agent in [Agent::Claude, Agent::Codex] {
        let mut t = T::new(agent, HOLD);
        let n = t.probe(0);
        t.s.open_human_g(1).unwrap();
        t.write(n, 1);
        t.ipc(1, Decision::Armed, n, 2);
        t.exit(1, 3);
        t.returns(4);
        assert_eq!(t.written(), [Some(n)]);
        let g = t.s.next_trigger().unwrap();
        t.write(g, 5);
        t.ipc(2, Decision::Armed, g, 6);
        let expected = match agent {
            Agent::Claude => Stage::Released,
            Agent::Codex => Stage::Matched,
        };
        assert_eq!(t.stages(6), [expected], "{agent:?}");
        t.exit(2, 7);
        t.out(&cat(&[OFF, ON]), 8);
        match agent {
            Agent::Claude => assert_eq!(t.written(), [Some(g)]),
            Agent::Codex => {
                assert_eq!(t.stages(8), [Stage::AwaitingPostFlush]);
                t.out(FRAME, 9);
                assert_eq!(t.written(), [Some(g)]);
            }
        }

        for policy in [HOLD, AUTH] {
            let mut t = T::new(agent, policy);
            let n = t.probe(0);
            t.s.open_human_g(1).unwrap();
            t.write(n, 1);
            t.ipc(1, Decision::Armed, n, 2);
            t.exit(1, 3);
            t.returns(4);
            let _ = t.written();
            let g = t.s.next_trigger().unwrap();
            t.write(g, 5);
            t.ipc(2, Decision::Unarmed, g, 6);
            t.report(2, Report::SpawnFailed { errno: Some(2) }, 7);
            assert_ne!(t.view(7)[0].stage, Stage::Released);
            t.exit(2, 8);
            t.out(&cat(&[OFF, ON]), 9);
            let mut done = t.written();
            if agent == Agent::Codex {
                assert!(done.is_empty(), "{policy:?}: E1 alone");
                t.out(FRAME, 10);
                done = t.written();
            }
            let expected = if policy == AUTH {
                vec![Some(g)]
            } else {
                vec![]
            };
            assert_eq!(done, expected, "{agent:?} {policy:?}");
        }
    }
}

/// Scenario: Take a probe back before the writer starts it, cancel a held Ctrl+G's op, fail a held Ctrl+G's trigger write while its op carries another, and retire. Each ending names what happened: `TakenBack` and `Cancelled` only for triggers never started, a withdrawal after the write refused, `WriteFailed` for the failed trigger and `Gone` for its op's next one, while another op's Ctrl+G is written next.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_truthful_endings() {
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.s.withdrawn(n).unwrap();
    assert_eq!(t.ended(), [(Some(n), Kind::Probe, Outcome::TakenBack)]);
    assert_eq!(t.s.open_human_g(1), Err(Refusal::NotHeld), "nothing holds");

    let n = t.probe(1);
    t.write(n, 2);
    assert_eq!(
        t.s.withdrawn(n),
        Err(Refusal::NotSubmitted),
        "it was written"
    );
    assert_eq!(t.s.write_failed(n), Err(Refusal::NotSubmitted));
    assert_eq!(t.s.withdrawn(99), Err(Refusal::NoSegment));
    t.s.open_human_g(10).unwrap();
    t.s.open_human_g(10).unwrap();
    t.s.open_human_g(11).unwrap();
    t.s.open_human_g(12).unwrap();
    t.s.open_human_g(12).unwrap();
    assert_eq!(t.s.cancelled(11), 1);
    assert_eq!(t.ended(), [(None, Kind::HumanG, Outcome::Cancelled)]);
    t.ipc(1, Decision::Armed, n, 3);
    t.exit(1, 4);
    t.returns(5);
    assert_eq!(t.written(), [Some(n)]);
    let g = t.s.next_trigger().unwrap();
    t.s.write_failed(g).unwrap();
    assert_eq!(
        t.ended(),
        [
            (Some(g), Kind::HumanG, Outcome::WriteFailed),
            (None, Kind::HumanG, Outcome::Gone)
        ]
    );
    let g = t.s.next_trigger().unwrap();
    t.s.withdrawn(g).unwrap();
    assert_eq!(t.ended(), [(Some(g), Kind::HumanG, Outcome::Cancelled)]);
    assert_eq!(t.stages(6), [Stage::Waiting], "op 12's other Ctrl+G");
    t.s.retire();
    assert_eq!(t.ended(), [(None, Kind::HumanG, Outcome::Retired)]);
    t.s.retire();
    assert!(t.ended().is_empty(), "retirement ends once");

    // The cap.
    let mut t = T::new(Agent::Codex, HOLD);
    t.probe(0);
    for op in 1..SEGMENT_CAP as u64 {
        t.s.open_human_g(op).unwrap();
    }
    assert_eq!(t.s.open_human_g(0), Err(Refusal::Full));
}

/// Scenario: Hold a Codex probe through E1 and an editor-bearing one through its drain, and read the view as the clock passes `NOT_RETURNED`: each shows its stage and turns `not_returned` only once 5 s have passed in it (from the drain, whatever output follows it, then from E1), without being released. A Claude probe released at its decision reads `Released`, never an awaiting stage, through its helper's exit and return markers.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_the_view_publishes_awaiting_stages_only_while_holding() {
    let wait = NOT_RETURNED.as_millis() as u64;
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Armed, n, 2);
    t.exit(1, 3);
    t.out(OFF, 100);
    assert!(!t.view(3 + wait - 1)[0].not_returned);
    assert!(
        t.view(3 + wait)[0].not_returned,
        "E5, from the drain, not from later output"
    );
    t.out(&cat(&[OFF, ON]), 10);
    let v = t.view(10 + wait - 1);
    assert_eq!(v[0].stage, Stage::AwaitingPostFlush);
    assert!(!v[0].not_returned, "E5′ runs from E1");
    assert!(t.view(10 + wait)[0].not_returned, "E5′");
    assert!(t.written().is_empty(), "the display releases nothing");

    let mut t = T::new(Agent::Codex, AUTH);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Unarmed, n, 2);
    t.report(1, Report::Done(Tree::Clean), 3);
    t.exit(1, 4);
    let v = t.view(4 + wait);
    assert_eq!(v[0].stage, Stage::AwaitingReturn);
    assert!(v[0].not_returned);
    assert_eq!(v[0].fact.map(|f| f.key), Some(key(1)));

    let mut t = T::new(Agent::Claude, HOLD);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Armed, n, 2);
    t.exit(1, 3);
    t.out(OFF, 4);
    for at in [3, 4, 4 + wait] {
        let v = t.view(at);
        assert_eq!(v[0].stage, Stage::Released);
        assert!(!v[0].not_returned);
    }
}

/// Scenario: Under `Hold`, let a probe's `Unarmed` editor exit clean, then let a later invocation of the agent's own subsume and delete its record; separately, the same for a Codex probe's `Armed` helper before its return. Each segment keeps the record's exit facts, shows its stage from them, and keeps holding through a full return: subsumption completes the record, not the segment.
#[spec("inbox/probe/009")]
#[test]
fn probe_009_a_pruned_record_leaves_its_facts_and_the_hold() {
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Unarmed, n, 2);
    t.report(1, Report::Done(Tree::Clean), 3);
    t.exit(1, 4);
    t.ipc(2, Decision::Armed, 0, 10);
    assert!(matches!(
        t.r.get(key(1)).unwrap().completion,
        Completion::Subsumed { .. }
    ));
    let pruned =
        t.r.prune(&mut t.f, G, key(1), true, time(t.t, 11))
            .unwrap()
            .is_some();
    assert!(pruned);
    assert!(t.r.get(key(1)).is_none());
    t.returns(12);
    t.s.settle(&mut t.r, &mut t.f, time(t.t, 12));
    let v = t.view(12);
    assert_eq!(v[0].stage, Stage::AwaitingReturn);
    assert_eq!(v[0].fact.map(|f| f.key), Some(key(1)));
    assert_eq!(v[0].fact.map(|f| f.decision), Some(Some(Decision::Unarmed)));
    assert!(t.written().is_empty());

    // A helper-only record subsumed before its return: its stage is read
    // from its kept facts, and its return still completes nothing.
    let mut t = T::new(Agent::Codex, HOLD);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Armed, n, 2);
    t.exit(1, 3);
    t.ipc(2, Decision::Armed, 0, 10);
    let pruned =
        t.r.prune(&mut t.f, G, key(1), true, time(t.t, 11))
            .unwrap()
            .is_some();
    assert!(pruned);
    assert_eq!(t.stages(11), [Stage::Matched]);
    t.out(&cat(&[OFF, ON]), 12);
    assert_eq!(t.stages(12), [Stage::AwaitingPostFlush]);
    t.out(FRAME, 13);
    assert_eq!(t.stages(13), [Stage::AwaitingPostFlush]);
    assert!(t.written().is_empty());
}

// ---- inbox/probe/050: E1F's truth table ----

/// Run a Codex (or Claude) probe whose `Armed` helper drained, then feed
/// `pushes` after its trigger: whether the segment completed.
fn completes(agent: Agent, pushes: &[&[u8]], drained: bool) -> bool {
    let mut t = T::new(agent, HOLD);
    let n = t.probe(0);
    t.write(n, 1);
    t.ipc(1, Decision::Armed, n, 2);
    if drained {
        t.exit(1, 3);
    }
    for p in pushes {
        t.out(p, 4);
    }
    !t.written().is_empty()
}

/// Scenario: On Codex, drive a drained, matched `Armed` probe with output after its trigger: nothing, the disable alone, E1 alone, a frame before the disable or before the enable, the frame with combined parameters or an interrupted escape, the markers inside an OSC or DCS payload, a combined enable, and the whole return with no exit drained. Only `?2004l`, `?2004h`, then an exact `?2026h`, with the exit drained, completes it, and on Claude the pair alone does.
#[spec("inbox/probe/050")]
#[test]
fn probe_050_the_e1f_truth_table() {
    let codex = |pushes: &[&[u8]]| completes(Agent::Codex, pushes, true);
    assert!(codex(&[OFF, ON, FRAME]), "the positive");
    assert!(!codex(&[]), "nothing");
    assert!(!codex(&[OFF]), "the disable alone");
    assert!(!codex(&[OFF, ON]), "E1 alone holds");
    assert!(!codex(&[FRAME, OFF, ON]), "a frame before the disable");
    assert!(!codex(&[OFF, FRAME, ON]), "a frame before the enable");
    assert!(!codex(&[ON, OFF, FRAME]), "an enable before the disable");
    for (frame, why) in [
        (&b"\x1b[?1049;2026h"[..], "combined parameters"),
        (b"\x1b[?2026;1h", "a mode after 2026"),
        (b"\x1b[?2026 h", "an intermediate"),
        (b"\x1b[?2026\x07h", "a BEL inside"),
        (b"\x1b\x07[?2026h", "ESC BEL ["),
        (b"\x1b[?2026l", "a reset"),
        (b"\x1b]0;[?2026h\x07", "an OSC payload"),
        (b"\x1bP[?2026h\x1b\\", "a DCS payload"),
    ] {
        assert!(!codex(&[OFF, ON, frame]), "{why}");
        let fresh_after = cat(&[frame, FRAME]);
        assert!(codex(&[OFF, ON, &fresh_after]), "{why}: the fresh control");
    }
    // An ESC inside a string ends it and opens the next sequence, so the
    // exact marker after it counts, as it does for `B` (`060`).
    assert!(codex(&[OFF, ON, b"\x1b]0;title\x1b[?2026h"]));
    // Every ESC-to-`[` gap of `060`.
    for gap in (0x00..=0x1fu8)
        .filter(|b| ![0x18, 0x1a, 0x1b].contains(b))
        .chain([0x7f])
        .chain(0x80..=0xff)
    {
        let frame = cat(&[b"\x1b", &[gap], b"[?2026h"]);
        assert!(!codex(&[OFF, ON, &frame]), "{gap:#04x} after the ESC");
    }
    for (enable, why) in [
        (&b"\x1b[?1;2004h"[..], "a combined enable"),
        (b"\x1b\x07[?2004h", "ESC BEL ["),
        (b"\x1b[?2004 h", "an intermediate"),
        (b"\x1b]0;[?2004h\x07", "an OSC payload"),
    ] {
        assert!(!codex(&[OFF, enable, FRAME]), "{why}");
        assert!(!completes(Agent::Claude, &[OFF, enable], true), "{why}");
    }
    assert!(!codex(&[b"\x1b[?1;2004l", ON, FRAME]), "a combined disable");
    // Split at every offset: completes once whole.
    let whole = cat(&[OFF, ON, FRAME]);
    for a in 0..=whole.len() {
        for b in a..=whole.len() {
            assert!(
                codex(&[&whole[..a], &whole[a..b], &whole[b..]]),
                "split at {a} and {b}"
            );
        }
    }
    assert!(
        !completes(Agent::Codex, &[OFF, ON, FRAME], false),
        "no exit"
    );
    assert!(completes(Agent::Claude, &[OFF, ON], true), "Claude: E1");
    assert!(
        !completes(Agent::Claude, &[OFF, ON], false),
        "Claude: no exit"
    );
    assert!(!completes(Agent::Claude, &[OFF], true));
}

/// Scenario: Feed the marker reader the three return markers, split across pushes, and their lookalikes, and read the offsets it reports. Each exact marker is reported once, with the offset of its own ESC however the bytes were split; a lookalike is reported as nothing.
#[spec("inbox/probe/050")]
#[test]
fn probe_050_the_markers_and_their_offsets() {
    let stream = cat(&[
        b"ab",
        OFF,
        b"\x1b[?1;2004h",
        ON,
        b"x",
        FRAME,
        b"\x1b[?2026l",
    ]);
    let expected = [
        Seen {
            marker: Marker::PasteOff,
            start: 2,
        },
        Seen {
            marker: Marker::PasteOn,
            start: 20,
        },
        Seen {
            marker: Marker::Frame,
            start: 29,
        },
    ];
    for at in 0..=stream.len() {
        let mut m = ReturnMarkers::default();
        let mut seen = Vec::new();
        m.feed(&stream[..at], &mut seen);
        m.feed(&stream[at..], &mut seen);
        assert_eq!(seen, expected, "split at {at}");
        assert_eq!(m.fed(), stream.len() as u64);
    }
}

/// Scenario: Watch EC after a frontier at offset 10 and E1 after a trigger at offset 10, feeding markers that start before, at and after it. A marker whose ESC is before the bound counts for neither, one at the bound counts, and EC needs no disable.
#[spec("inbox/probe/050")]
#[test]
fn probe_050_the_start_offset_boundary() {
    let at = |marker, start| Seen { marker, start };
    let mut ec = ReturnWatch::ec(10);
    ec.feed(at(Marker::PasteOn, 9));
    ec.feed(at(Marker::Frame, 12));
    assert!(!ec.enabled(), "an enable starting before o");
    ec.feed(at(Marker::PasteOn, 10));
    assert!(ec.returned(Agent::Claude));
    assert!(!ec.returned(Agent::Codex));
    ec.feed(at(Marker::PasteOff, 15));
    ec.feed(at(Marker::Frame, 20));
    assert!(ec.returned(Agent::Codex), "EC reads no disable");

    let mut e1 = ReturnWatch::e1(10);
    e1.feed(at(Marker::PasteOff, 9));
    e1.feed(at(Marker::PasteOn, 15));
    assert!(!e1.enabled(), "a disable before the trigger");
    e1.feed(at(Marker::PasteOff, 10));
    e1.feed(at(Marker::PasteOn, 10));
    assert!(!e1.enabled(), "an enable must start after the disable");
    e1.feed(at(Marker::PasteOn, 18));
    e1.feed(at(Marker::Frame, 18));
    assert!(
        !e1.returned(Agent::Codex),
        "a frame must start after the enable"
    );
    e1.feed(at(Marker::Frame, 26));
    assert!(e1.returned(Agent::Codex));
}

// ---- inbox/probe/058: the completion half ----

/// What the writer scope is when a segment's route is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Closed,
    /// Another helper seen by a scan with no decision yet.
    Pending,
    /// Another helper's `Survivors` report.
    Open,
}

/// Scenario: For each agent, policy, writer-scope state (closed, pending on another helper's undecided sighting, open on another helper's survivors) and route (a helper-only probe's E1/E1F, an editor-bearing probe's EC), complete the route's markers and read whether the segment completed. Only a closed scope completes, and EC only under `Authority`; when the pending helper then declares `Armed`, the refused helper-only route completes on the markers it already had.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_completion_routes_against_the_scope() {
    for agent in [Agent::Claude, Agent::Codex] {
        for policy in [HOLD, AUTH] {
            for scope in [Scope::Closed, Scope::Pending, Scope::Open] {
                for helper_only in [true, false] {
                    let mut t = T::new(agent, policy);
                    let n = t.probe(0);
                    t.write(n, 1);
                    match scope {
                        Scope::Closed => {}
                        Scope::Pending => {
                            t.helper(9, Channel::Scan, Attribution::Owned, None, None, (0, 2))
                        }
                        Scope::Open => {
                            t.ipc(9, Decision::Armed, 0, 2);
                            t.report(9, Report::Done(Tree::Survivors), 2);
                        }
                    }
                    let decision = if helper_only {
                        Decision::Armed
                    } else {
                        Decision::Unarmed
                    };
                    t.ipc(1, decision, n, 3);
                    if helper_only {
                        t.exit(1, 4);
                        t.returns(5);
                    } else {
                        t.report(1, Report::Done(Tree::Clean), 4);
                        t.exit(1, 4);
                        t.frontier(1);
                        t.out(&cat(&[ON, FRAME]), 5);
                    }
                    // The oracle, from the amendments: markers complete only
                    // in a closed scope, and EC only under Authority (under
                    // Hold the editor-bearing decision opens the scope too).
                    let expect = scope == Scope::Closed && (helper_only || policy == AUTH);
                    let done = !t.written().is_empty();
                    let case = format!("{agent:?} {policy:?} {scope:?} helper-only {helper_only}");
                    assert_eq!(done, expect, "{case}");
                    if scope == Scope::Pending && helper_only {
                        // The pending helper decides; the harness settles
                        // the segments after every record step.
                        t.ipc(9, Decision::Armed, 0, 6);
                        assert_eq!(t.r.scope().state(), ScopeState::Closed, "{case}");
                        assert_eq!(t.written(), [Some(n)], "{case}: re-evaluated");
                    }
                }
            }
        }
    }
}

/// Scenario: Under `Hold`, the production policy, a native record whose editor failed to start (an `Unarmed` decision, then `SpawnFailed`), and a native `Clean` editor with its frontier taken, are offered EC after their whole return: the writer scope or the policy refuses both. In a fresh generation whose scope is closed, the policy itself refuses EC to an `Armed` record.
#[spec("inbox/probe/058")]
#[test]
fn probe_058_hold_completes_nothing_by_ec() {
    assert_eq!(EC_POLICY, HOLD);
    // Each in its own generation, so neither subsumes the other.
    for (pid, report) in [
        (1, Report::SpawnFailed { errno: Some(2) }),
        (2, Report::Done(Tree::Clean)),
    ] {
        let mut t = T::new(Agent::Codex, EC_POLICY);
        t.ipc(pid, Decision::Unarmed, 0, 1);
        t.report(pid, report, 2);
        t.exit(pid, 3);
        t.frontier(pid);
        let mut watch = ReturnWatch::ec(t.m.fed());
        let mut seen = Vec::new();
        t.m.feed(&cat(&[ON, FRAME]), &mut seen);
        for s in seen {
            watch.feed(s);
        }
        assert!(watch.returned(Agent::Codex), "EC's markers are there");
        let refused = t.r.complete(
            &mut t.f,
            G,
            key(pid),
            Route::EditorBearing,
            time(t.t, 7),
            time(t.t, 7),
        );
        assert!(
            matches!(
                refused,
                Err(RecordRefusal::Scope(_)) | Err(RecordRefusal::Policy)
            ),
            "{pid}: {refused:?}"
        );
    }
    // With the scope closed, the policy is the refusal.
    let mut t = T::new(Agent::Codex, EC_POLICY);
    t.ipc(1, Decision::Armed, 0, 1);
    t.exit(1, 2);
    assert_eq!(
        t.r.complete(
            &mut t.f,
            G,
            key(1),
            Route::EditorBearing,
            time(t.t, 3),
            time(t.t, 3)
        ),
        Err(RecordRefusal::Policy),
        "the policy refuses before the route's own checks"
    );
}
