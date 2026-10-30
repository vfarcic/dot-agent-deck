//! Issue #1664: apply one session's hook events in the order they were sent.
//!
//! A hook process sends each event on a connection of its own and exits once
//! the write completes, so a session's next event arrives on a fresh
//! connection the daemon accepts after the previous one. The hook loop serves
//! every connection in a task of its own, and nothing ordered those tasks:
//! under load the later one could run first, so a `QuotaBlocked` was applied
//! before the `SessionStart` sent ahead of it, and the `SessionStart` then
//! reset the blocked card to Idle.
//!
//! When a sender opens its next connection only after its previous write
//! completed, as a hook process run once per event does, the order its
//! connections were ACCEPTED in is the order it sent them in. So the loop takes
//! a [`Ticket`] for each connection as it accepts it
//! ([`HookEventOrder::admit`]), and a raw event waits in
//! [`turn_for_line`] until every connection accepted ahead of it that carries
//! an event of the same session, or of the same pane, has been handled. Events
//! of other sessions and panes do not wait for each other.
//!
//! What an earlier connection carries is not known until its task reads it,
//! so an event also waits for earlier connections that have not yet sent a
//! line, but only until [`PREDECESSOR_WAIT`] after each was accepted. A peer
//! that connects and stays silent therefore delays later events by that much
//! at most, and only while it is new, rather than until the connection's idle
//! timeout reclaims it. A box starved for longer than that can still apply
//! two events out of order; this narrows the race to that, it does not close
//! it.
//!
//! Only a connection's first raw event takes part, and only when nothing but
//! an `inbox_announce` came before it on that connection: hook producers send
//! a raw event on a connection of its own, a `SessionStart` hook behind the
//! announce of its agent's inbox waiter (`hook::send_lines_and_drain`), and
//! lines on one connection are already read in order. That announce keeps the
//! connection's place for the event after it. Every other `DaemonMessage`
//! line, a `question` among them, is not ordered here: its ticket is released
//! when the line is read, so a held question or a dispatch never holds back
//! anyone's events.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Notify;
use tokio::time::Instant;
use tracing::debug;

/// How long after its accept an earlier connection that has sent nothing yet
/// can hold back a later event. A sender writes its line straight after
/// connecting, so this only has to cover a task that has not been scheduled
/// yet; a connection still silent past it is not a predecessor anyone is
/// waiting on, but a stalled or leaked peer.
pub(crate) const PREDECESSOR_WAIT: Duration = Duration::from_secs(2);

/// The order hook connections were accepted in, shared by the loop and every
/// connection task it spawns.
#[derive(Clone)]
pub(crate) struct HookEventOrder {
    shared: Arc<Shared>,
}

struct Shared {
    open: Mutex<Open>,
    changed: Notify,
    predecessor_wait: Duration,
}

#[derive(Default)]
struct Open {
    next: u64,
    /// Every connection not yet handled, by accept order.
    connections: BTreeMap<u64, Connection>,
}

struct Connection {
    accepted_at: Instant,
    /// `None` until the connection's first line is read.
    keys: Option<Keys>,
}

/// What an event is ordered by: its session, and its pane when it names one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Keys {
    session: String,
    pane: Option<String>,
}

impl Keys {
    fn overlaps(&self, other: &Keys) -> bool {
        self.session == other.session
            || matches!((&self.pane, &other.pane), (Some(a), Some(b)) if a == b)
    }
}

