//! Issue #1981 M7: one `agent.handoff` span per task an orchestrator
//! delegates, from the task's post to the worker's report arriving, and one
//! `agent.handoff.read` span per report the orchestrator then reads, from the
//! report arriving to the orchestrator acknowledging it.
//!
//! **What opens one.** The delegate path tells telemetry it posted a task
//! ([`super::observe_delegate_posted`]), which is the one place that knows a
//! queued inbox message is a delegated task, its worker and the
//! orchestrator together. Everything after that is read from the inbox's own
//! state changes on the daemon's broadcast ([`crate::event::InboxNotice`]):
//!
//! - the worker taking the task (`fetched`, `claimed` or `acknowledged` on
//!   the task's message) is the `deck.handoff.taken` event, once;
//! - a report naming the task reaching the orchestrator's inbox (`queued`,
//!   with the task's id in its provenance) ends the span `reported`, with a
//!   `deck.handoff.reported` event at its end, and takes the worker's run
//!   from the report's producer;
//! - the orchestrator acknowledging that report is the end of the
//!   `agent.handoff.read` span, a child of the hand-off's span that starts
//!   where the hand-off ended;
//! - the task found undeliverable ends it `undeliverable`, with the inbox's
//!   reason, or `superseded` when that reason is a newer task replacing it.
//!
//! **Why the read is a span of its own.** The hand-off ends when the report
//! arrives, so it measures the worker's side whatever the orchestrator does
//! next, and a span event cannot come after its span's end. The read is a
//! child of the hand-off rather than of the orchestrator's run so it joins to
//! the task structurally, by parent, in the same trace, as well as by
//! `deck.handoff.id`; its duration is the time the report waited to be read.
//! A report its orchestrator's run ends without reading has no read span, and
//! neither does one still unread after [`MAX_HANDOFF_AGE_NANOS`] or pushed out
//! by [`MAX_UNREAD_REPORTS`].
//!
//! A hand-off still open when its orchestrator's run ends ends with it:
//! `daemon_shutdown` when the daemon is shutting down, `unknown` otherwise. One still open after
//! [`MAX_HANDOFF_AGE_NANOS`] ends `unknown` at that age, whatever ends it
//! later — the exporter's usage poll checks every couple of seconds, and a
//! run's end or the daemon's shutdown checks before choosing an outcome
//! ([`Handoffs::expire`]). The oldest past [`MAX_OPEN_HANDOFFS`] ends
//! `unknown` where it stands.
//!
//! **The post keeps its place on the broadcast.** The exporter applies a post
//! between the broadcast messages it was made between, the way it places
//! human input, so a pane closed after the post cannot end the orchestrator's
//! run before the hand-off finds it.
//!
//! **A notice can overtake the post.** The inbox broadcasts the task's
//! `queued` state before the delegate path returns from posting it, so a fast
//! worker's fetch can reach telemetry first. A notice for a message no
//! hand-off knows is held, at most [`MAX_EARLY_NOTICES`] of them and for at
//! most [`EARLY_NOTICE_AGE_NANOS`], and applied in order when the post for
//! its id arrives.
//!
//! Only ids, the worker's role and enum names reach a span: never the task,
//! its subject, its label or the report.

use std::collections::{HashMap, VecDeque};

use crate::event::InboxNotice;
use crate::inbox::{StateName, UndeliverableReason};
use crate::report_recovery::ReportProvenance;

use super::span::{
    Attribute, SpanEventRecord, SpanId, SpanRecord, TraceId, new_span_id, new_trace_id,
};
use super::{
    AttrKey, EVENT_HANDOFF_REPORTED, EVENT_HANDOFF_TAKEN, SPAN_AGENT_HANDOFF,
    SPAN_AGENT_HANDOFF_READ,
};

/// Hand-offs open at once. Past it the oldest ends as `unknown`.
pub const MAX_OPEN_HANDOFFS: usize = 256;

/// How long a hand-off may stay open before it ends as `unknown`, and how
/// long a report may wait to be read before its read is forgotten.
pub const MAX_HANDOFF_AGE_NANOS: u64 = 24 * 60 * 60 * 1_000_000_000;

/// Reports that arrived and wait to be read. Past it the oldest is forgotten,
/// and its read is not recorded.
pub const MAX_UNREAD_REPORTS: usize = 256;

/// Notices for messages no hand-off knows yet, held for a post they may have
/// overtaken. Past it the oldest is dropped.
pub const MAX_EARLY_NOTICES: usize = 64;

/// How long such a notice is held.
pub const EARLY_NOTICE_AGE_NANOS: u64 = 60 * 1_000_000_000;

/// How a hand-off ended (`deck.handoff.outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The worker's report arrived.
    Reported,
    /// The task never reached its worker, for the inbox's reason.
    Undeliverable(Option<UndeliverableReason>),
    /// A newer task replaced it before its worker took it.
    Superseded,
    /// Its orchestrator's run ended with no report, or the hand-off was
    /// evicted.
    Unknown,
    /// The daemon shut down with no report.
    DaemonShutdown,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Reported => "reported",
            Outcome::Undeliverable(_) => "undeliverable",
            Outcome::Superseded => "superseded",
            Outcome::Unknown => "unknown",
            Outcome::DaemonShutdown => "daemon_shutdown",
        }
    }
}

/// The inbox's name for `reason`, as its own JSON spells it.
pub fn reason_name(reason: UndeliverableReason) -> &'static str {
    match reason {
        UndeliverableReason::AgentExited => "agent_exited",
        UndeliverableReason::MaxDeliver => "max_deliver",
        UndeliverableReason::Superseded => "superseded",
        UndeliverableReason::InboxShut => "inbox_shut",
        UndeliverableReason::Cancelled => "cancelled",
    }
}

/// [`super::DelegatePosted`], owned, with when it was posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub message_id: String,
    pub orchestrator_pane: String,
    pub orchestrator_agent_id: Option<String>,
    pub worker_pane: String,
    pub worker_agent_id: String,
    pub role: String,
    pub at: u64,
}

