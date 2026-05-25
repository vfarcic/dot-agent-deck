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
use dot_agent_deck::daemon_protocol::{
    KIND_EVENT, KIND_SHUTDOWN, bind_attach_listener, read_frame, serve_attach_with_counter,
    write_frame,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;
use dot_agent_deck::state::{AppState, SharedState};
use tokio::net::UnixListener;

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

// ---------------------------------------------------------------------------
// PRD #92 F1 followup — KIND_SHUTDOWN_ACK protocol hardening
//
// The original F1 wire used "socket close == ack" semantics. The
// reviewer flagged this as a blocker: a daemon predating PROTOCOL_VERSION
// 2 would close the connection on the unknown KIND_SHUTDOWN frame, and
// the client would interpret the close as a successful ack. The followup
// introduces an explicit KIND_SHUTDOWN_ACK frame the daemon writes
// BEFORE beginning teardown; the client waits up to 1s for it and treats
// timeout / EOF / unexpected-frame as errors.
//
// The next three tests use ad-hoc stub servers (raw UnixListener loops
// rather than the real `serve_attach_with_counter`) so they can simulate
// the three failure modes deterministically. The stubs sit in the
// `stub_server` module below to keep their oddities (e.g. holding the
// connection open silently for a timeout test) out of the production
// codebase.
// ---------------------------------------------------------------------------

mod stub_server {
    use super::*;

    /// Spawn a stub Unix-socket server that accepts ONE connection, runs
    /// the supplied callback over that connection, then exits. Returns
    /// the socket path; the temp dir is leaked into the JoinHandle's
    /// closure so it lives for the duration of the test.
    pub(super) async fn spawn<F, Fut>(handler: F) -> (PathBuf, JoinHandle<()>, TempDir)
    where
        F: FnOnce(tokio::net::UnixStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (dir, path, listener) = {
            // tempdir() must be created inside the lock: bind_socket (called by
            // bind_attach_listener in other concurrent tests) briefly flips the
            // process-global umask to 0o177. If tempdir() races with that flip,
            // the directory gets mode 0o600 (no execute bit) and the subsequent
            // UnixListener::bind inside it fails with EACCES.
            let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("stub.sock");
            let listener = UnixListener::bind(&path).expect("bind stub listener");
            (dir, path, listener)
        };
        let handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                handler(stream).await;
            }
        });
        (path, handle, dir)
    }
}

/// Stub server that reads the `KIND_SHUTDOWN` frame and then sits
/// silently. The client must time out after 1s and return Err.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_shutdown_times_out_without_ack() {
    let (path, _server_handle, _dir) = stub_server::spawn(|mut stream| async move {
        // Consume the inbound KIND_SHUTDOWN so the client's `write_frame`
        // completes cleanly, then deliberately do nothing — the client
        // must hit its 1-second timeout.
        let _ = read_frame(&mut stream).await;
        // Hold the connection open past the client's timeout. 2.5s is
        // comfortably more than the client's 1s budget without making
        // the test flaky.
        tokio::time::sleep(Duration::from_millis(2500)).await;
    })
    .await;

    let client = DaemonClient::new(path);
    let result = client.send_shutdown().await;
    assert!(
        result.is_err(),
        "send_shutdown must Err on ack timeout, got {result:?}"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("timed out") || msg.contains("timed-out"),
        "error must mention timeout, got: {msg}"
    );
}

/// Stub server that reads the `KIND_SHUTDOWN` frame then closes the
/// socket without sending any ack. This is the upgrade-mismatch case
/// (an old daemon that didn't recognise the frame just hangs up).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_shutdown_errors_on_eof_without_ack() {
    let (path, _server_handle, _dir) = stub_server::spawn(|mut stream| async move {
        let _ = read_frame(&mut stream).await;
        // Drop the stream → daemon-side socket close. The client's
        // `read_frame` returns `Ok(None)` and `send_shutdown` Errs.
        drop(stream);
    })
    .await;

    let client = DaemonClient::new(path);
    let result = client.send_shutdown().await;
    assert!(
        result.is_err(),
        "send_shutdown must Err on EOF without ack, got {result:?}"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("predating") || msg.contains("PROTOCOL_VERSION"),
        "error must hint at the version-mismatch cause, got: {msg}"
    );
}

