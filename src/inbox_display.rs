//! PRD #1666 §2.8: what a client shows of an agent's inbox, and the words it
//! uses.
//!
//! The TUI's card and the desktop app's tile say the same thing about the same
//! state (CLAUDE.md rule 22): `✉2 queued` while messages wait, `✉2 waiting to
//! wake: send it a prompt` once they have waited too long for an agent that is
//! not working, `✉ delivered` once the agent has acknowledged one and nothing
//! is pending, and `not delivered` for a message that could not reach it.
//! While the deck holds the doorbell for a reason it names ([`WakeHold`]),
//! the line keeps the count (`✉2`) and the notice row says why. The desktop's
//! copy of the words is `desktop/src/lib/inboxWords.ts`;
//! `tests/fixtures/inbox-display.json` pins both to the same values, so a word
//! changes in all three places at once.

/// Prefixes the card's pending count (`✉2`) and a deck message's prompt line.
pub const GLYPH: &str = "✉";
/// Messages waiting for the agent, taken or not.
pub const QUEUED: &str = "queued";
/// The agent's side acknowledged the message: its content reached the agent.
pub const DELIVERED: &str = "delivered";
/// The message could not reach the agent.
pub const NOT_DELIVERED: &str = "not delivered";
/// The worker reported back on a delegated task.
pub const WORK_DONE: &str = "work-done";
/// PR #1735 fifth review (R5): messages have waited longer than a wake-up
/// takes while the agent was not working, so nothing has woken it for them.
pub const WAKE_STALLED: &str = "waiting to wake";
/// What the user can do about [`WAKE_STALLED`]: a prompt starts a turn, and
/// every agent takes its messages at a turn's start or end.
pub const WAKE_STALLED_HINT: &str = "send it a prompt";
/// PRD #1666 part 3, round 5-wake (plan §5.2): the card's words for
/// [`WakeHold::PossibleDraft`], after the glyph and the pending count.
pub const WAKE_HOLD_POSSIBLE_DRAFT: &str =
    "waiting: there may be an unsent draft in its input. Send or clear it.";
/// The card's words for [`WakeHold::AwaitingPrompt`].
pub const WAKE_HOLD_AWAITING_PROMPT: &str =
    "not yet taken: the agent may be waiting at a prompt. Check its pane.";
/// The card's words for [`WakeHold::PasteOpen`].
pub const WAKE_HOLD_PASTE_OPEN: &str = "waiting: a paste into its input has not finished.";
/// The card's words for [`WakeHold::CannotType`].
pub const WAKE_HOLD_CANNOT_TYPE: &str = "waiting for the agent to fetch it. Send it a prompt.";
/// The card's words for [`WakeHold::NotTaken`].
pub const WAKE_HOLD_NOT_TAKEN: &str =
    "waiting: the agent did not take the deck's Enter. Send it a prompt.";
/// The card's words for [`WakeHold::UncertainInput`] while messages wait.
pub const WAKE_HOLD_UNCERTAIN_INPUT: &str =
    "waiting: the deck's notice may be in its input. Check it before Enter.";
/// The card's words for [`WakeHold::DialogDuringRing`] while messages wait.
pub const WAKE_HOLD_DIALOG_DURING_RING: &str =
    "waiting: a dialog may have taken the deck's keys. Check its pane.";
/// [`WAKE_HOLD_UNCERTAIN_INPUT`]'s words with nothing waiting: the warning
/// stands on its own.
pub const WAKE_HOLD_UNCERTAIN_INPUT_ALONE: &str =
    "The deck's notice may be in its input. Check it before pressing Enter.";
/// [`WAKE_HOLD_DIALOG_DURING_RING`]'s words with nothing waiting.
pub const WAKE_HOLD_DIALOG_DURING_RING_ALONE: &str =
    "A dialog may have taken the deck's keys. Check the agent's pane.";

/// The card notice for a message handed to the agent the most times allowed,
/// none acknowledged. Fixed text, as every delivery notice is
/// ([`crate::agent_pty::DeliveryNotice::detail`]).
pub const NOTICE_MAX_DELIVER: &str =
    "not delivered: a deck message was handed to this agent repeatedly and never acknowledged";
/// PRD #1666 long-turn delivery: the detail of the card notice that lifts
/// [`NOTICE_MAX_DELIVER`] once the message was acknowledged after all. Logged,
/// not shown: the card returns to the agent's own status.
pub const NOTICE_DELIVERED_LATE: &str =
    "delivered after all: a deck message reported not delivered reached this agent";
/// The card notice for a message whose agent exited while it was still owed.
pub const NOTICE_AGENT_EXITED: &str =
    "not delivered: this agent exited before a deck message reached it";

/// PRD #1666 §6.2: the headline of a deck message that may or may not have
/// reached the agent.
pub const UNCERTAIN: &str = "delivery uncertain";
/// PRD #1666 §6.4: the headline for several, after their count
/// (`3 deliveries uncertain`).
pub const UNCERTAIN_MANY: &str = "deliveries uncertain";
/// PRD #1666 §6.2: what an uncertain delivery means and what the two actions do.
pub const UNCERTAIN_REASON: &str = "a deck message may or may not have reached this agent. Retry sends it again marked as a possible repeat; Cancel stops delivering it.";
/// PRD #1666 §7: an uncertain delivery the deck may not send again.
pub const OUT_OF_ATTEMPTS: &str = "out of attempts: the deck sent this message as many times as it allows. Cancel it, or post it again.";
/// PRD #1666 §6.3: one-shot attention, never a failure.
pub const HELD_LONG: &str = "the agent has had a deck message for an hour without confirming it. It may still be working on it.";
/// PRD #1666 G3: the card line for a deck message the agent holds while it
/// has been idle for the window, after the glyph and any count
/// (`✉ may not have reached the agent`, `✉ 2 may not have reached the agent`).
pub const NOT_STARTED: &str = "may not have reached the agent";
/// PRD #1666 G3: what that line means and what the two actions do. It says
/// what the deck observes (the agent idle, and no start on the message
/// reported) rather than what the agent did, and that Retry may duplicate
/// it.
pub const NOT_STARTED_REASON: &str = "the agent has been idle for a few minutes and the deck has not seen it start on this deck message, so the message may not have reached it. Retry sends it again, so the agent may get it twice. Cancel stops further delivery but cannot take back a copy the agent already has.";
/// PRD #1666 §6.4: the two user actions, as both clients label them.
pub const RETRY: &str = "Retry";
pub const CANCEL: &str = "Cancel";
/// PRD #1666 §7: Retry refused past the attempt budget.
pub const RETRY_LIMIT: &str = "retry limit reached; Cancel it, or post it again";
/// PRD #1666 §2: the answer to the second of two clients acting at once.
pub const ACTION_STALE: &str =
    "another client already acted on this message; the card shows where it stands now";
