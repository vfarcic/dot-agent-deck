//! PRD #1666 part 3, round 5 (plan D7, D10, D12, D13, §3): what the deck keeps
//! about reports and dispatch results whose recipient may be gone. Pure data:
//! no locks, no IO. The inbox hub owns one [`ReportState`] behind its
//! [`crate::guarded::Guarded`] latch.
//!
//! - [`OutcomeStore`]: the recovery rows ("Deck notices"), bounded; evicting a
//!   row never evicts the body it names, which is a retained file or a
//!   fallback body.
//! - [`AcceptedReports`]: every posted report and dispatch result, by message
//!   id, independent of the inbox row, so trimming history or forgetting a
//!   pane cannot lose it before it is settled (D13).
//! - [`UnitTable`]: each dispatched unit's result, and the index of every
//!   terminal generation it has had (D12).
//! - [`Publications`]: the generations that will be a unit's terminal and may
//!   attest before they are indexed (D7).
//! - [`FallbackBodies`]: dispatch results the deck could not write to disk,
//!   held in memory under their report id (§3.2).
//! - [`TaskLedger`]: every delegated task, bound to the worker generation and
//!   orchestration instance it was posted to, while it is open, and a bounded
//!   tombstone once it is reported, closed or superseded (D2, D8, D12).
//!
//! The wire projection (`ReportProvenance`, `DeckOutcome`) is frozen here for
//! round 6. Every enum a newer deck may extend decodes an unknown value as
//! `Unknown`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::inbox::{Recipient, UndeliverableReason};

// ---------------------------------------------------------------------------
// The wire projection (D10)
// ---------------------------------------------------------------------------

/// Who produced a report: the generation that sent it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    pub pane_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Which leg of a dispatch a dispatch-result message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchPhase {
    Started,
    Failed,
    Ended,
    #[serde(other)]
    Unknown,
}

/// What a report or dispatch-result message is, carried on `InboxNotice` and
/// `MessageSummary` as `provenance`. No variant carries a field it cannot
/// fill truthfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReportProvenance {
    Task {
        task_id: String,
        report_id: String,
        producer: Producer,
    },
    Terminal {
        unit_id: String,
        report_id: String,
        producer: Producer,
    },
    Uncorrelated {
        report_id: String,
        producer: Producer,
    },
    DispatchResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit_id: Option<String>,
        phase: DispatchPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report_id: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

impl ReportProvenance {
    /// The retained file this message's body is, if it has one.
    pub fn report_id(&self) -> Option<&str> {
        match self {
            ReportProvenance::Task { report_id, .. }
            | ReportProvenance::Terminal { report_id, .. }
            | ReportProvenance::Uncorrelated { report_id, .. } => Some(report_id),
            ReportProvenance::DispatchResult { report_id, .. } => report_id.as_deref(),
            ReportProvenance::Unknown => None,
        }
    }
}

/// A recovery row's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomePhase {
    DispatchStarted,
    DispatchFailed,
    DispatchFinished,
    DispatchEnded,
    WorkerReport,
    #[serde(other)]
    Unknown,
}

/// Why a row exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeCause {
    RecipientExited,
    RecipientRefused,
    DeliveryFailed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// How far the message got before it became a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeDelivery {
    NeverHanded,
    HandedUnconfirmed,
    AcknowledgedLate,
    #[serde(other)]
    Unknown,
}

/// Where a row's body is: a retained file, a fallback body held in memory,
/// or nowhere (a `dispatch_ended` row, whose whole content is its summary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStorage {
    Artifact,
    Memory,
    None,
    #[serde(other)]
    Unknown,
}

/// What a client may do with a row. Round 5 ships the list empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeOp {
    ViewResult,
    Dismiss,
    #[serde(other)]
    Unknown,
}

/// A party a row names: the intended recipient, or the producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeParty {
    pub pane_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// The dispatched unit a row is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeUnit {
    pub unit_id: String,
    pub name: String,
}

/// One recovery row, as `ListOutcomes` answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckOutcome {
    pub outcome_id: String,
    pub phase: OutcomePhase,
    pub cause: OutcomeCause,
    pub delivery: OutcomeDelivery,
    pub label: String,
    pub original: OutcomeParty,
    pub producer: OutcomeParty,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<OutcomeUnit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_id: Option<String>,
    pub storage: OutcomeStorage,
    pub summary: String,
    pub created_at_ms: i64,
    pub revision: u64,
    #[serde(default)]
    pub operations: Vec<OutcomeOp>,
}

