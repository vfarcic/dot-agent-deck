//! PRD #633, discovery milestone 4: a narrow prototype of the execution
//! record, following `docs/develop/execution-accounting.md`. Nothing in the
//! product calls it yet: no daemon, TUI, hook or exporter path reaches this
//! module, and nothing here locates, watches or tails a file by itself.
//!
//! * **Provenance on every value** ([`Field`]): measured, provider-reported,
//!   estimated, agent-asserted, or unavailable with a reason. Unavailable is a
//!   value the record carries, not a missing field, and a cost of `0` for
//!   replies that used tokens is unavailable ([`Unavailable::SubscriptionZero`]),
//!   never a free run.
//! * **Normalisers** from each agent's usage observation, as typed usage-only
//!   structs, to one shape ([`AgentUsage`]): [`from_claude`], [`from_codex`],
//!   [`from_pi`], [`from_opencode`]. No input struct has a field that could
//!   hold a prompt, a reply, a tool's input or output, or file content.
//! * **Local capture** of one agent's store ([`CodexUsageReader`]): an
//!   incremental reader of a Codex rollout at a path it is given, reading only
//!   bytes appended since its last poll and deserialising only the model,
//!   usage, quota and turn-end fields of the few record types that carry them.
//! * **Aggregation** ([`rollup`]) of runs to a work item or an orchestration,
//!   without counting a run twice, a Claude Code subagent inside its parent's
//!   total, or money of different provenance as one number.
//! * **Export mapping** ([`run_attributes`], [`work_item_attributes`]): a pure
//!   function from the record to attributes, `gen_ai.usage.*` for tokens and
//!   `deck.*` for provenance and outcomes, keyed by a closed enum
//!   ([`AccountingKey`]) as #1741's [`AttrKey`] is.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;

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
    /// The quota window reset or changed during the run, so a delta would
    /// mislead.
    WindowReset,
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
            Unavailable::WindowReset => "window_reset",
            Unavailable::RunNotEnded => "run_not_ended",
            Unavailable::NoWorkItem => "no_work_item",
        }
    }
}

/// A value with its provenance. There is no way to hold a value without
/// saying where it came from, nor to leave a field out: an unknown value is
/// [`Field::Unavailable`] with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    pub fn is_zero(&self) -> bool {
        *self == Tokens::default()
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
}

/// One model's usage within a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsage {
    /// `None` when the usage cannot be tied to a model.
    pub model: Option<String>,
    pub tokens: Field<Tokens>,
    pub cost: Field<MicroUsd>,
}

/// How much of a plan's window a run took, in basis points (hundredths of a
/// percent) of that window. Concurrent runs share one window, so for a run
/// that overlapped another it is an upper bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaDelta {
    pub plan: Option<String>,
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
    pub error_type: Option<String>,
}

/// What an agent's own record says about its last turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTurnEnd {
    Completed,
    /// `kind` is the agent's error identifier, never its message.
    Failed {
        kind: Option<String>,
    },
    Interrupted,
    /// The record ends inside a turn.
    MidTurn,
}

/// A run's termination from what the deck saw (`end`, `None` while the run is
/// open) and what the agent's own record says about its last turn.
///
/// The agent's word decides a failure, an interruption and a completion
/// (provider-reported). A record that ends mid-turn is decided by the deck: a
/// run it closed or shut down was interrupted, any other end is unknown
/// (measured). With no agent record the deck knows only that the run ended.
pub fn run_termination(end: Option<EndReason>, agent: Option<&AgentTurnEnd>) -> Field<Termination> {
    let of = |outcome| Termination {
        outcome,
        error_type: None,
    };
    match (agent, end) {
        (Some(AgentTurnEnd::Failed { kind }), _) => Field::ProviderReported(Termination {
            outcome: RunTermination::Failed,
            error_type: kind.as_deref().and_then(identifier),
        }),
        (Some(AgentTurnEnd::Interrupted), _) => {
            Field::ProviderReported(of(RunTermination::Interrupted))
        }
        (Some(AgentTurnEnd::Completed), _) => {
            Field::ProviderReported(of(RunTermination::Completed))
        }
        (Some(AgentTurnEnd::MidTurn), None) | (None, None) => {
            Field::Unavailable(Unavailable::RunNotEnded)
        }
        (Some(AgentTurnEnd::MidTurn), Some(EndReason::Closed | EndReason::DaemonShutdown)) => {
            Field::Measured(of(RunTermination::Interrupted))
        }
        (Some(AgentTurnEnd::MidTurn), Some(EndReason::Exited | EndReason::Unknown))
        | (None, Some(_)) => Field::Measured(of(RunTermination::Unknown)),
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

/// Whether a work item's change was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Verified,
    Failing,
    Unverified,
}

impl Verification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Verification::Verified => "verified",
            Verification::Failing => "failing",
            Verification::Unverified => "unverified",
        }
    }
}

