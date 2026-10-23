//! The run tracker: turns what the daemon sees — agents starting and ending,
//! the events on its broadcast, human input — into finished
//! `agent.run`, `agent.status`, `agent.operation` and `agent.ci` spans.
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
//! them, and so are its checks, which make the run's `agent.ci` stretches
//! ([`super::ci`]). That state holds the events in memory, as every client's
//! does, minus what a tool call reports of its arguments (`KeptEvent`);
//! nothing from it but the status enum and the pull request's number and check
//! counts reaches a span, and it is dropped with the run, so the tracker holds
//! nothing per pane once a pane's runs have ended.
//!
//! **A shell command is an `agent.operation`**, from the shell tool's start to
//! its end as the agent's hooks report them. Its command text is classified
//! as the event arrives ([`super::operation::describe_tool`]) and only the
//! classification is kept: the copy of the event the tracker holds — in a
//! run's cards, or waiting for its run to open — has the tool's detail and the
//! command taken out.
//!
//! **A run is prepared before its agent starts.** [`RunTracker::prepare_run`]
//! is called before the agent process exists, so no event the agent causes can
//! arrive before the deck knows its run; events for a prepared run are held
//! (up to [`MAX_PENDING_EVENTS`]) and applied when [`RunTracker::commit_run`]
//! opens it, and [`RunTracker::abort_run`] forgets a run whose agent never
//! started.
//!
//! **A replaced run waits for its own end.** A respawn can open the new
//! generation's run before the old generation's end is seen; the old run is
//! then set aside, still open, and ended by its own generation's end, with that
//! end's reason and time. One whose end does not come within
//! [`REPLACED_RUN_GRACE_NANOS`], or that would hold more than
//! [`MAX_REPLACED_RUNS`] aside, or that is still aside at daemon shutdown, ends
//! as [`EndReason::Unknown`] at the time it was replaced.

use std::collections::{HashMap, VecDeque};

use crate::event::{AgentEvent, AgentType, DAEMON_PANE_CLOSED_METADATA_KEY, EventType};
use crate::state::{AppState, SessionStatus};

