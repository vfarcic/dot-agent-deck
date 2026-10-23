//! PRD #1666 part 3, round 5 (plan §2, D1, D4, D5, D12, D15): the single entry
//! a worker's `work-done` reaches the daemon through.
//!
//! [`complete`] is called by the hook socket's `work_done_task` arm, by the
//! legacy `work_done` arm (through [`complete_legacy`]), and by tests. The
//! connection writes its one line from the [`Completion`] it returns, never
//! before, and writes none when the outcome is unconfirmed.
//!
//! The transaction, in order:
//!
//! 1. **Admit, with no effects.** The report's size, its mode, and that task
//!    and terminal reports come from a generation the hook token attests. The
//!    role map (R0) is read into an owned snapshot and released before any
//!    ranked lock. A note checks the snapshot against the generation's own
//!    pinned seat, so it never goes to whoever holds the pane now.
//! 2. **Claim.** One hub section answers a repeat from what the first report
//!    left, refuses what is not this generation's to report, waits on a claim
//!    already saving the same task or unit, or installs a claim holding the
//!    room the report will consume (a terminal claim does this at the routes'
//!    linearization point, [`AgentPtyRegistry::admit_terminal_claim`]).
//! 3. **Save**, on a task of its own, under a supervisor: the report's file
//!    in the retained store (written, `fsync`ed, its directory synced), the
//!    convenience copies, and the composed post.
//! 4. **Stage, mark, commit, or fail.** The post is staged into the claim; a
//!    terminal claim's unit is marked complete with nothing held; then the
//!    commit publishes it. A failure before staging fails the claim; from
//!    staging on, only roll-forward exists.
//! 5. **Hand the result over** to the supervisor, which forwards it to the
//!    connection, before anything else runs. A panic in the save task is
//!    settled by the supervisor from the claim's phase.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, oneshot};
use tracing::{error, info, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::config_validation::escape_id_for_log;
use crate::event::{
    SignalAck, WorkDoneDisposition, WorkDoneMode, WorkDoneSignal, WorkDoneTaskReply,
};
use crate::hook_provenance::{AttestFacts, Provenance};
use crate::inbox::Recipient;
use crate::inbox_hub::{Admission, ClaimSpec, Claimed, CommitOutcome, FailOutcome};
use crate::report_recovery::{ClaimMode, ClaimPhase, ClaimState, Destination, UnitMark};
use crate::state::AppState;
use crate::work_done_text::{self as text, code};

/// Test pause point: after the report's provenance was classified and before
/// anything is read or reserved for it.
pub(crate) const PAUSE_ATTESTED: &str = "work-done-attested";
/// Test pause point: a terminal report after its role-map snapshot and before
/// it takes the routes.
pub(crate) const PAUSE_TERMINAL_RESOLVED: &str = "work-done-terminal-resolved";
/// Test pause point, in the save task: the report's file is written and
/// nothing is staged.
pub(crate) const PAUSE_SAVED: &str = "work-done-saved";
/// Test pause point, in the save task: a terminal claim's unit is marked and
/// the commit has not run.
pub(crate) const PAUSE_MARKED: &str = "work-done-marked";
/// Test pause point, on the connection: the result is in hand and no byte of
/// the answer is written yet.
pub(crate) const PAUSE_COMMITTED: &str = "work-done-committed";
/// Test pause point, on the connection: the answer's line is written.
pub(crate) const PAUSE_ACKED: &str = "work-done-acked";

/// Test fault: `Fail` makes the report's save fail.
pub(crate) const FAULT_FAIL_SAVE: &str = "work-done-fail-save";
/// Test fault: a panic in the save task after the save and before staging.
pub(crate) const FAULT_BEFORE_STAGE: &str = "work-done-before-stage";
/// Test fault: a panic after staging and before the unit mark.
pub(crate) const FAULT_STAGED: &str = "work-done-staged";
/// Test fault: a panic after the mark and before the commit.
pub(crate) const FAULT_MARKED: &str = "work-done-marked";
/// Test fault: a panic after the commit's section returned and before the
/// result is handed over (`/053` (e)).
pub(crate) const FAULT_COMMIT_RETURNED: &str = "commit-returned-before-handover";
/// Test fault: a panic in the after-section helpers, after the handover.
pub(crate) const FAULT_AFTER_HANDOVER: &str = "work-done-after-handover";

/// How long a repeat waits for a claim already saving its task or unit
/// before it answers `report_pending`, unless [`ENV_CLAIM_WAIT_MS`] says.
pub const CLAIM_WAIT: Duration = Duration::from_secs(30);
/// A test knob lowering [`CLAIM_WAIT`], in milliseconds.
pub const ENV_CLAIM_WAIT_MS: &str = "DOT_AGENT_DECK_WORK_DONE_WAIT_MS";

fn claim_wait() -> Duration {
    std::env::var(ENV_CLAIM_WAIT_MS)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(CLAIM_WAIT)
}

/// A report, as the transaction takes it.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub pane_id: String,
    pub mode: WorkDoneMode,
    pub report: String,
}