/// Verification from the checks on the item's head (#1741's `agent.ci`,
/// measured), else from the agent's own claim that its tests passed
/// (agent-asserted), else unavailable.
pub fn verification(
    checks_on_head: Option<CiState>,
    agent_claim: Option<bool>,
) -> Field<Verification> {
    match (checks_on_head, agent_claim) {
        (Some(CiState::Passing), _) => Field::Measured(Verification::Verified),
        (Some(CiState::Failing), _) => Field::Measured(Verification::Failing),
        (Some(CiState::Pending), _) => Field::Measured(Verification::Unverified),
        (None, Some(true)) => Field::AgentAsserted(Verification::Verified),
        (None, Some(false)) => Field::AgentAsserted(Verification::Failing),
        (None, None) => Field::Unavailable(Unavailable::NoChannel),
    }
}

/// A work item's outcome fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemRecord {
    pub disposition: Field<Disposition>,
    pub verification: Field<Verification>,
}

/// A short identifier from an agent's store — a model, a plan, an error kind —
/// or `None` for anything that does not look like one, so that no free text
/// can travel through a field meant for a name.
fn identifier(value: &str) -> Option<String> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/@+".contains(&b));
    ok.then(|| value.to_owned())
}

// ---------------------------------------------------------------------------
// Normalisers
// ---------------------------------------------------------------------------

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

