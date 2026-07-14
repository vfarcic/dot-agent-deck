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

use chrono::Utc;
use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, AgentRecord, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership,
};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, bind_attach_listener, read_frame,
    serve_attach, serve_attach_with_counter, write_frame,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::state::{
    ActiveTool, AppState, SessionSnapshot, SessionState, SessionStatus, SharedState,
};
use dot_agent_deck::ui::{
    dead_slot_pane_id, fill_dead_slots_with_placeholders, is_dead_slot_pane_id,
};
use spec::spec;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicUsize;

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

/// PRD #162: variant of [`start_real_server`] that serves with the daemon's
/// real, caller-supplied `state: SharedState` via `serve_attach_with_counter`
/// instead of `serve_attach`'s empty dummy state. This is the production path
/// the `ListAgents` handler reads to attach the live `SessionSnapshot`. The
/// caller owns the `registry` (so it can pre-spawn agents into it) and the
/// `state` (so it can populate `AppState.sessions` via `apply_event`). Returns
/// the tempdir (keep it alive — drop removes the socket) and the join handle
/// (abort it at teardown).
async fn start_server_with_state(
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
) -> (TempDir, PathBuf, JoinHandle<()>) {
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };
    let (event_tx, _rx) = tokio::sync::broadcast::channel(16);
    let client_count = Arc::new(AtomicUsize::new(0));
    let scheduler = Arc::new(dot_agent_deck::scheduler::Scheduler::with_stderr_notifier());
    let reuse = dot_agent_deck::spawn::new_reuse_registry();
    // PRD #120 added a 9th `worktree_registry` arg to `serve_attach_with_counter`;
    // this rehydration harness doesn't exercise issue dispatch, so it passes the
    // same empty stand-in the non-orchestration callers use.
    let worktrees = dot_agent_deck::issue_dispatch_run::new_worktree_registry();
    let registry_for_task = registry.clone();
    let handle = tokio::spawn(async move {
        let _ = serve_attach_with_counter(
            listener,
            registry_for_task,
            event_tx,
            client_count,
            state,
            None,
            scheduler,
            reuse,
            worktrees,
        )
        .await;
    });
    (dir, path, handle)
}

/// PRD #162: drive an `AppState` session via the same `apply_event` path the
/// daemon uses for hook events, until the session is `Working` with an active
/// tool, `tool_count > 0`, an event-derived `agent_type` of ClaudeCode, and a
/// recorded first/last prompt. Mirrors the status/transition apply_event flow:
/// SessionStart → Thinking(prompt) → ToolStart(Read) → ToolEnd → ToolStart(Edit).
fn drive_session_to_working(state: &mut AppState, session_id: &str, pane_id: &str, agent_id: &str) {
    let mk = |event_type: EventType,
              tool_name: Option<&str>,
              tool_detail: Option<&str>,
              user_prompt: Option<&str>| AgentEvent {
        session_id: session_id.to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type,
        tool_name: tool_name.map(str::to_string),
        tool_detail: tool_detail.map(str::to_string),
        cwd: None,
        timestamp: Utc::now(),
        user_prompt: user_prompt.map(str::to_string),
        metadata: HashMap::new(),
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
    };
    state.apply_event(mk(EventType::SessionStart, None, None, None));
    state.apply_event(mk(
        EventType::Thinking,
        None,
        None,
        Some("build the feature"),
    ));
    state.apply_event(mk(
        EventType::ToolStart,
        Some("Read"),
        Some("src/main.rs"),
        None,
    ));
    state.apply_event(mk(EventType::ToolEnd, None, None, None));
    state.apply_event(mk(
        EventType::ToolStart,
        Some("Edit"),
        Some("src/lib.rs"),
        None,
    ));
}

/// PRD #162: serve the *empty dummy-state* `serve_attach` path on a
/// caller-owned registry (so the same spawned agent can be queried over both
/// the populated and the dummy path). This is the older-daemon / test-harness
/// shape whose `ListAgents` must yield `live == None`.
async fn start_dummy_server_on(
    registry: Arc<AgentPtyRegistry>,
) -> (TempDir, PathBuf, JoinHandle<()>) {
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };
    let (event_tx, _rx) = tokio::sync::broadcast::channel(16);
    let registry_for_task = registry.clone();
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task, event_tx).await;
    });
    (dir, path, handle)
}

