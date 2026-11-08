//! PRD #1666 part 3 (Q35-20; core §4.5–§4.6, R5 §4.2, plan §3.7): the gate's
//! side of segments in the live gate, pure over plain values: the accounting
//! oracle extended with segment releases and triggers (`inbox/probe/010`),
//! the arm section against acceptance (`inbox/probe/011`, O-20.2), and the
//! second `ExternalEdit` producer on a generation's input
//! (`inbox/probe/048`'s unit half).

use std::sync::Arc;
use std::time::Instant;

use dot_agent_deck::composer_probe::epoch::{GenerationInput, Writer};
use dot_agent_deck::composer_probe::framing::Framing;
use dot_agent_deck::composer_probe::gate::{Admission, Cutting, FENCE_CAP, Gate, Measure};
use dot_agent_deck::composer_probe::paste_mode::OutputHook;
use dot_agent_deck::composer_probe::planner::{Cut, first_cut, next_cut};
use dot_agent_deck::composer_probe::probe;
use dot_agent_deck::composer_probe::query_ledger::CprWaitEnd;
use dot_agent_deck::composer_probe::record::{
    Attribution, Channel, Decision, EcPolicy, NotOwned, Observation, RecordKey, Records,
};
use dot_agent_deck::composer_probe::segments::{Agent, Front, ReturnMarkers};
use dot_agent_deck::doorbell_txn::{ClaimState, JobClaim, Start, StartCtx};
use spec::spec;

/// A held op as the oracle tracks it: which accepted op it is part of, its
/// bytes, its cut (as the planner finds it) and the input framing at its
/// first byte.
#[derive(Debug, Clone)]
struct Op {
    id: usize,
    bytes: Vec<u8>,
    cut: Option<Cut>,
    framing: Framing,
}

impl Op {
    /// The release's reading of it (the production `UserFrame::measure`'s
    /// rule): its last ground point, before its candidate when it holds one.
    fn measure(&self) -> Measure {
        let mut framing = self.framing.clone();
        let limit = self.cut.as_ref().map_or(self.bytes.len(), |c| c.start);
        let mut ground = framing.at_ground().then_some(0);
        for (i, &b) in self.bytes.iter().enumerate() {
            if i >= limit {
                break;
            }
            framing.feed(b, |_| {});
            if framing.at_ground() {
                ground = Some(i + 1);
            }
        }
        Measure {
            ground,
            candidate: self.cut.is_some(),
        }
    }

    /// Its first `n` bytes, the rest left here with framing and cut moved.
    fn split_at(&mut self, n: usize) -> Op {
        let rest = self.bytes.split_off(n);
        let prefix = std::mem::replace(&mut self.bytes, rest);
        let framing = self.framing.clone();
        for &b in &prefix {
            self.framing.feed(b, |_| {});
        }
        if let Some(cut) = self.cut.as_mut() {
            cut.at -= n;
            cut.start = cut.start.saturating_sub(n);
        }
        Op {
            id: self.id,
            bytes: prefix,
            cut: None,
            framing,
        }
    }

    /// The trigger's split: through its cut.
    fn split(&mut self) -> Cutting<Op> {
        let Some(cut) = self.cut.take() else {
            return Cutting::Whole;
        };
        if cut.at >= self.bytes.len() {
            return Cutting::Through;
        }
        let rest = self.bytes.split_off(cut.at);
        let prefix = std::mem::replace(&mut self.bytes, rest);
        let framing = std::mem::replace(&mut self.framing, cut.resume.clone());
        self.cut = next_cut(&cut.resume, &self.bytes);
        let n = prefix.len();
        Cutting::Split(
            Op {
                id: self.id,
                bytes: prefix,
                cut: None,
                framing,
            },
            n,
        )
    }
}

/// A small deterministic generator, so a failure reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Bytes a person's ops are drawn from: text, a BEL, the parts of a CSI-u
/// Ctrl+G (`ESC [ 103 ; 5 u`) and a lone ESC.
const ALPHABET: &[u8] = b"ab\x07\x1b[103;5u";