/// The answer to `ListOutcomes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckOutcomes {
    pub deck_id: String,
    pub incarnation: String,
    pub revision: u64,
    pub overflow: u64,
    pub outcomes: Vec<DeckOutcome>,
}

/// The most characters a row's label keeps.
pub const MAX_LABEL_CHARS: usize = 120;
/// The most bytes a row's summary keeps.
pub const MAX_SUMMARY_BYTES: usize = 1024;

/// A producer-influenced string, escaped and capped before it goes on the
/// wire.
pub fn wire_text(text: &str, max_chars: usize) -> String {
    let escaped = crate::config_validation::escape_field_for_log(text, max_chars);
    escaped.chars().take(max_chars).collect()
}

/// `text` cut to at most `max` bytes at a char boundary.
pub fn cut_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text[..at].to_string()
}

// ---------------------------------------------------------------------------
// The recovery rows
// ---------------------------------------------------------------------------

/// The environment variable that lowers the row bound (a test knob).
pub const ENV_OUTCOMES_MAX: &str = "DOT_AGENT_DECK_DECK_OUTCOMES_MAX";
/// The row bound.
pub const DEFAULT_OUTCOMES_MAX: usize = 64;

#[derive(Debug, Clone)]
struct Row {
    outcome: DeckOutcome,
    /// The message the row was made from, for a late acknowledgement.
    message_id: Option<String>,
}

/// The recovery rows: at most `max`, the oldest evicted first, counted in
/// `overflow`. Evicting a row never evicts the body it names.
#[derive(Debug)]
pub struct OutcomeStore {
    rows: VecDeque<Row>,
    max: usize,
    overflow: u64,
    revision: u64,
    next_id: u64,
}

impl OutcomeStore {
    pub fn new(max: usize) -> Self {
        OutcomeStore {
            rows: VecDeque::new(),
            max: max.max(1),
            overflow: 0,
            revision: 0,
            next_id: 1,
        }
    }

    pub fn from_env() -> Self {
        let max = std::env::var(ENV_OUTCOMES_MAX)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_OUTCOMES_MAX);
        Self::new(max)
    }

    /// Add a row, giving it an id and the store's next revision; the oldest
    /// row goes when the bound is passed. Answers the row's id.
    pub fn insert(&mut self, mut outcome: DeckOutcome, message_id: Option<String>) -> String {
        let id = format!("o-{}", self.next_id);
        self.next_id += 1;
        self.revision += 1;
        outcome.outcome_id = id.clone();
        outcome.revision = self.revision;
        self.rows.push_back(Row {
            outcome,
            message_id,
        });
        while self.rows.len() > self.max {
            self.rows.pop_front();
            self.overflow += 1;
        }
        id
    }

    /// Whether a row already names `report_id`.
    pub fn has_report(&self, report_id: &str) -> bool {
        self.rows
            .iter()
            .any(|row| row.outcome.report_id.as_deref() == Some(report_id))
    }

    /// A row's report was acknowledged after it was set aside: its delivery
    /// becomes `acknowledged_late`. Answers the row's report id when one
    /// changed.
    pub fn acknowledged_late(&mut self, message_id: &str) -> Option<Option<String>> {
        let row = self
            .rows
            .iter_mut()
            .find(|row| row.message_id.as_deref() == Some(message_id))?;
        self.revision += 1;
        row.outcome.delivery = OutcomeDelivery::AcknowledgedLate;
        row.outcome.revision = self.revision;
        Some(row.outcome.report_id.clone())
    }

    /// A held body was saved to a file after all: its rows' storage becomes
    /// `artifact`.
    pub fn saved(&mut self, report_id: &str) {
        let mut changed = false;
        for row in &mut self.rows {
            if row.outcome.report_id.as_deref() == Some(report_id)
                && row.outcome.storage == OutcomeStorage::Memory
            {
                row.outcome.storage = OutcomeStorage::Artifact;
                changed = true;
            }
        }
        if changed {
            self.revision += 1;
            let revision = self.revision;
            for row in &mut self.rows {
                if row.outcome.report_id.as_deref() == Some(report_id) {
                    row.outcome.revision = revision;
                }
            }
        }
    }

    pub fn rows(&self) -> Vec<DeckOutcome> {
        self.rows.iter().map(|row| row.outcome.clone()).collect()
    }

    pub fn overflow(&self) -> u64 {
        self.overflow
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
}

