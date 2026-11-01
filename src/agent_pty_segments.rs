//! PRD #1666 part 3 (Q35-20; core §4.5–§4.9, R5 §4.2, A4 §4, A5 §1.5): an
//! installed generation's segments in the live gate.
//!
//! **A probe** ([`AgentPtyRegistry::composer_probe`]) is asked at its
//! scheduling (the schedule, then admission with the facts read there),
//! opens its segment, and queues its `0x07` under the pane writer's lock,
//! so no person's op can be accepted between the gate learning of the job
//! and the job reaching the writer. On the writer thread its start condition
//! is the arm section ([`crate::composer_probe::gate::Gate::arm_section`]):
//! under the gate's lock, admission asked again with the writer's own
//! boundary, then the liability and segment number published and the fence
//! closed, before the byte. A person's op accepted before the section takes
//! the job back; one accepted after it is held (O-20.2).
//!
//! **The release driver** ([`drive_segments`]) is one task per installed
//! generation. Every step that may move the segments (a record step, the
//! output's return markers, a trigger written) wakes it; it takes the pane
//! writer's lock, so a release is ordered against acceptance, and does what
//! the front segment allows: nothing while it holds; the held input as far
//! as the next held Ctrl+G candidate once it has released; and with no
//! segment open, everything up to that candidate, whose bytes then go alone
//! as the next segment's trigger, its number published first. Nothing here
//! releases on a timer: the segments decide (core §4.6).
//!
//! **What it doesn't decide** (R35PLAN2-U2-01): whether a ring follows a
//! check, the card's hold reason and delivery after a check stay on today's
//! code until Q35-21. No production generation is `Armable` before an
//! activation commit, so outside the `e2e` build's assumption no probe is
//! ever admitted, no fence closes and the driver finds nothing to do.
//!
//! **Where each fact admission reads is read (O-20.7, U3-03).** A probe is
//! asked twice: when scheduled, under the pane writer's lock, and on the
//! writer thread at its arm section, under the gate's lock. The second ask
//! reads for itself what can change between the two:
//! - the permission: the seam's own, under the seam's lock, both times;
//! - the records, scope, hazards, estimate, epoch and retirement: under the
//!   input's lock (R6), in the hold that publishes the arm;
//! - the gate's fence and held ops: under the gate's lock;
//! - the written stream's boundary: the writer's own;
//! - a person's ticket: the ledger read again under the gate's lock;
//! - the screen: the frame read when scheduled stands only while the bus
//!   has taken no output since it (its `out_seq`), read on the writer thread
//!   just before the gate's lock, as the output hook's facts are: output
//!   after it and before the byte could as well land just after the byte.
//!
//! The keymap is file I/O and is read with no lock held: after a pending
//! permission is settled, and again after an O5 wait, so nothing read before
//! a sleep stands after it. Between its last read and the byte, a change to
//! the agent's key bindings is not seen: its re-check at settlement
//! (O-20.7b) is an activation blocker (`plan-p3.md`'s activation list).
//!
//! **Lock order.**
//! ```text
//! probe request: pane writer (async) ─> gate (probe_queued) ; submit job
//! writer thread: bus (the screen's `out_seq`, released) ; then
//!                arm section: gate ─> ledger (released) ; gate ─> seam ─>
//!                input (R6) ─> page stores
//! driver:        pane writer (async) ─> gate (release_until_if / release_cut)
//!                pane writer (async) ─> gate ─> input (R6) (release_until_if's
//!                                       re-read of the front, R-B-U2-02)
//!                pane writer (async) ─> seam (publish a HumanG's number)
//!                input (R6) read with nothing else held
//! observer:      seam ─> input (R6) ─> segments (settle, under R6) ; notify
//! output hook:   output lock ─> input (R6) ─> segments ; notify
//! ```
//! Nothing takes the gate under the seam or R6, nor the seam under R6.
//! Retirement: the registry retires the input (segments end `Retired`) and
//! the gate (held ops refused `exited` once), and the driver ends at its next
//! wake.

use super::*;
#[cfg(target_os = "linux")]
use crate::composer_probe::framing::Framing;
#[cfg(target_os = "linux")]
use crate::composer_probe::gate::{Cutting, Measure};
#[cfg(target_os = "linux")]
use crate::composer_probe::segments::Front;

/// Which of the deck's check writes a probe request asks for.
#[cfg(any(target_os = "linux", feature = "e2e"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeAsk {
    First,
    Retry,
    Lapse,
}

/// What a probe request did.
#[cfg(any(target_os = "linux", feature = "e2e"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// Not due by the generation's schedule: nothing was scheduled,
    /// published, closed or moved, and the schedule is as it was.
    NotDue,
    /// Refused, when scheduled or at the writer's arm section: nothing was
    /// written, no arm word moved and no fence closed.
    Refused(String),
    /// Its byte was written.
    Written,
    /// A person's op took its job back before the writer came to it.
    TakenBack,
    /// No observed generation on that pane, or none on this platform.
    Unavailable,
}

/// Why the writer refused a probe's job at its arm section.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeRefusal {
    Admission(crate::composer_probe::admission::Refusal),
    Retired,
    TakenBack,
}

/// How a probe's job ended.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeEnd {
    Written { offset: u64, at: Instant },
    WriteFailed,
    Refused(ProbeRefusal),
    TakenBack,
}

/// A probe's job on the writer, from its enqueue to its end.
#[cfg(target_os = "linux")]
pub(crate) struct ProbeJob {
    claim: Arc<crate::doorbell_txn::JobClaim>,
    in_flight: Arc<PtyInFlight>,
    done: oneshot::Receiver<PtyJobDone>,
    section: SectionOutcome,
    left: bool,
}

/// What a probe's arm section did, once it ran: the output offset and instant
/// it closed the fence at, or why it refused.
#[cfg(target_os = "linux")]
type SectionOutcome = Arc<Mutex<Option<Result<(u64, Instant), ProbeRefusal>>>>;