/// How a report was settled, as its connection answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// The answer.
    Reply(WorkDoneTaskReply),
    /// The outcome is not known: the connection closes with no line, and the
    /// sender says the report may have been accepted.
    Unconfirmed,
}

impl Completion {
    fn refused(code: &str, message: String) -> Self {
        Completion::Reply(WorkDoneTaskReply::refused(code, &message))
    }

    /// The legacy `work_done` verb's answer: a [`SignalAck`], or no line.
    pub fn to_signal_ack(&self) -> Option<SignalAck> {
        match self {
            Completion::Reply(reply) => Some(match reply.disposition {
                WorkDoneDisposition::Refused => SignalAck::refused(
                    reply.code.as_deref().unwrap_or("refused"),
                    reply.message.as_deref().unwrap_or_default(),
                ),
                _ => SignalAck::accepted(),
            }),
            Completion::Unconfirmed => None,
        }
    }
}

/// What the role map said about the reporting pane, read once (R0) and
/// released before any ranked lock.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    role: Option<String>,
    instance: Option<String>,
    is_orchestrator: bool,
    orchestrator_pane: Option<String>,
    cwd: Option<String>,
}

async fn snapshot(state: &RwLock<AppState>, pane_id: &str) -> Snapshot {
    Snapshot::of(&*state.read().await, pane_id)
}

impl Snapshot {
    fn of(state: &AppState, pane_id: &str) -> Snapshot {
        Snapshot {
            role: state.pane_role_map.get(pane_id).cloned(),
            instance: state
                .pane_orchestration_map
                .get(pane_id)
                .map(|identity| identity.id.clone()),
            is_orchestrator: state.orchestrator_pane_ids.contains(pane_id),
            orchestrator_pane: state.orchestrator_for_worker(pane_id),
            cwd: state.pane_cwd_map.get(pane_id).cloned(),
        }
    }
}

/// The legacy `work_done` verb, mapped (plan D1): `done` is a terminal
/// report, anything else a note; the same transaction runs.
pub async fn complete_legacy(
    state: &RwLock<AppState>,
    registry: &Arc<AgentPtyRegistry>,
    signal: WorkDoneSignal,
    provenance: &Provenance,
) -> Completion {
    let request = CompletionRequest {
        pane_id: signal.pane_id,
        mode: if signal.done {
            WorkDoneMode::Terminal
        } else {
            WorkDoneMode::Uncorrelated
        },
        report: signal.task,
    };
    complete(state, registry, request, provenance).await
}

