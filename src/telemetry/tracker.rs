//! The run tracker: turns what the daemon sees — agents starting and ending,
//! the events on its broadcast, human input, delegations — into finished
//! `agent.run` and `agent.status` spans.
//!
//! Pure and synchronous. Every method takes the current time, so tests drive
//! it deterministically; [`super::exporter::Telemetry`] owns one behind a
//! mutex and passes the clock.
//!
//! **Status comes from the same code the cards use.** Each open run keeps its
//! own [`AppState`] and applies the run's broadcast events to it, then reads
//! the pane's card status back, so an `agent.status` interval is exactly what a
//! dashboard card showed — not a second derivation that could drift from it.
//! The run's pull request is read back from the same card, so a newer report, an
//! older one and an explicit "no pull request" are folded as the card folds
//! them. That state holds the events' content in memory, as every client's
//! does; nothing from it but the status enum and the pull request's number
//! reaches a span, and it is dropped with the run, so the tracker holds nothing
//! per pane once a pane's runs have ended.
//!
//! **A run is prepared before its agent starts.** [`RunTracker::prepare_run`]
//! is called before the agent process exists, so no event the agent causes can
//! arrive before the deck knows its run; events for a prepared run are held
//! (up to [`MAX_PENDING_EVENTS`]) and applied when [`RunTracker::commit_run`]
//! opens it, and [`RunTracker::abort_run`] forgets a run whose agent never
//! started.

use std::collections::HashMap;

use crate::event::{AgentEvent, AgentType, DAEMON_PANE_CLOSED_METADATA_KEY, EventType};
use crate::state::{AppState, SessionStatus};

use super::run_context::RunContext;
use super::span::{
    Attribute, SpanEventRecord, SpanId, SpanRecord, TraceId, new_span_id, new_trace_id,
};
use super::{
    AttrKey, EVENT_DELEGATION, EVENT_INTERVENTION, EVENT_STATUS_CHANGE, SPAN_AGENT_RUN,
    SPAN_AGENT_STATUS,
};

/// Human input closer together than this is one intervention: a burst of
/// keystrokes, not one per key.
pub const INTERVENTION_COALESCE_NANOS: u64 = 5_000_000_000;

/// Span events kept per run. Past it the counters still count; only the
/// per-occurrence events stop, so a run left open for weeks cannot grow without
/// bound.
pub const MAX_EVENTS_PER_RUN: usize = 1024;

/// Events held for a prepared run until it is committed. Its agent has only
/// just started, so this is far more than the window ever sees.
pub const MAX_PENDING_EVENTS: usize = 256;

/// How a run ended, as far as the deck can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The agent process exited on its own.
    Exited,
    /// The pane was closed or the agent stopped through the deck.
    Closed,
    /// The daemon shut down with the run still open.
    DaemonShutdown,
    /// The run was replaced or lost without the deck seeing why.
    Unknown,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Exited => "exited",
            EndReason::Closed => "closed",
            EndReason::DaemonShutdown => "daemon_shutdown",
            EndReason::Unknown => "unknown",
        }
    }
}

/// The orchestration a pane is a role of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrchestrationInfo {
    pub role: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
    /// Whether this pane is the orchestrator seat.
    pub orchestrator: bool,
}

/// What the daemon knows when it starts an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStart {
    /// `deck.run.id`, minted by the caller so the same value can go into the
    /// agent's environment before it starts.
    pub run_id: String,
    pub pane_id: String,
    /// The registry's id for this agent generation, when known: events naming a
    /// different generation of the same pane are ignored.
    pub agent_id: Option<String>,
    pub agent: AgentType,
    pub orchestration: Option<OrchestrationInfo>,
    /// The model, the invoking skill and the issue are not held by today's
    /// daemon; they stay `None` until something supplies them, and their
    /// attributes stay absent.
    pub model: Option<String>,
    pub invoking_skill: Option<String>,
    pub issue_number: Option<u64>,
}

impl RunStart {
    /// What the spawn path knows about the agent it is starting on `pane_id`
    /// as generation `agent_id`: the declared agent kind, else the one the
    /// command's basename names, and the orchestration the pane is a role of.
    /// The run id is left for [`super::Telemetry::prepare_run`] to mint.
    pub fn for_spawn(
        pane_id: &str,
        agent_id: &str,
        declared: Option<&AgentType>,
        command: Option<&str>,
        membership: Option<&crate::agent_pty::TabMembership>,
    ) -> Self {
        let detected = command
            .and_then(|c| c.split_whitespace().next())
            .and_then(|program| std::path::Path::new(program).file_name())
            .and_then(|name| name.to_str())
            .and_then(crate::agent_registry::detect_from_basename);
        let orchestration = match membership {
            Some(crate::agent_pty::TabMembership::Orchestration {
                name,
                role_name,
                is_start_role,
                orchestration_id,
                ..
            }) => Some(OrchestrationInfo {
                role: Some(role_name.clone()).filter(|r| !r.is_empty()),
                name: Some(name.clone()).filter(|n| !n.is_empty()),
                id: orchestration_id.clone(),
                orchestrator: *is_start_role,
            }),
            _ => None,
        };
        RunStart {
            run_id: String::new(),
            pane_id: pane_id.to_string(),
            agent_id: Some(agent_id.to_string()),
            agent: declared.cloned().or(detected).unwrap_or(AgentType::None),
            orchestration,
            model: None,
            invoking_skill: None,
            issue_number: None,
        }
    }
}