/// An agent's own cost figure for `tokens`, as `estimated`: a `0` for replies
/// that used tokens is a subscription's, not a free run's.
fn agent_cost(cost: Option<MicroUsd>, tokens: &Tokens) -> Field<MicroUsd> {
    match cost {
        None => Field::Unavailable(Unavailable::NotRecorded),
        Some(MicroUsd(0)) if !tokens.is_zero() => Field::Unavailable(Unavailable::SubscriptionZero),
        Some(cost) => Field::Estimated(cost),
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
    let models = session
        .models
        .iter()
        .map(|m| {
            let tokens = Tokens {
                input: m
                    .input_tokens
                    .saturating_add(m.cache_read_input_tokens)
                    .saturating_add(m.cache_creation_input_tokens),
                output: m.output_tokens,
                cache_read: m.cache_read_input_tokens,
                cache_write: m.cache_creation_input_tokens,
            };
            let cost = if session.has_unknown_model_cost {
                Field::Unavailable(Unavailable::UnknownPrice)
            } else {
                agent_cost(m.cost_usd.and_then(MicroUsd::from_usd), &tokens)
            };
            ModelUsage {
                model: identifier(&m.model),
                tokens: Field::ProviderReported(tokens),
                cost,
            }
        })
        .collect();
    AgentUsage {
        models,
        quota: Field::Unavailable(Unavailable::NoChannel),
        last_turn: session.last_turn.clone(),
    }
}

/// Codex: tokens provider-reported, by model, from its rollout's running
/// total; no cost (the rollout carries none); the quota delta of its primary
/// rate-limit window.
pub fn from_codex(summary: &CodexRolloutSummary) -> AgentUsage {
    let models = if summary.tokens_by_model.is_empty() {
        usage_unavailable(Unavailable::NotRecorded)
    } else {
        summary
            .tokens_by_model
            .iter()
            .map(|(model, tokens)| ModelUsage {
                model: model.clone(),
                tokens: Field::ProviderReported(*tokens),
                cost: Field::Unavailable(Unavailable::NoChannel),
            })
            .collect()
    };
    let quota = match (&summary.quota_first, &summary.quota_last) {
        (Some(first), Some(last))
            if first.resets_at == last.resets_at && first.window_minutes == last.window_minutes =>
        {
            Field::ProviderReported(QuotaDelta {
                plan: last.plan.clone(),
                window_minutes: last.window_minutes,
                resets_at: last.resets_at,
                from_bp: first.used_bp,
                to_bp: last.used_bp,
            })
        }
        (Some(_), Some(_)) => Field::Unavailable(Unavailable::WindowReset),
        _ => Field::Unavailable(Unavailable::NotRecorded),
    };
    AgentUsage {
        models,
        quota,
        last_turn: summary.last_turn.clone(),
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

/// Pi: tokens provider-reported, summed per model; cost as Pi's estimate,
/// unavailable for a model when any of its replies has none.
pub fn from_pi(replies: &[PiReplyUsage]) -> AgentUsage {
    let mut by_model: BTreeMap<Option<String>, (Tokens, Option<MicroUsd>)> = BTreeMap::new();
    for reply in replies {
        let tokens = Tokens {
            input: reply
                .input
                .saturating_add(reply.cache_read)
                .saturating_add(reply.cache_write),
            output: reply.output,
            cache_read: reply.cache_read,
            cache_write: reply.cache_write,
        };
        let entry = by_model
            .entry(identifier(&reply.model))
            .or_insert((Tokens::default(), Some(MicroUsd(0))));
        entry.0.add(&tokens);
        entry.1 = match (entry.1, reply.cost_total_usd.and_then(MicroUsd::from_usd)) {
            (Some(sum), Some(cost)) => Some(MicroUsd(sum.0.saturating_add(cost.0))),
            _ => None,
        };
    }
    let models = if by_model.is_empty() {
        usage_unavailable(Unavailable::NotRecorded)
    } else {
        by_model
            .into_iter()
            .map(|(model, (tokens, cost))| ModelUsage {
                model,
                cost: agent_cost(cost, &tokens),
                tokens: Field::ProviderReported(tokens),
            })
            .collect()
    };
    let last_turn = replies.last().map(|r| match r.stop_reason {
        Some(PiStopReason::Error) => AgentTurnEnd::Failed { kind: None },
        Some(PiStopReason::Aborted) => AgentTurnEnd::Interrupted,
        _ => AgentTurnEnd::Completed,
    });
    AgentUsage {
        models,
        quota: Field::Unavailable(Unavailable::NoChannel),
        last_turn,
    }
}

/// One OpenCode message's usage (its database's per-message `tokens` and
/// `cost`). Its input count excludes cache reads and writes.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenCodeMessageUsage {
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
}

/// OpenCode: tokens provider-reported per model; cost as OpenCode's own
/// figure, unavailable when it is `0` for messages that used tokens, which is
/// what its subscription providers report.
pub fn from_opencode(messages: &[OpenCodeMessageUsage]) -> AgentUsage {
    let mut by_model: BTreeMap<Option<String>, (Tokens, Option<MicroUsd>)> = BTreeMap::new();
    for m in messages {
        let tokens = Tokens {
            input: m
                .input
                .saturating_add(m.cache_read)
                .saturating_add(m.cache_write),
            output: m.output,
            cache_read: m.cache_read,
            cache_write: m.cache_write,
        };
        let entry = by_model
            .entry(identifier(&m.model))
            .or_insert((Tokens::default(), Some(MicroUsd(0))));
        entry.0.add(&tokens);
        entry.1 = match (entry.1, MicroUsd::from_usd(m.cost_usd)) {
            (Some(sum), Some(cost)) => Some(MicroUsd(sum.0.saturating_add(cost.0))),
            _ => None,
        };
    }
    let models = if by_model.is_empty() {
        usage_unavailable(Unavailable::NotRecorded)
    } else {
        by_model
            .into_iter()
            .map(|(model, (tokens, cost))| ModelUsage {
                model,
                cost: agent_cost(cost, &tokens),
                tokens: Field::ProviderReported(tokens),
            })
            .collect()
    };
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunIdentity {
    pub run_id: String,
    pub agent: String,
    /// The run whose pane started this one, if any.
    pub parent_run_id: Option<String>,
    /// The run whose own total already contains this run's usage — a Claude
    /// Code subagent's parent session. Its usage is then counted there and
    /// never again here.
    pub usage_included_in: Option<String>,
    pub orchestration_id: Option<String>,
    /// The issue or pull request the run serves, when known (#628).
    pub work_item: Option<String>,
}

/// One run's accounting record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub identity: RunIdentity,
    pub models: Vec<ModelUsage>,
    pub quota: Field<QuotaDelta>,
    pub termination: Field<Termination>,
}

impl RunRecord {
    /// The record from the deck's view of the run and the agent's usage.
    pub fn new(identity: RunIdentity, usage: AgentUsage, end: Option<EndReason>) -> Self {
        let termination = run_termination(end, usage.last_turn.as_ref());
        RunRecord {
            identity,
            models: usage.models,
            quota: usage.quota,
            termination,
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// One plan window instance, the unit quota rolls up in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuotaKey {
    pub agent: String,
    pub plan: Option<String>,
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
    pub tokens: BTreeMap<(Option<String>, Provenance), Tokens>,
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

/// Roll `runs` up. A run id seen more than once is counted once, as its last
/// record, since a later snapshot of a running total supersedes an earlier
/// one. Distinct run ids are distinct attempts and are all counted, so a
/// retried work item costs what both attempts cost. A run whose usage is
/// included in another run of the set counts as a run and a termination but
/// adds no usage.
pub fn rollup<'a>(runs: impl IntoIterator<Item = &'a RunRecord>) -> Rollup {
    let mut order: Vec<&RunRecord> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for run in runs {
        match index.get(run.identity.run_id.as_str()) {
            Some(&i) => order[i] = run,
            None => {
                index.insert(run.identity.run_id.as_str(), order.len());
                order.push(run);
            }
        }
    }
    let mut out = Rollup {
        runs: order.len(),
        ..Rollup::default()
    };
    let mut quota_ranges: BTreeMap<QuotaKey, Vec<(u32, u32)>> = BTreeMap::new();
    for run in &order {
        match run.termination.value() {
            Some(t) => *out.terminations.entry(t.outcome).or_default() += 1,
            None => out.terminations_unavailable += 1,
        }
        if run
            .identity
            .usage_included_in
            .as_deref()
            .is_some_and(|parent| index.contains_key(parent))
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
                    sum.0 = sum.0.saturating_add(cost.0);
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

/// The runs that served `work_item`, rolled up.
pub fn rollup_work_item(runs: &[RunRecord], work_item: &str) -> Rollup {
    rollup(
        runs.iter()
            .filter(|r| r.identity.work_item.as_deref() == Some(work_item)),
    )
}

/// The runs of `orchestration_id`, rolled up.
pub fn rollup_orchestration(runs: &[RunRecord], orchestration_id: &str) -> Rollup {
    rollup(
        runs.iter()
            .filter(|r| r.identity.orchestration_id.as_deref() == Some(orchestration_id)),
    )
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
    pub tokens_by_model: BTreeMap<Option<String>, Tokens>,
    pub quota_first: Option<CodexRateSample>,
    pub quota_last: Option<CodexRateSample>,
    pub last_turn: Option<AgentTurnEnd>,
    current_model: Option<String>,
    last_total: Option<Tokens>,
}

/// One sample of a Codex plan's primary rate-limit window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRateSample {
    pub plan: Option<String>,
    pub used_bp: u32,
    pub window_minutes: u64,
    pub resets_at: i64,
}

/// The longest line the reader buffers across reads; a longer one is skipped
/// through its newline without being parsed. On the development machine the
/// longest line of a kind the reader parses was about 10 KiB (a
/// `turn_context`); the long lines are tool output.
pub const MAX_LINE_BYTES: usize = 256 * 1024;

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
}

/// An incremental reader of one Codex rollout, at the path it is given.
///
/// Each [`poll`](Self::poll) reads only the bytes appended since the last one.
/// A line is kept across polls until its newline arrives (at most
/// [`MAX_LINE_BYTES`]), then dropped. Only lines naming one of the record
/// types that carry usage are deserialised, and only into structs holding the
/// model, token counts, quota and turn-end kind, so a prompt, reply, tool
/// output or file content is scanned by the parser and, while its line is
/// incomplete, buffered, but kept in nothing the reader returns.
///
/// A file that gets shorter, or whose path now names another file (Unix),
/// is read again from the start with the summary reset: that is a new rollout
/// as far as the reader can tell.
#[derive(Debug)]
pub struct CodexUsageReader {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    offset: u64,
    partial: Vec<u8>,
    skipping: bool,
    summary: CodexRolloutSummary,
    stats: ReaderStats,
}

impl CodexUsageReader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        CodexUsageReader {
            path: path.into(),
            identity: None,
            offset: 0,
            partial: Vec::new(),
            skipping: false,
            summary: CodexRolloutSummary::default(),
            stats: ReaderStats::default(),
        }
    }

    pub fn summary(&self) -> &CodexRolloutSummary {
        &self.summary
    }

    pub fn stats(&self) -> ReaderStats {
        self.stats
    }

    /// Read what was appended since the last poll.
    pub fn poll(&mut self) -> io::Result<()> {
        let mut file = File::open(&self.path)?;
        let meta = file.metadata()?;
        let identity = file_identity(&meta);
        if (self.identity.is_some() && identity != self.identity) || meta.len() < self.offset {
            self.restart();
        }
        self.identity = identity;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = [0u8; READ_CHUNK];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            self.offset += n as u64;
            self.stats.bytes_read += n as u64;
            self.feed(&buf[..n]);
        }
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
                self.summary.observe(record);
            }
            Err(_) => self.stats.malformed += 1,
        }
    }
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
/// that decides which lines are deserialised at all.
fn line_is_candidate(line: &[u8]) -> bool {
    CANDIDATE_MARKERS
        .iter()
        .any(|m| line.windows(m.len()).any(|w| w == *m))
}