/// PRD #1666 §2: an action on a message that reached the agent meanwhile.
pub const ACTION_SETTLED: &str = "it reached the agent after all";
/// PRD #1666 §2: an action on a message that needs no decision any more.
pub const ACTION_GONE: &str = "this message no longer needs a decision";
/// PRD #1666 §6.4: a Retry the deck took.
pub const RETRY_APPLIED: &str = "sent again, marked as a possible repeat";
/// PRD #1666 §6.4: a Cancel the deck took.
pub const CANCEL_APPLIED: &str = "delivery cancelled";
/// PRD #1666 §6.4: a deck whose daemon predates Retry and Cancel.
pub const ACTION_UNSUPPORTED: &str =
    "this deck cannot Retry or Cancel deck messages: upgrade dot-agent-deck";
/// PRD #1666 §6.4: what the TUI's picker title says.
pub const ATTENTION_TITLE: &str = "Deck messages";

/// PRD #1666 §2: the words for the daemon's answer to a Retry (`retry`) or a
/// Cancel, by the answer's wire spelling (`applied`, `stale`, …). Both clients
/// show these.
pub fn action_text(retry: bool, outcome: &str) -> &'static str {
    match outcome {
        "applied" if retry => RETRY_APPLIED,
        "applied" => CANCEL_APPLIED,
        "stale" => ACTION_STALE,
        "already_settled" => ACTION_SETTLED,
        "retry_limit" => RETRY_LIMIT,
        "unsupported" => ACTION_UNSUPPORTED,
        _ => ACTION_GONE,
    }
}
/// PRD #1666 §11: a registration declared an integration below the floor.
pub const ADAPTER_TOO_OLD: &str =
    "this agent's deck integration is out of date; restart the agent to update it";
/// PRD #1666 §11: the deck could not rewrite the agent's integration files
/// as the agent started, so a restart does not update them.
pub const REFRESH_FAILED: &str = "the deck could not update this agent's deck integration: run dot-agent-deck hooks install --agent opencode to see why, then restart the agent";
/// PRD #1666 G6: [`REFRESH_FAILED`] for a Pi agent, whose extension is
/// written by `orchestrator setup` into the directory its own `HOME` and
/// `PI_CODING_AGENT_DIR` resolve.
pub const REFRESH_FAILED_PI: &str = "the deck could not update this agent's Pi extension: run dot-agent-deck orchestrator setup with the same HOME and PI_CODING_AGENT_DIR as the agent to see why, then restart the agent";
/// PRD #1666 §11: the capability probe proved the agent's CLI lacks the flag.
pub const CLI_TOO_OLD: &str = "the dot-agent-deck command this agent runs does not support this deck's integration: upgrade dot-agent-deck, then restart the agent";
/// PRD #1666 part 2 §2.7: the agent's deck hooks run a dot-agent-deck older
/// than this deck's messages need; [`hooks_too_old_text`] adds the fix.
pub const HOOKS_TOO_OLD: &str =
    "this agent's deck hooks run a dot-agent-deck too old for its messages";
/// [`HOOKS_TOO_OLD`]'s fix when the deck can name no command to run.
pub const HOOKS_TOO_OLD_UPGRADE: &str = "upgrade the dot-agent-deck the hooks run, or run hooks install from the copy you want them to use, then restart the agent";

/// [`HOOKS_TOO_OLD`] with its fix: `command`, the deck's own `hooks install`
/// line, when it has one.
pub fn hooks_too_old_text(command: Option<&str>) -> String {
    match command {
        Some(command) => format!("{HOOKS_TOO_OLD}: run {command}, then restart the agent"),
        None => format!("{HOOKS_TOO_OLD}: {HOOKS_TOO_OLD_UPGRADE}"),
    }
}
/// PRD #1666 §11: a reversible readiness notice, cleared by a registration.
pub const NOT_ARMED: &str =
    "this agent's deck integration has not connected yet; its messages wait until it does";
/// PRD #1666 step 4: an `inbox wait --stream` from a component the receipt
/// policy does not cover. Only a receipt receiver streams; hooks and the Claude
/// waiter take their mail once at a time.
pub const STREAM_UNSUPPORTED: &str =
    "this inbox component cannot stream; only a deck integration on message receipts can";
/// PRD #1666 part 3 (H5): an `inbox wait` once from a component whose hooks
/// take their messages themselves (Codex's, Devin's, an unknown one's), which
/// only a dot-agent-deck from before those hooks moved onto message receipts
/// sends.
pub const WAIT_UNSUPPORTED: &str = "this inbox component cannot wait for messages: the agent's hooks or `dot-agent-deck inbox fetch` take them, so update the dot-agent-deck the agent runs";
/// PRD #1666 §3: receipts cannot be recorded for this generation; `({error})`
/// follows.
pub const RECEIPTS_DEGRADED: &str =
    "messages may repeat: the deck cannot record message receipts for this agent";

