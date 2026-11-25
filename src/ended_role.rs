//! Issue #1965 Part 2: an orchestration role whose agent has exited, and what
//! the deck saw of how it ended.
//!
//! A role whose command dies at spawn (a launcher that fails, a missing
//! binary, an extension that does not load) leaves its daemon entry behind,
//! exited but not closed. `ListAgents` lists only live agents, so both clients
//! used to lose the role: the TUI drew a `No agent` placeholder titled with a
//! truncated synthetic pane id, which selecting deleted as stale, and the
//! desktop drew nothing. The daemon now reports each such role beside the live
//! records ([`crate::daemon_protocol::AttachResponse::ended_roles`]) with how
//! its pane ended and the last lines it showed, and both clients render it in
//! the words below, which `tests/fixtures/ended-role.json` pins to the
//! desktop's copy (CLAUDE.md rule 22).
//!
//! The deck observes its own child and the pane's terminal, never the agent
//! below them (the PRD #1666 part 3 rule [`PaneTermination`] follows), so the
//! words say the role's *command* exited, not that the agent did.

use crate::inbox_display::PaneTermination;

/// One orchestration role whose pane's agent exited on its own and whose pane
/// nobody has closed or handed to a successor yet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EndedRole {
    /// The pane's `DOT_AGENT_DECK_PANE_ID`, the id the orchestration surfaced
    /// the role under.
    pub pane_id: String,
    /// The exited generation's agent id.
    pub agent_id: String,
    /// The role's position in the orchestration's `roles` list.
    pub role_index: usize,
    /// The role's `name`, as the daemon registered it.
    #[serde(default)]
    pub role_name: String,
    /// The orchestration instance the role belongs to
    /// ([`crate::agent_pty::TabMembership::Orchestration`]'s `orchestration_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration_id: Option<String>,
    /// The orchestration's name and display title, so a client that has no
    /// live role of it left can still head a group for it.
    #[serde(default)]
    pub orchestration_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    /// How the pane ended, as the deck observed it. `None` while the pane's
    /// reader has not settled it yet, which a later listing fills in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<PaneTermination>,
    /// The last non-blank lines the pane's screen showed, oldest first, at most
    /// [`MAX_LAST_OUTPUT_LINES`]. Untrusted text from the pane: a client
    /// sanitises it before display ([`EndedRole::sanitized`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_output: Vec<String>,
}

/// How many of the pane's last screen lines an [`EndedRole`] carries.
pub const MAX_LAST_OUTPUT_LINES: usize = crate::pane_screen_text::MAX_REPORTED_ROWS;

/// Longest single line of [`EndedRole::last_output`] a client displays, in
/// bytes, the cut marker included.
pub const MAX_LAST_OUTPUT_LINE_BYTES: usize = 240;

/// The status a client shows for an ended role, where a live role shows its
/// agent's status.
pub const STATUS: &str = "Exited";
/// The label in front of [`EndedRole::last_output`].
pub const LAST_OUTPUT: &str = "Last output";
/// Why the role ended, for a command the deck saw exit with `{code}`.
pub const REASON_COMMAND_EXITED: &str = "its command exited (code {code})";
/// Why the role ended, for a pane whose output ended while the deck could not
/// see its command exit.
pub const REASON_CONNECTION_ENDED: &str = "the deck's connection to its pane ended";
/// Why the role ended, when the deck cannot say how.
pub const REASON_UNKNOWN: &str = "its pane ended";
/// One line naming the role and why it ended: `{role}` and `{reason}`.
pub const HEADLINE: &str = "{role} exited: {reason}";

impl EndedRole {
    /// Why the role ended, in the words both clients show.
    pub fn reason(&self) -> String {
        reason_words(self.termination.as_ref())
    }

    /// `<role> exited: <reason>`.
    pub fn headline(&self) -> String {
        HEADLINE
            .replace("{role}", &self.role_name)
            .replace("{reason}", &self.reason())
    }

    /// This record with every producer-supplied string made safe for one line
    /// of a terminal or a tile: control, bidi and invisible characters
    /// stripped, the role name bounded like a display name, and at most
    /// [`MAX_LAST_OUTPUT_LINES`] output lines of at most
    /// [`MAX_LAST_OUTPUT_LINE_BYTES`] each. Lines left empty are dropped.
    pub fn sanitized(mut self) -> Self {
        self.role_name = crate::untrusted_text::label_line(
            &self.role_name,
            crate::agent_pty::DISPLAY_NAME_MAX_LEN,
        );
        self.orchestration_name = crate::untrusted_text::label_line(
            &self.orchestration_name,
            crate::agent_pty::DISPLAY_NAME_MAX_LEN,
        );
        self.display_title = self
            .display_title
            .take()
            .map(|title| {
                crate::untrusted_text::label_line(&title, crate::agent_pty::DISPLAY_NAME_MAX_LEN)
            })
            .filter(|title| !title.is_empty());
        let lines = std::mem::take(&mut self.last_output);
        let skip = lines.len().saturating_sub(MAX_LAST_OUTPUT_LINES);
        self.last_output = lines
            .into_iter()
            .skip(skip)
            .map(|line| {
                let line: String = line
                    .chars()
                    .filter(|c| !crate::untrusted_text::is_invisible_format_char(*c))
                    .collect();
                crate::untrusted_text::display_line(&line, MAX_LAST_OUTPUT_LINE_BYTES)
                    .trim()
                    .to_string()
            })
            .filter(|line| !line.is_empty())
            .collect();
        self
    }
}

