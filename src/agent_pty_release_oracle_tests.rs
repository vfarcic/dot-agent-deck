//! PRD #1666 part 3 (Q35-20b, plan §3.7, X-3): the release-authority oracle,
//! composed with the production driver (`inbox/probe/010`).
//!
//! **What runs is production code.** A real [`PaneWriter`] over a scripted
//! PTY, its gate and writer thread; the generation's real input
//! ([`GenerationInput`]: records, segments, epoch, schedule) and its real
//! output hook ([`OutputHook`]), fed from before the segments attach; the
//! production release driver ([`super::segment_driver::drive_segments`]) as
//! its own task, woken as production wakes it; a probe queued through
//! [`PaneWriter::queue_probe`] with the gate's arm section closing its fence,
//! and its written report given as `composer_probe` gives it. What the
//! observer would see (a helper's records, its editor running, its report and
//! exit) reaches the input through the same calls the observer makes. Only
//! admission is left out: every probe here is admitted.
//!
//! **What judges it is not.** [`Model`] is the release table of core §4.6
//! and plan §3.7, written from the table, over the generated script alone:
//! which record matches which segment, which row releases what and when, and
//! what each trigger may carry. It never reads the implementation's state.
//! After every step, once the driver has had its chance, the bytes the PTY
//! took must be exactly what the model allows by then: a byte early is a
//! release without authority, a byte missing is a release that never came.
//! Every Ctrl+G must be written alone, after its number was published, and
//! nothing after retirement. The probe's settlement is judged the same way:
//! the schedule must stop being due exactly when the model settles it.
//!
//! The generated scripts carry the six traces of the Q35-20 review (R-U2-01
//! to 06) as ordinary steps: output before the segments attach (02), a
//! return and an editor read while a trigger's report is on its way (01),
//! typing after a release with nothing else moving (03), a deferred
//! settlement whose segment completes (05) and an unknown wait after a
//! release to a running editor (06). R-U2-04's paste-pair accounting has its
//! own generated model below ([`pairs`]). Each trace also has a fixed script.

use super::segment_driver::{ProbeEnd, ProbeRefusal, drive_segments};
use super::*;
use crate::composer_probe::epoch::GenerationInput;
use crate::composer_probe::paste_mode::OutputHook;
use crate::composer_probe::record::{
    Attribution, Channel, Decision, EcPolicy, NotOwned, Observation, RecordKey, Records, Report,
    Tree,
};
use crate::composer_probe::segments::{Agent, Stage};
use crate::ingress_ledger::IngressLedger;
use spec::spec;
use std::collections::VecDeque;

const G: u64 = 5;
const OFF: &[u8] = b"\x1b[?2004l";
const ON: &[u8] = b"\x1b[?2004h";
const FRAME: &[u8] = b"\x1b[?2026h";
const CTRL_G: u8 = 0x07;

/// What the PTY and the arm page saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// One write the PTY took.
    Took(Vec<u8>),
    /// A segment number published before its trigger.
    Published(u64),
}

/// What the agent does, at once, when the PTY takes a Ctrl+G (R-U2-01): it
/// reads the trigger and answers before the writer's report can reach the
/// segments.
type React = Box<dyn FnMut(&Rig) + Send>;

/// The PTY: takes everything, logs each write, and runs the agent's reaction
/// to a Ctrl+G inside the write, before the writer thread hears it went.
struct Pty {
    rig: Arc<std::sync::OnceLock<std::sync::Weak<Rig>>>,
}