/// Where the input is in ground after each prefix of `stream` (index `k`:
/// after `k` bytes), and where each candidate ends with the last ground point
/// before it, as the planner reads the whole stream.
fn model(stream: &[u8]) -> (Vec<bool>, Vec<(usize, usize)>) {
    let mut ground = vec![true];
    let mut framing = Framing::default();
    for &b in stream {
        framing.feed(b, |_| {});
        ground.push(framing.at_ground());
    }
    let mut found = Vec::new();
    let mut framing = Framing::default();
    let mut from = 0;
    while let Some(cut) = first_cut(&framing, &stream[from..], false) {
        let end = from + cut.at;
        let start = (0..end).rev().find(|&k| ground[k]).unwrap_or(0);
        found.push((end, start));
        framing = cut.resume;
        from = end;
    }
    (ground, found)
}

/// Scenario: Generate streams of held ops from text, BELs, CSI-u Ctrl+G parts and lone ESCs, each op cut where the planner reads a candidate, and drive the gate with random segment releases (as far as the next candidate, opening the fence or not) and triggers (through the next cut). Every byte comes out exactly once and in acceptance order; a release goes exactly as far as the last ground point before the next candidate (or everything when it may open with none held), never into it, and the gate's held byte count matches; no release submits an empty sub-op or leaves an emptied op held, one that may not open leaves the fence closed, and a trigger always ends exactly at a candidate's end; and once nothing holds a candidate, a release that may open the fence writes everything and opens it.
#[spec("inbox/probe/010")]
#[test]
fn probe_010_segment_releases_and_triggers_keep_every_byte_once_and_short_of_candidates() {
    for seed in 1..400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let gate: Arc<Gate<Op>> = Arc::default();
        assert!(gate.fence(0));
        let mut stream = Vec::new();
        let mut framing = Framing::default();
        let ops = 1 + rng.below(6) as usize;
        for id in 0..ops {
            let len = 1 + rng.below(8) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize])
                .collect();
            let op = Op {
                id,
                cut: first_cut(&framing, &bytes, false),
                framing: framing.clone(),
                bytes: bytes.clone(),
            };
            for &b in &bytes {
                framing.feed(b, |_| {});
            }
            stream.extend_from_slice(&bytes);
            let n = op.bytes.len();
            assert!(matches!(
                gate.admit(op, n, None, |_, _| false, |_| {}),
                Admission::Held
            ));
        }
        let (ground, marks) = model(&stream);
        let mut written: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut offset = 0usize;
        for _ in 0..12 {
            let next = marks.iter().find(|&&(end, _)| end > offset).copied();
            match rng.below(3) {
                0 | 1 => {
                    let open = rng.below(2) == 0;
                    let fenced = gate.status().fenced;
                    let out = gate.release_until(Op::measure, Op::split_at, open);
                    assert!(
                        out.iter().all(|o| !o.bytes.is_empty()),
                        "seed {seed}: a release submits no empty sub-op"
                    );
                    assert!(
                        !gate.holds_any(|o| o.bytes.is_empty()),
                        "seed {seed}: no emptied op stays held"
                    );
                    if fenced && !open {
                        assert!(
                            gate.status().fenced,
                            "seed {seed}: a release that may not open leaves the fence closed"
                        );
                    }
                    let n: usize = out.iter().map(|o| o.bytes.len()).sum();
                    written.extend(out.into_iter().map(|o| (o.id, o.bytes)));
                    // How far the model says this release goes: the last
                    // ground point before the next candidate; with none, the
                    // last ground point, or everything when it may open.
                    let expected = match next {
                        Some((_, start)) => start,
                        None if open => stream.len(),
                        None => (0..=stream.len()).rev().find(|&k| ground[k]).unwrap_or(0),
                    }
                    .max(offset);
                    assert_eq!(
                        offset + n,
                        expected,
                        "seed {seed}: a release went to {} where the model stops at {expected}",
                        offset + n
                    );
                    offset += n;
                    assert_eq!(
                        gate.status().held_bytes,
                        stream.len() - offset,
                        "seed {seed}: the gate counts what it holds"
                    );
                    if open && next.is_none() {
                        assert!(
                            !gate.status().fenced,
                            "seed {seed}: nothing held stays fenced"
                        );
                    }
                }
                _ => {
                    if !gate.holds_any(|op| op.cut.is_some()) {
                        continue;
                    }
                    let out = gate.release_cut(0, Op::split);
                    let n: usize = out.iter().map(|o| o.bytes.len()).sum();
                    written.extend(out.into_iter().map(|o| (o.id, o.bytes)));
                    offset += n;
                    assert_eq!(
                        gate.status().held_bytes,
                        stream.len() - offset,
                        "seed {seed}: the gate counts what it holds"
                    );
                    assert!(
                        marks.iter().any(|&(end, _)| end == offset),
                        "seed {seed}: a trigger ended at {offset}, not at a candidate's end"
                    );
                }
            }
        }
        // Drain: triggers and releases until nothing is held.
        loop {
            let mut out = gate.release_until(Op::measure, Op::split_at, true);
            if gate.holds_any(|op| op.cut.is_some()) {
                out.extend(gate.release_cut(0, Op::split));
            }
            if out.is_empty() && !gate.status().fenced {
                break;
            }
            if out.is_empty() && gate.status().held_ops == 0 {
                assert!(gate.open_if_empty());
                break;
            }
            written.extend(out.into_iter().map(|o| (o.id, o.bytes)));
        }
        let all: Vec<u8> = written.iter().flat_map(|(_, b)| b.clone()).collect();
        assert_eq!(all, stream, "seed {seed}: every byte once, in order");
        let ids: Vec<usize> = written.iter().map(|(id, _)| *id).collect();
        assert!(
            ids.windows(2).all(|w| w[0] <= w[1]),
            "seed {seed}: op order"
        );
    }
}

