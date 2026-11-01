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
/// The `x` makes sure `Ctrl+C` has something to clear: on an empty box
/// Claude Code asks for a second one to exit, and Codex exits (measured).
pub const WAKE_HOLD_POSSIBLE_DRAFT: &str =
    "waiting: there may be an unsent draft. Send it, or type x then Ctrl+C.";
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
/// PRD #1666 part 3 (Q35, R4 §7.5): the card's words for
/// [`WakeHold::NeedsFetch`].
pub const WAKE_HOLD_NEEDS_FETCH: &str = "waiting: ask the agent to run dot-agent-deck inbox fetch; reinstall the deck integration to restore automatic delivery";
/// PRD #1666 part 3 (Q35): the card's words for [`WakeHold::CannotCheck`]
/// when no [`HoldDetail`] this build understands says why.
pub const WAKE_HOLD_CANNOT_CHECK: &str =
    "waiting: the deck can't check this agent's input box. Check its pane.";
/// [`WakeHold::CannotCheck`]'s words with a [`HoldDetail`]: `{cause}` is the
/// cause's sentence ([`HoldCause::template`]), `{remedy}` its remedy.
pub const CANNOT_CHECK_DETAIL: &str =
    "waiting: the deck can't check this agent's input box: {cause}. {remedy}";
/// What a [`HoldCause`] sentence says in place of `{path}` when the detail
/// names none.
pub const HOLD_PATH_UNNAMED: &str = "a path the deck didn't name";
/// What [`HoldCause::Filesystem`]'s sentence says in place of `{filesystem}`
/// when the detail names none.
pub const HOLD_FILESYSTEM_UNNAMED: &str = "a filesystem the deck didn't name";
/// Core §6.3's remedy, with RV7's correction: root's sticky directories
/// above it are allowed, so not every directory above has to be the user's.
/// Text, never a command line.
pub const HOLD_REMEDY_RELOCATE: &str = "Point TMPDIR (or CODEX_HOME) at a directory on ext4, XFS, Btrfs or tmpfs (Linux), or APFS or HFS+ (macOS), owned by you, with no group or other write permission on it or on any directory above it other than a sticky directory root owns, such as /tmp.";
/// The remedy for a directory the user owns that others can write to (CX-12
/// option C): it names the command, never joined to the path.
pub const HOLD_REMEDY_REMOVE_WRITE: &str = "Remove the write permission others have on that directory, with chmod g-w (and chmod o-w if others can write too) on it.";
/// The remedy when a read the check needs failed.
pub const HOLD_REMEDY_REREAD: &str =
    "Check that you can read it, or point TMPDIR (or CODEX_HOME) at another directory.";

/// PRD #1666 part 3 (Q35, core §7): the card's words for each
/// [`InputHeldCause`], shown while the deck holds what the user typed.
pub const INPUT_HELD_CHECKING: &str =
    "input held: the deck is checking the input box; your typing will appear in a moment";
/// [`InputHeldCause::CheckNotOpened`]'s words.
pub const INPUT_HELD_CHECK_NOT_OPENED: &str = "input held: the agent hasn't opened the deck's check yet; your typing is kept until it does. Restarting the agent discards it.";
/// [`InputHeldCause::CheckNotReturned`]'s words.
pub const INPUT_HELD_CHECK_NOT_RETURNED: &str = "input held: the agent hasn't come back from the deck's check; your typing is kept until it does. Restarting the agent discards it.";
/// [`InputHeldCause::AfterYourCtrlG`]'s words.
pub const INPUT_HELD_AFTER_YOUR_CTRL_G: &str = "input held: the agent didn't open the editor for your Ctrl+G; your typing is kept. Restarting the agent discards it.";
/// [`InputHeldCause::QueueFull`]'s words.
pub const INPUT_HELD_QUEUE_FULL: &str =
    "input held: too much typing is waiting; the rest is paused";
/// [`InputHeldCause::CheckBlocked`]'s words.
pub const INPUT_HELD_CHECK_BLOCKED: &str =
    "waiting: can't check this input box; press Ctrl+G to let the deck check it";
/// [`InputHeldCause::Unknown`]'s words: a cause a newer deck names.
pub const INPUT_HELD_UNKNOWN: &str =
    "input held: your typing is kept while the deck checks this input box";
/// PRD #1666 part 3 (Q35-20e, Q104b): the successor's card, when the
/// generation it replaced still held typing as it retired
/// ([`InputDiscarded`]). Its first clause stands alone where a card cuts it.
pub const INPUT_NOT_DELIVERED: &str = "not delivered: your held typing; restarting the agent discarded it. Type it again if you still need it.";

/// What both clients say when the daemon refuses a pane's typed input
/// (`KIND_STREAM_REJECT`) for a reason it names: the TUI on its status line,
/// the desktop inside the terminal tile. One copy, pinned with the desktop's
/// by `tests/fixtures/inbox-display.json`'s `inputRejected` (CLAUDE.md rule
/// 22, PRD #1666 part 3 Q15-2).
pub const INPUT_REJECTED_HISTORY_ONLY: &str = "Input not delivered: session is history-only";
/// [`input_rejected`]'s words for `exited`.
pub const INPUT_REJECTED_EXITED: &str = "Input not delivered: the agent has exited";
/// [`input_rejected`]'s words for `write-failed`: the agent's terminal took
/// none or only part of it, so it is not reported as the agent exiting.
pub const INPUT_REJECTED_WRITE_FAILED: &str =
    "Input not delivered: the agent's terminal did not take it";
/// [`input_rejected`]'s words for `stale` and `wrong-session`.
pub const INPUT_REJECTED_SESSION_CHANGED: &str = "Input not delivered: the pane's session changed";
/// [`input_rejected`]'s words for a refusal that names no reason.
pub const INPUT_REJECTED_UNNAMED: &str = "Input not delivered";