impl Posted {
    pub fn of(posted: &super::DelegatePosted<'_>, at: u64) -> Self {
        Posted {
            message_id: posted.message_id.to_string(),
            orchestrator_pane: posted.orchestrator_pane.to_string(),
            orchestrator_agent_id: posted.orchestrator_agent_id.map(str::to_string),
            worker_pane: posted.worker_pane.to_string(),
            worker_agent_id: posted.worker_agent_id.to_string(),
            role: posted.role.to_string(),
            at,
        }
    }
}

/// What the tracker knows of a run a hand-off names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRef {
    pub run_id: String,
    /// The run's trace and span, once it is open.
    pub span: Option<(TraceId, SpanId)>,
    pub orchestration_id: Option<String>,
}

/// One inbox state change, as much of it as a hand-off reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The message was handed to its recipient.
    Taken(String),
    Acknowledged(String),
    Undeliverable(String, Option<UndeliverableReason>),
    /// Report `report` naming task `task` reached an inbox, from the agent
    /// `producer` (pane, agent id).
    Report {
        report: String,
        task: String,
        producer: (String, String),
    },
    /// A report naming no task reached the inbox of `pane`'s agent `agent`.
    Uncorrelated {
        pane: String,
        agent: Option<String>,
    },
}

impl Change {
    /// What `notice` changed for a hand-off, if anything. A notice that only
    /// repeats a state (a wake or attention update), hands a message back,
    /// or reports where an offer went says nothing new.
    pub fn of(notice: &InboxNotice) -> Option<Change> {
        if notice.wake_update || notice.attention_update || notice.returned {
            return None;
        }
        let id = notice.message_id.clone();
        match notice.state {
            StateName::Fetched | StateName::Claimed => Some(Change::Taken(id)),
            StateName::Acknowledged => Some(Change::Acknowledged(id)),
            StateName::Undeliverable => Some(Change::Undeliverable(id, notice.reason)),
            StateName::Queued if notice.requeued => None,
            StateName::Queued => match notice.provenance.as_ref()? {
                ReportProvenance::Task {
                    task_id, producer, ..
                } => Some(Change::Report {
                    report: id,
                    task: task_id.clone(),
                    producer: (producer.pane_id.clone(), producer.agent_id.clone()),
                }),
                ReportProvenance::Uncorrelated { .. } => Some(Change::Uncorrelated {
                    pane: notice.pane_id.clone(),
                    agent: notice.agent_id.clone(),
                }),
                _ => None,
            },
        }
    }

    /// The message this change is about.
    pub fn message_id(&self) -> Option<&str> {
        match self {
            Change::Taken(id) | Change::Acknowledged(id) | Change::Undeliverable(id, _) => Some(id),
            Change::Report { report, .. } => Some(report),
            Change::Uncorrelated { .. } => None,
        }
    }
}

#[derive(Debug)]
struct OpenHandoff {
    trace_id: TraceId,
    span_id: SpanId,
    parent: Option<SpanId>,
    start: u64,
    orchestrator_pane: String,
    orchestrator_run_id: Option<String>,
    orchestration_id: Option<String>,
    role: String,
    worker_run_id: Option<String>,
    taken: Option<u64>,
    /// When the report arrived: the hand-off ends then.
    reported: Option<u64>,
}

/// A report that arrived for a hand-off, waiting for its orchestrator to
/// read it: what its `agent.handoff.read` span needs.
#[derive(Debug)]
struct UnreadReport {
    task: String,
    trace_id: TraceId,
    /// The hand-off's span, the read's parent.
    handoff_span: SpanId,
    arrived: u64,
    orchestrator_pane: String,
    orchestrator_run_id: Option<String>,
    orchestration_id: Option<String>,
    role: String,
}

/// Every open hand-off, by task id. See the module docs.
#[derive(Debug, Default)]
pub struct Handoffs {
    open: HashMap<String, OpenHandoff>,
    /// Open task ids, oldest post first.
    order: VecDeque<String>,
    /// Reports that arrived and are not read yet, by report message id.
    unread: HashMap<String, UnreadReport>,
    /// Their ids, oldest arrival first.
    unread_order: VecDeque<String>,
    /// Changes for messages no hand-off knew yet, oldest first, with when
    /// each was seen.
    early: VecDeque<(Change, u64)>,
}

impl Handoffs {
    /// Open the hand-off of `posted`, in the trace of its orchestrator's run
    /// when `orchestrator` names an open one, with the worker's run
    /// `worker_run_id`, and apply the changes held for its id.
    pub fn posted(
        &mut self,
        posted: Posted,
        orchestrator: Option<RunRef>,
        worker_run_id: Option<String>,
        run_of: &dyn Fn(&str, &str) -> Option<RunRef>,
        finished: &mut Vec<SpanRecord>,
    ) {
        let now = posted.at;
        self.expire(now, finished);
        if self.open.contains_key(&posted.message_id) {
            return;
        }
        let (trace_id, parent) = match orchestrator.as_ref().and_then(|run| run.span) {
            Some((trace, span)) => (trace, Some(span)),
            None => (new_trace_id(), None),
        };
        let orchestration_id = orchestrator
            .as_ref()
            .and_then(|run| run.orchestration_id.clone());
        let id = posted.message_id.clone();
        self.open.insert(
            id.clone(),
            OpenHandoff {
                trace_id,
                span_id: new_span_id(),
                parent,
                start: posted.at,
                orchestrator_pane: posted.orchestrator_pane,
                orchestrator_run_id: orchestrator.map(|run| run.run_id),
                orchestration_id,
                role: posted.role,
                worker_run_id,
                taken: None,
                reported: None,
            },
        );
        self.order.push_back(id.clone());
        while self.order.len() > MAX_OPEN_HANDOFFS {
            let Some(oldest) = self.order.front().cloned() else {
                break;
            };
            self.end(&oldest, Outcome::Unknown, now, finished);
        }
        // Replay, in the order they were seen, what concerns this task: its
        // own changes, its report, and then that report's.
        let held = std::mem::take(&mut self.early);
        for (change, at) in held {
            if self.concerns(&id, &change) {
                self.apply_known(change, at.max(now), run_of, finished);
            } else {
                self.early.push_back((change, at));
            }
        }
    }

    fn concerns(&self, task: &str, change: &Change) -> bool {
        match change {
            Change::Report { task: t, .. } => t == task,
            other => other.message_id().is_some_and(|id| {
                id == task || self.unread.get(id).is_some_and(|r| r.task == task)
            }),
        }
    }

