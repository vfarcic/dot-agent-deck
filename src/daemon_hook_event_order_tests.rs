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
                    Arc::new(AgentPtyRegistry::new()),
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

/// Scenario: Drive the hook loop over a real socket. One session's `session_start` is sent on a connection the daemon accepts first, and its `quota_blocked` on a connection accepted after it, but the second line is written first and the first only once the daemon has had the chance to apply the second. The session must have both applied in the order they were accepted and read Blocked; a control session sent the ordinary way reads Blocked too.
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