// ---------------------------------------------------------------------------
// The accepted-report ledger (D13)
// ---------------------------------------------------------------------------

/// A posted report or dispatch result, held until its message is
/// acknowledged or set aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedReport {
    pub provenance: ReportProvenance,
    pub phase: OutcomePhase,
    /// The retained file, or `None` for `dispatch_ended`, whose text is
    /// `summary`.
    pub report_id: Option<String>,
    pub summary: String,
    pub storage: OutcomeStorage,
    pub label: String,
    pub original: OutcomeParty,
    pub producer: OutcomeParty,
    pub unit: Option<OutcomeUnit>,
    pub orchestration: Option<String>,
    pub task_id: Option<String>,
    /// Whether the message was ever handed to its recipient.
    pub handed: bool,
}

impl AcceptedReport {
    /// The row this entry becomes when its message is set aside.
    pub fn into_row(self, cause: OutcomeCause, now_ms: i64) -> DeckOutcome {
        DeckOutcome {
            outcome_id: String::new(),
            phase: self.phase,
            cause,
            delivery: if self.handed {
                OutcomeDelivery::HandedUnconfirmed
            } else {
                OutcomeDelivery::NeverHanded
            },
            label: self.label,
            original: self.original,
            producer: self.producer,
            unit: self.unit,
            orchestration: self.orchestration,
            task_id: self.task_id,
            report_id: self.report_id,
            storage: self.storage,
            summary: self.summary,
            created_at_ms: now_ms,
            revision: 0,
            operations: Vec::new(),
        }
    }
}

/// The cause a row gets from the reason its message was set aside, and
/// whether a row is made at all (`InboxShut` makes none: the daemon is
/// stopping, and the file outlives it).
pub fn cause_of(reason: UndeliverableReason) -> Option<OutcomeCause> {
    match reason {
        UndeliverableReason::AgentExited => Some(OutcomeCause::RecipientExited),
        UndeliverableReason::MaxDeliver => Some(OutcomeCause::DeliveryFailed),
        UndeliverableReason::Cancelled => Some(OutcomeCause::Cancelled),
        UndeliverableReason::InboxShut => None,
        UndeliverableReason::Superseded => Some(OutcomeCause::Unknown),
    }
}

/// D13's ledger: posted reports and dispatch results by message id.
pub type AcceptedReports = HashMap<String, AcceptedReport>;

// ---------------------------------------------------------------------------
// The unit table (D12)
// ---------------------------------------------------------------------------

/// Where a report was delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Inbox,
    Recovery,
    None,
    #[serde(other)]
    Unknown,
}

/// Whether the unit's completion was marked before its report was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitMark {
    Marked,
    UnitEndedFirst,
}

/// How a unit ended without a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndedHow {
    Exited,
    Closed,
    RespawnFailed,
}

impl EndedHow {
    /// The clause a `dispatch ended` notice uses.
    pub fn clause(self) -> &'static str {
        match self {
            EndedHow::Exited => "it exited",
            EndedHow::Closed => "it was closed",
            EndedHow::RespawnFailed => "it could not be restarted",
        }
    }
}

/// A unit's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitEntry {
    /// A report from its terminal generation is being saved.
    Claimed { claim_id: u64 },
    /// It reported.
    Completed {
        report_id: Option<String>,
        digest: [u8; 32],
        destination: Destination,
        unit_mark: UnitMark,
    },
    /// It ended without a report.
    Ended { how: EndedHow },
}

/// The environment variable that lowers the per-instance tombstone bound (a
/// test knob); the daemon-wide bound is sixteen times it.
pub const ENV_TOMBSTONES_MAX: &str = "DOT_AGENT_DECK_DECK_TOMBSTONES_MAX";
/// Tombstones kept per orchestration instance.
pub const DEFAULT_TOMBSTONES_MAX: usize = 256;

/// The per-instance tombstone bound, from [`ENV_TOMBSTONES_MAX`].
pub fn tombstones_max_from_env() -> usize {
    std::env::var(ENV_TOMBSTONES_MAX)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TOMBSTONES_MAX)
        .max(1)
}

