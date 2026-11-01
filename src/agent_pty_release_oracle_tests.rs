//! PRD #1666 part 3 (Q35-20b, plan §3.7, X-3): the release-authority oracle,
//! composed with the production driver (`inbox/probe/010`).
//!
//! **What runs is production code.** A real [`PaneWriter`] over a scripted
//! PTY, its gate and writer thread; the generation's real input
//! ([`GenerationInput`]: records, segments, epoch, schedule) and its real
//! output hook ([`OutputHook`]), fed from before the segments attach; the
//! production release driver ([`super::segment_driver::drive_segments`]) as
//! its own task, woken as production wakes it; a probe queued through
//! [`PaneWriter::queue_probe`] with production's arm section
//! ([`super::segment_driver::probe_section`]) closing its fence, and its
//! job's end reported to its segment by production's
//! [`super::segment_driver::end_probe_segment`]. What the observer would see
//! (a helper's records, the daemon's read of an `Armed` export, its editor
//! running, its report and exit) reaches the input through the same calls
//! the observer makes. Only admission is left out: every probe here is
//! admitted, and the section raises the liability as `arm_segment` does.
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
//! own generated model below ([`pairs`]). Each trace also has a fixed script,
//! and so does R-B-U2-02's: a drain that lands inside a driver pass, between
//! its read of the front segment and its extraction of what is held, forced
//! there by a test-only seam on the writer.
//!
//! **The policy it assumes (the audit's U3-2).** Every generation here runs
//! [`EcPolicy::Hold`], production's policy, under which `Records::complete`
//! refuses the EC route: the model has no EC or frontier rows, and no
//! Authority-policy EC completion is exercised (R-B-U3-02). If the policy
//! changes (Q76), the model needs those rows and `Records::complete`'s EC
//! route before this oracle says anything about it.
//!
//! **Where its power ends.** It is serial: each step settles before the
//! next, so a race between the observer and a driver pass in flight is found
//! only where a fixed script forces it. Its floors require a selection of the
//! release table's tuples ([`ROWS`], 14 when written), each settlement kind
//! and each kind of step, not every combination and not the whole table: no
//! Claude failed-start tuple and no tuple without a decision is required,
//! and a tuple's key does not tell an ending by a drain or an unknown wait
//! from a running editor (the cut-off has its own floor). Each is counted
//! once per segment that reached it, not per evaluation. Not exercised here, each its own obligation
//! and not evidence that it passed (R-B-U3-03): admission (its own tests and
//! S-20's `e2e_probe_admission`), the observer's `/proc` scan and editor
//! identity, more than one probe in a script, other Ctrl+G forms and
//! multi-candidate frames (the planner's own oracle, Q35-16), Codex timing
//! beyond its markers (M-PF1), and the planner's cut-open path (`hold_cut`
//! and `cut_open`, typing while a liability is out and no check is open),
//! which the accounting oracle covers and the activation list carries.
//! Linux only, as the driver is.

use super::segment_driver::{
    ProbeEnd, ProbeRefusal, drive_segments, end_probe_segment, probe_section,
};
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
    /// How many mid-pass facts fired inside a driver pass (R-B-U2-02).
    mid_fired: std::sync::atomic::AtomicUsize,
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
            mid_fired: std::sync::atomic::AtomicUsize::new(0),
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

    /// A probe, admitted: its segment opened, its `0x07` queued with
    /// production's arm section ([`probe_section`]) closing its fence, and
    /// its job's end reported to its segment as `composer_probe` reports it
    /// ([`end_probe_segment`]). Admission is left out: at the section, the
    /// liability is raised as the observer's `arm_segment` raises it.
    /// `before` runs between the job's end and that report (R-U2-01).
    async fn probe(&self, before: impl FnOnce(&Rig)) -> u64 {
        let now = Instant::now();
        let (number, job) = self.input.segment_open_probe(false, now).unwrap();
        let claim = Arc::new(crate::doorbell_txn::JobClaim::default());
        let w = self.writer.lock().await;
        let section = {
            let input = Arc::clone(&self.input);
            probe_section(
                Arc::clone(&w.gate.gate),
                Arc::clone(&self.input),
                Arc::clone(&claim),
                || true,
                move |ctx, _, _| {
                    input
                        .liability_raise(G, ctx.now)
                        .map(drop)
                        .map_err(|_| ProbeRefusal::Retired)
                },
            )
        };
        let probe = w.queue_probe(job, claim, section, || {}).unwrap();
        drop(w);
        let end = probe.end().await;
        assert!(
            matches!(end, ProbeEnd::Written { .. }),
            "the probe wasn't written: {end:?}"
        );
        before(self);
        end_probe_segment(&self.input, number, end);
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
        // The daemon's read of an `Armed` export arrives with the helper's
        // invocation on IPC, before its record (as `observe.rs` notes it).
        if let (Channel::Ipc, Some(Decision::Armed), Some(empty)) = (r.channel, r.decision, r.read)
        {
            self.input.note_read(key(r.pid), empty);
        }
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

    /// Arm helper `pid`'s drain with no report to land inside the next
    /// driver pass that finds typing held, between its read of the front
    /// segment and its extraction of what is held (R-B-U2-02).
    async fn exit_mid_pass(self: &Arc<Self>, pid: u32) {
        let weak = Arc::downgrade(self);
        let w = self.writer.lock().await;
        let gate = w.gate();
        w.set_mid_pass(Box::new(move || {
            if !gate.gate.holds_any(|_| true) {
                return false;
            }
            if let Some(rig) = weak.upgrade() {
                rig.exit(pid);
                rig.mid_fired.fetch_add(1, Ordering::SeqCst);
            }
            true
        }));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// The daemon's read of an `Armed` export on IPC: whether it was empty.
    read: Option<bool>,
    drained: bool,
    /// Its editor child was seen running (the observer's one report).
    editor: bool,
    matched: bool,
    /// When it was born and when it exited, on the model's own clock.
    born: u64,
    exited: Option<u64>,
    /// Its completion is no longer pending: a marker route completed it, or
    /// a later owned invocation subsumed it (A2 §3.1, as `record.rs`'s
    /// "Completion" section states it: an `Owned` record born after its
    /// exit, its editor known to have ended), after which its segment holds
    /// until retirement (`segments.rs`'s header).
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
    /// The release-table rows it reached ([`Stats::rows`]), once each.
    rows: Vec<Row>,
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
    /// The writer scope opened for good (A3 §4.3, A5 §1.4, A6 §1.3, as
    /// `record.rs`'s "writer scope" section states it): a decision that
    /// isn't helper-only (`Unarmed`, a tool, `Unknown`), a decisionless exit
    /// drain, or an editor-bearing exit with no clean report. Its cause is
    /// sticky until retirement, so under `Hold` nothing completes after it.
    scope_open: bool,
    /// Output bytes fed so far: the offset the next byte is at.
    fed: u64,
    retired: bool,
    /// The probe's settlement: `None` until it settles.
    settled: Option<Settled>,
    stats: Stats,
    /// The model's clock: one tick per birth or exit.
    clock: u64,
}

/// How a probe settled the schedule (R3 §3.4): an `Armed` empty read of
/// the deck's own claim; another `Armed`; anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Settled {
    Empty,
    NonEmpty,
    Failed,
}

