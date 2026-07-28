#![cfg(unix)]

//! L1 close-path tests backed by a synthetic Unix-socket daemon.
//!
//! These exercise the real `EmbeddedPaneController::close_pane` path without
//! spawning the dot-agent-deck binary. The fake daemon accepts StartAgent and
//! AttachStream, then returns a controlled StopAgent error so the observable
//! local pane registry can prove whether teardown completed or was retained.

use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

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
    stop_requests: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _dir: TempDir,
}

impl StopErrorDaemon {
    fn spawn(stop_error: &'static str) -> Self {
        let dir = tempfile::tempdir().expect("synthetic daemon tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = StdUnixListener::bind(&socket_path).expect("bind synthetic daemon socket");
        listener
            .set_nonblocking(true)
            .expect("set synthetic daemon listener nonblocking");

        let stop_requests = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests_for_thread = Arc::clone(&stop_requests);
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
                            tokio::spawn(async move {
                                handle_connection(stream, stop_error, stop_requests).await;
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
            shutdown,
            thread: Some(thread),
            _dir: dir,
        }
    }

    fn path(&self) -> &Path {
        &self.socket_path
    }

    fn stop_requests(&self) -> usize {
        self.stop_requests.load(Ordering::SeqCst)
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
    stop_error: &'static str,
    stop_requests: Arc<AtomicUsize>,
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
        AttachRequest::StartAgent { .. } => {
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
        AttachRequest::StopAgent { .. } => {
            stop_requests.fetch_add(1, Ordering::SeqCst);
            write_resp(&mut stream, &AttachResponse::err(stop_error))
                .await
                .expect("reply to StopAgent");
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
        2,
        "the existing reattach-race retry remains before NotFound is classified as already stopped"
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
        1,
        "only NotFound receives the reattach-race retry"
    );
}
