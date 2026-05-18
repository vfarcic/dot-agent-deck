//! Regression: daemon-side agent must be cleaned up when `attach` fails
//! after a successful `start_agent`.
//!
//! Before CodeRabbit Fix D, `create_stream_pane` would call
//! `start_agent` → `attach` and propagate any `attach` error to the user
//! while leaving the daemon-side PTY + session orphaned: the user never
//! got a pane to close it through, so the agent leaked for the lifetime
//! of the daemon. The fix captures the agent id from `start_agent` and,
//! on attach error, issues a best-effort `stop_agent` before propagating
//! the original failure.
//!
//! This test pins the cleanup: a mock daemon accepts `StartAgent` (returns
//! a fixed id), rejects `AttachStream` with a typed `Server` error, and
//! records every `StopAgent { id }` request. Calling the pane-creation
//! entry point must surface the attach error and produce exactly one
//! recorded `stop_agent` call for the same id.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, bind_attach_listener, read_frame,
    write_frame,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::{AgentSpawnOptions, PaneController, PaneError};

// `bind_attach_listener` flips the process-global umask while binding; share
// the lock with the other unix-socket tests so concurrent tempdir creation
// can't inherit a 0o600 dir during that window.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct MockServer {
    _dir: TempDir,
    path: PathBuf,
    stop_calls: Arc<Mutex<Vec<String>>>,
    start_id: String,
    handle: JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Spin up a mock daemon that:
/// - replies to `StartAgent` with `agent_id = start_id` (ok=true);
/// - replies to `AttachStream` with `ok=false, error = "attach denied"`;
/// - records every `StopAgent { id }` into `stop_calls` (ok=true reply).
///
/// One request per accepted connection — matches the real daemon's
/// short-lived connection pattern for non-streaming RPCs and the per-call
/// connect that `DaemonClient::{start_agent, attach, stop_agent}` uses.
async fn start_mock_server(start_id: &str) -> MockServer {
    let start_id_owned = start_id.to_string();
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };
    let listener: UnixListener = listener;
    let stop_calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_calls_for_task = stop_calls.clone();
    let start_id_for_task = start_id_owned.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let stop_calls = stop_calls_for_task.clone();
            let start_id = start_id_for_task.clone();
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
                    AttachRequest::StartAgent { .. } => {
                        let resp = AttachResponse::with_id(start_id);
                        let _ = write_resp(&mut stream, &resp).await;
                    }
                    AttachRequest::AttachStream { .. } => {
                        let resp = AttachResponse::err("attach denied");
                        let _ = write_resp(&mut stream, &resp).await;
                    }
                    AttachRequest::StopAgent { id } => {
                        stop_calls.lock().unwrap().push(id);
                        let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                    }
                    _ => {
                        let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                    }
                }
            });
        }
    });
    MockServer {
        _dir: dir,
        path,
        stop_calls,
        start_id: start_id_owned,
        handle,
    }
}

async fn write_resp(s: &mut UnixStream, resp: &AttachResponse) -> std::io::Result<()> {
    let payload = serde_json::to_vec(resp).expect("AttachResponse must serialize");
    write_frame(s, KIND_RESP, &payload).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_failure_cleans_up_daemon_agent() {
    let server = start_mock_server("leaked-agent-7").await;
    let socket = server.path.clone();
    let expected_id = server.start_id.clone();
    let stop_calls = server.stop_calls.clone();

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        socket,
        tokio::runtime::Handle::current(),
    ));

    // create_pane_with_options block_on's the daemon client; run on a
    // blocking thread so the runtime keeps polling the mock server.
    let result = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane_with_options(Some("/bin/sh"), None, AgentSpawnOptions::default())
        })
        .await
        .unwrap()
    };

    let err = result.expect_err("attach error must propagate to caller");
    match err {
        PaneError::CommandFailed(msg) => assert!(
            msg.contains("attach denied"),
            "error must surface the original attach failure; got: {msg}"
        ),
        other => panic!("expected PaneError::CommandFailed, got {other:?}"),
    }

    // The mock daemon must have observed exactly one StopAgent for the id
    // returned by StartAgent. Poll briefly because the stop_agent call
    // happens on the runtime after the blocking thread already returned —
    // the response RESP write on the mock side completes before our
    // assertion only after the runtime schedules the task.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut snapshot: Vec<String> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        snapshot = stop_calls.lock().unwrap().clone();
        if !snapshot.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        snapshot,
        vec![expected_id],
        "attach-failure cleanup must issue exactly one stop_agent for the id start_agent returned"
    );

    // No pane should have been registered locally — `wire_stream_pane`
    // only runs on the success branch.
    assert!(
        ctrl.pane_ids().is_empty(),
        "no pane should be registered when attach fails"
    );
}
