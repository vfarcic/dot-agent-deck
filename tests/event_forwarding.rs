//! M2.17 — daemon → TUI hook-event forwarding.
//!
//! Walks the full chain end-to-end: spin up a real daemon (hook socket +
//! attach socket via `run_daemon_with` / `Daemon::with_attach`), open a
//! `SubscribeEvents` connection from the client side, write a JSON
//! `AgentEvent` to the hook socket, and verify the same event reaches a
//! consumer `AppState.apply_event` via the broadcast bridge. This is the
//! regression guard for the symptom that motivated the milestone — the
//! remote-mode dashboard had no path from hook ingestion to the TUI's
//! `AppState`, so live tool counts / prompts / agent type stayed at
//! placeholder defaults.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
use dot_agent_deck::state::AppState;

mod common;

// Same umask-narrowing serialization as the other integration test binaries.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct DaemonHandle {
    _dir: TempDir,
    hook_path: PathBuf,
    attach_path: PathBuf,
    daemon_state: Arc<RwLock<AppState>>,
    handle: JoinHandle<()>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn spawn_daemon() -> DaemonHandle {
    common::init_test_env();
    let (dir, hook_path, attach_path) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hook.sock");
        let attach = dir.path().join("attach.sock");
        (dir, hook, attach)
    };

    let daemon_state = Arc::new(RwLock::new(AppState::default()));
    let state_for_daemon = daemon_state.clone();
    let attach_for_daemon = attach_path.clone();
    let hook_for_daemon = hook_path.clone();
    let lock_dir = common::lock_dir_path();
    let handle = tokio::spawn(async move {
        let daemon = Daemon::with_attach(state_for_daemon, attach_for_daemon)
            .with_lock_dir_override(lock_dir);
        let _ = run_daemon_with(&hook_for_daemon, daemon).await;
    });

    // Wait for the attach socket to be accept()-ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if attach_path.exists() && UnixStream::connect(&attach_path).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        attach_path.exists(),
        "attach socket did not appear within 5s"
    );

    DaemonHandle {
        _dir: dir,
        hook_path,
        attach_path,
        daemon_state,
        handle,
    }
}

/// Write one JSON AgentEvent + newline to the daemon's hook socket, which is
/// the same wire shape `crate::hook::send_to_socket` uses in production.
async fn send_hook_event(hook_path: &PathBuf, event: &AgentEvent) {
    let mut stream = UnixStream::connect(hook_path).await.expect("connect hook");
    let mut json = serde_json::to_vec(event).unwrap();
    json.push(b'\n');
    stream.write_all(&json).await.unwrap();
    stream.shutdown().await.unwrap();
}

fn make_tool_start_event(session_id: &str, pane_id: &str) -> AgentEvent {
    AgentEvent {
        session_id: session_id.into(),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::ToolStart,
        tool_name: Some("Read".into()),
        tool_detail: Some("src/main.rs".into()),
        cwd: Some("/work".into()),
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: std::collections::HashMap::new(),
        pane_id: Some(pane_id.into()),
        agent_id: None,
    }
}