/// Scenario: Queue a probe's job on a gate and accept a person's op before its arm section, then run the section; queue another and run the section first, then accept an op; refuse a section; retire the gate before one; and queue two jobs so the first is no longer the pending one. An op accepted first takes the job back and the section never runs or closes a fence; an op accepted after a successful section is held behind the fence it closed; a refused section closes nothing and a later op takes the job back; retirement and a superseded job refuse without running the section.
#[spec("inbox/probe/011")]
#[test]
fn probe_011_the_arm_section_and_acceptance_serialize_on_the_gate() {
    let held = |gate: &Arc<Gate<u8>>| {
        matches!(
            gate.admit(1u8, 1, None, |_, _| false, |_| {}),
            Admission::Held
        )
    };
    // Acceptance before the section: the take-back wins.
    let gate: Arc<Gate<u8>> = Arc::default();
    let claim = Arc::new(JobClaim::default());
    gate.probe_queued(Arc::clone(&claim));
    assert!(matches!(
        gate.admit(1u8, 1, None, |_, _| false, |_| {}),
        Admission::Open(1)
    ));
    assert_eq!(claim.state(), ClaimState::Withdrawn);
    let mut ran = false;
    let armed = gate.arm_section(
        &claim,
        "retired",
        "taken back",
        |_| {
            ran = true;
            Ok(())
        },
        || 7,
    );
    assert_eq!(armed, Err("taken back"));
    assert!(!ran, "no arm for a taken-back job");
    assert!(!gate.status().fenced);

    // The section first: the op is held behind the fence it closed.
    let gate: Arc<Gate<u8>> = Arc::default();
    let claim = Arc::new(JobClaim::default());
    gate.probe_queued(Arc::clone(&claim));
    assert_eq!(
        gate.arm_section(&claim, "retired", "taken back", |_| Ok(()), || 7),
        Ok(())
    );
    assert!(gate.status().fenced);
    assert!(held(&gate), "an op after the section is held");
    assert_eq!(claim.state(), ClaimState::Queued, "and takes nothing back");

    // A refused section: no fence, and a later op takes the job back.
    let gate: Arc<Gate<u8>> = Arc::default();
    let claim = Arc::new(JobClaim::default());
    gate.probe_queued(Arc::clone(&claim));
    assert_eq!(
        gate.arm_section(&claim, "retired", "taken back", |_| Err("refused"), || 7),
        Err("refused")
    );
    assert!(!gate.status().fenced);

    // Retirement before the section.
    let gate: Arc<Gate<u8>> = Arc::default();
    let claim = Arc::new(JobClaim::default());
    gate.probe_queued(Arc::clone(&claim));
    assert!(gate.retire().is_empty());
    let mut ran = false;
    assert_eq!(
        gate.arm_section(
            &claim,
            "retired",
            "taken back",
            |_| {
                ran = true;
                Ok(())
            },
            || 7
        ),
        Err("retired")
    );
    assert!(!ran);

    // A job that is no longer the pending one.
    let gate: Arc<Gate<u8>> = Arc::default();
    let first = Arc::new(JobClaim::default());
    let second = Arc::new(JobClaim::default());
    gate.probe_queued(Arc::clone(&first));
    gate.probe_queued(Arc::clone(&second));
    assert_eq!(
        gate.arm_section(&first, "retired", "taken back", |_| Ok(()), || 7),
        Err("taken back")
    );
    assert!(!gate.status().fenced);
}

