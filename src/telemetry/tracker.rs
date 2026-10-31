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
//!
//! **An intervention says what the person did** (`deck.intervention.kind`):
//! the strongest kind of input in its burst, from the bytes
//! ([`super::input::InterventionKind`]), raised to an answer when the run's own
//! cards show the agent waiting on a question or a permission prompt as the
//! tracker applies it. An answer the deck's question UI gave is one too, at the
//! time the daemon stamped its event saying so, since a held question's answer
//! types nothing into the pane.
//!
//! **Input, prompt changes and deck answers are applied in the order the deck
//! recorded them**, which is the caller's job, not the tracker's: the tracker
//! applies each in the order it is handed them, reads the prompt as the cards
//! show it then, and never moves a run's last input backward, so a burst's
//! time can only grow. [`super::exporter`] hands each pane's input over before
//! the first event broadcast after telemetry was told about it, just after
//! the pane accepted it, once this tracker has opened the run it was typed
//! into; the Escape key, and a key whose sequence a held `ESC` begins and the
//! next write completes, as of the write that held the `ESC`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::event::{AgentEvent, AgentType, DAEMON_PANE_CLOSED_METADATA_KEY, EventType};
use crate::state::{AppState, SessionStatus};

use super::accounting::AccountingKey;
use super::accounting::run_usage::{RunUsage, UsageDirs};
use super::ci::{CiEffect, CiState, PrCi};
use super::handoff::{Change, Handoffs, Posted, RunRef};
use super::input::InterventionKind;
use super::operation::{Description, describe_tool, is_shell_tool};
use super::run_context::RunContext;
use super::span::{
    Attribute, SpanEventRecord, SpanId, SpanRecord, TraceId, new_span_id, new_trace_id,
};
use super::{
    AttrKey, EVENT_CI_CHANGE, EVENT_INTERVENTION, EVENT_MODEL_USAGE, EVENT_STATUS_CHANGE,
    SPAN_AGENT_CI, SPAN_AGENT_OPERATION, SPAN_AGENT_RUN, SPAN_AGENT_STATUS,
};

/// Human input closer together than this is one intervention: a burst of
/// keystrokes, not one per key.
pub const INTERVENTION_COALESCE_NANOS: u64 = 5_000_000_000;

/// Span events kept per run. Past it the counters still count; only the
/// per-occurrence events stop, so a run left open for weeks cannot grow without
/// bound.
pub const MAX_EVENTS_PER_RUN: usize = 1024;

/// Events held for a prepared run until it is committed. Its agent has only
/// just started, so this is far more than the window ever sees. The same
/// number separately caps the run's held posts and its held notices about
/// them: a 257th post opens at once in a trace of its own with no parent
/// ([`RunTracker::place_post`]), and a 257th notice is applied at once, which
/// puts it in the early buffer to be replayed as its post opens
/// ([`RunTracker::hold_notice`]). That takes 257 posts, or 257 notices (an
/// ordinary task produces about five, so about 52 tasks), before telemetry
/// applies the run's `Commit`, which is not a realistic workload.
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

/// The questions one run remembers the deck answers by typing, newest kept.
pub const MAX_TYPED_QUESTIONS: usize = 16;

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

/// How an operation ended: on the agent's own end event, or by the deck with
/// the call still open, so its duration is not the command's (issue #1981).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationEnd {
    /// The agent reported the call's end.
    Agent,
    /// Still open when its run ended.
    RunEnd,
    /// The oldest open call, ended to stay under [`MAX_OPEN_OPERATIONS`].
    Cap,
}

impl OperationEnd {
    /// The `deck.operation.end` value, absent for an end the agent reported.
    fn as_str(self) -> Option<&'static str> {
        match self {
            OperationEnd::Agent => None,
            OperationEnd::RunEnd => Some("run_end"),
            OperationEnd::Cap => Some("cap"),
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
    /// Where its agent keeps its usage record ([`RunTracker::set_usage_dirs`]).
    usage_dirs: UsageDirs,
    /// Its agent already ended, before the run was committed: how and when,
    /// applied by [`RunTracker::commit_run`] once the run opens.
    ended: Option<(EndReason, Option<i32>, u64)>,
    /// Issue #1981 M7: tasks this run's agent delegated before the run
    /// opened, the inbox notices about them seen since, and the reports
    /// naming no task its agent received, in the order seen, each with how
    /// many of `events` were held before it, so
    /// [`RunTracker::commit_run`] applies each in its place among them
    /// ([`RunTracker::place_post`], [`RunTracker::observe_inbox`]).
    handoffs: Vec<(usize, HeldHandoff)>,
    /// The dispatched unit it belongs to, when known before it opens
    /// ([`RunTracker::begin_dispatch`]).
    dispatch_name: Option<String>,
    /// The branch it starts on ([`RunTracker::set_branch`]).
    branch: Option<String>,
}

/// What a prepared run holds of its hand-offs: see [`PendingRun::handoffs`].
enum HeldHandoff {
    Post(Posted),
    /// An inbox change about a held post, and when it was seen.
    Notice(Change, u64),
    /// Reports naming no task that reached this run's agent, counted on the
    /// run once it opens ([`RunTracker::observe_inbox`]).
    Uncorrelated(u64),
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
    /// `deck.pr.number`: the last pull request the run's card held. A later
    /// report of no pull request leaves it, so the run stays joinable to the
    /// pull request it worked on.
    pr_number: Option<u64>,
    status: Option<OpenStatus>,
    events: Vec<SpanEventRecord>,
    interventions: u64,
    status_changes: u64,
    last_human_input: Option<u64>,
    /// The kind of the current burst of input, and where its
    /// `deck.intervention` event is in `events` — `None` when the event was
    /// not kept ([`MAX_EVENTS_PER_RUN`]).
    burst: Option<(InterventionKind, Option<usize>)>,
    /// The last wait the agent reported is a notification that asks nothing:
    /// Claude Code's `idle_prompt`, after every ended turn. Input then is not
    /// an answer.
    idle_wait: bool,
    operations: Vec<OpenOperation>,
    operations_count: u64,
    /// The call ids this run's shell starts and ends named, at most
    /// [`MAX_SEEN_CALL_IDS`], oldest first.
    seen_call_ids: VecDeque<String>,
    /// The ids of the questions this run's agent asked that the deck answers
    /// by typing keys into the pane, at most [`MAX_TYPED_QUESTIONS`], oldest
    /// first: those keys are the person's input already, so the deck's
    /// "answered" event for one of them is not counted again.
    typed_questions: VecDeque<String>,
    /// The pull request whose checks the run's card holds now, whose CI state
    /// it shares with every other run holding it ([`RunTracker::ci`]).
    ci_pr: Option<PrKey>,
    /// What this run's card last showed of that pull request's checks.
    ci_seen: Option<CiState>,
    /// The report revision and time from which this run's card has shown
    /// those checks pending, while it does.
    ci_pending_since: Option<(u64, u64)>,
    /// Rounds of that pull request's checks this run's card saw go from
    /// pending to a result, and how many of them failed: what this run
    /// observed, so runs holding the same pull request each count the same
    /// round, whatever order the shared state took their reports in.
    ci_rounds: u64,
    ci_failures: u64,
    /// The run's usage readers (PRD #1858), taken as the run finishes.
    usage: Option<RunUsage>,
    /// `deck.dispatch.name`: the dispatched unit the run belongs to, as the
    /// deck's record of dispatched units names it
    /// ([`RunTracker::set_dispatch_name`]).
    dispatch_name: Option<String>,
    /// The agent the run's hooks last named, kept once the card that named it
    /// is gone ([`resolved_agent`]).
    learned_agent: Option<AgentType>,
    /// Issue #1981 M7: reports this run's agent received that named no task
    /// (`deck.handoffs.uncorrelated.count`).
    uncorrelated_reports: u64,
    /// Issue #1981 M6: `vcs.ref.head.name`, the branch the run started on.
    branch: Option<String>,
}

/// A pull request, told apart from another repository's of the same number by
/// its URL without the number. Kept in memory only, never sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PrKey {
    repository: String,
    number: u64,
}

impl PrKey {
    fn of(pr: &crate::pull_request_info::PullRequestInfo) -> Self {
        let repository = pr
            .url
            .rsplit_once("/pull/")
            .map_or(pr.url.as_str(), |(repository, _)| repository);
        PrKey {
            repository: repository.to_string(),
            number: pr.number,
        }
    }
}

/// One pull request's checks, as every open run holding it sees them: its
/// `agent.ci` spans and `deck.ci.change` events are recorded once, however
/// many runs hold it.
///
/// **Reports are folded in the order the daemon made them, not the order
/// they reach the tracker.** Each run's card is told separately, a prepared
/// run's reports are applied only when it opens, and a card can lag, so a
/// report can arrive after a newer one about the same pull request has moved
/// the shared state on. The daemon stamps every report with a revision from
/// one increasing counter ([`crate::pull_request::next_revision`]), so a
/// report whose revision is lower than the newest folded in is history the
/// state has moved past, and is left out.
///
/// **Leaving a report out does not leave its round out.** A newer report can
/// be a result whose pending phase the shared state never saw, so the rounds
/// it did record are kept as revision intervals ([`RecordedRounds`]), and a
/// run whose own card saw a round that none of them, nor the open stretch,
/// overlaps records that round itself ([`RunTracker::observe_ci`]).
#[derive(Default)]
struct SharedCi {
    ci: PrCi,
    /// The highest report revision folded into `ci`. `0` until one with a
    /// revision is: a report with none is folded by arrival.
    revision: u64,
    /// The revision of the report that opened the open stretch: `0` when it
    /// had none.
    open_since: u64,
    /// The rounds recorded for the pull request.
    rounds: RecordedRounds,
    /// The runs holding it, by run id, the longest-holding first. That one
    /// takes its `deck.ci.change` events, and a stretch that opens goes in its
    /// trace.
    holders: Vec<String>,
    /// Where the open stretch's `agent.ci` span goes.
    opener: Option<StretchOpener>,
}

/// What is kept of a pull request once no open run holds it, so a report
/// about it that arrives later — from a run prepared while it was held — is
/// still ordered against what was already recorded ([`SharedCi`]).
struct ReleasedCi {
    revision: u64,
    settled: Option<CiState>,
    rounds: RecordedRounds,
}

/// The stretches recorded for one pull request, as the revisions of the
/// reports that opened and ended them (`0` for an opening report that had
/// none, which overlaps everything before the end): the
/// [`MAX_RECORDED_ROUNDS`] that ended last, ordered by that ending revision
/// whatever order they were recorded in, and below `floor` every older one,
/// which counts as overlapping anything that starts there.
#[derive(Default)]
struct RecordedRounds {
    rounds: VecDeque<(u64, u64)>,
    floor: u64,
}

impl RecordedRounds {
    fn record(&mut self, from: u64, to: u64) {
        // Below the floor it is covered already.
        if to <= self.floor {
            return;
        }
        let at = self.rounds.partition_point(|&(a, b)| (b, a) <= (to, from));
        self.rounds.insert(at, (from, to));
        if self.rounds.len() > MAX_RECORDED_ROUNDS
            && let Some((_, oldest)) = self.rounds.pop_front()
        {
            self.floor = self.floor.max(oldest);
        }
    }

    /// Whether a round seen from revision `from` to `to` overlaps one already
    /// recorded.
    fn covers(&self, from: u64, to: u64) -> bool {
        from <= self.floor || self.rounds.iter().any(|&(a, b)| a <= to && from <= b)
    }
}

/// A unit being spawned: see [`RunTracker::begin_dispatch`].
struct Dispatching {
    token: u64,
    worktree: std::path::PathBuf,
    name: String,
}

/// Dispatched orchestration instances whose name the tracker keeps, so a role
/// started again later is named from its start too. Past it the oldest is
/// forgotten, and a role it starts is named by the deck's record of
/// dispatched units instead ([`RunTracker::runs_without_dispatch`]).
pub const MAX_DISPATCHED_ORCHESTRATIONS: usize = 256;

/// Pull requests no open run holds that the tracker keeps a
/// [`ReleasedCi`] for. Past it the one with the oldest report is forgotten.
pub const MAX_RELEASED_PULL_REQUESTS: usize = 256;

/// Recorded stretches one pull request keeps the revisions of
/// ([`RecordedRounds`]).
pub const MAX_RECORDED_ROUNDS: usize = 16;

/// The run a stretch of pending checks opened in.
struct StretchOpener {
    run_id: String,
    trace_id: TraceId,
    span_id: SpanId,
    /// The run's [`child_identity`], for a span that ends after the run did.
    identity: Vec<Attribute>,
}

/// An open run's generation, for looking up the dispatched unit it belongs to
/// ([`RunTracker::runs_without_dispatch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGeneration {
    pub run_id: String,
    pub pane_id: String,
    pub agent_id: Option<String>,
    pub orchestration_id: Option<String>,
}

impl PendingRun {
    fn held_posts(&self) -> usize {
        self.handoffs
            .iter()
            .filter(|(_, held)| matches!(held, HeldHandoff::Post(_)))
            .count()
    }

    fn held_notices(&self) -> usize {
        self.handoffs
            .iter()
            .filter(|(_, held)| matches!(held, HeldHandoff::Notice(..)))
            .count()
    }

    /// Count a report naming no task, after the events held so far: one
    /// arriving with no event held since the last adds to its count, so there
    /// is at most one more entry than events held ([`MAX_PENDING_EVENTS`]).
    fn hold_uncorrelated(&mut self) {
        let before = self.events.len();
        match self.handoffs.last_mut() {
            Some((at, HeldHandoff::Uncorrelated(count))) if *at == before => *count += 1,
            _ => self.handoffs.push((before, HeldHandoff::Uncorrelated(1))),
        }
    }

    fn holds_post(&self, task: &str) -> bool {
        self.handoffs
            .iter()
            .any(|(_, held)| matches!(held, HeldHandoff::Post(p) if p.message_id == task))
    }

    /// Whether `change` concerns a post held here: the task itself, its
    /// report, or that report's own changes. `early_report` names the task
    /// of a report the hand-offs hold for a post not seen yet.
    fn concerns(&self, change: &Change, early_report: &dyn Fn(&str) -> Option<String>) -> bool {
        match change {
            Change::Report { task, .. } => self.holds_post(task),
            Change::Uncorrelated { .. } => false,
            other => other.message_id().is_some_and(|id| {
                self.holds_post(id)
                    || self.handoffs.iter().any(|(_, held)| {
                        matches!(held, HeldHandoff::Notice(Change::Report { report, .. }, _) if report == id)
                    })
                    || early_report(id).is_some_and(|task| self.holds_post(&task))
            }),
        }
    }

    /// The run will not open: remove the usage file made for it.
    fn forget(self) {
        if let Some(file) = self.usage_dirs.usage_file {
            file.dir.remove(std::ffi::OsStr::new(&file.name));
        }
    }
}

/// A run a newer generation of its pane replaced before its own end was seen:
/// see [`RunTracker::begin_run`].
struct ReplacedRun {
    run: ActiveRun,
    /// When the newer generation's run opened.
    at: u64,
}

impl ActiveRun {
    /// Keep `event`, with the run's agent and orchestration as they are known
    /// now ([`agent_identity`]).
    fn push_event(&mut self, mut event: SpanEventRecord) {
        if self.events.len() < MAX_EVENTS_PER_RUN {
            event.attributes.extend(agent_identity(self));
            self.events.push(event);
        }
    }

