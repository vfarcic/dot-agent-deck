//! PRD #76, M2.1 — `dot-agent-deck daemon attach` stdio bridge.
//!
//! These tests drive `daemon_attach::run_daemon_attach` end-to-end against an
//! in-process attach server. Stdin/stdout are simulated with
//! `tokio::io::duplex` pipes, so the bridge runs in-process — the CLI binary
//! plumbing (`main.rs` → `tokio::io::stdin()/stdout()`) is the only line
//! these tests don't cover, and that's exercised manually before release.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_attach::{AttachError, run_daemon_attach};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, KIND_STREAM_OUT, bind_attach_listener,
    serve_attach,
};

// `bind_attach_listener` flips the process-global umask while binding. Mirror
// the harness lock from the M1.2/M1.3 tests so a tempdir created during that
// window doesn't inherit 0o600 and break a parallel bind. Lock-poisoning is
// ignored (matches the existing harnesses) — a poisoned lock just means a
// previous test panicked, not that the inner state is corrupt.
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

async fn start_server() -> Server {
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

/// Build the duplex pipes for one bridge run and spawn the bridge.
///
/// Returns `(tui_in, tui_out, bridge_handle)` where:
/// - `tui_in` is the test-side write end of the bridge's stdin.
/// - `tui_out` is the test-side read end of the bridge's stdout.
/// - `bridge_handle` resolves with the bridge's `Result`.
fn spawn_bridge(
    socket_path: PathBuf,
) -> (
    DuplexStream,
    DuplexStream,
    JoinHandle<Result<(), AttachError>>,
) {
    // Capacity sized large enough to hold a scrollback snapshot frame plus a
    // few PTY chunks without blocking the bridge mid-relay; the framing is
    // streaming so smaller would also work, just with more wakeups.
    let (tui_in, bridge_stdin) = tokio::io::duplex(64 * 1024);
    let (bridge_stdout, tui_out) = tokio::io::duplex(64 * 1024);
    let handle =
        tokio::spawn(
            async move { run_daemon_attach(&socket_path, bridge_stdin, bridge_stdout).await },
        );
    (tui_in, tui_out, handle)
}

async fn write_frame_to(s: &mut DuplexStream, kind: u8, payload: &[u8]) {
    let mut hdr = [0u8; 5];
    hdr[0] = kind;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    s.write_all(&hdr).await.unwrap();
    if !payload.is_empty() {
        s.write_all(payload).await.unwrap();
    }
    s.flush().await.unwrap();
}

async fn read_frame_from(s: &mut DuplexStream) -> Option<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    s.read_exact(&mut hdr).await.ok()?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        s.read_exact(&mut payload).await.ok()?;
    }
    Some((hdr[0], payload))
}

#[tokio::test]
async fn daemon_attach_relays_list_agents_round_trip() {
    let server = start_server().await;
    let (mut tui_in, mut tui_out, _bridge) = spawn_bridge(server.path.clone());

    // Send a single REQ ListAgents through the bridge → daemon.
    let req = serde_json::to_vec(&AttachRequest::ListAgents).unwrap();
    write_frame_to(&mut tui_in, KIND_REQ, &req).await;

    // Expect a RESP back through the bridge.
    let (kind, payload) =
        tokio::time::timeout(Duration::from_secs(5), read_frame_from(&mut tui_out))
            .await
            .expect("RESP should arrive within timeout")
            .expect("bridge should not close before RESP");
    assert_eq!(kind, KIND_RESP, "expected RESP, got 0x{kind:02x}");

    let resp: AttachResponse =
        serde_json::from_slice(&payload).expect("RESP payload must decode as AttachResponse");
    assert!(resp.ok, "list-agents failed via bridge: {:?}", resp.error);
    assert_eq!(
        resp.agents.expect("list-agents must include agents field"),
        Vec::<String>::new(),
        "registry was empty so the relayed list must be empty"
    );
}