/// What a worker in `pane_id` sends with `work-done --task-id`, for its
/// oldest open delegated task (a note when it has none, `--done` when
/// `terminal`), attested as the pane's live generation with that
/// generation's pinned facts. For tests and harnesses that drive the
/// transaction directly.
pub fn worker_report(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    report: &str,
    terminal: bool,
) -> (CompletionRequest, Provenance) {
    let mode = if terminal {
        WorkDoneMode::Terminal
    } else {
        match registry.inbox().open_tasks_of(pane_id).into_iter().next() {
            Some(task_id) => WorkDoneMode::Task { task_id },
            None => WorkDoneMode::Uncorrelated,
        }
    };
    let provenance = match registry.pane_current_agent_id(pane_id) {
        Some(agent_id) => Provenance::Attested {
            facts: registry.attest_facts(&agent_id).unwrap_or_default(),
            agent_id,
        },
        None => Provenance::Unattested,
    };
    (
        CompletionRequest {
            pane_id: pane_id.to_string(),
            mode,
            report: report.to_string(),
        },
        provenance,
    )
}

/// Take one `work-done` report: see the module documentation.
pub async fn complete(
    state: &RwLock<AppState>,
    registry: &Arc<AgentPtyRegistry>,
    request: CompletionRequest,
    provenance: &Provenance,
) -> Completion {
    registry.pause_point(PAUSE_ATTESTED).await;
    if let Err(refusal) = precheck(&request, provenance) {
        return refusal;
    }
    let snap = snapshot(state, &request.pane_id).await;
    transact(registry, request, provenance, snap).await
}

/// [`complete`], for a caller that already holds the role map: tests, and a
/// client-side harness. The snapshot is read from `state` the same way.
pub async fn complete_with_state(
    state: &AppState,
    registry: &Arc<AgentPtyRegistry>,
    request: CompletionRequest,
    provenance: &Provenance,
) -> Completion {
    registry.pause_point(PAUSE_ATTESTED).await;
    if let Err(refusal) = precheck(&request, provenance) {
        return refusal;
    }
    let snap = Snapshot::of(state, &request.pane_id);
    transact(registry, request, provenance, snap).await
}

/// Step 1's checks that read nothing: size, mode, attestation.
fn precheck(request: &CompletionRequest, provenance: &Provenance) -> Result<(), Completion> {
    let limit = crate::bounded_read::MAX_TASK_BYTES;
    if request.report.len() as u64 > limit {
        return Err(Completion::refused(
            code::REPORT_TOO_LARGE,
            text::report_too_large(request.report.len(), limit),
        ));
    }
    if request.mode == WorkDoneMode::Unknown {
        return Err(Completion::refused(
            code::UNSUPPORTED_MODE,
            text::unsupported_mode(),
        ));
    }
    if !matches!(provenance, Provenance::Attested { .. })
        && request.mode != WorkDoneMode::Uncorrelated
    {
        return Err(Completion::refused(code::UNATTESTED, text::unattested()));
    }
    Ok(())
}