/// One row of the release table a matched segment reached (R-B-U3-02):
/// the agent, the segment's kind, its record's decision, whether its editor
/// was seen running and whether its start failed, and whether the row
/// released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    agent: &'static str,
    kind: MKind,
    decision: &'static str,
    editor: bool,
    spawn_failed: bool,
    releases: bool,
}

/// How much of the table a run reached: a generator that never releases,
/// cuts or completes proves nothing.
#[derive(Debug, Clone, Default)]
struct Stats {
    released: usize,
    passed: usize,
    cut_off: usize,
    completed: usize,
    /// Each row reached, and by how many segments.
    rows: std::collections::BTreeMap<Row, usize>,
    /// Each kind of settlement reached.
    settled: std::collections::BTreeMap<Settled, usize>,
}

impl Stats {
    fn add(&mut self, other: &Stats) {
        self.released += other.released;
        self.passed += other.passed;
        self.cut_off += other.cut_off;
        self.completed += other.completed;
        for (row, n) in &other.rows {
            *self.rows.entry(*row).or_default() += n;
        }
        for (kind, n) in &other.settled {
            *self.settled.entry(*kind).or_default() += n;
        }
    }
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
            rows: Vec::new(),
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
        // A2 §3.1 (`record.rs`, "Completion"): an exited, pending record
        // whose editor is known to have ended is subsumed by any other owned
        // record born after its exit.
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
            let row = Row {
                agent: agent_name(self.agent),
                kind: seg.kind,
                decision: decision_name(r.decision),
                editor: r.editor,
                spawn_failed,
                releases: pass,
            };
            let seg = self.segs.front_mut().unwrap();
            if !seg.rows.contains(&row) {
                seg.rows.push(row);
                *self.stats.rows.entry(row).or_default() += 1;
            }
            if pass && !seg.released {
                seg.released = true;
                self.stats.released += 1;
            }
            if seg.released && unknown_end && !seg.cut_off {
                seg.cut_off = true;
                self.stats.cut_off += 1;
            }
            // The probe's settlement (R3 §3.4): its matched decision, once
            // an `Armed` one's read is known or its record drained. Every
            // matched record is owned with no other claim, so an empty read
            // is the deck's own.
            if seg.kind == MKind::Probe
                && self.settled.is_none()
                && let Some(d) = r.decision
                && (d != Decision::Armed || r.drained || r.read.is_some())
            {
                let kind = match (d, r.read) {
                    (Decision::Armed, Some(true)) => Settled::Empty,
                    (Decision::Armed, _) => Settled::NonEmpty,
                    _ => Settled::Failed,
                };
                self.settled = Some(kind);
                *self.stats.settled.entry(kind).or_default() += 1;
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
                    rows: Vec::new(),
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

fn agent_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude",
        Agent::Codex => "Codex",
    }
}

fn decision_name(decision: Option<Decision>) -> &'static str {
    match decision {
        None => "none",
        Some(Decision::Armed) => "Armed",
        Some(Decision::Unarmed) => "Unarmed",
        Some(Decision::Tool) => "Tool",
        Some(Decision::Unknown) => "Unknown",
    }
}

