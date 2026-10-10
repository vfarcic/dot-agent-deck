//! Issue #1383: acknowledged delegate delivery, retried in place.
//!
//! A delegate's task pointer is typed into the worker's PTY once the readiness
//! gate releases. An agent still booting can consume those bytes without acting
//! on them — the terminal-mode switch of a TUI starting up, or an input loop
//! that is not listening yet — and the orchestrator is then left waiting on a
//! healthy worker that never got its task. #1383 measured that on OpenCode,
//! whose readiness gate is a fixed interval because it announces nothing before
//! its first prompt, so no delay can prove the agent was ready.
//!
//! This module makes the delivery recoverable **in the same process**. Every
//! delegation carries a short delivery id (`d-` plus 8 hex characters), both in
//! the pointer (` [delivery d-…]`) and in an "acknowledge first" header at the
//! top of the task file. After the first write the daemon watches for proof
//! that the worker received the pointer — a turn beginning
//! ([`crate::state::worker_event_proves_delivery`]), a `dot-agent-deck ack` for
//! this id, a `work-done` from the pane, or a quota block — and re-sends the
//! pointer into the same pane on a bounded schedule while none arrives. It never
//! respawns anything.
//!
//! Every re-delivery starts with a submit-only probe — a bare Enter — and reads
//! the worker's screen. When the pointer ends at the cursor, it is most likely
//! parked unsubmitted in the agent's composer (issue #1243's shape), and the
//! re-delivery never types it again: after the grace below it presses Enter
//! once more if it is still there, because an agent that took the first CR into
//! a paste swallows the next Enter. When the id shows only in the transcript,
//! the pointer was submitted: that re-delivery is the probe's Enter alone, and
//! once any reading has seen that the pointer is never typed again for this
//! delivery. When it is not visible, the screen
//! cannot tell an empty composer from one holding the pointer where the screen
//! does not show it, so the loop waits a short grace ([`probe_grace`]) for the
//! turn that Enter would start, and types the pointer again, with the same id,
//! only if the worker is still silent. A screen the deck cannot read — blanked
//! by a resize since the pointer went in, empty, unparseable or too large —
//! gets the Enter alone. The task file tells the worker that a
//! repeated pointer is the same task.
//!
//! That ordering makes a doubled pointer unlikely, not impossible: a composer
//! that neither shows its text nor submits it on Enter, or an agent that starts
//! a turn without reporting one within the grace, still gets a second copy.
//!
//! What this is not: a receipt for the work. An ack claims only that the worker
//! read its task file.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, oneshot};
use tracing::{info, warn};

use crate::agent_pty::{AgentPtyRegistry, GuardedSend, GuardedSendDetail};
use crate::config_validation::escape_id_for_log;
use crate::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
use crate::state::OrchestrationIdentity;

/// The environment variable that sets the re-delivery schedule: comma-separated
/// milliseconds. See [`RetrySchedule::parse`] for the syntax and bounds.
pub const DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS: &str =
    "DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS";

/// The schedule used when nothing is configured. The first re-delivery lands
/// 20 s after the first write, which on the declared-no-signal path is well past
/// the ~12 s loaded OpenCode boot #1383 measured.
pub const DEFAULT_RETRY_SCHEDULE_MS: [u64; 3] = [20_000, 40_000, 80_000];

/// Smallest accepted schedule entry. A shorter wait is not a deliberate
/// interval, and would type into an agent faster than any agent can answer.
pub const MIN_RETRY_WAIT: Duration = Duration::from_millis(100);

/// Largest accepted schedule entry.
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(300);

/// At most this many schedule entries are read, so a pasted list cannot turn
/// one delegation into an unbounded stream of re-deliveries.
pub const MAX_RETRY_ENTRIES: usize = 8;

/// When a re-delivery happens, as the waits that precede each one.
///
/// Entry *i* is the wait from the first write (for *i* = 0) or from the start of
/// re-delivery *i* to the start of re-delivery *i + 1*; a probe's grace and its
/// retype come out of that wait. After the last re-delivery the loop waits the
/// last entry once more before declaring the delivery exhausted, so N entries
/// give N re-deliveries and a nominal span of `sum + last`. A `SessionStart`
/// postponement or a busy dispatch lock can stretch any wait; the silent-worker
/// report waits for the loop's real end rather than for this span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrySchedule {
    waits: Vec<Duration>,
}

impl RetrySchedule {
    /// The schedule configured for this process right now. Read at arm time and
    /// never cached, like the other delegate seams.
    pub fn from_env() -> Self {
        Self::parse(
            std::env::var(DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS)
                .ok()
                .as_deref(),
        )
    }

    /// Parse a schedule value.
    ///
    /// * unset: [`DEFAULT_RETRY_SCHEDULE_MS`];
    /// * `0`, an empty value or whitespace: disabled — the pointer is written
    ///   once, exactly as before #1383;
    /// * a comma-separated list of integers: one entry per re-delivery, each
    ///   clamped to `[`[`MIN_RETRY_WAIT`]`, `[`MAX_RETRY_WAIT`]`]`, at most
    ///   [`MAX_RETRY_ENTRIES`] read;
    /// * anything else among the entries that are read: the default, with one
    ///   `warn!`. Entries past [`MAX_RETRY_ENTRIES`] are not read, so a
    ///   malformed one there does not discard the valid ones before it.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::default_schedule();
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "0" {
            return Self::disabled();
        }
        let mut waits = Vec::new();
        for entry in trimmed.split(',') {
            // Before the entry is parsed: an entry past the limit is not read,
            // so it cannot be the one that throws the whole value out.
            if waits.len() == MAX_RETRY_ENTRIES {
                warn!(
                    max_entries = MAX_RETRY_ENTRIES,
                    "{DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS} has more entries than are read; \
                     the rest are ignored"
                );
                break;
            }
            let Ok(ms) = entry.trim().parse::<u128>() else {
                warn!(
                    value = %escape_id_for_log(raw),
                    "{DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS} is not a comma-separated list \
                     of milliseconds; using the default schedule"
                );
                return Self::default_schedule();
            };
            let clamped = ms.clamp(MIN_RETRY_WAIT.as_millis(), MAX_RETRY_WAIT.as_millis());
            if clamped != ms {
                warn!(
                    requested_ms = ms,
                    clamped_ms = clamped,
                    "{DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS} entry is out of range; clamped"
                );
            }
            // `clamped` is at most 300_000, so this never saturates.
            waits.push(Duration::from_millis(
                u64::try_from(clamped).unwrap_or(u64::MAX),
            ));
        }
        Self { waits }
    }

    fn default_schedule() -> Self {
        Self {
            waits: DEFAULT_RETRY_SCHEDULE_MS
                .iter()
                .map(|ms| Duration::from_millis(*ms))
                .collect(),
        }
    }

    /// A schedule that re-delivers nothing.
    pub fn disabled() -> Self {
        Self { waits: Vec::new() }
    }

    /// Whether any re-delivery is scheduled.
    pub fn is_enabled(&self) -> bool {
        !self.waits.is_empty()
    }

    /// The silence before each re-delivery.
    pub fn waits(&self) -> &[Duration] {
        &self.waits
    }

    /// How many re-deliveries the schedule allows.
    pub fn redeliveries(&self) -> usize {
        self.waits.len()
    }

    /// From the first write to the moment the loop declares exhaustion, not
    /// counting any postponement or time spent queued for the pane: every wait,
    /// plus the last one once more.
    pub fn total_span(&self) -> Duration {
        let sum: Duration = self.waits.iter().sum();
        sum + self.waits.last().copied().unwrap_or_default()
    }
}

/// Mint a delivery id: `d-` plus 8 lowercase hex characters from the OS RNG.
///
/// Short on purpose — a model has to copy it into a command — and unique only
/// where it has to be: an ack is matched on `(pane, id)`, so a collision needs
/// two live deliveries on one pane, and a pane holds one at a time.
///
/// Deliberately NOT [`crate::prompt_delivery::mint_delivery_id`], which embeds
/// the pane id and runs to about forty characters.
pub fn mint_delivery_id() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("OS randomness is unavailable; cannot mint a delivery id");
    let mut id = String::with_capacity(10);
    id.push_str("d-");
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Whether `id` is shaped like a delivery id this build mints:
/// `^d-[0-9a-f]{8}$`.
///
/// Checked by the `ack` CLI and again by the daemon before any lookup or log
/// line, so a producer-supplied value can never carry a control character, a
/// newline or an arbitrary length into either.
pub fn is_valid_delivery_id(id: &str) -> bool {
    let Some(hex) = id.strip_prefix("d-") else {
        return false;
    };
    hex.len() == 8
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The suffix every delegate pointer carries, so the worker, the screen and the
/// deck log all name the delivery the same way.
pub fn pointer_suffix(delivery_id: &str) -> String {
    format!(" [delivery {delivery_id}]")
}

/// The header prepended to a worker task file, so acknowledging is the first
/// thing a worker reads.
///
/// Daemon-written on every delegation, whatever the role's `prompt_template`
/// says, so no project has to opt in. It also tells the worker that a failing
/// `ack` is harmless: a `dot-agent-deck` binary in the pane older than the
/// daemon rejects the unknown subcommand, and a role whose tool allowlist does
/// not yet name `ack` refuses it or asks for approval (PR #1414 review). The
/// task must not stall on either.
pub fn task_file_ack_header(delivery_id: &str) -> String {
    let bin = crate::platform::paths::binary_name();
    format!(
        "## First: acknowledge this task\n\n\
         Run this command via Bash first:\n\n\
         ```bash\n\
         {bin} ack {delivery_id}\n\
         ```\n\n\
         It tells the deck this task reached you, so it stops re-sending the pointer. If the \
         command fails, is not recognised, or is refused or needs an approval you do not get, \
         skip it and carry on with the task. If you see \
         this task pointer more than once, it is the same task: do not start it again."
    )
}

/// What one event from the delegated worker means for the retry loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventVerdict {
    /// A turn began, so the pointer landed. Stop.
    Received,
    /// The provider refused the agent. Re-typing into a blocked agent only
    /// queues more prompts, and issue #714's blocked-worker notice owns this
    /// case. Stop.
    Blocked,
    /// A genuine `SessionStart` after the write: the agent was booting when the
    /// pointer went in. Hold the next attempt for a readiness interval, so the
    /// retry lands after the boot rather than inside it — and after issue
    /// #1031's own submit probe, when that one is armed too.
    Postpone,
    /// Says nothing about whether the pointer arrived.
    Ignore,
}

/// Classify one event from the delegated worker (already matched on both pane
/// and agent id by the caller).
///
/// "Any event means received" would disarm the loop in exactly the case it
/// exists for: Claude Code posts a `SessionStart` early in boot, and OpenCode
/// emits a startup `session.idle` that maps to `Idle`. Neither says anything
/// about the pointer. So proof is the daemon's existing "a turn began"
/// predicate, and everything ambiguous is ignored.
///
/// An exhaustive `match`, so a new [`EventType`] has to be classified on
/// purpose.
///
/// **Trust bound (audit M5).** These events arrive as raw `AgentEvent`s, which
/// carry no provenance token for any agent today (`docs/develop/hook-provenance.md`),
/// so a process running as the same user that knows a worker's pane and agent
/// ids can forge a `Thinking` and stop that worker's retry. Requiring
/// attestation here would stop the retry for every real hook too. What a forged
/// event can cause is bounded at the behaviour before #1383: the pointer is not
/// re-sent, and the silent-worker report still covers a worker that then says
/// nothing.
pub fn classify_event(event: &AgentEvent) -> EventVerdict {
    match event.event_type {
        // Issue #1493: a Codex pane's painted output, the pointer's own echo
        // included — see `crate::state::worker_event_proves_delivery`.
        _ if event.agent_type == crate::event::AgentType::Codex
            && event.is_wrapper_output_classified() =>
        {
            EventVerdict::Ignore
        }
        EventType::Thinking
        | EventType::ToolStart
        | EventType::ToolEnd
        | EventType::SubagentStart
        | EventType::SubagentStop
        | EventType::Compacting
        | EventType::PermissionRequest => {
            debug_assert!(crate::state::worker_event_proves_delivery(event));
            EventVerdict::Received
        }
        EventType::QuotaBlocked => EventVerdict::Blocked,
        // The wrapper's own fork-time or interface-watch start names the
        // WRAPPER's session; it is the deck watching a child paint, not an agent
        // announcing a conversation.
        EventType::SessionStart if event.is_wrapper_session_start() => EventVerdict::Ignore,
        EventType::SessionStart => EventVerdict::Postpone,
        // The exit signal owns a worker that went away, not this stream.
        EventType::SessionEnd
        | EventType::Idle
        | EventType::Error
        | EventType::WaitingForInput
        | EventType::ShellBusy
        | EventType::ShellIdle
        | EventType::PullRequest
        | EventType::Unknown => EventVerdict::Ignore,
    }
}

/// What the worker's screen shows about this delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Composer {
    /// The pointer ends right at the cursor: the pointer is most likely
    /// sitting in the composer, unsubmitted. The re-delivery is the submit-only
    /// probe, and after an unanswered grace one more Enter (issue #1243).
    PointerInComposer,
    /// The delivery id is on screen, but the pointer does not end at the
    /// cursor: most likely a submitted pointer still in the transcript above an input box that holds
    /// something else, or nothing. The re-delivery is the submit-only probe and
    /// nothing more — no second Enter into that other input — and, once any
    /// reading has seen this, the delivery is never retyped (PR #1414 review):
    /// a later screen that no longer shows the pointer has most likely only
    /// scrolled it away. Later re-deliveries still press their probe Enter,
    /// because a worker that is not reading its terminal yet has the pointer
    /// and each Enter echoed into this shape by the line discipline alone
    /// (`orchestration/delegate/043`).
    PointerInHistory,
    /// The screen says nothing about the composer: it shows nothing at all and
    /// the PTY was resized since the pointer was typed (a resize drops the
    /// scrollback ring the screen is read from, and an agent that has not
    /// repainted still holds whatever it held), or the snapshot is empty, does
    /// not parse, or is over [`MAX_RETRY_SCREEN_CELLS`]. The re-delivery is the
    /// submit-only probe alone, never a second copy.
    Unreadable,
    /// No trace of it on screen — which is not proof the composer is empty. The
    /// probe, then a retype only after an unanswered grace.
    Absent,
}

/// Classify the worker's screen for `delivery_id`. `screen_rows` is every row
/// of the screen, blank ones included, and `cursor_row` indexes it;
/// `cursor_col` is how many `char`s of that row sit at or before the cursor
/// (see [`crate::pane_screen_text::visible_rows_and_cursor`]).
///
/// Searches each row, and then every row concatenated with everything but ASCII
/// letters, digits and `-` stripped out, so an id split by a line wrap — with a
/// composer's border glyphs and padding around the split — is still found.
///
/// **Where the id is decides between the two `Pointer*` answers.** The pointer
/// ends with its delivery id, and an agent's input box leaves the terminal
/// cursor right after what was typed into it — measured on Claude Code 2.1.284
/// and Codex 0.156.1, both with a visible cursor on the composer row, directly
/// after the id, and Claude Code with three non-blank rows (border and footers)
/// below it. So the pointer is in the composer when it ENDS AT THE CURSOR: the
/// id followed by the rest of the pointer (`after_id`, the pointer's text after
/// the id: its closing `]`) ends at or just before the cursor's column, with
/// only whitespace between them. The id may sit on the cursor's row whole or
/// start on the row above it across a wrap, and when the id fills a row the
/// `]` wraps alone — then the cursor's row holds nothing before the cursor but
/// the rest of the pointer, and the id must end the row above. Anywhere else it
/// is transcript, including a copy earlier on the cursor's row with other text
/// after it. A row-count window from the bottom would not do: Claude Code's
/// footers put the composer's id three rows up, and a submitted pointer
/// directly above an empty input box sits only two rows further.
///
/// **Biased away from [`Composer::Absent`] on purpose.** Any sight of the id,
/// history included, rules out a retype: a false positive costs a retype that
/// never happens, and a false negative costs a probe's grace and then, if the
/// worker stays silent through it, a second copy of the pointer. Within the
/// visible answers the bias runs the other way — an agent that parks its cursor
/// away from typed text reads as [`Composer::PointerInHistory`] and forgoes the
/// second Enter, which the next re-delivery's probe makes up for.
///
/// `resized_since_write` is whether the PTY geometry moved since the pointer
/// was last typed; see [`Composer::Unreadable`].
pub fn classify_composer(
    screen_rows: &[String],
    cursor_row: usize,
    cursor_col: usize,
    delivery_id: &str,
    after_id: &str,
    resized_since_write: bool,
) -> Composer {
    if resized_since_write && screen_rows.iter().all(|row| row.trim().is_empty()) {
        return Composer::Unreadable;
    }
    if delivery_id.is_empty() {
        return Composer::Absent;
    }
    let before_cursor: String = screen_rows
        .get(cursor_row)
        .map(|row| row.chars().take(cursor_col).collect())
        .unwrap_or_default();
    if pointer_ends_at(
        screen_rows,
        cursor_row,
        before_cursor.trim_end(),
        delivery_id,
        after_id,
    ) {
        return Composer::PointerInComposer;
    }
    let squeezed: String = screen_rows
        .iter()
        .map(|row| squeeze_id_chars(row))
        .collect();
    if screen_rows.iter().any(|row| row.contains(delivery_id)) || squeezed.contains(delivery_id) {
        Composer::PointerInHistory
    } else {
        Composer::Absent
    }
}

