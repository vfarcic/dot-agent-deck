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

use dot_agent_deck::agent_pty::{AgentPtyRegistry, TabMembership};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, bind_attach_listener, read_frame,
    serve_attach, write_frame,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::event::AgentType;
use dot_agent_deck::state::AppState;
use dot_agent_deck::ui::{
    dead_slot_pane_id, fill_dead_slots_with_placeholders, is_dead_slot_pane_id,
};

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
    let (event_tx, _) = tokio::sync::broadcast::channel(16);
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task, event_tx).await;
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

    let ctrl = Arc::new(EmbeddedPaneController::new(
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
    let ctrl = Arc::new(EmbeddedPaneController::new(
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

    let ctrl = Arc::new(EmbeddedPaneController::new(
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

    let ctrl = Arc::new(EmbeddedPaneController::new(
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

    let ctrl = Arc::new(EmbeddedPaneController::new(
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

    let ctrl = Arc::new(EmbeddedPaneController::new(
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

    let ctrl = Arc::new(EmbeddedPaneController::new(
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

// ---------------------------------------------------------------------------
// PRD #76 M2.x rehydration FIXUP-2: pane_id_env capture/hydrate validation.
// ---------------------------------------------------------------------------
// These three tests pin the defense-in-depth scrub for caller-supplied
// DOT_AGENT_DECK_PANE_ID values. Without it a buggy/hostile same-user peer
// reaching the attach socket can poison the daemon's stored copy (echoed
// via `agent_records`) or, in the duplicate case, get one rehydrated pane
// to silently overwrite another in `wire_stream_pane`'s HashMap.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_drops_oversize_pane_id_env_at_capture() {
    // 200-char pane id: comfortably above PANE_ID_ENV_MAX_LEN (64). The
    // daemon must store None for this agent's record, and the TUI must
    // hydrate with a freshly-allocated numeric id rather than the poison
    // value — otherwise a near-MAX_FRAME_LEN value could push the
    // cumulative `list_agents` response past the frame cap and break
    // hydration for *every* agent on reconnect.
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let oversize: String = "a".repeat(200);
    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            env: vec![("DOT_AGENT_DECK_PANE_ID".to_string(), oversize.clone())],
            ..Default::default()
        })
        .await
        .expect("start_agent should succeed");

    // Daemon-side: list_agents must report pane_id_env = None for this id.
    let records = client
        .list_agents()
        .await
        .expect("list_agents should succeed");
    let record = records
        .iter()
        .find(|r| r.id == agent_id)
        .expect("just-spawned agent should be in list");
    assert!(
        record.pane_id_env.is_none(),
        "daemon must scrub oversize pane_id_env; got {:?}",
        record.pane_id_env
    );

    // Client-side: hydrate must produce a numeric (allocate_id) pane id.
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));
    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };
    assert_eq!(hydrated.len(), 1);
    assert_eq!(hydrated[0].agent_id, agent_id);
    assert!(
        hydrated[0].pane_id.parse::<u64>().is_ok(),
        "oversize pane_id_env must fall back to allocate_id() — got {:?}",
        hydrated[0].pane_id
    );
    assert_ne!(
        hydrated[0].pane_id, oversize,
        "the poison value must not surface as a pane id"
    );

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_drops_control_char_pane_id_env_at_capture() {
    // ANSI escape embedded in the pane id: anything outside [a-zA-Z0-9_-]
    // is rejected by `is_valid_pane_id_env`. The daemon stores None and
    // the client hydrates with a fresh numeric id — keeps debug-log output
    // free of injected color codes if anything ever prints a stored value.
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let poison = "pane\x1b[31mctl";
    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            env: vec![("DOT_AGENT_DECK_PANE_ID".to_string(), poison.to_string())],
            ..Default::default()
        })
        .await
        .expect("start_agent should succeed");

    let records = client
        .list_agents()
        .await
        .expect("list_agents should succeed");
    let record = records
        .iter()
        .find(|r| r.id == agent_id)
        .expect("just-spawned agent should be in list");
    assert!(
        record.pane_id_env.is_none(),
        "daemon must scrub control-char pane_id_env; got {:?}",
        record.pane_id_env
    );

    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));
    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };
    assert_eq!(hydrated.len(), 1);
    assert!(
        hydrated[0].pane_id.parse::<u64>().is_ok(),
        "control-char pane_id_env must fall back to allocate_id() — got {:?}",
        hydrated[0].pane_id
    );
    assert_ne!(
        hydrated[0].pane_id, poison,
        "the poison value must not surface as a pane id"
    );

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_pane_id_env_is_rejected_at_spawn_time() {
    // CodeRabbit MAJOR (PRD #93 round-9): two agents must never share a
    // `pane_id_env`. `write_to_pane_and_submit` keys off that string when routing
    // delegate/work-done writes, so a second spawn with the same id
    // would silently misroute every write to whichever `values().find`
    // entry the HashMap iterator happened to visit first.
    //
    // The pre-round-9 contract was looser: the daemon stored both, and
    // the client's `hydrate_from_daemon` deduped by keeping the first
    // reuse and falling the second back to `allocate_id()`. That
    // protected only the hydration HashMap-collision case in
    // `wire_stream_pane`; the delegate/work-done routing remained
    // broken because the daemon had no consistent winner. The new
    // contract: reject at spawn time, where the bad request originates.
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let shared_pane_env = "shared-pane-id";
    let agent_a = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            env: vec![(
                "DOT_AGENT_DECK_PANE_ID".to_string(),
                shared_pane_env.to_string(),
            )],
            ..Default::default()
        })
        .await
        .expect("start_agent A should succeed");

    let err = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            env: vec![(
                "DOT_AGENT_DECK_PANE_ID".to_string(),
                shared_pane_env.to_string(),
            )],
            ..Default::default()
        })
        .await
        .expect_err("duplicate pane_id_env spawn must fail");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("duplicate"),
        "expected a duplicate-pane-id error, got: {msg}"
    );

    // First agent must survive the rejected duplicate spawn.
    let records = server.registry.agent_records();
    assert_eq!(
        records.iter().filter(|r| r.id == agent_a).count(),
        1,
        "first agent must survive the rejected duplicate spawn"
    );

    let _ = server.registry.close_agent(&agent_a);
}