/// PRD #1666 part 3 (R-2): the headline of a card whose agent's own requests
/// for its deck messages the deck refused while messages wait for it.
pub const ADMISSION_REFUSED: &str = "Messages can't reach this agent";
/// [`ADMISSION_REFUSED`]'s remedy, after the cause.
pub const ADMISSION_REFUSED_REMEDY: &str = "Restart it from the deck; messages addressed to it are not handed to the restarted agent, so have them sent again.";
/// [`ADMISSION_REFUSED`]'s causes, by the refusal code the agent's request got
/// (`inbox_other_user`, …). An unknown code gets no cause.
pub const ADMISSION_CAUSE_OTHER_USER: &str = "a process running as another user asked for them";
pub const ADMISSION_CAUSE_UNIDENTIFIED: &str =
    "the deck could not identify the process that asked for them";
pub const ADMISSION_CAUSE_NOT_CALLER: &str = "its integration named a process other than its own";
pub const ADMISSION_CAUSE_NESTED: &str =
    "an agent of the same kind running inside it asked for them";
pub const ADMISSION_CAUSE_NOT_MEMBER: &str = "a process outside the agent asked for them";
pub const ADMISSION_CAUSE_ABOVE_AGENT: &str =
    "its integration named a process the agent itself runs under";

/// The cause [`ADMISSION_REFUSED`] names for the refusal `code`.
pub fn admission_cause(code: &str) -> Option<&'static str> {
    match code {
        "inbox_other_user" => Some(ADMISSION_CAUSE_OTHER_USER),
        "inbox_peer_unidentified" => Some(ADMISSION_CAUSE_UNIDENTIFIED),
        "inbox_component_not_caller" => Some(ADMISSION_CAUSE_NOT_CALLER),
        "inbox_nested_agent" => Some(ADMISSION_CAUSE_NESTED),
        "inbox_not_agent_process" => Some(ADMISSION_CAUSE_NOT_MEMBER),
        "inbox_receiver_above_agent" => Some(ADMISSION_CAUSE_ABOVE_AGENT),
        _ => None,
    }
}

/// PRD #1666 part 3 (R-2): the card's notice for a refusal with `code`:
/// `Messages can't reach this agent: its connection to the deck was refused
/// (<cause>). Restart it from the deck; messages addressed to it are not handed to the restarted agent, so have them sent again.`
pub fn admission_refused_text(code: &str) -> String {
    match admission_cause(code) {
        Some(cause) => format!(
            "{ADMISSION_REFUSED}: its connection to the deck was refused ({cause}). {ADMISSION_REFUSED_REMEDY}"
        ),
        None => format!(
            "{ADMISSION_REFUSED}: its connection to the deck was refused. {ADMISSION_REFUSED_REMEDY}"
        ),
    }
}

/// PRD #1666 §7: hand-outs paused for room in the evidence store.
pub fn evidence_paused_text(waiting: usize) -> String {
    format!(
        "deck messages paused: {waiting} delivered messages still wait for proof that older copies cannot run; restart the agent to clear them"
    )
}

/// PRD #1666 §3: [`RECEIPTS_DEGRADED`] with the deck's error.
pub fn receipts_degraded_text(error: &str) -> String {
    format!("{RECEIPTS_DEGRADED} ({error})")
}

/// PRD #1666 §6.1: the trusted line a repeat's frame carries, after `Sent by:`
/// and before any body.
pub fn repeat_line(original_id: &str) -> String {
    format!(
        "This may repeat dot-agent-deck message {original_id}, which may already have reached you. If you already started it, continue that work instead of starting it again."
    )
}

/// PRD #1666 §11: why a generation's integration cannot take deck messages
/// yet, as the snapshot and both clients name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationNotice {
    CliTooOld,
    AdapterTooOld,
    NotArmed,
    RefreshFailed,
    RefreshFailedPi,
    /// Part 2 §2.7: shown with its command ([`hooks_too_old_text`]).
    HooksTooOld,
}

impl IntegrationNotice {
    pub fn text(self) -> &'static str {
        match self {
            IntegrationNotice::CliTooOld => CLI_TOO_OLD,
            IntegrationNotice::AdapterTooOld => ADAPTER_TOO_OLD,
            IntegrationNotice::NotArmed => NOT_ARMED,
            IntegrationNotice::RefreshFailed => REFRESH_FAILED,
            IntegrationNotice::RefreshFailedPi => REFRESH_FAILED_PI,
            IntegrationNotice::HooksTooOld => HOOKS_TOO_OLD,
        }
    }

    fn code(self) -> u8 {
        match self {
            IntegrationNotice::CliTooOld => 1,
            IntegrationNotice::AdapterTooOld => 2,
            IntegrationNotice::NotArmed => 3,
            IntegrationNotice::RefreshFailed => 4,
            IntegrationNotice::RefreshFailedPi => 5,
            IntegrationNotice::HooksTooOld => 6,
        }
    }

    fn from_code(code: u8) -> Option<Option<Self>> {
        match code {
            0 => Some(None),
            1 => Some(Some(IntegrationNotice::CliTooOld)),
            2 => Some(Some(IntegrationNotice::AdapterTooOld)),
            3 => Some(Some(IntegrationNotice::NotArmed)),
            4 => Some(Some(IntegrationNotice::RefreshFailed)),
            5 => Some(Some(IntegrationNotice::RefreshFailedPi)),
            6 => Some(Some(IntegrationNotice::HooksTooOld)),
            _ => None,
        }
    }
}

/// PRD #1666 part 3, round 5-wake (plan §5.1–§5.2): why the deck is holding
/// the doorbell for an agent's waiting messages, as the snapshot and both
/// clients name it. The doorbell publishes one once a hold has lasted
/// [`crate::doorbell::DoorbellConfig::wake_stall_after`] of the agent not
/// working ([`crate::doorbell::HoldEpisode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeHold {
    /// The user may have an unsent draft in the agent's input.
    PossibleDraft,
    /// The agent may be waiting at a prompt: it reported it waits for input,
    /// or the deck's last try found no screen it could type into.
    AwaitingPrompt,
    /// A paste into the agent's input has not finished.
    PasteOpen,
    /// The deck won't type its notice into this agent: its release is not
    /// one the deck has measured, its input mode is not proven, or there is
    /// no way to see that the line was submitted.
    CannotType,
    /// The deck typed its notice and the agent did not take its Enter; the
    /// deck took its line back and rings no more this episode.
    NotTaken,
    /// The deck's notice line may still be in the agent's input: shown even
    /// with nothing waiting.
    UncertainInput,
    /// A dialog opened while the deck was typing, and may have received its
    /// keys: shown even with nothing waiting.
    DialogDuringRing,
    /// A reason a newer deck names that this build does not know. A client
    /// shows it with [`WAKE_STALLED`]'s words; this build never sends it.
    #[serde(other)]
    Unknown,
}

