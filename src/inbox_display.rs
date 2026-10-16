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

/// Metadata key of the inert journal entry that carries a card's inbox state
/// ([`CardInbox`]). The TUI's `SessionState` has no field for it, so the state
/// rides its event journal the way a live target does, and the journal keeps
/// that entry when it trims (see `crate::state`).
pub const CARD_INBOX_METADATA_KEY: &str = "inbox_card";

/// What a card shows of its agent's inbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardInbox {
    /// Messages queued or taken but not acknowledged.
    pub pending: usize,
    /// The agent has acknowledged a message.
    pub delivered: bool,
    /// PR #1735 fifth review (R5): the deck reports that nothing has woken
    /// the agent for its waiting messages ([`WAKE_STALLED`]).
    pub stalled: bool,
}

impl CardInbox {
    /// The card's inbox line: `✉2 queued`, `✉2 waiting to wake: send it a
    /// prompt`, `✉ delivered`, or nothing.
    pub fn full_text(self) -> Option<String> {
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

    /// The line a narrow card keeps: the pending count alone (`✉2`), and
    /// nothing once nothing is pending.
    pub fn short_text(self) -> Option<String> {
        (self.pending > 0).then(|| format!("{GLYPH}{}", self.pending))
    }

    /// The journal entry's metadata value.
    pub fn encode(self) -> String {
        format!(
            "{},{},{}",
            self.pending,
            u8::from(self.delivered),
            u8::from(self.stalled)
        )
    }

    /// Reads [`Self::encode`]'s value, and the two-field value written before
    /// `stalled` existed; `None` for anything else.
    pub fn decode(value: &str) -> Option<Self> {
        let flag = |text: &str| match text {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let mut parts = value.split(',');
        let pending = parts.next()?.parse().ok()?;
        let delivered = flag(parts.next()?)?;
        let stalled = match parts.next() {
            None => false,
            Some(text) => flag(text)?,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            pending,
            delivered,
            stalled,
        })
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
        };
        assert_eq!(pending.full_text().as_deref(), Some("✉2 queued"));
        assert_eq!(pending.short_text().as_deref(), Some("✉2"));
        let done = CardInbox {
            pending: 0,
            delivered: true,
            stalled: false,
        };
        assert_eq!(done.full_text().as_deref(), Some("✉ delivered"));
        assert_eq!(done.short_text(), None);
        assert_eq!(CardInbox::default().full_text(), None);
        let stalled = CardInbox {
            pending: 1,
            delivered: false,
            stalled: true,
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
                stalled: false,
            },
            CardInbox {
                pending: 2,
                delivered: false,
                stalled: true,
            },
        ] {
            assert_eq!(CardInbox::decode(&inbox.encode()), Some(inbox));
        }
        assert_eq!(
            CardInbox::decode("3,1"),
            Some(CardInbox {
                pending: 3,
                delivered: true,
                stalled: false,
            }),
            "a value written before `stalled` existed still reads"
        );
        for bad in ["", "1", "x,0", "1,2", "-1,0", "1,0,2", "1,0,1,0"] {
            assert_eq!(CardInbox::decode(bad), None, "{bad}");
        }
    }
}