// ---------------------------------------------------------------------------
// PRD #76 M2.13 fixup F2 — full hydration-path agent_type plumbing.
// ---------------------------------------------------------------------------
// Wire-format tests pin `StartAgent.agent_type` and `AgentRecord.agent_type`
// round-trips in isolation; the placeholder-seeding unit tests pin the
// `AppState` side. This test exercises the *full* path end-to-end so a future
// refactor that breaks any single link (StartAgent → daemon registry →
// AgentRecord → hydrate_from_daemon → HydratedPane → insert_placeholder_session)
// is caught before the dashboard goes back to rendering "No agent" on reconnect.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_preserves_agent_type_end_to_end() {
    // Spawn with an explicit `StartAgentOptions.agent_type = Some(ClaudeCode)`,
    // hydrate, then thread the hydrated value through `insert_placeholder_session`
    // exactly the way `ui.rs` does. The placeholder must end up with
    // `agent_type == ClaudeCode`, not `AgentType::None`.
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            agent_type: Some(AgentType::ClaudeCode),
            ..Default::default()
        })
        .await
        .expect("start_agent should succeed");

    let ctrl = Arc::new(EmbeddedPaneController::new(
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
    let h = &hydrated[0];
    assert_eq!(h.agent_id, agent_id);
    assert_eq!(
        h.agent_type,
        Some(AgentType::ClaudeCode),
        "hydrated pane must carry the StartAgent-supplied agent_type, \
         not None — got {:?}",
        h.agent_type
    );

    // Mirror `ui.rs` hydration: register the pane and seed the placeholder
    // with `h.agent_type`. Without M2.13 wiring this collapses to None.
    let mut state = AppState::default();
    state.register_pane(h.pane_id.clone());
    state.insert_placeholder_session(
        h.pane_id.clone(),
        h.cwd.clone(),
        h.agent_type.clone(),
        Some(h.agent_id.clone()),
    );

    let session = state
        .sessions
        .values()
        .find(|s| s.pane_id.as_deref() == Some(h.pane_id.as_str()))
        .expect("placeholder session must exist for the hydrated pane");
    assert_eq!(
        session.agent_type,
        AgentType::ClaudeCode,
        "placeholder session must inherit the daemon-recorded agent_type — \
         got {:?}",
        session.agent_type
    );

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}

// PRD #76 M2.13: the wire field is an enum; a serde rename or variant
// addition that breaks `OpenCode` round-trip would slip past the
// ClaudeCode-only end-to-end test above. Re-run the same hydration chain
// with `OpenCode` so any single-variant regression in `AgentRecord.agent_type`
// (or downstream plumbing) fails loudly on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_preserves_agent_type_end_to_end_opencode() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            agent_type: Some(AgentType::OpenCode),
            ..Default::default()
        })
        .await
        .expect("start_agent should succeed");

    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));
    let hydrated = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.hydrate_from_daemon())
            .await
            .unwrap()
    };

    assert_eq!(hydrated.len(), 1);
    let h = &hydrated[0];
    assert_eq!(h.agent_id, agent_id);
    assert_eq!(
        h.agent_type,
        Some(AgentType::OpenCode),
        "hydrated pane must carry the OpenCode agent_type, not None — got {:?}",
        h.agent_type
    );

    let mut state = AppState::default();
    state.register_pane(h.pane_id.clone());
    state.insert_placeholder_session(
        h.pane_id.clone(),
        h.cwd.clone(),
        h.agent_type.clone(),
        Some(h.agent_id.clone()),
    );

    let session = state
        .sessions
        .values()
        .find(|s| s.pane_id.as_deref() == Some(h.pane_id.as_str()))
        .expect("placeholder session must exist for the hydrated pane");
    assert_eq!(
        session.agent_type,
        AgentType::OpenCode,
        "placeholder session must inherit the daemon-recorded OpenCode \
         agent_type — got {:?}",
        session.agent_type
    );

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}