#[cfg(target_os = "linux")]
impl ProbeJob {
    pub(super) async fn end(mut self) -> ProbeEnd {
        let done = (&mut self.done).await;
        let section = *self.section.lock().unwrap_or_else(|e| e.into_inner());
        match (done, section) {
            (Ok(done), Some(Ok((offset, at)))) => {
                let ok = done.result.is_ok() && done.accepted == 1;
                if ok {
                    ProbeEnd::Written { offset, at }
                } else {
                    ProbeEnd::WriteFailed
                }
            }
            (Ok(_), _) => ProbeEnd::WriteFailed,
            (Err(_), Some(Err(refusal))) => {
                self.reconcile();
                ProbeEnd::Refused(refusal)
            }
            (Err(_), _) => {
                self.reconcile();
                match self.claim.state() {
                    crate::doorbell_txn::ClaimState::Refused => {
                        ProbeEnd::Refused(ProbeRefusal::Retired)
                    }
                    _ => ProbeEnd::TakenBack,
                }
            }
        }
    }

    /// A job taken back leaves the in-flight count once: nobody else does.
    fn reconcile(&mut self) {
        if self.claim.state() == crate::doorbell_txn::ClaimState::Withdrawn && !self.left {
            self.left = true;
            self.in_flight.left_by_withdrawal();
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for ProbeJob {
    fn drop(&mut self) {
        self.reconcile();
    }
}

#[cfg(target_os = "linux")]
impl UserFrame {
    /// PRD #1666 part 3 (Q35-20): how far a release may take this held
    /// frame: its last ground point, read from its own acceptance-time
    /// framing (or, for one with none, what the frame before it left in
    /// `carry`), and before its candidate when it holds one.
    fn measure(&self, carry: &mut Option<Framing>) -> Measure {
        let mut framing = self
            .framing
            .clone()
            .or_else(|| carry.take())
            .unwrap_or_default();
        let limit = self.cut.as_ref().map_or(self.bytes.len(), |cut| cut.start);
        let mut ground = framing.at_ground().then_some(0);
        for (i, &byte) in self.bytes.iter().enumerate() {
            if i >= limit {
                break;
            }
            framing.feed(byte, |_| {});
            if framing.at_ground() {
                ground = Some(i + 1);
            }
        }
        *carry = Some(framing);
        Measure {
            ground,
            candidate: self.cut.is_some(),
        }
    }

    /// PRD #1666 part 3 (Q35-20): its first `n` bytes as a sub-op of the
    /// same op with no ticket of its own, this frame left as the rest, its
    /// framing and cut moved past them.
    fn split_at(&mut self, n: usize) -> UserFrame {
        let whole = self.bytes.len();
        let part = Arc::clone(self.part.get_or_insert_with(|| SubOps::new(whole)));
        let rest = self.bytes.split_off(n);
        let prefix = std::mem::replace(&mut self.bytes, rest);
        if let Some(framing) = self.framing.as_mut() {
            for &byte in &prefix {
                framing.feed(byte, |_| {});
            }
        }
        if let Some(cut) = self.cut.as_mut() {
            cut.at = cut.at.saturating_sub(n);
            cut.start = cut.start.saturating_sub(n);
        }
        UserFrame {
            bytes: prefix,
            ticket: None,
            notice: self.notice.clone(),
            cut: None,
            part: Some(part),
            answer: None,
            framing: None,
        }
    }
}

#[cfg(target_os = "linux")]
impl PaneWriter {
    /// PRD #1666 part 3 (Q35-20): queue a probe's `0x07` as job `job` under
    /// `claim`, the gate told first so that a person's op accepted before
    /// its arm section takes it back. `section` is the arm section, run on
    /// the writer thread only while the job is queued; `disarm` undoes it if
    /// a withdrawal won the claim after it succeeded (`probe::start`).
    /// Called under this writer's lock. `None` when the writer is gone.
    pub(crate) fn queue_probe(
        &self,
        job: crate::composer_probe::epoch::DeckJobId,
        claim: Arc<crate::doorbell_txn::JobClaim>,
        section: impl FnOnce(&crate::doorbell_txn::StartCtx) -> Result<(u64, Instant), ProbeRefusal>
        + Send
        + 'static,
        disarm: impl FnOnce() + Send + 'static,
    ) -> Option<ProbeJob> {
        let (tx, done) = oneshot::channel();
        let outcome: SectionOutcome = Arc::default();
        let seen = Arc::clone(&outcome);
        let start: StartCheck = Box::new(move |claim, ctx| {
            let started = crate::composer_probe::probe::start(
                claim,
                ctx,
                |ctx| {
                    let armed = section(ctx);
                    let result = armed.map(drop);
                    *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(armed);
                    result
                },
                disarm,
            );
            matches!(started, crate::doorbell_txn::Start::Started)
        });
        self.gate.gate.probe_queued(Arc::clone(&claim));
        let (claim, _) = self.pty.submit_job(
            PtyOp::WriteAll(vec![0x07], ByteSource::Probe(job), true),
            PtyReply::Async(tx),
            None,
            claim,
            Acceptance::Now,
            Some(start),
        )?;
        Some(ProbeJob {
            claim,
            in_flight: Arc::clone(&self.pty.in_flight),
            done,
            section: outcome,
            left: false,
        })
    }

    /// PRD #1666 part 3 (Q35-20): one step of the release driver, under
    /// this writer's lock: what the front segment lets the held input do.
    /// `publish` puts a `HumanG`'s number on the arm page before its trigger
    /// is submitted. Returns once nothing more can move until the segments
    /// do.
    pub(crate) fn drive_segments(&self, publish: &dyn Fn(u64)) {
        let input = Arc::clone(&self.pty.input);
        let front = input.segments_front();
        #[cfg(test)]
        self.mid_pass();
        match front {
            None | Some(Front::Held | Front::Trigger) => {}
            Some(Front::Released) => {
                if self.gate.gate.segment_fenced() {
                    self.release_held(Front::Released);
                }
            }
            Some(Front::None) => {
                // Only a fence a probe closed is the driver's to open: one
                // the `e2e` control or a liability's cut closed isn't.
                if !self.gate.gate.segment_fenced() {
                    return;
                }
                self.release_held(Front::None);
                if !self.gate.gate.holds_any(|frame| frame.cut.is_some()) {
                    return;
                }
                let Ok(number) = input.segment_trigger() else {
                    return;
                };
                publish(number);
                self.submit_trigger(number);
            }
        }
    }

    /// Test-only: arm `hook` to run in a later driver pass between its read
    /// of the front segment and its extraction of held input, until it
    /// returns `true` (R-B-U2-02's forced interleave).
    #[cfg(test)]
    pub(super) fn set_mid_pass(&self, hook: Box<dyn FnMut() -> bool + Send>) {
        *self.mid_pass.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    fn mid_pass(&self) {
        let mut slot = self.mid_pass.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hook) = slot.as_mut()
            && hook()
        {
            *slot = None;
        }
    }

    /// Release the held input as far as the next held candidate
    /// ([`crate::composer_probe::gate::Gate::release_until_if`]) under the
    /// authority of the front segment this pass read, `front`: `Released`,
    /// or `None`, no segment being outstanding, which takes all of it when
    /// no candidate is held. The gate asks the segments again under its own
    /// lock, so a fact that withdraws the release after this pass read the
    /// front (R-B-U2-02: an unknown wait's cut-off) leaves everything held.
    /// A target that exited refuses what comes back, as a release does.
    fn release_held(&self, front: Front) -> usize {
        let in_flight = &self.pty.in_flight;
        let input = &self.pty.input;
        let mut carry: Option<Framing> = None;
        let frames = self.gate.gate.release_until_if(
            || input.segments_front() == Some(front),
            |frame| frame.measure(&mut carry),
            |frame, n| {
                // One more sub-op in flight: the rest stays held.
                in_flight.count.fetch_add(1, Ordering::SeqCst);
                frame.split_at(n)
            },
            front == Front::None,
        );
        let mut submitted = 0;
        for frame in frames {
            if self.exited.load(Ordering::SeqCst) {
                frame.refuse_accepted("exited");
                in_flight.leave();
            } else {
                self.pty.submit_released(frame, Arc::clone(&self.exited));
                submitted += 1;
            }
        }
        submitted
    }

    /// Submit segment `number`'s trigger, the held bytes through the first
    /// cut, alone (core §4.5): the fence stays closed behind it. The output
    /// offset and instant as its first part starts are the lower bound its
    /// return markers count from; its last part's answer reports it written
    /// or failed.
    fn submit_trigger(&self, number: u64) {
        let in_flight = &self.pty.in_flight;
        let input = Arc::clone(&self.pty.input);
        let frames = self
            .gate
            .gate
            .release_cut(input.output_offset(), |frame: &mut UserFrame| {
                let cutting = frame.split();
                if matches!(cutting, Cutting::Split(..)) {
                    in_flight.count.fetch_add(1, Ordering::SeqCst);
                }
                cutting
            });
        let started: Arc<Mutex<Option<(u64, Instant)>>> = Arc::default();
        let mut last = None;
        let count = frames.len();
        for (i, frame) in frames.into_iter().enumerate() {
            let start: Option<StartCheck> = (i == 0).then(|| {
                let started = Arc::clone(&started);
                let input = Arc::clone(&input);
                let check: StartCheck = Box::new(move |claim, ctx| {
                    *started.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((input.output_offset(), ctx.now));
                    claim.start()
                });
                check
            });
            let reply = self
                .pty
                .submit_released_reporting(frame, Arc::clone(&self.exited), start);
            if i + 1 == count {
                last = reply;
            }
        }
        let Some(done) = last else {
            let _ = input.segment_write_failed(number);
            return;
        };
        let report = async move {
            let written = matches!(done.await, Ok(PtyJobDone { result: Ok(()), .. }));
            let at = *started.lock().unwrap_or_else(|e| e.into_inner());
            match (written, at) {
                (true, Some((offset, at))) => {
                    let _ = input.segment_written(number, offset, at, Instant::now());
                }
                _ => {
                    let _ = input.segment_write_failed(number);
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(report);
            }
            Err(_) => {
                tracing::warn!(target: "composer_probe", "no runtime to follow a trigger");
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl PtyWriterThread {
    /// [`Self::submit_released`] with a start check and the job's answer
    /// returned (Q35-20's triggers).
    fn submit_released_reporting(
        &self,
        frame: UserFrame,
        exited: Arc<AtomicBool>,
        start: Option<StartCheck>,
    ) -> Option<oneshot::Receiver<PtyJobDone>> {
        let (tx, rx) = oneshot::channel();
        let op = PtyOp::WriteAll(frame.bytes, ByteSource::User, true);
        self.submit_job(
            op,
            PtyReply::Async(tx),
            frame.ticket,
            Arc::default(),
            Acceptance::Released(
                Released {
                    exited,
                    notice: frame.notice,
                },
                frame.part.as_ref().map(SubOps::run),
            ),
            start,
        )?;
        Some(rx)
    }
}

/// A probe's start condition on the writer thread (R5 §4.2, O-20.2): the
/// writer's own retirement refuses it; otherwise `fresh` is read, then the
/// gate's arm section runs `admit` under the gate's lock, with the writer's
/// start facts, the gate's counts as they stand and what `fresh` said, and
/// on its `Ok` closes the fence at the output offset read then: that offset
/// and the writer's instant are the section's outcome (O-20.7a). `fresh` is
/// the screen's freshness (U3-03), read here, before the gate's lock, as
/// the output hook's facts are: output after it and before the byte could
/// as well land just after the byte. Shared by
/// [`AgentPtyRegistry::composer_probe`] and the release oracle, so the
/// oracle runs this one (the audit's U3-9).
#[cfg(target_os = "linux")]
pub(super) fn probe_section(
    gate: Arc<crate::composer_probe::gate::Gate<UserFrame>>,
    input: Arc<crate::composer_probe::epoch::GenerationInput>,
    claim: Arc<crate::doorbell_txn::JobClaim>,
    fresh: impl FnOnce() -> bool + Send + 'static,
    admit: impl FnOnce(
        &crate::doorbell_txn::StartCtx,
        crate::composer_probe::gate::Status,
        bool,
    ) -> Result<(), ProbeRefusal>
    + Send
    + 'static,
) -> impl FnOnce(&crate::doorbell_txn::StartCtx) -> Result<(u64, Instant), ProbeRefusal> + Send + 'static
{
    move |ctx| {
        #[cfg(feature = "e2e")]
        probe_gate_scheduled();
        if ctx.retired {
            return Err(ProbeRefusal::Retired);
        }
        let fresh = fresh();
        let mut at = (0, ctx.now);
        gate.arm_section(
            &claim,
            ProbeRefusal::Retired,
            ProbeRefusal::TakenBack,
            |status| admit(ctx, status, fresh),
            || {
                at.0 = input.output_offset();
                at.0
            },
        )?;
        Ok(at)
    }
}

/// The facts the writer asks admission with at a probe's arm section
/// (O-20.7, U3-03), from those `scheduled` asked with when it was scheduled:
/// its own boundary (`ctx`), the fence as the gate's counts (`status`) show
/// it, a person's ticket as the ledger shows it now (`human_ticket`, read
/// under the gate's lock), and the screen only while no output was taken
/// since the frame it was read from (`screen_unchanged`). A lapse retry's
/// own byte moved the epoch at its enqueue, which nothing else may have
/// since the lapsed probe's (R5 §3.2). The permission is the seam's own at
/// the arm ([`crate::composer_probe::observe::Seam::arm`]); the keymap is
/// the one read when scheduled.
#[cfg(target_os = "linux")]
pub(super) fn writer_outside(
    scheduled: crate::composer_probe::admission::Outside,
    ctx: &crate::doorbell_txn::StartCtx,
    status: crate::composer_probe::gate::Status,
    human_ticket: bool,
    screen_unchanged: bool,
) -> crate::composer_probe::admission::Outside {
    use crate::composer_probe::admission::{Attempt, Fence, Outside};
    let fence = match (status.fenced, scheduled.attempt) {
        (false, _) => Fence::Open,
        (true, Attempt::Lapse { .. }) if status.held_ops == 0 => Fence::Lapsed,
        (true, _) => Fence::Closed,
    };
    let attempt = match scheduled.attempt {
        Attempt::Lapse { lapsed_e0 } => Attempt::Lapse {
            lapsed_e0: lapsed_e0.saturating_add(1),
        },
        other => other,
    };
    Outside {
        attempt,
        boundary: ctx.boundary,
        fence,
        human_ticket,
        surface: scheduled.surface && screen_unchanged,
        ..scheduled
    }
}

/// What a probe's job's end tells its segment: written at its offset and
/// instant, failed, or withdrawn (refused at the writer or taken back).
/// Shared with the release oracle.
#[cfg(target_os = "linux")]
pub(super) fn end_probe_segment(
    input: &crate::composer_probe::epoch::GenerationInput,
    number: u64,
    end: ProbeEnd,
) {
    let _ = match end {
        ProbeEnd::Written { offset, at } => {
            input.segment_written(number, offset, at, Instant::now())
        }
        ProbeEnd::WriteFailed => input.segment_write_failed(number),
        ProbeEnd::Refused(_) | ProbeEnd::TakenBack => input.segment_withdrawn(number),
    };
}

/// What a probe request does once its job ended (O-20.5): the end told to
/// its segment ([`end_probe_segment`]); a failed write (an error, or none of
/// its one byte taken) undoes its arm with `disarm`, which lowers the
/// liability, opens a fence nothing is held behind (the driver releases what
/// is, its segment gone) and counts a failure, as an attempt refused at the
/// writer does. A failed write answers its own reason, never an admission
/// refusal's. A lapse retry counts no failure here: its asker decides.
/// Shared with the release oracle.
#[cfg(target_os = "linux")]
pub(super) fn probe_ended(
    input: &crate::composer_probe::epoch::GenerationInput,
    gate: &crate::composer_probe::gate::Gate<UserFrame>,
    number: u64,
    end: ProbeEnd,
    ask: ProbeAsk,
    disarm: impl FnOnce(),
) -> ProbeOutcome {
    end_probe_segment(input, number, end);
    match end {
        ProbeEnd::Written { .. } => ProbeOutcome::Written,
        ProbeEnd::WriteFailed => {
            disarm();
            gate.open_if_empty();
            if ask != ProbeAsk::Lapse {
                input.probe_failed(Instant::now());
            }
            ProbeOutcome::Refused(PROBE_WRITE_FAILED.into())
        }
        ProbeEnd::Refused(refusal) => {
            tracing::debug!(
                target: "composer_probe",
                ?ask,
                ?refusal,
                "probe refused at the writer"
            );
            if ask != ProbeAsk::Lapse {
                input.probe_failed(Instant::now());
            }
            ProbeOutcome::Refused(format!("{refusal:?}"))
        }
        ProbeEnd::TakenBack => ProbeOutcome::TakenBack,
    }
}

/// Why a probe whose byte the PTY didn't take is refused.
#[cfg(target_os = "linux")]
pub(super) const PROBE_WRITE_FAILED: &str = "the probe's write failed";

/// PRD #1666 part 3 (Q35-20): an installed generation's release driver. It
/// ends when the generation retires.
#[cfg(target_os = "linux")]
pub(crate) async fn drive_segments(
    writer: Arc<AsyncMutex<PaneWriter>>,
    input: Arc<crate::composer_probe::epoch::GenerationInput>,
    publish: Box<dyn Fn(u64) + Send + Sync>,
) {
    loop {
        let moved = input.segments_moved().notified();
        tokio::pin!(moved);
        moved.as_mut().enable();
        if input.retired() {
            return;
        }
        {
            let w = writer.lock().await;
            w.drive_segments(&*publish);
        }
        moved.await;
    }
}

/// The `e2e` build's `scheduled` barrier (S-20's assumed interface): with
/// `DOT_AGENT_DECK_E2E_PROBE_GATE` naming one path component under the
/// daemon's `HOME` and `<root>/phase` reading `scheduled`, the writer thread
/// stops before the arm section asks admission again: it writes
/// `scheduled.arrived`, waits up to [`PROBE_GATE_WAIT`] for
/// `scheduled.release`, and writes `scheduled.timed-out` if that never comes.
/// It grants no admission and releases nothing.
#[cfg(all(feature = "e2e", target_os = "linux"))]
fn probe_gate_scheduled() {
    let Some(name) = std::env::var_os("DOT_AGENT_DECK_E2E_PROBE_GATE") else {
        return;
    };
    let one = std::path::Path::new(&name);
    if one.components().count() != 1 || one.is_absolute() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let root = std::path::Path::new(&home).join(one);
    if std::fs::read_to_string(root.join("phase")).ok().as_deref() != Some("scheduled") {
        return;
    }
    let _ = std::fs::write(root.join("scheduled.arrived"), "");
    let until = Instant::now() + PROBE_GATE_WAIT;
    while Instant::now() < until {
        if root.join("scheduled.release").exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = std::fs::write(root.join("scheduled.timed-out"), "");
}

/// The longest a probe attempt waits for its generation's query-wait
/// estimate (O5) before it is asked again, rather than refused.
#[cfg(target_os = "linux")]
const O5_WAIT: Duration = Duration::from_secs(10);

/// How long the `e2e` build's `scheduled` barrier waits.
#[cfg(all(feature = "e2e", target_os = "linux"))]
const PROBE_GATE_WAIT: Duration = Duration::from_secs(10);

/// One installed generation's handles for a probe, cloned out of the
/// registry under its lock.
#[cfg(target_os = "linux")]
struct ProbeHandles {
    writer: Arc<AsyncMutex<PaneWriter>>,
    gate: Arc<PaneGate>,
    bus: Arc<AgentBus>,
    ledger: Arc<crate::ingress_ledger::IngressLedger>,
    observer: Arc<crate::composer_probe::observe::Observer>,
    agent: Option<AgentType>,
}

impl AgentPtyRegistry {
    #[cfg(target_os = "linux")]
    fn probe_handles(&self, pane_id: &str, agent_id: &str) -> Option<ProbeHandles> {
        let inner = self.inner.lock();
        let agent = inner.agents.get(agent_id)?;
        if agent.pane_id_env.as_deref() != Some(pane_id) {
            return None;
        }
        Some(ProbeHandles {
            writer: Arc::clone(&agent.writer),
            gate: Arc::clone(&agent.gate),
            bus: Arc::clone(&agent.bus),
            ledger: Arc::clone(&agent.ingress),
            observer: agent.composer.clone()?,
            agent: agent
                .spawn_agent_type
                .clone()
                .or_else(|| agent.agent_type.clone()),
        })
    }

    /// PRD #1666 part 3 (Q35-20, R5 §4.2, A4 §4): one probe attempt on
    /// generation `agent_id`, the tick's and the `e2e` build's request
    /// alike. A first or retry attempt consults the generation's schedule
    /// first and does nothing when it isn't due; Claude's lapse retry
    /// supersedes its lapsed probe and asks admission for that. Admission is
    /// asked when it is scheduled, with the facts read there, and again on
    /// the writer thread at the arm section, with the writer's own boundary.
    /// A due attempt refused counts as a failure on the schedule. Answers
    /// once the job was refused, taken back or written, not after the
    /// helper's completion.
    #[cfg(target_os = "linux")]
    pub(crate) async fn composer_probe(
        &self,
        pane_id: &str,
        agent_id: &str,
        ask: ProbeAsk,
    ) -> ProbeOutcome {
        use crate::composer_probe::admission::{Attempt, Outside};
        let Some(h) = self.probe_handles(pane_id, agent_id) else {
            return ProbeOutcome::Unavailable;
        };
        let input = Arc::clone(h.bus.input());
        if !input.has_segments() {
            return ProbeOutcome::Unavailable;
        }
        if ask != ProbeAsk::Lapse && !input.probe_due(Instant::now()) {
            return ProbeOutcome::NotDue;
        }
        if h.observer.permission() == crate::composer_probe::Permission::Pending {
            // Settle it now rather than at the observer's next sweep, and
            // before the keymap read below, which an unsettled generation
            // answers `Unmeasured` (U3-03).
            let observer = Arc::clone(&h.observer);
            let _ = tokio::task::spawn_blocking(move || observer.settle()).await;
        }
        let mut waited = false;
        let (w, keymap) = loop {
            // The keymap and the screen are read with no lock held, and read
            // again after an O5 wait: nothing read before it stands after it
            // (U3-03).
            let keymap = {
                let observer = Arc::clone(&h.observer);
                tokio::task::spawn_blocking(move || observer.keymap())
                    .await
                    .unwrap_or(Err(crate::composer_probe::keymap::Hold::Unmeasured))
            };
            let surface = surface_shows_composer(h.agent.as_ref(), h.bus.clean_frame());
            let w = h.writer.lock().await;
            let now = Instant::now();
            let status = h.gate.gate.status();
            let (fence, lapsed_e0) = input.admission_fence(status.fenced, status.held_ops, now);
            let attempt = match ask {
                ProbeAsk::First => Attempt::First,
                ProbeAsk::Retry => Attempt::Retry,
                ProbeAsk::Lapse => Attempt::Lapse {
                    lapsed_e0: lapsed_e0.unwrap_or(u64::MAX),
                },
            };
            let outside = Outside {
                attempt,
                permission: h.observer.permission(),
                fence,
                human_ticket: h.ledger.any_outstanding(),
                boundary: w.physical_boundary(),
                keymap,
                surface,
            };
            match h.observer.admits(&outside, now) {
                // O5 is availability, not safety (A1 §4.4): an attempt due
                // before the agent's own redraw waits run out is scheduled
                // at the estimate, once, rather than refused.
                Err(crate::composer_probe::admission::Refusal::Estimate(at))
                    if !waited && at.saturating_duration_since(now) <= O5_WAIT =>
                {
                    waited = true;
                    drop(w);
                    tokio::time::sleep_until(at.into()).await;
                    continue;
                }
                _ => break (w, keymap),
            }
        };
        // The frame the screen is read from, and its place in the output:
        // the writer refuses the screen if output was taken since (U3-03).
        let frame = h.bus.clean_frame();
        let seen = frame.as_ref().map(|grid| grid.out_seq);
        let surface = surface_shows_composer(h.agent.as_ref(), frame);
        let now = Instant::now();
        let status = h.gate.gate.status();
        let (fence, lapsed_e0) = input.admission_fence(status.fenced, status.held_ops, now);
        let attempt = match ask {
            ProbeAsk::First => Attempt::First,
            ProbeAsk::Retry => Attempt::Retry,
            ProbeAsk::Lapse => Attempt::Lapse {
                lapsed_e0: lapsed_e0.unwrap_or(u64::MAX),
            },
        };
        let outside = Outside {
            attempt,
            permission: h.observer.permission(),
            fence,
            human_ticket: h.ledger.any_outstanding(),
            boundary: w.physical_boundary(),
            keymap,
            surface,
        };
        if let Err(refusal) = h.observer.admits(&outside, now) {
            tracing::debug!(
                target: "composer_probe",
                agent_id,
                ?ask,
                ?refusal,
                "probe refused when scheduled"
            );
            if ask != ProbeAsk::Lapse {
                input.probe_failed(now);
            }
            return ProbeOutcome::Refused(format!("{refusal:?}"));
        }
        let (number, job) = match input.segment_open_probe(status.held_ops > 0, now) {
            Ok(opened) => opened,
            Err(refusal) => {
                if ask != ProbeAsk::Lapse {
                    input.probe_failed(now);
                }
                return ProbeOutcome::Refused(format!("{refusal:?}"));
            }
        };
        let claim = Arc::new(crate::doorbell_txn::JobClaim::default());
        let section = {
            let observer = Arc::clone(&h.observer);
            let (bus, ledger) = (Arc::clone(&h.bus), Arc::clone(&h.ledger));
            probe_section(
                Arc::clone(&h.gate.gate),
                Arc::clone(&input),
                Arc::clone(&claim),
                move || seen.is_some_and(|seq| bus.evidence().2 == seq),
                move |ctx, status, screen_unchanged| {
                    let writer_outside = writer_outside(
                        outside,
                        ctx,
                        status,
                        ledger.any_outstanding(),
                        screen_unchanged,
                    );
                    observer
                        .arm_segment(number, &writer_outside, ctx.now)
                        .map(drop)
                        .map_err(ProbeRefusal::Admission)
                },
            )
        };
        let disarm = {
            let observer = Arc::clone(&h.observer);
            let input = Arc::clone(&input);
            let gate = Arc::clone(&h.gate.gate);
            move || {
                observer.disarm();
                let _ = input.segment_withdrawn(number);
                gate.open_if_empty();
            }
        };
        let Some(probe) = w.queue_probe(job, claim, section, disarm) else {
            let _ = input.segment_withdrawn(number);
            return ProbeOutcome::Refused("the writer is gone".into());
        };
        drop(w);
        let end = probe.end().await;
        let observer = Arc::clone(&h.observer);
        probe_ended(&input, &h.gate.gate, number, end, ask, move || {
            observer.disarm();
        })
    }

    /// No other platform probes: the `e2e` build's request answers that.
    #[cfg(all(not(target_os = "linux"), feature = "e2e"))]
    pub(crate) async fn composer_probe(
        &self,
        _pane_id: &str,
        _agent_id: &str,
        _ask: ProbeAsk,
    ) -> ProbeOutcome {
        ProbeOutcome::Unavailable
    }

    /// PRD #1666 part 3 (Q35-20): start generation `id`'s release driver,
    /// once its observer runs and its segments are attached. Needs a Tokio
    /// runtime; with none (a unit test's registry) nothing starts.
    #[cfg(target_os = "linux")]
    pub(super) fn start_segment_driver(&self, id: &str) {
        let taken = {
            let inner = self.inner.lock();
            let Some(agent) = inner.agents.get(id) else {
                return;
            };
            let Some(observer) = agent.composer.clone() else {
                return;
            };
            (
                Arc::clone(&agent.writer),
                Arc::clone(agent.bus.input()),
                observer,
            )
        };
        let (writer, input, observer) = taken;
        if !input.has_segments() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let publish: Box<dyn Fn(u64) + Send + Sync> =
            Box::new(move |number| observer.publish_segment(number));
        runtime.spawn(drive_segments(writer, input, publish));
    }
}

/// Whether the screen shows the agent's main composer (admission's tenth
/// conjunct, R3 §4.1.1 and §2.3 item 4): Claude's by its positive identity,
/// the ready screen with no dialog over it at a measured width; Codex's by
/// its placeholder. No clean frame, or no profile, shows none.
#[cfg(target_os = "linux")]
fn surface_shows_composer(
    agent: Option<&AgentType>,
    frame: Option<crate::doorbell_screen::Grid>,
) -> bool {
    let (Some(profile), Some(grid)) = (crate::doorbell_screen::profile_for_agent(agent), frame)
    else {
        return false;
    };
    if grid.cols < profile.min_cols || (profile.dialog)(&grid).is_some() || !(profile.ready)(&grid)
    {
        return false;
    }
    match agent {
        Some(AgentType::Codex) => profile.placeholder_empty(&grid),
        _ => (profile.composer)(&grid).is_some(),
    }
}

/// What the doorbell tick does for an `Armable` generation whose box was
/// touched (Q35-20, U3-02).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickProbe {
    /// A check is outstanding: ring nothing, ask nothing.
    Outstanding,
    /// Nothing outstanding and no attempt due: the tick goes on as today.
    NotDue,
    /// Ask this probe.
    Ask(ProbeAsk),
}

/// The tick's decision from the generation's input and its gate's status at
/// `now`: a check outstanding rings nothing, and asks the retry of its own
/// lapsed Claude probe once the schedule allows it; with nothing
/// outstanding, the schedule's first attempt or retry when one is due.
#[cfg(target_os = "linux")]
pub(crate) fn tick_probe(
    input: &crate::composer_probe::epoch::GenerationInput,
    status: crate::composer_probe::gate::Status,
    now: Instant,
) -> TickProbe {
    let outstanding = status.fenced
        || input
            .segments_front()
            .is_some_and(|front| front != Front::None);
    if outstanding {
        // U3-02: its own lapsed Claude probe, alone with nothing held
        // behind it, is superseded by the schedule's retry once due.
        if input.lapse_due(status.held_ops, now) {
            return TickProbe::Ask(ProbeAsk::Lapse);
        }
        return TickProbe::Outstanding;
    }
    if !input.probe_due(now) {
        return TickProbe::NotDue;
    }
    // A due attempt after a failure is the schedule's retry.
    TickProbe::Ask(if input.probe_retrying() {
        ProbeAsk::Retry
    } else {
        ProbeAsk::First
    })
}

impl AgentPtyRegistry {
    /// PRD #1666 part 3 (Q35-20): the doorbell tick's check of an
    /// `Armable` generation whose box was touched ([`tick_probe`]). Starts
    /// the probe it asks, off the tick, one request at a time; `true` while
    /// one is started or on its way or a check is outstanding (the tick then
    /// rings nothing this time), `false` when the tick goes on as it does
    /// today: not `Armable`, untouched, or no probe due. A refused retry of
    /// a lapsed probe counts as a failure, as a refused due attempt does.
    pub(crate) fn composer_probe_tick(self: &Arc<Self>, pane_id: &str, agent_id: &str) -> bool {
        #[cfg(target_os = "linux")]
        {
            let Some(h) = self.probe_handles(pane_id, agent_id) else {
                return false;
            };
            let input = h.bus.input();
            if !input.has_segments()
                || h.observer.permission() != crate::composer_probe::Permission::Armable
                || !input.touched()
            {
                return false;
            }
            let ask = match tick_probe(input, h.gate.gate.status(), Instant::now()) {
                TickProbe::Outstanding => return true,
                TickProbe::NotDue => return false,
                TickProbe::Ask(ask) => ask,
            };
            let Some(request) = input.begin_probe_request() else {
                return true;
            };
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                return false;
            };
            let registry = Arc::clone(self);
            let input = Arc::clone(input);
            let (pane, agent) = (pane_id.to_string(), agent_id.to_string());
            runtime.spawn(async move {
                let _request = request;
                let outcome = registry.composer_probe(&pane, &agent, ask).await;
                if ask == ProbeAsk::Lapse && matches!(outcome, ProbeOutcome::Refused(_)) {
                    input.probe_failed(Instant::now());
                }
                tracing::info!(
                    target: "composer_probe",
                    pane_id = %pane,
                    agent_id = %agent,
                    ?outcome,
                    "doorbell tick probe"
                );
            });
            true
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pane_id, agent_id);
            false
        }
    }
}

/// How long a hold lasts before the card shows it (R5 §10.1's display
/// timer): a check that settles sooner shows nothing. Display only.
pub(crate) const HELD_SHOWN_AFTER: Duration = Duration::from_millis(300);

/// How long a hold lasts before the card says what it waits on, rather than
/// that a check is running. Display only: it releases nothing.
pub(crate) const HELD_EXPLAINED_AFTER: Duration = Duration::from_secs(5);

/// The card's cause for typing held `age` so far, with `full_waiters` ops
/// waiting for room and `front` the open segment at the front, if any (core
/// §7). Display only.
pub(crate) fn held_cause(
    full_waiters: usize,
    age: Duration,
    front: Option<&crate::composer_probe::segments::View>,
) -> crate::inbox_display::InputHeldCause {
    use crate::composer_probe::segments::{Kind, Stage};
    use crate::inbox_display::InputHeldCause;
    if full_waiters > 0 {
        return InputHeldCause::QueueFull;
    }
    if age < HELD_EXPLAINED_AFTER {
        return InputHeldCause::Checking;
    }
    match front {
        None => InputHeldCause::Checking,
        Some(view) if view.kind == Kind::HumanG => InputHeldCause::AfterYourCtrlG,
        Some(view) => match view.stage {
            Stage::Waiting | Stage::Submitted | Stage::Unmatched | Stage::Lapsed => {
                InputHeldCause::CheckNotOpened
            }
            Stage::Matched
            | Stage::Mismatched
            | Stage::AwaitingPostFlush
            | Stage::AwaitingReturn
            | Stage::Released => InputHeldCause::CheckNotReturned,
        },
    }
}

impl AgentPtyRegistry {
    /// PRD #1666 part 3 (Q35-20, R5 §10.1): what generation `agent_id`'s
    /// card says of the typing its gate holds: nothing until it has been held
    /// [`HELD_SHOWN_AFTER`], then its counts, since when, and why (see
    /// [`held_cause`]). Display only.
    pub(crate) fn input_held(&self, agent_id: &str) -> Option<crate::inbox_display::InputHeld> {
        let generation: u64 = agent_id.parse().ok()?;
        let (gate, input) = {
            let inner = self.inner.lock();
            let agent = inner.agents.get(agent_id)?;
            (Arc::clone(&agent.gate), Arc::clone(agent.bus.input()))
        };
        let status = gate.gate.status();
        let since = status.held_since?;
        let now = Instant::now();
        let age = now.saturating_duration_since(since);
        if age < HELD_SHOWN_AFTER {
            return None;
        }
        let view = input.segments_view(now);
        let since_ms = std::time::SystemTime::now()
            .checked_sub(age)
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Some(crate::inbox_display::InputHeld {
            generation,
            cause: held_cause(status.full_waiters, age, view.first()),
            held_ops: u32::try_from(status.held_ops).unwrap_or(u32::MAX),
            held_bytes: u32::try_from(status.held_bytes).unwrap_or(u32::MAX),
            since_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_probe::record::ScopeState;
    use crate::composer_probe::segments::{Kind, Stage, View};
    use crate::inbox_display::InputHeldCause;
    use spec::spec;

    fn view(kind: Kind, stage: Stage) -> View {
        View {
            number: Some(1),
            kind,
            stage,
            record: None,
            fact: None,
            scope: ScopeState::Closed,
            ec: false,
            not_returned: false,
        }
    }

    /// An installed generation of `agent` with its records and segments.
    fn live(
        agent: crate::composer_probe::segments::Agent,
    ) -> Arc<crate::composer_probe::epoch::GenerationInput> {
        use crate::composer_probe::record::{EcPolicy, Records};
        let input = Arc::new(crate::composer_probe::epoch::GenerationInput::default());
        assert!(input.attach_records(Records::new(5, EcPolicy::Hold, Instant::now())));
        assert!(input.attach_segments(5, agent));
        input
    }

    /// Scenario: The doorbell tick finds its own Claude probe written more than the trigger's wait ago with no helper seen: lapsed, alone, nothing held behind it. The first tick counts the lapse as a failed attempt and asks nothing; once the schedule's backoff has passed it asks the lapse retry, and asks it again on the next tick without counting the same lapse twice. With typing held behind the lapsed probe it never asks; a Codex probe written as long ago never lapses; and while one request is on its way the tick can start no other.
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_the_tick_retries_its_own_lapsed_claude_probe() {
        use crate::composer_probe::gate::Status;
        use crate::composer_probe::probe::BACKOFF;
        use crate::composer_probe::query_ledger::T_TRIGGER;
        use crate::composer_probe::segments::Agent;
        let now = Instant::now();
        let Some(at) = now.checked_sub(T_TRIGGER + Duration::from_secs(1)) else {
            return;
        };
        let fenced = Status {
            fenced: true,
            ..Status::default()
        };
        let held = Status {
            held_ops: 1,
            held_bytes: 1,
            ..fenced
        };
        for agent in [Agent::Claude, Agent::Codex] {
            let input = live(agent);
            let (number, _) = input.segment_open_probe(false, now).unwrap();
            input.segment_written(number, 0, at, now).unwrap();
            assert_eq!(
                tick_probe(&input, fenced, now),
                TickProbe::Outstanding,
                "{agent:?}: a lapse is a failed attempt first"
            );
            input.advance_schedule_for_e2e(BACKOFF[0], now);
            assert_eq!(
                tick_probe(&input, held, now),
                TickProbe::Outstanding,
                "{agent:?}: typing waits behind it"
            );
            let retry = match agent {
                Agent::Claude => TickProbe::Ask(ProbeAsk::Lapse),
                _ => TickProbe::Outstanding,
            };
            assert_eq!(tick_probe(&input, fenced, now), retry, "{agent:?}");
            assert_eq!(
                tick_probe(&input, fenced, now),
                retry,
                "{agent:?}: the same lapse counts once"
            );
        }
        let input = live(Agent::Claude);
        let request = input.begin_probe_request().expect("the first request");
        assert!(input.begin_probe_request().is_none(), "one at a time");
        drop(request);
        assert!(
            input.begin_probe_request().is_some(),
            "free once it answered"
        );
    }

    /// Scenario: A generation's probe attempts fail three times in a row, so its observer is broken and its next attempt waits the backoff. A person types a key: one fresh attempt is due at once. A terminal's report alone, or an empty op, makes none due.
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_a_persons_key_makes_a_broken_observers_attempt_due() {
        use crate::composer_probe::epoch::Writer;
        use crate::composer_probe::segments::Agent;
        let input = live(Agent::Claude);
        let now = Instant::now();
        for _ in 0..crate::composer_probe::probe::BROKEN_AFTER {
            input.probe_failed(now);
        }
        assert!(!input.probe_due(now), "broken, waiting its backoff");
        input.enqueue(Writer::Human, b"");
        assert!(!input.probe_due(now), "an empty op is no typing");
        input.enqueue(Writer::Human, b"x");
        assert!(
            input.probe_due(now),
            "a person's key: one attempt due at once"
        );
    }

    /// Scenario: A probe was scheduled with no person's ticket outstanding and the main composer on screen. When the writer comes to its arm section, a person's op has been admitted to the ledger and the agent has drawn output since that frame: the writer asks admission with the ticket it reads now and with no main composer, so admission refuses. With neither change it asks with the scheduled facts, its own boundary and the gate's fence (U3-03, O-20.7's writer facts).
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/059")]
    #[test]
    fn probe_059_the_writer_asks_admission_with_the_facts_it_reads_itself() {
        use crate::composer_probe::admission::{Attempt, Fence, Outside};
        use crate::composer_probe::gate::Status;
        use crate::composer_probe::planner::Boundary;
        let scheduled = Outside {
            attempt: Attempt::First,
            permission: crate::composer_probe::Permission::Armable,
            fence: Fence::Open,
            human_ticket: false,
            boundary: Boundary::Inside,
            keymap: Ok(()),
            surface: true,
        };
        let ctx = crate::doorbell_txn::StartCtx {
            boundary: Boundary::Ground,
            retired: false,
            now: Instant::now(),
        };
        let open = Status::default();
        let same = writer_outside(scheduled, &ctx, open, false, true);
        assert_eq!(same.boundary, Boundary::Ground, "the writer's own boundary");
        assert_eq!(same.fence, Fence::Open);
        assert!(!same.human_ticket && same.surface && same.keymap.is_ok());
        let now = writer_outside(scheduled, &ctx, open, true, true);
        assert!(now.human_ticket, "a ticket admitted since scheduling");
        let drawn = writer_outside(scheduled, &ctx, open, false, false);
        assert!(
            !drawn.surface,
            "output since the frame the screen was read from"
        );
    }

    /// Scenario: Read the probe request's source: a pending permission is settled before the keymap is read, and the keymap is read inside the loop that waits for the agent's redraw estimate, so an attempt that waited asks admission with a keymap read after its wait, not before it (U3-03).
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/059")]
    #[test]
    fn probe_059_the_keymap_is_read_after_settlement_and_after_the_redraw_wait() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/agent_pty_segments.rs"),
        )
        .unwrap();
        let request = source
            .split("pub(crate) async fn composer_probe(")
            .nth(1)
            .and_then(|rest| rest.split("let claim = Arc::new(").next())
            .expect("the probe request");
        let settle = request.find("observer.settle()").expect("the settlement");
        let keymap = request.find("observer.keymap()").expect("the keymap read");
        let wait = request.find("loop {").expect("the redraw wait");
        assert!(
            settle < keymap,
            "the permission is settled before the keymap read"
        );
        assert!(wait < keymap, "the keymap is read again after a wait");
        assert_eq!(request.matches("observer.keymap()").count(), 1);
    }

    /// Scenario: Map held typing to the card's cause across ages, waiters and the front segment's kind and stage. A full queue says so at any age; under five seconds it says a check is running; after that a probe not yet opened, one opened and not returned, and a person's own Ctrl+G each get their own words, and with no segment it still says checking.
    #[spec("inbox/probe/046")]
    #[test]
    fn probe_046_held_typing_names_what_it_waits_on() {
        let late = HELD_EXPLAINED_AFTER;
        let early = HELD_SHOWN_AFTER;
        assert_eq!(
            held_cause(1, early, None),
            InputHeldCause::QueueFull,
            "a full queue at any age"
        );
        assert_eq!(
            held_cause(0, early, Some(&view(Kind::Probe, Stage::Unmatched))),
            InputHeldCause::Checking
        );
        assert_eq!(held_cause(0, late, None), InputHeldCause::Checking);
        for stage in [Stage::Submitted, Stage::Unmatched, Stage::Lapsed] {
            assert_eq!(
                held_cause(0, late, Some(&view(Kind::Probe, stage))),
                InputHeldCause::CheckNotOpened,
                "{stage:?}"
            );
        }
        for stage in [
            Stage::Matched,
            Stage::AwaitingReturn,
            Stage::AwaitingPostFlush,
            Stage::Released,
        ] {
            assert_eq!(
                held_cause(0, late, Some(&view(Kind::Probe, stage))),
                InputHeldCause::CheckNotReturned,
                "{stage:?}"
            );
        }
        assert_eq!(
            held_cause(0, late, Some(&view(Kind::HumanG, Stage::Matched))),
            InputHeldCause::AfterYourCtrlG
        );
    }
}