impl WakeHold {
    /// The card's words, after the glyph and the pending count; `None` for
    /// [`WakeHold::Unknown`], which keeps [`WAKE_STALLED`]'s words.
    pub fn words(self) -> Option<&'static str> {
        match self {
            WakeHold::PossibleDraft => Some(WAKE_HOLD_POSSIBLE_DRAFT),
            WakeHold::AwaitingPrompt => Some(WAKE_HOLD_AWAITING_PROMPT),
            WakeHold::PasteOpen => Some(WAKE_HOLD_PASTE_OPEN),
            WakeHold::CannotType => Some(WAKE_HOLD_CANNOT_TYPE),
            WakeHold::NotTaken => Some(WAKE_HOLD_NOT_TAKEN),
            WakeHold::UncertainInput => Some(WAKE_HOLD_UNCERTAIN_INPUT),
            WakeHold::DialogDuringRing => Some(WAKE_HOLD_DIALOG_DURING_RING),
            WakeHold::Unknown => None,
        }
    }

    /// The words with nothing waiting, for the two warnings shown even
    /// then; `None` for every other hold, which shows only while messages
    /// wait.
    pub fn alone_words(self) -> Option<&'static str> {
        match self {
            WakeHold::UncertainInput => Some(WAKE_HOLD_UNCERTAIN_INPUT_ALONE),
            WakeHold::DialogDuringRing => Some(WAKE_HOLD_DIALOG_DURING_RING_ALONE),
            _ => None,
        }
    }

    /// What the overdue notice tells an orchestrator about the hold
    /// ([`crate::inbox_hub::compose_undelivered_notice`]); `None` for
    /// [`WakeHold::Unknown`].
    pub fn notice_clause(self) -> Option<&'static str> {
        match self {
            WakeHold::PossibleDraft => Some(
                "the worker's pane may have an unsent draft in its input, which the user can send or clear",
            ),
            WakeHold::AwaitingPrompt => Some("the worker may be waiting at a prompt in its pane"),
            WakeHold::PasteOpen => Some("a paste into the worker's input has not finished"),
            WakeHold::CannotType => Some(
                "the deck can't type its notice into the worker's input, so the worker takes its messages when it is sent a prompt in its pane",
            ),
            WakeHold::NotTaken => Some(
                "the worker did not take the deck's Enter, so it takes its messages when it is sent a prompt in its pane",
            ),
            WakeHold::UncertainInput => Some("the deck's notice line may be in the worker's input"),
            WakeHold::DialogDuringRing => Some(
                "a dialog opened in the worker's pane while the deck was typing, and may have received the deck's keys",
            ),
            WakeHold::Unknown => None,
        }
    }

    fn code(self) -> u8 {
        match self {
            WakeHold::PossibleDraft => 1,
            WakeHold::AwaitingPrompt => 2,
            WakeHold::PasteOpen => 3,
            WakeHold::CannotType => 4,
            WakeHold::NotTaken => 5,
            WakeHold::UncertainInput => 6,
            WakeHold::DialogDuringRing => 7,
            WakeHold::Unknown => 9,
        }
    }

    fn from_code(code: u8) -> Option<Option<Self>> {
        match code {
            0 => Some(None),
            1 => Some(Some(WakeHold::PossibleDraft)),
            2 => Some(Some(WakeHold::AwaitingPrompt)),
            3 => Some(Some(WakeHold::PasteOpen)),
            4 => Some(Some(WakeHold::CannotType)),
            5 => Some(Some(WakeHold::NotTaken)),
            6 => Some(Some(WakeHold::UncertainInput)),
            7 => Some(Some(WakeHold::DialogDuringRing)),
            9 => Some(Some(WakeHold::Unknown)),
            _ => None,
        }
    }
}

/// PRD #1666 §6.4: what one listed item asks the user to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// Possibly handed, nobody possesses it, automatic repeats spent.
    Uncertain,
    /// Uncertain because no further hand-out fits the attempt budget.
    OutOfAttempts,
    /// Possessed for an hour without a commit (§6.3).
    HeldLong,
    /// PRD #1666 G3: held by the agent, uncommitted, while the agent has been
    /// idle for the not-started window.
    NotStarted,
}

impl AttentionKind {
    /// The row's explanation, the same in both clients.
    pub fn reason(self) -> &'static str {
        match self {
            AttentionKind::Uncertain => UNCERTAIN_REASON,
            AttentionKind::OutOfAttempts => OUT_OF_ATTEMPTS,
            AttentionKind::HeldLong => HELD_LONG,
            AttentionKind::NotStarted => NOT_STARTED_REASON,
        }
    }

    /// Whether the item is an uncertain delivery rather than attention.
    pub fn uncertain(self) -> bool {
        matches!(
            self,
            AttentionKind::Uncertain | AttentionKind::OutOfAttempts
        )
    }

    /// Whether Retry is offered: everything but an exhausted budget.
    pub fn retry_offered(self) -> bool {
        !matches!(self, AttentionKind::OutOfAttempts)
    }
}

