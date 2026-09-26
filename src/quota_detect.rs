//! Recognise an agent whose provider quota or credits are exhausted (issue #714).
//!
//! A worker whose provider refuses every request because its usage limit or
//! credit pool is spent stops working, but nothing about its hook stream says
//! so: Codex keeps its last `Thinking`, OpenCode reports a bare `Error` and then
//! `Idle`. The only honest evidence is the provider's own sentence on the
//! pane's screen. This module holds the two pure halves of reading it:
//!
//! * [`classify`] — given the trailing rows of a pane's reconstructed screen
//!   (`pane_screen_text::visible_tail_lines`, never raw bytes) and the pane's
//!   agent type, decide whether the screen is showing a provider quota error.
//! * [`QuotaDetector`] — the per-pane quiet/confirm state machine that decides
//!   *when* to look and when a match is trustworthy enough to report.
//!
//! Both are clock-injected and I/O-free; the daemon owns the byte hint, the
//! screen replay and the synthetic event.
//!
//! **Precision is the design constraint.** A false `Blocked` on a slow, healthy
//! agent is worse than today's stale status, so every guard below must pass:
//! patterns keyed to the agent type, whole-row anchoring after stripping only a
//! fixed set of TUI chrome glyphs, recency (the bottom [`QUOTA_TAIL_ROWS`]
//! non-blank rows), cancellation by any work-proving event, and a second
//! matching probe [`DEFAULT_QUOTA_CONFIRM`] after the first. Every miss is a
//! false negative, which is today's behaviour — that failure direction is
//! deliberate.
//!
//! **Named residual.** A pane of the right agent type that ends its turn with
//! the bare provider sentence as one of its last visible rows (for example a
//! Codex agent `cat`ing a fixture for this very feature), then emits no work
//! event and stays quiet through the confirmation window, is reported
//! `Blocked`. It clears on that agent's next work event.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::event::AgentType;
use crate::untrusted_text::strip_control_and_bidi;

/// Metadata key on a daemon-synthesized `quota_blocked` event carrying the
/// [`BlockedKind`] wire value. Daemon-owned: producers must not be able to set it.
pub const QUOTA_BLOCKED_KIND_METADATA_KEY: &str = "quota_blocked_kind";

/// Metadata key on a daemon-synthesized `quota_blocked` event carrying the
/// scrubbed matched text ([`BlockedReason::detail`]). Daemon-owned.
pub const QUOTA_BLOCKED_DETAIL_METADATA_KEY: &str = "quota_blocked_detail";

/// Only the bottom this-many non-blank screen rows may carry the match: the
/// error cell sits just above the composer and footer, and a line that has
/// scrolled further up no longer describes the pane's current state.
pub const QUOTA_TAIL_ROWS: usize = 10;

/// How many rows after the matched row are read as its continuation (a wrapped
/// message, or the suffix that decides [`BlockedKind`]).
const CONTINUATION_ROWS: usize = 2;

/// Longest [`BlockedReason::detail`], in characters, including the `…` marker.
pub const MAX_DETAIL_CHARS: usize = 160;

/// The PTY must have been silent this long before the screen is probed. A
/// working agent's TUI animates (spinners, elapsed-time counters), so a pane
/// that keeps emitting bytes is never probed at all.
pub const DEFAULT_QUOTA_QUIET: Duration = Duration::from_secs(5);

/// A first matching probe becomes a report only if a second probe this long
/// later still matches, with no work-proving event in between.
pub const DEFAULT_QUOTA_CONFIRM: Duration = Duration::from_secs(15);

/// A pane is probed at most once per this interval while no candidate is
/// pending, which bounds the replay cost of hints that never match.
pub const DEFAULT_QUOTA_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// Test/e2e seam overriding [`DEFAULT_QUOTA_CONFIRM`], in milliseconds —
/// the `DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS` pattern.
pub const DOT_AGENT_DECK_QUOTA_CONFIRM_MS: &str = "DOT_AGENT_DECK_QUOTA_CONFIRM_MS";

/// Smallest accepted [`DOT_AGENT_DECK_QUOTA_CONFIRM_MS`].
pub const MIN_QUOTA_CONFIRM_MS: u64 = 100;