/// PRD #162: hand-build a `SessionState` for the newest-wins join test
/// (session/live/003). Bypasses `apply_event` on purpose so two sessions can
/// coexist on the same `agent_id` + `pane_id` (the `/clear`-restart stale-entry
/// case `apply_event`'s reuse guard would otherwise collapse).
fn make_session(
    session_id: &str,
    pane_id: &str,
    agent_id: &str,
    status: SessionStatus,
    last_prompt: &str,
    last_activity: chrono::DateTime<Utc>,
) -> SessionState {
    SessionState {
        session_id: session_id.to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: None,
        status,
        active_tool: None,
        started_at: last_activity,
        last_activity,
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: Some(last_prompt.to_string()),
        first_prompts: vec![last_prompt.to_string()],
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        display_name: None,
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
                    display_title: None,
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

// ---------------------------------------------------------------------------
// PRD #89 M2b.1 — warm-daemon orchestration-tab hydration regression guard.
// ---------------------------------------------------------------------------
// The snapshot-fallback restore branch (M2b.3) rebuilds an orchestration tab
// from the disk snapshot only when the daemon is EMPTY. The warm-daemon case
// is already covered by the PRD #76 M2.12 + #111 hydration path: each role
// agent carries `TabMembership::Orchestration` and `hydrate_from_daemon`
// echoes it back, so the TUI can place every role pane at its `role_index`
// and recover the start-role cursor. This test pins that end-to-end so a
// regression in warm-daemon orchestration hydration fails here rather than
// silently shifting work onto the snapshot path.
//
// Written as a sync `#[test]` driving an explicit multi-thread runtime rather
// than `#[tokio::test]`: the linkage-check (PRD #77 Decision 17) ties each
// `#[spec(...)]` to the next `fn` definition and the function-name prefix, and
// its scanner only recognises a plain `fn` (not `async fn`). `block_on` keeps
// the async daemon/hydrate flow intact while exposing a sync `fn` to the gate.

/// Scenario: Spawn three orchestration role agents (orchestrator + coder +
/// reviewer) on a warm in-process daemon, each tagged with its
/// `TabMembership::Orchestration` role_index / role_name / is_start_role, then
/// build a fresh controller and hydrate. Asserts warm-daemon hydration
/// reproduces every role as a pane, that placing each hydrated pane at its
/// `role_index` yields the orchestrator + role panes in their saved display
/// order, and that the start (orchestrator) role — i.e. the `start_role_index`
/// cursor — is recoverable from `is_start_role`.
#[spec("session/restore/007")]
#[test]
fn restore_007_warm_daemon_hydrates_orchestration_roles_in_order() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(restore_007_warm_daemon_hydrates_orchestration_roles_in_order_inner());
}

async fn restore_007_warm_daemon_hydrates_orchestration_roles_in_order_inner() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let orchestration_name = "tdd-cycle";
    let cwd = server._dir.path().to_string_lossy().into_owned();
    let role_names = ["orchestrator", "coder", "reviewer"];
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
                    display_title: None,
                }),
                ..Default::default()
            })
            .await
            .expect("start_agent should succeed");
        spawned_ids.push(id);
    }

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
        role_names.len(),
        "every orchestration role should hydrate as a pane; got {hydrated:?}"
    );

    // Place each hydrated pane at its role_index, exactly as the production
    // hydration loop in ui.rs does, then assert the orchestrator + role panes
    // come back in their saved display order with the start role recoverable.
    let mut role_pane_ids: Vec<Option<String>> = vec![None; role_names.len()];
    let mut role_name_by_index: Vec<Option<String>> = vec![None; role_names.len()];
    let mut start_role_index: Option<usize> = None;
    for h in &hydrated {
        let Some(TabMembership::Orchestration {
            name,
            role_index,
            role_name,
            is_start_role,
            ..
        }) = &h.tab_membership
        else {
            panic!("hydrated pane lost its Orchestration tab membership: {h:?}");
        };
        assert_eq!(
            name, orchestration_name,
            "hydrated pane must carry the orchestration name"
        );
        assert!(
            *role_index < role_names.len(),
            "role_index {role_index} out of range"
        );
        role_pane_ids[*role_index] = Some(h.pane_id.clone());
        role_name_by_index[*role_index] = Some(role_name.clone());
        if *is_start_role {
            start_role_index = Some(*role_index);
        }
    }

    // Every role slot is filled — no gaps in the orchestrator + role panes.
    assert!(
        role_pane_ids.iter().all(Option::is_some),
        "every role slot must be filled by hydration; got {role_pane_ids:?}"
    );
    // The role names land back in their saved display order.
    let recovered_order: Vec<String> = role_name_by_index
        .into_iter()
        .map(|n| n.expect("each filled slot has a role name"))
        .collect();
    let expected_order: Vec<String> = role_names.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        recovered_order, expected_order,
        "warm-daemon hydration must reproduce the role panes in saved order"
    );
    // The start_role_index cursor (orchestrator at index 0) is recoverable.
    assert_eq!(
        start_role_index,
        Some(0),
        "the start (orchestrator) role must be recoverable from is_start_role"
    );

    drop(ctrl);
    for id in &spawned_ids {
        let _ = server.registry.close_agent(id);
    }
}