/// Units' results and the index of their terminal generations. A unit's
/// entry and its index entries are evicted together, oldest first, past the
/// daemon cap, never while the unit is `Claimed` or the caller's `protected`
/// answers true for it (a generation it names is still registered, or a
/// publication names it).
#[derive(Debug)]
pub struct UnitTable {
    units: HashMap<String, UnitEntry>,
    by_generation: HashMap<(String, String), String>,
    order: VecDeque<String>,
    cap: usize,
}

impl UnitTable {
    pub fn new(cap: usize) -> Self {
        UnitTable {
            units: HashMap::new(),
            by_generation: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    pub fn get(&self, unit_id: &str) -> Option<&UnitEntry> {
        self.units.get(unit_id)
    }

    /// The unit `(pane, agent)` is indexed to, if any.
    pub fn unit_of(&self, pane_id: &str, agent_id: &str) -> Option<&str> {
        self.by_generation
            .get(&(pane_id.to_string(), agent_id.to_string()))
            .map(String::as_str)
    }

    /// Index `(pane, agent)` as a terminal generation of `unit_id`.
    pub fn index(&mut self, pane_id: &str, agent_id: &str, unit_id: &str) {
        self.by_generation.insert(
            (pane_id.to_string(), agent_id.to_string()),
            unit_id.to_string(),
        );
        self.touch(unit_id);
    }

    /// Set `unit_id`'s entry.
    pub fn set(&mut self, unit_id: &str, entry: UnitEntry) {
        self.units.insert(unit_id.to_string(), entry);
        self.touch(unit_id);
    }

    pub fn remove(&mut self, unit_id: &str) -> Option<UnitEntry> {
        self.units.remove(unit_id)
    }

    fn touch(&mut self, unit_id: &str) {
        if !self.order.iter().any(|u| u == unit_id) {
            self.order.push_back(unit_id.to_string());
        }
    }

    /// Every generation indexed to `unit_id`.
    pub fn generations_of(&self, unit_id: &str) -> Vec<(String, String)> {
        self.by_generation
            .iter()
            .filter(|(_, unit)| *unit == unit_id)
            .map(|(generation, _)| generation.clone())
            .collect()
    }

    /// How many units the table holds.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Evict the oldest units past the cap, skipping a `Claimed` one and any
    /// `protected` answers true for (given the unit and its generations).
    /// Answers the units evicted.
    pub fn evict(&mut self, protected: impl Fn(&str, &[(String, String)]) -> bool) -> Vec<String> {
        let mut evicted = Vec::new();
        let mut kept = VecDeque::new();
        while self.order.len() + kept.len() > self.cap {
            let Some(unit) = self.order.pop_front() else {
                break;
            };
            let generations = self.generations_of(&unit);
            let claimed = matches!(self.units.get(&unit), Some(UnitEntry::Claimed { .. }));
            if claimed || protected(&unit, &generations) {
                kept.push_back(unit);
                continue;
            }
            self.units.remove(&unit);
            for generation in generations {
                self.by_generation.remove(&generation);
            }
            evicted.push(unit);
        }
        while let Some(unit) = kept.pop_back() {
            self.order.push_front(unit);
        }
        evicted
    }
}

// ---------------------------------------------------------------------------
// Publications (D7)
// ---------------------------------------------------------------------------

/// What a publication is keyed by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PublishKey {
    /// An orchestration dispatch, by its instance.
    Instance(String),
    /// A single dispatch's pane, or a respawn's pane.
    Pane(String),
}

/// A generation that will be a unit's terminal and may attest before it is
/// indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publication {
    /// A dispatch not yet registered.
    Initial,
    /// A respawn of unit `unit`'s terminal generation `from`, under ticket
    /// `ticket`; `closed` once a close settled the unit while it ran.
    Replacement {
        unit: String,
        from: String,
        ticket: u64,
        closed: bool,
    },
}

impl Publication {
    pub fn names_unit(&self, unit_id: &str) -> bool {
        matches!(self, Publication::Replacement { unit, .. } if unit == unit_id)
    }
}

pub type Publications = HashMap<PublishKey, Publication>;

// ---------------------------------------------------------------------------
// Fallback bodies (§3.2)
// ---------------------------------------------------------------------------

