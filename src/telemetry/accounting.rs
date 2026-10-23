//! PRD #633, discovery milestone 4: a narrow prototype of the execution
//! record, following `docs/develop/execution-accounting.md`. It is compiled
//! for tests only (`#[cfg(test)]` in `src/telemetry/mod.rs`), so it is no
//! product API and ships in no release build; wiring it to a live run, and
//! dropping that `cfg(test)`, is the follow-up PRD's first step. Nothing here
//! locates, watches or tails a file by itself.
//!
//! * **Provenance on every value** ([`Field`]): measured, provider-reported,
//!   estimated, agent-asserted, or unavailable with a reason. Unavailable is a
//!   value the record carries, not a missing field, and a cost of `0` for
//!   replies that used tokens is unavailable ([`Unavailable::SubscriptionZero`]),
//!   never a free run.
//! * **Identifier-shaped strings only** ([`Ident`]): every string the record
//!   holds or exports is an `Ident`, whose one constructor checks its shape,
//!   so neither a normaliser nor a record built by hand outside this module
//!   can put free text in it. The shape check is not a privacy allow-list
//!   (see [`Ident`]).
//! * **Normalisers** from each agent's usage observation, as typed usage-only
//!   structs, to one shape ([`AgentUsage`]): [`from_claude`], [`from_codex`],
//!   [`from_pi`], [`from_opencode`]. No input struct has a field that could
//!   hold a prompt, a reply, a tool's input or output, or file content.
//! * **Local capture** of one agent's store ([`CodexUsageReader`]): a bounded,
//!   incremental reader of a Codex rollout at a path it is given, reading only
//!   bytes appended since its last poll, deserialising only lines that contain
//!   one of five marker words, and keeping only the model, usage, quota and
//!   turn-end fields of the record types that carry them.
//! * **Aggregation** ([`rollup`]) of runs to a work item or an orchestration,
//!   without counting a run twice, a Claude Code subagent inside its parent's
//!   total, or money of different provenance as one number.
//! * **Export mapping** ([`run_attributes`], [`work_item_attributes`]): a pure
//!   function from the record to attributes, `gen_ai.usage.*` for tokens and
//!   `deck.*` for provenance and outcomes, keyed by a closed enum
//!   ([`AccountingKey`]) as #1741's [`AttrKey`] is.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::AttrKey;
use super::ci::CiState;
use super::span::AttrValue;
use super::tracker::EndReason;

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

/// Where a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Provenance {
    /// The deck, the forge or a collector observed it.
    Measured,
    /// The provider's or the agent's own count.
    ProviderReported,
    /// Computed from other values, for example tokens times a price list.
    Estimated,
    /// The agent's claim, which nothing checked.
    AgentAsserted,
    /// Not known, for the reason the field carries.
    Unavailable,
}

impl Provenance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Provenance::Measured => "measured",
            Provenance::ProviderReported => "provider_reported",
            Provenance::Estimated => "estimated",
            Provenance::AgentAsserted => "agent_asserted",
            Provenance::Unavailable => "unavailable",
        }
    }
}

/// Why a value is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unavailable {
    /// The agent exposes no channel for this value.
    NoChannel,
    /// The agent reported a cost of `0` for replies that used tokens: a
    /// subscription or plan, whose charge for the run is not knowable.
    SubscriptionZero,
    /// The agent marked a model's price as unknown.
    UnknownPrice,
    /// The channel exists but held no value for this run.
    NotRecorded,
    /// Some of the replies behind a total had the value and some did not (or
    /// lacked it for different reasons), so their sum would be a part
    /// presented as the whole.
    Incomplete,
    /// The quota window reset or changed length during the run, so a delta
    /// would mislead.
    WindowReset,
    /// The plan named on the quota samples changed during the run.
    PlanChanged,
    /// The used share of the window went down during the run, which a
    /// simple delta cannot account for.
    QuotaDecreased,
    /// The run has not ended, so how it ended is not known yet.
    RunNotEnded,
    /// The run serves no known work item.
    NoWorkItem,
}

impl Unavailable {
    pub const fn as_str(self) -> &'static str {
        match self {
            Unavailable::NoChannel => "no_channel",
            Unavailable::SubscriptionZero => "subscription_zero",
            Unavailable::UnknownPrice => "unknown_price",
            Unavailable::NotRecorded => "not_recorded",
            Unavailable::Incomplete => "incomplete",
            Unavailable::WindowReset => "window_reset",
            Unavailable::PlanChanged => "plan_changed",
            Unavailable::QuotaDecreased => "quota_decreased",
            Unavailable::RunNotEnded => "run_not_ended",
            Unavailable::NoWorkItem => "no_work_item",
        }
    }
}

/// A value with its provenance. There is no way to hold a value without
/// saying where it came from, nor to leave a field out: an unknown value is
/// [`Field::Unavailable`] with its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field<T> {
    Measured(T),
    ProviderReported(T),
    Estimated(T),
    AgentAsserted(T),
    Unavailable(Unavailable),
}

impl<T> Field<T> {
    pub fn provenance(&self) -> Provenance {
        match self {
            Field::Measured(_) => Provenance::Measured,
            Field::ProviderReported(_) => Provenance::ProviderReported,
            Field::Estimated(_) => Provenance::Estimated,
            Field::AgentAsserted(_) => Provenance::AgentAsserted,
            Field::Unavailable(_) => Provenance::Unavailable,
        }
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Field::Measured(v)
            | Field::ProviderReported(v)
            | Field::Estimated(v)
            | Field::AgentAsserted(v) => Some(v),
            Field::Unavailable(_) => None,
        }
    }

    pub fn unavailable(&self) -> Option<Unavailable> {
        match self {
            Field::Unavailable(reason) => Some(*reason),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// A short identifier — a run id, an agent, a model, a plan, an error kind, a
/// work item — and the only string type the record holds. [`Ident::new`] is
/// its one constructor, and it refuses anything that is not ASCII letters,
/// digits and `._-:/@+`, at most [`Ident::MAX_BYTES`] bytes, so no line
/// break, quote, space or non-ASCII byte can travel through a field meant for
/// a name, whoever outside this module builds the record.
///
/// **That is a shape check, not a privacy allow-list.** An absolute path
/// (`/home/x/.ssh/id`), an email address and many credentials have the
/// shape, and pass. Which values each field may carry — the registry's agent
/// names, deck-minted run ids, known model names with a rule for unknown
/// ones, enumerated plan and error kinds — is for the follow-up that wires
/// the record to decide per field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ident(String);

impl Ident {
    pub const MAX_BYTES: usize = 128;

    pub fn new(value: &str) -> Option<Ident> {
        let ok = !value.is_empty()
            && value.len() <= Self::MAX_BYTES
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-:/@+".contains(&b));
        ok.then(|| Ident(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// Token counts in one convention, whatever the agent's own: `input` is every
/// input token, cache reads and writes included, and `cache_read` and
/// `cache_write` are the parts of it served from and written to a cache. That
/// is the meaning `gen_ai.usage.input_tokens` gives its value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
    }

    /// `self - earlier`, or `None` when any count went down.
    fn since(&self, earlier: &Tokens) -> Option<Tokens> {
        Some(Tokens {
            input: self.input.checked_sub(earlier.input)?,
            output: self.output.checked_sub(earlier.output)?,
            cache_read: self.cache_read.checked_sub(earlier.cache_read)?,
            cache_write: self.cache_write.checked_sub(earlier.cache_write)?,
        })
    }

    pub fn is_zero(&self) -> bool {
        *self == Tokens::default()
    }

    /// Whether the cache parts fit inside the input count that includes
    /// them; a total where they do not is not a count in this convention.
    fn is_consistent(&self) -> bool {
        self.cache_read.saturating_add(self.cache_write) <= self.input
    }
}

/// Money in millionths of a US dollar. An integer, because #1741's attributes
/// have no float type and a sum of floats drifts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MicroUsd(pub u64);

impl MicroUsd {
    /// `None` for a negative or non-finite amount.
    pub fn from_usd(usd: f64) -> Option<MicroUsd> {
        (usd.is_finite() && usd >= 0.0).then(|| MicroUsd((usd * 1_000_000.0).round() as u64))
    }

    fn plus(self, other: MicroUsd) -> MicroUsd {
        MicroUsd(self.0.saturating_add(other.0))
    }
}

/// One model's usage within a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsage {
    /// `None` when the usage cannot be tied to a model: none was named, the
    /// name was not an [`Ident`], or [`MAX_MODELS`] models were already
    /// tracked.
    pub model: Option<Ident>,
    pub tokens: Field<Tokens>,
    pub cost: Field<MicroUsd>,
}

/// How much of a plan's window a run took, in basis points (hundredths of a
/// percent) of that window. Concurrent runs share one window, so for a run
/// that overlapped another it is an upper bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaDelta {
    pub plan: Option<Ident>,
    pub window_minutes: u64,
    /// When the window resets, in seconds since the Unix epoch: it names the
    /// window instance, so two runs' deltas are comparable only when it is
    /// the same.
    pub resets_at: i64,
    pub from_bp: u32,
    pub to_bp: u32,
}

impl QuotaDelta {
    pub fn consumed_bp(&self) -> u32 {
        self.to_bp.saturating_sub(self.from_bp)
    }
}

/// How a run ended (the doc's "run termination").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RunTermination {
    Completed,
    Failed,
    /// By a person, or by the deck stopping or restarting the run.
    Interrupted,
    /// Reserved: the deck sets no run time limit today, so nothing produces it.
    TimedOut,
    Unknown,
}

impl RunTermination {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunTermination::Completed => "completed",
            RunTermination::Failed => "failed",
            RunTermination::Interrupted => "interrupted",
            RunTermination::TimedOut => "timed_out",
            RunTermination::Unknown => "unknown",
        }
    }
}

/// A run's termination with the kind of its failure, when the agent named one
/// (`error.type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Termination {
    pub outcome: RunTermination,
    pub error_type: Option<Ident>,
}

/// What an agent's own record says about its last turn. It is the last
/// turn's status, not the run's end: an interactive agent completes a turn
/// and then waits for the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTurnEnd {
    Completed,
    /// `kind` is the agent's error identifier, never its message.
    Failed {
        kind: Option<Ident>,
    },
    Interrupted,
    /// The record ends inside a turn, for example on a reply that asked for a
    /// tool.
    MidTurn,
    /// The record's last turn ends in a way this prototype does not read, or
    /// names no end.
    Unknown,
}

impl AgentTurnEnd {
    pub const fn as_str(&self) -> &'static str {
        match self {
            AgentTurnEnd::Completed => "completed",
            AgentTurnEnd::Failed { .. } => "failed",
            AgentTurnEnd::Interrupted => "interrupted",
            AgentTurnEnd::MidTurn => "mid_turn",
            AgentTurnEnd::Unknown => "unknown",
        }
    }
}

/// A run's termination from what the deck saw (`end`, `None` while the run is
/// open) and what the agent's own record says about its last turn.
///
/// Until the deck reports an end the run has not terminated, whatever its
/// last turn says: an interactive agent that completed a turn is waiting, not
/// finished. Once it has ended, the agent's word decides a failure, an
/// interruption and a completion (provider-reported). A record that ends
/// mid-turn is decided by the deck: a run it closed or shut down was
/// interrupted, any other end is unknown (measured). A last turn the record
/// does not name, or no agent record, leaves only that the run ended:
/// unknown.
pub fn run_termination(end: Option<EndReason>, agent: Option<&AgentTurnEnd>) -> Field<Termination> {
    let Some(end) = end else {
        return Field::Unavailable(Unavailable::RunNotEnded);
    };
    let of = |outcome| Termination {
        outcome,
        error_type: None,
    };
    match agent {
        Some(AgentTurnEnd::Failed { kind }) => Field::ProviderReported(Termination {
            outcome: RunTermination::Failed,
            error_type: kind.clone(),
        }),
        Some(AgentTurnEnd::Interrupted) => Field::ProviderReported(of(RunTermination::Interrupted)),
        Some(AgentTurnEnd::Completed) => Field::ProviderReported(of(RunTermination::Completed)),
        Some(AgentTurnEnd::MidTurn)
            if matches!(end, EndReason::Closed | EndReason::DaemonShutdown) =>
        {
            Field::Measured(of(RunTermination::Interrupted))
        }
        Some(AgentTurnEnd::MidTurn | AgentTurnEnd::Unknown) | None => {
            Field::Measured(of(RunTermination::Unknown))
        }
    }
}

/// A work item's disposition (the doc's revised taxonomy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Merged,
    /// Closed with a successor named.
    Superseded,
    /// Closed without a successor.
    Rejected,
    /// The issue closed as done with no change.
    ResolvedWithoutChange,
    /// The issue closed as not planned.
    Deferred,
    /// No activity and no decision. Needs a time rule, so no forge state maps
    /// to it here.
    Abandoned,
    Open,
}

impl Disposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Disposition::Merged => "merged",
            Disposition::Superseded => "superseded",
            Disposition::Rejected => "rejected",
            Disposition::ResolvedWithoutChange => "resolved_without_change",
            Disposition::Deferred => "deferred",
            Disposition::Abandoned => "abandoned",
            Disposition::Open => "open",
        }
    }
}

/// A work item's pull request, as the forge reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Merged,
    Closed { successor: bool },
}

/// A work item's issue, as the forge reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    ClosedCompleted,
    ClosedNotPlanned,
}

/// The disposition from forge state (measured). A pull request decides when
/// there is one; otherwise the issue does.
pub fn work_disposition(
    pr: Option<PullRequestState>,
    issue: Option<IssueState>,
) -> Field<Disposition> {
    let disposition = match (pr, issue) {
        (Some(PullRequestState::Merged), _) => Disposition::Merged,
        (Some(PullRequestState::Closed { successor: true }), _) => Disposition::Superseded,
        (Some(PullRequestState::Closed { successor: false }), _) => Disposition::Rejected,
        (Some(PullRequestState::Open), _) | (None, Some(IssueState::Open)) => Disposition::Open,
        (None, Some(IssueState::ClosedCompleted)) => Disposition::ResolvedWithoutChange,
        (None, Some(IssueState::ClosedNotPlanned)) => Disposition::Deferred,
        (None, None) => return Field::Unavailable(Unavailable::NoWorkItem),
    };
    Field::Measured(disposition)
}

/// What the checks on a work item's pull request showed: the observed CI
/// rollup, **not verification**. #1741's CI state aggregates every check the
/// forge lists, optional ones included, and reads a rollup of only skipped
/// checks as passing; it names no commit. Verification proper (the required
/// checks passed on the head that merged) needs the required-check list and
/// the commit identity, and is deferred to the follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedChecks {
    Passing,
    Failing,
    Pending,
}

impl ObservedChecks {
    pub const fn as_str(self) -> &'static str {
        match self {
            ObservedChecks::Passing => "passing",
            ObservedChecks::Failing => "failing",
            ObservedChecks::Pending => "pending",
        }
    }
}

/// The observed checks from the CI rollup on the item's pull request
/// (#1741's `agent.ci`, measured), else from the agent's own claim that its
/// checks passed (agent-asserted), else unavailable.
pub fn observed_checks(
    rollup: Option<CiState>,
    agent_claim: Option<bool>,
) -> Field<ObservedChecks> {
    match (rollup, agent_claim) {
        (Some(CiState::Passing), _) => Field::Measured(ObservedChecks::Passing),
        (Some(CiState::Failing), _) => Field::Measured(ObservedChecks::Failing),
        (Some(CiState::Pending), _) => Field::Measured(ObservedChecks::Pending),
        (None, Some(true)) => Field::AgentAsserted(ObservedChecks::Passing),
        (None, Some(false)) => Field::AgentAsserted(ObservedChecks::Failing),
        (None, None) => Field::Unavailable(Unavailable::NoChannel),
    }
}

/// A work item's outcome fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemRecord {
    pub disposition: Field<Disposition>,
    pub checks: Field<ObservedChecks>,
}

// ---------------------------------------------------------------------------
// Normalisers
// ---------------------------------------------------------------------------

/// The most distinct named models one run tracks. Usage of any further model
/// is credited to `None` (not tied to a model), so the state a run holds is
/// bounded whatever its input names.
pub const MAX_MODELS: usize = 32;

/// The key `model` is counted under in `by_model`: itself, or `None` once
/// [`MAX_MODELS`] other named models are there.
fn capped<V>(by_model: &BTreeMap<Option<Ident>, V>, model: Option<Ident>) -> Option<Ident> {
    let named = by_model.len() - usize::from(by_model.contains_key(&None));
    if named >= MAX_MODELS && !by_model.contains_key(&model) {
        None
    } else {
        model
    }
}

/// One agent's usage for one run, in the record's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentUsage {
    pub models: Vec<ModelUsage>,
    pub quota: Field<QuotaDelta>,
    pub last_turn: Option<AgentTurnEnd>,
}

/// The one entry of a run whose usage is not known at all.
fn usage_unavailable(reason: Unavailable) -> Vec<ModelUsage> {
    vec![ModelUsage {
        model: None,
        tokens: Field::Unavailable(reason),
        cost: Field::Unavailable(reason),
    }]
}

/// An agent's own cost figure for one reply's `tokens`, as `estimated`: a `0`
/// for a reply that used tokens is a subscription's, not a free reply's.
fn agent_cost(cost: Option<MicroUsd>, tokens: &Tokens) -> Field<MicroUsd> {
    match cost {
        None => Field::Unavailable(Unavailable::NotRecorded),
        Some(MicroUsd(0)) if !tokens.is_zero() => Field::Unavailable(Unavailable::SubscriptionZero),
        Some(cost) => Field::Estimated(cost),
    }
}

