//! PRD #1589: the daemon side of `dot-agent-deck close` —
//! [`crate::daemon_protocol::AttachRequest::CloseAgents`].
//!
//! One request runs in this order, and the order is the design:
//!
//! 1. **Caller.** No claim is a person. A claim must be attested by the same
//!    [`crate::hook_provenance::classify`] the hook socket uses, AND the
//!    attested generation must still hold the pane it claims — attestation
//!    identifies a spawn, it does not say that spawn still speaks for the pane
//!    (auditor B1). Anything else refuses the whole request; nothing falls back
//!    to person authority.
//! 2. **Resolve** each selector entry to a target, against live records only,
//!    and deduplicate.
//! 3. **Authorize** each target: an agent may close only units its own
//!    generation dispatched, never its own pane or orchestration.
//! 4. A dry run reports here and stops nothing.
//! 5. Otherwise, per target: open the instance-scoped Closing admission state,
//!    take a cleanup hold on every member generation, re-check the caller, the
//!    membership and the forceable refusals, and only then disclose and stop —
//!    the orchestrator first. A failed preflight releases everything and stops
//!    nothing; termination itself is reported per pane and never promised
//!    all-or-none.
//! 6. Await the worktree cleanup's typed verdict, bounded.
//!
//! **What this guards, honestly.** It keeps a cooperative agent from closing
//! the wrong thing — a sibling's unit, a user's own pane, a stale or recycled
//! pane id, a survivor of a restart. It is not a boundary against a determined
//! same-uid process, which can drop its identity and ask as a person, send a
//! raw `StopAgent`, or read another agent's token (`docs/develop/hook-provenance.md`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use crate::agent_pty::{AgentPtyRegistry, AgentRecord, TabMembership};
use crate::daemon_protocol::{
    AmbiguousCandidate, CallerClaim, CloseDispatcher, CloseOutcome, ClosePane, CloseRefusal,
    CloseRefusalReason, CloseReport, CloseSelector, CloseTarget, OpenDescendant, WorktreeVerdict,
    WorktreeVerdictKind,
};
use crate::dispatched_units::{
    Caller, DispatchedUnit, EndReason, TargetFacts, UnitKind, authorize, default_refusals,
};
use crate::event::BroadcastMsg;
use crate::issue_dispatch_run::WorktreeRegistry;
use crate::state::SharedState;

/// How long a close waits for its worktree cleanup before answering
/// [`WorktreeVerdictKind::TimedOut`]. The cleanup is two `git` invocations
/// against a tree an agent worked in; seconds on a real checkout, so the bound
/// is generous. On timeout the cleanup carries on and still broadcasts its
/// verdict to attached clients.
pub const WORKTREE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times membership is re-enumerated while taking holds before a
/// close gives up. New members are refused once the instance is closing, so a
/// set that keeps changing is a spawn that reserved before the close and is
/// still publishing; each round takes a hold on what appeared.
const MEMBERSHIP_ROUNDS: usize = 4;

/// The person-or-agent label for logs and reports.
fn caller_label(caller: &Caller) -> String {
    match caller {
        Caller::Person => "person".to_string(),
        Caller::Agent {
            pane_id, agent_id, ..
        } => format!(
            "agent {agent_id} in pane {}",
            crate::config_validation::escape_id_for_log(pane_id)
        ),
    }
}

/// The orchestration instance a record's membership names.
fn instance_of(record: &AgentRecord) -> Option<&str> {
    match &record.tab_membership {
        Some(TabMembership::Orchestration {
            orchestration_id: Some(id),
            ..
        }) => Some(id),
        _ => None,
    }
}

fn role_of(record: &AgentRecord) -> (Option<String>, bool) {
    match &record.tab_membership {
        Some(TabMembership::Orchestration {
            role_name,
            is_start_role,
            ..
        }) => (Some(role_name.clone()), *is_start_role),
        _ => (None, false),
    }
}

/// Step 1: establish who is asking, or refuse the whole request.
pub fn establish_caller(
    claim: Option<&CallerClaim>,
    registry: &AgentPtyRegistry,
) -> Result<Caller, CloseRefusal> {
    let Some(claim) = claim else {
        return Ok(Caller::Person);
    };
    match crate::hook_provenance::classify(&claim.pane_id, Some(&claim.token), registry) {
        crate::hook_provenance::Provenance::Attested { agent_id } => {
            if registry.pane_current_agent_id(&claim.pane_id).as_deref() != Some(&agent_id) {
                return Err(CloseRefusal {
                    reason: CloseRefusalReason::Superseded,
                    message: "refused: the agent this pane's capability token was issued to no \
                              longer holds the pane, so it closes nothing. A person can still \
                              close the unit."
                        .to_string(),
                });
            }
            let orchestration_id = registry
                .agent_record_any(&agent_id)
                .and_then(|r| instance_of(&r).map(str::to_string));
            Ok(Caller::Agent {
                pane_id: claim.pane_id.clone(),
                agent_id,
                orchestration_id,
            })
        }
        crate::hook_provenance::Provenance::Unattested => Err(CloseRefusal {
            reason: CloseRefusalReason::NotAttested,
            message: "refused: this daemon never issued a capability token for the pane this \
                      request claims, so it cannot act for an agent there."
                .to_string(),
        }),
        crate::hook_provenance::Provenance::Refused(refusal) => Err(CloseRefusal {
            reason: CloseRefusalReason::NotAttested,
            message: refusal.caller_message(),
        }),
    }
}

/// Whether an agent caller established at step 1 still holds its pane. A
/// person always does.
fn caller_still_current(caller: &Caller, registry: &AgentPtyRegistry) -> bool {
    match caller {
        Caller::Person => true,
        Caller::Agent {
            pane_id, agent_id, ..
        } => registry.pane_current_agent_id(pane_id).as_deref() == Some(agent_id),
    }
}

/// Which generations a target covers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// Every record of an orchestration instance.
    Instance(String),
    /// One generation.
    Generation(String),
}

/// One resolved target.
#[derive(Debug, Clone)]
struct Resolved {
    selector: String,
    /// The unit, snapshotted at resolution. Re-read before acting.
    unit: Option<DispatchedUnit>,
    scope: Scope,
    /// Reached through `--pane`: the stranding refusal applies.
    by_pane: bool,
    /// The pane `--pane` / `--orchestration-of` named, for the own-pane check
    /// of a target that is not a unit.
    named_pane: Option<String>,
}

impl Resolved {
    fn dedupe_key(&self) -> String {
        match (&self.unit, &self.scope, self.by_pane) {
            (Some(unit), _, false) => format!("unit:{}", unit.id),
            (_, Scope::Instance(id), _) => format!("instance:{id}"),
            (_, Scope::Generation(id), _) => format!("generation:{id}"),
        }
    }
}

/// A member generation of a target, as enumerated from the registry.
#[derive(Debug, Clone)]
struct Member {
    record: AgentRecord,
    exited: bool,
}

/// Step 5's membership: generation-bound registry records, including exited
/// ones, orchestrator first.
fn enumerate(scope: &Scope, registry: &AgentPtyRegistry) -> Vec<Member> {
    let mut members: Vec<Member> = registry
        .agent_records_including_exited()
        .into_iter()
        .filter(|(record, _)| match scope {
            Scope::Instance(id) => instance_of(record) == Some(id.as_str()),
            Scope::Generation(id) => &record.id == id,
        })
        .map(|(record, exited)| Member { record, exited })
        .collect();
    // Orchestrator first, then spawn order: an orchestrator stopped first
    // cannot `pane spawn` while its workers go down.
    members.sort_by_key(|m| {
        let (_, orchestrator) = role_of(&m.record);
        (
            !orchestrator,
            m.record.id.parse::<u64>().unwrap_or(u64::MAX),
        )
    });
    members
}