async fn transact(
    registry: &Arc<AgentPtyRegistry>,
    request: CompletionRequest,
    provenance: &Provenance,
    snap: Snapshot,
) -> Completion {
    let attested = match provenance {
        Provenance::Attested { agent_id, facts } => Some((agent_id.clone(), facts.clone())),
        _ => None,
    };
    let worker_agent = match &attested {
        Some((agent, _)) => agent.clone(),
        None => registry
            .pane_current_agent_id(&request.pane_id)
            .unwrap_or_default(),
    };
    let digest = digest(&request.report);
    let report_id = crate::retained_store::mint_report_id();
    let framed = crate::state::frame_untrusted_report_for_file(&request.report);
    let spec = ClaimSpec {
        worker_pane: request.pane_id.clone(),
        worker_agent: worker_agent.clone(),
        digest,
        report_id,
        // The file's bound: the framed report and the largest envelope.
        file_bytes: (crate::retained_store::MAGIC.len()
            + crate::retained_store::ENVELOPE_MAX
            + framed.len()
            + 4) as u64,
    };
    let deadline = tokio::time::Instant::now() + claim_wait();
    loop {
        let admission = match &request.mode {
            WorkDoneMode::Task { task_id } => {
                if !crate::inbox::is_valid_message_id(task_id) {
                    return Completion::refused(
                        code::UNKNOWN_TASK,
                        text::unknown_task(
                            &escape_id_for_log(task_id),
                            crate::report_recovery::tombstones_max_from_env(),
                        ),
                    );
                }
                let seat_instance = attested
                    .as_ref()
                    .and_then(|(_, facts)| facts.seat.as_ref())
                    .map(|seat| seat.instance.clone());
                registry
                    .inbox()
                    .admit_task_claim(&spec, task_id, seat_instance.as_deref())
            }
            WorkDoneMode::Terminal => {
                registry.pause_point(PAUSE_TERMINAL_RESOLVED).await;
                let facts = attested
                    .as_ref()
                    .map(|(_, facts)| facts.clone())
                    .unwrap_or_default();
                registry.admit_terminal_claim(&spec, &facts)
            }
            WorkDoneMode::Uncorrelated => {
                match note_recipient(
                    registry,
                    &request.pane_id,
                    &snap,
                    attested
                        .as_ref()
                        .map(|(agent, facts)| (agent.as_str(), facts)),
                ) {
                    Ok(recipient) => registry.inbox().admit_uncorrelated_claim(
                        &spec,
                        recipient,
                        snap.role.clone(),
                    ),
                    Err(refusal) => return refusal,
                }
            }
            WorkDoneMode::Unknown => {
                return Completion::refused(code::UNSUPPORTED_MODE, text::unsupported_mode());
            }
        };
        match admission {
            Admission::Answer(reply) => {
                if reply.destination == Some(Destination::None) {
                    info!(
                        pane_id = %escape_id_for_log(&request.pane_id),
                        report_chars = request.report.chars().count(),
                        "orchestration complete (orchestrator --done)"
                    );
                }
                return Completion::Reply(reply);
            }
            Admission::Wait(mut settled) => {
                let waited = tokio::time::timeout_at(
                    deadline,
                    settled.wait_for(|state| *state != ClaimState::Pending),
                )
                .await;
                if waited.is_err() {
                    let why = match request.mode {
                        WorkDoneMode::Terminal => text::Pending::Unit,
                        _ => text::Pending::Task,
                    };
                    return Completion::refused(code::REPORT_PENDING, text::report_pending(why));
                }
                // Admitted again: the first report's result, or a fresh claim.
                continue;
            }
            Admission::Claimed(claimed) => {
                return supervise(
                    Arc::clone(registry),
                    claimed,
                    SaveContext {
                        spec: spec.clone(),
                        report: request.report.clone(),
                        framed,
                        snap,
                        worker_agent,
                    },
                )
                .await;
            }
        }
    }
}

/// The orchestrator a note goes to, from the snapshot, or the refusal it
/// gets. An attested note is checked against the generation's own seat: a
/// pane that now belongs to another orchestration gets nothing (plan §2
/// step 1). A generation spawned with no orchestration seat has only the
/// role map as evidence, so it must still hold its pane.
fn note_recipient(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    snap: &Snapshot,
    attested: Option<(&str, &AttestFacts)>,
) -> Result<Option<Recipient>, Completion> {
    let orchestrator_note =
        || Completion::refused(code::INCOMPATIBLE_MODE, text::orchestrator_note());
    match attested {
        Some((_, facts)) if facts.seat.as_ref().is_some_and(|seat| seat.is_orchestrator) => {
            return Err(orchestrator_note());
        }
        Some((_, facts))
            if facts
                .seat
                .as_ref()
                .is_some_and(|seat| snap.instance.as_deref() != Some(seat.instance.as_str())) =>
        {
            return Err(Completion::refused(code::NOT_A_WORKER, text::pane_reused()));
        }
        Some((agent, facts))
            if facts.seat.is_none()
                && registry.pane_current_agent_id(pane_id).as_deref() != Some(agent) =>
        {
            return Err(Completion::refused(code::NOT_A_WORKER, text::pane_reused()));
        }
        Some((_, facts)) if facts.seat.is_none() && snap.is_orchestrator => {
            return Err(orchestrator_note());
        }
        None if snap.is_orchestrator => return Err(orchestrator_note()),
        _ => {}
    }
    if snap.role.is_none() {
        // Issue #1082's log line: the pane id is producer-supplied here.
        warn!(
            pane_id = %escape_id_for_log(pane_id),
            "work-done from unknown pane"
        );
        return Err(Completion::refused(
            code::NOT_A_WORKER,
            text::not_a_worker(),
        ));
    }
    let Some(orchestrator) = snap.orchestrator_pane.as_deref() else {
        return Err(Completion::refused(
            code::NOT_A_WORKER,
            text::not_a_worker(),
        ));
    };
    Ok(registry
        .pane_current_agent_id(orchestrator)
        .map(|agent| Recipient::bound(orchestrator, &agent)))
}