impl std::io::Write for Pty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(rig) = self.rig.get().and_then(std::sync::Weak::upgrade) else {
            return Ok(buf.len());
        };
        rig.seen.lock().unwrap().push(Seen::Took(buf.to_vec()));
        if buf.contains(&CTRL_G) {
            let react = rig.react.lock().unwrap().take();
            if let Some(mut react) = react {
                react(&rig);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One generation on the real writer, input, hook and driver.
struct Rig {
    writer: Arc<AsyncMutex<PaneWriter>>,
    input: Arc<GenerationInput>,
    hook: Mutex<OutputHook>,
    seen: Mutex<Vec<Seen>>,
    react: Mutex<Option<React>>,
    ledger: Arc<IngressLedger>,
    retired: Arc<AtomicBool>,
}

impl Rig {
    /// A generation of `agent` whose output ran `prefix` before its
    /// segments attached (R-U2-02), with its driver running.
    fn new(agent: Agent, prefix: &[u8]) -> Arc<Rig> {
        let input = Arc::new(GenerationInput::default());
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
        let mut hook = OutputHook::new(Arc::clone(&input));
        hook.on_output(prefix);
        assert!(input.attach_segments(G, agent));
        let slot = Arc::new(std::sync::OnceLock::new());
        let retired = Arc::new(AtomicBool::new(false));
        let writer = PaneWriter::new(
            Box::new(Pty {
                rig: Arc::clone(&slot),
            }),
            None,
            Arc::new(Mutex::new(PaneInputState::default())),
            Arc::clone(&retired),
            Arc::new(AtomicBool::new(false)),
            None,
            Arc::clone(&input),
        );
        let rig = Arc::new(Rig {
            writer: Arc::new(AsyncMutex::new(writer)),
            input,
            hook: Mutex::new(hook),
            seen: Mutex::new(Vec::new()),
            react: Mutex::new(None),
            ledger: IngressLedger::new(),
            retired,
        });
        let _ = slot.set(Arc::downgrade(&rig));
        let log = Arc::downgrade(&rig);
        tokio::spawn(drive_segments(
            Arc::clone(&rig.writer),
            Arc::clone(&rig.input),
            Box::new(move |n| {
                if let Some(rig) = log.upgrade() {
                    rig.seen.lock().unwrap().push(Seen::Published(n));
                }
            }),
        ));
        rig
    }

    /// The bytes the PTY took, in order.
    fn taken(&self) -> Vec<u8> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| match s {
                Seen::Took(b) => Some(b.clone()),
                Seen::Published(_) => None,
            })
            .flatten()
            .collect()
    }

    /// A probe, admitted: its segment opened, its `0x07` queued with the
    /// gate's arm section closing its fence and raising its liability, and
    /// its written report given once the job ends, as `composer_probe`
    /// does. `before` runs between the job's end and that report (R-U2-01).
    async fn probe(&self, before: impl FnOnce(&Rig)) -> u64 {
        let now = Instant::now();
        let (number, job) = self.input.segment_open_probe(false, now).unwrap();
        let claim = Arc::new(crate::doorbell_txn::JobClaim::default());
        let w = self.writer.lock().await;
        let section = {
            let gate = Arc::clone(&w.gate.gate);
            let input = Arc::clone(&self.input);
            let claim = Arc::clone(&claim);
            move |ctx: &crate::doorbell_txn::StartCtx| {
                let mut at = (0, ctx.now);
                gate.arm_section(
                    &claim,
                    ProbeRefusal::Retired,
                    ProbeRefusal::TakenBack,
                    |_| {
                        input
                            .liability_raise(G, ctx.now)
                            .map(drop)
                            .map_err(|_| ProbeRefusal::Retired)
                    },
                    || {
                        at.0 = input.output_offset();
                        at.0
                    },
                )?;
                Ok(at)
            }
        };
        let probe = w.queue_probe(job, claim, section, || {}).unwrap();
        drop(w);
        match probe.end().await {
            ProbeEnd::Written { offset, at } => {
                before(self);
                self.input
                    .segment_written(number, offset, at, Instant::now())
                    .unwrap();
            }
            other => panic!("the probe wasn't written: {other:?}"),
        }
        number
    }

    async fn typed(&self, bytes: &[u8]) {
        let frame = UserFrame::new(bytes.to_vec(), Some(self.ledger.admit()), None);
        let accepted = self.writer.lock().await.accept(frame, None).await;
        let what = match accepted {
            Accepted::Held | Accepted::Written(Ok(())) => return,
            Accepted::Written(Err(e)) => format!("a failed write: {e}"),
            Accepted::Full(..) => "no room".to_string(),
            Accepted::Retired(_) => "retired".to_string(),
        };
        panic!("{bytes:?} wasn't accepted: {what}");
    }

    fn out(&self, bytes: &[u8]) {
        self.hook.lock().unwrap().on_output(bytes);
    }

    fn observe(&self, r: &MRecord) {
        let now = Instant::now();
        let _ = self.input.record_observe(
            &Observation {
                generation: G,
                key: key(r.pid),
                channel: r.channel,
                attribution: if r.owned {
                    Attribution::Owned
                } else {
                    Attribution::NotOwned(NotOwned::Tool)
                },
                claim: None,
                decision: r.decision,
                seg_seen: r.seg,
                native_seen: None,
                birth_floor: now,
            },
            now,
        );
    }

    fn editor(&self, pid: u32) {
        self.input.segment_editor_running(key(pid), Instant::now());
    }

    fn report(&self, pid: u32, report: Report) {
        let _ = self
            .input
            .record_report(G, key(pid), report, Instant::now());
    }

    fn exit(&self, pid: u32) {
        let now = Instant::now();
        let _ = self.input.record_exit(G, key(pid), now, now);
        let _ = self.input.record_drained(G, key(pid), now);
    }

    async fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.writer.lock().await.gate.retire();
        self.input.retire();
    }

    /// Wait for the driver and the writer to settle on `expected`: the PTY
    /// has taken that much, and no trigger's report is still on its way.
    /// Then a moment more, so a late release shows.
    async fn settle(&self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let submitted = self
                .input
                .segments_view(Instant::now())
                .iter()
                .any(|v| v.stage == Stage::Submitted);
            if !submitted && self.taken().len() >= expected.len() {
                break;
            }
            if Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
}