/// The unit a unit-less target belongs to, if any — so a dispatched unit
/// reached through `--pane` or `--orchestration-of` meets the same refusals as
/// by name (auditor B4).
fn unit_of_record(record: &AgentRecord, registry: &AgentPtyRegistry) -> Option<DispatchedUnit> {
    registry
        .dispatched_units()
        .unit_of_generation(
            record.pane_id_env.as_deref(),
            &record.id,
            instance_of(record),
        )
        .cloned()
}

/// The generation currently on `pane_id`: a live occupant, else a retired one
/// whose pane has not changed hands.
fn generation_on_pane(pane_id: &str, registry: &AgentPtyRegistry) -> Option<AgentRecord> {
    let id = registry
        .pane_current_agent_id(pane_id)
        .or_else(|| registry.agent_id_for_pane_any(pane_id))?;
    registry.agent_record_any(&id)
}

/// Step 2: resolve the selector into targets and refusals.
fn resolve(
    selector: &CloseSelector,
    caller: &Caller,
    registry: &AgentPtyRegistry,
) -> (Vec<Resolved>, Vec<CloseTarget>) {
    let mut resolved = Vec::new();
    let mut refused = Vec::new();
    match selector {
        CloseSelector::Units(units) => {
            let mut seen_slugs = Vec::new();
            for name in &units.names {
                let slug = crate::dispatched_units::slug_of(name);
                if seen_slugs.contains(&slug) {
                    continue;
                }
                seen_slugs.push(slug);
                // Resolve under the record's lock, then let it go before the
                // candidates' panes are read from the registry.
                let resolution = {
                    let records = registry.dispatched_units();
                    match records.resolve_name(name, caller) {
                        Ok(unit) => Ok(unit.clone()),
                        Err(r) => {
                            let candidates: Vec<DispatchedUnit> = r
                                .candidates
                                .iter()
                                .filter_map(|id| records.get(id).cloned())
                                .collect();
                            Err((r, candidates))
                        }
                    }
                };
                match resolution {
                    Ok(unit) => resolved.push(Resolved {
                        selector: name.clone(),
                        scope: scope_of_unit(&unit),
                        unit: Some(unit),
                        by_pane: false,
                        named_pane: None,
                    }),
                    Err((r, candidates)) => {
                        let mut target = CloseTarget::refused(name, r.reason, r.message);
                        target.candidates = candidates
                            .iter()
                            .map(|u| AmbiguousCandidate {
                                unit_id: u.id.clone(),
                                name: u.name.clone(),
                                worktree: u.worktree.to_string_lossy().into_owned(),
                                panes: unit_panes(u, registry),
                            })
                            .collect();
                        refused.push(target);
                    }
                }
            }
        }
        CloseSelector::UnitIds { ids } => {
            let mut seen = Vec::new();
            for id in ids {
                if seen.contains(id) {
                    continue;
                }
                seen.push(id.clone());
                match registry.dispatched_units().resolve_id(id) {
                    Ok(unit) => resolved.push(Resolved {
                        selector: id.clone(),
                        scope: scope_of_unit(unit),
                        unit: Some(unit.clone()),
                        by_pane: false,
                        named_pane: None,
                    }),
                    Err(r) => refused.push(CloseTarget::refused(id, r.reason, r.message)),
                }
            }
        }
        CloseSelector::Pane { pane_id } => match generation_on_pane(pane_id, registry) {
            None => refused.push(CloseTarget::refused(
                pane_id,
                CloseRefusalReason::UnknownPane,
                "no agent holds that pane".to_string(),
            )),
            Some(record) => resolved.push(Resolved {
                selector: pane_id.clone(),
                unit: unit_of_record(&record, registry),
                scope: Scope::Generation(record.id.clone()),
                by_pane: true,
                named_pane: Some(pane_id.clone()),
            }),
        },
        CloseSelector::OrchestrationOf { pane_id } => match generation_on_pane(pane_id, registry) {
            None => refused.push(CloseTarget::refused(
                pane_id,
                CloseRefusalReason::UnknownPane,
                "no agent holds that pane".to_string(),
            )),
            Some(record) => match instance_of(&record) {
                None => refused.push(CloseTarget::refused(
                    pane_id,
                    CloseRefusalReason::NotAnOrchestration,
                    "that pane is not a role of an orchestration; close it with --pane".to_string(),
                )),
                Some(instance) => resolved.push(Resolved {
                    selector: pane_id.clone(),
                    unit: registry
                        .dispatched_units()
                        .unit_of_instance(instance)
                        .cloned(),
                    scope: Scope::Instance(instance.to_string()),
                    by_pane: false,
                    named_pane: Some(pane_id.clone()),
                }),
            },
        },
        CloseSelector::AllUnits => {
            let records = registry.dispatched_units();
            for unit in records.live() {
                let mine = match caller {
                    Caller::Person => true,
                    Caller::Agent { agent_id, .. } => &unit.dispatcher.agent_id == agent_id,
                };
                if mine {
                    resolved.push(Resolved {
                        selector: unit.id.clone(),
                        scope: scope_of_unit(unit),
                        unit: Some(unit.clone()),
                        by_pane: false,
                        named_pane: None,
                    });
                }
            }
        }
    }
    let mut keys = Vec::new();
    resolved.retain(|r| {
        let key = r.dedupe_key();
        if keys.contains(&key) {
            false
        } else {
            keys.push(key);
            true
        }
    });
    (resolved, refused)
}

fn scope_of_unit(unit: &DispatchedUnit) -> Scope {
    match &unit.kind {
        UnitKind::Single { agent_id, .. } => Scope::Generation(agent_id.clone()),
        UnitKind::Orchestration {
            orchestration_id, ..
        } => Scope::Instance(orchestration_id.clone()),
    }
}

fn unit_panes(unit: &DispatchedUnit, registry: &AgentPtyRegistry) -> Vec<String> {
    enumerate(&scope_of_unit(unit), registry)
        .into_iter()
        .filter_map(|m| m.record.pane_id_env)
        .collect()
}

/// Step 3: may `caller` close this target?
fn authorize_target(
    caller: &Caller,
    target: &Resolved,
    members: &[Member],
) -> Result<(), CloseRefusal> {
    let Caller::Agent {
        pane_id: caller_pane,
        orchestration_id: caller_instance,
        ..
    } = caller
    else {
        return Ok(());
    };
    // Never its own pane or its own instance, whatever the target is.
    let own = members.iter().any(|m| {
        m.record.pane_id_env.as_deref() == Some(caller_pane.as_str())
            || (caller_instance.is_some() && instance_of(&m.record) == caller_instance.as_deref())
    }) || target.named_pane.as_deref() == Some(caller_pane.as_str());
    if own {
        return Err(CloseRefusal {
            reason: CloseRefusalReason::OwnPane,
            message: "an agent cannot close its own pane or its own orchestration".to_string(),
        });
    }
    match &target.unit {
        Some(unit) => authorize(caller, unit).map_err(|r| CloseRefusal {
            reason: r.reason,
            message: r.message,
        }),
        // A pane or orchestration nobody dispatched: no agent dispatched it, so
        // no agent may close it.
        None => Err(CloseRefusal {
            reason: CloseRefusalReason::NotYourUnit,
            message: "that pane is not part of a unit this agent dispatched".to_string(),
        }),
    }
}