/// The words for a refusal of typed input whose reason is `reason` (see
/// [`INPUT_REJECTED_EXITED`]). A reason this deck doesn't know is shown in
/// brackets after [`INPUT_REJECTED_UNNAMED`], so a newer daemon's reason
/// still reads as a refusal; the caller makes it safe to display first.
pub fn input_rejected(reason: &str) -> String {
    match reason.trim() {
        "" => INPUT_REJECTED_UNNAMED.to_string(),
        "history-only" => INPUT_REJECTED_HISTORY_ONLY.to_string(),
        "exited" => INPUT_REJECTED_EXITED.to_string(),
        "write-failed" => INPUT_REJECTED_WRITE_FAILED.to_string(),
        "stale" | "wrong-session" => INPUT_REJECTED_SESSION_CHANGED.to_string(),
        other => format!("{INPUT_REJECTED_UNNAMED} ({other})"),
    }
}

/// PRD #1666 part 3, round 5-wake (Q6): the deck-wide attention item names a
/// held orchestrator and counts the other held recipients after it
/// (`· 1 other held`); with no orchestrator held it only counts (`✉ 2 held`).
pub const ATTENTION_OTHER_HELD: &str = "other held";
/// [`ATTENTION_OTHER_HELD`]'s count when no orchestrator is named.
pub const ATTENTION_HELD: &str = "held";

/// The deck-wide attention item both clients show in their header, the same
/// words in each: `✉ <orchestrator>: <its hold words> · 1 other held`, or
/// `✉ 2 held` when no orchestrator is held; `None` when nothing is held.
/// `named` is the held orchestrator's name and [`CardInbox::attention_words`];
/// `others` counts every other held recipient, other orchestrators included.
pub fn deck_attention_text(named: Option<(&str, &str)>, others: usize) -> Option<String> {
    match named {
        Some((name, words)) if others > 0 => Some(format!(
            "{GLYPH} {name}: {words} · {others} {ATTENTION_OTHER_HELD}"
        )),
        Some((name, words)) => Some(format!("{GLYPH} {name}: {words}")),
        None => deck_attention_count(others),
    }
}

/// [`deck_attention_text`] counting every one of the `held` recipients and
/// naming none (`✉ 2 held`): its words when no orchestrator is held, and what
/// the TUI shows in their place on a row too narrow for them.
pub fn deck_attention_count(held: usize) -> Option<String> {
    (held > 0).then(|| format!("{GLYPH} {held} {ATTENTION_HELD}"))
}

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
/// The card notice for a message whose agent's pane ended while it was still
/// owed, when the deck can't say how ([`PaneTermination::Unknown`]). PRD #1666
/// part 3, round 5-wake (plan rev 2 §6): the deck sees its own child and the
/// pane's terminal, never the agent below them, so no notice says the agent
/// exited.
pub const NOTICE_AGENT_EXITED: &str =
    "not delivered: this agent's pane ended before a deck message reached it";
/// The card notice when the command the deck started in the pane exited.
pub const NOTICE_PANE_COMMAND_EXITED: &str = "not delivered: the command started in this agent's pane exited before a deck message reached it";
/// The card notice when the deck's connection to the pane ended while the
/// command it started there still ran, or its exit could not be read.
pub const NOTICE_CONNECTION_ENDED: &str = "not delivered: the deck's connection to this agent's pane ended before a deck message reached it";

/// PRD #1666 part 3, round 5-wake (plan rev 2 §6): what the deck observed
/// ending an agent generation's pane, on the notice of a message that became
/// undeliverable because of it (`agent_exited`). It names the process the deck
/// saw — the command it started in the pane, or that command's report through
/// the deck's wrapper — never the agent, which can be a descendant of either.
/// Additive on the wire: absent from an older deck, ignored by an older client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneTermination {
    /// The command the deck started in the pane exited with `code`.
    PaneCommandExited {
        code: u32,
        reported_by: TerminationSource,
    },
    /// The pane's output ended (`read_end`: `eof`, or the read error) while
    /// the command the deck started there had not exited, as far as the deck
    /// could tell in time.
    ConnectionEnded { read_end: String },
    /// The generation was removed by a respawn that published a successor in
    /// its pane. A record of that publication, not of the pane now: the
    /// successor may since have exited or been closed.
    Replaced,
    /// The generation was removed by an explicit close or the deck's
    /// shutdown. A record of that removal, not of the pane now.
    Closed,
    /// The pane ended and the deck cannot say how: a respawn removed the
    /// generation without a successor confirmed, or a newer deck names a
    /// kind this build does not know.
    #[serde(other)]
    Unknown,
}

/// Who reported [`PaneTermination::PaneCommandExited`]'s code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationSource {
    /// The deck's own wait on the process it started in the pane.
    Deck,
    /// The deck's wrapper, for the command it ran in the pane.
    Wrapper,
    /// A source a newer deck names.
    #[serde(other)]
    Unknown,
}

impl PaneTermination {
    /// Why a message the generation never took became undeliverable, as the
    /// orchestrator's notice and the desktop's events drawer say it (the
    /// shared words fixture's `terminations`, with `{code}` filled in).
    pub fn words(&self) -> String {
        match self {
            PaneTermination::PaneCommandExited { code, .. } => {
                TERMINATION_PANE_COMMAND_EXITED.replace("{code}", &code.to_string())
            }
            PaneTermination::ConnectionEnded { .. } => TERMINATION_CONNECTION_ENDED.to_string(),
            PaneTermination::Replaced => TERMINATION_REPLACED.to_string(),
            PaneTermination::Closed => TERMINATION_CLOSED.to_string(),
            PaneTermination::Unknown => TERMINATION_UNKNOWN.to_string(),
        }
    }