/// Largest accepted [`DOT_AGENT_DECK_QUOTA_CONFIRM_MS`] (one hour).
pub const MAX_QUOTA_CONFIRM_MS: u64 = 60 * 60 * 1000;

/// Which provider limit the pane reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    /// A usage limit, possibly windowed (`Try again at …`): it may reset on its
    /// own, which the deck learns only when the agent works again.
    UsageLimit,
    /// A spent credit pool (`purchase more credits`, `send a request to your
    /// admin`): it does not reset without someone acting.
    CreditsDepleted,
    /// Forward-compat catch-all for a kind this build does not know.
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
            BlockedKind::UsageLimit => "quota: usage limit",
            BlockedKind::CreditsDepleted => "quota: credits depleted (no reset)",
            BlockedKind::Unknown => "quota: blocked",
        }
    }
}

/// Why a session is `Blocked`. An additive optional field beside the unit
/// `SessionStatus::Blocked`, never a payload on it (a payload variant breaks
/// every older reader's decode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedReason {
    pub kind: BlockedKind,
    /// Epoch milliseconds at which the daemon confirmed the block.
    pub detected_at_ms: i64,
    /// The matched row(s), control/bidi-stripped and at most
    /// [`MAX_DETAIL_CHARS`]. Agent-controlled text: DISPLAY ONLY, never
    /// interpolated into a submitted notice or a delegate reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What [`classify`] found on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaMatch {
    pub kind: BlockedKind,
    /// Scrubbed display text; see [`BlockedReason::detail`].
    pub detail: String,
}

/// Codex's TUI sentence. U+2019 only: an ASCII `You've` is what an LLM types in
/// prose, and rejecting it keeps an agent's own writing out.
static CODEX_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![Regex::new(r"^You\x{2019}ve\s+hit\s+your\s+usage\s+limit(?:\s*\.|\s+for\s)").unwrap()]
});

/// The provider errors OpenCode passes through as `Error: <message>`.
static OPENCODE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"^(?:Error:\s+)?The\s+usage\s+limit\s+has\s+been\s+reached\b").unwrap(),
        Regex::new(r"^(?:Error:\s+)?You\s+exceeded\s+your\s+current\s+quota\b").unwrap(),
    ]
});

fn patterns_for(agent_type: &AgentType) -> &'static [Regex] {
    match agent_type {
        AgentType::Codex => &CODEX_PATTERNS,
        AgentType::OpenCode => &OPENCODE_PATTERNS,
        _ => &[],
    }
}

/// TUI chrome that may precede the provider sentence on its row. Deliberately
/// short: `"`, `` ` ``, `'`, `-`, `>`, `#`, `/`, digits and `path:line:`
/// prefixes are NOT here, which is what keeps quoted text, code, Markdown and
/// `grep` output from anchoring.
fn is_chrome(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '■' | '▌' | '│' | '┃' | '⎿' | '•' | '●' | '✗' | '⚠' | '\u{fe0f}'
        )
}

/// A row that begins a new item rather than continuing the previous one: a
/// composer prompt or a new message/bullet cell.
fn starts_new_item(row: &str) -> bool {
    matches!(
        row.trim_start().chars().next(),
        Some('›' | '❯' | '>' | '■' | '•' | '●' | '✗' | '⚠')
    )
}

fn strip_chrome(row: &str) -> &str {
    row.trim_start_matches(is_chrome).trim_end()
}

/// Decide whether `rows` — the trailing rows of a pane's reconstructed screen,
/// oldest first — show a provider quota error for an agent of `agent_type`.
///
/// Blank rows are ignored, only the bottom [`QUOTA_TAIL_ROWS`] non-blank rows
/// may start a match, and the bottom-most match wins. An agent type with no
/// patterns (anything but Codex and OpenCode) never matches.
pub fn classify(agent_type: &AgentType, rows: &[String]) -> Option<QuotaMatch> {
    let patterns = patterns_for(agent_type);
    if patterns.is_empty() {
        return None;
    }
    let non_blank: Vec<&str> = rows
        .iter()
        .map(String::as_str)
        .filter(|r| !r.trim().is_empty())
        .collect();
    let tail = &non_blank[non_blank.len().saturating_sub(QUOTA_TAIL_ROWS)..];

    for start in (0..tail.len()).rev() {
        let head = strip_chrome(tail[start]);
        if head.is_empty() {
            continue;
        }
        // Two joins, because the screen does not say how a row wrapped: a TUI's
        // word wrap drops the space at the break (join with one), while a hard
        // wrap at the terminal edge splits a word (join with none).
        let mut joined = head.to_string();
        let mut glued = head.to_string();
        for row in tail.iter().skip(start + 1).take(CONTINUATION_ROWS) {
            if starts_new_item(row) {
                break;
            }
            let cont = strip_chrome(row);
            if !cont.is_empty() {
                joined.push(' ');
                joined.push_str(cont);
                glued.push_str(cont);
            }
        }
        if patterns
            .iter()
            .any(|re| re.is_match(&joined) || re.is_match(&glued))
        {
            return Some(QuotaMatch {
                kind: kind_of(&joined),
                detail: scrub_detail(&joined),
            });
        }
    }
    None
}