/// Full chain: hook JSON → daemon hook loop → broadcast → attached client →
/// AppState. Mirrors what the TUI does in remote mode: holds its own
/// `AppState`, subscribes to the daemon, and feeds every received event
/// into `apply_event`.
#[tokio::test]
async fn hook_event_round_trips_to_attached_appstate() {
    let daemon = spawn_daemon().await;

    // Stand in for the TUI's AppState — separate from `daemon.daemon_state`,
    // mirroring the post-pivot architecture where the two processes have
    // independent `AppState` instances.
    let tui_state = Arc::new(RwLock::new(AppState::default()));
    tui_state.write().await.register_pane("p-17".into());

    // Open the subscribe connection BEFORE writing the hook event, so we
    // know the subscriber is wired into the broadcast when the event lands.
    let client = DaemonClient::new(daemon.attach_path.clone());
    let mut sub = client.subscribe_events().await.expect("subscribe ok");

    // Forwarder task: drain events into the TUI's AppState exactly the way
    // `spawn_event_subscriber` in main.rs does.
    let state_for_task = tui_state.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(Some(msg)) = sub.next_event().await {
            let BroadcastMsg::Event(event) = msg;
            state_for_task.write().await.apply_event(event);
        }
    });

    // Also register the same pane in the daemon's AppState so its own
    // apply_event accepts the event — keeps daemon-side state authoritative
    // and rules out a regression that silently dropped the broadcast when
    // apply_event rejected an unmanaged pane.
    daemon
        .daemon_state
        .write()
        .await
        .register_pane("p-17".into());

    // Drive: one ToolStart hook event.
    send_hook_event(&daemon.hook_path, &make_tool_start_event("sess-A", "p-17")).await;

    // Poll the TUI's AppState until the event materialises. Bounded so a
    // regression manifests as a quick test failure rather than a hang.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let s = tui_state.read().await;
        if let Some(sess) = s.sessions.get("sess-A")
            && sess.active_tool.is_some()
        {
            saw = true;
            break;
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw,
        "expected ToolStart event to reach the TUI's AppState via SubscribeEvents"
    );

    let s = tui_state.read().await;
    let sess = s.sessions.get("sess-A").expect("session present");
    assert_eq!(sess.agent_type, AgentType::ClaudeCode);
    assert_eq!(
        sess.active_tool.as_ref().map(|t| t.name.as_str()),
        Some("Read")
    );
    drop(s);

    forwarder.abort();
}

/// Sending a hook event with no subscribers must not be a fatal error on
/// the daemon side — `broadcast::Sender::send` returns Err when there are
/// zero receivers, and the hook loop ignores it. Without that, the daemon
/// would still apply events to its own AppState but the local-mode TUI
/// (which doesn't subscribe) would log a confusing error path.
#[tokio::test]
async fn hook_event_with_no_subscribers_still_applied_locally() {
    let daemon = spawn_daemon().await;
    daemon
        .daemon_state
        .write()
        .await
        .register_pane("p-17".into());

    send_hook_event(&daemon.hook_path, &make_tool_start_event("sess-B", "p-17")).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let s = daemon.daemon_state.read().await;
        if s.sessions.contains_key("sess-B") {
            saw = true;
            break;
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw,
        "daemon-side AppState should still receive events with zero subscribers"
    );
}

/// A subscriber that falls behind the broadcast capacity must be torn down
/// gracefully — the daemon emits `KIND_STREAM_END "lagged"` (covered by
/// `EventSubscription::next_event` as `Ok(None)`) instead of panicking or
/// pinning the connection. Stands up an attach server with a deliberately
/// tiny broadcast (capacity 2) so a single burst overflows it well before
/// the daemon's recv loop can drain it.
#[tokio::test]
async fn lagged_subscriber_receives_stream_end() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("attach.sock");

    let (event_tx, _) = broadcast::channel::<BroadcastMsg>(2);
    let registry = Arc::new(AgentPtyRegistry::new());

    let listener = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        bind_attach_listener(&socket_path).expect("bind attach listener")
    };

    let server_event_tx = event_tx.clone();
    let server_handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry, server_event_tx).await;
    });

    let client = DaemonClient::new(socket_path.clone());
    let mut sub = client.subscribe_events().await.expect("subscribe ok");

    // Flood the broadcast far beyond capacity (2) in a tight, await-free
    // burst — broadcast::Sender::send is sync, so by the time the daemon's
    // recv task is scheduled, the channel has already overflowed and its
    // first recv() returns RecvError::Lagged.
    for i in 0..5000 {
        let event = make_tool_start_event(&format!("sess-lag-{i}"), "p-lag");
        let _ = event_tx.send(BroadcastMsg::Event(event));
    }

    // Pull events until the daemon ends the stream. Some events may have
    // been written to the wire before the receiver lagged; the assertion
    // is that the stream eventually terminates with Ok(None) and no
    // panic, not that we see zero events.
    let test_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut got_terminal = false;
    while tokio::time::Instant::now() < test_deadline {
        match tokio::time::timeout(Duration::from_millis(500), sub.next_event()).await {
            Ok(Ok(None)) => {
                got_terminal = true;
                break;
            }
            Ok(Ok(Some(_))) => continue,
            Ok(Err(e)) => panic!("unexpected subscription error: {e}"),
            Err(_) => continue,
        }
    }
    assert!(
        got_terminal,
        "lagged subscriber should receive Ok(None) after broadcast overflow"
    );

    server_handle.abort();
}