/// Scenario: Spawn a registry agent whose spawn-time `agent_type` is `None`
/// (the "No agent" case) and drive a live `AppState` session on the same
/// `agent_id` + `pane_id` to `Working` with an active tool, `tool_count > 0`,
/// an event-derived ClaudeCode type and a first prompt; calling `ListAgents`
/// over the real attach socket must return the record with `live = Some(...)`
/// carrying that status, the event-derived agent_type (overriding the `None`
/// spawn-time value), the active tool, the tool count and the prompts. The same
/// registry served via the empty dummy-state `serve_attach` path must return
/// the record with `live == None` — today's behavior, no harness regression.
#[spec("session/live/002")]
#[test]
fn live_002_list_agents_attaches_live_snapshot() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(live_002_list_agents_attaches_live_snapshot_inner());
}

async fn live_002_list_agents_attaches_live_snapshot_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "pane-live";

    // Registry record: spawn-time agent_type is None — the legacy "No agent"
    // case the PRD fixes. The event-derived type below must override it.
    let agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("sleep 30"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            agent_type: None,
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");

    // Live, event-derived session state — the store the join must read.
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
    {
        let mut guard = state.write().await;
        drive_session_to_working(&mut guard, "sess-live", pane, &agent_id);
    }

    // Populated-state path: ListAgents must attach the snapshot.
    let (_dir, path, handle) = start_server_with_state(registry.clone(), state.clone()).await;
    let client = DaemonClient::new(path);
    let records = client
        .list_agents()
        .await
        .expect("list_agents should succeed");
    let rec = records
        .iter()
        .find(|r| r.id == agent_id)
        .expect("spawned agent must appear in list_agents");

    assert!(
        rec.agent_type.is_none(),
        "precondition: this record's spawn-time agent_type is None"
    );
    let live = rec
        .live
        .as_ref()
        .expect("reconnect must attach the live SessionSnapshot");
    assert_eq!(live.status, SessionStatus::Working, "live status restored");
    assert_eq!(
        live.agent_type,
        Some(AgentType::ClaudeCode),
        "event-derived agent_type must override the None spawn-time type"
    );
    assert_eq!(
        live.active_tool.as_ref().map(|t| t.name.as_str()),
        Some("Edit"),
        "active tool name preserved across reconnect"
    );
    assert!(
        live.tool_count > 0,
        "tool_count must be > 0, got {}",
        live.tool_count
    );
    assert!(
        live.first_prompts
            .iter()
            .any(|p| p.as_str() == "build the feature"),
        "first prompt context preserved, got {:?}",
        live.first_prompts
    );
    assert_eq!(
        live.last_user_prompt.as_deref(),
        Some("build the feature"),
        "last_user_prompt preserved"
    );

    // Dummy-state path: serve_attach uses an empty AppState → no snapshot,
    // exactly today's behavior (older daemon / test harness). No regression.
    let (_ddir, dpath, dhandle) = start_dummy_server_on(registry.clone()).await;
    let dclient = DaemonClient::new(dpath);
    let drecords = dclient
        .list_agents()
        .await
        .expect("list_agents should succeed");
    let drec = drecords
        .iter()
        .find(|r| r.id == agent_id)
        .expect("spawned agent must appear in dummy-state list_agents");
    assert!(
        drec.live.is_none(),
        "empty dummy-state serve_attach must yield live == None; got {:?}",
        drec.live
    );

    handle.abort();
    dhandle.abort();
    let _ = registry.close_agent(&agent_id);
}