/// `CreditsDepleted` when the message names a spent credit pool, otherwise
/// `UsageLimit`. Compared with all whitespace removed so a word split by a hard
/// wrap still counts.
fn kind_of(text: &str) -> BlockedKind {
    let squashed: String = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if squashed.contains("purchasemorecredits") || squashed.contains("requesttoyouradmin") {
        BlockedKind::CreditsDepleted
    } else {
        BlockedKind::UsageLimit
    }
}

fn scrub_detail(text: &str) -> String {
    let stripped = strip_control_and_bidi(text, false);
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_DETAIL_CHARS {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(MAX_DETAIL_CHARS - 1).collect();
    out.push('…');
    out
}

/// Durations driving [`QuotaDetector`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaTimings {
    pub quiet: Duration,
    pub confirm: Duration,
    pub probe_interval: Duration,
}

impl Default for QuotaTimings {
    fn default() -> Self {
        Self {
            quiet: DEFAULT_QUOTA_QUIET,
            confirm: DEFAULT_QUOTA_CONFIRM,
            probe_interval: DEFAULT_QUOTA_PROBE_INTERVAL,
        }
    }
}

impl QuotaTimings {
    /// Timings with the confirmation window taken from
    /// [`DOT_AGENT_DECK_QUOTA_CONFIRM_MS`] when set and in range.
    pub fn from_env() -> Self {
        Self::from_confirm_ms_value(
            std::env::var(DOT_AGENT_DECK_QUOTA_CONFIRM_MS)
                .ok()
                .as_deref(),
        )
    }

    /// [`QuotaTimings::from_env`] with the variable's value injected. A value
    /// that does not parse or falls outside
    /// [`MIN_QUOTA_CONFIRM_MS`]..=[`MAX_QUOTA_CONFIRM_MS`] is rejected with a
    /// warning and the default is kept; nothing is clamped silently.
    pub fn from_confirm_ms_value(value: Option<&str>) -> Self {
        let mut timings = Self::default();
        let Some(raw) = value else {
            return timings;
        };
        match raw.trim().parse::<u64>() {
            Ok(ms) if (MIN_QUOTA_CONFIRM_MS..=MAX_QUOTA_CONFIRM_MS).contains(&ms) => {
                timings.confirm = Duration::from_millis(ms);
            }
            _ => tracing::warn!(
                value = raw,
                min = MIN_QUOTA_CONFIRM_MS,
                max = MAX_QUOTA_CONFIRM_MS,
                "ignoring out-of-range {DOT_AGENT_DECK_QUOTA_CONFIRM_MS}; using the default"
            ),
        }
        timings
    }
}

/// Result of feeding a probe into [`QuotaDetector::record_probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The screen did not match: the hint and any candidate are discarded.
    NoMatch,
    /// First match: a candidate is recorded and needs a confirming probe.
    Candidate,
    /// Matched again, but the confirmation window has not elapsed yet.
    Pending,
    /// Matched again after the full window with no work in between: report it.
    Confirmed(BlockedKind),
}