fn key(pid: u32) -> RecordKey {
    RecordKey { pid, start: 1 }
}

// ---------------------------------------------------------------------------
// The model: core §4.6's release table over the script alone.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MKind {
    Probe,
    HumanG,
}

/// A helper's record as the script made it.
#[derive(Debug, Clone)]
struct MRecord {
    pid: u32,
    owned: bool,
    channel: Channel,
    seg: Option<u64>,
    decision: Option<Decision>,
    report: Report,
    drained: bool,
    /// Its editor child was seen running (the observer's one report).
    editor: bool,
    matched: bool,
    /// When it was born and when it exited, on the model's own clock.
    born: u64,
    exited: Option<u64>,
    /// Its completion is no longer pending: a marker route completed it, or
    /// a later owned invocation subsumed it (A2 §3.2), after which its
    /// segment holds until retirement.
    completed: bool,
    subsumed: bool,
}

#[derive(Debug, Clone, Default)]
struct Watch {
    off: bool,
    on: bool,
    frame: bool,
}

#[derive(Debug, Clone)]
struct MSeg {
    kind: MKind,
    number: u64,
    /// The output offset its trigger was written at.
    written: u64,
    record: Option<usize>,
    released: bool,
    cut_off: bool,
    e1: Watch,
}

/// The release table's view of one generation.
#[derive(Debug, Clone)]
struct Model {
    agent: Agent,
    records: Vec<MRecord>,
    segs: VecDeque<MSeg>,
    last: u64,
    /// Typed bytes the gate holds, in order.
    held: VecDeque<u8>,
    /// What the PTY may have taken by now, in order.
    expected: Vec<u8>,
    /// Each HumanG trigger: its number.
    triggers: Vec<u64>,
    /// The writer scope opened for good (under `Hold`).
    scope_open: bool,
    /// Output bytes fed so far: the offset the next byte is at.
    fed: u64,
    retired: bool,
    /// The probe's settlement: `None` until it settles.
    settled: Option<bool>,
    stats: Stats,
    /// The model's clock: one tick per birth or exit.
    clock: u64,
}

/// How much of the table a run reached: a generator that never releases,
/// cuts or completes proves nothing.
#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    released: usize,
    passed: usize,
    cut_off: usize,
    completed: usize,
}

impl Model {
    fn new(agent: Agent, prefix: usize) -> Self {
        Self {
            agent,
            records: Vec::new(),
            segs: VecDeque::new(),
            last: 0,
            held: VecDeque::new(),
            expected: Vec::new(),
            triggers: Vec::new(),
            scope_open: false,
            fed: prefix as u64,
            retired: false,
            settled: None,
            stats: Stats::default(),
            clock: 0,
        }
    }

    /// Whether the gate holds: a segment is open or typing is held.
    fn fenced(&self) -> bool {
        !self.segs.is_empty() || !self.held.is_empty()
    }

    fn probe(&mut self) {
        self.last += 1;
        self.segs.push_back(MSeg {
            kind: MKind::Probe,
            number: self.last,
            written: self.fed,
            record: None,
            released: false,
            cut_off: false,
            e1: Watch::default(),
        });
        self.expected.push(CTRL_G);
        self.step();
    }

    fn typed(&mut self, bytes: &[u8]) {
        if self.retired {
            return;
        }
        if self.fenced() {
            self.held.extend(bytes);
        } else {
            self.expected.extend(bytes);
        }
        self.step();
    }

    fn out(&mut self, bytes: &[u8]) {
        for (i, chunk) in [OFF, ON, FRAME].iter().enumerate() {
            if bytes == *chunk {
                let start = self.fed;
                if let Some(seg) = self.segs.front_mut()
                    && start >= seg.written
                {
                    let w = &mut seg.e1;
                    match i {
                        0 => {
                            *w = Watch {
                                off: true,
                                ..Watch::default()
                            }
                        }
                        1 if w.off && !w.on => w.on = true,
                        2 if w.on && !w.frame => w.frame = true,
                        _ => {}
                    }
                }
            }
        }
        self.fed += bytes.len() as u64;
        self.step();
    }

    fn observe(&mut self, mut r: MRecord) {
        self.clock += 1;
        r.born = self.clock;
        if let Some(d) = r.decision
            && matches!(d, Decision::Unarmed | Decision::Tool | Decision::Unknown)
        {
            self.scope_open = true;
        }
        self.records.push(r);
        self.step();
    }

    fn editor(&mut self, pid: u32) {
        if let Some(r) = self.records.iter_mut().find(|r| r.pid == pid) {
            r.editor = true;
        }
        self.step();
    }

