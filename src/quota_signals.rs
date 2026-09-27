//! Classify an agent's STRUCTURED failure signals into a provider quota block
//! (issue #714).
//!
//! Each supported agent reports a provider failure through a machine-readable
//! channel, and each classifier here reads exactly that channel:
//!
//! * Claude Code — the `StopFailure` hook's `error` field, confirmed against
//!   the transcript's last API-error record ([`classify_claude_stop_failure`]).
//! * OpenCode — the `session.error` event's error name, status, JSON response
//!   body and an allow-list of response headers ([`classify_opencode_error`]).
//! * Codex — the rollout JSONL's `task_complete` record for an armed turn,
//!   with the kind from that turn's last `token_count` ([`CodexTurnWatch`]).
//!
//! Every marker is a JSON key compared for string equality. No free text is
//! matched, and no screen or PTY output is read: a signal that exists only as
//! an English message is not covered, and a miss is a false negative (the card
//! shows `Error` or keeps its status), never a false `Blocked`.
//!
//! Everything here is pure and clock-injected except
//! [`read_claude_transcript_tail`], the bounded read the Claude hook process
//! makes of the transcript its own agent named.

use std::collections::HashMap;

use serde_json::Value;

use crate::quota_block::BlockedKind;

/// What a provider failure means for the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureOutcome {
    /// The provider refused the agent for a quota or credit limit.
    Blocked {
        kind: BlockedKind,
        /// When the provider says the limit resets, in epoch milliseconds.
        resets_at_ms: Option<i64>,
    },
    /// Any other failure: the turn ended on an error.
    Error,
}

impl FailureOutcome {
    fn blocked(kind: BlockedKind, resets_at_ms: Option<i64>) -> Self {
        FailureOutcome::Blocked { kind, resets_at_ms }
    }
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

/// How much of a Claude transcript the hook reads, from its end.
pub const CLAUDE_TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

/// Whether a `StopFailure` with this `error` needs the transcript to decide.
/// Only `rate_limit` is ambiguous: Claude Code assigns it to a subscription
/// usage limit, to an "extra usage credits required" refusal and to a transient
/// capacity 429 alike.
pub fn claude_stop_failure_needs_transcript(error: Option<&str>) -> bool {
    error == Some("rate_limit")
}

/// The record Claude Code wrote for the API error that ended the turn: the
/// last main-chain `assistant` record in `tail`, if it is an API-error one
/// (`isApiErrorMessage: true`). `None` when the last assistant record is not an
/// API error, or there is none.
///
/// Records of other types after it (Claude's own `system` records such as
/// `turn_duration`) are skipped, and so are sidechain (subagent) records. Only
/// lines naming `"assistant"` are parsed, and a line that does not parse is
/// skipped. [`read_claude_transcript_tail`] has already dropped a first line
/// that its window cut in half.
pub fn last_claude_api_error_record(tail: &[u8]) -> Option<Value> {
    for line in tail.split(|&b| b == b'\n').rev() {
        if !contains(line, b"\"assistant\"") {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) != Some("assistant")
            || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        return (record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true))
            .then_some(record);
    }
    None
}

/// Classify a Claude Code `StopFailure`: its `error` kind and, for the
/// ambiguous `rate_limit`, the API-error record from the transcript
/// ([`last_claude_api_error_record`]).
///
/// | `error` | record | outcome |
/// | --- | --- | --- |
/// | `billing_error` | not needed | credits depleted |
/// | `rate_limit` | `apiErrorIsTransient: true` | `Error` |
/// | `rate_limit` | `quotaLimits.status == "rejected"` | usage limit, reset from `quotaLimits.resetsAt` |
/// | `rate_limit` | `apiError == "model_requires_usage_credits"` | credits depleted |
/// | `rate_limit` | anything else, or no record | `Error` |
/// | any other kind | not read | `Error` |
///
/// `StopFailure` fires INSTEAD of `Stop`, so every kind maps to something: an
/// unmapped kind would leave the card `Thinking` after the turn has ended.
pub fn classify_claude_stop_failure(error: Option<&str>, record: Option<&Value>) -> FailureOutcome {
    match error {
        Some("billing_error") => FailureOutcome::blocked(BlockedKind::CreditsDepleted, None),
        Some("rate_limit") => {
            let Some(record) = record else {
                return FailureOutcome::Error;
            };
            if record.get("apiErrorIsTransient").and_then(Value::as_bool) == Some(true) {
                return FailureOutcome::Error;
            }
            let quota = record.get("quotaLimits");
            if quota.and_then(|q| q.get("status")).and_then(Value::as_str) == Some("rejected") {
                let resets_at_ms = quota
                    .and_then(|q| q.get("resetsAt"))
                    .and_then(Value::as_i64)
                    .and_then(|secs| secs.checked_mul(1000));
                return FailureOutcome::blocked(BlockedKind::UsageLimit, resets_at_ms);
            }
            if record.get("apiError").and_then(Value::as_str)
                == Some("model_requires_usage_credits")
            {
                return FailureOutcome::blocked(BlockedKind::CreditsDepleted, None);
            }
            FailureOutcome::Error
        }
        _ => FailureOutcome::Error,
    }
}