/// Per-pane quiet/confirm state machine. Every method takes the current
/// instant, so the whole thing runs on an injected clock.
///
/// Lifecycle: the byte scanner calls [`note_hint`](Self::note_hint) when it sees
/// the needle and [`note_output`](Self::note_output) for every other chunk; the
/// daemon calls [`note_work_event`](Self::note_work_event) for every
/// work-proving event; a periodic task asks [`should_probe`](Self::should_probe)
/// and, when it says yes, replays the screen, runs [`classify`] and hands the
/// result to [`record_probe`](Self::record_probe). After a `Confirmed` the
/// detector stays silent until a work event re-arms it.
#[derive(Debug, Clone, Default)]
pub struct QuotaDetector {
    timings: QuotaTimings,
    hint_at: Option<Instant>,
    last_output_at: Option<Instant>,
    last_work_event_at: Option<Instant>,
    last_probe_at: Option<Instant>,
    candidate_at: Option<Instant>,
    confirmed: bool,
}

impl QuotaDetector {
    pub fn new(timings: QuotaTimings) -> Self {
        Self {
            timings,
            ..Self::default()
        }
    }

    /// The PTY emitted a chunk containing the quota needle. Also counts as
    /// output, so the quiet window restarts.
    pub fn note_hint(&mut self, now: Instant) {
        self.hint_at = Some(now);
        self.note_output(now);
    }

    /// The PTY emitted a chunk.
    pub fn note_output(&mut self, now: Instant) {
        self.last_output_at = Some(now);
    }

    /// A work-proving event arrived (tool start/end, thinking, compacting,
    /// subagent, permission request, waiting-for-input, session start). Any
    /// hint at or before it is stale, a pending candidate is cancelled, and a
    /// confirmed block is lifted.
    pub fn note_work_event(&mut self, now: Instant) {
        self.last_work_event_at = Some(now);
        self.candidate_at = None;
        self.confirmed = false;
    }

    /// Whether a confirmed block is currently latched.
    pub fn is_confirmed(&self) -> bool {
        self.confirmed
    }

