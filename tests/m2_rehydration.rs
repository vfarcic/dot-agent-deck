//! PRD #76 M2.x — TUI session-list rehydration on bootstrap.
//!
//! The bug: in external-daemon mode the TUI never queried the daemon for
//! existing agents on startup, so an ssh-reconnect via `dot-agent-deck
//! connect` showed "No active sessions" even though the daemon had live
//! agents from the previous TUI session. `DaemonClient::list_agents` had
//! zero production callers.
//!
//! These tests pin the new bootstrap step in
//! [`EmbeddedPaneController::hydrate_from_daemon`]:
//!   - happy path: every `list_agents` id ends up as a stream-backed pane
//!     and STREAM_OUT bytes from each agent reach the corresponding vt100
//!     parser (the daemon-replayed scrollback snapshot, then live bytes);
//!   - empty list: hydrate returns no panes and does not error;
//!   - `list_agents` failure: hydrate logs at debug and returns no panes
//!     (the user can retry by reconnecting);
//!   - race: an agent terminates between `list_agents` and `attach` — the
//!     missing one is skipped, the rest still attach.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, bind_attach_listener, read_frame,
    serve_attach, write_frame,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;

// `bind_attach_listener` flips the process-global umask while binding; share
// the lock with the other M-series tests so concurrent tempdir creation
// can't inherit a 0o600 dir during that window.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct Server {
    _dir: TempDir,
    path: PathBuf,
    registry: Arc<AgentPtyRegistry>,
    handle: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_real_server() -> Server {
    let registry = Arc::new(AgentPtyRegistry::new());
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };
    let registry_for_task = registry.clone();
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task).await;
    });
    Server {
        _dir: dir,
        path,
        registry,
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

fn screen_contains(ctrl: &EmbeddedPaneController, pane_id: &str, needle: &str) -> bool {
    let Some(screen) = ctrl.get_screen(pane_id) else {
        return false;
    };
    let parser = screen.lock().unwrap();
    parser.screen().contents().contains(needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_creates_panes_for_existing_agents() {
    // Spawn three agents via StartAgent — each writes a unique marker — then
    // build a fresh controller and hydrate. Every agent id must surface as a
    // pane, and the daemon-replayed scrollback snapshot must put each marker
    // into the corresponding vt100 parser. This is the regression check for
    // the bug: before the fix, the controller would have been empty here.
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let mut started_ids = Vec::new();
    for i in 0..3 {
        let id = client
            .start_agent(StartAgentOptions {
                command: Some(format!("sh -c 'echo HYDRATE_MARKER_{i}; sleep 30'")),
                ..Default::default()
            })
            .await
            .expect("start_agent should succeed");
        started_ids.push(id);
    }

    // Give the daemon a moment to drain each agent's first stdout chunk into
    // its scrollback ring so the snapshot replayed on attach contains the
    // marker. Without this the `attach` could land before the agent has
    // emitted its echo, and the parser assertion below would race.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // hydrate_from_daemon block_on's the daemon client; run on a blocking
    // thread so the runtime keeps polling the in-process server.
    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };

    assert_eq!(
        hydrated.len(),
        started_ids.len(),
        "every started agent should be hydrated as a pane"
    );

    let hydrated_ids: Vec<String> = hydrated.iter().map(|h| h.agent_id.clone()).collect();
    for id in &started_ids {
        assert!(
            hydrated_ids.contains(id),
            "agent id {id} missing from hydrated set {hydrated_ids:?}"
        );
    }

    // Each pane should receive its corresponding marker via the daemon's
    // scrollback snapshot. Tie the assertion to the agent id so a mis-paired
    // wiring (e.g. all panes attached to the same agent) would be caught.
    for h in &hydrated {
        let idx = started_ids
            .iter()
            .position(|id| id == &h.agent_id)
            .expect("hydrated agent_id must be one we started");
        let needle = format!("HYDRATE_MARKER_{idx}");
        let ctrl_for_wait = ctrl.clone();
        let pane_for_wait = h.pane_id.clone();
        let needle_for_wait = needle.clone();
        let saw = wait_for(
            Duration::from_secs(5),
            Duration::from_millis(50),
            move || screen_contains(&ctrl_for_wait, &pane_for_wait, &needle_for_wait),
        )
        .await;
        assert!(
            saw,
            "expected marker '{needle}' to reach pane {} via STREAM_OUT scrollback replay",
            h.pane_id
        );
    }

    // Cleanup so the test doesn't leak the `sleep 30` children.
    drop(ctrl);
    for id in &started_ids {
        let _ = server.registry.close_agent(id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_returns_empty_when_no_agents_exist() {
    // Empty `list_agents` result: dashboard should fall through to its
    // normal "No active sessions..." view. The hydrate call must not error
    // and must not create any panes.
    let server = start_real_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };

    assert!(
        hydrated.is_empty(),
        "no agents → no hydrated panes; got {hydrated:?}"
    );
    assert!(
        ctrl.pane_ids().is_empty(),
        "no panes should have been registered"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_treats_list_agents_failure_as_empty() {
    // No daemon running at the configured path: list_agents will fail with
    // ECONNREFUSED / ENOENT. The TUI must not error out — log and treat as
    // empty so the user can reconnect.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.sock");

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        missing,
        tokio::runtime::Handle::current(),
    ));

    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };

    assert!(
        hydrated.is_empty(),
        "list_agents failure must surface as empty hydration; got {hydrated:?}"
    );
    assert!(
        ctrl.pane_ids().is_empty(),
        "no panes should have been registered on list_agents failure"
    );
}

// ---------------------------------------------------------------------------
// Mock daemon for the "agent disappears between list and attach" test.
// ---------------------------------------------------------------------------

/// Minimal mock that returns two agent ids on `ListAgents` but rejects the
/// `AttachStream` for one of them. Mirrors the real daemon's response shape
/// so the controller's `attach` call surfaces a typed `Server` error rather
/// than a malformed-frame error.
async fn run_partial_attach_server(listener: UnixListener) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::spawn(async move {
            let req = match read_frame(&mut stream).await {
                Ok(Some((KIND_REQ, payload))) => {
                    match serde_json::from_slice::<AttachRequest>(&payload) {
                        Ok(r) => r,
                        Err(_) => return,
                    }
                }
                _ => return,
            };
            match req {
                AttachRequest::ListAgents => {
                    let resp = AttachResponse {
                        ok: true,
                        agents: Some(vec!["agent-alive".to_string(), "agent-gone".to_string()]),
                        ..Default::default()
                    };
                    let _ = write_resp(&mut stream, &resp).await;
                }
                AttachRequest::AttachStream { id } => {
                    if id == "agent-gone" {
                        // Simulate the race: the agent terminated between
                        // ListAgents and AttachStream. The real daemon
                        // returns a typed error here.
                        let resp = AttachResponse::err("agent not found");
                        let _ = write_resp(&mut stream, &resp).await;
                        return;
                    }
                    // For the surviving agent, ack the attach and keep the
                    // connection open so the controller's reader can park
                    // on `read_frame` indefinitely (the test does not need
                    // STREAM_OUT bytes — only that the pane is wired up).
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                    loop {
                        match read_frame(&mut stream).await {
                            Ok(None) | Err(_) => break,
                            Ok(Some(_)) => continue,
                        }
                    }
                }
                _ => {
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                }
            }
        });
    }
}

async fn write_resp(s: &mut UnixStream, resp: &AttachResponse) -> std::io::Result<()> {
    let payload = serde_json::to_vec(resp).expect("AttachResponse must serialize");
    write_frame(s, KIND_RESP, &payload).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_preserves_pane_id_from_agent_env() {
    // Regression for the hook-routing bug: before this fix the hydrator
    // allocated a *fresh* local pane id, so hook events emitted by the
    // rehydrated agent (which still carry the original DOT_AGENT_DECK_PANE_ID
    // in its env) were silently dropped by `AppState::apply_event`. The fix
    // captures the spawn-time env on the daemon side and threads it through
    // `AttachResponse::agent_records`; rehydration must reuse that exact id.
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let pane_env = "pane-from-env-7";
    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            env: vec![("DOT_AGENT_DECK_PANE_ID".to_string(), pane_env.to_string())],
            ..Default::default()
        })
        .await
        .expect("start_agent should succeed");

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };

    assert_eq!(hydrated.len(), 1, "single agent should hydrate as one pane");
    assert_eq!(
        hydrated[0].pane_id, pane_env,
        "hydrated pane must reuse the spawn-time DOT_AGENT_DECK_PANE_ID, not allocate_id()"
    );
    assert_eq!(hydrated[0].agent_id, agent_id);

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}

