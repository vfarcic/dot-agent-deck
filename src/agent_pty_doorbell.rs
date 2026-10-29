//! PRD #1666 part 3, round 5-wake (plan §1.1, §2.3, §2.4, §3.4′, §4.6): the
//! doorbell's ring on the real writer, and the re-projection publisher.
//!
//! [`AgentPtyRegistry::ring_doorbell`] acquires the pane's writer and hands
//! it, owned, to a supervised task that drives one [`txn::Txn`] against the
//! daemon: the admission's inputs read from the generation (its activation,
//! its input-mode evidence, its ledger, its Clean frame, its settle window),
//! the final seam read under `try_lock`s that stay held across the job's
//! enqueue, each job queued under the transaction's own claim
//! ([`PaneWriter::queue_deck_job`]), and the writer's answers, the hook's
//! ring evidence and the passing time fed back as events. Dropping the
//! caller detaches it; the task ends at a terminal state or the deadline, and
//! only then releases the writer, so the human frames queued behind it are
//! written after it, in order.
//!
//! The publisher ([`AgentPtyRegistry::spawn_doorbell_publisher`]) is one task
//! per daemon that runs re-projection requests one at a time, coalescing a
//! generation's pending ones: each run reads the generation's latch cell and
//! its activation inputs as they are when it runs, and tells the inbox what
//! the card shows (`uncertain_input`, `dialog_during_ring`) and whether the
//! deck will type into the generation at all (Q16, Q18).

use super::*;
use crate::doorbell_txn as txn;

/// A transaction job the writer started and has not answered after this
/// long is reported stalled (plan §2.3's write caps).
pub(crate) const PHASE_WRITE_CAP: Duration = Duration::from_millis(200);

/// How often a ring that holds the writer asks again whether the generation
/// is still fit for it.
const STILL_ADMITTED_EVERY: Duration = Duration::from_millis(50);

/// The longest a transaction ending `Unresolved` before its deadline waits
/// for the publisher to show it, so the card says so before the writer is
/// released when the publisher is not contended (plan §2.4).
const REPROJECT_WAIT: Duration = Duration::from_millis(100);

/// The most a transaction step waits between looks at the pane.
const STEP: Duration = Duration::from_millis(10);

/// What [`AgentPtyRegistry::ring_doorbell`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingOutcome {
    /// The pane is gone or now belongs to another generation; nothing was
    /// written.
    NotOwner,
    /// The pane's writer could not be had, or the pane is not taking input;
    /// nothing was written.
    NotReading,
    /// The generation's doorbell is suspended on an earlier attempt; nothing
    /// is admitted until the user's own submit or clear, or exact evidence,
    /// lifts it.
    Suspended,
    /// The transaction ran and ended so.
    Txn(txn::Outcome),
}

/// A ring's report to the doorbell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingReport {
    pub outcome: RingOutcome,
    /// The attempt's number, when one was admitted.
    pub attempt: Option<u32>,
    /// An Enter reached the pane: a withdrawal after it is a not-taken Enter.
    pub cr_reached: bool,
}

impl RingReport {
    fn before_admission(outcome: RingOutcome) -> Self {
        Self {
            outcome,
            attempt: None,
            cr_reached: false,
        }
    }
}

/// What the doorbell may type into a generation under: the screen profile,
/// what it measured, and the first-ring settle window.
#[derive(Clone)]
pub(crate) struct Typing {
    pub(crate) profile: &'static crate::doorbell_screen::ScreenProfile,
    pub(crate) caps: txn::Caps,
    pub(crate) settle: Duration,
    /// How the verdict reads in the `doorbell attempt` line.
    pub(crate) verdict: String,
    /// The release the verdict was read for, latched from the generation's
    /// start-up header; none for the e2e build's stand-in.
    pub(crate) release: Option<String>,
}

/// A generation's handles, cloned out of the registry under its lock.
pub(crate) struct Generation {
    pub(crate) agent_id: String,
    pub(crate) pane_id: String,
    pub(crate) bus: Arc<AgentBus>,
    pub(crate) ledger: Arc<crate::ingress_ledger::IngressLedger>,
    pub(crate) mode: Arc<Mutex<crate::input_mode::InputModeStore>>,
    pub(crate) doorbell: Arc<crate::doorbell_generation::GenerationDoorbell>,
    pub(crate) retired: Arc<AtomicBool>,
    pub(crate) exited: Arc<AtomicBool>,
    pub(crate) writer: Arc<AsyncMutex<PaneWriter>>,
    /// The effective agent type: the spawn type, else the type the
    /// generation's own hooks reported.
    pub(crate) agent: Option<AgentType>,
    #[cfg_attr(not(feature = "e2e"), allow(dead_code))]
    pub(crate) child_pid: Option<u32>,
}

impl Generation {
    /// The input-mode evidence now (plan §1.3): a wrapped pane's from its
    /// wrapper's reports (or a pane any reporter is bound to), an unwrapped
    /// one's from the deck's own PTY master.
    fn mode_now(&self, store: &crate::input_mode::InputModeStore) -> Result<(), &'static str> {
        if self.doorbell.wrapped() || store.observer().is_some() || store.sticky().is_some() {
            let now = crate::input_mode::monotonic_ns()
                .ok_or(crate::input_mode::ModeClause::Unverified.as_str())?;
            store
                .usable(now)
                .map(|_| ())
                .map_err(crate::input_mode::ModeClause::as_str)
        } else {
            self.doorbell.unwrapped_mode()
        }
    }

    fn open(&self) -> bool {
        !self.retired.load(Ordering::SeqCst) && !self.exited.load(Ordering::SeqCst)
    }
}

/// The frame the evidence state shows now, through `profile`.
fn frame_of(
    state: &AgentBusState,
    profile: &crate::doorbell_screen::ScreenProfile,
    line: Option<&str>,
) -> txn::Frame {
    let epoch = state.evidence.layout_epoch();
    match state.evidence.clean_frame() {
        Some(grid) => crate::doorbell_screen::frame(profile, &grid, epoch, true, line),
        None => txn::Frame {
            epoch,
            clean: false,
            ..txn::Frame::default()
        },
    }
}