/// Read at most the last [`CLAUDE_TRANSCRIPT_TAIL_BYTES`] of the transcript at
/// `path`, which the agent's own `StopFailure` payload named.
///
/// Refused (`None`) unless the path is absolute and ends in `.jsonl`, opens
/// read-only, and the opened file is a regular file. On Unix the open is
/// non-blocking, so a FIFO planted at the path cannot hang the hook; it is then
/// refused by the regular-file check. When the read starts mid-file, a first
/// line the window cut in half is dropped; one that starts exactly at the
/// window's edge is kept.
pub fn read_claude_transcript_tail(path: &str) -> Option<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let path = std::path::Path::new(path);
    if !path.is_absolute() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    let start = len.saturating_sub(CLAUDE_TRANSCRIPT_TAIL_BYTES);
    // A window that starts mid-file also reads the byte before it, so the
    // first line is dropped only when the window cut it: when that byte is a
    // newline the window starts on a complete record, and only it is dropped.
    let lead = u64::from(start > 0);
    file.seek(SeekFrom::Start(start - lead)).ok()?;
    let mut buf = Vec::with_capacity((len - start + lead) as usize);
    file.take(CLAUDE_TRANSCRIPT_TAIL_BYTES + lead)
        .read_to_end(&mut buf)
        .ok()?;
    if lead > 0 {
        let cut = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |i| i + 1);
        buf.drain(..cut);
    }
    Some(buf)
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------

/// The structured fields the deck's OpenCode plugin forwards from a
/// `session.error` event (`src/opencode_manage.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenCodeErrorFields {
    /// `error.name`, e.g. `APIError`.
    pub error_name: Option<String>,
    /// The marker keys of `error.data.responseBody`, which the plugin parses
    /// WHOLE and reduces to the keys below, at the same paths — never a
    /// truncated copy of the body, which a long provider error would turn into
    /// invalid JSON. `None` when the body was absent, not a JSON object, or too
    /// large for the plugin to parse.
    pub response_markers: Option<Value>,
    /// The allow-listed response headers, names lowercased.
    pub response_headers: HashMap<String, String>,
}