    fn report(&mut self, pid: u32, report: Report) {
        if let Some(r) = self.records.iter_mut().find(|r| r.pid == pid)
            && r.report == Report::None
        {
            r.report = report;
        }
        self.step();
    }

    fn exit(&mut self, pid: u32) {
        self.clock += 1;
        if let Some(r) = self.records.iter_mut().find(|r| r.pid == pid) {
            r.drained = true;
            r.exited = Some(self.clock);
            let editor_bearing = r.decision != Some(Decision::Armed);
            let ended = matches!(
                r.report,
                Report::Done(Tree::Clean) | Report::SpawnFailed { .. }
            );
            if r.decision.is_none() || (editor_bearing && !ended) {
                self.scope_open = true;
            }
        }
        self.step();
    }

    fn retire(&mut self) {
        self.retired = true;
        self.held.clear();
        self.segs.clear();
    }

    /// Match, release and complete what the script now allows, then what
    /// the held typing may do: core §4.5–§4.6 from the table.
    fn step(&mut self) {
        if self.retired {
            return;
        }
        // A2 §3.2: an exited, pending record whose editor is known to have
        // ended is subsumed by any other owned record born after its exit.
        for i in 0..self.records.len() {
            let r = &self.records[i];
            let ended = r.decision == Some(Decision::Armed)
                || matches!(
                    r.report,
                    Report::SpawnFailed { .. } | Report::Done(Tree::Clean | Tree::Survivors)
                );
            if let Some(exit) = r.exited
                && r.owned
                && ended
                && !r.completed
                && !r.subsumed
                && self
                    .records
                    .iter()
                    .any(|s| s.pid != r.pid && s.owned && s.born > exit)
            {
                self.records[i].subsumed = true;
            }
        }
        while let Some(seg) = self.segs.front() {
            let number = seg.number;
            if seg.record.is_none() {
                // The one owned, unmatched record that read this number.
                let candidates: Vec<usize> = (0..self.records.len())
                    .filter(|&i| {
                        let r = &self.records[i];
                        r.owned && !r.matched && r.seg == Some(number)
                    })
                    .collect();
                if let [i] = candidates[..] {
                    self.records[i].matched = true;
                    self.segs.front_mut().unwrap().record = Some(i);
                }
            }
            let seg = self.segs.front().unwrap();
            let Some(ri) = seg.record else {
                break;
            };
            let r = self.records[ri].clone();
            let spawn_failed = matches!(r.report, Report::SpawnFailed { .. });
            let pass = !spawn_failed
                && match (self.agent, seg.kind, r.decision) {
                    (Agent::Claude, MKind::Probe, Some(Decision::Armed | Decision::Unarmed)) => {
                        true
                    }
                    (Agent::Claude, MKind::HumanG, Some(Decision::Armed)) => true,
                    (_, MKind::HumanG, Some(Decision::Unarmed)) => r.editor,
                    _ => false,
                };
            let unknown_end = r.drained
                && r.decision != Some(Decision::Armed)
                && matches!(r.report, Report::None | Report::Unrecognised);
            let seg = self.segs.front_mut().unwrap();
            if pass && !seg.released {
                seg.released = true;
                self.stats.released += 1;
            }
            if seg.released && unknown_end && !seg.cut_off {
                seg.cut_off = true;
                self.stats.cut_off += 1;
            }
            // The probe's settlement (R3 §3.4): its matched decision, once
            // an `Armed` one's read is known or its record drained.
            if seg.kind == MKind::Probe
                && self.settled.is_none()
                && let Some(d) = r.decision
                && (d != Decision::Armed || r.drained)
            {
                self.settled = Some(d == Decision::Armed);
            }
            let helper_only = r.decision == Some(Decision::Armed) || spawn_failed;
            let returned = match self.agent {
                Agent::Claude => seg.e1.on,
                Agent::Codex => seg.e1.frame,
            };
            if r.drained && helper_only && returned && !self.scope_open && !r.subsumed {
                self.records[ri].completed = true;
                self.segs.pop_front();
                self.stats.completed += 1;
                continue;
            }
            break;
        }
        // What the held typing may do now.
        let candidate = self.held.iter().position(|&b| b == CTRL_G);
        match self.segs.front() {
            None => {
                let upto = candidate.unwrap_or(self.held.len());
                self.expected.extend(self.held.drain(..upto));
                if candidate.is_none() {
                    return;
                }
                // The candidate alone, as the next segment's trigger.
                self.held.pop_front();
                self.expected.push(CTRL_G);
                self.last += 1;
                self.triggers.push(self.last);
                self.segs.push_back(MSeg {
                    kind: MKind::HumanG,
                    number: self.last,
                    written: self.fed,
                    record: None,
                    released: false,
                    cut_off: false,
                    e1: Watch::default(),
                });
            }
            Some(seg) if seg.released && !seg.cut_off => {
                let upto = candidate.unwrap_or(self.held.len());
                if upto > 0 {
                    self.stats.passed += 1;
                }
                self.expected.extend(self.held.drain(..upto));
            }
            Some(_) => {}
        }
    }

