//! PRD #92 F1 — end-to-end tests for the **Stop** option in the Ctrl+C
//! dialog. Three concerns are exercised here:
//!
//! 1. The `KIND_SHUTDOWN` wire frame reaches the daemon, drains the
//!    agent registry (SIGTERM-with-grace then SIGKILL via the existing
//!    teardown), and is idempotent on repeat.
//! 2. The TUI-side [`DaemonClient::send_shutdown`] helper returns
//!    cleanly without panicking once the daemon has acknowledged via
//!    socket close.
//! 3. The daemon-shutdown `Notify` wired via `serve_attach_with_counter`
//!    fires exactly once per `KIND_SHUTDOWN`, so the daemon's
//!    `run_hook_loop` exits and `run_daemon_with` returns. The
//!    integration with `run_daemon_with` itself is covered by the
//!    in-process tests in `src/daemon.rs`; this file exercises the
//!    attach-server slice.
//!
//! Harness mirrors `daemon_lifecycle.rs` and `daemon_protocol.rs`: an
//! in-process attach server bound to a tempdir socket, with a
//! process-wide bind lock because `bind_attach_listener` flips the umask
//! while binding.

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach_with_counter};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;
use dot_agent_deck::state::{AppState, SharedState};

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct Server {
    _dir: TempDir,
    path: PathBuf,
    registry: Arc<AgentPtyRegistry>,
    shutdown: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_server_with_shutdown() -> Server {
    let registry = Arc::new(AgentPtyRegistry::new());
    let shutdown = Arc::new(Notify::new());

    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };

    let registry_for_task = registry.clone();
    let shutdown_for_task = shutdown.clone();
    let (event_tx, _) = tokio::sync::broadcast::channel(16);
    let state: SharedState = Arc::new(RwLock::new(AppState::default()));
    let counter = Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn(async move {
        let _ = serve_attach_with_counter(
            listener,
            registry_for_task,
            event_tx,
            counter,
            state,
            Some(shutdown_for_task),
        )
        .await;
    });

    Server {
        _dir: dir,
        path,
        registry,
        shutdown,
        handle,
    }
}

async fn wait_for<F: FnMut() -> bool>(timeout: Duration, interval: Duration, mut pred: F) -> bool {
    let start = tokio::time::Instant::now();
    while tokio::time::Instant::now() - start < timeout {
        if pred() {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    pred()
}

/// `send_shutdown` returns cleanly even when no agents are managed.
/// This exercises the no-agents path documented in the F1 design
/// (primary dialog skips the secondary confirmation when count == 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_shutdown_returns_cleanly_with_no_agents() {
    let server = start_server_with_shutdown().await;
    let client = DaemonClient::new(server.path.clone());

    client
        .send_shutdown()
        .await
        .expect("send_shutdown on idle daemon must not error");

    // The shutdown Notify must have been signalled exactly once.
    let signalled = tokio::time::timeout(Duration::from_secs(2), server.shutdown.notified())
        .await
        .is_ok();
    assert!(
        signalled,
        "KIND_SHUTDOWN must trigger the daemon's shutdown Notify even when no agents are alive"
    );
}

/// With managed agents, `send_shutdown` drains the registry and signals
/// the shutdown notify. The agent receives SIGTERM during the graceful
/// phase; if it ignores SIGTERM (the `sh -c 'trap "" TERM; sleep 30'`
/// shape would), the daemon escalates to SIGKILL after the grace window.
/// We assert the registry-drain side effect rather than the specific
/// signal because `sh -c 'sleep N'` exits cleanly on SIGTERM, which is
/// the realistic agent shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_shutdown_drains_registry_and_signals_notify() {
    let server = start_server_with_shutdown().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Spawn two agents so the test exercises the multi-agent iteration
    // path in `shutdown_all_graceful`. Both are `sh -c 'sleep 30'` which
    // exits cleanly on SIGTERM — the grace window is therefore exercised
    // without hitting the SIGKILL fallback.
    for _ in 0..2 {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'sleep 30'"), None).unwrap()
        })
        .await
        .unwrap();
    }
    assert_eq!(server.registry.len(), 2, "agents must register before Stop");

    let client = DaemonClient::new(server.path.clone());
    client
        .send_shutdown()
        .await
        .expect("send_shutdown must not error");

    // The shutdown Notify fires synchronously inside the handler.
    let signalled = tokio::time::timeout(Duration::from_secs(2), server.shutdown.notified())
        .await
        .is_ok();
    assert!(signalled, "shutdown Notify must fire on KIND_SHUTDOWN");

    // Registry must be drained (graceful teardown removed all entries).
    let drained = wait_for(Duration::from_secs(5), Duration::from_millis(20), || {
        server.registry.is_empty()
    })
    .await;
    assert!(
        drained,
        "registry must drain after KIND_SHUTDOWN — found {} agents",
        server.registry.len()
    );
}

/// Idempotency: a second `KIND_SHUTDOWN` is a no-op on the registry
/// side. The first call sets the `shutting_down` latch on the registry;
/// the second observes it and returns immediately without re-iterating
/// (and without panicking on already-drained children). The daemon's
/// shutdown Notify is permitted to fire twice — `Notify::notify_one`
/// after the first wake is harmless — but the registry state must not
/// be corrupted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_shutdown_is_idempotent() {
    let server = start_server_with_shutdown().await;

    let client = DaemonClient::new(server.path.clone());
    client
        .send_shutdown()
        .await
        .expect("first send_shutdown must succeed");
    client
        .send_shutdown()
        .await
        .expect("second send_shutdown must succeed (no daemon-side panic)");

    // Sanity: the registry is still drained and empty after both calls.
    assert!(
        server.registry.is_empty(),
        "registry must remain empty after double shutdown"
    );
}
