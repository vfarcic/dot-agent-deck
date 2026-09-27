//! A provider quota block, as the agent itself reports it (issue #714).
//!
//! A worker whose provider refuses every request because its usage limit or
//! credit pool is spent stops working, and the deck marks its card
//! [`crate::state::SessionStatus::Blocked`]. This module holds the reason types
//! that ride beside that status, the metadata keys that carry them on a
//! `quota_blocked` event, and the rule for what lifts a block again.
//!
//! The block itself is reported from a STRUCTURED signal and nothing else: the
//! Claude Code `StopFailure` hook (with the transcript's `quotaLimits` record),
//! OpenCode's `session.error` fields, and the Codex rollout's
//! `task_complete.error` record read by the daemon
//! (`crate::codex_rollout_tail`). The classifiers for those live in
//! [`crate::quota_signals`]. Nothing here reads a pane's screen or PTY output.

use serde::{Deserialize, Serialize};

use crate::untrusted_text::strip_control_and_bidi;

/// Metadata key on a `quota_blocked` event carrying the [`BlockedKind`] wire
/// value. Set by the producer; the daemon maps anything it does not know to
/// [`BlockedKind::Unknown`] and strips the key from every other event type.
pub const QUOTA_BLOCKED_KIND_METADATA_KEY: &str = "quota_blocked_kind";

/// Metadata key on a `quota_blocked` event carrying the agent's own error
/// message ([`BlockedReason::detail`]). Display only.
pub const QUOTA_BLOCKED_DETAIL_METADATA_KEY: &str = "quota_blocked_detail";

/// Metadata key on a `quota_blocked` event carrying, as decimal epoch
/// milliseconds, when the provider says the limit resets
/// ([`BlockedReason::resets_at_ms`]). Optional; the daemon drops a value that
/// does not parse or lies outside a sane window.
pub const QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY: &str = "quota_blocked_resets_at_ms";

/// Metadata key marking the `quota_blocked` event the DAEMON authored from a
/// Codex rollout (value [`QUOTA_BLOCKED_SOURCE_CODEX_ROLLOUT`]). Daemon-owned:
/// the hook loop strips it from every producer frame, and
/// [`crate::event::AgentEvent::is_daemon_synthetic`] keys on it.
pub const QUOTA_BLOCKED_SOURCE_METADATA_KEY: &str = "quota_blocked_source";

/// The one [`QUOTA_BLOCKED_SOURCE_METADATA_KEY`] value.
pub const QUOTA_BLOCKED_SOURCE_CODEX_ROLLOUT: &str = "codex_rollout";

/// Metadata key marking the `Idle` event the DAEMON broadcasts when a pane
/// restart replaced a blocked agent (value
/// [`QUOTA_BLOCKED_LIFTED_BY_PANE_RESTART`]): the replaced agent's `Blocked`
/// card is lifted, because the agent it describes is gone. Daemon-owned like
/// [`QUOTA_BLOCKED_SOURCE_METADATA_KEY`]: stripped from every producer frame,
/// and [`crate::event::AgentEvent::is_daemon_synthetic`] keys on it. See
/// `crate::state::AppState::lift_replaced_quota_blocks`.
pub const QUOTA_BLOCKED_LIFTED_METADATA_KEY: &str = "quota_blocked_lifted";

/// The one [`QUOTA_BLOCKED_LIFTED_METADATA_KEY`] value.
pub const QUOTA_BLOCKED_LIFTED_BY_PANE_RESTART: &str = "pane_restart";

/// Every `quota_blocked_*` metadata key, so the daemon can strip them together
/// from any event that is not a `quota_blocked` one.
pub const QUOTA_BLOCKED_METADATA_KEYS: [&str; 5] = [
    QUOTA_BLOCKED_KIND_METADATA_KEY,
    QUOTA_BLOCKED_DETAIL_METADATA_KEY,
    QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY,
    QUOTA_BLOCKED_SOURCE_METADATA_KEY,
    QUOTA_BLOCKED_LIFTED_METADATA_KEY,
];

/// Metadata key forwarded on a Claude-shaped `Notification` event
/// (`WaitingForInput`) with the hook's `notification_type`. See
/// [`is_work_evidence`].
pub const NOTIFICATION_TYPE_METADATA_KEY: &str = "notification_type";

/// The `notification_type` values that are raised INSIDE a turn — a permission
/// or elicitation prompt — and so count as work evidence. Every other value
/// (notably `idle_prompt`, which Claude Code fires about a minute after a turn
/// ends, blocked or not) does not.
pub const WORKING_NOTIFICATION_TYPES: [&str; 4] = [
    "permission_prompt",
    "worker_permission_prompt",
    "elicitation_dialog",
    "elicitation_url_dialog",
];

/// Upper bound, in characters, on a stored [`BlockedReason::detail`].
pub const MAX_DETAIL_CHARS: usize = 160;