use super::ci::{CiEffect, RunCi};
use super::operation::{Description, describe_tool, is_shell_tool};
use super::run_context::RunContext;
use super::span::{
    Attribute, SpanEventRecord, SpanId, SpanRecord, TraceId, new_span_id, new_trace_id,
};
use super::{
    AttrKey, EVENT_CI_CHANGE, EVENT_INTERVENTION, EVENT_STATUS_CHANGE, SPAN_AGENT_CI,
    SPAN_AGENT_OPERATION, SPAN_AGENT_RUN, SPAN_AGENT_STATUS,
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

/// How long a replaced run waits for its own generation's end before it is
/// ended as [`EndReason::Unknown`]. A respawn stops the old agent, so its end
/// follows within moments; this only bounds an end that never comes.
pub const REPLACED_RUN_GRACE_NANOS: u64 = 60_000_000_000;

/// Replaced runs waiting for their own end at once, across every pane. Past
/// it the longest-waiting one is ended as [`EndReason::Unknown`].
pub const MAX_REPLACED_RUNS: usize = 64;

/// Shell commands open at once in one run. Past it the oldest is ended where
/// it stands, so starts whose end never arrives cannot pile up.
pub const MAX_OPEN_OPERATIONS: usize = 64;

/// The tool call ids one run remembers having seen, newest kept: an end
/// naming one of them ends nothing unless that call is still open.
pub const MAX_SEEN_CALL_IDS: usize = 256;

/// The event metadata key carrying the agent's id for one tool call, which
/// pairs a call's end with its start when several run at once.
const TOOL_CALL_ID_METADATA_KEY: &str = "tool_use_id";

/// Where a hook puts a shell command whole (`tool_detail` holds its first
/// line, cut short).
const BASH_COMMAND_METADATA_KEY: &str = "bash_command";

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
    /// command names — read as the spawn path reads it, through leading
    /// `VAR=VALUE` assignments and `env`, `sudo` and shell launchers
    /// ([`AgentType::from_command`]) — and the orchestration the pane is a
    /// role of. The run id is left for [`super::Telemetry::prepare_run`] to
    /// mint.
    pub fn for_spawn(
        pane_id: &str,
        agent_id: &str,
        declared: Option<&AgentType>,
        command: Option<&str>,
        membership: Option<&crate::agent_pty::TabMembership>,
    ) -> Self {
        let detected = AgentType::from_command(command);
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

/// A shell command still running: its classification only, never its text.
struct OpenOperation {
    span_id: SpanId,
    start: u64,
    operation: Description,
    /// The agent's id for the call, when it reports one.
    call_id: Option<String>,
}

/// A run whose agent is starting: see [`RunTracker::prepare_run`].
struct PendingRun {
    start: RunStart,
    prepared_at: u64,
    context: RunContext,
    events: Vec<(KeptEvent, u64)>,
    /// Its agent already ended, before the run was committed: how and when,
    /// applied by [`RunTracker::commit_run`] once the run opens.
    ended: Option<(EndReason, Option<i32>, u64)>,
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
    operations: Vec<OpenOperation>,
    operations_count: u64,
    /// The call ids this run's shell starts and ends named, at most
    /// [`MAX_SEEN_CALL_IDS`], oldest first.
    seen_call_ids: VecDeque<String>,
    ci: RunCi,
}

/// A run a newer generation of its pane replaced before its own end was seen:
/// see [`RunTracker::begin_run`].
struct ReplacedRun {
    run: ActiveRun,
    /// When the newer generation's run opened.
    at: u64,
}

impl ActiveRun {
    fn push_event(&mut self, event: SpanEventRecord) {
        if self.events.len() < MAX_EVENTS_PER_RUN {
            self.events.push(event);
        }
    }

    /// See [`RunTracker::observe_run_input`].
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
    /// Runs replaced before their own end was seen, the longest-waiting first.
    replaced: Vec<ReplacedRun>,
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
                ended: None,
            });
    }

    /// The agent of the prepared run `run_id` did not start: forget the run.
    pub fn abort_run(&mut self, run_id: &str) {
        self.pending.remove(run_id);
    }

    /// The agent of the prepared run `run_id` started: open its run, from the
    /// time it was prepared, and apply the events held for it. A run whose
    /// agent already ended ([`Self::end_agent_run`]) is ended straight after,
    /// as of when it did.
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
            self.observe_kept(event, at);
        }
        if let Some((reason, exit_code, at)) = pending.ended {
            self.end_run(&pane_id, reason, exit_code, at);
        }
    }

    /// Open a run. A run still open on the same pane is replaced: when both
    /// know their generation and the two differ, it is set aside to be ended
    /// by its own generation's end ([`Self::end_agent_run`], or the pane's
    /// close broadcast naming it); otherwise it is ended now with
    /// [`EndReason::Unknown`], since no end could be told apart as its own.
    pub fn begin_run(&mut self, start: RunStart, now: u64) {
        self.expire_replaced(now);
        if let Some(old) = self.runs.remove(&start.pane_id) {
            let own_end_can_come = matches!(
                (old.start_info.agent_id.as_deref(), start.agent_id.as_deref()),
                (Some(a), Some(b)) if a != b
            );
            if own_end_can_come {
                self.replaced.push(ReplacedRun { run: old, at: now });
                if self.replaced.len() > MAX_REPLACED_RUNS {
                    let oldest = self.replaced.remove(0);
                    self.finish(oldest.run, EndReason::Unknown, None, oldest.at);
                }
            } else {
                self.finish(old, EndReason::Unknown, None, now);
            }
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
                operations: Vec::new(),
                operations_count: 0,
                seen_call_ids: VecDeque::new(),
                ci: RunCi::default(),
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
            .chain(self.replaced.iter_mut().map(|r| &mut r.run))
            .find(|run| run.start_info.run_id == run_id)
        {
            run.context = context;
        }
    }

    /// Apply one event from the daemon's broadcast.
    ///
    /// An event that names no pane is applied by its session, as the daemon
    /// applies one from an agent it owns: to the run whose own cards hold that
    /// session on the run's pane, when exactly one does. The run's copy names
    /// the pane, which a card state with panes of its own needs to admit it.
    pub fn observe_event(&mut self, event: &AgentEvent, now: u64) {
        self.observe_kept(KeptEvent::of(event), now);
    }

    /// [`Self::observe_event`], for the copy of the event the tracker keeps.
    fn observe_kept(&mut self, mut kept: KeptEvent, now: u64) {
        let Some(pane_id) = kept.event.pane_id.clone() else {
            if let Some(pane_id) = self.pane_holding_session(&kept.event.session_id) {
                kept.event.pane_id = Some(pane_id.clone());
                self.apply_to_run(&pane_id, kept, now);
            }
            return;
        };
        // A prepared run takes the events of its own generation, and, while
        // the pane has no open run, the events that name no generation.
        let pane_has_run = self.runs.contains_key(&pane_id);
        if let Some(pending) = self.pending.values_mut().find(|p| {
            p.start.pane_id == pane_id
                && match (p.start.agent_id.as_deref(), kept.event.agent_id.as_deref()) {
                    (Some(ours), Some(theirs)) => ours == theirs,
                    _ => !pane_has_run,
                }
        }) {
            if pending.events.len() < MAX_PENDING_EVENTS {
                pending.events.push((kept, now));
            }
            return;
        }
        self.apply_to_run(&pane_id, kept, now);
    }

    /// The pane of the one open run whose cards hold `session_id` on that
    /// pane. `None` when no run does, or more than one.
    fn pane_holding_session(&self, session_id: &str) -> Option<String> {
        let mut holding = self.runs.iter().filter(|(pane_id, run)| {
            run.cards
                .sessions
                .get(session_id)
                .is_some_and(|s| s.pane_id.as_deref() == Some(pane_id.as_str()))
        });
        let (pane_id, _) = holding.next()?;
        holding.next().is_none().then(|| pane_id.clone())
    }

    /// Apply `event` to the run open on `pane_id`, unless it names another
    /// generation of the pane. The pane's close broadcast naming a replaced
    /// generation ends that generation's run.
    fn apply_to_run(&mut self, pane_id: &str, kept: KeptEvent, now: u64) {
        let event = &kept.event;
        let closed = event.event_type == EventType::SessionEnd
            && event.metadata.contains_key(DAEMON_PANE_CLOSED_METADATA_KEY);
        let Some(run) = self.runs.get_mut(pane_id) else {
            return;
        };
        if let (Some(ours), Some(theirs)) = (
            run.start_info.agent_id.as_deref(),
            event.agent_id.as_deref(),
        ) && ours != theirs
        {
            if closed {
                self.end_replaced(pane_id, theirs, EndReason::Closed, None, now);
            }
            return;
        }
        if closed {
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
            run.pr_number = pr.as_ref().map(|pr| pr.number);
            let checks = pr.and_then(|pr| pr.checks);
            for effect in run.ci.observe(run.pr_number, checks, now, new_span_id) {
                record_ci(run, effect, now, &mut self.finished);
            }
        }
        if is_shell_tool(event.tool_name.as_deref()) {
            observe_operation(run, &kept, now, &mut self.finished);
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

    /// [`Self::observe_run_input`] for whichever run is open on `pane_id`.
    #[cfg(test)]
    pub fn observe_human_input(&mut self, pane_id: &str, now: u64) {
        if let Some(run) = self.runs.get_mut(pane_id) {
            run.human_input(now);
        }
    }

    /// Input from a person at an attached client reached `pane_id`, while the
    /// run open on it, or replaced and waiting for its end, is `run_id`. Input
    /// the deck injects itself (a delegated task, an inbox delivery) must not
    /// be reported here. Keystrokes less than [`INTERVENTION_COALESCE_NANOS`]
    /// apart count as one intervention.
    pub fn observe_run_input(&mut self, pane_id: &str, run_id: &str, now: u64) {
        let open = self
            .runs
            .get_mut(pane_id)
            .filter(|run| run.start_info.run_id == run_id);
        if let Some(run) = open.or_else(|| {
            self.replaced
                .iter_mut()
                .map(|r| &mut r.run)
                .find(|run| run.start_info.pane_id == pane_id && run.start_info.run_id == run_id)
        }) {
            run.human_input(now);
        }
    }

    /// End the run on `pane_id`, if one is open.
    pub fn end_run(&mut self, pane_id: &str, reason: EndReason, exit_code: Option<i32>, now: u64) {
        if let Some(run) = self.runs.remove(pane_id) {
            self.finish(run, reason, exit_code, now);
        }
    }

    /// End the replaced run of generation `agent_id` on `pane_id`, if one is
    /// waiting. Whether one was.
    fn end_replaced(
        &mut self,
        pane_id: &str,
        agent_id: &str,
        reason: EndReason,
        exit_code: Option<i32>,
        now: u64,
    ) -> bool {
        let Some(i) = self.replaced.iter().position(|r| {
            r.run.start_info.pane_id == pane_id
                && r.run.start_info.agent_id.as_deref() == Some(agent_id)
        }) else {
            return false;
        };
        let replaced = self.replaced.remove(i);
        self.finish(replaced.run, reason, exit_code, now);
        true
    }

    /// End every replaced run whose own end has not come within
    /// [`REPLACED_RUN_GRACE_NANOS`] as [`EndReason::Unknown`], at the time it
    /// was replaced.
    fn expire_replaced(&mut self, now: u64) {
        let (expired, waiting) = std::mem::take(&mut self.replaced)
            .into_iter()
            .partition(|r| now.saturating_sub(r.at) >= REPLACED_RUN_GRACE_NANOS);
        self.replaced = waiting;
        for r in expired {
            self.finish(r.run, EndReason::Unknown, None, r.at);
        }
    }

    /// Finish `run`'s spans: its held-back input counted, its open status
    /// interval and the run itself ended at `now`.
    fn finish(&mut self, mut run: ActiveRun, reason: EndReason, exit_code: Option<i32>, now: u64) {
        if let Some(held) = self.held_input.as_mut() {
            for written in held(&run.start_info.pane_id, &run.start_info.run_id) {
                run.human_input(written);
            }
        }
        if let Some(open) = run.status.take() {
            self.finished.push(status_span(&run, open, now));
        }
        for open in std::mem::take(&mut run.operations) {
            self.finished.push(operation_span(&run, open, now));
        }
        if let Some(effect) = run.ci.end_run() {
            record_ci(&mut run, effect, now, &mut self.finished);
        }
        self.finished.push(run_span(run, reason, exit_code, now));
    }

    /// [`Self::end_run`], but only when the run open on `pane_id` belongs to
    /// `agent_id` (or either side does not know its agent). An exit observed
    /// late for a generation a respawn already replaced must not end the new
    /// generation's run: it ends the replaced run instead, with this reason
    /// and time.
    ///
    /// An agent that exits before its run is committed — a fast exit can reach
    /// its PTY's end before the spawn path publishes it — ends its prepared
    /// run instead, which [`Self::commit_run`] then opens and ends. Its agent
    /// was never registered, so nothing could have closed it: a `closed`
    /// reason, read from it no longer being registered, is `exited`.
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
        if self.runs.contains_key(pane_id) && !other_generation {
            self.end_run(pane_id, reason, exit_code, now);
        } else if let Some(agent_id) = agent_id
            && !self.end_replaced(pane_id, agent_id, reason, exit_code, now)
            && let Some(pending) = self.pending.values_mut().find(|p| {
                p.start.pane_id == pane_id && p.start.agent_id.as_deref() == Some(agent_id)
            })
        {
            let reason = match reason {
                EndReason::Closed => EndReason::Exited,
                other => other,
            };
            pending.ended.get_or_insert((reason, exit_code, now));
        }
        self.expire_replaced(now);
    }

    /// End every open run and forget every prepared one, for daemon
    /// shutdown. A replaced run still waiting for its own end ends as
    /// [`EndReason::Unknown`], at the time it was replaced.
    pub fn end_all(&mut self, reason: EndReason, now: u64) {
        self.pending.clear();
        for r in std::mem::take(&mut self.replaced) {
            self.finish(r.run, EndReason::Unknown, None, r.at);
        }
        let panes: Vec<String> = self.runs.keys().cloned().collect();
        for pane in panes {
            self.end_run(&pane, reason, None, now);
        }
    }

    /// Runs open (replaced ones still waiting for their end included) and
    /// prepared: what the tracker holds between spans.
    #[cfg(test)]
    pub fn held(&self) -> (usize, usize) {
        (self.runs.len() + self.replaced.len(), self.pending.len())
    }

    /// The spans finished since the last call.
    pub fn take_finished(&mut self) -> Vec<SpanRecord> {
        std::mem::take(&mut self.finished)
    }
}

/// A shell tool's start or end reached `run`: open an operation, or end the
/// one it belongs to. An end naming a call ends that call's operation only —
/// or, when the run has never seen that id, the oldest that has no id, since
/// an agent may name the call on its end only. An id seen before (a repeated
/// end, the end of a call the cap ended, a start's id whose operation is gone)
/// ends nothing. An end naming no call ends the oldest. A failed call ends as
/// an `Error` event, which ends it the same way.
fn observe_operation(
    run: &mut ActiveRun,
    kept: &KeptEvent,
    now: u64,
    finished: &mut Vec<SpanRecord>,
) {
    let event = &kept.event;
    let call_id = event.metadata.get(TOOL_CALL_ID_METADATA_KEY);
    let seen_before = call_id.is_some_and(|id| run.seen_call_ids.contains(id));
    if let Some(id) = call_id.filter(|_| !seen_before) {
        if run.seen_call_ids.len() >= MAX_SEEN_CALL_IDS {
            run.seen_call_ids.pop_front();
        }
        run.seen_call_ids.push_back(id.clone());
    }
    match event.event_type {
        EventType::ToolStart => {
            if run.operations.len() >= MAX_OPEN_OPERATIONS {
                let oldest = run.operations.remove(0);
                finished.push(operation_span(run, oldest, now));
            }
            run.operations_count += 1;
            run.operations.push(OpenOperation {
                span_id: new_span_id(),
                start: now,
                operation: kept
                    .operation
                    .clone()
                    .unwrap_or_else(|| describe_tool(event.tool_name.as_deref(), None)),
                call_id: call_id.cloned(),
            });
        }
        EventType::ToolEnd | EventType::Error => {
            let i = match call_id {
                Some(id) => run
                    .operations
                    .iter()
                    .position(|op| op.call_id.as_ref() == Some(id))
                    .or_else(|| {
                        (!seen_before)
                            .then(|| run.operations.iter().position(|op| op.call_id.is_none()))
                            .flatten()
                    }),
                None => (!run.operations.is_empty()).then_some(0),
            };
            if let Some(i) = i {
                let open = run.operations.remove(i);
                finished.push(operation_span(run, open, now));
            }
        }
        _ => {}
    }
}

/// An event as the tracker keeps it: without what a tool call reports of its
/// arguments — the tool's detail, the whole shell command, and the detail of
/// the tool a question asks about — and with the classification of a shell
/// command it starts, made before those were taken out.
struct KeptEvent {
    event: AgentEvent,
    operation: Option<Description>,
}

impl KeptEvent {
    fn of(event: &AgentEvent) -> Self {
        let operation = (event.event_type == EventType::ToolStart
            && is_shell_tool(event.tool_name.as_deref()))
        .then(|| describe_tool(event.tool_name.as_deref(), event.tool_detail.as_deref()));
        let mut event = AgentEvent {
            tool_detail: None,
            ..event.clone()
        };
        event.metadata.remove(BASH_COMMAND_METADATA_KEY);
        if event
            .metadata
            .contains_key(crate::event::QUESTION_METADATA_KEY)
        {
            match event.question() {
                Some(mut question) => {
                    if let Some(tool) = question.tool.as_mut() {
                        tool.detail = None;
                    }
                    event.set_question(&question);
                }
                // The cards read a question that does not decode as none.
                None => {
                    event.metadata.remove(crate::event::QUESTION_METADATA_KEY);
                }
            }
        }
        KeptEvent { event, operation }
    }
}

/// Record what one observation of the run's checks changed.
fn record_ci(run: &mut ActiveRun, effect: CiEffect, now: u64, finished: &mut Vec<SpanRecord>) {
    match effect {
        CiEffect::Change { from, to, pr } => {
            let mut attributes = Vec::with_capacity(3);
            if let Some(from) = from {
                attributes.push(Attribute::str(AttrKey::CiFrom, from.as_str()));
            }
            attributes.push(Attribute::str(AttrKey::CiTo, to.as_str()));
            attributes.push(Attribute::int(AttrKey::PrNumber, saturating_i64(pr)));
            run.push_event(SpanEventRecord {
                name: EVENT_CI_CHANGE,
                time_unix_nanos: now,
                attributes,
            });
        }
        CiEffect::Ended {
            stretch,
            result,
            checks,
        } => {
            let mut attributes = identity_attributes(&run.start_info);
            attributes.push(Attribute::int(
                AttrKey::PrNumber,
                saturating_i64(stretch.pr),
            ));
            if let Some(result) = result {
                attributes.push(Attribute::str(AttrKey::CiResult, result.as_str()));
            }
            if let Some(checks) = checks {
                attributes.push(Attribute::int(
                    AttrKey::CiChecksPassing,
                    i64::from(checks.passing),
                ));
                attributes.push(Attribute::int(
                    AttrKey::CiChecksFailing,
                    i64::from(checks.failing),
                ));
                attributes.push(Attribute::int(
                    AttrKey::CiChecksSkipped,
                    i64::from(checks.skipped),
                ));
            }
            finished.push(SpanRecord {
                trace_id: run.trace_id,
                span_id: stretch.span_id,
                parent_span_id: Some(run.span_id),
                name: SPAN_AGENT_CI,
                start_unix_nanos: stretch.start,
                end_unix_nanos: now.max(stretch.start),
                attributes,
                events: Vec::new(),
            });
        }
    }
}

/// The pull request the card `event` reported on now holds — `Some(None)` when
/// it holds none — or `None` when the run holds no such card, so the report had
/// nothing to land on. Read after the card applied the report, so the card's own
/// rules decide: a report older than the one it holds is ignored, and a report
/// of no pull request clears it.
fn card_pull_request(
    cards: &AppState,
    event: &AgentEvent,
) -> Option<Option<crate::pull_request_info::PullRequestInfo>> {
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
        .map(|(_, s)| s.pull_request.clone())
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

fn operation_span(run: &ActiveRun, open: OpenOperation, now: u64) -> SpanRecord {
    let mut attributes = identity_attributes(&run.start_info);
    let described = &open.operation;
    attributes.extend(Attribute::opt_str(
        AttrKey::OperationProgram,
        described.operation.program.as_deref(),
    ));
    attributes.extend(Attribute::opt_str(
        AttrKey::OperationWord,
        described.operation.word.as_deref(),
    ));
    attributes.extend(Attribute::opt_str(
        AttrKey::OperationCommands,
        described.commands_value().as_deref(),
    ));
    attributes.extend(Attribute::opt_str(
        AttrKey::OperationWrappedProgram,
        described.wrapped.program.as_deref(),
    ));
    attributes.extend(Attribute::opt_str(
        AttrKey::OperationWrappedWord,
        described.wrapped.word.as_deref(),
    ));
    SpanRecord {
        trace_id: run.trace_id,
        span_id: open.span_id,
        parent_span_id: Some(run.span_id),
        name: SPAN_AGENT_OPERATION,
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
    attributes.push(Attribute::int(
        AttrKey::OperationsCount,
        saturating_i64(run.operations_count),
    ));
    attributes.push(Attribute::int(
        AttrKey::CiRoundsCount,
        saturating_i64(run.ci.rounds),
    ));
    attributes.push(Attribute::int(
        AttrKey::CiFailuresCount,
        saturating_i64(run.ci.failures),
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

    fn generation(pane: &str, agent_id: &str) -> RunStart {
        let mut s = start(pane);
        s.run_id = format!("run-{pane}-{agent_id}");
        s.agent_id = Some(agent_id.into());
        s
    }

    fn end_of(spans: &[SpanRecord], run_id: &str) -> (AttrValue, u64) {
        let run = spans
            .iter()
            .find(|s| {
                s.name == SPAN_AGENT_RUN
                    && attr(s, AttrKey::RunId) == Some(&AttrValue::Str(run_id.into()))
            })
            .unwrap_or_else(|| panic!("an agent.run span for {run_id}"));
        (
            attr(run, AttrKey::EndReason).unwrap().clone(),
            run.end_unix_nanos,
        )
    }

    /// Qodo item 18: a respawn's run can open before the old generation's end
    /// is seen. The old run ends when its own end arrives, with that end's
    /// reason and time — by its agent's end or by the pane's close naming it —
    /// and the new run stays open.
    #[test]
    fn a_replaced_run_ends_with_its_own_late_end() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(generation("p1", "a1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&event("p1", EventType::ToolStart), 2);
        tracker.begin_run(generation("p1", "a2"), 10);
        assert!(
            !tracker
                .take_finished()
                .iter()
                .any(|s| s.name == SPAN_AGENT_RUN),
            "no run ended yet"
        );
        assert_eq!(tracker.held(), (2, 0));
        tracker.end_agent_run("p1", Some("a1"), EndReason::Closed, Some(0), 15);
        let spans = tracker.take_finished();
        assert_eq!(
            end_of(&spans, "run-p1-a1"),
            (AttrValue::Str("closed".into()), 15)
        );
        let status = spans
            .iter()
            .find(|s| s.name == SPAN_AGENT_STATUS)
            .expect("the old run's last status interval");
        assert_eq!(
            (attr(status, AttrKey::Status), status.end_unix_nanos),
            (Some(&AttrValue::Str("working".into())), 15)
        );
        assert_eq!(tracker.run_id("p1"), Some("run-p1-a2"));

        tracker.begin_run(generation("p1", "a3"), 20);
        let mut closed = event("p1", EventType::SessionEnd);
        closed.agent_id = Some("a2".into());
        closed
            .metadata
            .insert(DAEMON_PANE_CLOSED_METADATA_KEY.into(), "1".into());
        tracker.observe_event(&closed, 25);
        let spans = tracker.take_finished();
        assert_eq!(
            end_of(&spans, "run-p1-a2"),
            (AttrValue::Str("closed".into()), 25)
        );
        assert_eq!(tracker.run_id("p1"), Some("run-p1-a3"));
        tracker.end_agent_run("p1", Some("a2"), EndReason::Exited, None, 26);
        assert!(tracker.take_finished().is_empty(), "the old run ended once");
        tracker.end_agent_run("p1", Some("a3"), EndReason::Exited, Some(0), 30);
        assert_eq!(
            end_of(&tracker.take_finished(), "run-p1-a3"),
            (AttrValue::Str("exited".into()), 30)
        );
        assert_eq!(tracker.held(), (0, 0));
    }

    /// Qodo item 18: a replaced run whose own end never arrives is still
    /// finished, as `unknown` at the time it was replaced — at shutdown, once
    /// its grace passes, or when too many are waiting.
    #[test]
    fn a_replaced_run_whose_end_never_comes_ends_as_unknown() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(generation("p1", "a1"), 0);
        tracker.begin_run(generation("p1", "a2"), 10);
        tracker.end_all(EndReason::DaemonShutdown, 50);
        let spans = tracker.take_finished();
        assert_eq!(
            end_of(&spans, "run-p1-a1"),
            (AttrValue::Str("unknown".into()), 10)
        );
        assert_eq!(
            end_of(&spans, "run-p1-a2"),
            (AttrValue::Str("daemon_shutdown".into()), 50)
        );
        assert_eq!(tracker.held(), (0, 0));

        tracker.begin_run(generation("p1", "b1"), 100);
        tracker.begin_run(generation("p1", "b2"), 200);
        tracker.end_agent_run(
            "p2",
            Some("x"),
            EndReason::Exited,
            None,
            200 + REPLACED_RUN_GRACE_NANOS,
        );
        assert_eq!(
            end_of(&tracker.take_finished(), "run-p1-b1"),
            (AttrValue::Str("unknown".into()), 200)
        );
        assert_eq!(tracker.held(), (1, 0));

        let t = 300 + REPLACED_RUN_GRACE_NANOS;
        for i in 0..=MAX_REPLACED_RUNS {
            tracker.begin_run(generation("p3", &format!("c{i}")), t + i as u64);
        }
        tracker.begin_run(generation("p3", "last"), t + 1000);
        let spans = tracker.take_finished();
        let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
        assert_eq!(runs.len(), 1, "the longest-waiting one, past the cap");
        assert_eq!(
            end_of(&spans, "run-p3-c0"),
            (AttrValue::Str("unknown".into()), t + 1)
        );
        assert_eq!(tracker.held(), (MAX_REPLACED_RUNS + 2, 0));
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

    /// A fast agent can reach the end of its PTY before the spawn path
    /// commits its run. The end is kept for the prepared run, which the commit
    /// then opens and ends: one run, ended `exited`, nothing left open.
    #[test]
    fn an_end_before_the_commit_ends_the_run_when_it_opens() {
        for (reported, expected) in [
            // Not yet registered reads as closed; nothing could have closed it.
            (EndReason::Closed, "exited"),
            (EndReason::Exited, "exited"),
            (EndReason::DaemonShutdown, "daemon_shutdown"),
        ] {
            let mut tracker = RunTracker::new();
            let mut s = start("p1");
            s.agent_id = Some("agent-1".into());
            tracker.prepare_run(s, 100);
            let mut early = event("p1", EventType::SessionStart);
            early.agent_id = Some("agent-1".into());
            tracker.observe_event(&early, 105);
            tracker.end_agent_run("p1", Some("agent-1"), reported, Some(3), 110);
            assert!(tracker.take_finished().is_empty(), "not committed yet");
            tracker.commit_run("run-p1");
            let spans = tracker.take_finished();
            let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
            assert_eq!(runs.len(), 1, "{reported:?}");
            assert_eq!(
                attr(runs[0], AttrKey::EndReason),
                Some(&AttrValue::Str(expected.into())),
                "{reported:?}"
            );
            assert_eq!(attr(runs[0], AttrKey::ExitCode), Some(&AttrValue::Int(3)));
            assert_eq!(runs[0].end_unix_nanos, 110, "ended when the agent did");
            assert_eq!(
                statuses(&spans).last(),
                Some(&AttrValue::Str("idle".into()))
            );
            assert_eq!(tracker.held(), (0, 0), "{reported:?}");
        }
    }

    /// The early end is for its own generation only: an end naming another
    /// agent, or none, leaves a prepared run to open as usual.
    #[test]
    fn an_early_end_for_another_generation_leaves_the_prepared_run_open() {
        for other in [Some("agent-0"), None] {
            let mut tracker = RunTracker::new();
            let mut s = start("p1");
            s.agent_id = Some("agent-1".into());
            tracker.prepare_run(s, 100);
            tracker.end_agent_run("p1", other, EndReason::Exited, None, 110);
            tracker.commit_run("run-p1");
            assert!(tracker.take_finished().is_empty(), "{other:?}");
            assert_eq!(tracker.run_id("p1"), Some("run-p1"), "{other:?}");
        }
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

    /// An event that names no pane changes the status of the run whose card
    /// holds its session — and of no run when its session is nobody's.
    #[test]
    fn a_status_change_that_names_no_pane_lands_on_its_sessions_run() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.begin_run(start("p2"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&event("p1", EventType::ToolStart), 2);
        tracker.observe_event(&event("p2", EventType::SessionStart), 2);
        let paneless = |session: &str, event_type| {
            let mut e = event("p1", event_type);
            e.pane_id = None;
            e.session_id = session.to_string();
            e
        };
        tracker.observe_event(&paneless("session-p1", EventType::Idle), 3);
        tracker.observe_event(&paneless("session-elsewhere", EventType::ToolStart), 4);
        tracker.end_all(EndReason::Exited, 5);
        let spans = tracker.take_finished();
        let of_pane = |pane: &str| -> Vec<SpanRecord> {
            spans
                .iter()
                .filter(|s| attr(s, AttrKey::PaneId) == Some(&AttrValue::Str(pane.into())))
                .cloned()
                .collect()
        };
        let p1 = of_pane("p1");
        assert_eq!(
            statuses(&p1).last(),
            Some(&AttrValue::Str("idle".into())),
            "{:?}",
            statuses(&p1)
        );
        let idle = p1
            .iter()
            .rev()
            .find(|s| attr(s, AttrKey::Status) == Some(&AttrValue::Str("idle".into())))
            .unwrap();
        assert_eq!(idle.start_unix_nanos, 3);
        let p2 = of_pane("p2");
        assert!(
            !statuses(&p2).contains(&AttrValue::Str("working".into())),
            "{:?}",
            statuses(&p2)
        );
    }

    /// A shell tool event for `pane`: `tool` with `command` as its detail and
    /// `call` as the agent's id for the call.
    fn shell(
        pane: &str,
        event_type: EventType,
        tool: &str,
        command: Option<&str>,
        call: Option<&str>,
    ) -> AgentEvent {
        let mut e = event(pane, event_type);
        e.tool_name = Some(tool.into());
        e.tool_detail = command.map(str::to_string);
        if let Some(call) = call {
            e.metadata
                .insert(TOOL_CALL_ID_METADATA_KEY.into(), call.to_string());
        }
        e
    }

    fn operations(spans: &[SpanRecord]) -> Vec<(Vec<Attribute>, u64, u64)> {
        spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_OPERATION)
            .map(|s| {
                (
                    s.attributes[2..].to_vec(),
                    s.start_unix_nanos,
                    s.end_unix_nanos,
                )
            })
            .collect()
    }

    /// The attributes of a line of one command.
    fn op_attrs(program: Option<&str>, word: Option<&str>) -> Vec<Attribute> {
        let mut out = Attribute::opt_str(AttrKey::OperationProgram, program)
            .into_iter()
            .collect::<Vec<_>>();
        out.extend(Attribute::opt_str(AttrKey::OperationWord, word));
        let command = program.map(|p| word.map_or(p.to_string(), |w| format!("{p} {w}")));
        out.extend(Attribute::opt_str(
            AttrKey::OperationCommands,
            command.as_deref(),
        ));
        out
    }

    /// A chained line's span names each of its commands, and a wrapper's the
    /// command it runs, besides the first command's program and word.
    #[test]
    fn a_chained_or_wrapped_line_names_its_commands() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        for (t, (kind, command)) in [
            (
                EventType::ToolStart,
                "cargo fmt && cargo xtask affected-checks --run",
            ),
            (
                EventType::ToolEnd,
                "cargo fmt && cargo xtask affected-checks --run",
            ),
            (EventType::ToolStart, "cd x && timeout 600 cargo test"),
            (EventType::ToolEnd, "cd x && timeout 600 cargo test"),
        ]
        .into_iter()
        .enumerate()
        {
            tracker.observe_event(
                &shell("p1", kind, "Bash", Some(command), None),
                10 + t as u64,
            );
        }
        tracker.end_run("p1", EndReason::Exited, None, 100);
        let str_attr = |key, value: &str| Attribute::str(key, value);
        assert_eq!(
            operations(&tracker.take_finished()),
            vec![
                (
                    vec![
                        str_attr(AttrKey::OperationProgram, "cargo"),
                        str_attr(AttrKey::OperationWord, "fmt"),
                        str_attr(AttrKey::OperationCommands, "cargo fmt,cargo xtask"),
                    ],
                    10,
                    11
                ),
                (
                    vec![
                        str_attr(AttrKey::OperationProgram, "timeout"),
                        str_attr(AttrKey::OperationWord, "600"),
                        str_attr(AttrKey::OperationCommands, "timeout 600"),
                        str_attr(AttrKey::OperationWrappedProgram, "cargo"),
                        str_attr(AttrKey::OperationWrappedWord, "test"),
                    ],
                    12,
                    13
                ),
            ]
        );
    }

    /// One `agent.operation` per shell command, from its start to its end,
    /// paired by call id when several overlap and by age otherwise; other
    /// tools make none, and the run counts them.
    #[test]
    fn shell_commands_become_operations() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        let seq = [
            shell(
                "p1",
                EventType::ToolStart,
                "Bash",
                Some("cargo test --workspace"),
                Some("a"),
            ),
            shell(
                "p1",
                EventType::ToolStart,
                "Bash",
                Some("rwx sandbox exec -- make ci"),
                Some("b"),
            ),
            shell(
                "p1",
                EventType::ToolStart,
                "Read",
                Some("/secret/path"),
                Some("c"),
            ),
            shell(
                "p1",
                EventType::ToolEnd,
                "Bash",
                Some("rwx sandbox exec -- make ci"),
                Some("b"),
            ),
            shell("p1", EventType::ToolEnd, "Read", None, Some("c")),
            // Pi's end names the tool and nothing else.
            shell("p1", EventType::ToolEnd, "bash", None, None),
            shell(
                "p1",
                EventType::ToolStart,
                "exec",
                Some("echo hunter2"),
                None,
            ),
            shell("p1", EventType::Error, "exec", Some("echo hunter2"), None),
            shell(
                "p1",
                EventType::ToolStart,
                "shell",
                Some("/bin/sh -lc cargo fmt"),
                None,
            ),
        ];
        for (t, e) in seq.iter().enumerate() {
            tracker.observe_event(e, 10 + t as u64);
        }
        tracker.end_run("p1", EndReason::Exited, None, 100);
        let spans = tracker.take_finished();
        assert_eq!(
            operations(&spans),
            vec![
                (op_attrs(Some("rwx"), Some("sandbox")), 11, 13),
                (op_attrs(Some("cargo"), Some("test")), 10, 15),
                (op_attrs(Some("echo"), None), 16, 17),
                (op_attrs(Some("cargo"), None), 18, 100),
            ]
        );
        let run = run_span_of(&spans);
        for op in spans.iter().filter(|s| s.name == SPAN_AGENT_OPERATION) {
            assert_eq!(op.parent_span_id, Some(run.span_id));
            assert_eq!(op.trace_id, run.trace_id);
            assert_eq!(
                attr(op, AttrKey::RunId),
                Some(&AttrValue::Str("run-p1".into()))
            );
        }
        assert_eq!(
            attr(run, AttrKey::OperationsCount),
            Some(&AttrValue::Int(4))
        );
    }

    /// A failed call's `Error` moves the card to Error exactly as before: one
    /// status change, whether or not a shell operation was open for it.
    #[test]
    fn a_failed_call_is_one_status_change() {
        let changes = |with_start: bool| {
            let mut tracker = RunTracker::new();
            tracker.begin_run(start("p1"), 0);
            tracker.observe_event(&event("p1", EventType::SessionStart), 1);
            if with_start {
                tracker.observe_event(
                    &shell("p1", EventType::ToolStart, "Bash", Some("make"), Some("c")),
                    2,
                );
            } else {
                tracker.observe_event(
                    &shell("p1", EventType::ToolStart, "Read", Some("f"), Some("c")),
                    2,
                );
            }
            tracker.observe_event(&shell("p1", EventType::Error, "Bash", None, Some("c")), 3);
            tracker.end_run("p1", EndReason::Exited, None, 4);
            let spans = tracker.take_finished();
            (
                attr(run_span_of(&spans), AttrKey::StatusChangesCount).cloned(),
                statuses(&spans),
            )
        };
        assert_eq!(changes(true), changes(false));
    }

    /// An end naming a call ends that call's operation and no other: a
    /// repeated end, or the end of a call evicted by the cap, ends nothing.
    #[test]
    fn an_end_with_a_call_id_ends_only_that_call() {
        let bash = |t: EventType, call: &str| shell("p1", t, "Bash", Some("make"), Some(call));
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&bash(EventType::ToolStart, "a"), 1);
        tracker.observe_event(&bash(EventType::ToolStart, "b"), 2);
        tracker.observe_event(&bash(EventType::ToolEnd, "a"), 3);
        tracker.observe_event(&bash(EventType::ToolEnd, "a"), 4);
        assert_eq!(
            operations(&tracker.take_finished())
                .iter()
                .map(|(_, start, end)| (*start, *end))
                .collect::<Vec<_>>(),
            vec![(1, 3)],
            "the duplicate end left b open"
        );
        tracker.observe_event(&bash(EventType::ToolEnd, "b"), 5);
        assert_eq!(operations(&tracker.take_finished())[0].1..=5, 2..=5);

        // The cap evicts the oldest; its late end must not end the next one.
        for i in 0..=MAX_OPEN_OPERATIONS as u64 {
            tracker.observe_event(&bash(EventType::ToolStart, &format!("c{i}")), 10 + i);
        }
        assert_eq!(operations(&tracker.take_finished()).len(), 1, "c0 evicted");
        tracker.observe_event(&bash(EventType::ToolEnd, "c0"), 200);
        assert!(
            operations(&tracker.take_finished()).is_empty(),
            "c0's late end ended c1"
        );
        tracker.observe_event(&bash(EventType::ToolEnd, "c1"), 201);
        assert_eq!(operations(&tracker.take_finished())[0].1, 11);

        // An agent that names the call on its end only: the oldest unnamed.
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&bash(EventType::ToolStart, "x"), 1);
        tracker.observe_event(
            &shell("p1", EventType::ToolStart, "Bash", Some("make"), None),
            2,
        );
        tracker.observe_event(&bash(EventType::ToolEnd, "y"), 3);
        assert_eq!(operations(&tracker.take_finished())[0].1, 2);
    }

    /// An id that already ended an unnamed call does not end the next one
    /// when it is repeated; an id the run has never seen still does.
    #[test]
    fn a_repeated_end_only_id_ends_nothing() {
        let unnamed = || shell("p1", EventType::ToolStart, "Bash", Some("make"), None);
        let end = |call: &str| shell("p1", EventType::ToolEnd, "Bash", None, Some(call));
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&unnamed(), 1);
        tracker.observe_event(&end("a"), 2);
        assert_eq!(operations(&tracker.take_finished()).len(), 1);
        tracker.observe_event(&unnamed(), 3);
        tracker.observe_event(&end("a"), 4);
        assert!(
            operations(&tracker.take_finished()).is_empty(),
            "the repeated end of a ended the next unnamed call"
        );
        tracker.observe_event(&end("b"), 5);
        assert_eq!(
            operations(&tracker.take_finished())
                .iter()
                .map(|(_, start, end)| (*start, *end))
                .collect::<Vec<_>>(),
            vec![(3, 5)]
        );

        // The memory is bounded: an id pushed out of it is new again.
        for i in 0..MAX_SEEN_CALL_IDS {
            tracker.observe_event(&end(&format!("filler{i}")), 10);
        }
        tracker.observe_event(&unnamed(), 20);
        tracker.observe_event(&end("a"), 21);
        assert_eq!(operations(&tracker.take_finished()).len(), 1);
    }

    /// What the tracker keeps of an event — in a run's cards, and held for a
    /// run that has not opened yet — has no shell command in it, while the
    /// operation is still classified from it.
    #[test]
    fn no_command_text_is_retained() {
        const SECRET: &str = "retainedsecret1741";
        let command = format!("cargo test {SECRET}");
        let tool = |pane: &str, t: EventType| {
            let mut e = shell(pane, t, "Bash", Some(&command), Some("call-1"));
            e.metadata
                .insert(BASH_COMMAND_METADATA_KEY.into(), format!("{command}\nmore"));
            e
        };
        let mut asking = event("p1", EventType::PermissionRequest);
        asking.tool_name = Some("Bash".into());
        asking.tool_detail = Some(command.clone());
        asking.set_question(
            &serde_json::from_value(serde_json::json!({
                "id": "q1",
                "kind": "permission",
                "questions": [{"prompt": "Allow?", "options": [
                    {"index": 1, "label": "Yes", "role": "allow_once"},
                    {"index": 2, "label": "No", "role": "deny"},
                ]}],
                "raised_at_ms": 1,
                "tool": {"name": "Bash", "detail": command, "use_id": "call-1"},
                "channel": "keys",
            }))
            .unwrap(),
        );
        assert!(asking.question().is_some(), "the fixture is a question");

        let mut tracker = RunTracker::new();
        tracker.prepare_run(start("p1"), 0);
        for (t, e) in [
            event("p1", EventType::SessionStart),
            tool("p1", EventType::ToolStart),
            asking,
        ]
        .iter()
        .enumerate()
        {
            tracker.observe_event(e, 1 + t as u64);
        }
        let held = format!(
            "{:?}",
            tracker.pending["run-p1"]
                .events
                .iter()
                .map(|(k, _)| &k.event)
                .collect::<Vec<_>>()
        );
        assert!(held.contains("PermissionRequest"), "{held}");
        assert!(!held.contains(SECRET), "held for the run: {held}");

        tracker.commit_run("run-p1");
        let cards = format!("{:?}", tracker.runs["p1"].cards.state);
        assert!(cards.contains("Bash"), "the cards saw the call: {cards}");
        assert!(!cards.contains(SECRET), "in the run's cards: {cards}");
        assert_eq!(
            tracker.runs["p1"]
                .cards
                .sessions
                .values()
                .map(|s| status_name(&s.status))
                .collect::<Vec<_>>(),
            vec!["waiting"],
            "the question still counts"
        );

        tracker.observe_event(&tool("p1", EventType::ToolEnd), 10);
        assert!(!format!("{:?}", tracker.runs["p1"].cards.state).contains(SECRET));
        tracker.end_run("p1", EndReason::Exited, None, 20);
        assert_eq!(
            operations(&tracker.take_finished()),
            vec![(op_attrs(Some("cargo"), Some("test")), 2, 10)]
        );
    }

    /// Starts whose end never arrives are bounded per run.
    #[test]
    fn open_operations_are_bounded() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        for t in 0..(MAX_OPEN_OPERATIONS as u64 + 3) {
            tracker.observe_event(
                &shell("p1", EventType::ToolStart, "Bash", Some("make"), None),
                t,
            );
        }
        assert_eq!(operations(&tracker.take_finished()).len(), 3);
        tracker.end_run("p1", EndReason::Exited, None, 1000);
        assert_eq!(
            operations(&tracker.take_finished()).len(),
            MAX_OPEN_OPERATIONS
        );
    }

    fn checks_report(
        pane: &str,
        number: u64,
        revision: u64,
        state: &str,
        counts: (u32, u32, u32),
    ) -> AgentEvent {
        let mut pr = pr_report(pane, Some(number), Some(revision));
        pr.metadata.insert(
            crate::event::PULL_REQUEST_METADATA_KEY.into(),
            format!(
                r#"{{"number":{number},"url":"https://github.com/o/r/pull/{number}","state":"open","checks":{{"state":"{state}","passing":{},"failing":{},"pending":0,"skipped":{}}}}}"#,
                counts.0, counts.1, counts.2
            ),
        );
        pr
    }

    /// The run's pull request's checks, as the card folds the deck's reports:
    /// a change event per new result, an `agent.ci` span per pending stretch
    /// with its result and counts, and the run's round and failure counts. A
    /// report older than the card's is ignored.
    #[test]
    fn ci_time_follows_the_cards_checks() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        for (t, report) in [
            checks_report("p1", 9, 1, "pending", (0, 0, 0)),
            checks_report("p1", 9, 2, "failing", (5, 1, 2)),
            // Older than the card's: ignored.
            checks_report("p1", 9, 1, "pending", (0, 0, 0)),
            checks_report("p1", 9, 3, "pending", (5, 0, 2)),
            checks_report("p1", 9, 4, "passing", (8, 0, 2)),
            checks_report("p1", 9, 5, "pending", (0, 0, 0)),
        ]
        .iter()
        .enumerate()
        {
            tracker.observe_event(report, 10 * (t as u64 + 1));
        }
        tracker.end_run("p1", EndReason::Exited, None, 100);
        let spans = tracker.take_finished();
        let ci: Vec<_> = spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_CI)
            .map(|s| {
                (
                    attr(s, AttrKey::CiResult).cloned(),
                    attr(s, AttrKey::CiChecksPassing).cloned(),
                    attr(s, AttrKey::CiChecksFailing).cloned(),
                    attr(s, AttrKey::CiChecksSkipped).cloned(),
                    s.start_unix_nanos,
                    s.end_unix_nanos,
                )
            })
            .collect();
        let int = |v: i64| Some(AttrValue::Int(v));
        let s = |v: &str| Some(AttrValue::Str(v.into()));
        assert_eq!(
            ci,
            vec![
                (s("failing"), int(5), int(1), int(2), 10, 20),
                (s("passing"), int(8), int(0), int(2), 40, 50),
                (None, int(0), int(0), int(0), 60, 100),
            ]
        );
        let run = run_span_of(&spans);
        for span in spans.iter().filter(|s| s.name == SPAN_AGENT_CI) {
            assert_eq!(span.parent_span_id, Some(run.span_id));
            assert_eq!(attr(span, AttrKey::PrNumber), int(9).as_ref());
        }
        let changes: Vec<_> = run
            .events
            .iter()
            .filter(|e| e.name == EVENT_CI_CHANGE)
            .map(|e| e.attributes.clone())
            .collect();
        assert_eq!(changes.len(), 5);
        assert_eq!(
            changes[0],
            vec![
                Attribute::str(AttrKey::CiTo, "pending"),
                Attribute::int(AttrKey::PrNumber, 9)
            ],
            "no from on the first"
        );
        assert_eq!(
            changes[1][..2],
            [
                Attribute::str(AttrKey::CiFrom, "pending"),
                Attribute::str(AttrKey::CiTo, "failing"),
            ]
        );
        assert_eq!(attr(run, AttrKey::CiRoundsCount), int(2).as_ref());
        assert_eq!(attr(run, AttrKey::CiFailuresCount), int(1).as_ref());
    }

    /// A run with no pull request reports no CI, with zero counts.
    #[test]
    fn a_run_without_ci_counts_zero() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.end_run("p1", EndReason::Exited, None, 1);
        let spans = tracker.take_finished();
        assert_eq!(spans.len(), 1);
        assert_eq!(
            attr(&spans[0], AttrKey::CiRoundsCount),
            Some(&AttrValue::Int(0))
        );
        assert_eq!(
            attr(&spans[0], AttrKey::OperationsCount),
            Some(&AttrValue::Int(0))
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