/// Scenario: With two `SessionState`s in `AppState.sessions` that both map to
/// the same agent (same `agent_id` + `pane_id`, e.g. a `/clear` restart that
/// left a stale entry) but different `last_activity` and distinguishing
/// status/prompt, the `ListAgents` join must attach the snapshot from the entry
/// with the most-recent `last_activity` (the live session), not the dead
/// predecessor.
#[spec("session/live/003")]
#[test]
fn live_003_join_picks_newest_last_activity() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(live_003_join_picks_newest_last_activity_inner());
}

async fn live_003_join_picks_newest_last_activity_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "pane-dup";
    let agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("sleep 30"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            agent_type: None,
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");

    let older = Utc::now() - chrono::Duration::seconds(120);
    let newer = Utc::now();
    let dead = make_session(
        "dead-sess",
        pane,
        &agent_id,
        SessionStatus::Idle,
        "STALE PROMPT",
        older,
    );
    let live = make_session(
        "live-sess",
        pane,
        &agent_id,
        SessionStatus::Working,
        "FRESH PROMPT",
        newer,
    );

    let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
    {
        let mut guard = state.write().await;
        // Insert the dead predecessor first; the live session is newer.
        guard.sessions.insert("dead-sess".to_string(), dead);
        guard.sessions.insert("live-sess".to_string(), live);
    }

    let (_dir, path, handle) = start_server_with_state(registry.clone(), state.clone()).await;
    let client = DaemonClient::new(path);
    let records = client
        .list_agents()
        .await
        .expect("list_agents should succeed");
    let rec = records
        .iter()
        .find(|r| r.id == agent_id)
        .expect("spawned agent must appear in list_agents");
    let snap = rec
        .live
        .as_ref()
        .expect("a live snapshot must be attached for the duplicated agent");
    assert_eq!(
        snap.status,
        SessionStatus::Working,
        "newest-wins: must take the live (newer last_activity) session's status, not the dead Idle predecessor"
    );
    assert_eq!(
        snap.last_user_prompt.as_deref(),
        Some("FRESH PROMPT"),
        "newest-wins: must take the newer session's prompt, not the stale predecessor"
    );

    handle.abort();
    let _ = registry.close_agent(&agent_id);
}

/// Scenario: A warm in-process daemon carries two agents — agent A whose
/// spawn-time `agent_type` is `None` (the "No agent" case) driven via
/// `apply_event` to a live `Working` session with an active `Edit` tool,
/// `tool_count > 0`, an event-derived `ClaudeCode` type and a first prompt; and
/// agent B (spawn-time `OpenCode`) with NO live session. Hydrating a fresh
/// controller from that daemon threads the live `SessionSnapshot` through
/// `HydratedPane.live` (agent A `Some`, agent B `None`); seeding each hydrated
/// session the way `ui.rs` does — `AppState::seed_hydrated_session` — makes
/// agent A's card carry the snapshot's `status` / `agent_type` (overriding the
/// `None` spawn-time value) / `active_tool` / `tool_count` / `first_prompts` /
/// `last_user_prompt`, NOT a bare `Idle` / "No agent" placeholder, while agent
/// B's snapshot-absent card falls back to today's bare placeholder (Idle,
/// spawn-time `OpenCode`). Each pane seeds exactly one card — no duplicate.
#[spec("session/live/004")]
#[test]
fn live_004_hydrated_session_seeds_from_live_snapshot_with_fallback() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(live_004_hydrated_session_seeds_from_live_snapshot_with_fallback_inner());
}