/// The facts the forceable refusals read, measured now.
async fn facts(
    target: &Resolved,
    unit: Option<&DispatchedUnit>,
    members: &[Member],
    state: &SharedState,
    registry: &AgentPtyRegistry,
) -> (TargetFacts, Vec<Option<crate::state::SessionStatus>>) {
    let statuses: Vec<Option<crate::state::SessionStatus>> = {
        let state = state.read().await;
        members
            .iter()
            .map(|m| {
                if m.exited {
                    return None;
                }
                m.record
                    .pane_id_env
                    .as_deref()
                    .and_then(|p| state.pane_status_of(p))
            })
            .collect()
    };
    // `--pane` on one role of an instance with other live roles.
    let strands = target.by_pane
        && members.len() == 1
        && instance_of(&members[0].record).is_some_and(|instance| {
            registry
                .agent_records()
                .iter()
                .any(|r| r.id != members[0].record.id && instance_of(r) == Some(instance))
        });
    (
        TargetFacts {
            unit_reported: unit.map(|u| u.completed_at_ms.is_some()),
            statuses: statuses.iter().flatten().cloned().collect(),
            strands_orchestration: strands,
        },
        statuses,
    )
}

fn pane_report(member: &Member, status: Option<&crate::state::SessionStatus>) -> ClosePane {
    let (role, is_orchestrator) = role_of(&member.record);
    ClosePane {
        agent_id: member.record.id.clone(),
        pane_id: member.record.pane_id_env.clone(),
        role,
        is_orchestrator,
        status: status.map(|s| format!("{s:?}")),
        exited: member.exited,
        stopped: None,
    }
}

/// The report entry for a target, before its outcome is known.
fn base_target(
    target: &Resolved,
    unit: Option<&DispatchedUnit>,
    members: &[Member],
    statuses: &[Option<crate::state::SessionStatus>],
    registry: &AgentPtyRegistry,
) -> CloseTarget {
    let mut out =
        CloseTarget::refused(&target.selector, CloseRefusalReason::Unknown, String::new());
    out.reason = None;
    out.message = None;
    out.outcome = CloseOutcome::Listed;
    out.panes = members
        .iter()
        .zip(statuses.iter().chain(std::iter::repeat(&None)))
        .map(|(m, s)| pane_report(m, s.as_ref()))
        .collect();
    match unit {
        Some(unit) => {
            out.unit_id = Some(unit.id.clone());
            out.name = Some(unit.name.clone());
            out.kind = unit.kind.label().to_string();
            out.reported = Some(unit.completed_at_ms.is_some());
            out.completed_at_ms = unit.completed_at_ms;
            out.dispatched_at_ms = Some(unit.dispatched_at_ms);
            out.worktree = Some(unit.worktree.to_string_lossy().into_owned());
            out.branch = Some(unit.branch.clone());
            out.clone = Some(unit.clone_dir.to_string_lossy().into_owned());
            out.dispatcher = Some(CloseDispatcher {
                pane_id: unit.dispatcher.pane_id.clone(),
                agent_id: unit.dispatcher.agent_id.clone(),
            });
        }
        None => {
            out.kind = match target.scope {
                Scope::Instance(_) => "orchestration".to_string(),
                Scope::Generation(_) => "pane".to_string(),
            };
        }
    }
    let agent_ids: Vec<String> = members.iter().map(|m| m.record.id.clone()).collect();
    out.open_descendants = registry
        .dispatched_units()
        .open_descendants(&agent_ids)
        .map(|u| OpenDescendant {
            unit_id: u.id.clone(),
            name: u.name.clone(),
            clone: u.clone_dir.to_string_lossy().into_owned(),
        })
        .collect();
    out
}

fn refuse(out: &mut CloseTarget, reason: CloseRefusalReason, message: String) {
    out.outcome = CloseOutcome::Refused;
    out.reason = Some(reason);
    out.message = Some(message);
}

/// Handle one `CloseAgents` request. Never fails as a request: every refusal
/// and failure is in the report.
#[allow(clippy::too_many_arguments)]
pub async fn handle_close_agents(
    selector: CloseSelector,
    caller: Option<CallerClaim>,
    force: bool,
    dry_run: bool,
    registry: &Arc<AgentPtyRegistry>,
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    worktrees: &WorktreeRegistry,
) -> CloseReport {
    let mut report = CloseReport {
        dry_run,
        forced: force,
        refused: None,
        targets: Vec::new(),
    };
    if selector == CloseSelector::AllUnits && !dry_run {
        report.refused = Some(CloseRefusal {
            reason: CloseRefusalReason::BulkRequiresDryRun,
            message: "refused: closing every unit applies only as a dry run; close the units it \
                      lists by their unit ids"
                .to_string(),
        });
        return report;
    }
    let caller = match establish_caller(caller.as_ref(), registry) {
        Ok(caller) => caller,
        Err(refusal) => {
            tracing::info!(
                reason = refusal.reason.code(),
                "close: refused the whole request — the caller's pane claim does not stand"
            );
            report.refused = Some(refusal);
            return report;
        }
    };
    let (resolved, refused) = resolve(&selector, &caller, registry);
    report.targets.extend(refused);
    for target in resolved {
        let entry = close_one(
            &target, &caller, force, dry_run, registry, state, event_tx, worktrees,
        )
        .await;
        report.targets.push(entry);
    }
    report
}

