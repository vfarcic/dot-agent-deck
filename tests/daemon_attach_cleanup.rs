//! Regression: daemon-side agent must be cleaned up when `attach` fails
//! after a successful `start_agent`, and the cleanup itself must be
//! bounded — a wedged daemon answering `attach` with Err quickly and then
//! never responding to `stop_agent` must not pin `create_stream_pane`
//! forever (Fix D fixup, reviewer + auditor P3).
//!
//! Before CodeRabbit Fix D, `create_stream_pane` would call
//! `start_agent` → `attach` and propagate any `attach` error to the user
//! while leaving the daemon-side PTY + session orphaned: the user never
//! got a pane to close it through, so the agent leaked for the lifetime
//! of the daemon. The fix captures the agent id from `start_agent` and,
//! on attach error, issues a best-effort `stop_agent` before propagating
//! the original failure.
//!
//! These tests pin the cleanup:
//! - `attach_failure_cleans_up_daemon_agent`: mock daemon accepts
//!   `StartAgent`, rejects `AttachStream`, records every `StopAgent`
//!   request. `create_stream_pane` must surface the attach error and
//!   produce exactly one `stop_agent` call.
//! - `attach_failure_cleanup_does_not_hang_on_unresponsive_daemon`:
//!   mock daemon accepts `StartAgent`, rejects `AttachStream` quickly,
//!   but never responds to `StopAgent`. `create_stream_pane` must
//!   return within `CREATE_PANE_STOP_TIMEOUT + slack` with the ORIGINAL
//!   attach error.
//! - `attach_hang_does_not_pin_create_stream_pane`: mock daemon accepts
//!   `StartAgent` but never responds to `AttachStream`. The function
//!   must return within `CREATE_PANE_ATTACH_TIMEOUT + cleanup + slack`
//!   with a TimedOut error.

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

/// How the mock daemon should respond to `AttachStream`.
#[derive(Clone, Copy)]
enum AttachBehavior {
    /// Reply promptly with `ok=false, error="attach denied"`.
    Reject,
    /// Accept the request but never write a response (the await on the
    /// client side has to be bounded by a timeout to make progress).
    Hang,
    /// Reply promptly with `ok=true` and hold the connection open
    /// without sending any subsequent frames. The client's I/O task
    /// blocks on reads that never arrive — fine for tests that only
    /// need pane creation to succeed (e.g. exercising close_pane).
    Accept,
}

/// How the mock daemon should respond to `StopAgent`.
#[derive(Clone, Copy)]
enum StopBehavior {
    /// Reply promptly with `ok=true`.
    RespondOk,
    /// Record the request but never write a response — mirrors a wedged
    /// daemon answering one RPC and then deadlocking on the next.
    Hang,
}

#[derive(Clone, Copy)]
struct MockConfig {
    attach: AttachBehavior,
    stop: StopBehavior,
}

impl MockConfig {
    fn attach_reject_stop_ok() -> Self {
        Self {
            attach: AttachBehavior::Reject,
            stop: StopBehavior::RespondOk,
        }
    }
}