/// Only ASCII letters, digits and `-`: what survives a wrap and a border.
fn squeeze_id_chars(row: &str) -> String {
    row.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

/// `text` without whitespace and box-drawing or block glyphs (U+2500..=U+259F),
/// which is what a composer's border and padding add around its content.
fn glyphs_only(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && !('\u{2500}'..='\u{259F}').contains(c))
        .collect()
}

/// Whether the pointer — `delivery_id` then `after_id` — ends exactly where
/// `head`, row `row`'s text up to a point, ends: on that row whole, or with
/// `after_id` wrapped alone onto it and the id ending the row above.
fn pointer_ends_at(
    screen_rows: &[String],
    row: usize,
    head: &str,
    delivery_id: &str,
    after_id: &str,
) -> bool {
    if let Some(before_rest) = head.strip_suffix(after_id)
        && id_ends_at(screen_rows, row, before_rest, delivery_id)
    {
        return true;
    }
    // The rest of the pointer wrapped: nothing but (a tail of) it on this row,
    // behind any border and padding, and the id with whatever of the rest did
    // not wrap ending the row above.
    let rest = glyphs_only(after_id);
    let here = glyphs_only(head);
    let Some(unwrapped) = rest.strip_suffix(here.as_str()) else {
        return false;
    };
    let Some(above) = row.checked_sub(1).and_then(|above| screen_rows.get(above)) else {
        return false;
    };
    let above = trim_end_glyphs(above);
    !here.is_empty()
        && above
            .strip_suffix(unwrapped)
            .is_some_and(|before_rest| id_ends_at(screen_rows, row - 1, before_rest, delivery_id))
}

/// Whether `delivery_id` ends exactly where `head`, row `row`'s text up to a
/// point, ends: whole on the row, or wrapped onto it from the row above — then
/// `head` holds nothing but the id's tail behind any border and padding, and
/// the row above ends with the rest of it.
fn id_ends_at(screen_rows: &[String], row: usize, head: &str, delivery_id: &str) -> bool {
    if head.ends_with(delivery_id) {
        return true;
    }
    let tail = glyphs_only(head);
    let Some(start) = delivery_id.strip_suffix(tail.as_str()) else {
        return false;
    };
    !tail.is_empty()
        && !start.is_empty()
        && row
            .checked_sub(1)
            .and_then(|above| screen_rows.get(above))
            .is_some_and(|above| glyphs_only(above).ends_with(start))
}

/// `text` without trailing whitespace and box-drawing or block glyphs: a
/// composer row's content with its right border and padding cut away.
fn trim_end_glyphs(text: &str) -> &str {
    text.trim_end_matches(|c: char| c.is_whitespace() || ('\u{2500}'..='\u{259F}').contains(&c))
}

/// Whether a re-delivery may type the pointer again, decided once, when the
/// delivery is armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetypePolicy {
    /// A silent worker whose screen does not show the pointer gets a fresh copy.
    Allowed,
    /// Enter only: the probe, and the second Enter over a pointer still in the
    /// input box. For a worker whose delivery the deck cannot confirm (issue
    /// #1390's wrap-only Codex), a pointer that left the screen may well have
    /// been submitted, and a copy would start the task a second time.
    Never,
}

impl RetypePolicy {
    /// The policy for a worker of `agent_type`, or one the deck spawned as a
    /// wrapper host (`spawned_as_wrapper_host`, from
    /// [`AgentPtyRegistry::agent_spawned_as_wrapper_host`]).
    ///
    /// Keyed on the launch shape rather than on the wrapper's per-event
    /// `wrapper_prompt_reports_unavailable` marker: the policy is fixed before
    /// the first write, and a freshly respawned wrapper may not have emitted a
    /// single event by then, so the marker's absence proves nothing yet. That
    /// withdraws the retype from a wrapped Codex whose prompt hook works as
    /// well, which costs it a recovery the Enter does not give, never a
    /// duplicate. The wrapper's `Thinking` is no evidence either way: it is
    /// classified from painted output, not reported by a prompt hook.
    pub fn for_worker(agent_type: Option<&AgentType>, spawned_as_wrapper_host: bool) -> Self {
        let declared_wrapper = agent_type.is_some_and(|agent_type| {
            crate::agent_registry::spec(agent_type).strategy
                == Some(crate::agent_registry::IntegrationStrategy::Wrapper)
        });
        if spawned_as_wrapper_host || declared_wrapper {
            Self::Never
        } else {
            Self::Allowed
        }
    }
}

/// Handed back by [`PendingDeliveries::arm`] to the loop that owns the record.
#[derive(Debug)]
pub struct ArmedDelivery {
    /// This record's generation, for [`PendingDeliveries::finish`] and
    /// [`PendingDeliveries::is_current`].
    pub seq: u64,
    /// Fixed at arm time, so nothing the worker reports mid-loop can widen it.
    pub retype: RetypePolicy,
    /// Resolves when the record is removed by anything other than the loop
    /// itself: an ack, a `work-done`, or a newer delegation to the pane.
    pub cancel: oneshot::Receiver<()>,
}

/// What an `ack` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// It matched the pane's pending delivery, which is now stopped. Carries the
    /// silent-worker watch armed for the same delivery, which the ack also
    /// cancels: an ack is not an agent event, so that watch cannot see it.
    Stopped { silence_seq: Option<u64> },
    /// The pane's last acknowledged delivery, acknowledged again. A no-op.
    AlreadyAcknowledged,
    /// The pane's current delivery, with no retry pending to stop: its loop
    /// already ended (most often because a turn began, which a real agent
    /// reports before the `ack` it runs as a tool) or was never armed (the retry
    /// is off, the agent type is not retried, or a Pi seed delivery). A genuine
    /// acknowledgement of the task the pane was last given, so it carries the
    /// silent-worker watch bound to that delivery for the caller to cancel, as
    /// [`Self::Stopped`] does (Qodo, PR #1414): with the retry off no record
    /// holds that seq, and the watch would otherwise report an acknowledged
    /// worker as never having got its task. `None` once taken. Once the
    /// delivery is bound to its worker, only that worker is answered this way.
    NotPending { silence_seq: Option<u64> },
    /// The id is not the pane's current delivery (mistyped, or an earlier
    /// delegation's), or it is but was presented by an agent other than the
    /// worker the delivery is bound to, or by one that could not be identified.
    /// A no-op, so a pending retry keeps running and the watch stays armed.
    Unknown,
}

impl AckOutcome {
    /// Whether the id named the pane's current delivery, so the worker can be
    /// told its acknowledgement was recorded.
    pub fn matched(self) -> bool {
        match self {
            AckOutcome::Stopped { .. }
            | AckOutcome::AlreadyAcknowledged
            | AckOutcome::NotPending { .. } => true,
            AckOutcome::Unknown => false,
        }
    }
}

struct PendingDelivery {
    seq: u64,
    delivery_id: String,
    worker_agent_id: String,
    silence_seq: Option<u64>,
    /// Dropped with the record, which resolves the loop's cancel receiver.
    _cancel: oneshot::Sender<()>,
}

/// A pane's most recent delegation, whether or not a retry was armed for it.
struct CurrentDelivery {
    delivery_id: String,
    /// The worker the pointer was written for, once the dispatch has resolved
    /// it. `None` from [`PendingDeliveries::note_delivery`] until then, which is
    /// what tells [`PendingDeliveries::forget_pane`] that a dispatch may still
    /// be bringing up the agent this delivery belongs to.
    worker_agent_id: Option<String>,
    /// The silent-worker watch armed for this delivery, until an ack takes it.
    silence_seq: Option<u64>,
}

#[derive(Default)]
struct PendingInner {
    next_seq: u64,
    records: HashMap<String, PendingDelivery>,
    /// The last delivery id acknowledged per pane, so a repeat ack is told
    /// apart from one that never matched anything.
    last_acked: HashMap<String, String>,
    /// Each pane's most recent delegation, whether or not a retry was armed for
    /// it and after its loop ends, so an ack that arrives once the loop has
    /// already stopped is told apart from a mistyped or stale id — and can
    /// still cancel the delivery's silent-worker watch.
    current: HashMap<String, CurrentDelivery>,
}

/// The deliveries a retry loop is watching, one per worker pane.
///
/// Lives on [`AgentPtyRegistry`] because the three things that end a delivery —
/// the dispatch path, the hook loop's `ack` and `work-done` handlers — all hold
/// the registry and nothing else in common.
#[derive(Default)]
pub struct PendingDeliveries {
    inner: Mutex<PendingInner>,
}

impl PendingDeliveries {
    /// Register a delivery for `pane_id`, replacing (and so cancelling) any
    /// older one.
    ///
    /// `silence_seq` is the silent-worker watch armed for the same delivery,
    /// given here — before the first write — so an ack that arrives while that
    /// write is still in flight cancels the watch too (audit M3).
    pub fn arm(
        &self,
        pane_id: &str,
        delivery_id: &str,
        worker_agent_id: &str,
        silence_seq: Option<u64>,
        retype: RetypePolicy,
    ) -> ArmedDelivery {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let mut inner = self.inner.lock().unwrap();
        inner.next_seq += 1;
        let seq = inner.next_seq;
        inner.current.insert(
            pane_id.to_string(),
            CurrentDelivery {
                delivery_id: delivery_id.to_string(),
                worker_agent_id: Some(worker_agent_id.to_string()),
                silence_seq,
            },
        );
        inner.records.insert(
            pane_id.to_string(),
            PendingDelivery {
                seq,
                delivery_id: delivery_id.to_string(),
                worker_agent_id: worker_agent_id.to_string(),
                silence_seq,
                _cancel: cancel_tx,
            },
        );
        ArmedDelivery {
            seq,
            retype,
            cancel: cancel_rx,
        }
    }

    /// A newer delegation to `pane_id` begins: cancel whatever is pending.
    pub fn supersede(&self, pane_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.last_acked.remove(pane_id);
        inner.current.remove(pane_id);
        inner.records.remove(pane_id).is_some()
    }

    /// Record `delivery_id` as the pane's current delivery, armed or not, so
    /// its ack reads as [`AckOutcome::NotPending`] rather than
    /// [`AckOutcome::Unknown`] when no retry is pending. Called by every
    /// delegation, after [`Self::supersede`].
    pub fn note_delivery(&self, pane_id: &str, delivery_id: &str) {
        self.inner.lock().unwrap().current.insert(
            pane_id.to_string(),
            CurrentDelivery {
                delivery_id: delivery_id.to_string(),
                worker_agent_id: None,
                silence_seq: None,
            },
        );
    }

    /// Bind the pane's current delivery to the worker the pointer is written
    /// for and to the silent-worker watch armed for it, once the dispatch knows
    /// both — whether or not a retry is armed (Qodo, PR #1414). Without it a
    /// correct ack of a delivery with the retry off returned
    /// [`AckOutcome::NotPending`] and left the watch to report the worker quiet.
    ///
    /// A no-op unless `delivery_id` is still the pane's current delivery, so a
    /// dispatch that lost the pane to a newer delegation cannot attach its
    /// watch to that delegation's delivery.
    pub fn bind_current(
        &self,
        pane_id: &str,
        delivery_id: &str,
        worker_agent_id: &str,
        silence_seq: Option<u64>,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.current.get_mut(pane_id) {
            Some(current) if current.delivery_id == delivery_id => {
                current.worker_agent_id = Some(worker_agent_id.to_string());
                current.silence_seq = silence_seq;
                true
            }
            _ => false,
        }
    }

    /// The dispatch that noted `delivery_id` left without the pointer reaching
    /// anyone (Qodo, PR #1414): drop everything kept for it, so an ack of it
    /// from any sender answers [`AckOutcome::Unknown`] and records nothing.
    /// Left in place, an unbound delivery would take such an ack from anyone,
    /// for as long as no newer delegation replaced it, for a task nobody got.
    ///
    /// A no-op unless `delivery_id` is still the pane's current delivery, so a
    /// newer delegation that superseded it is never touched.
    pub fn forget_delivery_if_current(&self, pane_id: &str, delivery_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !inner
            .current
            .get(pane_id)
            .is_some_and(|current| current.delivery_id == delivery_id)
        {
            return false;
        }
        inner.current.remove(pane_id);
        if inner
            .records
            .get(pane_id)
            .is_some_and(|record| record.delivery_id == delivery_id)
        {
            inner.records.remove(pane_id);
        }
        // An ack taken before the dispatch gave up (the ack-before-bind
        // residual) must not make a later one read as a repeat.
        if inner
            .last_acked
            .get(pane_id)
            .is_some_and(|last| last == delivery_id)
        {
            inner.last_acked.remove(pane_id);
        }
        true
    }

    /// The pane closed for good (Qodo, PR #1414): drop what is kept for it, so
    /// a daemon that outlives many panes does not keep every one's last
    /// delivery. `closed_agent_id` is the agent whose record just left the
    /// registry, and the caller has established that no other agent holds the
    /// pane.
    ///
    /// `dispatch_idle` is whether the pane's dispatch lock was free. It must be
    /// read under the same registry lock hold that established the pane is
    /// unclaimed: a delegation notes its delivery under that lock before it
    /// knows its worker, and a `clear = true` delegate that re-creates a pane a
    /// concurrent close emptied (issue #606) keeps the pane id and brings up
    /// a new agent for a delivery noted before the close. So while a dispatch
    /// is in flight only what is bound to `closed_agent_id` goes; an unbound
    /// delivery, or one bound to any other agent, is the in-flight dispatch's
    /// and is kept. With the lock free every entry belongs to an agent no
    /// longer in the registry, and all of it goes.
    pub fn forget_pane(&self, pane_id: &str, closed_agent_id: &str, dispatch_idle: bool) {
        let mut inner = self.inner.lock().unwrap();
        if dispatch_idle
            || inner
                .records
                .get(pane_id)
                .is_some_and(|record| record.worker_agent_id == closed_agent_id)
        {
            inner.records.remove(pane_id);
        }
        let current_is_stale = match inner.current.get(pane_id) {
            None => true,
            Some(current) => {
                dispatch_idle || current.worker_agent_id.as_deref() == Some(closed_agent_id)
            }
        };
        if current_is_stale {
            inner.current.remove(pane_id);
            inner.last_acked.remove(pane_id);
        }
    }

    /// The pane's current delivery id and the watch bound to it, for tests that
    /// must ack a delivery no retry record holds.
    #[cfg(test)]
    pub(crate) fn current_for_test(&self, pane_id: &str) -> Option<(String, Option<u64>)> {
        self.inner
            .lock()
            .unwrap()
            .current
            .get(pane_id)
            .map(|current| (current.delivery_id.clone(), current.silence_seq))
    }

    /// Whether anything at all is kept for `pane_id`.
    #[cfg(test)]
    pub(crate) fn tracks_pane(&self, pane_id: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.records.contains_key(pane_id)
            || inner.current.contains_key(pane_id)
            || inner.last_acked.contains_key(pane_id)
    }