/// Classify an OpenCode `session.error`.
///
/// Only an `APIError` is considered, and only these key-anchored markers block.
/// The body keys are read from [`OpenCodeErrorFields::response_markers`], which
/// the plugin extracts from the whole body; a body that is not JSON arrives as
/// no markers:
///
/// | marker | outcome |
/// | --- | --- |
/// | `error.code` or `error.type` == `insufficient_quota` | credits depleted |
/// | `error.code` == `usage_not_included` | credits depleted |
/// | `error.type` == `usage_limit_reached` | usage limit (credits depleted when header `x-codex-rate-limit-reached-type` ends in `_credits_depleted`); reset from `error.resets_at` (s) or `error.resets_in_seconds` |
/// | `GoUsageLimitError` as `type`, `name`, `error.type`, `error.name` or `error.code` | usage limit; reset from `retry-after` |
/// | `FreeUsageLimitError` at the same keys | credits depleted |
/// | header `anthropic-ratelimit-unified-status` == `rejected` | usage limit; reset from `anthropic-ratelimit-unified-reset` (s) |
///
/// Anything else — including a bare status 429 — is `Error`: a 429 alone is
/// also what a transient rate limit that outlasted OpenCode's retries looks
/// like.
pub fn classify_opencode_error(fields: &OpenCodeErrorFields, now_ms: i64) -> FailureOutcome {
    if fields.error_name.as_deref() != Some("APIError") {
        return FailureOutcome::Error;
    }
    let header = |name: &str| {
        fields
            .response_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    let retry_after_ms = || {
        header("retry-after-ms")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|ms| ms as i64)
            .or_else(|| {
                header("retry-after")
                    .and_then(|v| v.parse::<i64>().ok())
                    .and_then(|s| s.checked_mul(1000))
            })
            .filter(|&ms| ms > 0)
            .map(|ms| now_ms.saturating_add(ms))
    };
    let body: Option<&Value> = fields.response_markers.as_ref().filter(|b| b.is_object());
    let error = body.and_then(|b| b.get("error"));
    fn at<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a str> {
        v.and_then(|v| v.get(key)).and_then(Value::as_str)
    }
    let err_code = at(error, "code");
    let err_type = at(error, "type");

    if err_code == Some("insufficient_quota") || err_type == Some("insufficient_quota") {
        return FailureOutcome::blocked(BlockedKind::CreditsDepleted, None);
    }
    if err_code == Some("usage_not_included") {
        return FailureOutcome::blocked(BlockedKind::CreditsDepleted, None);
    }
    if err_type == Some("usage_limit_reached") {
        let credits = header("x-codex-rate-limit-reached-type")
            .is_some_and(|v| v.ends_with("_credits_depleted"));
        if credits {
            return FailureOutcome::blocked(BlockedKind::CreditsDepleted, None);
        }
        let resets = error
            .and_then(|e| e.get("resets_at"))
            .and_then(Value::as_i64)
            .and_then(|s| s.checked_mul(1000))
            .or_else(|| {
                error
                    .and_then(|e| e.get("resets_in_seconds"))
                    .and_then(Value::as_i64)
                    .and_then(|s| s.checked_mul(1000))
                    .map(|ms| now_ms.saturating_add(ms))
            });
        return FailureOutcome::blocked(BlockedKind::UsageLimit, resets);
    }
    let named = |marker: &str| {
        [
            at(body, "type"),
            at(body, "name"),
            err_type,
            at(error, "name"),
            err_code,
        ]
        .contains(&Some(marker))
    };
    if named("GoUsageLimitError") {
        return FailureOutcome::blocked(BlockedKind::UsageLimit, retry_after_ms());
    }
    if named("FreeUsageLimitError") {
        return FailureOutcome::blocked(BlockedKind::CreditsDepleted, None);
    }
    if header("anthropic-ratelimit-unified-status") == Some("rejected") {
        let resets = header("anthropic-ratelimit-unified-reset")
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|s| s.checked_mul(1000));
        return FailureOutcome::blocked(BlockedKind::UsageLimit, resets);
    }
    FailureOutcome::Error
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// What one Codex rollout line meant for the turn being watched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexLineOutcome {
    /// Nothing that ends the watched turn.
    Nothing,
    /// The watched turn completed without a quota failure; stop watching.
    TurnEnded,
    /// The watched turn failed on the provider's usage limit.
    Blocked {
        kind: BlockedKind,
        resets_at_ms: Option<i64>,
        /// The `task_complete` error message, unscrubbed.
        message: Option<String>,
    },
}

/// One rate-limit window of a Codex `token_count` record.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CodexWindow {
    used_percent: f64,
    resets_at_s: Option<i64>,
}

/// The rate-limit facts from the last relevant `token_count` record.
#[derive(Debug, Clone, Default, PartialEq)]
struct CodexRateLimits {
    reached_type: Option<String>,
    windows: Vec<CodexWindow>,
}

