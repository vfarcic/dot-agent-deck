//! PRD #1666 §2.8: what a client shows of an agent's inbox, and the words it
//! uses.
//!
//! The TUI's card and the desktop app's tile say the same thing about the same
//! state (CLAUDE.md rule 22): `✉2 queued` while messages wait, `✉ delivered`
//! once the agent has acknowledged one and nothing is pending, and `not
//! delivered` for a message that could not reach it. The desktop's copy of the
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
}

impl CardInbox {
    /// The card's inbox line: `✉2 queued`, `✉ delivered`, or nothing.
    pub fn full_text(self) -> Option<String> {
        if self.pending > 0 {
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
        format!("{},{}", self.pending, u8::from(self.delivered))
    }

    /// Reads [`Self::encode`]'s value; `None` for anything else.
    pub fn decode(value: &str) -> Option<Self> {
        let (pending, delivered) = value.split_once(',')?;
        Some(Self {
            pending: pending.parse().ok()?,
            delivered: match delivered {
                "0" => false,
                "1" => true,
                _ => return None,
            },
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
        for notice in [NOTICE_MAX_DELIVER, NOTICE_AGENT_EXITED] {
            assert!(notice.starts_with(NOT_DELIVERED), "{notice}");
        }
    }

    #[test]
    fn the_card_line_says_queued_then_delivered() {
        let pending = CardInbox {
            pending: 2,
            delivered: true,
        };
        assert_eq!(pending.full_text().as_deref(), Some("✉2 queued"));
        assert_eq!(pending.short_text().as_deref(), Some("✉2"));
        let done = CardInbox {
            pending: 0,
            delivered: true,
        };
        assert_eq!(done.full_text().as_deref(), Some("✉ delivered"));
        assert_eq!(done.short_text(), None);
        assert_eq!(CardInbox::default().full_text(), None);
    }

    #[test]
    fn the_journal_value_round_trips_and_refuses_anything_else() {
        for inbox in [
            CardInbox::default(),
            CardInbox {
                pending: 7,
                delivered: true,
            },
        ] {
            assert_eq!(CardInbox::decode(&inbox.encode()), Some(inbox));
        }
        for bad in ["", "1", "x,0", "1,2", "-1,0"] {
            assert_eq!(CardInbox::decode(bad), None, "{bad}");
        }
    }
}
