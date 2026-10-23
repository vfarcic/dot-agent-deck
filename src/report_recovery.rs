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
//!
//! The wire projection (`ReportProvenance`, `DeckOutcome`) is frozen here for
//! round 6. Every enum a newer deck may extend decodes an unknown value as
//! `Unknown`.

use std::collections::{HashMap, VecDeque};
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
