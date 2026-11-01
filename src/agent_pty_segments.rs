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
//! - the screen: the frame read when scheduled, with its layout epoch and
//!   output position in the same hold, stands only while the evidence is
//!   still Clean in that epoch with no output taken since (R-D-U2-01: a
//!   layout edge or an unknown geometry invalidates it without moving the
//!   output), read on the writer thread just before the gate's lock, as the
//!   output hook's facts are: output after it and before the byte could as
//!   well land just after the byte.
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
//! writer thread: bus (the screen's status, epoch and `out_seq`, released) ; then
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
//! The gate's mutex is a plain mutex: [`crate::lock_rank`] doesn't rank or
//! check it, so its place above (under the pane writer's async lock, before
//! the seam and R6, the ledger a leaf under it) is this diagram's word, not
//! a debug-build check (the audit's U3-9d; ranking it is on the activation
//! list).
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

/// The probe request's reads when it is scheduled (U3-03, O-20.7b): a
/// pending permission is settled first (`settle`); then each pass reads the
/// keymap (`read`, file I/O with no lock held) and asks with it (`ask`, which
/// reads the rest and answers with what it holds and the O5 estimate that
/// alone refused it, if one did). O5 is availability, not safety (A1 §4.4):
/// a pass refused only by an estimate within [`O5_WAIT`] sleeps to it, once,
/// and passes again, so no keymap read before the sleep stands after it.
/// The last pass's answer and the keymap it asked with.
#[cfg(target_os = "linux")]
pub(super) async fn scheduling_reads<K, T, RF, AF>(
    settle: Option<impl std::future::Future<Output = ()>>,
    mut read: impl FnMut() -> RF,
    mut ask: impl FnMut(K) -> AF,
) -> (T, K)
where
    K: Copy,
    RF: std::future::Future<Output = K>,
    AF: std::future::Future<Output = (T, Option<Instant>)>,
{
    if let Some(settle) = settle {
        settle.await;
    }
    let mut waited = false;
    loop {
        let keymap = read().await;
        let (answer, estimate) = ask(keymap).await;
        match estimate {
            Some(at) if !waited && at.saturating_duration_since(Instant::now()) <= O5_WAIT => {
                waited = true;
                drop(answer);
                tokio::time::sleep_until(at.into()).await;
            }
            _ => return (answer, keymap),
        }
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

/// Whether the Clean frame the screen was read from when the probe was
/// scheduled (`mark`, read with it in one hold) still stands on `bus`: the
/// state still Clean, in the same layout epoch, with no output taken since
/// (U3-03, R-D-U2-01). A layout edge, a failed nudge restore or a learned
/// profile invalidates the frame without moving the output position; an
/// elapsed O5 estimate is no replacement frame. No frame, none stands.
#[cfg(target_os = "linux")]
pub(super) fn screen_fresh(bus: &AgentBus, mark: Option<crate::evidence_state::FrameMark>) -> bool {
    mark.is_some_and(|mark| bus.frame_unchanged(mark))
}

/// The writer's own reads at a probe's arm section, as
/// [`AgentPtyRegistry::composer_probe`] hands them to [`probe_section`]
/// (O-20.7, U3-03, the audit's U3-7): `fresh`, the screen's freshness on
/// `bus` against the scheduled frame's `mark` ([`screen_fresh`]), read on
/// the writer thread before the gate's lock; and `facts`, which, under the
/// gate's lock, reads a person's ticket from `ledger` and asks
/// [`writer_outside`] for the outside facts the arm asks admission with.
#[cfg(target_os = "linux")]
#[allow(clippy::type_complexity)]
pub(super) fn writer_facts(
    bus: Arc<AgentBus>,
    mark: Option<crate::evidence_state::FrameMark>,
    ledger: Arc<crate::ingress_ledger::IngressLedger>,
    scheduled: crate::composer_probe::admission::Outside,
) -> (
    impl FnOnce() -> bool + Send + 'static,
    impl FnOnce(
        &crate::doorbell_txn::StartCtx,
        crate::composer_probe::gate::Status,
        bool,
    ) -> crate::composer_probe::admission::Outside
    + Send
    + 'static,
) {
    (
        move || screen_fresh(&bus, mark),
        move |ctx: &crate::doorbell_txn::StartCtx, status, screen_unchanged| {
            writer_outside(
                scheduled,
                ctx,
                status,
                ledger.any_outstanding(),
                screen_unchanged,
            )
        },
    )
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
    agent_id: &str,
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
                agent_id,
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
        // Settle a pending permission now rather than at the observer's next
        // sweep, and before the keymap read, which an unsettled generation
        // answers `Unmeasured` (U3-03).
        let settle =
            (h.observer.permission() == crate::composer_probe::Permission::Pending).then(|| {
                let observer = Arc::clone(&h.observer);
                async move {
                    let _ = tokio::task::spawn_blocking(move || observer.settle()).await;
                }
            });
        let (hr, inp) = (&h, &input);
        let (w, keymap) = scheduling_reads(
            settle,
            || {
                let observer = Arc::clone(&hr.observer);
                async move {
                    tokio::task::spawn_blocking(move || observer.keymap())
                        .await
                        .unwrap_or(Err(crate::composer_probe::keymap::Hold::Unmeasured))
                }
            },
            move |keymap| async move {
                let surface = surface_shows_composer(hr.agent.as_ref(), hr.bus.clean_frame());
                let w = hr.writer.lock().await;
                let now = Instant::now();
                let status = hr.gate.gate.status();
                let (fence, lapsed_e0) = inp.admission_fence(status.fenced, status.held_ops, now);
                let attempt = match ask {
                    ProbeAsk::First => Attempt::First,
                    ProbeAsk::Retry => Attempt::Retry,
                    ProbeAsk::Lapse => Attempt::Lapse {
                        lapsed_e0: lapsed_e0.unwrap_or(u64::MAX),
                    },
                };
                let outside = Outside {
                    attempt,
                    permission: hr.observer.permission(),
                    fence,
                    human_ticket: hr.ledger.any_outstanding(),
                    boundary: w.physical_boundary(),
                    keymap,
                    surface,
                };
                let estimate = match hr.observer.admits(&outside, now) {
                    Err(crate::composer_probe::admission::Refusal::Estimate(at)) => Some(at),
                    _ => None,
                };
                (w, estimate)
            },
        )
        .await;
        // The frame the screen is read from, with its layout epoch and place
        // in the output, in one hold: the writer refuses the screen unless it
        // still stands (U3-03, R-D-U2-01).
        let marked = h.bus.clean_frame_marked();
        let mark = marked.as_ref().map(|(_, mark)| *mark);
        let surface = surface_shows_composer(h.agent.as_ref(), marked.map(|(grid, _)| grid));
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
            let (fresh, facts) =
                writer_facts(Arc::clone(&h.bus), mark, Arc::clone(&h.ledger), outside);
            probe_section(
                Arc::clone(&h.gate.gate),
                Arc::clone(&input),
                Arc::clone(&claim),
                fresh,
                move |ctx, status, screen_unchanged| {
                    observer
                        .arm_segment(number, &facts(ctx, status, screen_unchanged), ctx.now)
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
        probe_ended(
            &input,
            &h.gate.gate,
            agent_id,
            number,
            end,
            ask,
            move || {
                observer.disarm();
            },
        )
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

/// Whether a card shows typing held `age` so far, with `full_waiters` ops
/// waiting for room (core §7): at the cap at once, since the user's typing is
/// being paused and they need to know straight away (Q110b.3); otherwise once
/// it has been held [`HELD_SHOWN_AFTER`], so a check that settles sooner
/// doesn't flicker. Display only.
pub(crate) fn held_shown(full_waiters: usize, age: Duration) -> bool {
    full_waiters > 0 || age >= HELD_SHOWN_AFTER
}

/// The card's cause for typing held `age` so far, with `full_waiters` ops
/// waiting for room and `front` the open segment at the front, if any (core
/// §7). Display only.
///
/// Implementation notes on §7's causes:
/// - `QueueFull` shows at any age, as soon as an op waits for room
///   ([`held_shown`]).
/// - `AfterYourCtrlG` shows for a person's Ctrl+G segment at the front at
///   any stage once the hold is [`HELD_EXPLAINED_AFTER`] old, not only while
///   it is unmatched: the words stay true while that segment is the front one
///   (accepted in Q110b.4).
/// - `CheckBlocked` is not returned here: nothing holds keys in that state
///   (R5), so no hold reaches this mapping with it.
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
    /// card says of the typing its gate holds: nothing until [`held_shown`]
    /// (held [`HELD_SHOWN_AFTER`], or at once at the cap), then its counts,
    /// since when, and why (see [`held_cause`]). Display only.
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
        if !held_shown(status.full_waiters, age) {
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

    /// PRD #1666 part 3 (Q35-20e): every pane's current generation — the one
    /// its pane hasn't handed over — with what its card shows of held typing,
    /// and when the next timer may change that ([`HeldDisplayPass`]).
    pub(crate) fn held_display_pass(&self, now: Instant) -> HeldDisplayPass {
        // The gates are read after the registry's lock is let go, as
        // `input_held` reads them.
        let current: Vec<(String, String, Arc<PaneGate>)> = {
            let inner = self.inner.lock();
            inner
                .agents
                .iter()
                .filter(|(_, agent)| !agent.pane_retired.load(Ordering::SeqCst))
                .filter_map(|(id, agent)| {
                    agent
                        .pane_id_env
                        .clone()
                        .map(|pane| (pane, id.clone(), Arc::clone(&agent.gate)))
                })
                .collect()
        };
        let mut pass = HeldDisplayPass::default();
        for (pane, agent_id, gate) in current {
            let since = gate.held_status().held_since;
            let held = self.input_held(&agent_id);
            let discarded = self.input_discarded(&agent_id);
            let wake = held_wake(now, since, held.is_some(), discarded.is_some());
            pass.next = match (pass.next, wake) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            pass.panes.push(crate::event::InputHeldUpdate {
                pane_id: pane,
                agent_id,
                input_held: held,
                input_discarded: discarded,
            });
        }
        pass
    }
}

/// PRD #1666 part 3 (Q35-20e): when a timer may next change what a card
/// shows of a generation's held typing, held since `since` (`None` while
/// nothing is), with a hold `shown` and a not-delivered notice `discarded` on
/// it as of `now`: when a hold reaches [`HELD_SHOWN_AFTER`], then
/// [`HELD_EXPLAINED_AFTER`], with [`HELD_DISPLAY_POLL`] while it shows and
/// [`DISCARDED_DISPLAY_POLL`] while the notice does. `None` when only a gate's
/// move can change it.
fn held_wake(
    now: Instant,
    since: Option<Instant>,
    shown: bool,
    discarded: bool,
) -> Option<Instant> {
    let age = since.map(|since| (since, now.saturating_duration_since(since)));
    match age {
        Some((since, age)) if !shown && age < HELD_SHOWN_AFTER => Some(since + HELD_SHOWN_AFTER),
        Some((since, age)) if shown && age < HELD_EXPLAINED_AFTER => {
            Some((since + HELD_EXPLAINED_AFTER).min(now + HELD_DISPLAY_POLL))
        }
        // The check's stage moves the cause without moving the gate.
        _ if shown => Some(now + HELD_DISPLAY_POLL),
        // A keystroke reaching the pane ends the notice.
        _ if discarded => Some(now + DISCARDED_DISPLAY_POLL),
        _ => None,
    }
}

/// While a hold shows, how often its cause is read again: the check's stage
/// can change it without the gate moving.
const HELD_DISPLAY_POLL: Duration = Duration::from_millis(250);

/// While a not-delivered notice shows, how often the pane is checked for a
/// keystroke that ends it.
const DISCARDED_DISPLAY_POLL: Duration = Duration::from_secs(1);

/// PRD #1666 part 3 (Q35-20e): one read of what every pane's card shows of
/// held typing ([`AgentPtyRegistry::held_display_pass`]).
#[derive(Debug, Default)]
pub(crate) struct HeldDisplayPass {
    /// Each pane's current generation and what its card shows.
    pub(crate) panes: Vec<crate::event::InputHeldUpdate>,
    /// When a timer may next change what a card shows; `None` when only a
    /// gate's move can ([`AgentPtyRegistry::held_display`]).
    pub(crate) next: Option<Instant>,
}

/// PRD #1666 part 3 (Q35-20e): what a client was last told of a pane's held
/// typing, by the pane.
#[derive(Debug, Default)]
pub(crate) struct HeldDisplayShown(HashMap<String, ShownHeld>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShownHeld {
    agent_id: String,
    cause: Option<crate::inbox_display::InputHeldCause>,
    discarded: Option<u64>,
}

impl ShownHeld {
    fn of(update: &crate::event::InputHeldUpdate) -> Self {
        Self {
            agent_id: update.agent_id.clone(),
            cause: update.input_held.as_ref().map(|held| held.cause),
            discarded: update.input_discarded.as_ref().map(|d| d.generation),
        }
    }

    fn shows(&self) -> bool {
        self.cause.is_some() || self.discarded.is_some()
    }
}

impl HeldDisplayShown {
    /// The updates `pass` owes clients: a pane whose card now shows
    /// something else, and a pane that showed something and has no current
    /// generation now (its last one's, cleared). A generation first seen
    /// showing nothing owes nothing.
    pub(crate) fn changes(&mut self, pass: HeldDisplayPass) -> Vec<crate::event::InputHeldUpdate> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for update in pass.panes {
            let now = ShownHeld::of(&update);
            seen.insert(update.pane_id.clone());
            let before = self.0.insert(update.pane_id.clone(), now.clone());
            match before {
                Some(before) if before == now => {}
                Some(before) if !now.shows() && before.shows() => {
                    out.push(crate::event::InputHeldUpdate {
                        pane_id: update.pane_id.clone(),
                        agent_id: before.agent_id.clone(),
                        input_held: None,
                        input_discarded: None,
                    });
                    if before.agent_id != now.agent_id {
                        out.push(update);
                    }
                }
                None if !now.shows() => {}
                _ => out.push(update),
            }
        }
        self.0.retain(|pane, before| {
            if seen.contains(pane) {
                return true;
            }
            if before.shows() {
                out.push(crate::event::InputHeldUpdate {
                    pane_id: pane.clone(),
                    agent_id: before.agent_id.clone(),
                    input_held: None,
                    input_discarded: None,
                });
            }
            false
        });
        out
    }
}

/// PRD #1666 part 3 (Q35-20e): tell attached clients, through `event_tx`,
/// whenever what a pane's card shows of the typing the deck holds changes:
/// when a hold has lasted [`HELD_SHOWN_AFTER`], when its cause changes
/// ([`HELD_EXPLAINED_AFTER`], the queue filling, the check moving), when it
/// ends, and when a generation retired holding typing its successor's card
/// reports. Waits on the registry's display notice while no timer is due, so
/// a deck holding nothing runs no timer. Display only: it releases nothing.
pub async fn watch_input_held(
    registry: Arc<AgentPtyRegistry>,
    event_tx: tokio::sync::broadcast::Sender<crate::event::BroadcastMsg>,
) {
    let notify = registry.held_display();
    let mut shown = HeldDisplayShown::default();
    loop {
        // Before the pass, so a change made during it leaves a permit.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let pass = registry.held_display_pass(Instant::now());
        let next = pass.next;
        for update in shown.changes(pass) {
            let _ = event_tx.send(crate::event::BroadcastMsg::InputHeld(update));
        }
        match next {
            Some(at) => {
                tokio::select! {
                    () = &mut notified => {}
                    () = tokio::time::sleep_until(at.into()) => {}
                }
            }
            None => notified.await,
        }
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

    /// Scenario: A Claude probe is written with no helper seen and nothing held, and lapses: the tick counts the lapse as one failed attempt, so the retry waits the first backoff step. Then a late record for that same probe arrives with a failed check (Unarmed, Tool or an unknown decision): the one attempt still counts once, with the first step's deadline. A late record whose export the deck read still settles it: read empty, the schedule is due again at once; read non-empty, the failure is cleared and the recheck waits on the box (R-D-U2-02).
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_a_lapsed_probes_late_failed_record_counts_its_one_attempt_once() {
        use crate::composer_probe::gate::Status;
        use crate::composer_probe::probe::BACKOFF;
        use crate::composer_probe::query_ledger::T_TRIGGER;
        use crate::composer_probe::record::{
            Attribution, Channel, Decision, Observation, RecordKey,
        };
        use crate::composer_probe::segments::Agent;
        let now = Instant::now();
        let Some(at) = now.checked_sub(T_TRIGGER + Duration::from_secs(1)) else {
            return;
        };
        let fenced = Status {
            fenced: true,
            ..Status::default()
        };
        let failures = |input: &crate::composer_probe::epoch::GenerationInput| {
            input
                .advance_schedule_for_e2e(Duration::ZERO, now)
                .expect("a live schedule")
                .1
        };
        // A late record of `decision` for the lapsed probe; an `Armed` one
        // with the daemon's read of its export, `empty` or not.
        let late = |input: &crate::composer_probe::epoch::GenerationInput,
                    number: u64,
                    decision: Decision,
                    empty: bool| {
            let key = RecordKey {
                pid: 4242,
                start: 1,
            };
            if decision == Decision::Armed {
                input.note_read(key, empty);
            }
            let _ = input.record_observe(
                &Observation {
                    generation: 5,
                    key,
                    channel: Channel::Ipc,
                    attribution: Attribution::Owned,
                    claim: None,
                    decision: Some(decision),
                    seg_seen: Some(number),
                    native_seen: None,
                    birth_floor: at,
                },
                now,
            );
        };
        let lapsed = || {
            let input = live(Agent::Claude);
            let (number, _) = input.segment_open_probe(false, now).unwrap();
            input.segment_written(number, 0, at, now).unwrap();
            assert_eq!(tick_probe(&input, fenced, now), TickProbe::Outstanding);
            assert_eq!(failures(&input), 1, "the lapse is one failure");
            (input, number)
        };
        for decision in [Decision::Unarmed, Decision::Tool, Decision::Unknown] {
            let (input, number) = lapsed();
            late(&input, number, decision, false);
            assert_eq!(
                failures(&input),
                1,
                "{decision:?}: the lapsed attempt's late failed check counts it once, not twice"
            );
            assert!(
                !input.probe_due(now + BACKOFF[0] - Duration::from_millis(1)),
                "{decision:?}: not due before the first step's deadline"
            );
            assert!(
                input.probe_due(now + BACKOFF[0]),
                "{decision:?}: due at the first step's deadline, not the second's"
            );
        }
        // The controls: a late read still settles it, empty or not.
        let (input, number) = lapsed();
        late(&input, number, Decision::Armed, true);
        assert_eq!(failures(&input), 0, "a late empty read settles the probe");
        assert!(input.probe_due(now), "empty: due again at once");
        let (input, number) = lapsed();
        late(&input, number, Decision::Armed, false);
        assert_eq!(failures(&input), 0, "a late non-empty read settles it too");
        assert!(
            !input.probe_due(now + BACKOFF[0]),
            "non-empty: its recheck waits on the box, not the failure's backoff"
        );
    }

    /// Scenario: For Claude and Codex, a probe is written past the trigger's wait with no helper seen and nothing held behind it, and the agent may be waiting for a cursor report (`B` holds). Only Claude's reads as lapsed, so only Claude's lapse retry reaches admission's lapse checks, where `B` refuses it; Codex's is refused as having no lapsed probe to supersede, before any lapse check, whatever `B` says (O-20.7c: the lapse retry's checks are Claude's alone, though an attempt carries no agent).
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_only_a_claude_probe_reaches_the_lapse_retrys_checks() {
        use crate::composer_probe::admission::{Attempt, Outside, Refusal};
        use crate::composer_probe::epoch::OutputNote;
        use crate::composer_probe::planner::Boundary;
        use crate::composer_probe::query_ledger::{QueryKind, T_TRIGGER};
        use crate::composer_probe::segments::Agent;
        let now = Instant::now();
        let Some(at) = now.checked_sub(T_TRIGGER + Duration::from_secs(1)) else {
            return;
        };
        for (agent, refusal) in [
            (Agent::Claude, Refusal::Backlog),
            (Agent::Codex, Refusal::NotLapsed),
        ] {
            let input = live(agent);
            let (number, job) = input.segment_open_probe(false, now).unwrap();
            // Its own byte, through the input, as the writer enqueues it.
            input.enqueue(crate::composer_probe::epoch::Writer::Deck(job), b"\x07");
            input.segment_written(number, 0, at, now).unwrap();
            input.on_output(&[OutputNote::Query(QueryKind::Cpr)], now);
            assert!(input.backlog(), "{agent:?}: `B` holds");
            let (fence, lapsed_e0) = input.admission_fence(true, 0, now);
            let lapse = Outside {
                attempt: Attempt::Lapse {
                    lapsed_e0: lapsed_e0.unwrap_or(u64::MAX),
                },
                permission: crate::composer_probe::Permission::Armable,
                fence,
                human_ticket: false,
                boundary: Boundary::Ground,
                keymap: Ok(()),
                surface: true,
            };
            assert_eq!(input.admit_probe(&lapse, now), Err(refusal), "{agent:?}");
        }
    }

    /// Scenario: A generation's probe attempts fail three times in a row, so its observer is broken and its next attempt waits the backoff. An empty op makes none due, nor does a terminal's whole reply to a query the deck saw the agent ask (an exempt report). A person's key makes one fresh attempt due at once; so does a terminal reply the deck can't exempt (a duplicate of one it already answered), which counts as typing.
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_a_persons_key_makes_a_broken_observers_attempt_due() {
        use crate::composer_probe::epoch::{Enqueued, OutputNote, Writer};
        use crate::composer_probe::query_ledger::QueryKind;
        use crate::composer_probe::segments::Agent;
        const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
        let broken = || {
            let input = live(Agent::Claude);
            let now = Instant::now();
            for _ in 0..crate::composer_probe::probe::BROKEN_AFTER {
                input.probe_failed(now);
            }
            assert!(!input.probe_due(now), "broken, waiting its backoff");
            (input, now)
        };
        let (input, now) = broken();
        input.enqueue(Writer::Human, b"");
        assert!(!input.probe_due(now), "an empty op is no typing");
        input.on_output(&[OutputNote::Query(QueryKind::Da1)], now);
        assert!(
            matches!(input.enqueue(Writer::Human, DA1_REPLY), Enqueued::Exempt(_)),
            "the control: the reply is exempt"
        );
        assert!(!input.probe_due(now), "an exempt report is no typing");
        assert!(
            matches!(input.enqueue(Writer::Human, DA1_REPLY), Enqueued::Moved(_)),
            "a duplicate reply isn't exempt"
        );
        assert!(
            input.probe_due(now),
            "an unexempt terminal reply counts as typing: one attempt due"
        );
        let (input, now) = broken();
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

    /// A Claude Code frame at `cols`: the full-width rules around an empty
    /// composer, after Claude Code's full-redraw marker.
    #[cfg(target_os = "linux")]
    fn claude_frame(cols: u16) -> Vec<u8> {
        let rule = "─".repeat(cols as usize);
        format!("\x1b[2J\x1b[H\x1b[20;1H{rule}\x1b[21;1H❯\u{a0}\x1b[22;1H{rule}\x1b[23;3Hstatus\x1b[21;3H")
            .into_bytes()
    }

    /// One probe scheduled from a Clean Claude Code frame on a real bus, then
    /// `between` run before its arm section, which runs as the live writer
    /// runs it ([`probe_section`] with [`writer_facts`]) once the O5
    /// estimate, if the generation has one, has elapsed. The seam's arm is
    /// stood in for by the generation's own admission (`admit_probe`), which
    /// it asks first. The section's outcome and whether the fence closed.
    #[cfg(target_os = "linux")]
    fn arm_after(
        between: impl FnOnce(&AgentBus, &Arc<crate::ingress_ledger::IngressLedger>),
    ) -> (Result<(u64, Instant), ProbeRefusal>, bool) {
        use crate::composer_probe::admission::{Attempt, Fence, Outside};
        use crate::composer_probe::planner::Boundary;
        use crate::composer_probe::record::{EcPolicy, Records};
        let bus = Arc::new(AgentBus::profiled(
            Some(&crate::doorbell_screen::CLAUDE_PROFILE),
            24,
            80,
        ));
        let input = Arc::clone(bus.input());
        assert!(input.attach_records(Records::new(5, EcPolicy::Hold, Instant::now())));
        bus.push(claude_frame(80));
        // Scheduled: the frame and where it stands, in one hold.
        let (grid, mark) = bus.clean_frame_marked().expect("a Clean frame");
        let scheduled = Outside {
            attempt: Attempt::First,
            permission: crate::composer_probe::Permission::Armable,
            fence: Fence::Open,
            human_ticket: false,
            boundary: Boundary::Ground,
            keymap: Ok(()),
            surface: surface_shows_composer(Some(&AgentType::ClaudeCode), Some(grid)),
        };
        assert!(scheduled.surface, "scheduled at the main composer");
        let ledger = crate::ingress_ledger::IngressLedger::new();
        between(&bus, &ledger);
        let now = Instant::now();
        let at = input.query_wait_estimate().map_or(now, |due| due.max(now));
        let gate = Arc::new(crate::composer_probe::gate::Gate::<UserFrame>::default());
        let (fresh, facts) = writer_facts(Arc::clone(&bus), Some(mark), ledger, scheduled);
        let admits = Arc::clone(&input);
        let section = probe_section(
            Arc::clone(&gate),
            input,
            Arc::new(crate::doorbell_txn::JobClaim::default()),
            fresh,
            move |ctx, status, screen_unchanged| {
                admits
                    .admit_probe(&facts(ctx, status, screen_unchanged), ctx.now)
                    .map_err(ProbeRefusal::Admission)
            },
        );
        let outcome = section(&crate::doorbell_txn::StartCtx {
            boundary: Boundary::Ground,
            retired: false,
            now: at,
        });
        (outcome, gate.status().fenced)
    }

    /// Scenario: Schedule a probe from a Clean frame showing Claude Code's main composer, then, before the writer's arm section, invalidate that frame by its layout alone, with no output: a resize, a nudge's shrink, a failed nudge restore. The writer comes to the section after the generation's redraw estimate (O5) has run out, and refuses the screen, closing no fence: an elapsed estimate is no replacement frame. With nothing changed, or a person's ticket admitted meanwhile, the same section admits, or refuses that ticket (R-D-U2-01, the audit's U3-7).
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/059")]
    #[test]
    fn probe_059_a_layout_edge_since_the_frame_refuses_the_screen_at_the_arm() {
        use crate::composer_probe::admission::Refusal;
        use crate::evidence_state::EdgeReason;
        let (unchanged, fenced) = arm_after(|_, _| {});
        assert!(unchanged.is_ok(), "nothing since the frame: {unchanged:?}");
        assert!(fenced, "the admitted section closes the fence");
        type Edge = fn(&AgentBus);
        let edges: [(&str, Edge); 3] = [
            ("a resize", |bus| {
                bus.resize_and_clear(30, 100, || Ok::<(), ()>(())).unwrap();
            }),
            ("a nudge's shrink", |bus| {
                bus.resize_and_mark_edge(EdgeReason::NudgeShrink, 24, 79, || Ok::<(), ()>(()))
                    .unwrap();
            }),
            ("a failed nudge restore", |bus| {
                let _ = bus.resize_and_mark_edge(EdgeReason::NudgeRestore, 24, 80, || Err(()));
            }),
        ];
        for (what, edge) in edges {
            let (refused, fenced) = arm_after(|bus, _| {
                let seq = bus.evidence().2;
                edge(bus);
                assert_eq!(bus.evidence().2, seq, "{what}: layout only, no output");
            });
            assert_eq!(
                refused,
                Err(ProbeRefusal::Admission(Refusal::Surface)),
                "{what} since the frame, the estimate elapsed"
            );
            assert!(!fenced, "{what}: no fence closed");
        }
        let (output, _) = arm_after(|bus, _| bus.push(b"x".to_vec()));
        assert_eq!(
            output,
            Err(ProbeRefusal::Admission(Refusal::Surface)),
            "output since the frame"
        );
        let mut ticket = None;
        let (ticketed, _) = arm_after(|_, ledger| ticket = Some(ledger.admit()));
        assert_eq!(
            ticketed,
            Err(ProbeRefusal::Admission(Refusal::HumanTicket)),
            "a ticket admitted since scheduling, read by the writer"
        );
    }

    /// Scenario: Run the probe request's scheduling reads with a stand-in observer. With the permission pending, the settlement runs before the first keymap read, so the keymap reads settled (a keymap read before it would read Unmeasured); with nothing to settle the same read reads Unmeasured. When the first pass is refused only by the redraw estimate (O5), the request waits for it and reads the keymap again: a key binding changed during the wait is what it asks with and answers, not the one read before the wait. A second estimate refusal is answered as it is, with no second wait (R-D-U3-01, the audit's U3-6).
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/059")]
    #[tokio::test]
    async fn probe_059_the_keymap_is_read_after_settlement_and_again_after_the_redraw_wait() {
        use crate::composer_probe::keymap::Hold;
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        // Pending -> settled: the read after the settlement sees it.
        for pending in [true, false] {
            let settled = Arc::new(AtomicBool::new(false));
            let settle = pending.then(|| {
                let settled = Arc::clone(&settled);
                async move { settled.store(true, Ordering::SeqCst) }
            });
            let reads = Arc::new(Mutex::new(Vec::new()));
            let (asked, keymap) = scheduling_reads(
                settle,
                || {
                    let (settled, reads) = (Arc::clone(&settled), Arc::clone(&reads));
                    async move {
                        let read: Result<(), Hold> = if settled.load(Ordering::SeqCst) {
                            Ok(())
                        } else {
                            Err(Hold::Unmeasured)
                        };
                        reads.lock().unwrap().push(read);
                        read
                    }
                },
                |keymap| async move { (keymap, None) },
            )
            .await;
            let expected = if pending {
                Ok(())
            } else {
                Err(Hold::Unmeasured)
            };
            assert_eq!(*reads.lock().unwrap(), [expected], "pending: {pending}");
            assert_eq!((asked, keymap), (expected, expected), "pending: {pending}");
        }
        // A binding changed across the O5 wait: the second read stands.
        for estimates in [1, 2] {
            let binding = Arc::new(Mutex::new(Ok::<(), Hold>(())));
            let passes = Arc::new(AtomicUsize::new(0));
            let started = Instant::now();
            let (asked, keymap) = scheduling_reads(
                None::<std::future::Ready<()>>,
                || {
                    let binding = Arc::clone(&binding);
                    async move { *binding.lock().unwrap() }
                },
                |keymap| {
                    let (binding, passes) = (Arc::clone(&binding), Arc::clone(&passes));
                    async move {
                        let pass = passes.fetch_add(1, Ordering::SeqCst);
                        // Refused by the estimate on the first `estimates`
                        // passes; the binding changes during the first wait.
                        let estimate =
                            (pass < estimates).then(|| Instant::now() + Duration::from_millis(20));
                        if pass == 0 {
                            *binding.lock().unwrap() = Err(Hold::Changed);
                        }
                        (keymap, estimate)
                    }
                },
            )
            .await;
            assert_eq!(passes.load(Ordering::SeqCst), 2, "one wait, then once more");
            assert!(started.elapsed() >= Duration::from_millis(20), "it waited");
            assert_eq!(
                (asked, keymap),
                (Err(Hold::Changed), Err(Hold::Changed)),
                "the keymap read after the wait, not before it ({estimates} estimates)"
            );
        }
    }

    /// Scenario: A source-order check, not a behaviour check: read the probe request's source and find that it routes its scheduling reads through the function the behavioural test above runs, handing it the observer's settlement before its keymap read. It stands until the request itself runs under a test (A-20d-1 and A-20d-2's composed rig).
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
        let reads = request
            .find("scheduling_reads(")
            .expect("the scheduling reads");
        let keymap = request.find("observer.keymap()").expect("the keymap read");
        assert!(
            settle < reads && reads < keymap,
            "the settlement and the keymap read are handed to the scheduling reads"
        );
        assert_eq!(request.matches("observer.keymap()").count(), 1);
        assert_eq!(request.matches("scheduling_reads(").count(), 1);
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

    /// Scenario: Ask when the display watcher must next read a pane's held typing, as a hold ages and while a not-delivered notice shows. A new hold wakes it exactly when the hold reaches 300 ms; a shown hold is read again at least every 250 ms and no later than its 5 s mark; past 5 s it keeps that pace; a not-delivered notice is checked every second; with nothing held and nothing shown no timer runs.
    #[spec("inbox/probe/046")]
    #[test]
    fn probe_046_the_display_timers_wake_at_300_ms_then_5_s() {
        let since = Instant::now();
        let at = |ms: u64| since + Duration::from_millis(ms);
        assert_eq!(held_wake(at(0), Some(since), false, false), Some(at(300)));
        assert_eq!(held_wake(at(299), Some(since), false, false), Some(at(300)));
        assert_eq!(held_wake(at(300), Some(since), true, false), Some(at(550)));
        assert_eq!(
            held_wake(at(4_900), Some(since), true, false),
            Some(at(5_000))
        );
        assert_eq!(
            held_wake(at(6_000), Some(since), true, false),
            Some(at(6_250))
        );
        assert_eq!(held_wake(at(10), None, false, true), Some(at(1_010)));
        assert_eq!(held_wake(at(10), None, false, false), None);
        // A hold the card never shows (no numbered generation) runs no timer
        // once past its mark.
        assert_eq!(held_wake(at(400), Some(since), false, false), None);
    }

    /// Scenario: Ask whether a card shows held typing at ages either side of 300 ms, with and without typing waiting for room. A hold with no waiter is hidden until 300 ms, so a short check doesn't flicker; one at the cap shows at once, from age zero, and the watcher then re-reads it on the shown pace rather than waiting for the 300 ms mark.
    #[spec("inbox/probe/046")]
    #[test]
    fn probe_046_a_full_queue_shows_at_once() {
        let just_under = HELD_SHOWN_AFTER - Duration::from_millis(1);
        assert!(!held_shown(0, Duration::ZERO));
        assert!(!held_shown(0, just_under));
        assert!(held_shown(0, HELD_SHOWN_AFTER));
        assert!(held_shown(1, Duration::ZERO), "at the cap at once");
        assert!(held_shown(3, just_under));
        assert_eq!(
            held_cause(1, Duration::ZERO, None),
            InputHeldCause::QueueFull
        );
        let since = Instant::now();
        let at = |ms: u64| since + Duration::from_millis(ms);
        assert_eq!(
            held_wake(at(0), Some(since), held_shown(1, Duration::ZERO), false),
            Some(at(250)),
            "a hold shown at the cap is re-read on the shown pace"
        );
    }

    fn update(
        pane: &str,
        agent: &str,
        cause: Option<InputHeldCause>,
        discarded: Option<u64>,
    ) -> crate::event::InputHeldUpdate {
        crate::event::InputHeldUpdate {
            pane_id: pane.into(),
            agent_id: agent.into(),
            input_held: cause.map(|cause| crate::inbox_display::InputHeld {
                generation: agent.parse().unwrap(),
                cause,
                held_ops: 1,
                held_bytes: 3,
                since_ms: 7,
            }),
            input_discarded: discarded.map(|generation| crate::inbox_display::InputDiscarded {
                generation,
                held_ops: 1,
                held_bytes: 3,
                at_ms: 9,
            }),
        }
    }

    fn pass(panes: Vec<crate::event::InputHeldUpdate>) -> HeldDisplayPass {
        HeldDisplayPass { panes, next: None }
    }

    /// Scenario: Feed the display watcher's change detector a sequence of passes over two panes. A generation first seen holding nothing is not announced; a hold, a change of cause and its end are each announced once; counts that move under the same cause are not; a pane whose generation is gone is cleared under that generation; a successor that shows nothing clears its predecessor's hold, and a successor carrying the not-delivered notice is announced.
    #[spec("inbox/probe/046")]
    #[test]
    fn probe_046_the_watcher_announces_only_what_a_card_shows_differently() {
        let mut shown = HeldDisplayShown::default();
        assert!(
            shown
                .changes(pass(vec![
                    update("p", "7", None, None),
                    update("q", "8", None, None)
                ]))
                .is_empty()
        );
        let checking = update("p", "7", Some(InputHeldCause::Checking), None);
        assert_eq!(
            shown.changes(pass(vec![checking.clone(), update("q", "8", None, None)])),
            vec![checking.clone()]
        );
        let mut more = checking.clone();
        more.input_held.as_mut().unwrap().held_ops = 4;
        assert!(
            shown
                .changes(pass(vec![more, update("q", "8", None, None)]))
                .is_empty()
        );
        let explained = update("p", "7", Some(InputHeldCause::CheckNotReturned), None);
        assert_eq!(
            shown.changes(pass(vec![explained.clone(), update("q", "8", None, None)])),
            vec![explained]
        );
        // The pane's generation retires holding typing; its successor
        // carries the notice.
        let successor = update("p", "9", None, Some(7));
        assert_eq!(
            shown.changes(pass(vec![successor.clone(), update("q", "8", None, None)])),
            vec![successor]
        );
        // A keystroke ends the notice.
        assert_eq!(
            shown.changes(pass(vec![
                update("p", "9", None, None),
                update("q", "8", None, None)
            ])),
            vec![update("p", "9", None, None)]
        );
        // A hold on a pane whose generation then leaves is cleared under it.
        let held = update("q", "8", Some(InputHeldCause::QueueFull), None);
        assert_eq!(
            shown.changes(pass(vec![update("p", "9", None, None), held.clone()])),
            vec![held]
        );
        assert_eq!(
            shown.changes(pass(vec![update("p", "9", None, None)])),
            vec![update("q", "8", None, None)]
        );
        // A successor showing nothing clears its predecessor's hold.
        let held = update("p", "9", Some(InputHeldCause::Checking), None);
        assert_eq!(shown.changes(pass(vec![held.clone()])), vec![held]);
        assert_eq!(
            shown.changes(pass(vec![update("p", "10", None, None)])),
            vec![update("p", "9", None, None), update("p", "10", None, None)]
        );
    }
}