    /// The number the next helper should read to match the front segment.
    fn front(&self) -> Option<u64> {
        self.segs.front().map(|s| s.number)
    }
}

/// The checks after every step: the PTY took exactly what the model allows,
/// each Ctrl+G alone and after its number; the schedule is due while the
/// probe is unsettled, and not due as the step that settles it ends (later
/// typing moves the epoch, which may make it due again).
fn check(rig: &Rig, model: &Model, settled_before: Option<bool>, what: &str) {
    let taken = rig.taken();
    assert_eq!(
        String::from_utf8_lossy(&taken),
        String::from_utf8_lossy(&model.expected),
        "{what}: the PTY took {} bytes where the release table allows {}\n\
         segments {:#?}\ngate {:?}\nmodel {:#?}",
        taken.len(),
        model.expected.len(),
        rig.input.segments_view(Instant::now()),
        rig.input.segments_front(),
        model.segs,
    );
    let seen = rig.seen.lock().unwrap().clone();
    let mut published = Vec::new();
    let mut triggers = Vec::new();
    for s in &seen {
        match s {
            Seen::Published(n) => published.push(*n),
            Seen::Took(b) if b.contains(&CTRL_G) => {
                assert_eq!(b, &[CTRL_G], "{what}: a Ctrl+G went with other bytes");
                triggers.push(published.last().copied());
            }
            Seen::Took(_) => {}
        }
    }
    // The first Ctrl+G is the probe's own; each after it is a trigger,
    // written after its own number was published.
    let humang: Vec<_> = triggers.iter().skip(1).copied().collect();
    let want: Vec<_> = model.triggers.iter().map(|&n| Some(n)).collect();
    assert_eq!(humang, want, "{what}: each trigger after its own number");
    if model.retired {
        return;
    }
    match (settled_before, model.settled) {
        (_, None) => assert!(
            rig.input.probe_due(Instant::now()),
            "{what}: the probe is unsettled, so its schedule is as it was"
        ),
        (None, Some(armed)) => assert!(
            !rig.input.probe_due(Instant::now()),
            "{what}: the probe settled (armed {armed}), so it isn't due again yet"
        ),
        (Some(_), Some(_)) => {}
    }
}

// ---------------------------------------------------------------------------
// The script.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Step {
    Type(Vec<u8>),
    Observe(MRecord),
    Editor(u32),
    Report(u32, Report),
    Exit(u32),
    Out(&'static [u8]),
    /// The agent answers the next trigger inside its write: its return, and
    /// when `helper` is set a helper that reads its number, decides
    /// `Unarmed` and runs the user's editor (R-U2-01).
    Fast {
        helper: Option<u32>,
    },
    Retire,
}

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

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A script of `len` steps, each chosen against the model as it stands, so
/// that its records mostly target the open segment (positive rows) and
/// sometimes a stale number or another owner (negatives).
fn generate(rng: &mut Rng, model: &mut Model, len: usize) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut next_pid = 100;
    let mut pending = None;
    // How often a helper opens the user's editor (or a tool's): either
    // opens the writer scope for good under `Hold`, after which nothing
    // completes, so some scripts keep to `Armed` helpers.
    let editor_rate: u64 = [0, 15, 45, 70][rng.below(4) as usize];
    for _ in 0..len {
        if model.retired {
            break;
        }
        let pick = rng.below(100);
        if pick < 12
            && let Some(front) = model.front()
        {
            // The front segment's own helper decides `Armed`, exits, and
            // the agent returns: a positive control for completion.
            next_pid += 1;
            let mut batch = vec![
                helper(next_pid, front, Decision::Armed),
                Step::Exit(next_pid),
            ];
            batch.extend(fast_return(model.agent).into_iter().map(Step::Out));
            // The return before the exit, or after it.
            if rng.chance(50) {
                let exit = batch.remove(1);
                batch.push(exit);
            }
            for step in batch {
                apply_model(model, &step, &mut pending);
                steps.push(step);
            }
            continue;
        }
        let step = if pick < 30 {
            let n = 1 + rng.below(4) as usize;
            let mut bytes: Vec<u8> = (0..n).map(|_| b'a' + rng.below(26) as u8).collect();
            // At most one candidate per frame, and none with the fence open.
            if model.fenced() && rng.chance(35) {
                let at = rng.below(n as u64 + 1) as usize;
                bytes.insert(at, CTRL_G);
            }
            Step::Type(bytes)
        } else if pick < 52 {
            next_pid += 1;
            let front = model.front();
            let seg = match rng.below(10) {
                0 => front.map(|n| n.saturating_sub(1)),
                1 => Some(999),
                _ => front,
            };
            let decision = match rng.below(100) {
                n if n < editor_rate.saturating_sub(5) => Decision::Unarmed,
                n if n < editor_rate => Decision::Tool,
                _ => Decision::Armed,
            };
            Step::Observe(MRecord {
                pid: next_pid,
                owned: !rng.chance(10),
                channel: if rng.chance(50) {
                    Channel::Ipc
                } else {
                    Channel::Marker
                },
                seg,
                decision: Some(decision),
                report: Report::None,
                drained: false,
                editor: false,
                matched: false,
                born: 0,
                exited: None,
                completed: false,
                subsumed: false,
            })
        } else if pick < 60 {
            let waiting: Vec<u32> = model
                .records
                .iter()
                .filter(|r| r.decision == Some(Decision::Unarmed) && !r.drained && !r.editor)
                .map(|r| r.pid)
                .collect();
            match waiting.last() {
                Some(&pid) => Step::Editor(pid),
                None => continue,
            }
        } else if pick < 67 {
            let open: Vec<u32> = model
                .records
                .iter()
                .filter(|r| !r.drained && r.report == Report::None)
                .map(|r| r.pid)
                .collect();
            match open.last() {
                Some(&pid) => {
                    let report = match rng.below(3) {
                        0 => Report::SpawnFailed { errno: Some(2) },
                        _ => Report::Done(Tree::Clean),
                    };
                    Step::Report(pid, report)
                }
                None => continue,
            }
        } else if pick < 77 {
            let open: Vec<u32> = model
                .records
                .iter()
                .filter(|r| !r.drained)
                .map(|r| r.pid)
                .collect();
            if open.is_empty() {
                continue;
            }
            Step::Exit(open[rng.below(open.len() as u64) as usize])
        } else if pick < 95 {
            Step::Out(match rng.below(4) {
                0 => OFF,
                1 => ON,
                2 => FRAME,
                _ => b"text",
            })
        } else if pick < 98 {
            next_pid += 1;
            Step::Fast {
                helper: rng.chance(50).then_some(next_pid),
            }
        } else {
            Step::Retire
        };
        apply_model(model, &step, &mut pending);
        steps.push(step);
    }
    steps
}