    /// The exited generation's card notice.
    pub fn card_notice(&self) -> &'static str {
        match self {
            PaneTermination::PaneCommandExited { .. } => NOTICE_PANE_COMMAND_EXITED,
            PaneTermination::ConnectionEnded { .. } => NOTICE_CONNECTION_ENDED,
            _ => NOTICE_AGENT_EXITED,
        }
    }
}

/// [`PaneTermination::words`], by kind.
pub const TERMINATION_PANE_COMMAND_EXITED: &str =
    "the command started in the agent's pane exited (code {code}) before taking this message";
pub const TERMINATION_CONNECTION_ENDED: &str = "the deck's connection to the agent's pane ended before it took this message; whether the agent is still running is unknown";
pub const TERMINATION_REPLACED: &str = "the agent's pane was replaced before it took this message; the message stays with the previous agent";
pub const TERMINATION_CLOSED: &str = "the pane was closed before the agent took this message";
pub const TERMINATION_UNKNOWN: &str = "the agent's pane ended before it took this message";

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
    /// PRD #1666 part 3 (Q35): the deck holds its notice until the agent
    /// fetches its messages itself. Tolerant: an older client reads it as
    /// [`WakeHold::Unknown`]. No deck sets it yet.
    NeedsFetch,
    /// PRD #1666 part 3 (Q35): the deck can't check the agent's input box,
    /// for the cause [`HoldDetail`] names beside it. Tolerant and unset, as
    /// [`WakeHold::NeedsFetch`] is.
    CannotCheck,
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
            WakeHold::NeedsFetch => Some(WAKE_HOLD_NEEDS_FETCH),
            WakeHold::CannotCheck => Some(WAKE_HOLD_CANNOT_CHECK),
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
            WakeHold::NeedsFetch => Some(
                "the deck can't wake the worker, so it takes its messages when it runs dot-agent-deck inbox fetch",
            ),
            WakeHold::CannotCheck => Some(
                "the deck can't check the worker's input box, so it doesn't type its notice there; the user can check its pane",
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
            WakeHold::NeedsFetch => 10,
            WakeHold::CannotCheck => 11,
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
            10 => Some(Some(WakeHold::NeedsFetch)),
            11 => Some(Some(WakeHold::CannotCheck)),
            _ => None,
        }
    }
}

/// PRD #1666 part 3 (Q35, OQ1): why the deck can't check an agent's input
/// box, carried beside [`WakeHold::CannotCheck`] and cleared with it. A typed
/// cause, plus the refused path component and its filesystem's name as
/// display text, for a client to escape and never run. Additive: an older
/// client ignores it, and a cause this build does not know reads as
/// [`HoldCause::Unknown`]. No deck sets it yet.
///
/// Both strings are cut to [`MAX_HOLD_DETAIL_CHARS`] as they are read, so a
/// peer that sends more is bounded whichever constructor it bypassed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct HoldDetail {
    pub cause: HoldCause,
    /// The path component the check stopped at, at most
    /// [`MAX_HOLD_DETAIL_CHARS`] characters.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "bounded_detail"
    )]
    pub path_component: Option<String>,
    /// That component's filesystem, at most [`MAX_HOLD_DETAIL_CHARS`]
    /// characters.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "bounded_detail"
    )]
    pub filesystem: Option<String>,
}

/// The first [`MAX_HOLD_DETAIL_CHARS`] characters of `text`.
fn bounded_detail_text(text: &str) -> String {
    text.chars().take(MAX_HOLD_DETAIL_CHARS).collect()
}

/// [`HoldDetail`]'s strings as they are read: cut to
/// [`MAX_HOLD_DETAIL_CHARS`].
fn bounded_detail<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text: Option<String> = serde::Deserialize::deserialize(deserializer)?;
    Ok(text.map(|text| bounded_detail_text(&text)))
}

/// A [`HoldDetail`] string as a card shows it: at most
/// [`MAX_HOLD_DETAIL_CHARS`] characters of it, with control and bidi
/// characters escaped so they show rather than act
/// ([`crate::untrusted_text::escape_control_and_bidi`]). The desktop's
/// `holdDetailText` is the same.
pub fn hold_detail_display(text: &str) -> String {
    crate::untrusted_text::escape_control_and_bidi(&bounded_detail_text(text))
}

/// The longest [`HoldDetail`] string kept, in characters.
pub const MAX_HOLD_DETAIL_CHARS: usize = 256;

impl HoldDetail {
    /// `cause` with its strings cut to [`MAX_HOLD_DETAIL_CHARS`].
    pub fn new(cause: HoldCause, path_component: Option<&str>, filesystem: Option<&str>) -> Self {
        HoldDetail {
            cause,
            path_component: path_component.map(bounded_detail_text),
            filesystem: filesystem.map(bounded_detail_text),
        }
    }

    /// The card's words for [`WakeHold::CannotCheck`] with this detail, after
    /// the glyph and the pending count ([`CANNOT_CHECK_DETAIL`]); `None` for a
    /// cause this build does not know, which keeps
    /// [`WAKE_HOLD_CANNOT_CHECK`] and names none of the detail's path.
    pub fn words(&self) -> Option<String> {
        let template = self.cause.template()?;
        let path = self.path_component.as_deref().map_or_else(
            || HOLD_PATH_UNNAMED.to_string(),
            |path| format!("\u{201c}{}\u{201d}", hold_detail_display(path)),
        );
        let filesystem = self
            .filesystem
            .as_deref()
            .map_or_else(|| HOLD_FILESYSTEM_UNNAMED.to_string(), hold_detail_display);
        let cause = fill(
            template,
            &[("{path}", &path), ("{filesystem}", &filesystem)],
        );
        Some(fill(
            CANNOT_CHECK_DETAIL,
            &[("{cause}", &cause), ("{remedy}", self.cause.remedy())],
        ))
    }
}