    /// See [`RunTracker::observe_run_input`]. Input while the agent waits on
    /// a prompt is an answer, unless it interrupts. Input handed over with a
    /// time before the run's last input counts at that last time: the order
    /// it was handed over in is the order the deck recorded it in.
    fn human_input(&mut self, now: u64, kind: InterventionKind) {
        let kind = if self.prompt_open() {
            kind.max(InterventionKind::Answer)
        } else {
            kind
        };
        let now = self.last_human_input.map_or(now, |last| now.max(last));
        let new_burst = self
            .last_human_input
            .is_none_or(|last| now.saturating_sub(last) > INTERVENTION_COALESCE_NANOS);
        self.last_human_input = Some(now);
        if new_burst {
            self.interventions += 1;
            let kept = self.events.len() < MAX_EVENTS_PER_RUN;
            self.push_event(SpanEventRecord {
                name: EVENT_INTERVENTION,
                time_unix_nanos: now,
                attributes: vec![Attribute::str(AttrKey::InterventionKind, kind.as_str())],
            });
            self.burst = Some((kind, kept.then(|| self.events.len() - 1)));
        } else if let Some((burst_kind, at)) = self.burst.as_mut()
            && kind > *burst_kind
        {
            *burst_kind = kind;
            if let Some(attribute) = at.and_then(|i| self.events.get_mut(i)).and_then(|e| {
                e.attributes
                    .iter_mut()
                    .find(|a| a.key == AttrKey::InterventionKind)
            }) {
                *attribute = Attribute::str(AttrKey::InterventionKind, kind.as_str());
            }
        }
    }

    /// Remember whether the deck answers `question` by typing keys into the
    /// pane ([`crate::question::AnswerChannel::Keys`]): the latest
    /// registration of an id says.
    fn note_question(&mut self, question: &crate::question::PendingQuestion) {
        self.typed_questions.retain(|id| *id != question.id);
        if question.channel == crate::question::AnswerChannel::Keys {
            if self.typed_questions.len() >= MAX_TYPED_QUESTIONS {
                self.typed_questions.pop_front();
            }
            self.typed_questions.push_back(question.id.clone());
        }
    }

    /// Whether the deck answered question `id` by typing keys into the pane.
    fn answer_was_typed(&self, id: Option<&str>) -> bool {
        id.is_some_and(|id| self.typed_questions.iter().any(|typed| typed == id))
    }

    /// Whether the run's pane's card shows the agent waiting on a question
    /// or a permission prompt: a pending question, or a wait that is not
    /// [`Self::idle_wait`].
    fn prompt_open(&self) -> bool {
        let pane_id = self.start_info.pane_id.as_str();
        self.cards
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some(pane_id))
            .max_by_key(|s| s.last_activity)
            .is_some_and(|s| {
                s.pending_question.is_some()
                    || (s.status == SessionStatus::WaitingForInput && !self.idle_wait)
            })
    }
}

/// Asked, with the pane and the run id, as each run ends — by whatever ends
/// it — for the times of human input still held back for that run, oldest
/// first, which are then counted before the run's span is finished. See
/// [`RunTracker::set_held_input`].
pub type HeldInput = Box<dyn FnMut(&str, &str) -> Vec<(u64, InterventionKind)> + Send>;