// ---------------------------------------------------------------------------
// Mock daemon for the "older daemon (no agent_records)" test.
// ---------------------------------------------------------------------------

/// Mock that mimics an older daemon: replies to ListAgents with the legacy
/// `agents` field only (no `agent_records`). Verifies that a newer client
/// stays forward-compatible — pane hydrates with an allocated id, no panic.
async fn run_legacy_list_server(listener: UnixListener) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::spawn(async move {
            let req = match read_frame(&mut stream).await {
                Ok(Some((KIND_REQ, payload))) => {
                    match serde_json::from_slice::<AttachRequest>(&payload) {
                        Ok(r) => r,
                        Err(_) => return,
                    }
                }
                _ => return,
            };
            match req {
                AttachRequest::ListAgents => {
                    // Older daemons only knew about `agents`. The newer
                    // `agent_records` field must be left at None to model
                    // the legacy wire shape exactly.
                    let resp = AttachResponse {
                        ok: true,
                        agents: Some(vec!["legacy-agent".to_string()]),
                        agent_records: None,
                        ..Default::default()
                    };
                    let _ = write_resp(&mut stream, &resp).await;
                }
                AttachRequest::AttachStream { .. } => {
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                    loop {
                        match read_frame(&mut stream).await {
                            Ok(None) | Err(_) => break,
                            Ok(Some(_)) => continue,
                        }
                    }
                }
                _ => {
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_falls_back_to_allocated_id_for_legacy_daemon() {
    // Backward-compat: a daemon predating this fix returns `agents: Some(..)`
    // with `agent_records: None`. The hydrator must still produce a pane
    // (with a freshly-allocated id, since the daemon doesn't know the
    // original env) without panicking — losing hook routing on reconnect
    // matches the pre-fix behavior, but startup must not regress.
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = UnixListener::bind(&path).expect("bind mock attach socket");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, path, listener)
    };
    let server_handle = tokio::spawn(async move {
        run_legacy_list_server(listener).await;
    });

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        path,
        tokio::runtime::Handle::current(),
    ));

    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };

    assert_eq!(
        hydrated.len(),
        1,
        "legacy daemon listing must still hydrate one pane; got {hydrated:?}"
    );
    assert_eq!(hydrated[0].agent_id, "legacy-agent");
    // pane_id must be a parseable u64 (allocate_id output), not the agent id.
    assert!(
        hydrated[0].pane_id.parse::<u64>().is_ok(),
        "legacy fallback should use allocate_id() — got {:?}",
        hydrated[0].pane_id
    );

    drop(ctrl);
    server_handle.abort();
    drop(dir);
}

