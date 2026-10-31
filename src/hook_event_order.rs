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
//! Issue #2043: a line is classified by the provenance gate's own rules
//! ([`crate::hook_provenance::classify_event`] for a raw event,
//! [`crate::hook_provenance::classify`] for an `inbox_announce`) before it
//! waits or holds anyone back. A line the gate will refuse gives up its place
//! at once and takes no turn. A line the gate admits without attesting it, an
//! outside agent's, is ordered only behind other such lines and connections
//! not yet read, so it never holds back an attested event. The gate itself
//! still runs in the hook loop and decides what is acted on; this verdict
//! decides only who waits for whom. Both read the same registry, a moment
//! apart, so a token issued or retired in between can make them differ for
//! one line; ordering is all that follows this one. A connection that has
//! sent nothing yet is still waited for, as above, since nothing is known
//! about it.
//!
//! Only a connection's first raw event takes part, and only when nothing but
//! an `inbox_announce` came before it on that connection: hook producers send
//! a raw event on a connection of its own, a `SessionStart` hook behind the
//! announce of its agent's inbox waiter (`hook::send_lines_and_drain`), and
//! lines on one connection are already read in order. An announce the gate
//! admits keeps the connection's place for the event after it, for as long as
//! a connection that has sent nothing would hold it; an attested one holds
//! back only events of the pane it names. Every `DaemonMessage` line other
//! than an announce, a `question` among them, is not ordered here: its ticket
//! is released when the line is read, so a held question or a dispatch never
//! holds back anyone's events.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Notify;
use tokio::time::Instant;
use tracing::debug;

use crate::hook_provenance::{HookTokenDirectory, Policy, Provenance};

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
    slot: Slot,
}

/// What a connection is known to carry, as far as ordering is concerned.
#[derive(Clone, Debug)]
enum Slot {
    /// Nothing read yet.
    Silent,
    /// An admitted `inbox_announce` naming `pane`, its event not read yet.
    Announced { pane: String, standing: Standing },
    /// A raw event, waiting for its turn or being applied.
    Event { keys: Keys, standing: Standing },
}

/// Whether the provenance gate attests a line or admits it unproven. A line
/// it refuses has no standing and takes no place in line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Attested,
    Unproven,
}

impl Standing {
    /// `None` for a line the gate refuses under `policy`.
    fn of(provenance: &Provenance, policy: Policy) -> Option<Self> {
        match provenance {
            Provenance::Attested { .. } => Some(Standing::Attested),
            other => crate::hook_provenance::admits(other, policy)
                .ok()
                .map(|()| Standing::Unproven),
        }
    }

    /// Whether a connection of this standing can hold back an event of
    /// `waiter`'s standing: an unproven line never holds back an attested one.
    fn holds_back(self, waiter: Standing) -> bool {
        self == Standing::Attested || waiter == Standing::Unproven
    }
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
                slot: Slot::Silent,
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