/// Issue #2028: the generations whose terminal ended while the deck still
/// held them, noted by their PTY reader at that end
/// ([`super::Telemetry::agent_ended_by_itself`]) and forgotten once their own
/// end is applied. Locked after the tracker, never before it, and nothing
/// else is taken while it is held.
pub type EndedByThemselves = Arc<Mutex<HashSet<String>>>;

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
    /// Ended runs whose span waits for their usage to be read
    /// ([`Self::take_finished_deferred`]). A run with nothing to read
    /// ([`RunUsage::reads`]) is never one: its span is in `finished`.
    finishing: Vec<FinishingRun>,
    /// The CI state of every pull request an open run's card holds.
    ci: HashMap<PrKey, SharedCi>,
    /// The pull requests no open run holds any more, at most
    /// [`MAX_RELEASED_PULL_REQUESTS`].
    released_ci: HashMap<PrKey, ReleasedCi>,
    /// The units being spawned now.
    dispatching: Vec<Dispatching>,
    next_dispatch: u64,
    /// The names of dispatched orchestration instances, by instance token,
    /// oldest first, at most [`MAX_DISPATCHED_ORCHESTRATIONS`].
    dispatched_orchestrations: VecDeque<(String, String)>,
    held_input: Option<HeldInput>,
    /// Issue #1981 M7: the delegated tasks whose `agent.handoff` is open.
    handoffs: Handoffs,
    /// Issue #2028: see [`EndedByThemselves`]. A pane's close broadcast does
    /// not end the run of a generation in it: its own end does, as exited.
    ended_by_themselves: EndedByThemselves,
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

    /// The set the PTY readers note a generation that ended by itself in
    /// ([`EndedByThemselves`]).
    pub fn ended_by_themselves(&self) -> EndedByThemselves {
        Arc::clone(&self.ended_by_themselves)
    }

    /// Whether generation `agent_id` ended by itself ([`EndedByThemselves`]).
    fn ended_by_itself(&self, agent_id: Option<&str>) -> bool {
        agent_id.is_some_and(|agent_id| {
            self.ended_by_themselves
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(agent_id)
        })
    }

    /// Generation `agent_id`'s own end was applied: its note is done with.
    fn forget_ended_by_itself(&self, agent_id: &str) {
        self.ended_by_themselves
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(agent_id);
    }

    /// The `deck.run.id` of the run open on `pane_id`, if any.
    pub fn run_id(&self, pane_id: &str) -> Option<&str> {
        self.runs
            .get(pane_id)
            .map(|run| run.start_info.run_id.as_str())
    }

    /// The usage of the run open on `pane_id`, if it has any.
    #[cfg(test)]
    pub fn usage_of(&self, pane_id: &str) -> Option<RunUsage> {
        self.runs.get(pane_id)?.usage.clone()
    }

    /// An agent is about to start: remember its run, so events it causes before
    /// [`Self::commit_run`] are kept for it. A run id prepared twice keeps the
    /// first preparation.
    pub fn prepare_run(&mut self, start: RunStart, now: u64) {
        self.prepare_run_in(start, now, None);
    }

    /// [`Self::prepare_run`], for an agent starting in `cwd`: a run of a unit
    /// being dispatched, or of a dispatched orchestration, is named after the
    /// unit from here on.
    pub fn prepare_run_in(&mut self, start: RunStart, now: u64, cwd: Option<&std::path::Path>) {
        let dispatch_name = self.dispatch_name_for(&start, cwd);
        self.pending
            .entry(start.run_id.clone())
            .or_insert(PendingRun {
                start,
                prepared_at: now,
                context: RunContext::default(),
                events: Vec::new(),
                usage_dirs: UsageDirs::default(),
                ended: None,
                handoffs: Vec::new(),
                dispatch_name,
                branch: None,
            });
    }

    /// The unit a run about to start belongs to: the dispatched orchestration
    /// instance it is a role of, else the unit whose worktree `cwd` is in
    /// while that unit is being spawned — the innermost, should two nest.
    fn dispatch_name_for(&self, start: &RunStart, cwd: Option<&std::path::Path>) -> Option<String> {
        let instance = start.orchestration.as_ref().and_then(|o| o.id.as_deref());
        if let Some(instance) = instance
            && let Some((_, name)) = self
                .dispatched_orchestrations
                .iter()
                .find(|(id, _)| id == instance)
        {
            return Some(name.clone());
        }
        let cwd = cwd?;
        self.dispatching
            .iter()
            .filter(|d| cwd.starts_with(&d.worktree))
            .max_by_key(|d| d.worktree.as_os_str().len())
            .map(|d| d.name.clone())
    }

    /// A unit named `name` is about to be spawned in `worktree`: every run
    /// prepared in it until [`Self::end_dispatch`] is given the token
    /// returned here belongs to the unit. Naming a run as it is prepared means
    /// it has the name however soon it ends, before anything the deck records
    /// of the unit could be looked up.
    pub fn begin_dispatch(&mut self, name: &str, worktree: &std::path::Path) -> u64 {
        self.next_dispatch += 1;
        self.dispatching.push(Dispatching {
            token: self.next_dispatch,
            worktree: worktree.to_path_buf(),
            name: name.to_string(),
        });
        self.next_dispatch
    }

    /// The spawn [`Self::begin_dispatch`] returned `token` for is over.
    pub fn end_dispatch(&mut self, token: u64) {
        self.dispatching.retain(|d| d.token != token);
    }

    /// The unit `name` was spawned as the generations `agents`, by pane and
    /// agent id, and, for an orchestration, as instance `orchestration_id`:
    /// name each of their runs that has no name yet, prepared, open or
    /// replaced, and every role the instance starts later.
    pub fn name_dispatched(
        &mut self,
        name: &str,
        agents: &[(String, String)],
        orchestration_id: Option<&str>,
    ) {
        let ours = |start: &RunStart| {
            agents.iter().any(|(pane, agent)| {
                start.pane_id == *pane && start.agent_id.as_deref() == Some(agent.as_str())
            }) || (orchestration_id.is_some()
                && start.orchestration.as_ref().and_then(|o| o.id.as_deref()) == orchestration_id)
        };
        for pending in self.pending.values_mut() {
            if pending.dispatch_name.is_none() && ours(&pending.start) {
                pending.dispatch_name = Some(name.to_string());
            }
        }
        for run in self
            .runs
            .values_mut()
            .chain(self.replaced.iter_mut().map(|r| &mut r.run))
        {
            if run.dispatch_name.is_none() && ours(&run.start_info) {
                run.dispatch_name = Some(name.to_string());
            }
        }
        if let Some(instance) = orchestration_id
            && !self
                .dispatched_orchestrations
                .iter()
                .any(|(id, _)| id == instance)
        {
            if self.dispatched_orchestrations.len() >= MAX_DISPATCHED_ORCHESTRATIONS {
                self.dispatched_orchestrations.pop_front();
            }
            self.dispatched_orchestrations
                .push_back((instance.to_string(), name.to_string()));
        }
    }

    /// The agent of the prepared run `run_id` did not start: forget the run,
    /// after opening the hand-offs of any task held for it.
    pub fn abort_run(&mut self, run_id: &str) {
        self.release_posts(run_id);
        if let Some(pending) = self.pending.remove(run_id) {
            pending.forget();
        }
    }

    /// Issue #1981 M7: open the hand-off of each task held for the prepared
    /// run `run_id`, which will not open, and apply the notices held about
    /// them, in the order seen: while the run is still known, so each keeps
    /// its run id, though there is no run span to parent it.
    fn release_posts(&mut self, run_id: &str) {
        let held = self
            .pending
            .get_mut(run_id)
            .map(|p| std::mem::take(&mut p.handoffs))
            .unwrap_or_default();
        for (_, held) in held {
            self.apply_held(run_id, held);
        }
    }

    /// Apply `held`, held for the prepared run `run_id`.
    fn apply_held(&mut self, run_id: &str, held: HeldHandoff) {
        match held {
            HeldHandoff::Post(posted) => self.delegate_posted(posted),
            HeldHandoff::Notice(change, at) => self.apply_change(change, at),
            HeldHandoff::Uncorrelated(count) => self.count_uncorrelated(run_id, count),
        }
    }

    /// Add `count` reports naming no task to the run `run_id`, open or
    /// replaced; a run that did not open or has ended is not counted on.
    fn count_uncorrelated(&mut self, run_id: &str, count: u64) {
        if let Some(run) = self
            .runs
            .values_mut()
            .chain(self.replaced.iter_mut().map(|r| &mut r.run))
            .find(|run| run.start_info.run_id == run_id)
        {
            run.uncorrelated_reports += count;
        }
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
        let agent_id = pending.start.agent_id.clone();
        self.open_run(pending.start, pending.prepared_at, pending.usage_dirs);
        if let Some(run) = self.runs.get_mut(&pane_id) {
            run.context = pending.context;
            run.dispatch_name = pending.dispatch_name;
            run.branch = pending.branch;
        }
        // Each task posted, and each notice about one, in its place among
        // the events: one posted before a close is opened in the run before
        // the close ends it, and a report that arrived after the close finds
        // the hand-off already ended.
        let mut handoffs = pending.handoffs.into_iter().peekable();
        for (index, (event, at)) in pending.events.into_iter().enumerate() {
            while let Some((_, held)) = handoffs.next_if(|(before, _)| *before <= index) {
                self.apply_held(run_id, held);
            }
            self.observe_kept(event, at);
        }
        for (_, held) in handoffs {
            self.apply_held(run_id, held);
        }
        if let Some((reason, exit_code, at)) = pending.ended {
            self.end_run(&pane_id, reason, exit_code, at);
            if let Some(agent_id) = agent_id.as_deref() {
                self.forget_ended_by_itself(agent_id);
            }
        }
    }

    /// Open a run. A run still open on the same pane is replaced: when both
    /// know their generation and the two differ, it is set aside to be ended
    /// by its own generation's end ([`Self::end_agent_run`], or the pane's
    /// close broadcast naming it); otherwise it is ended now with
    /// [`EndReason::Unknown`], since no end could be told apart as its own.
    pub fn begin_run(&mut self, start: RunStart, now: u64) {
        self.open_run(start, now, UsageDirs::default());
    }

    /// [`Self::begin_run`], with where the run's agent keeps its usage record.
    fn open_run(&mut self, start: RunStart, now: u64, usage_dirs: UsageDirs) {
        self.expire_replaced(now);
        if let Some(mut old) = self.runs.remove(&start.pane_id) {
            let own_end_can_come = matches!(
                (old.start_info.agent_id.as_deref(), start.agent_id.as_deref()),
                (Some(a), Some(b)) if a != b
            );
            if own_end_can_come {
                // A replaced run gets no more pull-request reports, so it
                // lets go of its pull request now rather than open stretches
                // and take changes the live run sees.
                if let Some(key) = old.ci_pr.take() {
                    let identity = child_identity(&old);
                    self.release_ci(&key, &old.start_info.run_id, identity, now);
                }
                self.replaced.push(ReplacedRun { run: old, at: now });
                if self.replaced.len() > MAX_REPLACED_RUNS {
                    let oldest = self.replaced.remove(0);
                    self.retire_unended(oldest);
                }
            } else {
                self.finish(old, EndReason::Unknown, None, now);
            }
        }
        let pane_id = start.pane_id.clone();
        let usage = RunUsage::new(&start.agent, usage_dirs, now);
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
                burst: None,
                idle_wait: false,
                operations: Vec::new(),
                operations_count: 0,
                seen_call_ids: VecDeque::new(),
                typed_questions: VecDeque::new(),
                ci_pr: None,
                ci_seen: None,
                ci_pending_since: None,
                ci_rounds: 0,
                ci_failures: 0,
                usage: Some(usage),
                dispatch_name: None,
                learned_agent: None,
                uncorrelated_reports: 0,
                branch: None,
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

    /// Issue #1981 M6: the branch prepared run `run_id` starts on, read before
    /// its agent starts ([`super::run_context::branch_at`]).
    pub fn set_branch(&mut self, run_id: &str, branch: Option<String>) {
        if let Some(pending) = self.pending.get_mut(run_id) {
            pending.branch = branch;
        }
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

    /// Where the prepared run `run_id`'s agent keeps its usage record, read
    /// once the run opens. Ignored for a run that is not prepared.
    pub fn set_usage_dirs(&mut self, run_id: &str, dirs: UsageDirs) {
        if let Some(pending) = self.pending.get_mut(run_id) {
            pending.usage_dirs = dirs;
        }
    }

    /// The usage readers of every open run with a record to read (PRD
    /// #1858, [`RunUsage::reads`]), for the caller to [`RunUsage::poll`] once
    /// it has let go of the tracker: polling reads files, and nothing that
    /// waits for the tracker should wait for that. The daemon polls on an
    /// interval; a run's end drains its record however long ago the last
    /// poll was.
    pub fn usage_to_poll(&self) -> Vec<RunUsage> {
        self.runs
            .values()
            .chain(self.replaced.iter().map(|r| &r.run))
            .filter_map(|run| run.usage.clone())
            .filter(RunUsage::reads)
            .collect()
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
            if closed && !self.ended_by_itself(Some(theirs)) {
                self.end_replaced(pane_id, theirs, EndReason::Closed, None, now);
            }
            return;
        }
        if closed {
            // Issue #2028: a generation that ended by itself before the close
            // removed it is ended by its own end, as exited.
            let ours = run.start_info.agent_id.clone();
            if !self.ended_by_itself(ours.as_deref()) {
                let pane_id = pane_id.to_string();
                self.end_run(&pane_id, EndReason::Closed, None, now);
            }
            return;
        }

        if let Some(usage) = &run.usage {
            usage.observe(event);
        }
        // A known agent stays the kind the run started as, which is the value
        // the agent itself received in `OTEL_RESOURCE_ATTRIBUTES`: an event's
        // self-reported type relabelling only the deck's side would split the
        // join on that key. A run that started as no known agent takes the
        // kind its hooks name from here on ([`resolved_agent`]).
        run.cards.apply_event(event.clone());
        if let Some(agent) = hook_agent(run) {
            run.learned_agent = Some(agent);
        }
        match event.event_type {
            EventType::WaitingForInput => {
                run.idle_wait = event
                    .metadata
                    .get(crate::quota_block::NOTIFICATION_TYPE_METADATA_KEY)
                    .is_some_and(|kind| {
                        !crate::quota_block::WORKING_NOTIFICATION_TYPES.contains(&kind.as_str())
                    });
            }
            EventType::PermissionRequest => run.idle_wait = false,
            _ => {}
        }
        if let Some(question) = event.question() {
            run.note_question(&question);
        }
        // The daemon answered a question for a person in the deck's question
        // UI. The answer is theirs, and counts from when the daemon accepted
        // it: the daemon makes this event and stamps it then, on the clock
        // input is timed by. An answer the deck typed into the pane counted
        // as those keys were written, so its event is not counted again.
        if event
            .metadata
            .contains_key(crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY)
            && !run.answer_was_typed(event.resolved_question_id())
        {
            let accepted = event
                .timestamp
                .timestamp_nanos_opt()
                .and_then(|t| u64::try_from(t).ok())
                .map_or(now, |t| t.min(now));
            run.human_input(accepted, InterventionKind::Answer);
        }
        let pull_request = (event.event_type == EventType::PullRequest)
            .then(|| card_pull_request(&run.cards, event))
            .flatten();
        if let Some((pr, revision)) = pull_request {
            if let Some(pr) = &pr {
                run.pr_number = Some(pr.number);
            }
            let key = pr.as_ref().map(PrKey::of);
            self.observe_ci(pane_id, key, pr.and_then(|pr| pr.checks), revision, now);
        }
        let Some(run) = self.runs.get_mut(pane_id) else {
            return;
        };
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

    /// The card of the run open on `pane_id` now holds pull request `key`, or
    /// none, with `checks`, as the daemon's report of revision `revision`
    /// told it: the run joins that pull request's shared CI state, and lets go
    /// of the one it held before. The run counts a round it saw end; the
    /// shared state takes the report only when it is not older than the
    /// newest one it already took ([`SharedCi`]). A round the run saw end
    /// that overlaps no recorded round and not the open stretch is recorded
    /// as the run saw it, in the run's trace, and leaves the shared state as
    /// it is.
    fn observe_ci(
        &mut self,
        pane_id: &str,
        key: Option<PrKey>,
        checks: Option<crate::pull_request_info::PullRequestChecks>,
        revision: u64,
        now: u64,
    ) {
        let Some(run) = self.runs.get_mut(pane_id) else {
            return;
        };
        let run_id = run.start_info.run_id.clone();
        let held = std::mem::replace(&mut run.ci_pr, key.clone());
        if held != key {
            run.ci_seen = None;
            run.ci_pending_since = None;
            if let Some(held) = held {
                let identity = child_identity(run);
                self.release_ci(&held, &run_id, identity, now);
            }
        }
        let Some(key) = key else {
            return;
        };
        let seen = checks.and_then(|checks| CiState::from_checks(checks.state));
        // The round this report ended as the run's card saw it: from the
        // revision and time its pending began, to this result.
        let mut ended = None;
        if let Some(seen) = seen
            && let Some(run) = self.runs.get_mut(pane_id)
        {
            if run.ci_seen == Some(CiState::Pending) && seen != CiState::Pending {
                run.ci_rounds += 1;
                if seen == CiState::Failing {
                    run.ci_failures += 1;
                }
                ended = run.ci_pending_since.take().map(|since| (since, seen));
            } else if seen == CiState::Pending && run.ci_seen != Some(CiState::Pending) {
                run.ci_pending_since = Some((revision, now));
            }
            run.ci_seen = Some(seen);
        }
        let mut shared = match self.ci.remove(&key) {
            Some(shared) => shared,
            None => self.resume_ci(&key),
        };
        if !shared.holders.contains(&run_id) {
            shared.holders.push(run_id);
        }
        // A report with no checks, or checks in no known state, changes
        // nothing in the shared state, so it does not order it either: a
        // result another run reports at an older revision still ends the
        // stretch.
        if seen.is_some() && (revision == 0 || revision >= shared.revision) {
            shared.revision = shared.revision.max(revision);
            let was_open = shared.ci.is_open();
            let effects = shared
                .ci
                .observe(Some(key.number), checks, now, new_span_id);
            if shared.ci.is_open() && shared.opener.is_none() {
                shared.opener = self.run_by_id(&shared.holders[0]).map(|run| StretchOpener {
                    run_id: run.start_info.run_id.clone(),
                    trace_id: run.trace_id,
                    span_id: run.span_id,
                    identity: child_identity(run),
                });
            }
            let reopened = effects
                .iter()
                .any(|effect| matches!(effect, CiEffect::Ended { .. }));
            for effect in effects {
                self.record_ci(&mut shared, effect, now);
            }
            if shared.ci.is_open() && (!was_open || reopened) {
                shared.open_since = revision;
            }
        }
        if let Some(((from, start), result)) = ended
            && from != 0
            && revision != 0
            && !shared.rounds.covers(from, revision)
            && !(shared.ci.is_open() && shared.open_since <= revision)
            && let Some(run) = self.runs.get(pane_id)
        {
            shared.rounds.record(from, revision);
            self.finished.push(ci_span(
                (run.trace_id, run.span_id, child_identity(run)),
                new_span_id(),
                (start, now),
                key.number,
                Some(result),
                checks,
            ));
        }
        self.ci.insert(key, shared);
    }

    /// The shared CI state of pull request `key` as a run takes hold of it:
    /// fresh, but ordered after what was recorded while runs held it before
    /// ([`ReleasedCi`]).
    fn resume_ci(&mut self, key: &PrKey) -> SharedCi {
        let Some(released) = self.released_ci.remove(key) else {
            return SharedCi::default();
        };
        SharedCi {
            ci: released
                .settled
                .map(|settled| PrCi::resumed(key.number, settled))
                .unwrap_or_default(),
            revision: released.revision,
            rounds: released.rounds,
            ..SharedCi::default()
        }
    }

    /// The run `run_id`, whose [`child_identity`] is now `identity`, no
    /// longer holds pull request `key`: it ended, or its card moved on. The
    /// last run to let go ends the open stretch without a result.
    fn release_ci(&mut self, key: &PrKey, run_id: &str, identity: Vec<Attribute>, now: u64) {
        let Some(mut shared) = self.ci.remove(key) else {
            return;
        };
        shared.holders.retain(|holder| holder != run_id);
        if let Some(opener) = shared.opener.as_mut().filter(|o| o.run_id == run_id) {
            opener.identity = identity;
        }
        if !shared.holders.is_empty() {
            self.ci.insert(key.clone(), shared);
            return;
        }
        if let Some(effect) = shared.ci.end_run() {
            self.record_ci(&mut shared, effect, now);
        }
        if self.released_ci.len() >= MAX_RELEASED_PULL_REQUESTS
            && let Some(oldest) = self
                .released_ci
                .iter()
                .min_by_key(|(_, released)| released.revision)
                .map(|(key, _)| key.clone())
        {
            self.released_ci.remove(&oldest);
        }
        self.released_ci.insert(
            key.clone(),
            ReleasedCi {
                revision: shared.revision,
                settled: shared.ci.settled(),
                rounds: shared.rounds,
            },
        );
    }

    /// Record what one observation of `shared`'s checks changed: a change is
    /// an event on the run that has held the pull request longest, and a
    /// stretch that ended is a span in the trace of the run it opened in. Each
    /// run counts the rounds it saw itself ([`Self::observe_ci`]).
    fn record_ci(&mut self, shared: &mut SharedCi, effect: CiEffect, now: u64) {
        match effect {
            CiEffect::Change { from, to, pr } => {
                let mut attributes = Vec::with_capacity(3);
                if let Some(from) = from {
                    attributes.push(Attribute::str(AttrKey::CiFrom, from.as_str()));
                }
                attributes.push(Attribute::str(AttrKey::CiTo, to.as_str()));
                attributes.push(Attribute::int(AttrKey::PrNumber, saturating_i64(pr)));
                if let Some(run) = shared.holders.first().and_then(|id| self.run_by_id(id)) {
                    run.push_event(SpanEventRecord {
                        name: EVENT_CI_CHANGE,
                        time_unix_nanos: now,
                        attributes,
                    });
                }
            }
            CiEffect::Ended {
                stretch,
                result,
                checks,
            } => {
                // A stretch opened by a report with no revision may have
                // begun at any revision before its end.
                if shared.revision != 0 {
                    shared.rounds.record(shared.open_since, shared.revision);
                }
                let Some(opener) = shared.opener.take() else {
                    return;
                };
                let identity = match self.run_by_id(&opener.run_id) {
                    Some(run) => child_identity(run),
                    None => opener.identity,
                };
                self.finished.push(ci_span(
                    (opener.trace_id, opener.span_id, identity),
                    stretch.span_id,
                    (stretch.start, now),
                    stretch.pr,
                    result,
                    checks,
                ));
            }
        }
    }

    /// The open run `run_id`, replaced and waiting for its end or not.
    fn run_by_id(&mut self, run_id: &str) -> Option<&mut ActiveRun> {
        self.runs
            .values_mut()
            .chain(self.replaced.iter_mut().map(|r| &mut r.run))
            .find(|run| run.start_info.run_id == run_id)
    }

    /// The generations of the open runs not yet known to belong to a
    /// dispatched unit, for the caller to look up in the deck's record of
    /// dispatched units once it has let go of the tracker.
    pub fn runs_without_dispatch(&self) -> Vec<RunGeneration> {
        self.runs
            .values()
            .chain(self.replaced.iter().map(|r| &r.run))
            .filter(|run| run.dispatch_name.is_none())
            .map(|run| RunGeneration {
                run_id: run.start_info.run_id.clone(),
                pane_id: run.start_info.pane_id.clone(),
                agent_id: run.start_info.agent_id.clone(),
                orchestration_id: run
                    .start_info
                    .orchestration
                    .as_ref()
                    .and_then(|o| o.id.clone()),
            })
            .collect()
    }

    /// The open run `run_id` belongs to the dispatched unit `name`. Ignored
    /// for a run that has ended.
    pub fn set_dispatch_name(&mut self, run_id: &str, name: &str) {
        if let Some(run) = self.run_by_id(run_id) {
            run.dispatch_name = Some(name.to_string());
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
    pub fn observe_human_input(&mut self, pane_id: &str, now: u64, kind: InterventionKind) {
        if let Some(run) = self.runs.get_mut(pane_id) {
            run.human_input(now, kind);
        }
    }

    /// Whether `run_id` is open on `pane_id`, or replaced and waiting for its
    /// end: whether [`Self::observe_run_input`] would count input for it.
    pub fn takes_input(&self, pane_id: &str, run_id: &str) -> bool {
        self.run_id(pane_id) == Some(run_id)
            || self
                .replaced
                .iter()
                .any(|r| r.run.start_info.pane_id == pane_id && r.run.start_info.run_id == run_id)
    }

    /// Input from a person at an attached client reached `pane_id`, while the
    /// run open on it, or replaced and waiting for its end, is `run_id`. Input
    /// the deck injects itself (a delegated task, an inbox delivery) must not
    /// be reported here. Keystrokes less than [`INTERVENTION_COALESCE_NANOS`]
    /// apart count as one intervention, of the strongest `kind` among them.
    /// Input is read in the order it is reported in, against the run's cards
    /// as they stand then: report it before the first event the deck
    /// broadcast after it, and after every event before.
    pub fn observe_run_input(
        &mut self,
        pane_id: &str,
        run_id: &str,
        now: u64,
        kind: InterventionKind,
    ) {
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
            run.human_input(now, kind);
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
            self.retire_unended(r);
        }
    }

    /// End replaced run `r`, whose own end did not come, as
    /// [`EndReason::Unknown`] at the time it was replaced, and forget its
    /// generation's note ([`EndedByThemselves`]): with its run gone there is
    /// nothing left for the note to keep open, and the end that would have
    /// forgotten it may never come.
    fn retire_unended(&mut self, r: ReplacedRun) {
        if let Some(agent_id) = r.run.start_info.agent_id.clone() {
            self.forget_ended_by_itself(&agent_id);
        }
        self.finish(r.run, EndReason::Unknown, None, r.at);
    }

    /// Finish `run`'s spans: its held-back input counted, its open status
    /// interval and the run itself ended at `now`.
    fn finish(&mut self, mut run: ActiveRun, reason: EndReason, exit_code: Option<i32>, now: u64) {
        if let Some(held) = self.held_input.as_mut() {
            for (written, kind) in held(&run.start_info.pane_id, &run.start_info.run_id) {
                run.human_input(written, kind);
            }
        }
        if let Some(open) = run.status.take() {
            self.finished.push(status_span(&run, open, now));
        }
        for open in std::mem::take(&mut run.operations) {
            self.finished
                .push(operation_span(&run, open, now, OperationEnd::RunEnd));
        }
        if let Some(key) = run.ci_pr.take() {
            let identity = child_identity(&run);
            self.release_ci(&key, &run.start_info.run_id, identity, now);
        }
        let usage = run.usage.take();
        let run_id = run.start_info.run_id.clone();
        self.handoffs.run_ended(
            &run_id,
            reason == EndReason::DaemonShutdown,
            now,
            &mut self.finished,
        );
        let span = run_span(run, reason, exit_code, now);
        match usage {
            // A run with nothing to read is completed here, its span queued
            // with the others it finished.
            Some(usage) if !usage.reads() => self.finished.push(
                FinishingRun {
                    span,
                    usage,
                    run_id,
                    reason,
                    exit_code,
                }
                .complete(),
            ),
            Some(usage) => self.finishing.push(FinishingRun {
                span,
                usage,
                run_id,
                reason,
                exit_code,
            }),
            None => self.finished.push(span),
        }
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
        // A prepared run keeps its note until it opens with the close it
        // may hold ([`Self::commit_run`]).
        let mut held = false;
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
            held = true;
        }
        if !held && let Some(agent_id) = agent_id {
            self.forget_ended_by_itself(agent_id);
        }
        self.expire_replaced(now);
    }

    /// End every open run and forget every prepared one, for daemon
    /// shutdown. A replaced run still waiting for its own end ends as
    /// [`EndReason::Unknown`], at the time it was replaced.
    pub fn end_all(&mut self, reason: EndReason, now: u64) {
        let prepared: Vec<String> = self.pending.keys().cloned().collect();
        for run_id in prepared {
            self.release_posts(&run_id);
        }
        for (_, pending) in self.pending.drain() {
            pending.forget();
        }
        for r in std::mem::take(&mut self.replaced) {
            self.finish(r.run, EndReason::Unknown, None, r.at);
        }
        let panes: Vec<String> = self.runs.keys().cloned().collect();
        for pane in panes {
            self.end_run(&pane, reason, None, now);
        }
        self.ended_by_themselves
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.handoffs.end_all(now, &mut self.finished);
    }

    /// Issue #1981 M8: the runs open (replaced ones still waiting for their
    /// end included) and prepared — one per agent PTY the deck started.
    pub fn open_runs(&self) -> usize {
        self.runs.len() + self.replaced.len() + self.pending.len()
    }

    /// Issue #1981 M7: the delegate path posted a task: open its hand-off
    /// ([`super::handoff`]).
    pub fn delegate_posted(&mut self, posted: Posted) {
        let orchestrator = run_ref(
            &self.runs,
            &self.replaced,
            &self.pending,
            &posted.orchestrator_pane,
            posted.orchestrator_agent_id.as_deref(),
        );
        let worker = run_ref(
            &self.runs,
            &self.replaced,
            &self.pending,
            &posted.worker_pane,
            Some(&posted.worker_agent_id),
        )
        .map(|run| run.run_id);
        let (runs, replaced, pending) = (&self.runs, &self.replaced, &self.pending);
        let run_of = |pane: &str, agent: &str| run_ref(runs, replaced, pending, pane, Some(agent));
        self.handoffs
            .posted(posted, orchestrator, worker, &run_of, &mut self.finished);
    }

    /// Issue #1981 M7: the delegate path posted a task, and its place on the
    /// broadcast has come: open its hand-off now, or, while its
    /// orchestrator's run is prepared and not opened yet, hold it with that
    /// run's events, after the ones held so far, for
    /// [`Self::commit_run`] to open in that place. Opened now, the hand-off
    /// would miss the run's trace; opened after the events held, it would
    /// miss a run they end. Past [`MAX_PENDING_EVENTS`] posts held for one
    /// run, a post opens at once, in a trace of its own with no parent: that
    /// takes as many posts from one orchestrator before telemetry applies
    /// its run's `Commit`, which no real workload comes near.
    pub fn place_post(&mut self, posted: Posted) {
        let prepared = run_ref(
            &self.runs,
            &self.replaced,
            &self.pending,
            &posted.orchestrator_pane,
            posted.orchestrator_agent_id.as_deref(),
        )
        .filter(|run| run.span.is_none())
        .and_then(|run| self.pending.get_mut(&run.run_id))
        .filter(|pending| pending.held_posts() < MAX_PENDING_EVENTS);
        match prepared {
            Some(pending) => {
                let before = pending.events.len();
                pending.handoffs.push((before, HeldHandoff::Post(posted)));
            }
            None => self.delegate_posted(posted),
        }
    }

    /// Issue #1981 M7: apply one inbox state change from the daemon's
    /// broadcast to the hand-off it concerns, or count a report that names
    /// no task on the run of the agent it reached: that agent's generation
    /// on its pane, whether its run is open, replaced and waiting for its
    /// end, or prepared ([`run_ref`]). A prepared run holds the count after
    /// the events held so far, for [`Self::commit_run`] to add in that
    /// place. A report for a generation whose run has ended is counted on
    /// no run, never on the generation that replaced it.
    pub fn observe_inbox(&mut self, notice: &crate::event::InboxNotice, now: u64) {
        let Some(change) = Change::of(notice) else {
            return;
        };
        if let Change::Uncorrelated { pane, agent } = &change {
            let run = run_ref(
                &self.runs,
                &self.replaced,
                &self.pending,
                pane,
                agent.as_deref(),
            );
            match run {
                Some(run) if run.span.is_some() => self.count_uncorrelated(&run.run_id, 1),
                Some(run) => {
                    if let Some(pending) = self.pending.get_mut(&run.run_id) {
                        pending.hold_uncorrelated();
                    }
                }
                None => {}
            }
            return;
        }
        if !self.hold_notice(&change, now) {
            self.apply_change(change, now);
        }
    }

    /// Issue #1981 M7 review: hold `change`, seen at `now`, in the prepared
    /// run holding the post it concerns, after the events held so far, for
    /// [`Self::commit_run`] to apply in that place: applied before the run
    /// opens, the post would replay it ahead of a close held before it.
    /// Whether it was held. Past [`MAX_PENDING_EVENTS`] notices held for one
    /// run, a notice is applied at once, and replayed as the post opens.
    fn hold_notice(&mut self, change: &Change, now: u64) -> bool {
        let handoffs = &self.handoffs;
        let early_report = |id: &str| handoffs.early_report_task(id).map(str::to_string);
        let Some(pending) = self
            .pending
            .values_mut()
            .find(|pending| pending.concerns(change, &early_report))
            .filter(|pending| pending.held_notices() < MAX_PENDING_EVENTS)
        else {
            return false;
        };
        let before = pending.events.len();
        pending
            .handoffs
            .push((before, HeldHandoff::Notice(change.clone(), now)));
        true
    }

    /// Apply `change`, seen at `now`, to the hand-offs.
    fn apply_change(&mut self, change: Change, now: u64) {
        let (runs, replaced, pending) = (&self.runs, &self.replaced, &self.pending);
        let run_of = |pane: &str, agent: &str| run_ref(runs, replaced, pending, pane, Some(agent));
        self.handoffs
            .apply(change, now, &run_of, &mut self.finished);
    }

    /// Issue #1981 M7: end every hand-off past its age bound at `now`, and
    /// forget the held notices past theirs, with no inbox change needed to
    /// notice ([`super::handoff::Handoffs::expire`]).
    pub fn expire_handoffs(&mut self, now: u64) {
        self.handoffs.expire(now, &mut self.finished);
    }

    /// Hand-offs open now, and notices held for a post not seen yet.
    #[cfg(test)]
    pub fn handoffs_held(&self) -> (usize, usize) {
        (self.handoffs.open_count(), self.handoffs.early_count())
    }

    /// Hand-off reports that arrived and wait to be read.
    #[cfg(test)]
    pub fn unread_count(&self) -> usize {
        self.handoffs.unread_count()
    }

    /// Runs open (replaced ones still waiting for their end included) and
    /// prepared: what the tracker holds between spans.
    #[cfg(test)]
    pub fn held(&self) -> (usize, usize) {
        (self.runs.len() + self.replaced.len(), self.pending.len())
    }

    /// The spans finished since the last call, each run's usage read here
    /// and now ([`FinishingRun::complete`]).
    pub fn take_finished(&mut self) -> Vec<SpanRecord> {
        let (mut finished, finishing) = self.take_finished_deferred();
        finished.extend(finishing.into_iter().map(FinishingRun::complete));
        finished
    }

    /// The spans finished since the last call, and the runs that ended since,
    /// whose spans are complete once their usage is read
    /// ([`FinishingRun::complete`]): the caller reads it after letting go of
    /// the tracker.
    pub fn take_finished_deferred(&mut self) -> (Vec<SpanRecord>, Vec<FinishingRun>) {
        (
            std::mem::take(&mut self.finished),
            std::mem::take(&mut self.finishing),
        )
    }
}

/// A run that ended, whose `agent.run` span waits for its usage record to be
/// drained (PRD #1858).
pub struct FinishingRun {
    span: SpanRecord,
    usage: RunUsage,
    run_id: String,
    reason: EndReason,
    exit_code: Option<i32>,
}

impl FinishingRun {
    /// Drain the run's usage record and put what it says on the span: the
    /// usage and termination attributes, and one [`EVENT_MODEL_USAGE`] event
    /// per model, past the cap on the run's other events, so a run's usage is
    /// not dropped for having changed status often. A drain that panics
    /// gives the span with its usage incomplete
    /// ([`RunUsage::finish_or_incomplete`]). Reads files.
    pub fn complete(self) -> SpanRecord {
        let FinishingRun {
            mut span,
            usage,
            run_id,
            reason,
            exit_code,
        } = self;
        let usage = usage.finish_or_incomplete(&run_id, reason, exit_code);
        // The run's own id and agent are already the span's.
        let own = |a: &super::accounting::AccountingAttribute| {
            !matches!(a.key(), AccountingKey::RunId | AccountingKey::Agent)
        };
        let attribute = |a: &super::accounting::AccountingAttribute| Attribute {
            key: AttrKey::Accounting(a.key()),
            value: a.value().clone(),
        };
        // Each model's event names the run's agent and orchestration, as the
        // run's other events do.
        let identity: Vec<Attribute> = span
            .attributes
            .iter()
            .filter(|a| {
                matches!(
                    a.key,
                    AttrKey::Agent | AttrKey::Role | AttrKey::OrchestrationId
                )
            })
            .cloned()
            .collect();
        span.attributes
            .extend(usage.run.iter().filter(|a| own(a)).map(attribute));
        let end = span.end_unix_nanos;
        span.events.extend(usage.models.iter().map(|model| {
            SpanEventRecord {
                name: EVENT_MODEL_USAGE,
                time_unix_nanos: end,
                attributes: model
                    .iter()
                    .map(attribute)
                    .chain(identity.iter().cloned())
                    .collect(),
            }
        }));
        span
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
                finished.push(operation_span(run, oldest, now, OperationEnd::Cap));
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
                finished.push(operation_span(run, open, now, OperationEnd::Agent));
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
        // Claude Code's hook cuts `tool_detail` at 120 bytes and keeps the
        // whole command beside it, which the later commands of a long line
        // need.
        let command = event
            .metadata
            .get(BASH_COMMAND_METADATA_KEY)
            .or(event.tool_detail.as_ref());
        let operation = (event.event_type == EventType::ToolStart
            && is_shell_tool(event.tool_name.as_deref()))
        .then(|| describe_tool(event.tool_name.as_deref(), command.map(String::as_str)));
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

/// The pull request the card `event` reported on now holds — `Some(None)` when
/// it holds none — or `None` when the run holds no such card, so the report had
/// nothing to land on. Read after the card applied the report, so the card's own
/// rules decide: a report older than the one it holds is ignored, and a report
/// of no pull request clears it. With it, the revision of the report the card
/// holds it from ([`crate::state::SessionState::pull_request_revision`]).
fn card_pull_request(
    cards: &AppState,
    event: &AgentEvent,
) -> Option<(Option<crate::pull_request_info::PullRequestInfo>, u64)> {
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
        .map(|(_, s)| (s.pull_request.clone(), s.pull_request_revision))
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

/// `deck.agent.source` for an agent the run started as.
const AGENT_SOURCE_SPAWN: &str = "spawn";

/// `deck.agent.source` for an agent the run's hooks named.
const AGENT_SOURCE_HOOK: &str = "hook";

/// The agent `run` is, as far as the deck knows it now, and where that came
/// from: the kind it started as, which stays even when its hooks name
/// another; else the kind its hooks have named since, on the latest of the
/// pane's sessions that names one, kept when no session does any more (a
/// `SessionEnd` removed it); else [`AgentType::None`], from nowhere.
fn resolved_agent(run: &ActiveRun) -> (AgentType, Option<&'static str>) {
    if run.start_info.agent != AgentType::None {
        return (run.start_info.agent.clone(), Some(AGENT_SOURCE_SPAWN));
    }
    run.learned_agent
        .clone()
        .map_or((AgentType::None, None), |agent| {
            (agent, Some(AGENT_SOURCE_HOOK))
        })
}

/// The agent named on the latest of `run`'s pane's sessions that names one.
fn hook_agent(run: &ActiveRun) -> Option<AgentType> {
    run.cards
        .sessions
        .values()
        .filter(|s| {
            s.pane_id.as_deref() == Some(run.start_info.pane_id.as_str())
                && s.agent_type != AgentType::None
        })
        .max_by_key(|s| s.last_activity)
        .map(|s| s.agent_type.clone())
}

/// The run's agent, as [`resolved_agent`] knows it now, and its role and
/// orchestration where it has them, as its `agent.run` carries them: for its
/// child spans and its span events.
fn agent_identity(run: &ActiveRun) -> Vec<Attribute> {
    let mut attributes = vec![Attribute::str(
        AttrKey::Agent,
        agent_name(&resolved_agent(run).0),
    )];
    if let Some(orch) = &run.start_info.orchestration {
        attributes.extend(Attribute::opt_str(AttrKey::Role, orch.role.as_deref()));
        attributes.extend(Attribute::opt_str(
            AttrKey::OrchestrationId,
            orch.id.as_deref(),
        ));
    }
    attributes
}

fn identity_attributes(start: &RunStart) -> Vec<Attribute> {
    vec![
        Attribute::str(AttrKey::RunId, &start.run_id),
        Attribute::str(AttrKey::PaneId, &start.pane_id),
    ]
}

/// What a child span of `run` says of it: the run and pane, and
/// [`agent_identity`].
fn child_identity(run: &ActiveRun) -> Vec<Attribute> {
    let mut attributes = identity_attributes(&run.start_info);
    attributes.extend(agent_identity(run));
    attributes
}

fn status_span(run: &ActiveRun, open: OpenStatus, now: u64) -> SpanRecord {
    let mut attributes = child_identity(run);
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

/// An `agent.ci` span for a stretch of pending checks of pull request `pr`
/// over `(start, end)`, in the trace of the run `(trace, parent, identity)`
/// names, ending in `result` with `checks`.
fn ci_span(
    (trace_id, parent, mut attributes): (TraceId, SpanId, Vec<Attribute>),
    span_id: SpanId,
    (start, end): (u64, u64),
    pr: u64,
    result: Option<CiState>,
    checks: Option<crate::pull_request_info::PullRequestChecks>,
) -> SpanRecord {
    attributes.push(Attribute::int(AttrKey::PrNumber, saturating_i64(pr)));
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
    SpanRecord {
        trace_id,
        span_id,
        parent_span_id: Some(parent),
        name: SPAN_AGENT_CI,
        start_unix_nanos: start,
        end_unix_nanos: end.max(start),
        attributes,
        events: Vec::new(),
    }
}

fn operation_span(run: &ActiveRun, open: OpenOperation, now: u64, end: OperationEnd) -> SpanRecord {
    let mut attributes = child_identity(run);
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
    attributes.extend(Attribute::opt_str(AttrKey::OperationEnd, end.as_str()));
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
    let (agent, source) = resolved_agent(&run);
    attributes.push(Attribute::str(AttrKey::Agent, agent_name(&agent)));
    attributes.extend(Attribute::opt_str(AttrKey::AgentSource, source));
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
        AttrKey::BranchName,
        run.branch.as_deref(),
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
    attributes.extend(Attribute::opt_str(
        AttrKey::DispatchName,
        run.dispatch_name.as_deref(),
    ));
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
        saturating_i64(run.ci_rounds),
    ));
    attributes.push(Attribute::int(
        AttrKey::CiFailuresCount,
        saturating_i64(run.ci_failures),
    ));
    // On an orchestrator's run always, and on another only when it has one.
    let orchestrator = start.orchestration.as_ref().is_some_and(|o| o.orchestrator);
    if orchestrator || run.uncorrelated_reports > 0 {
        attributes.push(Attribute::int(
            AttrKey::HandoffsUncorrelatedCount,
            saturating_i64(run.uncorrelated_reports),
        ));
    }
    let end = now.max(run.start);
    SpanRecord {
        trace_id: run.trace_id,
        span_id: run.span_id,
        parent_span_id: None,
        name: SPAN_AGENT_RUN,
        start_unix_nanos: run.start,
        end_unix_nanos: end,
        attributes,
        events: run.events,
    }
}

/// The run of agent `agent_id` on `pane_id`: the one open there, else one
/// replaced and waiting for its end, else one prepared. An unknown agent id
/// matches the run open on the pane.
fn run_ref(
    runs: &HashMap<String, ActiveRun>,
    replaced: &[ReplacedRun],
    pending: &HashMap<String, PendingRun>,
    pane_id: &str,
    agent_id: Option<&str>,
) -> Option<RunRef> {
    let same = |ours: Option<&str>| match (ours, agent_id) {
        (Some(ours), Some(theirs)) => ours == theirs,
        _ => true,
    };
    let active = runs
        .get(pane_id)
        .filter(|run| same(run.start_info.agent_id.as_deref()))
        .or_else(|| {
            replaced.iter().map(|r| &r.run).find(|run| {
                run.start_info.pane_id == pane_id
                    && agent_id.is_some()
                    && run.start_info.agent_id.as_deref() == agent_id
            })
        });
    if let Some(run) = active {
        return Some(RunRef {
            run_id: run.start_info.run_id.clone(),
            span: Some((run.trace_id, run.span_id)),
            orchestration_id: run
                .start_info
                .orchestration
                .as_ref()
                .and_then(|o| o.id.clone()),
        });
    }
    pending
        .values()
        .find(|p| {
            p.start.pane_id == pane_id
                && agent_id.is_some()
                && p.start.agent_id.as_deref() == agent_id
        })
        .map(|p| RunRef {
            run_id: p.start.run_id.clone(),
            span: None,
            orchestration_id: p.start.orchestration.as_ref().and_then(|o| o.id.clone()),
        })
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
            tracker.observe_human_input("p1", t * second, InterventionKind::Text);
        }
        tracker.observe_human_input("p1", 20 * second, InterventionKind::Text);
        tracker.observe_human_input("unknown-pane", 21 * second, InterventionKind::Text);
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

    /// The `deck.intervention.kind` of each intervention on `pane`'s run,
    /// ended at `end`.
    fn kinds_at_end(tracker: &mut RunTracker, pane: &str, end: u64) -> (Vec<String>, i64) {
        tracker.end_run(pane, EndReason::Closed, None, end);
        let spans = tracker.take_finished();
        let run = run_span_of(&spans);
        let kinds = run
            .events
            .iter()
            .filter(|e| e.name == EVENT_INTERVENTION)
            .map(|e| {
                let kinds: Vec<_> = e
                    .attributes
                    .iter()
                    .filter(|a| a.key == AttrKey::InterventionKind)
                    .collect();
                match &kinds[..] {
                    [a] => match &a.value {
                        AttrValue::Str(kind) => kind.clone(),
                        other => panic!("{other:?}"),
                    },
                    other => panic!("{other:?}"),
                }
            })
            .collect();
        let Some(AttrValue::Int(count)) = attr(run, AttrKey::InterventionsCount) else {
            panic!("no intervention count");
        };
        (kinds, *count)
    }

    #[test]
    fn a_burst_takes_its_strongest_kind() {
        use InterventionKind::*;
        let second = 1_000_000_000;
        for (inputs, expected) in [
            (vec![Navigate, Text], "text"),
            (vec![Text, Navigate], "text"),
            (vec![Text, Paste], "paste"),
            (vec![Paste, Text, Interrupt, Navigate], "interrupt"),
            (vec![Navigate], "navigate"),
        ] {
            let mut tracker = RunTracker::new();
            tracker.begin_run(start("p1"), 0);
            for (i, kind) in inputs.iter().enumerate() {
                tracker.observe_human_input("p1", (i as u64 + 1) * second, *kind);
            }
            // A new burst starts with its own kind.
            tracker.observe_human_input("p1", 30 * second, Navigate);
            assert_eq!(
                kinds_at_end(&mut tracker, "p1", 40 * second),
                (vec![expected.to_string(), "navigate".to_string()], 2),
                "{inputs:?}"
            );
        }
    }

    /// A stronger kind later in a burst rewrites only the kind of the burst's
    /// event: the run's identity on it stays.
    #[test]
    fn a_stronger_kind_keeps_the_events_identity() {
        let mut tracker = RunTracker::new();
        let mut s = start("p1");
        s.orchestration = Some(OrchestrationInfo {
            role: Some("coder".into()),
            name: Some("tdd".into()),
            id: Some("orch-1".into()),
            orchestrator: false,
        });
        tracker.begin_run(s, 0);
        tracker.observe_human_input("p1", 1, InterventionKind::Text);
        tracker.observe_human_input("p1", 2, InterventionKind::Interrupt);
        tracker.end_run("p1", EndReason::Closed, None, 10);
        let spans = tracker.take_finished();
        let run = run_span_of(&spans);
        let interventions: Vec<_> = run
            .events
            .iter()
            .filter(|e| e.name == EVENT_INTERVENTION)
            .collect();
        let [event] = &interventions[..] else {
            panic!("{:?}", run.events);
        };
        assert_eq!(
            event.attributes,
            vec![
                Attribute::str(AttrKey::InterventionKind, "interrupt"),
                Attribute::str(AttrKey::Agent, "claude"),
                Attribute::str(AttrKey::Role, "coder"),
                Attribute::str(AttrKey::OrchestrationId, "orch-1"),
            ]
        );
    }

    /// Past the per-run event cap a burst still moves the count, and a
    /// stronger kind later in it rewrites nothing it did not keep.
    #[test]
    fn a_burst_past_the_event_cap_counts_without_an_event() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        let gap = INTERVENTION_COALESCE_NANOS + 1;
        for i in 0..=MAX_EVENTS_PER_RUN as u64 {
            tracker.observe_human_input("p1", (i + 1) * gap, InterventionKind::Navigate);
        }
        let last = (MAX_EVENTS_PER_RUN as u64 + 1) * gap;
        tracker.observe_human_input("p1", last + 1, InterventionKind::Interrupt);
        let (kinds, count) = kinds_at_end(&mut tracker, "p1", last + gap);
        assert_eq!(count, MAX_EVENTS_PER_RUN as i64 + 1);
        assert_eq!(kinds.len(), MAX_EVENTS_PER_RUN);
        assert!(kinds.iter().all(|k| k == "navigate"), "{kinds:?}");
    }

    /// Input while the agent waits on a permission prompt or a question is an
    /// answer; Ctrl+C or Escape then is still an interrupt; Claude's
    /// `idle_prompt` notification is no prompt, so input after it keeps its
    /// own kind.
    #[test]
    fn input_while_a_prompt_is_open_is_an_answer() {
        let idle_prompt = || {
            let mut e = event("p1", EventType::WaitingForInput);
            e.metadata.insert(
                crate::quota_block::NOTIFICATION_TYPE_METADATA_KEY.into(),
                "idle_prompt".into(),
            );
            e
        };
        let permission_prompt = || {
            let mut e = event("p1", EventType::WaitingForInput);
            e.metadata.insert(
                crate::quota_block::NOTIFICATION_TYPE_METADATA_KEY.into(),
                "permission_prompt".into(),
            );
            e
        };
        for (wait, kind, expected) in [
            (
                event("p1", EventType::PermissionRequest),
                InterventionKind::Text,
                "answer",
            ),
            (
                event("p1", EventType::WaitingForInput),
                InterventionKind::Navigate,
                "answer",
            ),
            (permission_prompt(), InterventionKind::Paste, "answer"),
            (
                event("p1", EventType::PermissionRequest),
                InterventionKind::Interrupt,
                "interrupt",
            ),
            (idle_prompt(), InterventionKind::Text, "text"),
            (event("p1", EventType::Idle), InterventionKind::Text, "text"),
        ] {
            let mut tracker = RunTracker::new();
            tracker.begin_run(start("p1"), 0);
            tracker.observe_event(&event("p1", EventType::SessionStart), 1);
            tracker.observe_event(&wait, 2);
            tracker.observe_human_input("p1", 3, kind);
            assert_eq!(
                kinds_at_end(&mut tracker, "p1", 4),
                (vec![expected.to_string()], 1),
                "{:?} {:?}",
                wait.event_type,
                wait.metadata
            );
        }
    }

    /// Input is read against the prompt as the cards show it when the tracker
    /// applies it, which is the order it is handed over in: a status applied
    /// after it — often one the input itself caused — changes nothing, in
    /// either direction.
    #[test]
    fn input_is_read_against_the_prompt_in_the_order_it_is_applied() {
        for (before, after, expected) in [
            // Answered, and the agent went back to work.
            (EventType::PermissionRequest, EventType::Thinking, "answer"),
            // Typed, and a prompt opened after.
            (EventType::Thinking, EventType::PermissionRequest, "text"),
        ] {
            let mut tracker = RunTracker::new();
            tracker.begin_run(start("p1"), 0);
            tracker.observe_event(&event("p1", EventType::SessionStart), 1);
            tracker.observe_event(&event("p1", before.clone()), 2);
            tracker.observe_human_input("p1", 3, InterventionKind::Text);
            tracker.observe_event(&event("p1", after.clone()), 4);
            assert_eq!(
                kinds_at_end(&mut tracker, "p1", 5),
                (vec![expected.to_string()], 1),
                "{before:?} then {after:?}"
            );
        }
    }

    /// What an event's producer stamped on it plays no part in reading the
    /// prompt: a prompt that opens again with a stamp behind the ones before
    /// it is open for the input applied after it.
    #[test]
    fn a_producer_stamp_does_not_place_a_prompt() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        for (event_type, stamped, applied) in [
            (EventType::PermissionRequest, 100, 100),
            (EventType::Thinking, 101, 101),
            (EventType::PermissionRequest, 99, 110),
        ] {
            let mut e = event("p1", event_type);
            e.timestamp = chrono::DateTime::from_timestamp_nanos(stamped);
            tracker.observe_event(&e, applied);
        }
        tracker.observe_human_input("p1", 103, InterventionKind::Text);
        assert_eq!(
            kinds_at_end(&mut tracker, "p1", 120),
            (vec!["answer".to_string()], 1)
        );
    }

    /// Input handed over with a time before the run's last input counts at
    /// that last time, in the burst it joins: the last input never moves
    /// backward, so later input is measured from it.
    #[test]
    fn the_last_input_never_moves_backward() {
        let second = 1_000_000_000;
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_human_input("p1", 11 * second, InterventionKind::Navigate);
        tracker.observe_human_input("p1", second, InterventionKind::Text);
        assert_eq!(tracker.runs["p1"].last_human_input, Some(11 * second));
        tracker.observe_human_input("p1", 12 * second, InterventionKind::Navigate);
        tracker.end_run("p1", EndReason::Closed, None, 20 * second);
        let spans = tracker.take_finished();
        let run = run_span_of(&spans);
        let interventions: Vec<_> = run
            .events
            .iter()
            .filter(|e| e.name == EVENT_INTERVENTION)
            .map(|e| (e.time_unix_nanos, e.attributes[0].value.clone()))
            .collect();
        assert_eq!(
            interventions,
            vec![(11 * second, AttrValue::Str("text".into()))]
        );
    }

    /// The deck's answer counts from when the daemon stamped its event, which
    /// is when it accepted the answer, not from when the tracker applies it.
    #[test]
    fn a_deck_answer_counts_from_its_stamp() {
        let second = 1_000_000_000;
        let mut answered = event("p1", EventType::Thinking);
        answered.metadata.insert(
            crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY.into(),
            crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.into(),
        );
        answered.timestamp = chrono::DateTime::from_timestamp_nanos(11 * second as i64);
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_human_input("p1", second, InterventionKind::Text);
        tracker.observe_event(&answered, 15 * second);
        tracker.observe_human_input("p1", 12 * second, InterventionKind::Text);
        tracker.end_run("p1", EndReason::Closed, None, 20 * second);
        let spans = tracker.take_finished();
        let run = run_span_of(&spans);
        let interventions: Vec<_> = run
            .events
            .iter()
            .filter(|e| e.name == EVENT_INTERVENTION)
            .map(|e| (e.time_unix_nanos, e.attributes[0].value.clone()))
            .collect();
        assert_eq!(
            interventions,
            vec![
                (second, AttrValue::Str("text".into())),
                (11 * second, AttrValue::Str("answer".into())),
            ]
        );
    }

    /// The deck's own "answered" event is an answer: a held question's answer
    /// types nothing into the pane. Keys the deck typed for the same answer
    /// coalesce with it into one.
    #[test]
    fn an_answer_from_the_deck_question_ui_is_an_answer() {
        let answered = || {
            let mut e = event("p1", EventType::Thinking);
            e.metadata.insert(
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY.into(),
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.into(),
            );
            e
        };
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&answered(), 10);
        assert_eq!(
            kinds_at_end(&mut tracker, "p1", 20),
            (vec!["answer".to_string()], 1)
        );

        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::PermissionRequest), 1);
        tracker.observe_human_input("p1", 10, InterventionKind::Text);
        tracker.observe_event(&answered(), 11);
        assert_eq!(
            kinds_at_end(&mut tracker, "p1", 20),
            (vec!["answer".to_string()], 1)
        );

        // A question the producer stopped waiting on is not an answer.
        let mut released = event("p1", EventType::Unknown);
        released.metadata.insert(
            crate::event::QUESTION_RELEASED_BY_DECK_METADATA_KEY.into(),
            crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.into(),
        );
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&released, 10);
        assert_eq!(kinds_at_end(&mut tracker, "p1", 20), (vec![], 0));
    }

    /// The deck's "answered" event for a question it answers by typing keys
    /// into the pane is not counted again: the keys counted as they were
    /// written, however long before the event. A held question's answer types
    /// nothing, so its event is the one count.
    #[test]
    fn a_deck_answer_counts_once_on_either_channel() {
        let second = 1_000_000_000;
        let asking = |channel: &str| {
            let mut e = event("p1", EventType::PermissionRequest);
            e.set_question(
                &serde_json::from_value(serde_json::json!({
                    "id": "q1",
                    "kind": "permission",
                    "questions": [{"prompt": "Allow?", "options": [
                        {"index": 1, "label": "Yes", "role": "allow_once"},
                        {"index": 2, "label": "No", "role": "deny"},
                    ]}],
                    "raised_at_ms": 1,
                    "channel": channel,
                }))
                .unwrap(),
            );
            assert!(e.question().is_some(), "the fixture is a question");
            e
        };
        let answered = || {
            let mut e = event("p1", EventType::Thinking);
            e.metadata.insert(
                crate::event::QUESTION_RESOLVED_METADATA_KEY.into(),
                "q1".into(),
            );
            e.metadata.insert(
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY.into(),
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.into(),
            );
            e.timestamp = chrono::DateTime::from_timestamp_nanos(10 * second as i64);
            e
        };

        // The deck typed the answer's last key more than the coalescing
        // window before it broadcast the event.
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&asking("keys"), 2);
        tracker.observe_human_input("p1", 3 * second, InterventionKind::Text);
        tracker.observe_event(&answered(), 10 * second);
        assert_eq!(
            kinds_at_end(&mut tracker, "p1", 20 * second),
            (vec!["answer".to_string()], 1),
            "keys channel"
        );

        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&asking("held"), 2);
        tracker.observe_event(&answered(), 10 * second);
        assert_eq!(
            kinds_at_end(&mut tracker, "p1", 20 * second),
            (vec!["answer".to_string()], 1),
            "held channel"
        );

        // A question asked again under its id on the held channel is answered
        // without typing, whatever channel it was on before.
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&asking("keys"), 2);
        tracker.observe_event(&asking("held"), 3);
        tracker.observe_event(&answered(), 10 * second);
        assert_eq!(
            kinds_at_end(&mut tracker, "p1", 20 * second),
            (vec!["answer".to_string()], 1),
            "re-asked on the held channel"
        );
    }

    /// Issue #1981 M6: the branch read as the run was prepared is the run's,
    /// on its span however soon the run ends; a run begun without one has
    /// none.
    #[test]
    fn a_prepared_runs_branch_is_on_its_span() {
        let mut tracker = RunTracker::new();
        tracker.prepare_run(start("p1"), 0);
        tracker.set_branch("run-p1", Some("agent/dispatch-x".into()));
        tracker.commit_run("run-p1");
        tracker.end_run("p1", EndReason::Exited, Some(0), 1);
        tracker.begin_run(start("p2"), 0);
        tracker.end_run("p2", EndReason::Exited, Some(0), 1);
        let spans = tracker.take_finished();
        assert_eq!(
            attr(run_span_on(&spans, "p1"), AttrKey::BranchName),
            Some(&AttrValue::Str("agent/dispatch-x".into()))
        );
        assert_eq!(attr(run_span_on(&spans, "p2"), AttrKey::BranchName), None);
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

    /// Issue #2028: a generation whose reader saw it end by itself keeps its
    /// run open through a pane close that removes it afterwards, open or
    /// replaced, and its own end ends it as exited, with its code. The note
    /// is forgotten with that end.
    #[test]
    fn a_close_after_a_generation_ended_by_itself_leaves_its_own_end_to_end_it() {
        let mut tracker = RunTracker::new();
        let noted = tracker.ended_by_themselves();
        let closed = |agent_id: &str| {
            let mut closed = event("p1", EventType::SessionEnd);
            closed.agent_id = Some(agent_id.into());
            closed
                .metadata
                .insert(DAEMON_PANE_CLOSED_METADATA_KEY.into(), "1".into());
            closed
        };
        tracker.begin_run(generation("p1", "a1"), 0);
        noted.lock().unwrap().insert("a1".into());
        tracker.observe_event(&closed("a1"), 5);
        assert_eq!(tracker.run_id("p1"), Some("run-p1-a1"), "the close waits");
        tracker.end_agent_run("p1", Some("a1"), EndReason::Exited, Some(7), 6);
        let spans = tracker.take_finished();
        assert_eq!(
            end_of(&spans, "run-p1-a1"),
            (AttrValue::Str("exited".into()), 6)
        );
        assert_eq!(
            attr(run_span_of(&spans), AttrKey::ExitCode),
            Some(&AttrValue::Int(7))
        );
        assert!(noted.lock().unwrap().is_empty(), "forgotten with its end");

        tracker.begin_run(generation("p1", "b1"), 10);
        tracker.begin_run(generation("p1", "b2"), 11);
        noted.lock().unwrap().insert("b1".into());
        tracker.observe_event(&closed("b1"), 12);
        assert_eq!(tracker.held(), (2, 0), "the replaced run still waits");
        tracker.end_agent_run("p1", Some("b1"), EndReason::Exited, Some(0), 13);
        assert_eq!(
            end_of(&tracker.take_finished(), "run-p1-b1"),
            (AttrValue::Str("exited".into()), 13)
        );
        assert!(noted.lock().unwrap().is_empty());
        tracker.observe_event(&closed("b2"), 14);
        assert_eq!(
            end_of(&tracker.take_finished(), "run-p1-b2"),
            (AttrValue::Str("closed".into()), 14),
            "a generation nothing noted still ends with its close"
        );
    }

    /// Issue #2028 (review U3): a noted generation whose own end never comes
    /// loses its note when its replaced run is retired without it, once its
    /// grace passes or when too many runs wait, and a still-waiting
    /// generation's note is kept.
    #[test]
    fn a_retired_replaced_run_takes_its_note_with_it() {
        let mut tracker = RunTracker::new();
        let noted = tracker.ended_by_themselves();
        let is_noted = |agent_id: &str| noted.lock().unwrap().contains(agent_id);

        // Grace expiry.
        tracker.begin_run(generation("p1", "a1"), 0);
        tracker.begin_run(generation("p1", "a2"), 10);
        noted.lock().unwrap().insert("a1".into());
        tracker.end_agent_run(
            "p2",
            Some("x"),
            EndReason::Exited,
            None,
            10 + REPLACED_RUN_GRACE_NANOS,
        );
        assert_eq!(
            end_of(&tracker.take_finished(), "run-p1-a1"),
            (AttrValue::Str("unknown".into()), 10)
        );
        assert!(!is_noted("a1"), "the expired run took its note with it");

        // Capacity eviction: the oldest waiting run goes, and its note with
        // it; a later one, still waiting, keeps its own.
        let now = 20 + REPLACED_RUN_GRACE_NANOS;
        for n in 0..=MAX_REPLACED_RUNS {
            tracker.begin_run(generation("p3", &format!("c{n}")), now);
        }
        noted.lock().unwrap().insert("c0".into());
        noted.lock().unwrap().insert("c1".into());
        tracker.begin_run(generation("p3", "last"), now);
        assert_eq!(
            end_of(&tracker.take_finished(), "run-p3-c0"),
            (AttrValue::Str("unknown".into()), now)
        );
        assert!(!is_noted("c0"), "the evicted run took its note with it");
        assert!(is_noted("c1"), "a run still waiting keeps its note");
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
            tracker.observe_human_input(&pane, i, InterventionKind::Text);
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
            Some(AttrValue::Int(10)),
            "an explicit no-PR report leaves the last pull request (#1981)"
        );
        assert_eq!(
            pr_of(&[
                pr_report("p1", Some(10), Some(2)),
                pr_report("p1", None, Some(3)),
                pr_report("p1", Some(12), Some(4))
            ]),
            Some(AttrValue::Int(12)),
            "a later pull request after none replaces it"
        );
        assert_eq!(
            pr_of(&[pr_report("p1", None, Some(3))]),
            None,
            "a run that never held one has none"
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
                    // What describes the operation, without the run's identity.
                    s.attributes
                        .iter()
                        .filter(|a| {
                            !matches!(
                                a.key,
                                AttrKey::RunId
                                    | AttrKey::PaneId
                                    | AttrKey::Agent
                                    | AttrKey::Role
                                    | AttrKey::OrchestrationId
                            )
                        })
                        .cloned()
                        .collect(),
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

    /// A line longer than the hook's cut `tool_detail` is described from the
    /// whole command the hook keeps beside it.
    #[test]
    fn a_long_line_is_described_from_the_whole_command() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        let line = format!("git commit -m '{}' && cargo test", "x".repeat(150));
        let mut begin = shell("p1", EventType::ToolStart, "Bash", Some(&line[..120]), None);
        begin.tool_detail = Some(format!("{}…", &line[..120]));
        begin
            .metadata
            .insert(BASH_COMMAND_METADATA_KEY.into(), line.clone());
        tracker.observe_event(&begin, 10);
        tracker.observe_event(&shell("p1", EventType::ToolEnd, "Bash", None, None), 11);
        tracker.end_run("p1", EndReason::Exited, None, 100);
        let ops = operations(&tracker.take_finished());
        assert_eq!(ops.len(), 1);
        assert!(
            ops[0].0.contains(&Attribute::str(
                AttrKey::OperationCommands,
                "git commit,cargo test"
            )),
            "{:?}",
            ops[0].0
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
                (
                    [
                        op_attrs(Some("cargo"), None),
                        vec![Attribute::str(AttrKey::OperationEnd, "run_end")],
                    ]
                    .concat(),
                    18,
                    100
                ),
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

    /// Starts whose end never arrives are bounded per run, and the deck marks
    /// each operation it ends itself with why: `cap` for one the bound evicts,
    /// `run_end` for one still open when its run ends. An end the agent
    /// reports carries no mark.
    #[test]
    fn open_operations_are_bounded() {
        let ends = |spans: &[SpanRecord]| {
            spans
                .iter()
                .filter(|s| s.name == SPAN_AGENT_OPERATION)
                .map(|s| attr(s, AttrKey::OperationEnd).cloned())
                .collect::<Vec<_>>()
        };
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        for t in 0..(MAX_OPEN_OPERATIONS as u64 + 3) {
            tracker.observe_event(
                &shell("p1", EventType::ToolStart, "Bash", Some("make"), None),
                t,
            );
        }
        assert_eq!(
            ends(&tracker.take_finished()),
            vec![Some(AttrValue::Str("cap".into())); 3]
        );
        tracker.observe_event(&shell("p1", EventType::ToolEnd, "Bash", None, None), 500);
        assert_eq!(ends(&tracker.take_finished()), vec![None]);
        tracker.end_run("p1", EndReason::Exited, None, 1000);
        assert_eq!(
            ends(&tracker.take_finished()),
            vec![Some(AttrValue::Str("run_end".into())); MAX_OPEN_OPERATIONS - 1]
        );
    }

    pub(crate) fn checks_report(
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
            changes[0][..2],
            [
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

    /// `checks_report` for pull request `number` of `repository`, whose
    /// URL is `https://github.com/<repository>/pull/<number>`.
    fn checks_report_in(
        pane: &str,
        repository: &str,
        number: u64,
        revision: u64,
        state: &str,
    ) -> AgentEvent {
        let mut pr = checks_report(pane, number, revision, state, (0, 0, 0));
        let json = pr.metadata[crate::event::PULL_REQUEST_METADATA_KEY]
            .replace("github.com/o/r/", &format!("github.com/{repository}/"));
        pr.metadata
            .insert(crate::event::PULL_REQUEST_METADATA_KEY.into(), json);
        pr
    }

    fn ci_spans(spans: &[SpanRecord]) -> Vec<&SpanRecord> {
        spans.iter().filter(|s| s.name == SPAN_AGENT_CI).collect()
    }

    fn run_span_on<'a>(spans: &'a [SpanRecord], pane: &str) -> &'a SpanRecord {
        spans
            .iter()
            .find(|s| {
                s.name == SPAN_AGENT_RUN
                    && attr(s, AttrKey::PaneId) == Some(&AttrValue::Str(pane.into()))
            })
            .unwrap_or_else(|| panic!("no agent.run for {pane}"))
    }

    fn ci_changes(run: &SpanRecord) -> Vec<Option<AttrValue>> {
        run.events
            .iter()
            .filter(|e| e.name == EVENT_CI_CHANGE)
            .map(|e| {
                e.attributes
                    .iter()
                    .find(|a| a.key == AttrKey::CiTo)
                    .map(|a| a.value.clone())
            })
            .collect()
    }

    /// Issue #1981 M3: two runs whose cards hold one pull request, each told
    /// of every change to its checks in either order, record its CI history
    /// once — two stretches and four changes, all in the run that has held it
    /// longest — while each run counts the two rounds and one failure it saw.
    #[test]
    fn runs_holding_one_pull_request_record_its_ci_once() {
        let mut tracker = RunTracker::new();
        for pane in ["p1", "p2"] {
            tracker.begin_run(start(pane), 0);
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        for (revision, state) in [
            (1, "pending"),
            (2, "failing"),
            (3, "pending"),
            (4, "passing"),
        ] {
            let t = 10 * revision;
            // The second pane is told first of every other change.
            let panes = if revision % 2 == 0 {
                ["p2", "p1"]
            } else {
                ["p1", "p2"]
            };
            for (i, pane) in panes.into_iter().enumerate() {
                tracker.observe_event(
                    &checks_report(pane, 9, revision, state, (0, 0, 0)),
                    t + i as u64,
                );
            }
        }
        tracker.end_run("p1", EndReason::Exited, None, 100);
        tracker.end_run("p2", EndReason::Exited, None, 110);
        let spans = tracker.take_finished();
        let (p1, p2) = (run_span_on(&spans, "p1"), run_span_on(&spans, "p2"));
        let ci = ci_spans(&spans);
        assert_eq!(ci.len(), 2, "one span per stretch: {ci:?}");
        for span in &ci {
            assert_eq!(span.trace_id, p1.trace_id);
            assert_eq!(span.parent_span_id, Some(p1.span_id));
        }
        let s = |v: &str| Some(AttrValue::Str(v.into()));
        assert_eq!(
            ci_changes(p1),
            vec![s("pending"), s("failing"), s("pending"), s("passing")]
        );
        assert!(ci_changes(p2).is_empty(), "no copy on the second run");
        for run in [p1, p2] {
            assert_eq!(attr(run, AttrKey::CiRoundsCount), Some(&AttrValue::Int(2)));
            assert_eq!(
                attr(run, AttrKey::CiFailuresCount),
                Some(&AttrValue::Int(1))
            );
            assert_eq!(attr(run, AttrKey::PrNumber), Some(&AttrValue::Int(9)));
        }
    }

    /// Issue #1981 M3: a stretch outlives the run it opened in while another
    /// run still holds the pull request. Its span stays in the first run's
    /// trace, with that run's identity, and only the runs holding the pull
    /// request when the round ended count it. The last run to let go ends an
    /// open stretch without a result.
    #[test]
    fn a_stretch_outlives_the_run_it_opened_in() {
        let mut tracker = RunTracker::new();
        for pane in ["p1", "p2"] {
            tracker.begin_run(start(pane), 0);
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p2", 9, 1, "pending", (0, 0, 0)), 11);
        tracker.end_run("p1", EndReason::Exited, None, 20);
        assert!(
            ci_spans(&tracker.take_finished()).is_empty(),
            "the stretch stays open while p2 holds the pull request"
        );
        tracker.observe_event(&checks_report("p2", 9, 2, "passing", (0, 0, 0)), 30);
        tracker.observe_event(&checks_report("p2", 9, 3, "pending", (0, 0, 0)), 40);
        tracker.end_run("p2", EndReason::Exited, None, 50);
        let spans = tracker.take_finished();
        let p2 = run_span_on(&spans, "p2");
        let ci = ci_spans(&spans);
        assert_eq!(ci.len(), 2);
        let passing = ci
            .iter()
            .find(|s| attr(s, AttrKey::CiResult).is_some())
            .unwrap();
        assert_eq!((passing.start_unix_nanos, passing.end_unix_nanos), (10, 30));
        assert_ne!(passing.trace_id, p2.trace_id, "in the trace it opened in");
        assert_eq!(
            attr(passing, AttrKey::RunId),
            Some(&AttrValue::Str("run-p1".into()))
        );
        let unended = ci
            .iter()
            .find(|s| attr(s, AttrKey::CiResult).is_none())
            .unwrap();
        assert_eq!((unended.start_unix_nanos, unended.end_unix_nanos), (40, 50));
        assert_eq!(unended.trace_id, p2.trace_id);
        let s = |v: &str| Some(AttrValue::Str(v.into()));
        assert_eq!(ci_changes(p2), vec![s("passing"), s("pending")]);
        assert_eq!(attr(p2, AttrKey::CiRoundsCount), Some(&AttrValue::Int(1)));
        assert_eq!(tracker.ci.len(), 0, "nothing held once every run ended");
    }

    /// Issue #1981 M3: a pull request of the same number in another
    /// repository is another pull request, with a CI history of its own.
    #[test]
    fn the_same_number_in_another_repository_is_another_pull_request() {
        let mut tracker = RunTracker::new();
        for (pane, repository) in [("p1", "o/r"), ("p2", "o/other")] {
            tracker.begin_run(start(pane), 0);
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
            tracker.observe_event(&checks_report_in(pane, repository, 9, 1, "pending"), 10);
            tracker.observe_event(&checks_report_in(pane, repository, 9, 2, "passing"), 20);
        }
        tracker.end_run("p1", EndReason::Exited, None, 30);
        tracker.end_run("p2", EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        assert_eq!(ci_spans(&spans).len(), 2);
        for pane in ["p1", "p2"] {
            assert_eq!(ci_changes(run_span_on(&spans, pane)).len(), 2, "{pane}");
        }
    }

    /// Issue #1981 M6: a report of no pull request still ends the stretch
    /// the run's card held, as the card does, while `deck.pr.number` keeps
    /// the pull request.
    #[test]
    fn a_report_of_no_pull_request_ends_the_stretch_and_keeps_the_number() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&pr_report("p1", None, Some(2)), 20);
        let spans = tracker.take_finished();
        let ci = ci_spans(&spans);
        assert_eq!(ci.len(), 1, "ended when the card let go");
        assert_eq!(ci[0].end_unix_nanos, 20);
        assert_eq!(attr(ci[0], AttrKey::CiResult), None);
        tracker.end_run("p1", EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        assert_eq!(
            attr(run_span_of(&spans), AttrKey::PrNumber),
            Some(&AttrValue::Int(9))
        );
    }

    fn agent_identity_of(attributes: &[Attribute]) -> Vec<Attribute> {
        attributes
            .iter()
            .filter(|a| {
                matches!(
                    a.key,
                    AttrKey::Agent | AttrKey::Role | AttrKey::OrchestrationId
                )
            })
            .cloned()
            .collect()
    }

    /// Issue #1981 M2: every child span and every event of a run names the
    /// run's agent, role and orchestration, as its `agent.run` does.
    #[test]
    fn child_spans_and_events_carry_the_runs_identity() {
        let mut tracker = RunTracker::new();
        let mut s = start("p1");
        s.agent = AgentType::Pi;
        s.orchestration = Some(OrchestrationInfo {
            role: Some("coder".into()),
            name: Some("tdd".into()),
            id: Some("orch-1".into()),
            orchestrator: false,
        });
        tracker.begin_run(s, 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(
            &shell("p1", EventType::ToolStart, "Bash", Some("cargo fmt"), None),
            2,
        );
        tracker.observe_event(
            &shell("p1", EventType::ToolEnd, "Bash", Some("cargo fmt"), None),
            3,
        );
        tracker.observe_human_input("p1", 4, InterventionKind::Text);
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 5);
        tracker.observe_event(&checks_report("p1", 9, 2, "passing", (1, 0, 0)), 6);
        tracker.end_run("p1", EndReason::Exited, None, 10);
        let spans = tracker.take_finished();
        let run = run_span_of(&spans);
        let expected = vec![
            Attribute::str(AttrKey::Agent, "pi"),
            Attribute::str(AttrKey::Role, "coder"),
            Attribute::str(AttrKey::OrchestrationId, "orch-1"),
        ];
        assert_eq!(agent_identity_of(&run.attributes), expected);
        let mut names = Vec::new();
        for child in spans.iter().filter(|s| s.name != SPAN_AGENT_RUN) {
            names.push(child.name);
            assert_eq!(agent_identity_of(&child.attributes), expected, "{child:?}");
            assert_eq!(
                attr(child, AttrKey::RunId),
                Some(&AttrValue::Str("run-p1".into()))
            );
        }
        for name in [SPAN_AGENT_STATUS, SPAN_AGENT_OPERATION, SPAN_AGENT_CI] {
            assert!(names.contains(&name), "no {name} in {names:?}");
        }
        let mut events = Vec::new();
        for event in &run.events {
            events.push(event.name);
            assert_eq!(agent_identity_of(&event.attributes), expected, "{event:?}");
        }
        for name in [
            EVENT_STATUS_CHANGE,
            EVENT_INTERVENTION,
            EVENT_CI_CHANGE,
            EVENT_MODEL_USAGE,
        ] {
            assert!(events.contains(&name), "no {name} in {events:?}");
        }
    }

    /// Issue #1981 M4: a run started as no known agent takes the agent its
    /// hooks name, from then on: the spans and events before that say
    /// `none`, the run and what follows say the hooks' agent, with
    /// `deck.agent.source` `hook`. A run that started as a known agent stays
    /// it whatever its hooks say, with source `spawn`, and a run never named
    /// stays `none` with no source.
    #[test]
    fn a_run_started_as_no_agent_takes_the_agent_its_hooks_name() {
        let mut tracker = RunTracker::new();
        for (pane, agent) in [
            ("learned", AgentType::None),
            ("declared", AgentType::Pi),
            ("silent", AgentType::None),
        ] {
            tracker.begin_run(
                RunStart {
                    agent,
                    ..start(pane)
                },
                0,
            );
        }
        let anonymous = |pane: &str, event_type| AgentEvent {
            agent_type: AgentType::None,
            ..event(pane, event_type)
        };
        tracker.observe_event(&anonymous("learned", EventType::SessionStart), 1);
        tracker.observe_event(&anonymous("learned", EventType::Thinking), 2);
        for pane in ["learned", "declared"] {
            tracker.observe_event(
                &shell(pane, EventType::ToolStart, "Bash", Some("cargo fmt"), None),
                3,
            );
            tracker.observe_event(
                &shell(pane, EventType::ToolEnd, "Bash", Some("cargo fmt"), None),
                4,
            );
        }
        for pane in ["learned", "declared", "silent"] {
            tracker.end_run(pane, EndReason::Exited, None, 10);
        }
        let spans = tracker.take_finished();
        let text = |v: &str| Some(AttrValue::Str(v.into()));
        for (pane, agent, source) in [
            ("learned", "claude", text("hook")),
            ("declared", "pi", text("spawn")),
            ("silent", "none", None),
        ] {
            let run = run_span_on(&spans, pane);
            assert_eq!(attr(run, AttrKey::Agent), text(agent).as_ref(), "{pane}");
            assert_eq!(attr(run, AttrKey::AgentSource), source.as_ref(), "{pane}");
            for child in spans.iter().filter(|s| {
                s.name != SPAN_AGENT_RUN && attr(s, AttrKey::PaneId) == text(pane).as_ref()
            }) {
                let early = child.name == SPAN_AGENT_STATUS && child.end_unix_nanos <= 2;
                let expected = if pane == "learned" && early {
                    "none"
                } else {
                    agent
                };
                assert_eq!(
                    attr(child, AttrKey::Agent),
                    text(expected).as_ref(),
                    "{child:?}"
                );
            }
        }
        let learned: Vec<_> = run_span_on(&spans, "learned")
            .events
            .iter()
            .filter(|e| e.name == EVENT_STATUS_CHANGE)
            .map(|e| {
                e.attributes
                    .iter()
                    .find(|a| a.key == AttrKey::Agent)
                    .map(|a| a.value.clone())
            })
            .collect();
        assert_eq!(
            learned[..2],
            [text("none"), text("none")],
            "events are not rewritten once the agent is known"
        );
        assert_eq!(learned.last(), Some(&text("claude")));
    }

    /// Issue #1981 M4 (review of PR #1991): a run started as no known agent
    /// keeps the agent its hooks named after a `SessionEnd` removes the card
    /// that named it, so the run still ends as that agent, from its hooks.
    #[test]
    fn a_learned_agent_outlives_its_session_end() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(
            RunStart {
                agent: AgentType::None,
                ..start("p1")
            },
            0,
        );
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&event("p1", EventType::ToolStart), 2);
        tracker.observe_event(&event("p1", EventType::SessionEnd), 3);
        tracker.end_run("p1", EndReason::Exited, None, 10);
        let spans = tracker.take_finished();
        let run = run_span_on(&spans, "p1");
        assert_eq!(
            attr(run, AttrKey::Agent),
            Some(&AttrValue::Str("claude".into()))
        );
        assert_eq!(
            attr(run, AttrKey::AgentSource),
            Some(&AttrValue::Str("hook".into()))
        );
    }

    /// Issue #1981 M6: the open runs not yet named list their generations,
    /// and a run named after its dispatched unit carries that name exactly.
    #[test]
    fn a_dispatched_run_carries_its_units_name() {
        let mut tracker = RunTracker::new();
        let mut s = start("p1");
        s.agent_id = Some("a1".into());
        s.orchestration = Some(OrchestrationInfo {
            id: Some("orch-1".into()),
            ..OrchestrationInfo::default()
        });
        tracker.begin_run(s, 0);
        tracker.begin_run(start("p2"), 0);
        let mut listed = tracker.runs_without_dispatch();
        listed.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        assert_eq!(
            listed[0],
            RunGeneration {
                run_id: "run-p1".into(),
                pane_id: "p1".into(),
                agent_id: Some("a1".into()),
                orchestration_id: Some("orch-1".into()),
            }
        );
        let name = "Fix: the unit's Very Long name, with spaces — and more than a slug keeps";
        tracker.set_dispatch_name("run-p1", name);
        assert_eq!(
            tracker
                .runs_without_dispatch()
                .iter()
                .map(|r| r.pane_id.as_str())
                .collect::<Vec<_>>(),
            ["p2"],
            "a named run is not asked about again"
        );
        tracker.end_all(EndReason::Exited, 1);
        let spans = tracker.take_finished();
        assert_eq!(
            attr(run_span_on(&spans, "p1"), AttrKey::DispatchName),
            Some(&AttrValue::Str(name.into()))
        );
        assert_eq!(attr(run_span_on(&spans, "p2"), AttrKey::DispatchName), None);
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

    /// The `agent.ci` spans, `deck.ci.change` events and per-run round counts
    /// of a finished set of spans, for the runs on `panes`.
    fn ci_record(spans: &[SpanRecord], panes: &[&str]) -> (usize, Vec<usize>, Vec<Option<i64>>) {
        let rounds = |pane: &str| match attr(run_span_on(spans, pane), AttrKey::CiRoundsCount) {
            Some(AttrValue::Int(n)) => Some(*n),
            _ => None,
        };
        (
            ci_spans(spans).len(),
            panes
                .iter()
                .map(|pane| ci_changes(run_span_on(spans, pane)).len())
                .collect(),
            panes.iter().map(|pane| rounds(pane)).collect(),
        )
    }

    /// Issue #1981 M3: a run prepared while another run's card watched its
    /// pull request's checks go from pending to passing is told of the same
    /// round — under the daemon's own, interleaved revisions — only once it
    /// opens, after the shared state has recorded the result. Its replayed
    /// reports are older than that result, so the round is recorded once: one
    /// `agent.ci` span and two changes, and each run counts the one round its
    /// card saw.
    #[test]
    fn a_run_committed_after_a_round_ended_does_not_record_it_again() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.prepare_run(start("p2"), 0);
        for pane in ["p1", "p2"] {
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p2", 9, 2, "pending", (0, 0, 0)), 11);
        tracker.observe_event(&checks_report("p1", 9, 3, "passing", (3, 0, 0)), 20);
        tracker.observe_event(&checks_report("p2", 9, 4, "passing", (3, 0, 0)), 21);
        tracker.commit_run("run-p2");
        tracker.end_run("p1", EndReason::Exited, None, 30);
        tracker.end_run("p2", EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        assert_eq!(
            ci_record(&spans, &["p1", "p2"]),
            (1, vec![2, 0], vec![Some(1), Some(1)])
        );
        let span = ci_spans(&spans)[0];
        assert_eq!((span.start_unix_nanos, span.end_unix_nanos), (10, 20));
        assert_eq!(
            attr(span, AttrKey::CiResult),
            Some(&AttrValue::Str("passing".into()))
        );
    }

    /// Issue #1981 M3 (review of PR #1991): a report of the pull request
    /// with no checks changes nothing in the shared state, so it does not
    /// order it either. Two runs see the checks go pending; a newer report
    /// with no checks on the first, then the second's result at an older
    /// revision, still records the round once, with its result.
    #[test]
    fn a_report_without_checks_does_not_hide_an_older_result() {
        let mut tracker = RunTracker::new();
        for pane in ["p1", "p2"] {
            tracker.begin_run(start(pane), 0);
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p2", 9, 2, "pending", (0, 0, 0)), 11);
        tracker.observe_event(&pr_report("p1", Some(9), Some(5)), 15);
        tracker.observe_event(&checks_report("p2", 9, 4, "passing", (3, 0, 0)), 20);
        tracker.end_run("p1", EndReason::Exited, None, 30);
        tracker.end_run("p2", EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        let ci = ci_spans(&spans);
        assert_eq!(ci.len(), 1, "one round: {ci:?}");
        assert_eq!((ci[0].start_unix_nanos, ci[0].end_unix_nanos), (10, 20));
        assert_eq!(
            attr(ci[0], AttrKey::CiResult),
            Some(&AttrValue::Str("passing".into()))
        );
        let s = |v: &str| Some(AttrValue::Str(v.into()));
        assert_eq!(
            ci_changes(run_span_on(&spans, "p1")),
            vec![s("pending"), s("passing")]
        );
    }

    /// Issue #1981 M3 (review of PR #1991): a run replaced by its pane's next
    /// generation lets go of its pull request then. The live run opens the
    /// next stretch and takes its changes, and no stretch ends before it
    /// began when the replaced run's own end comes later.
    #[test]
    fn a_replaced_run_lets_go_of_its_pull_request() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(generation("p1", "a1"), 0);
        tracker.observe_event(&event("p1", EventType::SessionStart), 1);
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.begin_run(generation("p1", "a2"), 20);
        tracker.observe_event(&event("p1", EventType::SessionStart), 21);
        tracker.observe_event(&checks_report("p1", 9, 2, "pending", (0, 0, 0)), 30);
        tracker.observe_event(&checks_report("p1", 9, 3, "passing", (3, 0, 0)), 40);
        tracker.end_agent_run("p1", Some("a1"), EndReason::Exited, None, 50);
        tracker.end_agent_run("p1", Some("a2"), EndReason::Exited, None, 60);
        let spans = tracker.take_finished();
        let run_of = |run_id: &str| {
            spans
                .iter()
                .find(|s| {
                    s.name == SPAN_AGENT_RUN
                        && attr(s, AttrKey::RunId) == Some(&AttrValue::Str(run_id.into()))
                })
                .unwrap_or_else(|| panic!("an agent.run span for {run_id}"))
        };
        let (old, new) = (run_of("run-p1-a1"), run_of("run-p1-a2"));
        let ci = ci_spans(&spans);
        for span in &ci {
            assert!(span.start_unix_nanos <= span.end_unix_nanos, "{span:?}");
        }
        let passing = ci
            .iter()
            .find(|s| attr(s, AttrKey::CiResult).is_some())
            .expect("the round the live run saw");
        assert_eq!(passing.trace_id, new.trace_id, "in the live run's trace");
        assert_eq!((passing.start_unix_nanos, passing.end_unix_nanos), (30, 40));
        let unended = ci
            .iter()
            .find(|s| attr(s, AttrKey::CiResult).is_none())
            .expect("the replaced run's stretch, ended as it let go");
        assert_eq!(unended.trace_id, old.trace_id);
        assert_eq!((unended.start_unix_nanos, unended.end_unix_nanos), (10, 20));
        let s = |v: &str| Some(AttrValue::Str(v.into()));
        assert_eq!(ci_changes(old), vec![s("pending")]);
        assert_eq!(ci_changes(new), vec![s("pending"), s("passing")]);
    }

    /// Issue #1981 M3: a second open run whose card lags is told of a round's
    /// pending only after the first run's card showed its result. That
    /// report is older than the result and records nothing; the lagging
    /// card's own result then repeats the shared state and records nothing
    /// either, while the lagging run still counts the round it saw.
    #[test]
    fn a_stale_report_on_an_open_run_does_not_roll_the_shared_state_back() {
        let mut tracker = RunTracker::new();
        for pane in ["p1", "p2"] {
            tracker.begin_run(start(pane), 0);
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p1", 9, 3, "passing", (3, 0, 0)), 20);
        tracker.observe_event(&checks_report("p2", 9, 2, "pending", (0, 0, 0)), 21);
        tracker.observe_event(&checks_report("p2", 9, 4, "passing", (3, 0, 0)), 22);
        // The next round is still recorded, by either card.
        tracker.observe_event(&checks_report("p2", 9, 5, "pending", (0, 0, 0)), 30);
        tracker.observe_event(&checks_report("p1", 9, 6, "pending", (0, 0, 0)), 31);
        tracker.observe_event(&checks_report("p1", 9, 7, "failing", (2, 1, 0)), 40);
        tracker.end_run("p1", EndReason::Exited, None, 50);
        tracker.end_run("p2", EndReason::Exited, None, 50);
        let spans = tracker.take_finished();
        assert_eq!(
            ci_record(&spans, &["p1", "p2"]),
            (2, vec![4, 0], vec![Some(2), Some(1)])
        );
        let windows: Vec<_> = ci_spans(&spans)
            .iter()
            .map(|s| (s.start_unix_nanos, s.end_unix_nanos))
            .collect();
        assert_eq!(windows, vec![(10, 20), (30, 40)]);
    }

    /// Issue #1981 M3: the order holds after every run holding the pull
    /// request has let go of it. A run committed afterwards, whose reports
    /// were made while the first run held it, records nothing the first run
    /// already did, and a newer round it reports is still recorded.
    #[test]
    fn reports_older_than_a_released_pull_requests_record_nothing() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.prepare_run(start("p2"), 0);
        for pane in ["p1", "p2"] {
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p2", 9, 2, "pending", (0, 0, 0)), 11);
        tracker.observe_event(&checks_report("p1", 9, 3, "passing", (3, 0, 0)), 20);
        tracker.observe_event(&checks_report("p2", 9, 4, "passing", (3, 0, 0)), 21);
        tracker.end_run("p1", EndReason::Exited, None, 25);
        assert_eq!(tracker.ci.len(), 0);
        tracker.commit_run("run-p2");
        tracker.observe_event(&checks_report("p2", 9, 5, "pending", (0, 0, 0)), 30);
        tracker.observe_event(&checks_report("p2", 9, 6, "passing", (4, 0, 0)), 40);
        tracker.end_run("p2", EndReason::Exited, None, 50);
        let spans = tracker.take_finished();
        assert_eq!(
            ci_record(&spans, &["p1", "p2"]),
            (2, vec![2, 2], vec![Some(1), Some(2)])
        );
        let windows: Vec<_> = ci_spans(&spans)
            .iter()
            .map(|s| (s.start_unix_nanos, s.end_unix_nanos))
            .collect();
        assert_eq!(windows, vec![(10, 20), (30, 40)]);
        assert_eq!(
            ci_changes(run_span_on(&spans, "p2")),
            vec![
                Some(AttrValue::Str("pending".into())),
                Some(AttrValue::Str("passing".into()))
            ],
            "no copy of the first round's changes"
        );
    }

    /// Issue #1981 M3: a run prepared while its card watched a round go from
    /// pending to passing opens only after another run's card first showed
    /// the pull request already passing, under a newer revision. Both its
    /// replayed reports are older than that, so the shared state stays
    /// passing, but no recorded round overlaps the one its card saw: that
    /// round is recorded once, as its card saw it, in its own trace.
    #[test]
    fn a_round_another_run_saw_only_the_result_of_is_recorded_once() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.prepare_run(start("p2"), 0);
        for pane in ["p1", "p2"] {
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p2", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p2", 9, 2, "passing", (3, 0, 1)), 20);
        tracker.observe_event(&checks_report("p1", 9, 3, "passing", (3, 0, 1)), 21);
        tracker.commit_run("run-p2");
        tracker.end_run("p1", EndReason::Exited, None, 30);
        tracker.end_run("p2", EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        assert_eq!(
            ci_record(&spans, &["p1", "p2"]),
            (1, vec![1, 0], vec![Some(0), Some(1)])
        );
        let span = ci_spans(&spans)[0];
        assert_eq!((span.start_unix_nanos, span.end_unix_nanos), (10, 20));
        assert_eq!(span.parent_span_id, Some(run_span_on(&spans, "p2").span_id));
        assert_eq!(
            attr(span, AttrKey::CiResult),
            Some(&AttrValue::Str("passing".into()))
        );
        assert_eq!(
            attr(span, AttrKey::CiChecksPassing),
            Some(&AttrValue::Int(3))
        );
    }

    /// Issue #1981 M3: the same, except the first run's card also saw the
    /// round's pending phase, before the prepared run's did. The round the
    /// shared state recorded overlaps the one the prepared run's card saw, so
    /// it is still recorded once.
    #[test]
    fn a_round_another_run_saw_whole_is_not_recorded_again() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.prepare_run(start("p2"), 0);
        for pane in ["p1", "p2"] {
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p1", 9, 1, "pending", (0, 0, 0)), 9);
        tracker.observe_event(&checks_report("p2", 9, 2, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p2", 9, 3, "passing", (3, 0, 0)), 20);
        tracker.observe_event(&checks_report("p1", 9, 4, "passing", (3, 0, 0)), 21);
        tracker.commit_run("run-p2");
        tracker.end_run("p1", EndReason::Exited, None, 30);
        tracker.end_run("p2", EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        assert_eq!(
            ci_record(&spans, &["p1", "p2"]),
            (1, vec![2, 0], vec![Some(1), Some(1)])
        );
        let span = ci_spans(&spans)[0];
        assert_eq!((span.start_unix_nanos, span.end_unix_nanos), (9, 21));
        assert_eq!(span.parent_span_id, Some(run_span_on(&spans, "p1").span_id));
    }

    /// Issue #1981 M3: a pull request keeps the revisions of its newest
    /// [`MAX_RECORDED_ROUNDS`] rounds, and counts every older one as
    /// overlapping a round that starts before them.
    #[test]
    fn recorded_rounds_are_bounded_and_cover_what_they_forgot() {
        let mut rounds = RecordedRounds::default();
        for n in 0..MAX_RECORDED_ROUNDS as u64 + 1 {
            rounds.record(10 * n + 1, 10 * n + 2);
        }
        assert_eq!(rounds.rounds.len(), MAX_RECORDED_ROUNDS);
        assert!(rounds.covers(1, 2), "the forgotten round");
        assert!(rounds.covers(12, 15));
        assert!(rounds.covers(5, 11));
        assert!(!rounds.covers(13, 20));
        assert!(!rounds.covers(10 * MAX_RECORDED_ROUNDS as u64 + 3, u64::MAX));
    }

    /// Issue #1981 M3: the first run's card saw the round open at a report
    /// with no revision, and its result under a newer one than either of the
    /// prepared run's replayed reports. The round recorded from that unknown
    /// opening overlaps the one the prepared run's card saw, so it is still
    /// recorded once.
    #[test]
    fn a_round_opened_without_a_revision_is_not_recorded_again() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.prepare_run(start("p2"), 0);
        for pane in ["p1", "p2"] {
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        tracker.observe_event(&checks_report("p1", 9, 0, "pending", (0, 0, 0)), 9);
        tracker.observe_event(&checks_report("p2", 9, 1, "pending", (0, 0, 0)), 10);
        tracker.observe_event(&checks_report("p2", 9, 2, "passing", (3, 0, 0)), 20);
        tracker.observe_event(&checks_report("p1", 9, 3, "passing", (3, 0, 0)), 21);
        tracker.commit_run("run-p2");
        tracker.end_run("p1", EndReason::Exited, None, 30);
        tracker.end_run("p2", EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        assert_eq!(
            ci_record(&spans, &["p1", "p2"]),
            (1, vec![2, 0], vec![Some(1), Some(1)])
        );
        let span = ci_spans(&spans)[0];
        assert_eq!((span.start_unix_nanos, span.end_unix_nanos), (9, 21));
        assert_eq!(span.parent_span_id, Some(run_span_on(&spans, "p1").span_id));
    }

    /// Issue #1981 M3: rounds recorded out of revision order — a newer one
    /// first, then a full cache of older replayed ones — forget the oldest
    /// by revision, so a round between them is not counted as covered.
    #[test]
    fn recorded_rounds_forget_the_oldest_by_revision() {
        let mut rounds = RecordedRounds::default();
        rounds.record(100, 101);
        for n in 0..MAX_RECORDED_ROUNDS as u64 {
            rounds.record(2 * n + 1, 2 * n + 2);
        }
        assert_eq!(rounds.rounds.len(), MAX_RECORDED_ROUNDS);
        assert_eq!(rounds.floor, 2);
        assert!(rounds.covers(1, 2), "the forgotten round");
        assert!(rounds.covers(100, 101));
        assert!(!rounds.covers(50, 51));
        // A round older than the floor is not kept in place of a newer one.
        rounds.record(1, 2);
        assert!(rounds.covers(31, 32));
        assert_eq!(rounds.floor, 2);
    }

    /// Issue #1981 M3: the same, end to end. One run's card records a round
    /// while two runs prepared before it buffer older ones: one sixteen
    /// rounds, the other a round between those and the first run's. Each
    /// prepared run, once it opens, records every round its card saw, the
    /// last one included.
    #[test]
    fn older_replayed_rounds_do_not_hide_a_newer_one() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 0);
        tracker.prepare_run(start("p2"), 0);
        tracker.prepare_run(start("p3"), 0);
        for pane in ["p1", "p2", "p3"] {
            tracker.observe_event(&event(pane, EventType::SessionStart), 1);
        }
        for n in 0..MAX_RECORDED_ROUNDS as u64 {
            let (pending, passing) = (2 * n + 1, 2 * n + 2);
            tracker.observe_event(
                &checks_report("p2", 9, pending, "pending", (0, 0, 0)),
                pending,
            );
            tracker.observe_event(
                &checks_report("p2", 9, passing, "passing", (1, 0, 0)),
                passing,
            );
        }
        tracker.observe_event(&checks_report("p3", 9, 50, "pending", (0, 0, 0)), 50);
        tracker.observe_event(&checks_report("p3", 9, 51, "passing", (1, 0, 0)), 51);
        tracker.observe_event(&checks_report("p1", 9, 100, "pending", (0, 0, 0)), 100);
        tracker.observe_event(&checks_report("p1", 9, 101, "passing", (1, 0, 0)), 101);
        tracker.commit_run("run-p2");
        tracker.commit_run("run-p3");
        for pane in ["p1", "p2", "p3"] {
            tracker.end_run(pane, EndReason::Exited, None, 200);
        }
        let spans = tracker.take_finished();
        assert_eq!(ci_spans(&spans).len(), MAX_RECORDED_ROUNDS + 2);
        let p3 = run_span_on(&spans, "p3").span_id;
        let recovered: Vec<_> = ci_spans(&spans)
            .into_iter()
            .filter(|span| span.parent_span_id == Some(p3))
            .map(|span| (span.start_unix_nanos, span.end_unix_nanos))
            .collect();
        assert_eq!(recovered, vec![(50, 51)]);
    }

    /// Issue #1981 M6: a run prepared in the worktree of a unit being
    /// dispatched is named after the unit from its start — so a run whose
    /// pane closes before it is even committed keeps the name — while a run
    /// prepared elsewhere, or in the worktree once the spawn is over, is not.
    #[test]
    fn a_run_prepared_in_a_dispatched_worktree_is_named_from_its_start() {
        let mut tracker = RunTracker::new();
        let name = "Fix: the unit, exactly";
        let token = tracker.begin_dispatch(name, std::path::Path::new("/w/unit"));
        let in_unit = std::path::Path::new("/w/unit/sub");
        tracker.prepare_run_in(start("p1"), 0, Some(in_unit));
        tracker.prepare_run_in(start("p2"), 0, Some(std::path::Path::new("/w/unit-2")));
        let mut closed = event("p1", EventType::SessionEnd);
        closed
            .metadata
            .insert(DAEMON_PANE_CLOSED_METADATA_KEY.into(), "1".into());
        tracker.observe_event(&closed, 5);
        tracker.end_dispatch(token);
        tracker.prepare_run_in(start("p3"), 6, Some(in_unit));
        for run in ["run-p1", "run-p2", "run-p3"] {
            tracker.commit_run(run);
        }
        for pane in ["p2", "p3"] {
            tracker.end_run(pane, EndReason::Exited, None, 10);
        }
        let spans = tracker.take_finished();
        let named = |pane: &str| attr(run_span_on(&spans, pane), AttrKey::DispatchName).cloned();
        assert_eq!(named("p1"), Some(AttrValue::Str(name.into())));
        assert_eq!(
            attr(run_span_on(&spans, "p1"), AttrKey::EndReason),
            Some(&AttrValue::Str("closed".into()))
        );
        assert_eq!(named("p2"), None, "a sibling directory is not the worktree");
        assert_eq!(named("p3"), None, "prepared after the spawn was over");
    }

    /// Issue #1981 M6: once the spawn says which generations it started, a
    /// run of one of them that started outside the worktree is named too, as
    /// is every role its orchestration instance starts later, and nothing
    /// else.
    #[test]
    fn a_dispatched_units_generations_and_instance_are_named() {
        let mut tracker = RunTracker::new();
        let role = |pane: &str, agent: &str, instance: &str| RunStart {
            agent_id: Some(agent.into()),
            orchestration: Some(OrchestrationInfo {
                id: Some(instance.into()),
                ..OrchestrationInfo::default()
            }),
            ..start(pane)
        };
        tracker.begin_run(role("p1", "a1", "orch-1"), 0);
        tracker.prepare_run(role("p2", "a2", "orch-1"), 0);
        tracker.begin_run(role("p3", "a3", "orch-other"), 0);
        tracker.name_dispatched("Team", &[("p1".into(), "a1".into())], Some("orch-1"));
        tracker.commit_run("run-p2");
        // A role the instance starts later.
        tracker.prepare_run(role("p4", "a4", "orch-1"), 5);
        tracker.commit_run("run-p4");
        for pane in ["p1", "p2", "p3", "p4"] {
            tracker.end_run(pane, EndReason::Exited, None, 10);
        }
        let spans = tracker.take_finished();
        let named: Vec<_> = spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN)
            .map(|s| {
                (
                    attr(s, AttrKey::RunId).cloned(),
                    attr(s, AttrKey::DispatchName).cloned(),
                )
            })
            .collect();
        let team = Some(AttrValue::Str("Team".into()));
        for run in ["run-p1", "run-p2", "run-p4"] {
            assert!(
                named.contains(&(Some(AttrValue::Str(run.into())), team.clone())),
                "{run}: {named:?}"
            );
        }
        assert!(named.contains(&(Some(AttrValue::Str("run-p3".into())), None)));
    }
}