/// Scenario: Run a probe's start on a claim whose arm section succeeds on the gate, closing its fence, and have a withdrawal land after the section and before the writer's swap, with nothing held behind the fence. The job is not started, the undo runs once, and the fence the section composed opens again, so no stray fence survives a lost withdrawal (O-20.2).
#[spec("inbox/probe/011")]
#[test]
fn probe_011_a_withdrawal_after_the_section_undoes_the_composed_fence() {
    let gate: Arc<Gate<u8>> = Arc::default();
    let claim = Arc::new(JobClaim::default());
    gate.probe_queued(Arc::clone(&claim));
    let ctx = StartCtx {
        boundary: dot_agent_deck::composer_probe::planner::Boundary::Ground,
        retired: false,
        now: Instant::now(),
    };
    let mut undone = 0;
    let started = probe::start(
        &claim,
        &ctx,
        |_| -> Result<(), &str> {
            gate.arm_section(&claim, "retired", "taken back", |_| Ok(()), || 3)?;
            assert!(claim.withdraw(), "the withdrawal lands mid-start");
            Ok(())
        },
        || {
            undone += 1;
            assert!(gate.open_if_empty(), "the composed fence goes with it");
        },
    );
    assert_eq!(started, Start::NotQueued);
    assert_eq!(undone, 1);
    assert!(!gate.status().fenced);
}

fn live_input() -> GenerationInput {
    let input = GenerationInput::new(CprWaitEnd::Counted);
    assert!(input.attach_records(Records::new(9, EcPolicy::Hold, Instant::now())));
    assert!(input.attach_segments(9, Agent::Claude));
    input
}

fn helper(input: &GenerationInput, pid: u32, attribution: Attribution, born: Instant) {
    let _ = input.record_observe(
        &Observation {
            generation: 9,
            key: RecordKey { pid, start: 1 },
            channel: Channel::Ipc,
            attribution,
            claim: None,
            decision: Some(Decision::Unarmed),
            seg_seen: None,
            native_seen: None,
            birth_floor: born,
        },
        born,
    );
}

/// The generation's output as its hook reads it: one marker reader over
/// every byte, in order.
struct Output(ReturnMarkers);

impl Output {
    fn push(&mut self, input: &GenerationInput, bytes: &[u8], at: Instant) {
        let mut seen = Vec::new();
        self.0.feed(bytes, &mut seen);
        input.on_return_markers(&seen, at);
    }
}

