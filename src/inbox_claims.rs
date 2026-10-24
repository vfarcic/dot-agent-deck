//! PRD #1666 part 3, round 5 (plan §2, D4, D5, D12, D15): the inbox hub's half
//! of a task-scoped `work-done`: the claim admission installs, staging, the
//! commit and the failure.
//!
//! A claim is installed in one hub section with the room it will consume: one
//! counted reply room and the retained store's bytes and file slot for its
//! report. From then on it owns that room, and exactly one of [`commit`] or
//! [`fail`] settles it. A failure is refused once a claim is staged: a staged
//! report is only ever rolled forward.
//!
//! Every lock this module needs is acquired, and checked, before its first
//! write. With the report state torn, or the retained store unavailable, the
//! operation writes nothing and says so (plan D15 rule 4). The registry's
//! locks (R1, R2) are taken by the registry, which hands them in or does the
//! tracker's half itself, still holding them.
//!
//! [`commit`]: InboxHub::commit_claim_locked
//! [`fail`]: InboxHub::fail_claim_locked

use super::reports::{AfterSection, ReturnsGuard, dispatch_ended_for};
use super::*;
use crate::event::{WorkDoneDisposition, WorkDoneTaskReply};
use crate::guarded::Torn;
use crate::hook_provenance::{AttestFacts, InstanceOrigin};
use crate::report_recovery::{
    AcceptedReport, Claim, ClaimMode, ClaimPhase, ClaimState, Destination, OutcomeCause,
    OutcomeParty, OutcomePhase, OutcomeStorage, OutcomeUnit, PublishKey, ReportProvenance,
    ReportState, SettledTask, TaskLookup, TaskState, Tombstone, UnitEntry, UnitMark,
};
use crate::retained_store::{Reservation, StoreRefusal};
use crate::work_done_text::{self as text, Pending, code};

/// What a report's admission found.
#[derive(Debug)]
pub(crate) enum Admission {
    /// A claim is installed; the save task owns it.
    Claimed(Claimed),
    /// The answer, final: nothing was claimed or consumed.
    Answer(WorkDoneTaskReply),
    /// A claim for the same task or unit is being saved: wait for it, holding
    /// nothing, then admit again.
    Wait(tokio::sync::watch::Receiver<ClaimState>),
}

/// An installed claim, as the save task takes it over.
#[derive(Debug)]
pub(crate) struct Claimed {
    pub claim_id: u64,
    pub message_id: String,
    pub reservation: Reservation,
    pub recipient: Option<Recipient>,
    pub role: Option<String>,
    pub mode: ClaimMode,
    /// The dispatched unit's name, for a terminal claim.
    pub unit_name: Option<String>,
    /// The orchestration instance the report is attributed to: a task's own
    /// binding, never the pane's current occupant. Admission sets it for a
    /// task; the transaction sets it from the attested facts otherwise.
    pub instance: Option<String>,
}

/// What every admission needs about the report.
#[derive(Debug, Clone)]
pub(crate) struct ClaimSpec {
    pub worker_pane: String,
    pub worker_agent: String,
    pub digest: [u8; 32],
    pub report_id: String,
    /// The exact size of the report's retained file.
    pub file_bytes: u64,
}

/// How a commit ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    /// The report was published: `destination`, under `report_id`.
    Committed {
        destination: Destination,
        report_id: String,
        task_id: Option<String>,
        /// The task's worker pane and debt, for the registry to release
        /// exactly under R1.
        debt: Option<(String, DelegateLedger)>,
    },
    /// No such claim: it was already settled.
    Gone,
    /// A lock or the report state was unavailable: nothing was written.
    Unavailable,
}

/// How a failure ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FailOutcome {
    /// The claim was failed. `debt` is a closed task's worker pane and debt,
    /// for exact release.
    Failed {
        debt: Option<(String, DelegateLedger)>,
    },
    /// The claim is staged or later: a staged report is never failed.
    Staged,
    Gone,
    Unavailable,
}

fn refused(code: &str, message: String) -> WorkDoneTaskReply {
    WorkDoneTaskReply::refused(code, &message)
}

fn answered(
    disposition: WorkDoneDisposition,
    task_id: Option<String>,
    report_id: Option<String>,
    destination: Destination,
    differs: bool,
) -> WorkDoneTaskReply {
    WorkDoneTaskReply {
        disposition,
        task_id,
        report_id,
        destination: Some(destination),
        differs,
        code: None,
        message: None,
    }
}

