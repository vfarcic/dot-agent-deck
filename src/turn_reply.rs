//! The daemon's fan-out of finished-turn replies (PRD #1497).
//!
//! A turn's final reply reaches the daemon on the hook socket (beside a
//! `Stop` / `StopFailure` / OpenCode `session.idle` event, under the
//! [`TURN_REPLY_LINE_KEY`] key) or from the Codex rollout tailer
//! (`crate::codex_rollout_tail`). Either way it is published here, and only
//! [`crate::daemon_protocol::AttachRequest::SubscribeTurnReplies`] connections
//! receive it — it never rides the daemon-wide `BroadcastMsg` stream.
//!
//! A turn that ended with nothing to read is published too, as a reply whose
//! text is empty ([`crate::daemon_protocol::FinalReply::is_empty`]), from the
//! same report that would have carried the text. So each turn end a producer
//! reports reaches a subscriber as exactly one frame, in the order the turns
//! ended, and a subscriber never has to guess from the agent's status and a
//! timer whether a turn ended without a reply.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};

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

/// How many `subscribe-turn-replies` connections the daemon serves at once.
/// The desktop's reading opens one per agent on the deck it views (PRD #1497,
/// decision 3 of 2026-10-09), so this also bounds how many of this daemon's
/// agents all open windows can read at once; a subscription past it is
/// refused with nothing opened, and a slot is freed when its connection ends
/// ([`TurnReplyReceiver`]).
pub const MAX_TURN_REPLY_SUBSCRIBERS: usize = 32;

/// A turn reply as it appears on a hook-socket line, read leniently: a reply
/// whose shape is wrong is no report of a turn end, and never costs the event.
#[derive(Debug, Default, Deserialize)]
struct PresentedTurnReply {
    #[serde(default)]
    turn_reply: Option<serde_json::Value>,
}

/// The [`FinalReply`] a hook-socket `line` carries under
/// [`TURN_REPLY_LINE_KEY`], re-bounded — the socket accepts lines from any
/// same-uid producer, not only the deck's hook CLI. A reply with blank text is
/// the report of a turn that ended with nothing to read.
pub fn reply_from_line(line: &str) -> Option<FinalReply> {
    let value = serde_json::from_str::<PresentedTurnReply>(line)
        .ok()?
        .turn_reply?;
    let reply = serde_json::from_value::<FinalReply>(value).ok()?;
    Some(normalize(reply))
}

/// `reply` with its text clamped and an over-long turn id dropped. Blank text
/// becomes empty: the turn ended with no reply to read, which is still a turn
/// end to deliver.
pub fn normalize(reply: FinalReply) -> FinalReply {
    let text = clamp_turn_reply(&reply.text);
    FinalReply {
        turn_id: reply
            .turn_id
            .filter(|t| !t.is_empty() && t.len() <= MAX_TURN_ID_BYTES),
        text: if text.trim().is_empty() {
            String::new()
        } else {
            text.to_owned()
        },
        failed: reply.failed,
    }
}

/// The daemon's turn-reply fan-out. Held by
/// [`crate::agent_pty::AgentPtyRegistry`], the one object the hook loop, the
/// Codex rollout monitor and the attach server all share.
///
/// Replies are broadcast as `Arc<TurnReply>`, so every subscriber shares the
/// one copy of a reply body rather than cloning it per receiver.
#[derive(Debug)]
pub struct TurnReplyHub {
    tx: broadcast::Sender<Arc<TurnReply>>,
    sequence: AtomicU64,
    delivered: Mutex<AgentTurns>,
    begun: Mutex<AgentTurns>,
    subscribers: Arc<Semaphore>,
}

/// One turn id per agent, the oldest agent forgotten past
/// [`MAX_REMEMBERED_TURNS`].
#[derive(Debug, Default)]
struct AgentTurns {
    last: HashMap<String, String>,
    order: VecDeque<String>,
}

impl AgentTurns {
    /// Record `turn_id` as `agent_id`'s; `false` when it already was.
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

    /// Forget and answer `agent_id`'s turn.
    fn take(&mut self, agent_id: &str) -> Option<String> {
        let turn = self.last.remove(agent_id)?;
        self.order.retain(|agent| agent != agent_id);
        Some(turn)
    }
}

impl Default for TurnReplyHub {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(CAPACITY).0,
            sequence: AtomicU64::new(0),
            delivered: Mutex::new(AgentTurns::default()),
            begun: Mutex::new(AgentTurns::default()),
            subscribers: Arc::new(Semaphore::new(MAX_TURN_REPLY_SUBSCRIBERS)),
        }
    }
}

/// A receiver of the replies published from the moment it was opened, holding
/// one of the hub's [`MAX_TURN_REPLY_SUBSCRIBERS`] slots until it is dropped.
#[derive(Debug)]
pub struct TurnReplyReceiver {
    pub rx: broadcast::Receiver<Arc<TurnReply>>,
    _slot: OwnedSemaphorePermit,
}

impl TurnReplyHub {
    /// A receiver of every reply published from now on, or `None` when
    /// [`MAX_TURN_REPLY_SUBSCRIBERS`] are already open.
    pub fn subscribe(&self) -> Option<TurnReplyReceiver> {
        let slot = Arc::clone(&self.subscribers).try_acquire_owned().ok()?;
        Some(TurnReplyReceiver {
            rx: self.tx.subscribe(),
            _slot: slot,
        })
    }

