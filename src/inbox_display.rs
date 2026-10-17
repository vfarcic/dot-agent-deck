//! PRD #1666 §2.8: what a client shows of an agent's inbox, and the words it
//! uses.
//!
//! The TUI's card and the desktop app's tile say the same thing about the same
//! state (CLAUDE.md rule 22): `✉2 queued` while messages wait, `✉2 waiting to
//! wake: send it a prompt` once they have waited too long for an agent that is
//! not working, `✉ delivered` once the agent has acknowledged one and nothing
//! is pending, and `not delivered` for a message that could not reach it. The desktop's copy of the
//! words is `desktop/src/lib/inboxWords.ts`; `tests/fixtures/inbox-display.json`
//! pins both to the same values, so a word changes in all three places at once.

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
/// PRD #1666 §11: the capability probe proved the agent's CLI lacks the flag.
pub const CLI_TOO_OLD: &str = "the dot-agent-deck command this agent runs does not support this deck's integration: upgrade dot-agent-deck, then restart the agent";
/// PRD #1666 §11: a reversible readiness notice, cleared by a registration.
pub const NOT_ARMED: &str =
    "this agent's deck integration has not connected yet; its messages wait until it does";
/// PRD #1666 §3: receipts cannot be recorded for this generation; `({error})`
/// follows.
pub const RECEIPTS_DEGRADED: &str =
    "messages may repeat: the deck cannot record message receipts for this agent";

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
}

impl IntegrationNotice {
    pub fn text(self) -> &'static str {
        match self {
            IntegrationNotice::CliTooOld => CLI_TOO_OLD,
            IntegrationNotice::AdapterTooOld => ADAPTER_TOO_OLD,
            IntegrationNotice::NotArmed => NOT_ARMED,
            IntegrationNotice::RefreshFailed => REFRESH_FAILED,
        }
    }

    fn code(self) -> u8 {
        match self {
            IntegrationNotice::CliTooOld => 1,
            IntegrationNotice::AdapterTooOld => 2,
            IntegrationNotice::NotArmed => 3,
            IntegrationNotice::RefreshFailed => 4,
        }
    }

    fn from_code(code: u8) -> Option<Option<Self>> {
        match code {
            0 => Some(None),
            1 => Some(Some(IntegrationNotice::CliTooOld)),
            2 => Some(Some(IntegrationNotice::AdapterTooOld)),
            3 => Some(Some(IntegrationNotice::NotArmed)),
            4 => Some(Some(IntegrationNotice::RefreshFailed)),
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
}

impl AttentionKind {
    /// The row's explanation, the same in both clients.
    pub fn reason(self) -> &'static str {
        match self {
            AttentionKind::Uncertain => UNCERTAIN_REASON,
            AttentionKind::OutOfAttempts => OUT_OF_ATTEMPTS,
            AttentionKind::HeldLong => HELD_LONG,
        }
    }

    /// Whether the item is an uncertain delivery rather than attention.
    pub fn uncertain(self) -> bool {
        !matches!(self, AttentionKind::HeldLong)
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
    /// PRD #1666 §6.3: messages held an hour without a commit.
    pub held_long: usize,
    /// PRD #1666 §11: why the agent's integration cannot take messages yet.
    pub integration: Option<IntegrationNotice>,
    /// PRD #1666 §7: delivered messages whose evidence pauses hand-outs.
    pub evidence_paused: Option<usize>,
    /// PRD #1666 §3: the deck's receipt-storage error for this agent.
    pub receipts_degraded: Option<String>,
}

impl CardInbox {
    /// The card's inbox line, the same words in both clients:
    /// `✉ delivery uncertain` (`✉ 3 deliveries uncertain`) while deliveries
    /// are uncertain, then `✉2 queued`, `✉2 waiting to wake: send it a
    /// prompt`, `✉ delivered`, or nothing. What the card says beside it is
    /// [`Self::notice_text`].
    pub fn full_text(&self) -> Option<String> {
        if self.uncertain == 1 {
            return Some(format!("{GLYPH} {UNCERTAIN}"));
        }
        if self.uncertain > 1 {
            return Some(format!("{GLYPH} {} {UNCERTAIN_MANY}", self.uncertain));
        }
        if self.pending > 0 && self.stalled {
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
    /// nothing yet, paused hand-outs, what an uncertain delivery means,
    /// held_long attention, then failed receipt storage — the first that
    /// holds.
    pub fn notice_text(&self) -> Option<String> {
        if let Some(notice) = self.integration {
            return Some(notice.text().to_string());
        }
        if let Some(waiting) = self.evidence_paused {
            return Some(evidence_paused_text(waiting));
        }
        if self.uncertain > 0 {
            return Some(UNCERTAIN_REASON.to_string());
        }
        if self.held_long > 0 {
            return Some(HELD_LONG.to_string());
        }
        self.receipts_degraded
            .as_deref()
            .map(receipts_degraded_text)
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
            "{},{},{},{},{},{},{}",
            self.pending,
            u8::from(self.delivered),
            u8::from(self.stalled),
            self.uncertain,
            self.held_long,
            self.integration.map_or(0, IntegrationNotice::code),
            self.evidence_paused
                .map_or_else(|| "-".to_string(), |n| n.to_string()),
        )
    }

    /// Reads [`Self::encode`]'s value, and the two- and three-field values
    /// written before the later fields existed; `None` for anything else.
    pub fn decode(value: &str) -> Option<Self> {
        let flag = |text: &str| match text {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let parts: Vec<&str> = value.split(',').collect();
        if !matches!(parts.len(), 2 | 3 | 7) {
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
        if parts.len() == 7 {
            inbox.uncertain = parts[3].parse().ok()?;
            inbox.held_long = parts[4].parse().ok()?;
            inbox.integration = IntegrationNotice::from_code(parts[5].parse().ok()?)?;
            inbox.evidence_paused = match parts[6] {
                "-" => None,
                n => Some(n.parse().ok()?),
            };
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
        assert_eq!(words["uncertain"], UNCERTAIN);
        assert_eq!(words["uncertainMany"], UNCERTAIN_MANY);
        assert_eq!(words["uncertainReason"], UNCERTAIN_REASON);
        assert_eq!(words["outOfAttempts"], OUT_OF_ATTEMPTS);
        assert_eq!(words["heldLong"], HELD_LONG);
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
        assert_eq!(words["receiptsDegraded"], RECEIPTS_DEGRADED);
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
            "1,0,0,0,0,5,-",
            "1,0,0,0,0,0,x",
        ] {
            assert_eq!(CardInbox::decode(bad), None, "{bad}");
        }
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
        ] {
            let blocked = CardInbox {
                integration: Some(notice),
                ..paused.clone()
            };
            assert_eq!(blocked.notice_text().as_deref(), Some(notice.text()));
        }
        assert_eq!(CardInbox::default().notice_text(), None);
    }
}
