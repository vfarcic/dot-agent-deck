//! Issue #1664: the hook loop applies one session's events in the order their
//! connections reached it, even when the later connection's task reads its
//! line first.
//!
//! A hook process sends each event on a connection of its own and exits once
//! the write completes, so a session's next event is on a fresh connection
//! that the daemon accepts after the previous one. Each connection is its own
//! task; under load the later task can run first. The tests make that happen
//! on demand: the earlier connection is accepted first but writes its line
//! only after the later one has been sent, which from inside the daemon is
//! the same as the earlier task being scheduled late.

use super::*;
use spec::spec;
use std::os::unix::fs::PermissionsExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

/// The hook loop on a socket of its own, with `limits`.
struct Loop {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    state: SharedState,
    handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
}

impl Drop for Loop {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Loop {
    async fn start(limits: HookLoopLimits) -> Self {
        Self::start_with(limits, Arc::new(AgentPtyRegistry::new())).await
    }

    /// [`Self::start`], serving the panes `registry` spawned.
    async fn start_with(limits: HookLoopLimits, registry: Arc<AgentPtyRegistry>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        tokio::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .await
            .expect("chmod tempdir");
        let socket = dir.path().join("hook.sock");
        // Bound without `bind_socket` for the reason `HookLoopFixture` gives:
        // that helper flips the process-wide umask around `bind`.
        let listener = IpcListener::from_tokio_listener(
            UnixListener::bind(&socket).expect("bind hook socket"),
        );
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let state = state.clone();
            async move {
                run_hook_loop_with_limits(
                    listener,
                    state,
                    event_tx,
                    registry,
                    Arc::new(Notify::new()),
                    crate::issue_dispatch_run::new_worktree_registry(),
                    limits,
                )
                .await
            }
        });
        Self {
            _dir: dir,
            socket,
            state,
            handle,
        }
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.socket)
            .await
            .expect("connect hook socket")
    }

    /// The event types `session` has had applied, oldest first.
    async fn applied(&self, session: &str) -> Vec<crate::event::EventType> {
        self.state
            .read()
            .await
            .sessions
            .get(session)
            .map(|s| {
                s.recent_events
                    .iter()
                    .map(|e| e.event_type.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn status(&self, session: &str) -> Option<crate::state::SessionStatus> {
        self.state
            .read()
            .await
            .sessions
            .get(session)
            .map(|s| s.status.clone())
    }

    /// Wait up to `budget` for `session` to have had `count` events applied.
    async fn wait_applied(&self, session: &str, count: usize, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if self.applied(session).await.len() >= count {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// One raw OpenCode event line for `session`.
fn event(session: &str, event_type: &str) -> String {
    let line = serde_json::json!({
        "session_id": session, "agent_type": "open_code",
        "event_type": event_type, "timestamp": "2026-10-30T12:00:00Z",
        "metadata": {},
    });
    format!("{line}\n")
}

/// Write `line` on `stream` and close it, as a hook process does.
async fn send(mut stream: UnixStream, line: &str) {
    stream
        .write_all(line.as_bytes())
        .await
        .expect("write hook line");
    stream.shutdown().await.expect("close hook connection");
}

/// Production limits, except that an earlier connection that has not sent its
/// line yet holds later events back for as long as the test can take, so how
/// late `first` writes cannot decide the result on a loaded box.
fn limits() -> HookLoopLimits {
    HookLoopLimits {
        event_order_wait: Duration::from_secs(60),
        ..HookLoopLimits::production(HOOK_CONNECTION_IDLE_TIMEOUT)
    }
}

/// Scenario: Drive the hook loop over a real socket. One session's `session_start` is sent on a connection the daemon accepts first, and its `quota_blocked` on a connection accepted after it, but the second line is written first and the first only once the daemon has had the chance to apply the second. The session must have both applied in the order they were accepted and read Blocked; a control session sent the ordinary way reads Blocked too. The same holds when the earlier connection first sends an `inbox_announce` and its `session_start` after it, as a `SessionStart` hook does.
#[spec("hooks/ingest/009")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_009_one_sessions_events_apply_in_the_order_they_were_sent() {
    use crate::event::EventType::{QuotaBlocked, SessionStart};
    let deck = Loop::start(limits()).await;

    // Control: the same two events, each written as soon as its connection
    // is open. This is the path an idle box takes, and it reads Blocked.
    send(deck.connect().await, &event("in-order", "session_start")).await;
    assert!(
        deck.wait_applied("in-order", 1, Duration::from_secs(10))
            .await,
        "the control session's session_start was never applied"
    );
    send(deck.connect().await, &event("in-order", "quota_blocked")).await;
    assert!(
        deck.wait_applied("in-order", 2, Duration::from_secs(10))
            .await,
        "the control session's quota_blocked was never applied"
    );
    assert_eq!(
        deck.applied("in-order").await,
        vec![SessionStart, QuotaBlocked]
    );
    assert_eq!(
        deck.status("in-order").await,
        Some(crate::state::SessionStatus::Blocked),
        "control: session_start then quota_blocked must leave the card Blocked"
    );

    // The reordering: `first` is accepted ahead of `second` (one listener,
    // FIFO backlog, connected in this order), but `second`'s line is read
    // first.
    let first = deck.connect().await;
    let second = deck.connect().await;
    send(second, &event("reordered", "quota_blocked")).await;
    // Give the daemon the chance to apply `second` ahead of `first`. With
    // the events ordered it never does, and this wait runs out; without, the
    // quota_blocked is applied here, which is the defect.
    let overtook = deck
        .wait_applied("reordered", 1, Duration::from_secs(1))
        .await;
    send(first, &event("reordered", "session_start")).await;
    assert!(
        deck.wait_applied("reordered", 2, Duration::from_secs(10))
            .await,
        "both of the reordered session's events must be applied, got {:?}",
        deck.applied("reordered").await
    );
    assert_eq!(
        deck.status("reordered").await,
        Some(crate::state::SessionStatus::Blocked),
        "a quota-blocked card must not be reset to Idle by the session_start sent before it"
    );
    assert_eq!(
        deck.applied("reordered").await,
        vec![SessionStart, QuotaBlocked],
        "one session's events must be applied in the order their connections were accepted"
    );
    assert!(
        !overtook,
        "the quota_blocked was applied before the session_start its sender sent first"
    );

    // The same, with the earlier connection carrying an `inbox_announce`
    // ahead of its event, as a `SessionStart` hook with an inbox waiter sends
    // them (`hook::send_lines_and_drain`). The announce is read first and
    // must keep the connection's place for the event behind it.
    let mut first = deck.connect().await;
    let second = deck.connect().await;
    send(second, &event("announced", "quota_blocked")).await;
    first
        .write_all(format!("{}\n", announce()).as_bytes())
        .await
        .expect("write the announce");
    let overtook = deck
        .wait_applied("announced", 1, Duration::from_secs(1))
        .await;
    send(first, &event("announced", "session_start")).await;
    assert!(
        deck.wait_applied("announced", 2, Duration::from_secs(10))
            .await,
        "both of the announced session's events must be applied, got {:?}",
        deck.applied("announced").await
    );
    assert_eq!(
        deck.status("announced").await,
        Some(crate::state::SessionStatus::Blocked),
        "an announce ahead of the session_start must not let the quota_blocked overtake it"
    );
    assert_eq!(
        deck.applied("announced").await,
        vec![SessionStart, QuotaBlocked]
    );
    assert!(
        !overtook,
        "the quota_blocked was applied while the announced session_start's connection held \
         only its announce"
    );
}

/// An `inbox_announce` for a pane this loop never issued a token for. The
/// daemon refuses it with a reply line and goes on reading the connection.
fn announce() -> String {
    serde_json::to_string(&DaemonMessage::InboxAnnounce(
        crate::event::InboxAnnounceRequest {
            pane_id: "pane-announced".to_string(),
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

/// Scenario: Drive the hook loop with a short predecessor wait. A peer connects and never writes; a session's `session_start` is then sent on a connection accepted after it. The event must still be applied, once the silent peer has had its predecessor wait, while that peer's connection is still open.
#[spec("hooks/ingest/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_010_a_silent_earlier_connection_holds_later_events_only_briefly() {
    let deck = Loop::start(HookLoopLimits {
        event_order_wait: Duration::from_millis(200),
        ..HookLoopLimits::production(HOOK_CONNECTION_IDLE_TIMEOUT)
    })
    .await;
    let silent = deck.connect().await;
    send(
        deck.connect().await,
        &event("behind-silent", "session_start"),
    )
    .await;
    // Far short of the 60 s idle timeout that would reclaim the silent peer,
    // so only the predecessor wait running out can let the event through.
    assert!(
        deck.wait_applied("behind-silent", 1, Duration::from_secs(20))
            .await,
        "an event behind a connection that never writes must be applied once that \
         connection has had its predecessor wait"
    );
    drop(silent);
}

/// One raw OpenCode event line for `session` in `pane`, carrying `token`.
fn event_in(session: &str, event_type: &str, pane: &str, token: &str) -> String {
    let line = serde_json::json!({
        "session_id": session, "agent_type": "open_code",
        "event_type": event_type, "timestamp": "2026-10-30T12:00:00Z",
        "metadata": {}, "pane_id": pane, "token": token,
    });
    format!("{line}\n")
}

/// One raw OpenCode event line for `session` in `pane`, a pane this daemon
/// never spawned, carrying no token: an outside agent's.
fn outside_event(session: &str, event_type: &str, pane: &str) -> String {
    let line = serde_json::json!({
        "session_id": session, "agent_type": "open_code",
        "event_type": event_type, "timestamp": "2026-10-30T12:00:00Z",
        "metadata": {}, "pane_id": pane,
    });
    format!("{line}\n")
}

/// An `inbox_announce` claiming `pane`, presenting `token`.
fn announce_for(pane: &str, token: Option<String>) -> String {
    let line = serde_json::to_string(&DaemonMessage::InboxAnnounce(
        crate::event::InboxAnnounceRequest {
            pane_id: pane.to_string(),
            agent_id: None,
            token,
            component: crate::event::InboxComponent::ClaudeHooks,
            component_version: None,
            wake: crate::event::InboxWake::TurnEnd,
            pid: None,
        },
    ))
    .expect("serialize an inbox_announce");
    format!("{line}\n")
}

/// Scenario: Drive the hook loop over a real socket for panes the daemon spawned, so their events carry an attested token. Another peer sends an `inbox_announce` the daemon cannot attest and keeps its connection open; a genuine `session_start` of a spawned pane, accepted after it, must be applied straight away rather than after the predecessor wait. The same holds behind an announce whose token provenance refuses, and an unattested event behind that refused announce goes ahead too; as a control, a genuine pane's own two events are still applied in the order they were sent.
#[spec("hooks/ingest/011")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_011_an_unproven_line_does_not_hold_back_a_genuine_event_of_another_pane() {
    use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
    use crate::event::EventType::{QuotaBlocked, SessionStart};
    let registry = Arc::new(AgentPtyRegistry::new());
    // One spawned pane per case, so each case's genuine session is the only
    // one its pane has had.
    let spawn = |pane: &str| {
        let agent = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn a genuine pane's agent");
        registry
            .hook_token_of(&agent)
            .expect("the pane's capability")
    };
    let (token_1, token_2, token_3) = (spawn("pane-1"), spawn("pane-2"), spawn("pane-3"));
    // Far short of the 60 s predecessor wait `limits()` pins, so an event
    // held back by the earlier connection cannot be applied within it.
    let promptly = Duration::from_secs(10);
    let deck = Loop::start_with(limits(), registry.clone()).await;

    // An announce for a pane this daemon never spawned: provenance admits it
    // as unattested, and its connection stays open with no event after it.
    let mut unattested = deck.connect().await;
    unattested
        .write_all(announce_for("pane-outside", None).as_bytes())
        .await
        .expect("write the unattested announce");
    send(
        deck.connect().await,
        &event_in("genuine-1", "session_start", "pane-1", &token_1),
    )
    .await;
    assert!(
        deck.wait_applied("genuine-1", 1, promptly).await,
        "a genuine event of one pane was held back by an unattested announce of another pane"
    );
    drop(unattested);

    // An announce provenance refuses: a well-formed token this daemon never
    // minted. It must give up its place, so neither a genuine event nor an
    // unattested one waits behind it.
    let mut refused = deck.connect().await;
    refused
        .write_all(announce_for("pane-elsewhere", Some(crate::hook_provenance::mint())).as_bytes())
        .await
        .expect("write the refused announce");
    send(
        deck.connect().await,
        &event_in("genuine-2", "session_start", "pane-2", &token_2),
    )
    .await;
    send(
        deck.connect().await,
        &outside_event("outside-2", "session_start", "pane-outside-2"),
    )
    .await;
    assert!(
        deck.wait_applied("genuine-2", 1, promptly).await,
        "a genuine event was held back by an announce that provenance refused"
    );
    assert!(
        deck.wait_applied("outside-2", 1, promptly).await,
        "an announce that provenance refused kept its place in line"
    );
    drop(refused);

    // Control: the genuine pane's own events, with the earlier connection
    // written late, are still applied in the order they were accepted.
    let first = deck.connect().await;
    let second = deck.connect().await;
    send(
        second,
        &event_in("genuine-3", "quota_blocked", "pane-3", &token_3),
    )
    .await;
    let overtook = deck
        .wait_applied("genuine-3", 1, Duration::from_secs(1))
        .await;
    send(
        first,
        &event_in("genuine-3", "session_start", "pane-3", &token_3),
    )
    .await;
    assert!(
        deck.wait_applied("genuine-3", 2, promptly).await,
        "both of the genuine session's events must be applied, got {:?}",
        deck.applied("genuine-3").await
    );
    assert_eq!(
        deck.applied("genuine-3").await,
        vec![SessionStart, QuotaBlocked],
        "one genuine session's events must still be applied in the order they were sent"
    );
    assert!(
        !overtook,
        "the genuine quota_blocked overtook its session_start"
    );
    registry.shutdown_all();
}