/// Scenario: On an installed generation's input, read a complete bracketed-paste pair with no record made, a pair after an owned helper's record (before and after its disable), a pair after only a tool's record, a lone disable and a stale enable. Only the unexplained pairs move the epoch, once each and as foreign, the explained pair moves nothing whichever side the record came, and neither a lone disable nor an enable with no disable before it moves anything.
#[spec("inbox/probe/048")]
#[test]
fn probe_048_an_unexplained_paste_pair_moves_the_epoch_once() {
    const OFF: &[u8] = b"\x1b[?2004l";
    const ON: &[u8] = b"\x1b[?2004h";
    let t = Instant::now();
    let at = |ms: u64| t + std::time::Duration::from_millis(ms);
    let input = live_input();
    let mut out = Output(ReturnMarkers::default());
    out.push(&input, ON, at(0));
    assert_eq!(input.epoch().epoch(), 0, "an enable alone is no pair");
    out.push(&input, &[OFF, ON].concat(), at(1));
    let e = input.epoch();
    assert_eq!(
        (e.epoch(), e.last_foreign()),
        (1, 1),
        "an unreported editor"
    );
    helper(&input, 41, Attribution::Owned, at(2));
    out.push(&input, &[OFF, ON].concat(), at(3));
    assert_eq!(input.epoch().epoch(), 1, "the record explains the pair");
    out.push(&input, OFF, at(4));
    helper(&input, 42, Attribution::Owned, at(5));
    out.push(&input, ON, at(6));
    assert_eq!(input.epoch().epoch(), 1, "whichever arrives first");
    helper(&input, 43, Attribution::NotOwned(NotOwned::Tool), at(7));
    out.push(&input, &[OFF, ON].concat(), at(8));
    assert_eq!(input.epoch().epoch(), 2, "a tool's record explains nothing");
    out.push(&input, OFF, at(9));
    assert_eq!(input.epoch().epoch(), 2, "a lone disable");
}

/// Scenario: Fill a closed fence with one op of its whole byte cap, so the next op is handed back with a place in the queue, then let a segment release the held op without opening the fence. The waiting op's place is woken by the release, so its caller tries again rather than waiting for a fence that may never open.
#[spec("inbox/probe/010")]
#[test]
fn probe_010_a_segment_release_wakes_a_full_waiter() {
    let gate: Arc<Gate<Op>> = Arc::default();
    assert!(gate.fence(0));
    let big = Op {
        id: 0,
        bytes: vec![b'a'; FENCE_CAP],
        cut: None,
        framing: Framing::default(),
    };
    assert!(matches!(
        gate.admit(big, FENCE_CAP, None, |_, _| false, |_| {}),
        Admission::Held
    ));
    let next = Op {
        id: 1,
        bytes: b"b".to_vec(),
        cut: None,
        framing: Framing::default(),
    };
    let Admission::Full(_, mut waiter) = gate.admit(next, 1, None, |_, _| false, |_| {}) else {
        panic!("no room behind a full fence");
    };
    let out = gate.release_until(Op::measure, Op::split_at, false);
    assert_eq!(out.len(), 1, "the held op goes");
    assert!(gate.status().fenced, "the segment still holds the fence");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter.changed()).await
        })
        .expect("the release wakes the waiting op");
}

const OFF: &[u8] = b"\x1b[?2004l";
const ON: &[u8] = b"\x1b[?2004h";

/// An owned helper `pid` of generation 9 on `channel` that read segment
/// `seg` and decided `decision`, born at `born`.
fn owned(
    input: &GenerationInput,
    pid: u32,
    channel: Channel,
    decision: Decision,
    seg: Option<u64>,
    born: Instant,
) {
    input
        .record_observe(
            &Observation {
                generation: 9,
                key: RecordKey { pid, start: 1 },
                channel,
                attribution: Attribution::Owned,
                claim: None,
                decision: Some(decision),
                seg_seen: seg,
                native_seen: None,
                birth_floor: born,
            },
            born,
        )
        .unwrap();
}

/// Helper `pid`'s exit, observed and drained at `at`.
fn exits(input: &GenerationInput, pid: u32, at: Instant) {
    let key = RecordKey { pid, start: 1 };
    input.record_exit(9, key, at, at).unwrap();
    input.record_drained(9, key, at).unwrap();
}

/// A Claude probe on `input`, its own byte enqueued and its trigger reported
/// written at the output offset as it stands: its number.
fn written_probe(input: &GenerationInput, now: Instant) -> u64 {
    input.liability_raise(9, now).unwrap();
    let (n, job) = input.segment_open_probe(false, now).unwrap();
    input.enqueue(Writer::Deck(job), &[0x07]);
    input
        .segment_written(n, input.output_offset(), now, now)
        .unwrap();
    n
}

