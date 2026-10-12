//! PRD #1666 S6: the deck's own prompts, as an agent's hooks see them.
//!
//! A deck message reaches an agent's model in one of two prompt shapes:
//!
//! - **A native wake.** Claude Code's `asyncRewake` waiter exits 2 with the
//!   message blocks on stderr, and Claude submits them as a
//!   `<task-notification>` prompt. The blocks open with
//!   `[dot-agent-deck message <id> from …]`.
//! - **A doorbell.** No waiter was armed, so the daemon typed the fixed line
//!   ([`crate::doorbell::doorbell_line`]). The agent's `UserPromptSubmit` hook
//!   then fetches the content itself and hands it to the model as
//!   `additionalContext`, so the model is never asked to fetch. Codex trims the
//!   line's leading space, so the scan does not anchor on it.
//!
//! Either way the prompt is the deck's, not the user's: it acknowledges the
//! message (the content is in the model's context), and the card shows
//! `✉ <subject>` instead of the raw prompt and keeps it out of the user's
//! prompt history. The hook names the ids in the event's metadata; the daemon
//! acknowledges them for the agent the event's token attests and stamps the
//! card label ([`stamp_deck_message_prompt`]), because only the daemon can
//! attest the agent and knows the subject.
//!
//! Agent-neutral: Claude Code uses it in S6, and Codex, whose hooks take the
//! same `additionalContext` shape, reuses it in S7 with its own budget.

use std::sync::LazyLock;

use regex::Regex;

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::event::{
    AgentEvent, DaemonMessage, InboxFetchConsumer, InboxFetchRequest, InboxWaitReply,
};

/// Event metadata, set by the hook: every deck message id the prompt carries,
/// comma-separated. The daemon reads it to label the card and removes it.
pub const DECK_MESSAGE_IDS_METADATA_KEY: &str = "deck_message_ids";

/// Event metadata, set by the hook: the ids whose content the prompt itself
/// carries, which the daemon acknowledges as observed. Removed by the daemon.
pub const DECK_MESSAGE_ACK_IDS_METADATA_KEY: &str = "deck_message_ack_ids";

/// Event metadata, set ONLY by the daemon (an incoming value is discarded):
/// this event's `user_prompt` is a deck message's label, not something the
/// user typed, so it stays out of `last_user_prompt` and `first_prompts`.
pub const DECK_MESSAGE_METADATA_KEY: &str = "deck_message";

/// The label a card shows for a deck message whose subject is unknown.
const UNKNOWN_SUBJECT: &str = "dot-agent-deck message";

/// The most ids one prompt is read for.
const MAX_IDS: usize = 16;

/// The most characters of a subject a card label carries.
const MAX_SUBJECT_CHARS: usize = 120;

const ID_PATTERN: &str = "(m-[a-z2-7]{8}-[0-9a-f]{16})";

static OPENER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"\[dot-agent-deck message {ID_PATTERN}"))
        .expect("the opener pattern is valid")
});

static DOORBELL: LazyLock<Regex> = LazyLock::new(|| {
    let prefix = regex::escape(crate::doorbell::LINE_PREFIX.trim());
    let suffix = regex::escape(
        crate::doorbell::LINE_SUFFIX
            .split(',')
            .next()
            .unwrap_or(crate::doorbell::LINE_SUFFIX),
    );
    Regex::new(&format!("{prefix} {ID_PATTERN}{suffix}")).expect("the doorbell pattern is valid")
});

/// The deck message ids a prompt carries.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeckPromptIds {
    /// Ids whose block opener is in the prompt: the content is in context.
    pub opened: Vec<String>,
    /// Ids a doorbell line names: the content is still in the inbox.
    pub rung: Vec<String>,
}

impl DeckPromptIds {
    pub fn is_empty(&self) -> bool {
        self.opened.is_empty() && self.rung.is_empty()
    }
}