    /// Acknowledge `delivery_id` on `pane_id`.
    ///
    /// Matches only the pane's CURRENT delivery, and only that delivery's
    /// worker — an older generation's ack cannot stop a newer generation's
    /// loop. `agent_id` is who sent the ack; `None` means the sender could not
    /// be identified, and never matches a pending record (Qodo, #1414), so the
    /// retry keeps running. The same holds once nothing is pending: a delivery
    /// bound to its worker answers any other sender [`AckOutcome::Unknown`].
    /// Idempotent. The outcome is what the `ack` CLI
    /// reports to the worker; see [`AckOutcome::matched`].
    pub fn acknowledge(
        &self,
        pane_id: &str,
        delivery_id: &str,
        agent_id: Option<&str>,
    ) -> AckOutcome {
        let mut inner = self.inner.lock().unwrap();
        let matches = inner.records.get(pane_id).is_some_and(|record| {
            record.delivery_id == delivery_id
                && agent_id.is_some_and(|agent| agent == record.worker_agent_id)
        });
        if matches {
            let record = inner
                .records
                .remove(pane_id)
                .expect("record present under the same lock");
            inner
                .last_acked
                .insert(pane_id.to_string(), delivery_id.to_string());
            // The watch is taken here, so a later ack cannot cancel it twice.
            if let Some(current) = inner.current.get_mut(pane_id) {
                current.silence_seq = None;
            }
            return AckOutcome::Stopped {
                silence_seq: record.silence_seq,
            };
        }
        // Still pending under this id, but presented by another agent or by
        // one that could not be identified: the retry keeps running, so it
        // must not read as recorded.
        if inner.records.contains_key(pane_id) {
            return AckOutcome::Unknown;
        }
        // The same rule once nothing is pending (Qodo, PR #1414): a delivery
        // bound to its worker is acknowledged only by that worker. Anyone else
        // — an older generation that read the overwritten role file, or a
        // sender that could not be identified — is answered `Unknown`, never
        // told its ack was recorded, and changes nothing.
        if let Some(current) = inner.current.get(pane_id)
            && current.delivery_id == delivery_id
            && current
                .worker_agent_id
                .as_deref()
                .is_some_and(|worker| agent_id != Some(worker))
        {
            return AckOutcome::Unknown;
        }
        if inner
            .last_acked
            .get(pane_id)
            .is_some_and(|last| last == delivery_id)
        {
            return AckOutcome::AlreadyAcknowledged;
        }
        if let Some(current) = inner.current.get_mut(pane_id)
            && current.delivery_id == delivery_id
        {
            // Bound to this sender, or not bound yet: an ack that lands between
            // `note_delivery` and `bind_current` has no worker to check against
            // and is taken as the pane's (an accepted residual,
            // docs/develop/delegate-delivery.md).
            let silence_seq = current.silence_seq.take();
            inner
                .last_acked
                .insert(pane_id.to_string(), delivery_id.to_string());
            return AckOutcome::NotPending { silence_seq };
        }
        AckOutcome::Unknown
    }

    /// A `work-done` arrived from `pane_id`: its delivery is proven.
    ///
    /// A stale completion from an older delegation also stops a newer loop.
    /// That is the safe direction — no duplicate — and degrades to the
    /// behaviour before #1383.
    pub fn retire_on_work_done(&self, pane_id: &str) -> bool {
        self.inner.lock().unwrap().records.remove(pane_id).is_some()
    }

    /// Remove the pane's record only if it is still generation `seq`. The loop
    /// calls this on every exit, so a finished loop leaves nothing behind and a
    /// newer delivery's record is never touched.
    pub fn finish(&self, pane_id: &str, seq: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.records.get(pane_id).is_some_and(|r| r.seq == seq) {
            inner.records.remove(pane_id);
            true
        } else {
            false
        }
    }

    /// Whether `seq` is still the pane's pending delivery.
    pub fn is_current(&self, pane_id: &str, seq: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .records
            .get(pane_id)
            .is_some_and(|r| r.seq == seq)
    }

    /// The pane's pending delivery id, for tests that must ack a delivery the
    /// dispatch minted.
    #[cfg(test)]
    pub(crate) fn pending_id_for_test(&self, pane_id: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .records
            .get(pane_id)
            .map(|record| record.delivery_id.clone())
    }

    /// Whether anything is pending for `pane_id`.
    pub fn is_pending(&self, pane_id: &str) -> bool {
        self.inner.lock().unwrap().records.contains_key(pane_id)
    }
}

/// Whether a delivery to a worker of `agent_type` may be retried at all.
///
/// A pane whose agent the deck cannot identify has no channel that could ever
/// report a turn, so every attempt would be a potential duplicate: it gets no
/// retry, and the docs tell the user to declare the agent (CLAUDE.md rule 20).
pub fn agent_type_supports_retry(agent_type: Option<&AgentType>) -> bool {
    match agent_type {
        Some(
            AgentType::ClaudeCode
            | AgentType::OpenCode
            | AgentType::Pi
            | AgentType::Codex
            | AgentType::Devin,
        ) => true,
        Some(AgentType::None) | None => false,
    }
}

/// How a retry loop ended. Returned for tests and logged by the loop itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryEnd {
    /// The worker began a turn.
    Received,
    /// The worker's agent reported a quota block.
    Blocked,
    /// The event stream lagged, so proof may have been dropped. A duplicate is
    /// the dangerous direction, so this stops the loop like proof would.
    Lagged,
    /// An ack, a `work-done` or a newer delegation removed the record.
    Cancelled,
    /// A newer delegation took the pane's dispatch lock first.
    Superseded,
    /// The worker pane began closing.
    PaneClosed,
    /// The worker agent exited.
    AgentExited,
    /// The event bus closed (daemon shutdown).
    BusClosed,
    /// A re-delivery was refused or failed; see the log line for which.
    WriteStopped,
    /// Every scheduled re-delivery went out and none produced proof.
    Exhausted,
}

/// Everything one retry loop needs, captured by the dispatch before its first
/// write.
pub(crate) struct DeliveryRetry {
    pub registry: Arc<AgentPtyRegistry>,
    /// Subscribed BEFORE the first write, so a fast agent's first event cannot
    /// land before the loop is polled.
    pub event_rx: broadcast::Receiver<BroadcastMsg>,
    pub armed: ArmedDelivery,
    pub schedule: RetrySchedule,
    pub pane_id: String,
    pub worker_agent_id: String,
    pub role: String,
    pub delivery_id: String,
    /// The exact pointer the first write typed.
    pub pointer: String,
    /// The worker's PTY geometry epoch
    /// ([`AgentPtyRegistry::geometry_changes_of`]), sampled BEFORE the first
    /// write began (Qodo, PR #1414). Sampling it when the loop starts would miss
    /// a resize during that write — which holds its CR for up to the echo bound
    /// plus `SUBMIT_DELAY` — and read the blank screen it leaves as a pointer
    /// that never went in, retyping a duplicate. Sampled early, a resize at any
    /// point after the write started counts; one before it only makes the
    /// first reading more cautious.
    pub pointer_epoch: Option<u64>,
    pub orchestration: Option<OrchestrationIdentity>,
    /// What the re-deliveries wrote, shared with the silent-worker report so
    /// it can say what was tried.
    pub redeliveries: Arc<RedeliveryCounts>,
    /// Whether a silent-worker report will cover exhaustion. When it will not,
    /// the loop logs exhaustion itself.
    pub silence_report_armed: bool,
    /// Told how the loop ended, once it has. The silent-worker report waits on
    /// it, so the report is never written while a re-delivery is still pending
    /// — however far a `SessionStart` or a busy dispatch lock pushed one — and
    /// the count it quotes is final.
    pub done: Option<oneshot::Sender<RetryEnd>>,
}

/// Spawn the retry loop onto its own task. The dispatch returns as it does
/// without a retry, so the loop never holds the pane's dispatch lock between
/// attempts and a superseding delegate does not queue behind it.
pub(crate) fn spawn(retry: DeliveryRetry) -> tokio::task::JoinHandle<RetryEnd> {
    tokio::spawn(run(retry))
}

/// The longest a submit-only probe waits for proof of a turn before the pointer
/// is typed again. See [`probe_grace`].
pub const PROBE_GRACE: Duration = Duration::from_secs(5);

/// How long a submit-only probe over a screen that does not show the pointer
/// waits, from the moment its Enter went out, before the pointer is retyped:
/// [`PROBE_GRACE`], or half the wait before the next re-delivery when that is
/// shorter, so a probe written on time has its retype land before the next
/// attempt is due. A probe held up past that point still gets the whole grace;
/// the next attempt stays anchored where its wait began, never sooner than
/// [`MIN_RETRY_WAIT`] after the retype.
pub fn probe_grace(next_wait: Duration) -> Duration {
    PROBE_GRACE.min(next_wait / 2)
}

/// The signals a retry loop waits on between writes.
struct Watch {
    cancel: oneshot::Receiver<()>,
    closing: oneshot::Receiver<()>,
    exited: oneshot::Receiver<()>,
    event_rx: broadcast::Receiver<BroadcastMsg>,
}

impl Watch {
    /// Wait until `due` for proof or for anything that ends the loop. `Ok` when
    /// the time passed with neither.
    ///
    /// A genuine `SessionStart` pushes `due` out once per call by a readiness
    /// interval, so the next write lands after the boot rather than inside it —
    /// and after issue #1031's own submit probe, when that one is armed too.
    async fn until(
        &mut self,
        mut due: tokio::time::Instant,
        pane_id: &str,
        worker_agent_id: &str,
        role: &str,
        delivery_id: &str,
    ) -> Result<(), RetryEnd> {
        let mut postponed = false;
        loop {
            tokio::select! {
                biased;
                _ = &mut self.cancel => return Err(RetryEnd::Cancelled),
                _ = &mut self.closing => return Err(RetryEnd::PaneClosed),
                _ = &mut self.exited => return Err(RetryEnd::AgentExited),
                msg = self.event_rx.recv() => match msg {
                    Ok(BroadcastMsg::Event(event)) => {
                        // Matched by pane and agent only: events carry no
                        // delivery id, so a late event from a superseded
                        // delegation can stop this loop. Safe direction — the
                        // pointer stays typed once — like a stale work-done.
                        if event.pane_id.as_deref() != Some(pane_id)
                            || event.agent_id.as_deref() != Some(worker_agent_id)
                        {
                            continue;
                        }
                        match classify_event(&event) {
                            EventVerdict::Received => return Err(RetryEnd::Received),
                            EventVerdict::Blocked => return Err(RetryEnd::Blocked),
                            EventVerdict::Postpone if !postponed => {
                                postponed = true;
                                let interval = crate::state::delegate_readiness_buffer()
                                    .max(crate::prompt_delivery::REARM_READINESS_BUFFER);
                                due = due.max(tokio::time::Instant::now() + interval);
                                info!(
                                    pane_id = %escape_id_for_log(pane_id),
                                    role = %escape_id_for_log(role),
                                    delivery_id = %delivery_id,
                                    interval_ms = interval.as_millis(),
                                    "delegate retry: the worker's agent announced a session \
                                     after the pointer went in; holding the next re-delivery \
                                     for a readiness interval"
                                );
                            }
                            EventVerdict::Postpone | EventVerdict::Ignore => {}
                        }
                    }
                    // Not evidence about this pane. Listed rather than
                    // wildcarded so a future variant is classified on purpose.
                    Ok(
                        BroadcastMsg::OrchestrationSurface(_)
                        | BroadcastMsg::WorktreeKept(_)
                        | BroadcastMsg::Unknown,
                    ) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => return Err(RetryEnd::Lagged),
                    Err(broadcast::error::RecvError::Closed) => return Err(RetryEnd::BusClosed),
                },
                _ = tokio::time::sleep_until(due) => return Ok(()),
            }
        }
    }
}

/// Issue #1383: what the in-place retry wrote for one delivery, counted where
/// the bytes went out. An attempt is not a re-send of the pointer: each begins
/// with a bare Enter, and only some go on to type the pointer again, so the
/// silent-worker report quotes the two apart (Qodo, PR #1414).
#[derive(Debug, Default)]
pub struct RedeliveryCounts {
    attempts: AtomicU32,
    enters: AtomicU32,
    retypes: AtomicU32,
}

/// One reading of [`RedeliveryCounts`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RedeliveryTally {
    /// Re-delivery attempts that wrote anything.
    pub attempts: u32,
    /// Bare Enters written: each attempt's probe, and any second Enter.
    pub enters: u32,
    /// Times the pointer itself was typed again and submitted.
    pub retypes: u32,
}

impl RedeliveryCounts {
    /// Re-delivery attempts that wrote anything.
    pub fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::SeqCst)
    }

    pub fn tally(&self) -> RedeliveryTally {
        RedeliveryTally {
            attempts: self.attempts(),
            enters: self.enters.load(Ordering::SeqCst),
            retypes: self.retypes.load(Ordering::SeqCst),
        }
    }
}

/// The loop body. See the module docs.
pub(crate) async fn run(retry: DeliveryRetry) -> RetryEnd {
    let DeliveryRetry {
        registry,
        event_rx,
        armed,
        schedule,
        pane_id,
        worker_agent_id,
        role,
        delivery_id,
        pointer,
        pointer_epoch,
        orchestration,
        redeliveries,
        silence_report_armed,
        done,
    } = retry;
    let ArmedDelivery {
        seq,
        retype,
        cancel,
    } = armed;
    let mut watch = Watch {
        cancel,
        closing: registry.pane_close_signal(&pane_id),
        exited: registry.agent_exit_signal(&worker_agent_id),
        event_rx,
    };
    let waits = schedule.waits().to_vec();
    let total_attempts = waits.len();
    let wait_at = |index: usize| {
        waits
            .get(index)
            .or(waits.last())
            .copied()
            .unwrap_or_default()
    };
    let mut last_classification: Option<Composer> = None;
    // PR #1414 review: latched once any reading saw the pointer submitted
    // (`Composer::PointerInHistory`). From then on this delivery is never
    // retyped, whatever a later screen shows.
    let mut seen_submitted = false;
    // The PTY geometry epoch from before the pointer's bytes were last typed. A
    // resize clears the scrollback the composer is read from (PRD #104 M3), so
    // after one a blank screen is no evidence the pointer is gone. The first
    // write's comes from the dispatch, sampled before that write began.
    let mut pointer_epoch = pointer_epoch;
    // When the current wait began: the first write, then the start of each
    // re-delivery. Each schedule entry is measured from here, so the retype
    // comes out of the wait rather than stretching the schedule. A probe's
    // grace is not: it runs from the probe's own write (below).
    let mut anchor = tokio::time::Instant::now();
    let redeliver_ctx = RedeliverCtx {
        registry: &registry,
        seq,
        retype,
        pane_id: &pane_id,
        worker_agent_id: &worker_agent_id,
        role: &role,
        delivery_id: &delivery_id,
        pointer: &pointer,
        orchestration: orchestration.as_ref(),
        total_attempts,
        counts: &redeliveries,
    };

    let end = 'outer: {
        // One more wait than there are re-deliveries: the last is the silence
        // after the final re-delivery, before exhaustion is declared.
        for index in 0..=total_attempts {
            // Never sooner than the shortest wait from now, however long the
            // previous write spent queued behind the pane's dispatch lock.
            let due = (anchor + wait_at(index)).max(tokio::time::Instant::now() + MIN_RETRY_WAIT);
            if let Err(end) = watch
                .until(due, &pane_id, &worker_agent_id, &role, &delivery_id)
                .await
            {
                break 'outer end;
            }
            if index == total_attempts {
                break 'outer RetryEnd::Exhausted;
            }
            let attempt = index + 1;
            anchor = tokio::time::Instant::now();
            let probe_attempt =
                redeliver(&redeliver_ctx, Phase::Probe, attempt, &mut pointer_epoch).await;
            // Qodo round 7 on PR #1414: the grace runs from the moment the
            // probe's Enter went out, not from `anchor`. The probe queues on the
            // pane's dispatch lock and writer, reads the screen off the executor
            // and holds its CR to the `SUBMIT_DELAY` floor, so under a short
            // schedule a grace measured from `anchor` can have run out before
            // the Enter lands, and the retype would follow it with no time for
            // the turn it starts to show.
            let probe_written = tokio::time::Instant::now();
            let probe = match probe_attempt {
                Attempt::Written(composer) => {
                    last_classification = Some(composer);
                    redeliveries.attempts.fetch_add(1, Ordering::SeqCst);
                    composer
                }
                // The probe never declines: it writes its Enter whatever the screen shows.
                Attempt::Skipped | Attempt::Declined(_) => continue,
                Attempt::Stop(end) => break 'outer end,
            };
            match probe {
                // A screen the deck cannot read gets the Enter and nothing
                // more, as before this audit: a resize-blanked one belongs to a
                // worker that has not repainted since and may well hold the
                // pointer already (`orchestration/delegate/044` attaches the
                // worker pane, which resizes it, after the task was accepted).
                Composer::Unreadable => continue,
                // PR #1414 review: the pointer was already submitted. The
                // probe's Enter is all this attempt sends; the grace and the
                // retype step could only add a second copy, over a screen that
                // scrolled the pointer away meanwhile. Latched, so no later
                // attempt retypes it either.
                Composer::PointerInHistory => {
                    seen_submitted = true;
                    continue;
                }
                Composer::PointerInComposer | Composer::Absent => {}
            }
            // The screen does not show the pointer, which cannot tell an empty
            // composer from one holding the pointer off-screen or unechoed. The
            // Enter just sent submits it in the second case, and the turn it
            // starts is proof; only silence past the grace means there was
            // nothing to submit.
            //
            // Issue #1243: a pointer the screen DOES show waits out the same
            // grace and then, if it is still in the input box, gets a second
            // Enter, never a copy; one showing only in the transcript gets
            // neither (see `Composer::PointerInHistory`). Measured against
            // Claude Code: once a CR has been taken into a paste, the composer
            // holds the pointer and an empty line, the next Enter is swallowed
            // and the one after it submits, in every case observed (3 lost
            // first writes under load, 2 forced ones). One Enter per re-delivery
            // recovered it only at the second re-delivery.
            let grace = probe_grace(wait_at(attempt));
            if let Err(end) = watch
                .until(
                    probe_written + grace,
                    &pane_id,
                    &worker_agent_id,
                    &role,
                    &delivery_id,
                )
                .await
            {
                break 'outer end;
            }
            match redeliver(
                &redeliver_ctx,
                Phase::Retype {
                    probe,
                    seen_submitted,
                },
                attempt,
                &mut pointer_epoch,
            )
            .await
            {
                Attempt::Written(composer) | Attempt::Declined(composer) => {
                    last_classification = Some(composer);
                    seen_submitted |= composer == Composer::PointerInHistory;
                }
                Attempt::Skipped => {}
                Attempt::Stop(end) => break 'outer end,
            }
        }
        RetryEnd::Exhausted
    };

    registry.pending_deliveries().finish(&pane_id, seq);
    match end {
        RetryEnd::Exhausted if !silence_report_armed => warn!(
            pane_id = %escape_id_for_log(&pane_id),
            role = %escape_id_for_log(&role),
            delivery_id = %delivery_id,
            redeliveries = ?redeliveries.tally(),
            last_classification = ?last_classification,
            "delegate retry: every scheduled re-delivery went out and the worker produced no \
             event and no ack; the task may never have reached it. The worker is not respawned"
        ),
        RetryEnd::Exhausted => info!(
            pane_id = %escape_id_for_log(&pane_id),
            role = %escape_id_for_log(&role),
            delivery_id = %delivery_id,
            redeliveries = ?redeliveries.tally(),
            last_classification = ?last_classification,
            "delegate retry: schedule exhausted; the silent-worker report covers it"
        ),
        other => info!(
            pane_id = %escape_id_for_log(&pane_id),
            role = %escape_id_for_log(&role),
            delivery_id = %delivery_id,
            redeliveries = ?redeliveries.tally(),
            end = ?other,
            "delegate retry: stopped"
        ),
    }
    if let Some(done) = done {
        // The report may already have been cancelled; nobody listening is fine.
        let _ = done.send(end);
    }
    end
}