/// Stub server that reads the `KIND_SHUTDOWN` frame then sends a frame
/// of the wrong kind (`KIND_EVENT`) instead of `KIND_SHUTDOWN_ACK`. The
/// client must reject the response and return Err.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_shutdown_errors_on_unexpected_frame() {
    let (path, _server_handle, _dir) = stub_server::spawn(|mut stream| async move {
        let _ = read_frame(&mut stream).await;
        // Respond with KIND_EVENT — a valid frame kind that's never
        // legal in response to KIND_SHUTDOWN.
        let _ = write_frame(&mut stream, KIND_EVENT, &[]).await;
        // Hold the connection open so the client gets a clean
        // unexpected-frame error rather than racing into an EOF.
        tokio::time::sleep(Duration::from_millis(500)).await;
    })
    .await;

    let client = DaemonClient::new(path);
    let result = client.send_shutdown().await;
    assert!(
        result.is_err(),
        "send_shutdown must Err on unexpected frame kind, got {result:?}"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("expected KIND_SHUTDOWN_ACK") && msg.contains("0x14"),
        "error must report the unexpected kind, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Auditor #1 — KIND_SHUTDOWN with non-empty payload must be rejected
// ---------------------------------------------------------------------------

/// Build a connection that sends `KIND_SHUTDOWN` with a 4-byte garbage
/// payload. The daemon must NOT drain the registry, must NOT signal the
/// shutdown notify, and must close the connection without sending an
/// ack. We talk to the real daemon harness so the test exercises the
/// production handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kind_shutdown_with_payload_is_rejected_by_daemon() {
    let server = start_server_with_shutdown().await;

    // Spawn an agent so we can later assert it survives the malformed
    // frame. A `sleep 30` agent that exits cleanly on SIGTERM is the
    // realistic shape — if it gets terminated, that's the regression.
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));
    {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'sleep 30'"), None).unwrap()
        })
        .await
        .unwrap();
    }
    assert_eq!(server.registry.len(), 1);

    // Hand-craft a KIND_SHUTDOWN frame with a non-empty payload and send
    // it directly to the daemon socket.
    let mut stream = tokio::net::UnixStream::connect(&server.path)
        .await
        .expect("connect to daemon");
    write_frame(&mut stream, KIND_SHUTDOWN, b"junk")
        .await
        .expect("send malformed KIND_SHUTDOWN");
    // Give the daemon a moment to process and close.
    let _ = tokio::time::timeout(Duration::from_millis(500), read_frame(&mut stream)).await;
    drop(stream);

    // Sanity: the agent survives (the registry still has it) — the
    // daemon refused to enter its teardown path.
    assert_eq!(
        server.registry.len(),
        1,
        "agent must survive a malformed KIND_SHUTDOWN frame"
    );

    // The shutdown Notify must NOT have been signalled. Use a short
    // timeout — if no signal arrives in 500ms, the daemon correctly
    // refused the malformed frame.
    let signalled = tokio::time::timeout(Duration::from_millis(500), server.shutdown.notified())
        .await
        .is_ok();
    assert!(
        !signalled,
        "shutdown Notify must NOT fire for a malformed KIND_SHUTDOWN frame"
    );
}

// ---------------------------------------------------------------------------
// Auditor #2 — StartAgent during shutdown must be refused
// ---------------------------------------------------------------------------

/// After the registry enters its shutdown path (latch flipped), a
/// `StartAgent` request must return a server error rather than spawning
/// a new agent the teardown is about to miss. We exercise this by
/// flipping the latch directly (via the public `shutdown_all_graceful`
/// entry point with a zero-grace shortcut would also work, but calling
/// `shutdown_all_graceful` on an empty registry is the cleanest way
/// to set the flag) and then issuing a `StartAgent` via the daemon
/// client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_agent_during_shutdown_is_refused() {
    use dot_agent_deck::daemon_client::StartAgentOptions;

    let server = start_server_with_shutdown().await;

    // Flip the shutting_down latch by triggering a real shutdown on an
    // empty registry. Returns synchronously (no agents to terminate).
    let registry_for_shutdown = server.registry.clone();
    tokio::task::spawn_blocking(move || {
        registry_for_shutdown.shutdown_all_graceful(Duration::from_millis(0));
    })
    .await
    .unwrap();
    assert!(
        server.registry.is_shutting_down(),
        "registry must be in shutting_down state after the priming call"
    );

    let client = DaemonClient::new(server.path.clone());
    let opts = StartAgentOptions {
        command: Some("sh".to_string()),
        cwd: None,
        display_name: None,
        rows: 24,
        cols: 80,
        env: Vec::new(),
        tab_membership: None,
        agent_type: None,
    };
    let result = client.start_agent(opts).await;
    assert!(
        result.is_err(),
        "start_agent must Err while daemon is shutting down, got {result:?}"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("shutting down"),
        "error must mention shutting down, got: {msg}"
    );

    // Sanity: the registry stays empty — the refused request did NOT
    // spawn a child.
    assert!(
        server.registry.is_empty(),
        "no agent should have been spawned after the refusal"
    );
}