/// The checks after every step: the PTY took exactly what the model allows,
/// each Ctrl+G alone and after its number; the schedule is due while the
/// probe is unsettled, and as the step that settles it ends, shows how it
/// settled (U3-5): an empty read makes it due again at once, a non-empty one
/// waits with no failure counted, a failed check waits as a retry. Later
/// steps can move the epoch, which may make it due again.
fn check(rig: &Rig, model: &Model, settled_before: Option<Settled>, what: &str) {
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
        (None, Some(kind)) => {
            let due = rig.input.probe_due(Instant::now());
            let retrying = rig.input.probe_retrying();
            let want = match kind {
                Settled::Empty => (true, false),
                Settled::NonEmpty => (false, false),
                Settled::Failed => (false, true),
            };
            assert_eq!(
                (due, retrying),
                want,
                "{what}: the probe settled {kind:?}: (due, retrying)"
            );
        }
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
    /// Helper `pid` drains with no report inside the next driver pass that
    /// finds typing held, after the pass read the front segment and before
    /// it takes what is held (R-B-U2-02). For the model, it drains now.
    ExitMidPass(u32),
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
    // Most scripts open with a directed prelude, since a random script
    // seldom completes its probe before an editor opens the writer scope:
    // a Ctrl+G held behind the probe, the probe's own `Armed` helper and
    // return completing it, so the Ctrl+G becomes the next segment's
    // trigger, and then that trigger's own helper, deciding `Armed`,
    // `Unarmed` or a tool, its editor seen running or not, its start failed
    // or not. Others open with the probe's own helper deciding, for the
    // probe's rows and its settlements. The model judges every step of
    // either as any other.
    let prelude = rng.below(100);
    if prelude < 25
        && let Some(probe) = model.front()
    {
        next_pid += 1;
        let decision = [Decision::Armed, Decision::Unarmed, Decision::Tool][rng.below(3) as usize];
        let mut own = helper(next_pid, probe, decision);
        if let Step::Observe(r) = &mut own {
            r.read = (decision == Decision::Armed)
                .then(|| daemon_read(rng))
                .flatten();
        }
        let mut directed = vec![own];
        if decision == Decision::Unarmed && rng.chance(50) {
            directed.push(Step::Editor(next_pid));
        }
        for step in directed {
            apply_model(model, &step, &mut pending);
            steps.push(step);
        }
    } else if prelude < 70
        && let Some(probe) = model.front()
    {
        next_pid += 1;
        let mut own = helper(next_pid, probe, Decision::Armed);
        if let Step::Observe(r) = &mut own {
            r.read = daemon_read(rng);
        }
        let mut prelude = vec![
            Step::Type(vec![b'a', CTRL_G, b'b']),
            own,
            Step::Exit(next_pid),
        ];
        prelude.extend(fast_return(model.agent).into_iter().map(Step::Out));
        for step in prelude {
            apply_model(model, &step, &mut pending);
            steps.push(step);
        }
        if let Some(trigger) = model.front() {
            next_pid += 1;
            let decision = [
                Decision::Armed,
                Decision::Unarmed,
                Decision::Unarmed,
                Decision::Tool,
            ][rng.below(4) as usize];
            let mut directed = vec![helper(next_pid, trigger, decision)];
            if decision == Decision::Unarmed && rng.chance(60) {
                directed.push(Step::Editor(next_pid));
            }
            if rng.chance(25) {
                directed.push(Step::Report(
                    next_pid,
                    Report::SpawnFailed { errno: Some(2) },
                ));
            }
            directed.push(Step::Type(b"cd".to_vec()));
            for step in directed {
                apply_model(model, &step, &mut pending);
                steps.push(step);
            }
        }
    }
    for i in 0..len {
        if model.retired {
            break;
        }
        // Retirement only in a script's last quarter, so most scripts run
        // long enough to reach a held Ctrl+G's segment.
        let pick = if i < len * 3 / 4 {
            rng.below(98)
        } else {
            rng.below(100)
        };
        if pick < 12
            && let Some(front) = model.front()
        {
            // The front segment's own helper decides `Armed`, exits, and
            // the agent returns: a positive control for completion.
            next_pid += 1;
            let mut own = helper(next_pid, front, Decision::Armed);
            if let Step::Observe(r) = &mut own {
                r.read = daemon_read(rng);
            }
            let mut batch = vec![own, Step::Exit(next_pid)];
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
            // At most one candidate per frame, and only while the gate holds
            // (a segment open or typing held): a Ctrl+G typed with nothing
            // out is native and written whole (the model's first
            // correction), which this generator doesn't script.
            if model.fenced() && rng.chance(50) {
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
            let channel = if rng.chance(50) {
                Channel::Ipc
            } else {
                Channel::Marker
            };
            Step::Observe(MRecord {
                pid: next_pid,
                owned: !rng.chance(10),
                channel,
                seg,
                decision: Some(decision),
                report: Report::None,
                read: (channel == Channel::Ipc && decision == Decision::Armed)
                    .then(|| daemon_read(rng))
                    .flatten(),
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
            // Half the time, the released front segment's own helper drains
            // with no report while its editor may still run: the unknown
            // wait that cuts the release off (R-U2-06), generated directly.
            let unknown = model
                .segs
                .front()
                .filter(|s| s.released && !s.cut_off)
                .and_then(|s| s.record)
                .map(|i| &model.records[i])
                .filter(|r| {
                    !r.drained && r.report == Report::None && r.decision != Some(Decision::Armed)
                })
                .map(|r| r.pid);
            let open: Vec<u32> = model
                .records
                .iter()
                .filter(|r| !r.drained)
                .map(|r| r.pid)
                .collect();
            match unknown {
                Some(pid) if rng.chance(50) => Step::Exit(pid),
                _ if open.is_empty() => continue,
                _ => Step::Exit(open[rng.below(open.len() as u64) as usize]),
            }
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

/// The daemon's read of an `Armed` export on IPC, when it arrives: empty
/// or not.
fn daemon_read(rng: &mut Rng) -> Option<bool> {
    rng.chance(70).then(|| rng.chance(50))
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
        Step::Exit(pid) | Step::ExitMidPass(pid) => model.exit(*pid),
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
        read: None,
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
            Step::ExitMidPass(pid) => rig.exit_mid_pass(*pid).await,
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
    let armed = steps
        .iter()
        .filter(|s| matches!(s, Step::ExitMidPass(_)))
        .count();
    assert_eq!(
        rig.mid_fired.load(Ordering::SeqCst),
        armed,
        "{what}: each mid-pass drain landed inside a driver pass with typing held"
    );
    model
}

/// What a run of seeds reached.
#[derive(Debug, Default)]
struct Reached {
    stats: Stats,
    triggers: usize,
    retired: usize,
}

/// Generate and run one script per seed in `seeds`, alternating Claude and
/// Codex.
async fn run_seeds(seeds: std::ops::RangeInclusive<u64>) -> Reached {
    let mut reached = Reached::default();
    for seed in seeds {
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
        reached.stats.add(&m.stats);
        reached.triggers += m.triggers.len();
        reached.retired += usize::from(m.retired);
    }
    reached
}

/// The release-table tuples a run must reach (R-B-U3-02): `(agent, kind,
/// decision, editor seen, start failed, releases)`, a selection of the
/// table, not all of it (see the module documentation). Under `EcPolicy::Hold`
/// no row completes by EC, and the Authority policy's EC rows are out of
/// this rig's reach (see the module documentation).
const ROWS: &[(&str, MKind, &str, bool, bool, bool)] = &[
    // Claude: a probe releases at either decision; a held Ctrl+G at
    // `Armed`, and at `Unarmed` once its editor runs.
    ("Claude", MKind::Probe, "Armed", false, false, true),
    ("Claude", MKind::Probe, "Unarmed", false, false, true),
    ("Claude", MKind::HumanG, "Armed", false, false, true),
    ("Claude", MKind::HumanG, "Unarmed", false, false, false),
    ("Claude", MKind::HumanG, "Unarmed", true, false, true),
    // Codex: only a held Ctrl+G whose editor runs releases before E1F.
    ("Codex", MKind::Probe, "Armed", false, false, false),
    ("Codex", MKind::Probe, "Unarmed", false, false, false),
    ("Codex", MKind::HumanG, "Armed", false, false, false),
    ("Codex", MKind::HumanG, "Unarmed", false, false, false),
    ("Codex", MKind::HumanG, "Unarmed", true, false, true),
    // A tool's editor releases nothing, on either agent.
    ("Claude", MKind::HumanG, "Tool", false, false, false),
    ("Codex", MKind::HumanG, "Tool", false, false, false),
    // A failed start releases nothing before its completion, even with the
    // editor seen running before the failure was reported.
    ("Codex", MKind::HumanG, "Unarmed", true, true, false),
    ("Codex", MKind::HumanG, "Tool", false, true, false),
];

/// Assert that `reached` reached each tuple of [`ROWS`], each kind of step
/// and each kind of settlement at least `floor` times.
fn assert_reached(reached: &Reached, floor: usize) {
    let r = &reached.stats;
    eprintln!(
        "reached: {} releases, {} passed, {} cut off, {} completions, {} triggers, \
         {} retirements, settled {:?}",
        r.released, r.passed, r.cut_off, r.completed, reached.triggers, reached.retired, r.settled
    );
    for (row, n) in &r.rows {
        eprintln!("  row {row:?}: {n}");
    }
    for &(agent, kind, decision, editor, spawn_failed, releases) in ROWS {
        let row = Row {
            agent,
            kind,
            decision,
            editor,
            spawn_failed,
            releases,
        };
        let n = r.rows.get(&row).copied().unwrap_or(0);
        assert!(n >= floor, "only {n} segments reached {row:?}");
    }
    for (what, n) in [
        ("releases", r.released),
        ("bytes passed to a released segment", r.passed),
        ("unknown endings cutting a release off", r.cut_off),
        ("completions", r.completed),
        ("triggers", reached.triggers),
        ("retirements", reached.retired),
    ] {
        assert!(n >= floor, "only {n} {what} across the seeds");
    }
    for kind in [Settled::Empty, Settled::NonEmpty, Settled::Failed] {
        let n = r.settled.get(&kind).copied().unwrap_or(0);
        assert!(n >= floor, "only {n} probes settled {kind:?}");
    }
}

/// Scenario: Generate scripts against the release table, one generation each, Claude and Codex: a probe whose fence closes in production's arm section, then typing with and without a Ctrl+G, helpers that read the open segment's number, a stale one or none, decide `Armed` (with the daemon's read of the box, empty or not, or none), `Unarmed` or a tool, run the user's editor, report, and exit, return markers, an agent that answers a trigger before its written report arrives, and retirement, after output that ran before the segments attached. After every step, once the production driver has had its chance, the PTY has taken exactly what the table allows: nothing early, nothing missing, each Ctrl+G alone after its number, nothing after retirement, and the probe's schedule shows how its decision settled exactly when it does. This is the fast tier's share; lane 1 runs a longer one.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_releases_follow_the_release_table_on_the_production_driver() {
    let reached = run_seeds(1..=FAST_SEEDS).await;
    assert_reached(&reached, 1);
}

/// Scenario: The generated release-table scripts of `probe_010_releases_follow_the_release_table_on_the_production_driver`, over a longer, different run of seeds, each release-table tuple the oracle requires reached at least three times.
#[cfg(feature = "e2e")]
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_the_long_run_follows_the_release_table() {
    let reached = run_seeds(1_001..=1_000 + LONG_SEEDS).await;
    assert_reached(&reached, 3);
}

/// The fast tier's seeds, and lane 1's (the audit's U3-9: each seed takes
/// about 0.15 s, most of it the settle waits).
const FAST_SEEDS: u64 = 40;
#[cfg(feature = "e2e")]
const LONG_SEEDS: u64 = 240;

/// An owned helper `pid` that read `seg` and decided `decision`.
fn helper(pid: u32, seg: u64, decision: Decision) -> Step {
    Step::Observe(MRecord {
        pid,
        owned: true,
        channel: Channel::Ipc,
        seg: Some(seg),
        decision: Some(decision),
        report: Report::None,
        read: None,
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
            read: None,
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

/// Scenario: A held Ctrl+G's trigger is answered by a helper that runs the user's editor, so the segment releases and typing goes on to the editor; then, for Claude and Codex, more typing is held and the release driver is stopped between reading that the segment released and taking what is held, while the helper drains with no report (an unknown wait). The bytes still held when the unknown wait became known stay held: the PTY takes nothing more (R-B-U2-02).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_an_unknown_wait_inside_a_driver_pass_keeps_what_is_held() {
    for agent in [Agent::Claude, Agent::Codex] {
        let mut steps = vec![
            Step::Type(b"\x07".to_vec()),
            helper(41, 1, Decision::Armed),
            Step::Exit(41),
        ];
        steps.extend(fast_return(agent).into_iter().map(Step::Out));
        steps.extend([
            helper(42, 2, Decision::Unarmed),
            Step::Editor(42),
            Step::Type(b"gh".to_vec()),
            Step::ExitMidPass(42),
            Step::Type(b"ij".to_vec()),
            Step::Type(b"kl".to_vec()),
        ]);
        run(agent, b"", false, &steps, &format!("R-B-U2-02 {agent:?}")).await;
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

    fn generation() -> (Arc<GenerationInput>, OutputHook) {
        let input = Arc::new(GenerationInput::default());
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
        assert!(input.attach_segments(G, Agent::Claude));
        let hook = OutputHook::new(Arc::clone(&input));
        (input, hook)
    }

    fn session(pid: u32, born: Instant) -> Observation {
        Observation {
            generation: G,
            key: key(pid),
            channel: Channel::Ipc,
            attribution: Attribution::Owned,
            claim: None,
            decision: Some(Decision::Unarmed),
            seg_seen: None,
            native_seen: None,
            birth_floor: born,
        }
    }

    /// Scenario: One more editor session than the accounting remembers pairs for: each session's bracketed-paste pair is read before its record, so each moves the epoch, and then all their records arrive late, every helper born before the first pair ended. A later, unrelated pair still moves the epoch: no late record banks credit for a pair that already moved it (R-B-U2-01, the audit's U3-4).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_a_backlog_past_the_cap_lends_nothing_to_a_later_pair() {
        let (input, mut hook) = generation();
        let pair = [OFF, ON].concat();
        let born = Instant::now();
        let sessions = u32::from(crate::composer_probe::epoch::EXPLAINED_CAP) + 1;
        for _ in 0..sessions {
            hook.on_output(&pair);
        }
        assert_eq!(input.epoch().last_foreign(), u64::from(sessions));
        for pid in 1..=sessions {
            input
                .record_observe(&session(pid, born), Instant::now())
                .unwrap();
        }
        assert_eq!(
            input.epoch().last_foreign(),
            u64::from(sessions),
            "a late record moves nothing"
        );
        hook.on_output(&pair);
        assert_eq!(
            input.epoch().last_foreign(),
            u64::from(sessions) + 1,
            "an unrelated pair after the late records moves the epoch"
        );
    }

    /// Scenario: One more editor session than the accounting remembers pairs for, with each helper born after the previous session's bracketed-paste pair ended and every record arriving late, oldest first. Each pair moves the epoch; the late records move nothing; and an unrelated pair read after them still moves the epoch, because the oldest record may be the forgotten pair's session and so spends no newer session's debt (R-B-U2-01, staggered births).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_staggered_births_past_the_cap_lend_nothing_to_a_later_pair() {
        let (input, _hook) = generation();
        let pair = pair_markers();
        let base = Instant::now();
        let at = |n: u64| base + Duration::from_millis(n);
        let sessions = u64::from(crate::composer_probe::epoch::EXPLAINED_CAP) + 1;
        // b1 < p1 < b2 < p2 < … : helper `s` is born at 2s, its pair ends at
        // 2s + 1, and no record has arrived yet.
        for s in 1..=sessions {
            input.on_return_markers(&pair, at(2 * s + 1));
        }
        assert_eq!(input.epoch().last_foreign(), sessions);
        let late = at(2 * sessions + 10);
        for s in 1..=sessions {
            input
                .record_observe(&session(s as u32, at(2 * s)), late)
                .unwrap();
        }
        assert_eq!(
            input.epoch().last_foreign(),
            sessions,
            "a late record moves nothing"
        );
        input.on_return_markers(&pair, at(2 * sessions + 11));
        assert_eq!(
            input.epoch().last_foreign(),
            sessions + 1,
            "an unrelated pair after the staggered late records moves the epoch"
        );
    }

    /// The return markers of one whole bracketed-paste pair.
    fn pair_markers() -> [crate::composer_probe::segments::Seen; 2] {
        use crate::composer_probe::segments::{Marker, Seen};
        [
            Seen {
                marker: Marker::PasteOff,
                start: 0,
            },
            Seen {
                marker: Marker::PasteOn,
                start: 8,
            },
        ]
    }

    /// Scenario: A helper's record is made, so its session may account for one bracketed-paste pair, but its pair never comes (the agent dropped it). While the helper runs, and for a grace after its drain, a pair read is still taken as its session's; once the grace is over, its credit is gone, and an unrelated pair moves the epoch (the audit's U3-4). A session's own pair delivered after the grace (a paused or backlogged output reader) also moves the epoch, once: the grace is a conservative policy, not a bound on output delivery. A session that never drains keeps its credit however long its editor runs.
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_a_drained_record_whose_pair_never_came_lends_nothing() {
        use crate::composer_probe::epoch::CREDIT_AFTER_DRAIN;
        let pair = pair_markers();
        let past = |at: Instant| at + CREDIT_AFTER_DRAIN + Duration::from_millis(1);

        // Drained, and the grace is over: the pair is an external edit.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        let drained = Instant::now();
        input.record_exit(G, key(1), drained, drained).unwrap();
        input.record_drained(G, key(1), drained).unwrap();
        input.on_return_markers(&pair, past(Instant::now()));
        assert_eq!(
            input.epoch().last_foreign(),
            1,
            "a credit outlives its drained record by the grace only"
        );

        // Control: drained, the pair read inside the grace is its session's.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        let drained = Instant::now();
        input.record_exit(G, key(1), drained, drained).unwrap();
        input.record_drained(G, key(1), drained).unwrap();
        input.on_return_markers(&pair, Instant::now());
        assert_eq!(input.epoch().last_foreign(), 0, "its own pair, just after");

        // Control: still running, however long: its pair is still to come.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        input.on_return_markers(&pair, past(past(Instant::now())));
        assert_eq!(input.epoch().last_foreign(), 0, "a long editor session");

        // Control: its own pair, delayed past the grace after its drain, is
        // read as an external edit, once; an unrelated pair after it moves
        // the epoch too, so nothing is left banked by the late one.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        let drained = Instant::now();
        input.record_exit(G, key(1), drained, drained).unwrap();
        input.record_drained(G, key(1), drained).unwrap();
        let late = past(drained);
        input.on_return_markers(&pair, late);
        assert_eq!(
            input.epoch().last_foreign(),
            1,
            "its own pair, delivered after the grace, costs one more epoch"
        );
        input.on_return_markers(&pair, late + Duration::from_millis(1));
        assert_eq!(
            input.epoch().last_foreign(),
            2,
            "an unrelated pair after it"
        );
    }

    /// One event of an interleaved pair script.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        /// Session `s`'s helper is born.
        Born(usize),
        /// Its record is made.
        Record(usize),
        /// Its pair is read.
        Pair(usize),
        /// Its helper exits and drains.
        Drain(usize),
        /// A pair no record accounts for.
        Foreign,
        /// The clock moves past [`CREDIT_AFTER_DRAIN`].
        ///
        /// [`CREDIT_AFTER_DRAIN`]: crate::composer_probe::epoch::CREDIT_AFTER_DRAIN
        Wait,
    }

    /// The script shapes [`probe_048_interleaved_sessions_and_backlogs_keep_every_foreign_pair`]
    /// generates, and how many of each it ran.
    #[derive(Debug, Default)]
    struct Shapes {
        /// More pairs read before their records than the accounting
        /// remembers.
        backlogs: usize,
        /// A backlog past the cap in which a helper was born after an
        /// earlier pair had ended: R-B-U2-01's staggered births.
        staggered: usize,
        /// A session whose pair never comes.
        pairless: usize,
        /// Ended with every session drained, the grace over, and a final
        /// unrelated pair.
        quiescent: usize,
        /// A session whose pair is read after its credit expired.
        late_own: usize,
    }

    /// One generated script: its events, and which sessions have no pair.
    fn script(rng: &mut Rng, cap: usize) -> (Vec<Event>, Vec<bool>) {
        let shape = rng.below(3);
        let sessions = if shape == 2 {
            cap + 1 + rng.below(5) as usize
        } else {
            rng.below(10) as usize
        };
        let foreign = rng.below(4) as usize;
        // In one script in three, one or two sessions whose pair never comes
        // (the agent exited, or the pair was lost).
        let mut pairless = vec![false; sessions];
        if sessions > 0 && rng.chance(33) {
            for _ in 0..=usize::from(rng.chance(30)) {
                pairless[rng.below(sessions as u64) as usize] = true;
            }
        }
        let mut events = match shape {
            0 => {
                // Every pair, the foreign ones among them, before any record.
                let mut pairs: Vec<Event> = (0..sessions)
                    .filter(|&s| !pairless[s])
                    .map(Event::Pair)
                    .collect();
                for _ in 0..foreign {
                    let at = rng.below(pairs.len() as u64 + 1) as usize;
                    pairs.insert(at, Event::Foreign);
                }
                let mut events: Vec<Event> = (0..sessions).map(Event::Born).collect();
                events.extend(pairs);
                for s in 0..sessions {
                    events.push(Event::Record(s));
                    if rng.chance(50) {
                        events.push(Event::Drain(s));
                    }
                }
                events
            }
            1 => {
                // Each session's own order, interleaved at random with the
                // others'.
                let mut lanes: Vec<VecDeque<Event>> = (0..sessions)
                    .map(|s| {
                        let mut lane = VecDeque::from([Event::Born(s)]);
                        let mut rest = if pairless[s] {
                            vec![Event::Record(s)]
                        } else if rng.chance(50) {
                            vec![Event::Record(s), Event::Pair(s)]
                        } else {
                            vec![Event::Pair(s), Event::Record(s)]
                        };
                        if rng.chance(60) {
                            // Its drain, after its record, before or after
                            // its pair.
                            let record = rest.iter().position(|e| *e == Event::Record(s));
                            let at = record.unwrap() + 1;
                            let at = at + rng.below((rest.len() - at) as u64 + 1) as usize;
                            rest.insert(at, Event::Drain(s));
                        }
                        lane.extend(rest);
                        lane
                    })
                    .collect();
                lanes.extend((0..foreign).map(|_| VecDeque::from([Event::Foreign])));
                interleave(rng, lanes)
            }
            _ => {
                // R-B-U2-01's shape: births staggered across earlier pairs'
                // endings (each helper born, then its pair, interleaved with
                // the other sessions'), every record late, read after its own
                // pair and interleaved with later births and pairs.
                let mut lanes: Vec<VecDeque<Event>> = (0..sessions)
                    .map(|s| {
                        if pairless[s] {
                            VecDeque::from([Event::Born(s)])
                        } else {
                            VecDeque::from([Event::Born(s), Event::Pair(s)])
                        }
                    })
                    .collect();
                lanes.extend((0..foreign).map(|_| VecDeque::from([Event::Foreign])));
                let mut events = interleave(rng, lanes);
                for (s, &missing) in pairless.iter().enumerate() {
                    let after = events
                        .iter()
                        .position(|e| {
                            *e == if missing {
                                Event::Born(s)
                            } else {
                                Event::Pair(s)
                            }
                        })
                        .unwrap()
                        + 1;
                    // Late, and most often after every pair.
                    let at = if rng.chance(60) {
                        events.len()
                    } else {
                        after + rng.below((events.len() - after) as u64 + 1) as usize
                    };
                    events.insert(at, Event::Record(s));
                    if rng.chance(50) {
                        let after = at + 1;
                        let at = after + rng.below((events.len() - after) as u64 + 1) as usize;
                        events.insert(at, Event::Drain(s));
                    }
                }
                events
            }
        };
        // The clock passes the grace now and then, mid-script.
        if rng.chance(30) {
            for _ in 0..=rng.below(2) {
                let at = rng.below(events.len() as u64 + 1) as usize;
                events.insert(at, Event::Wait);
            }
        }
        (events, pairless)
    }

    /// Lanes popped from the front in a random order.
    fn interleave(rng: &mut Rng, mut lanes: Vec<VecDeque<Event>>) -> Vec<Event> {
        let mut events = Vec::new();
        while lanes.iter().any(|l| !l.is_empty()) {
            let open: Vec<usize> = (0..lanes.len()).filter(|&i| !lanes[i].is_empty()).collect();
            let lane = open[rng.below(open.len() as u64) as usize];
            events.push(lanes[lane].pop_front().unwrap());
        }
        events
    }

    /// Scenario: Generate scripts in which editor sessions overlap rather than following one another: each helper is born, then its record and its bracketed-paste pair arrive in either order, interleaved with other sessions' and with pairs no record accounts for. Some scripts read a backlog of pairs past what the accounting remembers before any of their records; some stagger the helpers' births across earlier pairs' endings, with every record late (R-B-U2-01's shape); some have a session whose pair never comes; the clock passes the credit's grace now and then; and most end with an unrelated pair, after every session drained and the grace passed when a session's pair never came. From any point in a script, the epoch moves at least once for each pair no record accounts for read from there on, less one for each session whose pair never came, and the final unrelated pair after the grace always moves it (R-B-U2-01, the audit's U3-1).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_interleaved_sessions_and_backlogs_keep_every_foreign_pair() {
        use crate::composer_probe::epoch::CREDIT_AFTER_DRAIN;
        let cap = usize::from(crate::composer_probe::epoch::EXPLAINED_CAP);
        let mut shapes = Shapes::default();
        for seed in 1..=600u64 {
            let mut rng = Rng(0x2545_f491_4f6c_dd1d ^ seed.wrapping_mul(0xd1b5_4a32_d192_ed03));
            let (mut events, pairless) = script(&mut rng, cap);
            let sessions = pairless.len();
            let missing = pairless.iter().filter(|&&p| p).count();
            let quiescent = missing > 0 || rng.chance(30);
            if quiescent {
                // Every helper exits, the grace passes, and an unrelated
                // pair is read: no credit is left to explain it.
                let drained: Vec<usize> = events
                    .iter()
                    .filter_map(|e| match e {
                        Event::Drain(s) => Some(*s),
                        _ => None,
                    })
                    .collect();
                events.extend(
                    (0..sessions)
                        .filter(|s| !drained.contains(s))
                        .map(Event::Drain),
                );
                events.extend([Event::Wait, Event::Foreign]);
            } else if rng.chance(70) {
                events.push(Event::Foreign);
            }
            // A virtual clock, so a helper's birth, a pair's ending and the
            // grace are ordered exactly as scripted.
            let (input, _hook) = generation();
            let pair = pair_markers();
            let mut clock = Instant::now();
            let mut born = vec![clock; sessions];
            let mut recorded = vec![false; sessions];
            let mut drained_at: Vec<Option<Instant>> = vec![None; sessions];
            let mut early = 0usize;
            let mut staggered = false;
            let mut any_pair = false;
            let mut late_own = false;
            // Each pair read: whether no record accounts for it, and whether
            // it moved the epoch.
            let mut pairs: Vec<(bool, bool)> = Vec::new();
            for &event in &events {
                clock += Duration::from_millis(1);
                let before = input.epoch().last_foreign();
                match event {
                    Event::Born(s) => {
                        born[s] = clock;
                        staggered |= any_pair;
                    }
                    Event::Record(s) => {
                        recorded[s] = true;
                        input
                            .record_observe(&session(s as u32 + 1, born[s]), clock)
                            .unwrap();
                    }
                    Event::Drain(s) => {
                        drained_at[s] = Some(clock);
                        input
                            .record_exit(G, key(s as u32 + 1), clock, clock)
                            .unwrap();
                        input.record_drained(G, key(s as u32 + 1), clock).unwrap();
                    }
                    Event::Pair(s) => {
                        any_pair = true;
                        early += usize::from(!recorded[s]);
                        late_own |= drained_at[s]
                            .is_some_and(|d| clock.duration_since(d) >= CREDIT_AFTER_DRAIN);
                        input.on_return_markers(&pair, clock);
                    }
                    Event::Foreign => input.on_return_markers(&pair, clock),
                    Event::Wait => clock += CREDIT_AFTER_DRAIN,
                }
                let moved = input.epoch().last_foreign() - before;
                assert!(
                    moved <= 1,
                    "seed {seed}: {event:?} moved the epoch {moved} times"
                );
                if matches!(event, Event::Pair(_) | Event::Foreign) {
                    pairs.push((event == Event::Foreign, moved == 1));
                } else {
                    assert_eq!(moved, 0, "seed {seed}: {event:?} moved the epoch");
                }
            }
            if early > cap {
                shapes.backlogs += 1;
                shapes.staggered += usize::from(staggered);
            }
            shapes.pairless += usize::from(missing > 0);
            shapes.late_own += usize::from(late_own);
            // A count over suffixes, not prompt invalidation: credits are
            // consumed oldest first, so a foreign pair read while another
            // session's credit is live moves the epoch at that session's own
            // pair instead (the audit's U3-1), and a session whose pair never
            // comes may leave one foreign pair unmoved until its grace ends.
            for from in 0..pairs.len() {
                let foreign = pairs[from..].iter().filter(|p| p.0).count();
                let moves = pairs[from..].iter().filter(|p| p.1).count();
                assert!(
                    moves + missing >= foreign,
                    "seed {seed}: from pair {from} on, {foreign} pairs no record accounts for \
                     moved the epoch only {moves} times, with {missing} sessions whose pair \
                     never came\nevents {events:?}\npairs {pairs:?}"
                );
            }
            if quiescent {
                shapes.quiescent += 1;
                assert_eq!(
                    pairs.last(),
                    Some(&(true, true)),
                    "seed {seed}: an unrelated pair after every session drained and the grace \
                     passed moved nothing\nevents {events:?}\npairs {pairs:?}"
                );
            }
        }
        eprintln!("probe_048 interleaved shapes over 600 seeds: {shapes:?}");
        assert!(
            shapes.backlogs >= 20,
            "too few backlogs past the cap: {shapes:?}"
        );
        assert!(
            shapes.staggered >= 20,
            "too few staggered backlogs: {shapes:?}"
        );
        assert!(
            shapes.pairless >= 20,
            "too few pairless scripts: {shapes:?}"
        );
        assert!(shapes.quiescent >= 20, "too few quiescent ends: {shapes:?}");
        assert!(shapes.late_own >= 5, "too few late own pairs: {shapes:?}");
    }
}