impl HookEventOrder {
    pub(crate) fn new(predecessor_wait: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                open: Mutex::new(Open::default()),
                changed: Notify::new(),
                predecessor_wait,
            }),
        }
    }

    /// A ticket for a connection the loop has just accepted. Called in accept
    /// order, from the accept loop itself, before the connection's task is
    /// spawned.
    pub(crate) fn admit(&self) -> Ticket {
        let mut open = self.shared.lock();
        let seq = open.next;
        open.next += 1;
        open.connections.insert(
            seq,
            Connection {
                accepted_at: Instant::now(),
                keys: None,
            },
        );
        Ticket {
            shared: Arc::clone(&self.shared),
            seq,
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Open> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A connection's place in accept order. Dropping it — the connection's line
/// handled, or the connection gone without one — lets the connections behind
/// it go ahead.
pub(crate) struct Ticket {
    shared: Arc<Shared>,
    seq: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.shared.lock().connections.remove(&self.seq);
        self.shared.changed.notify_waiters();
    }
}

/// Held while a raw event is applied; the connections behind it with the same
/// session or pane wait until it is dropped.
pub(crate) struct Turn {
    _ticket: Ticket,
}

/// The fields of a hook line that decide whether, and by what, it is ordered.
#[derive(Deserialize)]
struct OrderedFields {
    #[serde(default)]
    message_type: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    pane_id: Option<String>,
}

/// How the hook loop will read a line, as far as ordering is concerned.
#[derive(Debug, PartialEq, Eq)]
enum LineKind {
    /// A raw `AgentEvent`, with its session and pane.
    Event(Keys),
    /// An `inbox_announce`, which a `SessionStart` hook sends ahead of its
    /// event on the same connection.
    Announce,
    /// Anything else.
    Other,
}

/// How the hook loop will read `line`. The loop tries `DaemonMessage` first,
/// so a line that parses as one is not an event here either.
fn line_kind(line: &str) -> LineKind {
    let Ok(fields) = serde_json::from_str::<OrderedFields>(line) else {
        return LineKind::Other;
    };
    if fields.message_type.is_some() {
        match serde_json::from_str::<crate::event::DaemonMessage>(line) {
            Ok(crate::event::DaemonMessage::InboxAnnounce(_)) => return LineKind::Announce,
            Ok(_) => return LineKind::Other,
            Err(_) => {}
        }
    }
    match fields.session_id {
        Some(session) => LineKind::Event(Keys {
            session,
            pane: fields.pane_id,
        }),
        None => LineKind::Other,
    }
}

/// Called with each line a hook connection reads, and the connection's ticket
/// while it still has one. For a raw event that the ticket still covers,
/// waits until the events accepted ahead of it with the same session or pane
/// have been handled, and returns the turn to hold while it is applied. An
/// `inbox_announce` leaves the ticket for the line after it; anything else
/// releases it. There is no turn for either.
pub(crate) async fn turn_for_line(ticket: &mut Option<Ticket>, line: &str) -> Option<Turn> {
    ticket.as_ref()?;
    let keys = match line_kind(line) {
        LineKind::Event(keys) => keys,
        LineKind::Announce => return None,
        LineKind::Other => {
            ticket.take();
            return None;
        }
    };
    let ticket = ticket.take()?;
    let shared = Arc::clone(&ticket.shared);
    if let Some(connection) = shared.lock().connections.get_mut(&ticket.seq) {
        connection.keys = Some(keys.clone());
    }
    // A later connection may be waiting for this one to say what it carries.
    shared.changed.notify_waiters();
    let mut gave_up_on_silent = false;
    loop {
        let changed = shared.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let now = Instant::now();
        let (same_keys_ahead, silent_until) = {
            let open = shared.lock();
            let mut same_keys_ahead = false;
            let mut silent_until: Option<Instant> = None;
            for connection in open.connections.range(..ticket.seq).map(|(_, c)| c) {
                match &connection.keys {
                    Some(theirs) if theirs.overlaps(&keys) => same_keys_ahead = true,
                    Some(_) => {}
                    None => {
                        let until = connection.accepted_at + shared.predecessor_wait;
                        if until > now {
                            silent_until = Some(silent_until.map_or(until, |u| u.min(until)));
                        } else {
                            gave_up_on_silent = true;
                        }
                    }
                }
            }
            (same_keys_ahead, silent_until)
        };
        if !same_keys_ahead && silent_until.is_none() {
            break;
        }
        match silent_until {
            Some(until) => {
                let _ = tokio::time::timeout_at(until, changed).await;
            }
            None => changed.await,
        }
    }
    if gave_up_on_silent {
        debug!(
            predecessor_wait_ms = shared.predecessor_wait.as_millis(),
            "hook event order: applying an event ahead of an earlier connection that has \
             sent nothing within the predecessor wait"
        );
    }
    Some(Turn { _ticket: ticket })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(session: &str, pane: Option<&str>) -> String {
        serde_json::json!({
            "session_id": session, "agent_type": "open_code",
            "event_type": "session_start", "timestamp": "2026-10-30T12:00:00Z",
            "pane_id": pane,
        })
        .to_string()
    }

    /// Whether `fut` is still waiting after the runtime has had every chance
    /// to run it (time is paused, so this advances nothing).
    async fn still_waiting<F: std::future::Future>(fut: &mut std::pin::Pin<Box<F>>) -> bool {
        tokio::select! {
            biased;
            _ = fut.as_mut() => false,
            _ = tokio::task::yield_now() => true,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_event_of_the_same_session_waits_for_the_earlier_one() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let second = order.admit();
        let line = event("s", None);
        let mut second = Some(second);
        let mut later = Box::pin(turn_for_line(&mut second, &line));
        assert!(
            still_waiting(&mut later).await,
            "the earlier one has not been read"
        );
        let earlier = turn_for_line(&mut Some(first), &event("s", None))
            .await
            .expect("an event takes a turn");
        assert!(
            still_waiting(&mut later).await,
            "the earlier one is being applied"
        );
        drop(earlier);
        assert!(later.await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_pane_orders_events_of_different_sessions() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let second = order.admit();
        let earlier = turn_for_line(&mut Some(first), &event("old", Some("p")))
            .await
            .expect("an event takes a turn");
        let line = event("new", Some("p"));
        let mut second = Some(second);
        let mut later = Box::pin(turn_for_line(&mut second, &line));
        assert!(still_waiting(&mut later).await);
        drop(earlier);
        assert!(later.await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn other_sessions_do_not_wait_once_their_predecessor_is_read() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let second = order.admit();
        let _earlier = turn_for_line(&mut Some(first), &event("a", Some("p1")))
            .await
            .expect("an event takes a turn");
        assert!(
            turn_for_line(&mut Some(second), &event("b", Some("p2")))
                .await
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_message_releases_its_ticket_and_takes_no_turn() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let second = order.admit();
        let message = serde_json::json!({
            "message_type": "work_done", "pane_id": "p", "task": "t", "done": false,
            "session_id": "s", "timestamp": "2026-10-30T12:00:00Z",
        })
        .to_string();
        assert!(turn_for_line(&mut Some(first), &message).await.is_none());
        assert!(
            turn_for_line(&mut Some(second), &event("s", Some("p")))
                .await
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_gone_without_a_line_releases_its_place() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let second = order.admit();
        let line = event("s", None);
        let mut second = Some(second);
        let mut later = Box::pin(turn_for_line(&mut second, &line));
        assert!(still_waiting(&mut later).await);
        drop(first);
        assert!(later.await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_predecessor_holds_a_later_event_only_for_the_predecessor_wait() {
        let wait = Duration::from_secs(2);
        let order = HookEventOrder::new(wait);
        let _silent = order.admit();
        let second = order.admit();
        let started = Instant::now();
        assert!(
            turn_for_line(&mut Some(second), &event("s", None))
                .await
                .is_some()
        );
        assert_eq!(started.elapsed(), wait);
    }

    /// The `inbox_announce` a `SessionStart` hook sends ahead of its event on
    /// the same connection.
    fn announce(pane: &str) -> String {
        serde_json::to_string(&crate::event::DaemonMessage::InboxAnnounce(
            crate::event::InboxAnnounceRequest {
                pane_id: pane.to_string(),
                agent_id: None,
                token: None,
                component: crate::event::InboxComponent::ClaudeHooks,
                component_version: None,
                wake: crate::event::InboxWake::TurnEnd,
                pid: None,
            },
        ))
        .expect("serialize an inbox_announce")
    }

    #[tokio::test(start_paused = true)]
    async fn an_announce_keeps_the_place_for_the_event_after_it() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let mut first = Some(order.admit());
        let mut second = Some(order.admit());
        assert!(turn_for_line(&mut first, &announce("p")).await.is_none());
        assert!(first.is_some(), "the announce must not give up the place");
        let line = event("s", Some("p"));
        let mut later = Box::pin(turn_for_line(&mut second, &line));
        assert!(
            still_waiting(&mut later).await,
            "the announced event is not read yet"
        );
        let earlier = turn_for_line(&mut first, &event("s", Some("p")))
            .await
            .expect("the announced event takes the connection's turn");
        assert!(
            still_waiting(&mut later).await,
            "the announced event is being applied"
        );
        drop(earlier);
        assert!(later.await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_first_line_of_a_connection_is_ordered() {
        assert!(turn_for_line(&mut None, &event("s", None)).await.is_none());
    }
}