/// Symptom 2 regression
/// (`.dot-agent-deck/agent-card-lifecycle-bugs.md`): a role whose daemon
/// agent has died (e.g., a `clear = false` `release` agent that runs
/// through its workflow and exits cleanly) is absent from
/// `agent_records()` on reconnect. The TUI's hydration partition
/// therefore receives sparse role slots — the bucket has one fewer
/// entry than the orchestration config declares. Pre-fix the missing
/// role's slot disappeared from the rebuilt orchestration tab
/// entirely.
///
/// This test pins the fix end-to-end at the integration boundary the
/// real reconnect path uses:
///   1. Spawn 5 orchestration role agents through `DaemonClient`,
///      mirroring the production `.dot-agent-deck.toml` layout
///      (orchestrator + coder + reviewer + auditor + release).
///   2. Kill the LAST agent (`release`-equivalent) the same way a
///      `clear = false` agent exiting cleanly would die — its
///      registry entry is pruned, `list_agents` no longer reports
///      it.
///   3. Hydrate. Verify `hydrate_from_daemon` returns 4 panes (the
///      live ones) — that's the underlying state the fix has to cope
///      with.
///   4. Build a `Vec<Option<String>>` of length 5 the way the
///      hydration loop in `ui.rs` does (one `Some(pane_id)` per
///      hydrated role slot, the dead role's slot is `None`).
///   5. Call `fill_dead_slots_with_placeholders` and assert every
///      slot now carries a non-empty id, the dead one is the
///      deterministic synthetic id, and a placeholder session has
///      been seeded so the orchestration tab's card filter
///      (`pane_id ∈ role_pane_ids`) finds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_role_stays_visible_on_reconnect_as_placeholder_card() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let orchestration_name = "tdd-cycle";
    let cwd = server._dir.path().to_string_lossy().into_owned();
    let role_names = ["orchestrator", "coder", "reviewer", "auditor", "release"];
    let mut spawned_ids: Vec<String> = Vec::new();
    for (role_index, role_name) in role_names.iter().enumerate() {
        let pane_env = format!("pane-{role_name}");
        let id = client
            .start_agent(StartAgentOptions {
                command: Some("sh -c 'sleep 30'".to_string()),
                cwd: Some(cwd.clone()),
                display_name: Some((*role_name).to_string()),
                env: vec![("DOT_AGENT_DECK_PANE_ID".to_string(), pane_env)],
                tab_membership: Some(TabMembership::Orchestration {
                    name: orchestration_name.to_string(),
                    role_index,
                    role_name: (*role_name).to_string(),
                    is_start_role: role_index == 0,
                    orchestration_cwd: Some(cwd.clone()),
                }),
                ..Default::default()
            })
            .await
            .expect("start_agent should succeed");
        spawned_ids.push(id);
    }

    // Kill the LAST role (`release`-equivalent). Matches the
    // production failure mode: an agent that exits cleanly (or that
    // the user explicitly closed) is pruned from `agent_records()`
    // and disappears from `list_agents`.
    let release_id = spawned_ids.last().unwrap().clone();
    server
        .registry
        .close_agent(&release_id)
        .expect("close_agent for release should succeed");

    // Hydrate. Confirms the precondition the fix has to cope with:
    // only 4 panes are returned even though the orchestration
    // declares 5.
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
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
        4,
        "release agent was closed, so only 4 of 5 should hydrate; \
         got {hydrated:?}"
    );

    // Build `role_pane_ids` the way the hydration loop does. We map
    // each surviving role into its slot by role_index.
    let mut role_pane_ids: Vec<Option<String>> = vec![None; role_names.len()];
    for h in &hydrated {
        if let Some(TabMembership::Orchestration { role_index, .. }) = &h.tab_membership {
            role_pane_ids[*role_index] = Some(h.pane_id.clone());
        }
    }
    assert!(
        role_pane_ids[4].is_none(),
        "role_index 4 must be the dead slot"
    );

    // Apply the fix: every dead slot gets a synthetic id and a
    // placeholder session is seeded so the orchestration tab keeps
    // the role's card visible.
    let mut state = AppState::default();
    // Seed placeholders for the live hydrated panes too — mirrors the
    // hydration loop's normal path so the post-fix AppState looks like
    // the real run_tui's would.
    for h in &hydrated {
        state.register_pane(h.pane_id.clone());
        state.insert_placeholder_session(
            h.pane_id.clone(),
            h.cwd.clone(),
            h.agent_type.clone(),
            Some(h.agent_id.clone()),
        );
    }
    fill_dead_slots_with_placeholders(&mut role_pane_ids, &cwd, orchestration_name, &mut state);

    // Every role slot is now filled.
    assert!(
        role_pane_ids.iter().all(Option::is_some),
        "all 5 role slots must have a pane id after the dead-slot fill; \
         got {role_pane_ids:?}"
    );
    let dead_id = role_pane_ids[4].as_deref().unwrap();
    assert_eq!(
        dead_id,
        dead_slot_pane_id(&cwd, orchestration_name, 4),
        "dead slot id must be the deterministic synthetic"
    );
    assert!(is_dead_slot_pane_id(dead_id));

    // The placeholder session backing the dead slot exists, has the
    // 'No agent' shape, and would be picked up by the orchestration
    // tab's card-filter (`pane_id ∈ role_pane_ids`).
    let dead_session = state
        .sessions
        .values()
        .find(|s| s.pane_id.as_deref() == Some(dead_id))
        .expect("dead-slot placeholder session must exist in AppState");
    assert_eq!(dead_session.agent_type, AgentType::None);

    // And we end up with one session per role — five total, not
    // four. Pre-fix this would have been four.
    let cards_per_pane: Vec<&str> = role_pane_ids
        .iter()
        .filter_map(|p| p.as_deref())
        .filter(|pid| {
            state
                .sessions
                .values()
                .any(|s| s.pane_id.as_deref() == Some(*pid))
        })
        .collect();
    assert_eq!(
        cards_per_pane.len(),
        5,
        "exactly one card must exist per role slot; got {cards_per_pane:?}"
    );

    drop(ctrl);
    // Clean up surviving agents so the test doesn't leak the `sleep 30`
    // children.
    for id in &spawned_ids[..spawned_ids.len() - 1] {
        let _ = server.registry.close_agent(id);
    }
}