    /// Record what connection `seq` carries, and wake whoever is waiting to
    /// learn it.
    fn set_slot(&self, seq: u64, slot: Slot) {
        if let Some(connection) = self.lock().connections.get_mut(&seq) {
            connection.slot = slot;
        }
        self.changed.notify_waiters();
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
    #[serde(default)]
    agent_id: Option<String>,
}

/// How the hook loop will read a line, as far as ordering is concerned, and
/// the standing the provenance gate will give it (`None`: refused).
#[derive(Debug, PartialEq, Eq)]
enum LineKind {
    /// A raw `AgentEvent`, with its session and pane.
    Event(Keys, Option<Standing>),
    /// An `inbox_announce`, which a `SessionStart` hook sends ahead of its
    /// event on the same connection, with the pane it names.
    Announce(String, Option<Standing>),
    /// Anything else.
    Other,
}

/// How the hook loop will read `line`. The loop tries `DaemonMessage` first,
/// so a line that parses as one is not an event here either.
fn line_kind(line: &str, directory: &impl HookTokenDirectory, policy: Policy) -> LineKind {
    let Ok(fields) = serde_json::from_str::<OrderedFields>(line) else {
        return LineKind::Other;
    };
    if fields.message_type.is_some() {
        match serde_json::from_str::<crate::event::DaemonMessage>(line) {
            Ok(msg @ crate::event::DaemonMessage::InboxAnnounce(_)) => {
                let provenance = crate::hook_provenance::classify(
                    msg.claimed_pane(),
                    msg.presented_token(),
                    directory,
                );
                return LineKind::Announce(
                    msg.claimed_pane().to_string(),
                    Standing::of(&provenance, policy),
                );
            }
            Ok(_) => return LineKind::Other,
            Err(_) => {}
        }
    }
    let Some(session) = fields.session_id else {
        return LineKind::Other;
    };
    // Read the way the hook loop's raw-event gate reads it: a `token` that is
    // not a string is a malformed token, not none.
    let presented = match serde_json::from_str::<crate::event::PresentedToken>(line) {
        Ok(presented) => presented.token,
        Err(_) => Some(String::new()),
    };
    let provenance = crate::hook_provenance::classify_event(
        fields.pane_id.as_deref(),
        fields.agent_id.as_deref(),
        presented.as_deref(),
        directory,
    );
    LineKind::Event(
        Keys {
            session,
            pane: fields.pane_id,
        },
        Standing::of(&provenance, policy),
    )
}

/// Called with each line a hook connection reads, and the connection's ticket
/// while it still has one; `directory` is what the provenance gate checks the
/// line against. For a raw event the gate admits and the ticket still covers,
/// the returned future waits until the events accepted ahead of it that it is
/// ordered behind have been handled, and yields the turn to hold while it is
/// applied. An `inbox_announce` the gate admits leaves the ticket for the line
/// after it; anything else, a line the gate refuses among them, releases it.
/// There is no turn for any of those.
///
/// The line is classified before the future is returned, so the future holds
/// nothing of `line` or `directory`.
pub(crate) fn turn_for_line<'a>(
    ticket: &'a mut Option<Ticket>,
    line: &str,
    directory: &impl HookTokenDirectory,
) -> impl Future<Output = Option<Turn>> + Send + 'a {
    let kind = ticket
        .is_some()
        .then(|| line_kind(line, directory, crate::hook_provenance::policy()));
    turn_for_kind(ticket, kind)
}

