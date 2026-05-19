//! PRD #76, M2.10 audit follow-up — coalescing of stream-pane resizes.
//!
//! Before the fix, every `resize_pane_pty` on a stream-backed pane spawned a
//! detached task that opened a fresh Unix socket and issued a one-shot
//! `Resize` op. Rapid layout churn could pile up tasks, FDs, and per-connection
//! daemon work without bound. The fix routes all resizes for a pane through a
//! per-pane single-slot watch channel drained by one resize worker, so
//! intermediate values are dropped and at most one daemon Resize is in flight
//! per pane at a time.
//!
//! This test exercises that property end-to-end: it stands up a tiny mock
//! daemon that counts `Resize` requests, fires many rapid `resize_pane_pty`
//! calls through a real `EmbeddedPaneController`, and asserts the daemon saw
//! far fewer Resize requests than the controller did.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, read_frame, write_frame,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;

// `bind_attach_listener` flips the process-global umask while binding. Other
// tests that touch tempdirs hold this same lock — share it so we don't race
// against them and inherit a 0o600 tempdir.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

/// Mock daemon: handles `StartAgent`, `AttachStream`, `StopAgent`, and counts
/// `Resize` requests. Intentionally minimal — does not spawn a real PTY and
/// does not stream any output. The controller's reader half blocks on
/// `read_frame` after the initial RESP, which is exactly what we want here:
/// the test's interesting traffic is the resize fan-out, not the agent
/// stdout.
async fn run_counting_server(listener: UnixListener, resize_count: Arc<AtomicUsize>) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let count = resize_count.clone();
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
                    let resp = AttachResponse {
                        ok: true,
                        id: Some("mock-agent-1".to_string()),
                        ..Default::default()
                    };
                    let _ = write_resp(&mut stream, &resp).await;
                }
                AttachRequest::AttachStream { .. } => {
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                    // Keep the connection open until the controller drops
                    // its end. Drain STREAM_IN frames so the writer side
                    // never blocks the controller's io_task.
                    loop {
                        match read_frame(&mut stream).await {
                            Ok(None) | Err(_) => break,
                            Ok(Some(_)) => continue,
                        }
                    }
                }
                AttachRequest::Resize { .. } => {
                    count.fetch_add(1, Ordering::Relaxed);
                    // Hold the response so the controller-side worker can't
                    // dispatch the next value until this one completes. The
                    // delay is what makes coalescing observable: it forces
                    // the burst of 100 controller calls to land in the watch
                    // slot while exactly one in-flight Resize is parked
                    // here, so they collapse to a single trailing reconcile
                    // dispatch instead of producing a fan-out.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                }
                AttachRequest::StopAgent { .. } => {
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
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

struct MockServer {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
    resize_count: Arc<AtomicUsize>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_mock_server() -> MockServer {
    let resize_count = Arc::new(AtomicUsize::new(0));
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = UnixListener::bind(&path).expect("bind mock attach socket");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, path, listener)
    };
    let count_clone = resize_count.clone();
    let handle = tokio::spawn(async move {
        run_counting_server(listener, count_clone).await;
    });
    MockServer {
        _dir: dir,
        path,
        handle,
        resize_count,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rapid_resize_calls_coalesce_to_few_daemon_requests() {
    let server = start_mock_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // create_pane is blocking → it `block_on`s the mock daemon. Run it on a
    // blocking thread so the runtime keeps polling the in-process server.
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh"), None)
                .expect("create_pane should succeed against the mock daemon")
        })
        .await
        .unwrap()
    };

    // Fire many resizes back-to-back. Without coalescing each one would
    // produce a fresh Unix-socket connection and a daemon Resize request;
    // with coalescing only the first plus (at most) one trailing dispatch
    // should reach the wire.
    let n: usize = 100;
    for i in 0..n {
        let rows = 24 + (i as u16 % 10);
        let cols = 80 + (i as u16 % 10);
        ctrl.resize_pane_pty(&pane_id, rows, cols)
            .expect("resize_pane_pty should not fail");
    }

    // The mock daemon parks each Resize for 100ms before responding, so
    // the worker can dispatch at most one Resize during the burst. After
    // that in-flight call completes, the worker reads the latest value
    // from the watch slot and dispatches one trailing "reconcile" Resize
    // — total 2. Wait well past two response intervals so the trailing
    // dispatch has time to land before we sample the counter.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let observed = server.resize_count.load(Ordering::Relaxed);
    assert!(
        observed >= 1,
        "expected at least one Resize request to reach the daemon, got {observed}"
    );
    // Without coalescing this would be ~100. With coalescing + a 100ms
    // mock-daemon delay we expect 1 in-flight + 1 trailing reconcile = 2.
    // The bound is intentionally a touch loose to absorb CI scheduling
    // jitter without losing the regression signal.
    assert!(
        observed <= 3,
        "expected coalescing to skip intermediate values; got {observed} daemon Resize requests for {n} controller calls"
    );

    drop(ctrl);
}