/// Two costs, each already classified, as one. Values of one provenance add;
/// a reply with zero tokens and an explicitly reported zero cost adds
/// nothing; any other mix — a priced reply with an unpriced one, a reply
/// whose cost was not recorded at all, or two reasons for unavailable — is
/// [`Unavailable::Incomplete`], never the priced part alone.
fn add_cost(sum: Field<MicroUsd>, next: Field<MicroUsd>) -> Field<MicroUsd> {
    match (sum, next) {
        (Field::Estimated(MicroUsd(0)), other) | (other, Field::Estimated(MicroUsd(0))) => other,
        (Field::Estimated(a), Field::Estimated(b)) => Field::Estimated(a.plus(b)),
        (Field::ProviderReported(a), Field::ProviderReported(b)) => {
            Field::ProviderReported(a.plus(b))
        }
        (Field::Unavailable(a), Field::Unavailable(b)) if a == b => Field::Unavailable(a),
        _ => Field::Unavailable(Unavailable::Incomplete),
    }
}

/// Two token counts, each already classified, as one: provider-reported
/// counts add, the same reason for unavailable stays, and any other mix is
/// [`Unavailable::Incomplete`].
fn add_tokens(sum: Field<Tokens>, next: Field<Tokens>) -> Field<Tokens> {
    match (sum, next) {
        (Field::ProviderReported(mut a), Field::ProviderReported(b)) => {
            a.add(&b);
            Field::ProviderReported(a)
        }
        (Field::Unavailable(a), Field::Unavailable(b)) if a == b => Field::Unavailable(a),
        _ => Field::Unavailable(Unavailable::Incomplete),
    }
}

/// Usage per model from per-reply (or per-model) entries, each with its
/// tokens and cost already classified: tokens combined by [`add_tokens`],
/// costs by [`add_cost`], at most [`MAX_MODELS`] named models.
fn per_model(
    entries: impl IntoIterator<Item = (Option<Ident>, Field<Tokens>, Field<MicroUsd>)>,
) -> Vec<ModelUsage> {
    let mut by_model: BTreeMap<Option<Ident>, (Field<Tokens>, Field<MicroUsd>)> = BTreeMap::new();
    for (model, tokens, cost) in entries {
        match by_model.entry(capped(&by_model, model)) {
            Entry::Vacant(entry) => {
                entry.insert((tokens, cost));
            }
            Entry::Occupied(mut entry) => {
                let (sum, sum_cost) = entry.get_mut();
                *sum = add_tokens(*sum, tokens);
                *sum_cost = add_cost(*sum_cost, cost);
            }
        }
    }
    if by_model.is_empty() {
        return usage_unavailable(Unavailable::NotRecorded);
    }
    by_model
        .into_iter()
        .map(|(model, (tokens, cost))| ModelUsage {
            model,
            tokens,
            cost,
        })
        .collect()
}

/// Tokens from an agent that counts input without the cache.
fn input_plus_cache(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Tokens {
    Tokens {
        input: input.saturating_add(cache_read).saturating_add(cache_write),
        output,
        cache_read,
        cache_write,
    }
}

/// One model's entry in Claude Code's own session total (`cost-state`'s
/// `modelUsage`). Its input count excludes cache reads and writes, as the
/// Anthropic API's does.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeModelUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cost_usd: Option<f64>,
}

/// Claude Code's own session total. It already includes the session's
/// subagents, which is why a subagent's run is recorded as included in its
/// parent ([`RunIdentity::usage_included_in`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeSessionUsage {
    pub models: Vec<ClaudeModelUsage>,
    pub has_unknown_model_cost: bool,
    pub last_turn: Option<AgentTurnEnd>,
}

/// Claude Code: tokens provider-reported from its own total, cost as Claude
/// Code's own estimate, which its documentation calls an approximation. A
/// session with no total (`None`) has its usage unavailable, not zero.
pub fn from_claude(session: Option<&ClaudeSessionUsage>) -> AgentUsage {
    let Some(session) = session else {
        return AgentUsage {
            models: usage_unavailable(Unavailable::NotRecorded),
            quota: Field::Unavailable(Unavailable::NoChannel),
            last_turn: None,
        };
    };
    let models = per_model(session.models.iter().map(|m| {
        let tokens = input_plus_cache(
            m.input_tokens,
            m.output_tokens,
            m.cache_read_input_tokens,
            m.cache_creation_input_tokens,
        );
        let cost = if session.has_unknown_model_cost {
            Field::Unavailable(Unavailable::UnknownPrice)
        } else {
            agent_cost(m.cost_usd.and_then(MicroUsd::from_usd), &tokens)
        };
        (Ident::new(&m.model), Field::ProviderReported(tokens), cost)
    }));
    AgentUsage {
        models,
        quota: Field::Unavailable(Unavailable::NoChannel),
        last_turn: session.last_turn.clone(),
    }
}

/// Codex: tokens provider-reported, by model, from its rollout's running
/// total, or [`Unavailable::Incomplete`] once that total went down
/// ([`CodexRolloutSummary::tokens_incomplete`]); no cost (the rollout carries
/// none); the quota delta of its primary rate-limit window, unavailable when
/// the window, its length or the plan changed, or its used share went down,
/// during the run. The models pass through the same cap as every other
/// normaliser's, so a summary built by hand with more than [`MAX_MODELS`]
/// named models is bounded too.
///
/// Quota is assumed to be one account's: the samples name a plan, not an
/// account, so deltas from several machines or accounts on the same plan and
/// window are indistinguishable here.
pub fn from_codex(summary: &CodexRolloutSummary) -> AgentUsage {
    let models = per_model(summary.tokens_by_model.iter().map(|(model, tokens)| {
        let tokens = if summary.tokens_incomplete {
            Field::Unavailable(Unavailable::Incomplete)
        } else {
            Field::ProviderReported(*tokens)
        };
        (
            model.clone(),
            tokens,
            Field::Unavailable(Unavailable::NoChannel),
        )
    }));
    let quota = match (&summary.quota_first, &summary.quota_last) {
        (Some(first), Some(last)) => {
            match summary.quota_break.or_else(|| quota_break(first, last)) {
                Some(reason) => Field::Unavailable(reason),
                None => Field::ProviderReported(QuotaDelta {
                    plan: last.plan.clone(),
                    window_minutes: last.window_minutes,
                    resets_at: last.resets_at,
                    from_bp: first.used_bp,
                    to_bp: last.used_bp,
                }),
            }
        }
        _ => Field::Unavailable(Unavailable::NotRecorded),
    };
    AgentUsage {
        models,
        quota,
        last_turn: summary.last_turn.clone(),
    }
}

/// Why `next` cannot follow `prev` in one simple quota delta, if it cannot.
fn quota_break(prev: &CodexRateSample, next: &CodexRateSample) -> Option<Unavailable> {
    if prev.resets_at != next.resets_at || prev.window_minutes != next.window_minutes {
        Some(Unavailable::WindowReset)
    } else if prev.plan != next.plan {
        Some(Unavailable::PlanChanged)
    } else if next.used_bp < prev.used_bp {
        Some(Unavailable::QuotaDecreased)
    } else {
        None
    }
}

/// How a Pi reply stopped (`stopReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiStopReason {
    Stop,
    ToolUse,
    Length,
    Error,
    Aborted,
}

/// One Pi reply's usage (`message.usage` in its session file, or the deck
/// extension's `pi.model_usage`). Its input count excludes cache reads and
/// writes.
#[derive(Debug, Clone, PartialEq)]
pub struct PiReplyUsage {
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Pi's own estimate from its model catalogue (`usage.cost.total`).
    pub cost_total_usd: Option<f64>,
    pub stop_reason: Option<PiStopReason>,
}

/// Pi: tokens provider-reported, summed per model; each reply's cost
/// classified as Pi's estimate (or unavailable) before it is summed. The last
/// reply names the last turn's end: a reply that asked for a tool leaves the
/// turn open, and one with no stop reason leaves it unknown.
pub fn from_pi(replies: &[PiReplyUsage]) -> AgentUsage {
    let models = per_model(replies.iter().map(|reply| {
        let tokens = input_plus_cache(
            reply.input,
            reply.output,
            reply.cache_read,
            reply.cache_write,
        );
        let cost = agent_cost(reply.cost_total_usd.and_then(MicroUsd::from_usd), &tokens);
        (
            Ident::new(&reply.model),
            Field::ProviderReported(tokens),
            cost,
        )
    }));
    let last_turn = replies.last().map(|r| match r.stop_reason {
        Some(PiStopReason::Stop | PiStopReason::Length) => AgentTurnEnd::Completed,
        Some(PiStopReason::ToolUse) => AgentTurnEnd::MidTurn,
        Some(PiStopReason::Error) => AgentTurnEnd::Failed { kind: None },
        Some(PiStopReason::Aborted) => AgentTurnEnd::Interrupted,
        None => AgentTurnEnd::Unknown,
    });
    AgentUsage {
        models,
        quota: Field::Unavailable(Unavailable::NoChannel),
        last_turn,
    }
}

/// One OpenCode message: its model, and its usage when the provider
/// returned one.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenCodeMessageUsage {
    pub model: String,
    /// `None` when the message holds no usage from the provider. OpenCode
    /// initialises a message's counters and cost to 0 and keeps them when the
    /// request fails or the reply never completes, so those zeros are not a
    /// count, and whoever extracts the message passes `None` for them; a
    /// completed message that really used no tokens is `Some` of zeros.
    pub usage: Option<OpenCodeUsage>,
}

/// One OpenCode message's usage (its database's per-message `tokens` and
/// `cost`). Its input count excludes cache reads and writes.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenCodeUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
}

/// OpenCode: tokens provider-reported per model; each message's cost
/// classified as OpenCode's own figure before it is summed, unavailable when
/// it is `0` for a message that used tokens, which is what its subscription
/// providers report. A message with no usage has its tokens and cost
/// [`Unavailable::NotRecorded`], never zero, so a model whose messages mix
/// the two has them [`Unavailable::Incomplete`].
pub fn from_opencode(messages: &[OpenCodeMessageUsage]) -> AgentUsage {
    let models = per_model(messages.iter().map(|m| {
        let model = Ident::new(&m.model);
        let Some(u) = &m.usage else {
            let none = Unavailable::NotRecorded;
            return (model, Field::Unavailable(none), Field::Unavailable(none));
        };
        let tokens = input_plus_cache(u.input, u.output, u.cache_read, u.cache_write);
        let cost = agent_cost(MicroUsd::from_usd(u.cost_usd), &tokens);
        (model, Field::ProviderReported(tokens), cost)
    }));
    AgentUsage {
        models,
        quota: Field::Unavailable(Unavailable::NoChannel),
        last_turn: None,
    }
}

// ---------------------------------------------------------------------------
// The run record
// ---------------------------------------------------------------------------

/// What the deck knows about a run without asking the agent (#1741).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    pub run_id: Ident,
    pub agent: Ident,
    /// The run whose pane started this one, if any.
    pub parent_run_id: Option<Ident>,
    /// The run whose own total already contains this run's usage — a Claude
    /// Code subagent's parent session. Its usage is then counted there and
    /// never again here.
    pub usage_included_in: Option<Ident>,
    pub orchestration_id: Option<Ident>,
    /// The issue or pull request the run serves, when known (#628).
    pub work_item: Option<Ident>,
}

impl RunIdentity {
    pub fn new(run_id: Ident, agent: Ident) -> Self {
        RunIdentity {
            run_id,
            agent,
            parent_run_id: None,
            usage_included_in: None,
            orchestration_id: None,
            work_item: None,
        }
    }
}

/// One run's accounting record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub identity: RunIdentity,
    pub models: Vec<ModelUsage>,
    pub quota: Field<QuotaDelta>,
    /// How the run ended; unavailable while it is open.
    pub termination: Field<Termination>,
    /// The agent's last turn, kept apart from the termination, so an open
    /// run still says where its agent is.
    pub last_turn: Field<AgentTurnEnd>,
}

impl RunRecord {
    /// The record from the deck's view of the run and the agent's usage.
    pub fn new(identity: RunIdentity, usage: AgentUsage, end: Option<EndReason>) -> Self {
        let termination = run_termination(end, usage.last_turn.as_ref());
        let last_turn = match usage.last_turn {
            Some(turn) => Field::ProviderReported(turn),
            None => Field::Unavailable(Unavailable::NotRecorded),
        };
        RunRecord {
            identity,
            models: usage.models,
            quota: usage.quota,
            termination,
            last_turn,
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// One plan window instance, the unit quota rolls up in. It names no
/// account, so a set of runs is assumed to be one account's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuotaKey {
    pub agent: Ident,
    pub plan: Option<Ident>,
    pub window_minutes: u64,
    pub resets_at: i64,
}

/// Totals over a set of runs. Money is kept per provenance and never added
/// across them; quota is kept per plan window and never turned into money.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rollup {
    /// Distinct runs, by run id.
    pub runs: usize,
    /// Runs whose usage was already in another run of the set.
    pub runs_included_elsewhere: usize,
    pub tokens: BTreeMap<(Option<Ident>, Provenance), Tokens>,
    pub tokens_unavailable: usize,
    pub cost: BTreeMap<Provenance, MicroUsd>,
    pub cost_unavailable: usize,
    /// Basis points of each window consumed while any run of the set was in
    /// it: the union of the runs' ranges, so overlapping runs are not counted
    /// twice.
    pub quota_bp: BTreeMap<QuotaKey, u32>,
    pub quota_unavailable: usize,
    pub terminations: BTreeMap<RunTermination, usize>,
    pub terminations_unavailable: usize,
}

/// Each run id's last record, in the order run ids were first seen: a later
/// snapshot of a running total supersedes an earlier one, membership in a
/// work item or orchestration included.
fn latest<'a>(runs: impl IntoIterator<Item = &'a RunRecord>) -> Vec<&'a RunRecord> {
    let mut order: Vec<&RunRecord> = Vec::new();
    let mut index: HashMap<&Ident, usize> = HashMap::new();
    for run in runs {
        match index.get(&run.identity.run_id) {
            Some(&i) => order[i] = run,
            None => {
                index.insert(&run.identity.run_id, order.len());
                order.push(run);
            }
        }
    }
    order
}

/// Roll `runs` up. A run id seen more than once is counted once, as its last
/// record. Distinct run ids are distinct attempts and are all counted, so a
/// retried work item costs what both attempts cost. A run whose usage is
/// included in another run of the set counts as a run and a termination but
/// adds no usage.
pub fn rollup<'a>(runs: impl IntoIterator<Item = &'a RunRecord>) -> Rollup {
    rollup_distinct(&latest(runs))
}

/// The runs that served `work_item`, rolled up. Snapshots are collapsed to
/// each run's last before membership is decided, so a run whose work item was
/// corrected counts under the corrected one only.
pub fn rollup_work_item(runs: &[RunRecord], work_item: &str) -> Rollup {
    let selected: Vec<&RunRecord> = latest(runs)
        .into_iter()
        .filter(|r| r.identity.work_item.as_ref().map(Ident::as_str) == Some(work_item))
        .collect();
    rollup_distinct(&selected)
}

/// The runs of `orchestration_id`, rolled up, with membership decided on each
/// run's last snapshot as in [`rollup_work_item`].
pub fn rollup_orchestration(runs: &[RunRecord], orchestration_id: &str) -> Rollup {
    let selected: Vec<&RunRecord> = latest(runs)
        .into_iter()
        .filter(|r| {
            r.identity.orchestration_id.as_ref().map(Ident::as_str) == Some(orchestration_id)
        })
        .collect();
    rollup_distinct(&selected)
}

/// `runs`, already one record per run id, rolled up.
fn rollup_distinct(runs: &[&RunRecord]) -> Rollup {
    let ids: HashSet<&Ident> = runs.iter().map(|r| &r.identity.run_id).collect();
    let mut out = Rollup {
        runs: runs.len(),
        ..Rollup::default()
    };
    let mut quota_ranges: BTreeMap<QuotaKey, Vec<(u32, u32)>> = BTreeMap::new();
    for run in runs {
        match run.termination.value() {
            Some(t) => *out.terminations.entry(t.outcome).or_default() += 1,
            None => out.terminations_unavailable += 1,
        }
        if run
            .identity
            .usage_included_in
            .as_ref()
            .is_some_and(|parent| ids.contains(parent))
        {
            out.runs_included_elsewhere += 1;
            continue;
        }
        for usage in &run.models {
            match usage.tokens.value() {
                Some(tokens) => out
                    .tokens
                    .entry((usage.model.clone(), usage.tokens.provenance()))
                    .or_default()
                    .add(tokens),
                None => out.tokens_unavailable += 1,
            }
            match usage.cost.value() {
                Some(cost) => {
                    let sum = out.cost.entry(usage.cost.provenance()).or_default();
                    *sum = sum.plus(*cost);
                }
                None => out.cost_unavailable += 1,
            }
        }
        match run.quota.value() {
            Some(q) => quota_ranges
                .entry(QuotaKey {
                    agent: run.identity.agent.clone(),
                    plan: q.plan.clone(),
                    window_minutes: q.window_minutes,
                    resets_at: q.resets_at,
                })
                .or_default()
                .push((q.from_bp, q.to_bp.max(q.from_bp))),
            None => out.quota_unavailable += 1,
        }
    }
    for (key, mut ranges) in quota_ranges {
        ranges.sort_unstable();
        let mut total = 0u32;
        let mut current: Option<(u32, u32)> = None;
        for (from, to) in ranges {
            current = match current {
                Some((a, b)) if from <= b => Some((a, b.max(to))),
                Some((a, b)) => {
                    total += b - a;
                    Some((from, to))
                }
                None => Some((from, to)),
            };
        }
        if let Some((a, b)) = current {
            total += b - a;
        }
        out.quota_bp.insert(key, total);
    }
    out
}

