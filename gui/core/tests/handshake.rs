//! Integration test for the GUI core's connect → `Hello` → bridge path.
//!
//! It stands up an in-process *stub daemon* on a temp Unix socket — no real
//! `dot-agent-deck` binary, no webview — that speaks exactly the `Hello`
//! handshake (using the shared `protocol` wire types) and bridges one frame
//! each direction. That keeps the test hermetic and fast enough for the
//! `cargo test-fast` tier while still exercising the real socket transport,
//! the real frame codec, and the real version-negotiation logic.

use dad_gui_core::{ConnectError, PROTOCOL_VERSION, connect_and_handshake};
use protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, KIND_STREAM_IN, KIND_STREAM_OUT,
    read_frame, write_frame,
};
use tokio::net::UnixListener;

/// Accept one connection, complete the `Hello` handshake reporting
/// `server_version`, then bridge one frame each direction: read the client's
/// `KIND_STREAM_IN` and echo it back as `KIND_STREAM_OUT`.
async fn stub_daemon(listener: UnixListener, server_version: u32) {
    let (stream, _) = listener.accept().await.expect("accept");
    let (mut rd, mut wr) = stream.into_split();

    // 1. Read the Hello REQ and assert the client advertised its version.
    let (kind, payload) = read_frame(&mut rd)
        .await
        .expect("read hello")
        .expect("hello frame, not EOF");
    assert_eq!(kind, KIND_REQ, "first frame must be a REQ");
    let req: AttachRequest = serde_json::from_slice(&payload).expect("decode hello");
    match req {
        AttachRequest::Hello { client_version, .. } => {
            assert_eq!(
                client_version, PROTOCOL_VERSION,
                "client_version on the wire"
            );
        }
        other => panic!("expected Hello, got {other:?}"),
    }

    // 2. Reply with our server version + identifying strings.
    let resp = AttachResponse::hello(server_version, "stub-build".into(), "0.0.0-test".into());
    let body = serde_json::to_vec(&resp).expect("encode resp");
    write_frame(&mut wr, KIND_RESP, &body)
        .await
        .expect("write resp");

    // 3. Bridge one frame each direction.
    let (k, p) = read_frame(&mut rd)
        .await
        .expect("read stream-in")
        .expect("stream-in frame, not EOF");
    assert_eq!(k, KIND_STREAM_IN, "client should have sent a STREAM_IN");
    write_frame(&mut wr, KIND_STREAM_OUT, &p)
        .await
        .expect("echo stream-out");
}

#[tokio::test]
async fn connect_handshake_and_bridge_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let listener = UnixListener::bind(&sock).expect("bind stub socket");
    let server = tokio::spawn(stub_daemon(listener, PROTOCOL_VERSION));

    // Connect + Hello negotiation succeeds and the negotiated version matches.
    let mut conn = connect_and_handshake(&sock, Some("gui-test".into()))
        .await
        .expect("handshake should succeed");
    assert_eq!(conn.protocol_version, PROTOCOL_VERSION);
    assert_eq!(conn.daemon_version.as_deref(), Some("0.0.0-test"));
    assert_eq!(conn.build_version.as_deref(), Some("stub-build"));

    // Bridge a frame each direction: send STREAM_IN, receive the echoed
    // STREAM_OUT.
    conn.send_frame(KIND_STREAM_IN, b"PING")
        .await
        .expect("send STREAM_IN");
    let frame = conn
        .next_frame()
        .await
        .expect("read bridged frame")
        .expect("a bridged frame, not EOF");
    assert_eq!(frame.kind, KIND_STREAM_OUT);
    assert_eq!(frame.payload, b"PING");

    server.await.expect("stub daemon task");
}

#[tokio::test]
async fn missing_socket_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("does-not-exist.sock");
    let err = connect_and_handshake(&sock, None)
        .await
        .expect_err("missing socket must fail");
    assert!(
        matches!(&err, ConnectError::SocketMissing(p) if p == &sock),
        "got {err:?}"
    );
}

#[tokio::test]
async fn version_mismatch_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let listener = UnixListener::bind(&sock).expect("bind stub socket");
    // Daemon reports a version one ahead of ours.
    let server = tokio::spawn(stub_daemon(listener, PROTOCOL_VERSION + 1));

    let err = connect_and_handshake(&sock, None)
        .await
        .expect_err("version mismatch must fail");
    assert!(
        matches!(
            err,
            ConnectError::VersionMismatch { local, remote }
                if local == PROTOCOL_VERSION && remote == Some(PROTOCOL_VERSION + 1)
        ),
        "expected VersionMismatch, got {err:?}"
    );

    // The stub returns after replying to Hello — it never reaches the bridge
    // step because we bail on the mismatch. Drop it.
    server.abort();
    let _ = server.await;
}

/// Sanity: the re-exported discovery resolves *some* attach socket path and is
/// callable from the GUI core's public API. We don't mutate the process env
/// here (that would race other tests in the shared `cargo test` process), so
/// we only assert it returns a non-empty path; the env/XDG/`/tmp`-per-uid
/// resolution is unit-tested in the `protocol` crate.
#[test]
fn attach_socket_path_is_exposed() {
    let p = dad_gui_core::attach_socket_path();
    assert!(
        !p.as_os_str().is_empty(),
        "attach_socket_path() returned an empty path"
    );
}