fn digest(report: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(report.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// What the save task needs, captured at admission.
struct SaveContext {
    spec: ClaimSpec,
    report: String,
    framed: String,
    snap: Snapshot,
    worker_agent: String,
}

/// Spawn the save task under its supervisor, and wait for the result the
/// supervisor forwards (plan D12 rules 2 and 11).
async fn supervise(
    registry: Arc<AgentPtyRegistry>,
    claimed: Claimed,
    ctx: SaveContext,
) -> Completion {
    let (to_connection, from_supervisor) = oneshot::channel::<Completion>();
    let (handed, result) = oneshot::channel::<Completion>();
    let claim_id = claimed.claim_id;
    let mode = claimed.mode.clone();
    let report_id = ctx.spec.report_id.clone();
    let message_id = claimed.message_id.clone();
    let recipient_pane = claimed.recipient.as_ref().map(|to| to.pane_id.clone());
    let unit_id = match &claimed.mode {
        ClaimMode::Terminal { unit_id, .. } => Some(unit_id.clone()),
        _ => None,
    };
    let task = tokio::spawn(save(Arc::clone(&registry), claimed, ctx, handed));
    tokio::spawn(async move {
        let mut task = task;
        let mut result = result;
        tokio::select! {
            biased;
            handed = &mut result => {
                if let Ok(completion) = handed {
                    let _ = to_connection.send(completion);
                    if let Err(join) = task.await {
                        error!(%join, report_id = %report_id, "work-done: the save task panicked after handing over its result");
                    }
                    return;
                }
            }
            _ = &mut task => {}
        }
        // The save task ended without handing a result over: read the claim.
        if let Ok(completion) = result.try_recv() {
            let _ = to_connection.send(completion);
            return;
        }
        if let Some(completion) = recover(
            &registry,
            claim_id,
            &mode,
            &report_id,
            &message_id,
            recipient_pane.as_deref(),
            unit_id.as_deref(),
        )
        .await
        {
            let _ = to_connection.send(completion);
        }
        // Otherwise the oneshot is dropped with no value: unconfirmed.
    });
    from_supervisor.await.unwrap_or(Completion::Unconfirmed)
}

/// The supervisor's settlement of a save task that ended without a result
/// (D12 rule 11). `None` answers nothing: the connection reads unconfirmed.
async fn recover(
    registry: &Arc<AgentPtyRegistry>,
    claim_id: u64,
    mode: &ClaimMode,
    report_id: &str,
    message_id: &str,
    recipient_pane: Option<&str>,
    unit_id: Option<&str>,
) -> Option<Completion> {
    let file = format!("r-{report_id}.md");
    let phase = match registry.inbox().claim_phase(claim_id) {
        Ok(phase) => phase,
        Err(_) => {
            error!(
                report_id,
                file = %file,
                "work-done: a report's save task ended and the deck's report state is torn; its \
                 result is unconfirmed, and the report is kept in its file"
            );
            return None;
        }
    };
    match phase {
        None => match registry
            .inbox()
            .committed_answer(mode, report_id, message_id, recipient_pane)
        {
            Ok(Some(reply)) => Some(Completion::Reply(reply)),
            _ => None,
        },
        Some(ClaimPhase::Admitted | ClaimPhase::Saving) => match registry.fail_claim(claim_id) {
            (FailOutcome::Failed { .. }, after) => {
                if let Some(after) = after {
                    registry.inbox().after_section(after);
                }
                Some(Completion::refused(
                    code::REPORT_SAVE_FAILED,
                    text::save_failed("the deck's save of it stopped"),
                ))
            }
            _ => None,
        },
        Some(ClaimPhase::Staged | ClaimPhase::Marked) => {
            if let Some(unit_id) = unit_id {
                let mark = registry
                    .dispatched_units()
                    .mark_completed_unit(unit_id, chrono::Utc::now().timestamp_millis());
                let _ = registry.inbox().note_claim_marked(claim_id, mark);
            }
            let (outcome, after) = registry.commit_claim(claim_id, unit_id.is_some());
            if let Some(after) = after {
                registry.inbox().after_section(after);
            }
            committed(outcome, mode)
        }
        Some(ClaimPhase::Applying) => {
            error!(
                report_id,
                file = %file,
                "work-done: a report's commit was interrupted; its result is unconfirmed, and the \
                 report is kept in its file"
            );
            None
        }
    }
}

/// The answer a commit gives, or `None` when it is unknown.
fn committed(outcome: CommitOutcome, mode: &ClaimMode) -> Option<Completion> {
    match outcome {
        CommitOutcome::Committed {
            destination,
            report_id,
            task_id,
            ..
        } => Some(Completion::Reply(WorkDoneTaskReply {
            disposition: match mode {
                ClaimMode::Uncorrelated => WorkDoneDisposition::Uncorrelated,
                _ => WorkDoneDisposition::Accepted,
            },
            task_id,
            report_id: Some(report_id),
            destination: Some(destination),
            differs: false,
            code: None,
            message: None,
        })),
        CommitOutcome::Gone | CommitOutcome::Unavailable => None,
    }
}

/// The save task (plan §2 steps 4 and 5).
async fn save(
    registry: Arc<AgentPtyRegistry>,
    claimed: Claimed,
    ctx: SaveContext,
    handed: oneshot::Sender<Completion>,
) {
    let claim_id = claimed.claim_id;
    let _ = registry
        .inbox()
        .note_claim_phase(claim_id, ClaimPhase::Saving);
    let Some(store) = registry.inbox().retained_store() else {
        settle_failure(
            &registry,
            claim_id,
            "the deck has no saved-results store",
            handed,
        );
        return;
    };
    // The retained file.
    let (task_id, unit_id) = match &claimed.mode {
        ClaimMode::Task { task_id } => (Some(task_id.clone()), None),
        ClaimMode::Terminal { unit_id, .. } => (None, Some(unit_id.clone())),
        ClaimMode::Uncorrelated => (None, None),
    };
    let envelope = crate::retained_store::Envelope {
        report_id: ctx.spec.report_id.clone(),
        kind: if unit_id.is_some() {
            crate::retained_store::ArtifactKind::DispatchFinished
        } else {
            crate::retained_store::ArtifactKind::WorkerReport
        },
        task_id: task_id.clone(),
        unit_id: unit_id.clone(),
        deck_id: registry.inbox().deck().to_string(),
        run_id: store.run_id().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        producer_agent_id: ctx.worker_agent.clone(),
        recipient_agent_id: claimed
            .recipient
            .as_ref()
            .and_then(|to| to.agent_id.clone())
            .unwrap_or_default(),
        instance: ctx.snap.instance.clone(),
        producer_supplied: crate::retained_store::ProducerSupplied {
            unit_name: claimed.unit_name.clone(),
            role: claimed.role.clone().or_else(|| ctx.snap.role.clone()),
            producer_pane_id: ctx.spec.worker_pane.clone(),
            recipient_pane_id: claimed
                .recipient
                .as_ref()
                .map(|to| to.pane_id.clone())
                .unwrap_or_default(),
            orchestration: None,
        },
        truncated: Vec::new(),
    };
    let contents = crate::retained_store::compose_file(&envelope, &ctx.framed);
    let reservation = claimed.reservation;
    if contents.len() as u64 > reservation.bytes() {
        store.release(reservation);
        settle_failure(
            &registry,
            claim_id,
            "the report's file is larger than its room",
            handed,
        );
        return;
    }
    let written = {
        let store = Arc::clone(&store);
        let report_id = ctx.spec.report_id.clone();
        tokio::task::spawn_blocking(move || {
            store
                .write(
                    reservation,
                    &report_id,
                    &contents,
                    crate::retained_store::Now::current(),
                )
                .map_err(|failure| format!("{failure:?}"))
        })
        .await
    };
    registry.pause_point(PAUSE_SAVED).await;
    // Test seam: the save fails here, after the file was written; the file
    // is removed as a failed save's is.
    let written = match written {
        Ok(Ok(())) if registry.fault_point(FAULT_FAIL_SAVE) => {
            store.discard(&ctx.spec.report_id);
            Ok(Err("an injected save failure (test)".to_string()))
        }
        other => other,
    };
    match written {
        Ok(Ok(())) => {}
        Ok(Err(detail)) => {
            warn!(
                report_id = %ctx.spec.report_id,
                dir = %store.dir().display(),
                %detail,
                "work-done: the report could not be saved; nothing was reported"
            );
            settle_failure(
                &registry,
                claim_id,
                &format!("{}: {detail}", store.dir().display()),
                handed,
            );
            return;
        }
        Err(join) => {
            error!(%join, "work-done: saving the report did not finish");
            settle_failure(&registry, claim_id, "its save did not finish", handed);
            return;
        }
    }
    let _ = registry.fault_point(FAULT_BEFORE_STAGE);
    // The composed post, with the convenience copies.
    let post = match &claimed.mode {
        ClaimMode::Task { task_id } => crate::state::ReportPost::Task {
            task_id: task_id.clone(),
            role: claimed
                .role
                .clone()
                .or_else(|| ctx.snap.role.clone())
                .unwrap_or_else(|| "worker".to_string()),
            cwd: ctx.snap.cwd.clone(),
        },
        ClaimMode::Uncorrelated => crate::state::ReportPost::Note {
            role: ctx.snap.role.clone(),
            cwd: ctx.snap.cwd.clone(),
        },
        ClaimMode::Terminal { .. } => crate::state::ReportPost::Terminal {
            unit_name: claimed.unit_name.clone().unwrap_or_default(),
            unit_cwd: registry
                .agent_record_any(&ctx.worker_agent)
                .and_then(|record| record.cwd),
        },
    };
    let content = crate::state::compose_report_post(
        post,
        &ctx.spec.worker_pane,
        &ctx.worker_agent,
        &ctx.report,
    )
    .await;
    if registry.inbox().stage_claim(claim_id, content).is_err() {
        // Torn: nothing in-process settles it, and the report is in its file.
        error!(
            report_id = %ctx.spec.report_id,
            "work-done: the report was saved but the deck's report state is torn; its result is \
             unconfirmed"
        );
        let _ = handed.send(Completion::Unconfirmed);
        return;
    }
    let _ = registry.fault_point(FAULT_STAGED);
    if let Some(unit_id) = unit_id.as_deref() {
        let mark = registry
            .dispatched_units()
            .mark_completed_unit(unit_id, chrono::Utc::now().timestamp_millis());
        if mark == UnitMark::UnitEndedFirst {
            info!(
                unit_id,
                "work-done: the unit ended before its report was published"
            );
        }
        let _ = registry.inbox().note_claim_marked(claim_id, mark);
        registry.pause_point(PAUSE_MARKED).await;
    }
    let _ = registry.fault_point(FAULT_MARKED);
    let (outcome, after) = registry.commit_claim(claim_id, unit_id.is_some());
    let _ = registry.fault_point(FAULT_COMMIT_RETURNED);
    let completion = committed(outcome, &claimed.mode).unwrap_or(Completion::Unconfirmed);
    let _ = handed.send(completion);
    // Only after the handover: the wakes and the owed notices.
    let _ = registry.fault_point(FAULT_AFTER_HANDOVER);
    if let Some(after) = after {
        registry.inbox().after_section(after);
    }
}

/// Fail claim `claim_id` after its save failed, and hand the refusal over.
fn settle_failure(
    registry: &Arc<AgentPtyRegistry>,
    claim_id: u64,
    detail: &str,
    handed: oneshot::Sender<Completion>,
) {
    let (outcome, after) = registry.fail_claim(claim_id);
    let completion = match outcome {
        FailOutcome::Failed { .. } => {
            Completion::refused(code::REPORT_SAVE_FAILED, text::save_failed(detail))
        }
        FailOutcome::Staged | FailOutcome::Gone | FailOutcome::Unavailable => {
            Completion::Unconfirmed
        }
    };
    let _ = handed.send(completion);
    if let Some(after) = after {
        registry.inbox().after_section(after);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::agent_pty::TestFault;

    /// The synchronous pause blocks only its own thread, reports when it is
    /// reached, and returns once released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_point_sync_blocks_its_thread_until_released() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (reached, release) = registry.pause_at_sync_for_test("window");
        let paused = {
            let registry = Arc::clone(&registry);
            tokio::task::spawn_blocking(move || {
                registry.pause_point_sync("window");
                "past the pause"
            })
        };
        reached
            .recv_timeout(Duration::from_secs(5))
            .expect("the pause is reached");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!paused.is_finished(), "the pause holds its thread");
        release.send(()).expect("release the pause");
        assert_eq!(paused.await.expect("the paused thread"), "past the pause");
        // Armed once: a second visit passes straight through.
        registry.pause_point_sync("window");
    }

    /// An injected fault fires once: a `Fail` asks its step to fail, a `Panic`
    /// panics with its payload, and an unarmed point does nothing.
    #[test]
    fn fault_point_fires_what_was_injected_once() {
        let registry = AgentPtyRegistry::new();
        assert!(!registry.fault_point("step"));
        registry.inject_fault_for_test("step", TestFault::Fail);
        assert!(registry.fault_point("step"));
        assert!(!registry.fault_point("step"));
        registry.inject_fault_for_test("step", TestFault::Panic("injected-7d31"));
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.fault_point("step");
        }))
        .expect_err("the injected panic fires");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"injected-7d31"));
        assert!(!registry.fault_point("step"));
    }

    /// A report over the size limit, an unknown mode, and an unattested task
    /// report are refused before anything is read or reserved.
    #[tokio::test]
    async fn complete_refuses_before_admission() {
        let state = RwLock::new(AppState::default());
        let registry = Arc::new(AgentPtyRegistry::new());
        let request = |mode: WorkDoneMode, report: String| CompletionRequest {
            pane_id: "task-completion-seam-pane".to_string(),
            mode,
            report,
        };
        let code_of = |completion: Completion| match completion {
            Completion::Reply(reply) => reply.code,
            Completion::Unconfirmed => None,
        };
        let big = "x".repeat(crate::bounded_read::MAX_TASK_BYTES as usize + 1);
        assert_eq!(
            code_of(
                complete(
                    &state,
                    &registry,
                    request(WorkDoneMode::Uncorrelated, big),
                    &Provenance::Unattested
                )
                .await
            )
            .as_deref(),
            Some(code::REPORT_TOO_LARGE)
        );
        assert_eq!(
            code_of(
                complete(
                    &state,
                    &registry,
                    request(WorkDoneMode::Unknown, "r".into()),
                    &Provenance::Unattested
                )
                .await
            )
            .as_deref(),
            Some(code::UNSUPPORTED_MODE)
        );
        assert_eq!(
            code_of(
                complete(
                    &state,
                    &registry,
                    request(
                        WorkDoneMode::Task {
                            task_id: "m-1".into()
                        },
                        "r".into()
                    ),
                    &Provenance::Unattested
                )
                .await
            )
            .as_deref(),
            Some(code::UNATTESTED)
        );
        assert_eq!(
            code_of(
                complete(
                    &state,
                    &registry,
                    request(WorkDoneMode::Terminal, "r".into()),
                    &Provenance::Unattested
                )
                .await
            )
            .as_deref(),
            Some(code::UNATTESTED)
        );
    }
}