// ---------------------------------------------------------------------------
// Local capture: a Codex rollout
// ---------------------------------------------------------------------------

/// The usage a Codex rollout records, as [`CodexUsageReader`] accumulates it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexRolloutSummary {
    /// Each `token_count`'s growth of the running total, credited to the model
    /// of the turn it came in, so the sum over models is the rollout's own
    /// total.
    pub tokens_by_model: BTreeMap<Option<Ident>, Tokens>,
    /// A running total went down in some count. The reader accepts nothing
    /// in the rollout as saying whether its counters restarted or the total
    /// is wrong, so the run's token total is not known: [`from_codex`]
    /// reports it as [`Unavailable::Incomplete`].
    pub tokens_incomplete: bool,
    pub quota_first: Option<CodexRateSample>,
    pub quota_last: Option<CodexRateSample>,
    /// The first reason two consecutive quota samples could not be one simple
    /// delta ([`quota_break`]), if any.
    pub quota_break: Option<Unavailable>,
    pub last_turn: Option<AgentTurnEnd>,
    current_model: Option<Ident>,
    last_total: Option<Tokens>,
}

/// One sample of a Codex plan's primary rate-limit window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRateSample {
    pub plan: Option<Ident>,
    pub used_bp: u32,
    pub window_minutes: u64,
    pub resets_at: i64,
}

/// The longest line the reader buffers across reads; a longer one is skipped
/// through its newline without being parsed. On the development machine the
/// longest line of a kind the reader parses was about 10 KiB (a
/// `turn_context`); the long lines are tool output.
pub const MAX_LINE_BYTES: usize = 256 * 1024;

/// How many bytes one poll reads at most, unless the reader is built with
/// another budget: a large backlog is drained over several polls.
pub const DEFAULT_POLL_BUDGET: u64 = 4 * 1024 * 1024;

/// How much the reader reads per `read` call, into a buffer on the stack, so
/// a poll allocates nothing for the read itself.
const READ_CHUNK: usize = 16 * 1024;

/// What a reader has done, for measuring it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderStats {
    pub bytes_read: u64,
    pub lines: u64,
    /// Lines that passed the cheap pre-filter and were deserialised.
    pub parsed: u64,
    /// Lines that passed the pre-filter but did not deserialise.
    pub malformed: u64,
    pub overlong: u64,
    /// Times the file was truncated or replaced and reading started over.
    pub restarts: u64,
    /// Running totals skipped without moving the baseline: missing a primary
    /// count, missing a cache count that was not zero before, or not a count
    /// at all (cache parts larger than the input that includes them).
    pub unusable_totals: u64,
    /// Usable running totals lower than the baseline in some count. Each one
    /// makes the run's token total incomplete and leaves the baseline where
    /// it was.
    pub regressions: u64,
    /// Token deltas credited to no model because [`MAX_MODELS`] models were
    /// already tracked.
    pub models_overflowed: u64,
    /// Bytes the file held past the reader's offset when the last poll ended.
    pub backlog: u64,
    /// Polls that stopped at their byte budget with bytes left to read.
    pub polls_cut_short: u64,
}

/// An incremental reader of one Codex rollout, at the path it is given.
///
/// Each [`poll`](Self::poll) reads only bytes appended since the last one,
/// at most its budget ([`DEFAULT_POLL_BUDGET`]) and never past the file's
/// length when the poll began, so one call is bounded however large the
/// backlog is or however fast the file grows; the rest waits for the next
/// poll ([`ReaderStats::backlog`]). That is a bound on bytes and parsing
/// work per poll, not on wall-clock time: a read from a slow disk still takes
/// as long as the disk does. A line is kept across polls until its newline
/// arrives (at most [`MAX_LINE_BYTES`]), then dropped.
///
/// Any complete line that contains one of five quoted marker words
/// ([`line_is_candidate`]) is deserialised, wherever in the line the marker
/// is, so a content line with a string that is exactly a marker word is
/// deserialised too. What is kept is then decided by the parsed record's
/// type and its payload's type: only `turn_context`, `token_count`,
/// `task_started`, `task_complete` and `turn_aborted` update the summary,
/// and only with the model, token counts, quota and turn-end kind, the
/// fields the narrow structs name. So a prompt, reply, tool output or file
/// content is scanned by the parser and, while its line is incomplete,
/// buffered, but kept in nothing the reader returns, and its `Debug` shows
/// neither the buffer nor the path.
///
/// On Unix the path is opened with `O_NOFOLLOW`, which refuses a symlink in
/// its final component only: a symlinked parent directory is followed, so
/// this is no confinement to a trusted root (that policy is #1858's to
/// decide before the reader is wired). It is opened with `O_NONBLOCK` so a
/// FIFO with no writer cannot stall the open, and anything but a regular
/// file is refused, from the opened descriptor's own metadata, before a byte
/// is read. Elsewhere it is a plain open, and the regular-file check follows
/// it.
///
/// A file that gets shorter, or whose path now names another file (Unix),
/// is read again from the start with the summary reset: that is a new rollout
/// as far as the reader can tell. A file truncated and regrown past the
/// reader's offset between two polls, keeping its inode, is not noticed: the
/// reader carries on from its offset into the new content.
pub struct CodexUsageReader {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    offset: u64,
    partial: Vec<u8>,
    skipping: bool,
    poll_budget: u64,
    summary: CodexRolloutSummary,
    stats: ReaderStats,
}

impl fmt::Debug for CodexUsageReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexUsageReader")
            .field("offset", &self.offset)
            .field("partial_bytes", &self.partial.len())
            .field("skipping", &self.skipping)
            .field("poll_budget", &self.poll_budget)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl CodexUsageReader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        CodexUsageReader {
            path: path.into(),
            identity: None,
            offset: 0,
            partial: Vec::new(),
            skipping: false,
            poll_budget: DEFAULT_POLL_BUDGET,
            summary: CodexRolloutSummary::default(),
            stats: ReaderStats::default(),
        }
    }

    /// The same reader with a byte budget per poll other than
    /// [`DEFAULT_POLL_BUDGET`] (at least one byte).
    pub fn with_poll_budget(mut self, bytes: u64) -> Self {
        self.poll_budget = bytes.max(1);
        self
    }

    pub fn summary(&self) -> &CodexRolloutSummary {
        &self.summary
    }

    pub fn stats(&self) -> ReaderStats {
        self.stats
    }

    /// Read what was appended since the last poll, within the budget.
    pub fn poll(&mut self) -> io::Result<()> {
        let mut file = open_rollout(&self.path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let identity = file_identity(&meta);
        let end = meta.len();
        if (self.identity.is_some() && identity != self.identity) || end < self.offset {
            self.restart();
        }
        self.identity = identity;
        let stop = end.min(self.offset.saturating_add(self.poll_budget));
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = [0u8; READ_CHUNK];
        while self.offset < stop {
            let want =
                usize::try_from(stop - self.offset).map_or(READ_CHUNK, |n| n.min(READ_CHUNK));
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            self.offset += n as u64;
            self.stats.bytes_read += n as u64;
            self.feed(&buf[..n]);
        }
        self.stats.backlog = end.saturating_sub(self.offset);
        if self.stats.backlog > 0 {
            self.stats.polls_cut_short += 1;
        }
        Ok(())
    }

    fn restart(&mut self) {
        self.offset = 0;
        self.partial.clear();
        self.skipping = false;
        self.summary = CodexRolloutSummary::default();
        self.stats.restarts += 1;
    }

    fn feed(&mut self, mut data: &[u8]) {
        while let Some(nl) = data.iter().position(|b| *b == b'\n') {
            let (head, rest) = data.split_at(nl);
            data = &rest[1..];
            if self.skipping {
                self.skipping = false;
                continue;
            }
            if self.partial.is_empty() {
                self.line(head);
            } else if self.partial.len() + head.len() > MAX_LINE_BYTES {
                self.partial.clear();
                self.stats.overlong += 1;
                self.stats.lines += 1;
            } else {
                let mut line = std::mem::take(&mut self.partial);
                line.extend_from_slice(head);
                self.line(&line);
                line.clear();
                self.partial = line;
            }
        }
        if self.skipping || data.is_empty() {
            return;
        }
        if self.partial.len() + data.len() > MAX_LINE_BYTES {
            self.partial.clear();
            self.skipping = true;
            self.stats.overlong += 1;
            self.stats.lines += 1;
        } else {
            self.partial.extend_from_slice(data);
        }
    }

    fn line(&mut self, line: &[u8]) {
        self.stats.lines += 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line_is_candidate(line) {
            return;
        }
        match serde_json::from_slice::<NarrowRecord>(line) {
            Ok(record) => {
                self.stats.parsed += 1;
                self.summary.observe(record, &mut self.stats);
            }
            Err(_) => self.stats.malformed += 1,
        }
    }
}

/// Open `path` for reading, refusing a symlink in its final component only
/// (parent directories are followed), and without blocking on the open (a
/// FIFO with no writer would otherwise block it). `O_NONBLOCK` does not make
/// reading a regular file any faster on a slow disk.
#[cfg(unix)]
fn open_rollout(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_rollout(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

const CANDIDATE_MARKERS: [&[u8]; 5] = [
    b"\"turn_context\"",
    b"\"token_count\"",
    b"\"task_started\"",
    b"\"task_complete\"",
    b"\"turn_aborted\"",
];

/// Whether a line could be a record the reader uses: the cheap pre-filter
/// that decides which lines are deserialised at all. It matches a quoted
/// marker anywhere in the line, not only as the record's type, so it lets
/// through some content lines, which [`CodexRolloutSummary::observe`] then
/// ignores by type.
fn line_is_candidate(line: &[u8]) -> bool {
    CANDIDATE_MARKERS
        .iter()
        .any(|m| line.windows(m.len()).any(|w| w == *m))
}

impl CodexRolloutSummary {
    fn observe(&mut self, record: NarrowRecord, stats: &mut ReaderStats) {
        let payload = record.payload.unwrap_or_default();
        match (record.kind.0.as_deref(), payload.kind.0.as_deref()) {
            (Some("turn_context"), _) => {
                self.current_model = payload.model.0.as_deref().and_then(Ident::new);
            }
            (Some("event_msg"), Some("token_count")) => {
                if let Some(total) = payload.info.and_then(|i| i.total_token_usage) {
                    self.observe_total(&total, stats);
                }
                if let Some(sample) = payload.rate_limits.and_then(NarrowRateLimits::sample) {
                    if let Some(last) = &self.quota_last {
                        self.quota_break = self.quota_break.or_else(|| quota_break(last, &sample));
                    }
                    self.quota_first.get_or_insert_with(|| sample.clone());
                    self.quota_last = Some(sample);
                }
            }
            (Some("event_msg"), Some("task_started")) => {
                self.last_turn = Some(AgentTurnEnd::MidTurn);
            }
            (Some("event_msg"), Some("task_complete")) => {
                self.last_turn = Some(match payload.error {
                    Some(ErrorKind(kind)) => AgentTurnEnd::Failed {
                        kind: kind.as_deref().and_then(Ident::new),
                    },
                    None => AgentTurnEnd::Completed,
                });
            }
            (Some("event_msg"), Some("turn_aborted")) => {
                self.last_turn = Some(AgentTurnEnd::Interrupted);
            }
            _ => {}
        }
    }

    /// Credit the growth of the running total to the current model. A
    /// total that is incomplete or not a count at all is skipped and leaves
    /// the baseline where it was, so the next usable total is measured from
    /// the last good one. A usable total lower than the baseline in any count
    /// is ambiguous: the counters may have restarted or the total may be
    /// wrong, and the reader accepts nothing in the rollout as telling
    /// which, so it makes the run's token total incomplete rather than adding
    /// either reading, and also leaves the baseline where it was.
    fn observe_total(&mut self, usage: &CodexTokenUsage, stats: &mut ReaderStats) {
        let Some(total) = usage.usable(self.last_total.as_ref()) else {
            stats.unusable_totals += 1;
            return;
        };
        let delta = match &self.last_total {
            None => total,
            Some(prev) => match total.since(prev) {
                Some(delta) => delta,
                None => {
                    stats.regressions += 1;
                    self.tokens_incomplete = true;
                    return;
                }
            },
        };
        self.last_total = Some(total);
        if delta.is_zero() {
            return;
        }
        let key = capped(&self.tokens_by_model, self.current_model.clone());
        if key.is_none() && self.current_model.is_some() {
            stats.models_overflowed += 1;
        }
        self.tokens_by_model.entry(key).or_default().add(&delta);
    }
}

// The narrow shapes a rollout line is deserialised into. Every field not
// named here is skipped by the parser without being kept.

#[derive(Deserialize)]
struct NarrowRecord {
    #[serde(rename = "type", default)]
    kind: OptStr,
    #[serde(default)]
    payload: Option<NarrowPayload>,
}

#[derive(Deserialize, Default)]
struct NarrowPayload {
    #[serde(rename = "type", default)]
    kind: OptStr,
    #[serde(default)]
    model: OptStr,
    #[serde(default)]
    info: Option<NarrowInfo>,
    #[serde(default)]
    rate_limits: Option<NarrowRateLimits>,
    /// Present (as the kind it names, if any) when the turn failed.
    #[serde(default)]
    error: Option<ErrorKind>,
}

#[derive(Deserialize)]
struct NarrowInfo {
    #[serde(default)]
    total_token_usage: Option<CodexTokenUsage>,
}

/// Codex's token counts, each as the rollout has it or `None` when absent.
/// Its `input_tokens` already includes the cached tokens, as OpenAI's API
/// counts them. Whether it also includes `cache_write_input_tokens` is not
/// known: that count was 0 on every sampled record, so it is taken as
/// included, the same as cached tokens.
#[derive(Deserialize)]
struct CodexTokenUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cached_input_tokens: Option<u64>,
    #[serde(default)]
    cache_write_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

impl CodexTokenUsage {
    /// The total, or `None` when it is incomplete — input or output
    /// missing, or a cache count missing that was not zero in `prev` — or
    /// inconsistent: its cache counts add up to more than the input that
    /// includes them. A cache count missing that was zero (or with nothing
    /// before it) is read as zero.
    fn usable(&self, prev: Option<&Tokens>) -> Option<Tokens> {
        let cache = |value: Option<u64>, before: u64| value.or((before == 0).then_some(0));
        let total = Tokens {
            input: self.input_tokens?,
            output: self.output_tokens?,
            cache_read: cache(self.cached_input_tokens, prev.map_or(0, |p| p.cache_read))?,
            cache_write: cache(
                self.cache_write_input_tokens,
                prev.map_or(0, |p| p.cache_write),
            )?,
        };
        total.is_consistent().then_some(total)
    }
}

#[derive(Deserialize)]
struct NarrowRateLimits {
    #[serde(default)]
    primary: Option<NarrowWindow>,
    #[serde(default)]
    plan_type: OptStr,
}

#[derive(Deserialize)]
struct NarrowWindow {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    window_minutes: Option<u64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

impl NarrowRateLimits {
    fn sample(self) -> Option<CodexRateSample> {
        let primary = self.primary?;
        let used = primary
            .used_percent
            .filter(|p| p.is_finite() && *p >= 0.0)?;
        Some(CodexRateSample {
            plan: self.plan_type.0.as_deref().and_then(Ident::new),
            used_bp: (used * 100.0).round().min(u32::MAX as f64) as u32,
            window_minutes: primary.window_minutes?,
            resets_at: primary.resets_at?,
        })
    }
}

/// A string, or `None` for a value of any other type, which is skipped.
#[derive(Default)]
struct OptStr(Option<String>);

impl<'de> Deserialize<'de> for OptStr {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(OptStrVisitor).map(OptStr)
    }
}

struct OptStrVisitor;

impl<'de> Visitor<'de> for OptStrVisitor {
    type Value = Option<String>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Some(v.to_owned()))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
}

/// A turn's `error`: the `codex_error_info` kind when it is a string, and
/// nothing else of it — its `message` is skipped unread.
struct ErrorKind(Option<String>);

impl<'de> Deserialize<'de> for ErrorKind {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ErrorKind;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an error object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ErrorKind, A::Error> {
                let mut kind = None;
                while let Some(key) = map.next_key::<OptStr>()? {
                    if key.0.as_deref() == Some("codex_error_info") {
                        kind = map.next_value::<OptStr>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(ErrorKind(kind))
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<ErrorKind, E> {
                Ok(ErrorKind(None))
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<ErrorKind, E> {
                Ok(ErrorKind(None))
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<ErrorKind, E> {
                Ok(ErrorKind(None))
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<ErrorKind, E> {
                Ok(ErrorKind(None))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<ErrorKind, E> {
                Ok(ErrorKind(None))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ErrorKind, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ErrorKind(None))
            }
        }
        d.deserialize_any(V)
    }
}

// ---------------------------------------------------------------------------
// Export mapping
// ---------------------------------------------------------------------------

/// Every attribute key the accounting record maps to. As with [`AttrKey`],
/// the closed set is the allow-list. `gen_ai.*` names follow the GenAI
/// semantic conventions, all in development status as read at
/// `open-telemetry/semantic-conventions-genai` commit
/// `9ffc7e00c639be7712e544e3e54331e885dccf84`, so a change there may rename
/// them; `error.type` is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountingKey {
    RunId,
    Agent,
    ResponseModel,
    InputTokens,
    OutputTokens,
    CacheReadInputTokens,
    CacheWriteInputTokens,
    TokensProvenance,
    TokensUnavailableReason,
    CostMicroUsd,
    CostProvenance,
    CostUnavailableReason,
    QuotaPlan,
    QuotaWindowMinutes,
    QuotaResetsAt,
    QuotaFromBp,
    QuotaToBp,
    QuotaProvenance,
    QuotaUnavailableReason,
    Termination,
    TerminationProvenance,
    TerminationUnavailableReason,
    LastTurn,
    LastTurnProvenance,
    LastTurnUnavailableReason,
    ErrorType,
    Disposition,
    DispositionProvenance,
    DispositionUnavailableReason,
    Checks,
    ChecksProvenance,
    ChecksUnavailableReason,
}

impl AccountingKey {
    /// Every key, for the allow-list test.
    pub const ALL: &'static [AccountingKey] = &[
        AccountingKey::RunId,
        AccountingKey::Agent,
        AccountingKey::ResponseModel,
        AccountingKey::InputTokens,
        AccountingKey::OutputTokens,
        AccountingKey::CacheReadInputTokens,
        AccountingKey::CacheWriteInputTokens,
        AccountingKey::TokensProvenance,
        AccountingKey::TokensUnavailableReason,
        AccountingKey::CostMicroUsd,
        AccountingKey::CostProvenance,
        AccountingKey::CostUnavailableReason,
        AccountingKey::QuotaPlan,
        AccountingKey::QuotaWindowMinutes,
        AccountingKey::QuotaResetsAt,
        AccountingKey::QuotaFromBp,
        AccountingKey::QuotaToBp,
        AccountingKey::QuotaProvenance,
        AccountingKey::QuotaUnavailableReason,
        AccountingKey::Termination,
        AccountingKey::TerminationProvenance,
        AccountingKey::TerminationUnavailableReason,
        AccountingKey::LastTurn,
        AccountingKey::LastTurnProvenance,
        AccountingKey::LastTurnUnavailableReason,
        AccountingKey::ErrorType,
        AccountingKey::Disposition,
        AccountingKey::DispositionProvenance,
        AccountingKey::DispositionUnavailableReason,
        AccountingKey::Checks,
        AccountingKey::ChecksProvenance,
        AccountingKey::ChecksUnavailableReason,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            AccountingKey::RunId => AttrKey::RunId.as_str(),
            AccountingKey::Agent => AttrKey::Agent.as_str(),
            AccountingKey::ResponseModel => "gen_ai.response.model",
            AccountingKey::InputTokens => "gen_ai.usage.input_tokens",
            AccountingKey::OutputTokens => "gen_ai.usage.output_tokens",
            AccountingKey::CacheReadInputTokens => "gen_ai.usage.cache_read.input_tokens",
            AccountingKey::CacheWriteInputTokens => "gen_ai.usage.cache_write.input_tokens",
            AccountingKey::TokensProvenance => "deck.tokens.provenance",
            AccountingKey::TokensUnavailableReason => "deck.tokens.unavailable_reason",
            AccountingKey::CostMicroUsd => "deck.cost.microusd",
            AccountingKey::CostProvenance => "deck.cost.provenance",
            AccountingKey::CostUnavailableReason => "deck.cost.unavailable_reason",
            AccountingKey::QuotaPlan => "deck.quota.plan",
            AccountingKey::QuotaWindowMinutes => "deck.quota.window_minutes",
            AccountingKey::QuotaResetsAt => "deck.quota.resets_at",
            AccountingKey::QuotaFromBp => "deck.quota.used_from_bp",
            AccountingKey::QuotaToBp => "deck.quota.used_to_bp",
            AccountingKey::QuotaProvenance => "deck.quota.provenance",
            AccountingKey::QuotaUnavailableReason => "deck.quota.unavailable_reason",
            AccountingKey::Termination => "deck.run.termination",
            AccountingKey::TerminationProvenance => "deck.run.termination.provenance",
            AccountingKey::TerminationUnavailableReason => {
                "deck.run.termination.unavailable_reason"
            }
            AccountingKey::LastTurn => "deck.run.last_turn",
            AccountingKey::LastTurnProvenance => "deck.run.last_turn.provenance",
            AccountingKey::LastTurnUnavailableReason => "deck.run.last_turn.unavailable_reason",
            AccountingKey::ErrorType => "error.type",
            AccountingKey::Disposition => "deck.work.disposition",
            AccountingKey::DispositionProvenance => "deck.work.disposition.provenance",
            AccountingKey::DispositionUnavailableReason => {
                "deck.work.disposition.unavailable_reason"
            }
            AccountingKey::Checks => "deck.work.checks",
            AccountingKey::ChecksProvenance => "deck.work.checks.provenance",
            AccountingKey::ChecksUnavailableReason => "deck.work.checks.unavailable_reason",
        }
    }
}

/// One allow-listed accounting attribute. Its fields are private, and the
/// only constructors take an enum's name or an [`Ident`], so a string value
/// is always one of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountingAttribute {
    key: AccountingKey,
    value: AttrValue,
}

/// An enum's name.
fn name(key: AccountingKey, value: &'static str) -> AccountingAttribute {
    AccountingAttribute {
        key,
        value: AttrValue::Str(value.to_owned()),
    }
}

fn ident(key: AccountingKey, value: &Ident) -> AccountingAttribute {
    AccountingAttribute {
        key,
        value: AttrValue::Str(value.as_str().to_owned()),
    }
}

fn int(key: AccountingKey, value: u64) -> AccountingAttribute {
    AccountingAttribute {
        key,
        value: AttrValue::Int(i64::try_from(value).unwrap_or(i64::MAX)),
    }
}

/// A field's provenance and, when unavailable, its reason.
fn provenance<T>(
    out: &mut Vec<AccountingAttribute>,
    field: &Field<T>,
    key: AccountingKey,
    reason_key: AccountingKey,
) {
    out.push(name(key, field.provenance().as_str()));
    if let Some(reason) = field.unavailable() {
        out.push(name(reason_key, reason.as_str()));
    }
}

/// A run's attributes: `run` for the run itself (an `agent.run` span), and one
/// set per model in `models` (one span event each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunExport {
    pub run: Vec<AccountingAttribute>,
    pub models: Vec<Vec<AccountingAttribute>>,
}