/// Which provider limit the agent reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    /// A usage limit, usually windowed: it may reset on its own, which the deck
    /// learns only when the agent works again.
    UsageLimit,
    /// A spent credit pool or billing balance: it does not reset without
    /// someone acting.
    CreditsDepleted,
    /// A provider limit whose kind the report did not say, or a kind this build
    /// does not know (forward-compat catch-all).
    #[serde(other)]
    Unknown,
}

impl BlockedKind {
    /// The snake_case wire value, as carried in
    /// [`QUOTA_BLOCKED_KIND_METADATA_KEY`].
    pub fn as_wire(self) -> &'static str {
        match self {
            BlockedKind::UsageLimit => "usage_limit",
            BlockedKind::CreditsDepleted => "credits_depleted",
            BlockedKind::Unknown => "unknown",
        }
    }

    /// Inverse of [`BlockedKind::as_wire`]; anything unrecognised is
    /// [`BlockedKind::Unknown`], matching the serde behaviour.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "usage_limit" => BlockedKind::UsageLimit,
            "credits_depleted" => BlockedKind::CreditsDepleted,
            _ => BlockedKind::Unknown,
        }
    }

    /// Fixed, daemon-authored card label for this kind.
    pub fn label(self) -> &'static str {
        match self {
            BlockedKind::UsageLimit => "Usage limit reached",
            BlockedKind::CreditsDepleted => "Credits depleted (no reset)",
            BlockedKind::Unknown => "Provider limit reached",
        }
    }
}

/// Why a session is `Blocked`. An additive optional field beside the unit
/// `SessionStatus::Blocked`, never a payload on it (a payload variant breaks
/// every older reader's decode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedReason {
    pub kind: BlockedKind,
    /// Epoch milliseconds of the event that reported the block.
    pub detected_at_ms: i64,
    /// The agent's own error message, control/bidi-stripped and at most
    /// [`MAX_DETAIL_CHARS`]. Agent-controlled text: DISPLAY ONLY, never
    /// interpolated into a submitted notice or a delegate reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Epoch milliseconds at which the provider said the limit resets, when it
    /// said. Additive optional, like `detail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
}

/// Control/bidi-strip `text`, collapse its whitespace and bound it to
/// [`MAX_DETAIL_CHARS`] — the one scrub every [`BlockedReason::detail`] passes
/// through, at the producer and again wherever the value is ingested.
pub fn scrub_detail(text: &str) -> String {
    let stripped = strip_control_and_bidi(text, false);
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_DETAIL_CHARS {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(MAX_DETAIL_CHARS - 1).collect();
    out.push('…');
    out
}

/// Whether `event` is evidence that the pane's agent is WORKING — the set that
/// clears a `Blocked` card and the daemon's per-agent block latch.
///
/// A turn is underway or starting: a submitted prompt (`Thinking`), a tool, a
/// subagent, a compaction, a permission request, a genuine session start, or a
/// `WaitingForInput` raised inside a turn. NOT evidence: `Idle` and `Error`
/// (OpenCode sends `session.error` and `session.idle` right beside a quota
/// failure), the shell-activity pair, `Unknown`, and anything the daemon or the
/// wrapper synthesized rather than the agent reporting — a wrapper's
/// fork/interface `SessionStart`, the card-surfacing start, and the wrapper's
/// stdout line classifier, which calls every printed line `Working`.
///
/// Nor anything a SUBAGENT reported ([`crate::event::SUBAGENT_ID_METADATA_KEY`],
/// issue #1354). The card and the latch describe the main thread, and a
/// subagent's calls say nothing about whether its provider still refuses the
/// main thread — it may run on another model with a quota of its own. Worse, a
/// background agent Claude Code runs after the blocked turn ended would lift
/// the card to `Thinking`, and since a subagent's events assert no status,
/// nothing would ever end that `Thinking`. The main thread's own call to the
/// tool that launches a subagent carries no such key and still counts.
///
/// A `WaitingForInput` carrying [`NOTIFICATION_TYPE_METADATA_KEY`] counts only
/// for one of [`WORKING_NOTIFICATION_TYPES`]: Claude Code fires `idle_prompt`
/// once a turn has ended, including one that ended on a quota failure, so
/// counting it would clear a genuine block about a minute after it was
/// reported. One with no type (an older Claude, Devin, OpenCode's `waiting`)
/// keeps its meaning and counts.
pub fn is_work_evidence(event: &crate::event::AgentEvent) -> bool {
    use crate::event::EventType;
    let work_type = match event.event_type {
        EventType::ToolStart
        | EventType::ToolEnd
        | EventType::Thinking
        | EventType::Compacting
        | EventType::SubagentStart
        | EventType::SubagentStop
        | EventType::PermissionRequest
        | EventType::SessionStart => true,
        EventType::WaitingForInput => event
            .metadata
            .get(NOTIFICATION_TYPE_METADATA_KEY)
            .is_none_or(|kind| WORKING_NOTIFICATION_TYPES.contains(&kind.as_str())),
        _ => false,
    };
    work_type
        && !event.is_from_subagent()
        && !event.is_daemon_synthetic()
        && !event.is_wrapper_session_start()
        && !event.is_wrapper_output_classified()
}
