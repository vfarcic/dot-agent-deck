#![cfg(unix)]

//! L1 close-path tests backed by a synthetic Unix-socket daemon.
//!
//! These exercise the real `EmbeddedPaneController::close_pane` path without
//! spawning the dot-agent-deck binary. The fake daemon accepts StartAgent and
//! AttachStream, then returns a controlled StopAgent error so the observable
//! local pane registry can prove whether teardown completed or was retained.

use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, read_frame, write_resp,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;
use spec::spec;
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};

struct StopErrorDaemon {
    socket_path: PathBuf,
    stop_requests: Arc<Mutex<Vec<String>>>,
    list_requests: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _dir: TempDir,
}

#[derive(Clone, Copy)]
enum StopPlan {
    Error(&'static str),
    Replacement(&'static str),
}

impl StopErrorDaemon {
    fn spawn(stop_error: &'static str) -> Self {
        Self::spawn_with_plan(StopPlan::Error(stop_error))
    }

    fn spawn_with_replacement(replacement_id: &'static str) -> Self {
        Self::spawn_with_plan(StopPlan::Replacement(replacement_id))
    }

    fn spawn_with_plan(stop_plan: StopPlan) -> Self {
        let dir = tempfile::tempdir().expect("synthetic daemon tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = StdUnixListener::bind(&socket_path).expect("bind synthetic daemon socket");
        listener
            .set_nonblocking(true)
            .expect("set synthetic daemon listener nonblocking");

        let stop_requests = Arc::new(Mutex::new(Vec::new()));
        let list_requests = Arc::new(AtomicUsize::new(0));
        let pane_id_env = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests_for_thread = Arc::clone(&stop_requests);
        let lists_for_thread = Arc::clone(&list_requests);
        let pane_id_for_thread = Arc::clone(&pane_id_env);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("synthetic daemon runtime");
            runtime.block_on(async move {
                let listener = UnixListener::from_std(listener)
                    .expect("convert synthetic daemon listener to tokio");
                while !shutdown_for_thread.load(Ordering::SeqCst) {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.expect("accept synthetic daemon client");
                            let stop_requests = Arc::clone(&requests_for_thread);
                            let list_requests = Arc::clone(&lists_for_thread);
                            let pane_id_env = Arc::clone(&pane_id_for_thread);
                            tokio::spawn(async move {
                                handle_connection(
                                    stream,
                                    stop_plan,
                                    stop_requests,
                                    list_requests,
                                    pane_id_env,
                                ).await;
                            });
                        }
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                }
            });
        });

        Self {
            socket_path,
            stop_requests,
            list_requests,
            shutdown,
            thread: Some(thread),
            _dir: dir,
        }
    }

    fn path(&self) -> &Path {
        &self.socket_path
    }

    fn stop_requests(&self) -> Vec<String> {
        self.stop_requests.lock().unwrap().clone()
    }

    fn list_requests(&self) -> usize {
        self.list_requests.load(Ordering::SeqCst)
    }
}

impl Drop for StopErrorDaemon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join synthetic daemon thread");
        }
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    stop_plan: StopPlan,
    stop_requests: Arc<Mutex<Vec<String>>>,
    list_requests: Arc<AtomicUsize>,
    pane_id_env: Arc<Mutex<Option<String>>>,
) {
    let Some((kind, payload)) = read_frame(&mut stream)
        .await
        .expect("read synthetic daemon request")
    else {
        return;
    };
    assert_eq!(kind, KIND_REQ, "controller must send a request frame");
    let request: AttachRequest =
        serde_json::from_slice(&payload).expect("decode synthetic daemon request");

    match request {
        AttachRequest::StartAgent { env, .. } => {
            *pane_id_env.lock().unwrap() = env
                .into_iter()
                .find_map(|(key, value)| (key == "DOT_AGENT_DECK_PANE_ID").then_some(value));
            write_resp(&mut stream, &AttachResponse::with_id("agent-1".to_string()))
                .await
                .expect("reply to StartAgent");
        }
        AttachRequest::AttachStream { .. } => {
            write_resp(&mut stream, &AttachResponse::ok())
                .await
                .expect("reply to AttachStream");
            // Keep the long-lived attach connection open until the controller
            // tears down the pane and drops its stream task.
            while read_frame(&mut stream).await.ok().flatten().is_some() {}
        }
        AttachRequest::StopAgent { id } => {
            stop_requests.lock().unwrap().push(id.clone());
            let response = match stop_plan {
                StopPlan::Error(stop_error) => AttachResponse::err(stop_error),
                StopPlan::Replacement(replacement_id) if id == replacement_id => {
                    AttachResponse::ok()
                }
                StopPlan::Replacement(_) => AttachResponse::err(format!("Agent {id} not found")),
            };
            write_resp(&mut stream, &response)
                .await
                .expect("reply to StopAgent");
        }
        AttachRequest::ListAgents => {
            list_requests.fetch_add(1, Ordering::SeqCst);
            let records = match stop_plan {
                StopPlan::Error(_) => Vec::new(),
                StopPlan::Replacement(replacement_id) => vec![AgentRecord {
                    id: replacement_id.to_string(),
                    pane_id_env: pane_id_env.lock().unwrap().clone(),
                    display_name: None,
                    cwd: None,
                    tab_membership: None,
                    agent_type: None,
                    rows: 24,
                    cols: 80,
                    live: None,
                }],
            };
            write_resp(&mut stream, &AttachResponse::agent_records(records))
                .await
                .expect("reply to ListAgents");
        }
        other => panic!("unexpected synthetic daemon request: {other:?}"),
    }
}

