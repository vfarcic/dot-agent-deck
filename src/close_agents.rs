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

use std::collections::{HashMap, HashSet};
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

/// PRD #1589 (auditor S4): the most entries a selector may name. Names and ids
/// are typed or copied out of a listing, so a few hundred is far past any real
/// request while keeping the work and the report one request can cause small.
/// A larger selector is refused whole ([`CloseRefusalReason::SelectorTooLarge`])
/// before anything is resolved or stopped. It bounds how many targets one
/// report has; what each target lists is bounded separately
/// ([`MAX_LISTED_TARGETS`], [`MAX_NESTED_REPORT_BYTES`]).
pub const MAX_SELECTOR_ENTRIES: usize = 256;

/// PRD #1589 (auditor S4): the longest single selector entry, in bytes. A
/// dispatch name is recorded at 120 characters, a unit id and a pane id are far
/// shorter.
pub const MAX_SELECTOR_ENTRY_BYTES: usize = 512;

/// PRD #1589 (auditor S4): the most bytes all of a selector's entries may add up
/// to.
pub const MAX_SELECTOR_BYTES: usize = 32 * 1024;

/// PRD #1589 (auditor S4): the most units `--all` lists in one report, and the
/// most entries any one list in a target's report holds — an `ambiguous`
/// refusal's candidates, a candidate's panes, a target's open descendants and
/// panes. A daemon holding more live units than this lists the first ones and
/// says the listing was cut short ([`CloseReport::truncated`]); a cut list says
/// so on its target ([`CloseTarget::truncated`]). Applied while each list is
/// built: an entry past the limit is never cloned (auditor S3/SF2). A target's
/// `panes` is its report, not its stop set — a close stops every member,
/// however many of them its report lists.
pub const MAX_LISTED_TARGETS: usize = MAX_SELECTOR_ENTRIES;

/// PRD #1589 (auditor S3/SF2/SF3): the most bytes all of one report's nested
/// lists — panes, open descendants, candidates and their panes — may take
/// once serialized. Each entry is charged its JSON-encoded size, escapes
/// included, plus [`NESTED_ENTRY_OVERHEAD`], BEFORE its strings are cloned
/// into the report; once an entry does not fit, it and every later nested
/// entry of the report are left out and their targets marked
/// [`CloseTarget::truncated`]. A target's own fields are bounded by
/// [`MAX_REPORT_BYTES`] instead.
pub const MAX_NESTED_REPORT_BYTES: usize = 4 * 1024 * 1024;

/// What [`MAX_NESTED_REPORT_BYTES`] charges a nested entry beyond its
/// serialized strings: its field names, other values and punctuation.
const NESTED_ENTRY_OVERHEAD: usize = 128;

/// PRD #1589 (auditor SF3): the most bytes a whole serialized report may take.
/// The rest of the attach frame ([`REPORT_ENVELOPE_RESERVE`]) is left for the
/// response around it. A report over this — its targets' own fields are not
/// in [`MAX_NESTED_REPORT_BYTES`] — sheds nested lists and then shortens long
/// fields, last target first, until it fits ([`fit_deliverable`]), so the
/// response is written rather than refused by the frame cap.
pub const MAX_REPORT_BYTES: usize = crate::daemon_protocol::MAX_FRAME_LEN - REPORT_ENVELOPE_RESERVE;

/// The part of the attach frame a close report leaves to the response that
/// carries it.
const REPORT_ENVELOPE_RESERVE: usize = 1024 * 1024;

/// How many characters [`fit_deliverable`] keeps of a target's string field
/// it has to shorten.
const CLIPPED_FIELD_CHARS: usize = 256;

/// How many survivors [`fit_deliverable`] keeps of a target it has to
/// shorten.
const CLIPPED_SURVIVORS: usize = 8;

/// How many times one target is retried when the unit it names moved to a new
/// generation while it was being closed (auditor S2). Each retry re-reads the
/// unit's generation from the ledger; a unit that keeps being replaced fails.
const REBIND_ATTEMPTS: usize = 3;

/// The whole-request refusal for a selector past the limits above, or `None`.
/// The entry count is read from the slice's length before any entry is looked
/// at, so an oversized selector costs nothing proportional to its size; only an
/// accepted count of entries is walked for their bytes (auditor S3).
fn selector_too_large(selector: &CloseSelector) -> Option<CloseRefusal> {
    let entries: &[String] = match selector {
        CloseSelector::Units(units) => &units.names,
        CloseSelector::UnitIds { ids } => ids,
        CloseSelector::Pane { pane_id } | CloseSelector::OrchestrationOf { pane_id } => {
            std::slice::from_ref(pane_id)
        }
        CloseSelector::AllUnits => &[],
    };
    let limits = format!(
        "refused: a close names at most {MAX_SELECTOR_ENTRIES} entries of at most \
         {MAX_SELECTOR_ENTRY_BYTES} bytes each, {MAX_SELECTOR_BYTES} bytes in all"
    );
    let advice = "Nothing was closed; close them in smaller batches.";
    if entries.len() > MAX_SELECTOR_ENTRIES {
        return Some(CloseRefusal {
            reason: CloseRefusalReason::SelectorTooLarge,
            message: format!(
                "{limits}; this one names {} entries. {advice}",
                entries.len()
            ),
        });
    }
    let total: usize = entries.iter().map(String::len).sum();
    if entries.iter().all(|e| e.len() <= MAX_SELECTOR_ENTRY_BYTES) && total <= MAX_SELECTOR_BYTES {
        return None;
    }
    Some(CloseRefusal {
        reason: CloseRefusalReason::SelectorTooLarge,
        message: format!(
            "{limits}; this one names {} entries, {total} bytes. {advice}",
            entries.len()
        ),
    })
}

/// A sink that only counts what is written to it.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The bytes `value` takes as JSON, escapes included, counted without
/// building it (auditor SF3).
fn json_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut count = ByteCount(0);
    // The sink never fails, and the report's types serialize infallibly.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

fn opt_json_len(value: Option<&str>) -> usize {
    value.map_or(0, json_len)
}

/// PRD #1589 (auditor S3/SF2): what is left of one report's
/// [`MAX_NESTED_REPORT_BYTES`]. Cloned to mark a point an attempt can go back
/// to, so a target retried or re-projected is charged once.
#[derive(Clone)]
struct ReportBudget {
    remaining: usize,
}

impl ReportBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_NESTED_REPORT_BYTES,
        }
    }

    /// Charge `cost`, or answer `false` — and spend the rest, so every later
    /// nested entry is left out too — when it does not fit.
    fn take(&mut self, cost: usize) -> bool {
        if cost > self.remaining {
            self.remaining = 0;
            return false;
        }
        self.remaining -= cost;
        true
    }

    fn exhausted(&self) -> bool {
        self.remaining == 0
    }

    /// Re-charge the entries of an already built `list`, keeping those that
    /// fit, in order; `true` when one was cut.
    fn fit<T>(&mut self, list: &mut Vec<T>, cost: impl Fn(&T) -> usize) -> bool {
        let mut kept = 0;
        for entry in list.iter() {
            if !self.take(cost(entry)) {
                break;
            }
            kept += 1;
        }
        let cut = kept < list.len();
        list.truncate(kept);
        cut
    }
}

fn descendant_cost(d: &OpenDescendant) -> usize {
    NESTED_ENTRY_OVERHEAD + json_len(&d.unit_id) + json_len(&d.name) + json_len(&d.clone)
}

/// The report entries for `members` (each with the status it was read at),
/// in order: at most [`MAX_LISTED_TARGETS`] of them, each charged to `budget`
/// before it is built, and whether any was left out. What a close stops is
/// `members`, not this (auditor SF2).
fn project_panes<'a>(
    members: impl IntoIterator<Item = (&'a Member, Option<&'a crate::state::SessionStatus>)>,
    budget: &mut ReportBudget,
) -> (Vec<ClosePane>, bool) {
    let mut panes = Vec::new();
    for (member, status) in members {
        if panes.len() == MAX_LISTED_TARGETS {
            return (panes, true);
        }
        let role = match &member.record.tab_membership {
            Some(TabMembership::Orchestration { role_name, .. }) => Some(role_name.as_str()),
            _ => None,
        };
        let status_label = status.map(|s| format!("{s:?}"));
        let cost = NESTED_ENTRY_OVERHEAD
            + json_len(&member.record.id)
            + opt_json_len(member.record.pane_id_env.as_deref())
            + opt_json_len(role)
            + opt_json_len(status_label.as_deref());
        if !budget.take(cost) {
            return (panes, true);
        }
        panes.push(pane_report(member, status));
    }
    (panes, false)
}

/// PRD #1589 (auditor SF3): keep the serialized report within
/// [`MAX_REPORT_BYTES`]. Measured with a counting serializer, never built.
/// Over it, the nested lists are shed and then long fields shortened, last
/// target first, each target so cut marked [`CloseTarget::truncated`]. A
/// report has at most [`MAX_SELECTOR_ENTRIES`] targets, and
/// `a_shortened_target_fits_the_report_bound` pins that that many shortened
/// targets fit.
fn fit_deliverable(report: &mut CloseReport) {
    let targets = std::mem::take(&mut report.targets);
    let envelope = json_len(&*report);
    report.targets = targets;
    let mut sizes: Vec<usize> = report.targets.iter().map(json_len).collect();
    let total = |sizes: &[usize]| envelope + sizes.iter().sum::<usize>() + sizes.len();
    for shorten in [shed_lists as fn(&mut CloseTarget), clip_fields] {
        for at in (0..report.targets.len()).rev() {
            if total(&sizes) <= MAX_REPORT_BYTES {
                return;
            }
            shorten(&mut report.targets[at]);
            sizes[at] = json_len(&report.targets[at]);
        }
    }
    if total(&sizes) > MAX_REPORT_BYTES {
        tracing::warn!(
            bytes = total(&sizes),
            "close: the report is over its size bound after shortening every target"
        );
    }
}

fn shed_lists(target: &mut CloseTarget) {
    let had = !target.panes.is_empty()
        || !target.open_descendants.is_empty()
        || !target.candidates.is_empty();
    target.panes = Vec::new();
    target.open_descendants = Vec::new();
    target.candidates = Vec::new();
    target.truncated |= had;
}

/// Shorten every string field of `target` but `unit_id` to
/// [`CLIPPED_FIELD_CHARS`] characters and its survivors to
/// [`CLIPPED_SURVIVORS`].
fn clip_fields(target: &mut CloseTarget) {
    fn clip(s: &mut String) -> bool {
        match s.char_indices().nth(CLIPPED_FIELD_CHARS) {
            Some((at, _)) => {
                s.truncate(at);
                s.push('…');
                true
            }
            None => false,
        }
    }
    fn clip_opt(s: &mut Option<String>) -> bool {
        s.as_mut().is_some_and(clip)
    }
    let mut cut = target.survivors.len() > CLIPPED_SURVIVORS;
    target.survivors.truncate(CLIPPED_SURVIVORS);
    for survivor in &mut target.survivors {
        cut |= clip(survivor);
    }
    cut |= clip(&mut target.selector);
    cut |= clip(&mut target.kind);
    // `unit_id` is never shortened: it is the key a caller re-targets with
    // `close --unit-id` (reviewer R1).
    for field in [
        &mut target.name,
        &mut target.message,
        &mut target.error,
        &mut target.worktree,
        &mut target.branch,
        &mut target.clone,
    ] {
        cut |= clip_opt(field);
    }
    if let Some(dispatcher) = target.dispatcher.as_mut() {
        cut |= clip(&mut dispatcher.pane_id);
        cut |= clip(&mut dispatcher.agent_id);
    }
    if let Some(verdict) = target.worktree_verdict.as_mut() {
        cut |= clip(&mut verdict.path);
    }
    target.truncated |= cut;
}

/// At most [`MAX_LISTED_TARGETS`] items of `items`, and whether there were more
/// — taking one past the limit to know, and no further.
fn listed<T>(items: impl IntoIterator<Item = T>) -> (Vec<T>, bool) {
    let mut items = items.into_iter();
    let kept: Vec<T> = items.by_ref().take(MAX_LISTED_TARGETS).collect();
    let more = items.next().is_some();
    (kept, more)
}

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