/// The refusal a retained store's refusal reads as.
fn store_refusal(refusal: StoreRefusal, dir: &str) -> WorkDoneTaskReply {
    match refusal {
        StoreRefusal::Bytes { .. } => refused(code::REPORT_STORE_FULL, text::store_full(dir)),
        StoreRefusal::Files { max, .. } => {
            refused(code::REPORT_STORE_FULL, text::store_files_full(max, dir))
        }
        StoreRefusal::Unusable { reason, .. } => refused(
            code::REPORT_SAVE_FAILED,
            text::save_failed(&format!("{dir}: {reason}")),
        ),
        StoreRefusal::Unavailable | StoreRefusal::Slots { .. } => {
            refused(code::DECK_UNAVAILABLE, text::deck_unavailable(dir))
        }
    }
}

/// Reserve the claim's room in one step: one counted reply room, then the
/// store's bytes and file slot. A refusal of either consumes nothing.
fn reserve_room(
    st: &mut HubState,
    store: &crate::retained_store::RetainedStore,
    bytes: u64,
) -> Result<Reservation, WorkDoneTaskReply> {
    if let Err(error) = st.inbox.reserve_reply() {
        return Err(refused(
            code::DECK_FULL,
            text::deck_full(&error.to_string()),
        ));
    }
    match store.reserve(bytes) {
        Ok(reservation) => Ok(reservation),
        Err(refusal) => {
            st.inbox.release_reply();
            Err(store_refusal(refusal, &store.dir().display().to_string()))
        }
    }
}

/// Install a claim and answer it.
#[allow(clippy::too_many_arguments)]
fn install(
    reports: &mut ReportState,
    spec: &ClaimSpec,
    mode: ClaimMode,
    recipient: Option<Recipient>,
    role: Option<String>,
    debt: (Option<u64>, Option<u64>),
    unit_name: Option<String>,
    message_id: &str,
    reservation: Reservation,
) -> Claimed {
    let claim_id = reports.claims.mint();
    let (settled, _) = tokio::sync::watch::channel(ClaimState::Pending);
    reports.claims.insert(Claim {
        claim_id,
        mode: mode.clone(),
        worker_pane: spec.worker_pane.clone(),
        worker_agent: spec.worker_agent.clone(),
        recipient: recipient.clone(),
        role: role.clone(),
        arm_id: debt.0,
        seq: debt.1,
        digest: spec.digest,
        report_id: spec.report_id.clone(),
        message_id: message_id.to_string(),
        room: true,
        recipient_gone: false,
        reserved_bytes: spec.file_bytes,
        worker_gone: None,
        unit_gone: None,
        withheld: Vec::new(),
        phase: ClaimPhase::Admitted,
        prepared: None,
        unit_mark: None,
        unit_name: unit_name.clone(),
        settled,
    });
    Claimed {
        claim_id,
        message_id: message_id.to_string(),
        reservation,
        recipient,
        role,
        mode,
        unit_name,
        instance: None,
    }
}

/// The watch of claim `claim_id`, for a duplicate to wait on.
fn wait_on(reports: &ReportState, claim_id: u64) -> Option<Admission> {
    reports
        .claims
        .get(claim_id)
        .map(|claim| Admission::Wait(claim.settled.subscribe()))
}

impl InboxHub {
    /// The retained store and its directory, or the refusal a report gets
    /// without one.
    fn report_store(
        st: &HubState,
    ) -> Result<Arc<crate::retained_store::RetainedStore>, WorkDoneTaskReply> {
        st.retained.clone().ok_or_else(|| {
            refused(
                code::REPORT_SAVE_FAILED,
                text::save_failed("the deck has no saved-results store"),
            )
        })
    }

