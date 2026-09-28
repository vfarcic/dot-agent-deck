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
//! Each re-delivery first reads the worker's screen. When the delivery id is
//! visible there, the pointer is most likely parked unsubmitted in the agent's
//! composer (issue #1243's shape), so the re-delivery is a submit-only probe (a
//! bare Enter) rather than a second copy of the text. Otherwise the pointer is
//! typed again with the same id, and the task file tells the worker that a
//! repeated pointer is the same task.
//!
//! What this is not: a receipt for the work. An ack claims only that the worker
//! read its task file.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, oneshot};
use tracing::{info, warn};

use crate::agent_pty::{AgentPtyRegistry, GuardedSend, GuardedSendDetail};
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

/// When a re-delivery happens, as the silences that precede each one.
///
/// Entry *i* is how long the worker may stay silent after write *i* before
/// re-delivery *i + 1*. After the last re-delivery the loop waits the last
/// entry once more before declaring the delivery exhausted, so N entries give N
/// re-deliveries and a total span of `sum + last`.
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
    /// * anything else: the default, with one `warn!`.
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
            let Ok(ms) = entry.trim().parse::<u128>() else {
                warn!(
                    value = %crate::config_validation::escape_id_for_log(raw),
                    "{DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS} is not a comma-separated list \
                     of milliseconds; using the default schedule"
                );
                return Self::default_schedule();
            };
            if waits.len() == MAX_RETRY_ENTRIES {
                warn!(
                    max_entries = MAX_RETRY_ENTRIES,
                    "{DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS} has more entries than are read; \
                     the rest are ignored"
                );
                break;
            }
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
    /// counting any postponement: every wait, plus the last one once more.
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
/// daemon rejects the unknown subcommand, and the task must not stall on that.
pub fn task_file_ack_header(delivery_id: &str) -> String {
    let bin = crate::platform::paths::binary_name();
    format!(
        "## First: acknowledge this task\n\n\
         Run this command via Bash before anything else:\n\n\
         ```bash\n\
         {bin} ack {delivery_id}\n\
         ```\n\n\
         It tells the deck this task reached you, so it stops re-sending the pointer. If the \
         command fails or is not recognised, ignore that and carry on with the task. If you see \
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
pub fn classify_event(event: &AgentEvent) -> EventVerdict {
    match event.event_type {
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
        | EventType::Unknown => EventVerdict::Ignore,
    }
}

/// What the worker's screen shows about this delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Composer {
    /// The delivery id is on screen: the pointer is most likely sitting in the
    /// composer, unsubmitted. Re-deliver with a submit-only probe.
    PointerVisible,
    /// The screen shows nothing at all, and the PTY was resized since the
    /// pointer was typed. A resize drops the scrollback ring the screen is read
    /// from, so a blank screen then says nothing about the composer — an agent
    /// that has not repainted still holds whatever it held. Treated like
    /// [`Self::PointerVisible`]: a submit-only probe, never a second copy.
    Unreadable,
    /// No trace of it: re-type the pointer.
    Absent,
}

/// Classify the worker's visible screen for `delivery_id`.
///
/// Searches each row, and then every row concatenated with everything but ASCII
/// letters, digits and `-` stripped out, so an id split by a line wrap — with a
/// composer's border glyphs and padding around the split — is still found.
///
/// **Biased toward [`Composer::PointerVisible`] on purpose.** A false positive,
/// for example a submitted pointer still visible in the transcript, costs one
/// Enter on an empty composer, which the supported agents' TUIs ignore. A false
/// negative costs a second copy of the pointer in the composer.
///
/// `resized_since_write` is whether the PTY geometry moved since the pointer
/// was last typed; see [`Composer::Unreadable`].
pub fn classify_composer(
    screen_rows: &[String],
    delivery_id: &str,
    resized_since_write: bool,
) -> Composer {
    if resized_since_write && screen_rows.iter().all(|row| row.trim().is_empty()) {
        return Composer::Unreadable;
    }
    if delivery_id.is_empty() {
        return Composer::Absent;
    }
    if screen_rows.iter().any(|row| row.contains(delivery_id)) {
        return Composer::PointerVisible;
    }
    let squeezed: String = screen_rows
        .iter()
        .flat_map(|row| row.chars())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if squeezed.contains(delivery_id) {
        Composer::PointerVisible
    } else {
        Composer::Absent
    }
}

/// Handed back by [`PendingDeliveries::arm`] to the loop that owns the record.
#[derive(Debug)]
pub struct ArmedDelivery {
    /// This record's generation, for [`PendingDeliveries::finish`] and
    /// [`PendingDeliveries::is_current`].
    pub seq: u64,
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
    /// Nothing pending matches: a delivery that already ended (a turn began,
    /// the schedule ran out, it was superseded), one that was never armed (the
    /// retry is off, or a Pi seed delivery), or another generation's id. A
    /// no-op.
    Unknown,
}

struct PendingDelivery {
    seq: u64,
    delivery_id: String,
    worker_agent_id: String,
    silence_seq: Option<u64>,
    /// Dropped with the record, which resolves the loop's cancel receiver.
    _cancel: oneshot::Sender<()>,
}

#[derive(Default)]
struct PendingInner {
    next_seq: u64,
    records: HashMap<String, PendingDelivery>,
    /// The last delivery id acknowledged per pane, so a repeat ack is told
    /// apart from one that never matched anything.
    last_acked: HashMap<String, String>,
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
    pub fn arm(
        &self,
        pane_id: &str,
        delivery_id: &str,
        worker_agent_id: &str,
        silence_seq: Option<u64>,
    ) -> ArmedDelivery {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let mut inner = self.inner.lock().unwrap();
        inner.next_seq += 1;
        let seq = inner.next_seq;
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
            cancel: cancel_rx,
        }
    }

    /// Record the silent-worker watch armed for the pane's current delivery, so
    /// an ack can cancel it too. A no-op unless `seq` is still current.
    pub fn attach_silence_seq(&self, pane_id: &str, seq: u64, silence_seq: Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(record) = inner.records.get_mut(pane_id)
            && record.seq == seq
        {
            record.silence_seq = silence_seq;
        }
    }

    /// A newer delegation to `pane_id` begins: cancel whatever is pending.
    pub fn supersede(&self, pane_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.last_acked.remove(pane_id);
        inner.records.remove(pane_id).is_some()
    }

    /// Acknowledge `delivery_id` on `pane_id`.
    ///
    /// Matches only the pane's CURRENT delivery, and, when the caller presents
    /// an agent id, only that delivery's worker — an older generation's ack
    /// cannot stop a newer generation's loop. Idempotent.
    pub fn acknowledge(
        &self,
        pane_id: &str,
        delivery_id: &str,
        agent_id: Option<&str>,
    ) -> AckOutcome {
        let mut inner = self.inner.lock().unwrap();
        let matches = inner.records.get(pane_id).is_some_and(|record| {
            record.delivery_id == delivery_id
                && agent_id.is_none_or(|agent| agent == record.worker_agent_id)
        });
        if matches {
            let record = inner
                .records
                .remove(pane_id)
                .expect("record present under the same lock");
            inner
                .last_acked
                .insert(pane_id.to_string(), delivery_id.to_string());
            return AckOutcome::Stopped {
                silence_seq: record.silence_seq,
            };
        }
        if inner
            .last_acked
            .get(pane_id)
            .is_some_and(|last| last == delivery_id)
        {
            return AckOutcome::AlreadyAcknowledged;
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
    pub orchestration: Option<OrchestrationIdentity>,
    /// Re-deliveries that were written, shared with the silent-worker report so
    /// it can say how many there were.
    pub redeliveries: Arc<AtomicU32>,
    /// Whether a silent-worker report will cover exhaustion. When it will not,
    /// the loop logs exhaustion itself.
    pub silence_report_armed: bool,
}

/// Spawn the retry loop onto its own task. The dispatch returns as it does
/// without a retry, so the loop never holds the pane's dispatch lock between
/// attempts and a superseding delegate does not queue behind it.
pub(crate) fn spawn(retry: DeliveryRetry) -> tokio::task::JoinHandle<RetryEnd> {
    tokio::spawn(run(retry))
}

/// The loop body. See the module docs.
pub(crate) async fn run(retry: DeliveryRetry) -> RetryEnd {
    let DeliveryRetry {
        registry,
        mut event_rx,
        armed,
        schedule,
        pane_id,
        worker_agent_id,
        role,
        delivery_id,
        pointer,
        orchestration,
        redeliveries,
        silence_report_armed,
    } = retry;
    let ArmedDelivery { seq, mut cancel } = armed;
    let mut closing = registry.pane_close_signal(&pane_id);
    let mut exited = registry.agent_exit_signal(&worker_agent_id);
    let waits = schedule.waits().to_vec();
    let total_attempts = waits.len();
    let mut last_classification: Option<Composer> = None;
    // The PTY geometry epoch the pointer's bytes were last typed at. A resize
    // clears the scrollback the composer is read from (PRD #104 M3), so after one
    // a blank screen is no evidence the pointer is gone.
    let mut pointer_epoch = registry.geometry_changes_of(&worker_agent_id);

    let end = 'outer: {
        // One more wait than there are re-deliveries: the last is the silence
        // after the final re-delivery, before exhaustion is declared.
        for index in 0..=total_attempts {
            let wait = waits
                .get(index)
                .or(waits.last())
                .copied()
                .unwrap_or_default();
            let mut due = tokio::time::Instant::now() + wait;
            let mut postponed = false;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancel => break 'outer RetryEnd::Cancelled,
                    _ = &mut closing => break 'outer RetryEnd::PaneClosed,
                    _ = &mut exited => break 'outer RetryEnd::AgentExited,
                    msg = event_rx.recv() => match msg {
                        Ok(BroadcastMsg::Event(event)) => {
                            if event.pane_id.as_deref() != Some(pane_id.as_str())
                                || event.agent_id.as_deref() != Some(worker_agent_id.as_str())
                            {
                                continue;
                            }
                            match classify_event(&event) {
                                EventVerdict::Received => break 'outer RetryEnd::Received,
                                EventVerdict::Blocked => break 'outer RetryEnd::Blocked,
                                EventVerdict::Postpone if !postponed => {
                                    postponed = true;
                                    let interval = crate::state::delegate_readiness_buffer()
                                        .max(crate::prompt_delivery::REARM_READINESS_BUFFER);
                                    due = due.max(tokio::time::Instant::now() + interval);
                                    info!(
                                        pane_id = %pane_id,
                                        role = %role,
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
                        // wildcarded so a future variant is classified on
                        // purpose.
                        Ok(
                            BroadcastMsg::OrchestrationSurface(_)
                            | BroadcastMsg::WorktreeKept(_)
                            | BroadcastMsg::Unknown,
                        ) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => break 'outer RetryEnd::Lagged,
                        Err(broadcast::error::RecvError::Closed) => break 'outer RetryEnd::BusClosed,
                    },
                    _ = tokio::time::sleep_until(due) => break,
                }
            }
            if index == total_attempts {
                break 'outer RetryEnd::Exhausted;
            }
            let attempt = index + 1;
            match redeliver(
                &registry,
                seq,
                &pane_id,
                &worker_agent_id,
                &role,
                &delivery_id,
                &pointer,
                orchestration.as_ref(),
                attempt,
                total_attempts,
                &mut pointer_epoch,
            )
            .await
            {
                Attempt::Written(composer) => {
                    last_classification = Some(composer);
                    redeliveries.fetch_add(1, Ordering::SeqCst);
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
            pane_id = %pane_id,
            role = %role,
            delivery_id = %delivery_id,
            redeliveries = redeliveries.load(Ordering::SeqCst),
            last_classification = ?last_classification,
            "delegate retry: every scheduled re-delivery went out and the worker produced no \
             event and no ack; the task may never have reached it. The worker is not respawned"
        ),
        RetryEnd::Exhausted => info!(
            pane_id = %pane_id,
            role = %role,
            delivery_id = %delivery_id,
            redeliveries = redeliveries.load(Ordering::SeqCst),
            last_classification = ?last_classification,
            "delegate retry: schedule exhausted; the silent-worker report covers it"
        ),
        other => info!(
            pane_id = %pane_id,
            role = %role,
            delivery_id = %delivery_id,
            redeliveries = redeliveries.load(Ordering::SeqCst),
            end = ?other,
            "delegate retry: stopped"
        ),
    }
    end
}

enum Attempt {
    Written(Composer),
    Skipped,
    Stop(RetryEnd),
}

#[allow(clippy::too_many_arguments)]
async fn redeliver(
    registry: &Arc<AgentPtyRegistry>,
    seq: u64,
    pane_id: &str,
    worker_agent_id: &str,
    role: &str,
    delivery_id: &str,
    pointer: &str,
    orchestration: Option<&OrchestrationIdentity>,
    attempt: usize,
    total_attempts: usize,
    pointer_epoch: &mut Option<u64>,
) -> Attempt {
    // The dispatch lock, then the generation check: a newer delegation that took
    // the lock first has already superseded this record, so an old pointer can
    // never land after a new delegation starts.
    let dispatch_mutex = registry.pane_dispatch_lock(pane_id);
    let _dispatch_guard = dispatch_mutex.lock().await;
    if !registry.pending_deliveries().is_current(pane_id, seq) {
        return Attempt::Stop(RetryEnd::Superseded);
    }
    // Never type onto a human's draft. The attempt still counts toward the
    // bound.
    if registry.user_typed_since_automatic_write(pane_id) {
        info!(
            pane_id = %pane_id,
            role = %role,
            delivery_id = %delivery_id,
            attempt,
            total_attempts,
            "delegate retry: someone has typed into the worker pane since the pointer went in; \
             skipping this re-delivery"
        );
        return Attempt::Skipped;
    }
    let composer = match registry.snapshot_with_pty_size(worker_agent_id) {
        Ok((bytes, rows, cols)) => classify_composer(
            &crate::pane_screen_text::visible_tail_lines(&bytes, rows, cols, rows as usize),
            delivery_id,
            registry.geometry_changes_of(worker_agent_id) != *pointer_epoch,
        ),
        Err(_) => return Attempt::Stop(RetryEnd::AgentExited),
    };
    let text = match composer {
        Composer::PointerVisible | Composer::Unreadable => "",
        Composer::Absent => pointer,
    };
    let revalidate_registry = Arc::clone(registry);
    let revalidate_pane = pane_id.to_string();
    let expected_orchestration = orchestration.cloned();
    let outcome = registry
        .write_and_submit_guarded_detailed(pane_id, text, worker_agent_id, || async move {
            if revalidate_registry.is_pane_closing(&revalidate_pane) {
                return false;
            }
            crate::state::orchestration_still_matches(
                expected_orchestration.as_ref(),
                revalidate_registry
                    .pane_orchestration(&revalidate_pane)
                    .as_ref(),
            )
        })
        .await;
    match outcome {
        Ok(GuardedSendDetail::Outcome(GuardedSend::Applied)) => {
            if !text.is_empty() {
                crate::state::settle_one_shot_payload_record(
                    registry,
                    pane_id,
                    text,
                    Some(GuardedSend::Applied),
                );
                // The pointer's bytes are on screen at the current geometry now.
                *pointer_epoch = registry.geometry_changes_of(worker_agent_id);
            }
            info!(
                pane_id = %pane_id,
                role = %role,
                delivery_id = %delivery_id,
                attempt,
                total_attempts,
                classification = ?composer,
                "delegate retry: no proof the worker received its task pointer; {}",
                match composer {
                    Composer::PointerVisible =>
                        "the pointer is visible on its screen, so pressed Enter instead of retyping it",
                    Composer::Unreadable =>
                        "its screen was cleared by a resize since the pointer went in, so pressed \
                         Enter rather than risk typing a second copy",
                    Composer::Absent => "re-typed the pointer into the same process",
                }
            );
            Attempt::Written(composer)
        }
        // A partial write: a prefix may be in the box, and a further attempt
        // could submit it. Stop, and leave the payload record standing (#715).
        Ok(GuardedSendDetail::Outcome(GuardedSend::Ambiguous)) => {
            warn!(
                pane_id = %pane_id,
                role = %role,
                delivery_id = %delivery_id,
                attempt,
                "delegate retry: the re-delivery was ambiguous (partial write); no further \
                 attempts"
            );
            Attempt::Stop(RetryEnd::WriteStopped)
        }
        Ok(other) => {
            info!(
                pane_id = %pane_id,
                role = %role,
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
                pane_id = %pane_id,
                role = %role,
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
        assert!(header.contains("If the command fails or is not recognised"));
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
            EventType::Unknown,
        ] {
            let e = event(event_type);
            assert_eq!(
                classify_event(&e) == EventVerdict::Received,
                crate::state::worker_event_proves_delivery(&e),
                "{:?}",
                e.event_type
            );
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

    #[test]
    fn classify_composer_finds_the_id_on_a_row() {
        let screen = rows(&[
            "welcome",
            "> Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21]",
        ]);
        assert_eq!(
            classify_composer(&screen, "d-7f3a9c21", false),
            Composer::PointerVisible
        );
    }

    #[test]
    fn classify_composer_finds_an_id_split_across_rows() {
        let screen = rows(&["> Read … for your task. [delivery d-7f3a", "9c21]"]);
        assert_eq!(
            classify_composer(&screen, "d-7f3a9c21", false),
            Composer::PointerVisible
        );
    }

    #[test]
    fn classify_composer_finds_an_id_behind_border_glyphs() {
        let screen = rows(&[
            "┃ > Read .dot-agent-deck/worker-task-coder.md [delivery d-7f3a   ┃",
            "┃ 9c21]                                                          ┃",
        ]);
        assert_eq!(
            classify_composer(&screen, "d-7f3a9c21", false),
            Composer::PointerVisible
        );
    }

    #[test]
    fn classify_composer_ignores_another_delivery_and_a_blank_screen() {
        let screen = rows(&["> Read … for your task. [delivery d-00000000]"]);
        assert_eq!(
            classify_composer(&screen, "d-7f3a9c21", false),
            Composer::Absent
        );
        assert_eq!(
            classify_composer(&[], "d-7f3a9c21", false),
            Composer::Absent
        );
        assert_eq!(
            classify_composer(&rows(&["", "   "]), "d-7f3a9c21", false),
            Composer::Absent
        );
    }

    #[test]
    fn classify_composer_a_screen_blanked_by_a_resize_is_unreadable() {
        assert_eq!(
            classify_composer(&[], "d-7f3a9c21", true),
            Composer::Unreadable
        );
        // A screen the agent has repainted since is read as usual.
        assert_eq!(
            classify_composer(&rows(&["> "]), "d-7f3a9c21", true),
            Composer::Absent
        );
        assert_eq!(
            classify_composer(&rows(&["> [delivery d-7f3a9c21]"]), "d-7f3a9c21", true),
            Composer::PointerVisible
        );
    }

    #[test]
    fn pending_deliveries_ack_stops_the_loop_and_is_idempotent() {
        let store = PendingDeliveries::default();
        let mut armed = store.arm("p1", "d-11111111", "a1", Some(7));
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

    #[test]
    fn pending_deliveries_unknown_id_leaves_the_record_armed() {
        let store = PendingDeliveries::default();
        let armed = store.arm("p1", "d-11111111", "a1", None);
        assert_eq!(
            store.acknowledge("p1", "d-22222222", None),
            AckOutcome::Unknown
        );
        assert!(store.is_current("p1", armed.seq));
    }

    #[test]
    fn pending_deliveries_ack_from_another_pane_or_agent_does_not_match() {
        let store = PendingDeliveries::default();
        let armed = store.arm("p1", "d-11111111", "a1", None);
        assert_eq!(
            store.acknowledge("p2", "d-11111111", None),
            AckOutcome::Unknown
        );
        assert_eq!(
            store.acknowledge("p1", "d-11111111", Some("a0")),
            AckOutcome::Unknown,
            "an older generation's ack must not stop this generation's loop"
        );
        assert!(store.is_current("p1", armed.seq));
    }

    #[test]
    fn pending_deliveries_supersede_cancels_the_older_loop() {
        let store = PendingDeliveries::default();
        let mut old = store.arm("p1", "d-11111111", "a1", None);
        assert!(store.supersede("p1"));
        assert!(old.cancel.try_recv().is_err());
        let mut older = store.arm("p1", "d-22222222", "a1", None);
        let newer = store.arm("p1", "d-33333333", "a1", None);
        assert!(older.cancel.try_recv().is_err(), "re-arming replaces");
        assert!(store.is_current("p1", newer.seq));
    }

    #[test]
    fn pending_deliveries_stale_finish_leaves_a_newer_record() {
        let store = PendingDeliveries::default();
        let old = store.arm("p1", "d-11111111", "a1", None);
        let newer = store.arm("p1", "d-22222222", "a1", None);
        assert!(!store.finish("p1", old.seq));
        assert!(store.is_current("p1", newer.seq));
        assert!(store.finish("p1", newer.seq));
        assert!(!store.is_pending("p1"));
    }

    #[test]
    fn pending_deliveries_work_done_cancels() {
        let store = PendingDeliveries::default();
        let mut armed = store.arm("p1", "d-11111111", "a1", None);
        assert!(store.retire_on_work_done("p1"));
        assert!(armed.cancel.try_recv().is_err());
        assert!(!store.retire_on_work_done("p1"));
    }

    #[test]
    fn pending_deliveries_attach_silence_seq_only_to_the_current_record() {
        let store = PendingDeliveries::default();
        let old = store.arm("p1", "d-11111111", "a1", None);
        let newer = store.arm("p1", "d-22222222", "a1", None);
        store.attach_silence_seq("p1", old.seq, Some(3));
        store.attach_silence_seq("p1", newer.seq, Some(4));
        assert_eq!(
            store.acknowledge("p1", "d-22222222", None),
            AckOutcome::Stopped {
                silence_seq: Some(4)
            }
        );
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
            let dir = tempfile::tempdir().unwrap();
            let sink = dir.path().join("sink");
            let stty = if echo { "true" } else { "stty -echo" };
            let command = format!("{stty} && printf READY && exec cat > '{}'", sink.display());
            let registry = Arc::new(AgentPtyRegistry::new());
            let agent = registry
                .spawn_agent(SpawnOptions {
                    command: Some(&command),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn worker stand-in");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !String::from_utf8_lossy(&registry.snapshot(&agent).unwrap_or_default())
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
        ) -> (tokio::task::JoinHandle<RetryEnd>, Arc<AtomicU32>) {
            let event_rx = self.tx.subscribe();
            let armed = self
                .registry
                .pending_deliveries()
                .arm(&self.pane, ID, &self.agent, None);
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
            let redeliveries = Arc::new(AtomicU32::new(0));
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
                orchestration: None,
                redeliveries: Arc::clone(&redeliveries),
                silence_report_armed: false,
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

        /// The lines the worker received, once the count has settled.
        async fn received_lines(&self, expected: usize) -> Vec<String> {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let text = std::fs::read_to_string(&self.sink).unwrap_or_default();
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
        assert_eq!(redeliveries.load(Ordering::SeqCst), 2);
        let lines = fx.received_lines(3).await;
        assert_eq!(
            lines.iter().filter(|l| l.as_str() == POINTER).count(),
            3,
            "the first write plus two re-typed pointers: {lines:?}"
        );
        assert!(!fx.registry.pending_deliveries().is_pending(&fx.pane));
        fx.stop();
    }

    #[tokio::test]
    async fn retry_loop_presses_enter_instead_of_retyping_a_visible_pointer() {
        let fx = Fixture::start("retry-probe", true).await;
        let (handle, redeliveries) = fx.deliver_and_retry("150,150").await;
        assert_eq!(end_of(handle).await, RetryEnd::Exhausted);
        assert_eq!(redeliveries.load(Ordering::SeqCst), 2);
        let lines = fx.received_lines(3).await;
        assert_eq!(
            lines.iter().filter(|l| l.contains(ID)).count(),
            1,
            "a pointer visible on screen must never be typed a second time: {lines:?}"
        );
        assert_eq!(lines.len(), 3, "two submit-only probes: {lines:?}");
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
        assert_eq!(redeliveries.load(Ordering::SeqCst), 0);
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
        assert_eq!(redeliveries.load(Ordering::SeqCst), 1);
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
        assert_eq!(redeliveries.load(Ordering::SeqCst), 0);
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
        assert_eq!(redeliveries.load(Ordering::SeqCst), 0);
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
        assert_eq!(redeliveries.load(Ordering::SeqCst), 0);
        fx.stop();
    }
}