// ---------------------------------------------------------------------------
// PRD #104 R1 (reviewer): the M4 reproducer in `tests/snapshot_replay_dims.rs`
// pins `parser_init_dims` in isolation, but a regression that swapped the
// helper out for hard-coded `24, 80` at the `hydrate_from_daemon` call-site
// would still pass that test (it only proves the helper itself is correct).
//
// This test exercises the actual call-site: spawn a real agent at 40×120 on
// the in-process daemon, hydrate via `EmbeddedPaneController::hydrate_from_daemon`,
// and assert the pane's vt100 parser is sized to the daemon-reported dims.
// A regression that re-introduces the hard-coded fall-back would fail here
// even if `parser_init_dims` itself stayed correct.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_sizes_parser_to_daemon_reported_pty_dims() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    // Daemon-side spawn at non-default 40×120. Pre-PRD the client's parser
    // would have been built at 24×80 regardless; the fix wires `record.rows`
    // / `record.cols` through `parser_init_dims` into `vt100::Parser::new`.
    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            rows: 40,
            cols: 120,
            ..Default::default()
        })
        .await
        .expect("start_agent should succeed");

    let ctrl = Arc::new(EmbeddedPaneController::new(
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
    let pane_id = hydrated[0].pane_id.clone();

    let screen = ctrl
        .get_screen(&pane_id)
        .expect("hydrated pane must expose its vt100 parser");
    let size = {
        let parser = screen.lock().unwrap();
        parser.screen().size()
    };
    assert_eq!(
        size,
        (40, 120),
        "PRD #104 R1: hydrate_from_daemon must size the parser to AgentRecord.rows/cols, \
         not the pre-PRD hard-coded 24×80 placeholder"
    );

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}