    /// Apply `change`, seen at `now`. A change about a message no hand-off
    /// knows is held for a post it may have overtaken. `run_of` finds the run
    /// open on a pane for an agent id.
    pub fn apply(
        &mut self,
        change: Change,
        now: u64,
        run_of: &dyn Fn(&str, &str) -> Option<RunRef>,
        finished: &mut Vec<SpanRecord>,
    ) {
        self.expire(now, finished);
        if !self.apply_known(change.clone(), now, run_of, finished) {
            self.early.push_back((change, now));
            while self.early.len() > MAX_EARLY_NOTICES {
                self.early.pop_front();
            }
        }
    }

    /// Apply `change` to the hand-off it concerns. Whether one did.
    fn apply_known(
        &mut self,
        change: Change,
        now: u64,
        run_of: &dyn Fn(&str, &str) -> Option<RunRef>,
        finished: &mut Vec<SpanRecord>,
    ) -> bool {
        match change {
            Change::Taken(id) => {
                // The orchestrator taking a report is not reading it.
                if self.unread.contains_key(&id) {
                    return true;
                }
                let Some(open) = self.open.get_mut(&id) else {
                    return false;
                };
                open.taken.get_or_insert(now.max(open.start));
            }
            Change::Acknowledged(id) => {
                if let Some(unread) = self.unread.remove(&id) {
                    self.unread_order.retain(|r| *r != id);
                    finished.push(read_span(unread, now));
                } else if let Some(open) = self.open.get_mut(&id) {
                    open.taken.get_or_insert(now.max(open.start));
                } else {
                    return false;
                }
            }
            Change::Undeliverable(id, reason) => {
                // A task whose report arrived has ended, delivered, whatever
                // becomes of its message now.
                if !self.open.contains_key(&id) {
                    return false;
                }
                let outcome = match reason {
                    Some(UndeliverableReason::Superseded) => Outcome::Superseded,
                    other => Outcome::Undeliverable(other),
                };
                self.end(&id, outcome, now, finished);
            }
            Change::Report {
                report,
                task,
                producer: (pane, agent),
            } => {
                let Some(open) = self.open.get_mut(&task) else {
                    return false;
                };
                let arrived = now.max(open.start).max(open.taken.unwrap_or(0));
                open.reported = Some(arrived);
                if let Some(run) = run_of(&pane, &agent) {
                    open.worker_run_id = Some(run.run_id);
                }
                let unread = UnreadReport {
                    task: task.clone(),
                    trace_id: open.trace_id,
                    handoff_span: open.span_id,
                    arrived,
                    orchestrator_pane: open.orchestrator_pane.clone(),
                    orchestrator_run_id: open.orchestrator_run_id.clone(),
                    orchestration_id: open.orchestration_id.clone(),
                    role: open.role.clone(),
                };
                self.end(&task, Outcome::Reported, arrived, finished);
                self.unread.insert(report.clone(), unread);
                self.unread_order.push_back(report);
                while self.unread_order.len() > MAX_UNREAD_REPORTS {
                    if let Some(oldest) = self.unread_order.pop_front() {
                        self.unread.remove(&oldest);
                    }
                }
            }
            // Counted on the run by the tracker.
            Change::Uncorrelated { .. } => {}
        }
        true
    }

    /// The orchestrator run `run_id` ended at `now`, `shutting_down` when the
    /// daemon is: end every hand-off in it, after the ones past their age
    /// bound end `unknown` at it, and forget the reports it left unread.
    pub fn run_ended(
        &mut self,
        run_id: &str,
        shutting_down: bool,
        now: u64,
        finished: &mut Vec<SpanRecord>,
    ) {
        self.expire(now, finished);
        let ending: Vec<String> = self
            .order
            .iter()
            .filter(|id| {
                self.open
                    .get(*id)
                    .is_some_and(|h| h.orchestrator_run_id.as_deref() == Some(run_id))
            })
            .cloned()
            .collect();
        for id in ending {
            self.end_unreported(&id, shutting_down, now, finished);
        }
        self.unread
            .retain(|_, r| r.orchestrator_run_id.as_deref() != Some(run_id));
        let unread = &self.unread;
        self.unread_order.retain(|id| unread.contains_key(id));
    }

    /// Daemon shutdown at `now`: end every hand-off still open, the ones
    /// past their age bound `unknown` at it.
    pub fn end_all(&mut self, now: u64, finished: &mut Vec<SpanRecord>) {
        self.expire(now, finished);
        let open: Vec<String> = self.order.iter().cloned().collect();
        for id in open {
            self.end_unreported(&id, true, now, finished);
        }
        self.early.clear();
        self.unread.clear();
        self.unread_order.clear();
    }

    /// End `id`, whose report has not arrived.
    fn end_unreported(
        &mut self,
        id: &str,
        shutting_down: bool,
        now: u64,
        finished: &mut Vec<SpanRecord>,
    ) {
        let outcome = if shutting_down {
            Outcome::DaemonShutdown
        } else {
            Outcome::Unknown
        };
        self.end(id, outcome, now, finished);
    }

    /// End every hand-off open [`MAX_HANDOFF_AGE_NANOS`] or longer by `now`
    /// as `unknown`, at the moment it reached that age, and forget every held
    /// change past [`EARLY_NOTICE_AGE_NANOS`]. Called before anything else
    /// touches the hand-offs, and periodically ([`super::RunTracker::expire_handoffs`]),
    /// so one past its bound never ends any other way.
    pub fn expire(&mut self, now: u64, finished: &mut Vec<SpanRecord>) {
        // Every one, not only from the front: posts are stamped where they
        // are made, so the order of their stamps can differ from the order
        // they were applied in.
        let expired: Vec<(String, u64)> = self
            .order
            .iter()
            .filter_map(|id| {
                let bound = self
                    .open
                    .get(id)?
                    .start
                    .saturating_add(MAX_HANDOFF_AGE_NANOS);
                (now >= bound).then(|| (id.clone(), bound))
            })
            .collect();
        for (id, bound) in expired {
            self.end(&id, Outcome::Unknown, bound, finished);
        }
        self.early
            .retain(|(_, at)| now.saturating_sub(*at) < EARLY_NOTICE_AGE_NANOS);
        self.unread
            .retain(|_, r| now.saturating_sub(r.arrived) < MAX_HANDOFF_AGE_NANOS);
        let unread = &self.unread;
        self.unread_order.retain(|id| unread.contains_key(id));
    }