    fn hint_pending(&self) -> bool {
        match (self.hint_at, self.last_work_event_at) {
            (Some(hint), Some(work)) => hint > work,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Whether the screen should be probed now: a live hint newer than the last
    /// work event, the PTY quiet for the quiet window, and the probe rate limit
    /// (or, with a candidate pending, the confirmation window) elapsed.
    pub fn should_probe(&self, now: Instant) -> bool {
        if self.confirmed || !self.hint_pending() {
            return false;
        }
        if let Some(out) = self.last_output_at
            && now.saturating_duration_since(out) < self.timings.quiet
        {
            return false;
        }
        match (self.candidate_at, self.last_probe_at) {
            (Some(candidate), _) => {
                now.saturating_duration_since(candidate) >= self.timings.confirm
            }
            (None, Some(probe)) => {
                now.saturating_duration_since(probe) >= self.timings.probe_interval
            }
            (None, None) => true,
        }
    }

    /// Record the result of a probe taken at `now`.
    pub fn record_probe(&mut self, now: Instant, found: Option<BlockedKind>) -> ProbeOutcome {
        self.last_probe_at = Some(now);
        let Some(kind) = found else {
            self.hint_at = None;
            self.candidate_at = None;
            return ProbeOutcome::NoMatch;
        };
        if let (Some(candidate), Some(work)) = (self.candidate_at, self.last_work_event_at)
            && candidate <= work
        {
            self.candidate_at = None;
        }
        match self.candidate_at {
            None => {
                self.candidate_at = Some(now);
                ProbeOutcome::Candidate
            }
            Some(candidate) if now.saturating_duration_since(candidate) >= self.timings.confirm => {
                self.candidate_at = None;
                self.confirmed = true;
                ProbeOutcome::Confirmed(kind)
            }
            Some(_) => ProbeOutcome::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_screen_text::visible_tail_lines;
    use spec::spec;

    const CODEX_USAGE: &[&str] = &[
        "You\u{2019}ve hit your usage limit.",
        "You\u{2019}ve hit your usage limit for gpt-5-codex. Switch to another model now, or try again at 3:00 PM.",
        "You\u{2019}ve hit your usage limit. Upgrade to Plus to continue using Codex (https://chatgpt.com/explore/plus), or try again at Sep 27th, 2026 3:00 PM.",
        "You\u{2019}ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), or try again at 3:00 PM.",
        "You\u{2019}ve hit your usage limit. Try again at 3:00 PM.",
    ];
    const CODEX_CREDITS: &[&str] = &[
        "You\u{2019}ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits",
        "You\u{2019}ve hit your usage limit. To get more access now, send a request to your admin",
    ];
    const OPENCODE_USAGE: &[&str] = &[
        "Error: The usage limit has been reached",
        "The usage limit has been reached",
        "Error: You exceeded your current quota, please check your plan and billing details.",
    ];

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    /// Render `line` behind `prefix` as a TUI would, with a composer and footer
    /// below it, and read it back through the daemon's own screen replay at
    /// `cols` columns.
    fn screen(prefix: &str, line: &str, cols: u16) -> Vec<String> {
        let bytes = format!(
            "\u{2022} Ran cargo build\r\n\r\n{prefix}{line}\r\n\r\n\u{203a} Ask Codex to do anything\r\n  ? for shortcuts\r\n"
        );
        visible_tail_lines(bytes.as_bytes(), 24, cols, QUOTA_TAIL_ROWS)
    }

    fn assert_kind(agent: &AgentType, lines: &[&str], want: BlockedKind) {
        for line in lines {
            for prefix in ["", "\u{25a0} ", "\u{2502} "] {
                let direct = classify(agent, &rows(&[&format!("{prefix}{line}")]));
                assert_eq!(
                    direct.as_ref().map(|m| m.kind),
                    Some(want),
                    "{agent:?} {prefix:?}{line:?}"
                );
                for cols in [40u16, 80] {
                    let got = classify(agent, &screen(prefix, line, cols));
                    assert_eq!(
                        got.as_ref().map(|m| m.kind),
                        Some(want),
                        "{agent:?} {prefix:?}{line:?} at {cols} cols: {:?}",
                        screen(prefix, line, cols)
                    );
                    let detail = got.unwrap().detail;
                    assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
                    assert!(!detail.contains("Ask Codex"), "composer leaked: {detail:?}");
                }
            }
        }
    }

    /// Scenario: Feed every real Codex and OpenCode quota sentence, bare and
    /// behind the `■ ` / `│ ` chrome, and wrapped at 40 and 80 columns through
    /// the daemon's screen replay, into the classifier. Each must be recognised
    /// with the right kind: credit-pool wording is CreditsDepleted, the rest
    /// (including `Try again at`) UsageLimit.
    #[spec("status/blocked/001")]
    #[test]
    fn status_blocked_001_classifier_accepts_real_provider_lines() {
        assert_kind(&AgentType::Codex, CODEX_USAGE, BlockedKind::UsageLimit);
        assert_kind(
            &AgentType::Codex,
            CODEX_CREDITS,
            BlockedKind::CreditsDepleted,
        );
        assert_kind(
            &AgentType::OpenCode,
            OPENCODE_USAGE,
            BlockedKind::UsageLimit,
        );

        // The detail is the matched message, scrubbed and bounded.
        let m = classify(
            &AgentType::Codex,
            &rows(&[
                "\u{25a0} You\u{2019}ve hit your usage limit.\u{1b}[31m Try again at 3:00 PM.",
            ]),
        )
        .unwrap();
        assert_eq!(
            m.detail,
            "You\u{2019}ve hit your usage limit.[31m Try again at 3:00 PM."
        );
        let long = format!("You\u{2019}ve hit your usage limit. {}", "x".repeat(400));
        let m = classify(&AgentType::Codex, &rows(&[&long])).unwrap();
        assert_eq!(m.detail.chars().count(), MAX_DETAIL_CHARS);
        assert!(m.detail.ends_with('…'));

        // The wire round-trip the daemon's metadata keys rely on.
        for kind in [
            BlockedKind::UsageLimit,
            BlockedKind::CreditsDepleted,
            BlockedKind::Unknown,
        ] {
            assert_eq!(BlockedKind::from_wire(kind.as_wire()), kind);
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.as_wire().to_string())
            );
        }
        assert_eq!(
            serde_json::from_str::<BlockedKind>("\"future_kind\"").unwrap(),
            BlockedKind::Unknown
        );
    }

    /// Scenario: Feed text that merely mentions a quota — ASCII apostrophes,
    /// quoted or Markdown-formatted sentences, grep output, code comments,
    /// mid-sentence prose, another provider's wording, the wrong agent type, or
    /// a match scrolled above the bottom ten rows. None of it may classify as
    /// blocked.
    #[spec("status/blocked/002")]
    #[test]
    fn status_blocked_002_classifier_rejects_near_misses() {
        let codex_near_misses = [
            "You've hit your usage limit.",
            "\"You\u{2019}ve hit your usage limit.\"",
            "- `You\u{2019}ve hit your usage limit.`",
            "`You\u{2019}ve hit your usage limit.`",
            "> You\u{2019}ve hit your usage limit.",
            "# You\u{2019}ve hit your usage limit.",
            "src/x.rs:12:You\u{2019}ve hit your usage limit.",
            "12: You\u{2019}ve hit your usage limit.",
            "// You\u{2019}ve hit your usage limit.",
            "You\u{2019}ve hit your usage limits today",
            "rate limit exceeded",
            "Error: The usage limit has been reached",
        ];
        for line in codex_near_misses {
            for prefix in ["", "\u{25a0} "] {
                let input = rows(&[&format!("{prefix}{line}")]);
                assert_eq!(
                    classify(&AgentType::Codex, &input),
                    None,
                    "{prefix:?}{line:?}"
                );
            }
        }

        let opencode_near_misses = [
            "// The usage limit has been reached",
            "\"The usage limit has been reached\"",
            "src/x.rs:3:The usage limit has been reached",
            "rate limit exceeded",
            "Error: rate limit exceeded",
            "I checked and the usage limit has been reached, so I will wait.",
            "the usage limit has been reached, so the job paused",
            "You\u{2019}ve hit your usage limit.",
        ];
        for line in opencode_near_misses {
            assert_eq!(
                classify(&AgentType::OpenCode, &rows(&[line])),
                None,
                "{line:?}"
            );
        }

        // No patterns at all for any other type, even for exact provider text.
        for agent in [
            AgentType::None,
            AgentType::ClaudeCode,
            AgentType::Pi,
            AgentType::Devin,
        ] {
            for line in CODEX_USAGE
                .iter()
                .chain(CODEX_CREDITS)
                .chain(OPENCODE_USAGE)
            {
                assert_eq!(classify(&agent, &rows(&[line])), None, "{agent:?} {line:?}");
            }
        }

        // Recency: the match must be within the bottom QUOTA_TAIL_ROWS non-blank
        // rows. Blank rows do not count toward the ten.
        let filler: Vec<String> = (0..QUOTA_TAIL_ROWS)
            .map(|i| format!("\u{2022} step {i}"))
            .collect();
        let mut scrolled = vec![CODEX_USAGE[0].to_string(), String::new()];
        scrolled.extend(filler.iter().cloned());
        assert_eq!(classify(&AgentType::Codex, &scrolled), None);
        let mut in_window = vec![CODEX_USAGE[0].to_string(), String::new()];
        in_window.extend(filler.iter().skip(1).cloned());
        assert!(classify(&AgentType::Codex, &in_window).is_some());
    }

    /// Scenario: Drive the per-pane detector on an injected clock. A hint
    /// followed by quiet and two matching probes a confirmation window apart
    /// reports the block; a work event between probes cancels it; a pane that
    /// keeps emitting output is never probed; a hint older than the last work
    /// event is ignored; and probes respect the rate limit.
    #[spec("status/blocked/003")]
    #[test]
    fn status_blocked_003_state_machine_needs_quiet_and_confirmation() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let timings = QuotaTimings::default();
        let quiet = timings.quiet.as_millis() as u64;
        let confirm = timings.confirm.as_millis() as u64;
        let interval = timings.probe_interval.as_millis() as u64;

        // Happy path: hint → quiet → candidate → confirm window → confirmed.
        let mut d = QuotaDetector::new(timings);
        assert!(!d.should_probe(at(0)), "no hint, no probe");
        d.note_hint(at(0));
        assert!(!d.should_probe(at(quiet - 1)), "not quiet yet");
        assert!(d.should_probe(at(quiet)));
        assert_eq!(
            d.record_probe(at(quiet), Some(BlockedKind::UsageLimit)),
            ProbeOutcome::Candidate
        );
        assert!(
            !d.should_probe(at(quiet + confirm - 1)),
            "confirm window not elapsed"
        );
        assert!(d.should_probe(at(quiet + confirm)));
        assert_eq!(
            d.record_probe(at(quiet + confirm), Some(BlockedKind::CreditsDepleted)),
            ProbeOutcome::Confirmed(BlockedKind::CreditsDepleted)
        );
        assert!(d.is_confirmed());
        assert!(
            !d.should_probe(at(quiet + 10 * confirm)),
            "latched: no re-report"
        );
        // A work event lifts the latch; a fresh hint re-arms from scratch.
        d.note_work_event(at(quiet + 10 * confirm));
        assert!(!d.is_confirmed());
        assert!(
            !d.should_probe(at(quiet + 20 * confirm)),
            "old hint is stale"
        );
        d.note_hint(at(quiet + 20 * confirm + 1));
        assert!(d.should_probe(at(2 * quiet + 20 * confirm + 1)));

        // A premature second probe (only reachable by a caller ignoring
        // should_probe) is Pending, never Confirmed.
        let mut d = QuotaDetector::new(timings);
        d.note_hint(at(0));
        d.record_probe(at(quiet), Some(BlockedKind::UsageLimit));
        assert_eq!(
            d.record_probe(at(quiet + 1), Some(BlockedKind::UsageLimit)),
            ProbeOutcome::Pending
        );

        // A work event between the probes cancels the candidate and stales the hint.
        let mut d = QuotaDetector::new(timings);
        d.note_hint(at(0));
        assert_eq!(
            d.record_probe(at(quiet), Some(BlockedKind::UsageLimit)),
            ProbeOutcome::Candidate
        );
        d.note_work_event(at(quiet + 1));
        assert!(
            !d.should_probe(at(quiet + confirm)),
            "hint predates the work event"
        );
        // Even a caller that probes anyway starts a fresh candidate, not a confirm.
        assert_eq!(
            d.record_probe(at(quiet + confirm), Some(BlockedKind::UsageLimit)),
            ProbeOutcome::Candidate
        );
        assert!(!d.is_confirmed());

        // Output that never goes quiet (spinner bytes every second) is never probed.
        let mut d = QuotaDetector::new(timings);
        d.note_hint(at(0));
        for s in 1..=60u64 {
            d.note_output(at(s * 1000));
            assert!(
                !d.should_probe(at(s * 1000 + 999)),
                "probed a live pane at {s}s"
            );
        }
        assert!(
            d.should_probe(at(60_000 + quiet)),
            "quiet again after the spinner stops"
        );

        // A hint older than the last work event is discarded.
        let mut d = QuotaDetector::new(timings);
        d.note_hint(at(0));
        d.note_work_event(at(1));
        assert!(!d.should_probe(at(10 * quiet)));
        let mut d = QuotaDetector::new(timings);
        d.note_work_event(at(0));
        d.note_hint(at(0));
        assert!(
            !d.should_probe(at(10 * quiet)),
            "a hint AT the work event is not newer"
        );

        // Rate limit: a non-matching probe drops the hint; a new hint cannot be
        // probed again before the interval, even once quiet.
        let mut d = QuotaDetector::new(timings);
        d.note_hint(at(0));
        assert_eq!(d.record_probe(at(quiet), None), ProbeOutcome::NoMatch);
        assert!(
            !d.should_probe(at(quiet + interval)),
            "hint dropped on no-match"
        );
        d.note_hint(at(quiet + 1));
        assert!(
            !d.should_probe(at(2 * quiet + 1)),
            "quiet but inside the probe interval"
        );
        assert!(!d.should_probe(at(quiet + interval - 1)));
        assert!(d.should_probe(at(quiet + interval)));
        // A no-match on the confirming probe cancels the candidate.
        d.record_probe(at(quiet + interval), Some(BlockedKind::UsageLimit));
        assert_eq!(
            d.record_probe(at(quiet + interval + confirm), None),
            ProbeOutcome::NoMatch
        );
        assert!(!d.should_probe(at(quiet + interval + 10 * confirm)));

        // Env seam: in-range values override confirm only; garbage keeps the default.
        let fast = QuotaTimings::from_confirm_ms_value(Some("250"));
        assert_eq!(fast.confirm, Duration::from_millis(250));
        assert_eq!(fast.quiet, DEFAULT_QUOTA_QUIET);
        for bad in ["0", "99", "abc", "3600001", ""] {
            assert_eq!(
                QuotaTimings::from_confirm_ms_value(Some(bad)),
                QuotaTimings::default()
            );
        }
        assert_eq!(
            QuotaTimings::from_confirm_ms_value(None),
            QuotaTimings::default()
        );
    }
}