    /// Admit a report of delegated task `task_id` (plan §2 step 3): answer a
    /// repeat from the task's tombstone, refuse what is not this generation's
    /// open task, wait on a claim already saving it, or install the claim.
    pub(crate) fn admit_task_claim(
        &self,
        spec: &ClaimSpec,
        task_id: &str,
        seat_instance: Option<&str>,
    ) -> Admission {
        let mut st = self.lock();
        let store = match Self::report_store(&st) {
            Ok(store) => store,
            Err(reply) => return Admission::Answer(reply),
        };
        let dir = store.dir().display().to_string();
        // The store (R5) is checked before anything is read or reserved
        // (plan §2 step 1): unavailable, nothing is admitted.
        if store.lock().is_err() {
            return Admission::Answer(refused(
                code::DECK_UNAVAILABLE,
                text::deck_unavailable(&dir),
            ));
        }
        let message_id = MessageId::generate(st.inbox.deck()).to_string();
        let remembered = crate::report_recovery::tombstones_max_from_env();
        let admitted = st.guarded_section(|st, reports| {
            let mine =
                |pane: &str, agent: &str| pane == spec.worker_pane && agent == spec.worker_agent;
            match reports.tasks.lookup(task_id) {
                TaskLookup::Unknown => Admission::Answer(refused(
                    code::UNKNOWN_TASK,
                    text::unknown_task(task_id, remembered),
                )),
                TaskLookup::Settled(SettledTask {
                    worker_pane,
                    worker_agent,
                    ..
                }) if !mine(worker_pane, worker_agent) => Admission::Answer(refused(
                    code::UNKNOWN_TASK,
                    text::unknown_task(task_id, remembered),
                )),
                TaskLookup::Settled(settled) => Admission::Answer(match &settled.tombstone {
                    Tombstone::Reported {
                        report_id,
                        digest,
                        destination,
                    } => answered(
                        WorkDoneDisposition::AlreadyAccepted,
                        Some(task_id.to_string()),
                        report_id.clone(),
                        *destination,
                        digest.is_some_and(|d| d != spec.digest),
                    ),
                    Tombstone::Superseded { by } => refused(
                        code::TASK_SUPERSEDED,
                        text::task_superseded(task_id, by.as_deref()),
                    ),
                    Tombstone::Closed => refused(code::TASK_CLOSED, text::task_closed(task_id)),
                }),
                TaskLookup::Open(task) => {
                    let binding = &task.binding;
                    if !mine(&binding.worker_pane, &binding.worker_agent)
                        || (binding.instance.is_some()
                            && seat_instance.is_some()
                            && binding.instance.as_deref() != seat_instance)
                    {
                        return Admission::Answer(refused(
                            code::UNKNOWN_TASK,
                            text::unknown_task(task_id, remembered),
                        ));
                    }
                    if let TaskState::Reporting { claim_id } = task.state {
                        return wait_on(reports, claim_id).unwrap_or_else(|| {
                            Admission::Answer(refused(
                                code::REPORT_PENDING,
                                text::report_pending(Pending::Task),
                            ))
                        });
                    }
                    if reports
                        .claims
                        .of_generation(&spec.worker_pane, &spec.worker_agent)
                        .is_some()
                    {
                        return Admission::Answer(refused(
                            code::REPORT_PENDING,
                            text::report_pending(Pending::Generation),
                        ));
                    }
                    let recipient = binding.orchestrator.clone();
                    let role = Some(binding.role.clone());
                    let instance = binding.instance.clone();
                    let debt = (binding.arm_id, binding.seq);
                    let reservation = match reserve_room(st, &store, spec.file_bytes) {
                        Ok(reservation) => reservation,
                        Err(reply) => return Admission::Answer(reply),
                    };
                    let mut claimed = install(
                        reports,
                        spec,
                        ClaimMode::Task {
                            task_id: task_id.to_string(),
                        },
                        recipient,
                        role,
                        debt,
                        None,
                        &message_id,
                        reservation,
                    );
                    claimed.instance = instance;
                    if let Some(task) = reports.tasks.open_task_mut(task_id) {
                        task.state = TaskState::Reporting {
                            claim_id: claimed.claim_id,
                        };
                    }
                    Admission::Claimed(claimed)
                }
            }
        });
        admitted.unwrap_or_else(|_| {
            st.log_torn_once("work-done admission");
            Admission::Answer(refused(
                code::DECK_UNAVAILABLE,
                text::deck_unavailable(&dir),
            ))
        })
    }

    /// Admit a note that completes no task (plan §2 step 3), to `recipient`,
    /// the orchestrator captured at admission (`None` keeps it in recovery).
    pub(crate) fn admit_uncorrelated_claim(
        &self,
        spec: &ClaimSpec,
        recipient: Option<Recipient>,
        role: Option<String>,
    ) -> Admission {
        let mut st = self.lock();
        let store = match Self::report_store(&st) {
            Ok(store) => store,
            Err(reply) => return Admission::Answer(reply),
        };
        let dir = store.dir().display().to_string();
        // The store (R5) is checked before anything is read or reserved
        // (plan §2 step 1): unavailable, nothing is admitted.
        if store.lock().is_err() {
            return Admission::Answer(refused(
                code::DECK_UNAVAILABLE,
                text::deck_unavailable(&dir),
            ));
        }
        let message_id = MessageId::generate(st.inbox.deck()).to_string();
        let admitted = st.guarded_section(|st, reports| {
            if reports
                .claims
                .of_generation(&spec.worker_pane, &spec.worker_agent)
                .is_some()
            {
                return Admission::Answer(refused(
                    code::REPORT_PENDING,
                    text::report_pending(Pending::Generation),
                ));
            }
            let reservation = match reserve_room(st, &store, spec.file_bytes) {
                Ok(reservation) => reservation,
                Err(reply) => return Admission::Answer(reply),
            };
            Admission::Claimed(install(
                reports,
                spec,
                ClaimMode::Uncorrelated,
                recipient,
                role,
                (None, None),
                None,
                &message_id,
                reservation,
            ))
        });
        admitted.unwrap_or_else(|_| {
            st.log_torn_once("work-done admission");
            Admission::Answer(refused(
                code::DECK_UNAVAILABLE,
                text::deck_unavailable(&dir),
            ))
        })
    }