    fn end(&mut self, id: &str, outcome: Outcome, now: u64, finished: &mut Vec<SpanRecord>) {
        self.order.retain(|open| open != id);
        let Some(open) = self.open.remove(id) else {
            return;
        };
        finished.push(span(id, open, outcome, now));
    }

    /// The task named by report `report`, when the report is held for a
    /// post not seen yet.
    pub fn early_report_task(&self, report: &str) -> Option<&str> {
        self.early.iter().find_map(|(change, _)| match change {
            Change::Report {
                report: r, task, ..
            } if r == report => Some(task.as_str()),
            _ => None,
        })
    }

    /// Hand-offs open now.
    #[cfg(test)]
    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// Changes held for a post not seen yet.
    #[cfg(test)]
    pub fn early_count(&self) -> usize {
        self.early.len()
    }

    /// Reports that arrived and are not read yet.
    #[cfg(test)]
    pub fn unread_count(&self) -> usize {
        self.unread.len()
    }
}

/// The `agent.handoff.read` span of `unread`, which its orchestrator read at
/// `now`.
fn read_span(unread: UnreadReport, now: u64) -> SpanRecord {
    let mut attributes = vec![Attribute::str(AttrKey::HandoffId, &unread.task)];
    attributes.extend(Attribute::opt_str(
        AttrKey::RunId,
        unread.orchestrator_run_id.as_deref(),
    ));
    attributes.push(Attribute::str(AttrKey::PaneId, &unread.orchestrator_pane));
    attributes.extend(Attribute::opt_str(AttrKey::Role, Some(&unread.role)));
    attributes.extend(Attribute::opt_str(
        AttrKey::OrchestrationId,
        unread.orchestration_id.as_deref(),
    ));
    SpanRecord {
        trace_id: unread.trace_id,
        span_id: new_span_id(),
        parent_span_id: Some(unread.handoff_span),
        name: SPAN_AGENT_HANDOFF_READ,
        start_unix_nanos: unread.arrived,
        end_unix_nanos: now.max(unread.arrived),
        attributes,
        events: Vec::new(),
    }
}

