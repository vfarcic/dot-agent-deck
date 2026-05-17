//! Orchestrator → role-agent delegate propagation across the
//! external-daemon hop.
//!
//! Regression guard for the bug where `dot-agent-deck delegate` from an
//! orchestrator pane sent its `DelegateSignal` to the daemon's hook
//! socket but the signal never reached the TUI: the daemon's own
//! `AppState.pane_role_map` is always empty in external-daemon mode (the
//! TUI owns the role map), so `state.handle_delegate` rejected every
//! signal as "delegate from unknown pane" and dropped it silently. The
//! `AgentEvent` broadcast added in M2.17 already forwards hook events to
//! attached TUIs; this test pins the equivalent forwarding for delegate
//! signals.
//!
//! End-to-end: a real daemon + attach server, a `subscribe_events`
//! connection from a synthetic TUI client, a `DelegateSignal` written
//! to the daemon's hook socket as JSON (same wire format
//! `crate::hook::send_to_socket` produces), and an assertion that the
//! signal materialises in the TUI-side `AppState.delegate_events` via
//! the broadcast bridge.
//!
//! NOTE: `pane_role_map` and `orchestrator_pane_ids` must be populated
//! in the TUI-side state *before* the signal arrives — the TUI-side
//! `handle_delegate` still validates that the sender is a registered
//! orchestrator (preserves the same anti-spoofing guard the daemon-side
//! pre-broadcast code applied in shared-state mode).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::event::{BroadcastMsg, DaemonMessage, DelegateSignal};
use dot_agent_deck::state::AppState;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct DaemonHandle {
    _dir: TempDir,
    hook_path: PathBuf,
    attach_path: PathBuf,
    handle: JoinHandle<()>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn spawn_daemon() -> DaemonHandle {
    let (dir, hook_path, attach_path) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hook.sock");
        let attach = dir.path().join("attach.sock");
        (dir, hook, attach)
    };

    let daemon_state = Arc::new(RwLock::new(AppState::default()));
    let attach_for_daemon = attach_path.clone();
    let hook_for_daemon = hook_path.clone();
    let handle = tokio::spawn(async move {
        let daemon = Daemon::with_attach(daemon_state, attach_for_daemon);
        let _ = run_daemon_with(&hook_for_daemon, daemon).await;
    });

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
        handle,
    }
}

async fn send_delegate(hook_path: &PathBuf, signal: &DelegateSignal) {
    let mut stream = UnixStream::connect(hook_path).await.expect("connect hook");
    let msg = DaemonMessage::Delegate(signal.clone());
    let mut json = serde_json::to_vec(&msg).unwrap();
    json.push(b'\n');
    stream.write_all(&json).await.unwrap();
    stream.shutdown().await.unwrap();
}

/// Full chain: orchestrator's delegate JSON → daemon hook loop →
/// broadcast → attached TUI subscriber → TUI-side `AppState`.
#[tokio::test]
async fn delegate_signal_round_trips_to_attached_appstate() {
    let daemon = spawn_daemon().await;

    // Stand in for the TUI's `AppState` — separate from the daemon's
    // (external-daemon mode). Populate the role map and mark the
    // orchestrator pane so `handle_delegate` accepts the signal.
    let tui_state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut st = tui_state.write().await;
        st.register_pane("orch-pane".into());
        st.pane_role_map
            .insert("orch-pane".into(), "orchestrator".into());
        st.orchestrator_pane_ids.insert("orch-pane".into());
    }

    let client = DaemonClient::new(daemon.attach_path.clone());
    let mut sub = client.subscribe_events().await.expect("subscribe ok");

    // Mirror what `spawn_event_subscriber` does in production: route the
    // delegate variant into `state.handle_delegate`.
    let state_for_task = tui_state.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(Some(msg)) = sub.next_event().await {
            match msg {
                BroadcastMsg::Event(event) => {
                    state_for_task.write().await.apply_event(event);
                }
                BroadcastMsg::Delegate(signal) => {
                    state_for_task.write().await.handle_delegate(signal);
                }
            }
        }
    });

    let signal = DelegateSignal {
        pane_id: "orch-pane".into(),
        task: "implement the auth module".into(),
        to: vec!["coder".into()],
        timestamp: Utc::now(),
    };
    send_delegate(&daemon.hook_path, &signal).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let s = tui_state.read().await;
        if let Some(received) = s.delegate_events.first() {
            assert_eq!(received.pane_id, "orch-pane");
            assert_eq!(received.task, "implement the auth module");
            assert_eq!(received.to, vec!["coder".to_string()]);
            saw = true;
            break;
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw,
        "expected DelegateSignal to reach the TUI's AppState via subscribe_events"
    );

    forwarder.abort();
}

/// A delegate from a pane the TUI doesn't recognise as an orchestrator
/// must still be dropped on the TUI side (the daemon is a dumb pipe in
/// external mode, but the role-validation guard moves to the TUI).
#[tokio::test]
async fn delegate_signal_from_non_orchestrator_is_dropped() {
    let daemon = spawn_daemon().await;

    let tui_state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut st = tui_state.write().await;
        st.register_pane("worker-pane".into());
        st.pane_role_map
            .insert("worker-pane".into(), "coder".into());
        // Deliberately NOT in `orchestrator_pane_ids`.
    }

    let client = DaemonClient::new(daemon.attach_path.clone());
    let mut sub = client.subscribe_events().await.expect("subscribe ok");

    let state_for_task = tui_state.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(Some(msg)) = sub.next_event().await {
            if let BroadcastMsg::Delegate(signal) = msg {
                state_for_task.write().await.handle_delegate(signal);
            }
        }
    });

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "worker-pane".into(),
            task: "spoof".into(),
            to: vec!["reviewer".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Give the forwarder a generous window to receive + drop. A
    // straight sleep is the cleanest assertion of "nothing happened."
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = tui_state.read().await;
    assert!(
        s.delegate_events.is_empty(),
        "non-orchestrator delegate must not enqueue"
    );

    forwarder.abort();
}
