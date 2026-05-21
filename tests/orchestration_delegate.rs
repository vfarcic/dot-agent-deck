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
use dot_agent_deck::event::{BroadcastMsg, DaemonMessage, DelegateSignal, WorkDoneSignal};
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

async fn send_work_done(hook_path: &PathBuf, signal: &WorkDoneSignal) {
    let mut stream = UnixStream::connect(hook_path).await.expect("connect hook");
    let msg = DaemonMessage::WorkDone(signal.clone());
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
                BroadcastMsg::WorkDone(signal) => {
                    state_for_task.write().await.handle_work_done(signal);
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

/// Symmetric regression guard for the `work-done` direction. Same shape
/// of bug as the delegate variant above: `dot-agent-deck work-done` from
/// a worker pane sent a `WorkDoneSignal` to the daemon hook socket, but
/// in external-daemon mode the daemon's `pane_role_map` / `pane_cwd_map`
/// are empty (the TUI owns them), so `state.handle_work_done` rejected
/// every signal as "work-done from unknown pane" and the orchestrator
/// pane never got the feedback message. Mirrors `Delegate` by adding a
/// `BroadcastMsg::WorkDone` hop that the TUI-side subscriber re-applies
/// against the real state.
#[tokio::test]
async fn work_done_signal_round_trips_to_attached_appstate() {
    let daemon = spawn_daemon().await;

    // TUI-side state: the worker pane is registered with its role and cwd
    // so the TUI's `handle_work_done` can resolve role → summary file.
    let cwd_dir = tempfile::tempdir().unwrap();
    let tui_state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut st = tui_state.write().await;
        st.register_pane("coder-pane".into());
        st.pane_role_map.insert("coder-pane".into(), "coder".into());
        st.pane_cwd_map.insert(
            "coder-pane".into(),
            cwd_dir.path().to_string_lossy().into_owned(),
        );
    }

    let client = DaemonClient::new(daemon.attach_path.clone());
    let mut sub = client.subscribe_events().await.expect("subscribe ok");

    let state_for_task = tui_state.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(Some(msg)) = sub.next_event().await {
            if let BroadcastMsg::WorkDone(signal) = msg {
                state_for_task.write().await.handle_work_done(signal);
            }
        }
    });

    send_work_done(
        &daemon.hook_path,
        &WorkDoneSignal {
            pane_id: "coder-pane".into(),
            task: "implemented the auth module".into(),
            done: false,
            timestamp: Utc::now(),
        },
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let s = tui_state.read().await;
        if let Some(received) = s.work_done_events.first() {
            assert_eq!(received.pane_id, "coder-pane");
            assert_eq!(received.task, "implemented the auth module");
            assert!(!received.done);
            saw = true;
            break;
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw,
        "expected WorkDoneSignal to reach the TUI's AppState via subscribe_events"
    );

    // Summary file is the user-visible side effect — the feedback
    // message pointing the orchestrator at it would be dead without it.
    let summary = cwd_dir.path().join(".dot-agent-deck/work-done-coder.md");
    assert!(
        summary.exists(),
        "work-done-coder.md must be written to the worker's cwd"
    );
    let body = std::fs::read_to_string(&summary).unwrap();
    assert_eq!(body, "implemented the auth module");

    forwarder.abort();
}

/// Detach-window replay guard for `work-done`.
///
/// The original bug: with no TUI attached, a worker's `dot-agent-deck
/// work-done` reached the daemon's hook loop but `event_tx.send(...)`
/// returned Err (zero subscribers), the signal was logged and dropped,
/// and the orchestrator never saw the feedback message on reattach.
///
/// This test pins the fix: with no subscriber, the signal must be
/// recorded into the daemon's `pending_broadcasts` and replayed to the
/// next attaching subscriber before live broadcasts resume.
#[tokio::test]
async fn work_done_signal_replayed_after_reattach() {
    let daemon = spawn_daemon().await;

    // Worker pane setup mirrors the live-path test — the TUI's
    // handle_work_done resolves role → summary file using these maps.
    let cwd_dir = tempfile::tempdir().unwrap();
    let tui_state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut st = tui_state.write().await;
        st.register_pane("coder-pane".into());
        st.pane_role_map.insert("coder-pane".into(), "coder".into());
        st.pane_cwd_map.insert(
            "coder-pane".into(),
            cwd_dir.path().to_string_lossy().into_owned(),
        );
    }

    // First attach: open a subscription, immediately drop it. This
    // reproduces the "user detached the deck" state — at the moment of
    // the work-done signal below, zero subscribers exist on the daemon's
    // broadcast channel.
    let client = DaemonClient::new(daemon.attach_path.clone());
    {
        let _initial = client.subscribe_events().await.expect("initial subscribe");
        // Allow the daemon-side per-connection task to actually call
        // `event_tx.subscribe()` and then observe the EOF when we drop.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Wait for the daemon's receiver to be torn down — `send` only
    // returns Err once the receiver count actually drops to zero, which
    // happens asynchronously after our subscriber socket closes.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Worker fires while the deck is detached.
    send_work_done(
        &daemon.hook_path,
        &WorkDoneSignal {
            pane_id: "coder-pane".into(),
            task: "implemented under detached deck".into(),
            done: false,
            timestamp: Utc::now(),
        },
    )
    .await;
    // Hook loop is async — give it time to drain the line and record.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reattach. The new subscriber must receive the buffered signal as
    // its first event, before any live messages.
    let mut sub = client.subscribe_events().await.expect("reattach subscribe");
    let state_for_task = tui_state.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(Some(msg)) = sub.next_event().await {
            if let BroadcastMsg::WorkDone(signal) = msg {
                state_for_task.write().await.handle_work_done(signal);
            }
        }
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let s = tui_state.read().await;
        if let Some(received) = s.work_done_events.first() {
            assert_eq!(received.pane_id, "coder-pane");
            assert_eq!(received.task, "implemented under detached deck");
            saw = true;
            break;
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw,
        "WorkDoneSignal sent during detach window must be replayed to the next subscriber \
         (regression for the silent-loss bug)"
    );

    forwarder.abort();
}

/// Parallel guard for `delegate`: same shape of bug, same fix. An
/// orchestrator's `dot-agent-deck delegate` fired while the deck is
/// detached must be replayed to the next attaching TUI.
#[tokio::test]
async fn delegate_signal_replayed_after_reattach() {
    let daemon = spawn_daemon().await;

    let tui_state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut st = tui_state.write().await;
        st.register_pane("orch-pane".into());
        st.pane_role_map
            .insert("orch-pane".into(), "orchestrator".into());
        st.orchestrator_pane_ids.insert("orch-pane".into());
    }

    let client = DaemonClient::new(daemon.attach_path.clone());
    {
        let _initial = client.subscribe_events().await.expect("initial subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "delegated while detached".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sub = client.subscribe_events().await.expect("reattach subscribe");
    let state_for_task = tui_state.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(Some(msg)) = sub.next_event().await {
            if let BroadcastMsg::Delegate(signal) = msg {
                state_for_task.write().await.handle_delegate(signal);
            }
        }
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let s = tui_state.read().await;
        if let Some(received) = s.delegate_events.first() {
            assert_eq!(received.pane_id, "orch-pane");
            assert_eq!(received.task, "delegated while detached");
            assert_eq!(received.to, vec!["coder".to_string()]);
            saw = true;
            break;
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw,
        "DelegateSignal sent during detach window must be replayed to the next subscriber"
    );

    forwarder.abort();
}