struct OpenStatus {
    name: &'static str,
    span_id: SpanId,
    start: u64,
}

/// A run whose agent is starting: see [`RunTracker::prepare_run`].
struct PendingRun {
    start: RunStart,
    prepared_at: u64,
    context: RunContext,
    events: Vec<(AgentEvent, u64)>,
}

/// A run's own card state, dropped with the run. Tests count the live ones
/// ([`RunTracker::live_shadow_states`]).
struct ShadowCards {
    state: Box<AppState>,
    #[cfg(test)]
    _live: LiveWitness,
}

impl std::ops::Deref for ShadowCards {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.state
    }
}

impl std::ops::DerefMut for ShadowCards {
    fn deref_mut(&mut self) -> &mut AppState {
        &mut self.state
    }
}

/// Counts itself into a tracker's live shadow states while it exists.
#[cfg(test)]
struct LiveWitness(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl LiveWitness {
    fn new(count: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        LiveWitness(std::sync::Arc::clone(count))
    }
}

#[cfg(test)]
impl Drop for LiveWitness {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

struct ActiveRun {
    start_info: RunStart,
    trace_id: TraceId,
    span_id: SpanId,
    start: u64,
    context: RunContext,
    /// This run's own card state, dropped with the run.
    cards: ShadowCards,
    pr_number: Option<u64>,
    status: Option<OpenStatus>,
    events: Vec<SpanEventRecord>,
    interventions: u64,
    status_changes: u64,
    last_human_input: Option<u64>,
}

impl ActiveRun {
    fn push_event(&mut self, event: SpanEventRecord) {
        if self.events.len() < MAX_EVENTS_PER_RUN {
            self.events.push(event);
        }
    }

    /// See [`RunTracker::observe_human_input`].
    fn human_input(&mut self, now: u64) {
        let new_burst = self
            .last_human_input
            .is_none_or(|last| now.saturating_sub(last) > INTERVENTION_COALESCE_NANOS);
        self.last_human_input = Some(now);
        if new_burst {
            self.interventions += 1;
            self.push_event(SpanEventRecord {
                name: EVENT_INTERVENTION,
                time_unix_nanos: now,
                attributes: vec![Attribute::str(AttrKey::InterventionKind, "input")],
            });
        }
    }
}

/// Asked, with the pane and the run id, as each run ends — by whatever ends
/// it — for the times of human input still held back for that run, oldest
/// first, which are then counted before the run's span is finished. See
/// [`RunTracker::set_held_input`].
pub type HeldInput = Box<dyn FnMut(&str, &str) -> Vec<u64> + Send>;

/// See the module docs.
#[derive(Default)]
pub struct RunTracker {
    /// Open runs, by pane.
    runs: HashMap<String, ActiveRun>,
    /// Prepared runs, by run id.
    pending: HashMap<String, PendingRun>,
    finished: Vec<SpanRecord>,
    held_input: Option<HeldInput>,
    /// Test witness: the [`ShadowCards`] alive.
    #[cfg(test)]
    shadow_states: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RunTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask `held` for a run's held-back human input as the run ends — input
    /// accepted but not yet applied, and a lone `ESC` still waiting to be told
    /// apart from a report, which is the Escape key once the run is over —
    /// whether its agent exited, its pane closed, a new generation replaced it
    /// or the daemon shut down.
    pub fn set_held_input(&mut self, held: HeldInput) {
        self.held_input = Some(held);
    }

    /// The `deck.run.id` of the run open on `pane_id`, if any.
    pub fn run_id(&self, pane_id: &str) -> Option<&str> {
        self.runs
            .get(pane_id)
            .map(|run| run.start_info.run_id.as_str())
    }

    /// An agent is about to start: remember its run, so events it causes before
    /// [`Self::commit_run`] are kept for it. A run id prepared twice keeps the
    /// first preparation.
    pub fn prepare_run(&mut self, start: RunStart, now: u64) {
        self.pending
            .entry(start.run_id.clone())
            .or_insert(PendingRun {
                start,
                prepared_at: now,
                context: RunContext::default(),
                events: Vec::new(),
            });
    }

    /// The agent of the prepared run `run_id` did not start: forget the run.
    pub fn abort_run(&mut self, run_id: &str) {
        self.pending.remove(run_id);
    }

    /// The agent of the prepared run `run_id` started: open its run, from the
    /// time it was prepared, and apply the events held for it.
    pub fn commit_run(&mut self, run_id: &str) {
        let Some(pending) = self.pending.remove(run_id) else {
            return;
        };
        let pane_id = pending.start.pane_id.clone();
        self.begin_run(pending.start, pending.prepared_at);
        if let Some(run) = self.runs.get_mut(&pane_id) {
            run.context = pending.context;
        }
        for (event, at) in pending.events {
            self.observe_event(&event, at);
        }
    }

    /// Open a run. A run still open on the same pane is ended first with
    /// [`EndReason::Unknown`]: the deck replaced it without reporting how.
    pub fn begin_run(&mut self, start: RunStart, now: u64) {
        if self.runs.contains_key(&start.pane_id) {
            self.end_run(&start.pane_id, EndReason::Unknown, None, now);
        }
        let pane_id = start.pane_id.clone();
        self.runs.insert(
            pane_id,
            ActiveRun {
                start_info: start,
                trace_id: new_trace_id(),
                span_id: new_span_id(),
                start: now,
                context: RunContext::default(),
                cards: self.new_cards(),
                pr_number: None,
                status: None,
                events: Vec::new(),
                interventions: 0,
                status_changes: 0,
                last_human_input: None,
            },
        );
    }

    fn new_cards(&self) -> ShadowCards {
        ShadowCards {
            state: Box::default(),
            #[cfg(test)]
            _live: LiveWitness::new(&self.shadow_states),
        }
    }

    /// Test witness: the per-run card states alive, whoever holds them.
    #[cfg(test)]
    pub fn live_shadow_states(&self) -> usize {
        self.shadow_states.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Attach the git-derived context once it has been resolved. Ignored when
    /// the run it was resolved for has already ended or been replaced.
    pub fn set_context(&mut self, run_id: &str, context: RunContext) {
        if let Some(pending) = self.pending.get_mut(run_id) {
            pending.context = context;
        } else if let Some(run) = self
            .runs
            .values_mut()
            .find(|run| run.start_info.run_id == run_id)
        {
            run.context = context;
        }
    }

    /// Apply one event from the daemon's broadcast.
    pub fn observe_event(&mut self, event: &AgentEvent, now: u64) {
        let Some(pane_id) = event.pane_id.as_deref() else {
            return;
        };
        // A prepared run takes the events of its own generation, and, while
        // the pane has no open run, the events that name no generation.
        let pane_has_run = self.runs.contains_key(pane_id);
        if let Some(pending) = self.pending.values_mut().find(|p| {
            p.start.pane_id == pane_id
                && match (p.start.agent_id.as_deref(), event.agent_id.as_deref()) {
                    (Some(ours), Some(theirs)) => ours == theirs,
                    _ => !pane_has_run,
                }
        }) {
            if pending.events.len() < MAX_PENDING_EVENTS {
                pending.events.push((event.clone(), now));
            }
            return;
        }
        let Some(run) = self.runs.get_mut(pane_id) else {
            return;
        };
        if let (Some(ours), Some(theirs)) = (
            run.start_info.agent_id.as_deref(),
            event.agent_id.as_deref(),
        ) && ours != theirs
        {
            return;
        }
        if event.event_type == EventType::SessionEnd
            && event.metadata.contains_key(DAEMON_PANE_CLOSED_METADATA_KEY)
        {
            let pane_id = pane_id.to_string();
            self.end_run(&pane_id, EndReason::Closed, None, now);
            return;
        }

        // `deck.agent` stays the kind the run started as, which is the value
        // the agent itself received in `OTEL_RESOURCE_ATTRIBUTES`: an event's
        // self-reported type relabelling only the deck's side would split the
        // join on that key.
        run.cards.apply_event(event.clone());
        if event.event_type == EventType::PullRequest
            && let Some(pr) = card_pull_request(&run.cards, event)
        {
            run.pr_number = pr;
        }
        let status = run
            .cards
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some(pane_id))
            .max_by_key(|s| s.last_activity)
            .map(|s| status_name(&s.status));
        if let Some(status) = status {
            let pane_id = pane_id.to_string();
            self.transition(&pane_id, status, now);
        }
    }

    fn transition(&mut self, pane_id: &str, to: &'static str, now: u64) {
        let Some(run) = self.runs.get_mut(pane_id) else {
            return;
        };
        let from = run.status.as_ref().map(|s| s.name);
        if from == Some(to) {
            return;
        }
        if let Some(open) = run.status.take() {
            self.finished.push(status_span(run, open, now));
        }
        run.status = Some(OpenStatus {
            name: to,
            span_id: new_span_id(),
            start: now,
        });
        run.status_changes += 1;
        let mut attributes = Vec::with_capacity(2);
        if let Some(from) = from {
            attributes.push(Attribute::str(AttrKey::StatusFrom, from));
        }
        attributes.push(Attribute::str(AttrKey::StatusTo, to));
        run.push_event(SpanEventRecord {
            name: EVENT_STATUS_CHANGE,
            time_unix_nanos: now,
            attributes,
        });
    }

    /// Input from a person at an attached client reached `pane_id`. Input the
    /// deck injects itself (a delegated task, an inbox delivery) must not be
    /// reported here. Keystrokes less than [`INTERVENTION_COALESCE_NANOS`] apart
    /// count as one intervention.
    pub fn observe_human_input(&mut self, pane_id: &str, now: u64) {
        if let Some(run) = self.runs.get_mut(pane_id) {
            run.human_input(now);
        }
    }

    /// [`Self::observe_human_input`], only while the run open on `pane_id` is
    /// `run_id`.
    pub fn observe_run_input(&mut self, pane_id: &str, run_id: &str, now: u64) {
        if let Some(run) = self.runs.get_mut(pane_id)
            && run.start_info.run_id == run_id
        {
            run.human_input(now);
        }
    }

    /// `from_pane` delegated work to `to_pane`. Each side's run records the
    /// other pane's id and nothing about the task.
    pub fn observe_delegation(&mut self, from_pane: &str, to_pane: &str, now: u64) {
        if let Some(run) = self.runs.get_mut(from_pane) {
            run.push_event(SpanEventRecord {
                name: EVENT_DELEGATION,
                time_unix_nanos: now,
                attributes: vec![Attribute::str(AttrKey::DelegationToPane, to_pane)],
            });
        }
        if let Some(run) = self.runs.get_mut(to_pane) {
            run.push_event(SpanEventRecord {
                name: EVENT_DELEGATION,
                time_unix_nanos: now,
                attributes: vec![Attribute::str(AttrKey::DelegationFromPane, from_pane)],
            });
        }
    }

    /// End the run on `pane_id`, if one is open.
    pub fn end_run(&mut self, pane_id: &str, reason: EndReason, exit_code: Option<i32>, now: u64) {
        let Some(mut run) = self.runs.remove(pane_id) else {
            return;
        };
        if let Some(held) = self.held_input.as_mut() {
            for written in held(pane_id, &run.start_info.run_id) {
                run.human_input(written);
            }
        }
        if let Some(open) = run.status.take() {
            self.finished.push(status_span(&run, open, now));
        }
        self.finished.push(run_span(run, reason, exit_code, now));
    }

    /// [`Self::end_run`], but only when the run open on `pane_id` belongs to
    /// `agent_id` (or either side does not know its agent). An exit observed
    /// late for a generation a respawn already replaced must not end the new
    /// generation's run.
    pub fn end_agent_run(
        &mut self,
        pane_id: &str,
        agent_id: Option<&str>,
        reason: EndReason,
        exit_code: Option<i32>,
        now: u64,
    ) {
        let other_generation = self.runs.get(pane_id).is_some_and(|run| {
            matches!((run.start_info.agent_id.as_deref(), agent_id), (Some(a), Some(b)) if a != b)
        });
        if !other_generation {
            self.end_run(pane_id, reason, exit_code, now);
        }
    }

    /// End every open run and forget every prepared one, for daemon
    /// shutdown.
    pub fn end_all(&mut self, reason: EndReason, now: u64) {
        self.pending.clear();
        let panes: Vec<String> = self.runs.keys().cloned().collect();
        for pane in panes {
            self.end_run(&pane, reason, None, now);
        }
    }

    /// Runs open and prepared: what the tracker holds between spans.
    pub fn held(&self) -> (usize, usize) {
        (self.runs.len(), self.pending.len())
    }

    /// The spans finished since the last call.
    pub fn take_finished(&mut self) -> Vec<SpanRecord> {
        std::mem::take(&mut self.finished)
    }
}

/// The pull request the card `event` reported on now holds — `Some(None)` when
/// it holds none — or `None` when the run holds no such card, so the report had
/// nothing to land on. Read after the card applied the report, so the card's own
/// rules decide: a report older than the one it holds is ignored, and a report
/// of no pull request clears it.
fn card_pull_request(cards: &AppState, event: &AgentEvent) -> Option<Option<u64>> {
    cards
        .sessions
        .iter()
        .filter(|(id, s)| {
            **id == event.session_id
                || (event.pane_id.is_some()
                    && s.pane_id == event.pane_id
                    && s.agent_id == event.agent_id)
        })
        .max_by_key(|(_, s)| s.last_activity)
        .map(|(_, s)| s.pull_request.as_ref().map(|pr| pr.number))
}

/// The `deck.status` value for a card status.
pub fn status_name(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "thinking",
        SessionStatus::Working => "working",
        SessionStatus::Compacting => "compacting",
        SessionStatus::WaitingForInput => "waiting",
        SessionStatus::Idle => "idle",
        SessionStatus::Error => "error",
        SessionStatus::Blocked => "blocked",
        SessionStatus::Unknown => "unknown",
    }
}

/// The `deck.agent` value: the agent's registry name (`claude`, `opencode`,
/// …), or `none` for a pane running something the deck does not recognise.
pub fn agent_name(agent: &AgentType) -> &'static str {
    crate::agent_registry::spec(agent)
        .detect_basenames
        .first()
        .copied()
        .unwrap_or("none")
}