    /// Admit a terminal report (plan §2 step 2), at the linearization point:
    /// the registry holds the routes (R2) and hands them in. Every branch
    /// that accepts reads a positive fact; every other answers pending or
    /// refuses.
    pub(crate) fn install_terminal_claim(
        &self,
        returns: &mut ReturnsGuard<'_>,
        spec: &ClaimSpec,
        facts: &AttestFacts,
    ) -> Admission {
        let mut st = self.lock();
        let store = match Self::report_store(&st) {
            Ok(store) => store,
            Err(reply) => return Admission::Answer(reply),
        };
        let dir = store.dir().display().to_string();
        // The store (R5) is checked before anything is read or reserved
        // (plan §2 step 1): unavailable, nothing is admitted.
        if store.lock().is_err() {
            return Admission::Answer(refused(
                code::DECK_UNAVAILABLE,
                text::deck_unavailable(&dir),
            ));
        }
        let message_id = MessageId::generate(st.inbox.deck()).to_string();
        let (pane, agent) = (spec.worker_pane.as_str(), spec.worker_agent.as_str());
        let admitted = st.guarded_section(|st, reports| {
            // 1. Indexed to a unit that has an entry: its final answer.
            let unit = reports.units.unit_of(pane, agent).map(str::to_string);
            if let Some(unit) = unit.as_deref() {
                match reports.units.get(unit) {
                    Some(UnitEntry::Completed {
                        report_id,
                        digest,
                        destination,
                        ..
                    }) => {
                        return Admission::Answer(answered(
                            WorkDoneDisposition::AlreadyAccepted,
                            None,
                            report_id.clone(),
                            *destination,
                            *digest != spec.digest,
                        ));
                    }
                    Some(UnitEntry::Ended { .. }) => {
                        return Admission::Answer(refused(code::TASK_CLOSED, text::unit_ended()));
                    }
                    Some(UnitEntry::Claimed { claim_id }) => {
                        return wait_on(reports, *claim_id).unwrap_or_else(|| {
                            Admission::Answer(refused(
                                code::REPORT_PENDING,
                                text::report_pending(Pending::Unit),
                            ))
                        });
                    }
                    None => {}
                }
            }
            // 2. One claim per generation.
            if reports.claims.of_generation(pane, agent).is_some() {
                return Admission::Answer(refused(
                    code::REPORT_PENDING,
                    text::report_pending(Pending::Generation),
                ));
            }
            // 3. A publication covers the reporter.
            let seat = facts.seat.as_ref();
            if let Some(publication) = reports
                .publications
                .get(&PublishKey::Pane(pane.to_string()))
            {
                let why = match publication {
                    crate::report_recovery::Publication::Initial => Pending::Registration,
                    crate::report_recovery::Publication::Replacement { .. } => Pending::Replacement,
                };
                return Admission::Answer(refused(code::REPORT_PENDING, text::report_pending(why)));
            }
            if let Some(seat) = seat
                && seat.is_orchestrator
                && reports
                    .publications
                    .contains_key(&PublishKey::Instance(seat.instance.clone()))
            {
                return Admission::Answer(refused(
                    code::REPORT_PENDING,
                    text::report_pending(Pending::Registration),
                ));
            }
            // 4. Indexed with no entry: the route decides.
            if let Some(unit) = unit {
                let route = returns.resolve(pane).cloned();
                return match route {
                    Some(route) if route.unit_id == unit && route.unit_agent_id == agent => {
                        let reservation = match reserve_room(st, &store, spec.file_bytes) {
                            Ok(reservation) => reservation,
                            Err(reply) => return Admission::Answer(reply),
                        };
                        let caller = Some(Recipient::bound(
                            &route.caller.pane_id,
                            &route.caller.agent_id,
                        ));
                        let orphaned = route.orphaned;
                        let claimed = install(
                            reports,
                            spec,
                            ClaimMode::Terminal {
                                unit_id: unit.clone(),
                                unit_pane: pane.to_string(),
                            },
                            caller,
                            None,
                            (None, None),
                            Some(route.caller.unit_name.clone()),
                            &message_id,
                            reservation,
                        );
                        reports.units.set(
                            &unit,
                            UnitEntry::Claimed {
                                claim_id: claimed.claim_id,
                            },
                        );
                        if orphaned && let Some(claim) = reports.claims.get_mut(claimed.claim_id) {
                            claim.recipient_gone = true;
                        }
                        Admission::Claimed(claimed)
                    }
                    _ if returns.route_of_unit(&unit).is_some() => {
                        Admission::Answer(refused(code::TASK_CLOSED, text::replaced_generation()))
                    }
                    _ => {
                        tracing::warn!(
                            unit_id = %unit,
                            "work-done: a unit's terminal reported with no route and no entry"
                        );
                        Admission::Answer(refused(code::UNKNOWN_TASK, text::forgotten_unit()))
                    }
                };
            }
            // 5. No index entry: the request's own pinned facts decide.
            if facts.unit.is_some() {
                return Admission::Answer(refused(code::UNKNOWN_TASK, text::forgotten_unit()));
            }
            match seat {
                Some(seat) if seat.is_orchestrator => match seat.origin {
                    Some(InstanceOrigin::Direct) => Admission::Answer(answered(
                        WorkDoneDisposition::Accepted,
                        None,
                        None,
                        Destination::None,
                        false,
                    )),
                    Some(InstanceOrigin::Dispatched) => {
                        Admission::Answer(refused(code::UNKNOWN_TASK, text::forgotten_unit()))
                    }
                    Some(InstanceOrigin::Conflict) => {
                        tracing::error!(
                            instance = %crate::config_validation::escape_id_for_log(&seat.instance),
                            "work-done: an orchestrator whose instance's generations disagree on \
                             how it was started reported its orchestration complete"
                        );
                        Admission::Answer(refused(
                            code::INCOMPATIBLE_MODE,
                            text::conflicting_origin(),
                        ))
                    }
                    None => {
                        tracing::error!(
                            instance = %crate::config_validation::escape_id_for_log(&seat.instance),
                            "work-done: an orchestration seat with no recorded origin; answering \
                             pending"
                        );
                        Admission::Answer(refused(
                            code::REPORT_PENDING,
                            text::report_pending(Pending::Registration),
                        ))
                    }
                },
                _ => Admission::Answer(refused(code::INCOMPATIBLE_MODE, text::not_a_terminal())),
            }
        });
        admitted.unwrap_or_else(|_| {
            st.log_torn_once("work-done admission");
            Admission::Answer(refused(
                code::DECK_UNAVAILABLE,
                text::deck_unavailable(&dir),
            ))
        })
    }