impl CodexRolloutSummary {
    fn observe(&mut self, record: NarrowRecord) {
        let payload = record.payload.unwrap_or_default();
        match (record.kind.0.as_deref(), payload.kind.0.as_deref()) {
            (Some("turn_context"), _) => {
                self.current_model = payload.model.0.as_deref().and_then(identifier);
            }
            (Some("event_msg"), Some("token_count")) => {
                if let Some(total) = payload.info.and_then(|i| i.total_token_usage) {
                    self.observe_total(total.tokens());
                }
                if let Some(sample) = payload.rate_limits.and_then(NarrowRateLimits::sample) {
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
                        kind: kind.as_deref().and_then(identifier),
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

    /// Credit the growth of the running total to the current model. A total
    /// that went down means the counters restarted, so all of it is new.
    fn observe_total(&mut self, total: Tokens) {
        let delta = match self.last_total {
            Some(prev)
                if total.input >= prev.input
                    && total.output >= prev.output
                    && total.cache_read >= prev.cache_read
                    && total.cache_write >= prev.cache_write =>
            {
                Tokens {
                    input: total.input - prev.input,
                    output: total.output - prev.output,
                    cache_read: total.cache_read - prev.cache_read,
                    cache_write: total.cache_write - prev.cache_write,
                }
            }
            _ => total,
        };
        self.last_total = Some(total);
        if !delta.is_zero() {
            self.tokens_by_model
                .entry(self.current_model.clone())
                .or_default()
                .add(&delta);
        }
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

/// Codex's token counts. Its `input_tokens` already includes the cached
/// tokens, as OpenAI's API counts them. Whether it also includes
/// `cache_write_input_tokens` is not known: that count was 0 on every sampled
/// record, so it is taken as included, the same as cached tokens.
#[derive(Deserialize)]
struct CodexTokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl CodexTokenUsage {
    fn tokens(&self) -> Tokens {
        Tokens {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cached_input_tokens,
            cache_write: self.cache_write_input_tokens,
        }
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
            plan: self.plan_type.0.as_deref().and_then(identifier),
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
/// semantic conventions, all in development status as of 2026-10-23
/// (`open-telemetry/semantic-conventions-genai`), so a change there may
/// rename them; `error.type` is stable.
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
    QuotaFromBp,
    QuotaToBp,
    QuotaProvenance,
    QuotaUnavailableReason,
    Termination,
    TerminationProvenance,
    TerminationUnavailableReason,
    ErrorType,
    Disposition,
    DispositionProvenance,
    DispositionUnavailableReason,
    Verification,
    VerificationProvenance,
    VerificationUnavailableReason,
}

impl AccountingKey {
    /// Every key, for the allow-list test.
    #[cfg(test)]
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
        AccountingKey::QuotaFromBp,
        AccountingKey::QuotaToBp,
        AccountingKey::QuotaProvenance,
        AccountingKey::QuotaUnavailableReason,
        AccountingKey::Termination,
        AccountingKey::TerminationProvenance,
        AccountingKey::TerminationUnavailableReason,
        AccountingKey::ErrorType,
        AccountingKey::Disposition,
        AccountingKey::DispositionProvenance,
        AccountingKey::DispositionUnavailableReason,
        AccountingKey::Verification,
        AccountingKey::VerificationProvenance,
        AccountingKey::VerificationUnavailableReason,
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
            AccountingKey::QuotaFromBp => "deck.quota.used_from_bp",
            AccountingKey::QuotaToBp => "deck.quota.used_to_bp",
            AccountingKey::QuotaProvenance => "deck.quota.provenance",
            AccountingKey::QuotaUnavailableReason => "deck.quota.unavailable_reason",
            AccountingKey::Termination => "deck.run.termination",
            AccountingKey::TerminationProvenance => "deck.run.termination.provenance",
            AccountingKey::TerminationUnavailableReason => {
                "deck.run.termination.unavailable_reason"
            }
            AccountingKey::ErrorType => "error.type",
            AccountingKey::Disposition => "deck.work.disposition",
            AccountingKey::DispositionProvenance => "deck.work.disposition.provenance",
            AccountingKey::DispositionUnavailableReason => {
                "deck.work.disposition.unavailable_reason"
            }
            AccountingKey::Verification => "deck.work.verification",
            AccountingKey::VerificationProvenance => "deck.work.verification.provenance",
            AccountingKey::VerificationUnavailableReason => {
                "deck.work.verification.unavailable_reason"
            }
        }
    }
}

/// One allow-listed accounting attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountingAttribute {
    pub key: AccountingKey,
    pub value: AttrValue,
}

fn s(key: AccountingKey, value: &str) -> AccountingAttribute {
    AccountingAttribute {
        key,
        value: AttrValue::Str(value.to_owned()),
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
    out.push(s(key, field.provenance().as_str()));
    if let Some(reason) = field.unavailable() {
        out.push(s(reason_key, reason.as_str()));
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
        s(K::RunId, &record.identity.run_id),
        s(K::Agent, &record.identity.agent),
    ];
    if let Some(t) = record.termination.value() {
        run.push(s(K::Termination, t.outcome.as_str()));
        if let Some(kind) = &t.error_type {
            run.push(s(K::ErrorType, kind));
        }
    }
    provenance(
        &mut run,
        &record.termination,
        K::TerminationProvenance,
        K::TerminationUnavailableReason,
    );
    if let Some(q) = record.quota.value() {
        if let Some(plan) = &q.plan {
            run.push(s(K::QuotaPlan, plan));
        }
        run.push(int(K::QuotaWindowMinutes, q.window_minutes));
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
                attrs.push(s(K::ResponseModel, model));
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
        out.push(s(K::Disposition, d.as_str()));
    }
    provenance(
        &mut out,
        &item.disposition,
        K::DispositionProvenance,
        K::DispositionUnavailableReason,
    );
    if let Some(v) = item.verification.value() {
        out.push(s(K::Verification, v.as_str()));
    }
    provenance(
        &mut out,
        &item.verification,
        K::VerificationProvenance,
        K::VerificationUnavailableReason,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    // --- fixtures (synthetic; shaped like Codex 0.164.0's records) ---

    fn turn_context(model: &str) -> String {
        format!(
            r#"{{"timestamp":"t","type":"turn_context","payload":{{"turn_id":"u1","cwd":"/w","model":"{model}","collaboration_mode":{{"settings":{{"developer_instructions":"be brief"}}}}}}}}"#
        )
    }

    fn token_count(input: u64, cached: u64, output: u64, used: f64, resets_at: i64) -> String {
        format!(
            r#"{{"timestamp":"t","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{t}}},"last_token_usage":{{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":2}},"model_context_window":1000}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used},"window_minutes":10080,"resets_at":{resets_at}}},"secondary":null,"plan_type":"prolite"}}}}}}"#,
            t = input + output
        )
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

    fn identity(run_id: &str, agent: &str) -> RunIdentity {
        RunIdentity {
            run_id: run_id.into(),
            agent: agent.into(),
            ..RunIdentity::default()
        }
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
                model: Some(model.into()),
                tokens: Field::ProviderReported(t),
                cost,
            }],
            quota: Field::Unavailable(Unavailable::NoChannel),
            termination: Field::ProviderReported(Termination {
                outcome: RunTermination::Completed,
                error_type: None,
            }),
        }
    }

    // --- provenance ---

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
    fn termination_takes_the_agents_word_and_the_decks_end_for_a_cut_turn() {
        let failed = AgentTurnEnd::Failed {
            kind: Some("server_overloaded".into()),
        };
        let t = run_termination(Some(EndReason::Exited), Some(&failed));
        assert_eq!(t.provenance(), Provenance::ProviderReported);
        assert_eq!(t.value().unwrap().outcome, RunTermination::Failed);
        assert_eq!(
            t.value().unwrap().error_type.as_deref(),
            Some("server_overloaded")
        );
        let prose = AgentTurnEnd::Failed {
            kind: Some("the provider said no".into()),
        };
        assert_eq!(
            run_termination(None, Some(&prose))
                .value()
                .unwrap()
                .error_type,
            None,
            "an error kind that is not an identifier is dropped"
        );
        let cases = [
            (
                Some(EndReason::Closed),
                Some(AgentTurnEnd::MidTurn),
                Field::Measured(RunTermination::Interrupted),
            ),
            (
                Some(EndReason::DaemonShutdown),
                Some(AgentTurnEnd::MidTurn),
                Field::Measured(RunTermination::Interrupted),
            ),
            (
                Some(EndReason::Exited),
                Some(AgentTurnEnd::MidTurn),
                Field::Measured(RunTermination::Unknown),
            ),
            (
                None,
                Some(AgentTurnEnd::MidTurn),
                Field::Unavailable(Unavailable::RunNotEnded),
            ),
            (
                Some(EndReason::Closed),
                Some(AgentTurnEnd::Completed),
                Field::ProviderReported(RunTermination::Completed),
            ),
            (
                Some(EndReason::Exited),
                Some(AgentTurnEnd::Interrupted),
                Field::ProviderReported(RunTermination::Interrupted),
            ),
            (
                Some(EndReason::Exited),
                None,
                Field::Measured(RunTermination::Unknown),
            ),
            (None, None, Field::Unavailable(Unavailable::RunNotEnded)),
        ];
        for (end, agent, want) in cases {
            let got = run_termination(end, agent.as_ref());
            assert_eq!(got.provenance(), want.provenance(), "{end:?} {agent:?}");
            assert_eq!(
                got.value().map(|t| t.outcome),
                want.value().copied(),
                "{end:?} {agent:?}"
            );
        }
    }

    #[test]
    fn disposition_and_verification_follow_the_forge_then_the_agent() {
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
            verification(Some(CiState::Passing), Some(false)),
            Field::Measured(Verification::Verified)
        );
        assert_eq!(
            verification(Some(CiState::Pending), None),
            Field::Measured(Verification::Unverified)
        );
        assert_eq!(
            verification(None, Some(true)),
            Field::AgentAsserted(Verification::Verified)
        );
        assert_eq!(
            verification(None, None),
            Field::Unavailable(Unavailable::NoChannel)
        );
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

    #[test]
    fn codex_has_no_cost_and_a_quota_delta_only_within_one_window() {
        let mut summary = CodexRolloutSummary::default();
        summary
            .tokens_by_model
            .insert(Some("gpt-5.5".into()), tokens(100, 5));
        let sample = |bp, resets_at| CodexRateSample {
            plan: Some("prolite".into()),
            used_bp: bp,
            window_minutes: 10080,
            resets_at,
        };
        summary.quota_first = Some(sample(2600, 99));
        summary.quota_last = Some(sample(4500, 99));
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

        summary.quota_last = Some(sample(100, 200));
        assert_eq!(
            from_codex(&summary).quota,
            Field::Unavailable(Unavailable::WindowReset)
        );
        assert_eq!(
            from_codex(&CodexRolloutSummary::default()).models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
    }

    #[test]
    fn pi_sums_per_model_and_its_last_reply_names_the_turn_end() {
        let reply = |model: &str, cost, stop| PiReplyUsage {
            model: model.into(),
            input: 10,
            output: 2,
            cache_read: 100,
            cache_write: 0,
            cost_total_usd: cost,
            stop_reason: stop,
        };
        let usage = from_pi(&[
            reply("gpt-5.5", Some(0.01), Some(PiStopReason::ToolUse)),
            reply("gpt-5.5", Some(0.02), Some(PiStopReason::Stop)),
            reply("other", None, Some(PiStopReason::Error)),
        ]);
        let gpt = usage
            .models
            .iter()
            .find(|m| m.model.as_deref() == Some("gpt-5.5"))
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
            .find(|m| m.model.as_deref() == Some("other"))
            .unwrap();
        assert_eq!(other.cost, Field::Unavailable(Unavailable::NotRecorded));
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::Failed { kind: None }));
        let zero = from_pi(&[reply("gpt-5.5", Some(0.0), None)]);
        assert_eq!(
            zero.models[0].cost,
            Field::Unavailable(Unavailable::SubscriptionZero)
        );
        assert_eq!(
            from_pi(&[]).models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
    }

    #[test]
    fn opencode_subscription_zero_is_unavailable_not_free() {
        let message = |cost| OpenCodeMessageUsage {
            model: "gpt-5.5".into(),
            input: 5,
            output: 3,
            cache_read: 7,
            cache_write: 0,
            cost_usd: cost,
        };
        let subscription = from_opencode(&[message(0.0), message(0.0)]);
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
        let priced = from_opencode(&[message(0.5)]);
        assert_eq!(priced.models[0].cost, Field::Estimated(MicroUsd(500_000)));
    }

    // --- the incremental reader ---

    #[test]
    fn the_reader_credits_each_models_growth_and_reads_only_what_was_appended() {
        let dir = tempfile::tempdir().unwrap();
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
            summary.tokens_by_model.get(&Some("gpt-5.5".into())),
            Some(&Tokens {
                input: 100,
                output: 10,
                cache_read: 40,
                cache_write: 0
            })
        );
        assert_eq!(
            summary.tokens_by_model.get(&Some("gpt-5.5-mini".into())),
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
    }

    #[test]
    fn a_partial_trailing_line_waits_for_its_newline() {
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-c.jsonl");
        let mut file = File::create(&path).unwrap();
        let overlong = format!(
            r#"{{"type":"event_msg","payload":{{"type":"token_count","pad":"{}"}}}}"#,
            "x".repeat(MAX_LINE_BYTES)
        );
        write_lines(
            &mut file,
            &[
                r#"{"type":"event_msg","payload":{"type":"token_count","info":"#,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":"many"}}}}"#,
                "not json at all",
                &overlong,
                &token_count(7, 0, 1, 1.0, 5),
            ],
        );
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
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

    #[test]
    fn a_truncated_or_replaced_rollout_is_read_again_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
                kind: Some("server_overloaded".into())
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
    /// it. Each content line also holds a string that is exactly one of the
    /// pre-filter's markers, so it is deserialised rather than skipped.
    #[test]
    fn content_never_reaches_the_summary_the_record_or_the_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-g.jsonl");
        let mut file = File::create(&path).unwrap();
        let s = SENTINEL;
        let lines = [
            format!(
                r#"{{"type":"session_meta","payload":{{"cwd":"/{s}","base_instructions":{{"text":"{s}"}},"cli_version":"0.164.0"}}}}"#
            ),
            format!(
                r#"{{"type":"turn_context","payload":{{"model":"gpt-5.5","cwd":"/{s}","collaboration_mode":{{"settings":{{"developer_instructions":"{s}"}}}}}}}}"#
            ),
            format!(
                r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{s}"}},{{"type":"input_text","text":"token_count"}},{{"type":"input_text","text":"turn_context"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec","arguments":"{{\"cmd\":\"cat {s}\"}}","model":"{s}"}}}}"#
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
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_lines(&mut file, &refs);
        let mut reader = CodexUsageReader::new(&path);
        reader.poll().unwrap();
        assert_eq!(
            reader.stats().parsed,
            7,
            "the content lines were deserialised"
        );
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

    // --- aggregation ---

    #[test]
    fn a_retry_is_a_second_attempt_and_a_repeated_run_id_counts_once() {
        let mut first = run(
            RunIdentity {
                work_item: Some("issue-1676".into()),
                ..identity("a1", "claude")
            },
            "claude-opus-5-5",
            tokens(100, 10),
            Field::Estimated(MicroUsd(3_000_000)),
        );
        let second = run(
            RunIdentity {
                work_item: Some("issue-1676".into()),
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
            r.tokens[&(Some("claude-opus-5-5".into()), Provenance::ProviderReported)],
            tokens(450, 45),
            "the later snapshot of a1 replaces the earlier one"
        );
        assert_eq!(r.cost[&Provenance::Estimated], MicroUsd(13_000_000));
        assert_eq!(r.terminations[&RunTermination::Completed], 2);
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
                parent_run_id: Some("p".into()),
                usage_included_in: Some("p".into()),
                ..identity("p-sub", "claude")
            },
            "claude-haiku-5-5",
            tokens(200, 20),
            Field::Estimated(MicroUsd(1)),
        );
        let child = run(
            RunIdentity {
                parent_run_id: Some("p".into()),
                ..identity("c", "codex")
            },
            "gpt-5.5",
            tokens(50, 5),
            Field::Unavailable(Unavailable::NoChannel),
        );
        let r = rollup([&parent, &subagent, &child]);
        assert_eq!(r.runs, 3);
        assert_eq!(r.runs_included_elsewhere, 1);
        assert!(!r.tokens.contains_key(&(
            Some("claude-haiku-5-5".into()),
            Provenance::ProviderReported
        )));
        assert_eq!(r.cost[&Provenance::Estimated], MicroUsd(9));
        assert_eq!(r.cost_unavailable, 1);
        assert_eq!(
            r.tokens[&(Some("gpt-5.5".into()), Provenance::ProviderReported)],
            tokens(50, 5)
        );
        // Without its parent in the set, the subagent's usage is not lost.
        let alone = rollup([&subagent]);
        assert_eq!(alone.runs_included_elsewhere, 0);
        assert_eq!(alone.cost[&Provenance::Estimated], MicroUsd(1));
    }

    #[test]
    fn an_orchestration_rolls_up_by_run_id_with_duplicates_collapsed() {
        let in_orch = |id: &str, agent: &str, t| {
            run(
                RunIdentity {
                    orchestration_id: Some("o1".into()),
                    ..identity(id, agent)
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
            r.tokens[&(Some("m".into()), Provenance::ProviderReported)],
            tokens(60, 6)
        );
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
        let quota = |id: &str, from, to, resets_at| {
            let mut r = run(
                identity(id, "codex"),
                "gpt-5.5",
                tokens(1, 1),
                Field::Unavailable(Unavailable::NoChannel),
            );
            r.quota = Field::ProviderReported(QuotaDelta {
                plan: Some("prolite".into()),
                window_minutes: 10080,
                resets_at,
                from_bp: from,
                to_bp: to,
            });
            r
        };
        // Two overlapping runs in one window, one run after them, and one in
        // the next window.
        let runs = [
            claude,
            api_key,
            opencode,
            quota("q1", 2600, 3000, 1),
            quota("q2", 2800, 3400, 1),
            quota("q3", 4000, 4500, 1),
            quota("q4", 100, 300, 2),
        ];
        let r = rollup(&runs);
        assert_eq!(r.cost.len(), 2, "never one number: {:?}", r.cost);
        assert_eq!(r.cost[&Provenance::Estimated], MicroUsd(30_000_000));
        assert_eq!(r.cost[&Provenance::ProviderReported], MicroUsd(4_000_000));
        assert_eq!(r.cost_unavailable, 5);
        let window = |resets_at| QuotaKey {
            agent: "codex".into(),
            plan: Some("prolite".into()),
            window_minutes: 10080,
            resets_at,
        };
        assert_eq!(r.quota_bp[&window(1)], 800 + 500, "overlap counted once");
        assert_eq!(r.quota_bp[&window(2)], 200);
        assert_eq!(r.quota_unavailable, 3);
    }

    // --- export mapping ---

    #[test]
    fn the_export_names_provenance_and_reasons_and_uses_1741s_run_keys() {
        let record = RunRecord::new(
            RunIdentity {
                work_item: Some("issue-1".into()),
                ..identity("r1", "opencode")
            },
            from_opencode(&[OpenCodeMessageUsage {
                model: "gpt-5.5".into(),
                input: 5,
                output: 3,
                cache_read: 0,
                cache_write: 0,
                cost_usd: 0.0,
            }]),
            Some(EndReason::Exited),
        );
        let export = run_attributes(&record);
        let has = |attrs: &[AccountingAttribute], key: AccountingKey, value: &str| {
            attrs.contains(&s(key, value))
        };
        assert!(has(&export.run, AccountingKey::RunId, "r1"));
        assert!(has(&export.run, AccountingKey::Termination, "unknown"));
        assert!(has(
            &export.run,
            AccountingKey::TerminationProvenance,
            "measured"
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
            verification: verification(None, Some(true)),
        };
        let attrs = work_item_attributes(&item);
        assert!(has(&attrs, AccountingKey::Disposition, "merged"));
        assert!(has(&attrs, AccountingKey::Verification, "verified"));
        assert!(has(
            &attrs,
            AccountingKey::VerificationProvenance,
            "agent_asserted"
        ));
    }

    #[test]
    fn every_accounting_key_is_unique_and_reuses_1741s_names_where_shared() {
        let names: std::collections::HashSet<_> =
            AccountingKey::ALL.iter().map(|k| k.as_str()).collect();
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
}