async fn live_004_hydrated_session_seeds_from_live_snapshot_with_fallback_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
    let (_dir, path, handle) = start_server_with_state(registry.clone(), state.clone()).await;
    let client = DaemonClient::new(path.clone());

    // Agent A: spawn-time agent_type None — the "No agent" case. It WILL get a
    // live event-derived session below, which the snapshot must surface.
    let pane_a = "pane-live-a";
    let agent_a = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            agent_type: None,
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_a.to_string())],
            ..Default::default()
        })
        .await
        .expect("start_agent A should succeed");

    // Agent B: spawn-time agent_type OpenCode but NO live session → live None.
    // The fallback must seed the bare placeholder from this spawn-time value.
    let pane_b = "pane-bare-b";
    let agent_b = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            agent_type: Some(AgentType::OpenCode),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_b.to_string())],
            ..Default::default()
        })
        .await
        .expect("start_agent B should succeed");

    // Drive ONLY agent A's session to Working (same apply_event flow the daemon
    // uses for hook events).
    {
        let mut guard = state.write().await;
        drive_session_to_working(&mut guard, "sess-a", pane_a, &agent_a);
    }

    // Hydrate a fresh controller from the warm daemon.
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
        2,
        "both agents should hydrate; got {hydrated:?}"
    );

    let h_a = hydrated
        .iter()
        .find(|h| h.agent_id == agent_a)
        .expect("agent A must hydrate as a pane");
    let h_b = hydrated
        .iter()
        .find(|h| h.agent_id == agent_b)
        .expect("agent B must hydrate as a pane");

    // M2.1: the live snapshot threads through HydratedPane.live.
    let live_a = h_a
        .live
        .as_ref()
        .expect("agent A's hydrated pane must carry the live SessionSnapshot");
    assert_eq!(
        live_a.status,
        SessionStatus::Working,
        "live status threaded"
    );
    assert_eq!(
        live_a.agent_type,
        Some(AgentType::ClaudeCode),
        "event-derived agent_type must override the None spawn-time value"
    );
    assert_eq!(
        live_a.active_tool.as_ref().map(|t| t.name.as_str()),
        Some("Edit"),
        "active tool threaded through HydratedPane.live"
    );
    assert!(live_a.tool_count > 0, "tool_count threaded");
    assert!(
        live_a
            .first_prompts
            .iter()
            .any(|p| p.as_str() == "build the feature"),
        "first prompt threaded, got {:?}",
        live_a.first_prompts
    );
    assert!(
        h_b.live.is_none(),
        "agent B has no live session → HydratedPane.live must be None; got {:?}",
        h_b.live
    );

    // M2.2: seed each hydrated session exactly the way the ui.rs hydration loop
    // does — a snapshot-aware insert that seeds from `h.live` when present and
    // falls back to today's bare placeholder when absent. PRD #110 agent_id
    // minting is preserved on the seeded card.
    let mut tui_state = AppState::default();
    for h in &hydrated {
        tui_state.register_pane(h.pane_id.clone());
        tui_state.seed_hydrated_session(
            h.pane_id.clone(),
            h.cwd.clone(),
            h.agent_type.clone(),
            Some(h.agent_id.clone()),
            h.live.as_ref(),
        );
    }

    // Snapshot-seeded card (A): the real live state, NOT Idle / "No agent".
    let a_sessions: Vec<&SessionState> = tui_state
        .sessions
        .values()
        .filter(|s| s.pane_id.as_deref() == Some(pane_a))
        .collect();
    assert_eq!(
        a_sessions.len(),
        1,
        "exactly one card for the live pane (no duplicate); got {a_sessions:?}"
    );
    let sess_a = a_sessions[0];
    assert_eq!(
        sess_a.status,
        SessionStatus::Working,
        "seeded card must show the live Working status, not Idle"
    );
    assert_eq!(
        sess_a.agent_type,
        AgentType::ClaudeCode,
        "seeded card must show the event-derived agent_type, not None ('No agent')"
    );
    assert_eq!(
        sess_a.active_tool.as_ref().map(|t| t.name.as_str()),
        Some("Edit"),
        "seeded card must keep its active tool across the reconnect"
    );
    assert!(
        sess_a.tool_count > 0,
        "seeded card must keep its tool count"
    );
    assert!(
        sess_a
            .first_prompts
            .iter()
            .any(|p| p.as_str() == "build the feature"),
        "seeded card must keep its first-prompt context, got {:?}",
        sess_a.first_prompts
    );
    assert_eq!(
        sess_a.last_user_prompt.as_deref(),
        Some("build the feature"),
        "seeded card must keep its last_user_prompt"
    );
    assert_eq!(
        sess_a.agent_id.as_deref(),
        Some(agent_a.as_str()),
        "PRD #110 agent_id minting must be preserved on the seeded card"
    );

    // Fallback card (B): no snapshot → today's bare placeholder.
    let b_sessions: Vec<&SessionState> = tui_state
        .sessions
        .values()
        .filter(|s| s.pane_id.as_deref() == Some(pane_b))
        .collect();
    assert_eq!(
        b_sessions.len(),
        1,
        "exactly one card for the bare pane (no duplicate); got {b_sessions:?}"
    );
    let sess_b = b_sessions[0];
    assert_eq!(
        sess_b.status,
        SessionStatus::Idle,
        "no snapshot must fall back to today's bare Idle placeholder"
    );
    assert_eq!(
        sess_b.agent_type,
        AgentType::OpenCode,
        "fallback must seed the spawn-time agent_type"
    );
    assert!(
        sess_b.active_tool.is_none(),
        "bare placeholder has no active tool"
    );

    drop(ctrl);
    handle.abort();
    let _ = registry.close_agent(&agent_a);
    let _ = registry.close_agent(&agent_b);
}