/// Metadata key of the inert journal entry that carries a card's inbox state
/// ([`CardInbox`]). The TUI's `SessionState` has no field for it, so the state
/// rides its event journal the way a live target does, and the journal keeps
/// that entry when it trims (see `crate::state`).
pub const CARD_INBOX_METADATA_KEY: &str = "inbox_card";
/// Beside [`CARD_INBOX_METADATA_KEY`]: the deck's receipt-storage error, when
/// [`CardInbox::receipts_degraded`] holds one.
pub const CARD_INBOX_DEGRADED_METADATA_KEY: &str = "inbox_card_degraded";
/// Beside [`CARD_INBOX_METADATA_KEY`]: the command that fixes the integration
/// notice, when [`CardInbox::integration_command`] holds one.
pub const CARD_INBOX_COMMAND_METADATA_KEY: &str = "inbox_card_command";
/// Beside [`CARD_INBOX_METADATA_KEY`]: the refusal code of the agent's own
/// request for its messages, when [`CardInbox::admission_refused`] holds one.
pub const CARD_INBOX_ADMISSION_METADATA_KEY: &str = "inbox_card_admission";

/// What a card shows of its agent's inbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardInbox {
    /// Messages queued or taken but not acknowledged.
    pub pending: usize,
    /// The agent has acknowledged a message.
    pub delivered: bool,
    /// PR #1735 fifth review (R5): the deck reports that nothing has woken
    /// the agent for its waiting messages ([`WAKE_STALLED`]).
    pub stalled: bool,
    /// PRD #1666 §6: deliveries that may or may not have reached the agent.
    pub uncertain: usize,
    /// PRD #1666 §7: how many of `uncertain` are out of attempts, whose row
    /// offers Cancel alone.
    pub out_of_attempts: usize,
    /// PRD #1666 §6.3: messages held an hour without a commit.
    pub held_long: usize,
    /// PRD #1666 G3: messages handed to an agent that started no work since.
    pub not_started: usize,
    /// PRD #1666 §11: why the agent's integration cannot take messages yet.
    pub integration: Option<IntegrationNotice>,
    /// Part 2 §2.7: the command that fixes `integration`.
    pub integration_command: Option<String>,
    /// PRD #1666 §7: delivered messages whose evidence pauses hand-outs.
    pub evidence_paused: Option<usize>,
    /// PRD #1666 §3: the deck's receipt-storage error for this agent.
    pub receipts_degraded: Option<String>,
    /// PRD #1666 part 3 (R-2): the refusal code of this agent's own request
    /// for its deck messages, while messages wait for it.
    pub admission_refused: Option<String>,
    /// PRD #1666 part 3, round 5-wake: why the deck holds the doorbell for
    /// the waiting messages, once it says so ([`WakeHold`]).
    pub hold: Option<WakeHold>,
}

impl CardInbox {
    /// The card's inbox line, the same words in both clients:
    /// `✉ delivery uncertain` (`✉ 3 deliveries uncertain`) while deliveries
    /// are uncertain, then `✉ may not have reached the agent` (`✉ 2 may not
    /// have reached the agent`), then `✉2` alone while the notice row says why
    /// the deck holds the doorbell ([`Self::hold_text`]), `✉2 waiting to wake:
    /// send it a prompt` (also for a hold this build does not know), `✉2
    /// queued`, `✉ delivered`, or nothing. What the card says beside it is
    /// [`Self::notice_text`].
    pub fn full_text(&self) -> Option<String> {
        if self.uncertain == 1 {
            return Some(format!("{GLYPH} {UNCERTAIN}"));
        }
        if self.uncertain > 1 {
            return Some(format!("{GLYPH} {} {UNCERTAIN_MANY}", self.uncertain));
        }
        if self.not_started == 1 {
            return Some(format!("{GLYPH} {NOT_STARTED}"));
        }
        if self.not_started > 1 {
            return Some(format!("{GLYPH} {} {NOT_STARTED}", self.not_started));
        }
        if self.hold_in_notice() && self.pending > 0 {
            return self.short_text();
        }
        if self.pending > 0 && (self.stalled || self.hold == Some(WakeHold::Unknown)) {
            Some(format!(
                "{GLYPH}{} {WAKE_STALLED}: {WAKE_STALLED_HINT}",
                self.pending
            ))
        } else if self.pending > 0 {
            Some(format!("{GLYPH}{} {QUEUED}", self.pending))
        } else if self.delivered {
            Some(format!("{GLYPH} {DELIVERED}"))
        } else {
            None
        }
    }

    /// The card's inbox notice row, the same words in both clients, when the
    /// deck has something to say beyond the line: why the integration takes
    /// nothing yet, paused hand-outs, what an uncertain delivery means (what
    /// running out of attempts means, when every uncertain one has: its row
    /// offers no Retry), what a message the agent may not have received
    /// means, held_long attention, then failed receipt storage — the first
    /// that holds. A refused request of the agent's own for its messages comes
    /// first of all (PRD #1666 part 3, R-2): nothing else reaches it until
    /// that is fixed.
    pub fn notice_text(&self) -> Option<String> {
        if let Some(code) = &self.admission_refused {
            return Some(admission_refused_text(code));
        }
        if self.integration == Some(IntegrationNotice::HooksTooOld) {
            return Some(hooks_too_old_text(self.integration_command.as_deref()));
        }
        if let Some(notice) = self.integration {
            return Some(notice.text().to_string());
        }
        if let Some(waiting) = self.evidence_paused {
            return Some(evidence_paused_text(waiting));
        }
        if self.uncertain > self.out_of_attempts {
            return Some(UNCERTAIN_REASON.to_string());
        }
        if self.uncertain > 0 {
            return Some(OUT_OF_ATTEMPTS.to_string());
        }
        if self.not_started > 0 {
            return Some(NOT_STARTED_REASON.to_string());
        }
        if self.held_long > 0 {
            return Some(HELD_LONG.to_string());
        }
        if let Some(text) = self.hold_text() {
            return Some(text);
        }
        self.receipts_degraded
            .as_deref()
            .map(receipts_degraded_text)
    }