/// `template` with each placeholder replaced by its value, in one pass, so
/// what a value says is shown and never read as a placeholder.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        match values.iter().find(|(key, _)| rest.starts_with(key)) {
            Some((key, value)) => {
                out.push_str(value);
                rest = &rest[key.len()..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What a [`HoldDetail`] says stopped the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldCause {
    /// A component is on a filesystem the check doesn't trust.
    Filesystem,
    /// A directory the user owns can be written by others.
    WritableDirectory,
    /// A component is owned by neither the user nor root, or is another
    /// user's inside a shared directory.
    ForeignOwner,
    /// An ACL grants write to others, or can't be read.
    Acl,
    /// The agent's exported file isn't one the check can trust.
    ExportFile,
    /// A read the check needs failed.
    Io,
    /// A cause a newer deck names: a client shows its generic words.
    #[serde(other)]
    Unknown,
}

impl HoldCause {
    /// The cause's sentence in [`CANNOT_CHECK_DETAIL`], with `{path}` for the
    /// path component and `{filesystem}` for its filesystem; `None` for
    /// [`HoldCause::Unknown`].
    pub fn template(self) -> Option<&'static str> {
        match self {
            HoldCause::Filesystem => Some(HOLD_CAUSE_FILESYSTEM),
            HoldCause::WritableDirectory => Some(HOLD_CAUSE_WRITABLE_DIRECTORY),
            HoldCause::ForeignOwner => Some(HOLD_CAUSE_FOREIGN_OWNER),
            HoldCause::Acl => Some(HOLD_CAUSE_ACL),
            HoldCause::ExportFile => Some(HOLD_CAUSE_EXPORT_FILE),
            HoldCause::Io => Some(HOLD_CAUSE_IO),
            HoldCause::Unknown => None,
        }
    }

    /// What the user can do about the cause: remove others' write from a
    /// directory of theirs, read the path again, or move the check to a
    /// directory that passes.
    pub fn remedy(self) -> &'static str {
        match self {
            HoldCause::WritableDirectory => HOLD_REMEDY_REMOVE_WRITE,
            HoldCause::Io => HOLD_REMEDY_REREAD,
            HoldCause::Filesystem
            | HoldCause::ForeignOwner
            | HoldCause::Acl
            | HoldCause::ExportFile
            | HoldCause::Unknown => HOLD_REMEDY_RELOCATE,
        }
    }
}

/// [`HoldCause::Filesystem`]'s sentence.
pub const HOLD_CAUSE_FILESYSTEM: &str =
    "{path} is on {filesystem}, which the check doesn't support";
/// [`HoldCause::WritableDirectory`]'s sentence.
pub const HOLD_CAUSE_WRITABLE_DIRECTORY: &str = "others can write to {path}, a directory you own";
/// [`HoldCause::ForeignOwner`]'s sentence.
pub const HOLD_CAUSE_FOREIGN_OWNER: &str = "{path} belongs to another user";
/// [`HoldCause::Acl`]'s sentence.
pub const HOLD_CAUSE_ACL: &str =
    "an access control list on {path} lets others write to it, or can't be read";
/// [`HoldCause::ExportFile`]'s sentence.
pub const HOLD_CAUSE_EXPORT_FILE: &str = "the agent's file {path} isn't one the check can trust";
/// [`HoldCause::Io`]'s sentence.
pub const HOLD_CAUSE_IO: &str = "the deck couldn't read {path}";

/// PRD #1666 part 3 (Q35, R5 §10.1): the deck is holding what the user typed
/// into an agent's pane while it checks the agent's input box. Bound to the
/// agent generation it names. Additive: an older client ignores it, and its
/// typing simply appears when the hold ends. No deck sets it yet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputHeld {
    pub generation: u64,
    pub cause: InputHeldCause,
    /// Writes held.
    pub held_ops: u32,
    /// Bytes held.
    pub held_bytes: u32,
    /// When the hold began, in milliseconds since the Unix epoch.
    pub since_ms: u64,
}

/// PRD #1666 part 3 (Q35-20e, Q104b): the typing the deck held for a pane's
/// previous agent generation when that generation retired, which never
/// reached it. Carried on the pane's later generation, whose card says so
/// ([`INPUT_NOT_DELIVERED`]); bound to the retired `generation`, so it never
/// shows on that generation's own card. Additive: an older client ignores it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputDiscarded {
    /// The retired generation that held it.
    pub generation: u64,
    /// Writes discarded.
    pub held_ops: u32,
    /// Bytes discarded.
    pub held_bytes: u32,
    /// When it retired, in milliseconds since the Unix epoch.
    pub at_ms: u64,
}

/// Why an [`InputHeld`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputHeldCause {
    /// A check is running.
    Checking,
    /// The agent hasn't opened the deck's check.
    CheckNotOpened,
    /// The agent hasn't come back from the deck's check.
    CheckNotReturned,
    /// The agent didn't open the editor for the user's own Ctrl+G.
    AfterYourCtrlG,
    /// Too much typing is waiting; the rest is paused.
    QueueFull,
    /// The deck can't check this input box until the user presses Ctrl+G.
    CheckBlocked,
    /// A cause a newer deck names: a client shows its generic words.
    #[serde(other)]
    Unknown,
}