/// Watch a Codex rollout for the outcome of ONE turn, named by the `turn_id`
/// the Codex `UserPromptSubmit` hook carried.
///
/// A block needs a `task_complete` event for exactly that turn whose
/// `error.codex_error_info` is `usage_limit_exceeded`. The kind comes from the
/// last `token_count` seen for the turn, via `rate_limits.rate_limit_reached_type`:
///
/// | `rate_limit_reached_type` | outcome |
/// | --- | --- |
/// | `workspace_{owner,member}_credits_depleted` | credits depleted, no reset |
/// | `workspace_{owner,member}_usage_limit_reached`, `rate_limit_reached` | usage limit; reset from the windows |
/// | none | unknown (the API-key quota case) |
///
/// Never a block: `rate_limits.credits.has_credits: false` (it appears on
/// healthy sessions), any other `codex_error_info`, a `task_complete` for any
/// other turn, and a line that is not JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexTurnWatch {
    turn_id: String,
    rate_limits: Option<CodexRateLimits>,
}

impl CodexTurnWatch {
    pub fn new(turn_id: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            rate_limits: None,
        }
    }

    /// The turn this watch is for.
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// Whether a raw rollout line could matter to any watch — the cheap
    /// pre-filter that decides which lines are JSON-parsed at all. The parse is
    /// what decides.
    pub fn line_is_candidate(line: &[u8]) -> bool {
        contains(line, b"\"task_complete\"")
            || contains(line, b"\"task_started\"")
            || contains(line, b"\"token_count\"")
    }

    /// Feed one complete rollout line (without its newline).
    pub fn observe_line(&mut self, line: &[u8]) -> CodexLineOutcome {
        if !Self::line_is_candidate(line) {
            return CodexLineOutcome::Nothing;
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            return CodexLineOutcome::Nothing;
        };
        if record.get("type").and_then(Value::as_str) != Some("event_msg") {
            return CodexLineOutcome::Nothing;
        }
        let Some(payload) = record.get("payload") else {
            return CodexLineOutcome::Nothing;
        };
        let turn = payload.get("turn_id").and_then(Value::as_str);
        match payload.get("type").and_then(Value::as_str) {
            Some("task_started") => {
                // Whichever turn starts, the token counts seen so far belong to
                // an earlier one.
                self.rate_limits = None;
                CodexLineOutcome::Nothing
            }
            Some("token_count") => {
                if turn.is_none_or(|t| t == self.turn_id)
                    && let Some(limits) = payload.get("rate_limits")
                {
                    self.rate_limits = Some(parse_codex_rate_limits(limits));
                }
                CodexLineOutcome::Nothing
            }
            Some("task_complete") if turn == Some(self.turn_id.as_str()) => {
                let error = payload.get("error").filter(|e| e.is_object());
                let info = error.and_then(|e| e.get("codex_error_info"));
                let usage_limit = match info {
                    Some(Value::String(s)) => s == "usage_limit_exceeded",
                    Some(Value::Object(map)) => map.contains_key("usage_limit_exceeded"),
                    _ => false,
                };
                if !usage_limit {
                    return CodexLineOutcome::TurnEnded;
                }
                let (kind, resets_at_ms) = self
                    .rate_limits
                    .as_ref()
                    .map_or((BlockedKind::Unknown, None), codex_kind_and_reset);
                CodexLineOutcome::Blocked {
                    kind,
                    resets_at_ms,
                    message: error
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }
            }
            _ => CodexLineOutcome::Nothing,
        }
    }
}