/// Spin up a mock daemon that:
/// - replies to `StartAgent` with `agent_id = start_id` (ok=true);
/// - handles `AttachStream` per `config.attach`;
/// - records every `StopAgent { id }` into `stop_calls` and handles the
///   reply per `config.stop` (still records even when the response is
///   never written).
///
/// One request per accepted connection — matches the real daemon's
/// short-lived connection pattern for non-streaming RPCs and the per-call
/// connect that `DaemonClient::{start_agent, attach, stop_agent}` uses.
async fn start_mock_server(start_id: &str, config: MockConfig) -> MockServer {
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
                    AttachRequest::AttachStream { .. } => match config.attach {
                        AttachBehavior::Reject => {
                            let resp = AttachResponse::err("attach denied");
                            let _ = write_resp(&mut stream, &resp).await;
                        }
                        AttachBehavior::Hang => {
                            // Hold the connection open without writing a
                            // response. The client's read await blocks
                            // until its `tokio::time::timeout` fires.
                            std::future::pending::<()>().await;
                        }
                        AttachBehavior::Accept => {
                            // Attach succeeds → pane creation completes
                            // and the pane lands in the registry. Hold
                            // the connection open afterward so the
                            // client-side I/O task doesn't see EOF (it
                            // just blocks on reads that never arrive,
                            // which is fine for close-path tests).
                            let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                            std::future::pending::<()>().await;
                        }
                    },
                    AttachRequest::StopAgent { id } => {
                        stop_calls.lock().unwrap().push(id);
                        match config.stop {
                            StopBehavior::RespondOk => {
                                let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                            }
                            StopBehavior::Hang => {
                                std::future::pending::<()>().await;
                            }
                        }
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
    let server = start_mock_server("leaked-agent-7", MockConfig::attach_reject_stop_ok()).await;
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

/// Auditor P3 on Fix D: a wedged same-UID daemon could answer `attach`
/// with Err promptly then *never* respond to the cleanup `stop_agent`,
/// pinning `create_stream_pane` on the cleanup await forever. The fix
/// bounds the cleanup with `CREATE_PANE_STOP_TIMEOUT` (2s in source);
/// this test pins that behavior with a mock daemon that records the
/// stop_agent request and then holds the connection open forever via
/// `std::future::pending`. The call must return within the cleanup
/// timeout + slack with the ORIGINAL attach error (not a stop-timeout
/// error), confirming that cleanup outcome doesn't mask the real cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_failure_cleanup_does_not_hang_on_unresponsive_daemon() {
    let server = start_mock_server(
        "leaked-agent-stop-hang",
        MockConfig {
            attach: AttachBehavior::Reject,
            stop: StopBehavior::Hang,
        },
    )
    .await;
    let socket = server.path.clone();
    let expected_id = server.start_id.clone();
    let stop_calls = server.stop_calls.clone();

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        socket,
        tokio::runtime::Handle::current(),
    ));

    // Hard cap matches `CREATE_PANE_STOP_TIMEOUT` (2s) + generous slack
    // for the blocking-thread + runtime hop. Without the cleanup timeout
    // this future would hang indefinitely and `tokio::time::timeout`
    // would fire instead, surfacing as the test panic below.
    let outer_deadline = Duration::from_secs(4);
    let started = tokio::time::Instant::now();

    let result = {
        let ctrl = ctrl.clone();
        let join = tokio::task::spawn_blocking(move || {
            ctrl.create_pane_with_options(Some("/bin/sh"), None, AgentSpawnOptions::default())
        });
        match tokio::time::timeout(outer_deadline, join).await {
            Ok(j) => j.unwrap(),
            Err(_) => panic!(
                "create_pane_with_options must not hang when stop_agent never responds; \
                 wedged for > {outer_deadline:?}"
            ),
        }
    };

    let elapsed = started.elapsed();
    let err = result.expect_err("attach error must propagate to caller");
    match err {
        PaneError::CommandFailed(msg) => {
            assert!(
                msg.contains("attach denied"),
                "cleanup-timeout path must still propagate the ORIGINAL attach error; got: {msg}"
            );
            assert!(
                !msg.contains("stop_agent timed out"),
                "must not surface the cleanup-stage timeout as the propagated error; got: {msg}"
            );
        }
        other => panic!("expected PaneError::CommandFailed, got {other:?}"),
    }

    // Sanity: returned within roughly CREATE_PANE_STOP_TIMEOUT (2s) +
    // overhead. If this fails the function probably waited on the stop
    // future without a timeout.
    assert!(
        elapsed < outer_deadline,
        "create_pane_with_options took {elapsed:?} — expected < {outer_deadline:?}"
    );

    // The stop_agent request was issued (and recorded by the mock) even
    // though the mock never replied — proves the cleanup branch ran
    // before the cleanup timeout fired.
    let snapshot = stop_calls.lock().unwrap().clone();
    assert_eq!(
        snapshot,
        vec![expected_id],
        "cleanup must still issue stop_agent (and have it recorded) before its timeout"
    );

    assert!(
        ctrl.pane_ids().is_empty(),
        "no pane should be registered when attach fails"
    );
}