/// The largest screen, in character cells, the retry reads. Above it the
/// screen reads as [`Composer::Unreadable`] — the probe's Enter, never a
/// retype — rather than building and scanning a parser the size of an attach
/// client's geometry on every attempt. The echo gate's cap, for the same
/// reason; see [`crate::submit_echo::MAX_ECHO_WATCH_CELLS`].
pub const MAX_RETRY_SCREEN_CELLS: u32 = crate::submit_echo::MAX_ECHO_WATCH_CELLS;

/// Read the worker's screen for this delivery: [`classify_composer`] over the
/// snapshot, or [`Composer::Unreadable`] when the screen cannot be read at all
/// — over [`MAX_RETRY_SCREEN_CELLS`], an empty snapshot, or a parser panic.
/// None of those is evidence the pointer is gone, so none of them may retype.
fn read_composer(
    snapshot: &[u8],
    rows: u16,
    cols: u16,
    delivery_id: &str,
    after_id: &str,
    resized_since_write: bool,
) -> Composer {
    if u32::from(rows) * u32::from(cols) > MAX_RETRY_SCREEN_CELLS {
        return Composer::Unreadable;
    }
    let Some((screen_rows, cursor_row, cursor_col)) =
        crate::pane_screen_text::visible_rows_and_cursor(snapshot, rows, cols)
    else {
        return Composer::Unreadable;
    };
    classify_composer(
        &screen_rows,
        cursor_row,
        cursor_col,
        delivery_id,
        after_id,
        resized_since_write,
    )
}

enum Attempt {
    Written(Composer),
    /// The screen was read, and what it showed means nothing is written.
    Declined(Composer),
    Skipped,
    Stop(RetryEnd),
}

/// Which half of a re-delivery to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Every re-delivery starts with a submit-only probe: a bare Enter, which
    /// submits a pointer still sitting in the composer and is ignored by an
    /// empty one.
    Probe,
    /// After a probe went unanswered: type the pointer again over a screen that
    /// does not show it, press Enter once more over an input box that holds it
    /// (issue #1243), or do nothing when it shows only in the transcript.
    /// `probe` is what this attempt's probe read: a pointer the probe saw in
    /// the input box is never retyped, whatever the screen shows now.
    /// `seen_submitted` is whether any reading of this delivery has seen the
    /// pointer submitted, after which it is never retyped at all.
    Retype {
        probe: Composer,
        seen_submitted: bool,
    },
}

/// The per-loop constants [`redeliver`] reads.
#[derive(Clone, Copy)]
struct RedeliverCtx<'a> {
    registry: &'a Arc<AgentPtyRegistry>,
    seq: u64,
    retype: RetypePolicy,
    pane_id: &'a str,
    worker_agent_id: &'a str,
    role: &'a str,
    delivery_id: &'a str,
    pointer: &'a str,
    orchestration: Option<&'a OrchestrationIdentity>,
    total_attempts: usize,
    counts: &'a RedeliveryCounts,
}

async fn redeliver(
    ctx: &RedeliverCtx<'_>,
    phase: Phase,
    attempt: usize,
    pointer_epoch: &mut Option<u64>,
) -> Attempt {
    let RedeliverCtx {
        registry,
        seq,
        retype,
        pane_id,
        worker_agent_id,
        role,
        delivery_id,
        pointer,
        orchestration,
        total_attempts,
        counts,
    } = *ctx;
    // The dispatch lock, then the generation check: a newer delegation that took
    // the lock first has already superseded this record, so an old pointer can
    // never land after a new delegation starts.
    //
    // Issue #544 (PR #1398): held as a `PaneDispatchHold`, #1398's own shape for
    // the delegate pointer, because a retype waits behind the worker's draft
    // the way the first write does and sets the lock down for as long as it
    // sleeps there. A superseding delegate or a `pane restart` is therefore
    // never parked behind that wait (up to the draft cap); the write re-checks
    // the delivery under the pane's writer once the wait is over (below).
    let mut dispatch_hold = registry.hold_pane_dispatch(pane_id).await;
    if !registry.pending_deliveries().is_current(pane_id, seq) {
        return Attempt::Stop(RetryEnd::Superseded);
    }
    // Never type onto a draft someone started since the deck's last write. The
    // attempt still counts toward the bound.
    if registry.user_typed_since_automatic_write(pane_id) {
        info!(
            pane_id = %escape_id_for_log(pane_id),
            role = %escape_id_for_log(role),
            delivery_id = %delivery_id,
            attempt,
            total_attempts,
            phase = ?phase,
            "delegate retry: someone has typed into the worker pane since the deck last wrote to \
             it; skipping this re-delivery"
        );
        return Attempt::Skipped;
    }
    // Issue #544 (PR #1398): a bare Enter submits whatever the composer holds,
    // and #1398 does not defer an empty payload, so the probe is skipped
    // outright while the worker pane has an unsent draft. That can hold with
    // nobody having typed since the deck's last write: a bracketed paste left
    // open takes the first write's own CR as paste content, so the draft
    // survives it. The attempt still counts toward the bound. This is the early
    // out, before the screen is read; the check that holds is the one under the
    // pane's writer (below), which also covers the second Enter and a draft that
    // appears meanwhile. A retype waits for the draft instead.
    if phase == Phase::Probe && registry.draft_pending(pane_id) {
        log_draft_skip(ctx, attempt, phase);
        return Attempt::Skipped;
    }
    // Off the async worker: the snapshot takes the registry's synchronous mutex
    // and copies the scrollback, and the parse behind the cell cap is CPU work
    // (Qodo, #1414; the same move as #1347's). Still under the dispatch lock, so
    // no newer delegation's write can land between this read and the one below.
    let resized_since_write = registry.geometry_changes_of(worker_agent_id) != *pointer_epoch;
    let snapshot_registry = Arc::clone(registry);
    let snapshot_agent = worker_agent_id.to_string();
    let snapshot_id = delivery_id.to_string();
    let after_id = pointer
        .rsplit_once(delivery_id)
        .map_or("", |(_, after_id)| after_id)
        .to_string();
    let read = tokio::task::spawn_blocking(move || {
        snapshot_registry
            .snapshot_with_pty_size(&snapshot_agent)
            .map(|(bytes, rows, cols)| {
                read_composer(
                    &bytes,
                    rows,
                    cols,
                    &snapshot_id,
                    &after_id,
                    resized_since_write,
                )
            })
    })
    .await;
    let composer = match read {
        Ok(Ok(composer)) => composer,
        Ok(Err(_)) => return Attempt::Stop(RetryEnd::AgentExited),
        // The blocking task panicked: nothing was read, so nothing is known.
        Err(_) => Composer::Unreadable,
    };
    // Only the SECOND Enter is scoped to the input box (auditor M2 / reviewer
    // LOW-1). The probe is the same bare Enter whatever the screen shows:
    // `Absent` gets it on no evidence at all, and the probe exists because the
    // screen cannot be trusted to show what the input box holds — a worker
    // that never reads its terminal has its pointer echoed and "submitted" by
    // the line discipline, and an agent that parks its cursor away from typed
    // text (Devin and Pi were not measured) would show a pointer stuck in its
    // composer as history. Withholding the probe there would forfeit the
    // recovery on a reading that is only a heuristic.
    let text = match (phase, composer) {
        (Phase::Probe, _) => "",
        // The unanswered Enter left it in the composer: a copy would double
        // it. Press Enter once more instead (issue #1243, see the loop).
        (Phase::Retype { .. }, Composer::PointerInComposer) => "",
        // Only in the transcript: the Enter submitted it, so the task landed.
        // The input box holds something else, or nothing, and an Enter there
        // would submit that; a copy would be a second turn for the same task.
        (Phase::Retype { .. }, Composer::PointerInHistory) => {
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                total_attempts,
                "delegate retry: after the Enter the pointer shows only above the worker's input \
                 box, not in it, so it was submitted; not pressing Enter into whatever that box \
                 holds, and never retyping it for this delivery"
            );
            return Attempt::Declined(composer);
        }
        (Phase::Retype { .. }, Composer::Unreadable) => {
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                total_attempts,
                "delegate retry: the worker's screen cannot be read after the Enter (cleared by a \
                 resize, empty, unparseable or over the size cap); not retyping into a composer \
                 the deck cannot read"
            );
            return Attempt::Declined(composer);
        }
        // PR #1414 review: the probe saw the pointer in the input box, so its
        // disappearance since is the Enter taking it or the screen moving on,
        // never evidence the box is empty.
        (
            Phase::Retype {
                probe: Composer::PointerInComposer,
                ..
            },
            Composer::Absent,
        ) => {
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                total_attempts,
                "delegate retry: the pointer was in the worker's input box before the Enter and \
                 is gone from its screen now; not retyping it"
            );
            return Attempt::Declined(composer);
        }
        (
            Phase::Retype {
                seen_submitted: true,
                ..
            },
            Composer::Absent,
        ) => {
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                total_attempts,
                "delegate retry: an earlier reading showed the pointer already submitted, so its \
                 absence from the screen now is not evidence it never went in; not retyping it"
            );
            return Attempt::Declined(composer);
        }
        // Issue #1383 audit: a worker whose delivery the deck cannot confirm
        // (#1390) may have taken the first pointer, cleared it and started
        // working without any turn the loop can see. A copy would be a second
        // turn for the same task.
        (Phase::Retype { .. }, Composer::Absent) if retype == RetypePolicy::Never => {
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                total_attempts,
                "delegate retry: the worker stayed silent after the Enter, but its agent cannot \
                 confirm a delivered prompt (a wrapper-hosted pane), so the pointer that left \
                 its screen may already be running; not retyping it"
            );
            return Attempt::Declined(composer);
        }
        (Phase::Retype { .. }, Composer::Absent) => pointer,
    };
    let revalidate_registry = Arc::clone(registry);
    let revalidate_pane = pane_id.to_string();
    let expected_orchestration = orchestration.cloned();
    // Issue #544 / PR #1398: every write here goes through the DEFERRING
    // first-write entry. A retype is the same pointer the dispatch's first write
    // waited to type, so it waits for the worker's unsent draft the same way,
    // with the dispatch lock set down meanwhile. An empty payload is never
    // deferred (#1398 exempts it); the checks above and under the writer keep
    // an Enter off a draft instead.
    //
    // A deferred retype can sleep until the draft cap, and an ack, a
    // `work-done`, a supersede or a pane close can land in that time. The
    // closure below is what re-checks all of them: it runs once, under the
    // pane's writer, on the pass that writes — so after any wait — and nothing
    // is written when the delivery is no longer current. It also refuses while
    // a draft is still pending, which is the cap letting the write go on top of
    // it: #1398 degrades a FIRST write to that rather than lose a prompt, but a
    // re-send has a first write behind it and skips instead. And a retype is
    // refused once someone has typed since the deck last wrote, which is how
    // most drafts end: the payload guard under the writer only refuses a
    // repeat of bytes still in the box, and the first write's were released on
    // `Applied`.
    //
    // Issue #1243: a retype holds its CR until the pointer renders, like the
    // first write and every other submit. It lands in a worker that may be as
    // busy as the one that lost the first write's Enter; a pane that does not
    // echo pays the gate's bound on each retype.
    //
    // The geometry epoch is read BEFORE the write, as the dispatch reads the
    // first write's: a resize during this write's `SUBMIT_DELAY` then counts.
    //
    // With a draft wait in front of it, "before the write" means after the
    // wait: the closure samples the epoch under the writer.
    let epoch_before_write = Arc::new(Mutex::new(None::<u64>));
    let refused_for_draft = Arc::new(AtomicBool::new(false));
    let epoch_sample = Arc::clone(&epoch_before_write);
    let draft_flag = Arc::clone(&refused_for_draft);
    let epoch_agent = worker_agent_id.to_string();
    let retyping = !text.is_empty();
    let outcome = registry
        .write_and_submit_guarded_first_write_parking(
            pane_id,
            text,
            worker_agent_id,
            || async move {
                if revalidate_registry.is_pane_closing(&revalidate_pane) {
                    return false;
                }
                // Issue #1383 audit (M4): an ack, a `work-done` or a newer
                // delegation that removed this delivery while the write waited for
                // the pane's writer, or for the worker's draft (#544), must stop it
                // here, under that writer, rather than type bytes after it. The ack
                // path does not take the dispatch lock, so the check above cannot
                // cover that wait.
                //
                // Issue #1383 audit (M3), accepted residual: this runs ONCE, before
                // the payload. An ack that lands after it — during the
                // `SUBMIT_DELAY` between the payload and the CR — does not stop the
                // CR: a probe or second Enter still writes its one `\r`, and a
                // retype, whose pointer bytes are already in the PTY by then, still
                // writes the `\r` that submits them. An ack means the agent is
                // already working, so what that costs is one CR, or one queued copy
                // of the pointer, in the composer of a worker that has its task.
                // Serialising the ack against this write was judged not worth
                // holding the ack path on the pane writer for.
                if !revalidate_registry
                    .pending_deliveries()
                    .is_current(&revalidate_pane, seq)
                {
                    return false;
                }
                if revalidate_registry.draft_pending(&revalidate_pane)
                    || (retyping
                        && revalidate_registry.user_typed_since_automatic_write(&revalidate_pane))
                {
                    draft_flag.store(true, Ordering::SeqCst);
                    return false;
                }
                *epoch_sample.lock().unwrap() =
                    revalidate_registry.geometry_changes_of(&epoch_agent);
                crate::state::orchestration_still_matches(
                    expected_orchestration.as_ref(),
                    revalidate_registry
                        .pane_orchestration(&revalidate_pane)
                        .as_ref(),
                )
            },
            std::time::Instant::now(),
            &mut dispatch_hold,
            || {},
        )
        .await
        .map(|sent| sent.detail);
    match outcome {
        Ok(GuardedSendDetail::Outcome(GuardedSend::Applied)) => {
            if text.is_empty() {
                counts.enters.fetch_add(1, Ordering::SeqCst);
            } else {
                counts.retypes.fetch_add(1, Ordering::SeqCst);
                crate::state::settle_one_shot_payload_record(
                    registry,
                    pane_id,
                    text,
                    Some(GuardedSend::Applied),
                );
                // The pointer's bytes went in at this epoch, or before a resize
                // a later reading will see.
                *pointer_epoch = *epoch_before_write.lock().unwrap();
            }
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                total_attempts,
                classification = ?composer,
                "delegate retry: no proof the worker received its task pointer; {}",
                match (phase, composer) {
                    (Phase::Probe, Composer::PointerInComposer) =>
                        "the pointer is in its input box, so pressed Enter instead of retyping it",
                    (Phase::Probe, Composer::Unreadable) =>
                        "its screen cannot be read (cleared by a resize since the pointer went in, \
                         empty, unparseable or over the size cap), so pressed Enter rather than \
                         risk typing a second copy",
                    (Phase::Probe, Composer::Absent) if retype == RetypePolicy::Never =>
                        "the pointer is not on its screen, so pressed Enter; its agent cannot \
                         confirm a delivered prompt, so the pointer is never retyped",
                    (Phase::Probe, Composer::Absent) =>
                        "the pointer is not on its screen, which cannot tell an empty input box \
                         from one holding it unshown, so pressed Enter first; the pointer is \
                         retyped only if the worker stays silent",
                    (Phase::Probe, Composer::PointerInHistory) =>
                        "the pointer shows on its screen but not in its input box, so pressed \
                         Enter rather than type a second copy; no second Enter follows, and it is \
                         never retyped for this delivery",
                    (Phase::Retype { .. }, Composer::PointerInComposer) =>
                        "the pointer is still in its input box after the Enter, and an agent that \
                         took the first CR into a paste swallows the next Enter, so pressed Enter \
                         once more rather than retyping it",
                    (Phase::Retype { .. }, _) =>
                        "the worker stayed silent after the Enter, so re-typed the pointer into \
                         the same process",
                }
            );
            Attempt::Written(composer)
        }
        // Removed while the write waited on the writer: nothing was written.
        Ok(_) if !registry.pending_deliveries().is_current(pane_id, seq) => {
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                "delegate retry: the delivery was acknowledged, completed or superseded while the \
                 re-delivery waited for the worker pane; nothing written"
            );
            Attempt::Stop(RetryEnd::Cancelled)
        }
        // Issue #544: refused under the writer for a draft; nothing written.
        Ok(_) if refused_for_draft.load(Ordering::SeqCst) => {
            log_draft_skip(ctx, attempt, phase);
            Attempt::Skipped
        }
        // A partial write: a prefix may be in the box, and a further attempt
        // could submit it. Stop, and leave the payload record standing (#715).
        Ok(GuardedSendDetail::Outcome(GuardedSend::Ambiguous)) => {
            warn!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                "delegate retry: the re-delivery was ambiguous (partial write); no further \
                 attempts"
            );
            Attempt::Stop(RetryEnd::WriteStopped)
        }
        Ok(other) => {
            info!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                outcome = ?other,
                "delegate retry: the re-delivery was refused (the pane changed hands, is closing, \
                 or someone typed into it); no further attempts"
            );
            Attempt::Stop(RetryEnd::WriteStopped)
        }
        Err(error) => {
            warn!(
                pane_id = %escape_id_for_log(pane_id),
                role = %escape_id_for_log(role),
                delivery_id = %delivery_id,
                attempt,
                error = %error,
                "delegate retry: the re-delivery failed to reach the worker pane; no further \
                 attempts"
            );
            Attempt::Stop(RetryEnd::WriteStopped)
        }
    }
}

