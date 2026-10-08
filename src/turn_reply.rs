//! The daemon's fan-out of finished-turn replies (PRD #1497).
//!
//! A turn's final reply reaches the daemon on the hook socket (beside a
//! `Stop` / `StopFailure` / OpenCode `session.idle` event, under the
//! [`TURN_REPLY_LINE_KEY`] key) or from the Codex rollout tailer
//! (`crate::codex_rollout_tail`). Either way it is published here, and only
//! [`crate::daemon_protocol::AttachRequest::SubscribeTurnReplies`] connections
//! receive it — it never rides the daemon-wide `BroadcastMsg` stream.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use tokio::sync::broadcast;

use crate::daemon_protocol::{FinalReply, TurnReply, clamp_turn_reply};

/// The key a hook-socket event line carries its turn's [`FinalReply`] under,
/// beside the event's own keys. Not a field of [`crate::event::AgentEvent`],
/// for the reason [`crate::event::PresentedToken`] gives for the capability
/// token: the event the daemon keeps and broadcasts never carries it, so no
/// fan-out path has to remember to strip a reply body. An older daemon ignores
/// the key, because `AgentEvent` does not deny unknown fields.
pub const TURN_REPLY_LINE_KEY: &str = "turn_reply";

/// How many replies a subscriber may fall behind before its stream is ended as
/// lagged. Replies come one per finished turn, so this is generous.
const CAPACITY: usize = 64;

/// How many agents' last delivered turn ids are remembered for
/// de-duplication. Past it the oldest is forgotten, which at worst lets one
/// turn reported through both routes be delivered twice.
const MAX_REMEMBERED_TURNS: usize = 1024;

/// The longest [`FinalReply::turn_id`] kept; a longer one is dropped (the reply
/// is still delivered, without de-duplication).
const MAX_TURN_ID_BYTES: usize = 256;

/// A turn reply as it appears on a hook-socket line, read leniently: a reply
/// whose shape is wrong is no reply, and never costs the event.
#[derive(Debug, Default, Deserialize)]
struct PresentedTurnReply {
    #[serde(default)]
    turn_reply: Option<serde_json::Value>,
}

/// The [`FinalReply`] a hook-socket `line` carries under
/// [`TURN_REPLY_LINE_KEY`], re-bounded — the socket accepts lines from any
/// same-uid producer, not only the deck's hook CLI.
pub fn reply_from_line(line: &str) -> Option<FinalReply> {
    let value = serde_json::from_str::<PresentedTurnReply>(line)
        .ok()?
        .turn_reply?;
    let reply = serde_json::from_value::<FinalReply>(value).ok()?;
    normalize(reply)
}

/// `reply` with its text clamped and an over-long turn id dropped, or `None`
/// when no text is left.
pub fn normalize(reply: FinalReply) -> Option<FinalReply> {
    let text = clamp_turn_reply(&reply.text);
    if text.trim().is_empty() {
        return None;
    }
    Some(FinalReply {
        turn_id: reply
            .turn_id
            .filter(|t| !t.is_empty() && t.len() <= MAX_TURN_ID_BYTES),
        text: text.to_owned(),
        failed: reply.failed,
    })
}

/// The daemon's turn-reply fan-out. Held by
/// [`crate::agent_pty::AgentPtyRegistry`], the one object the hook loop, the
/// Codex rollout monitor and the attach server all share.
#[derive(Debug)]
pub struct TurnReplyHub {
    tx: broadcast::Sender<TurnReply>,
    sequence: AtomicU64,
    delivered: Mutex<DeliveredTurns>,
}

#[derive(Debug, Default)]
struct DeliveredTurns {
    last: HashMap<String, String>,
    order: VecDeque<String>,
}

impl DeliveredTurns {
    /// Record `turn_id` as `agent_id`'s last delivered turn; `false` when it
    /// already was.
    fn note(&mut self, agent_id: &str, turn_id: &str) -> bool {
        if self.last.get(agent_id).is_some_and(|t| t == turn_id) {
            return false;
        }
        if self
            .last
            .insert(agent_id.to_owned(), turn_id.to_owned())
            .is_none()
        {
            self.order.push_back(agent_id.to_owned());
            while self.order.len() > MAX_REMEMBERED_TURNS {
                if let Some(oldest) = self.order.pop_front() {
                    self.last.remove(&oldest);
                }
            }
        }
        true
    }
}

impl Default for TurnReplyHub {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(CAPACITY).0,
            sequence: AtomicU64::new(0),
            delivered: Mutex::new(DeliveredTurns::default()),
        }
    }
}

impl TurnReplyHub {
    /// A receiver of every reply published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<TurnReply> {
        self.tx.subscribe()
    }

    /// Publish `reply` as `agent_id`'s, in `pane_id`, and return its sequence
    /// number — or `None` when `reply` names a turn already delivered for this
    /// agent (Codex reports a turn both through its `Stop` hook and in its
    /// rollout). The caller has already checked that `agent_id` is the pane's
    /// live owner.
    pub fn publish(&self, agent_id: &str, pane_id: &str, reply: FinalReply) -> Option<u64> {
        if let Some(turn_id) = reply.turn_id.as_deref()
            && !self
                .delivered
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .note(agent_id, turn_id)
        {
            return None;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send(TurnReply {
            agent_id: agent_id.to_owned(),
            pane_id: pane_id.to_owned(),
            sequence,
            reply,
        });
        Some(sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(turn_id: Option<&str>, text: &str) -> FinalReply {
        FinalReply {
            turn_id: turn_id.map(str::to_owned),
            text: text.into(),
            failed: false,
        }
    }

    #[test]
    fn a_turn_reported_twice_is_delivered_once_and_sequences_increase() {
        let hub = TurnReplyHub::default();
        let mut rx = hub.subscribe();
        let first = hub.publish("a", "p", reply(Some("t1"), "one")).unwrap();
        assert_eq!(hub.publish("a", "p", reply(Some("t1"), "one")), None);
        let second = hub.publish("a", "p", reply(Some("t2"), "two")).unwrap();
        let third = hub.publish("a", "p", reply(None, "no id")).unwrap();
        assert!(first < second && second < third);
        assert_eq!(rx.try_recv().unwrap().reply.text, "one");
        assert_eq!(rx.try_recv().unwrap().reply.text, "two");
        assert_eq!(rx.try_recv().unwrap().reply.text, "no id");
        assert!(rx.try_recv().is_err());
        // Another agent's turn with the same id is its own.
        assert!(hub.publish("b", "q", reply(Some("t2"), "b")).is_some());
    }

    #[test]
    fn a_line_reply_is_read_leniently_and_rebounded() {
        let long = "y".repeat(crate::daemon_protocol::MAX_TURN_REPLY_BYTES + 10);
        let line = serde_json::json!({
            "session_id": "s",
            TURN_REPLY_LINE_KEY: {"text": long, "turn_id": "x".repeat(300), "failed": true},
        })
        .to_string();
        let got = reply_from_line(&line).unwrap();
        assert_eq!(got.text.len(), crate::daemon_protocol::MAX_TURN_REPLY_BYTES);
        assert_eq!(got.turn_id, None);
        assert!(got.failed);
        for bad in [
            serde_json::json!({TURN_REPLY_LINE_KEY: "text"}),
            serde_json::json!({TURN_REPLY_LINE_KEY: {"text": 3}}),
            serde_json::json!({TURN_REPLY_LINE_KEY: {"text": "  "}}),
            serde_json::json!({"session_id": "s"}),
        ] {
            assert_eq!(reply_from_line(&bad.to_string()), None, "{bad}");
        }
    }
}