#[tokio::test]
async fn daemon_attach_relays_attach_stream_bytes() {
    let server = start_server().await;

    // Pre-spawn an agent directly in the registry so the test doesn't have to
    // serialize start-agent + attach-stream through one bridge connection
    // (per-connection state machine: one REQ per UnixStream). Using `cat`
    // gives us a PTY whose initial scrollback frame is empty but which will
    // never close on its own — exactly what we need to prove STREAM_OUT
    // bytes flow back through the bridge once we send STREAM_IN.
    let id = server
        .registry
        .spawn_agent(dot_agent_deck::agent_pty::SpawnOptions {
            command: Some("cat"),
            ..Default::default()
        })
        .expect("spawn cat agent");

    let (mut tui_in, mut tui_out, _bridge) = spawn_bridge(server.path.clone());

    // REQ AttachStream { id }
    let req = serde_json::to_vec(&AttachRequest::AttachStream { id: id.clone() }).unwrap();
    write_frame_to(&mut tui_in, KIND_REQ, &req).await;

    // First frame back: RESP { ok: true } — the bridge is transparent so the
    // daemon's RESP reaches us byte-for-byte.
    let (kind, payload) =
        tokio::time::timeout(Duration::from_secs(5), read_frame_from(&mut tui_out))
            .await
            .expect("attach RESP should arrive within timeout")
            .expect("bridge closed before attach RESP");
    assert_eq!(kind, KIND_RESP, "expected RESP first, got 0x{kind:02x}");
    let resp: AttachResponse = serde_json::from_slice(&payload).unwrap();
    assert!(resp.ok, "attach-stream failed: {:?}", resp.error);

    // Drive output: send STREAM_IN bytes through the bridge → daemon → cat
    // → STREAM_OUT back through the bridge.
    write_frame_to(
        &mut tui_in,
        dot_agent_deck::daemon_protocol::KIND_STREAM_IN,
        b"M2_1_BRIDGE_MARKER\n",
    )
    .await;

    // Read frames until we see STREAM_OUT carrying the marker. The first
    // STREAM_OUT may be the (empty) scrollback snapshot, so we accumulate.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut acc: Vec<u8> = Vec::new();
    let mut saw_stream_out = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let frame = match tokio::time::timeout(remaining, read_frame_from(&mut tui_out)).await {
            Ok(Some(f)) => f,
            _ => break,
        };
        if frame.0 == KIND_STREAM_OUT {
            saw_stream_out = true;
            acc.extend_from_slice(&frame.1);
            if acc
                .windows(b"M2_1_BRIDGE_MARKER".len())
                .any(|w| w == b"M2_1_BRIDGE_MARKER")
            {
                break;
            }
        }
    }
    assert!(
        saw_stream_out,
        "expected at least one STREAM_OUT frame to arrive through the bridge"
    );
    assert!(
        acc.windows(b"M2_1_BRIDGE_MARKER".len())
            .any(|w| w == b"M2_1_BRIDGE_MARKER"),
        "expected echoed marker through the bridge; got {:?}",
        String::from_utf8_lossy(&acc)
    );

    // Cleanup so the test doesn't leak the cat child past test exit.
    let _ = server.registry.close_agent(&id);
}

#[tokio::test]
async fn daemon_attach_socket_missing_returns_error() {
    // Point at a path that definitely does not exist. The pre-flight check
    // must fire before any UnixStream::connect attempt — we want the
    // actionable "is the daemon running?" error, not a generic ENOENT from
    // connect(2).
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.sock");

    let (tui_in, tui_out, handle) = spawn_bridge(missing.clone());
    drop(tui_in);
    drop(tui_out);

    let res = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("bridge should return immediately on missing socket")
        .expect("bridge task should not panic");

    let err = res.expect_err("missing socket must surface as Err");
    match &err {
        AttachError::SocketMissing { path } => {
            assert_eq!(path, &missing, "error must name the offending path");
        }
        other => panic!("expected SocketMissing, got {other:?}"),
    }

    // Display impl is what the CLI handler prints to stderr; verify it
    // names the path so an operator knows which socket to look at.
    let rendered = err.to_string();
    assert!(
        rendered.contains(missing.to_str().unwrap()),
        "error message must include the socket path (got {rendered:?})"
    );
}

#[tokio::test]
async fn daemon_attach_propagates_eof_on_stdin_close() {
    let server = start_server().await;
    let (tui_in, _tui_out, handle) = spawn_bridge(server.path.clone());

    // Simulate ssh hanging up on the parent side: drop the test-side write
    // half of the bridge's stdin. The bridge's inbound copy must observe
    // EOF and complete, taking the outbound copy down with it via the
    // shared `select!`.
    drop(tui_in);

    let res = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("bridge must complete within 2s of stdin EOF (no hang)")
        .expect("bridge task must not panic");
    assert!(
        res.is_ok(),
        "stdin EOF is a clean shutdown, not a bridge error: {res:?}"
    );
}