// ---------------------------------------------------------------------------
// Mock daemon for the "list_agents hangs past timeout" test.
// ---------------------------------------------------------------------------

/// Mock that accepts the ListAgents REQ and never replies. Used to verify
/// the hydration list-call timeout path: the controller must give up and
/// return an empty hydration rather than blocking TUI startup forever.
async fn run_silent_list_server(listener: UnixListener) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::spawn(async move {
            // Read the REQ but never write a RESP. Hold the stream so the
            // client doesn't see an EOF either — purely a hang.
            let _ = read_frame(&mut stream).await;
            std::future::pending::<()>().await;
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_treats_list_agents_timeout_as_empty() {
    // A daemon that accepts the connection but never answers must not pin
    // TUI startup. The HYDRATE_LIST_TIMEOUT bound in `hydrate_from_daemon`
    // gives up and the controller proceeds with an empty pane set so the
    // user can see the dashboard and reconnect.
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = UnixListener::bind(&path).expect("bind mock attach socket");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, path, listener)
    };
    let server_handle = tokio::spawn(async move {
        run_silent_list_server(listener).await;
    });

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        path,
        tokio::runtime::Handle::current(),
    ));

    // Outer guard: even if the timeout regressed, this catches the regression
    // before the test runner's global timeout. HYDRATE_LIST_TIMEOUT is 5s; we
    // give 10s headroom for slow CI.
    let hydrated_result = tokio::time::timeout(Duration::from_secs(10), {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
    })
    .await
    .expect("hydrate_from_daemon must not hang past HYDRATE_LIST_TIMEOUT")
    .expect("blocking task should not panic");

    assert!(
        hydrated_result.is_empty(),
        "list_agents timeout must surface as empty hydration; got {hydrated_result:?}"
    );

    drop(ctrl);
    server_handle.abort();
    drop(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_skips_agent_that_disappears_between_list_and_attach() {
    // Race coverage: ListAgents reports two ids; AttachStream succeeds for
    // one and fails for the other. The hydrator must skip the failing one
    // and continue with the rest — a single missing agent must not sink the
    // whole rehydration.
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = UnixListener::bind(&path).expect("bind mock attach socket");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, path, listener)
    };
    let server_handle = tokio::spawn(async move {
        run_partial_attach_server(listener).await;
    });

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        path,
        tokio::runtime::Handle::current(),
    ));

    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };

    assert_eq!(
        hydrated.len(),
        1,
        "the surviving agent should hydrate; the disappeared one should be skipped — got {hydrated:?}"
    );
    assert_eq!(
        hydrated[0].agent_id, "agent-alive",
        "the kept pane should be the agent that successfully attached"
    );

    drop(ctrl);
    server_handle.abort();
    drop(dir);
}
