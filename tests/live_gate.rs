//! PRD #1666 part 3 (Q35-20; core §4.5–§4.6, R5 §4.2, plan §3.7): the gate's
//! side of segments in the live gate, pure over plain values: the accounting
//! oracle extended with segment releases and triggers (`inbox/probe/010`),
//! the arm section against acceptance (`inbox/probe/011`, O-20.2), and the
//! second `ExternalEdit` producer on a generation's input
//! (`inbox/probe/048`'s unit half).

use std::sync::Arc;
use std::time::Instant;

use dot_agent_deck::composer_probe::epoch::GenerationInput;
use dot_agent_deck::composer_probe::framing::Framing;
use dot_agent_deck::composer_probe::gate::{Admission, Cutting, Gate, Measure};
use dot_agent_deck::composer_probe::planner::{Cut, first_cut, next_cut};
use dot_agent_deck::composer_probe::probe;
use dot_agent_deck::composer_probe::query_ledger::CprWaitEnd;
use dot_agent_deck::composer_probe::record::{
    Attribution, Channel, Decision, EcPolicy, NotOwned, Observation, RecordKey, Records,
};
use dot_agent_deck::composer_probe::segments::Agent;
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

/// Where the candidates in `stream` end and where each began (the last
/// ground point before it), as the planner reads the whole stream.
fn candidates(stream: &[u8]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut framing = Framing::default();
    let mut from = 0;
    loop {
        let Some(cut) = first_cut(&framing, &stream[from..], false) else {
            return found;
        };
        // `start` is relative to `from` and to a ground point there.
        let mut start = from + cut.start;
        if cut.start == 0 && !framing.at_ground() {
            start = found.last().map_or(0, |&(end, _)| end);
        }
        found.push((from + cut.at, start));
        framing = cut.resume;
        from += cut.at;
    }
}

/// Scenario: Generate streams of held ops from text, BELs, CSI-u Ctrl+G parts and lone ESCs, each op cut where the planner reads a candidate, and drive the gate with random segment releases (as far as the next candidate, opening the fence or not) and triggers (through the next cut). Every byte comes out exactly once and in acceptance order; a release never writes any byte from a candidate's start onward; a trigger always ends exactly at a candidate's end; and once nothing holds a candidate, a release that may open the fence writes everything and opens it.
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
        let marks = candidates(&stream);
        let mut written: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut offset = 0usize;
        for _ in 0..12 {
            let next = marks.iter().find(|&&(end, _)| end > offset).copied();
            match rng.below(3) {
                0 | 1 => {
                    let open = rng.below(2) == 0;
                    let out = gate.release_until(Op::measure, Op::split_at, open);
                    let n: usize = out.iter().map(|o| o.bytes.len()).sum();
                    written.extend(out.into_iter().map(|o| (o.id, o.bytes)));
                    if let Some((_, start)) = next {
                        assert!(
                            offset + n <= start,
                            "seed {seed}: a release wrote into a candidate ({} > {start})",
                            offset + n
                        );
                    }
                    offset += n;
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

fn helper(input: &GenerationInput, pid: u32, attribution: Attribution) {
    let now = Instant::now();
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
            birth_floor: now,
        },
        now,
    );
}

/// Scenario: On an installed generation's input, read a complete bracketed-paste pair with no record made, a pair after an owned helper's record (before and after its disable), a pair after only a tool's record, a lone disable and a stale enable. Only the unexplained pairs move the epoch, once each and as foreign, the explained pair moves nothing whichever side the record came, and neither a lone disable nor an enable with no disable before it moves anything.
#[spec("inbox/probe/048")]
#[test]
fn probe_048_an_unexplained_paste_pair_moves_the_epoch_once() {
    const OFF: &[u8] = b"\x1b[?2004l";
    const ON: &[u8] = b"\x1b[?2004h";
    let now = Instant::now();
    let input = live_input();
    input.on_output_bytes(ON, now);
    assert_eq!(input.epoch().epoch(), 0, "an enable alone is no pair");
    input.on_output_bytes(&[OFF, ON].concat(), now);
    let e = input.epoch();
    assert_eq!(
        (e.epoch(), e.last_foreign()),
        (1, 1),
        "an unreported editor"
    );
    helper(&input, 41, Attribution::Owned);
    input.on_output_bytes(&[OFF, ON].concat(), now);
    assert_eq!(input.epoch().epoch(), 1, "the record explains the pair");
    input.on_output_bytes(OFF, now);
    helper(&input, 42, Attribution::Owned);
    input.on_output_bytes(ON, now);
    assert_eq!(input.epoch().epoch(), 1, "whichever arrives first");
    helper(&input, 43, Attribution::NotOwned(NotOwned::Tool));
    input.on_output_bytes(&[OFF, ON].concat(), now);
    assert_eq!(input.epoch().epoch(), 2, "a tool's record explains nothing");
    input.on_output_bytes(OFF, now);
    assert_eq!(input.epoch().epoch(), 2, "a lone disable");
}