/// Scenario: After hydration seeds a card from a live `SessionSnapshot` via
/// `AppState::seed_hydrated_session` (PRD #110 `agent_id` minted on the seeded
/// placeholder), a subsequent post-reconnect `SessionStart` event from the SAME
/// agent — same `pane_id` + `agent_id`, a distinct `session_id` — must remap
/// onto the hydrated card rather than spawning a second one. Asserts exactly one
/// session/pane survives for that agent (no duplicate) and the minted `agent_id`
/// is preserved through the remap.
#[spec("session/live/005")]
#[test]
fn live_005_post_reconnect_session_start_remaps_onto_seeded_card() {
    let pane = "pane-remap";
    let agent_id = "agent-remap-xyz";

    // The live snapshot the daemon would have attached on reconnect.
    let snap = SessionSnapshot {
        status: SessionStatus::Working,
        agent_type: Some(AgentType::ClaudeCode),
        active_tool: Some(ActiveTool {
            name: "Read".into(),
            detail: Some("src/main.rs".into()),
        }),
        tool_count: 2,
        first_prompts: vec!["build the feature".into()],
        last_user_prompt: Some("build the feature".into()),
    };

    // Hydration seeds the card from the snapshot; agent_id is minted on it so
    // the same-agent reuse guard in apply_event can remap a later SessionStart.
    let mut state = AppState::default();
    state.register_pane(pane.to_string());
    state.seed_hydrated_session(
        pane.to_string(),
        None,
        None, // spawn-time agent_type None — overridden by the snapshot
        Some(agent_id.to_string()),
        Some(&snap),
    );
    assert_eq!(
        state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some(pane))
            .count(),
        1,
        "precondition: exactly one seeded card before the SessionStart"
    );

    // A post-reconnect SessionStart from the SAME agent (same pane + agent_id,
    // distinct session_id) must collapse onto the seeded card.
    state.apply_event(AgentEvent {
        session_id: "real-sess".into(),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: Utc::now(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some(pane.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
    });

    let sessions: Vec<&SessionState> = state
        .sessions
        .values()
        .filter(|s| s.pane_id.as_deref() == Some(pane))
        .collect();
    assert_eq!(
        sessions.len(),
        1,
        "post-reconnect SessionStart from the same agent must remap onto the \
         hydrated card, not spawn a duplicate; got {sessions:?}"
    );
    assert_eq!(
        sessions[0].agent_id.as_deref(),
        Some(agent_id),
        "PRD #110 agent_id must be preserved through the remap"
    );
}

// ---------------------------------------------------------------------------
// Mock daemon for the wire-boundary hardening test (session/live/007).
// ---------------------------------------------------------------------------

/// Returns true if `s` carries any ASCII control byte (`< 0x20` or DEL
/// `0x7f`) — the same "no raw control bytes survive into the rendered cell"
/// policy `is_valid_cwd` / `is_valid_display_name` enforce elsewhere on the
/// `list_agents` wire boundary.
fn has_control_bytes(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// Mock that mimics a hostile / malformed daemon: replies to ListAgents with
/// ONE `AgentRecord` whose live `SessionSnapshot` carries ANSI escapes, NUL
/// bytes, and other control chars in `last_user_prompt`, in every
/// `first_prompts` entry, and in `active_tool.name` / `.detail`, where
/// `last_user_prompt`, `active_tool.name`, `active_tool.detail`, AND every
/// `first_prompts` entry are ALSO over-long (~100 KiB each), and where
/// `first_prompts` carries 6 entries (double `MAX_FIRST_PROMPTS`). The
/// TUI-side `list_agents` boundary must scrub control bytes from AND
/// length-clamp every one of these strings before they can corrupt a rebuilt
/// card — not just the `first_prompts` entries.
async fn run_hostile_live_list_server(listener: UnixListener) {
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
            if let AttachRequest::ListAgents = req {
                let over_long = "a".repeat(100_000);
                let hostile = AgentRecord {
                    id: "hostile-live-7".into(),
                    pane_id_env: Some("pane-hostile".into()),
                    display_name: None,
                    cwd: None,
                    tab_membership: None,
                    agent_type: Some(AgentType::ClaudeCode),
                    rows: 0,
                    cols: 0,
                    live: Some(SessionSnapshot {
                        status: SessionStatus::Working,
                        agent_type: Some(AgentType::ClaudeCode),
                        active_tool: Some(ActiveTool {
                            // Oversized in BOTH dimensions: control-laden AND
                            // over-long (~100 KiB) so the length clamp must
                            // apply here too, not only control-stripping.
                            name: format!("Ed\x1bit\x00{over_long}"),
                            detail: Some(format!("src/\x1b[2Jmain.rs\x07{over_long}")),
                        }),
                        tool_count: 3,
                        // Oversized in BOTH dimensions: 6 entries (> the
                        // MAX_FIRST_PROMPTS cap of 3), each control-laden and
                        // each over-long.
                        first_prompts: (0..6)
                            .map(|i| format!("p{i} \x1b[31m\x00{over_long}"))
                            .collect(),
                        // Oversized in BOTH dimensions, like the active-tool
                        // strings: control-laden AND over-long (~100 KiB).
                        last_user_prompt: Some(format!(
                            "run \x1b[31mhostile\x07 \x00prompt {over_long}"
                        )),
                    }),
                };
                let resp = AttachResponse {
                    ok: true,
                    agent_records: Some(vec![hostile]),
                    ..Default::default()
                };
                let _ = write_resp(&mut stream, &resp).await;
            } else {
                let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
            }
        });
    }
}