/// Issue #544 (PR #1398): the one log line for a re-delivery skipped over an
/// unsent draft in the worker pane, whichever check caught it.
fn log_draft_skip(ctx: &RedeliverCtx<'_>, attempt: usize, phase: Phase) {
    info!(
        pane_id = %escape_id_for_log(ctx.pane_id),
        role = %escape_id_for_log(ctx.role),
        delivery_id = %ctx.delivery_id,
        attempt,
        total_attempts = ctx.total_attempts,
        phase = ?phase,
        "delegate retry: the worker pane holds an unsent draft, or someone typed into it while \
         this re-delivery waited for one; an Enter or a retyped pointer would submit it, so \
         nothing written and skipping this re-delivery"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|v| Duration::from_millis(*v)).collect()
    }

    #[test]
    fn retry_schedule_unset_is_the_default() {
        let schedule = RetrySchedule::parse(None);
        assert_eq!(schedule.waits(), ms(&DEFAULT_RETRY_SCHEDULE_MS).as_slice());
        assert!(schedule.is_enabled());
        assert_eq!(schedule.redeliveries(), 3);
    }

    #[test]
    fn retry_schedule_zero_empty_and_whitespace_disable() {
        for raw in ["0", "", "   ", " 0 "] {
            let schedule = RetrySchedule::parse(Some(raw));
            assert!(!schedule.is_enabled(), "{raw:?} must disable the retry");
            assert_eq!(schedule.total_span(), Duration::ZERO);
        }
    }

    #[test]
    fn retry_schedule_parses_a_list() {
        let schedule = RetrySchedule::parse(Some("1500, 3000 ,6000"));
        assert_eq!(schedule.waits(), ms(&[1500, 3000, 6000]).as_slice());
    }

    #[test]
    fn retry_schedule_clamps_each_entry() {
        let schedule = RetrySchedule::parse(Some("1,99999999999999999999999,5000"));
        assert_eq!(schedule.waits(), ms(&[100, 300_000, 5000]).as_slice());
    }

    #[test]
    fn retry_schedule_truncates_to_the_entry_limit() {
        let raw = ["1000"; MAX_RETRY_ENTRIES + 4].join(",");
        let schedule = RetrySchedule::parse(Some(&raw));
        assert_eq!(schedule.redeliveries(), MAX_RETRY_ENTRIES);
    }

    /// Qodo round 4: an entry past the limit is not read, so a malformed one
    /// there cannot discard the valid entries before it.
    #[test]
    fn retry_schedule_ignores_a_malformed_entry_past_the_limit() {
        let mut entries = vec!["1000"; MAX_RETRY_ENTRIES];
        entries.extend(["soon", "-5"]);
        let schedule = RetrySchedule::parse(Some(&entries.join(",")));
        assert_eq!(
            schedule.waits(),
            ms(&[1000; MAX_RETRY_ENTRIES]).as_slice(),
            "the eight valid entries were thrown out for one that is never read"
        );
        // Within the limit a malformed entry still falls back to the default.
        let mut entries = vec!["1000"; MAX_RETRY_ENTRIES - 1];
        entries.push("soon");
        assert_eq!(
            RetrySchedule::parse(Some(&entries.join(","))),
            RetrySchedule::parse(None)
        );
    }

    #[test]
    fn retry_schedule_garbage_falls_back_to_the_default() {
        for raw in ["soon", "1500,x", "1500,,3000", "-5", "1.5"] {
            assert_eq!(
                RetrySchedule::parse(Some(raw)),
                RetrySchedule::parse(None),
                "{raw:?} must fall back to the default"
            );
        }
    }

    #[test]
    fn retry_schedule_total_span_is_the_sum_plus_the_last_entry() {
        let schedule = RetrySchedule::parse(Some("1500,3000,6000"));
        assert_eq!(schedule.total_span(), Duration::from_millis(16_500));
        assert_eq!(
            RetrySchedule::parse(None).total_span(),
            Duration::from_millis(220_000)
        );
    }

    #[test]
    fn delivery_id_is_d_dash_eight_lowercase_hex() {
        for _ in 0..100 {
            let id = mint_delivery_id();
            assert_eq!(id.len(), 10, "{id}");
            assert!(is_valid_delivery_id(&id), "{id}");
        }
    }

    #[test]
    fn delivery_id_mints_do_not_repeat() {
        let ids: std::collections::HashSet<String> =
            (0..1000).map(|_| mint_delivery_id()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn delivery_id_validator_rejects_malformed_values() {
        for bad in [
            "",
            "d-",
            "d-1234567",
            "d-123456789",
            "d-ABCDEF12",
            "x-12345678",
            "D-12345678",
            "d-1234567g",
            "d-1234\n678",
            "d-12345678\n",
            " d-12345678",
            "d-12345678\u{1b}",
        ] {
            assert!(!is_valid_delivery_id(bad), "{bad:?} must be rejected");
        }
        assert!(is_valid_delivery_id("d-0123abcd"));
    }

    #[test]
    fn pointer_suffix_and_header_name_the_delivery() {
        assert_eq!(pointer_suffix("d-7f3a9c21"), " [delivery d-7f3a9c21]");
        let header = task_file_ack_header("d-7f3a9c21");
        assert!(header.starts_with("## First: acknowledge this task"));
        assert!(header.contains(&format!(
            "{} ack d-7f3a9c21",
            crate::platform::paths::binary_name()
        )));
        assert!(header.contains("If the command fails, is not recognised"));
        // PR #1414 review: a role whose allowlist does not name `ack` yet must
        // not stall at the approval prompt.
        assert!(header.contains("needs an approval you do not get"));
        assert!(header.contains("skip it and carry on with the task"));
        assert!(header.contains("it is the same task: do not start it again"));
    }

    fn event(event_type: EventType) -> AgentEvent {
        serde_json::from_value(serde_json::json!({
            "session_id": "s",
            "agent_type": "claude_code",
            "event_type": event_type,
            "timestamp": "2026-09-28T00:00:00Z",
            "pane_id": "p1",
            "agent_id": "a1",
        }))
        .unwrap()
    }

    #[test]
    fn classify_event_covers_every_event_type() {
        let table = [
            (EventType::Thinking, EventVerdict::Received),
            (EventType::ToolStart, EventVerdict::Received),
            (EventType::ToolEnd, EventVerdict::Received),
            (EventType::SubagentStart, EventVerdict::Received),
            (EventType::SubagentStop, EventVerdict::Received),
            (EventType::Compacting, EventVerdict::Received),
            (EventType::PermissionRequest, EventVerdict::Received),
            (EventType::QuotaBlocked, EventVerdict::Blocked),
            (EventType::SessionStart, EventVerdict::Postpone),
            (EventType::SessionEnd, EventVerdict::Ignore),
            (EventType::Idle, EventVerdict::Ignore),
            (EventType::Error, EventVerdict::Ignore),
            (EventType::WaitingForInput, EventVerdict::Ignore),
            (EventType::ShellBusy, EventVerdict::Ignore),
            (EventType::ShellIdle, EventVerdict::Ignore),
            (EventType::PullRequest, EventVerdict::Ignore),
            (EventType::Unknown, EventVerdict::Ignore),
        ];
        for (event_type, expected) in table {
            assert_eq!(
                classify_event(&event(event_type.clone())),
                expected,
                "{event_type:?}"
            );
        }
    }

    #[test]
    fn classify_event_agrees_with_the_proof_predicate() {
        // `Received` is exactly the silence watch's "a turn began" set, so the
        // two can never disagree about what counts as delivery.
        for event_type in [
            EventType::Thinking,
            EventType::ToolStart,
            EventType::ToolEnd,
            EventType::SubagentStart,
            EventType::SubagentStop,
            EventType::Compacting,
            EventType::PermissionRequest,
            EventType::QuotaBlocked,
            EventType::SessionStart,
            EventType::SessionEnd,
            EventType::Idle,
            EventType::Error,
            EventType::WaitingForInput,
            EventType::ShellBusy,
            EventType::ShellIdle,
            EventType::PullRequest,
            EventType::Unknown,
        ] {
            let e = event(event_type);
            assert_eq!(
                classify_event(&e) == EventVerdict::Received,
                crate::state::worker_event_proves_delivery(&e),
                "{:?}",
                e.event_type
            );
            // Issue #1493: the same frame drawn by a Codex pane's wrapper.
            let mut codex_output = e.clone();
            codex_output.agent_type = AgentType::Codex;
            codex_output.metadata.insert(
                crate::event::WRAPPER_OUTPUT_CLASSIFIED_METADATA_KEY.to_string(),
                crate::event::WRAPPER_OUTPUT_CLASSIFIED_METADATA_VALUE.to_string(),
            );
            assert_eq!(classify_event(&codex_output), EventVerdict::Ignore);
            assert!(!crate::state::worker_event_proves_delivery(&codex_output));
        }
    }

    #[test]
    fn classify_event_boot_chatter_never_counts_as_received() {
        // Claude Code's boot `SessionStart` postpones; OpenCode's startup
        // `session.idle` is ignored. Neither disarms the loop.
        let mut claude_start = event(EventType::SessionStart);
        claude_start.agent_type = AgentType::ClaudeCode;
        assert_eq!(classify_event(&claude_start), EventVerdict::Postpone);
        let mut opencode_idle = event(EventType::Idle);
        opencode_idle.agent_type = AgentType::OpenCode;
        assert_eq!(classify_event(&opencode_idle), EventVerdict::Ignore);
    }

    #[test]
    fn classify_event_wrapper_session_start_is_ignored() {
        let mut start = event(EventType::SessionStart);
        start.metadata.insert(
            crate::event::SESSION_START_ORIGIN_METADATA_KEY.to_string(),
            crate::event::WRAPPER_FORK_SESSION_START_ORIGIN.to_string(),
        );
        assert!(start.is_wrapper_session_start());
        assert_eq!(classify_event(&start), EventVerdict::Ignore);
    }

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    /// The last row, where a screen that ends in its input line has its cursor.
    fn last(screen: &[String]) -> usize {
        screen.len().saturating_sub(1)
    }

    /// The cursor column right after row `row`'s text, where an input box
    /// leaves it after typing.
    fn end(screen: &[String], row: usize) -> usize {
        screen.get(row).map_or(0, |row| row.chars().count())
    }

    #[test]
    fn classify_composer_finds_the_id_on_the_cursor_row() {
        let screen = rows(&[
            "welcome",
            "> Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21]",
        ]);
        assert_eq!(
            classify_composer(
                &screen,
                last(&screen),
                end(&screen, last(&screen)),
                "d-7f3a9c21",
                "]",
                false
            ),
            Composer::PointerInComposer
        );
    }

    #[test]
    fn classify_composer_finds_an_id_split_across_rows() {
        let screen = rows(&["> Read … for your task. [delivery d-7f3a", "9c21]"]);
        assert_eq!(
            classify_composer(
                &screen,
                last(&screen),
                end(&screen, last(&screen)),
                "d-7f3a9c21",
                "]",
                false
            ),
            Composer::PointerInComposer
        );
    }

    /// The cursor sits right after the typed `]`, inside the box: the right
    /// border beyond it is not between the pointer and the cursor.
    #[test]
    fn classify_composer_finds_an_id_behind_border_glyphs() {
        let screen = rows(&[
            "┃ > Read .dot-agent-deck/worker-task-coder.md [delivery d-7f3a   ┃",
            "┃ 9c21]                                                          ┃",
        ]);
        assert_eq!(
            classify_composer(
                &screen,
                last(&screen),
                "┃ 9c21]".chars().count(),
                "d-7f3a9c21",
                "]",
                false
            ),
            Composer::PointerInComposer
        );
    }

    /// The id fills its row exactly and only the pointer's closing `]` wraps
    /// onto the cursor's row — measured at 77 columns, which is where
    /// `orchestration/delegate/044`'s worker pane lands. Bare, and inside
    /// OpenCode-style border glyphs.
    #[test]
    fn classify_composer_an_id_whose_closing_bracket_wrapped_is_in_the_composer() {
        let bare = rows(&[
            "Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21",
            "]",
        ]);
        assert_eq!(
            classify_composer(&bare, 1, end(&bare, 1), "d-7f3a9c21", "]", false),
            Composer::PointerInComposer
        );
        let bordered = rows(&[
            "┃  Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-",
            "┃  7f3a9c21",
            "┃  ]",
        ]);
        assert_eq!(
            classify_composer(&bordered, 2, end(&bordered, 2), "d-7f3a9c21", "]", false),
            Composer::PointerInComposer
        );
        // Anything but the rest of the pointer on the cursor's row is another
        // input: an empty prompt glyph, or text.
        for cursor_row in ["❯", "> x", "]]"] {
            let screen = rows(&[
                "Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21",
                cursor_row,
            ]);
            assert_eq!(
                classify_composer(&screen, 1, end(&screen, 1), "d-7f3a9c21", "]", false),
                Composer::PointerInHistory,
                "cursor row {cursor_row:?}"
            );
        }
    }

    /// Claude Code 2.1.284's measured layout: the cursor on the composer row,
    /// right after the id, with a border and two footer rows below it.
    #[test]
    fn classify_composer_reads_the_composer_above_claude_codes_footers() {
        let screen = rows(&[
            "▝▜██████▀  Haiku 4.5",
            "",
            "──────────",
            "❯ Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21]",
            "──────────",
            "⚠ Transcript saving is off",
            "⏸ manual mode on",
        ]);
        assert_eq!(
            classify_composer(&screen, 3, end(&screen, 3), "d-7f3a9c21", "]", false),
            Composer::PointerInComposer
        );
    }

    /// Auditor M2 / reviewer LOW-1: a submitted pointer still in the transcript,
    /// directly above an empty input box, is not a pointer in the composer —
    /// whether the box is bordered or a bare prompt line.
    #[test]
    fn classify_composer_an_id_in_history_above_an_empty_composer_is_not_in_it() {
        let bordered = rows(&[
            "> Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21]",
            "──────────",
            "❯",
            "──────────",
            "? for shortcuts",
        ]);
        assert_eq!(
            classify_composer(&bordered, 2, end(&bordered, 2), "d-7f3a9c21", "]", false),
            Composer::PointerInHistory
        );
        let bare = rows(&[
            "> Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21]",
            "> ",
        ]);
        assert_eq!(
            classify_composer(&bare, 1, end(&bare, 1), "d-7f3a9c21", "]", false),
            Composer::PointerInHistory,
            "a whole copy on the row above is not a wrap onto the cursor row"
        );
        // The same screen with the cursor parked above the composer, as a cooked
        // terminal leaves it after a submitted line.
        let submitted = rows(&["READY> [delivery d-7f3a9c21]", ""]);
        assert_eq!(
            classify_composer(&submitted, 1, end(&submitted, 1), "d-7f3a9c21", "]", false),
            Composer::PointerInHistory
        );
    }

    /// Auditor re-check of c9ad0a10: a copy of the id earlier on the cursor's
    /// row, with other text after it, is transcript sharing that row — and so
    /// is a pointer that ends to the RIGHT of the cursor. Only a pointer that
    /// ends at the cursor, whitespace aside, is in the composer.
    #[test]
    fn classify_composer_an_id_earlier_on_the_cursor_row_is_not_in_the_composer() {
        let followed = rows(&[
            "welcome",
            "READY> [delivery d-7f3a9c21] accepted, working on it > next",
        ]);
        assert_eq!(
            classify_composer(&followed, 1, end(&followed, 1), "d-7f3a9c21", "]", false),
            Composer::PointerInHistory
        );
        // Unrelated text straight after the id, before any `]`.
        let glued = rows(&["> [delivery d-7f3a9c21 !!]"]);
        assert_eq!(
            classify_composer(&glued, 0, end(&glued, 0), "d-7f3a9c21", "]", false),
            Composer::PointerInHistory
        );
        // The pointer ends after the cursor, which sits at the prompt.
        let ahead = rows(&["> [delivery d-7f3a9c21]"]);
        assert_eq!(
            classify_composer(&ahead, 0, 2, "d-7f3a9c21", "]", false),
            Composer::PointerInHistory
        );
        // Whitespace between the pointer's end and the cursor is still the end.
        let padded = rows(&["> [delivery d-7f3a9c21]   "]);
        assert_eq!(
            classify_composer(&padded, 0, end(&padded, 0), "d-7f3a9c21", "]", false),
            Composer::PointerInComposer
        );
        // A wrapped id whose tail shares the cursor's row with other text.
        let wrapped = rows(&["> Read … for your task. [delivery d-7f3a", "9c21] done"]);
        assert_eq!(
            classify_composer(&wrapped, 1, end(&wrapped, 1), "d-7f3a9c21", "]", false),
            Composer::PointerInHistory
        );
    }

    #[test]
    fn classify_composer_ignores_another_delivery_and_a_blank_screen() {
        let screen = rows(&["> Read … for your task. [delivery d-00000000]"]);
        assert_eq!(
            classify_composer(&screen, 0, end(&screen, 0), "d-7f3a9c21", "]", false),
            Composer::Absent
        );
        assert_eq!(
            classify_composer(&[], 0, 0, "d-7f3a9c21", "]", false),
            Composer::Absent
        );
        assert_eq!(
            classify_composer(&rows(&["", "   "]), 1, 3, "d-7f3a9c21", "]", false),
            Composer::Absent
        );
    }

    /// PR #1414 review (Greptile P1): a screen the deck cannot read at all —
    /// an empty snapshot, or one the parser panics on — is `Unreadable`, never
    /// `Absent`, so it can never earn a retype.
    #[test]
    fn read_composer_an_empty_snapshot_is_unreadable_not_absent() {
        assert_eq!(
            read_composer(b"", 24, 80, "d-7f3a9c21", "]", false),
            Composer::Unreadable
        );
        // The same helper reads a real screen as usual.
        assert_eq!(
            read_composer(b"> [delivery d-7f3a9c21]", 24, 80, "d-7f3a9c21", "]", false),
            Composer::PointerInComposer
        );
        assert_eq!(
            read_composer(b"> ", 24, 80, "d-7f3a9c21", "]", false),
            Composer::Absent
        );
    }

    /// PR #1414 review (Greptile P2): the retry's screen read has the echo
    /// gate's size bound. Over it, the screen is not parsed at all and reads as
    /// `Unreadable` (the probe's Enter, never a retype), even when the pointer
    /// is plainly there.
    #[test]
    fn read_composer_over_the_cell_cap_is_unreadable() {
        let screen = b"> [delivery d-7f3a9c21]";
        assert_eq!(
            read_composer(screen, 4096, 4096, "d-7f3a9c21", "]", false),
            Composer::Unreadable
        );
        // 1000 x 123 = 123,000 cells, inside the cap.
        assert_eq!(
            read_composer(screen, 123, 1000, "d-7f3a9c21", "]", false),
            Composer::PointerInComposer
        );
        // One row over the cap, whichever axis carries it.
        let cols = 1000u16;
        let rows = u16::try_from(MAX_RETRY_SCREEN_CELLS / u32::from(cols) + 1).unwrap();
        assert!(u32::from(rows) * u32::from(cols) > MAX_RETRY_SCREEN_CELLS);
        assert_eq!(
            read_composer(screen, rows, cols, "d-7f3a9c21", "]", false),
            Composer::Unreadable
        );
    }

    #[test]
    fn classify_composer_a_screen_blanked_by_a_resize_is_unreadable() {
        assert_eq!(
            classify_composer(&[], 0, 0, "d-7f3a9c21", "]", true),
            Composer::Unreadable
        );
        // A screen the agent has repainted since is read as usual.
        assert_eq!(
            classify_composer(&rows(&["> "]), 0, 2, "d-7f3a9c21", "]", true),
            Composer::Absent
        );
        assert_eq!(
            classify_composer(
                &rows(&["> [delivery d-7f3a9c21]"]),
                0,
                23,
                "d-7f3a9c21",
                "]",
                true
            ),
            Composer::PointerInComposer
        );
    }

    #[test]
    fn pending_deliveries_ack_stops_the_loop_and_is_idempotent() {
        let store = PendingDeliveries::default();
        let mut armed = store.arm("p1", "d-11111111", "a1", Some(7), RetypePolicy::Allowed);
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::Stopped {
                silence_seq: Some(7)
            }
        );
        assert!(armed.cancel.try_recv().is_err(), "cancel must resolve");
        assert!(!store.is_pending("p1"));
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::AlreadyAcknowledged
        );
    }

    /// PR #1414 review: an ack that arrives after the pane's retry already
    /// ended — a real agent reports the turn before the `ack` it runs as a
    /// tool — or for a delivery never armed is still the pane's current
    /// delivery, and reads as recorded. An earlier delegation's id does not.
    #[test]
    fn pending_deliveries_ack_of_the_current_delivery_with_nothing_pending_is_recorded() {
        let store = PendingDeliveries::default();
        let armed = store.arm("p1", "d-11111111", "a1", None, RetypePolicy::Allowed);
        assert!(store.finish("p1", armed.seq));
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::NotPending { silence_seq: None }
        );
        assert!(AckOutcome::NotPending { silence_seq: None }.matched());
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::AlreadyAcknowledged
        );
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a1")),
            AckOutcome::Unknown
        );
        assert!(!AckOutcome::Unknown.matched());

        // Never armed (retry off, or an agent type that is not retried).
        store.supersede("p1");
        store.note_delivery("p1", "d-33333333");
        assert!(!store.is_pending("p1"));
        assert_eq!(
            store.acknowledge("p1", "d-11111111", None),
            AckOutcome::Unknown,
            "a superseded delivery's id is stale"
        );
        assert_eq!(
            store.acknowledge("p1", "d-33333333", None),
            AckOutcome::NotPending { silence_seq: None }
        );
        assert_eq!(
            store.acknowledge("p2", "d-33333333", None),
            AckOutcome::Unknown,
            "another pane's delivery"
        );
    }

    #[test]
    fn pending_deliveries_unknown_id_leaves_the_record_armed() {
        let store = PendingDeliveries::default();
        let armed = store.arm("p1", "d-11111111", "a1", None, RetypePolicy::Allowed);
        assert_eq!(
            store.acknowledge("p1", "d-22222222", None),
            AckOutcome::Unknown
        );
        assert!(store.is_current("p1", armed.seq));
    }

    #[test]
    fn pending_deliveries_ack_from_another_pane_or_agent_does_not_match() {
        let store = PendingDeliveries::default();
        let armed = store.arm("p1", "d-11111111", "a1", None, RetypePolicy::Allowed);
        assert_eq!(
            store.acknowledge("p2", "d-11111111", None),
            AckOutcome::Unknown
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a0")),
            AckOutcome::Unknown,
            "an older generation's ack must not stop this generation's loop"
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", None),
            AckOutcome::Unknown,
            "an ack whose sender could not be identified must not stop the loop"
        );
        assert!(store.is_current("p1", armed.seq));
    }

    #[test]
    fn pending_deliveries_supersede_cancels_the_older_loop() {
        let store = PendingDeliveries::default();
        let mut old = store.arm("p1", "d-11111111", "a1", None, RetypePolicy::Allowed);
        assert!(store.supersede("p1"));
        assert!(old.cancel.try_recv().is_err());
        let mut older = store.arm("p1", "d-22222222", "a1", None, RetypePolicy::Allowed);
        let newer = store.arm("p1", "d-33333333", "a1", None, RetypePolicy::Allowed);
        assert!(older.cancel.try_recv().is_err(), "re-arming replaces");
        assert!(store.is_current("p1", newer.seq));
    }

    #[test]
    fn pending_deliveries_stale_finish_leaves_a_newer_record() {
        let store = PendingDeliveries::default();
        let old = store.arm("p1", "d-11111111", "a1", None, RetypePolicy::Allowed);
        let newer = store.arm("p1", "d-22222222", "a1", None, RetypePolicy::Allowed);
        assert!(!store.finish("p1", old.seq));
        assert!(store.is_current("p1", newer.seq));
        assert!(store.finish("p1", newer.seq));
        assert!(!store.is_pending("p1"));
    }

    #[test]
    fn pending_deliveries_work_done_cancels() {
        let store = PendingDeliveries::default();
        let mut armed = store.arm("p1", "d-11111111", "a1", None, RetypePolicy::Allowed);
        assert!(store.retire_on_work_done("p1"));
        assert!(armed.cancel.try_recv().is_err());
        assert!(!store.retire_on_work_done("p1"));
    }

    #[test]
    fn pending_deliveries_ack_returns_the_silence_seq_registered_at_arm() {
        // The seq is known before the first write, so an ack that lands before
        // that write completes still names the watch to cancel (audit M3).
        let store = PendingDeliveries::default();
        let _old = store.arm("p1", "d-11111111", "a1", Some(3), RetypePolicy::Allowed);
        let _newer = store.arm("p1", "d-22222222", "a1", Some(4), RetypePolicy::Allowed);
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a1")),
            AckOutcome::Stopped {
                silence_seq: Some(4)
            }
        );
    }

    /// Qodo, PR #1414: with the retry off no record is armed, so the watch seq
    /// rides on the current delivery. Its worker's ack hands it back once; an
    /// ack from anyone else hands back nothing and does not use it up.
    #[test]
    fn pending_deliveries_ack_with_nothing_armed_returns_the_bound_silence_seq() {
        let store = PendingDeliveries::default();
        store.note_delivery("p1", "d-11111111");
        assert!(store.bind_current("p1", "d-11111111", "a1", Some(9)));
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a0")),
            AckOutcome::Unknown,
            "an older generation's ack must not cancel this delivery's watch"
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", None),
            AckOutcome::Unknown,
            "an unidentified sender must not cancel the watch"
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::NotPending {
                silence_seq: Some(9)
            }
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::AlreadyAcknowledged
        );
        // A dispatch that lost the pane to a newer delegation binds nothing.
        store.supersede("p1");
        store.note_delivery("p1", "d-22222222");
        assert!(!store.bind_current("p1", "d-11111111", "a1", Some(10)));
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a1")),
            AckOutcome::NotPending { silence_seq: None }
        );
    }

    /// Qodo, PR #1414: a dispatch that gives up on its delivery drops it, so
    /// no sender's ack of it is recorded afterwards — including one taken
    /// before it gave up — and a newer delegation's delivery is left alone.
    #[test]
    fn pending_deliveries_forget_delivery_if_current_drops_only_its_own_delivery() {
        let store = PendingDeliveries::default();
        store.note_delivery("p1", "d-11111111");
        // The ack-before-bind residual: taken from anyone while unbound.
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a0")),
            AckOutcome::NotPending { silence_seq: None }
        );
        assert!(store.forget_delivery_if_current("p1", "d-11111111"));
        for sender in [Some("a0"), Some("a1"), None] {
            assert_eq!(
                store.acknowledge("p1", "d-11111111", sender),
                AckOutcome::Unknown,
                "sender {sender:?}"
            );
        }
        assert!(!store.tracks_pane("p1"), "nothing may be kept for it");

        // A newer delegation superseded it: forgetting the old id is a no-op.
        store.note_delivery("p1", "d-22222222");
        store.supersede("p1");
        store.note_delivery("p1", "d-33333333");
        let armed = store.arm("p1", "d-33333333", "a1", Some(2), RetypePolicy::Never);
        assert!(!store.forget_delivery_if_current("p1", "d-22222222"));
        assert!(store.is_current("p1", armed.seq));
        assert_eq!(
            store.current_for_test("p1"),
            Some(("d-33333333".to_string(), Some(2)))
        );
        // Its own id drops the armed record too.
        assert!(store.forget_delivery_if_current("p1", "d-33333333"));
        assert!(!store.is_pending("p1"));
    }

    /// Qodo, PR #1414: with nothing pending, a delivery bound to its worker is
    /// never reported as recorded to anyone else — before the worker's ack,
    /// after it, or after the retry loop ended — and such an ack changes
    /// nothing the worker's own ack then depends on.
    #[test]
    fn pending_deliveries_ack_of_a_bound_delivery_from_another_sender_is_unknown() {
        let store = PendingDeliveries::default();
        // Retry off: bound, never armed.
        store.note_delivery("p1", "d-11111111");
        assert!(store.bind_current("p1", "d-11111111", "a1", Some(4)));
        for sender in [Some("a0"), None] {
            let outcome = store.acknowledge("p1", "d-11111111", sender);
            assert_eq!(outcome, AckOutcome::Unknown, "sender {sender:?}");
            assert!(
                !outcome.matched(),
                "sender {sender:?} was told Acknowledged"
            );
        }
        assert_eq!(
            store.current_for_test("p1"),
            Some(("d-11111111".to_string(), Some(4))),
            "another sender's ack must leave the watch bound"
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::NotPending {
                silence_seq: Some(4)
            }
        );
        // After the worker's ack, a repeat from anyone else is still not its.
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a0")),
            AckOutcome::Unknown
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::AlreadyAcknowledged
        );

        // Retry on, loop already ended: the record's worker still decides.
        let armed = store.arm("p1", "d-22222222", "a1", Some(5), RetypePolicy::Allowed);
        assert!(store.finish("p1", armed.seq));
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a0")),
            AckOutcome::Unknown
        );
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a1")),
            AckOutcome::NotPending {
                silence_seq: Some(5)
            }
        );
    }

    /// A retry that already ended leaves the watch on the current delivery,
    /// and a stop hands it back only once.
    #[test]
    fn pending_deliveries_watch_is_handed_back_once_whether_stopped_or_not() {
        let store = PendingDeliveries::default();
        let armed = store.arm("p1", "d-11111111", "a1", Some(5), RetypePolicy::Allowed);
        assert!(store.finish("p1", armed.seq));
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::NotPending {
                silence_seq: Some(5)
            }
        );

        let _armed = store.arm("p1", "d-22222222", "a1", Some(6), RetypePolicy::Allowed);
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a1")),
            AckOutcome::Stopped {
                silence_seq: Some(6)
            }
        );
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a1")),
            AckOutcome::AlreadyAcknowledged
        );
    }

    /// Qodo, PR #1414: a pane that closes for good leaves nothing behind, but a
    /// delivery an in-flight dispatch noted for the pane's next agent survives
    /// the close of its previous one.
    #[test]
    fn pending_deliveries_forget_pane_keeps_an_in_flight_dispatchs_delivery() {
        let store = PendingDeliveries::default();
        let _armed = store.arm("p1", "d-11111111", "a1", Some(1), RetypePolicy::Allowed);
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a1")),
            AckOutcome::Stopped {
                silence_seq: Some(1)
            }
        );
        store.forget_pane("p1", "a1", true);
        assert!(!store.tracks_pane("p1"));

        // A clear=true delegate noted its delivery, then a concurrent close of
        // the old agent landed while it was still bringing the new one up.
        store.note_delivery("p1", "d-22222222");
        store.forget_pane("p1", "a1", false);
        assert!(store.bind_current("p1", "d-22222222", "a2", Some(2)));
        assert_eq!(
            store.acknowledge("p1", "d-22222222", Some("a2")),
            AckOutcome::NotPending {
                silence_seq: Some(2)
            }
        );

        // Mid-dispatch, what is bound to the closing agent still goes.
        let _armed = store.arm("p1", "d-33333333", "a2", None, RetypePolicy::Allowed);
        store.forget_pane("p1", "a2", false);
        assert!(!store.tracks_pane("p1"));

        // And another pane is untouched.
        store.note_delivery("p2", "d-44444444");
        store.forget_pane("p1", "a2", true);
        assert!(store.tracks_pane("p2"));
    }

    #[test]
    fn agent_type_gate_excludes_an_unidentified_agent() {
        assert!(agent_type_supports_retry(Some(&AgentType::OpenCode)));
        assert!(agent_type_supports_retry(Some(&AgentType::ClaudeCode)));
        assert!(agent_type_supports_retry(Some(&AgentType::Codex)));
        assert!(agent_type_supports_retry(Some(&AgentType::Devin)));
        assert!(agent_type_supports_retry(Some(&AgentType::Pi)));
        assert!(!agent_type_supports_retry(Some(&AgentType::None)));
        assert!(!agent_type_supports_retry(None));
    }
}