async fn turn_for_kind(ticket: &mut Option<Ticket>, kind: Option<LineKind>) -> Option<Turn> {
    let (keys, standing) = match kind? {
        LineKind::Event(keys, Some(standing)) => (keys, standing),
        LineKind::Announce(pane, Some(standing)) => {
            if let Some(ticket) = ticket.as_ref() {
                ticket
                    .shared
                    .set_slot(ticket.seq, Slot::Announced { pane, standing });
            }
            return None;
        }
        LineKind::Event(_, None) | LineKind::Announce(_, None) | LineKind::Other => {
            ticket.take();
            return None;
        }
    };
    let ticket = ticket.take()?;
    let shared = Arc::clone(&ticket.shared);
    // A later connection may be waiting for this one to say what it carries.
    shared.set_slot(
        ticket.seq,
        Slot::Event {
            keys: keys.clone(),
            standing,
        },
    );
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
                // Whether this connection, until its predecessor wait runs
                // out, may yet turn out to carry an event this one is ordered
                // behind.
                let pending = match &connection.slot {
                    Slot::Event {
                        keys: theirs,
                        standing: theirs_standing,
                    } => {
                        if theirs_standing.holds_back(standing) && theirs.overlaps(&keys) {
                            same_keys_ahead = true;
                        }
                        false
                    }
                    Slot::Silent => true,
                    Slot::Announced {
                        pane,
                        standing: Standing::Attested,
                    } => keys.pane.as_deref() == Some(pane.as_str()),
                    Slot::Announced {
                        standing: Standing::Unproven,
                        ..
                    } => standing == Standing::Unproven,
                };
                if !pending {
                    continue;
                }
                let until = connection.accepted_at + shared.predecessor_wait;
                if until > now {
                    silent_until = Some(silent_until.map_or(until, |u| u.min(until)));
                } else {
                    gave_up_on_silent = true;
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
    use crate::hook_provenance::{AttestFacts, TokenOwner};

    /// The token a [`Directory`] issued for its one pane, `spawned`.
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    /// Well-formed, but minted for nobody.
    const STRANGER: &str = "ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00";

    /// A daemon that spawned one agent, `a`, in pane `spawned`, with [`TOKEN`].
    struct Directory;

    impl HookTokenDirectory for Directory {
        fn owner_of_hook_token(&self, token: &str) -> Option<TokenOwner> {
            (token == TOKEN).then(|| TokenOwner {
                agent_id: "a".to_string(),
                pane_id: Some("spawned".to_string()),
                facts: AttestFacts::default(),
            })
        }

        fn pane_was_issued_a_hook_token(&self, pane_id: &str) -> bool {
            pane_id == "spawned"
        }

        fn paneless_agent_was_issued_a_hook_token(&self, _agent_id: &str) -> bool {
            false
        }

        fn token_owner_speaks_for_pane(&self, agent_id: &str, pane_id: &str) -> bool {
            agent_id == "a" && pane_id == "spawned"
        }
    }

    /// [`turn_for_line`] against [`Directory`].
    fn turn<'a>(
        ticket: &'a mut Option<Ticket>,
        line: &str,
    ) -> impl Future<Output = Option<Turn>> + Send + 'a {
        turn_for_line(ticket, line, &Directory)
    }

    fn event(session: &str, pane: Option<&str>) -> String {
        serde_json::json!({
            "session_id": session, "agent_type": "open_code",
            "event_type": "session_start", "timestamp": "2026-10-30T12:00:00Z",
            "pane_id": pane,
        })
        .to_string()
    }

    /// An event of pane `spawned` presenting `token`: attested when it is [`TOKEN`].
    fn event_of_p(session: &str, token: &str) -> String {
        serde_json::json!({
            "session_id": session, "agent_type": "open_code",
            "event_type": "session_start", "timestamp": "2026-10-30T12:00:00Z",
            "pane_id": "spawned", "token": token,
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
        let mut later = Box::pin(turn(&mut second, &line));
        assert!(
            still_waiting(&mut later).await,
            "the earlier one has not been read"
        );
        let earlier = turn(&mut Some(first), &event("s", None))
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
        let earlier = turn(&mut Some(first), &event("old", Some("p")))
            .await
            .expect("an event takes a turn");
        let line = event("new", Some("p"));
        let mut second = Some(second);
        let mut later = Box::pin(turn(&mut second, &line));
        assert!(still_waiting(&mut later).await);
        drop(earlier);
        assert!(later.await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn other_sessions_do_not_wait_once_their_predecessor_is_read() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let second = order.admit();
        let _earlier = turn(&mut Some(first), &event("a", Some("p1")))
            .await
            .expect("an event takes a turn");
        assert!(
            turn(&mut Some(second), &event("b", Some("p2")))
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
        assert!(turn(&mut Some(first), &message).await.is_none());
        assert!(
            turn(&mut Some(second), &event("s", Some("p")))
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
        let mut later = Box::pin(turn(&mut second, &line));
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
        assert!(turn(&mut Some(second), &event("s", None)).await.is_some());
        assert_eq!(started.elapsed(), wait);
    }

    /// The `inbox_announce` a `SessionStart` hook sends ahead of its event on
    /// the same connection.
    fn announce(pane: &str) -> String {
        announce_with(pane, None)
    }

    fn announce_with(pane: &str, token: Option<&str>) -> String {
        serde_json::to_string(&crate::event::DaemonMessage::InboxAnnounce(
            crate::event::InboxAnnounceRequest {
                pane_id: pane.to_string(),
                agent_id: None,
                token: token.map(str::to_string),
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
        assert!(turn(&mut first, &announce("p")).await.is_none());
        assert!(first.is_some(), "the announce must not give up the place");
        let line = event("s", Some("p"));
        let mut later = Box::pin(turn(&mut second, &line));
        assert!(
            still_waiting(&mut later).await,
            "the announced event is not read yet"
        );
        let earlier = turn(&mut first, &event("s", Some("p")))
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
    async fn attested_events_of_one_session_still_wait_for_each_other() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let mut second = Some(order.admit());
        let earlier = turn(&mut Some(first), &event_of_p("s", TOKEN))
            .await
            .expect("an attested event takes a turn");
        let line = event_of_p("s", TOKEN);
        let mut later = Box::pin(turn(&mut second, &line));
        assert!(still_waiting(&mut later).await);
        drop(earlier);
        assert!(later.await.is_some());
    }

    /// Issue #2043: an outside agent's event naming the same session as an
    /// attested one, applied ahead of it, does not hold the attested one
    /// back; another unproven event of that session still waits for it.
    #[tokio::test(start_paused = true)]
    async fn an_unproven_event_does_not_hold_back_an_attested_one() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let first = order.admit();
        let mut second = Some(order.admit());
        let mut third = Some(order.admit());
        let _unproven = turn(&mut Some(first), &event("s", Some("outside")))
            .await
            .expect("an unproven event takes a turn");
        let unproven_behind = event("s", None);
        let mut behind = Box::pin(turn(&mut third, &unproven_behind));
        assert!(
            turn(&mut second, &event_of_p("s", TOKEN)).await.is_some(),
            "an attested event waited behind an unproven one"
        );
        assert!(
            still_waiting(&mut behind).await,
            "control: an unproven event of the same session still waits"
        );
    }

    /// Issue #2043: a raw event provenance will refuse, here a token minted
    /// for nobody claiming pane `spawned`, takes no turn and gives up its place.
    #[tokio::test(start_paused = true)]
    async fn a_refused_event_gives_up_its_place_and_takes_no_turn() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let mut first = Some(order.admit());
        let mut second = Some(order.admit());
        assert!(turn(&mut first, &event_of_p("s", STRANGER)).await.is_none());
        assert!(first.is_none(), "a refused event must give up its place");
        let started = Instant::now();
        assert!(turn(&mut second, &event_of_p("s", TOKEN)).await.is_some());
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// Issue #2043: an `inbox_announce` provenance will refuse gives up its
    /// place, so even an unproven event behind it does not wait.
    #[tokio::test(start_paused = true)]
    async fn a_refused_announce_gives_up_its_place() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let mut first = Some(order.admit());
        let mut second = Some(order.admit());
        assert!(
            turn(&mut first, &announce_with("q", Some(STRANGER)))
                .await
                .is_none()
        );
        assert!(first.is_none(), "a refused announce must give up its place");
        let started = Instant::now();
        assert!(turn(&mut second, &event("s", None)).await.is_some());
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// Issue #2043: an unattested announce, its event not yet sent, holds
    /// back an unproven event but not an attested one.
    #[tokio::test(start_paused = true)]
    async fn an_unproven_announce_does_not_hold_back_an_attested_event() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let mut first = Some(order.admit());
        let mut second = Some(order.admit());
        let mut third = Some(order.admit());
        assert!(turn(&mut first, &announce("outside")).await.is_none());
        assert!(first.is_some(), "an admitted announce keeps its place");
        let unproven_behind = event("s", None);
        let mut behind = Box::pin(turn(&mut third, &unproven_behind));
        let started = Instant::now();
        assert!(turn(&mut second, &event_of_p("t", TOKEN)).await.is_some());
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "an attested event waited behind an unattested announce"
        );
        assert!(
            still_waiting(&mut behind).await,
            "control: an unproven event still waits for the announced one"
        );
    }

    /// An attested announce holds back only events of the pane it names.
    #[tokio::test(start_paused = true)]
    async fn an_attested_announce_holds_back_only_its_own_pane() {
        let order = HookEventOrder::new(Duration::from_secs(60));
        let mut first = Some(order.admit());
        let mut second = Some(order.admit());
        let mut third = Some(order.admit());
        assert!(
            turn(&mut first, &announce_with("spawned", Some(TOKEN)))
                .await
                .is_none()
        );
        assert!(first.is_some(), "an attested announce keeps its place");
        let same_pane = event_of_p("s", TOKEN);
        let mut held = Box::pin(turn(&mut third, &same_pane));
        assert!(
            turn(&mut second, &event("other", Some("q")))
                .await
                .is_some(),
            "an event of another pane waited behind the announce"
        );
        assert!(
            still_waiting(&mut held).await,
            "an event of the announced pane must wait for the announced event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_first_line_of_a_connection_is_ordered() {
        assert!(turn(&mut None, &event("s", None)).await.is_none());
    }
}