/// Scenario: A hostile / malformed daemon advertises via `list_agents` an
/// `AgentRecord.live` whose prompt and active-tool strings carry ANSI escapes,
/// NUL bytes, and other control chars AND are over-long (~100 KiB each), and
/// whose `first_prompts` is oversized (6 entries, each also over-long).
/// Calling `DaemonClient::list_agents` against that daemon must return the
/// record with its live snapshot preserved (the agent is real) but SCRUBBED —
/// no control bytes survive in `last_user_prompt`, any `first_prompts` entry,
/// or `active_tool.name` / `.detail` — and CLAMPED — every one of
/// `last_user_prompt`, `active_tool.name`, `active_tool.detail`, and each
/// `first_prompts` entry is length-bounded to <= 65536 bytes, and
/// `first_prompts` is cut to at most `MAX_FIRST_PROMPTS` (3) entries — so a
/// malformed daemon can't corrupt the rebuilt card (parallels
/// embed/attach/005's `tab_membership` scrub).
// Written as a sync `#[test]` driving an explicit multi-thread runtime rather
// than `#[tokio::test]`: the linkage-check (PRD #77 Decision 17) ties each
// `#[spec(...)]` to the next plain `fn` definition and the function-name
// prefix, and does not recognize a `#[tokio::test] async fn` — so the spec'd
// entry point must be a sync `#[test]` that block_on's the async body.
#[spec("session/live/007")]
#[test]
fn live_007_list_agents_sanitizes_and_clamps_hostile_live_snapshot() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(live_007_list_agents_sanitizes_and_clamps_hostile_live_snapshot_inner());
}

