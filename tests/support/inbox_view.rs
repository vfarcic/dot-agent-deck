//! PRD #1666 S5: read what the deck posted to a pane's inbox, over the attach
//! socket, with each message rendered whole as the agent sees it.
//!
//! The deck no longer types a prompt, task or notice into a pane: it posts it
//! to the pane's inbox. A test that used to wait for text to appear in a
//! stand-in's PTY waits for it here instead. Reading changes no message's
//! state.

#![allow(dead_code)] // Each target uses a different subset of these helpers.

use std::path::Path;
use std::time::Duration;

use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::inbox::MessageSummary;

use crate::common;

/// Every message in `pane_id`'s inbox, live and recent history, with its text.
pub fn messages(attach_socket: &Path, pane_id: &str) -> Vec<MessageSummary> {
    common::attach_request_on(
        attach_socket,
        &AttachRequest::ListInbox {
            pane_id: Some(pane_id.to_string()),
            bodies: true,
        },
    )
    .ok()
    .and_then(|response| response.inbox_messages)
    .unwrap_or_default()
}

/// Every message in the deck's inboxes, with its text.
pub fn all_messages(attach_socket: &Path) -> Vec<MessageSummary> {
    common::attach_request_on(
        attach_socket,
        &AttachRequest::ListInbox {
            pane_id: None,
            bodies: true,
        },
    )
    .ok()
    .and_then(|response| response.inbox_messages)
    .unwrap_or_default()
}

/// The text of every message in `pane_id`'s inbox, joined in order.
pub fn text(attach_socket: &Path, pane_id: &str) -> String {
    messages(attach_socket, pane_id)
        .into_iter()
        .filter_map(|message| message.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many messages in `pane_id`'s inbox contain `needle`.
pub fn count_containing(attach_socket: &Path, pane_id: &str, needle: &str) -> usize {
    messages(attach_socket, pane_id)
        .into_iter()
        .filter(|message| message.text.as_deref().is_some_and(|t| t.contains(needle)))
        .count()
}

/// Wait up to `timeout` for a message containing `needle` to reach
/// `pane_id`'s inbox. Answers whether one did.
pub fn wait_for(attach_socket: &Path, pane_id: &str, needle: &str, timeout: Duration) -> bool {
    common::wait_until(timeout, || {
        count_containing(attach_socket, pane_id, needle) > 0
    })
}