/// [`EndedRole::reason`] for a termination the deck may not have settled yet.
pub fn reason_words(termination: Option<&PaneTermination>) -> String {
    match termination {
        Some(PaneTermination::PaneCommandExited { code, .. }) => {
            REASON_COMMAND_EXITED.replace("{code}", &code.to_string())
        }
        Some(PaneTermination::ConnectionEnded { .. }) => REASON_CONNECTION_ENDED.to_string(),
        Some(PaneTermination::Replaced | PaneTermination::Closed | PaneTermination::Unknown)
        | None => REASON_UNKNOWN.to_string(),
    }
}

/// What the deck keeps on an exited generation's registry entry for
/// [`EndedRole`]: settled once, by the pane's reader, after its EOF.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneEnding {
    /// How the pane ended.
    pub termination: Option<PaneTermination>,
    /// The pane's last screen lines.
    pub last_output: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_display::TerminationSource;

    fn role(termination: Option<PaneTermination>) -> EndedRole {
        EndedRole {
            pane_id: "7".into(),
            agent_id: "3".into(),
            role_index: 1,
            role_name: "reviewer".into(),
            orchestration_id: Some("orch-1".into()),
            orchestration_name: "mixed".into(),
            display_title: None,
            termination,
            last_output: Vec::new(),
        }
    }

    #[test]
    fn the_reason_names_the_exit_code_the_deck_saw() {
        let ended = role(Some(PaneTermination::PaneCommandExited {
            code: 1,
            reported_by: TerminationSource::Deck,
        }));
        assert_eq!(ended.reason(), "its command exited (code 1)");
        assert_eq!(
            ended.headline(),
            "reviewer exited: its command exited (code 1)"
        );
        assert_eq!(
            role(Some(PaneTermination::ConnectionEnded {
                read_end: "eof".into()
            }))
            .reason(),
            REASON_CONNECTION_ENDED
        );
        assert_eq!(role(None).reason(), REASON_UNKNOWN);
        assert_eq!(
            role(Some(PaneTermination::Unknown)).reason(),
            REASON_UNKNOWN
        );
    }

    #[test]
    fn sanitized_strips_controls_and_keeps_the_last_lines() {
        let mut ended = role(None);
        ended.role_name = "rev\u{202e}iewer\n".into();
        ended.last_output = (0..10)
            .map(|i| format!("line {i}\u{1b}[31m"))
            .chain(["   ".to_string(), "x".repeat(1000)])
            .collect();
        let clean = ended.sanitized();
        assert_eq!(clean.role_name, "reviewer");
        assert!(clean.last_output.len() <= MAX_LAST_OUTPUT_LINES);
        assert!(
            clean
                .last_output
                .iter()
                .all(|l| !l.contains('\u{1b}') && l.len() <= MAX_LAST_OUTPUT_LINE_BYTES)
        );
        assert!(clean.last_output.last().unwrap().ends_with('…'));
        assert!(!clean.last_output.iter().any(|l| l.trim().is_empty()));
    }

    /// The broadcast is tagged `role_ended`, round-trips, and a client that
    /// does not know the tag reads it as `Unknown` instead of failing.
    #[test]
    fn the_role_ended_broadcast_is_additive_on_the_wire() {
        let msg =
            crate::event::BroadcastMsg::RoleEnded(role(Some(PaneTermination::PaneCommandExited {
                code: 7,
                reported_by: TerminationSource::Deck,
            })));
        let wire = serde_json::to_value(&msg).unwrap();
        assert_eq!(wire["kind"], "role_ended");
        let back: crate::event::BroadcastMsg = serde_json::from_value(wire).unwrap();
        assert!(
            matches!(back, crate::event::BroadcastMsg::RoleEnded(r) if r.role_name == "reviewer")
        );
        let unknown: crate::event::BroadcastMsg =
            serde_json::from_value(serde_json::json!({"kind": "role_ended_v2", "x": 1})).unwrap();
        assert!(matches!(unknown, crate::event::BroadcastMsg::Unknown));
    }

    /// The words are the ones the desktop reads from the shared fixture.
    #[test]
    fn the_words_match_the_shared_fixture() {
        let words: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/ended-role.json")).unwrap();
        assert_eq!(words["status"], STATUS);
        assert_eq!(words["lastOutput"], LAST_OUTPUT);
        assert_eq!(words["headline"], HEADLINE);
        assert_eq!(
            words["reasons"]["pane_command_exited"],
            REASON_COMMAND_EXITED
        );
        assert_eq!(
            words["reasons"]["connection_ended"],
            REASON_CONNECTION_ENDED
        );
        assert_eq!(words["reasons"]["unknown"], REASON_UNKNOWN);
    }
}
