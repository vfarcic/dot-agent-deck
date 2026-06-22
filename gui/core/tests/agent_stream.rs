//! Integration tests for the M1.3 attach-stream + listing + resize path.
//!
//! Each test stands up an in-process *stub daemon* on a temp Unix socket that
//! speaks the shared `protocol` wire types — no real `dot-agent-deck` binary,
//! no webview — so the suite stays hermetic and fast enough for `test-fast`
//! while exercising the real socket transport, frame codec, and request shapes
//! the GUI sends. The deterministic single-slot coalescing semantics are
//! unit-tested inside `agent.rs`; these tests cover the wire round-trips.

use dad_gui_core::{attach_stream, list_agents, resize_channel, run_resize_worker};
use protocol::{
    AgentRecord, AgentType, AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, KIND_STREAM_IN,
    KIND_STREAM_OUT, read_frame, write_frame,
};
use tokio::net::UnixListener;

/// Read one `REQ` frame and decode it as an [`AttachRequest`].
async fn read_req(rd: &mut (impl tokio::io::AsyncRead + Unpin)) -> AttachRequest {
    let (kind, payload) = read_frame(rd)
        .await
        .expect("read req")
        .expect("a REQ frame, not EOF");
    assert_eq!(kind, KIND_REQ, "first frame must be a REQ");
    serde_json::from_slice(&payload).expect("decode request")
}

/// Stub daemon: reply to a `ListAgents` with two `agent_records`.
async fn stub_list(listener: UnixListener) {
    let (stream, _) = listener.accept().await.expect("accept");
    let (mut rd, mut wr) = stream.into_split();
    match read_req(&mut rd).await {
        AttachRequest::ListAgents => {}
        other => panic!("expected ListAgents, got {other:?}"),
    }
    let records = vec![
        AgentRecord {
            id: "1".into(),
            pane_id_env: Some("pid-1".into()),
            display_name: Some("coder".into()),
            cwd: Some("/work".into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            rows: 40,
            cols: 120,
        },
        AgentRecord {
            id: "2".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
        },
    ];
    let resp = AttachResponse::agent_records(records);
    let body = serde_json::to_vec(&resp).expect("encode resp");
    write_frame(&mut wr, KIND_RESP, &body)
        .await
        .expect("write resp");
}

#[tokio::test]
async fn list_agents_parses_records() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    let server = tokio::spawn(stub_list(listener));

    let records = list_agents(&sock).await.expect("list_agents");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].id, "1");
    assert_eq!(records[0].display_name.as_deref(), Some("coder"));
    assert_eq!(records[0].rows, 40);
    assert_eq!(records[0].cols, 120);
    assert_eq!(records[1].id, "2");
    assert!(records[1].display_name.is_none());

    server.await.expect("stub task");
}

/// Stub daemon: confirm an `AttachStream`, replay a snapshot as
/// `KIND_STREAM_OUT`, then read back one `KIND_STREAM_IN` and assert its bytes.
async fn stub_attach(listener: UnixListener) {
    let (stream, _) = listener.accept().await.expect("accept");
    let (mut rd, mut wr) = stream.into_split();
    match read_req(&mut rd).await {
        AttachRequest::AttachStream { id } => assert_eq!(id, "7"),
        other => panic!("expected AttachStream, got {other:?}"),
    }
    // Confirm the attach.
    let body = serde_json::to_vec(&AttachResponse::ok()).expect("encode");
    write_frame(&mut wr, KIND_RESP, &body)
        .await
        .expect("write resp");
    // Replay a "scrollback snapshot" the terminal paints on first frame.
    write_frame(&mut wr, KIND_STREAM_OUT, b"hello from agent\r\n")
        .await
        .expect("write stream-out");
    // Expect one keystroke chunk back, byte-exact.
    let (kind, payload) = read_frame(&mut rd)
        .await
        .expect("read stream-in")
        .expect("a STREAM_IN frame");
    assert_eq!(kind, KIND_STREAM_IN);
    assert_eq!(payload, b"echo hi\r");
}

#[tokio::test]
async fn attach_stream_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    let server = tokio::spawn(stub_attach(listener));

    let mut stream = attach_stream(&sock, "7").await.expect("attach_stream");

    // Snapshot bytes stream in as KIND_STREAM_OUT.
    let out = stream
        .next_output()
        .await
        .expect("read output")
        .expect("snapshot bytes, not EOF");
    assert_eq!(out, b"hello from agent\r\n");

    // Keystrokes go back byte-exact as KIND_STREAM_IN.
    stream.write_input(b"echo hi\r").await.expect("write input");

    server.await.expect("stub task");
}

/// Stub daemon: accept exactly one `Resize` connection and report the dims it
/// received back to the test over a channel.
async fn stub_resize(listener: UnixListener, tx: tokio::sync::oneshot::Sender<(u16, u16)>) {
    let (stream, _) = listener.accept().await.expect("accept");
    let (mut rd, mut wr) = stream.into_split();
    let (rows, cols) = match read_req(&mut rd).await {
        AttachRequest::Resize { id, rows, cols } => {
            assert_eq!(id, "9");
            (rows, cols)
        }
        other => panic!("expected Resize, got {other:?}"),
    };
    let body = serde_json::to_vec(&AttachResponse::ok()).expect("encode");
    write_frame(&mut wr, KIND_RESP, &body)
        .await
        .expect("write resp");
    let _ = tx.send((rows, cols));
}

#[tokio::test]
async fn resize_worker_forwards_to_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    let (tx, rx_dims) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(stub_resize(listener, tx));

    let (handle, rx) = resize_channel();
    let worker = tokio::spawn(run_resize_worker(rx, sock.clone(), "9".into()));

    // A single resize must reach the daemon with the exact (rows, cols).
    handle.resize(50, 200);

    let dims = tokio::time::timeout(std::time::Duration::from_secs(5), rx_dims)
        .await
        .expect("daemon should receive the resize")
        .expect("dims channel");
    assert_eq!(dims, (50, 200));

    // Dropping the handle ends the worker.
    drop(handle);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker).await;
    server.await.expect("stub task");
}