/// Steps 3 to 6 for one target.
#[allow(clippy::too_many_arguments)]
async fn close_one(
    target: &Resolved,
    caller: &Caller,
    force: bool,
    dry_run: bool,
    registry: &Arc<AgentPtyRegistry>,
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    worktrees: &WorktreeRegistry,
) -> CloseTarget {
    let members = enumerate(&target.scope, registry);
    let (target_facts, statuses) =
        facts(target, target.unit.as_ref(), &members, state, registry).await;
    let mut out = base_target(target, target.unit.as_ref(), &members, &statuses, registry);
    if let Err(refusal) = authorize_target(caller, target, &members) {
        refuse(&mut out, refusal.reason, refusal.message);
        return out;
    }
    let refusals = default_refusals(&target_facts);
    if dry_run {
        out.would_refuse = refusals.iter().map(|(r, _)| *r).collect();
        return out;
    }
    if members.is_empty() {
        // A unit whose records are all gone: nothing to stop. End it, so its
        // name stops resolving, and clean its worktree up.
        if let Some(unit) = target.unit.as_ref() {
            registry.dispatched_units().end(
                &unit.id,
                EndReason::ClosedByVerb,
                chrono::Utc::now().timestamp_millis(),
            );
            out.worktree_verdict =
                Some(cleanup_worktree(unit.worktree.clone(), registry, worktrees, event_tx).await);
        }
        out.outcome = CloseOutcome::Closed;
        return out;
    }

    // Step 5a: the Closing admission state — for the unit, and for an
    // orchestration instance taken whole.
    let unit_prior = match target.unit.as_ref() {
        Some(unit) if !target.by_pane => {
            match registry.dispatched_units().begin_closing(&unit.id) {
                Some(prior) => Some((unit.id.clone(), prior)),
                None => {
                    out.outcome = CloseOutcome::Failed;
                    out.error =
                        Some("another close of this unit is already in progress".to_string());
                    return out;
                }
            }
        }
        _ => None,
    };
    let restore_unit = |registry: &AgentPtyRegistry| {
        if let Some((id, prior)) = unit_prior.as_ref() {
            registry.dispatched_units().abort_closing(id, *prior);
        }
    };
    let mut instance_guard = match &target.scope {
        Scope::Instance(id) => match registry.begin_instance_close(id) {
            Some(guard) => Some(guard),
            None => {
                restore_unit(registry);
                out.outcome = CloseOutcome::Failed;
                out.error =
                    Some("another close of this orchestration is already in progress".to_string());
                return out;
            }
        },
        Scope::Generation(_) => None,
    };

    // Step 5b: a cleanup hold on every member generation, re-enumerating until
    // the membership is stable. New members are refused from here on, so this
    // converges; one that reserved before the close began is seen and held.
    let mut holds: Vec<(String, crate::agent_pty::PaneCleanupHold)> = Vec::new();
    let mut held: Vec<String> = Vec::new();
    let mut members = members;
    let mut stable = false;
    for _ in 0..MEMBERSHIP_ROUNDS {
        let mut preflight_error = None;
        for member in &members {
            if held.contains(&member.record.id) {
                continue;
            }
            held.push(member.record.id.clone());
            let Some(pane) = member.record.pane_id_env.as_deref() else {
                continue;
            };
            match registry.hold_pane_for_cleanup(pane, &member.record.id) {
                Some(hold) => holds.push((member.record.id.clone(), hold)),
                // A retired generation whose pane another member now holds is
                // stopped without pane cleanup, exactly as `StopAgent` does.
                None if member.exited => {}
                None => {
                    preflight_error = Some(format!(
                        "pane {} is being closed by another request",
                        crate::config_validation::escape_id_for_log(pane)
                    ));
                    break;
                }
            }
        }
        if let Some(error) = preflight_error {
            drop(holds);
            drop(instance_guard);
            restore_unit(registry);
            out.outcome = CloseOutcome::Failed;
            out.error = Some(error);
            return out;
        }
        let again = enumerate(&target.scope, registry);
        let same = again.len() == members.len()
            && again
                .iter()
                .all(|m| members.iter().any(|old| old.record.id == m.record.id));
        members = again;
        if same {
            stable = true;
            break;
        }
    }
    if !stable {
        drop(holds);
        drop(instance_guard);
        restore_unit(registry);
        out.outcome = CloseOutcome::Failed;
        out.error = Some("the orchestration kept changing while it was being closed".to_string());
        return out;
    }

    // Step 5c: re-validate everything decided before the holds.
    let unit_now = target
        .unit
        .as_ref()
        .and_then(|u| registry.dispatched_units().get(&u.id).cloned());
    let (now_facts, now_statuses) =
        facts(target, unit_now.as_ref(), &members, state, registry).await;
    out.panes = members
        .iter()
        .zip(now_statuses.iter())
        .map(|(m, s)| pane_report(m, s.as_ref()))
        .collect();
    let release = |holds, guard, registry: &AgentPtyRegistry| {
        drop::<Vec<(String, crate::agent_pty::PaneCleanupHold)>>(holds);
        drop::<Option<crate::agent_pty::InstanceCloseGuard>>(guard);
        restore_unit(registry);
    };
    if !caller_still_current(caller, registry) {
        release(holds, instance_guard, registry);
        refuse(
            &mut out,
            CloseRefusalReason::Superseded,
            "refused: the calling agent no longer holds its pane".to_string(),
        );
        return out;
    }
    if let Err(refusal) = authorize_target(caller, target, &members) {
        release(holds, instance_guard, registry);
        refuse(&mut out, refusal.reason, refusal.message);
        return out;
    }
    let refusals = default_refusals(&now_facts);
    if !refusals.is_empty() && !force {
        release(holds, instance_guard, registry);
        let (reason, detail) = refusals[0].clone();
        let message = match reason {
            CloseRefusalReason::NotReported => {
                format!("{detail}; pass --force to close it anyway")
            }
            CloseRefusalReason::Busy => format!("{detail}; pass --force to close it anyway"),
            _ => format!("{detail}; or pass --force"),
        };
        refuse(&mut out, reason, message);
        return out;
    }
    out.forced_over = refusals.iter().map(|(r, _)| *r).collect();

    // Step 5d: disclose, #1109-style — one line, naming the caller, the
    // selector and every pane, and at `warn!` with what was overridden when
    // forced.
    let lines: Vec<String> = members
        .iter()
        .map(|m| crate::daemon_stop::teardown_agent_line(&m.record))
        .collect();
    let selector = crate::config_validation::escape_field_for_log(
        &target.selector,
        crate::config_validation::MAX_QUOTED_VALUE_CHARS,
    );
    if refusals.is_empty() {
        tracing::info!(
            caller = %caller_label(caller),
            selector = %selector,
            unit_id = target.unit.as_ref().map(|u| u.id.as_str()).unwrap_or("-"),
            panes = %format!("[{}]", lines.join("; ")),
            "close: closing"
        );
    } else {
        let overrode: Vec<String> = refusals
            .iter()
            .map(|(r, detail)| format!("{} ({detail})", r.code()))
            .collect();
        tracing::warn!(
            caller = %caller_label(caller),
            selector = %selector,
            unit_id = target.unit.as_ref().map(|u| u.id.as_str()).unwrap_or("-"),
            panes = %format!("[{}]", lines.join("; ")),
            forced_over = %overrode.join(", "),
            "close: closing with --force, overriding its refusals"
        );
    }

    // Step 5e: stop, orchestrator first.
    let mut survivors = Vec::new();
    let mut errors = Vec::new();
    for (index, member) in members.iter().enumerate() {
        let hold_pane = holds
            .iter()
            .find(|(id, _)| id == &member.record.id)
            .map(|(_, hold)| hold.pane_id().to_string());
        let result = crate::daemon_protocol::stop_agent_steps(
            &member.record.id,
            Some(&member.record),
            hold_pane.as_deref(),
            registry,
            state,
            event_tx,
            EndReason::ClosedByVerb,
        )
        .await;
        out.panes[index].stopped = Some(result.is_ok());
        if let Err(error) = result {
            survivors.push(
                member
                    .record
                    .pane_id_env
                    .clone()
                    .unwrap_or_else(|| member.record.id.clone()),
            );
            errors.push(error);
        }
    }
    drop(holds);
    let stopped_any = survivors.len() < members.len();
    if survivors.is_empty() {
        out.outcome = CloseOutcome::Closed;
        if let Some(guard) = instance_guard.as_mut() {
            guard.finish();
        }
    } else if stopped_any {
        out.outcome = CloseOutcome::PartiallyClosed;
        out.survivors = survivors;
        out.error = Some(errors.join("; "));
    } else {
        out.outcome = CloseOutcome::Failed;
        out.survivors = survivors;
        out.error = Some(errors.join("; "));
    }
    // A unit that still has members (a partial close, or one role closed by
    // `--pane`) stays live.
    restore_unit(registry);

    // Step 6: the worktree, once nothing of the target is left in it. Only for
    // a whole target: one role closed by `--pane` leaves its siblings rooted
    // there.
    if out.outcome == CloseOutcome::Closed && !target.by_pane {
        let worktree = target
            .unit
            .as_ref()
            .map(|u| u.worktree.clone())
            .or_else(|| {
                members
                    .first()
                    .and_then(|m| crate::issue_dispatch_run::worktree_of_record(&m.record))
            });
        if let Some(worktree) = worktree {
            let recorded = worktrees
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&worktree);
            // An ordinary pane's cwd is not a worktree this deck created; only
            // a recorded one, or any unit's, gets a verdict.
            if recorded || target.unit.is_some() {
                out.worktree_verdict =
                    Some(cleanup_worktree(worktree, registry, worktrees, event_tx).await);
            }
        }
    }
    drop(instance_guard);
    out
}