    /// Advance claim `claim_id` to `phase`, before the step it names.
    pub(crate) fn note_claim_phase(&self, claim_id: u64, phase: ClaimPhase) -> Result<(), Torn> {
        let mut st = self.lock();
        st.guarded_section(|_, reports| {
            if let Some(claim) = reports.claims.get_mut(claim_id) {
                claim.phase = phase;
            }
        })
    }

    /// Stage claim `claim_id`'s composed post (D12 rule 8): from here on it is
    /// only rolled forward.
    pub(crate) fn stage_claim(&self, claim_id: u64, post: Content) -> Result<(), Torn> {
        let mut st = self.lock();
        st.guarded_section(|_, reports| {
            if let Some(claim) = reports.claims.get_mut(claim_id) {
                claim.prepared = Some(post);
                claim.phase = ClaimPhase::Staged;
            }
        })
    }

    /// Record a terminal claim's unit mark (D12 rule 8 step 2).
    pub(crate) fn note_claim_marked(&self, claim_id: u64, mark: UnitMark) -> Result<(), Torn> {
        let mut st = self.lock();
        st.guarded_section(|_, reports| {
            if let Some(claim) = reports.claims.get_mut(claim_id) {
                claim.unit_mark = Some(mark);
                claim.phase = ClaimPhase::Marked;
            }
        })
    }

    /// Claim `claim_id`'s phase, for the supervisor (`Ok(None)`: no claim).
    pub(crate) fn claim_phase(&self, claim_id: u64) -> Result<Option<ClaimPhase>, Torn> {
        self.lock()
            .guarded_read(|reports| reports.claims.get(claim_id).map(|claim| claim.phase))
    }