fn identity_attributes(start: &RunStart) -> Vec<Attribute> {
    vec![
        Attribute::str(AttrKey::RunId, &start.run_id),
        Attribute::str(AttrKey::PaneId, &start.pane_id),
    ]
}

fn status_span(run: &ActiveRun, open: OpenStatus, now: u64) -> SpanRecord {
    let mut attributes = identity_attributes(&run.start_info);
    attributes.push(Attribute::str(AttrKey::Status, open.name));
    SpanRecord {
        trace_id: run.trace_id,
        span_id: open.span_id,
        parent_span_id: Some(run.span_id),
        name: SPAN_AGENT_STATUS,
        start_unix_nanos: open.start,
        end_unix_nanos: now.max(open.start),
        attributes,
        events: Vec::new(),
    }
}

fn run_span(run: ActiveRun, reason: EndReason, exit_code: Option<i32>, now: u64) -> SpanRecord {
    let start = &run.start_info;
    let mut attributes = identity_attributes(start);
    attributes.push(Attribute::str(AttrKey::Agent, agent_name(&start.agent)));
    attributes.extend(Attribute::opt_str(AttrKey::Model, start.model.as_deref()));
    if let Some(orch) = &start.orchestration {
        attributes.extend(Attribute::opt_str(AttrKey::Role, orch.role.as_deref()));
        attributes.extend(Attribute::opt_str(
            AttrKey::OrchestrationName,
            orch.name.as_deref(),
        ));
        attributes.extend(Attribute::opt_str(
            AttrKey::OrchestrationId,
            orch.id.as_deref(),
        ));
        attributes.push(Attribute::bool(AttrKey::Orchestrator, orch.orchestrator));
    }
    attributes.extend(Attribute::opt_str(
        AttrKey::InstructionsCommit,
        run.context.instructions_commit.as_deref(),
    ));
    attributes.extend(Attribute::opt_str(
        AttrKey::RepositoryName,
        run.context.repository_name.as_deref(),
    ));
    attributes.extend(Attribute::opt_str(
        AttrKey::InvokingSkill,
        start.invoking_skill.as_deref(),
    ));
    if let Some(issue) = start.issue_number {
        attributes.push(Attribute::int(AttrKey::IssueNumber, saturating_i64(issue)));
    }
    if let Some(pr) = run.pr_number {
        attributes.push(Attribute::int(AttrKey::PrNumber, saturating_i64(pr)));
    }
    attributes.push(Attribute::str(AttrKey::EndReason, reason.as_str()));
    if let Some(code) = exit_code {
        attributes.push(Attribute::int(AttrKey::ExitCode, i64::from(code)));
    }
    attributes.push(Attribute::int(
        AttrKey::InterventionsCount,
        saturating_i64(run.interventions),
    ));
    attributes.push(Attribute::int(
        AttrKey::StatusChangesCount,
        saturating_i64(run.status_changes),
    ));
    SpanRecord {
        trace_id: run.trace_id,
        span_id: run.span_id,
        parent_span_id: None,
        name: SPAN_AGENT_RUN,
        start_unix_nanos: run.start,
        end_unix_nanos: now.max(run.start),
        attributes,
        events: run.events,
    }
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::telemetry::span::AttrValue;

    pub(crate) fn event(pane: &str, event_type: EventType) -> AgentEvent {
        serde_json::from_value(serde_json::json!({
            "session_id": format!("session-{pane}"),
            "agent_type": "claude_code",
            "event_type": event_type,
            "timestamp": "2026-10-22T10:00:00Z",
            "pane_id": pane,
        }))
        .expect("a minimal AgentEvent decodes")
    }

    pub(crate) fn start(pane: &str) -> RunStart {
        RunStart {
            run_id: format!("run-{pane}"),
            pane_id: pane.to_string(),
            agent_id: None,
            agent: AgentType::ClaudeCode,
            orchestration: None,
            model: None,
            invoking_skill: None,
            issue_number: None,
        }
    }

    /// A daemon pull-request report for `pane`'s card: `number` is the pull
    /// request it has (`None`: it has none), `revision` the report's revision.
    pub(crate) fn pr_report(pane: &str, number: Option<u64>, revision: Option<u64>) -> AgentEvent {
        let mut pr = event(pane, EventType::PullRequest);
        if let Some(number) = number {
            pr.metadata.insert(
                crate::event::PULL_REQUEST_METADATA_KEY.into(),
                format!(r#"{{"number":{number},"url":"https://github.com/o/r/pull/{number}","state":"open"}}"#),
            );
        }
        if let Some(revision) = revision {
            pr.metadata.insert(
                crate::event::PULL_REQUEST_REVISION_METADATA_KEY.into(),
                revision.to_string(),
            );
        }
        pr
    }

    fn attr(span: &SpanRecord, key: AttrKey) -> Option<&AttrValue> {
        span.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| &a.value)
    }

    fn run_span_of(spans: &[SpanRecord]) -> &SpanRecord {
        spans
            .iter()
            .find(|s| s.name == SPAN_AGENT_RUN)
            .expect("an agent.run span")
    }

    #[test]
    fn a_run_with_status_intervals_and_counters() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 100);
        tracker.observe_event(&event("p1", EventType::SessionStart), 110);
        tracker.observe_event(&event("p1", EventType::ToolStart), 120);
        tracker.observe_event(&event("p1", EventType::ToolStart), 125);
        tracker.observe_event(&event("p1", EventType::Idle), 130);
        tracker.end_run("p1", EndReason::Exited, Some(0), 140);
        let spans = tracker.take_finished();

        let statuses: Vec<_> = spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_STATUS)
            .map(|s| {
                (
                    attr(s, AttrKey::Status).cloned(),
                    s.start_unix_nanos,
                    s.end_unix_nanos,
                )
            })
            .collect();
        let run = run_span_of(&spans);
        assert!(
            statuses
                .iter()
                .any(|(s, ..)| *s == Some(AttrValue::Str("working".into()))),
            "{statuses:?}"
        );
        assert_eq!(
            statuses.last().unwrap(),
            &(Some(AttrValue::Str("idle".into())), 130, 140)
        );
        for status in spans.iter().filter(|s| s.name == SPAN_AGENT_STATUS) {
            assert_eq!(status.parent_span_id, Some(run.span_id));
            assert_eq!(status.trace_id, run.trace_id);
        }
        assert_eq!(
            attr(run, AttrKey::StatusChangesCount),
            Some(&AttrValue::Int(statuses.len() as i64))
        );
        assert_eq!(
            run.events
                .iter()
                .filter(|e| e.name == EVENT_STATUS_CHANGE)
                .count(),
            statuses.len()
        );
        assert_eq!(
            attr(run, AttrKey::EndReason),
            Some(&AttrValue::Str("exited".into()))
        );
        assert_eq!(attr(run, AttrKey::ExitCode), Some(&AttrValue::Int(0)));
        assert_eq!(
            attr(run, AttrKey::Agent),
            Some(&AttrValue::Str("claude".into()))
        );
        assert_eq!(
            attr(run, AttrKey::RunId),
            Some(&AttrValue::Str("run-p1".into()))
        );
        assert_eq!((run.start_unix_nanos, run.end_unix_nanos), (100, 140));
        assert!(attr(run, AttrKey::Role).is_none());
        assert!(attr(run, AttrKey::Model).is_none());
        assert!(attr(run, AttrKey::InstructionsCommit).is_none());
    }

    #[test]
    fn human_input_is_coalesced_into_bursts() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        let second = 1_000_000_000;
        for t in [1, 2, 3, 4] {
            tracker.observe_human_input("p1", t * second);
        }
        tracker.observe_human_input("p1", 20 * second);
        tracker.observe_human_input("unknown-pane", 21 * second);
        tracker.end_run("p1", EndReason::Closed, None, 30 * second);
        let spans = tracker.take_finished();
        let run = run_span_of(&spans);
        assert_eq!(
            attr(run, AttrKey::InterventionsCount),
            Some(&AttrValue::Int(2))
        );
        assert_eq!(
            run.events
                .iter()
                .filter(|e| e.name == EVENT_INTERVENTION)
                .count(),
            2
        );
        assert!(attr(run, AttrKey::ExitCode).is_none());
    }

    #[test]
    fn delegation_records_only_the_other_pane() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("orch"), 0);
        tracker.begin_run(start("worker"), 0);
        tracker.observe_delegation("orch", "worker", 5);
        tracker.end_all(EndReason::DaemonShutdown, 9);
        let spans = tracker.take_finished();
        for span in spans.iter().filter(|s| s.name == SPAN_AGENT_RUN) {
            assert_eq!(
                attr(span, AttrKey::EndReason),
                Some(&AttrValue::Str("daemon_shutdown".into()))
            );
            let delegation = span
                .events
                .iter()
                .find(|e| e.name == EVENT_DELEGATION)
                .expect("a delegation event");
            let pane = attr(span, AttrKey::PaneId).unwrap();
            let expected = if *pane == AttrValue::Str("orch".into()) {
                Attribute::str(AttrKey::DelegationToPane, "worker")
            } else {
                Attribute::str(AttrKey::DelegationFromPane, "orch")
            };
            assert_eq!(delegation.attributes, vec![expected]);
        }
    }

    #[test]
    fn orchestration_context_and_pr_attributes() {
        let mut tracker = RunTracker::new();
        let mut s = start("p1");
        s.orchestration = Some(OrchestrationInfo {
            role: Some("coder".into()),
            name: Some("tdd".into()),
            id: Some("orch-1".into()),
            orchestrator: false,
        });
        tracker.begin_run(s, 0);
        tracker.set_context(
            "run-p1",
            RunContext {
                instructions_commit: Some("a".repeat(40)),
                repository_name: Some("dot-agent-deck".into()),
            },
        );
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&pr_report("p1", Some(1741), None), 1);
        tracker.end_run("p1", EndReason::Exited, Some(3), 2);
        let spans = tracker.take_finished();
        let run = run_span_of(&spans);
        assert_eq!(
            attr(run, AttrKey::Role),
            Some(&AttrValue::Str("coder".into()))
        );
        assert_eq!(
            attr(run, AttrKey::OrchestrationName),
            Some(&AttrValue::Str("tdd".into()))
        );
        assert_eq!(
            attr(run, AttrKey::OrchestrationId),
            Some(&AttrValue::Str("orch-1".into()))
        );
        assert_eq!(
            attr(run, AttrKey::Orchestrator),
            Some(&AttrValue::Bool(false))
        );
        assert_eq!(
            attr(run, AttrKey::RepositoryName),
            Some(&AttrValue::Str("dot-agent-deck".into()))
        );
        assert_eq!(attr(run, AttrKey::PrNumber), Some(&AttrValue::Int(1741)));
        assert_eq!(attr(run, AttrKey::ExitCode), Some(&AttrValue::Int(3)));
    }

    #[test]
    fn a_daemon_pane_closed_end_closes_the_run_and_other_generations_are_ignored() {
        let mut tracker = RunTracker::new();
        let mut s = start("p1");
        s.agent_id = Some("agent-2".into());
        tracker.begin_run(s, 0);
        let mut stale = event("p1", EventType::ToolStart);
        stale.agent_id = Some("agent-1".into());
        tracker.observe_event(&stale, 1);
        let mut closed = event("p1", EventType::SessionEnd);
        closed
            .metadata
            .insert(DAEMON_PANE_CLOSED_METADATA_KEY.into(), "1".into());
        tracker.observe_event(&closed, 2);
        assert!(tracker.run_id("p1").is_none());
        let spans = tracker.take_finished();
        assert_eq!(spans.len(), 1, "no status interval from another generation");
        assert_eq!(
            attr(&spans[0], AttrKey::EndReason),
            Some(&AttrValue::Str("closed".into()))
        );
    }

    #[test]
    fn a_restarted_pane_ends_the_previous_run_as_unknown() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        let mut next = start("p1");
        next.run_id = "run-p1-b".into();
        tracker.begin_run(next, 5);
        assert_eq!(tracker.run_id("p1"), Some("run-p1-b"));
        let spans = tracker.take_finished();
        assert_eq!(
            attr(&spans[0], AttrKey::EndReason),
            Some(&AttrValue::Str("unknown".into()))
        );
    }

    #[test]
    fn a_late_exit_of_a_replaced_generation_leaves_the_new_run_open() {
        let mut tracker = RunTracker::new();
        let mut s = start("p1");
        s.agent_id = Some("agent-2".into());
        tracker.begin_run(s, 0);
        tracker.end_agent_run("p1", Some("agent-1"), EndReason::Exited, None, 1);
        assert_eq!(tracker.run_id("p1"), Some("run-p1"));
        tracker.end_agent_run("p1", Some("agent-2"), EndReason::Exited, None, 2);
        assert!(tracker.run_id("p1").is_none());
    }

    fn statuses(spans: &[SpanRecord]) -> Vec<AttrValue> {
        spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_STATUS)
            .filter_map(|s| attr(s, AttrKey::Status).cloned())
            .collect()
    }

    /// R1: an agent's first events can arrive before its run is committed.
    /// They are held for the prepared run, not dropped.
    #[test]
    fn events_before_the_commit_are_kept_for_the_prepared_run() {
        let mut tracker = RunTracker::new();
        let mut s = start("p1");
        s.agent_id = Some("agent-1".into());
        tracker.prepare_run(s, 100);
        for event_type in [EventType::SessionStart, EventType::ToolStart] {
            let mut early = event("p1", event_type);
            early.agent_id = Some("agent-1".into());
            tracker.observe_event(&early, 105);
        }
        assert!(tracker.run_id("p1").is_none(), "not open yet");
        tracker.commit_run("run-p1");
        tracker.end_run("p1", EndReason::Exited, None, 120);
        let spans = tracker.take_finished();
        assert_eq!(
            statuses(&spans).last(),
            Some(&AttrValue::Str("working".into())),
            "{:?}",
            statuses(&spans)
        );
        let run = run_span_of(&spans);
        assert_eq!(
            run.start_unix_nanos, 100,
            "the run starts when it was prepared"
        );
        assert_eq!(tracker.held(), (0, 0));
    }

    #[test]
    fn an_aborted_run_leaves_nothing() {
        let mut tracker = RunTracker::new();
        tracker.prepare_run(start("p1"), 1);
        tracker.observe_event(&event("p1", EventType::SessionStart), 2);
        tracker.abort_run("run-p1");
        tracker.commit_run("run-p1");
        assert!(tracker.take_finished().is_empty());
        assert_eq!(tracker.held(), (0, 0));
    }

    /// A respawn prepares the next generation while the previous one is still
    /// open: each takes the events naming it.
    #[test]
    fn a_prepared_generation_and_the_open_one_each_take_their_own_events() {
        let mut tracker = RunTracker::new();
        let mut old = start("p1");
        old.agent_id = Some("agent-1".into());
        tracker.begin_run(old, 0);
        let mut next = start("p1");
        next.run_id = "run-p1-b".into();
        next.agent_id = Some("agent-2".into());
        tracker.prepare_run(next, 5);
        for (agent, event_type) in [
            ("agent-1", EventType::SessionStart),
            ("agent-2", EventType::SessionStart),
            ("agent-1", EventType::ToolStart),
            ("agent-2", EventType::Idle),
        ] {
            let mut e = event("p1", event_type);
            e.agent_id = Some(agent.into());
            tracker.observe_event(&e, 6);
        }
        tracker.end_agent_run("p1", Some("agent-1"), EndReason::Closed, None, 8);
        tracker.commit_run("run-p1-b");
        tracker.end_run("p1", EndReason::Exited, None, 9);
        let spans = tracker.take_finished();
        let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
        assert_eq!(runs.len(), 2);
        assert_eq!(
            attr(runs[0], AttrKey::EndReason),
            Some(&AttrValue::Str("closed".into()))
        );
        let old: Vec<_> = spans
            .iter()
            .filter(|s| s.trace_id == runs[0].trace_id)
            .cloned()
            .collect();
        let new: Vec<_> = spans
            .iter()
            .filter(|s| s.trace_id == runs[1].trace_id)
            .cloned()
            .collect();
        assert_eq!(
            statuses(&old).last(),
            Some(&AttrValue::Str("working".into()))
        );
        assert_eq!(statuses(&new).last(), Some(&AttrValue::Str("idle".into())));
        assert!(!statuses(&new).contains(&AttrValue::Str("working".into())));
    }

    /// R3: the tracker's per-run state is gone once the run ends, however
    /// many runs came and went.
    #[test]
    fn many_runs_leave_the_tracker_at_its_baseline() {
        let mut tracker = RunTracker::new();
        for i in 0..200u64 {
            let pane = format!("pane-{i}");
            let mut s = start(&pane);
            s.agent_id = Some(format!("agent-{i}"));
            tracker.prepare_run(s, i);
            tracker.commit_run(&format!("run-{pane}"));
            for event_type in [
                EventType::SessionStart,
                EventType::ToolStart,
                EventType::Idle,
            ] {
                let mut e = event(&pane, event_type);
                e.agent_id = Some(format!("agent-{i}"));
                tracker.observe_event(&e, i);
            }
            tracker.observe_human_input(&pane, i);
            tracker.end_agent_run(
                &pane,
                Some(&format!("agent-{i}")),
                EndReason::Exited,
                None,
                i,
            );
            tracker.take_finished();
        }
        assert_eq!(tracker.held(), (0, 0));
        assert!(tracker.finished.is_empty());
        assert_eq!(tracker.live_shadow_states(), 0);
    }

    /// R3: a run's card state lives exactly as long as the run — through a
    /// respawn that replaces a generation, a prepared run that is aborted, a
    /// late exit for a replaced generation, and daemon shutdown.
    #[test]
    fn every_card_state_ends_with_its_run() {
        let mut tracker = RunTracker::new();
        let generation = |pane: &str, agent: &str| {
            let mut s = start(pane);
            s.run_id = format!("run-{pane}-{agent}");
            s.agent_id = Some(agent.to_string());
            s
        };
        let event_of = |pane: &str, agent: &str, t: EventType| {
            let mut e = event(pane, t);
            e.agent_id = Some(agent.to_string());
            e
        };
        for i in 0..50u64 {
            let pane = format!("pane-{}", i % 5);
            let (old, new) = (format!("a{i}"), format!("b{i}"));
            tracker.prepare_run(generation(&pane, &old), i);
            tracker.commit_run(&format!("run-{pane}-{old}"));
            tracker.observe_event(&event_of(&pane, &old, EventType::SessionStart), i);
            assert!(tracker.live_shadow_states() >= 1);
            // A respawn: the new generation replaces the old one's run.
            tracker.begin_run(generation(&pane, &new), i);
            tracker.observe_event(&event_of(&pane, &new, EventType::ToolStart), i);
            // The old generation's exit arrives late and ends nothing.
            tracker.end_agent_run(&pane, Some(&old), EndReason::Exited, None, i);
            assert!(tracker.run_id(&pane).is_some_and(|id| id.ends_with(&new)));
            // A spawn refused after preparing holds no card state.
            tracker.prepare_run(generation(&pane, "refused"), i);
            tracker.abort_run(&format!("run-{pane}-refused"));
            if i % 2 == 0 {
                tracker.end_agent_run(&pane, Some(&new), EndReason::Exited, None, i);
            }
            tracker.take_finished();
            assert_eq!(
                tracker.live_shadow_states(),
                tracker.held().0,
                "after run {i}"
            );
        }
        assert!(tracker.live_shadow_states() > 0, "some runs are still open");
        assert_eq!(tracker.live_shadow_states(), tracker.held().0);
        tracker.end_all(EndReason::DaemonShutdown, 99);
        tracker.take_finished();
        assert_eq!(tracker.held(), (0, 0));
        assert_eq!(tracker.live_shadow_states(), 0);
    }

    /// R7: the run's pull request is the card's — a newer report replaces
    /// it, an older one is ignored, and an explicit "none" clears it.
    #[test]
    fn the_pull_request_follows_the_card() {
        let pr_of = |reports: &[AgentEvent]| {
            let mut tracker = RunTracker::new();
            tracker.begin_run(start("p1"), 0);
            tracker.observe_event(&event("p1", EventType::SessionStart), 1);
            for report in reports {
                tracker.observe_event(report, 2);
            }
            tracker.end_run("p1", EndReason::Exited, None, 3);
            let spans = tracker.take_finished();
            attr(run_span_of(&spans), AttrKey::PrNumber).cloned()
        };
        assert_eq!(
            pr_of(&[pr_report("p1", Some(10), Some(2))]),
            Some(AttrValue::Int(10))
        );
        assert_eq!(
            pr_of(&[
                pr_report("p1", Some(10), Some(2)),
                pr_report("p1", Some(11), Some(3))
            ]),
            Some(AttrValue::Int(11)),
            "a newer revision replaces it"
        );
        assert_eq!(
            pr_of(&[
                pr_report("p1", Some(10), Some(2)),
                pr_report("p1", Some(5), Some(1))
            ]),
            Some(AttrValue::Int(10)),
            "an older revision is ignored"
        );
        assert_eq!(
            pr_of(&[
                pr_report("p1", Some(10), Some(2)),
                pr_report("p1", None, Some(3))
            ]),
            None,
            "an explicit no-PR report clears it"
        );
        assert_eq!(
            pr_of(&[
                pr_report("p1", Some(10), Some(2)),
                pr_report("p1", None, Some(1))
            ]),
            Some(AttrValue::Int(10)),
            "an older clear is ignored too"
        );
    }

    #[test]
    fn agent_names_come_from_the_registry() {
        assert_eq!(agent_name(&AgentType::ClaudeCode), "claude");
        assert_eq!(agent_name(&AgentType::OpenCode), "opencode");
        assert_eq!(agent_name(&AgentType::Codex), "codex");
        assert_eq!(agent_name(&AgentType::None), "none");
    }

    #[test]
    fn a_spawn_names_its_agent_and_orchestration_as_far_as_it_knows_them() {
        let membership = crate::agent_pty::TabMembership::Orchestration {
            name: "tdd".into(),
            role_index: 0,
            role_name: "orchestrator".into(),
            is_start_role: true,
            orchestration_cwd: Some("/private/path".into()),
            display_title: Some("private title".into()),
            orchestration_id: Some("orch-1".into()),
        };
        let start = RunStart::for_spawn(
            "pane-1",
            "agent-1",
            None,
            Some("/usr/local/bin/claude --model haiku"),
            Some(&membership),
        );
        assert_eq!(
            start.agent,
            AgentType::ClaudeCode,
            "detected from the command"
        );
        assert_eq!(start.agent_id.as_deref(), Some("agent-1"));
        assert!(start.run_id.is_empty(), "minted by prepare_run");
        assert_eq!(
            start.orchestration,
            Some(OrchestrationInfo {
                role: Some("orchestrator".into()),
                name: Some("tdd".into()),
                id: Some("orch-1".into()),
                orchestrator: true,
            })
        );

        let declared = RunStart::for_spawn(
            "pane-2",
            "agent-2",
            Some(&AgentType::OpenCode),
            Some("claude"),
            None,
        );
        assert_eq!(declared.agent, AgentType::OpenCode, "a declared kind wins");
        assert_eq!(declared.orchestration, None);
        let unknown = RunStart::for_spawn("pane-3", "agent-3", None, Some("/bin/sh -c x"), None);
        assert_eq!(unknown.agent, AgentType::None);
    }
}