    /// Record `turn_id` as the turn `agent_id` has just begun — Codex names it
    /// on `UserPromptSubmit` — for [`Self::take_begun_turn`]. An over-long id
    /// is not recorded.
    pub fn begin_turn(&self, agent_id: &str, turn_id: &str) {
        if turn_id.is_empty() || turn_id.len() > MAX_TURN_ID_BYTES {
            return;
        }
        self.begun
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .note(agent_id, turn_id);
    }

    /// The turn [`Self::begin_turn`] last recorded for `agent_id`, forgotten so
    /// it names one turn end only.
    ///
    /// Review RV-B1: Codex's `Stop` hook carries no turn id, while its rollout's
    /// `task_complete` for the same turn does. Giving the `Stop`'s reply the
    /// turn its `UserPromptSubmit` named is what lets [`Self::publish`] see the
    /// two as one turn. A reply-bearing turn end with no `Thinking` before it
    /// (abnormal) would take an older turn's recorded id; at most one entry
    /// per agent is kept, so that is bounded to one stale id.
    pub fn take_begun_turn(&self, agent_id: &str) -> Option<String> {
        self.begun
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take(agent_id)
    }

    /// Publish `reply` as `agent_id`'s, in `pane_id`, and return its sequence
    /// number — or `None` when `reply` names a turn already delivered for this
    /// agent (Codex reports a turn both through its `Stop` hook and in its
    /// rollout; whichever reaches here first is the turn's one frame, an empty
    /// one included). The caller has already checked that `agent_id` is the
    /// pane's live owner.
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
        let _ = self.tx.send(Arc::new(TurnReply {
            agent_id: agent_id.to_owned(),
            pane_id: pane_id.to_owned(),
            sequence,
            reply,
        }));
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
        let mut rx = hub.subscribe().unwrap().rx;
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

    /// Audit AU-S2: past [`MAX_TURN_REPLY_SUBSCRIBERS`] open receivers a
    /// subscription is refused, and dropping one frees its slot. Every
    /// receiver shares the one published reply rather than a copy of it.
    #[test]
    fn subscriptions_are_bounded_and_share_one_reply() {
        let hub = TurnReplyHub::default();
        let mut open: Vec<_> = (0..MAX_TURN_REPLY_SUBSCRIBERS)
            .map(|_| hub.subscribe().expect("within the bound"))
            .collect();
        assert!(hub.subscribe().is_none(), "refused at the bound");
        hub.publish("a", "p", reply(None, "shared")).unwrap();
        let first = open[0].rx.try_recv().unwrap();
        let last = open.last_mut().unwrap().rx.try_recv().unwrap();
        assert!(Arc::ptr_eq(&first, &last), "one copy, shared");
        drop(open.pop());
        let mut again = hub.subscribe().expect("a dropped receiver frees its slot");
        assert!(again.rx.try_recv().is_err(), "nothing replayed");
        assert!(hub.subscribe().is_none());
    }

    /// Review RV-B1: the turn Codex's prompt began is handed to that turn's
    /// end once, and only to that agent.
    #[test]
    fn a_begun_turn_is_taken_once_per_agent() {
        let hub = TurnReplyHub::default();
        hub.begin_turn("a", "t1");
        hub.begin_turn("a", "t2");
        hub.begin_turn("b", "x");
        hub.begin_turn("c", &"y".repeat(MAX_TURN_ID_BYTES + 1));
        assert_eq!(hub.take_begun_turn("a").as_deref(), Some("t2"));
        assert_eq!(hub.take_begun_turn("a"), None);
        assert_eq!(hub.take_begun_turn("b").as_deref(), Some("x"));
        assert_eq!(hub.take_begun_turn("c"), None);
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
            serde_json::json!({TURN_REPLY_LINE_KEY: {}}),
            serde_json::json!({"session_id": "s"}),
        ] {
            assert_eq!(reply_from_line(&bad.to_string()), None, "{bad}");
        }
    }

    /// PRD #1497 audit A2: a blank reply is the report of a turn that ended
    /// with nothing to read. It is delivered as one empty frame, de-duplicated
    /// by its turn id like any other, so a turn reported by two routes is one
    /// frame whether or not it had text.
    #[test]
    fn a_turn_with_no_reply_is_one_empty_frame() {
        let line =
            serde_json::json!({TURN_REPLY_LINE_KEY: {"text": " \n ", "turn_id": "t1"}}).to_string();
        let empty = reply_from_line(&line).expect("a blank reply is a turn end");
        assert!(empty.is_empty());
        assert_eq!(empty.text, "");
        assert_eq!(empty.turn_id.as_deref(), Some("t1"));

        let hub = TurnReplyHub::default();
        let mut rx = hub.subscribe().unwrap().rx;
        assert!(hub.publish("a", "p", empty).is_some());
        assert_eq!(
            hub.publish("a", "p", reply(Some("t1"), "late text for the same turn")),
            None,
            "the turn already has its frame"
        );
        assert!(hub.publish("a", "p", reply(None, "")).is_some());
        assert!(rx.try_recv().unwrap().reply.is_empty());
        assert!(rx.try_recv().unwrap().reply.is_empty());
        assert!(rx.try_recv().is_err());
    }
}
