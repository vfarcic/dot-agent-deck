//! M4.2 — single-machine end-to-end integration test for the daemon's
//! streaming attach protocol.
//!
//! Walks one client through the complete user journey in a single linear
//! scenario: spin up the daemon (hook-ingestion + attach servers via
//! `run_daemon_with` / `Daemon::with_attach`, the same entrypoint
//! `main.rs` uses), list (empty), start an agent, list (one), attach,
//! drive input, observe output, detach, observe the agent survives,
//! reattach and observe scrollback, stop, list (empty). The isolated
//! per-feature tests in `tests/daemon_protocol.rs` cover each step on
//! its own; this test proves they compose.
//!
//! In-process rather than spawning a separate `dot-agent-deck daemon`
//! subprocess: this matches the existing convention in
//! `tests/integration_test.rs`, exercises the real production daemon
//! entry (`run_daemon_with`), and avoids the build-product path /
//! cargo-test-binary indirection that gives end-to-end tests a poor
//! signal-to-noise ratio.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_DETACH, KIND_REQ, KIND_RESP, KIND_STREAM_IN,
    KIND_STREAM_OUT,
};
use dot_agent_deck::state::AppState;

// Same umask-narrowing serialization as the other integration test
// binaries — see comments in tests/daemon_protocol.rs.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct DaemonHandle {
    _dir: TempDir,
    attach_path: PathBuf,
    handle: JoinHandle<()>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn spawn_daemon() -> DaemonHandle {
    let (dir, hook_path, attach_path) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hook.sock");
        let attach = dir.path().join("attach.sock");
        (dir, hook, attach)
    };

    let state = Arc::new(RwLock::new(AppState::default()));
    let attach_for_daemon = attach_path.clone();
    let handle = tokio::spawn(async move {
        let daemon = Daemon::with_attach(state, attach_for_daemon);
        let _ = run_daemon_with(&hook_path, daemon).await;
    });

    // Wait for both sockets to bind. `run_daemon_with` binds the hook
    // socket first, then spawns the attach server. We poll the attach
    // socket since that's the one this test connects to.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if attach_path.exists() {
            // Inode exists, but the listener may not be accepting yet.
            // A trial connect is the cheapest way to confirm readiness.
            if UnixStream::connect(&attach_path).await.is_ok() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        attach_path.exists(),
        "attach socket did not appear within 5s"
    );

    DaemonHandle {
        _dir: dir,
        attach_path,
        handle,
    }
}

async fn write_frame(s: &mut UnixStream, kind: u8, payload: &[u8]) {
    let mut hdr = [0u8; 5];
    hdr[0] = kind;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    s.write_all(&hdr).await.unwrap();
    if !payload.is_empty() {
        s.write_all(payload).await.unwrap();
    }
}

async fn read_frame(s: &mut UnixStream) -> Option<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    s.read_exact(&mut hdr).await.ok()?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        s.read_exact(&mut payload).await.ok()?;
    }
    Some((hdr[0], payload))
}

async fn write_request(s: &mut UnixStream, req: &AttachRequest) {
    let payload = serde_json::to_vec(req).unwrap();
    write_frame(s, KIND_REQ, &payload).await;
}

async fn read_response(s: &mut UnixStream) -> AttachResponse {
    let (kind, payload) = read_frame(s).await.expect("expected RESP frame");
    assert_eq!(kind, KIND_RESP, "expected RESP, got 0x{kind:02x}");
    serde_json::from_slice(&payload).expect("RESP payload must be valid JSON")
}

async fn list_agents(daemon: &DaemonHandle) -> Vec<String> {
    let mut s = UnixStream::connect(&daemon.attach_path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok, "list-agents failed: {:?}", resp.error);
    resp.agents.unwrap_or_default()
}

async fn read_until_marker(s: &mut UnixStream, marker: &[u8], budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    let mut acc: Vec<u8> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(s)).await {
            Ok(Some((KIND_STREAM_OUT, bytes))) => {
                acc.extend_from_slice(&bytes);
                if acc.windows(marker.len()).any(|w| w == marker) {
                    return true;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
    false
}

#[tokio::test]
async fn end_to_end_lifecycle_start_attach_detach_reattach_stop() {
    let daemon = spawn_daemon().await;

    // 1. Empty start.
    assert!(list_agents(&daemon).await.is_empty());

    // 2. Start an agent.
    let id = {
        let mut s = UnixStream::connect(&daemon.attach_path).await.unwrap();
        write_request(
            &mut s,
            &AttachRequest::StartAgent {
                command: Some("/bin/sh".into()),
                cwd: None,
                rows: 24,
                cols: 80,
                env: vec![],
            },
        )
        .await;
        let resp = read_response(&mut s).await;
        assert!(resp.ok, "start-agent failed: {:?}", resp.error);
        resp.id.expect("start-agent response missing id")
    };

    // 3. List sees the new id.
    assert_eq!(list_agents(&daemon).await, vec![id.clone()]);

    // 4. First attach: drive input, observe output.
    {
        let mut a = UnixStream::connect(&daemon.attach_path).await.unwrap();
        write_request(&mut a, &AttachRequest::AttachStream { id: id.clone() }).await;
        let resp = read_response(&mut a).await;
        assert!(resp.ok, "attach failed: {:?}", resp.error);

        write_frame(&mut a, KIND_STREAM_IN, b"echo LIFECYCLE-PRE\n").await;
        assert!(
            read_until_marker(&mut a, b"LIFECYCLE-PRE", Duration::from_secs(5)).await,
            "first attach must observe LIFECYCLE-PRE"
        );

        // 5. Explicit detach.
        write_frame(&mut a, KIND_DETACH, &[]).await;
    }

    // 6. Detach must not kill the agent.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        list_agents(&daemon).await,
        vec![id.clone()],
        "agent must survive detach"
    );

    // 7. Reattach: initial-snapshot prelude must replay LIFECYCLE-PRE.
    {
        let mut a = UnixStream::connect(&daemon.attach_path).await.unwrap();
        write_request(&mut a, &AttachRequest::AttachStream { id: id.clone() }).await;
        let resp = read_response(&mut a).await;
        assert!(resp.ok, "reattach failed: {:?}", resp.error);

        assert!(
            read_until_marker(&mut a, b"LIFECYCLE-PRE", Duration::from_secs(2)).await,
            "reattach must replay LIFECYCLE-PRE from scrollback"
        );

        write_frame(&mut a, KIND_DETACH, &[]).await;
    }

    // 8. Stop.
    {
        let mut s = UnixStream::connect(&daemon.attach_path).await.unwrap();
        write_request(&mut s, &AttachRequest::StopAgent { id: id.clone() }).await;
        let resp = read_response(&mut s).await;
        assert!(resp.ok, "stop-agent failed: {:?}", resp.error);
    }

    // 9. List is empty again.
    assert!(list_agents(&daemon).await.is_empty());
}