/// The model's side of `step`. A `Fast` step waits for the next trigger:
/// `pending` carries it until one is cut.
fn apply_model(model: &mut Model, step: &Step, pending: &mut Option<Option<u32>>) {
    let before = model.triggers.len();
    match step {
        Step::Type(b) => model.typed(b),
        Step::Observe(r) => model.observe(r.clone()),
        Step::Editor(pid) => model.editor(*pid),
        Step::Report(pid, report) => model.report(*pid, *report),
        Step::Exit(pid) => model.exit(*pid),
        Step::Out(b) => model.out(b),
        Step::Fast { helper } => *pending = Some(*helper),
        Step::Retire => model.retire(),
    }
    fast_answers(model, before, pending);
}

/// A trigger cut while a `Fast` answer waits: the agent's return and helper,
/// as the PTY's reaction runs them.
fn fast_answers(model: &mut Model, before: usize, pending: &mut Option<Option<u32>>) {
    if model.triggers.len() > before
        && let Some(helper) = pending.take()
    {
        let number = *model.triggers.last().unwrap();
        if let Some(pid) = helper {
            model.observe(fast_helper(pid, number));
            model.editor(pid);
        }
        for m in fast_return(model.agent) {
            model.out(m);
        }
    }
}

fn fast_helper(pid: u32, number: u64) -> MRecord {
    MRecord {
        pid,
        owned: true,
        channel: Channel::Ipc,
        seg: Some(number),
        decision: Some(Decision::Unarmed),
        report: Report::None,
        drained: false,
        editor: false,
        matched: false,
        born: 0,
        exited: None,
        completed: false,
        subsumed: false,
    }
}

fn fast_return(agent: Agent) -> Vec<&'static [u8]> {
    match agent {
        Agent::Claude => vec![OFF, ON],
        Agent::Codex => vec![OFF, ON, FRAME],
    }
}

