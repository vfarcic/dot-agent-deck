//! Round-trip tests for the M1.2 streaming attach protocol. Each test spins
//! up an in-process attach server bound to a tempdir socket, drives it with
//! a UnixStream client, and verifies every message kind round-trips
//! correctly — including concurrent attach-stream subscribers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_DETACH, KIND_REQ, KIND_RESP, KIND_STREAM_END,
    KIND_STREAM_IN, KIND_STREAM_OUT, run_attach_server,
};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

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

async fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attach.sock");
    let registry = Arc::new(AgentPtyRegistry::new());

    let registry_for_task = registry.clone();
    let path_for_task = path.clone();
    let handle = tokio::spawn(async move {
        let _ = run_attach_server(&path_for_task, registry_for_task).await;
    });

    // Wait for the listener to actually accept connections. The socket
    // inode appearing on disk is *necessary but not sufficient* — under
    // parallel test load, `bind()` can return before `accept()` is ready
    // to be called, so we probe with a real connect-and-disconnect.
    let mut connected = false;
    for _ in 0..300 {
        if path.exists()
            && let Ok(stream) = UnixStream::connect(&path).await
        {
            drop(stream);
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(connected, "attach socket never accepted a connection");

    Server {
        _dir: dir,
        path,
        registry,
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

async fn start_agent(server: &Server, command: &str) -> String {
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::StartAgent {
            command: Some(command.into()),
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
}

async fn connect_attach(server: &Server, id: &str) -> UnixStream {
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::AttachStream { id: id.into() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok, "attach-stream failed: {:?}", resp.error);
    s
}

/// Read STREAM_OUT frames from `s` until the accumulated bytes contain
/// `marker`, or the timeout fires. Returns the accumulated bytes (only
/// useful for diagnostics on failure).
async fn read_until_contains(s: &mut UnixStream, marker: &[u8]) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut acc: Vec<u8> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(s)).await {
            Ok(Some((KIND_STREAM_OUT, bytes))) => {
                acc.extend_from_slice(&bytes);
                if acc.windows(marker.len()).any(|w| w == marker) {
                    return acc;
                }
            }
            Ok(Some((KIND_STREAM_END, _))) => {
                panic!(
                    "stream ended before marker {:?} appeared; got {:?}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&acc)
                );
            }
            Ok(Some(_)) => continue,
            Ok(None) => {
                panic!(
                    "connection closed before marker {:?}; got {:?}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&acc)
                );
            }
            Err(_) => break,
        }
    }
    panic!(
        "timeout waiting for marker {:?}; got {:?}",
        String::from_utf8_lossy(marker),
        String::from_utf8_lossy(&acc)
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_agents_returns_empty_initially() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);
    assert_eq!(resp.agents.as_deref(), Some(&[][..] as &[String]));
}

#[tokio::test]
async fn start_list_stop_round_trip() {
    let server = start_server().await;

    let id = start_agent(&server, "/bin/sh").await;

    // list-agents sees the new id.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert_eq!(resp.agents, Some(vec![id.clone()]));

    // stop-agent succeeds and the registry is empty afterwards.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::StopAgent { id: id.clone() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);

    // Verify via the in-process registry handle.
    assert!(server.registry.is_empty());
}

#[tokio::test]
async fn stop_unknown_agent_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::StopAgent {
            id: "does-not-exist".into(),
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
    assert!(resp.error.is_some());
}

#[tokio::test]
async fn malformed_request_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    // Send a REQ frame with non-JSON payload.
    write_frame(&mut s, KIND_REQ, b"not json").await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
    assert!(
        resp.error
            .as_deref()
            .is_some_and(|e| e.contains("malformed request"))
    );
}

#[tokio::test]
async fn wrong_first_frame_kind_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    // Server expects KIND_REQ as the first frame; sending STREAM_IN should
    // produce an error response.
    write_frame(&mut s, KIND_STREAM_IN, b"oops").await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
}

#[tokio::test]
async fn attach_stream_forwards_keystrokes_and_output() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;

    // Send a command and verify its output flows back as STREAM_OUT.
    write_frame(&mut a, KIND_STREAM_IN, b"echo HELLO-MARKER\n").await;
    read_until_contains(&mut a, b"HELLO-MARKER").await;

    // Cleanup: kill the agent.
    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn two_attach_clients_both_receive_output() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;
    let mut b = connect_attach(&server, &id).await;

    // Client A drives the keystroke; both A and B must see the resulting
    // PTY output. This is the concurrent-attach property from PRD line 199.
    write_frame(&mut a, KIND_STREAM_IN, b"echo SHARED-MARKER\n").await;

    let (got_a, got_b) = tokio::join!(
        read_until_contains(&mut a, b"SHARED-MARKER"),
        read_until_contains(&mut b, b"SHARED-MARKER"),
    );
    assert!(!got_a.is_empty());
    assert!(!got_b.is_empty());

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn snapshot_returns_scrollback() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    // Drive some output into scrollback via an attach session, then close
    // that session.
    {
        let mut a = connect_attach(&server, &id).await;
        write_frame(&mut a, KIND_STREAM_IN, b"echo SCROLL-MARKER\n").await;
        read_until_contains(&mut a, b"SCROLL-MARKER").await;
        write_frame(&mut a, KIND_DETACH, &[]).await;
    }

    // Give the daemon a moment to process the detach.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Snapshot should still contain the marker.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::Snapshot { id: id.clone() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);

    let mut snap: Vec<u8> = Vec::new();
    loop {
        match read_frame(&mut s).await {
            Some((KIND_STREAM_OUT, b)) => snap.extend_from_slice(&b),
            Some((KIND_STREAM_END, _)) => break,
            Some(_) => continue,
            None => break,
        }
    }
    assert!(
        String::from_utf8_lossy(&snap).contains("SCROLL-MARKER"),
        "snapshot should contain SCROLL-MARKER; got: {:?}",
        String::from_utf8_lossy(&snap)
    );

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn snapshot_unknown_agent_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::Snapshot { id: "nope".into() }).await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
}

#[tokio::test]
async fn detach_does_not_kill_agent() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;
    write_frame(&mut a, KIND_DETACH, &[]).await;
    drop(a);

    // The agent must still be in the registry.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert_eq!(resp.agents, Some(vec![id.clone()]));

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn killing_agent_emits_stream_end_to_attached_clients() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;

    // Stop the agent via the protocol from a separate connection.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::StopAgent { id: id.clone() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);

    // The attached client must see STREAM_END (possibly preceded by some
    // residual STREAM_OUT bytes) within a reasonable timeout.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_end = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(&mut a)).await {
            Ok(Some((KIND_STREAM_END, _))) => {
                saw_end = true;
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw_end,
        "attached client should observe STREAM_END after agent stop"
    );
}

#[tokio::test]
async fn attach_unknown_agent_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::AttachStream {
            id: "missing".into(),
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
}