/// The attributes for `record`. Pure; the live exporter does not call it.
pub fn run_attributes(record: &RunRecord) -> RunExport {
    use AccountingKey as K;
    let mut run = vec![
        ident(K::RunId, &record.identity.run_id),
        ident(K::Agent, &record.identity.agent),
    ];
    if let Some(t) = record.termination.value() {
        run.push(name(K::Termination, t.outcome.as_str()));
        if let Some(kind) = &t.error_type {
            run.push(ident(K::ErrorType, kind));
        }
    }
    provenance(
        &mut run,
        &record.termination,
        K::TerminationProvenance,
        K::TerminationUnavailableReason,
    );
    if let Some(turn) = record.last_turn.value() {
        run.push(name(K::LastTurn, turn.as_str()));
    }
    provenance(
        &mut run,
        &record.last_turn,
        K::LastTurnProvenance,
        K::LastTurnUnavailableReason,
    );
    if let Some(q) = record.quota.value() {
        if let Some(plan) = &q.plan {
            run.push(ident(K::QuotaPlan, plan));
        }
        run.push(int(K::QuotaWindowMinutes, q.window_minutes));
        run.push(AccountingAttribute {
            key: K::QuotaResetsAt,
            value: AttrValue::Int(q.resets_at),
        });
        run.push(int(K::QuotaFromBp, u64::from(q.from_bp)));
        run.push(int(K::QuotaToBp, u64::from(q.to_bp)));
    }
    provenance(
        &mut run,
        &record.quota,
        K::QuotaProvenance,
        K::QuotaUnavailableReason,
    );
    let models = record
        .models
        .iter()
        .map(|usage| {
            let mut attrs = Vec::new();
            if let Some(model) = &usage.model {
                attrs.push(ident(K::ResponseModel, model));
            }
            if let Some(t) = usage.tokens.value() {
                attrs.push(int(K::InputTokens, t.input));
                attrs.push(int(K::OutputTokens, t.output));
                attrs.push(int(K::CacheReadInputTokens, t.cache_read));
                attrs.push(int(K::CacheWriteInputTokens, t.cache_write));
            }
            provenance(
                &mut attrs,
                &usage.tokens,
                K::TokensProvenance,
                K::TokensUnavailableReason,
            );
            if let Some(cost) = usage.cost.value() {
                attrs.push(int(K::CostMicroUsd, cost.0));
            }
            provenance(
                &mut attrs,
                &usage.cost,
                K::CostProvenance,
                K::CostUnavailableReason,
            );
            attrs
        })
        .collect();
    RunExport { run, models }
}