impl InputHeldCause {
    /// The card's words for the hold (core §7), the same in both clients.
    pub fn words(self) -> &'static str {
        match self {
            InputHeldCause::Checking => INPUT_HELD_CHECKING,
            InputHeldCause::CheckNotOpened => INPUT_HELD_CHECK_NOT_OPENED,
            InputHeldCause::CheckNotReturned => INPUT_HELD_CHECK_NOT_RETURNED,
            InputHeldCause::AfterYourCtrlG => INPUT_HELD_AFTER_YOUR_CTRL_G,
            InputHeldCause::QueueFull => INPUT_HELD_QUEUE_FULL,
            InputHeldCause::CheckBlocked => INPUT_HELD_CHECK_BLOCKED,
            InputHeldCause::Unknown => INPUT_HELD_UNKNOWN,
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
/// Beside [`CARD_INBOX_METADATA_KEY`]: [`CardInbox::hold_detail`], as JSON.
pub const CARD_INBOX_HOLD_DETAIL_METADATA_KEY: &str = "inbox_card_hold_detail";
/// Beside [`CARD_INBOX_METADATA_KEY`]: [`CardInbox::input_held`], its wire
/// value.
pub const CARD_INBOX_INPUT_HELD_METADATA_KEY: &str = "inbox_card_input_held";
/// Beside [`CARD_INBOX_METADATA_KEY`]: present when
/// [`CardInbox::input_discarded`] is set.
pub const CARD_INBOX_INPUT_DISCARDED_METADATA_KEY: &str = "inbox_card_input_discarded";
/// Beside [`CARD_INBOX_METADATA_KEY`]: the text of the agent's card notice,
/// when [`CardInbox::agent_notice`] holds one (PRD #2050).
pub const CARD_INBOX_AGENT_NOTICE_METADATA_KEY: &str = "inbox_card_agent_notice";

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
    /// PRD #1666 part 3 (Q35): why the deck can't check the agent's input
    /// box, beside [`WakeHold::CannotCheck`] and replaced with it.
    pub hold_detail: Option<HoldDetail>,
    /// PRD #1666 part 3 (Q35): why the deck holds what the user typed into
    /// the agent's pane, from the daemon's last snapshot of it.
    pub input_held: Option<InputHeldCause>,
    /// PRD #1666 part 3 (Q35-20e): typing the pane's previous generation held
    /// was discarded when it retired ([`InputDiscarded`]).
    pub input_discarded: bool,
    /// PRD #2050: the daemon's notice about a way the agent runs that the deck
    /// cannot fully support ([`crate::agent_notice::AgentNotice::display_text`]).
    pub agent_notice: Option<String>,
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
        if self.pending > 0 && (self.stalled || self.hold.is_some_and(|h| h.words().is_none())) {
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
    /// deck has something to say beyond the line — the first that holds of:
    /// the daemon's notice about a way the agent runs that the deck cannot
    /// fully support (`agent_notice`, PRD #2050), which explains everything
    /// below it; a refused request of the agent's own for its messages (PRD
    /// #1666 part 3, R-2), since nothing else reaches it until that is fixed;
    /// why the integration takes nothing yet; paused hand-outs; what an
    /// uncertain delivery means (what running out of attempts means, when
    /// every uncertain one has: its row offers no Retry); what a message the
    /// agent may not have received means; held_long attention; then failed
    /// receipt storage.
    pub fn notice_text(&self) -> Option<String> {
        // PRD #2050: first, because it explains why the agent's integration
        // takes nothing (or reports nothing) and says what to do about it.
        if let Some(notice) = self.agent_notice.as_deref().filter(|n| !n.is_empty()) {
            return Some(notice.to_string());
        }
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
    /// `✉2 waiting: there may be an unsent draft. Send it, or type x then
    /// Ctrl+C.`. With nothing pending, only the two warnings that stand on their
    /// own (the deck's line may be in the input, a dialog may have taken its
    /// keys); `None` otherwise and for a hold this build does not know.
    pub fn hold_text(&self) -> Option<String> {
        let hold = self.hold?;
        let words = self.hold_words()?;
        if self.pending > 0 {
            Some(format!("{GLYPH}{} {words}", self.pending))
        } else {
            hold.alone_words().map(str::to_string)
        }
    }

    /// The hold's words: [`WakeHold::CannotCheck`]'s with the cause, path
    /// and remedy its [`HoldDetail`] names, when this build knows the cause
    /// ([`HoldDetail::words`]), and [`WakeHold::words`] otherwise. A detail
    /// beside any other hold is a stale one and says nothing.
    fn hold_words(&self) -> Option<String> {
        let hold = self.hold?;
        if hold == WakeHold::CannotCheck
            && let Some(words) = self.hold_detail.as_ref().and_then(HoldDetail::words)
        {
            return Some(words);
        }
        hold.words().map(str::to_string)
    }

    /// PRD #1666 part 3 (Q35, core §7): the row saying the deck holds what
    /// the user typed, and why ([`InputHeldCause::words`]), whatever else the
    /// card shows; with nothing held, that the previous generation's held
    /// typing was not delivered ([`INPUT_NOT_DELIVERED`], Q35-20e).
    pub fn input_held_text(&self) -> Option<&'static str> {
        self.input_held
            .map(InputHeldCause::words)
            .or(self.input_discarded.then_some(INPUT_NOT_DELIVERED))
    }

    /// PRD #1666 part 3, round 5-wake (Q6): the words the deck-wide attention
    /// item gives this agent's hold, without the count: the hold's words while
    /// messages wait ([`WAKE_STALLED`]'s for a hold this build does not
    /// know), the two warnings that stand alone with nothing waiting, and
    /// `None` when the deck holds nothing for it.
    pub fn attention_words(&self) -> Option<String> {
        let hold = self.hold?;
        if self.pending == 0 {
            return hold.alone_words().map(str::to_string);
        }
        Some(
            self.hold_words()
                .unwrap_or_else(|| format!("{WAKE_STALLED}: {WAKE_STALLED_HINT}")),
        )
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

    /// PRD #1666 part 3, round 5-wake (Q6): the deck-wide attention item, in
    /// the forms `deckAttentionText` (`desktop/src/lib/inboxWords.ts`) builds
    /// from the same words; its tests pin the same strings.
    #[test]
    fn the_deck_attention_names_one_orchestrator_and_counts_the_rest() {
        assert_eq!(
            deck_attention_text(Some(("Captain", WAKE_HOLD_POSSIBLE_DRAFT)), 1).as_deref(),
            Some(
                "✉ Captain: waiting: there may be an unsent draft. Send it, or type x then Ctrl+C. · 1 other held"
            )
        );
        assert_eq!(
            deck_attention_text(Some(("Captain", WAKE_HOLD_PASTE_OPEN)), 0).as_deref(),
            Some("✉ Captain: waiting: a paste into its input has not finished.")
        );
        assert_eq!(deck_attention_text(None, 2).as_deref(), Some("✉ 2 held"));
        assert_eq!(deck_attention_text(None, 0), None);
    }

    /// PRD #1666 part 3 (Q35): a cannot-check detail in words, the exact
    /// strings `holdDetailWords`' tests in `desktop/src/lib/inboxWords.test.ts`
    /// pin, so the two clients say the same sentence.
    #[test]
    fn a_hold_detail_names_its_cause_path_and_remedy() {
        let detail = HoldDetail::new(HoldCause::Filesystem, Some("/mnt/nfs/tmp"), Some("nfs"));
        assert_eq!(
            detail.words().as_deref(),
            Some(
                "waiting: the deck can't check this agent's input box: \u{201c}/mnt/nfs/tmp\u{201d} is on nfs, which the check doesn't support. Point TMPDIR (or CODEX_HOME) at a directory on ext4, XFS, Btrfs or tmpfs (Linux), or APFS or HFS+ (macOS), owned by you, with no group or other write permission on it or on any directory above it other than a sticky directory root owns, such as /tmp."
            )
        );
        assert_eq!(
            HoldDetail::new(HoldCause::WritableDirectory, None, None)
                .words()
                .as_deref(),
            Some(
                "waiting: the deck can't check this agent's input box: others can write to a path the deck didn't name, a directory you own. Remove the write permission others have on that directory, with chmod g-w (and chmod o-w if others can write too) on it."
            )
        );
        assert_eq!(
            HoldDetail::new(HoldCause::Filesystem, Some("/x"), None)
                .words()
                .as_deref(),
            Some(
                "waiting: the deck can't check this agent's input box: \u{201c}/x\u{201d} is on a filesystem the deck didn't name, which the check doesn't support. Point TMPDIR (or CODEX_HOME) at a directory on ext4, XFS, Btrfs or tmpfs (Linux), or APFS or HFS+ (macOS), owned by you, with no group or other write permission on it or on any directory above it other than a sticky directory root owns, such as /tmp."
            )
        );
        assert_eq!(
            HoldDetail::new(HoldCause::Io, Some("/a"), None)
                .words()
                .as_deref(),
            Some(
                "waiting: the deck can't check this agent's input box: the deck couldn't read \u{201c}/a\u{201d}. Check that you can read it, or point TMPDIR (or CODEX_HOME) at another directory."
            )
        );
        assert_eq!(
            HoldDetail::new(HoldCause::Unknown, Some("/a"), None).words(),
            None
        );
    }

    /// What a detail says is shown as it is: a path naming a placeholder is
    /// not filled in, and its controls and bidi characters are escaped.
    #[test]
    fn a_hold_detail_shows_its_text_and_never_fills_it() {
        let words = HoldDetail::new(
            HoldCause::Filesystem,
            Some("/a/{filesystem}\n\u{202e}"),
            Some("{path}\u{1b}"),
        )
        .words()
        .unwrap();
        assert!(
            words
                .contains("\u{201c}/a/{filesystem}\\n\\u{202e}\u{201d} is on {path}\\u{1b}, which"),
            "{words}"
        );
        assert_eq!(fill("{a}{b}{", &[("{a}", "{b}"), ("{b}", "x")]), "{b}x{");
        assert_eq!(fill("no {placeholder}", &[]), "no {placeholder}");
    }

    /// A detail read off the wire is bounded whichever constructor its
    /// sender bypassed, and shown bounded too.
    #[test]
    fn a_hold_detail_off_the_wire_is_bounded() {
        let long = "é".repeat(MAX_HOLD_DETAIL_CHARS + 10);
        let detail: HoldDetail = serde_json::from_value(serde_json::json!({
            "cause": "io", "path_component": long, "filesystem": long
        }))
        .unwrap();
        for text in [&detail.path_component, &detail.filesystem] {
            assert_eq!(
                text.as_deref().map(|text| text.chars().count()),
                Some(MAX_HOLD_DETAIL_CHARS)
            );
        }
        assert_eq!(
            hold_detail_display(&long).chars().count(),
            MAX_HOLD_DETAIL_CHARS
        );
        let absent: HoldDetail =
            serde_json::from_value(serde_json::json!({"cause": "acl"})).unwrap();
        assert_eq!(absent, HoldDetail::new(HoldCause::Acl, None, None));
    }

    /// The card's hold row: a cannot-check detail it understands names the
    /// cause, one it does not keeps the generic words, and a detail beside
    /// any other hold says nothing. A hold with no words keeps the stall's
    /// words on the inbox line, stalled or not; the held-typing row stands on
    /// its own.
    #[test]
    fn the_card_shows_a_cannot_check_detail_and_held_typing() {
        let card = |hold, detail: Option<HoldDetail>| CardInbox {
            pending: 1,
            hold: Some(hold),
            hold_detail: detail,
            ..CardInbox::default()
        };
        let io = HoldDetail::new(HoldCause::Io, Some("/a"), None);
        assert_eq!(
            card(WakeHold::CannotCheck, Some(io.clone())).hold_text(),
            Some(format!("{GLYPH}1 {}", io.words().unwrap()))
        );
        assert_eq!(
            card(WakeHold::CannotCheck, Some(io.clone())).attention_words(),
            io.words()
        );
        assert_eq!(
            card(
                WakeHold::CannotCheck,
                Some(HoldDetail::new(HoldCause::Unknown, Some("/a"), None))
            )
            .hold_text(),
            Some(format!("{GLYPH}1 {WAKE_HOLD_CANNOT_CHECK}"))
        );
        assert_eq!(
            card(WakeHold::NeedsFetch, Some(io)).hold_text(),
            Some(format!("{GLYPH}1 {WAKE_HOLD_NEEDS_FETCH}"))
        );
        assert_eq!(
            card(WakeHold::Unknown, None).full_text(),
            Some(format!("{GLYPH}1 {WAKE_STALLED}: {WAKE_STALLED_HINT}"))
        );
        assert_eq!(
            card(WakeHold::CannotCheck, None).full_text(),
            Some(format!("{GLYPH}1"))
        );
        let held = CardInbox {
            input_held: Some(InputHeldCause::QueueFull),
            ..CardInbox::default()
        };
        assert_eq!(held.input_held_text(), Some(INPUT_HELD_QUEUE_FULL));
        assert_eq!(held.full_text(), None);
        assert_eq!(held.notice_text(), None);
        assert_eq!(CardInbox::default().input_held_text(), None);
        // Q35-20e: the predecessor's discarded typing, until the successor
        // holds typing of its own.
        let discarded = CardInbox {
            input_discarded: true,
            ..CardInbox::default()
        };
        assert_eq!(discarded.input_held_text(), Some(INPUT_NOT_DELIVERED));
        assert_eq!(
            CardInbox {
                input_held: Some(InputHeldCause::Checking),
                ..discarded
            }
            .input_held_text(),
            Some(INPUT_HELD_CHECKING)
        );
    }

    /// The overdue notice names the new holds too, and nothing for one this
    /// build does not know.
    #[test]
    fn the_overdue_notice_names_every_hold_it_knows() {
        for hold in [WakeHold::NeedsFetch, WakeHold::CannotCheck] {
            assert!(
                hold.notice_clause()
                    .is_some_and(|clause| clause.contains("worker"))
            );
            assert!(hold.words().is_some());
            assert_eq!(hold.alone_words(), None);
        }
        assert_eq!(WakeHold::Unknown.notice_clause(), None);
    }

    /// The attention item's words for one agent: the hold's words while
    /// messages wait, the stall's for a hold this build does not know, the
    /// standalone warnings with nothing waiting, and nothing otherwise.
    #[test]
    fn attention_words_follow_the_card_hold() {
        let card = |pending, hold| CardInbox {
            pending,
            hold,
            ..CardInbox::default()
        };
        assert_eq!(
            card(2, Some(WakeHold::CannotType))
                .attention_words()
                .as_deref(),
            Some(WAKE_HOLD_CANNOT_TYPE)
        );
        assert_eq!(
            card(1, Some(WakeHold::Unknown))
                .attention_words()
                .as_deref(),
            Some("waiting to wake: send it a prompt")
        );
        assert_eq!(
            card(0, Some(WakeHold::DialogDuringRing))
                .attention_words()
                .as_deref(),
            Some(WAKE_HOLD_DIALOG_DURING_RING_ALONE)
        );
        assert_eq!(
            card(0, Some(WakeHold::PossibleDraft)).attention_words(),
            None
        );
        assert_eq!(card(0, Some(WakeHold::Unknown)).attention_words(), None);
        assert_eq!(card(3, None).attention_words(), None);
    }

    /// PRD #1666 part 3, round 5-wake (plan rev 2 §6, §8): `termination`
    /// is additive and internally tagged; a kind or a source a newer deck
    /// names reads as unknown, with the unknown words, rather than failing
    /// the notice, and every kind's words and card notice stop at the pane.
    #[test]
    fn a_termination_decodes_across_versions() {
        let decode = |value: serde_json::Value| -> PaneTermination {
            serde_json::from_value(value).unwrap()
        };
        let exited = PaneTermination::PaneCommandExited {
            code: 7,
            reported_by: TerminationSource::Wrapper,
        };
        assert_eq!(
            serde_json::to_value(&exited).unwrap(),
            serde_json::json!({"kind": "pane_command_exited", "code": 7, "reported_by": "wrapper"})
        );
        assert_eq!(decode(serde_json::to_value(&exited).unwrap()), exited);
        assert_eq!(
            decode(serde_json::json!({"kind": "replaced"})),
            PaneTermination::Replaced
        );
        assert_eq!(
            decode(serde_json::json!({"kind": "meteor_strike", "crater": 3})),
            PaneTermination::Unknown
        );
        assert_eq!(PaneTermination::Unknown.words(), TERMINATION_UNKNOWN);
        assert_eq!(
            decode(
                serde_json::json!({"kind": "pane_command_exited", "code": 0, "reported_by": "telepathy"})
            ),
            PaneTermination::PaneCommandExited {
                code: 0,
                reported_by: TerminationSource::Unknown
            }
        );
        let notice = |extra: serde_json::Value| -> crate::event::InboxNotice {
            let mut value = serde_json::json!({
                "pane_id": "p", "agent_id": "a", "message_id": "m-1", "subject": "s",
                "from_label": "the orchestrator", "state": "undeliverable", "reason": "agent_exited",
            });
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(value).unwrap()
        };
        let old = notice(serde_json::json!({}));
        assert_eq!(old.termination, None, "an older deck sends none");
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("termination")
                .is_none(),
            "and none is sent when there is none"
        );
        assert_eq!(
            notice(serde_json::json!({"termination": {"kind": "a_newer_kind"}})).termination,
            Some(PaneTermination::Unknown)
        );
        for termination in [
            exited.clone(),
            PaneTermination::ConnectionEnded {
                read_end: "eof".into(),
            },
            PaneTermination::Replaced,
            PaneTermination::Closed,
            PaneTermination::Unknown,
        ] {
            for text in [termination.words(), termination.card_notice().to_string()] {
                assert!(!text.contains("agent exited"), "{text}");
                assert!(!text.contains("agent's process"), "{text}");
            }
        }
        assert_eq!(
            exited.words(),
            "the command started in the agent's pane exited (code 7) before taking this message"
        );
    }

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
            ("needs_fetch", WakeHold::NeedsFetch),
            ("cannot_check", WakeHold::CannotCheck),
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
            Some(9),
            "the fixture lists every hold the deck names"
        );
        // PRD #1666 part 3 (Q35): held typing, and why a check can't run.
        for (value, cause) in [
            ("checking", InputHeldCause::Checking),
            ("check_not_opened", InputHeldCause::CheckNotOpened),
            ("check_not_returned", InputHeldCause::CheckNotReturned),
            ("after_your_ctrl_g", InputHeldCause::AfterYourCtrlG),
            ("queue_full", InputHeldCause::QueueFull),
            ("check_blocked", InputHeldCause::CheckBlocked),
            ("unknown", InputHeldCause::Unknown),
        ] {
            assert_eq!(words["inputHeld"][value], cause.words(), "{value}");
            if cause != InputHeldCause::Unknown {
                assert_eq!(
                    serde_json::to_value(cause).unwrap(),
                    serde_json::json!(value)
                );
            }
        }
        // Q15-2: a refused input, in the words both clients show.
        for (reason, expected) in [
            ("history-only", INPUT_REJECTED_HISTORY_ONLY),
            ("exited", INPUT_REJECTED_EXITED),
            ("write-failed", INPUT_REJECTED_WRITE_FAILED),
            ("stale", INPUT_REJECTED_SESSION_CHANGED),
            ("wrong-session", INPUT_REJECTED_SESSION_CHANGED),
            ("", INPUT_REJECTED_UNNAMED),
        ] {
            assert_eq!(words["inputRejected"][reason], expected, "{reason:?}");
            assert_eq!(input_rejected(reason), expected, "{reason:?}");
        }
        assert_eq!(
            words["inputRejected"]
                .as_object()
                .map(|reasons| reasons.len()),
            Some(6),
            "the fixture lists every refusal the deck names"
        );
        assert_eq!(
            input_rejected("newer-reason"),
            "Input not delivered (newer-reason)"
        );
        assert_eq!(
            words["inputHeld"].as_object().map(|causes| causes.len()),
            Some(7)
        );
        assert_eq!(words["inputNotDelivered"], INPUT_NOT_DELIVERED);
        assert_eq!(words["cannotCheckDetail"], CANNOT_CHECK_DETAIL);
        assert_eq!(words["holdPathUnnamed"], HOLD_PATH_UNNAMED);
        assert_eq!(words["holdFilesystemUnnamed"], HOLD_FILESYSTEM_UNNAMED);
        for (value, cause, remedy) in [
            ("filesystem", HoldCause::Filesystem, "relocate"),
            (
                "writable_directory",
                HoldCause::WritableDirectory,
                "remove_write",
            ),
            ("foreign_owner", HoldCause::ForeignOwner, "relocate"),
            ("acl", HoldCause::Acl, "relocate"),
            ("export_file", HoldCause::ExportFile, "relocate"),
            ("io", HoldCause::Io, "reread"),
        ] {
            assert_eq!(
                serde_json::to_value(cause).unwrap(),
                serde_json::json!(value)
            );
            assert_eq!(
                words["holdCauses"][value],
                cause.template().unwrap(),
                "{value}"
            );
            assert_eq!(words["holdRemedies"][remedy], cause.remedy(), "{value}");
        }
        assert_eq!(
            words["holdCauses"].as_object().map(|causes| causes.len()),
            Some(6)
        );
        assert_eq!(HoldCause::Unknown.template(), None);
        assert_eq!(
            words["holdRemedies"]
                .as_object()
                .map(|remedies| remedies.len()),
            Some(3)
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
        assert_eq!(words["attentionOtherHeld"], ATTENTION_OTHER_HELD);
        assert_eq!(words["attentionHeld"], ATTENTION_HELD);
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
        for (kind, termination) in [
            ("pane_command_exited", TERMINATION_PANE_COMMAND_EXITED),
            ("connection_ended", TERMINATION_CONNECTION_ENDED),
            ("replaced", TERMINATION_REPLACED),
            ("closed", TERMINATION_CLOSED),
            ("unknown", TERMINATION_UNKNOWN),
        ] {
            assert_eq!(words["terminations"][kind], termination, "{kind}");
        }
        assert_eq!(
            words["terminations"].as_object().map(|kinds| kinds.len()),
            Some(5)
        );
        for notice in [
            NOTICE_MAX_DELIVER,
            NOTICE_AGENT_EXITED,
            NOTICE_PANE_COMMAND_EXITED,
            NOTICE_CONNECTION_ENDED,
        ] {
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