/// Scenario: Let an agent's output run before the generation's segments are attached, ending part-way through an escape sequence, and keep going as they attach. A probe's trigger is then written at the output offset counted from the generation's birth, its helper decides `Armed` with its number and exits, and Claude's return follows: the segment completes, its markers read at that same coordinate and in a framing that saw every byte (R-U2-02).
#[spec("inbox/probe/009")]
#[test]
fn probe_009_return_markers_count_from_the_generations_birth() {
    let now = Instant::now();
    let input = Arc::new(GenerationInput::new(CprWaitEnd::Counted));
    assert!(input.attach_records(Records::new(9, EcPolicy::Hold, now)));
    let mut hook = OutputHook::new(Arc::clone(&input));
    hook.on_output(&[b"x".repeat(4096).as_slice(), b"\x1b[?20"].concat());
    assert!(input.attach_segments(9, Agent::Claude));
    hook.on_output(b"04h ready");
    let n = written_probe(&input, now);
    owned(&input, 41, Channel::Ipc, Decision::Armed, Some(n), now);
    exits(&input, 41, now);
    assert_eq!(input.segments_view(now).len(), 1, "no return yet");
    hook.on_output(&[OFF, ON].concat());
    assert!(
        input.segments_view(now).is_empty(),
        "the return completes it: {:?}",
        input.segments_view(now)
    );
    assert_eq!(input.segments_front(), Some(Front::None));
}

/// Scenario: On an installed generation's input, a complete bracketed-paste pair is read before the record of the helper whose session it was (its helper born before the pair ended), so it moves the epoch; then that record is made, and then an unrelated editor's pair with no record. The late record accounts for the pair it was, banking nothing, so the unrelated pair still moves the epoch as foreign; a record whose helper was born after every such pair still explains its own pair (R-U2-04).
#[spec("inbox/probe/048")]
#[test]
fn probe_048_a_late_record_lends_nothing_to_a_later_pair() {
    let born = Instant::now();
    let input = Arc::new(GenerationInput::new(CprWaitEnd::Counted));
    assert!(input.attach_records(Records::new(9, EcPolicy::Hold, born)));
    assert!(input.attach_segments(9, Agent::Claude));
    let mut hook = OutputHook::new(Arc::clone(&input));
    hook.on_output(&[OFF, ON].concat());
    assert_eq!(input.epoch().last_foreign(), 1, "a pair with no record yet");
    owned(&input, 41, Channel::Ipc, Decision::Unarmed, None, born);
    hook.on_output(&[OFF, ON].concat());
    let e = input.epoch();
    assert_eq!(
        (e.epoch(), e.last_foreign()),
        (2, 2),
        "the late record lent its explanation to an unrelated pair"
    );
    let after = Instant::now() + std::time::Duration::from_secs(1);
    owned(&input, 42, Channel::Ipc, Decision::Unarmed, None, after);
    hook.on_output(&[OFF, ON].concat());
    assert_eq!(
        input.epoch().epoch(),
        2,
        "a record born after explains its own"
    );
}

/// Scenario: A Claude probe's helper decides `Armed` with its number on a marker before its own invocation, which would carry the daemon's read of its export, so its settlement waits; that invocation never comes. Claude's return then completes the segment as its exit is drained, in either order. The settlement still runs: the schedule records an `Armed` check with no empty read, and isn't due again until the epoch moves, and the segment is gone (R-U2-05).
#[spec("inbox/probe/010")]
#[test]
fn probe_010_a_completion_keeps_a_deferred_settlement() {
    for return_first in [true, false] {
        let now = Instant::now();
        let input = Arc::new(GenerationInput::new(CprWaitEnd::Counted));
        assert!(input.attach_records(Records::new(9, EcPolicy::Hold, now)));
        assert!(input.attach_segments(9, Agent::Claude));
        let mut hook = OutputHook::new(Arc::clone(&input));
        let n = written_probe(&input, now);
        assert!(input.probe_due(now), "{return_first}: no check yet");
        owned(&input, 41, Channel::Marker, Decision::Armed, Some(n), now);
        assert!(input.probe_due(now), "{return_first}: waits for the read");
        if return_first {
            hook.on_output(&[OFF, ON].concat());
            exits(&input, 41, now);
        } else {
            exits(&input, 41, now);
            hook.on_output(&[OFF, ON].concat());
        }
        assert!(
            input.segments_view(now).is_empty(),
            "{return_first}: complete"
        );
        assert!(
            !input.probe_due(now),
            "{return_first}: the settlement was lost with the segment"
        );
    }
}