async fn live_007_list_agents_sanitizes_and_clamps_hostile_live_snapshot_inner() {
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = UnixListener::bind(&path).expect("bind mock attach socket");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, path, listener)
    };
    let server_handle = tokio::spawn(async move {
        run_hostile_live_list_server(listener).await;
    });

    let client = DaemonClient::new(path);
    let records = client
        .list_agents()
        .await
        .expect("list_agents must succeed");
    assert_eq!(
        records.len(),
        1,
        "one hostile record advertised; got {records:?}"
    );
    let live = records[0].live.as_ref().expect(
        "the live snapshot must be preserved (the agent is real) — scrubbed/clamped, not dropped",
    );

    // No raw control bytes survive into any rendered string, AND each string
    // is length-bounded — a 100 KiB prompt or tool string must be clamped, not
    // passed through verbatim once the control bytes are stripped.
    let last = live.last_user_prompt.as_deref().unwrap_or_default();
    assert!(
        !has_control_bytes(last),
        "last_user_prompt must be scrubbed of control bytes; got {last:?}"
    );
    assert!(
        last.len() <= 65536,
        "last_user_prompt must be length-clamped, not passed through verbatim; got {} bytes",
        last.len()
    );
    let tool = live
        .active_tool
        .as_ref()
        .expect("active_tool must be preserved");
    assert!(
        !has_control_bytes(&tool.name),
        "active_tool.name must be scrubbed of control bytes; got {:?}",
        tool.name
    );
    assert!(
        tool.name.len() <= 65536,
        "active_tool.name must be length-clamped, not passed through verbatim; got {} bytes",
        tool.name.len()
    );
    let detail = tool.detail.as_deref().unwrap_or_default();
    assert!(
        !has_control_bytes(detail),
        "active_tool.detail must be scrubbed of control bytes; got {:?}",
        tool.detail
    );
    assert!(
        detail.len() <= 65536,
        "active_tool.detail must be length-clamped, not passed through verbatim; got {} bytes",
        detail.len()
    );

    // first_prompts clamped to <= MAX_FIRST_PROMPTS (3) entries, each scrubbed
    // and length-bounded so an over-long prompt can't blow up the card.
    assert!(
        live.first_prompts.len() <= 3,
        "first_prompts must be clamped to <= MAX_FIRST_PROMPTS (3); got {} entries",
        live.first_prompts.len()
    );
    for (i, p) in live.first_prompts.iter().enumerate() {
        assert!(
            !has_control_bytes(p),
            "first_prompts[{i}] must be scrubbed of control bytes; got {p:?}"
        );
        assert!(
            p.len() <= 65536,
            "first_prompts[{i}] must be length-clamped, not passed through verbatim; got {} bytes",
            p.len()
        );
    }

    server_handle.abort();
    drop(dir);
}

/// Scenario: A live `SessionState` whose EVENT-DERIVED `agent_type` is
/// `AgentType::None` (the agent has emitted events but never identified itself)
/// is snapshotted via `SessionState::live_snapshot` and seeded onto a
/// reconnected card whose SPAWN-TIME `agent_type` is `Some(ClaudeCode)`.
/// `live_snapshot` must map `AgentType::None` to `Option::None` so the snapshot
/// does NOT shadow the spawn-time fallback, and `seed_hydrated_session` must
/// therefore surface the REAL `ClaudeCode` type on the card — not "No agent".
#[spec("session/live/008")]
#[test]
fn live_008_event_none_agent_type_falls_back_to_spawn_time() {
    let pane = "pane-none-type";
    let agent_id = "agent-none-type";

    // A live session that has emitted events but never resolved its agent type.
    let session = SessionState {
        session_id: format!("pane-{pane}"),
        agent_type: AgentType::None,
        cwd: None,
        status: SessionStatus::Working,
        active_tool: None,
        started_at: Utc::now(),
        last_activity: Utc::now(),
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: None,
        first_prompts: Vec::new(),
        pane_id: Some(pane.to_string()),
        agent_id: Some(agent_id.to_string()),
        display_name: None,
    };

    // The fix lands here: an event-derived AgentType::None must snapshot as
    // Option::None so the spawn-time fallback in seed_hydrated_session wins.
    let snap = session.live_snapshot();
    assert_eq!(
        snap.agent_type, None,
        "live_snapshot must map event-derived AgentType::None to Option::None so it does \
         not shadow the spawn-time agent_type; got {:?}",
        snap.agent_type
    );

    // Seed a reconnected card: the spawn-time type is the REAL ClaudeCode.
    let mut state = AppState::default();
    state.register_pane(pane.to_string());
    state.seed_hydrated_session(
        pane.to_string(),
        None,
        Some(AgentType::ClaudeCode), // spawn-time agent_type — the real one
        Some(agent_id.to_string()),
        Some(&snap),
    );

    let sessions: Vec<&SessionState> = state
        .sessions
        .values()
        .filter(|s| s.pane_id.as_deref() == Some(pane))
        .collect();
    assert_eq!(
        sessions.len(),
        1,
        "exactly one seeded card for the pane; got {sessions:?}"
    );
    assert_eq!(
        sessions[0].agent_type,
        AgentType::ClaudeCode,
        "event-derived AgentType::None must fall back to the spawn-time ClaudeCode, not \
         seed the card as 'No agent'"
    );
}