/// Issue #1383: the loop, driven against a real registry and a real PTY. Real
/// time rather than a paused clock, because the guarded write sleeps its own
/// submit delay on the Tokio clock and the PTY reader runs on a thread.
#[cfg(all(test, unix))]
mod loop_tests {
    use super::*;
    use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};

    const POINTER: &str =
        "Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-1383beef]";
    const ID: &str = "d-1383beef";

    /// [`Fixture::start_composer`]'s worker.
    const COMPOSER: &str = r#"import os
import sys
import tty

tty.setraw(0)
sink = open(sys.argv[1], 'ab', buffering=0)
os.write(1, b'READY')
while chunk := os.read(0, 4096):
    sink.write(chunk)
    os.write(1, chunk.replace(b'\r', b'').replace(b'\n', b''))
"#;

    /// A worker that does not echo and, on every resize, clears its screen the
    /// way an agent's repaint does: after a resize its screen holds bytes, all
    /// of them blank. An empty snapshot is unreadable on its own, so a blank
    /// screen needs this to depend on whether the resize was seen at all.
    const CLEARS_ON_RESIZE: &str = r#"import os
import signal
import sys
import termios

attrs = termios.tcgetattr(0)
attrs[3] &= ~termios.ECHO
termios.tcsetattr(0, termios.TCSANOW, attrs)
signal.signal(signal.SIGWINCH, lambda *_: os.write(1, b'\x1b[2J\x1b[H'))
sink = open(sys.argv[1], 'ab', buffering=0)
os.write(1, b'READY')
while chunk := os.read(0, 4096):
    sink.write(chunk)