/// PRD #1589 D6: remove a closed unit's worktree under the existing policy
/// (`KeepIfDirty` for a dispatch, the branch always kept), holding the
/// directory against new spawns while it is deleted, and answer with the
/// typed verdict — waiting at most [`WORKTREE_CLEANUP_TIMEOUT`]. A kept tree
/// is broadcast as `WorktreeKept`, as `StopAgent`'s detached cleanup does.
pub async fn cleanup_worktree(
    worktree: PathBuf,
    registry: &Arc<AgentPtyRegistry>,
    worktrees: &WorktreeRegistry,
    event_tx: &broadcast::Sender<BroadcastMsg>,
) -> WorktreeVerdict {
    let path = worktree.to_string_lossy().into_owned();
    let registry = registry.clone();
    let worktrees = worktrees.clone();
    let event_tx = event_tx.clone();
    let task = tokio::spawn(async move {
        let Some(entry) = crate::issue_dispatch_run::take_worktree(&worktrees, &worktree) else {
            return WorktreeVerdictKind::NotRecorded;
        };
        let Some(_removal_hold) = registry.hold_dir_for_removal(&worktree) else {
            // Something still runs in it. Put the entry back so the close that
            // stops it can clean up.
            crate::issue_dispatch_run::record_worktree(
                &worktrees,
                &worktree,
                &entry.clone_dir,
                entry.policy,
            );
            return WorktreeVerdictKind::StillInUse;
        };
        let removal = crate::issue_dispatch_run::remove_worktree_outcome(
            &worktree,
            &entry.clone_dir,
            entry.policy,
        )
        .await;
        if let Some(kept) = removal.kept(&worktree) {
            let _ = event_tx.send(BroadcastMsg::WorktreeKept(kept));
        }
        match removal {
            crate::issue_dispatch_run::WorktreeRemoval::Removed => WorktreeVerdictKind::Removed,
            crate::issue_dispatch_run::WorktreeRemoval::KeptDirty => WorktreeVerdictKind::KeptDirty,
            crate::issue_dispatch_run::WorktreeRemoval::KeptCouldNotCheck => {
                WorktreeVerdictKind::KeptCouldNotCheck
            }
            crate::issue_dispatch_run::WorktreeRemoval::RemoveFailed => {
                WorktreeVerdictKind::RemoveFailed
            }
        }
    });
    let verdict = match tokio::time::timeout(WORKTREE_CLEANUP_TIMEOUT, task).await {
        Ok(Ok(verdict)) => verdict,
        Ok(Err(join_error)) => {
            tracing::warn!(error = %join_error, "close: the worktree cleanup task failed");
            WorktreeVerdictKind::RemoveFailed
        }
        Err(_) => WorktreeVerdictKind::TimedOut,
    };
    WorktreeVerdict { path, verdict }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent_pty::DOT_AGENT_DECK_PANE_ID;
    use crate::daemon_client::{DaemonClient, StartAgentOptions};
    use crate::daemon_protocol::{UnitSelector, bind_attach_listener, serve_attach_with_counter};
    use crate::dispatched_units::{Dispatcher, NewUnit};

    /// A real attach server over a real registry, with agents started through
    /// the socket the way a client starts them.
    struct Deck {
        dir: tempfile::TempDir,
        registry: Arc<AgentPtyRegistry>,
        state: SharedState,
        worktrees: WorktreeRegistry,
        client: DaemonClient,
        server: tokio::task::JoinHandle<()>,
    }

    impl Deck {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let sock = dir.path().join("attach.sock");
            let registry = Arc::new(AgentPtyRegistry::new());
            let (event_tx, _rx) = broadcast::channel(64);
            let state: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            let worktrees = crate::issue_dispatch_run::new_worktree_registry();
            let listener = bind_attach_listener(&sock).expect("bind");
            let server = {
                let registry = registry.clone();
                let state = state.clone();
                let worktrees = worktrees.clone();
                tokio::spawn(async move {
                    let _ = serve_attach_with_counter(
                        listener,
                        registry,
                        event_tx,
                        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                        state,
                        None,
                        Arc::new(crate::scheduler::Scheduler::with_stderr_notifier()),
                        crate::spawn::new_reuse_registry(),
                        worktrees,
                    )
                    .await;
                })
            };
            let client = DaemonClient::new(sock);
            Deck {
                dir,
                registry,
                state,
                worktrees,
                client,
                server,
            }
        }

        async fn start(
            &self,
            pane: &str,
            command: &str,
            orch: Option<(&str, &str, bool)>,
        ) -> String {
            let cwd = self.dir.path().to_string_lossy().into_owned();
            let tab_membership = orch.map(|(id, role, start)| TabMembership::Orchestration {
                name: "team".to_string(),
                role_index: usize::from(!start),
                role_name: role.to_string(),
                is_start_role: start,
                orchestration_cwd: Some(cwd.clone()),
                display_title: None,
                orchestration_id: Some(id.to_string()),
            });
            self.client
                .start_agent(StartAgentOptions {
                    command: Some(command.to_string()),
                    cwd: Some(cwd),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                    tab_membership,
                    ..StartAgentOptions::default()
                })
                .await
                .expect("start an agent through the attach socket")
        }

        fn claim(&self, pane: &str, agent: &str) -> CallerClaim {
            CallerClaim {
                pane_id: pane.to_string(),
                token: self.registry.hook_token_of(agent).expect("a minted token"),
            }
        }

        fn single_unit(
            &self,
            name: &str,
            dispatcher: (&str, &str),
            pane: &str,
            agent: &str,
        ) -> String {
            self.registry.dispatched_units().register(NewUnit {
                name: name.to_string(),
                worktree: self.dir.path().join(format!("wt-{name}")),
                branch: format!("agent/dispatch-{name}"),
                clone_dir: self.dir.path().to_path_buf(),
                dispatcher: Dispatcher {
                    pane_id: dispatcher.0.to_string(),
                    agent_id: dispatcher.1.to_string(),
                },
                kind: UnitKind::Single {
                    pane_id: pane.to_string(),
                    agent_id: agent.to_string(),
                },
                dispatched_at_ms: 1,
            })
        }

        fn orch_unit(
            &self,
            name: &str,
            dispatcher: (&str, &str),
            orch: &str,
            pane: &str,
            agent: &str,
        ) -> String {
            self.registry.dispatched_units().register(NewUnit {
                name: name.to_string(),
                worktree: self.dir.path().join(format!("wt-{name}")),
                branch: format!("agent/dispatch-{name}"),
                clone_dir: self.dir.path().to_path_buf(),
                dispatcher: Dispatcher {
                    pane_id: dispatcher.0.to_string(),
                    agent_id: dispatcher.1.to_string(),
                },
                kind: UnitKind::Orchestration {
                    orchestration_id: orch.to_string(),
                    name: "team".to_string(),
                    terminal_pane_id: pane.to_string(),
                    terminal_agent_id: agent.to_string(),
                },
                dispatched_at_ms: 1,
            })
        }

        fn report_done(&self, pane: &str, agent: &str) {
            assert!(
                self.registry
                    .dispatched_units()
                    .mark_completed(pane, agent, 2)
                    .is_some()
            );
        }

        async fn thinking(&self, pane: &str, agent: &str) {
            self.state
                .write()
                .await
                .apply_event(crate::event::AgentEvent {
                    session_id: format!("{pane}-session"),
                    agent_type: crate::event::AgentType::Pi,
                    event_type: crate::event::EventType::Thinking,
                    tool_name: None,
                    tool_detail: None,
                    cwd: None,
                    timestamp: chrono::Utc::now(),
                    user_prompt: None,
                    metadata: Default::default(),
                    pane_id: Some(pane.to_string()),
                    agent_id: Some(agent.to_string()),
                    agent_version: None,
                    schema_version: None,
                    live_target: None,
                });
            assert_eq!(
                self.state.read().await.pane_status_of(pane),
                Some(crate::state::SessionStatus::Thinking),
                "the status must have landed for the busy check to read it"
            );
        }

        async fn close(
            &self,
            selector: CloseSelector,
            caller: Option<CallerClaim>,
            force: bool,
            dry_run: bool,
        ) -> CloseReport {
            self.client
                .close_agents(selector, caller, force, dry_run)
                .await
                .expect("a report")
        }

        fn live(&self, agent: &str) -> bool {
            self.registry.agent_is_live(agent)
        }

        async fn shutdown(self) {
            self.registry.shutdown_all();
            self.server.abort();
            let _ = self.worktrees;
        }
    }

    fn by_name(name: &str) -> CloseSelector {
        CloseSelector::Units(UnitSelector {
            names: vec![name.to_string()],
        })
    }

    async fn wait_exited(registry: &AgentPtyRegistry, agent: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.agent_is_live(agent) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the child never exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Scenario: an agent presents a pane claim whose token is not one this
    /// daemon minted for that pane. The whole request is refused as
    /// not-attested and nothing is stopped — it never falls back to a person.
    #[tokio::test]
    async fn a_non_attested_claim_is_refused_whole() {
        let deck = Deck::new().await;
        let dispatcher = deck.start("disp", "sleep 30", None).await;
        let unit_agent = deck.start("unit", "sleep 30", None).await;
        deck.single_unit("u1", ("disp", &dispatcher), "unit", &unit_agent);
        deck.report_done("unit", &unit_agent);
        let forged = CallerClaim {
            pane_id: "disp".to_string(),
            token: "0".repeat(crate::hook_provenance::TOKEN_LEN),
        };
        let report = deck.close(by_name("u1"), Some(forged), true, false).await;
        let refused = report.refused.expect("refused whole");
        assert_eq!(refused.reason, CloseRefusalReason::NotAttested);
        assert!(report.targets.is_empty());
        assert!(deck.live(&unit_agent), "nothing may be stopped");
        // A claim naming another pane with a real token is refused the same way.
        let wrong_pane = CallerClaim {
            pane_id: "unit".to_string(),
            token: deck.registry.hook_token_of(&dispatcher).unwrap(),
        };
        let report = deck
            .close(by_name("u1"), Some(wrong_pane), true, false)
            .await;
        assert_eq!(
            report.refused.unwrap().reason,
            CloseRefusalReason::NotAttested
        );
        assert!(deck.live(&unit_agent));
        deck.shutdown().await;
    }

    /// Scenario: a dispatcher's generation exits and a new agent takes its
    /// pane. The old generation's token still attests itself, but it no
    /// longer holds the pane, so its close is refused as superseded — and
    /// stays refused after the new occupant exits too. The new occupant cannot
    /// close the old dispatcher's unit either.
    #[tokio::test]
    async fn a_superseded_caller_is_refused_even_after_its_successor_exits() {
        let deck = Deck::new().await;
        let old = deck.start("disp", "/usr/bin/true", None).await;
        wait_exited(&deck.registry, &old).await;
        let unit_agent = deck.start("unit", "sleep 30", None).await;
        deck.single_unit("u1", ("disp", &old), "unit", &unit_agent);
        deck.report_done("unit", &unit_agent);
        let old_claim = deck.claim("disp", &old);
        // Retired, no successor yet: fail closed.
        let report = deck
            .close(by_name("u1"), Some(old_claim.clone()), false, false)
            .await;
        assert_eq!(
            report.refused.unwrap().reason,
            CloseRefusalReason::Superseded
        );
        // Handover: a new agent on the same pane.
        let new = deck.start("disp", "sleep 30", None).await;
        let report = deck
            .close(by_name("u1"), Some(old_claim.clone()), false, false)
            .await;
        assert_eq!(
            report.refused.unwrap().reason,
            CloseRefusalReason::Superseded
        );
        let report = deck
            .close(by_name("u1"), Some(deck.claim("disp", &new)), true, false)
            .await;
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::NotYourUnit)
        );
        assert!(deck.live(&unit_agent));
        deck.shutdown().await;
    }

    /// Scenario: a dispatched orchestration has reported done, but one of its
    /// roles is mid-turn. Closing it by name is refused as busy and NO pane is
    /// stopped; with --force the whole instance closes, orchestrator first, the
    /// report names the busy refusal it overrode, and the unit ends.
    #[tokio::test]
    async fn one_busy_role_refuses_the_whole_unit_and_force_closes_it() {
        let deck = Deck::new().await;
        let dispatcher = deck.start("disp", "sleep 30", None).await;
        let orch = deck
            .start("o-orch", "sleep 30", Some(("orch-1", "orchestrator", true)))
            .await;
        let worker = deck
            .start("o-coder", "sleep 30", Some(("orch-1", "coder", false)))
            .await;
        deck.orch_unit("team-1", ("disp", &dispatcher), "orch-1", "o-orch", &orch);
        deck.report_done("o-orch", &orch);
        deck.thinking("o-coder", &worker).await;
        let claim = deck.claim("disp", &dispatcher);

        let report = deck
            .close(by_name("team-1"), Some(claim.clone()), false, false)
            .await;
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Refused);
        assert_eq!(target.reason, Some(CloseRefusalReason::Busy));
        assert!(
            deck.live(&orch) && deck.live(&worker),
            "no pane may be stopped"
        );
        assert!(
            deck.registry
                .dispatched_units()
                .unit_of_instance("orch-1")
                .is_some(),
            "the refused unit stays recorded and closeable"
        );

        let report = deck
            .close(by_name("team-1"), Some(claim), true, false)
            .await;
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
        assert_eq!(target.forced_over, vec![CloseRefusalReason::Busy]);
        assert_eq!(
            target.panes[0].pane_id.as_deref(),
            Some("o-orch"),
            "the orchestrator is stopped first"
        );
        assert!(target.panes.iter().all(|p| p.stopped == Some(true)));
        assert!(!deck.live(&orch) && !deck.live(&worker));
        assert!(
            deck.registry
                .dispatched_units()
                .unit_of_instance("orch-1")
                .is_none()
        );
        assert!(deck.live(&dispatcher), "the dispatcher is untouched");
        // A finished instance takes no new pane.
        assert!(deck.registry.is_instance_closing("orch-1"));
        deck.shutdown().await;
    }

    /// Scenario: a dry run lists the unit with what would refuse it, and stops
    /// nothing; a raw bulk request without dry_run is refused by the daemon and
    /// also stops nothing.
    #[tokio::test]
    async fn dry_runs_and_raw_bulk_requests_stop_nothing() {
        let deck = Deck::new().await;
        let dispatcher = deck.start("disp", "sleep 30", None).await;
        let unit_agent = deck.start("unit", "sleep 30", None).await;
        let id = deck.single_unit("u1", ("disp", &dispatcher), "unit", &unit_agent);
        let claim = deck.claim("disp", &dispatcher);

        let report = deck
            .close(by_name("u1"), Some(claim.clone()), false, true)
            .await;
        assert!(report.dry_run);
        assert_eq!(report.targets[0].outcome, CloseOutcome::Listed);
        assert_eq!(
            report.targets[0].would_refuse,
            vec![CloseRefusalReason::NotReported]
        );
        assert!(deck.live(&unit_agent));

        let report = deck
            .close(CloseSelector::AllUnits, Some(claim.clone()), true, true)
            .await;
        assert_eq!(report.targets.len(), 1);
        assert_eq!(report.targets[0].unit_id.as_deref(), Some(id.as_str()));
        assert!(deck.live(&unit_agent));

        let report = deck
            .close(CloseSelector::AllUnits, Some(claim), true, false)
            .await;
        assert_eq!(
            report.refused.unwrap().reason,
            CloseRefusalReason::BulkRequiresDryRun
        );
        assert!(deck.live(&unit_agent), "a raw bulk close must stop nothing");
        deck.shutdown().await;
    }

    /// Scenario: `--all --yes` applies by unit id. An id that ended, and one a
    /// previous daemon issued, are refused rather than retargeted.
    #[tokio::test]
    async fn stale_and_unknown_unit_ids_are_refused() {
        let deck = Deck::new().await;
        let unit_agent = deck.start("unit", "sleep 30", None).await;
        let id = deck.single_unit("u1", ("disp", "nobody"), "unit", &unit_agent);
        deck.report_done("unit", &unit_agent);
        let report = deck
            .close(
                CloseSelector::UnitIds {
                    ids: vec![id.clone()],
                },
                None,
                false,
                false,
            )
            .await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        let report = deck
            .close(
                CloseSelector::UnitIds {
                    ids: vec![id, "u-zzzzzz-1".to_string()],
                },
                None,
                false,
                false,
            )
            .await;
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::AlreadyEnded)
        );
        assert_eq!(
            report.targets[1].reason,
            Some(CloseRefusalReason::UnknownUnit)
        );
        assert!(
            report.targets[1]
                .message
                .as_deref()
                .unwrap()
                .contains("restarted")
        );
        deck.shutdown().await;
    }

    /// Scenario: closing an orchestration whose second role's stop fails. The
    /// report says partially-closed and names the survivor; it is never
    /// reported closed, and the unit stays recorded with its survivor.
    #[tokio::test]
    async fn a_failed_second_role_stop_is_reported_partially_closed() {
        let deck = Deck::new().await;
        let orch = deck
            .start("o-orch", "sleep 30", Some(("orch-2", "orchestrator", true)))
            .await;
        let worker = deck
            .start("o-coder", "sleep 30", Some(("orch-2", "coder", false)))
            .await;
        deck.orch_unit("team-2", ("disp", "nobody"), "orch-2", "o-orch", &orch);
        deck.report_done("o-orch", &orch);
        deck.registry.fail_next_close_for_test(&worker);
        let report = deck.close(by_name("team-2"), None, false, false).await;
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::PartiallyClosed, "{target:?}");
        assert_eq!(target.survivors, vec!["o-coder".to_string()]);
        assert!(!deck.live(&orch));
        assert!(deck.live(&worker));
        assert!(
            deck.registry
                .dispatched_units()
                .unit_of_instance("orch-2")
                .is_some()
        );
        assert!(
            !deck.registry.is_instance_closing("orch-2"),
            "a partial close releases the instance so it can be closed again"
        );
        let report = deck.close(by_name("team-2"), None, false, false).await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        assert!(!deck.live(&worker));
        deck.shutdown().await;
    }

    /// Scenario: while one instance is closing, a spawn into it is refused;
    /// another instance keeps spawning. After the close finishes, the closed
    /// instance still takes no new pane.
    #[tokio::test]
    async fn a_spawn_into_a_closing_instance_is_refused_and_others_are_not() {
        let deck = Deck::new().await;
        let mut guard = deck
            .registry
            .begin_instance_close("orch-a")
            .expect("begins");
        assert!(
            deck.registry.begin_instance_close("orch-a").is_none(),
            "one close at a time"
        );
        let cwd = deck.dir.path().to_string_lossy().into_owned();
        let into = |id: &str, pane: &str| StartAgentOptions {
            command: Some("sleep 30".to_string()),
            cwd: Some(cwd.clone()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: "team".to_string(),
                role_index: 1,
                role_name: "coder".to_string(),
                is_start_role: false,
                orchestration_cwd: Some(cwd.clone()),
                display_title: None,
                orchestration_id: Some(id.to_string()),
            }),
            ..StartAgentOptions::default()
        };
        let err = deck
            .client
            .start_agent(into("orch-a", "a-new"))
            .await
            .expect_err("a closing instance takes no new pane");
        assert!(
            err.to_string()
                .contains(crate::agent_pty::INSTANCE_CLOSING_REASON),
            "{err}"
        );
        deck.client
            .start_agent(into("orch-b", "b-new"))
            .await
            .expect("another instance is unaffected");
        guard.finish();
        drop(guard);
        deck.client
            .start_agent(into("orch-a", "a-later"))
            .await
            .expect_err("a closed instance cannot be resurrected");
        deck.shutdown().await;
    }

    /// Scenario: one name matches live units in two clones. A person's close by
    /// that name is refused as ambiguous and stops nothing; the refusal lists
    /// each candidate's unit id, panes and worktree so it can be closed by id.
    #[tokio::test]
    async fn an_ambiguous_name_lists_every_candidate_and_stops_nothing() {
        let deck = Deck::new().await;
        let a = deck.start("unit-a", "sleep 30", None).await;
        let b = deck.start("unit-b", "sleep 30", None).await;
        let first = deck.single_unit("fix", ("disp", "nobody"), "unit-a", &a);
        let second = deck.single_unit("fix", ("disp", "nobody"), "unit-b", &b);
        let report = deck.close(by_name("fix"), None, true, false).await;
        let target = &report.targets[0];
        assert_eq!(target.reason, Some(CloseRefusalReason::Ambiguous));
        let ids: Vec<&str> = target
            .candidates
            .iter()
            .map(|c| c.unit_id.as_str())
            .collect();
        assert_eq!(ids, vec![first.as_str(), second.as_str()]);
        assert_eq!(target.candidates[0].panes, vec!["unit-a".to_string()]);
        assert!(target.candidates[1].worktree.ends_with("wt-fix"));
        assert!(deck.live(&a) && deck.live(&b));
        deck.shutdown().await;
    }

    /// Scenario: an agent may not close a sibling dispatcher's unit, a pane no
    /// unit owns, or its own pane — and --force changes none of that.
    #[tokio::test]
    async fn authority_refusals_are_never_forceable() {
        let deck = Deck::new().await;
        let me = deck.start("me", "sleep 30", None).await;
        let sibling = deck.start("sib", "sleep 30", None).await;
        let theirs = deck.start("theirs", "sleep 30", None).await;
        let plain = deck.start("plain", "sleep 30", None).await;
        deck.single_unit("their-unit", ("sib", &sibling), "theirs", &theirs);
        deck.report_done("theirs", &theirs);
        let claim = deck.claim("me", &me);

        let report = deck
            .close(by_name("their-unit"), Some(claim.clone()), true, false)
            .await;
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::NotYourUnit)
        );
        let report = deck
            .close(
                CloseSelector::Pane {
                    pane_id: "plain".into(),
                },
                Some(claim.clone()),
                true,
                false,
            )
            .await;
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::NotYourUnit)
        );
        let report = deck
            .close(
                CloseSelector::Pane {
                    pane_id: "me".into(),
                },
                Some(claim),
                true,
                false,
            )
            .await;
        assert_eq!(report.targets[0].reason, Some(CloseRefusalReason::OwnPane));
        assert!(deck.live(&me) && deck.live(&theirs) && deck.live(&plain));
        deck.shutdown().await;
    }

    /// Scenario: an unreported unit is refused not-reported whether it is
    /// named by its dispatch name or by `--pane` (auditor B4), and the name is
    /// resolved only once when it is given twice.
    #[tokio::test]
    async fn the_same_unit_meets_the_same_refusal_through_every_selector() {
        let deck = Deck::new().await;
        let dispatcher = deck.start("disp", "sleep 30", None).await;
        let unit_agent = deck.start("unit", "sleep 30", None).await;
        deck.single_unit("u1", ("disp", &dispatcher), "unit", &unit_agent);
        let claim = deck.claim("disp", &dispatcher);
        let report = deck
            .close(
                CloseSelector::Units(UnitSelector {
                    names: vec!["u1".into(), "u1".into()],
                }),
                Some(claim.clone()),
                false,
                false,
            )
            .await;
        assert_eq!(report.targets.len(), 1, "aliases are deduplicated");
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::NotReported)
        );
        let report = deck
            .close(
                CloseSelector::Pane {
                    pane_id: "unit".into(),
                },
                Some(claim.clone()),
                false,
                false,
            )
            .await;
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::NotReported)
        );
        assert!(deck.live(&unit_agent));
        let report = deck
            .close(
                CloseSelector::Pane {
                    pane_id: "unit".into(),
                },
                Some(claim),
                true,
                false,
            )
            .await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        assert_eq!(
            report.targets[0].forced_over,
            vec![CloseRefusalReason::NotReported]
        );
        assert!(!deck.live(&unit_agent));
        deck.shutdown().await;
    }

    /// Scenario: the orchestrator pane of a dispatched orchestration is closed
    /// first through the ordinary stop. The unit stays recorded and closeable
    /// by name with its remaining worker; closing it then ends the unit.
    #[tokio::test]
    async fn an_orchestrator_closed_first_leaves_the_unit_closeable() {
        let deck = Deck::new().await;
        let orch = deck
            .start("o-orch", "sleep 30", Some(("orch-3", "orchestrator", true)))
            .await;
        let worker = deck
            .start("o-coder", "sleep 30", Some(("orch-3", "coder", false)))
            .await;
        deck.orch_unit("team-3", ("disp", "nobody"), "orch-3", "o-orch", &orch);
        deck.report_done("o-orch", &orch);
        deck.client
            .stop_agent(&orch)
            .await
            .expect("stop the orchestrator");
        assert!(
            deck.registry
                .dispatched_units()
                .unit_of_instance("orch-3")
                .is_some(),
            "the unit outlives its orchestrator while a worker remains"
        );
        let report = deck.close(by_name("team-3"), None, false, false).await;
        assert_eq!(
            report.targets[0].outcome,
            CloseOutcome::Closed,
            "{:?}",
            report.targets[0]
        );
        assert!(!deck.live(&worker));
        assert!(
            deck.registry
                .dispatched_units()
                .unit_of_instance("orch-3")
                .is_none()
        );
        deck.shutdown().await;
    }

    /// Scenario: a unit whose agent exited on its own is still closeable — by a
    /// person with --force when it never reported — and the close ends it.
    #[tokio::test]
    async fn an_exited_unit_is_still_closeable() {
        let deck = Deck::new().await;
        let unit_agent = deck.start("unit", "/usr/bin/true", None).await;
        let id = deck.single_unit("u1", ("disp", "nobody"), "unit", &unit_agent);
        wait_exited(&deck.registry, &unit_agent).await;
        deck.registry
            .note_unit_member_exited(&unit_agent, Some("unit"));
        assert_eq!(
            deck.registry.dispatched_units().get(&id).unwrap().state,
            crate::dispatched_units::UnitState::Exited
        );
        let report = deck.close(by_name("u1"), None, false, false).await;
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::NotReported)
        );
        let report = deck.close(by_name("u1"), None, true, false).await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        deck.shutdown().await;
    }

    fn init_repo_with_worktree(
        sandbox: &std::path::Path,
        repo: &std::path::Path,
        wt: &std::path::Path,
    ) {
        let run = |dir: &std::path::Path, args: &[&str]| {
            let out = crate::git_env::fixture_git(dir, sandbox)
                .args(args)
                .output()
                .expect("git available");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        std::fs::create_dir_all(repo).unwrap();
        run(repo, &["init", "-q", "."]);
        crate::worktree_owner::pin_fixture_eol(repo, sandbox);
        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-qm", "init"]);
        run(
            repo,
            &["worktree", "add", "-q", "-b", "wt", &wt.to_string_lossy()],
        );
    }

    /// Scenario: the close's worktree cleanup answers with a typed verdict — a
    /// dirty tree is kept-dirty, a clean one removed, one with an agent still
    /// in it still-in-use (its entry kept for the close that stops it), and one
    /// the daemon has no record of not-recorded.
    #[tokio::test]
    async fn the_worktree_cleanup_reports_a_typed_verdict() {
        let sandbox = tempfile::tempdir().unwrap();
        let repo = sandbox.path().join("repo");
        let wt = sandbox.path().join("repo-dispatch-x");
        init_repo_with_worktree(sandbox.path(), &repo, &wt);
        let registry = Arc::new(AgentPtyRegistry::new());
        let worktrees = crate::issue_dispatch_run::new_worktree_registry();
        let (event_tx, _rx) = broadcast::channel(8);
        let record = |wts: &WorktreeRegistry| {
            crate::issue_dispatch_run::record_worktree(
                wts,
                &wt,
                &repo,
                crate::issue_dispatch_run::RemovalPolicy::KeepIfDirty,
            )
        };

        assert_eq!(
            cleanup_worktree(wt.clone(), &registry, &worktrees, &event_tx)
                .await
                .verdict,
            WorktreeVerdictKind::NotRecorded
        );

        record(&worktrees);
        std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();
        assert_eq!(
            cleanup_worktree(wt.clone(), &registry, &worktrees, &event_tx)
                .await
                .verdict,
            WorktreeVerdictKind::KeptDirty
        );
        std::fs::remove_file(wt.join("dirty.txt")).unwrap();

        record(&worktrees);
        let reservation = registry
            .reserve_spawn_in_for_test(&wt)
            .expect("a start rooted in the tree");
        assert_eq!(
            cleanup_worktree(wt.clone(), &registry, &worktrees, &event_tx)
                .await
                .verdict,
            WorktreeVerdictKind::StillInUse
        );
        assert!(
            worktrees.lock().unwrap().contains_key(&wt),
            "a tree still in use keeps its entry for the close that frees it"
        );
        registry.release_spawn_for_test(&reservation);

        assert_eq!(
            cleanup_worktree(wt.clone(), &registry, &worktrees, &event_tx)
                .await
                .verdict,
            WorktreeVerdictKind::Removed
        );
        assert!(!wt.exists());
    }
}