/// Companion to the cleanup-hang test: if `attach` itself never resolves,
/// `create_stream_pane` must return within `CREATE_PANE_ATTACH_TIMEOUT`
/// + cleanup + slack with a TimedOut error and still issue the bounded
/// `stop_agent` cleanup. Without the attach timeout the function would
/// pin on `attach().await` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_hang_does_not_pin_create_stream_pane() {
    let server = start_mock_server(
        "leaked-agent-attach-hang",
        MockConfig {
            attach: AttachBehavior::Hang,
            stop: StopBehavior::RespondOk,
        },
    )
    .await;
    let socket = server.path.clone();
    let expected_id = server.start_id.clone();
    let stop_calls = server.stop_calls.clone();

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        socket,
        tokio::runtime::Handle::current(),
    ));

    // Hard cap = CREATE_PANE_ATTACH_TIMEOUT (3s) + cleanup (~0s here,
    // mock responds promptly) + slack for the blocking-thread hop.
    let outer_deadline = Duration::from_secs(6);
    let started = tokio::time::Instant::now();

    let result = {
        let ctrl = ctrl.clone();
        let join = tokio::task::spawn_blocking(move || {
            ctrl.create_pane_with_options(Some("/bin/sh"), None, AgentSpawnOptions::default())
        });
        match tokio::time::timeout(outer_deadline, join).await {
            Ok(j) => j.unwrap(),
            Err(_) => panic!(
                "create_pane_with_options must not hang when attach never responds; \
                 wedged for > {outer_deadline:?}"
            ),
        }
    };

    let elapsed = started.elapsed();
    let err = result.expect_err("attach timeout must propagate to caller");
    match err {
        PaneError::CommandFailed(msg) => assert!(
            msg.contains("attach timed out"),
            "attach-hang path must surface a TimedOut error; got: {msg}"
        ),
        other => panic!("expected PaneError::CommandFailed, got {other:?}"),
    }
    assert!(
        elapsed < outer_deadline,
        "create_pane_with_options took {elapsed:?} — expected < {outer_deadline:?}"
    );

    // Cleanup must still have fired for the agent_id captured at start.
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
        "attach-timeout path must also run the bounded stop_agent cleanup"
    );

    assert!(
        ctrl.pane_ids().is_empty(),
        "no pane should be registered when attach times out"
    );
}

/// CodeRabbit Fix E: the Ctrl+W close path (`EmbeddedPaneController::close_pane`)
/// used to `block_on(client.stop_agent(...))` unbounded. A wedged daemon
/// would freeze the TUI renderer indefinitely while the pane had already
/// been removed from the registry — phantom-closed-pane bug.
///
/// Pin the fix: mock daemon accepts attach so the pane lands in the
/// registry, then *never* responds to stop_agent. close_pane must:
/// - return within `CREATE_PANE_STOP_TIMEOUT` (2s) + slack,
/// - surface a `PaneError::CommandFailed` whose message contains "timed out",
/// - restore the pane to the registry so the user can retry (same
///   restore-on-failure semantics as the RPC-error branch).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_w_stop_agent_timeout_restores_pane_and_returns_error() {
    let server = start_mock_server(
        "ctrl-w-stop-hang",
        MockConfig {
            attach: AttachBehavior::Accept,
            stop: StopBehavior::Hang,
        },
    )
    .await;
    let socket = server.path.clone();

    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        socket,
        tokio::runtime::Handle::current(),
    ));

    // First create a pane successfully so close_pane has something to
    // operate on. AttachBehavior::Accept replies OK to AttachStream and
    // holds the stream open; the client's I/O task then just waits on
    // reads — fine for our purposes.
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane_with_options(Some("/bin/sh"), None, AgentSpawnOptions::default())
        })
        .await
        .unwrap()
        .expect("create_pane must succeed when attach is accepted")
        .0
    };
    assert!(
        ctrl.pane_ids().contains(&pane_id),
        "pane must be registered after successful create"
    );

    // Now call close_pane. The mock daemon will never answer stop_agent.
    // Without the timeout this future would block_on forever; the outer
    // deadline catches that regression by panicking.
    let outer_deadline = Duration::from_secs(4);
    let started = tokio::time::Instant::now();

    let result = {
        let ctrl = ctrl.clone();
        let pane_id = pane_id.clone();
        let join = tokio::task::spawn_blocking(move || ctrl.close_pane(&pane_id));
        match tokio::time::timeout(outer_deadline, join).await {
            Ok(j) => j.unwrap(),
            Err(_) => panic!(
                "close_pane must not hang when stop_agent never responds; \
                 wedged for > {outer_deadline:?}"
            ),
        }
    };

    let elapsed = started.elapsed();
    let err = result.expect_err("stop_agent timeout must surface as Err");
    match err {
        PaneError::CommandFailed(msg) => assert!(
            msg.contains("timed out"),
            "stop_agent timeout must surface a 'timed out' message; got: {msg}"
        ),
        other => panic!("expected PaneError::CommandFailed, got {other:?}"),
    }

    // Bound: should complete within CREATE_PANE_STOP_TIMEOUT (2s) plus
    // generous slack for the blocking-thread + runtime hop. If this
    // fails the timeout is probably missing or too loose.
    assert!(
        elapsed < outer_deadline,
        "close_pane took {elapsed:?} — expected < {outer_deadline:?}"
    );

    // Pane must be restored so the user can retry Ctrl+W rather than
    // see a phantom-closed pane.
    assert!(
        ctrl.pane_ids().contains(&pane_id),
        "pane must be restored to the registry after stop_agent timeout (got: {:?})",
        ctrl.pane_ids()
    );
}