/// The final seam (plan §2.4): the ledger, the bus state and the mode store
/// by `try_lock`, held from the read until the driver has enqueued whatever
/// the transaction decided on it, then dropped.
struct DaemonSeam<'g> {
    generation: &'g Generation,
    /// Every pane's input state, read for the input proof by `try_lock` and
    /// not held: the seam's ledger hold already keeps human operations out.
    pane_input: &'g Mutex<PaneInputState>,
    profile: &'static crate::doorbell_screen::ScreenProfile,
    line: String,
    #[allow(clippy::type_complexity)]
    held: Option<(
        crate::ingress_ledger::LedgerHold<'g>,
        std::sync::MutexGuard<'g, AgentBusState>,
        std::sync::MutexGuard<'g, crate::input_mode::InputModeStore>,
    )>,
}

impl DaemonSeam<'_> {
    fn release(&mut self) {
        self.held = None;
    }
}

impl txn::Seam for DaemonSeam<'_> {
    fn read(&mut self) -> Option<txn::SeamRead> {
        self.held = None;
        let ledger = self.generation.ledger.try_hold()?;
        let bus = self.generation.bus.state.try_lock().ok()?;
        let mode = self.generation.mode.try_lock().ok()?;
        let input = self
            .pane_input
            .try_lock()
            .ok()?
            .input_proof(&self.generation.pane_id, Some(self.profile.input));
        let marks = ledger.marks();
        let read = txn::SeamRead {
            watermark: marks.watermark,
            job_mark: marks.job_mark,
            frame: txn::Frame {
                input,
                ..frame_of(&bus, self.profile, Some(&self.line))
            },
            mode: self.generation.mode_now(&mode),
            open: self.generation.open(),
        };
        self.held = Some((ledger, bus, mode));
        Some(read)
    }
}

/// What admission reads of a generation, with nothing written yet.
struct AdmissionReads {
    typing: Result<Typing, &'static str>,
    /// The ledger's counters and outstanding set, in one hold.
    marks: crate::ingress_ledger::LedgerMarks,
    frame: txn::Frame,
    mode: Result<(), &'static str>,
    anchor: Option<u64>,
    nudge_in_flight: bool,
}

/// The transaction's one job on the writer, from its enqueue until it ends
/// or is taken back.
struct JobSlot {
    job: DeckJob,
    started_at: Option<Instant>,
    stalled: bool,
}

/// Ring evidence from the pane's channel, kept when it is this
/// generation's and carries a token.
fn note_evidence(
    got: Result<RingEvidence, broadcast::error::RecvError>,
    agent: &str,
    pending: &mut std::collections::VecDeque<txn::Event>,
) {
    if let Ok(seen) = got
        && seen.agent_id == agent
        && let Some(token) = txn::RingToken::parse(&seen.token)
    {
        pending.push_back(txn::Event::Evidence {
            token,
            exact: seen.shape == crate::event::PromptShape::Exact,
        });
    }
}

/// A wait on the wall clock, which the transaction's deadline lives on, and
/// not on Tokio's (see [`within_real_time`]).
async fn real_sleep(duration: Duration) {
    let _ = real_time_alarm(duration).await;
}

impl AgentPtyRegistry {
    /// `agent_id`'s handles, when it still holds `pane_id`.
    pub(crate) fn doorbell_generation(&self, pane_id: &str, agent_id: &str) -> Option<Generation> {
        let inner = self.inner.lock();
        let agent = inner.agents.get(agent_id)?;
        if agent.pane_id_env.as_deref() != Some(pane_id) {
            return None;
        }
        Some(Generation {
            agent_id: agent_id.to_string(),
            pane_id: pane_id.to_string(),
            bus: Arc::clone(&agent.bus),
            ledger: Arc::clone(&agent.ingress),
            mode: Arc::clone(&agent.input_mode),
            doorbell: Arc::clone(&agent.doorbell),
            retired: Arc::clone(&agent.pane_retired),
            exited: Arc::clone(&agent.exited),
            writer: Arc::clone(&agent.writer),
            agent: agent
                .spawn_agent_type
                .clone()
                .or_else(|| agent.agent_type.clone()),
            child_pid: agent.child.process_id(),
        })
    }