fn span(id: &str, open: OpenHandoff, outcome: Outcome, now: u64) -> SpanRecord {
    let mut attributes = vec![Attribute::str(AttrKey::HandoffId, id)];
    attributes.extend(Attribute::opt_str(
        AttrKey::RunId,
        open.orchestrator_run_id.as_deref(),
    ));
    attributes.push(Attribute::str(AttrKey::PaneId, &open.orchestrator_pane));
    attributes.extend(Attribute::opt_str(
        AttrKey::HandoffWorkerRunId,
        open.worker_run_id.as_deref(),
    ));
    attributes.extend(Attribute::opt_str(AttrKey::Role, Some(&open.role)));
    attributes.extend(Attribute::opt_str(
        AttrKey::OrchestrationId,
        open.orchestration_id.as_deref(),
    ));
    attributes.push(Attribute::str(AttrKey::HandoffOutcome, outcome.as_str()));
    if let Outcome::Undeliverable(Some(reason)) = outcome {
        attributes.push(Attribute::str(
            AttrKey::HandoffUndeliverableReason,
            reason_name(reason),
        ));
    }
    let mut events = Vec::with_capacity(2);
    if let Some(at) = open.taken {
        events.push(SpanEventRecord {
            name: EVENT_HANDOFF_TAKEN,
            time_unix_nanos: at,
            attributes: Vec::new(),
        });
    }
    if let Some(at) = open.reported {
        events.push(SpanEventRecord {
            name: EVENT_HANDOFF_REPORTED,
            time_unix_nanos: at,
            attributes: Vec::new(),
        });
    }
    let latest = events
        .iter()
        .map(|e| e.time_unix_nanos)
        .fold(open.start, u64::max);
    SpanRecord {
        trace_id: open.trace_id,
        span_id: open.span_id,
        parent_span_id: open.parent,
        name: SPAN_AGENT_HANDOFF,
        start_unix_nanos: open.start,
        end_unix_nanos: now.max(latest),
        attributes,
        events,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report_recovery::Producer;
    use crate::telemetry::span::AttrValue;
    use crate::telemetry::tracker::tests::start;
    use crate::telemetry::{EndReason, OrchestrationInfo, RunTracker, SPAN_AGENT_RUN};

    const ORCH: &str = "orch-pane";
    const WORKER: &str = "worker-pane";
    const TASK: &str = "m-task";
    const REPORT: &str = "m-report";

    fn attr(span: &SpanRecord, key: AttrKey) -> Option<&AttrValue> {
        span.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| &a.value)
    }

    fn text(span: &SpanRecord, key: AttrKey) -> Option<&str> {
        match attr(span, key)? {
            AttrValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A tracker with an orchestrator run and a worker run (generation
    /// `worker_agent`) open.
    fn team(worker_agent: &str) -> RunTracker {
        let mut tracker = RunTracker::new();
        let mut orch = start(ORCH);
        orch.agent_id = Some("orch-agent".into());
        orch.orchestration = Some(OrchestrationInfo {
            role: Some("orchestrator".into()),
            name: Some("tdd".into()),
            id: Some("orch-1".into()),
            orchestrator: true,
        });
        tracker.begin_run(orch, 0);
        tracker.begin_run(worker(worker_agent), 0);
        tracker
    }

    fn worker(agent: &str) -> RunStart {
        let mut worker = start(WORKER);
        worker.run_id = format!("run-{agent}");
        worker.agent_id = Some(agent.into());
        worker
    }

    use crate::telemetry::RunStart;

    fn post(tracker: &mut RunTracker, at: u64) {
        tracker.delegate_posted(Posted {
            message_id: TASK.into(),
            orchestrator_pane: ORCH.into(),
            orchestrator_agent_id: Some("orch-agent".into()),
            worker_pane: WORKER.into(),
            worker_agent_id: "worker-1".into(),
            role: "coder".into(),
            at,
        });
    }

    fn notice(pane: &str, id: &str, state: StateName) -> InboxNotice {
        InboxNotice {
            pane_id: pane.into(),
            agent_id: None,
            message_id: id.into(),
            subject: "PRIVATE subject".into(),
            from_label: "PRIVATE label".into(),
            state,
            reason: None,
            returned: false,
            wake_stalled: false,
            wake_update: false,
            attention_update: false,
            requeued: false,
            provenance: None,
            notices: Default::default(),
        }
    }

    fn report(task: &str, producer_agent: &str) -> InboxNotice {
        let mut n = notice(ORCH, REPORT, StateName::Queued);
        n.provenance = Some(ReportProvenance::Task {
            task_id: task.into(),
            report_id: "r-1".into(),
            producer: Producer {
                pane_id: WORKER.into(),
                agent_id: producer_agent.into(),
                role: Some("coder".into()),
            },
        });
        n
    }

    fn undeliverable(reason: UndeliverableReason) -> InboxNotice {
        let mut n = notice(WORKER, TASK, StateName::Undeliverable);
        n.reason = Some(reason);
        n
    }

    fn handoffs(spans: &[SpanRecord]) -> Vec<&SpanRecord> {
        spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_HANDOFF)
            .collect()
    }

    fn reads(spans: &[SpanRecord]) -> Vec<&SpanRecord> {
        spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_HANDOFF_READ)
            .collect()
    }

    fn only_handoff(tracker: &mut RunTracker) -> SpanRecord {
        let spans = tracker.take_finished();
        let found = handoffs(&spans);
        assert_eq!(found.len(), 1, "{spans:?}");
        found[0].clone()
    }

    fn event_names(span: &SpanRecord) -> Vec<(&'static str, u64)> {
        span.events
            .iter()
            .map(|e| (e.name, e.time_unix_nanos))
            .collect()
    }

    /// The hand-off ends when the report arrives, and the orchestrator's
    /// read of it is a span of its own, a child of the hand-off from where it
    /// ended to the acknowledgement.
    #[test]
    fn a_task_ends_reported_when_its_report_arrives() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&notice(WORKER, TASK, StateName::Fetched), 20);
        tracker.observe_inbox(&notice(WORKER, TASK, StateName::Acknowledged), 25);
        assert!(handoffs(&tracker.take_finished()).is_empty());
        tracker.observe_inbox(&report(TASK, "worker-1"), 30);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffId), Some(TASK));
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("reported"));
        assert_eq!(text(&span, AttrKey::RunId), Some("run-orch-pane"));
        assert_eq!(text(&span, AttrKey::PaneId), Some(ORCH));
        assert_eq!(text(&span, AttrKey::Role), Some("coder"));
        assert_eq!(text(&span, AttrKey::OrchestrationId), Some("orch-1"));
        assert_eq!(
            text(&span, AttrKey::HandoffWorkerRunId),
            Some("run-worker-1")
        );
        assert_eq!(attr(&span, AttrKey::HandoffUndeliverableReason), None);
        assert_eq!(
            event_names(&span),
            vec![(EVENT_HANDOFF_TAKEN, 20), (EVENT_HANDOFF_REPORTED, 30)],
            "a fetch and an acknowledgement are one take"
        );
        assert_eq!((span.start_unix_nanos, span.end_unix_nanos), (10, 30));
        assert_eq!(tracker.unread_count(), 1);

        // The orchestrator fetching the report is not reading it.
        tracker.observe_inbox(&notice(ORCH, REPORT, StateName::Fetched), 35);
        assert!(tracker.take_finished().is_empty());
        tracker.observe_inbox(&notice(ORCH, REPORT, StateName::Acknowledged), 40);
        let spans = tracker.take_finished();
        let found = reads(&spans);
        assert_eq!((found.len(), spans.len()), (1, 1), "{spans:?}");
        let read = found[0];
        assert_eq!((read.start_unix_nanos, read.end_unix_nanos), (30, 40));
        assert_eq!(
            (read.trace_id, read.parent_span_id),
            (span.trace_id, Some(span.span_id))
        );
        assert_eq!(text(read, AttrKey::HandoffId), Some(TASK));
        assert_eq!(text(read, AttrKey::RunId), Some("run-orch-pane"));
        assert_eq!(text(read, AttrKey::PaneId), Some(ORCH));
        assert_eq!(text(read, AttrKey::Role), Some("coder"));
        assert_eq!(text(read, AttrKey::OrchestrationId), Some("orch-1"));
        assert_eq!(tracker.unread_count(), 0);

        // Parented to the orchestrator's run, in its trace.
        tracker.end_run(ORCH, EndReason::Exited, None, 50);
        let spans = tracker.take_finished();
        assert!(handoffs(&spans).is_empty(), "the run's end sends it once");
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(span.trace_id, run.trace_id);
        assert_eq!(span.parent_span_id, Some(run.span_id));
        assert_eq!(tracker.handoffs_held(), (0, 0));
    }

    #[test]
    fn a_notice_that_overtakes_the_post_is_applied_when_it_arrives() {
        let mut tracker = team("worker-1");
        // A fast worker's take reaches telemetry before the post does.
        tracker.observe_inbox(&notice(WORKER, TASK, StateName::Acknowledged), 5);
        assert_eq!(tracker.handoffs_held(), (0, 1));
        post(&mut tracker, 10);
        assert_eq!(tracker.handoffs_held(), (1, 0));
        tracker.observe_inbox(&report(TASK, "worker-1"), 30);
        let span = only_handoff(&mut tracker);
        assert_eq!(
            event_names(&span),
            vec![(EVENT_HANDOFF_TAKEN, 10), (EVENT_HANDOFF_REPORTED, 30)]
        );
    }

    /// A report and its acknowledgement that both overtake the post are
    /// replayed in order: the hand-off ends at the report, and the read
    /// follows it.
    #[test]
    fn a_report_and_its_read_that_overtake_the_post_are_applied_in_order() {
        let mut tracker = team("worker-1");
        tracker.observe_inbox(&report(TASK, "worker-1"), 5);
        tracker.observe_inbox(&notice(ORCH, REPORT, StateName::Acknowledged), 6);
        assert_eq!(tracker.handoffs_held(), (0, 2));
        post(&mut tracker, 10);
        let spans = tracker.take_finished();
        let handoff = handoffs(&spans)[0];
        let read = reads(&spans)[0];
        assert_eq!(text(handoff, AttrKey::HandoffOutcome), Some("reported"));
        assert_eq!(read.parent_span_id, Some(handoff.span_id));
        assert_eq!(read.start_unix_nanos, handoff.end_unix_nanos);
        assert_eq!(tracker.handoffs_held(), (0, 0));
        assert_eq!(tracker.unread_count(), 0);
    }

    #[test]
    fn early_notices_are_bounded_in_count_and_age() {
        let mut tracker = team("worker-1");
        for i in 0..MAX_EARLY_NOTICES + 10 {
            tracker.observe_inbox(
                &notice(WORKER, &format!("m-other-{i}"), StateName::Fetched),
                1,
            );
        }
        assert_eq!(tracker.handoffs_held(), (0, MAX_EARLY_NOTICES));
        tracker.observe_inbox(
            &notice(WORKER, "m-late", StateName::Fetched),
            1 + EARLY_NOTICE_AGE_NANOS,
        );
        assert_eq!(tracker.handoffs_held(), (0, 1), "the old ones aged out");
        // An aged-out take is not applied to a post that comes after it.
        tracker.observe_inbox(
            &notice(WORKER, TASK, StateName::Fetched),
            2 + EARLY_NOTICE_AGE_NANOS,
        );
        post(&mut tracker, 3 + 2 * EARLY_NOTICE_AGE_NANOS);
        tracker.end_run(
            ORCH,
            EndReason::Exited,
            None,
            4 + 2 * EARLY_NOTICE_AGE_NANOS,
        );
        let span = only_handoff(&mut tracker);
        assert!(span.events.is_empty(), "{span:?}");
    }

    #[test]
    fn an_orchestrator_run_ending_first_ends_the_handoff_or_forgets_its_read() {
        // No report: unknown, at the run's end.
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&notice(WORKER, TASK, StateName::Acknowledged), 20);
        tracker.end_run(ORCH, EndReason::Exited, None, 50);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("unknown"));
        assert_eq!(span.end_unix_nanos, 50);
        assert_eq!(event_names(&span), vec![(EVENT_HANDOFF_TAKEN, 20)]);

        // The report arrived but was never read: the hand-off ended
        // `reported` when it arrived, and the run's end records no read.
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&report(TASK, "worker-1"), 30);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("reported"));
        assert_eq!(span.end_unix_nanos, 30);
        tracker.end_run(ORCH, EndReason::Exited, None, 60);
        let spans = tracker.take_finished();
        assert!(handoffs(&spans).is_empty() && reads(&spans).is_empty());
        assert_eq!(tracker.unread_count(), 0);
        // A late acknowledgement of that report finds nothing to read.
        tracker.observe_inbox(&notice(ORCH, REPORT, StateName::Acknowledged), 70);
        assert!(reads(&tracker.take_finished()).is_empty());
    }

    #[test]
    fn an_exited_worker_ends_the_handoff_undeliverable_with_the_reason() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&undeliverable(UndeliverableReason::AgentExited), 20);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("undeliverable"));
        assert_eq!(
            text(&span, AttrKey::HandoffUndeliverableReason),
            Some("agent_exited")
        );
        assert!(span.events.is_empty());
    }

    #[test]
    fn superseded_is_only_the_inbox_saying_so() {
        // A newer task to the same worker leaves the first one open.
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.delegate_posted(Posted {
            message_id: "m-newer".into(),
            orchestrator_pane: ORCH.into(),
            orchestrator_agent_id: Some("orch-agent".into()),
            worker_pane: WORKER.into(),
            worker_agent_id: "worker-1".into(),
            role: "coder".into(),
            at: 15,
        });
        assert_eq!(tracker.handoffs_held(), (2, 0));
        tracker.observe_inbox(&undeliverable(UndeliverableReason::Superseded), 20);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffId), Some(TASK));
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("superseded"));
        assert_eq!(attr(&span, AttrKey::HandoffUndeliverableReason), None);
        assert_eq!(tracker.handoffs_held(), (1, 0));
    }

    #[test]
    fn a_delivered_task_is_not_made_undeliverable_after_its_report() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&report(TASK, "worker-1"), 30);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("reported"));
        tracker.observe_inbox(&undeliverable(UndeliverableReason::AgentExited), 35);
        assert!(handoffs(&tracker.take_finished()).is_empty());
        tracker.observe_inbox(&notice(ORCH, REPORT, StateName::Acknowledged), 40);
        assert_eq!(reads(&tracker.take_finished()).len(), 1);
    }

    #[test]
    fn unread_reports_are_bounded_in_count_and_age() {
        let mut tracker = team("worker-1");
        for i in 0..MAX_UNREAD_REPORTS + 1 {
            let task = format!("m-task-{i}");
            tracker.delegate_posted(Posted {
                message_id: task.clone(),
                orchestrator_pane: ORCH.into(),
                orchestrator_agent_id: Some("orch-agent".into()),
                worker_pane: WORKER.into(),
                worker_agent_id: "worker-1".into(),
                role: "coder".into(),
                at: 10,
            });
            let mut n = report(&task, "worker-1");
            n.message_id = format!("m-report-{i}");
            tracker.observe_inbox(&n, 20);
        }
        assert_eq!(tracker.unread_count(), MAX_UNREAD_REPORTS);
        tracker.take_finished();
        // The oldest was forgotten: its acknowledgement records no read.
        tracker.observe_inbox(&notice(ORCH, "m-report-0", StateName::Acknowledged), 30);
        assert!(reads(&tracker.take_finished()).is_empty());
        tracker.observe_inbox(&notice(ORCH, "m-report-1", StateName::Acknowledged), 30);
        assert_eq!(reads(&tracker.take_finished()).len(), 1);
        tracker.expire_handoffs(20 + MAX_HANDOFF_AGE_NANOS);
        assert_eq!(tracker.unread_count(), 0, "forgotten at the age bound");
    }

    #[test]
    fn the_report_names_the_run_of_the_worker_that_sent_it() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        // The worker respawns before reporting.
        tracker.begin_run(worker("worker-2"), 20);
        tracker.observe_inbox(&report(TASK, "worker-2"), 30);
        let span = only_handoff(&mut tracker);
        assert_eq!(
            text(&span, AttrKey::HandoffWorkerRunId),
            Some("run-worker-2")
        );
    }

    #[test]
    fn daemon_shutdown_ends_open_handoffs_as_daemon_shutdown() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        // One with no orchestrator run the tracker knows.
        tracker.delegate_posted(Posted {
            message_id: "m-orphan".into(),
            orchestrator_pane: "elsewhere".into(),
            orchestrator_agent_id: None,
            worker_pane: WORKER.into(),
            worker_agent_id: "worker-1".into(),
            role: "coder".into(),
            at: 12,
        });
        tracker.end_all(EndReason::DaemonShutdown, 90);
        let spans = tracker.take_finished();
        let found = handoffs(&spans);
        assert_eq!(found.len(), 2, "{spans:?}");
        for span in &found {
            assert_eq!(text(span, AttrKey::HandoffOutcome), Some("daemon_shutdown"));
            assert_eq!(span.end_unix_nanos, 90);
        }
        let orphan = found
            .iter()
            .find(|s| text(s, AttrKey::HandoffId) == Some("m-orphan"))
            .unwrap();
        assert_eq!(orphan.parent_span_id, None);
        assert_eq!(text(orphan, AttrKey::RunId), None);
        assert_eq!(tracker.handoffs_held(), (0, 0));
    }

    #[test]
    fn open_handoffs_are_bounded_in_count_and_age() {
        let mut tracker = team("worker-1");
        for i in 0..MAX_OPEN_HANDOFFS + 1 {
            tracker.delegate_posted(Posted {
                message_id: format!("m-{i}"),
                orchestrator_pane: ORCH.into(),
                orchestrator_agent_id: Some("orch-agent".into()),
                worker_pane: WORKER.into(),
                worker_agent_id: "worker-1".into(),
                role: "coder".into(),
                at: 10 + i as u64,
            });
        }
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffId), Some("m-0"));
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("unknown"));
        assert_eq!(tracker.handoffs_held().0, MAX_OPEN_HANDOFFS);
        // A day on, the next inbox change ends every one of them.
        tracker.observe_inbox(
            &notice(WORKER, "m-unrelated", StateName::Fetched),
            11 + MAX_OPEN_HANDOFFS as u64 + MAX_HANDOFF_AGE_NANOS,
        );
        let spans = tracker.take_finished();
        assert_eq!(handoffs(&spans).len(), MAX_OPEN_HANDOFFS);
        assert_eq!(tracker.handoffs_held().0, 0);
    }

    /// Issue #1981 M7 review: a hand-off left open on a quiet daemon ends
    /// `unknown` at its age bound when the periodic check runs, with no inbox
    /// change to prompt it, and a held notice past its own bound goes too.
    #[test]
    fn a_quiet_handoff_ends_unknown_at_its_age_bound() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&notice(WORKER, TASK, StateName::Acknowledged), 20);
        tracker.observe_inbox(&notice(WORKER, "m-early", StateName::Fetched), 30);
        assert_eq!(tracker.handoffs_held(), (1, 1));
        tracker.expire_handoffs(10 + MAX_HANDOFF_AGE_NANOS - 1);
        assert!(handoffs(&tracker.take_finished()).is_empty());
        assert_eq!(tracker.handoffs_held(), (1, 0), "the held notice aged out");
        tracker.expire_handoffs(10 + MAX_HANDOFF_AGE_NANOS + 5_000);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("unknown"));
        assert_eq!(span.end_unix_nanos, 10 + MAX_HANDOFF_AGE_NANOS);
        assert_eq!(event_names(&span), vec![(EVENT_HANDOFF_TAKEN, 20)]);
        assert_eq!(tracker.handoffs_held(), (0, 0));
    }

    /// Issue #1981 M7 review: a daemon shut down after a hand-off passed its
    /// age bound ends it `unknown` at that bound, not `daemon_shutdown` at the
    /// shutdown.
    #[test]
    fn a_shutdown_past_the_age_bound_ends_the_handoff_unknown() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        let hour = 60 * 60 * 1_000_000_000;
        tracker.end_all(EndReason::DaemonShutdown, 10 + 25 * hour);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffOutcome), Some("unknown"));
        assert_eq!(span.end_unix_nanos, 10 + MAX_HANDOFF_AGE_NANOS);
        // Parented to the orchestrator's run, which ended with the shutdown.
        assert!(span.parent_span_id.is_some());
    }

    /// A report still unread past the age bound is forgotten, whether the
    /// check or the orchestrator's run ending finds it first: its late
    /// acknowledgement records no read.
    #[test]
    fn an_unread_report_past_the_age_bound_records_no_read() {
        let hour = 60 * 60 * 1_000_000_000;
        // The periodic check finds it first.
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&report(TASK, "worker-1"), 30);
        assert_eq!(
            text(&only_handoff(&mut tracker), AttrKey::HandoffOutcome),
            Some("reported")
        );
        tracker.expire_handoffs(30 + MAX_HANDOFF_AGE_NANOS);
        assert_eq!(tracker.unread_count(), 0);
        tracker.observe_inbox(
            &notice(ORCH, REPORT, StateName::Acknowledged),
            10 + 26 * hour,
        );
        assert!(reads(&tracker.take_finished()).is_empty());

        // An acknowledgement past the bound, with no check in between: the
        // bound is applied before the acknowledgement is.
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&report(TASK, "worker-1"), 30);
        tracker.take_finished();
        tracker.observe_inbox(
            &notice(ORCH, REPORT, StateName::Acknowledged),
            30 + MAX_HANDOFF_AGE_NANOS,
        );
        assert!(reads(&tracker.take_finished()).is_empty());
    }

    /// The age bound is each hand-off's own, not only the oldest post's: a
    /// post stamped earlier but applied later still ends on time.
    #[test]
    fn every_handoff_past_its_bound_ends_not_only_the_first_posted() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 100);
        tracker.delegate_posted(Posted {
            message_id: "m-stamped-earlier".into(),
            orchestrator_pane: ORCH.into(),
            orchestrator_agent_id: Some("orch-agent".into()),
            worker_pane: WORKER.into(),
            worker_agent_id: "worker-1".into(),
            role: "coder".into(),
            at: 50,
        });
        tracker.expire_handoffs(50 + MAX_HANDOFF_AGE_NANOS);
        let span = only_handoff(&mut tracker);
        assert_eq!(text(&span, AttrKey::HandoffId), Some("m-stamped-earlier"));
        assert_eq!(tracker.handoffs_held(), (1, 0));
    }

    fn uncorrelated(requeued: bool) -> InboxNotice {
        let mut n = notice(ORCH, "m-note", StateName::Queued);
        n.agent_id = Some("orch-agent".into());
        n.requeued = requeued;
        n.provenance = Some(ReportProvenance::Uncorrelated {
            report_id: "r-2".into(),
            producer: Producer {
                pane_id: WORKER.into(),
                agent_id: "worker-1".into(),
                role: None,
            },
        });
        n
    }

    #[test]
    fn a_report_naming_no_task_counts_once_on_the_orchestrator_run() {
        let mut tracker = team("worker-1");
        tracker.observe_inbox(&uncorrelated(false), 10);
        // Handed back and queued again, and repeated by a wake update: the
        // same report, not another.
        tracker.observe_inbox(&uncorrelated(true), 20);
        let mut wake = uncorrelated(false);
        wake.wake_update = true;
        tracker.observe_inbox(&wake, 25);
        tracker.end_run(ORCH, EndReason::Exited, None, 30);
        tracker.end_run(WORKER, EndReason::Exited, None, 30);
        let spans = tracker.take_finished();
        assert!(handoffs(&spans).is_empty());
        let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
        let count = |pane: &str| {
            let run = runs
                .iter()
                .find(|s| text(s, AttrKey::PaneId) == Some(pane))
                .unwrap();
            attr(run, AttrKey::HandoffsUncorrelatedCount).cloned()
        };
        assert_eq!(count(ORCH), Some(AttrValue::Int(1)));
        assert_eq!(count(WORKER), None, "not on a worker that received none");
    }

    /// The uncorrelated-report count on each `agent.run` span finished now,
    /// by run id.
    fn uncorrelated_counts(tracker: &mut RunTracker) -> Vec<(String, Option<AttrValue>)> {
        let mut counts: Vec<_> = tracker
            .take_finished()
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN)
            .map(|s| {
                (
                    text(s, AttrKey::RunId).unwrap().to_string(),
                    attr(s, AttrKey::HandoffsUncorrelatedCount).cloned(),
                )
            })
            .collect();
        counts.sort_by(|a, b| a.0.cmp(&b.0));
        counts
    }

    /// The orchestrator's generation `agent` on [`ORCH`].
    fn orchestrator(agent: &str) -> RunStart {
        let mut orch = start(ORCH);
        orch.run_id = format!("run-{agent}");
        orch.agent_id = Some(agent.into());
        orch
    }

    /// PR #2010 review: a report naming no task that reaches the
    /// orchestrator while its run is prepared, its `Commit` not applied yet,
    /// is counted on that run once it opens.
    #[test]
    fn a_report_naming_no_task_before_its_runs_commit_counts_once_it_opens() {
        let mut tracker = RunTracker::new();
        tracker.prepare_run(orchestrator("orch-agent"), 0);
        tracker.observe_inbox(&uncorrelated(false), 10);
        tracker.observe_inbox(&uncorrelated(false), 15);
        tracker.commit_run("run-orch-agent");
        tracker.end_run(ORCH, EndReason::Exited, None, 30);
        assert_eq!(
            uncorrelated_counts(&mut tracker),
            vec![("run-orch-agent".into(), Some(AttrValue::Int(2)))]
        );
    }

    /// PR #2010 review: a report naming no task that reaches a generation a
    /// newer one replaced, while the older one still runs, is counted on the
    /// older generation's run, not the newer one.
    #[test]
    fn a_report_naming_no_task_for_a_replaced_generation_counts_on_that_generation() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(orchestrator("orch-agent"), 0);
        tracker.begin_run(orchestrator("orch-agent-2"), 5);
        tracker.observe_inbox(&uncorrelated(false), 10);
        tracker.end_agent_run(ORCH, Some("orch-agent"), EndReason::Exited, None, 20);
        tracker.end_run(ORCH, EndReason::Exited, None, 30);
        assert_eq!(
            uncorrelated_counts(&mut tracker),
            vec![
                ("run-orch-agent".into(), Some(AttrValue::Int(1))),
                ("run-orch-agent-2".into(), None),
            ]
        );
    }

    /// PR #2010 review: after a restart, a report naming no task that
    /// reaches the generation whose run has ended is counted on no run: not
    /// on the new generation, open or prepared.
    #[test]
    fn a_report_for_an_ended_generation_is_not_counted_on_the_one_after_a_restart() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(orchestrator("orch-agent"), 0);
        tracker.end_run(ORCH, EndReason::Exited, None, 5);
        tracker.prepare_run(orchestrator("orch-agent-2"), 6);
        tracker.observe_inbox(&uncorrelated(false), 7);
        tracker.commit_run("run-orch-agent-2");
        tracker.observe_inbox(&uncorrelated(false), 10);
        tracker.end_run(ORCH, EndReason::Exited, None, 30);
        assert_eq!(
            uncorrelated_counts(&mut tracker),
            vec![
                ("run-orch-agent".into(), None),
                ("run-orch-agent-2".into(), None),
            ]
        );
    }

    #[test]
    fn no_task_or_report_text_reaches_a_span() {
        let mut tracker = team("worker-1");
        post(&mut tracker, 10);
        tracker.observe_inbox(&report(TASK, "worker-1"), 30);
        tracker.observe_inbox(&notice(ORCH, REPORT, StateName::Acknowledged), 40);
        let spans = tracker.take_finished();
        assert_eq!((handoffs(&spans).len(), reads(&spans).len()), (1, 1));
        let all = format!("{spans:?}");
        assert!(!all.contains("PRIVATE"), "{all}");
    }
}