/// Run `steps` on a fresh rig and model, checking after each.
async fn run(agent: Agent, prefix: &[u8], fast_probe: bool, steps: &[Step], what: &str) -> Model {
    if std::env::var_os("DAD_ORACLE_TRACE").is_some() {
        eprintln!("{what}: fast probe {fast_probe}, prefix {prefix:?}");
        for (i, step) in steps.iter().enumerate() {
            eprintln!("  {i}: {step:?}");
        }
    }
    let rig = Rig::new(agent, prefix);
    let mut model = Model::new(agent, prefix.len());
    let mut pending: Option<Option<u32>> = None;
    // The probe; with `fast_probe`, Claude's return is read before its
    // written report (R-U2-01).
    let return_first = fast_return(agent);
    rig.probe(|rig| {
        if fast_probe {
            for m in &return_first {
                rig.out(m);
            }
        }
    })
    .await;
    model.probe();
    if fast_probe {
        for m in &return_first {
            model.out(m);
        }
    }
    rig.settle(&model.expected).await;
    check(&rig, &model, None, &format!("{what}: the probe"));
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Type(b) => rig.typed(b).await,
            Step::Observe(r) => rig.observe(r),
            Step::Editor(pid) => rig.editor(*pid),
            Step::Report(pid, report) => rig.report(*pid, *report),
            Step::Exit(pid) => rig.exit(*pid),
            Step::Out(b) => rig.out(b),
            Step::Fast { helper } => {
                let helper = *helper;
                let answer = fast_return(agent);
                *rig.react.lock().unwrap() = Some(Box::new(move |rig: &Rig| {
                    let number = rig
                        .seen
                        .lock()
                        .unwrap()
                        .iter()
                        .rev()
                        .find_map(|s| match s {
                            Seen::Published(n) => Some(*n),
                            Seen::Took(_) => None,
                        })
                        .unwrap();
                    if let Some(pid) = helper {
                        rig.observe(&fast_helper(pid, number));
                        rig.editor(pid);
                    }
                    for m in &answer {
                        rig.out(m);
                    }
                }));
            }
            Step::Retire => rig.retire().await,
        }
        let settled = model.settled;
        apply_model(&mut model, step, &mut pending);
        rig.settle(&model.expected).await;
        check(&rig, &model, settled, &format!("{what}: step {i} {step:?}"));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    check(&rig, &model, model.settled, &format!("{what}: at rest"));
    model
}

/// Scenario: Generate scripts against the release table, one generation each, Claude and Codex: a probe whose fence closes in the gate's arm section, then typing with and without a Ctrl+G, helpers that read the open segment's number, a stale one or none, decide `Armed`, `Unarmed` or a tool, run the user's editor, report, and exit, return markers, an agent that answers a trigger before its written report arrives, and retirement, after output that ran before the segments attached. After every step, once the production driver has had its chance, the PTY has taken exactly what the table allows: nothing early, nothing missing, each Ctrl+G alone after its number, nothing after retirement, and the probe's schedule stops being due exactly when its decision settles.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_releases_follow_the_release_table_on_the_production_driver() {
    let mut reached = Stats::default();
    let (mut triggers, mut settled, mut retired) = (0, 0, 0);
    for seed in 1..=200u64 {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
        let agent = if seed % 2 == 0 {
            Agent::Codex
        } else {
            Agent::Claude
        };
        let prefix_len = rng.below(64) as usize;
        let mut prefix: Vec<u8> = (0..prefix_len).map(|_| b'x').collect();
        // Output that ends part-way through an escape (R-U2-02).
        if rng.chance(50) {
            prefix.extend_from_slice(b"\x1b[?20");
        }
        let fast_probe = rng.chance(30);
        let mut model = Model::new(agent, prefix.len());
        model.probe();
        if fast_probe {
            for m in fast_return(agent) {
                model.out(m);
            }
        }
        let steps = generate(&mut rng, &mut model, 40);
        let what = format!("seed {seed} {agent:?}");
        let m = run(agent, &prefix, fast_probe, &steps, &what).await;
        reached.released += m.stats.released;
        reached.passed += m.stats.passed;
        reached.cut_off += m.stats.cut_off;
        reached.completed += m.stats.completed;
        triggers += m.triggers.len();
        settled += usize::from(m.settled.is_some());
        retired += usize::from(m.retired);
    }
    // The scripts reached every kind of step the table has.
    eprintln!(
        "reached: {reached:?}, {triggers} triggers, {settled} settlements, {retired} retirements"
    );
    for (what, n) in [
        ("releases", reached.released),
        ("bytes passed to a released segment", reached.passed),
        ("unknown endings cutting a release off", reached.cut_off),
        ("completions", reached.completed),
        ("triggers", triggers),
        ("probe settlements", settled),
        ("retirements", retired),
    ] {
        assert!(n >= 5, "only {n} {what} across the seeds");
    }
}

/// An owned helper `pid` that read `seg` and decided `decision`.
fn helper(pid: u32, seg: u64, decision: Decision) -> Step {
    Step::Observe(MRecord {
        pid,
        owned: true,
        channel: Channel::Ipc,
        seg: Some(seg),
        decision: Some(decision),
        report: Report::None,
        drained: false,
        editor: false,
        matched: false,
        born: 0,
        exited: None,
        completed: false,
        subsumed: false,
    })
}