/// A dispatch result held in memory because its file could not be written.
#[derive(Debug)]
pub struct FallbackBody {
    /// The whole file the deck would have written.
    pub contents: Vec<u8>,
    pub held_since: Instant,
    /// Its retained room, for the retry.
    pub held: crate::retained_store::HeldResult,
    /// When its lifetime ends: 24 hours after its message was acknowledged,
    /// or 7 days after it was set aside; `None` while it is referenced.
    pub expires: Option<Instant>,
}

pub type FallbackBodies = HashMap<String, FallbackBody>;

// ---------------------------------------------------------------------------
// The task ledger (D2, D8, D12)
// ---------------------------------------------------------------------------

/// What a delegated task is bound to when it is posted. Immutable: a report
/// is checked against it, never against whoever holds the worker's pane now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBinding {
    pub worker_pane: String,
    pub worker_agent: String,
    /// The worker's orchestration instance, when it has one.
    pub instance: Option<String>,
    pub role: String,
    /// The orchestrator generation the task's notices and report go to.
    pub orchestrator: Option<Recipient>,
    /// The commission arm and idle-worker record its delegate armed.
    pub arm_id: Option<u64>,
    pub seq: Option<u64>,
}

/// Where the task's message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskDelivery {
    /// Not yet acknowledged, and still deliverable.
    Live,
    /// The worker acknowledged it.
    Acknowledged,
    /// It became undeliverable for a reason other than the worker's exit
    /// (out of hand-overs, cancelled), so its exit owes no undeliverable
    /// notice for it.
    Ended,
}

/// An open task's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Waiting for its report.
    Open { revision: u64 },
    /// A report for it is being saved.
    Reporting { claim_id: u64 },
}

/// A task that is not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tombstone {
    /// It was reported. A report taken before task-scoped completion carries
    /// no report id or digest.
    Reported {
        report_id: Option<String>,
        digest: Option<[u8; 32]>,
        destination: Destination,
    },
    /// Its worker generation ended before it was reported.
    Closed,
    /// A newer task to the same worker replaced it (`by`), or the worker was
    /// restarted (`None`).
    Superseded { by: Option<String> },
}

/// An open task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub binding: TaskBinding,
    pub delivery: TaskDelivery,
    pub state: TaskState,
    /// Its worker's exit decided which notices it owed (D12 rule 4).
    pub exit_walked: bool,
}