    /// The committed answer for a claim that is gone, read by its keys, for
    /// the supervisor: a task's `Reported` tombstone, a unit's `Completed`
    /// entry, or, for a note, its pre-minted message or its row.
    pub(crate) fn committed_answer(
        &self,
        mode: &ClaimMode,
        report_id: &str,
        message_id: &str,
        recipient_pane: Option<&str>,
    ) -> Result<Option<WorkDoneTaskReply>, Torn> {
        let st = self.lock();
        let in_inbox = recipient_pane.is_some_and(|pane| st.inbox.get(message_id, pane).is_some());
        st.guarded_read(|reports| match mode {
            ClaimMode::Task { task_id } => match reports.tasks.lookup(task_id) {
                TaskLookup::Settled(SettledTask {
                    tombstone:
                        Tombstone::Reported {
                            report_id,
                            destination,
                            ..
                        },
                    ..
                }) => Some(answered(
                    WorkDoneDisposition::Accepted,
                    Some(task_id.clone()),
                    report_id.clone(),
                    *destination,
                    false,
                )),
                _ => None,
            },
            ClaimMode::Terminal { unit_id, .. } => match reports.units.get(unit_id) {
                Some(UnitEntry::Completed {
                    report_id,
                    destination,
                    ..
                }) => Some(answered(
                    WorkDoneDisposition::Accepted,
                    None,
                    report_id.clone(),
                    *destination,
                    false,
                )),
                _ => None,
            },
            ClaimMode::Uncorrelated => {
                if in_inbox || reports.accepted.contains_key(message_id) {
                    Some(answered(
                        WorkDoneDisposition::Uncorrelated,
                        None,
                        Some(report_id.to_string()),
                        Destination::Inbox,
                        false,
                    ))
                } else if reports.outcomes.has_report(report_id) {
                    Some(answered(
                        WorkDoneDisposition::Uncorrelated,
                        None,
                        Some(report_id.to_string()),
                        Destination::Recovery,
                        false,
                    ))
                } else {
                    None
                }
            }
        })
    }

