//! Issue #1693: the hook loop's reply writes are bounded by the connection's
//! idle timeout, so a peer that sends lines without reading its replies loses
//! its hook-pool slot instead of holding it forever.

use super::*;
use spec::spec;
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// A line the provenance gate refuses whatever the pane — its token is not
/// even well formed — and answers with one refusal line on the connection.
fn malformed_work_done() -> String {
    let line = serde_json::json!({
        "message_type": "work_done", "pane_id": "no-such-pane",
        "task": "t", "done": false, "token": "not-a-token",
        "timestamp": "2026-10-27T12:00:00Z",
    });
    format!("{line}\n")
}

fn session_start(session_id: &str) -> String {
    let line = serde_json::json!({
        "session_id": session_id, "agent_type": "claude_code",
        "event_type": "session_start", "timestamp": "2026-10-27T12:00:00Z",
        "metadata": {},
    });
    format!("{line}\n")
}

/// Scenario: Drive the hook loop with room for one connection and a short idle bound. One peer sends refused `work_done` lines without ever reading the refusals, until the daemon's replies fill its socket buffer; a second peer then sends a `session_start`. The daemon must drop the non-reading connection — its replies end in EOF — and the second peer's event must be applied, which needs the only slot back.
#[spec("hooks/ingest/007")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_007_a_peer_that_never_reads_its_replies_loses_its_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("chmod tempdir");
    let socket = dir.path().join("hook.sock");
    // Bound without `bind_socket` for the reason `HookLoopFixture` gives: that
    // helper flips the process-wide umask around `bind`.
    let listener =
        IpcListener::from_tokio_listener(UnixListener::bind(&socket).expect("bind hook socket"));
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
    let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
    let limits = HookLoopLimits {
        idle_timeout: Duration::from_millis(300),
        // One slot, so the second peer's event can only be applied once the
        // non-reading peer's slot is freed.
        max_connections: 1,
        ..HookLoopLimits::production(Duration::from_millis(300))
    };
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

    // The peer that never reads: it writes refused lines until the daemon
    // drops the connection. The daemon stops reading once a reply write
    // blocks on the full buffer, so this writer blocks too, and only the
    // daemon dropping the connection ends it.
    let (mut replies, mut requests) = UnixStream::connect(&socket)
        .await
        .expect("connect hook socket")
        .into_split();
    let writer = tokio::spawn(async move {
        let line = malformed_work_done();
        while requests.write_all(line.as_bytes()).await.is_ok() {}
    });

    let mut live = UnixStream::connect(&socket)
        .await
        .expect("connect hook socket");
    live.write_all(session_start("after-reply-deadline").as_bytes())
        .await
        .expect("the write lands in the socket buffer even unaccepted");
    live.flush().await.unwrap();

    // The slot comes back. Without the write deadline the first connection's
    // task waits in `write_all` for good and this poll runs out.
    let mut applied = false;
    for _ in 0..400 {
        if state
            .read()
            .await
            .sessions
            .contains_key("after-reply-deadline")
        {
            applied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        applied,
        "an event behind a peer that never reads its replies must be applied once that \
         peer's slot is reclaimed"
    );

    // And the connection really was dropped: its writer is refused, and
    // reading its replies ends — in EOF, or in a reset when the daemon closed
    // with this peer's unread requests still queued on its side.
    tokio::time::timeout(Duration::from_secs(10), writer)
        .await
        .expect("the daemon dropped the connection, so the writer's next write fails")
        .expect("writer task");
    let mut drained = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(10), replies.read_to_end(&mut drained))
        .await
        .expect("reading the replies ends once the daemon drops the connection");
    match ended {
        Ok(_) => {}
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset,
            "the read ends because the daemon closed the connection: {e}"
        ),
    }

    drop(live);
    handle.abort();
    let _ = handle.await;
}

/// A write that cannot finish within the deadline reports failure rather than
/// waiting, and one that can reports success.
#[tokio::test]
async fn write_hook_reply_is_bounded_by_its_deadline() {
    let (mut ours, mut theirs) = tokio::io::duplex(16);
    let line = "x".repeat(64);
    assert!(
        !write_hook_reply(&mut ours, &line, Duration::from_millis(50)).await,
        "nobody reads the other end, so the write cannot finish"
    );
    let mut sink = vec![0u8; 16];
    let _ = theirs.read(&mut sink).await;
    let (mut ours, mut theirs) = tokio::io::duplex(1024);
    assert!(write_hook_reply(&mut ours, "ok\n", Duration::from_millis(50)).await);
    let mut got = [0u8; 3];
    theirs.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"ok\n");
}