/// What the ledger knows about a task id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskLookup<'a> {
    Open(&'a Task),
    Settled(&'a Tombstone),
    Unknown,
}

/// A task id is already known to the ledger: ids are bound once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebind;

/// The tombstone key for a task with no orchestration instance.
const NO_INSTANCE: &str = "";

/// Every delegated task the deck still knows, by its message id. Open tasks
/// are never evicted. Tombstones are kept up to `per_instance` for each
/// orchestration instance and `global` across the daemon, oldest evicted
/// first; a report naming an evicted task is unknown, and its id is never
/// bound again (ids are minted once).
#[derive(Debug)]
pub struct TaskLedger {
    open: HashMap<String, Task>,
    settled: HashMap<String, (String, Tombstone)>,
    by_instance: HashMap<String, VecDeque<String>>,
    order: VecDeque<String>,
    per_instance: usize,
    global: usize,
}

impl TaskLedger {
    pub fn new(per_instance: usize) -> Self {
        let per_instance = per_instance.max(1);
        TaskLedger {
            open: HashMap::new(),
            settled: HashMap::new(),
            by_instance: HashMap::new(),
            order: VecDeque::new(),
            per_instance,
            global: per_instance.saturating_mul(16),
        }
    }

    /// Bind `task_id`, open and live. Refused for an id the ledger knows.
    pub fn bind(&mut self, task_id: &str, binding: TaskBinding) -> Result<(), Rebind> {
        if self.open.contains_key(task_id) || self.settled.contains_key(task_id) {
            return Err(Rebind);
        }
        self.open.insert(
            task_id.to_string(),
            Task {
                binding,
                delivery: TaskDelivery::Live,
                state: TaskState::Open { revision: 0 },
                exit_walked: false,
            },
        );
        Ok(())
    }

    pub fn lookup(&self, task_id: &str) -> TaskLookup<'_> {
        if let Some(task) = self.open.get(task_id) {
            return TaskLookup::Open(task);
        }
        match self.settled.get(task_id) {
            Some((_, tombstone)) => TaskLookup::Settled(tombstone),
            None => TaskLookup::Unknown,
        }
    }

    pub fn open_task_mut(&mut self, task_id: &str) -> Option<&mut Task> {
        self.open.get_mut(task_id)
    }

    /// The open tasks bound to `(pane, agent)`, oldest id order not implied.
    pub fn open_of_generation(&self, pane_id: &str, agent_id: &str) -> Vec<String> {
        self.open_where(|b| b.worker_pane == pane_id && b.worker_agent == agent_id)
    }

    /// The open tasks bound to any generation of `pane_id`.
    pub fn open_of_pane(&self, pane_id: &str) -> Vec<String> {
        self.open_where(|b| b.worker_pane == pane_id)
    }

    fn open_where(&self, keep: impl Fn(&TaskBinding) -> bool) -> Vec<String> {
        let mut ids: Vec<String> = self
            .open
            .iter()
            .filter(|(_, task)| keep(&task.binding))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Settle open task `task_id` as `tombstone`, and evict the oldest
    /// tombstones past the bounds. Answers the task, or `None` when it was
    /// not open.
    pub fn settle(&mut self, task_id: &str, tombstone: Tombstone) -> Option<Task> {
        let task = self.open.remove(task_id)?;
        let instance = task
            .binding
            .instance
            .clone()
            .unwrap_or_else(|| NO_INSTANCE.to_string());
        self.settled
            .insert(task_id.to_string(), (instance.clone(), tombstone));
        self.by_instance
            .entry(instance.clone())
            .or_default()
            .push_back(task_id.to_string());
        self.order.push_back(task_id.to_string());
        self.evict(&instance);
        Some(task)
    }

    fn evict(&mut self, instance: &str) {
        while self
            .by_instance
            .get(instance)
            .is_some_and(|ids| ids.len() > self.per_instance)
        {
            let Some(oldest) = self
                .by_instance
                .get_mut(instance)
                .and_then(VecDeque::pop_front)
            else {
                break;
            };
            self.forget(&oldest);
        }
        while self.order.len() > self.global {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((instance, _)) = self.settled.remove(&oldest)
                && let Some(ids) = self.by_instance.get_mut(&instance)
            {
                ids.retain(|id| *id != oldest);
                if ids.is_empty() {
                    self.by_instance.remove(&instance);
                }
            }
        }
    }

    fn forget(&mut self, task_id: &str) {
        self.settled.remove(task_id);
        self.order.retain(|id| id != task_id);
    }

    /// How many tombstones the ledger holds.
    pub fn settled_len(&self) -> usize {
        self.settled.len()
    }
}

// ---------------------------------------------------------------------------
// The report state
// ---------------------------------------------------------------------------

/// Everything round 5 adds to the inbox hub, kept behind the hub's
/// [`crate::guarded::Guarded`] latch.
#[derive(Debug)]
pub struct ReportState {
    pub outcomes: OutcomeStore,
    pub accepted: AcceptedReports,
    pub units: UnitTable,
    pub publications: Publications,
    pub fallback: FallbackBodies,
    pub tasks: TaskLedger,
    /// Tasks a worker's exit walk decided (D12 rule 4), until the exit's
    /// `AgentExited` event for each is settled: `settle_delegate` posts no
    /// notice of its own for them.
    pub exit_walked: HashSet<String>,
    /// This run of the daemon, as `DeckOutcomes` names it.
    pub incarnation: String,
}

impl ReportState {
    pub fn from_env() -> Self {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        ReportState {
            outcomes: OutcomeStore::from_env(),
            accepted: HashMap::new(),
            units: UnitTable::new(tombstones_max_from_env().saturating_mul(16)),
            publications: HashMap::new(),
            fallback: HashMap::new(),
            tasks: TaskLedger::new(tombstones_max_from_env()),
            exit_walked: HashSet::new(),
            incarnation: bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    /// Whether a publication names `unit_id`.
    pub fn published(&self, unit_id: &str) -> bool {
        self.publications.values().any(|p| p.names_unit(unit_id))
    }
}

/// The recipient a row names, from an inbox recipient.
pub fn party(recipient: &Recipient, role: Option<String>) -> OutcomeParty {
    OutcomeParty {
        pane_id: recipient.pane_id.clone(),
        agent_id: recipient.agent_id.clone().unwrap_or_default(),
        role,
    }
}

#[cfg(test)]
mod tests {
    use spec::spec;

    use super::*;

    #[test]
    fn unknown_wire_values_decode_as_unknown() {
        let p: ReportProvenance = serde_json::from_str(r#"{"kind":"from_the_future"}"#).unwrap();
        assert_eq!(p, ReportProvenance::Unknown);
        let phase: DispatchPhase = serde_json::from_str(r#""later""#).unwrap();
        assert_eq!(phase, DispatchPhase::Unknown);
        let op: OutcomeOp = serde_json::from_str(r#""reopen""#).unwrap();
        assert_eq!(op, OutcomeOp::Unknown);
    }

    #[test]
    fn the_row_bound_evicts_the_oldest_and_counts_overflow() {
        let mut store = OutcomeStore::new(1);
        let row = |report: &str| DeckOutcome {
            outcome_id: String::new(),
            phase: OutcomePhase::WorkerReport,
            cause: OutcomeCause::RecipientExited,
            delivery: OutcomeDelivery::NeverHanded,
            label: "l".into(),
            original: OutcomeParty {
                pane_id: "p".into(),
                agent_id: "1".into(),
                role: None,
            },
            producer: OutcomeParty {
                pane_id: "q".into(),
                agent_id: "2".into(),
                role: None,
            },
            unit: None,
            orchestration: None,
            task_id: None,
            report_id: Some(report.into()),
            storage: OutcomeStorage::Artifact,
            summary: String::new(),
            created_at_ms: 0,
            revision: 0,
            operations: Vec::new(),
        };
        store.insert(row("a"), None);
        store.insert(row("b"), None);
        assert_eq!(store.overflow(), 1);
        assert!(!store.has_report("a"));
        assert!(store.has_report("b"));
    }

    fn binding(pane: &str, agent: &str, instance: Option<&str>) -> TaskBinding {
        TaskBinding {
            worker_pane: pane.to_string(),
            worker_agent: agent.to_string(),
            instance: instance.map(str::to_string),
            role: "coder".to_string(),
            orchestrator: None,
            arm_id: None,
            seq: None,
        }
    }

    /// How a report from `(pane, agent)` naming `task_id` reads against the
    /// ledger: a report names its task and is checked against the task's own
    /// binding, never inferred from whichever task is open.
    fn report(
        ledger: &mut TaskLedger,
        task_id: Option<&str>,
        pane: &str,
        agent: &str,
    ) -> &'static str {
        let Some(id) = task_id else {
            return "uncorrelated";
        };
        match ledger.lookup(id) {
            TaskLookup::Open(task)
                if task.binding.worker_pane == pane && task.binding.worker_agent == agent =>
            {
                ledger.settle(
                    id,
                    Tombstone::Reported {
                        report_id: Some(format!("r-{id}")),
                        digest: None,
                        destination: Destination::Inbox,
                    },
                );
                "reported"
            }
            TaskLookup::Settled(Tombstone::Reported { .. }) => "already reported",
            _ => "unknown",
        }
    }

    /// Scenario: An orchestrator delegates task t1, the worker reports it by
    /// id, the orchestrator delegates t2, and the worker then sends two
    /// reports with no task id, one naming t1 again, one naming an unknown
    /// task and one from another generation of the pane. Reports without an
    /// id retire nothing and t2 stays open, in the deck's task ledger.
    #[spec("inbox/attempts/015")]
    #[test]
    fn attempts_015_a_report_without_a_task_id_retires_nothing() {
        let mut tasks = TaskLedger::new(256);
        tasks.bind("t1", binding("2", "g1", None)).unwrap();
        assert_eq!(report(&mut tasks, Some("t1"), "2", "g1"), "reported");
        tasks.bind("t2", binding("2", "g1", None)).unwrap();
        assert_eq!(report(&mut tasks, None, "2", "g1"), "uncorrelated");
        assert_eq!(report(&mut tasks, None, "2", "g1"), "uncorrelated");
        assert_eq!(tasks.open_of_pane("2"), vec!["t2".to_string()]);
        assert_eq!(
            report(&mut tasks, Some("t1"), "2", "g1"),
            "already reported"
        );
        assert_eq!(report(&mut tasks, Some("t9"), "2", "g1"), "unknown");
        assert_eq!(report(&mut tasks, Some("t2"), "2", "g2"), "unknown");
        assert_eq!(report(&mut tasks, Some("t2"), "3", "g1"), "unknown");
        assert_eq!(tasks.open_of_pane("2"), vec!["t2".to_string()]);
    }

    /// Scenario: Tasks are bound, acknowledged, claimed and settled as
    /// reported, closed or superseded; an id the ledger knows is never bound
    /// again; tombstones past the per-instance bound, and past the daemon-wide
    /// bound across instances, are evicted oldest first, after which the id
    /// is unknown; an open task is never evicted.
    #[spec("orchestration/work-done/030")]
    #[test]
    fn work_done_030_the_task_ledger_and_its_tombstone_bounds() {
        let mut tasks = TaskLedger::new(2);
        tasks.bind("t1", binding("w", "a", Some("i1"))).unwrap();
        assert_eq!(tasks.bind("t1", binding("w", "a", Some("i1"))), Err(Rebind));
        let task = tasks.open_task_mut("t1").unwrap();
        assert_eq!(task.delivery, TaskDelivery::Live);
        assert_eq!(task.state, TaskState::Open { revision: 0 });
        task.delivery = TaskDelivery::Acknowledged;
        task.state = TaskState::Reporting { claim_id: 7 };
        assert!(matches!(
            tasks.lookup("t1"),
            TaskLookup::Open(Task {
                state: TaskState::Reporting { claim_id: 7 },
                delivery: TaskDelivery::Acknowledged,
                ..
            })
        ));
        tasks.settle("t1", Tombstone::Closed).unwrap();
        assert_eq!(tasks.lookup("t1"), TaskLookup::Settled(&Tombstone::Closed));
        assert_eq!(
            tasks.bind("t1", binding("w", "a", Some("i1"))),
            Err(Rebind),
            "a settled id is not bound again"
        );
        assert!(
            tasks.settle("t1", Tombstone::Closed).is_none(),
            "settled once"
        );

        // The per-instance bound: two tombstones of i1 are kept.
        for id in ["t2", "t3"] {
            tasks.bind(id, binding("w", "a", Some("i1"))).unwrap();
            tasks
                .settle(
                    id,
                    Tombstone::Superseded {
                        by: Some("t9".into()),
                    },
                )
                .unwrap();
        }
        assert_eq!(
            tasks.lookup("t1"),
            TaskLookup::Unknown,
            "the oldest of i1 is gone"
        );
        assert!(matches!(tasks.lookup("t2"), TaskLookup::Settled(_)));
        assert!(matches!(tasks.lookup("t3"), TaskLookup::Settled(_)));

        // An open task outlives any number of tombstones.
        tasks.bind("open", binding("w", "a", Some("i1"))).unwrap();
        // The daemon-wide bound: 16 times the per-instance one, across
        // instances, oldest first.
        for n in 0..40 {
            let id = format!("x{n}");
            tasks
                .bind(&id, binding("w", "a", Some(&format!("i{n}"))))
                .unwrap();
            tasks.settle(&id, Tombstone::Closed).unwrap();
        }
        assert_eq!(tasks.settled_len(), 32);
        assert_eq!(tasks.lookup("t2"), TaskLookup::Unknown);
        assert_eq!(tasks.lookup("x7"), TaskLookup::Unknown);
        assert!(matches!(tasks.lookup("x8"), TaskLookup::Settled(_)));
        assert!(matches!(tasks.lookup("open"), TaskLookup::Open(_)));
        assert_eq!(tasks.open_of_generation("w", "a"), vec!["open".to_string()]);
        assert!(tasks.open_of_generation("w", "b").is_empty());
    }

    #[test]
    fn unit_eviction_skips_claimed_and_protected_units_and_takes_the_index_along() {
        let mut table = UnitTable::new(1);
        table.index("p1", "a1", "u1");
        table.set("u1", UnitEntry::Claimed { claim_id: 1 });
        table.index("p2", "a2", "u2");
        table.set(
            "u2",
            UnitEntry::Ended {
                how: EndedHow::Exited,
            },
        );
        table.index("p3", "a3", "u3");
        table.set(
            "u3",
            UnitEntry::Ended {
                how: EndedHow::Closed,
            },
        );
        let evicted = table.evict(|unit, _| unit == "u2");
        assert_eq!(evicted, vec!["u3".to_string()]);
        assert!(table.unit_of("p3", "a3").is_none());
        assert_eq!(table.unit_of("p1", "a1"), Some("u1"));
        assert_eq!(table.unit_of("p2", "a2"), Some("u2"));
    }
}