    /// The commit (D12 rule 8), once per claim, with the registry holding the
    /// tracker (R1) and, for a terminal claim, the routes (R2) and handing
    /// the routes in. It takes the hub, checks the retained store before its
    /// first write and holds it to the end, and publishes the report: to the
    /// recipient's inbox if its generation is live and the post is taken,
    /// otherwise as a recovery row. Then the task is reported, or the unit
    /// completed, the route taken, the claim removed and its waiters told.
    pub(crate) fn commit_claim_locked(
        &self,
        returns: Option<&mut ReturnsGuard<'_>>,
        claim_id: u64,
        live: &dyn Fn(&str, &str) -> bool,
    ) -> (CommitOutcome, AfterSection) {
        let mut after = AfterSection::default();
        let mut st = self.lock();
        let Some(store) = st.retained.clone() else {
            return (CommitOutcome::Unavailable, after);
        };
        let outcome = st.guarded_section(|st, reports| {
            let Some(claim) = reports.claims.get(claim_id) else {
                return CommitOutcome::Gone;
            };
            debug_assert!(
                claim.phase < ClaimPhase::Applying && claim.prepared.is_some(),
                "a claim is committed once, staged"
            );
            if claim.prepared.is_none() || claim.phase >= ClaimPhase::Applying {
                return CommitOutcome::Unavailable;
            }
            // The branch: the recipient's live generation (R4, read and
            // released), checked in the section that inserts, as every post's
            // check is, and before the store is taken (R5).
            let recipient = claim.recipient.clone().filter(|to| {
                !claim.recipient_gone
                    && to
                        .agent_id
                        .as_deref()
                        .is_some_and(|agent| live(&to.pane_id, agent))
            });
            let Ok(mut held) = store.lock() else {
                return CommitOutcome::Unavailable;
            };
            #[cfg(test)]
            if let Some(hook) =
                claim_seams::take(claim_seams::APPLY_BEFORE_POST, &claim.worker_pane)
            {
                // Marked before the hook, so a panic in it reads as `Applying`.
                if let Some(claim) = reports.claims.get_mut(claim_id) {
                    claim.phase = ClaimPhase::Applying;
                }
                hook();
            }
            let Some(claim) = reports.claims.get_mut(claim_id) else {
                return CommitOutcome::Gone;
            };
            claim.phase = ClaimPhase::Applying;
            let now = Instant::now();
            let now_store = crate::retained_store::Now::current();
            let now_ms = chrono::Utc::now().timestamp_millis();
            let mut content = claim
                .prepared
                .take()
                .unwrap_or_else(|| Content::from_deck(NOTICE_REASON, "work-done", String::new()));
            let report_id = claim.report_id.clone();
            let producer = crate::report_recovery::Producer {
                pane_id: claim.worker_pane.clone(),
                agent_id: claim.worker_agent.clone(),
                role: claim.role.clone(),
            };
            let (provenance, phase, task_id, unit) = match &claim.mode {
                ClaimMode::Task { task_id } => (
                    ReportProvenance::Task {
                        task_id: task_id.clone(),
                        report_id: report_id.clone(),
                        producer: producer.clone(),
                    },
                    OutcomePhase::WorkerReport,
                    Some(task_id.clone()),
                    None,
                ),
                ClaimMode::Terminal { unit_id, .. } => (
                    ReportProvenance::Terminal {
                        unit_id: unit_id.clone(),
                        report_id: report_id.clone(),
                        producer: producer.clone(),
                    },
                    OutcomePhase::DispatchFinished,
                    None,
                    Some(OutcomeUnit {
                        unit_id: unit_id.clone(),
                        name: crate::report_recovery::wire_text(
                            claim.unit_name.as_deref().unwrap_or_default(),
                            crate::report_recovery::MAX_LABEL_CHARS,
                        ),
                    }),
                ),
                ClaimMode::Uncorrelated => (
                    ReportProvenance::Uncorrelated {
                        report_id: report_id.clone(),
                        producer: producer.clone(),
                    },
                    OutcomePhase::WorkerReport,
                    None,
                    None,
                ),
            };
            content.provenance = Some(provenance.clone());
            content.id = MessageId::parse(&claim.message_id);
            let entry = AcceptedReport {
                provenance,
                phase,
                report_id: Some(report_id.clone()),
                summary: crate::report_recovery::cut_bytes(
                    &content.body,
                    crate::report_recovery::MAX_SUMMARY_BYTES,
                ),
                storage: OutcomeStorage::Artifact,
                label: crate::report_recovery::wire_text(
                    &content.subject,
                    crate::report_recovery::MAX_LABEL_CHARS,
                ),
                original: claim
                    .recipient
                    .as_ref()
                    .map(|to| crate::report_recovery::party(to, None))
                    .unwrap_or(OutcomeParty {
                        pane_id: String::new(),
                        agent_id: String::new(),
                        role: None,
                    }),
                producer: OutcomeParty {
                    pane_id: producer.pane_id.clone(),
                    agent_id: producer.agent_id.clone(),
                    role: producer.role.clone(),
                },
                unit,
                orchestration: None,
                task_id: task_id.clone(),
                handed: false,
            };
            let room = std::mem::replace(&mut claim.room, false);
            let posted = match recipient.clone() {
                Some(to) => st.inbox.post_reserved_reply(now, to, content).ok(),
                None => None,
            };
            #[cfg(test)]
            if let Some(hook) = claim_seams::take(claim_seams::APPLY_AFTER_POST, &claim.worker_pane)
            {
                hook();
            }
            let destination = match posted {
                Some(id) => {
                    reports.accepted.insert(id.to_string(), entry);
                    if let Some(to) = recipient {
                        after.wakes.push(to);
                    }
                    Destination::Inbox
                }
                None => {
                    if room {
                        st.inbox.release_reply();
                    }
                    let cause = if claim.recipient.is_some() && recipient.is_some() {
                        OutcomeCause::RecipientRefused
                    } else {
                        OutcomeCause::RecipientExited
                    };
                    if !reports.outcomes.has_report(&report_id) {
                        reports.outcomes.insert(entry.into_row(cause, now_ms), None);
                    }
                    tracing::info!(
                        report_id = %report_id,
                        pane_id = %claim.worker_pane,
                        "work-done: the report's recipient is no longer running; it is kept as a \
                         recovery row"
                    );
                    held.recovered(&report_id, now_store);
                    Destination::Recovery
                }
            };
            let Some(claim) = reports.claims.remove(claim_id) else {
                return CommitOutcome::Gone;
            };
            let mut debt = None;
            match &claim.mode {
                ClaimMode::Task { task_id } => {
                    let _ = reports.tasks.settle(
                        task_id,
                        Tombstone::Reported {
                            report_id: Some(report_id.clone()),
                            digest: Some(claim.digest),
                            destination,
                        },
                    );
                    retire_reported_task(st, now, task_id, &claim.worker_pane);
                    debt = Some((
                        claim.worker_pane.clone(),
                        DelegateLedger {
                            commission_arm_id: claim.arm_id,
                            delegation_seq: claim.seq,
                            instance: None,
                        },
                    ));
                }
                ClaimMode::Terminal { unit_id, unit_pane } => {
                    reports.units.set(
                        unit_id,
                        UnitEntry::Completed {
                            report_id: Some(report_id.clone()),
                            digest: claim.digest,
                            destination,
                            unit_mark: claim.unit_mark.unwrap_or(UnitMark::Marked),
                        },
                    );
                    if let Some(returns) = returns {
                        let _ = returns.take_for_unit(unit_pane, unit_id);
                    }
                }
                ClaimMode::Uncorrelated => {}
            }
            drain_events_in_section(st, reports, &mut StoreAccess::Held(&mut held));
            let task_id = match &claim.mode {
                ClaimMode::Task { task_id } => Some(task_id.clone()),
                _ => None,
            };
            // Last: the claim is gone, and its waiters are told.
            let _ = claim.settled.send(ClaimState::Committed);
            CommitOutcome::Committed {
                destination,
                report_id,
                task_id,
                debt,
            }
        });
        match outcome {
            Ok(outcome) => (outcome, after),
            Err(_) => {
                st.log_torn_once("work-done commit");
                (CommitOutcome::Unavailable, AfterSection::default())
            }
        }
    }