"#;

    /// Whether `python3` can be spawned. Through Tokio's process API so the
    /// check does not block a runtime worker.
    async fn python3_available() -> bool {
        tokio::process::Command::new("python3")
            .arg("--version")
            .output()
            .await
            .is_ok_and(|out| out.status.success())
    }

    struct Fixture {
        registry: Arc<AgentPtyRegistry>,
        agent: String,
        pane: String,
        tx: broadcast::Sender<BroadcastMsg>,
        sink: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        /// A worker that copies its input lines into a file. `echo = false`
        /// turns the terminal echo off, so a typed pointer never reaches the
        /// screen — the composer then reads as empty.
        async fn start(pane: &str, echo: bool) -> Self {
            Self::start_with(pane, if echo { "true" } else { "stty -echo" }).await
        }

        /// A worker whose terminal is set up by `stty` before it copies its
        /// input into a file.
        async fn start_with(pane: &str, stty: &str) -> Self {
            Self::spawn(pane, |sink| {
                format!("{stty} && printf READY && exec cat > '{}'", sink.display())
            })
            .await
        }

        /// A worker whose input box shows what is typed into it and keeps the
        /// cursor right after it, as an agent's composer that ignores Enter
        /// does: raw, painting every byte but CR and LF, and copying all of
        /// them into the file. A raw terminal's own echo is no stand-in for it
        /// — it paints each ignored Enter after the pointer as `^M`. `None`
        /// where `python3` is not available.
        async fn start_composer(pane: &str) -> Option<Self> {
            if !python3_available().await {
                return None;
            }
            Some(Self::spawn_python(pane, COMPOSER).await)
        }

        /// A worker running [`CLEARS_ON_RESIZE`]. `None` where `python3` is
        /// not available.
        async fn start_clearing_on_resize(pane: &str) -> Option<Self> {
            if !python3_available().await {
                return None;
            }
            Some(Self::spawn_python(pane, CLEARS_ON_RESIZE).await)
        }

        /// A worker running the Python `source`, which is handed the sink's path
        /// as its one argument. The script is written through Tokio's file API
        /// so the fixture does not block a runtime worker.
        async fn spawn_python(pane: &str, source: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("composer.py");
            tokio::fs::write(&script, source).await.unwrap();
            Self::spawn_in(dir, pane, |sink| {
                format!(
                    "exec python3 -u '{}' '{}'",
                    script.display(),
                    sink.display()
                )
            })
            .await
        }

        async fn spawn(pane: &str, command: impl FnOnce(&std::path::Path) -> String) -> Self {
            Self::spawn_in(tempfile::tempdir().unwrap(), pane, command).await
        }

        async fn spawn_in(
            dir: tempfile::TempDir,
            pane: &str,
            command: impl FnOnce(&std::path::Path) -> String,
        ) -> Self {
            let sink = dir.path().join("sink");
            let command = command(&sink);
            let registry = Arc::new(AgentPtyRegistry::new());
            let agent = registry
                .spawn_agent(SpawnOptions {
                    command: Some(&command),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn worker stand-in");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !String::from_utf8_lossy(
                &registry
                    .snapshot_off_runtime(&agent)
                    .await
                    .unwrap_or_default(),
            )
            .contains("READY")
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "worker never came up"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let (tx, _) = broadcast::channel(64);
            Self {
                registry,
                agent,
                pane: pane.to_string(),
                tx,
                sink,
                _dir: dir,
            }
        }

        /// The dispatch's first write, then the loop, exactly as
        /// `dispatch_one_owned` arms them.
        async fn deliver_and_retry(
            &self,
            schedule: &str,
        ) -> (tokio::task::JoinHandle<RetryEnd>, Arc<RedeliveryCounts>) {
            self.deliver_and_retry_with(schedule, RetypePolicy::Allowed)
                .await
        }

        async fn deliver_and_retry_with(
            &self,
            schedule: &str,
            retype: RetypePolicy,
        ) -> (tokio::task::JoinHandle<RetryEnd>, Arc<RedeliveryCounts>) {
            self.deliver_then_retry(schedule, retype, |_| {}).await
        }

        /// [`Self::deliver_and_retry_with`], running `between` after the first
        /// write and before the loop is spawned.
        async fn deliver_then_retry(
            &self,
            schedule: &str,
            retype: RetypePolicy,
            between: impl FnOnce(&Self),
        ) -> (tokio::task::JoinHandle<RetryEnd>, Arc<RedeliveryCounts>) {
            let event_rx = self.tx.subscribe();
            let armed =
                self.registry
                    .pending_deliveries()
                    .arm(&self.pane, ID, &self.agent, None, retype);
            // Before the write, as the dispatch samples it.
            let pointer_epoch = self.registry.geometry_changes_of(&self.agent);
            let first = self
                .registry
                .write_and_submit_guarded_detailed(&self.pane, POINTER, &self.agent, || async {
                    true
                })
                .await
                .expect("first write");
            assert_eq!(first, GuardedSendDetail::Outcome(GuardedSend::Applied));
            crate::state::settle_one_shot_payload_record(
                &self.registry,
                &self.pane,
                POINTER,
                Some(GuardedSend::Applied),
            );
            between(self);
            let redeliveries = Arc::new(RedeliveryCounts::default());
            let handle = spawn(DeliveryRetry {
                registry: Arc::clone(&self.registry),
                event_rx,
                armed,
                schedule: RetrySchedule::parse(Some(schedule)),
                pane_id: self.pane.clone(),
                worker_agent_id: self.agent.clone(),
                role: "coder".to_string(),
                delivery_id: ID.to_string(),
                pointer: POINTER.to_string(),
                pointer_epoch,
                orchestration: None,
                redeliveries: Arc::clone(&redeliveries),
                silence_report_armed: false,
                done: None,
            });
            (handle, redeliveries)
        }

        fn event(&self, event_type: EventType, agent_id: &str) -> BroadcastMsg {
            BroadcastMsg::Event(
                serde_json::from_value(serde_json::json!({
                    "session_id": "s",
                    "agent_type": "open_code",
                    "event_type": event_type,
                    "timestamp": "2026-09-28T00:00:00Z",
                    "pane_id": self.pane,
                    "agent_id": agent_id,
                }))
                .unwrap(),
            )
        }

        /// Everything the worker has received so far, read through Tokio's
        /// file API so a polling test does not block a runtime worker.
        async fn sink_text(&self) -> String {
            tokio::fs::read_to_string(&self.sink)
                .await
                .unwrap_or_default()
        }

        /// The lines the worker received, once the count has settled.
        async fn received_lines(&self, expected: usize) -> Vec<String> {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let text = self.sink_text().await;
                let lines: Vec<String> = text.lines().map(str::to_string).collect();
                if lines.len() >= expected || tokio::time::Instant::now() >= deadline {
                    return lines;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        fn stop(self) {
            self.registry.shutdown_all();
        }
    }

    async fn end_of(handle: tokio::task::JoinHandle<RetryEnd>) -> RetryEnd {
        tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("the retry loop must end on its own")
            .expect("the retry loop must not panic")
    }

    #[tokio::test]
    async fn retry_loop_retypes_the_pointer_when_the_composer_shows_nothing() {
        let fx = Fixture::start("retry-retype", false).await;
        let (handle, redeliveries) = fx.deliver_and_retry("150,150").await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(
            redeliveries.tally(),
            RedeliveryTally {
                attempts: 2,
                enters: 2,
                retypes: 2
            },
            "what the went-quiet report quotes must be what was written"
        );
        // The first write, then per re-delivery an unanswered Enter (an empty
        // line) and the retyped pointer.
        let lines = fx.received_lines(5).await;
        assert_eq!(
            lines.iter().filter(|l| l.as_str() == POINTER).count(),
            3,
            "the first write plus two re-typed pointers: {lines:?}"
        );
        assert_eq!(
            lines,
            [POINTER, "", POINTER, "", POINTER],
            "each retype must follow an Enter that went unanswered: {lines:?}"
        );
        assert!(!fx.registry.pending_deliveries().is_pending(&fx.pane));
        fx.stop();
    }

    /// Qodo, PR #1414: a resize that lands after the first write began but
    /// before the loop starts still counts. The worker does not echo, so the
    /// pointer never reaches its screen, and on the resize it clears what the
    /// screen did hold: every reading is blank. Blank after a resize is no
    /// evidence the pointer is gone, so every re-delivery is the probe Enter
    /// and the pointer is never typed again. Seen from an epoch sampled after
    /// the resize, the same screen reads as a composer with nothing in it, and
    /// the pointer is retyped.
    #[tokio::test]
    async fn retry_loop_counts_a_resize_between_the_first_write_and_its_start() {
        let Some(fx) = Fixture::start_clearing_on_resize("retry-resize-before-loop").await else {
            eprintln!("SKIP: python3 is not available");
            return;
        };
        let (handle, redeliveries) = fx
            .deliver_then_retry("150,150", RetypePolicy::Allowed, |fx| {
                let (rows, cols) = fx.registry.pty_size_for_pane(&fx.pane).expect("worker");
                fx.registry
                    .resize(&fx.agent, rows + 1, cols + 1)
                    .expect("resize the worker");
            })
            .await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(
            redeliveries.tally(),
            RedeliveryTally {
                attempts: 2,
                enters: 2,
                retypes: 0
            },
            "a screen blanked by a resize after the write began must never be retyped into"
        );
        let lines = fx.received_lines(3).await;
        assert_eq!(
            lines,
            [POINTER, "", ""],
            "the first write, then only the probe Enters: {lines:?}"
        );
        fx.stop();
    }

    /// Issue #1383 audit: a wrapper-hosted worker (#1390's wrap-only Codex)
    /// that took the pointer, cleared it from its screen and reported no turn
    /// gets the probe Enter on every re-delivery and never a second copy — its
    /// silence is no evidence the task did not start.
    #[tokio::test]
    async fn retry_loop_never_retypes_into_a_worker_whose_delivery_cannot_be_confirmed() {
        let fx = Fixture::start("retry-wrapper-host", false).await;
        let (handle, redeliveries) = fx
            .deliver_and_retry_with("150,150", RetypePolicy::Never)
            .await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.attempts(), 2);
        fx.received_lines(3).await;
        // Past the loop's end, so a late copy would be in the sink too.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let lines = fx.received_lines(3).await;
        assert_eq!(
            lines.iter().filter(|l| l.as_str() == POINTER).count(),
            1,
            "only the first write may carry the pointer: {lines:?}"
        );
        assert_eq!(
            lines,
            [POINTER, "", ""],
            "one probe Enter per re-delivery and nothing else: {lines:?}"
        );
        fx.stop();
    }

    /// The Enter-only half survives the policy: a wrapper-hosted worker whose
    /// input box still holds the pointer after the probe gets the second Enter.
    #[tokio::test]
    async fn retry_loop_still_presses_the_second_enter_for_a_worker_that_is_never_retyped() {
        let Some(fx) = Fixture::start_composer("retry-wrapper-composer").await else {
            eprintln!("SKIP: python3 is not available");
            return;
        };
        let (handle, redeliveries) = fx
            .deliver_and_retry_with("150,150", RetypePolicy::Never)
            .await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.attempts(), 2);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let raw = fx.sink_text().await;
        assert_eq!(raw, format!("{POINTER}\r\r\r\r\r"));
        fx.stop();
    }

    #[test]
    fn retype_policy_withholds_the_retype_from_a_wrapper_hosted_worker() {
        assert_eq!(
            RetypePolicy::for_worker(Some(&AgentType::Codex), false),
            RetypePolicy::Never,
            "a declared Codex worker is hosted by the wrapper"
        );
        assert_eq!(
            RetypePolicy::for_worker(Some(&AgentType::ClaudeCode), true),
            RetypePolicy::Never,
            "the launch shape wins over a declared type"
        );
        for agent_type in [
            AgentType::ClaudeCode,
            AgentType::OpenCode,
            AgentType::Pi,
            AgentType::Devin,
        ] {
            assert_eq!(
                RetypePolicy::for_worker(Some(&agent_type), false),
                RetypePolicy::Allowed,
                "{agent_type:?}"
            );
        }
    }

    /// A pointer the worker's input box still holds after the probe — echoed,
    /// with the cursor left on its row (raw mode echoes a CR as a return to
    /// column 0 of the same row) — gets one more Enter, and is never typed
    /// again.
    #[tokio::test]
    async fn retry_loop_presses_enter_twice_on_a_pointer_left_in_the_composer() {
        let Some(fx) = Fixture::start_composer("retry-probe").await else {
            eprintln!("SKIP: python3 is not available");
            return;
        };
        let (handle, redeliveries) = fx.deliver_and_retry("150,150").await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(
            redeliveries.tally(),
            RedeliveryTally {
                attempts: 2,
                enters: 4,
                retypes: 0
            },
            "an Enter-only re-delivery must not be counted as a re-send of the pointer"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let raw = fx.sink_text().await;
        // Issue #1243: per re-delivery, the probe and, with the pointer still
        // in the input box after the grace, one more Enter.
        assert_eq!(
            raw,
            format!("{POINTER}\r\r\r\r\r"),
            "the first write, then two re-deliveries of two Enters each"
        );
        fx.stop();
    }

    /// Auditor M2 / reviewer LOW-1: a pointer that shows only in the transcript
    /// — a cooked terminal submitted it and left the cursor on the empty line
    /// below — gets the probe Enter and nothing more: no second Enter into the
    /// input box below it, and no second copy.
    #[tokio::test]
    async fn retry_loop_presses_no_second_enter_when_the_pointer_is_only_in_history() {
        let fx = Fixture::start("retry-history", true).await;
        let (handle, redeliveries) = fx.deliver_and_retry("150,150").await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.attempts(), 2);
        // Past the loop's end, so a late fourth line would be in the sink too.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let lines = fx.received_lines(3).await;
        assert_eq!(
            lines,
            [POINTER, "", ""],
            "one probe Enter per re-delivery, no second Enter and no copy: {lines:?}"
        );
        fx.stop();
    }

    /// PR #1414 review (Qodo): the first probe sees the pointer submitted, then
    /// the worker's output clears the screen. The second re-delivery's readings
    /// are `Absent`, but the first reading is latched: that re-delivery presses
    /// its probe Enter and never retypes the pointer.
    #[tokio::test]
    async fn retry_loop_never_retypes_a_pointer_the_probe_saw_submitted() {
        let fx = Fixture::spawn("retry-history-cleared", |sink| {
            format!(
                "printf READY; IFS= read -r line; printf '%s\\n' \"$line\" > '{s}'; \
                 IFS= read -r line; printf '%s\\n' \"$line\" >> '{s}'; \
                 printf '\\033[2J\\033[3J\\033[H'; exec cat >> '{s}'",
                s = sink.display()
            )
        })
        .await;
        // The second re-delivery comes a second after the first, and its
        // retype step half a second after its probe, well after the clear.
        let (handle, redeliveries) = fx.deliver_and_retry("200,1000").await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.attempts(), 2);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let lines = fx.received_lines(3).await;
        assert_eq!(
            lines,
            [POINTER, "", ""],
            "one probe Enter per re-delivery and never a second copy: {lines:?}"
        );
        fx.stop();
    }

    /// PR #1414 review (Qodo): a probe that saw the pointer in the input box
    /// never leads to a full retype in that attempt, even when the screen has
    /// been cleared by the time the retype step reads it.
    #[tokio::test]
    async fn retry_loop_never_retypes_a_pointer_the_probe_saw_in_the_composer() {
        if !python3_available().await {
            eprintln!("SKIP: python3 is not available");
            return;
        }
        // `COMPOSER`, except that the probe's Enter (the second CR) clears the
        // screen instead of being ignored.
        const CLEARING_COMPOSER: &str = r#"import os
import sys
import tty

tty.setraw(0)
sink = open(sys.argv[1], 'ab', buffering=0)
os.write(1, b'READY')
crs = 0
while chunk := os.read(0, 4096):
    sink.write(chunk)
    crs += chunk.count(b'\r')
    if crs == 2:
        os.write(1, b'\x1b[2J\x1b[3J\x1b[H')
        crs = 3
    else:
        os.write(1, chunk.replace(b'\r', b'').replace(b'\n', b''))
"#;
        let fx = Fixture::spawn_python("retry-composer-cleared", CLEARING_COMPOSER).await;
        let (handle, redeliveries) = fx.deliver_and_retry("1000").await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.attempts(), 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let raw = fx.sink_text().await;
        assert_eq!(
            raw.matches(ID).count(),
            1,
            "a pointer the probe saw in the input box must not be typed again: {raw:?}"
        );
        fx.stop();
    }

    /// Audit H1: a composer that holds the pointer while the screen does not
    /// show its id — no echo, and the first CR not taken as submit. The re-
    /// delivery must press Enter first, and when that starts a turn it must
    /// never type a second copy of the pointer behind the first.
    #[tokio::test]
    async fn retry_loop_probes_before_retyping_a_pointer_the_screen_does_not_show() {
        // Raw mode: every byte, CRs included, reaches the sink as typed, and
        // nothing is echoed to the screen.
        let fx = Fixture::start_with("retry-hidden", "stty raw -echo").await;
        let (handle, redeliveries) = fx.deliver_and_retry("1000").await;
        // Play the agent: its composer ignored the first CR and kept the
        // pointer; the next Enter submits it, and the turn is reported.
        let submitted = format!("{POINTER}\r\r");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let raw = fx.sink_text().await;
            if raw.contains(&submitted) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the retry never pressed Enter on the retained pointer: {raw:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fx.tx
            .send(fx.event(EventType::Thinking, &fx.agent))
            .unwrap();
        assert_eq!(end_of(handle).await, RetryEnd::Received);
        assert_eq!(redeliveries.attempts(), 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let raw = fx.sink_text().await;
        assert_eq!(
            raw.matches(ID).count(),
            1,
            "a pointer held in the composer must not be typed a second time: {raw:?}"
        );
        fx.stop();
    }

    /// Qodo round 7 on PR #1414: a probe held up on the pane's writer past its
    /// grace must still give the worker the whole grace after its Enter before
    /// the pointer is retyped. Measured from the start of the attempt, the grace
    /// had already run out when the Enter went out, and the retype followed it
    /// at once.
    #[tokio::test]
    async fn retry_loop_gives_the_whole_grace_after_a_delayed_probe_before_retyping() {
        // Raw mode: every byte, CRs included, reaches the sink as typed, and
        // nothing is echoed, so the screen never shows the pointer.
        let fx = Fixture::start_with("retry-grace", "stty raw -echo").await;
        let (handle, redeliveries) = fx.deliver_and_retry("400,400").await;
        let grace = probe_grace(Duration::from_millis(400));
        let first = format!("{POINTER}\r");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !fx.sink_text().await.contains(&first) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "precondition: the first write never landed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Past the 400 ms wait and past the grace: the probe is queued behind
        // the held writer.
        let writer = fx.registry.hold_pane_writer_for_test(&fx.pane).await;
        tokio::time::sleep(Duration::from_millis(900)).await;
        drop(writer);
        let probed = format!("{POINTER}\r\r");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut enter_seen = None;
        let retyped_at = loop {
            let raw = fx.sink_text().await;
            let now = tokio::time::Instant::now();
            if raw.matches(ID).count() >= 2 {
                break now;
            }
            if enter_seen.is_none() && raw.contains(&probed) {
                enter_seen = Some(now);
            }
            assert!(now < deadline, "the pointer was never retyped: {raw:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let enter_seen = enter_seen.expect("the probe's Enter was seen before the retype");
        // The poll sees each write at most one poll late; the slack covers the
        // Enter being seen late, which is the only way it shrinks this gap.
        assert!(
            retyped_at - enter_seen >= grace - Duration::from_millis(50),
            "the pointer was retyped {:?} after the probe's Enter, inside the {grace:?} grace",
            retyped_at - enter_seen
        );
        fx.tx
            .send(fx.event(EventType::Thinking, &fx.agent))
            .unwrap();
        assert_eq!(end_of(handle).await, RetryEnd::Received);
        assert_eq!(redeliveries.attempts(), 1);
        fx.stop();
    }

    /// Audit M4: an ack that lands while a re-delivery is parked on the pane's
    /// writer must stop it there. Nothing may be typed after the ack.
    #[tokio::test]
    async fn retry_loop_writes_nothing_after_an_ack_that_lands_while_it_waits_on_the_writer() {
        let fx = Fixture::start("retry-ack-writer", false).await;
        let (handle, redeliveries) = fx.deliver_and_retry("150").await;
        let lines = fx.received_lines(1).await;
        assert_eq!(lines, [POINTER], "precondition: the first write landed");
        let writer = fx.registry.hold_pane_writer_for_test(&fx.pane).await;
        // Past the 150 ms wait: the probe is now queued behind the held writer.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            matches!(
                fx.registry
                    .pending_deliveries()
                    .acknowledge(&fx.pane, ID, Some(&fx.agent)),
                AckOutcome::Stopped { .. }
            ),
            "the ack must match the pending delivery"
        );
        drop(writer);
        assert_eq!(end_of(handle).await, RetryEnd::Cancelled);
        assert_eq!(redeliveries.attempts(), 0);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            fx.sink_text().await,
            format!("{POINTER}\n"),
            "nothing may reach the worker after its ack"
        );
        fx.stop();
    }

    /// Issue #544 × #1383: a bare Enter submits whatever the worker's
    /// composer holds, so no re-delivery presses one while the pane has an
    /// unsent draft — here a bracketed paste left open, whose draft survives
    /// the first write because that write's CR lands inside the paste. Nobody
    /// has typed since the deck's last write, so the older "someone typed"
    /// check does not catch it. The skipped attempts still count toward the
    /// bound, so the loop ends on schedule.
    #[tokio::test]
    async fn retry_loop_presses_no_enter_while_a_draft_is_pending() {
        let fx = Fixture::start_with("retry-draft-probe", "stty raw -echo").await;
        let (handle, redeliveries) = fx
            .deliver_then_retry("150,150", RetypePolicy::Allowed, |fx| {
                fx.registry.note_deck_bytes_for_test(&fx.pane, b"\x1b[200~");
            })
            .await;
        assert!(fx.registry.draft_pending(&fx.pane), "precondition: a draft");
        assert!(
            !fx.registry.user_typed_since_automatic_write(&fx.pane),
            "precondition: nobody typed since the first write"
        );
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.tally(), RedeliveryTally::default());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            fx.sink_text().await,
            format!("{POINTER}\r"),
            "no Enter may reach a pane holding a draft"
        );
        fx.stop();
    }

    /// The same, for a draft that appears after the attempt's first check while
    /// its Enter waits for the pane's writer: the check under the writer is the
    /// one that holds.
    #[tokio::test]
    async fn retry_loop_presses_no_enter_over_a_draft_that_appears_while_it_waits_on_the_writer() {
        let fx = Fixture::start_with("retry-draft-writer", "stty raw -echo").await;
        let (handle, redeliveries) = fx.deliver_and_retry("150").await;
        let first = format!("{POINTER}\r");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while fx.sink_text().await != first {
            assert!(
                tokio::time::Instant::now() < deadline,
                "precondition: the first write never landed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let writer = fx.registry.hold_pane_writer_for_test(&fx.pane).await;
        // Past the 150 ms wait: the probe has passed its first check and is
        // queued behind the held writer.
        tokio::time::sleep(Duration::from_millis(400)).await;
        fx.registry.note_deck_bytes_for_test(&fx.pane, b"\x1b[200~");
        drop(writer);
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.tally(), RedeliveryTally::default());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            fx.sink_text().await,
            first,
            "no Enter may reach a pane holding a draft"
        );
        fx.stop();
    }

    /// Deliver the pointer, let the probe's Enter go unanswered, and park the
    /// retype that follows on a draft that appears after its attempt began:
    /// the writer is held through the probe's grace, so the retype passes its
    /// checks and queues on it with no draft pending, and the draft is opened
    /// before the writer is let go. Returns the loop and what the worker has
    /// received so far, once the retype has set the dispatch lock down.
    async fn park_a_retype_behind_a_draft(
        fx: &Fixture,
    ) -> (
        tokio::task::JoinHandle<RetryEnd>,
        Arc<RedeliveryCounts>,
        String,
    ) {
        assert!(
            fx.registry.draft_defer_cap() >= Duration::from_secs(10),
            "precondition: the draft deferral is on"
        );
        let (handle, redeliveries) = fx.deliver_and_retry("2000").await;
        // The probe's Enter goes in at 2 s; the retype follows a 1 s grace.
        let probed = format!("{POINTER}\r\r");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while fx.sink_text().await != probed {
            assert!(
                tokio::time::Instant::now() < deadline,
                "precondition: the probe never landed: {:?}",
                fx.sink_text().await
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let writer = fx.registry.hold_pane_writer_for_test(&fx.pane).await;
        tokio::time::sleep(probe_grace(Duration::from_millis(2000)) + Duration::from_millis(500))
            .await;
        fx.registry.note_deck_bytes_for_test(&fx.pane, b"\x1b[200~");
        drop(writer);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while fx.registry.pane_dispatch_lock(&fx.pane).try_lock().is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a retype waiting for the draft must set the dispatch lock down"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (handle, redeliveries, probed)
    }

    /// Issue #544 × #1383: a retype goes through #1398's deferring entry, so a
    /// draft that appears after its attempt began holds it, with the dispatch
    /// lock set down as #1398 does for the first write, and the pointer is
    /// typed once the draft ends — not skipped, and not typed onto it.
    #[tokio::test]
    async fn retry_loop_retype_waits_for_a_draft_and_then_writes() {
        let fx = Fixture::start_with("retry-draft-retype-wait", "stty raw -echo").await;
        let (handle, redeliveries, probed) = park_a_retype_behind_a_draft(&fx).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            fx.sink_text().await,
            probed,
            "nothing may be typed while the draft is pending"
        );
        fx.registry.note_deck_bytes_for_test(&fx.pane, b"\x1b[201~");
        let retyped = format!("{probed}{POINTER}\r");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while fx.sink_text().await != retyped {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the retype must follow the draft's end: {:?}",
                fx.sink_text().await
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fx.tx
            .send(fx.event(EventType::Thinking, &fx.agent))
            .unwrap();
        assert_eq!(end_of(handle).await, RetryEnd::Received);
        assert_eq!(redeliveries.tally().retypes, 1);
        fx.stop();
    }

    /// An ack that lands while a retype waits for a draft leaves nothing
    /// written once the draft ends: the delivery is re-checked under the
    /// writer after the wait. The draft ends with no keystroke on record, so
    /// that re-check is the only thing that can stop the write.
    #[tokio::test]
    async fn retry_loop_deferred_retype_writes_nothing_after_an_ack_during_the_wait() {
        let fx = Fixture::start_with("retry-draft-retype-ack", "stty raw -echo").await;
        let (handle, redeliveries, probed) = park_a_retype_behind_a_draft(&fx).await;
        assert!(
            matches!(
                fx.registry
                    .pending_deliveries()
                    .acknowledge(&fx.pane, ID, Some(&fx.agent)),
                AckOutcome::Stopped { .. }
            ),
            "the ack must match the pending delivery"
        );
        fx.registry.note_deck_bytes_for_test(&fx.pane, b"\x1b[201~");
        assert!(!fx.registry.draft_pending(&fx.pane));
        assert!(!fx.registry.user_typed_since_automatic_write(&fx.pane));
        assert_eq!(end_of(handle).await, RetryEnd::Cancelled);
        assert_eq!(redeliveries.tally().retypes, 0);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            fx.sink_text().await,
            probed,
            "nothing may be typed after the ack"
        );
        fx.stop();
    }

    #[tokio::test]
    async fn retry_loop_stops_on_a_turn_from_the_worker() {
        let fx = Fixture::start("retry-proof", false).await;
        let (handle, redeliveries) = fx.deliver_and_retry("2000").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        fx.tx
            .send(fx.event(EventType::Thinking, &fx.agent))
            .unwrap();
        assert_eq!(end_of(handle).await, RetryEnd::Received);
        assert_eq!(redeliveries.attempts(), 0);
        assert!(!fx.registry.pending_deliveries().is_pending(&fx.pane));
        fx.stop();
    }

    #[tokio::test]
    async fn retry_loop_ignores_a_turn_from_another_agent_on_the_pane() {
        let fx = Fixture::start("retry-stranger", false).await;
        let (handle, redeliveries) = fx.deliver_and_retry("300").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        fx.tx
            .send(fx.event(EventType::Thinking, "some-other-agent"))
            .unwrap();
        fx.tx.send(fx.event(EventType::Idle, &fx.agent)).unwrap();
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.attempts(), 1);
        fx.stop();
    }

    #[tokio::test]
    async fn retry_loop_stops_on_ack() {
        let fx = Fixture::start("retry-ack", false).await;
        let (handle, redeliveries) = fx.deliver_and_retry("2000").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(
            fx.registry
                .pending_deliveries()
                .acknowledge(&fx.pane, ID, Some(&fx.agent)),
            AckOutcome::Stopped { .. }
        ));
        assert_eq!(end_of(handle).await, RetryEnd::Cancelled);
        assert_eq!(redeliveries.attempts(), 0);
        fx.stop();
    }

    #[tokio::test]
    async fn retry_loop_stops_when_the_pane_closes() {
        let fx = Fixture::start("retry-close", false).await;
        let (handle, _) = fx.deliver_and_retry("2000").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        fx.registry.begin_pane_close(&fx.pane);
        assert_eq!(end_of(handle).await, RetryEnd::PaneClosed);
        assert!(!fx.registry.pending_deliveries().is_pending(&fx.pane));
        fx.stop();
    }

    #[tokio::test]
    async fn retry_loop_skips_a_re_delivery_after_user_input() {
        let fx = Fixture::start("retry-user", false).await;
        let (handle, redeliveries) = fx.deliver_and_retry("200").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        fx.registry.note_user_input(&fx.pane);
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.attempts(), 0);
        let lines = fx.received_lines(1).await;
        assert_eq!(
            lines.iter().filter(|l| l.as_str() == POINTER).count(),
            1,
            "nothing may be typed onto a human's draft: {lines:?}"
        );
        fx.stop();
    }

    #[tokio::test]
    async fn retry_loop_stops_when_a_newer_delegation_supersedes_it() {
        let fx = Fixture::start("retry-supersede", false).await;
        let (handle, redeliveries) = fx.deliver_and_retry("2000").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(fx.registry.pending_deliveries().supersede(&fx.pane));
        assert_eq!(end_of(handle).await, RetryEnd::Cancelled);
        assert_eq!(redeliveries.attempts(), 0);
        fx.stop();
    }
}
