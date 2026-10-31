//! PRD #2050: what the deck tells the user, on an agent's card, about a way
//! that agent runs which the deck cannot fully support — and what to do about
//! it.
//!
//! The daemon decides ([`crate::opencode_mode`] for OpenCode) and sets the
//! answer with [`crate::inbox_hub::InboxHub::set_agent_notice`], which keeps it
//! in the agent's card notices (`InboxCardNotices::agent_notice`) and tells the
//! clients only of a change. Both clients show the notice's
//! [`AgentNotice::text`] in the card's notice row, ahead of the inbox's own
//! notices. The text is composed here and nowhere else, so the TUI card and
//! the desktop tile say the same words (CLAUDE.md rule 22).
//!
//! Each sentence leads with what to do and stays short, so a card narrow
//! enough to show only its first two rows still shows the remedy.

use serde::{Deserialize, Serialize};

/// Which unsupported way of running a notice reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentNoticeKind {
    /// OpenCode 2 attached to its shared background service, where the deck's
    /// plugin cannot tell this pane from any other.
    OpencodeSharedService,
    /// OpenCode is running in the pane, but the deck's plugin has not reported
    /// from it.
    OpencodeNotReporting,
    /// The OpenCode in the pane is older than the oldest version the deck's
    /// plugin supports.
    OpencodeUnsupportedVersion,
    /// A kind a newer daemon sends that this build does not know. Its `text`
    /// is still shown.
    #[serde(other)]
    Unknown,
}

/// A notice on one agent's card: its kind, for code that branches on it, and
/// the sentence both clients show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNotice {
    pub kind: AgentNoticeKind,
    pub text: String,
}

/// The oldest OpenCode the deck's plugin supports: OpenCode's migration guide
/// says the object entrypoint the plugin exports (`{ id, server, setup }`)
/// works from 1.18.29.
pub const OPENCODE_SUPPORTED_FLOOR: crate::opencode_version::Version = (1, 18, 29);

/// [`AgentNoticeKind::OpencodeSharedService`]'s sentence.
pub const OPENCODE_SHARED_SERVICE: &str = "Add --standalone to the command or run opencode service set disabled true, then restart: OpenCode 2's shared service hides this agent";

/// [`AgentNoticeKind::OpencodeNotReporting`]'s sentence.
pub const OPENCODE_NOT_REPORTING: &str = "Run dot-agent-deck hooks install --agent opencode, then restart: the deck's OpenCode plugin is not reporting from this agent";

impl AgentNotice {
    /// OpenCode 2 on its shared background service.
    pub fn opencode_shared_service() -> Self {
        Self {
            kind: AgentNoticeKind::OpencodeSharedService,
            text: OPENCODE_SHARED_SERVICE.to_string(),
        }
    }

    /// The deck's plugin has not reported from a running OpenCode.
    pub fn opencode_not_reporting() -> Self {
        Self {
            kind: AgentNoticeKind::OpencodeNotReporting,
            text: OPENCODE_NOT_REPORTING.to_string(),
        }
    }

    /// OpenCode `version`, older than [`OPENCODE_SUPPORTED_FLOOR`].
    pub fn opencode_unsupported_version(version: crate::opencode_version::Version) -> Self {
        let (major, minor, patch) = version;
        let (floor_major, floor_minor, floor_patch) = OPENCODE_SUPPORTED_FLOOR;
        Self {
            kind: AgentNoticeKind::OpencodeUnsupportedVersion,
            text: format!(
                "Upgrade OpenCode to {floor_major}.{floor_minor}.{floor_patch} or later, then \
                 restart: version {major}.{minor}.{patch} is too old for the deck"
            ),
        }
    }

    /// The text a client shows: control characters removed and the length
    /// bounded, since it arrives over the wire from the daemon.
    pub fn display_text(&self) -> String {
        self.text
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_TEXT_CHARS)
            .collect()
    }
}

/// The most of a notice's text a client shows.
pub const MAX_TEXT_CHARS: usize = 400;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_round_trip_and_an_unknown_kind_keeps_its_text() {
        let notice = AgentNotice::opencode_shared_service();
        let json = serde_json::to_string(&notice).unwrap();
        assert!(
            json.contains("\"kind\":\"opencode_shared_service\""),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<AgentNotice>(&json).unwrap(), notice);

        let newer: AgentNotice =
            serde_json::from_str(r#"{"kind":"something_newer","text":"do this"}"#).unwrap();
        assert_eq!(newer.kind, AgentNoticeKind::Unknown);
        assert_eq!(newer.display_text(), "do this");
    }

    #[test]
    fn each_opencode_notice_says_what_to_do() {
        let shared = AgentNotice::opencode_shared_service().text;
        assert!(shared.contains("--standalone"), "{shared}");
        assert!(
            shared.contains("opencode service set disabled true"),
            "{shared}"
        );
        let silent = AgentNotice::opencode_not_reporting().text;
        assert!(
            silent.contains("dot-agent-deck hooks install --agent opencode"),
            "{silent}"
        );
        let old = AgentNotice::opencode_unsupported_version((1, 18, 28)).text;
        assert_eq!(
            old,
            "Upgrade OpenCode to 1.18.29 or later, then restart: version 1.18.28 is too old for \
             the deck"
        );
    }

    #[test]
    fn display_text_drops_control_characters_and_is_bounded() {
        let notice = AgentNotice {
            kind: AgentNoticeKind::Unknown,
            text: format!("a\u{1b}[31mb\n{}", "x".repeat(1000)),
        };
        let shown = notice.display_text();
        assert!(shown.starts_with("a[31mb"), "{shown}");
        assert_eq!(shown.chars().count(), MAX_TEXT_CHARS);
    }
}