    /// The failure (D12 rule 9), with the registry holding the tracker (R1).
    /// Refused for a staged claim. Otherwise the room comes back, and the
    /// task opens again, or closes if its worker is gone (posting what the
    /// exit withheld); a terminal claim's unit ends if it is gone, and is
    /// forgotten otherwise, so a retry is admitted fresh.
    pub(crate) fn fail_claim_locked(&self, claim_id: u64) -> (FailOutcome, AfterSection) {
        let mut after = AfterSection::default();
        let mut st = self.lock();
        let Some(store) = st.retained.clone() else {
            return (FailOutcome::Unavailable, after);
        };
        let outcome = st.guarded_section(|st, reports| {
            let Some(claim) = reports.claims.get(claim_id) else {
                return FailOutcome::Gone;
            };
            // A staged report is only ever rolled forward (D12 rules 9, 11):
            // refused here, so it is a property of this function and not
            // only of its callers.
            if claim.phase >= ClaimPhase::Staged {
                return FailOutcome::Staged;
            }
            // Checked before the first write; the failure writes nothing in
            // the store, so it is released at once (the notices below may read
            // R4, which ranks below it).
            if store.lock().is_err() {
                return FailOutcome::Unavailable;
            }
            let Some(mut claim) = reports.claims.remove(claim_id) else {
                return FailOutcome::Gone;
            };
            if std::mem::replace(&mut claim.room, false) {
                st.inbox.release_reply();
            }
            let mut debt = None;
            match &claim.mode {
                ClaimMode::Task { task_id } => match claim.worker_gone {
                    Some(_) => {
                        if let Some(task) = reports.tasks.settle(task_id, Tombstone::Closed) {
                            debt = Some((claim.worker_pane.clone(), task_debt(&task)));
                        }
                        st.owed_notices.append(&mut claim.withheld);
                    }
                    None => {
                        if let Some(task) = reports.tasks.open_task_mut(task_id) {
                            let revision = match task.state {
                                TaskState::Open { revision } => revision,
                                TaskState::Reporting { .. } => 0,
                            };
                            task.state = TaskState::Open {
                                revision: revision + 1,
                            };
                        }
                    }
                },
                ClaimMode::Terminal { unit_id, unit_pane } => match claim.unit_gone {
                    Some(how) => {
                        reports.units.set(unit_id, UnitEntry::Ended { how });
                        if let Some(caller) = claim.recipient.as_ref() {
                            dispatch_ended_for(
                                st,
                                reports,
                                unit_id,
                                claim.unit_name.as_deref().unwrap_or_default(),
                                caller,
                                unit_pane,
                                &claim.worker_agent,
                                how,
                                &mut after,
                            );
                        }
                    }
                    None => {
                        reports.units.remove(unit_id);
                    }
                },
                ClaimMode::Uncorrelated => {}
            }
            let _ = claim.settled.send(ClaimState::Failed);
            FailOutcome::Failed { debt }
        });
        match outcome {
            Ok(outcome) => (outcome, after),
            Err(_) => {
                st.log_torn_once("work-done failure");
                (FailOutcome::Unavailable, AfterSection::default())
            }
        }
    }

    /// Test probe: claim `claim_id`, read.
    #[cfg(test)]
    pub(crate) fn claim_for_test<R>(
        &self,
        claim_id: u64,
        f: impl FnOnce(&Claim) -> R,
    ) -> Option<R> {
        self.lock()
            .guarded_read(|reports| reports.claims.get(claim_id).map(f))
            .ok()
            .flatten()
    }
}

/// Test seams inside the commit's section, process-wide.
#[cfg(test)]
pub(crate) mod claim_seams {
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Hook = Box<dyn FnOnce() + Send>;

    /// After `phase = Applying` and before the inbox insert (`/054`).
    pub(crate) const APPLY_BEFORE_POST: &str = "apply-before-post";
    /// After the inbox insert and before the tombstone or unit entry is
    /// written (`/053` (d)).
    pub(crate) const APPLY_AFTER_POST: &str = "apply-after-post";

    type Key = (&'static str, String);

    static HOOKS: Mutex<Option<HashMap<Key, Hook>>> = Mutex::new(None);

    /// Run `hook` at seam `name` of the next commit of a report from
    /// `worker_pane`; keyed by pane, so concurrent tests do not meet.
    pub(crate) fn on(name: &'static str, worker_pane: &str, hook: impl FnOnce() + Send + 'static) {
        HOOKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert((name, worker_pane.to_string()), Box::new(hook));
    }

    pub(crate) fn take(name: &'static str, worker_pane: &str) -> Option<Hook> {
        HOOKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
            .and_then(|hooks| hooks.remove(&(name, worker_pane.to_string())))
    }
}