/// Scan `prompt` for deck message ids. Local and cheap: an ordinary prompt
/// costs one substring check and no daemon call.
pub fn scan(prompt: &str) -> DeckPromptIds {
    let mut ids = DeckPromptIds::default();
    if !prompt.contains("dot-agent-deck") {
        return ids;
    }
    let collect = |re: &Regex, out: &mut Vec<String>| {
        for capture in re.captures_iter(prompt) {
            let id = &capture[1];
            if out.len() < MAX_IDS
                && crate::inbox::is_valid_message_id(id)
                && !out.iter().any(|seen| seen == id)
            {
                out.push(id.to_string());
            }
        }
    };
    collect(&OPENER, &mut ids.opened);
    collect(&DOORBELL, &mut ids.rung);
    ids
}

/// What a doorbell-started prompt hands the model: the framing and the
/// fetched blocks, and which ids the fetch acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub context: String,
    pub fetched: Vec<String>,
}

/// Whether this process runs in a deck pane: the deck put a pane id in its
/// environment.
pub fn in_deck_pane() -> bool {
    std::env::var(DOT_AGENT_DECK_PANE_ID).is_ok_and(|pane| !pane.is_empty())
}

/// Fetch, and acknowledge in the same step, what is waiting for this pane's
/// agent, up to `budget` characters including the framing sentence. `None`
/// outside a deck pane, without a token, when the daemon does not answer, or
/// when nothing is waiting. Bounded by the hook socket's 5 s request timeout.
pub fn doorbell_injection(budget: usize) -> Option<Injection> {
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID)
        .ok()
        .filter(|pane| !pane.is_empty())?;
    let token = crate::hook_provenance::token_from_env()?;
    let framing = crate::inbox_hub::SESSION_FRAMING;
    let request = DaemonMessage::InboxFetch(InboxFetchRequest {
        pane_id,
        agent_id: std::env::var(DOT_AGENT_DECK_AGENT_ID)
            .ok()
            .filter(|agent| !agent.is_empty()),
        token: Some(token),
        consumer: InboxFetchConsumer::DoorbellHook,
        max_chars: Some(budget.saturating_sub(framing.chars().count() + 2).max(1)),
        ack: true,
    });
    let json = serde_json::to_string(&request).ok()?;
    let line = crate::hook::request_from_socket(&json)?;
    let messages = serde_json::from_str::<InboxWaitReply>(&line)
        .ok()?
        .messages
        .filter(|messages| !messages.is_empty())?;
    let mut context = framing.to_string();
    for message in &messages {
        context.push_str("\n\n");
        context.push_str(&message.text);
    }
    Some(Injection {
        context,
        fetched: messages.into_iter().map(|message| message.id).collect(),
    })
}

/// The hook's stdout handing `context` to the model with the prompt or
/// session `hook_event_name` names.
pub fn hook_output(hook_event_name: &str, context: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": context,
        }
    })
    .to_string()
}

/// Mark `event` as carrying the deck's prompt: the raw prompt is dropped, and
/// the ids go to the daemon to acknowledge and label ([`stamp_deck_message_prompt`]).
pub fn mark_event(event: &mut AgentEvent, ids: &DeckPromptIds, fetched: &[String]) {
    event.user_prompt = None;
    let mut all: Vec<&str> = Vec::new();
    for id in fetched.iter().chain(&ids.opened).chain(&ids.rung) {
        if !all.contains(&id.as_str()) {
            all.push(id);
        }
    }
    let acks: Vec<&str> = ids
        .opened
        .iter()
        .map(String::as_str)
        .filter(|id| !fetched.iter().any(|done| done == id))
        .collect();
    event
        .metadata
        .insert(DECK_MESSAGE_IDS_METADATA_KEY.to_string(), all.join(","));
    if !acks.is_empty() {
        event.metadata.insert(
            DECK_MESSAGE_ACK_IDS_METADATA_KEY.to_string(),
            acks.join(","),
        );
    }
}

/// Whether `event`'s prompt is a deck message's label rather than the user's
/// ([`DECK_MESSAGE_METADATA_KEY`]).
pub fn is_deck_message(event: &AgentEvent) -> bool {
    event.metadata.contains_key(DECK_MESSAGE_METADATA_KEY)
}