fn controller_for(
    daemon: &StopErrorDaemon,
) -> (tokio::runtime::Runtime, EmbeddedPaneController, String) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("controller runtime");
    let controller =
        EmbeddedPaneController::new(daemon.path().to_path_buf(), runtime.handle().clone());
    let pane_id = controller
        .create_pane(Some("cat"), Some("/tmp"))
        .expect("synthetic daemon should create and attach the pane");
    assert_eq!(controller.pane_ids(), vec![pane_id.clone()]);
    (runtime, controller, pane_id)
}

/// Scenario: Create and attach a pane through a synthetic daemon, then have both StopAgent attempts report that the agent is not found. Closing must treat that response as already stopped, return success, and remove the pane from the local registry instead of restoring a ghost card.
#[spec("lifecycle/stop/005")]
#[test]
fn stop_005_not_found_completes_teardown() {
    let daemon = StopErrorDaemon::spawn("Agent agent-1 not found");
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let result = controller.close_pane(&pane_id);

    assert!(
        result.is_ok(),
        "an already-gone daemon agent must count as a successful close, got {result:?}"
    );
    assert!(
        controller.pane_ids().is_empty(),
        "a successful already-stopped close must not re-insert the pane"
    );
    assert_eq!(
        daemon.stop_requests(),
        vec!["agent-1", "agent-1"],
        "the existing reattach-race retry remains before NotFound is classified as already stopped"
    );
    assert!(
        daemon.list_requests() > 0,
        "the test must exercise the real pane-slot resolution after both NotFound replies"
    );
}

/// Scenario: Create and attach a pane through a synthetic daemon, then make StopAgent fail with a genuine server error. Closing must surface the error and retain the pane in the local registry so the user can see it and retry.
#[spec("lifecycle/stop/006")]
#[test]
fn stop_006_genuine_error_retains_pane() {
    let daemon = StopErrorDaemon::spawn("permission denied while stopping agent");
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let error = controller
        .close_pane(&pane_id)
        .expect_err("a genuine StopAgent error must remain visible");

    assert!(
        error.to_string().contains("permission denied"),
        "the surfaced close error must preserve the daemon's reason: {error}"
    );
    assert_eq!(
        controller.pane_ids(),
        vec![pane_id],
        "a genuine stop failure must re-insert the pane for retry"
    );
    assert_eq!(
        daemon.stop_requests(),
        vec!["agent-1"],
        "only NotFound receives the reattach-race retry"
    );
}

/// Scenario: Close live panes against daemon errors that contain `not found` but are not the exact requested-agent error. Every case must surface the original error, retain the pane for retry, and avoid the NotFound retry/slot-resolution path.
#[spec("lifecycle/stop/007")]
#[test]
fn stop_007_unrelated_not_found_errors_retain_live_pane() {
    for stop_error in [
        "pane not found",
        "session not found",
        "Agent agent-2 not found",
        "stop failed: Agent agent-1 not found",
    ] {
        let daemon = StopErrorDaemon::spawn(stop_error);
        let (_runtime, controller, pane_id) = controller_for(&daemon);

        let error = controller
            .close_pane(&pane_id)
            .expect_err("a merely-containing not-found error must remain visible");

        assert!(
            error.to_string().contains(stop_error),
            "the surfaced error must retain the daemon message {stop_error:?}: {error}"
        );
        assert_eq!(
            controller.pane_ids(),
            vec![pane_id],
            "a live pane must be retained for {stop_error:?}"
        );
        assert_eq!(
            daemon.stop_requests(),
            vec!["agent-1"],
            "an unrelated not-found message must not enter the exact-agent retry path"
        );
        assert_eq!(
            daemon.list_requests(),
            0,
            "slot resolution is reserved for exact id-scoped Agent not found replies"
        );
    }
}

/// Scenario: Create a pane whose original agent disappears while a replacement now owns the same stable pane slot. After both stale-id StopAgent attempts return exact NotFound, close must discover the replacement through ListAgents, stop it, and complete teardown without orphaning it.
#[spec("lifecycle/stop/008")]
#[test]
fn stop_008_replacement_slot_owner_is_stopped() {
    let daemon = StopErrorDaemon::spawn_with_replacement("agent-2");
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let result = controller.close_pane(&pane_id);

    assert!(result.is_ok(), "replacement-aware close failed: {result:?}");
    assert!(
        controller.pane_ids().is_empty(),
        "the pane may be removed only after its replacement agent is stopped"
    );
    assert_eq!(
        daemon.stop_requests(),
        vec!["agent-1", "agent-1", "agent-2"],
        "the close path must stop the replacement discovered in the pane slot"
    );
    assert!(
        daemon.list_requests() > 0,
        "the replacement must be discovered through the real ListAgents path"
    );
}