/// The attributes for a work item's outcome fields.
pub fn work_item_attributes(item: &WorkItemRecord) -> Vec<AccountingAttribute> {
    use AccountingKey as K;
    let mut out = Vec::new();
    if let Some(d) = item.disposition.value() {
        out.push(name(K::Disposition, d.as_str()));
    }
    provenance(
        &mut out,
        &item.disposition,
        K::DispositionProvenance,
        K::DispositionUnavailableReason,
    );
    if let Some(c) = item.checks.value() {
        out.push(name(K::Checks, c.as_str()));
    }
    provenance(
        &mut out,
        &item.checks,
        K::ChecksProvenance,
        K::ChecksUnavailableReason,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::time::{Duration, Instant};

    // --- fixtures (synthetic; shaped like Codex 0.164.0's records) ---

    fn turn_context(model: &str) -> String {
        format!(
            r#"{{"timestamp":"t","type":"turn_context","payload":{{"turn_id":"u1","cwd":"/w","model":"{model}","collaboration_mode":{{"settings":{{"developer_instructions":"be brief"}}}}}}}}"#
        )
    }

    /// A `total_token_usage` object with every count present.
    fn usage(input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{t}}}"#,
            t = input + output
        )
    }

    /// A `token_count` record with `total` as its running total.
    fn token_count_with(total: &str, used: f64, resets_at: i64, plan: &str) -> String {
        format!(
            r#"{{"timestamp":"t","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{total},"last_token_usage":{{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":2}},"model_context_window":1000}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used},"window_minutes":10080,"resets_at":{resets_at}}},"secondary":null,"plan_type":"{plan}"}}}}}}"#
        )
    }

    fn token_count(input: u64, cached: u64, output: u64, used: f64, resets_at: i64) -> String {
        token_count_with(&usage(input, cached, output), used, resets_at, "prolite")
    }

    fn task_started() -> &'static str {
        r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_started","turn_id":"u1"}}"#
    }

    fn task_complete() -> &'static str {
        r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","turn_id":"u1","last_agent_message":"done"}}"#
    }

    fn write_lines(file: &mut File, lines: &[&str]) {
        for line in lines {
            file.write_all(line.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.flush().unwrap();
    }

    /// A reader that has polled a fresh rollout holding `lines`.
    fn read(lines: &[String]) -> CodexUsageReader {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut file = File::create(&path).unwrap();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_lines(&mut file, &refs);
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
        reader
    }

    fn id(value: &str) -> Ident {
        Ident::new(value).unwrap()
    }

    fn identity(run_id: &str, agent: &str) -> RunIdentity {
        RunIdentity::new(id(run_id), id(agent))
    }

    fn tokens(input: u64, output: u64) -> Tokens {
        Tokens {
            input,
            output,
            ..Tokens::default()
        }
    }

    fn run(identity: RunIdentity, model: &str, t: Tokens, cost: Field<MicroUsd>) -> RunRecord {
        RunRecord {
            identity,
            models: vec![ModelUsage {
                model: Some(id(model)),
                tokens: Field::ProviderReported(t),
                cost,
            }],
            quota: Field::Unavailable(Unavailable::NoChannel),
            termination: Field::ProviderReported(Termination {
                outcome: RunTermination::Completed,
                error_type: None,
            }),
            last_turn: Field::ProviderReported(AgentTurnEnd::Completed),
        }
    }

    fn pi_reply(model: &str, cost: Option<f64>, stop: Option<PiStopReason>) -> PiReplyUsage {
        PiReplyUsage {
            model: model.into(),
            input: 10,
            output: 2,
            cache_read: 100,
            cache_write: 0,
            cost_total_usd: cost,
            stop_reason: stop,
        }
    }

    /// A message with usage; `used_tokens: false` is a reported zero.
    fn opencode_message(model: &str, used_tokens: bool, cost: f64) -> OpenCodeMessageUsage {
        let n = u64::from(used_tokens);
        OpenCodeMessageUsage {
            model: model.into(),
            usage: Some(OpenCodeUsage {
                input: 5 * n,
                output: 3 * n,
                cache_read: 7 * n,
                cache_write: 0,
                cost_usd: cost,
            }),
        }
    }

    /// A failed or never-completed message: no usage from the provider.
    fn opencode_unrecorded(model: &str) -> OpenCodeMessageUsage {
        OpenCodeMessageUsage {
            model: model.into(),
            usage: None,
        }
    }

    // --- provenance and names ---

    #[test]
    fn each_provenance_names_itself_and_only_unavailable_lacks_a_value() {
        let fields = [
            (Field::Measured(1), Provenance::Measured),
            (Field::ProviderReported(1), Provenance::ProviderReported),
            (Field::Estimated(1), Provenance::Estimated),
            (Field::AgentAsserted(1), Provenance::AgentAsserted),
            (
                Field::Unavailable(Unavailable::NoChannel),
                Provenance::Unavailable,
            ),
        ];
        for (field, provenance) in fields {
            assert_eq!(field.provenance(), provenance);
            assert_eq!(
                field.value().is_some(),
                provenance != Provenance::Unavailable
            );
            assert_eq!(
                field.unavailable().is_some(),
                provenance == Provenance::Unavailable
            );
        }
        assert_eq!(Provenance::ProviderReported.as_str(), "provider_reported");
        assert_eq!(Provenance::AgentAsserted.as_str(), "agent_asserted");
    }

    /// Every name the export can emit for an enum is distinct within its enum
    /// and snake_case, the reserved ones (`timed_out`, `abandoned`) included.
    #[test]
    fn every_exported_enum_name_is_distinct_and_snake_case() {
        let groups: [Vec<&str>; 6] = [
            [
                Provenance::Measured,
                Provenance::ProviderReported,
                Provenance::Estimated,
                Provenance::AgentAsserted,
                Provenance::Unavailable,
            ]
            .map(Provenance::as_str)
            .to_vec(),
            [
                Unavailable::NoChannel,
                Unavailable::SubscriptionZero,
                Unavailable::UnknownPrice,
                Unavailable::NotRecorded,
                Unavailable::Incomplete,
                Unavailable::WindowReset,
                Unavailable::PlanChanged,
                Unavailable::QuotaDecreased,
                Unavailable::RunNotEnded,
                Unavailable::NoWorkItem,
            ]
            .map(Unavailable::as_str)
            .to_vec(),
            [
                RunTermination::Completed,
                RunTermination::Failed,
                RunTermination::Interrupted,
                RunTermination::TimedOut,
                RunTermination::Unknown,
            ]
            .map(RunTermination::as_str)
            .to_vec(),
            [
                AgentTurnEnd::Completed,
                AgentTurnEnd::Failed { kind: None },
                AgentTurnEnd::Interrupted,
                AgentTurnEnd::MidTurn,
                AgentTurnEnd::Unknown,
            ]
            .iter()
            .map(AgentTurnEnd::as_str)
            .collect(),
            [
                Disposition::Merged,
                Disposition::Superseded,
                Disposition::Rejected,
                Disposition::ResolvedWithoutChange,
                Disposition::Deferred,
                Disposition::Abandoned,
                Disposition::Open,
            ]
            .map(Disposition::as_str)
            .to_vec(),
            [
                ObservedChecks::Passing,
                ObservedChecks::Failing,
                ObservedChecks::Pending,
            ]
            .map(ObservedChecks::as_str)
            .to_vec(),
        ];
        for names in groups {
            let distinct: HashSet<&str> = names.iter().copied().collect();
            assert_eq!(distinct.len(), names.len(), "{names:?}");
            for name in names {
                assert!(
                    name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn a_subscription_zero_is_unavailable_and_a_tokenless_zero_is_zero() {
        let used = tokens(10, 1);
        assert_eq!(
            agent_cost(Some(MicroUsd(0)), &used),
            Field::Unavailable(Unavailable::SubscriptionZero)
        );
        assert_eq!(
            agent_cost(Some(MicroUsd(0)), &Tokens::default()),
            Field::Estimated(MicroUsd(0))
        );
        assert_eq!(
            agent_cost(Some(MicroUsd(5)), &used),
            Field::Estimated(MicroUsd(5))
        );
        assert_eq!(
            agent_cost(None, &used),
            Field::Unavailable(Unavailable::NotRecorded)
        );
        assert_eq!(MicroUsd::from_usd(1.5), Some(MicroUsd(1_500_000)));
        assert_eq!(MicroUsd::from_usd(-1.0), None);
        assert_eq!(MicroUsd::from_usd(f64::NAN), None);
    }

    #[test]
    fn costs_are_classified_before_they_are_added() {
        let est = |n| Field::Estimated(MicroUsd(n));
        let zero = Field::Unavailable(Unavailable::SubscriptionZero);
        let none = Field::Unavailable(Unavailable::NotRecorded);
        let incomplete = Field::Unavailable(Unavailable::Incomplete);
        assert_eq!(add_cost(est(2), est(3)), est(5));
        assert_eq!(add_cost(zero, zero), zero);
        assert_eq!(add_cost(zero, est(1_000_000)), incomplete);
        assert_eq!(add_cost(est(1_000_000), zero), incomplete);
        assert_eq!(add_cost(zero, none), incomplete, "two different reasons");
        assert_eq!(
            add_cost(est(4), Field::ProviderReported(MicroUsd(4))),
            incomplete,
            "two provenances are never one sum"
        );
        assert_eq!(
            add_cost(est(0), zero),
            zero,
            "a tokenless reply adds nothing"
        );
        assert_eq!(add_cost(zero, est(0)), zero);
        assert_eq!(add_cost(est(0), est(7)), est(7));
    }

    // --- run termination ---

    #[test]
    fn an_ended_run_takes_the_agents_word_and_the_decks_end_for_a_cut_turn() {
        let failed = AgentTurnEnd::Failed {
            kind: Some(id("server_overloaded")),
        };
        let t = run_termination(Some(EndReason::Exited), Some(&failed));
        assert_eq!(t.provenance(), Provenance::ProviderReported);
        assert_eq!(t.value().unwrap().outcome, RunTermination::Failed);
        assert_eq!(t.value().unwrap().error_type, Some(id("server_overloaded")));
        use AgentTurnEnd as A;
        use EndReason as E;
        use RunTermination as T;
        let cases = [
            (
                E::Closed,
                Some(A::MidTurn),
                Provenance::Measured,
                T::Interrupted,
            ),
            (
                E::DaemonShutdown,
                Some(A::MidTurn),
                Provenance::Measured,
                T::Interrupted,
            ),
            (
                E::Exited,
                Some(A::MidTurn),
                Provenance::Measured,
                T::Unknown,
            ),
            (
                E::Unknown,
                Some(A::MidTurn),
                Provenance::Measured,
                T::Unknown,
            ),
            (
                E::Closed,
                Some(A::Unknown),
                Provenance::Measured,
                T::Unknown,
            ),
            (
                E::Exited,
                Some(A::Unknown),
                Provenance::Measured,
                T::Unknown,
            ),
            (
                E::Closed,
                Some(A::Completed),
                Provenance::ProviderReported,
                T::Completed,
            ),
            (
                E::Exited,
                Some(A::Interrupted),
                Provenance::ProviderReported,
                T::Interrupted,
            ),
            (E::Exited, None, Provenance::Measured, T::Unknown),
            (E::Closed, None, Provenance::Measured, T::Unknown),
        ];
        for (end, agent, provenance, outcome) in cases {
            let got = run_termination(Some(end), agent.as_ref());
            assert_eq!(got.provenance(), provenance, "{end:?} {agent:?}");
            assert_eq!(
                got.value().map(|t| t.outcome),
                Some(outcome),
                "{end:?} {agent:?}"
            );
        }
    }

    /// While the deck reports no end, the run has not terminated, whatever
    /// the agent's last turn says, and the record still carries that turn.
    #[test]
    fn an_open_run_has_no_termination_and_keeps_its_last_turn() {
        let turns = [
            None,
            Some(AgentTurnEnd::Completed),
            Some(AgentTurnEnd::Failed {
                kind: Some(id("server_overloaded")),
            }),
            Some(AgentTurnEnd::Interrupted),
            Some(AgentTurnEnd::MidTurn),
            Some(AgentTurnEnd::Unknown),
        ];
        for turn in turns {
            assert_eq!(
                run_termination(None, turn.as_ref()),
                Field::Unavailable(Unavailable::RunNotEnded),
                "{turn:?}"
            );
            let record = RunRecord::new(
                identity("r", "codex"),
                AgentUsage {
                    models: usage_unavailable(Unavailable::NotRecorded),
                    quota: Field::Unavailable(Unavailable::NotRecorded),
                    last_turn: turn.clone(),
                },
                None,
            );
            assert_eq!(
                record.termination,
                Field::Unavailable(Unavailable::RunNotEnded)
            );
            match &turn {
                Some(turn) => assert_eq!(record.last_turn, Field::ProviderReported(turn.clone())),
                None => assert_eq!(
                    record.last_turn,
                    Field::Unavailable(Unavailable::NotRecorded)
                ),
            }
            let export = run_attributes(&record);
            assert!(
                !export
                    .run
                    .iter()
                    .any(|a| a.key == AccountingKey::Termination),
                "an open run exports no termination"
            );
        }
    }

    /// A Pi run whose last reply asked for a tool was cut off mid-turn: closed
    /// by the deck it was interrupted, otherwise its end is unknown — never
    /// completed.
    #[test]
    fn a_pi_run_ending_on_a_tool_call_is_not_completed() {
        let replies = [
            pi_reply("gpt-5.5", Some(0.01), Some(PiStopReason::Stop)),
            pi_reply("gpt-5.5", Some(0.01), Some(PiStopReason::ToolUse)),
        ];
        assert_eq!(from_pi(&replies).last_turn, Some(AgentTurnEnd::MidTurn));
        let closed = RunRecord::new(
            identity("r", "pi"),
            from_pi(&replies),
            Some(EndReason::Closed),
        );
        assert_eq!(
            closed.termination,
            Field::Measured(Termination {
                outcome: RunTermination::Interrupted,
                error_type: None
            })
        );
        let exited = RunRecord::new(
            identity("r", "pi"),
            from_pi(&replies),
            Some(EndReason::Exited),
        );
        assert_eq!(
            exited.termination.value().map(|t| t.outcome),
            Some(RunTermination::Unknown)
        );
        let mapping = [
            (Some(PiStopReason::Stop), AgentTurnEnd::Completed),
            (Some(PiStopReason::Length), AgentTurnEnd::Completed),
            (Some(PiStopReason::ToolUse), AgentTurnEnd::MidTurn),
            (
                Some(PiStopReason::Error),
                AgentTurnEnd::Failed { kind: None },
            ),
            (Some(PiStopReason::Aborted), AgentTurnEnd::Interrupted),
            (None, AgentTurnEnd::Unknown),
        ];
        for (stop, turn) in mapping {
            assert_eq!(
                from_pi(&[pi_reply("gpt-5.5", Some(0.01), stop)]).last_turn,
                Some(turn),
                "{stop:?}"
            );
        }
        let unstated = [pi_reply("gpt-5.5", Some(0.01), None)];
        let closed = RunRecord::new(
            identity("r", "pi"),
            from_pi(&unstated),
            Some(EndReason::Closed),
        );
        assert_eq!(
            closed.termination.value().map(|t| t.outcome),
            Some(RunTermination::Unknown)
        );
    }

    // --- work items ---

    #[test]
    fn disposition_and_checks_follow_the_forge_then_the_agent() {
        use PullRequestState as Pr;
        let cases = [
            (Some(Pr::Merged), None, Some(Disposition::Merged)),
            (
                Some(Pr::Closed { successor: true }),
                None,
                Some(Disposition::Superseded),
            ),
            (
                Some(Pr::Closed { successor: false }),
                None,
                Some(Disposition::Rejected),
            ),
            (
                Some(Pr::Open),
                Some(IssueState::ClosedCompleted),
                Some(Disposition::Open),
            ),
            (
                None,
                Some(IssueState::ClosedCompleted),
                Some(Disposition::ResolvedWithoutChange),
            ),
            (
                None,
                Some(IssueState::ClosedNotPlanned),
                Some(Disposition::Deferred),
            ),
            (None, Some(IssueState::Open), Some(Disposition::Open)),
            (None, None, None),
        ];
        for (pr, issue, want) in cases {
            let got = work_disposition(pr, issue);
            assert_eq!(got.value().copied(), want, "{pr:?} {issue:?}");
            assert_eq!(
                got.provenance(),
                if want.is_some() {
                    Provenance::Measured
                } else {
                    Provenance::Unavailable
                }
            );
        }
        assert_eq!(
            observed_checks(Some(CiState::Passing), Some(false)),
            Field::Measured(ObservedChecks::Passing)
        );
        assert_eq!(
            observed_checks(Some(CiState::Pending), None),
            Field::Measured(ObservedChecks::Pending)
        );
        assert_eq!(
            observed_checks(None, Some(true)),
            Field::AgentAsserted(ObservedChecks::Passing)
        );
        assert_eq!(
            observed_checks(None, None),
            Field::Unavailable(Unavailable::NoChannel)
        );
    }

    /// The prototype records the CI rollup it observed, which counts optional
    /// checks and reads a skipped-only rollup as passing, so it exports that
    /// as `deck.work.checks = passing`: there is no `verified` value and no
    /// verification key, which the follow-up adds from the required checks
    /// on the merged head.
    #[test]
    fn a_passing_rollup_is_observed_checks_not_verification() {
        let item = WorkItemRecord {
            disposition: work_disposition(Some(PullRequestState::Merged), None),
            checks: observed_checks(Some(CiState::Passing), None),
        };
        let attrs = work_item_attributes(&item);
        assert!(attrs.contains(&name(AccountingKey::Checks, "passing")));
        assert!(attrs.contains(&name(AccountingKey::ChecksProvenance, "measured")));
        for key in AccountingKey::ALL {
            assert!(!key.as_str().contains("verif"), "{}", key.as_str());
        }
        for attr in &attrs {
            assert_ne!(attr.value, AttrValue::Str("verified".into()));
        }
    }

    // --- normalisers ---

    #[test]
    fn claude_counts_cache_in_input_and_keeps_its_cost_as_an_estimate() {
        let session = ClaudeSessionUsage {
            models: vec![ClaudeModelUsage {
                model: "claude-opus-5-5".into(),
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: 300,
                cache_creation_input_tokens: 40,
                cost_usd: Some(1.25),
            }],
            has_unknown_model_cost: false,
            last_turn: Some(AgentTurnEnd::Completed),
        };
        let usage = from_claude(Some(&session));
        assert_eq!(usage.models.len(), 1);
        assert_eq!(
            usage.models[0].tokens,
            Field::ProviderReported(Tokens {
                input: 350,
                output: 20,
                cache_read: 300,
                cache_write: 40
            })
        );
        assert_eq!(usage.models[0].cost, Field::Estimated(MicroUsd(1_250_000)));
        assert_eq!(usage.quota, Field::Unavailable(Unavailable::NoChannel));

        let unknown = ClaudeSessionUsage {
            has_unknown_model_cost: true,
            ..session
        };
        assert_eq!(
            from_claude(Some(&unknown)).models[0].cost,
            Field::Unavailable(Unavailable::UnknownPrice)
        );
        let missing = from_claude(None);
        assert_eq!(
            missing.models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
        assert_eq!(
            missing.models[0].cost,
            Field::Unavailable(Unavailable::NotRecorded)
        );
    }

    fn sample(plan: &str, bp: u32, resets_at: i64) -> CodexRateSample {
        CodexRateSample {
            plan: Some(id(plan)),
            used_bp: bp,
            window_minutes: 10080,
            resets_at,
        }
    }

    #[test]
    fn codex_has_no_cost_and_a_quota_delta_only_within_one_window_and_plan() {
        let mut summary = CodexRolloutSummary::default();
        summary
            .tokens_by_model
            .insert(Some(id("gpt-5.5")), tokens(100, 5));
        summary.quota_first = Some(sample("prolite", 2600, 99));
        summary.quota_last = Some(sample("prolite", 4500, 99));
        let usage = from_codex(&summary);
        assert_eq!(
            usage.models[0].cost,
            Field::Unavailable(Unavailable::NoChannel)
        );
        assert_eq!(
            usage.models[0].tokens,
            Field::ProviderReported(tokens(100, 5))
        );
        assert_eq!(usage.quota.value().unwrap().consumed_bp(), 1900);

        let quota_with = |last| {
            let mut s = summary.clone();
            s.quota_last = Some(last);
            from_codex(&s).quota
        };
        assert_eq!(
            quota_with(sample("prolite", 100, 200)),
            Field::Unavailable(Unavailable::WindowReset)
        );
        assert_eq!(
            quota_with(sample("pro", 4500, 99)),
            Field::Unavailable(Unavailable::PlanChanged)
        );
        assert_eq!(
            quota_with(sample("prolite", 2000, 99)),
            Field::Unavailable(Unavailable::QuotaDecreased)
        );
        assert_eq!(
            from_codex(&CodexRolloutSummary::default()).models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
    }

    /// A plan change or a drop between two samples in the middle of a rollout
    /// breaks the delta even when the first and last samples look comparable.
    #[test]
    fn a_mid_run_plan_change_or_drop_in_the_reader_breaks_the_quota_delta() {
        let total = usage(10, 0, 1);
        let reader = read(&[
            token_count_with(&total, 20.0, 99, "prolite"),
            token_count_with(&total, 25.0, 99, "pro"),
            token_count_with(&total, 30.0, 99, "prolite"),
        ]);
        assert_eq!(
            from_codex(reader.summary()).quota,
            Field::Unavailable(Unavailable::PlanChanged)
        );
        let reader = read(&[
            token_count_with(&total, 20.0, 99, "prolite"),
            token_count_with(&total, 5.0, 99, "prolite"),
            token_count_with(&total, 30.0, 99, "prolite"),
        ]);
        assert_eq!(
            from_codex(reader.summary()).quota,
            Field::Unavailable(Unavailable::QuotaDecreased)
        );
        let reader = read(&[
            token_count_with(&total, 20.0, 99, "prolite"),
            token_count_with(&total, 20.0, 99, "prolite"),
            token_count_with(&total, 30.0, 99, "prolite"),
        ]);
        assert_eq!(
            from_codex(reader.summary())
                .quota
                .value()
                .unwrap()
                .consumed_bp(),
            1000
        );
    }

    #[test]
    fn pi_sums_per_model_and_its_last_reply_names_the_turn_end() {
        let usage = from_pi(&[
            pi_reply("gpt-5.5", Some(0.01), Some(PiStopReason::ToolUse)),
            pi_reply("gpt-5.5", Some(0.02), Some(PiStopReason::Stop)),
            pi_reply("other", None, Some(PiStopReason::Error)),
        ]);
        let gpt = usage
            .models
            .iter()
            .find(|m| m.model == Some(id("gpt-5.5")))
            .unwrap();
        assert_eq!(
            gpt.tokens,
            Field::ProviderReported(Tokens {
                input: 220,
                output: 4,
                cache_read: 200,
                cache_write: 0
            })
        );
        assert_eq!(gpt.cost, Field::Estimated(MicroUsd(30_000)));
        let other = usage
            .models
            .iter()
            .find(|m| m.model == Some(id("other")))
            .unwrap();
        assert_eq!(other.cost, Field::Unavailable(Unavailable::NotRecorded));
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::Failed { kind: None }));
        let zero = from_pi(&[pi_reply("gpt-5.5", Some(0.0), None)]);
        assert_eq!(
            zero.models[0].cost,
            Field::Unavailable(Unavailable::SubscriptionZero)
        );
        assert_eq!(
            from_pi(&[]).models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
    }

    /// One model's replies, one on a subscription (cost 0 with tokens) and
    /// one priced: the total is incomplete, not the priced reply's cost.
    #[test]
    fn mixed_subscription_and_priced_replies_make_an_incomplete_cost() {
        let pi = from_pi(&[
            pi_reply("gpt-5.5", Some(0.0), Some(PiStopReason::Stop)),
            pi_reply("gpt-5.5", Some(1.0), Some(PiStopReason::Stop)),
        ]);
        assert_eq!(pi.models.len(), 1);
        assert_eq!(
            pi.models[0].cost,
            Field::Unavailable(Unavailable::Incomplete)
        );
        let pi = from_pi(&[
            pi_reply("gpt-5.5", Some(1.0), Some(PiStopReason::Stop)),
            pi_reply("gpt-5.5", None, Some(PiStopReason::Stop)),
        ]);
        assert_eq!(
            pi.models[0].cost,
            Field::Unavailable(Unavailable::Incomplete)
        );

        let opencode = from_opencode(&[
            opencode_message("gpt-5.5", true, 0.0),
            opencode_message("gpt-5.5", true, 1.0),
        ]);
        assert_eq!(opencode.models.len(), 1);
        assert_eq!(
            opencode.models[0].cost,
            Field::Unavailable(Unavailable::Incomplete)
        );
        let opencode = from_opencode(&[
            opencode_message("gpt-5.5", true, 1.0),
            opencode_message("gpt-5.5", true, 0.0),
        ]);
        assert_eq!(
            opencode.models[0].cost,
            Field::Unavailable(Unavailable::Incomplete)
        );
        // A completed reply that reported zero tokens and a zero cost leaves
        // a subscription total a subscription total.
        let opencode = from_opencode(&[
            opencode_message("gpt-5.5", false, 0.0),
            opencode_message("gpt-5.5", true, 0.0),
        ]);
        assert_eq!(
            opencode.models[0].cost,
            Field::Unavailable(Unavailable::SubscriptionZero)
        );
        let rolled = rollup([&RunRecord::new(
            identity("r", "opencode"),
            from_opencode(&[
                opencode_message("gpt-5.5", true, 0.0),
                opencode_message("gpt-5.5", true, 1.0),
            ]),
            Some(EndReason::Exited),
        )]);
        assert!(rolled.cost.is_empty(), "{:?}", rolled.cost);
        assert_eq!(rolled.cost_unavailable, 1);
    }

    #[test]
    fn opencode_subscription_zero_is_unavailable_not_free() {
        let subscription = from_opencode(&[
            opencode_message("gpt-5.5", true, 0.0),
            opencode_message("gpt-5.5", true, 0.0),
        ]);
        assert_eq!(
            subscription.models[0].cost,
            Field::Unavailable(Unavailable::SubscriptionZero)
        );
        assert_eq!(
            subscription.models[0].tokens,
            Field::ProviderReported(Tokens {
                input: 24,
                output: 6,
                cache_read: 14,
                cache_write: 0
            })
        );
        let priced = from_opencode(&[opencode_message("gpt-5.5", true, 0.5)]);
        assert_eq!(priced.models[0].cost, Field::Estimated(MicroUsd(500_000)));
    }

    /// Only zero tokens with an explicitly reported zero cost adds nothing;
    /// a tokenless reply whose cost was not recorded is not a zero, so beside
    /// a priced reply it makes the model's cost incomplete.
    #[test]
    fn a_missing_cost_beside_a_priced_reply_is_incomplete_not_zero() {
        let tokenless = |cost| PiReplyUsage {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            ..pi_reply("gpt-5.5", cost, Some(PiStopReason::Stop))
        };
        let priced = pi_reply("gpt-5.5", Some(1.0), Some(PiStopReason::Stop));
        let missing = from_pi(&[tokenless(None), priced.clone()]);
        assert_eq!(
            missing.models[0].cost,
            Field::Unavailable(Unavailable::Incomplete)
        );
        let zero = from_pi(&[tokenless(Some(0.0)), priced]);
        assert_eq!(zero.models[0].cost, Field::Estimated(MicroUsd(1_000_000)));
    }

    /// A message with no usage (a failed or never-completed request, whose
    /// counters OpenCode left at their initial 0) has its tokens and cost
    /// `not_recorded`, apart from a completed reply that reported zero
    /// tokens, which is a provider-reported zero; mixed with reported
    /// usage, the model's tokens and cost are incomplete.
    #[test]
    fn opencode_absent_usage_is_not_recorded_and_a_reported_zero_is_zero() {
        let absent = from_opencode(&[opencode_unrecorded("gpt-5.5")]);
        assert_eq!(absent.models.len(), 1);
        assert_eq!(absent.models[0].model, Some(id("gpt-5.5")));
        assert_eq!(
            absent.models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
        assert_eq!(
            absent.models[0].cost,
            Field::Unavailable(Unavailable::NotRecorded)
        );

        let reported_zero = from_opencode(&[opencode_message("gpt-5.5", false, 0.0)]);
        assert_eq!(
            reported_zero.models[0].tokens,
            Field::ProviderReported(Tokens::default())
        );
        assert_eq!(reported_zero.models[0].cost, Field::Estimated(MicroUsd(0)));

        let mixed = from_opencode(&[
            opencode_unrecorded("gpt-5.5"),
            opencode_message("gpt-5.5", true, 0.0),
        ]);
        assert_eq!(
            mixed.models[0].tokens,
            Field::Unavailable(Unavailable::Incomplete)
        );
        assert_eq!(
            mixed.models[0].cost,
            Field::Unavailable(Unavailable::Incomplete)
        );
        let rolled = rollup([&RunRecord::new(
            identity("r", "opencode"),
            from_opencode(&[opencode_unrecorded("gpt-5.5")]),
            Some(EndReason::Exited),
        )]);
        assert!(rolled.tokens.is_empty(), "{:?}", rolled.tokens);
        assert_eq!(rolled.tokens_unavailable, 1);
    }

    #[test]
    fn a_normaliser_tracks_at_most_max_models_named_models() {
        let messages: Vec<OpenCodeMessageUsage> = (0..MAX_MODELS + 5)
            .map(|i| opencode_message(&format!("m{i}"), true, 0.5))
            .collect();
        let usage = from_opencode(&messages);
        let named = usage.models.iter().filter(|m| m.model.is_some()).count();
        assert_eq!(named, MAX_MODELS);
        let other = usage.models.iter().find(|m| m.model.is_none()).unwrap();
        assert_eq!(other.tokens.value().unwrap().output, 3 * 5);
        assert_eq!(other.cost, Field::Estimated(MicroUsd(2_500_000)));
    }

    /// A Codex summary built by hand, not by the reader, with more named
    /// models than the cap and an unnamed bucket already in it: the
    /// normaliser keeps at most [`MAX_MODELS`] named models and one unnamed
    /// one, with the total conserved, however often it is called.
    #[test]
    fn the_codex_normaliser_caps_a_summary_built_by_hand() {
        let mut summary = CodexRolloutSummary::default();
        summary.tokens_by_model.insert(None, tokens(1000, 100));
        let n = MAX_MODELS as u64 + 8;
        for i in 0..n {
            summary
                .tokens_by_model
                .insert(Some(id(&format!("m{i:02}"))), tokens(10, 1));
        }
        let total = |models: &[ModelUsage]| {
            let mut sum = Tokens::default();
            for m in models {
                sum.add(m.tokens.value().unwrap());
            }
            sum
        };
        for _ in 0..3 {
            let usage = from_codex(&summary);
            let named = usage.models.iter().filter(|m| m.model.is_some()).count();
            let unnamed: Vec<&ModelUsage> =
                usage.models.iter().filter(|m| m.model.is_none()).collect();
            assert_eq!(named, MAX_MODELS);
            assert_eq!(unnamed.len(), 1);
            assert_eq!(
                unnamed[0].tokens,
                Field::ProviderReported(tokens(1000 + 80, 100 + 8))
            );
            assert_eq!(total(&usage.models), tokens(1000 + 10 * n, 100 + n));
            assert!(
                usage
                    .models
                    .iter()
                    .all(|m| m.cost == Field::Unavailable(Unavailable::NoChannel))
            );
        }
    }

    // --- the incremental reader ---

    #[test]
    fn the_reader_credits_each_models_growth_and_reads_only_what_was_appended() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout-a.jsonl");
        let mut file = File::create(&path).unwrap();
        write_lines(
            &mut file,
            &[
                &turn_context("gpt-5.5"),
                task_started(),
                &token_count(100, 40, 10, 26.0, 99),
                &token_count(100, 40, 10, 26.0, 99),
                task_complete(),
            ],
        );
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
        let first_bytes = reader.stats().bytes_read;
        assert_eq!(reader.stats().parsed, 5);
        write_lines(
            &mut file,
            &[
                &turn_context("gpt-5.5-mini"),
                task_started(),
                &token_count(150, 60, 12, 45.0, 99),
            ],
        );
        reader.poll().unwrap();
        let appended = std::fs::metadata(&path).unwrap().len() - first_bytes;
        assert_eq!(reader.stats().bytes_read - first_bytes, appended);
        let summary = reader.summary();
        assert_eq!(
            summary.tokens_by_model.get(&Some(id("gpt-5.5"))),
            Some(&Tokens {
                input: 100,
                output: 10,
                cache_read: 40,
                cache_write: 0
            })
        );
        assert_eq!(
            summary.tokens_by_model.get(&Some(id("gpt-5.5-mini"))),
            Some(&Tokens {
                input: 50,
                output: 2,
                cache_read: 20,
                cache_write: 0
            })
        );
        assert_eq!(summary.last_turn, Some(AgentTurnEnd::MidTurn));
        let usage = from_codex(summary);
        assert_eq!(usage.quota.value().unwrap().from_bp, 2600);
        assert_eq!(usage.quota.value().unwrap().to_bp, 4500);
        reader.poll().unwrap();
        assert_eq!(
            reader.stats().bytes_read - first_bytes,
            appended,
            "nothing new"
        );
        assert_eq!(reader.stats().backlog, 0);
    }

    #[test]
    fn a_partial_trailing_line_waits_for_its_newline() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout-b.jsonl");
        let mut file = File::create(&path).unwrap();
        let line = token_count(100, 0, 10, 1.0, 5);
        let (a, b) = line.split_at(line.len() / 2);
        file.write_all(a.as_bytes()).unwrap();
        file.flush().unwrap();
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
        assert_eq!(reader.stats().lines, 0);
        assert!(reader.summary().tokens_by_model.is_empty());
        file.write_all(b.as_bytes()).unwrap();
        file.write_all(b"\r\n").unwrap();
        file.flush().unwrap();
        reader.poll().unwrap();
        assert_eq!(reader.stats().parsed, 1);
        assert_eq!(reader.stats().malformed, 0);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(100, 10)),
            "usage before any turn_context has no model"
        );
    }

    #[test]
    fn malformed_and_overlong_lines_are_counted_and_skipped() {
        let overlong = format!(
            r#"{{"type":"event_msg","payload":{{"type":"token_count","pad":"{}"}}}}"#,
            "x".repeat(MAX_LINE_BYTES)
        );
        let reader = read(&[
            r#"{"type":"event_msg","payload":{"type":"token_count","info":"#.into(),
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":"many"}}}}"#.into(),
            "not json at all".into(),
            overlong,
            token_count(7, 0, 1, 1.0, 5),
        ]);
        let stats = reader.stats();
        assert_eq!(stats.malformed, 2);
        assert_eq!(stats.overlong, 1);
        assert_eq!(stats.parsed, 1);
        assert_eq!(stats.lines, 5);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(7, 1))
        );
    }

    /// An incomplete total between two valid ones is skipped without moving
    /// the baseline, so the valid totals are not counted twice: 100 then
    /// `{}` then 110 is 110, not 210.
    #[test]
    fn an_incomplete_running_total_does_not_reset_the_baseline() {
        for incomplete in [
            "{}",
            r#"{"output_tokens":10}"#,
            r#"{"input_tokens":100}"#,
            r#"{"input_tokens":null,"output_tokens":10}"#,
        ] {
            let reader = read(&[
                token_count(100, 0, 10, 1.0, 5),
                token_count_with(incomplete, 1.0, 5, "prolite"),
                token_count(110, 0, 11, 1.0, 5),
            ]);
            assert_eq!(
                reader.summary().tokens_by_model.get(&None),
                Some(&tokens(110, 11)),
                "{incomplete}"
            );
            assert_eq!(reader.stats().unusable_totals, 1, "{incomplete}");
            assert_eq!(reader.stats().regressions, 0, "{incomplete}");
            assert!(!reader.summary().tokens_incomplete, "{incomplete}");
        }
    }

    /// A cache count left out after it was non-zero is incomplete; left out
    /// while it was zero, it reads as zero.
    #[test]
    fn an_omitted_cache_count_is_incomplete_only_when_it_was_not_zero() {
        let reader = read(&[
            token_count(100, 40, 10, 1.0, 5),
            token_count_with(
                r#"{"input_tokens":120,"output_tokens":12}"#,
                1.0,
                5,
                "prolite",
            ),
            token_count(130, 50, 13, 1.0, 5),
        ]);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&Tokens {
                input: 130,
                output: 13,
                cache_read: 50,
                cache_write: 0
            })
        );
        assert_eq!(reader.stats().unusable_totals, 1);

        let reader = read(&[
            token_count_with(
                r#"{"input_tokens":100,"output_tokens":10}"#,
                1.0,
                5,
                "prolite",
            ),
            token_count(130, 50, 13, 1.0, 5),
        ]);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&Tokens {
                input: 130,
                output: 13,
                cache_read: 50,
                cache_write: 0
            })
        );
        assert_eq!(reader.stats().unusable_totals, 0);
    }

    /// A total whose cache parts exceed the input that includes them is not a
    /// count: valid, then such a total with a lower input, then valid again
    /// is the last valid total, with the baseline never moved by the middle
    /// one, and the run's tokens stay provider-reported.
    #[test]
    fn an_inconsistent_total_is_skipped_without_moving_the_baseline() {
        for inconsistent in [
            usage(0, 40, 10),
            r#"{"input_tokens":30,"cached_input_tokens":20,"cache_write_input_tokens":20,"output_tokens":10}"#.into(),
        ] {
            let reader = read(&[
                token_count(100, 40, 10, 1.0, 5),
                token_count_with(&inconsistent, 1.0, 5, "prolite"),
                token_count(110, 50, 11, 1.0, 5),
            ]);
            let want = Tokens {
                input: 110,
                output: 11,
                cache_read: 50,
                cache_write: 0,
            };
            assert_eq!(
                reader.summary().tokens_by_model.get(&None),
                Some(&want),
                "{inconsistent}"
            );
            assert_eq!(reader.stats().unusable_totals, 1, "{inconsistent}");
            assert_eq!(reader.stats().regressions, 0, "{inconsistent}");
            assert_eq!(
                from_codex(reader.summary()).models[0].tokens,
                Field::ProviderReported(want),
                "{inconsistent}"
            );
        }
    }

    /// A usable total lower than the one before, in input or in any other
    /// count, may be a counter restart or a wrong total, and the reader takes
    /// nothing in the rollout as saying which: the run's token total becomes
    /// incomplete, and stays so, rather than adding either reading.
    #[test]
    fn a_lower_running_total_makes_the_token_total_incomplete() {
        for lower in [usage(30, 0, 3), usage(510, 0, 40), usage(500, 0, 50)] {
            let mut lines = vec![token_count(500, 10, 50, 1.0, 5)];
            lines.push(token_count_with(&lower, 1.0, 5, "prolite"));
            lines.push(token_count(520, 10, 52, 1.0, 5));
            let reader = read(&lines);
            assert!(reader.summary().tokens_incomplete, "{lower}");
            assert_eq!(reader.stats().regressions, 1, "{lower}");
            assert_eq!(reader.stats().unusable_totals, 0, "{lower}");
            let usage = from_codex(reader.summary());
            assert!(!usage.models.is_empty());
            for m in &usage.models {
                assert_eq!(
                    m.tokens,
                    Field::Unavailable(Unavailable::Incomplete),
                    "{lower}"
                );
                assert_eq!(m.cost, Field::Unavailable(Unavailable::NoChannel));
            }
        }
    }

    /// The one start-over the reader recognises: the file itself got shorter
    /// or was replaced, so it is read as a new rollout with the summary reset,
    /// and its lower totals are not a regression.
    #[test]
    fn a_truncated_or_replaced_rollout_is_read_again_from_the_start() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout-d.jsonl");
        let mut file = File::create(&path).unwrap();
        write_lines(
            &mut file,
            &[
                &token_count(500, 0, 50, 1.0, 5),
                &token_count(900, 0, 90, 2.0, 5),
            ],
        );
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(900, 90))
        );

        let mut file = File::create(&path).unwrap();
        write_lines(&mut file, &[&token_count(3, 0, 1, 1.0, 5)]);
        reader.poll().unwrap();
        assert_eq!(reader.stats().restarts, 1);
        assert_eq!(reader.stats().regressions, 0);
        assert!(!reader.summary().tokens_incomplete);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(3, 1))
        );

        // Replaced by another file at the same path, longer than what was read.
        let other = dir.path().join("rollout-e.jsonl");
        let mut file = File::create(&other).unwrap();
        write_lines(
            &mut file,
            &[
                &token_count(20, 0, 2, 1.0, 5),
                &token_count(40, 0, 4, 1.0, 5),
                &token_count(60, 0, 6, 1.0, 5),
            ],
        );
        std::fs::rename(&other, &path).unwrap();
        reader.poll().unwrap();
        #[cfg(unix)]
        {
            assert_eq!(reader.stats().restarts, 2);
            assert_eq!(
                reader.summary().tokens_by_model.get(&None),
                Some(&tokens(60, 6))
            );
        }
    }

    #[test]
    fn a_failed_and_an_aborted_turn_are_read_by_kind_only() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout-f.jsonl");
        let mut file = File::create(&path).unwrap();
        write_lines(
            &mut file,
            &[
                task_started(),
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"u1","error":{"message":"overloaded, try later","codex_error_info":"server_overloaded"}}}"#,
            ],
        );
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
        assert_eq!(
            reader.summary().last_turn,
            Some(AgentTurnEnd::Failed {
                kind: Some(id("server_overloaded"))
            })
        );
        write_lines(
            &mut file,
            &[
                task_started(),
                r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"u2","reason":"interrupted"}}"#,
            ],
        );
        reader.poll().unwrap();
        assert_eq!(reader.summary().last_turn, Some(AgentTurnEnd::Interrupted));
    }

    /// More distinct models than [`MAX_MODELS`], across several polls: the
    /// named set stops growing, the rest is credited to no model, and the
    /// total still equals the rollout's own.
    #[test]
    fn model_cardinality_is_capped_across_polls() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout-m.jsonl");
        let mut file = File::create(&path).unwrap();
        let mut reader = CodexUsageReader::new(&path);
        let extra = 8;
        for i in 0..(MAX_MODELS + extra) as u64 {
            write_lines(
                &mut file,
                &[
                    &turn_context(&format!("model-{i}")),
                    &token_count(10 * (i + 1), 0, i + 1, 1.0, 5),
                ],
            );
            if i % 5 == 0 {
                reader.poll().unwrap();
            }
        }
        reader.poll().unwrap();
        let summary = reader.summary();
        let named = summary
            .tokens_by_model
            .keys()
            .filter(|k| k.is_some())
            .count();
        assert_eq!(named, MAX_MODELS);
        assert_eq!(reader.stats().models_overflowed, extra as u64);
        assert_eq!(
            summary.tokens_by_model[&None],
            tokens(10 * extra as u64, extra as u64)
        );
        let mut sum = Tokens::default();
        for t in summary.tokens_by_model.values() {
            sum.add(t);
        }
        let n = (MAX_MODELS + extra) as u64;
        assert_eq!(sum, tokens(10 * n, n));
    }

    /// The reader's `Debug` shows its counters, not the bytes of a line it
    /// is still waiting on, nor the path it was given.
    #[test]
    fn the_readers_debug_shows_neither_buffered_bytes_nor_its_path() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join(format!("{SENTINEL}-rollout.jsonl"));
        let mut file = File::create(&path).unwrap();
        file.write_all(
            format!(r#"{{"type":"response_item","payload":{{"text":"{SENTINEL}"#).as_bytes(),
        )
        .unwrap();
        file.flush().unwrap();
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
        assert!(
            reader
                .partial
                .windows(SENTINEL.len())
                .any(|w| w == SENTINEL.as_bytes())
        );
        let debug = format!("{reader:?}");
        assert!(!debug.contains(SENTINEL), "{debug}");
        assert!(!debug.contains("rollout.jsonl"), "{debug}");
        assert!(debug.contains("partial_bytes"), "{debug}");
    }

    /// A FIFO with no writer would block a plain open forever; the reader
    /// refuses it at once, as it refuses anything but a regular file.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = crate::test_temp::tempdir().unwrap();
        let fifo = dir.path().join("rollout-fifo.jsonl");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstring");
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `mkfifo` only reads through it.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", io::Error::last_os_error());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = CodexUsageReader::new(&fifo);
            let _ = tx.send(reader.poll().map_err(|e| e.kind()));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the poll blocked on a FIFO");
        assert_eq!(got, Err(io::ErrorKind::InvalidInput));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused() {
        let dir = crate::test_temp::tempdir().unwrap();
        let real = dir.path().join("rollout-real.jsonl");
        let mut file = File::create(&real).unwrap();
        write_lines(&mut file, &[&token_count(7, 0, 1, 1.0, 5)]);
        let link = dir.path().join("rollout-link.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut reader = CodexUsageReader::new(&link);
        let err = reader.poll().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "{err}");
        assert_eq!(reader.stats().bytes_read, 0);
        assert!(reader.summary().tokens_by_model.is_empty());
    }

    /// The policy today: only the final component is refused as a symlink,
    /// and a symlinked parent directory is followed. Confining the reader to
    /// a trusted root is #1858's decision; this test changes with it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_is_followed() {
        let dir = crate::test_temp::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let mut file = File::create(real.join("rollout.jsonl")).unwrap();
        write_lines(&mut file, &[&token_count(7, 0, 1, 1.0, 5)]);
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let mut reader = CodexUsageReader::new(linked.join("rollout.jsonl"));
        reader.poll().unwrap();
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(7, 1))
        );
    }

    /// Budget cuts landing inside a line longer than [`MAX_LINE_BYTES`]:
    /// the buffer never holds more than that, the line is counted once as
    /// overlong and skipped through its newline over later polls, and the
    /// lines on either side of it are read.
    #[test]
    fn a_budget_cut_inside_an_overlong_line_skips_it_across_polls() {
        let overlong = format!(
            r#"{{"type":"event_msg","payload":{{"type":"token_count","pad":"{}"}}}}"#,
            "x".repeat(MAX_LINE_BYTES + 40 * 1024)
        );
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout-cut.jsonl");
        let mut file = File::create(&path).unwrap();
        write_lines(
            &mut file,
            &[
                &token_count(10, 0, 1, 1.0, 5),
                &overlong,
                &token_count(20, 0, 2, 1.0, 5),
            ],
        );
        let mut reader = CodexUsageReader::new(&path).with_poll_budget(100 * 1024);
        let mut polls = 0;
        loop {
            reader.poll().unwrap();
            polls += 1;
            assert!(reader.partial.len() <= MAX_LINE_BYTES);
            if reader.stats().backlog == 0 {
                break;
            }
            assert!(polls < 100, "the backlog never drained");
        }
        assert!(polls >= 3, "{polls} polls");
        let stats = reader.stats();
        assert_eq!(stats.overlong, 1);
        assert_eq!(stats.parsed, 2);
        assert_eq!(stats.malformed, 0);
        assert_eq!(stats.lines, 3);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(20, 2))
        );
    }

    /// A backlog larger than the budget is drained over several polls, none
    /// reading more than its budget, with lines cut by a budget completed on
    /// the next poll, and the total the same as one unbounded read.
    #[test]
    fn a_large_backlog_is_drained_over_bounded_polls() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("rollout-big.jsonl");
        let mut file = File::create(&path).unwrap();
        let lines: Vec<String> = (1..=400u64)
            .map(|i| token_count(10 * i, 0, i, 1.0, 5))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_lines(&mut file, &refs);
        let budget = 8 * 1024;
        let mut reader = CodexUsageReader::new(&path).with_poll_budget(budget);
        let mut polls = 0u64;
        loop {
            let before = reader.stats().bytes_read;
            reader.poll().unwrap();
            polls += 1;
            assert!(reader.stats().bytes_read - before <= budget);
            if reader.stats().backlog == 0 {
                break;
            }
            assert!(polls < 1_000, "the backlog never drained");
        }
        assert!(polls > 10, "{polls} polls");
        assert_eq!(reader.stats().polls_cut_short, polls - 1);
        assert_eq!(reader.stats().parsed, 400);
        assert_eq!(reader.stats().malformed, 0);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(4000, 400))
        );
        write_lines(&mut file, &[&token_count(4010, 0, 401, 1.0, 5)]);
        reader.poll().unwrap();
        assert_eq!(reader.stats().backlog, 0);
        assert_eq!(
            reader.summary().tokens_by_model.get(&None),
            Some(&tokens(4010, 401))
        );
    }

    // --- leakage ---

    const SENTINEL: &str = "SENTINEL7f3a9c";

    fn contains_sentinel(attrs: &[AccountingAttribute]) -> bool {
        attrs.iter().any(|a| match &a.value {
            AttrValue::Str(s) => s.contains(SENTINEL),
            _ => false,
        })
    }

    /// Content-bearing records carrying a sentinel — a prompt, a reply, tool
    /// input and output, an error message, developer instructions, compacted
    /// history — produce a summary, a record and attributes with no trace of
    /// it. Each content line, `session_meta` and the tool call included, also
    /// holds a string that is exactly one of the pre-filter's markers, so it
    /// is deserialised rather than skipped.
    #[test]
    fn content_never_reaches_the_summary_the_record_or_the_attributes() {
        let s = SENTINEL;
        let reader = read(&[
            format!(
                r#"{{"type":"session_meta","payload":{{"cwd":"/{s}","base_instructions":{{"text":"{s}"}},"originator":"task_started","cli_version":"0.164.0"}}}}"#
            ),
            format!(
                r#"{{"type":"turn_context","payload":{{"model":"gpt-5.5","cwd":"/{s}","collaboration_mode":{{"settings":{{"developer_instructions":"{s}"}}}}}}}}"#
            ),
            format!(
                r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{s}"}},{{"type":"input_text","text":"token_count"}},{{"type":"input_text","text":"turn_context"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec","arguments":"{{\"cmd\":\"cat {s}\"}}","call_id":"turn_context","model":"{s}"}}}}"#
            ),
            format!(
                r#"{{"type":"response_item","payload":{{"type":"function_call_output","output":[{{"type":"text","text":"{s}"}},{{"type":"text","text":"task_complete"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"event_msg","payload":{{"type":"item_completed","item":{{"command":["{s}"],"stdout":"token_count","aggregated_output":"{s}"}}}}}}"#
            ),
            format!(
                r#"{{"type":"compacted","payload":{{"message":"{s}","retained_context":{{"user_messages":[{{"text":"{s}"}},{{"text":"turn_aborted"}}]}}}}}}"#
            ),
            token_count(1000, 400, 50, 30.0, 77),
            format!(
                r#"{{"type":"event_msg","payload":{{"type":"task_complete","last_agent_message":"{s}","error":{{"message":"{s}","codex_error_info":"{s} is not an identifier"}}}}}}"#
            ),
        ]);
        assert_eq!(reader.stats().parsed, 9, "every line was deserialised");
        assert_eq!(reader.stats().malformed, 0);
        let summary = reader.summary().clone();
        assert!(!format!("{summary:?}").contains(SENTINEL), "{summary:?}");
        assert_eq!(
            summary.last_turn,
            Some(AgentTurnEnd::Failed { kind: None }),
            "a failure is kept, its non-identifier kind is not"
        );
        let record = RunRecord::new(
            identity("r1", "codex"),
            from_codex(&summary),
            Some(EndReason::Exited),
        );
        assert!(!format!("{record:?}").contains(SENTINEL), "{record:?}");
        let export = run_attributes(&record);
        assert!(!contains_sentinel(&export.run));
        assert!(export.models.iter().all(|m| !contains_sentinel(m)));
        assert!(export.models[0].contains(&AccountingAttribute {
            key: AccountingKey::InputTokens,
            value: AttrValue::Int(1000),
        }));
    }

    /// What the identifier check keeps and drops. Path-, email- and
    /// credential-shaped values are kept: the check is about shape, so it is
    /// no privacy filter. Unicode, a line break, a space, a quote, a
    /// backslash, the empty string and 129 bytes are dropped; 128 are kept.
    #[test]
    fn the_identifier_check_is_a_shape_check_not_a_privacy_filter() {
        let at_limit = "a".repeat(Ident::MAX_BYTES);
        let over_limit = "a".repeat(Ident::MAX_BYTES + 1);
        for kept in [
            format!("/home/{SENTINEL}/.ssh/id_ed25519"),
            format!("{SENTINEL}@example.com"),
            format!("sk-{SENTINEL}0123456789abcdef"),
            at_limit,
        ] {
            assert!(Ident::new(&kept).is_some(), "{kept}");
        }
        for dropped in [
            format!("{SENTINEL}\u{e9}"),
            format!("{SENTINEL}\u{2028}x"),
            format!("{SENTINEL}\nnext"),
            format!("{SENTINEL} with spaces"),
            format!("{SENTINEL}\"quoted"),
            format!("{SENTINEL}\\"),
            String::new(),
            over_limit,
        ] {
            assert!(Ident::new(&dropped).is_none(), "{dropped:?}");
        }
    }

    /// Strings reach the record only as identifiers, on every path a string
    /// enters by. Through the reader, a JSON escape is judged after decoding
    /// (an escaped hyphen or underscore is kept, an escaped line break is
    /// not) and the 128/129-byte boundary holds, for each of the three string
    /// fields it keeps: the turn's model, the plan and the error kind.
    /// Through each normaliser that takes a raw model name (Claude Code, Pi,
    /// OpenCode), a space, a line break and 129 bytes are dropped and 128
    /// kept. A run id or agent is an `Ident` before a record can hold it. An
    /// identifier-shaped plan or model is kept, which is the residual risk
    /// the doc names.
    #[test]
    fn strings_reach_the_record_only_as_identifiers() {
        let at_limit = "a".repeat(Ident::MAX_BYTES);
        let over_limit = "b".repeat(Ident::MAX_BYTES + 1);

        // The model, through the reader.
        let reader = read(&[
            turn_context(r"gpt-5.5"),
            token_count(10, 0, 1, 1.0, 5),
            turn_context(&format!(r"{SENTINEL}\u000aline")),
            token_count(20, 0, 2, 1.0, 5),
            turn_context(&format!(r"{SENTINEL}é")),
            token_count(30, 0, 3, 1.0, 5),
            turn_context(&over_limit),
            token_count(40, 0, 4, 1.0, 5),
            turn_context(&at_limit),
            token_count_with(&usage(50, 0, 5), 1.0, 5, &format!("{SENTINEL}@example.com")),
        ]);
        let summary = reader.summary();
        let keys: Vec<Option<Ident>> = summary.tokens_by_model.keys().cloned().collect();
        assert_eq!(keys, vec![None, Some(id(&at_limit)), Some(id("gpt-5.5"))]);
        assert_eq!(summary.tokens_by_model[&None], tokens(30, 3));
        assert_eq!(
            summary.quota_last.as_ref().unwrap().plan,
            Some(id(&format!("{SENTINEL}@example.com")))
        );

        // The plan and the error kind, through the reader.
        let cases = [
            (r"pro_lite".to_owned(), Some("pro_lite".to_owned())),
            (format!(r"{SENTINEL}\u000aline"), None),
            (format!("{SENTINEL} space"), None),
            (at_limit.clone(), Some(at_limit.clone())),
            (over_limit.clone(), None),
        ];
        for (raw, want) in &cases {
            let reader = read(&[
                token_count_with(&usage(10, 0, 1), 1.0, 5, raw),
                format!(
                    r#"{{"type":"event_msg","payload":{{"type":"task_complete","error":{{"message":"m","codex_error_info":"{raw}"}}}}}}"#
                ),
            ]);
            let summary = reader.summary();
            let want = want.as_deref().map(id);
            assert_eq!(summary.quota_last.as_ref().unwrap().plan, want, "{raw}");
            assert_eq!(
                summary.last_turn,
                Some(AgentTurnEnd::Failed { kind: want }),
                "{raw}"
            );
        }

        // The model, through every normaliser that takes a raw name.
        let cases = [
            (format!("{SENTINEL} with space"), None),
            (format!("{SENTINEL}\nline"), None),
            (over_limit.clone(), None),
            (at_limit.clone(), Some(id(&at_limit))),
            (
                format!("/home/{SENTINEL}/.ssh/id"),
                Some(id(&format!("/home/{SENTINEL}/.ssh/id"))),
            ),
        ];
        for (raw, want) in &cases {
            let claude = from_claude(Some(&ClaudeSessionUsage {
                models: vec![ClaudeModelUsage {
                    model: raw.clone(),
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cost_usd: Some(0.1),
                }],
                has_unknown_model_cost: false,
                last_turn: None,
            }));
            let pi = from_pi(&[pi_reply(raw, Some(0.1), None)]);
            let opencode = from_opencode(&[opencode_message(raw, true, 0.1)]);
            for (agent, got) in [("claude", claude), ("pi", pi), ("opencode", opencode)] {
                assert_eq!(got.models.len(), 1, "{agent} {raw:?}");
                assert_eq!(&got.models[0].model, want, "{agent} {raw:?}");
            }
        }

        // A run id and an agent: no `Ident`, no record.
        for hostile in [
            format!("{SENTINEL}\nline"),
            format!("{SENTINEL} space"),
            over_limit.clone(),
        ] {
            assert!(Ident::new(&hostile).is_none(), "{hostile:?}");
        }
        let export = run_attributes(&RunRecord::new(
            RunIdentity::new(id(&at_limit), id("codex")),
            from_codex(&CodexRolloutSummary::default()),
            None,
        ));
        assert!(has(&export.run, AccountingKey::RunId, &at_limit));
    }

    /// Every string value the export can emit is an enum's name or an
    /// [`Ident`], for records built by the normalisers from hostile input and
    /// for a record built field by field by hand, since an `Ident` has no
    /// other constructor than its check.
    #[test]
    fn every_exported_string_is_identifier_shaped() {
        let hostile = format!("{SENTINEL}\n\"quoted\" é");
        let by_hand = RunRecord {
            identity: RunIdentity {
                parent_run_id: Ident::new(&hostile),
                usage_included_in: Some(id("p")),
                orchestration_id: Some(id("o1")),
                work_item: Some(id("issue-1")),
                ..identity("r1", "claude")
            },
            models: vec![ModelUsage {
                model: Ident::new(&hostile),
                tokens: Field::Measured(tokens(1, 1)),
                cost: Field::AgentAsserted(MicroUsd(1)),
            }],
            quota: Field::ProviderReported(QuotaDelta {
                plan: Ident::new(&hostile),
                window_minutes: 1,
                resets_at: -1,
                from_bp: 1,
                to_bp: 2,
            }),
            termination: Field::Measured(Termination {
                outcome: RunTermination::Failed,
                error_type: Ident::new(&hostile),
            }),
            last_turn: Field::ProviderReported(AgentTurnEnd::Failed {
                kind: Ident::new(&hostile),
            }),
        };
        let records = [
            by_hand,
            RunRecord::new(
                identity("r2", "pi"),
                from_pi(&[pi_reply(&hostile, Some(0.5), Some(PiStopReason::Error))]),
                Some(EndReason::Closed),
            ),
            RunRecord::new(
                identity("r3", "claude"),
                from_claude(Some(&ClaudeSessionUsage {
                    models: vec![ClaudeModelUsage {
                        model: hostile.clone(),
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cost_usd: Some(0.1),
                    }],
                    has_unknown_model_cost: false,
                    last_turn: Some(AgentTurnEnd::Failed {
                        kind: Ident::new(&hostile),
                    }),
                })),
                Some(EndReason::Exited),
            ),
            RunRecord::new(
                identity("r4", "opencode"),
                from_opencode(&[
                    opencode_message(&hostile, true, 0.5),
                    opencode_unrecorded(&hostile),
                ]),
                Some(EndReason::Exited),
            ),
        ];
        let item = WorkItemRecord {
            disposition: work_disposition(None, None),
            checks: observed_checks(None, Some(false)),
        };
        let mut seen = 0;
        let mut check = |attrs: &[AccountingAttribute]| {
            for attr in attrs {
                if let AttrValue::Str(value) = &attr.value {
                    seen += 1;
                    assert!(Ident::new(value).is_some(), "{value:?}");
                    assert!(!value.contains(SENTINEL), "{value:?}");
                }
            }
        };
        for record in &records {
            let export = run_attributes(record);
            check(&export.run);
            export.models.iter().for_each(|m| check(m));
        }
        check(&work_item_attributes(&item));
        assert!(seen > 20, "{seen}");
    }

    // --- aggregation ---

    #[test]
    fn a_retry_is_a_second_attempt_and_a_repeated_run_id_counts_once() {
        let mut first = run(
            RunIdentity {
                work_item: Some(id("issue-1676")),
                ..identity("a1", "claude")
            },
            "claude-opus-5-5",
            tokens(100, 10),
            Field::Estimated(MicroUsd(3_000_000)),
        );
        let second = run(
            RunIdentity {
                work_item: Some(id("issue-1676")),
                ..identity("a2", "claude")
            },
            "claude-opus-5-5",
            tokens(300, 30),
            Field::Estimated(MicroUsd(10_000_000)),
        );
        let early_snapshot = first.clone();
        first.models[0].tokens = Field::ProviderReported(tokens(150, 15));
        let other = run(
            identity("b1", "claude"),
            "claude-opus-5-5",
            tokens(1, 1),
            Field::Estimated(MicroUsd(1)),
        );
        let runs = [early_snapshot, second, first, other];
        let r = rollup_work_item(&runs, "issue-1676");
        assert_eq!(r.runs, 2);
        assert_eq!(
            r.tokens[&(Some(id("claude-opus-5-5")), Provenance::ProviderReported)],
            tokens(450, 45),
            "the later snapshot of a1 replaces the earlier one"
        );
        assert_eq!(r.cost[&Provenance::Estimated], MicroUsd(13_000_000));
        assert_eq!(r.terminations[&RunTermination::Completed], 2);
    }

    /// A run first attributed to one work item (and orchestration) and then
    /// corrected to another counts under the corrected one only.
    #[test]
    fn a_corrected_membership_counts_under_the_later_snapshot_only() {
        let snapshot = |work_item: &str, orchestration: &str, t| {
            run(
                RunIdentity {
                    work_item: Some(id(work_item)),
                    orchestration_id: Some(id(orchestration)),
                    ..identity("r", "codex")
                },
                "gpt-5.5",
                t,
                Field::Unavailable(Unavailable::NoChannel),
            )
        };
        let runs = [
            snapshot("issue-a", "o-a", tokens(10, 1)),
            snapshot("issue-b", "o-b", tokens(20, 2)),
        ];
        let a = rollup_work_item(&runs, "issue-a");
        assert_eq!(a.runs, 0);
        assert!(a.tokens.is_empty());
        let b = rollup_work_item(&runs, "issue-b");
        assert_eq!(b.runs, 1);
        assert_eq!(
            b.tokens[&(Some(id("gpt-5.5")), Provenance::ProviderReported)],
            tokens(20, 2)
        );
        assert_eq!(rollup_orchestration(&runs, "o-a").runs, 0);
        assert_eq!(rollup_orchestration(&runs, "o-b").runs, 1);
    }

    #[test]
    fn a_claude_subagent_is_counted_in_its_parent_and_a_child_codex_run_is_not() {
        let parent = run(
            identity("p", "claude"),
            "claude-opus-5-5",
            tokens(1000, 100),
            Field::Estimated(MicroUsd(9)),
        );
        let subagent = run(
            RunIdentity {
                parent_run_id: Some(id("p")),
                usage_included_in: Some(id("p")),
                ..identity("p-sub", "claude")
            },
            "claude-haiku-5-5",
            tokens(200, 20),
            Field::Estimated(MicroUsd(1)),
        );
        let child = run(
            RunIdentity {
                parent_run_id: Some(id("p")),
                ..identity("c", "codex")
            },
            "gpt-5.5",
            tokens(50, 5),
            Field::Unavailable(Unavailable::NoChannel),
        );
        let r = rollup([&parent, &subagent, &child]);
        assert_eq!(r.runs, 3);
        assert_eq!(r.runs_included_elsewhere, 1);
        assert!(
            !r.tokens
                .contains_key(&(Some(id("claude-haiku-5-5")), Provenance::ProviderReported))
        );
        assert_eq!(r.cost[&Provenance::Estimated], MicroUsd(9));
        assert_eq!(r.cost_unavailable, 1);
        assert_eq!(
            r.tokens[&(Some(id("gpt-5.5")), Provenance::ProviderReported)],
            tokens(50, 5)
        );
        // Without its parent in the set, the subagent's usage is not lost.
        let alone = rollup([&subagent]);
        assert_eq!(alone.runs_included_elsewhere, 0);
        assert_eq!(alone.cost[&Provenance::Estimated], MicroUsd(1));
    }

    #[test]
    fn an_orchestration_rolls_up_by_run_id_with_duplicates_collapsed() {
        let in_orch = |run_id: &str, agent: &str, t| {
            run(
                RunIdentity {
                    orchestration_id: Some(id("o1")),
                    ..identity(run_id, agent)
                },
                "m",
                t,
                Field::Unavailable(Unavailable::NoChannel),
            )
        };
        let runs = [
            in_orch("r1", "codex", tokens(10, 1)),
            in_orch("r2", "pi", tokens(20, 2)),
            in_orch("r1", "codex", tokens(10, 1)),
            in_orch("r3", "claude", tokens(30, 3)),
            in_orch("r2", "pi", tokens(20, 2)),
            run(
                identity("x", "codex"),
                "m",
                tokens(999, 9),
                Field::Unavailable(Unavailable::NoChannel),
            ),
        ];
        let r = rollup_orchestration(&runs, "o1");
        assert_eq!(r.runs, 3);
        assert_eq!(
            r.tokens[&(Some(id("m")), Provenance::ProviderReported)],
            tokens(60, 6)
        );
    }

    fn with_quota(run_id: &str, from: u32, to: u32, resets_at: i64) -> RunRecord {
        let mut r = run(
            identity(run_id, "codex"),
            "gpt-5.5",
            tokens(1, 1),
            Field::Unavailable(Unavailable::NoChannel),
        );
        r.quota = Field::ProviderReported(QuotaDelta {
            plan: Some(id("prolite")),
            window_minutes: 10080,
            resets_at,
            from_bp: from,
            to_bp: to,
        });
        r
    }

    #[test]
    fn money_of_different_provenance_and_quota_stay_apart() {
        let claude = run(
            identity("a", "claude"),
            "claude-opus-5-5",
            tokens(1, 1),
            Field::Estimated(MicroUsd(30_000_000)),
        );
        let api_key = run(
            identity("b", "claude"),
            "claude-opus-5-5",
            tokens(1, 1),
            Field::ProviderReported(MicroUsd(4_000_000)),
        );
        let opencode = run(
            identity("c", "opencode"),
            "gpt-5.5",
            tokens(1, 1),
            Field::Unavailable(Unavailable::SubscriptionZero),
        );
        // Two overlapping runs in one window, one run after them, and one in
        // the next window.
        let runs = [
            claude,
            api_key,
            opencode,
            with_quota("q1", 2600, 3000, 1),
            with_quota("q2", 2800, 3400, 1),
            with_quota("q3", 4000, 4500, 1),
            with_quota("q4", 100, 300, 2),
        ];
        let r = rollup(&runs);
        assert_eq!(r.cost.len(), 2, "never one number: {:?}", r.cost);
        assert_eq!(r.cost[&Provenance::Estimated], MicroUsd(30_000_000));
        assert_eq!(r.cost[&Provenance::ProviderReported], MicroUsd(4_000_000));
        assert_eq!(r.cost_unavailable, 5);
        let window = |resets_at| QuotaKey {
            agent: id("codex"),
            plan: Some(id("prolite")),
            window_minutes: 10080,
            resets_at,
        };
        assert_eq!(r.quota_bp[&window(1)], 800 + 500, "overlap counted once");
        assert_eq!(r.quota_bp[&window(2)], 200);
        assert_eq!(r.quota_unavailable, 3);
    }

    // --- export mapping ---

    fn has(attrs: &[AccountingAttribute], key: AccountingKey, value: &str) -> bool {
        attrs.contains(&AccountingAttribute {
            key,
            value: AttrValue::Str(value.into()),
        })
    }

    #[test]
    fn the_export_names_provenance_and_reasons_and_uses_1741s_run_keys() {
        let record = RunRecord::new(
            RunIdentity {
                work_item: Some(id("issue-1")),
                ..identity("r1", "opencode")
            },
            from_opencode(&[OpenCodeMessageUsage {
                model: "gpt-5.5".into(),
                usage: Some(OpenCodeUsage {
                    input: 5,
                    output: 3,
                    cache_read: 0,
                    cache_write: 0,
                    cost_usd: 0.0,
                }),
            }]),
            Some(EndReason::Exited),
        );
        let export = run_attributes(&record);
        assert!(has(&export.run, AccountingKey::RunId, "r1"));
        assert!(has(&export.run, AccountingKey::Termination, "unknown"));
        assert!(has(
            &export.run,
            AccountingKey::TerminationProvenance,
            "measured"
        ));
        assert!(has(
            &export.run,
            AccountingKey::LastTurnUnavailableReason,
            "not_recorded"
        ));
        assert!(has(
            &export.run,
            AccountingKey::QuotaProvenance,
            "unavailable"
        ));
        assert!(has(
            &export.run,
            AccountingKey::QuotaUnavailableReason,
            "no_channel"
        ));
        let model = &export.models[0];
        assert!(has(model, AccountingKey::ResponseModel, "gpt-5.5"));
        assert!(has(
            model,
            AccountingKey::TokensProvenance,
            "provider_reported"
        ));
        assert!(has(model, AccountingKey::CostProvenance, "unavailable"));
        assert!(has(
            model,
            AccountingKey::CostUnavailableReason,
            "subscription_zero"
        ));
        assert!(
            !model.iter().any(|a| a.key == AccountingKey::CostMicroUsd),
            "an unavailable cost is not exported as 0"
        );

        let item = WorkItemRecord {
            disposition: work_disposition(Some(PullRequestState::Merged), None),
            checks: observed_checks(None, Some(true)),
        };
        let attrs = work_item_attributes(&item);
        assert!(has(&attrs, AccountingKey::Disposition, "merged"));
        assert!(has(&attrs, AccountingKey::Checks, "passing"));
        assert!(has(
            &attrs,
            AccountingKey::ChecksProvenance,
            "agent_asserted"
        ));
    }

    /// Two deltas alike but for their window instance export different
    /// attributes, so a backend can keep successive windows apart.
    #[test]
    fn deltas_in_different_quota_windows_export_different_attributes() {
        let this_week = run_attributes(&with_quota("q", 100, 300, 1_000)).run;
        let next_week = run_attributes(&with_quota("q", 100, 300, 2_000)).run;
        assert_ne!(this_week, next_week);
        let resets_at = |attrs: &[AccountingAttribute]| {
            attrs
                .iter()
                .find(|a| a.key == AccountingKey::QuotaResetsAt)
                .map(|a| a.value.clone())
        };
        assert_eq!(resets_at(&this_week), Some(AttrValue::Int(1_000)));
        assert_eq!(resets_at(&next_week), Some(AttrValue::Int(2_000)));
        assert_eq!(
            AccountingKey::QuotaResetsAt.as_str(),
            "deck.quota.resets_at"
        );
    }

    /// The exact spellings of the keys borrowed from OpenTelemetry's
    /// conventions, so a rename on either side is a visible test change.
    #[test]
    fn the_borrowed_keys_are_spelled_as_the_conventions_spell_them() {
        let table = [
            (AccountingKey::InputTokens, "gen_ai.usage.input_tokens"),
            (AccountingKey::OutputTokens, "gen_ai.usage.output_tokens"),
            (
                AccountingKey::CacheReadInputTokens,
                "gen_ai.usage.cache_read.input_tokens",
            ),
            (
                AccountingKey::CacheWriteInputTokens,
                "gen_ai.usage.cache_write.input_tokens",
            ),
            (AccountingKey::ResponseModel, "gen_ai.response.model"),
            (AccountingKey::ErrorType, "error.type"),
            (AccountingKey::RunId, "deck.run.id"),
            (AccountingKey::Agent, "deck.agent"),
        ];
        for (key, spelling) in table {
            assert_eq!(key.as_str(), spelling);
        }
    }

    #[test]
    fn every_accounting_key_is_unique_and_reuses_1741s_names_where_shared() {
        let names: HashSet<_> = AccountingKey::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), AccountingKey::ALL.len());
        for key in AccountingKey::ALL {
            let name = key.as_str();
            assert!(
                name.starts_with("deck.") || name.starts_with("gen_ai.") || name == "error.type",
                "{name}"
            );
            let shared = AttrKey::is_allowed(name);
            assert_eq!(
                shared,
                matches!(key, AccountingKey::RunId | AccountingKey::Agent),
                "{name} collides with a #1741 key"
            );
        }
    }

    // --- overhead benchmark ---

    /// The `.jsonl` files under `dir`, with their lengths, not following
    /// symlinks.
    fn rollouts_under(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let path = entry.path();
            if meta.is_dir() {
                rollouts_under(&path, out);
            } else if meta.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                out.push((path, meta.len()));
            }
        }
    }

    fn load_average() -> String {
        std::fs::read_to_string("/proc/loadavg")
            .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" / "))
            .unwrap_or_else(|_| "unavailable".into())
    }

    /// The `p`th quantile of `sorted`, in microseconds.
    fn quantile_us(sorted: &[Duration], p: f64) -> f64 {
        let i = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[i].as_secs_f64() * 1e6
    }

    /// The overhead benchmark behind the doc's "Overhead" table. Not a
    /// `#[spec]` test, and a no-op unless `DAD_ACCOUNTING_BENCH_DIR` names a
    /// directory of Codex rollouts: then it reads every `.jsonl` under it
    /// whole, against a `serde_json::Value` baseline, checks each rollout's
    /// per-model sums against its own running total, replays the largest ones
    /// a line at a time into a scratch file polling after each line, and
    /// times idle polls. It prints aggregates only — counts, bytes, times,
    /// the load average — and never a path, a file name or a line. Run it in
    /// a release build; `docs/develop/execution-accounting.md` has the
    /// command.
    #[test]
    fn overhead_over_a_directory_of_rollouts() {
        let Some(dir) = std::env::var_os("DAD_ACCOUNTING_BENCH_DIR") else {
            return;
        };
        let mut files = Vec::new();
        rollouts_under(Path::new(&dir), &mut files);
        assert!(!files.is_empty(), "no .jsonl file under the directory");
        let total_bytes: u64 = files.iter().map(|(_, len)| len).sum();
        println!("load average at start: {}", load_average());
        println!("rollouts: {}, bytes: {total_bytes}", files.len());

        for pass in 1..=3 {
            let started = Instant::now();
            let (mut stats, mut errors, mut agree, mut regressed) =
                (ReaderStats::default(), 0, 0, 0);
            for (path, _) in &files {
                let mut reader = CodexUsageReader::new(path).with_poll_budget(u64::MAX);
                if reader.poll().is_err() {
                    errors += 1;
                    continue;
                }
                let s = reader.stats();
                stats.lines += s.lines;
                stats.parsed += s.parsed;
                stats.malformed += s.malformed;
                stats.overlong += s.overlong;
                stats.unusable_totals += s.unusable_totals;
                stats.regressions += s.regressions;
                let summary = reader.summary();
                let mut sum = Tokens::default();
                for t in summary.tokens_by_model.values() {
                    sum.add(t);
                }
                if s.regressions > 0 {
                    regressed += 1;
                } else if summary.last_total.unwrap_or_default() == sum {
                    agree += 1;
                }
            }
            let secs = started.elapsed().as_secs_f64();
            println!(
                "full read, pass {pass}: {secs:.2} s, {:.0} MB/s; lines {}, deserialised {}, malformed {}, overlong {}, unusable totals {}, regressions {}, open errors {errors}; per-model sums equal the running total in {agree} rollouts, {regressed} with a total that went down",
                total_bytes as f64 / 1e6 / secs,
                stats.lines,
                stats.parsed,
                stats.malformed,
                stats.overlong,
                stats.unusable_totals,
                stats.regressions,
            );
        }

        let started = Instant::now();
        let mut values = 0u64;
        for (path, _) in &files {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            for line in bytes.split(|b| *b == b'\n') {
                if line_is_candidate(line)
                    && serde_json::from_slice::<serde_json::Value>(line).is_ok()
                {
                    values += 1;
                }
            }
        }
        println!(
            "baseline, whole file into serde_json::Value: {:.2} s, {values} lines parsed",
            started.elapsed().as_secs_f64()
        );

        let mut largest = files.clone();
        largest.sort_by_key(|(_, len)| std::cmp::Reverse(*len));
        largest.truncate(20);
        let scratch = crate::test_temp::tempdir().unwrap();
        let mut polls: Vec<Duration> = Vec::new();
        let mut appended = 0u64;
        let mut idle: Vec<Duration> = Vec::new();
        for (i, (path, _)) in largest.iter().enumerate() {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let target = scratch.path().join(format!("replay-{i}.jsonl"));
            let mut out = File::create(&target).unwrap();
            let mut reader = CodexUsageReader::new(&target);
            for line in bytes.split_inclusive(|b| *b == b'\n') {
                out.write_all(line).unwrap();
                out.flush().unwrap();
                appended += line.len() as u64;
                let started = Instant::now();
                reader.poll().unwrap();
                polls.push(started.elapsed());
            }
            for _ in 0..100 {
                let started = Instant::now();
                reader.poll().unwrap();
                idle.push(started.elapsed());
            }
        }
        if !polls.is_empty() {
            let total: Duration = polls.iter().sum();
            polls.sort_unstable();
            idle.sort_unstable();
            println!(
                "incremental, {} rollouts, {appended} bytes in {} polls: mean {:.1} µs, median {:.1} µs, p99 {:.1} µs, max {:.1} µs, {:.2} µs per KB appended",
                largest.len(),
                polls.len(),
                total.as_secs_f64() * 1e6 / polls.len() as f64,
                quantile_us(&polls, 0.5),
                quantile_us(&polls, 0.99),
                quantile_us(&polls, 1.0),
                total.as_secs_f64() * 1e6 / (appended as f64 / 1024.0),
            );
            println!(
                "idle poll, nothing appended, {} polls: median {:.1} µs, p99 {:.1} µs",
                idle.len(),
                quantile_us(&idle, 0.5),
                quantile_us(&idle, 0.99),
            );
        }
        println!("load average at end: {}", load_average());
    }
}