/// Daemon side, at hook ingestion: act on the ids [`mark_event`] named, for
/// `attested_agent` only. Each opened id is acknowledged as observed, and the
/// event's prompt becomes `✉ <subject>` with [`DECK_MESSAGE_METADATA_KEY`] set.
/// The keys are removed whatever happens, and an incoming marker is discarded,
/// so a hook cannot label a prompt the deck did not send.
pub fn stamp_deck_message_prompt(
    event: &mut AgentEvent,
    attested_agent: Option<&str>,
    hub: &crate::inbox_hub::InboxHub,
) {
    event.metadata.remove(DECK_MESSAGE_METADATA_KEY);
    let ids = event.metadata.remove(DECK_MESSAGE_IDS_METADATA_KEY);
    let acks = event.metadata.remove(DECK_MESSAGE_ACK_IDS_METADATA_KEY);
    let Some(ids) = ids else {
        return;
    };
    let (Some(agent_id), Some(pane_id)) = (attested_agent, event.pane_id.clone()) else {
        return;
    };
    let valid = |list: &str| -> Vec<String> {
        list.split(',')
            .filter(|id| crate::inbox::is_valid_message_id(id))
            .take(MAX_IDS)
            .map(str::to_string)
            .collect()
    };
    for id in acks.as_deref().map(valid).unwrap_or_default() {
        hub.ack(
            &id,
            &pane_id,
            agent_id,
            crate::inbox::AckHow::PromptObserved,
        );
    }
    let ids = valid(&ids);
    let listed = hub.list(&pane_id);
    let subject = ids.iter().find_map(|id| {
        listed
            .iter()
            .find(|summary| summary.id.as_str() == id)
            .map(|summary| summary.subject.clone())
    });
    let subject: String = subject
        .as_deref()
        .unwrap_or(UNKNOWN_SUBJECT)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_SUBJECT_CHARS)
        .collect();
    event.user_prompt = Some(format!("✉ {subject}"));
    event
        .metadata
        .insert(DECK_MESSAGE_METADATA_KEY.to_string(), "1".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "m-k3xzq2ab-0f3c9a71d2e4b850";
    const OTHER: &str = "m-k3xzq2ab-1111111111111111";

    #[test]
    fn an_ordinary_prompt_carries_no_ids() {
        assert!(scan("please fix the build").is_empty());
        assert!(scan("mention dot-agent-deck without an id").is_empty());
    }

    #[test]
    fn a_doorbell_line_is_found_with_or_without_its_leading_space() {
        let line = crate::doorbell::doorbell_line(ID);
        for prompt in [line.as_str(), line.trim_start()] {
            let ids = scan(prompt);
            assert_eq!(ids.rung, vec![ID.to_string()], "{prompt:?}");
            assert!(ids.opened.is_empty());
        }
    }

    #[test]
    fn a_native_notification_names_its_opened_blocks_once() {
        let prompt = format!(
            "<task-notification><system-reminder>Stop hook blocking error: \
             [dot-agent-deck message {ID} from the orchestrator]\nbody\n\
             [end of dot-agent-deck message {ID}]\n\n\
             [dot-agent-deck message {OTHER} from the orchestrator]\nmore\n\
             [end of dot-agent-deck message {OTHER}]</system-reminder></task-notification>"
        );
        let ids = scan(&prompt);
        assert_eq!(ids.opened, vec![ID.to_string(), OTHER.to_string()]);
        assert!(ids.rung.is_empty());
    }

    #[test]
    fn a_malformed_id_is_ignored() {
        assert!(scan("[dot-agent-deck message m-k3x9q2ab-0f3c9a71d2e4b850 from x]").is_empty());
    }

    #[test]
    fn mark_event_drops_the_prompt_and_acks_only_unfetched_opened_ids() {
        let mut event = AgentEvent {
            session_id: "s".into(),
            agent_type: crate::event::AgentType::ClaudeCode,
            event_type: crate::event::EventType::Thinking,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: Some("raw".into()),
            metadata: Default::default(),
            pane_id: None,
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
        };
        let ids = DeckPromptIds {
            opened: vec![ID.into(), OTHER.into()],
            rung: vec![],
        };
        mark_event(&mut event, &ids, &[OTHER.to_string()]);
        assert_eq!(event.user_prompt, None);
        assert_eq!(
            event.metadata[DECK_MESSAGE_IDS_METADATA_KEY],
            format!("{OTHER},{ID}")
        );
        assert_eq!(event.metadata[DECK_MESSAGE_ACK_IDS_METADATA_KEY], ID);
    }
}