/// Scenario: The Q35-20 review's six traces as fixed scripts on the production driver, each judged by the release table: a return read before a probe's written report and a trigger's (R-U2-01), output that ran before the segments attached and ended inside an escape (R-U2-02), typing after a release to a running editor with nothing else moving (R-U2-03), a probe's `Armed` decision whose read never comes while its return and exit complete it (R-U2-05), and an unknown wait after a release to a running editor, with typing before and after it (R-U2-06).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_the_review_traces_follow_the_release_table() {
    for agent in [Agent::Claude, Agent::Codex] {
        let ret: Vec<Step> = fast_return(agent).into_iter().map(Step::Out).collect();
        // R-U2-01 and 02: early output, then a probe answered before its
        // report, completing; a held Ctrl+G's trigger answered inside its
        // write by a helper and its editor.
        let u01 = vec![
            Step::Type(b"ab\x07cd".to_vec()),
            Step::Fast { helper: Some(12) },
            helper(11, 1, Decision::Armed),
            Step::Exit(11),
            Step::Type(b"ef".to_vec()),
        ];
        run(
            agent,
            b"xxxx\x1b[?20",
            true,
            &u01,
            &format!("R-U2-01/02 {agent:?}"),
        )
        .await;

        // R-U2-03 and 06: a held Ctrl+G released to the user's running
        // editor; typing after it goes on to the editor; once its helper
        // drains with no report, what is still held stays.
        let mut u03 = vec![
            Step::Type(b"\x07".to_vec()),
            helper(21, 1, Decision::Armed),
            Step::Exit(21),
        ];
        u03.extend(ret.clone());
        u03.extend([
            helper(22, 2, Decision::Unarmed),
            Step::Editor(22),
            Step::Type(b"gh".to_vec()),
            Step::Type(b"ij".to_vec()),
            Step::Exit(22),
            Step::Type(b"kl".to_vec()),
        ]);
        run(agent, b"", false, &u03, &format!("R-U2-03/06 {agent:?}")).await;

        // R-U2-05: `Armed` on a marker, no read, then the return before the
        // exit: the settlement still runs.
        let mut u05 = vec![Step::Observe(MRecord {
            pid: 31,
            owned: true,
            channel: Channel::Marker,
            seg: Some(1),
            decision: Some(Decision::Armed),
            report: Report::None,
            drained: false,
            editor: false,
            matched: false,
            born: 0,
            exited: None,
            completed: false,
            subsumed: false,
        })];
        u05.extend(ret.clone());
        u05.push(Step::Exit(31));
        run(agent, b"", false, &u05, &format!("R-U2-05 {agent:?}")).await;
    }
}

/// R-U2-04's model: paste pairs and the records that account for them.
mod pairs {
    use super::*;

    /// One unit of a script: an editor session the deck saw (its record and
    /// its pair, in either order), or a pair no record accounts for.
    #[derive(Debug, Clone, Copy)]
    enum Unit {
        Session { pair_first: bool },
        Foreign,
    }

    /// Scenario: Generate sequences of editor sessions the deck saw (a helper's record and its bracketed-paste pair, in either order) and pairs no record accounts for, on a generation's real output hook and input. A foreign pair always moves the epoch as an external edit, once; a session whose record came first moves nothing; one whose pair came first moves it once and lends nothing to a later pair (R-U2-04).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_paste_pairs_follow_their_records() {
        for seed in 1..=200u64 {
            let mut rng = Rng(0xd1b5_4a32_d192_ed03 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let input = Arc::new(GenerationInput::default());
            assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
            assert!(input.attach_segments(G, Agent::Claude));
            let mut hook = OutputHook::new(Arc::clone(&input));
            let mut want = 0u64;
            let mut pid = 0;
            let units: Vec<Unit> = (0..12)
                .map(|_| match rng.below(3) {
                    0 => Unit::Foreign,
                    _ => Unit::Session {
                        pair_first: rng.chance(50),
                    },
                })
                .collect();
            for unit in &units {
                let pair = [OFF, ON].concat();
                match *unit {
                    Unit::Foreign => {
                        hook.on_output(&pair);
                        want += 1;
                    }
                    Unit::Session { pair_first } => {
                        pid += 1;
                        // Born before its own pair ends, after every earlier
                        // pair.
                        let born = Instant::now();
                        let record = Observation {
                            generation: G,
                            key: key(pid),
                            channel: Channel::Ipc,
                            attribution: Attribution::Owned,
                            claim: None,
                            decision: Some(Decision::Unarmed),
                            seg_seen: None,
                            native_seen: None,
                            birth_floor: born,
                        };
                        if pair_first {
                            hook.on_output(&pair);
                            input.record_observe(&record, Instant::now()).unwrap();
                            want += 1;
                        } else {
                            input.record_observe(&record, Instant::now()).unwrap();
                            hook.on_output(&pair);
                        }
                    }
                }
                assert_eq!(
                    input.epoch().last_foreign(),
                    want,
                    "seed {seed}: after {unit:?} in {units:?}"
                );
            }
        }
    }
}