/// PRD #1589 (auditor B1/B2): the members of `scope` that are in their
/// respawn window — lifted out of the registry's records and not yet replaced.
/// [`enumerate`] cannot see them, and a target whose only members are here is
/// not gone.
fn respawning(scope: &Scope, registry: &AgentPtyRegistry) -> Vec<AgentRecord> {
    registry
        .respawning_members()
        .into_iter()
        .filter(|record| match scope {
            Scope::Instance(id) => instance_of(record) == Some(id.as_str()),
            Scope::Generation(id) => &record.id == id,
        })
        .collect()
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

/// Step 2: resolve the selector into targets and refusals, and whether `--all`
/// had more live units than it lists. An ambiguous name's candidates are
/// charged to `budget` as they are listed (auditor SF2).
fn resolve(
    selector: &CloseSelector,
    caller: &Caller,
    registry: &AgentPtyRegistry,
    budget: &mut ReportBudget,
) -> (Vec<Resolved>, Vec<CloseTarget>, bool) {
    let mut resolved = Vec::new();
    let mut refused = Vec::new();
    let mut truncated = false;
    match selector {
        CloseSelector::Units(units) => {
            let mut seen_slugs = HashSet::new();
            let mut projections = PaneProjections::default();
            for name in &units.names {
                if !seen_slugs.insert(crate::dispatched_units::slug_of(name)) {
                    continue;
                }
                // Resolve under the record's lock, then let it go before the
                // candidates' panes are read from the registry.
                let resolution = {
                    let records = registry.dispatched_units();
                    // A spent budget lists no candidate, so it collects no ids.
                    let max = if budget.exhausted() {
                        0
                    } else {
                        MAX_LISTED_TARGETS
                    };
                    match records.resolve_name(name, caller, max) {
                        Ok(unit) => Ok(unit.clone()),
                        Err(r) => {
                            // Each candidate is charged before its strings are
                            // cloned, and listing stops when the budget is
                            // spent (auditor SF2/SF3).
                            let mut candidates = Vec::new();
                            let mut cut = r.candidates_total > r.candidates.len();
                            for id in &r.candidates {
                                let Some(unit) = records.get(id) else {
                                    continue;
                                };
                                let worktree = unit.worktree.to_string_lossy();
                                let cost = NESTED_ENTRY_OVERHEAD
                                    + json_len(&unit.id)
                                    + json_len(&unit.name)
                                    + json_len(worktree.as_ref());
                                if !budget.take(cost) {
                                    cut = true;
                                    break;
                                }
                                candidates.push((
                                    AmbiguousCandidate {
                                        unit_id: unit.id.clone(),
                                        name: unit.name.clone(),
                                        worktree: worktree.into_owned(),
                                        panes: Vec::new(),
                                    },
                                    scope_of_unit(unit),
                                ));
                            }
                            Err((r, candidates, cut))
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
                    Err((r, candidates, cut)) => {
                        let mut target = CloseTarget::refused(name, r.reason, r.message);
                        target.truncated = cut;
                        target.candidates = candidates
                            .into_iter()
                            .map(|(mut candidate, scope)| {
                                let (panes, more) =
                                    scope_panes(&scope, registry, budget, &mut projections);
                                candidate.panes = panes;
                                target.truncated |= more;
                                candidate
                            })
                            .collect();
                        refused.push(target);
                    }
                }
            }
        }
        CloseSelector::UnitIds { ids } => {
            let mut seen = HashSet::new();
            for id in ids {
                if !seen.insert(id.as_str()) {
                    continue;
                }
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
            // Cut at `MAX_LISTED_TARGETS` while walking the ledger, so only the
            // units listed are cloned (auditor S3).
            let records = registry.dispatched_units();
            let mine = records.live().filter(|unit| match caller {
                Caller::Person => true,
                Caller::Agent { agent_id, .. } => &unit.dispatcher.agent_id == agent_id,
            });
            let (units, more) = listed(mine);
            truncated = more;
            resolved.extend(units.into_iter().map(|unit| Resolved {
                selector: unit.id.clone(),
                scope: scope_of_unit(unit),
                unit: Some(unit.clone()),
                by_pane: false,
                named_pane: None,
            }));
        }
    }
    let mut keys = HashSet::new();
    resolved.retain(|r| keys.insert(r.dedupe_key()));
    (resolved, refused, truncated)
}

/// PRD #1589 (auditor S2): the generations `target` covers now. A unit's
/// generation moves when it is replaced in its pane (`note_generation_replaced`),
/// so the scope is re-read from the ledger by the unit's id — which no other
/// unit of this daemon ever carries — rather than kept from the resolution. A
/// pane target follows its pane only to a generation of the same unit; one with
/// no unit, or whose pane now holds another unit's agent, keeps the generation
/// it resolved to.
fn current_scope(target: &Resolved, registry: &AgentPtyRegistry) -> Scope {
    let Some(unit) = target.unit.as_ref() else {
        return target.scope.clone();
    };
    if !target.by_pane {
        return registry
            .dispatched_units()
            .get(&unit.id)
            .map(scope_of_unit)
            .unwrap_or_else(|| target.scope.clone());
    }
    if let Some(pane) = target.named_pane.as_deref()
        && let Some(record) = generation_on_pane(pane, registry)
        && unit_of_record(&record, registry).is_some_and(|now| now.id == unit.id)
    {
        return Scope::Generation(record.id);
    }
    target.scope.clone()
}

fn scope_of_unit(unit: &DispatchedUnit) -> Scope {
    match &unit.kind {
        UnitKind::Single { agent_id, .. } => Scope::Generation(agent_id.clone()),
        UnitKind::Orchestration {
            orchestration_id, ..
        } => Scope::Instance(orchestration_id.clone()),
    }
}

/// PRD #1589 (auditor SF5): the most pane ids one request's
/// [`PaneProjections`] keeps. Four instances' worth of a full listing.
const MAX_CACHED_PANES: usize = 4 * MAX_LISTED_TARGETS;

/// PRD #1589 (auditor SF5): one request's projections of orchestration
/// instances' panes, so candidates that share an instance reuse its
/// projection while it is resident here rather than walking the registry once
/// each. Each projection holds at most [`MAX_LISTED_TARGETS`] pane ids and the
/// cache at most [`MAX_CACHED_PANES`]: a new projection that would take it past
/// that clears it first, so an instance projected before the clear is walked
/// again if a later candidate lists it. What it holds is
/// charged to a report's budget each time it is copied into one. It is a
/// report's view only; what a close stops is read separately.
#[derive(Default)]
struct PaneProjections {
    instances: HashMap<String, (Vec<String>, bool)>,
    panes: usize,
}

impl PaneProjections {
    fn instance(&mut self, instance: &str, registry: &AgentPtyRegistry) -> &(Vec<String>, bool) {
        if !self.instances.contains_key(instance) {
            let projection = registry.instance_panes(instance, MAX_LISTED_TARGETS);
            if self.panes + projection.0.len() > MAX_CACHED_PANES {
                self.instances.clear();
                self.panes = 0;
            }
            self.panes += projection.0.len();
            self.instances.insert(instance.to_string(), projection);
        }
        &self.instances[instance]
    }
}

/// The panes of `scope`'s records for a candidate's report entry: at most
/// [`MAX_LISTED_TARGETS`], each charged to `budget` before it is copied into
/// the entry; and whether any was left out (auditor SF2). A spent budget lists
/// none and reads nothing from the registry, and says the list was cut
/// whether or not the scope had panes (auditor SF5).
fn scope_panes(
    scope: &Scope,
    registry: &AgentPtyRegistry,
    budget: &mut ReportBudget,
    projections: &mut PaneProjections,
) -> (Vec<String>, bool) {
    if budget.exhausted() {
        return (Vec::new(), true);
    }
    // A pane id is one array element: its JSON and a comma.
    let cost = |pane: &str| json_len(pane) + 1;
    match scope {
        Scope::Generation(id) => match registry.generation_pane(id) {
            None => (Vec::new(), false),
            Some(pane) if budget.take(cost(&pane)) => (vec![pane], false),
            Some(_) => (Vec::new(), true),
        },
        Scope::Instance(id) => {
            let (listed, more) = projections.instance(id, registry);
            let mut panes = Vec::new();
            for pane in listed {
                if !budget.take(cost(pane)) {
                    return (panes, true);
                }
                panes.push(pane.clone());
            }
            (panes, *more)
        }
    }
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
    // `--pane` on one role of an instance with other live roles — counting a
    // sibling in its respawn window, which is lifted out of the records while
    // its replacement starts and is still a teammate (review on PR #1681).
    let strands = target.by_pane
        && members.len() == 1
        && instance_of(&members[0].record).is_some_and(|instance| {
            registry
                .agent_records()
                .into_iter()
                .chain(registry.respawning_members())
                .any(|r| r.id != members[0].record.id && instance_of(&r) == Some(instance))
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

/// The report entry for a target, before its outcome is known, and whether
/// its open descendants were cut short. Its lists are charged to `budget` as
/// they are built: panes, then open descendants.
fn base_target(
    target: &Resolved,
    unit: Option<&DispatchedUnit>,
    members: &[Member],
    statuses: &[Option<crate::state::SessionStatus>],
    registry: &AgentPtyRegistry,
    budget: &mut ReportBudget,
) -> (CloseTarget, bool) {
    let mut out =
        CloseTarget::refused(&target.selector, CloseRefusalReason::Unknown, String::new());
    out.reason = None;
    out.message = None;
    out.outcome = CloseOutcome::Listed;
    let (panes, panes_cut) = project_panes(
        members.iter().zip(
            statuses
                .iter()
                .map(Option::as_ref)
                .chain(std::iter::repeat(None)),
        ),
        budget,
    );
    out.panes = panes;
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
    let mut descendants_cut = false;
    {
        let records = registry.dispatched_units();
        for u in records.open_descendants(&agent_ids) {
            if out.open_descendants.len() == MAX_LISTED_TARGETS {
                descendants_cut = true;
                break;
            }
            let clone = u.clone_dir.to_string_lossy();
            let cost = NESTED_ENTRY_OVERHEAD
                + json_len(&u.id)
                + json_len(&u.name)
                + json_len(clone.as_ref());
            if !budget.take(cost) {
                descendants_cut = true;
                break;
            }
            out.open_descendants.push(OpenDescendant {
                unit_id: u.id.clone(),
                name: u.name.clone(),
                clone: clone.into_owned(),
            });
        }
    }
    out.truncated = panes_cut || descendants_cut;
    (out, descendants_cut)
}

/// The refusal sentence for a forceable refusal a close was not forced past.
fn refusal_message(reason: CloseRefusalReason, detail: &str) -> String {
    match reason {
        CloseRefusalReason::NotReported | CloseRefusalReason::Busy => {
            format!("{detail}; pass --force to close it anyway")
        }
        _ => format!("{detail}; or pass --force"),
    }
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
        truncated: false,
        targets: Vec::new(),
    };
    // Before anything is resolved, and before the caller's claim costs a
    // registry walk: a selector this large is refused whole (auditor S4).
    if let Some(refusal) = selector_too_large(&selector) {
        tracing::info!(
            reason = refusal.reason.code(),
            "close: refused the whole request — the selector is too large"
        );
        report.refused = Some(refusal);
        return report;
    }
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
    // One budget for the whole report, charged as each nested entry is built
    // (auditor SF2).
    let mut budget = ReportBudget::new();
    let (resolved, refused, truncated) = resolve(&selector, &caller, registry, &mut budget);
    report.truncated = truncated;
    report.targets.extend(refused);
    for target in resolved {
        // Only the last attempt's entry is reported, so each attempt is
        // charged from the same point.
        let before = budget.clone();
        let mut entry = None;
        for _ in 0..REBIND_ATTEMPTS {
            budget = before.clone();
            match close_one(
                &target,
                &caller,
                force,
                dry_run,
                registry,
                state,
                event_tx,
                worktrees,
                &mut budget,
            )
            .await
            {
                Attempt::Done(done) => {
                    entry = Some(done);
                    break;
                }
                Attempt::Rebind(failed) => entry = Some(failed),
            }
        }
        report
            .targets
            .push(entry.expect("REBIND_ATTEMPTS is at least one"));
    }
    fit_deliverable(&mut report);
    report
}

/// What one attempt at closing a target came to.
enum Attempt {
    /// The target's report entry.
    Done(CloseTarget),
    /// The unit moved to a new generation while it was being closed, and
    /// nothing was stopped: try again against its generation now. Carries the
    /// entry to report if no attempt is left (auditor S2).
    Rebind(CloseTarget),
}

/// The member a respawn lifted out and that never came back: its record is
/// gone and no generation holds its pane. Its child was terminated by the
/// respawn, which the close's admission state then refused a replacement.
fn aborted_respawn_member(record: AgentRecord) -> Member {
    Member {
        record,
        exited: true,
    }
}

/// Whether any generation holds `pane_id` now, live or retired.
fn pane_held(pane_id: &str, registry: &AgentPtyRegistry) -> bool {
    registry.pane_current_agent_id(pane_id).is_some()
        || registry.agent_id_for_pane_any(pane_id).is_some()
}

/// What [`end_unit_if_nothing_remains`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndCheck {
    /// The unit ended here.
    Ended,
    /// The unit was not live any more: a stop ended it, or something else did.
    Gone,
    /// Something of the unit remains, so it stays live.
    Remains,
    /// The unit now names generations other than the ones this close held:
    /// it moved to a successor, which this close did not stop (auditor SF1).
    Moved,
}

/// PRD #1589 (auditor B1): end the unit `unit_id` only once nothing of it
/// remains — no record, no respawn in its window, and for an orchestration no
/// pane of its instance in the role maps either, the rule `stop_agent_steps`
/// ends a unit by. A close that stopped every member has normally ended the
/// unit already, through that seam; this covers a close with nothing left to
/// stop.
///
/// Judged against `held`, the generations the unit named when the close took
/// its holds, and only while the close's admission state still refuses a
/// respawn of them (auditor SF1): so no respawn can be admitted for them
/// during this check, and one admitted before it was waited for. The ledger's
/// binding is compared with `held` again under the same ledger lock that
/// ends the unit, so a unit whose binding moved anyway is reported
/// [`EndCheck::Moved`] rather than ended behind a live successor. An
/// orchestration's unit ends only once its instance is finished
/// ([`AgentPtyRegistry::seal_instance_if_empty`]), which is what keeps a fresh
/// role from joining it when this close held one generation, not the instance.
async fn end_unit_if_nothing_remains(
    unit_id: &str,
    held: &Scope,
    registry: &AgentPtyRegistry,
    state: &SharedState,
) -> EndCheck {
    match registry.dispatched_units().get(unit_id).map(scope_of_unit) {
        None => return EndCheck::Gone,
        Some(now) if now != *held => return EndCheck::Moved,
        Some(_) => {}
    }
    if !enumerate(held, registry).is_empty() || !respawning(held, registry).is_empty() {
        return EndCheck::Remains;
    }
    if let Scope::Instance(instance) = held {
        // No record of the instance is left and none is mid-respawn, so a
        // role still registered for a pane no generation holds is a dead
        // registration (a respawn whose replacement failed before this close,
        // say), not a member: a role that joins registers only after its
        // generation holds the pane. Take it down, so it neither routes a
        // delegate to nothing nor keeps the unit from ending.
        let mut state = state.write().await;
        let dead: Vec<String> = state
            .pane_orchestration_map
            .iter()
            .filter(|(pane, identity)| identity.id == *instance && !pane_held(pane, registry))
            .map(|(pane, _)| pane.clone())
            .collect();
        for pane in &dead {
            state.unregister_pane(pane);
        }
        let in_state = state
            .pane_orchestration_map
            .values()
            .any(|identity| identity.id == *instance);
        if in_state {
            return EndCheck::Remains;
        }
    }
    #[cfg(test)]
    registry.pause_point("close-end-check").await;
    // A `--pane` close of an orchestration's last role holds only that
    // generation's admission state, so a fresh role may still join the
    // instance. The unit ends only together with finishing the instance, in
    // one registry lock hold that sees a role published before it and refuses
    // one publishing after: an ended unit never sits beside a live role of its
    // instance, and a role that joined in time keeps the unit live.
    if let Scope::Instance(instance) = held
        && !registry.seal_instance_if_empty(instance)
    {
        return EndCheck::Remains;
    }
    let mut records = registry.dispatched_units();
    match records.get(unit_id).map(scope_of_unit) {
        None => EndCheck::Gone,
        Some(now) if now != *held => EndCheck::Moved,
        Some(_) => {
            records.end(
                unit_id,
                EndReason::ClosedByVerb,
                chrono::Utc::now().timestamp_millis(),
            );
            EndCheck::Ended
        }
    }
}

/// The Closing admission states one close attempt holds: the instance's for
/// an orchestration taken whole, the generation's for one agent (auditor
/// SF1). Dropping it re-admits spawns, except into an instance whose guard was
/// finished.
#[derive(Default)]
struct Gates {
    instance: Option<crate::agent_pty::InstanceCloseGuard>,
    generation: Option<crate::agent_pty::GenerationCloseGuard>,
}

/// Steps 3 to 6 for one target. Its report entry is charged to `budget`.
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
    budget: &mut ReportBudget,
) -> Attempt {
    // Where this entry's charges start: its panes are projected again after
    // the holds, and charged once.
    let budget_mark = budget.clone();
    // The generations the target covers now, and its unit as the ledger has
    // it now (auditor S2).
    let scope = current_scope(target, registry);
    let unit_then: Option<DispatchedUnit> = target.unit.as_ref().map(|u| {
        registry
            .dispatched_units()
            .get(&u.id)
            .cloned()
            .unwrap_or_else(|| u.clone())
    });
    // A member in its respawn window counts as one for authority, the
    // refusals and the listing: it is still part of the target (auditor B1).
    let mut members = enumerate(&scope, registry);
    members.extend(
        respawning(&scope, registry)
            .into_iter()
            .map(|record| Member {
                record,
                exited: false,
            }),
    );
    let (target_facts, statuses) =
        facts(target, unit_then.as_ref(), &members, state, registry).await;
    let (mut out, descendants_cut) = base_target(
        target,
        unit_then.as_ref(),
        &members,
        &statuses,
        registry,
        budget,
    );
    if let Err(refusal) = authorize_target(caller, target, &members) {
        refuse(&mut out, refusal.reason, refusal.message);
        return Attempt::Done(out);
    }
    let refusals = default_refusals(&target_facts);
    if dry_run {
        out.would_refuse = refusals.iter().map(|(r, _)| *r).collect();
        return Attempt::Done(out);
    }
    // Refusals already decidable are answered here, before any admission
    // state opens (auditor B1): an open one refuses respawns of the target's
    // members, so a close that was always going to refuse must not get that
    // far. They are checked again after the holds, for what changed since.
    if !caller_still_current(caller, registry) {
        refuse(
            &mut out,
            CloseRefusalReason::Superseded,
            "refused: the calling agent no longer holds its pane".to_string(),
        );
        return Attempt::Done(out);
    }
    if let Some((reason, detail)) = refusals.first().filter(|_| !force) {
        refuse(&mut out, *reason, refusal_message(*reason, detail));
        return Attempt::Done(out);
    }

    // Step 5a: the Closing admission state — for the unit, and for what the
    // target's scope covers: an orchestration instance taken whole, or one
    // generation (auditor SF1). Taken for a target with no records left as
    // much as for one with many (auditor B1): an empty snapshot is not proof
    // the target is gone.
    let unit_prior = match target.unit.as_ref() {
        Some(unit) if !target.by_pane => {
            match registry.dispatched_units().begin_closing(&unit.id) {
                Some(prior) => Some((unit.id.clone(), prior)),
                None => {
                    out.outcome = CloseOutcome::Failed;
                    out.error =
                        Some("another close of this unit is already in progress".to_string());
                    return Attempt::Done(out);
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
    let mut gates = Gates::default();
    match &scope {
        Scope::Instance(id) => match registry.begin_instance_close(id) {
            Some(guard) => gates.instance = Some(guard),
            None => {
                restore_unit(registry);
                out.outcome = CloseOutcome::Failed;
                out.error =
                    Some("another close of this orchestration is already in progress".to_string());
                return Attempt::Done(out);
            }
        },
        Scope::Generation(id) => match registry.begin_generation_close(id) {
            Some(guard) => gates.generation = Some(guard),
            None => {
                restore_unit(registry);
                out.outcome = CloseOutcome::Failed;
                out.error = Some("another close of this agent is already in progress".to_string());
                return Attempt::Done(out);
            }
        },
    }

    // Step 5a′: respawns admitted before the admission state opened (auditor
    // B1/B2/SF1). Read after it opened: from here on a respawn of a member is
    // refused before it touches the running generation, so this set only
    // shrinks. Each has terminated, or is terminating, its old child. Its
    // replacement is let through the admission state (only a finished close
    // refuses one), so once it has settled it is an ordinary member below:
    // stopped if this close goes ahead, left running if it refuses. A
    // replacement that failed for its own reasons leaves its pane empty, and
    // is reported as stopped.
    let pending = respawning(&scope, registry);
    let mut aborted: Vec<Member> = Vec::new();
    if !pending.is_empty() {
        let ids: Vec<String> = pending.iter().map(|r| r.id.clone()).collect();
        if !registry
            .wait_respawns_settled(&ids, crate::agent_pty::RESPAWN_SETTLE_TIMEOUT)
            .await
        {
            drop(gates);
            restore_unit(registry);
            out.outcome = CloseOutcome::Failed;
            out.error = Some(
                "this agent is still being replaced in its pane; nothing was closed".to_string(),
            );
            return Attempt::Done(out);
        }
        let replaced = pending.iter().any(|record| {
            record
                .pane_id_env
                .as_deref()
                .is_some_and(|pane| pane_held(pane, registry))
        });
        if matches!(scope, Scope::Generation(_)) && replaced {
            // One generation's replacement is a new generation, outside this
            // scope: follow it if the unit did, and otherwise say so rather
            // than report a close of nothing while it runs (auditor SF1).
            drop(gates);
            restore_unit(registry);
            out.outcome = CloseOutcome::Failed;
            out.error = Some(
                "the agent was replaced in its pane while it was being closed; nothing was \
                 closed — close it again"
                    .to_string(),
            );
            if current_scope(target, registry) != scope {
                return Attempt::Rebind(out);
            }
            return Attempt::Done(out);
        }
        aborted = pending
            .into_iter()
            .filter(|record| {
                record
                    .pane_id_env
                    .as_deref()
                    .is_none_or(|pane| !pane_held(pane, registry))
            })
            .map(aborted_respawn_member)
            .collect();
    }

    // Step 5b: a cleanup hold on every member generation, re-enumerating until
    // the membership is stable. New members are refused from here on, so this
    // converges; one that reserved before the close began is seen and held.
    let mut holds: Vec<(String, crate::agent_pty::PaneCleanupHold)> = Vec::new();
    let mut held: HashSet<String> = HashSet::new();
    #[cfg(test)]
    registry.pause_point("close-holds").await;
    let mut members = enumerate(&scope, registry);
    let mut stable = false;
    for _ in 0..MEMBERSHIP_ROUNDS {
        let mut preflight_error = None;
        for member in &members {
            if !held.insert(member.record.id.clone()) {
                continue;
            }
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
            drop(gates);
            restore_unit(registry);
            out.outcome = CloseOutcome::Failed;
            out.error = Some(error);
            return Attempt::Done(out);
        }
        let again = enumerate(&scope, registry);
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
        drop(gates);
        restore_unit(registry);
        out.outcome = CloseOutcome::Failed;
        out.error = Some("the orchestration kept changing while it was being closed".to_string());
        return Attempt::Done(out);
    }
    #[cfg(test)]
    registry.before_close_revalidation().await;

    // Step 5c: re-validate everything decided before the holds — for a target
    // whose members are all gone as much as for any other (auditor B1).
    let unit_now = target
        .unit
        .as_ref()
        .and_then(|u| registry.dispatched_units().get(&u.id).cloned());
    let (now_facts, now_statuses) =
        facts(target, unit_now.as_ref(), &members, state, registry).await;
    // The report's panes again, for the membership now held — charged from
    // where this entry started, then its descendants re-charged after them.
    // At most `MAX_LISTED_TARGETS` are listed; every member is stopped
    // (auditor SF2).
    *budget = budget_mark;
    let (panes, panes_cut) = project_panes(
        members
            .iter()
            .zip(now_statuses.iter().map(Option::as_ref))
            .chain(aborted.iter().map(|m| (m, None))),
        budget,
    );
    out.panes = panes;
    let refit_cut = budget.fit(&mut out.open_descendants, descendant_cost);
    out.truncated = panes_cut || descendants_cut || refit_cut;
    let release = |holds, gates, registry: &AgentPtyRegistry| {
        drop::<Vec<(String, crate::agent_pty::PaneCleanupHold)>>(holds);
        drop::<Gates>(gates);
        restore_unit(registry);
    };
    // The unit moved to a successor generation since the scope was read: what
    // was held is not the unit any more. Nothing was stopped; retry against
    // the generation it has now (auditor S2).
    if current_scope(target, registry) != scope {
        release(holds, gates, registry);
        out.outcome = CloseOutcome::Failed;
        out.error = Some(
            "the agent was replaced in its pane while it was being closed; nothing was closed \
             — close it again"
                .to_string(),
        );
        return Attempt::Rebind(out);
    }
    if !caller_still_current(caller, registry) {
        release(holds, gates, registry);
        refuse(
            &mut out,
            CloseRefusalReason::Superseded,
            "refused: the calling agent no longer holds its pane".to_string(),
        );
        return Attempt::Done(out);
    }
    let all_members: Vec<Member> = members.iter().chain(aborted.iter()).cloned().collect();
    if let Err(refusal) = authorize_target(caller, target, &all_members) {
        release(holds, gates, registry);
        refuse(&mut out, refusal.reason, refusal.message);
        return Attempt::Done(out);
    }
    let refusals = default_refusals(&now_facts);
    if let Some((reason, detail)) = refusals.first().filter(|_| !force) {
        release(holds, gates, registry);
        refuse(&mut out, *reason, refusal_message(*reason, detail));
        return Attempt::Done(out);
    }
    out.forced_over = refusals.iter().map(|(r, _)| *r).collect();

    // Step 5d: disclose, #1109-style — one line, naming the caller, the
    // selector and every pane, and at `warn!` with what was overridden when
    // forced.
    let lines: Vec<String> = all_members
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
        // The report may list fewer panes than were stopped.
        if let Some(pane) = out.panes.get_mut(index) {
            pane.stopped = Some(result.is_ok());
        }
        #[cfg(test)]
        if index == 0 {
            registry.pause_point("close-after-first-stop").await;
        }
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
    // A respawn whose replacement failed left its pane with no agent and its
    // role still registered: take the registration down the way a stop does,
    // and tell the clients the pane is gone.
    for (offset, member) in aborted.iter().enumerate() {
        if let Some(pane) = member.record.pane_id_env.as_deref()
            && !pane_held(pane, registry)
        {
            state.write().await.unregister_pane(pane);
            crate::spawn::surface_attach_stopped_agent(event_tx, &member.record, pane);
        }
        if let Some(pane) = out.panes.get_mut(members.len() + offset) {
            pane.stopped = Some(true);
        }
    }
    let stopped_any = survivors.len() < members.len() + aborted.len();
    if survivors.is_empty() {
        out.outcome = CloseOutcome::Closed;
        if let Some(guard) = gates.instance.as_mut() {
            guard.finish();
        }
        // Judged while `gates` still refuses a respawn of what was held
        // (auditor SF1).
        if let Some(unit) = unit_now.as_ref()
            && end_unit_if_nothing_remains(&unit.id, &scope_of_unit(unit), registry, state).await
                == EndCheck::Moved
        {
            out.outcome = if stopped_any {
                CloseOutcome::PartiallyClosed
            } else {
                CloseOutcome::Failed
            };
            out.error = Some(
                "the unit moved to a new agent while it was being closed, and that agent is still running — close it again"
                    .to_string(),
            );
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

    // Step 6: the worktree, once this close freed it — decided by what the
    // stop left, never by the selector that reached it (auditor S3). A unit's
    // worktree is freed when the unit ended, which a single agent closed by
    // `--pane` does and one role of a live orchestration does not; the
    // removal's own directory hold still answers still-in-use if anything
    // else is rooted there. A target no unit owns gets a verdict only for a
    // worktree the deck recorded and nothing else is using.
    if out.outcome == CloseOutcome::Closed {
        let worktree = target
            .unit
            .as_ref()
            .map(|u| u.worktree.clone())
            .or_else(|| {
                members
                    .first()
                    .and_then(|m| crate::issue_dispatch_run::worktree_of_record(&m.record))
            });
        let freed = match (target.unit.as_ref(), worktree.as_ref()) {
            (Some(unit), Some(_)) => registry.dispatched_units().get(&unit.id).is_none(),
            (None, Some(worktree)) => {
                // The guard is a temporary of this statement alone, so it is
                // released before the registry's own lock is taken and before
                // any await (review on PR #1681).
                let recorded = worktrees
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(worktree);
                recorded && !registry.dir_in_use(worktree)
            }
            (_, None) => false,
        };
        if freed && let Some(worktree) = worktree {
            out.worktree_verdict =
                Some(cleanup_worktree(worktree, registry, worktrees, event_tx).await);
        }
    }
    drop(gates);
    Attempt::Done(out)
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
        event_tx: broadcast::Sender<BroadcastMsg>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Deck {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let sock = dir.path().join("attach.sock");
            let registry = Arc::new(AgentPtyRegistry::new());
            let (event_tx, _rx) = broadcast::channel(1024);
            let state: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            let worktrees = crate::issue_dispatch_run::new_worktree_registry();
            let listener = bind_attach_listener(&sock).expect("bind");
            let server = {
                let registry = registry.clone();
                let state = state.clone();
                let worktrees = worktrees.clone();
                let event_tx = event_tx.clone();
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
                event_tx,
                server,
            }
        }

        /// A second client on the same socket, for a request run alongside
        /// another.
        fn other_client(&self) -> DaemonClient {
            DaemonClient::new(self.dir.path().join("attach.sock"))
        }

        async fn start(
            &self,
            pane: &str,
            command: &str,
            orch: Option<(&str, &str, bool)>,
        ) -> String {
            self.start_in(pane, command, orch, self.dir.path()).await
        }

        async fn start_in(
            &self,
            pane: &str,
            command: &str,
            orch: Option<(&str, &str, bool)>,
            cwd: &std::path::Path,
        ) -> String {
            let cwd = cwd.to_string_lossy().into_owned();
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
        // The successor exits too (auditor N1). The predecessor's record is
        // still registered, so a fallback to "the pane's last occupant" would
        // hand the unit back to it; it must stay refused, even with --force.
        deck.client
            .stop_agent(&new)
            .await
            .expect("stop the successor");
        assert!(!deck.live(&new), "the successor is gone");
        assert!(
            deck.registry.agent_record_any(&old).is_some(),
            "the predecessor's record is retained, so the refusal below is not \
             merely a missing record"
        );
        let report = deck
            .close(by_name("u1"), Some(old_claim), true, false)
            .await;
        assert_eq!(
            report.refused.map(|r| r.reason),
            Some(CloseRefusalReason::Superseded)
        );
        assert!(report.targets.is_empty(), "nothing is resolved or stopped");
        assert!(deck.live(&unit_agent));
        assert!(
            deck.registry
                .dispatched_units()
                .live()
                .any(|u| u.name == "u1"),
            "the unit stays live"
        );
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

    /// A dispatched orchestration `orch` with a live orchestrator and worker,
    /// registered as unit `name`. Returns (orchestrator, worker, unit id).
    async fn orchestration(
        deck: &Deck,
        name: &str,
        orch: &str,
        dispatcher: (&str, &str),
    ) -> (String, String, String) {
        let o = deck
            .start(
                &format!("{orch}-o"),
                "sleep 30",
                Some((orch, "orchestrator", true)),
            )
            .await;
        let w = deck
            .start(
                &format!("{orch}-w"),
                "sleep 30",
                Some((orch, "coder", false)),
            )
            .await;
        let id = deck.orch_unit(name, dispatcher, orch, &format!("{orch}-o"), &o);
        (o, w, id)
    }

    /// Start a respawn of `pane` that pauses once it has lifted the old record
    /// out, and wait until it has.
    async fn paused_respawn(
        deck: &Deck,
        pane: &str,
    ) -> (
        tokio::task::JoinHandle<Result<String, crate::agent_pty::AgentPtyError>>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, release) = deck.registry.pause_next_respawn_for_test();
        let registry = deck.registry.clone();
        let pane = pane.to_string();
        let respawn =
            tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "sleep 30").await });
        reached.await.expect("the respawn reaches its window");
        (respawn, release)
    }

    /// A close run in the background, through a second client.
    fn close_in_background(
        deck: &Deck,
        selector: CloseSelector,
        caller: Option<CallerClaim>,
        force: bool,
    ) -> tokio::task::JoinHandle<CloseReport> {
        let client = deck.other_client();
        tokio::spawn(async move {
            client
                .close_agents(selector, caller, force, false)
                .await
                .expect("a report")
        })
    }

    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(tokio::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn mapped_in_state(state: &crate::state::AppState, pane: &str) -> bool {
        state.pane_orchestration_map.contains_key(pane)
    }

    /// How many "this pane was closed" removals were broadcast for `pane`
    /// since `events` subscribed.
    fn pane_closed_broadcasts(events: &mut broadcast::Receiver<BroadcastMsg>, pane: &str) -> usize {
        let mut count = 0;
        while let Ok(msg) = events.try_recv() {
            if let BroadcastMsg::Event(event) = msg
                && event.pane_id.as_deref() == Some(pane)
                && event
                    .metadata
                    .contains_key(crate::event::DAEMON_PANE_CLOSED_METADATA_KEY)
            {
                count += 1;
            }
        }
        count
    }

    /// Scenario: auditor B1 — an orchestration's orchestrator was stopped
    /// first, and its only remaining worker is mid-respawn, so the registry
    /// holds no record of the unit at all. Closing it by name waits for the
    /// respawn instead of reading the empty snapshot as "gone"; the respawn's
    /// replacement lands and is stopped as a member, the report names the
    /// worker's pane as stopped, its role registration is taken down, and no
    /// agent comes back in the pane behind the ended unit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_unit_whose_last_worker_is_mid_respawn_does_not_resurrect() {
        let deck = Deck::new().await;
        let (o, _w, id) = orchestration(&deck, "team-b1", "orch-b1", ("disp", "nobody")).await;
        deck.report_done("orch-b1-o", &o);
        deck.client
            .stop_agent(&o)
            .await
            .expect("stop the orchestrator");
        let (respawn, release) = paused_respawn(&deck, "orch-b1-w").await;
        assert!(enumerate(&Scope::Instance("orch-b1".into()), &deck.registry).is_empty());

        let close = close_in_background(&deck, by_name("team-b1"), None, false);
        wait_until("the close opens the instance's admission state", || {
            deck.registry.is_instance_closing("orch-b1")
        })
        .await;
        let _ = release.send(());
        let replaced = respawn
            .await
            .unwrap()
            .expect("an admitted respawn's replacement is let through");
        let report = close.await.unwrap();
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
        assert_eq!(target.panes.len(), 1, "{target:?}");
        assert_eq!(target.panes[0].pane_id.as_deref(), Some("orch-b1-w"));
        assert_eq!(target.panes[0].agent_id, replaced);
        assert_eq!(target.panes[0].stopped, Some(true));
        assert!(!deck.live(&replaced));
        assert!(deck.registry.pane_current_agent_id("orch-b1-w").is_none());
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        assert!(!mapped_in_state(&*deck.state.read().await, "orch-b1-w"));
        // A queued respawn after the close cannot bring it back either.
        assert!(deck.registry.is_instance_closing("orch-b1"));
        deck.shutdown().await;
    }

    /// Scenario: auditor B1 — the same empty-snapshot unit, never reported,
    /// while its worker is mid-respawn. A close without --force is refused
    /// not-reported before it opens the instance's admission state, so the
    /// worker's replacement lands and keeps running in its pane, still
    /// registered, with no removal announced; --force then closes the unit,
    /// replacement included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_snapshot_unit_still_meets_the_default_refusals() {
        let deck = Deck::new().await;
        let (o, _w, id) = orchestration(&deck, "team-b1r", "orch-b1r", ("disp", "nobody")).await;
        deck.client
            .stop_agent(&o)
            .await
            .expect("stop the orchestrator");
        let mut events = deck.event_tx.subscribe();
        let (respawn, release) = paused_respawn(&deck, "orch-b1r-w").await;
        let report = deck.close(by_name("team-b1r"), None, false, false).await;
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::NotReported),
            "{:?}",
            report.targets[0]
        );
        assert!(
            !deck.registry.is_instance_closing("orch-b1r"),
            "a close that refuses never opens the admission state"
        );
        let _ = release.send(());
        let replaced = respawn
            .await
            .unwrap()
            .expect("the worker's replacement lands");
        assert!(deck.live(&replaced));
        assert_eq!(
            deck.registry.pane_current_agent_id("orch-b1r-w").as_deref(),
            Some(replaced.as_str())
        );
        assert!(mapped_in_state(&*deck.state.read().await, "orch-b1r-w"));
        assert!(
            deck.registry.dispatched_units().get(&id).is_some(),
            "a refused unit is not tombstoned"
        );
        assert_eq!(pane_closed_broadcasts(&mut events, "orch-b1r-w"), 0);

        let report = deck.close(by_name("team-b1r"), None, true, false).await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        assert!(!deck.live(&replaced));
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        assert!(!mapped_in_state(&*deck.state.read().await, "orch-b1r-w"));
        deck.shutdown().await;
    }

    /// Scenario: auditor B1 — while a close waits for the unit's respawning
    /// worker, the dispatcher that asked is replaced in its pane. The close is
    /// refused superseded once it re-validates, the unit is not ended, and the
    /// worker's replacement keeps running in its pane with no removal
    /// announced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_superseded_while_a_respawn_settles_is_refused() {
        let deck = Deck::new().await;
        let dispatcher = deck.start("disp", "sleep 30", None).await;
        let (o, _w, id) = orchestration(&deck, "team-b1s", "orch-b1s", ("disp", &dispatcher)).await;
        deck.report_done("orch-b1s-o", &o);
        deck.client
            .stop_agent(&o)
            .await
            .expect("stop the orchestrator");
        let claim = deck.claim("disp", &dispatcher);
        let mut events = deck.event_tx.subscribe();
        let (respawn, release) = paused_respawn(&deck, "orch-b1s-w").await;
        let close = close_in_background(&deck, by_name("team-b1s"), Some(claim), false);
        wait_until("the close opens the instance's admission state", || {
            deck.registry.is_instance_closing("orch-b1s")
        })
        .await;
        deck.client
            .stop_agent(&dispatcher)
            .await
            .expect("stop the dispatcher");
        deck.start("disp", "sleep 30", None).await;
        let _ = release.send(());
        let replaced = respawn
            .await
            .unwrap()
            .expect("the worker's replacement lands");
        let report = close.await.unwrap();
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::Superseded),
            "{:?}",
            report.targets[0]
        );
        assert!(deck.registry.dispatched_units().get(&id).is_some());
        assert!(deck.live(&replaced), "the refused close stopped nothing");
        assert_eq!(
            deck.registry.pane_current_agent_id("orch-b1s-w").as_deref(),
            Some(replaced.as_str())
        );
        assert!(mapped_in_state(&*deck.state.read().await, "orch-b1s-w"));
        assert!(!deck.registry.is_instance_closing("orch-b1s"));
        assert_eq!(pane_closed_broadcasts(&mut events, "orch-b1s-w"), 0);
        deck.shutdown().await;
    }

    /// Scenario: auditor B2 — while a close holds an instance, a respawn of
    /// its healthy worker is refused before it touches the running agent: the
    /// worker keeps its generation and its record, the recovering wrapper is
    /// refused the same way, and a worker of another instance still respawns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_respawn_into_a_closing_instance_leaves_the_worker_running() {
        let deck = Deck::new().await;
        let (_o, w, _id) = orchestration(&deck, "team-b2", "orch-b2", ("disp", "nobody")).await;
        let other_dir = deck.dir.path().join("other");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other = deck
            .start_in(
                "other-w",
                "sleep 30",
                Some(("orch-other", "coder", false)),
                &other_dir,
            )
            .await;
        let guard = deck
            .registry
            .begin_instance_close("orch-b2")
            .expect("begins");
        let err = deck
            .registry
            .respawn_agent_for_pane("orch-b2-w", "sleep 30")
            .await
            .expect_err("refused");
        assert!(
            err.to_string()
                .contains(crate::agent_pty::INSTANCE_CLOSING_REASON),
            "{err}"
        );
        assert!(deck.live(&w), "the original generation is still running");
        assert_eq!(
            deck.registry.pane_current_agent_id("orch-b2-w").as_deref(),
            Some(w.as_str())
        );
        let identity = crate::agent_pty::PaneRecreateIdentity {
            cwd: Some(deck.dir.path().to_string_lossy().into_owned()),
            display_name: Some("coder".into()),
            tab_membership: deck.registry.agent_record_any(&w).unwrap().tab_membership,
            agent_type: None,
            env: Vec::new(),
        };
        let started = tokio::time::Instant::now();
        deck.registry
            .respawn_or_recreate_agent_for_pane("orch-b2-w", "sleep 30", &identity)
            .await
            .expect_err("the recovering wrapper is refused too");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(deck.live(&w));
        let replaced = deck
            .registry
            .respawn_agent_for_pane("other-w", "sleep 30")
            .await
            .expect("another instance still respawns");
        assert_ne!(replaced, other);
        drop(guard);
        deck.shutdown().await;
    }

    /// Scenario: auditor B2 — a worker's respawn is admitted just before a
    /// close of its unit begins. The close waits for it, stops the replacement
    /// as a member, and the report accounts for the worker's pane as stopped
    /// beside the orchestrator; nothing is left running in the unit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_respawn_admitted_before_the_close_is_in_its_report() {
        let deck = Deck::new().await;
        let (o, _w, id) = orchestration(&deck, "team-b2a", "orch-b2a", ("disp", "nobody")).await;
        deck.report_done("orch-b2a-o", &o);
        let (respawn, release) = paused_respawn(&deck, "orch-b2a-w").await;
        let close = close_in_background(&deck, by_name("team-b2a"), None, false);
        wait_until("the close opens the instance's admission state", || {
            deck.registry.is_instance_closing("orch-b2a")
        })
        .await;
        let _ = release.send(());
        let _ = respawn.await.unwrap();
        let report = close.await.unwrap();
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
        let panes: Vec<(Option<&str>, Option<bool>)> = target
            .panes
            .iter()
            .map(|p| (p.pane_id.as_deref(), p.stopped))
            .collect();
        assert_eq!(
            panes,
            vec![
                (Some("orch-b2a-o"), Some(true)),
                (Some("orch-b2a-w"), Some(true))
            ]
        );
        assert!(!deck.live(&o));
        assert!(deck.registry.pane_current_agent_id("orch-b2a-w").is_none());
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        deck.shutdown().await;
    }

    /// Scenario: auditor S1 — a role's start has published its generation and
    /// is about to register it when a close of its orchestration takes it
    /// down. When the start resumes it registers no role and announces no
    /// card for the closed pane.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_resuming_after_its_close_registers_nothing() {
        let deck = Deck::new().await;
        let o = deck
            .start("s1-o", "sleep 30", Some(("orch-s1", "orchestrator", true)))
            .await;
        deck.orch_unit("team-s1", ("disp", "nobody"), "orch-s1", "s1-o", &o);
        deck.report_done("s1-o", &o);
        let mut events = deck.event_tx.subscribe();
        let (reached, release) = deck.registry.pause_next_role_registration_for_test();
        let client = deck.other_client();
        let cwd = deck.dir.path().to_string_lossy().into_owned();
        let start = tokio::spawn(async move {
            client
                .start_agent(StartAgentOptions {
                    command: Some("sleep 30".to_string()),
                    cwd: Some(cwd.clone()),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "s1-w".to_string())],
                    tab_membership: Some(TabMembership::Orchestration {
                        name: "team".to_string(),
                        role_index: 1,
                        role_name: "coder".to_string(),
                        is_start_role: false,
                        orchestration_cwd: Some(cwd),
                        display_title: None,
                        orchestration_id: Some("orch-s1".to_string()),
                    }),
                    ..StartAgentOptions::default()
                })
                .await
        });
        reached.await.expect("the start publishes and pauses");
        let worker = deck
            .registry
            .pane_current_agent_id("s1-w")
            .expect("published");
        let report = deck.close(by_name("team-s1"), None, false, false).await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        assert!(
            report.targets[0]
                .panes
                .iter()
                .any(|p| p.agent_id == worker && p.stopped == Some(true))
        );
        let _ = release.send(());
        let _ = start.await.unwrap();
        assert!(!mapped_in_state(&*deck.state.read().await, "s1-w"));
        assert!(!deck.live(&worker));
        while let Ok(msg) = events.try_recv() {
            if let BroadcastMsg::OrchestrationSurface(surface) = msg {
                assert!(
                    surface.roles.iter().all(|r| r.pane_id != "s1-w"),
                    "a card was announced for the closed pane: {surface:?}"
                );
            }
        }
        deck.shutdown().await;
    }

    /// Scenario: D11's after-holds window — a dispatcher's close has taken its
    /// holds when the dispatcher is replaced in its pane. Re-validation refuses
    /// it superseded, and the unit's agent is left running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_superseded_after_the_holds_is_refused() {
        let deck = Deck::new().await;
        let dispatcher = deck.start("disp", "sleep 30", None).await;
        let unit_agent = deck.start("unit", "sleep 30", None).await;
        let id = deck.single_unit("u1", ("disp", &dispatcher), "unit", &unit_agent);
        deck.report_done("unit", &unit_agent);
        let claim = deck.claim("disp", &dispatcher);
        let (reached, release) = deck.registry.pause_next_close_revalidation_for_test();
        let close = close_in_background(&deck, by_name("u1"), Some(claim), false);
        reached.await.expect("the close takes its holds");
        deck.client
            .stop_agent(&dispatcher)
            .await
            .expect("stop the dispatcher");
        deck.start("disp", "sleep 30", None).await;
        let _ = release.send(());
        let report = close.await.unwrap();
        assert_eq!(
            report.targets[0].reason,
            Some(CloseRefusalReason::Superseded),
            "{:?}",
            report.targets[0]
        );
        assert!(deck.live(&unit_agent));
        assert!(deck.registry.dispatched_units().get(&id).is_some());
        deck.shutdown().await;
    }

    /// Scenario: auditor S2 — `close --all` lists and stops nothing, so does
    /// `--all --yes --dry-run`, and `--all --yes` previews and then closes
    /// exactly the units its preview listed, by id.
    #[tokio::test]
    async fn all_yes_previews_then_applies_exactly_the_listed_ids() {
        let deck = Deck::new().await;
        let a = deck.start("ua", "sleep 30", None).await;
        let b = deck.start("ub", "sleep 30", None).await;
        let ida = deck.single_unit("ua", ("disp", "nobody"), "ua", &a);
        let idb = deck.single_unit("ub", ("disp", "nobody"), "ub", &b);
        deck.report_done("ua", &a);
        deck.report_done("ub", &b);
        let person = crate::close_cli::AmbientIdentity::Person;
        let all = crate::close_cli::CloseArgs {
            all: true,
            json: true,
            ..Default::default()
        };
        let out = crate::close_cli::run_close(&deck.client, &all, &person).await;
        assert_eq!(out.code, crate::close_cli::EXIT_OK, "{out:?}");
        assert!(deck.live(&a) && deck.live(&b));
        let dry = crate::close_cli::CloseArgs {
            yes: true,
            dry_run: true,
            ..all.clone()
        };
        let out = crate::close_cli::run_close(&deck.client, &dry, &person).await;
        assert_eq!(out.code, crate::close_cli::EXIT_OK, "{out:?}");
        assert!(deck.live(&a) && deck.live(&b));

        let yes = crate::close_cli::CloseArgs {
            yes: true,
            ..all.clone()
        };
        let out = crate::close_cli::run_close(&deck.client, &yes, &person).await;
        assert_eq!(out.code, crate::close_cli::EXIT_OK, "{out:?}");
        let json: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
        let ids = |v: &serde_json::Value| {
            let mut ids: Vec<String> = v
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["unit_id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        let mut expected = vec![ida, idb];
        expected.sort();
        assert_eq!(ids(&json["preview"]["listed"]), expected);
        assert_eq!(ids(&json["closed"]), expected);
        assert!(!deck.live(&a) && !deck.live(&b));
        deck.shutdown().await;
    }

    /// Scenario: one name matches two live units, and the refusal lists both
    /// ids. Closing by one of those ids with `--unit-id` closes exactly that
    /// unit and leaves the other running.
    #[tokio::test]
    async fn an_ambiguous_names_candidate_id_closes_exactly_that_unit() {
        let deck = Deck::new().await;
        let a = deck.start("unit-a", "sleep 30", None).await;
        let b = deck.start("unit-b", "sleep 30", None).await;
        deck.single_unit("fix", ("disp", "nobody"), "unit-a", &a);
        deck.single_unit("fix", ("disp", "nobody"), "unit-b", &b);
        deck.report_done("unit-a", &a);
        deck.report_done("unit-b", &b);
        let report = deck.close(by_name("fix"), None, false, false).await;
        let candidate = report.targets[0].candidates[0].unit_id.clone();
        let args = crate::close_cli::CloseArgs {
            unit_ids: vec![candidate.clone()],
            ..Default::default()
        };
        let out = crate::close_cli::run_close(
            &deck.client,
            &args,
            &crate::close_cli::AmbientIdentity::Person,
        )
        .await;
        assert_eq!(out.code, crate::close_cli::EXIT_OK, "{out:?}");
        assert!(out.stdout.contains(&candidate), "{}", out.stdout);
        assert!(!deck.live(&a));
        assert!(deck.live(&b), "the other candidate is untouched");
        deck.shutdown().await;
    }

    /// Scenario: auditor S4 — a selector naming more ids than a request may,
    /// or one entry far longer than any name, is refused whole and quickly: no
    /// target is resolved, nothing is stopped, and the report stays small.
    #[tokio::test]
    async fn an_oversized_selector_is_refused_whole() {
        let deck = Deck::new().await;
        let unit_agent = deck.start("unit", "sleep 30", None).await;
        let id = deck.single_unit("u1", ("disp", "nobody"), "unit", &unit_agent);
        deck.report_done("unit", &unit_agent);
        let mut ids: Vec<String> = (0..MAX_SELECTOR_ENTRIES + 1)
            .map(|n| format!("u-unknown-{n}"))
            .collect();
        ids.push(id.clone());
        let started = std::time::Instant::now();
        for selector in [
            CloseSelector::UnitIds { ids },
            CloseSelector::Units(UnitSelector {
                names: vec!["u1".into(), "x".repeat(MAX_SELECTOR_ENTRY_BYTES + 1)],
            }),
        ] {
            let report = deck.close(selector, None, true, false).await;
            assert_eq!(
                report.refused.as_ref().map(|r| r.reason),
                Some(CloseRefusalReason::SelectorTooLarge)
            );
            assert!(report.targets.is_empty());
            assert!(serde_json::to_string(&report).unwrap().len() < 1024);
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(deck.live(&unit_agent), "nothing is stopped");
        assert!(deck.registry.dispatched_units().get(&id).is_some());
        assert_eq!(
            CloseRefusalReason::SelectorTooLarge.code(),
            "selector-too-large"
        );
        deck.shutdown().await;
    }

    /// Scenario: PRD #1589 D5 — the orchestrator a unit's task went to is
    /// replaced in its pane by a registry respawn. The replacement's
    /// completion marks the unit; the old generation's no longer would.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_respawned_terminal_generation_still_completes_its_unit() {
        let deck = Deck::new().await;
        let (o, _w, id) = orchestration(&deck, "team-d5", "orch-d5", ("disp", "nobody")).await;
        let new = deck
            .registry
            .respawn_agent_for_pane("orch-d5-o", "sleep 30")
            .await
            .expect("respawn the orchestrator");
        {
            let mut units = deck.registry.dispatched_units();
            assert_eq!(units.mark_completed("orch-d5-o", &o, 2), None);
            assert_eq!(units.mark_completed("orch-d5-o", &new, 2), Some(id));
        }
        deck.shutdown().await;
    }

    /// A dispatched single unit whose agent runs in a real, clean worktree
    /// the daemon recorded, as `dispatch` leaves one.
    async fn single_in_worktree(deck: &Deck, name: &str) -> (String, String, PathBuf) {
        let repo = deck.dir.path().join(format!("repo-{name}"));
        let wt = deck.dir.path().join(format!("repo-{name}-dispatch"));
        init_repo_with_worktree(deck.dir.path(), &repo, &wt);
        crate::issue_dispatch_run::record_worktree(
            &deck.worktrees,
            &wt,
            &repo,
            crate::issue_dispatch_run::RemovalPolicy::KeepIfDirty,
        );
        let agent = deck.start_in(name, "sleep 30", None, &wt).await;
        let id = deck.registry.dispatched_units().register(NewUnit {
            name: name.to_string(),
            worktree: wt.clone(),
            branch: "wt".to_string(),
            clone_dir: repo,
            dispatcher: Dispatcher {
                pane_id: "disp".into(),
                agent_id: "nobody".into(),
            },
            kind: UnitKind::Single {
                pane_id: name.to_string(),
                agent_id: agent.clone(),
            },
            dispatched_at_ms: 1,
        });
        deck.report_done(name, &agent);
        (agent, id, wt)
    }

    /// Scenario: auditor S3 / reviewer SF1 — a dispatched single unit closed
    /// by `--pane` ends the unit and removes its clean worktree with a verdict,
    /// exactly as closing it by name does.
    #[tokio::test]
    async fn a_single_unit_closed_by_pane_cleans_its_worktree() {
        let deck = Deck::new().await;
        let (agent, id, wt) = single_in_worktree(&deck, "solo").await;
        let report = deck
            .close(
                CloseSelector::Pane {
                    pane_id: "solo".into(),
                },
                None,
                false,
                false,
            )
            .await;
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
        assert_eq!(
            target.worktree_verdict.as_ref().map(|v| v.verdict),
            Some(WorktreeVerdictKind::Removed),
            "{target:?}"
        );
        assert!(!wt.exists());
        assert!(!deck.live(&agent));
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        deck.shutdown().await;
    }

    /// Scenario: auditor S3 — an orchestration's roles share one worktree.
    /// Closing one role by `--pane --force` while its sibling still runs there
    /// removes nothing and reports no verdict; closing the last role by
    /// `--pane` ends the unit and removes the worktree.
    #[tokio::test]
    async fn the_last_role_closed_by_pane_cleans_the_shared_worktree() {
        let deck = Deck::new().await;
        let repo = deck.dir.path().join("repo-team");
        let wt = deck.dir.path().join("repo-team-dispatch");
        init_repo_with_worktree(deck.dir.path(), &repo, &wt);
        crate::issue_dispatch_run::record_worktree(
            &deck.worktrees,
            &wt,
            &repo,
            crate::issue_dispatch_run::RemovalPolicy::KeepIfDirty,
        );
        let o = deck
            .start_in(
                "t-o",
                "sleep 30",
                Some(("orch-s3", "orchestrator", true)),
                &wt,
            )
            .await;
        let w = deck
            .start_in("t-w", "sleep 30", Some(("orch-s3", "coder", false)), &wt)
            .await;
        let id = deck.registry.dispatched_units().register(NewUnit {
            name: "team-s3".into(),
            worktree: wt.clone(),
            branch: "wt".into(),
            clone_dir: repo,
            dispatcher: Dispatcher {
                pane_id: "disp".into(),
                agent_id: "nobody".into(),
            },
            kind: UnitKind::Orchestration {
                orchestration_id: "orch-s3".into(),
                name: "team".into(),
                terminal_pane_id: "t-o".into(),
                terminal_agent_id: o.clone(),
            },
            dispatched_at_ms: 1,
        });
        deck.report_done("t-o", &o);
        let pane = |p: &str| CloseSelector::Pane { pane_id: p.into() };

        let report = deck.close(pane("t-o"), None, true, false).await;
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
        assert_eq!(
            target.forced_over,
            vec![CloseRefusalReason::StrandsOrchestration]
        );
        assert_eq!(target.worktree_verdict, None, "a sibling still uses it");
        assert!(wt.exists());
        assert!(deck.registry.dispatched_units().get(&id).is_some());

        let report = deck.close(pane("t-w"), None, false, false).await;
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
        assert_eq!(
            target.worktree_verdict.as_ref().map(|v| v.verdict),
            Some(WorktreeVerdictKind::Removed),
            "{target:?}"
        );
        assert!(!wt.exists());
        assert!(!deck.live(&w));
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        deck.shutdown().await;
    }

    /// Scenario: review on PR #1681 — an orchestration's only worker is
    /// restarting, paused in its respawn window, so the registry's records
    /// hold the orchestrator alone. Closing the idle orchestrator of the
    /// reported unit by `--pane` without `--force` is still refused as
    /// stranding the team: the orchestrator keeps running, and the worker's
    /// replacement lands once the respawn is let through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pane_close_counts_a_restarting_teammate_as_stranded() {
        let deck = Deck::new().await;
        let o = deck
            .start("rs-o", "sleep 30", Some(("orch-rs", "orchestrator", true)))
            .await;
        let w = deck
            .start("rs-w", "sleep 30", Some(("orch-rs", "coder", false)))
            .await;
        let id = deck.orch_unit("team-rs", ("disp", "nobody"), "orch-rs", "rs-o", &o);
        deck.report_done("rs-o", &o);
        let (respawn, release) = paused_respawn(&deck, "rs-w").await;
        assert!(
            deck.registry.agent_records().iter().all(|r| r.id != w),
            "the worker is lifted out of the records in its respawn window"
        );

        let report = deck
            .close(
                CloseSelector::Pane {
                    pane_id: "rs-o".into(),
                },
                None,
                false,
                false,
            )
            .await;
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Refused, "{target:?}");
        assert_eq!(
            target.reason,
            Some(CloseRefusalReason::StrandsOrchestration),
            "{target:?}"
        );
        assert!(
            deck.live(&o),
            "the refused close left the orchestrator running"
        );
        assert!(deck.registry.dispatched_units().get(&id).is_some());

        let _ = release.send(());
        let replacement = respawn
            .await
            .unwrap()
            .expect("the worker's replacement lands");
        assert!(deck.live(&replacement));
        deck.shutdown().await;
    }

    /// Start a role of the instance `orch` in `pane`, the way `pane spawn`
    /// joins one: through the attach socket, with the instance's membership.
    async fn join_instance(
        deck: &Deck,
        orch: &str,
        pane: &str,
    ) -> Result<String, crate::daemon_client::ClientError> {
        let cwd = deck.dir.path().to_string_lossy().into_owned();
        deck.client
            .start_agent(StartAgentOptions {
                command: Some("sleep 30".to_string()),
                cwd: Some(cwd.clone()),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                tab_membership: Some(TabMembership::Orchestration {
                    name: "team".to_string(),
                    role_index: 1,
                    role_name: format!("coder-{pane}"),
                    is_start_role: false,
                    orchestration_cwd: Some(cwd),
                    display_title: None,
                    orchestration_id: Some(orch.to_string()),
                }),
                ..StartAgentOptions::default()
            })
            .await
    }

    /// Whether any live agent is a role of the instance `orch`.
    fn instance_has_live_role(deck: &Deck, orch: &str) -> bool {
        deck.registry.agent_records().iter().any(|r| {
            crate::agent_pty::orchestration_instance_of(r.tab_membership.as_ref()) == Some(orch)
                && deck.live(&r.id)
        })
    }

    /// Scenario: a dispatched orchestration's last live role is closed by
    /// `--pane`, beside a dead role registration, so its stop leaves the unit
    /// live and the close's final end check decides. Paused inside that
    /// check, a new role joins the instance, as `pane spawn` does. The close
    /// resumes and does not end the unit behind the new role: the pane it was
    /// asked for is reported closed, the unit stays live, and the new role
    /// runs. Closing that role then ends the unit, and no role joins after.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_role_joining_during_a_last_role_pane_close_keeps_the_unit_live() {
        let deck = Deck::new().await;
        let o = deck
            .start("pe-o", "sleep 30", Some(("orch-pe", "orchestrator", true)))
            .await;
        let id = deck.orch_unit("team-pe", ("disp", "nobody"), "orch-pe", "pe-o", &o);
        deck.report_done("pe-o", &o);
        deck.state.write().await.register_orchestration_role(
            "pe-dead",
            "ghost",
            false,
            crate::state::OrchestrationIdentity {
                id: "orch-pe".to_string(),
                name: "team".to_string(),
            },
            None,
        );
        let (reached, release) = deck.registry.pause_at_for_test("close-end-check");
        let pane = |p: &str| CloseSelector::Pane { pane_id: p.into() };
        let close = close_in_background(&deck, pane("pe-o"), None, true);
        reached
            .await
            .expect("the close reaches its final end check");
        assert!(!deck.live(&o));
        assert!(
            deck.registry.dispatched_units().get(&id).is_some(),
            "precondition: the stop left the unit for the end check"
        );
        let joined = join_instance(&deck, "orch-pe", "pe-new")
            .await
            .expect("the instance is open while its last role's pane closes");
        let _ = release.send(());
        let report = close.await.unwrap();
        let target = &report.targets[0];
        assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
        assert_eq!(target.error, None, "{target:?}");
        assert_eq!(target.worktree_verdict, None, "the unit still runs there");
        assert!(deck.live(&joined));
        assert!(
            deck.registry.dispatched_units().get(&id).is_some(),
            "the unit is not ended behind its live role"
        );
        assert!(!deck.registry.is_instance_closing("orch-pe"));

        let report = deck.close(pane("pe-new"), None, true, false).await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        let refused = join_instance(&deck, "orch-pe", "pe-late")
            .await
            .expect_err("no role joins an instance whose unit ended");
        assert!(
            refused
                .to_string()
                .contains(crate::agent_pty::INSTANCE_CLOSING_REASON),
            "{refused}"
        );
        assert!(!instance_has_live_role(&deck, "orch-pe"));
        deck.shutdown().await;
    }

    /// Scenario: closing one role of a dispatched orchestration by `--pane`
    /// leaves the instance open, so a new role still joins it. Closing the
    /// last role by `--pane` ends the unit through that role's stop, and from
    /// then on a role that tries to join the instance is refused: an ended
    /// unit never has a live role.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_last_role_closed_by_pane_ends_the_unit_and_its_instance() {
        let deck = Deck::new().await;
        let o = deck
            .start("pf-o", "sleep 30", Some(("orch-pf", "orchestrator", true)))
            .await;
        let w = deck
            .start("pf-w", "sleep 30", Some(("orch-pf", "coder", false)))
            .await;
        let id = deck.orch_unit("team-pf", ("disp", "nobody"), "orch-pf", "pf-o", &o);
        deck.report_done("pf-o", &o);
        let pane = |p: &str| CloseSelector::Pane { pane_id: p.into() };

        let report = deck.close(pane("pf-o"), None, true, false).await;
        assert_eq!(report.targets[0].outcome, CloseOutcome::Closed);
        assert!(deck.registry.dispatched_units().get(&id).is_some());
        let joined = join_instance(&deck, "orch-pf", "pf-new")
            .await
            .expect("closing a role that is not the last leaves the instance open");

        for (p, agent) in [("pf-w", &w), ("pf-new", &joined)] {
            let report = deck.close(pane(p), None, true, false).await;
            let target = &report.targets[0];
            assert_eq!(target.outcome, CloseOutcome::Closed, "{target:?}");
            assert!(!deck.live(agent));
        }
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        let refused = join_instance(&deck, "orch-pf", "pf-late")
            .await
            .expect_err("no role joins an instance whose unit ended");
        assert!(
            refused
                .to_string()
                .contains(crate::agent_pty::INSTANCE_CLOSING_REASON),
            "{refused}"
        );
        assert!(!instance_has_live_role(&deck, "orch-pf"));
        deck.shutdown().await;
    }

    /// Run `target` through one close attempt in-process, as the person.
    async fn close_attempt(deck: &Deck, target: &Resolved) -> Attempt {
        close_one(
            target,
            &Caller::Person,
            false,
            false,
            &deck.registry,
            &deck.state,
            &deck.event_tx,
            &deck.worktrees,
            &mut ReportBudget::new(),
        )
        .await
    }

    /// Scenario: auditor S2 — a single unit is resolved to its agent A, and
    /// before the close acts a respawn replaces A with B in the same pane,
    /// moving the unit to B. The close follows the unit to B and stops it:
    /// it never reports the unit closed, or ends it, while B keeps running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_unit_replaced_after_resolution_is_closed_in_its_successor() {
        let deck = Deck::new().await;
        let a = deck.start("solo-s2", "sleep 30", None).await;
        let id = deck.single_unit("solo-s2", ("disp", "nobody"), "solo-s2", &a);
        deck.report_done("solo-s2", &a);
        let (resolved, refused, _) = resolve(
            &by_name("solo-s2"),
            &Caller::Person,
            &deck.registry,
            &mut ReportBudget::new(),
        );
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(resolved[0].scope, Scope::Generation(a.clone()));
        let b = deck
            .registry
            .respawn_agent_for_pane("solo-s2", "sleep 30")
            .await
            .expect("respawn the unit's agent");
        let entry = match close_attempt(&deck, &resolved[0]).await {
            Attempt::Done(entry) => entry,
            Attempt::Rebind(entry) => panic!("the scope is read when the close starts: {entry:?}"),
        };
        assert_eq!(entry.outcome, CloseOutcome::Closed, "{entry:?}");
        assert!(
            entry
                .panes
                .iter()
                .any(|p| p.agent_id == b && p.stopped == Some(true)),
            "{entry:?}"
        );
        assert!(
            !deck.live(&b),
            "a Closed report never leaves the successor running"
        );
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        deck.shutdown().await;
    }

    /// Scenario: auditor SF1 — a close of a single unit has opened its
    /// admission state and checked for a respawn in flight, and is paused
    /// before it takes its holds; a respawn of the unit's agent A then arrives.
    /// The respawn is refused before it touches A, never reaching its
    /// remove→replace window; the close resumes, stops A and ends the unit, and
    /// no replacement ever runs in the pane behind the Closed report.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_respawn_after_the_pending_check_is_refused_before_it_touches_the_agent() {
        let deck = Deck::new().await;
        let a = deck.start("solo-s2b", "sleep 30", None).await;
        let id = deck.single_unit("solo-s2b", ("disp", "nobody"), "solo-s2b", &a);
        deck.report_done("solo-s2b", &a);
        let (reached, release) = deck.registry.pause_at_for_test("close-holds");
        let close = close_in_background(&deck, by_name("solo-s2b"), None, false);
        reached.await.expect("the close passes its respawn check");
        assert!(deck.registry.is_generation_closing(&a));
        let (mut window, _release_respawn) = deck.registry.pause_next_respawn_for_test();
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            deck.registry.respawn_agent_for_pane("solo-s2b", "sleep 30"),
        )
        .await
        .expect("refused at once, never paused in its remove→replace window")
        .expect_err("a respawn of a generation under close is refused");
        assert!(
            refused
                .to_string()
                .contains(crate::agent_pty::GENERATION_CLOSING_REASON),
            "{refused}"
        );
        assert!(
            window.try_recv().is_err(),
            "the refused respawn never reached its remove→replace window"
        );
        assert!(deck.live(&a), "the refusal left A exactly as it was");
        assert_eq!(
            deck.registry.pane_current_agent_id("solo-s2b").as_deref(),
            Some(a.as_str())
        );
        let _ = release.send(());
        let report = close.await.unwrap();
        let entry = &report.targets[0];
        assert_eq!(entry.outcome, CloseOutcome::Closed, "{entry:?}");
        assert!(
            entry
                .panes
                .iter()
                .any(|p| p.agent_id == a && p.stopped == Some(true)),
            "{entry:?}"
        );
        assert!(!deck.live(&a));
        assert!(
            deck.registry.pane_current_agent_id("solo-s2b").is_none(),
            "no replacement runs behind a Closed report"
        );
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        assert!(!deck.registry.is_generation_closing(&a));
        deck.shutdown().await;
    }

    /// Scenario: auditor SF1 — a respawn of a single unit's agent A is
    /// admitted first and is paused in its remove→replace window when the
    /// close begins. The close finds it pending and waits; the respawn is let
    /// through, its replacement B lands and the unit moves to B; the close
    /// follows the unit to B and stops it, so the Closed report is never
    /// followed by a live B and the unit ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_respawn_admitted_before_the_close_is_waited_for_and_rebound() {
        let deck = Deck::new().await;
        let a = deck.start("solo-sf1", "sleep 30", None).await;
        let id = deck.single_unit("solo-sf1", ("disp", "nobody"), "solo-sf1", &a);
        deck.report_done("solo-sf1", &a);
        let (respawn, release) = paused_respawn(&deck, "solo-sf1").await;
        let close = close_in_background(&deck, by_name("solo-sf1"), None, false);
        wait_until("the close opens the generation's admission state", || {
            deck.registry.is_generation_closing(&a)
        })
        .await;
        let _ = release.send(());
        let b = respawn
            .await
            .unwrap()
            .expect("an admitted respawn's replacement is let through");
        let report = close.await.unwrap();
        let entry = &report.targets[0];
        assert_eq!(entry.outcome, CloseOutcome::Closed, "{entry:?}");
        assert!(
            entry
                .panes
                .iter()
                .any(|p| p.agent_id == b && p.stopped == Some(true)),
            "{entry:?}"
        );
        assert!(!deck.live(&b), "a Closed report never leaves B running");
        assert!(deck.registry.pane_current_agent_id("solo-sf1").is_none());
        assert!(deck.registry.dispatched_units().get(&id).is_none());
        deck.shutdown().await;
    }

    /// Scenario: auditor SF1 — a single unit whose agent is already gone is
    /// closed, so the close stops nothing and goes straight to its final
    /// check that nothing of the unit remains. Paused inside that check, the
    /// unit's binding moves to a live agent B in another pane. The close does
    /// not end the unit behind B: the unit stays live and bound to B, B keeps
    /// running, and the report says the close did not finish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_unit_moved_during_the_final_end_check_is_not_ended() {
        let deck = Deck::new().await;
        let id = deck.single_unit("solo-end", ("disp", "nobody"), "solo-end", "gone-a");
        deck.report_done("solo-end", "gone-a");
        let (reached, release) = deck.registry.pause_at_for_test("close-end-check");
        let close = close_in_background(&deck, by_name("solo-end"), None, false);
        reached
            .await
            .expect("the close reaches its final end check");
        let b = deck.start("solo-end", "sleep 30", None).await;
        assert_eq!(
            deck.registry
                .dispatched_units()
                .note_generation_replaced("solo-end", "gone-a", &b)
                .as_deref(),
            Some(id.as_str())
        );
        let _ = release.send(());
        let report = close.await.unwrap();
        let entry = &report.targets[0];
        assert_ne!(entry.outcome, CloseOutcome::Closed, "{entry:?}");
        assert_eq!(
            entry.error.as_deref(),
            Some(
                "the unit moved to a new agent while it was being closed, and that agent is still running — close it again"
            ),
            "{entry:?}"
        );
        assert!(deck.live(&b));
        let bound = deck
            .registry
            .dispatched_units()
            .get(&id)
            .map(scope_of_unit)
            .expect("the unit is not ended behind B");
        assert_eq!(bound, Scope::Generation(b.clone()));
        deck.shutdown().await;
    }

    /// Scenario: auditor S3 — a selector of a million (empty) ids, already
    /// decoded, is refused by its length alone, before the caller's forged
    /// claim is even looked at; the refusal counts the entries without adding
    /// up their bytes.
    #[tokio::test]
    async fn an_oversized_selector_is_refused_by_its_length_before_its_caller() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (event_tx, _rx) = broadcast::channel(16);
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let worktrees = crate::issue_dispatch_run::new_worktree_registry();
        let forged = CallerClaim {
            pane_id: "disp".to_string(),
            token: "0".repeat(crate::hook_provenance::TOKEN_LEN),
        };
        let report = handle_close_agents(
            CloseSelector::UnitIds {
                ids: vec![String::new(); 1_000_000],
            },
            Some(forged),
            false,
            false,
            &registry,
            &state,
            &event_tx,
            &worktrees,
        )
        .await;
        let refused = report.refused.expect("refused whole");
        assert_eq!(refused.reason, CloseRefusalReason::SelectorTooLarge);
        assert!(
            refused.message.contains("names 1000000 entries."),
            "{}",
            refused.message
        );
        assert!(report.targets.is_empty());
    }

    /// Scenario: auditor S3/SF3 — a ledger holding thousands of live units,
    /// many sharing names, with long worktree paths and names made of control
    /// characters that JSON escapes to six bytes each, and no agent running.
    /// `close --all` lists the first 256 and says it was cut short; closing 40
    /// ambiguous names lists at most 256 candidates each, marks every entry
    /// whose list was cut, and the whole serialized response stays within the
    /// nested-list budget and under the attach frame cap, so it is written and
    /// read back rather than refused.
    #[tokio::test]
    async fn a_populated_ledger_yields_a_bounded_report() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (event_tx, _rx) = broadcast::channel(16);
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let worktrees = crate::issue_dispatch_run::new_worktree_registry();
        let long = "\u{1}".repeat(2000);
        let names: Vec<String> = (0..40)
            .map(|k| format!("fix-{k}{}", "\u{2}".repeat(100)))
            .collect();
        {
            let mut units = registry.dispatched_units();
            for name in &names {
                for n in 0..300 {
                    units.register(NewUnit {
                        name: name.clone(),
                        worktree: PathBuf::from(format!("/{long}/{name}-{n}")),
                        branch: format!("agent/{name}"),
                        clone_dir: PathBuf::from("/clone"),
                        dispatcher: Dispatcher {
                            pane_id: "disp".into(),
                            agent_id: "nobody".into(),
                        },
                        kind: UnitKind::Single {
                            pane_id: format!("{name}-{n}"),
                            agent_id: format!("gone-{name}-{n}"),
                        },
                        dispatched_at_ms: 1,
                    });
                }
            }
        }
        let close = |selector: CloseSelector| {
            handle_close_agents(
                selector, None, false, true, &registry, &state, &event_tx, &worktrees,
            )
        };

        let report = close(CloseSelector::AllUnits).await;
        assert!(report.truncated);
        assert_eq!(report.targets.len(), MAX_LISTED_TARGETS);

        let report = close(CloseSelector::Units(UnitSelector {
            names: names.clone(),
        }))
        .await;
        assert_eq!(report.targets.len(), names.len());
        for target in &report.targets {
            assert_eq!(target.reason, Some(CloseRefusalReason::Ambiguous));
            assert!(target.candidates.len() <= MAX_LISTED_TARGETS);
            assert!(target.truncated, "300 candidates never fit in the list");
        }
        let listed: usize = report.targets.iter().map(|t| t.candidates.len()).sum();
        assert!(listed > 0, "the budget lists what fits");
        assert!(
            listed < names.len() * MAX_LISTED_TARGETS,
            "the budget cut the candidates short: {listed}"
        );
        let bytes = serde_json::to_string(&report).unwrap().len();
        assert_eq!(bytes, json_len(&report), "the counter counts what is sent");
        assert!(
            bytes < MAX_NESTED_REPORT_BYTES + 256 * 1024,
            "the report is bounded by the budget, escapes included: {bytes} bytes"
        );
        let delivered = deliver(report).await;
        assert!(delivered.targets.iter().all(|t| t.truncated));
    }

    /// Write `report` as the attach socket answers it, and read it back:
    /// panics if the frame cap refuses it.
    async fn deliver(report: CloseReport) -> CloseReport {
        let resp = crate::daemon_protocol::AttachResponse {
            close_report: Some(report),
            ..crate::daemon_protocol::AttachResponse::ok()
        };
        let mut wire: Vec<u8> = Vec::new();
        crate::daemon_protocol::write_resp(&mut wire, &resp)
            .await
            .expect("the response fits the attach frame");
        assert!(wire.len() - 5 <= crate::daemon_protocol::MAX_FRAME_LEN);
        let back: crate::daemon_protocol::AttachResponse =
            serde_json::from_slice(&wire[5..]).expect("the frame holds the response");
        back.close_report.expect("a close report")
    }

    /// A ledger of `names` dispatch names, each shared by `per_name`
    /// orchestration units of the one instance `instance`.
    fn shared_names(
        registry: &AgentPtyRegistry,
        names: &[String],
        per_name: usize,
        instance: &str,
    ) {
        let mut units = registry.dispatched_units();
        for name in names {
            for n in 0..per_name {
                units.register(NewUnit {
                    name: name.clone(),
                    worktree: PathBuf::from(format!("/wt/{name}-{n}")),
                    branch: format!("agent/{name}"),
                    clone_dir: PathBuf::from("/clone"),
                    dispatcher: Dispatcher {
                        pane_id: "disp".into(),
                        agent_id: "nobody".into(),
                    },
                    kind: UnitKind::Orchestration {
                        orchestration_id: instance.to_string(),
                        name: "team".to_string(),
                        terminal_pane_id: format!("{instance}-0"),
                        terminal_agent_id: "gone".to_string(),
                    },
                    dispatched_at_ms: 1,
                });
            }
        }
    }

    /// Scenario: auditor SF2 — 256 ambiguous names, each shared by 260 units
    /// of one orchestration that has 300 roles. Resolving the whole request
    /// builds at most 256 candidates per name and 256 panes per candidate,
    /// charges each as it is built, and stops building once the report's
    /// budget is spent: what resolution itself produced — before anything
    /// trims it — stays within that budget, and the names after it list no
    /// candidates and say so.
    #[tokio::test]
    async fn a_whole_ambiguous_request_is_bounded_while_it_is_built() {
        let registry = Arc::new(AgentPtyRegistry::new());
        for n in 0..300 {
            registry.insert_idle_member_for_test(&format!("big-{n}"), "big", n == 0);
        }
        let names: Vec<String> = (0..MAX_SELECTOR_ENTRIES)
            .map(|k| format!("dup-{k}"))
            .collect();
        shared_names(&registry, &names, 260, "big");
        let mut budget = ReportBudget::new();
        let (resolved, refused, _) = resolve(
            &CloseSelector::Units(UnitSelector {
                names: names.clone(),
            }),
            &Caller::Person,
            &registry,
            &mut budget,
        );
        assert!(resolved.is_empty());
        assert_eq!(refused.len(), names.len());
        assert!(budget.exhausted(), "the budget was the limit");
        for target in &refused {
            assert_eq!(target.reason, Some(CloseRefusalReason::Ambiguous));
            assert!(target.truncated, "{} lists were cut", target.selector);
            assert!(target.candidates.len() <= MAX_LISTED_TARGETS);
            for candidate in &target.candidates {
                assert!(candidate.panes.len() <= MAX_LISTED_TARGETS);
            }
        }
        assert_eq!(
            refused[0].candidates[0].panes.first().map(String::as_str),
            Some("big-0"),
            "the orchestrator's pane is listed first"
        );
        assert!(
            refused.last().unwrap().candidates.is_empty(),
            "nothing is built once the budget is spent"
        );
        let nested: usize = refused
            .iter()
            .flat_map(|t| &t.candidates)
            .map(json_len)
            .sum();
        assert!(
            nested <= MAX_NESTED_REPORT_BYTES,
            "built within the budget: {nested} bytes"
        );
    }

    /// Scenario: auditor SF5 — one ambiguous name shared by 40 units of one
    /// orchestration that has 300 roles. Every candidate lists that
    /// orchestration's panes, and the registry is walked for them once, not
    /// once per candidate.
    #[tokio::test]
    async fn candidates_sharing_an_instance_walk_the_registry_once() {
        let registry = Arc::new(AgentPtyRegistry::new());
        for n in 0..300 {
            registry.insert_idle_member_for_test(&format!("big-{n}"), "big", n == 0);
        }
        let names = vec!["dup".to_string()];
        shared_names(&registry, &names, 40, "big");
        let before = registry.member_visits_for_test();
        let (_, refused, _) = resolve(
            &CloseSelector::Units(UnitSelector { names }),
            &Caller::Person,
            &registry,
            &mut ReportBudget::new(),
        );
        let candidates = &refused[0].candidates;
        assert_eq!(candidates.len(), 40);
        assert!(
            candidates
                .iter()
                .all(|c| c.panes.len() == MAX_LISTED_TARGETS)
        );
        assert_eq!(
            registry.member_visits_for_test() - before,
            300,
            "one walk of the 300 records"
        );
    }

    /// Scenario: auditor SF5 — one ambiguous name shared by 10 units, each of
    /// its own 20-role orchestration, resolved with a report budget that holds
    /// every candidate but the panes of only three and a half of them. The
    /// fourth candidate's panes are cut where the budget ran out, the six
    /// after it list none, the target is marked truncated, and the registry is
    /// not walked again for any of those six.
    #[tokio::test]
    async fn a_spent_budget_walks_the_registry_no_further() {
        let registry = Arc::new(AgentPtyRegistry::new());
        for c in 0..10 {
            let instance = format!("inst-{c}");
            for n in 0..20 {
                registry.insert_idle_member_for_test(&format!("i{c}-{n}"), &instance, n == 0);
            }
            shared_names(&registry, &["dup".to_string()], 1, &instance);
        }
        let records = 200;
        let selector = CloseSelector::Units(UnitSelector {
            names: vec!["dup".to_string()],
        });
        let (_, whole, _) = resolve(
            &selector,
            &Caller::Person,
            &registry,
            &mut ReportBudget::new(),
        );
        let candidates = &whole[0].candidates;
        assert_eq!(candidates.len(), 10);
        let headers: usize = candidates
            .iter()
            .map(|c| {
                NESTED_ENTRY_OVERHEAD
                    + json_len(&c.unit_id)
                    + json_len(&c.name)
                    + json_len(&c.worktree)
            })
            .sum();
        let panes_of =
            |c: &AmbiguousCandidate| -> usize { c.panes.iter().map(|p| json_len(p) + 1).sum() };
        let room = headers
            + candidates[..3].iter().map(panes_of).sum::<usize>()
            + panes_of(&candidates[3]) / 2;
        let before = registry.member_visits_for_test();
        let (_, cut, _) = resolve(
            &selector,
            &Caller::Person,
            &registry,
            &mut ReportBudget { remaining: room },
        );
        let target = &cut[0];
        assert!(target.truncated);
        assert_eq!(target.candidates.len(), 10, "every header fit");
        assert!(target.candidates[..3].iter().all(|c| c.panes.len() == 20));
        assert!(!target.candidates[3].panes.is_empty());
        assert!(target.candidates[3].panes.len() < 20);
        assert!(target.candidates[4..].iter().all(|c| c.panes.is_empty()));
        assert_eq!(
            registry.member_visits_for_test() - before,
            4 * records,
            "walked for the four candidates the budget reached, and no further"
        );
    }

    /// Scenario: auditor SF2 — an orchestration with 300 roles is closed whole
    /// by a person with --force. Every one of the 300 roles is stopped, while
    /// the report lists the first 256 panes, orchestrator first, and marks
    /// the target truncated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_target_with_more_panes_than_a_list_holds_still_stops_them_all() {
        let deck = Deck::new().await;
        let ids: Vec<String> = (0..300)
            .map(|n| {
                deck.registry
                    .insert_idle_member_for_test(&format!("wide-{n}"), "wide", n == 0)
            })
            .collect();
        let report = deck
            .close(
                CloseSelector::OrchestrationOf {
                    pane_id: "wide-0".to_string(),
                },
                None,
                true,
                false,
            )
            .await;
        let entry = &report.targets[0];
        assert_eq!(entry.outcome, CloseOutcome::Closed, "{:?}", entry.error);
        assert_eq!(entry.panes.len(), MAX_LISTED_TARGETS);
        assert!(entry.truncated);
        assert_eq!(entry.panes[0].pane_id.as_deref(), Some("wide-0"));
        assert!(entry.panes.iter().all(|p| p.stopped == Some(true)));
        let left: Vec<String> = deck
            .registry
            .agent_records_including_exited()
            .into_iter()
            .filter(|(r, _)| ids.contains(&r.id))
            .map(|(r, _)| r.id)
            .collect();
        assert!(left.is_empty(), "every role was stopped: {left:?}");
        deck.shutdown().await;
    }

    /// The longest unit id the ledger mints: `u-<epoch>-<u64::MAX>`.
    fn longest_unit_id() -> String {
        format!(
            "u-{}-{}",
            "f".repeat(crate::dispatched_units::EPOCH_HEX_CHARS),
            u64::MAX
        )
    }

    /// A target whose every string field but its unit id is `chars` control
    /// characters long, and whose survivors and nested lists hold `copies`
    /// entries each. The unit id is the longest the ledger mints, since a
    /// report never shortens it.
    fn bloated_target(chars: usize, copies: usize) -> CloseTarget {
        let big = "\u{1}".repeat(chars);
        let mut t = CloseTarget::refused(&big, CloseRefusalReason::Unknown, big.clone());
        t.unit_id = Some(longest_unit_id());
        t.name = Some(big.clone());
        t.kind = big.clone();
        t.error = Some(big.clone());
        t.worktree = Some(big.clone());
        t.branch = Some(big.clone());
        t.clone = Some(big.clone());
        t.survivors = vec![big.clone(); copies];
        t.dispatcher = Some(CloseDispatcher {
            pane_id: big.clone(),
            agent_id: big.clone(),
        });
        t.worktree_verdict = Some(WorktreeVerdict {
            path: big.clone(),
            verdict: WorktreeVerdictKind::Removed,
        });
        t.panes = vec![
            ClosePane {
                agent_id: big.clone(),
                pane_id: Some(big.clone()),
                role: Some(big.clone()),
                is_orchestrator: false,
                status: Some(big.clone()),
                exited: false,
                stopped: Some(true),
            };
            copies
        ];
        t.open_descendants = vec![
            OpenDescendant {
                unit_id: big.clone(),
                name: big.clone(),
                clone: big.clone(),
            };
            copies
        ];
        t.candidates = vec![
            AmbiguousCandidate {
                unit_id: big.clone(),
                name: big.clone(),
                worktree: big.clone(),
                panes: vec![big.clone(); copies],
            };
            copies
        ];
        t
    }

    /// Scenario: auditor SF3 — the most targets a report can hold, each
    /// shortened as far as the size bound shortens one, still fit within the
    /// report's bound, however long the fields were — so the bound is
    /// reachable for every report.
    #[test]
    fn a_shortened_target_fits_the_report_bound() {
        let mut target = bloated_target(10_000, 40);
        shed_lists(&mut target);
        clip_fields(&mut target);
        assert!(target.truncated);
        assert_eq!(
            target.unit_id.as_deref(),
            Some(longest_unit_id().as_str()),
            "a unit id is never shortened (reviewer R1)"
        );
        let envelope = json_len(&CloseReport {
            dry_run: true,
            forced: true,
            refused: None,
            truncated: true,
            targets: Vec::new(),
        });
        let worst = envelope + MAX_SELECTOR_ENTRIES * (json_len(&target) + 1);
        assert!(
            worst <= MAX_REPORT_BYTES,
            "{worst} bytes for {MAX_SELECTOR_ENTRIES} shortened targets"
        );
    }

    /// Scenario: auditor SF3 — a report of 256 targets whose own fields,
    /// escapes included, add up to far more than the attach frame holds. The
    /// report is shortened to its size bound, each shortened target marked
    /// truncated, and the response is then written and read back whole.
    #[tokio::test]
    async fn an_oversized_report_is_shortened_and_delivered() {
        let mut report = CloseReport {
            dry_run: true,
            forced: false,
            refused: None,
            truncated: false,
            targets: vec![bloated_target(1_000, 1); MAX_SELECTOR_ENTRIES],
        };
        assert!(json_len(&report) > crate::daemon_protocol::MAX_FRAME_LEN);
        let mut shed = report.clone();
        shed.targets.iter_mut().for_each(shed_lists);
        assert!(
            json_len(&shed) > MAX_REPORT_BYTES,
            "shedding the lists alone does not fit it, so fields are shortened too"
        );
        drop(shed);
        fit_deliverable(&mut report);
        assert!(json_len(&report) <= MAX_REPORT_BYTES);
        assert!(
            report.targets.last().unwrap().truncated,
            "the last target is shortened first"
        );
        let delivered = deliver(report).await;
        assert_eq!(delivered.targets.len(), MAX_SELECTOR_ENTRIES);
    }
}