    /// PRD #1666 part 3, round 5-wake (plan §5.2): why the deck holds the
    /// doorbell, with the pending count, while messages wait:
    /// `✉2 waiting: there may be an unsent draft in its input. Send or clear
    /// it.`. With nothing pending, only the two warnings that stand on their
    /// own (the deck's line may be in the input, a dialog may have taken its
    /// keys); `None` otherwise and for a hold this build does not know.
    pub fn hold_text(&self) -> Option<String> {
        let hold = self.hold?;
        let words = hold.words()?;
        if self.pending > 0 {
            Some(format!("{GLYPH}{} {words}", self.pending))
        } else {
            hold.alone_words().map(str::to_string)
        }
    }

    /// Whether the notice row is the hold's ([`Self::notice_text`] reaches
    /// [`Self::hold_text`]), so the inbox line keeps only the count.
    fn hold_in_notice(&self) -> bool {
        self.hold_text().is_some()
            && self.admission_refused.is_none()
            && self.integration.is_none()
            && self.evidence_paused.is_none()
            && self.uncertain == 0
            && self.not_started == 0
            && self.held_long == 0
    }

    /// The line a narrow card keeps: the pending count alone (`✉2`), and
    /// nothing once nothing is pending.
    pub fn short_text(&self) -> Option<String> {
        (self.pending > 0).then(|| format!("{GLYPH}{}", self.pending))
    }