fn parse_codex_rate_limits(limits: &Value) -> CodexRateLimits {
    let windows = ["primary", "secondary"]
        .iter()
        .filter_map(|key| limits.get(*key).filter(|w| w.is_object()))
        .map(|w| CodexWindow {
            used_percent: w.get("used_percent").and_then(Value::as_f64).unwrap_or(0.0),
            resets_at_s: w.get("resets_at").and_then(Value::as_i64),
        })
        .collect();
    CodexRateLimits {
        reached_type: limits
            .get("rate_limit_reached_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        windows,
    }
}

fn codex_kind_and_reset(limits: &CodexRateLimits) -> (BlockedKind, Option<i64>) {
    match limits.reached_type.as_deref() {
        Some("workspace_owner_credits_depleted" | "workspace_member_credits_depleted") => {
            (BlockedKind::CreditsDepleted, None)
        }
        Some(
            "workspace_owner_usage_limit_reached"
            | "workspace_member_usage_limit_reached"
            | "rate_limit_reached",
        ) => {
            // The later reset of the windows that are exhausted, else the
            // earlier reset of any window.
            let exhausted = limits
                .windows
                .iter()
                .filter(|w| w.used_percent >= 100.0)
                .filter_map(|w| w.resets_at_s)
                .max();
            let any = limits.windows.iter().filter_map(|w| w.resets_at_s).min();
            let resets = exhausted.or(any).and_then(|s| s.checked_mul(1000));
            (BlockedKind::UsageLimit, resets)
        }
        _ => (BlockedKind::Unknown, None),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// The record Claude Code 2.1.283 wrote when a subscription's session limit
    /// ended a turn, verbatim from a transcript on the development machine
    /// (2026-09-26, issue #714's investigation).
    const OBSERVED_CLAUDE_QUOTA_RECORD: &str = r#"{"parentUuid":"22bb71eb-7f43-4c0c-87fc-3e032a675e15","isSidechain":false,"type":"assistant","uuid":"b23235c6-af65-4595-a31e-01ddf9b58617","timestamp":"2026-09-26T07:20:46.304Z","message":{"diagnostics":null,"id":"8329a87c-0097-4d5e-a7e8-86a7b86e2dd6","container":null,"model":"<synthetic>","role":"assistant","stop_details":null,"stop_reason":"stop_sequence","stop_sequence":"","type":"message","usage":{"output_tokens_details":null,"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":null,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":null,"iterations":null,"speed":null},"content":[{"type":"text","text":"You've hit your individual spend limit · run /usage-credits to ask your admin for a higher limit · your session limit resets 7:50am (UTC)"}],"context_management":null},"requestId":"req_011CfReT1AfV2r2jQNpbgXaT","quotaLimits":{"status":"rejected","resetsAt":1790409000,"unifiedRateLimitFallbackAvailable":false,"rateLimitType":"five_hour","overageStatus":"rejected","overageResetsAt":1790812800,"overageDisabledReason":"org_spend_cap_reached","upgradePaths":["overage"],"isUsingOverage":false},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,"perTurnEffort":"high","session_id":"1236a0e4-a23b-45e5-9cc0-82b3e548e468","userType":"external","entrypoint":"cli","cwd":"/home/vfarcic/code/dot-agent-deck-dispatch-issue-714","sessionId":"1236a0e4-a23b-45e5-9cc0-82b3e548e468","version":"2.1.283","gitBranch":"agent/dispatch-issue-714"}"#;

    /// The two `system` records Claude Code wrote right after it.
    const TRAILING_SYSTEM_RECORDS: &str = concat!(
        r#"{"parentUuid":"b23235c6-af65-4595-a31e-01ddf9b58617","isSidechain":false,"type":"system","subtype":"informational","content":"Usage limit reached","isMeta":false}"#,
        "\n",
        r#"{"parentUuid":"7ff310df-ab4a-4b66-9625-ac471933b722","isSidechain":false,"type":"system","subtype":"turn_duration","durationMs":613}"#,
        "\n"
    );

    fn transcript(lines: &[&str]) -> Vec<u8> {
        let mut out = String::new();
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        out.into_bytes()
    }

    fn classify_tail(error: &str, tail: &[u8]) -> FailureOutcome {
        let record = last_claude_api_error_record(tail);
        classify_claude_stop_failure(Some(error), record.as_ref())
    }

    /// Scenario: Classify Claude Code `StopFailure` hooks against transcript
    /// tails, starting from the real quota record Claude Code wrote. A
    /// rejected-quota `rate_limit` blocks with its reset, `billing_error` and a
    /// usage-credits refusal block as credits, and a transient 429, a missing or
    /// superseded record, and every other error kind end the turn as Error.
    #[spec("status/blocked/010")]
    #[test]
    fn status_blocked_010_claude_stop_failure_classifies_from_hook_and_transcript() {
        let user =
            r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":"go"}}"#;
        let observed = transcript(&[user, OBSERVED_CLAUDE_QUOTA_RECORD]);
        let mut with_trailer = observed.clone();
        with_trailer.extend_from_slice(TRAILING_SYSTEM_RECORDS.as_bytes());
        for tail in [&observed, &with_trailer] {
            assert_eq!(
                classify_tail("rate_limit", tail),
                FailureOutcome::blocked(BlockedKind::UsageLimit, Some(1_790_409_000_000)),
            );
        }

        // billing_error needs no transcript at all.
        assert!(!claude_stop_failure_needs_transcript(Some("billing_error")));
        assert!(claude_stop_failure_needs_transcript(Some("rate_limit")));
        assert_eq!(
            classify_claude_stop_failure(Some("billing_error"), None),
            FailureOutcome::blocked(BlockedKind::CreditsDepleted, None)
        );

        let credits = r#"{"type":"assistant","isSidechain":false,"error":"rate_limit","isApiErrorMessage":true,"apiError":"model_requires_usage_credits"}"#;
        assert_eq!(
            classify_tail("rate_limit", &transcript(&[user, credits])),
            FailureOutcome::blocked(BlockedKind::CreditsDepleted, None)
        );

        // Negatives.
        let transient = r#"{"type":"assistant","isSidechain":false,"error":"rate_limit","isApiErrorMessage":true,"apiErrorIsTransient":true,"quotaLimits":{"status":"rejected","resetsAt":1}}"#;
        assert_eq!(
            classify_tail("rate_limit", &transcript(&[user, transient])),
            FailureOutcome::Error,
            "a transient 429 that outlived Claude's retries is not a quota block"
        );
        let bare = r#"{"type":"assistant","isSidechain":false,"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#;
        assert_eq!(
            classify_tail("rate_limit", &transcript(&[user, bare])),
            FailureOutcome::Error
        );
        assert_eq!(
            classify_tail("rate_limit", &transcript(&[user])),
            FailureOutcome::Error,
            "no API-error record"
        );
        assert_eq!(
            classify_claude_stop_failure(Some("rate_limit"), None),
            FailureOutcome::Error
        );
        let later_ok = r#"{"type":"assistant","isSidechain":false,"message":{"content":"done"}}"#;
        assert_eq!(
            classify_tail(
                "rate_limit",
                &transcript(&[OBSERVED_CLAUDE_QUOTA_RECORD, user, later_ok])
            ),
            FailureOutcome::Error,
            "an API-error record that is not the last assistant record is history"
        );
        let sidechain = r#"{"type":"assistant","isSidechain":true,"message":{"content":"sub"}}"#;
        assert_eq!(
            classify_tail(
                "rate_limit",
                &transcript(&[user, OBSERVED_CLAUDE_QUOTA_RECORD, sidechain])
            ),
            FailureOutcome::blocked(BlockedKind::UsageLimit, Some(1_790_409_000_000)),
            "a subagent's record does not hide the main chain's"
        );
        let malformed = b"{not json \"assistant\"\n".to_vec();
        assert_eq!(
            classify_tail("rate_limit", &malformed),
            FailureOutcome::Error
        );
        for other in [
            "overloaded",
            "server_error",
            "authentication_failed",
            "model_not_found",
            "invalid_request",
            "unknown",
        ] {
            assert_eq!(
                classify_tail(other, &observed),
                FailureOutcome::Error,
                "{other} is not a quota block"
            );
        }
        assert_eq!(
            classify_claude_stop_failure(None, None),
            FailureOutcome::Error
        );
    }

    fn token_count(turn: Option<&str>, reached: Option<&str>, has_credits: bool) -> String {
        let turn = turn.map_or(String::new(), |t| format!(r#""turn_id":"{t}","#));
        let reached = reached.map_or("null".to_string(), |r| format!("\"{r}\""));
        format!(
            r#"{{"timestamp":"2026-09-26T04:19:31.813Z","type":"event_msg","payload":{{"type":"token_count",{turn}"info":null,"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":100.0,"window_minutes":300,"resets_at":1790714971}},"secondary":{{"used_percent":40.0,"window_minutes":10080,"resets_at":1791000000}},"credits":{{"has_credits":{has_credits},"unlimited":false,"balance":"0"}},"plan_type":"prolite","rate_limit_reached_type":{reached}}}}}}}"#
        )
    }

    fn task_started(turn: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-09-26T04:19:27.827Z","type":"event_msg","payload":{{"type":"task_started","turn_id":"{turn}","started_at":1790396367}}}}"#
        )
    }

    fn task_complete(turn: &str, info: Option<&str>) -> String {
        let error = info.map_or("null".to_string(), |i| {
            format!(r#"{{"message":"You've hit your usage limit.","codex_error_info":"{i}"}}"#)
        });
        format!(
            r#"{{"timestamp":"2026-09-26T04:20:00.000Z","type":"event_msg","payload":{{"type":"task_complete","turn_id":"{turn}","last_agent_message":null,"error":{error}}}}}"#
        )
    }

    fn run(watch: &mut CodexTurnWatch, lines: &[String]) -> Vec<CodexLineOutcome> {
        lines
            .iter()
            .map(|l| watch.observe_line(l.as_bytes()))
            .filter(|o| *o != CodexLineOutcome::Nothing)
            .collect()
    }

    /// Scenario: Feed a watch armed for one Codex turn the rollout records a
    /// usage-limit failure produces. It blocks only on that turn's
    /// `task_complete` with `usage_limit_exceeded`, takes the kind and reset
    /// from the turn's last `token_count`, and never blocks on another turn,
    /// on `has_credits:false`, on another error, or on malformed JSON.
    #[spec("status/blocked/012")]
    #[test]
    fn status_blocked_012_codex_rollout_records_block_only_the_armed_turn() {
        let turn = "01a0dbf0-839e-7d71-b4cb-b06e1dea7067";
        let blocked = |kind, resets_at_ms| CodexLineOutcome::Blocked {
            kind,
            resets_at_ms,
            message: Some("You've hit your usage limit.".to_string()),
        };

        let mut w = CodexTurnWatch::new(turn);
        assert_eq!(
            run(
                &mut w,
                &[
                    task_started(turn),
                    token_count(Some(turn), Some("workspace_member_credits_depleted"), false),
                    task_complete(turn, Some("usage_limit_exceeded")),
                ]
            ),
            vec![blocked(BlockedKind::CreditsDepleted, None)]
        );

        // Windowed: the exhausted window's reset. token_count without a turn id
        // (older Codex) counts when it is the last one before the completion.
        let mut w = CodexTurnWatch::new(turn);
        assert_eq!(
            run(
                &mut w,
                &[
                    token_count(None, Some("workspace_owner_credits_depleted"), true),
                    task_started(turn),
                    token_count(None, Some("rate_limit_reached"), true),
                    task_complete(turn, Some("usage_limit_exceeded")),
                ]
            ),
            vec![blocked(BlockedKind::UsageLimit, Some(1_790_714_971_000))]
        );

        // No reached type: the API-key quota case.
        let mut w = CodexTurnWatch::new(turn);
        assert_eq!(
            run(
                &mut w,
                &[
                    task_started(turn),
                    task_complete(turn, Some("usage_limit_exceeded"))
                ]
            ),
            vec![blocked(BlockedKind::Unknown, None)]
        );

        // Negatives.
        let mut w = CodexTurnWatch::new(turn);
        assert_eq!(
            run(
                &mut w,
                &[
                    task_started("other-turn"),
                    token_count(Some("other-turn"), Some("rate_limit_reached"), false),
                    task_complete("other-turn", Some("usage_limit_exceeded")),
                ]
            ),
            vec![],
            "a completion for a turn that was not armed never blocks"
        );
        let mut w = CodexTurnWatch::new(turn);
        assert_eq!(
            run(
                &mut w,
                &[
                    task_started(turn),
                    token_count(Some(turn), None, false),
                    task_complete(turn, None),
                ]
            ),
            vec![CodexLineOutcome::TurnEnded],
            "has_credits:false on a healthy turn is not a block"
        );
        let mut w = CodexTurnWatch::new(turn);
        assert_eq!(
            run(
                &mut w,
                &[task_complete(turn, Some("context_window_exceeded"))]
            ),
            vec![CodexLineOutcome::TurnEnded]
        );
        let mut w = CodexTurnWatch::new(turn);
        let truncated = task_complete(turn, Some("usage_limit_exceeded"));
        let truncated = truncated[..truncated.len() - 3].to_string();
        assert_eq!(run(&mut w, &[truncated]), vec![]);
        assert_eq!(
            w.observe_line(br#"{"type":"response_item","payload":{"type":"message"}}"#),
            CodexLineOutcome::Nothing
        );
    }

    /// `body` is a provider response body as text; like the plugin, a body
    /// that is not JSON yields no markers.
    fn opencode(body: Option<&str>, headers: &[(&str, &str)]) -> OpenCodeErrorFields {
        OpenCodeErrorFields {
            error_name: Some("APIError".to_string()),
            response_markers: body.and_then(|b| serde_json::from_str(b).ok()),
            response_headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// Scenario: Classify OpenCode `session.error` fields against each
    /// provider marker the design names. Each structured marker blocks with its
    /// kind and reset, while a bare 429, a Pi-shaped `rate_limit_error`, the
    /// real 400 unsupported-model body, a non-JSON body and a non-API error all
    /// stay Error.
    #[spec("status/blocked/014")]
    #[test]
    fn status_blocked_014_opencode_error_blocks_only_on_a_provider_marker() {
        let now = 1_790_000_000_000;
        let usage = |r| FailureOutcome::blocked(BlockedKind::UsageLimit, r);
        let credits = FailureOutcome::blocked(BlockedKind::CreditsDepleted, None);

        let cases: Vec<(OpenCodeErrorFields, FailureOutcome)> = vec![
            (
                opencode(
                    Some(
                        r#"{"error":{"code":"insufficient_quota","type":"insufficient_quota","message":"You exceeded your current quota"}}"#,
                    ),
                    &[],
                ),
                credits.clone(),
            ),
            (
                opencode(
                    Some(r#"{"error":{"type":"x","code":"usage_not_included"}}"#),
                    &[],
                ),
                credits.clone(),
            ),
            (
                opencode(
                    Some(
                        r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_at":1790001000}}"#,
                    ),
                    &[],
                ),
                usage(Some(1_790_001_000_000)),
            ),
            (
                opencode(
                    Some(r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":60}}"#),
                    &[],
                ),
                usage(Some(now + 60_000)),
            ),
            (
                opencode(
                    Some(r#"{"error":{"type":"usage_limit_reached"}}"#),
                    &[(
                        "x-codex-rate-limit-reached-type",
                        "workspace_member_credits_depleted",
                    )],
                ),
                credits.clone(),
            ),
            (
                opencode(
                    Some(r#"{"type":"error","error":{"type":"GoUsageLimitError","message":"m"}}"#),
                    &[("retry-after", "3600")],
                ),
                usage(Some(now + 3_600_000)),
            ),
            (
                opencode(Some(r#"{"name":"GoUsageLimitError"}"#), &[]),
                usage(None),
            ),
            (
                opencode(Some(r#"{"error":{"code":"FreeUsageLimitError"}}"#), &[]),
                credits.clone(),
            ),
            (
                opencode(
                    Some(r#"{"type":"error","error":{"type":"rate_limit_error","message":"x"}}"#),
                    &[
                        ("anthropic-ratelimit-unified-status", "rejected"),
                        ("anthropic-ratelimit-unified-reset", "1790409000"),
                    ],
                ),
                usage(Some(1_790_409_000_000)),
            ),
            // Negatives.
            (opencode(None, &[]), FailureOutcome::Error),
            (
                opencode(
                    Some(
                        r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your account's monthly spend limit. Please try again later."}}"#,
                    ),
                    &[("retry-after", "30")],
                ),
                FailureOutcome::Error,
            ),
            (
                opencode(
                    Some(
                        r#"{"error":{"message":"The requested model is not supported.","type":"invalid_request_error","param":"model","code":"model_not_supported"}}"#,
                    ),
                    &[],
                ),
                FailureOutcome::Error,
            ),
            (
                opencode(
                    Some("insufficient_quota usage_limit_reached GoUsageLimitError"),
                    &[],
                ),
                FailureOutcome::Error,
            ),
            (
                opencode(Some(r#"{"error":{"message":"insufficient_quota"}}"#), &[]),
                FailureOutcome::Error,
            ),
            (
                OpenCodeErrorFields {
                    error_name: Some("UnknownError".to_string()),
                    ..opencode(Some(r#"{"error":{"code":"insufficient_quota"}}"#), &[])
                },
                FailureOutcome::Error,
            ),
            (
                opencode(None, &[("anthropic-ratelimit-unified-status", "allowed")]),
                FailureOutcome::Error,
            ),
        ];
        for (fields, want) in cases {
            assert_eq!(classify_opencode_error(&fields, now), want, "{fields:?}");
        }
    }
}