    /// What the doorbell may type into `generation` under, or the clause
    /// that holds it (plan §4.6): the activation table over the
    /// generation's effective agent type and latched release, with the e2e
    /// build's stand-in editor typed into under its own profile.
    /// `hook_route` says whether the generation's own prompt hook reports
    /// its prompts.
    pub(crate) fn doorbell_typing(
        &self,
        generation: &Generation,
        hook_route: bool,
    ) -> Result<Typing, &'static str> {
        self.doorbell_typing_from(generation, hook_route, |state| Some(state.lock().unwrap()))
            .expect("a blocking read always reads")
    }

    /// [`Self::doorbell_typing`] with the generation's bus state read by
    /// `try_lock`: `None` while it is contended, for a reader that holds the
    /// writer and so must not wait on it (plan §2.3).
    fn doorbell_typing_now(
        &self,
        generation: &Generation,
        hook_route: bool,
    ) -> Option<Result<Typing, &'static str>> {
        self.doorbell_typing_from(generation, hook_route, |state| state.try_lock().ok())
    }

    fn doorbell_typing_from<'s>(
        &self,
        generation: &'s Generation,
        hook_route: bool,
        lock: impl FnOnce(&'s Mutex<AgentBusState>) -> Option<std::sync::MutexGuard<'s, AgentBusState>>,
    ) -> Option<Result<Typing, &'static str>> {
        #[cfg(feature = "e2e")]
        if let Some(binary) = generation.doorbell.standin() {
            if !crate::doorbell_screen::standin_running(generation.child_pid, binary) {
                return Some(Err(
                    crate::doorbell_activation::NoGo::UnmeasuredRelease.clause()
                ));
            }
            return Some(Ok(Typing {
                profile: &crate::doorbell_screen::STANDIN_PROFILE,
                caps: txn::Caps {
                    delete_key: true,
                    placeholder: true,
                    hook_route: false,
                    transcript_route: true,
                    staged_head: None,
                },
                settle: Duration::ZERO,
                verdict: "Go (e2e stand-in)".to_string(),
                release: None,
            }));
        }
        let agent = generation.agent.clone();
        let (latch, evidence_profile) = {
            let state = lock(&generation.bus.state)?;
            let latch = agent
                .as_ref()
                .map(|a| state.release.latched_for(a))
                .unwrap_or(crate::doorbell_activation::LatchedRelease::Unseen);
            (latch, state.evidence.profile())
        };
        Some(self.typing_for(hook_route, agent, latch, evidence_profile))
    }

    /// The verdict and its typing, from what the generation latched.
    fn typing_for(
        &self,
        hook_route: bool,
        agent: Option<AgentType>,
        latch: crate::doorbell_activation::LatchedRelease,
        evidence_profile: Option<&'static crate::doorbell_screen::ScreenProfile>,
    ) -> Result<Typing, &'static str> {
        use crate::doorbell_activation::{Activation, RELEASE_RECORDS};
        let verdict = crate::doorbell_generation::generation_activation(
            &RELEASE_RECORDS,
            agent.as_ref(),
            &latch,
        );
        let released = match &latch {
            crate::doorbell_activation::LatchedRelease::Known(release) => Some(release.clone()),
            _ => None,
        };
        let (profile_id, staged, settle) = match &verdict {
            Activation::Go {
                profile,
                staged,
                settle,
            } => (*profile, *staged, *settle),
            Activation::NoGo(no_go) => return Err(no_go.clause()),
        };
        let profile = crate::doorbell_screen::profile(profile_id)
            .filter(|p| evidence_profile.is_some_and(|e| e.id == p.id))
            .ok_or(crate::doorbell_activation::NoGo::UnmeasuredRelease.clause())?;
        let record = RELEASE_RECORDS
            .iter()
            .find(|r| Some(&r.agent) == agent.as_ref() && Some(&r.release) == released.as_ref())
            .ok_or(crate::doorbell_activation::NoGo::UnmeasuredRelease.clause())?;
        let verified = |m: bool| m;
        let caps = txn::Caps {
            delete_key: verified(record.delete_key.verified().is_some()),
            placeholder: verified(record.placeholder_empty.verified().is_some()),
            hook_route: hook_route
                && matches!(agent, Some(AgentType::ClaudeCode | AgentType::Codex)),
            transcript_route: record.transcript_echo.verified().is_some(),
            // Q27: the first stage is the token head alone.
            staged_head: staged.then_some(crate::doorbell_screen::TOKEN_HEAD_CHARS),
        };
        #[cfg(test)]
        let settle = self
            .doorbell_settle_for_test
            .lock()
            .unwrap()
            .unwrap_or(settle);
        Ok(Typing {
            profile,
            caps,
            settle,
            verdict: format!("{verdict:?}"),
            release: released.as_ref().map(ToString::to_string),
        })
    }

    /// Admission's reads (plan §1.1 step 1), each lock by `try_lock`: `None`
    /// while any is contended, since the caller holds the writer. The
    /// ledger, the bus state and the mode store are held together, in the
    /// final seam's order, and the pane input state is read while they are,
    /// so the frame, the input proof, the mode and the ledger's marks are
    /// one moment.
    fn admission_reads(&self, generation: &Generation, hook_route: bool) -> Option<AdmissionReads> {
        let typing = self.doorbell_typing_now(generation, hook_route)?;
        let ledger = generation.ledger.try_hold()?;
        let state = generation.bus.state.try_lock().ok()?;
        let store = generation.mode.try_lock().ok()?;
        let input = self.pane_input.try_lock().ok()?.input_proof(
            &generation.pane_id,
            typing.as_ref().ok().map(|typing| typing.profile.input),
        );
        let frame = match &typing {
            Ok(typing) => txn::Frame {
                input,
                ..frame_of(&state, typing.profile, None)
            },
            Err(_) => txn::Frame::default(),
        };
        let reads = AdmissionReads {
            marks: ledger.marks(),
            frame,
            mode: generation.mode_now(&store),
            anchor: store.anchor(),
            nudge_in_flight: false,
            typing,
        };
        drop((store, state, ledger));
        let nudge_in_flight = self
            .repaint_nudges
            .try_lock()
            .ok()?
            .contains_key(&generation.agent_id);
        Some(AdmissionReads {
            nudge_in_flight,
            ..reads
        })
    }

    /// PRD #1666 part 3, Q32: how the input box of `agent_id`'s generation
    /// in `pane_id` edits, from the screen profile its evidence tracks (the
    /// e2e build's stand-in editor's own, when it runs one); `None` when the
    /// generation is gone or has no measured profile.
    pub fn doorbell_input_semantics(
        &self,
        pane_id: &str,
        agent_id: &str,
    ) -> Option<crate::composer_model::Semantics> {
        let generation = self.doorbell_generation(pane_id, agent_id)?;
        #[cfg(feature = "e2e")]
        if generation.doorbell.standin().is_some() {
            return Some(crate::doorbell_screen::STANDIN_PROFILE.input);
        }
        let state = generation.bus.state.lock().unwrap();
        state.evidence.profile().map(|profile| profile.input)
    }

    /// The clause that holds `agent_id`'s generation in `pane_id` from the
    /// typed doorbell right now, for its card (plan §5.1 row 8): its
    /// activation, its input-mode evidence, its spent attempt numbers, no
    /// submit route. `None` when the doorbell may type, or the generation is
    /// gone.
    pub fn doorbell_refusal(
        &self,
        pane_id: &str,
        agent_id: &str,
        hook_route: bool,
    ) -> Option<&'static str> {
        let generation = self.doorbell_generation(pane_id, agent_id)?;
        let typing = match self.doorbell_typing(&generation, hook_route) {
            Ok(typing) => typing,
            Err(clause) => return Some(clause),
        };
        if let Err(clause) = generation.mode_now(&generation.mode.lock().unwrap()) {
            return Some(clause);
        }
        // The same clause the publisher gives WAKE (Q18).
        if generation.doorbell.counter().exhausted() {
            return Some("attempt_exhausted");
        }
        if !(typing.caps.hook_route || typing.caps.transcript_route) {
            return Some("no_submit_route");
        }
        None
    }

    /// PRD #1666 part 3, Q28: whether `agent_id`'s generation in `pane_id`
    /// may be rung before its own hooks report: its effective agent's first
    /// hook comes with its first prompt
    /// ([`crate::agent_registry::FirstHook::WithFirstPrompt`]) and its own
    /// latched release is activated. That is permission to try, not
    /// readiness: every ring's transaction still needs the generation's own
    /// Clean frame of the empty ready composer, mode evidence and settle
    /// anchor, and no hook route is assumed (`hook_route` stays false, so
    /// submit evidence is the transcript's).
    pub fn doorbell_ready_before_hooks(&self, pane_id: &str, agent_id: &str) -> bool {
        let Some(generation) = self.doorbell_generation(pane_id, agent_id) else {
            return false;
        };
        let Some(agent) = generation.agent.as_ref() else {
            return false;
        };
        crate::agent_registry::spec(agent).first_hook
            == crate::agent_registry::FirstHook::WithFirstPrompt
            && self.doorbell_typing(&generation, false).is_ok()
    }

    /// Whether `agent_id`'s first ring still waits out the settle window
    /// (plan §4.4), so its hold clock is paused. A release with no window
    /// waits for nothing. An unwrapped generation's anchor is its first Clean
    /// frame showing the empty ready composer, noted here when the doorbell
    /// first sees it as well as at admission.
    pub fn doorbell_settling(&self, pane_id: &str, agent_id: &str, hook_route: bool) -> bool {
        let Some(generation) = self.doorbell_generation(pane_id, agent_id) else {
            return false;
        };
        let Ok(typing) = self.doorbell_typing(&generation, hook_route) else {
            return false;
        };
        if typing.settle.is_zero() {
            return false;
        }
        let Some(now) = crate::input_mode::monotonic_ns() else {
            return true;
        };
        if !generation.doorbell.wrapped() {
            let input = self
                .pane_input
                .lock()
                .unwrap()
                .input_proof(&generation.pane_id, Some(typing.profile.input));
            let ready = txn::Frame {
                input,
                ..frame_of(&generation.bus.state.lock().unwrap(), typing.profile, None)
            }
            .empty_and_ready();
            if ready {
                generation.doorbell.note_ready_frame(now);
            }
        }
        let anchor = generation.mode.lock().unwrap().anchor();
        generation.doorbell.settling(anchor, typing.settle, now)
    }

    /// Whether `agent_id`'s doorbell is suspended on an attempt.
    pub fn doorbell_suspended(&self, agent_id: &str) -> bool {
        self.generation_doorbell(agent_id)
            .is_some_and(|doorbell| doorbell.suspended())
    }

    /// Whether `agent_id`'s evidence state is waiting for the agent to redraw
    /// after a layout edge (Dirty), which only an agent's own redraw or a
    /// refresh nudge ends.
    pub fn doorbell_evidence_dirty(&self, agent_id: &str) -> bool {
        let bus = {
            let inner = self.inner.lock();
            inner.agents.get(agent_id).map(|a| Arc::clone(&a.bus))
        };
        bus.is_some_and(|bus| {
            matches!(
                bus.evidence().0,
                crate::evidence_state::EvidenceStatus::Dirty { .. }
                    | crate::evidence_state::EvidenceStatus::Pending { .. }
            )
        })
    }

    /// PRD #1666 part 3, round 5-wake (plan §1.1): ring the doorbell for
    /// message `message` in `pane_id`, whose live generation must be
    /// `agent_id`: one transaction, from admission to release, on the real
    /// writer. `still_admitted` is asked before admission and again while
    /// the transaction holds the writer; `hook_route` says whether the
    /// generation's own prompt hook reports its prompts. Never admits while
    /// the generation's doorbell is suspended.
    pub async fn ring_doorbell<F, Fut>(
        self: &Arc<Self>,
        pane_id: &str,
        agent_id: &str,
        message: crate::inbox::MessageId,
        hook_route: bool,
        still_admitted: F,
    ) -> RingReport
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = bool> + Send,
    {
        match self.writer_target_for_pane(pane_id) {
            Some(target)
                if target.agent_id == agent_id && !target.exited.load(Ordering::SeqCst) => {}
            _ => return RingReport::before_admission(RingOutcome::NotOwner),
        }
        let Some(generation) = self.doorbell_generation(pane_id, agent_id) else {
            return RingReport::before_admission(RingOutcome::NotOwner);
        };
        // The doorbell reads the cell for the generation it is about to
        // ring (plan §3.4′): a suspended one is not admitted.
        if generation.doorbell.suspended() {
            return RingReport::before_admission(RingOutcome::Suspended);
        }
        let stall = self.pty_write_stall_bound();
        let Some(guard) =
            within_real_time(stall, Arc::clone(&generation.writer).lock_owned()).await
        else {
            return RingReport::before_admission(RingOutcome::NotReading);
        };
        // The one deadline of the whole hold (plan §2.3): `D` runs from the
        // moment the writer is the ring's, so admission's reads spend the same
        // budget as the transaction.
        let deadline = Instant::now() + txn::TXN_MAX;
        let registry = Arc::clone(self);
        // Supervised: the task owns the writer, so dropping the caller (a
        // doorbell tick, a shutdown path) detaches it and does not drop the
        // guard. It ends at a terminal state or at the deadline.
        let task = tokio::spawn(async move {
            registry
                .run_ring(
                    generation,
                    guard,
                    deadline,
                    message,
                    hook_route,
                    still_admitted,
                )
                .await
        });
        task.await
            .unwrap_or_else(|_| RingReport::before_admission(RingOutcome::NotReading))
    }

    async fn run_ring<F, Fut>(
        self: Arc<Self>,
        generation: Generation,
        guard: tokio::sync::OwnedMutexGuard<PaneWriter>,
        deadline: Instant,
        message: crate::inbox::MessageId,
        hook_route: bool,
        still_admitted: F,
    ) -> RingReport
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = bool> + Send,
    {
        let pane = generation.pane_id.clone();
        let agent = generation.agent_id.clone();
        let left = || deadline.saturating_duration_since(Instant::now());
        let expired = |guard| {
            drop(guard);
            RingReport::before_admission(RingOutcome::Txn(txn::Outcome::NothingWritten(
                txn::NotTyped::Deadline,
            )))
        };
        // Read again under the writer: a ring queued behind one that
        // suspended the cell is not admitted either.
        if generation.doorbell.suspended() {
            drop(guard);
            return RingReport::before_admission(RingOutcome::Suspended);
        }
        let mut evidence = self.subscribe_doorbell_evidence(&pane);
        // Step 1: admission, with nothing written yet. The generation's
        // locks and the pane input state are read by `try_lock` and retried
        // until `D`, the admission check reads the hub the same way, and the
        // awaits wait at most for what is left of `D`. Leaf locks on the way
        // (the evidence channel map) are waited on, and `D` does not bound
        // that wait.
        let drained = within_real_time(Duration::from_millis(100).min(left()), guard.until_idle())
            .await
            .is_some();
        #[cfg(test)]
        self.ring_admission_seam_run();
        let reads = loop {
            if let Some(reads) = self.admission_reads(&generation, hook_route) {
                break reads;
            }
            if left().is_zero() {
                return expired(guard);
            }
            real_sleep(txn::SEAM_RETRY.min(left())).await;
        };
        let AdmissionReads {
            typing,
            marks,
            frame,
            mode,
            anchor,
            nudge_in_flight,
        } = reads;
        // Idle is one snapshot with `W0` and `J0`: a human ticket outstanding
        // in it refuses the ring, and one admitted after it moves the
        // watermark the first seam checks (Q19).
        let ledger_idle = drained && marks.outstanding == 0;
        let now_ns = crate::input_mode::monotonic_ns().unwrap_or(0);
        if frame.empty_and_ready() && !generation.doorbell.wrapped() {
            generation.doorbell.note_ready_frame(now_ns);
        }
        let settle = typing.as_ref().map_or(Duration::ZERO, |t| t.settle);
        let settling = generation.doorbell.settling(anchor, settle, now_ns);
        let Some(still) = within_real_time(left(), still_admitted()).await else {
            return expired(guard);
        };
        if left().is_zero() {
            return expired(guard);
        }
        let caps = typing.as_ref().map_or(
            txn::Caps {
                delete_key: false,
                placeholder: false,
                hook_route: false,
                transcript_route: false,
                staged_head: None,
            },
            |t| t.caps,
        );
        let admission = txn::Admission {
            activation: typing.as_ref().map(|_| ()).map_err(|clause| *clause),
            mode,
            ledger_idle,
            watermark: marks.watermark,
            job_mark: marks.job_mark,
            frame,
            nudge_in_flight,
            settling,
            submit_route: caps.hook_route || caps.transcript_route,
            still_admitted: still,
        };
        let draw = || {
            let mut byte = [0u8; 1];
            let _ = getrandom::fill(&mut byte);
            byte[0]
        };
        let admitted = txn::admit(
            &admission,
            generation.doorbell.counter(),
            generation.doorbell.ring(),
            generation.doorbell.cell(),
            draw,
            &message,
            caps,
            deadline - txn::TXN_MAX,
        );
        let mut txn = match admitted {
            Ok(txn) => txn,
            Err(why) => {
                tracing::debug!(
                    target: "doorbell",
                    pane_id = %pane,
                    agent_id = %agent,
                    message_id = %message.as_str(),
                    verdict = typing.as_ref().map_or_else(|c| c.to_string(), |t| t.verdict.clone()),
                    mode = ?admission.mode,
                    ledger_idle,
                    watermark = marks.watermark,
                    job_mark = marks.job_mark,
                    clean = admission.frame.clean,
                    layout_epoch = admission.frame.epoch,
                    settling,
                    nudge_in_flight,
                    still_admitted = still,
                    ?why,
                    "doorbell attempt: not admitted"
                );
                drop(guard);
                return RingReport::before_admission(RingOutcome::Txn(
                    txn::Outcome::NothingWritten(why),
                ));
            }
        };
        let typing = typing.expect("an admitted attempt has its typing");
        generation.doorbell.admitted();
        if generation.doorbell.counter().exhausted() {
            // That was the generation's last attempt number (Q18): from now
            // its typed doorbell is refused, which WAKE reports.
            self.request_reprojection(&pane, &agent);
        }
        tracing::info!(
            target: "doorbell",
            pane_id = %pane,
            agent_id = %agent,
            message_id = %message.as_str(),
            attempt = txn.attempt(),
            token = %txn.token().letters(),
            activation = %typing.verdict,
            release = typing.release.as_deref().unwrap_or("none"),
            release_source = if typing.release.is_some() { "header" } else { "none" },
            profile = typing.profile.id,
            w0 = marks.watermark,
            j0 = marks.job_mark,
            layout_epoch = admission.frame.epoch,
            staged = typing.caps.staged_head.is_some(),
            hook_route = typing.caps.hook_route,
            "doorbell attempt"
        );
        // E2E interleave seams (`inbox/doorbell/031`), on a generation's
        // later attempts: admitted, holding the writer, nothing written.
        #[cfg(feature = "e2e")]
        if txn.attempt() > 1 {
            crate::doorbell::pause_rering_for_test(&pane, "admitted").await;
        }
        self.doorbell_txn_begin(&agent);
        let line = txn.line().to_string();
        #[cfg(feature = "e2e")]
        let mut paused_after_line = false;
        let mut job: Option<JobSlot> = None;
        let mut pending: std::collections::VecDeque<txn::Event> = std::collections::VecDeque::new();
        let mut lost = false;
        let mut asked_at = Instant::now();
        let mut mode_was_usable = true;
        let mut next: Option<txn::Event> = None;
        let outcome = loop {
            let (wake, released) = self.ring_step(
                &mut txn,
                next.take(),
                &generation,
                typing.profile,
                &line,
                &guard,
                &mut job,
                &mut pending,
            );
            if let Some(outcome) = released {
                break outcome;
            }
            if let Some(event) = pending.pop_front() {
                next = Some(event);
                continue;
            }
            let now = Instant::now();
            let until = wake
                .unwrap_or(now + STEP)
                .min(now + STEP)
                .min(txn.deadline().max(now));
            let running = job
                .as_ref()
                .is_some_and(|slot| slot.started_at.is_some() || slot.job.started());
            let answered = match job.as_mut() {
                Some(slot) if running => {
                    tokio::select! {
                        biased;
                        done = slot.job.done() => Some(done.unwrap_or((0, false))),
                        got = evidence.recv() => {
                            note_evidence(got, &agent, &mut pending);
                            None
                        }
                        _ = real_sleep(until.saturating_duration_since(now)) => None,
                    }
                }
                _ => {
                    tokio::select! {
                        biased;
                        got = evidence.recv() => note_evidence(got, &agent, &mut pending),
                        _ = real_sleep(until.saturating_duration_since(now)) => {}
                    }
                    None
                }
            };
            if let Some((accepted, ok)) = answered {
                job = None;
                // E2E interleave seam (`inbox/doorbell/031`): the first line
                // stage written, the writer held, before the transaction
                // hears that its job ended.
                #[cfg(feature = "e2e")]
                if !paused_after_line && !txn.cr_reached() && txn.attempt() > 1 {
                    paused_after_line = true;
                    crate::doorbell::pause_rering_for_test(&pane, "after-line").await;
                }
                pending.push_back(txn::Event::JobEnded { accepted, ok });
            }
            // What the pane shows now, in the order the transaction needs it.
            if !generation.open() {
                pending.push_back(txn::Event::Retired);
            }
            if let Some(slot) = job.as_mut() {
                if slot.started_at.is_none() && slot.job.started() {
                    slot.started_at = Some(Instant::now());
                    pending.push_back(txn::Event::JobStarted);
                }
                if let Some(at) = slot.started_at
                    && !slot.stalled
                    && at.elapsed() >= PHASE_WRITE_CAP
                {
                    if let Some((accepted, ok)) = slot.job.try_done() {
                        job = None;
                        pending.push_back(txn::Event::JobEnded { accepted, ok });
                    } else {
                        slot.stalled = true;
                        pending.push_back(txn::Event::JobStalled);
                    }
                }
            }
            let usable = generation
                .mode
                .try_lock()
                .ok()
                .map(|store| generation.mode_now(&store).is_ok());
            if let Some(usable) = usable {
                if !usable && mode_was_usable {
                    pending.push_back(txn::Event::ModeChanged);
                }
                mode_was_usable = usable;
            }
            if !lost && asked_at.elapsed() >= STILL_ADMITTED_EVERY {
                asked_at = Instant::now();
                // Bounded by what is left of `D`: an answer still out then is
                // no answer, and the deadline ends the transaction.
                let left = txn.deadline().saturating_duration_since(asked_at);
                if within_real_time(left, still_admitted()).await == Some(false) {
                    lost = true;
                    pending.push_back(txn::Event::AdmissionLost);
                }
            }
            let frame = generation
                .bus
                .state
                .try_lock()
                .ok()
                .map(|state| frame_of(&state, typing.profile, Some(&line)));
            pending.push_back(frame.map_or(txn::Event::Tick, txn::Event::Frame));
            next = pending.pop_front();
        };
        // Submitted, with no human job since admission: the deck's own
        // verified submit reconciles a stale draft bit, under the writer
        // (plan §1.4).
        // The ledger by `try_lock`: contended, a human operation is moving
        // through it, so the bit is not the deck's to clear. The pane input
        // state by `try_lock` too: every pane's writes parse their bytes
        // under it, so it is not waited on under the writer; contended, the
        // bit stays as it was, which holds a later ring rather than letting
        // one through.
        if outcome == txn::Outcome::Submitted
            && generation
                .ledger
                .try_hold()
                .is_some_and(|ledger| ledger.marks().job_mark == txn.j0())
            && let Ok(mut input) = self.pane_input.try_lock()
        {
            input.reconcile_deck_submit(&pane);
        }
        if let txn::Outcome::Unresolved(cause) = outcome {
            tracing::warn!(
                target: "doorbell",
                pane_id = %pane,
                agent_id = %agent,
                attempt = txn.attempt(),
                ?cause,
                cell = ?generation.doorbell.cell().load(),
                "doorbell unresolved"
            );
            let left = txn.deadline().saturating_duration_since(Instant::now());
            if !left.is_zero() {
                let waited = self.request_reprojection_and_wait(&pane, &agent);
                let _ = within_real_time(REPROJECT_WAIT.min(left), waited).await;
            }
        }
        tracing::info!(
            target: "doorbell",
            pane_id = %pane,
            agent_id = %agent,
            attempt = txn.attempt(),
            ?outcome,
            cr_reached = txn.cr_reached(),
            "doorbell attempt ended"
        );
        let report = RingReport {
            outcome: RingOutcome::Txn(outcome),
            attempt: Some(txn.attempt()),
            cr_reached: txn.cr_reached(),
        };
        // A job the writer is still running (stalled) runs on after the
        // release; one still queued was taken back by the transaction.
        drop(job);
        drop(guard);
        self.doorbell_txn_end(&agent);
        report
    }

    /// One step of a transaction, with no await in it (plan §2.4): `event`
    /// (none for its start) fed to the transaction through a fresh seam, and
    /// what it asked for done, its job enqueued while the seam's guards are
    /// still held; then the guards dropped, and a job the transaction took
    /// back accounted once. Answers when to look again and, once it ended,
    /// how.
    #[allow(clippy::too_many_arguments)]
    fn ring_step(
        &self,
        txn: &mut txn::Txn<'_>,
        event: Option<txn::Event>,
        generation: &Generation,
        profile: &'static crate::doorbell_screen::ScreenProfile,
        line: &str,
        guard: &tokio::sync::OwnedMutexGuard<PaneWriter>,
        job: &mut Option<JobSlot>,
        pending: &mut std::collections::VecDeque<txn::Event>,
    ) -> (Option<Instant>, Option<txn::Outcome>) {
        let mut seam = DaemonSeam {
            generation,
            pane_input: &self.pane_input,
            profile,
            line: line.to_string(),
            held: None,
        };
        let now = Instant::now();
        let actions = match event {
            None => txn.start(&mut seam, now),
            Some(event) => txn.on(event, &mut seam, now),
        };
        let mut wake: Option<Instant> = None;
        let mut released = None;
        for action in actions {
            match action {
                txn::Action::Enqueue(next, claim) => {
                    let bytes = match next {
                        txn::Job::Line(text) => text.into_bytes(),
                        txn::Job::Cr => b"\r".to_vec(),
                        txn::Job::Deletes(n) => vec![0x7f; n],
                    };
                    tracing::debug!(
                        target: "doorbell",
                        pane_id = %generation.pane_id,
                        agent_id = %generation.agent_id,
                        attempt = txn.attempt(),
                        bytes = bytes.len(),
                        checked = if job.is_none() && !txn.cr_reached() { "W0 or J0" } else { "J0" },
                        "doorbell seam"
                    );
                    match guard.queue_deck_job(bytes, claim) {
                        Some(queued) => {
                            *job = Some(JobSlot {
                                job: queued,
                                started_at: None,
                                stalled: false,
                            })
                        }
                        None => pending.push_back(txn::Event::JobEnded {
                            accepted: 0,
                            ok: false,
                        }),
                    }
                }
                txn::Action::WakeAt(at) => wake = Some(wake.map_or(at, |w: Instant| w.min(at))),
                txn::Action::Reproject => {
                    self.request_reprojection(&generation.pane_id, &generation.agent_id)
                }
                txn::Action::Release(outcome) => released = Some(outcome),
            }
        }
        seam.release();
        if job
            .as_mut()
            .is_some_and(|slot| slot.job.reconcile_take_back())
        {
            *job = None;
        }
        (wake, released)
    }

    /// Ask the publisher to re-project `agent_id`'s generation in `pane_id`
    /// (plan §3.4′). Never blocks and takes no lock of the transaction's;
    /// nothing is done when no publisher runs.
    pub fn request_reprojection(&self, pane_id: &str, agent_id: &str) {
        if let Some(tx) = self.doorbell_publisher.get() {
            let _ = tx.send((pane_id.to_string(), agent_id.to_string(), None));
        }
    }

    /// [`Self::request_reprojection`], answering once it has run.
    fn request_reprojection_and_wait(
        &self,
        pane_id: &str,
        agent_id: &str,
    ) -> impl std::future::Future<Output = ()> + use<> {
        let (done, ran) = oneshot::channel();
        if let Some(tx) = self.doorbell_publisher.get() {
            let _ = tx.send((pane_id.to_string(), agent_id.to_string(), Some(done)));
        }
        async move {
            let _ = ran.await;
        }
    }

    /// Start the re-projection publisher (plan §3.4′): one task that runs
    /// requests one at a time, in order, a generation's pending ones
    /// coalesced. A second call starts nothing.
    pub fn spawn_doorbell_publisher(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        type Key = (String, String);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Reprojection>();
        self.doorbell_publisher.set(tx).ok()?;
        let registry = Arc::downgrade(self);
        Some(tokio::spawn(async move {
            // Each key once, with when it was first asked for: a request for a
            // key already queued joins it, and one for a key being run is
            // queued again behind it.
            let mut queue: std::collections::VecDeque<(Key, Instant)> =
                std::collections::VecDeque::new();
            let mut waiting: HashMap<Key, Vec<oneshot::Sender<()>>> = HashMap::new();
            let take =
                |request: Reprojection,
                 queue: &mut std::collections::VecDeque<(Key, Instant)>,
                 waiting: &mut HashMap<Key, Vec<oneshot::Sender<()>>>| {
                    let (pane, agent, done) = request;
                    let key = (pane, agent);
                    if let Some(done) = done {
                        waiting.entry(key.clone()).or_default().push(done);
                    }
                    if !queue.iter().any(|(queued, _)| *queued == key) {
                        queue.push_back((key, Instant::now()));
                    }
                };
            while let Some(request) = rx.recv().await {
                take(request, &mut queue, &mut waiting);
                while let Ok(request) = rx.try_recv() {
                    take(request, &mut queue, &mut waiting);
                }
                while let Some(((pane, agent), asked)) = queue.pop_front() {
                    let Some(registry) = registry.upgrade() else {
                        return;
                    };
                    #[cfg(test)]
                    registry.publisher_gate_wait().await;
                    let started = Instant::now();
                    // A run that panics is logged and the publisher goes on:
                    // it serves every generation in the daemon.
                    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        registry.reproject(&pane, &agent)
                    }));
                    if ran.is_err() {
                        tracing::error!(
                            target: "doorbell",
                            pane_id = %pane,
                            agent_id = %agent,
                            "doorbell reprojection panicked; the publisher goes on"
                        );
                    }
                    tracing::debug!(
                        target: "doorbell",
                        pane_id = %pane,
                        agent_id = %agent,
                        queued_us = started.duration_since(asked).as_micros() as u64,
                        took_us = started.elapsed().as_micros() as u64,
                        "doorbell reprojection"
                    );
                    for done in waiting.remove(&(pane, agent)).unwrap_or_default() {
                        let _ = done.send(());
                    }
                    drop(registry);
                    while let Ok(request) = rx.try_recv() {
                        take(request, &mut queue, &mut waiting);
                    }
                }
            }
        }))
    }

    /// One re-projection (plan §3.4′): nothing for a generation that no
    /// longer holds its pane; otherwise its cell as it is now onto its card,
    /// and whether the deck will type into it at all onto its WAKE (Q16,
    /// Q18). Installs nothing of its own.
    pub(super) fn reproject(&self, pane_id: &str, agent_id: &str) {
        let _serial = self
            .doorbell_publishing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.pane_current_agent_id(pane_id).as_deref() != Some(agent_id) {
            return;
        }
        let Some(generation) = self.doorbell_generation(pane_id, agent_id) else {
            return;
        };
        let shown = match txn::project(generation.doorbell.cell().load()) {
            Some(txn::Shown::UncertainInput) => {
                Some(crate::inbox_display::WakeHold::UncertainInput)
            }
            Some(txn::Shown::DialogDuringRing) => {
                Some(crate::inbox_display::WakeHold::DialogDuringRing)
            }
            None => None,
        };
        // Its activation verdict, then the clauses that hold it for the rest
        // of its life (Q18); transient ones stay out, so WAKE does not flap.
        let refused = self
            .doorbell_typing(&generation, true)
            .err()
            .or_else(|| {
                generation
                    .mode
                    .lock()
                    .unwrap()
                    .sticky()
                    .map(crate::input_mode::ModeClause::as_str)
            })
            .or_else(|| {
                generation
                    .doorbell
                    .counter()
                    .exhausted()
                    .then_some("attempt_exhausted")
            });
        #[cfg(test)]
        self.publisher_write_gate_wait();
        // Written only while the generation is still the pane's live one,
        // asked under the hub's hold: an exit since the reads above leaves
        // nothing behind for the retired generation.
        let (_, refusal_changed) =
            self.inbox()
                .set_wake_projection(pane_id, agent_id, shown, refused, || {
                    self.pane_current_agent_id(pane_id).as_deref() == Some(agent_id)
                });
        if refusal_changed && let Some(clause) = refused {
            // The line troubleshooting points at: why the deck types nothing
            // into this generation, and the releases of its agent it would.
            let measured: Vec<String> = generation
                .agent
                .as_ref()
                .map(|agent| {
                    crate::doorbell_activation::RELEASE_RECORDS
                        .iter()
                        .filter(|record| record.agent == *agent)
                        .filter(|record| {
                            matches!(
                                crate::doorbell_activation::activation(
                                    &crate::doorbell_activation::RELEASE_RECORDS,
                                    agent,
                                    &crate::doorbell_activation::LatchedRelease::Known(
                                        record.release.clone()
                                    ),
                                ),
                                crate::doorbell_activation::Activation::Go { .. }
                            )
                        })
                        .map(|record| record.release.to_string())
                        .collect()
                })
                .unwrap_or_default();
            tracing::info!(
                target: "doorbell",
                pane_id,
                agent_id,
                agent = ?generation.agent,
                clause,
                measured = %measured.join(", "),
                "doorbell off: the deck does not type into this agent"
            );
        }
    }

    /// Test seam: while held, a re-projection that has read its inputs
    /// waits here, before its hub write, and says it got there.
    #[cfg(test)]
    fn publisher_write_gate_wait(&self) {
        let (held, reached) = &*self.publisher_write_gate;
        let mut held_now = held.lock().unwrap();
        if *held_now {
            reached.store(true, Ordering::SeqCst);
        }
        while *held_now {
            held_now = self.publisher_write_released.wait(held_now).unwrap();
        }
    }

    /// Test seam: hold (`true`) re-projections between their reads and their
    /// hub write, or let them go. Answers whether one has reached the hold
    /// since it was set.
    #[cfg(test)]
    pub(crate) fn hold_publisher_write(&self, held: bool) {
        let (gate, reached) = &*self.publisher_write_gate;
        *gate.lock().unwrap() = held;
        if held {
            reached.store(false, Ordering::SeqCst);
        } else {
            self.publisher_write_released.notify_all();
        }
    }

    /// Test seam: whether a re-projection waits at the write hold.
    #[cfg(test)]
    pub(crate) fn publisher_write_reached(&self) -> bool {
        self.publisher_write_gate.1.load(Ordering::SeqCst)
    }

    /// Test seam: hold the publisher before its next run until released.
    #[cfg(test)]
    async fn publisher_gate_wait(&self) {
        let gate = self.publisher_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            let _ = gate.acquire().await;
        }
    }

    /// Test seam: run `seam` once, in the next ring, after its ledger drained
    /// and before admission reads the ledger.
    #[cfg(test)]
    pub(crate) fn set_ring_admission_seam(&self, seam: impl FnOnce() + Send + 'static) {
        *self.ring_admission_seam.lock().unwrap() = Some(Box::new(seam));
    }

    #[cfg(test)]
    fn ring_admission_seam_run(&self) {
        let seam = self.ring_admission_seam.lock().unwrap().take();
        if let Some(seam) = seam {
            seam();
        }
    }

    /// Test seam: give every typed generation the first-ring settle window
    /// `settle`, or its release's again with `None`.
    #[cfg(test)]
    pub(crate) fn set_doorbell_settle_for_test(&self, settle: Option<Duration>) {
        *self.doorbell_settle_for_test.lock().unwrap() = settle;
    }

    /// Test seam: close the publisher's gate (`true`) so its runs wait, or
    /// open it.
    #[cfg(test)]
    pub(crate) fn hold_publisher(&self, held: bool) {
        let mut gate = self.publisher_gate.lock().unwrap();
        match (held, gate.as_ref()) {
            (true, None) => *gate = Some(Arc::new(tokio::sync::Semaphore::new(0))),
            (false, Some(semaphore)) => {
                semaphore.add_permits(usize::MAX >> 4);
                *gate = None;
            }
            _ => {}
        }
    }
}

/// One re-projection request: the pane, the generation, and who waits for
/// it to have run.
pub(crate) type Reprojection = (String, String, Option<oneshot::Sender<()>>);