    /// The journal entry's metadata value (the degraded error rides
    /// [`CARD_INBOX_DEGRADED_METADATA_KEY`]).
    pub fn encode(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{}",
            self.pending,
            u8::from(self.delivered),
            u8::from(self.stalled),
            self.uncertain,
            self.held_long,
            self.integration.map_or(0, IntegrationNotice::code),
            self.evidence_paused
                .map_or_else(|| "-".to_string(), |n| n.to_string()),
            self.not_started,
            self.out_of_attempts,
            self.hold.map_or(0, WakeHold::code),
        )
    }

    /// Reads [`Self::encode`]'s value, and the two-, three-, seven-, eight-
    /// and nine-field values written before the later fields existed; `None`
    /// for anything else, out-of-attempts counts above the uncertain one
    /// included.
    pub fn decode(value: &str) -> Option<Self> {
        let flag = |text: &str| match text {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let parts: Vec<&str> = value.split(',').collect();
        if !matches!(parts.len(), 2 | 3 | 7 | 8 | 9 | 10) {
            return None;
        }
        let mut inbox = Self {
            pending: parts[0].parse().ok()?,
            delivered: flag(parts[1])?,
            ..Self::default()
        };
        if let Some(text) = parts.get(2) {
            inbox.stalled = flag(text)?;
        }
        if parts.len() >= 7 {
            inbox.uncertain = parts[3].parse().ok()?;
            inbox.held_long = parts[4].parse().ok()?;
            inbox.integration = IntegrationNotice::from_code(parts[5].parse().ok()?)?;
            inbox.evidence_paused = match parts[6] {
                "-" => None,
                n => Some(n.parse().ok()?),
            };
        }
        if let Some(text) = parts.get(7) {
            inbox.not_started = text.parse().ok()?;
        }
        if let Some(text) = parts.get(8) {
            inbox.out_of_attempts = text.parse().ok()?;
            if inbox.out_of_attempts > inbox.uncertain {
                return None;
            }
        }
        if let Some(text) = parts.get(9) {
            inbox.hold = WakeHold::from_code(text.parse().ok()?)?;
        }
        Some(inbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words are the ones the desktop reads from the shared fixture.
    #[test]
    fn the_words_match_the_shared_fixture() {
        let words: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/inbox-display.json")).unwrap();
        assert_eq!(words["glyph"], GLYPH);
        assert_eq!(words["queued"], QUEUED);
        assert_eq!(words["delivered"], DELIVERED);
        assert_eq!(words["notDelivered"], NOT_DELIVERED);
        assert_eq!(words["workDone"], WORK_DONE);
        assert_eq!(words["wakeStalled"], WAKE_STALLED);
        assert_eq!(words["wakeStalledHint"], WAKE_STALLED_HINT);
        for (value, hold) in [
            ("possible_draft", WakeHold::PossibleDraft),
            ("awaiting_prompt", WakeHold::AwaitingPrompt),
            ("paste_open", WakeHold::PasteOpen),
            ("cannot_type", WakeHold::CannotType),
            ("not_taken", WakeHold::NotTaken),
            ("uncertain_input", WakeHold::UncertainInput),
            ("dialog_during_ring", WakeHold::DialogDuringRing),
        ] {
            assert_eq!(
                serde_json::to_value(hold).unwrap(),
                serde_json::json!(value)
            );
            assert_eq!(words["wakeHolds"][value], hold.words().unwrap(), "{value}");
            match hold.alone_words() {
                Some(alone) => assert_eq!(words["wakeHoldsAlone"][value], alone, "{value}"),
                None => assert!(words["wakeHoldsAlone"][value].is_null(), "{value}"),
            }
        }
        assert_eq!(
            words["wakeHolds"].as_object().map(|holds| holds.len()),
            Some(7),
            "the fixture lists every hold the deck names"
        );
        assert_eq!(
            words["wakeHoldsAlone"].as_object().map(|holds| holds.len()),
            Some(2)
        );
        assert_eq!(words["uncertain"], UNCERTAIN);
        assert_eq!(words["uncertainMany"], UNCERTAIN_MANY);
        assert_eq!(words["uncertainReason"], UNCERTAIN_REASON);
        assert_eq!(words["outOfAttempts"], OUT_OF_ATTEMPTS);
        assert_eq!(words["heldLong"], HELD_LONG);
        assert_eq!(words["notStarted"], NOT_STARTED);
        assert_eq!(words["notStartedReason"], NOT_STARTED_REASON);
        assert_eq!(words["retry"], RETRY);
        assert_eq!(words["cancel"], CANCEL);
        assert_eq!(words["retryLimit"], RETRY_LIMIT);
        assert_eq!(words["actionStale"], ACTION_STALE);
        assert_eq!(words["actionSettled"], ACTION_SETTLED);
        assert_eq!(words["actionGone"], ACTION_GONE);
        assert_eq!(words["retryApplied"], RETRY_APPLIED);
        assert_eq!(words["cancelApplied"], CANCEL_APPLIED);
        assert_eq!(words["actionUnsupported"], ACTION_UNSUPPORTED);
        assert_eq!(words["attentionTitle"], ATTENTION_TITLE);
        assert_eq!(words["adapterTooOld"], ADAPTER_TOO_OLD);
        assert_eq!(words["cliTooOld"], CLI_TOO_OLD);
        assert_eq!(words["notArmed"], NOT_ARMED);
        assert_eq!(words["refreshFailed"], REFRESH_FAILED);
        assert_eq!(words["refreshFailedPi"], REFRESH_FAILED_PI);
        assert_eq!(words["hooksTooOld"], HOOKS_TOO_OLD);
        assert_eq!(words["hooksTooOldUpgrade"], HOOKS_TOO_OLD_UPGRADE);
        assert_eq!(words["receiptsDegraded"], RECEIPTS_DEGRADED);
        assert_eq!(words["admissionRefused"], ADMISSION_REFUSED);
        assert_eq!(words["admissionRefusedRemedy"], ADMISSION_REFUSED_REMEDY);
        for (code, cause) in [
            ("inbox_other_user", ADMISSION_CAUSE_OTHER_USER),
            ("inbox_peer_unidentified", ADMISSION_CAUSE_UNIDENTIFIED),
            ("inbox_component_not_caller", ADMISSION_CAUSE_NOT_CALLER),
            ("inbox_nested_agent", ADMISSION_CAUSE_NESTED),
            ("inbox_not_agent_process", ADMISSION_CAUSE_NOT_MEMBER),
            ("inbox_receiver_above_agent", ADMISSION_CAUSE_ABOVE_AGENT),
        ] {
            assert_eq!(words["admissionCauses"][code], cause, "{code}");
            assert_eq!(admission_cause(code), Some(cause));
        }
        assert_eq!(
            words["admissionCauses"]
                .as_object()
                .map(|causes| causes.len()),
            Some(6),
            "the fixture lists every cause the deck names"
        );
        assert_eq!(
            words["evidencePaused"]
                .as_str()
                .unwrap()
                .replace("{n}", "3"),
            evidence_paused_text(3)
        );
        for notice in [NOTICE_MAX_DELIVER, NOTICE_AGENT_EXITED] {
            assert!(notice.starts_with(NOT_DELIVERED), "{notice}");
        }
    }

    #[test]
    fn the_card_line_says_queued_then_delivered() {
        let pending = CardInbox {
            pending: 2,
            delivered: true,
            stalled: false,
            ..CardInbox::default()
        };
        assert_eq!(pending.full_text().as_deref(), Some("✉2 queued"));
        assert_eq!(pending.short_text().as_deref(), Some("✉2"));
        let done = CardInbox {
            pending: 0,
            delivered: true,
            stalled: false,
            ..CardInbox::default()
        };
        assert_eq!(done.full_text().as_deref(), Some("✉ delivered"));
        assert_eq!(done.short_text(), None);
        assert_eq!(CardInbox::default().full_text(), None);
        let stalled = CardInbox {
            pending: 1,
            delivered: false,
            stalled: true,
            ..CardInbox::default()
        };
        assert_eq!(
            stalled.full_text().as_deref(),
            Some("✉1 waiting to wake: send it a prompt")
        );
        assert_eq!(stalled.short_text().as_deref(), Some("✉1"));
        let settled = CardInbox {
            pending: 0,
            delivered: true,
            stalled: true,
            ..CardInbox::default()
        };
        assert_eq!(settled.full_text().as_deref(), Some("✉ delivered"));
    }

    #[test]
    fn the_journal_value_round_trips_and_refuses_anything_else() {
        for inbox in [
            CardInbox::default(),
            CardInbox {
                pending: 7,
                delivered: true,
                ..CardInbox::default()
            },
            CardInbox {
                pending: 2,
                stalled: true,
                ..CardInbox::default()
            },
            CardInbox {
                pending: 3,
                uncertain: 2,
                out_of_attempts: 1,
                held_long: 1,
                integration: Some(IntegrationNotice::NotArmed),
                evidence_paused: Some(4),
                ..CardInbox::default()
            },
            CardInbox {
                pending: 1,
                integration: Some(IntegrationNotice::RefreshFailed),
                ..CardInbox::default()
            },
            CardInbox {
                pending: 2,
                stalled: true,
                hold: Some(WakeHold::PossibleDraft),
                ..CardInbox::default()
            },
            CardInbox {
                pending: 1,
                hold: Some(WakeHold::Unknown),
                ..CardInbox::default()
            },
        ] {
            assert_eq!(CardInbox::decode(&inbox.encode()), Some(inbox.clone()));
        }
        assert_eq!(
            CardInbox::decode("3,1"),
            Some(CardInbox {
                pending: 3,
                delivered: true,
                ..CardInbox::default()
            }),
            "a value written before `stalled` existed still reads"
        );
        for bad in [
            "",
            "1",
            "x,0",
            "1,2",
            "-1,0",
            "1,0,2",
            "1,0,1,0",
            "1,0,0,0,0,7,-",
            "1,0,0,0,0,0,x",
            "1,0,0,1,0,0,-,0,2",
            "1,0,0,1,0,0,-,0,x",
            "1,0,0,0,0,0,-,0,0,8",
            "1,0,0,0,0,0,-,0,0,x",
        ] {
            assert_eq!(CardInbox::decode(bad), None, "{bad}");
        }
    }

    /// PRD #1666 part 3, round 5-wake: while the deck holds the doorbell for
    /// a reason this build knows, the notice row says why with the pending
    /// count and the line keeps the count alone, so the stall's remedy never
    /// sits beside the hold's; a reason it does not know keeps the stall's
    /// words; a notice row another notice takes leaves the line as it was;
    /// and with nothing pending only the deck's-line and dialog warnings
    /// still show, in their own words, beside the line as it is.
    #[test]
    fn a_hold_takes_the_notice_row_and_leaves_the_line_the_count() {
        for (hold, words) in [
            (WakeHold::PossibleDraft, WAKE_HOLD_POSSIBLE_DRAFT),
            (WakeHold::AwaitingPrompt, WAKE_HOLD_AWAITING_PROMPT),
            (WakeHold::PasteOpen, WAKE_HOLD_PASTE_OPEN),
            (WakeHold::CannotType, WAKE_HOLD_CANNOT_TYPE),
            (WakeHold::NotTaken, WAKE_HOLD_NOT_TAKEN),
            (WakeHold::UncertainInput, WAKE_HOLD_UNCERTAIN_INPUT),
            (WakeHold::DialogDuringRing, WAKE_HOLD_DIALOG_DURING_RING),
        ] {
            let held = CardInbox {
                pending: 2,
                stalled: true,
                hold: Some(hold),
                ..CardInbox::default()
            };
            assert_eq!(held.full_text().as_deref(), Some("✉2"), "{hold:?}");
            assert_eq!(held.notice_text(), Some(format!("✉2 {words}")), "{hold:?}");
            let empty = CardInbox {
                pending: 0,
                delivered: true,
                ..held.clone()
            };
            assert_eq!(empty.full_text().as_deref(), Some("✉ delivered"));
            assert_eq!(
                empty.notice_text().as_deref(),
                hold.alone_words(),
                "{hold:?}: only the two warnings stand alone"
            );
            let behind = CardInbox {
                held_long: 1,
                ..held.clone()
            };
            assert_eq!(behind.notice_text().as_deref(), Some(HELD_LONG));
            assert_eq!(
                behind.full_text().as_deref(),
                Some("✉2 waiting to wake: send it a prompt")
            );
        }
        let unknown = CardInbox {
            pending: 1,
            hold: Some(WakeHold::Unknown),
            ..CardInbox::default()
        };
        assert_eq!(
            unknown.full_text().as_deref(),
            Some("✉1 waiting to wake: send it a prompt")
        );
        assert_eq!(unknown.notice_text(), None);
        assert_eq!(
            serde_json::from_value::<WakeHold>(serde_json::json!("a_newer_reason")).unwrap(),
            WakeHold::Unknown
        );
    }

    /// The line says uncertain first; the notice row says, in a fixed order,
    /// what blocks delivery or puts it in doubt, in the shared words.
    #[test]
    fn the_card_notice_row_puts_blocking_notices_first() {
        let base = CardInbox {
            pending: 2,
            uncertain: 1,
            held_long: 1,
            receipts_degraded: Some("disk full".into()),
            ..CardInbox::default()
        };
        assert_eq!(base.full_text().as_deref(), Some("✉ delivery uncertain"));
        assert_eq!(base.notice_text().as_deref(), Some(UNCERTAIN_REASON));
        // PR #1735 review 4309809292: a delivery out of attempts is still
        // uncertain, but its row offers Cancel alone, so while no other
        // uncertain one is listed the notice says what running out means
        // rather than offering Retry.
        let exhausted = CardInbox {
            out_of_attempts: 1,
            ..base.clone()
        };
        assert_eq!(
            exhausted.full_text().as_deref(),
            Some("✉ delivery uncertain")
        );
        assert_eq!(exhausted.notice_text().as_deref(), Some(OUT_OF_ATTEMPTS));
        let mixed = CardInbox {
            uncertain: 2,
            out_of_attempts: 1,
            ..base.clone()
        };
        assert_eq!(mixed.notice_text().as_deref(), Some(UNCERTAIN_REASON));
        let many = CardInbox {
            uncertain: 3,
            ..base.clone()
        };
        assert_eq!(
            many.full_text().as_deref(),
            Some("✉ 3 deliveries uncertain")
        );
        let held = CardInbox {
            uncertain: 0,
            ..base.clone()
        };
        assert_eq!(held.full_text().as_deref(), Some("✉2 queued"));
        assert_eq!(held.notice_text().as_deref(), Some(HELD_LONG));
        let degraded = CardInbox {
            uncertain: 0,
            held_long: 0,
            ..base.clone()
        };
        assert_eq!(
            degraded.notice_text(),
            Some(format!("{RECEIPTS_DEGRADED} (disk full)"))
        );
        let paused = CardInbox {
            evidence_paused: Some(5),
            ..base.clone()
        };
        assert_eq!(paused.notice_text(), Some(evidence_paused_text(5)));
        for notice in [
            IntegrationNotice::CliTooOld,
            IntegrationNotice::AdapterTooOld,
            IntegrationNotice::NotArmed,
            IntegrationNotice::RefreshFailed,
            IntegrationNotice::RefreshFailedPi,
        ] {
            let blocked = CardInbox {
                integration: Some(notice),
                ..paused.clone()
            };
            assert_eq!(blocked.notice_text().as_deref(), Some(notice.text()));
        }
        assert_eq!(CardInbox::default().notice_text(), None);
    }

    /// PRD #1666 part 3 (R-2): a refused request of the agent's own comes
    /// ahead of every other notice, names its cause and the remedy, and an
    /// unknown code still says what to do.
    #[test]
    fn a_refused_connection_heads_the_notice_row_with_its_cause_and_remedy() {
        let refused = CardInbox {
            pending: 2,
            uncertain: 1,
            integration: Some(IntegrationNotice::HooksTooOld),
            admission_refused: Some("inbox_peer_unidentified".into()),
            ..CardInbox::default()
        };
        assert_eq!(
            refused.notice_text().as_deref(),
            Some(
                "Messages can't reach this agent: its connection to the deck was refused (the \
                 deck could not identify the process that asked for them). Restart it from the \
                 deck; messages addressed to it are not handed to the restarted agent, so have \
                 them sent again."
            )
        );
        assert_eq!(
            admission_refused_text("inbox_from_a_later_release"),
            "Messages can't reach this agent: its connection to the deck was refused. Restart \
             it from the deck; messages addressed to it are not handed to the restarted agent, so \
             have them sent again."
        );
    }
}
