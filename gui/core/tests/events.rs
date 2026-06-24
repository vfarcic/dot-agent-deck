//! Integration test for the GUI core's `SubscribeEvents` consumer.
//!
//! Stands up an in-process stub daemon on a temp Unix socket that speaks the
//! `SubscribeEvents` handshake (ack with an OK `RESP`) and then pushes one
//! `KIND_EVENT` frame carrying a `BroadcastMsg::Event`. Exercises the real
//! socket transport, frame codec, and event decoding without a real daemon —
//! fast enough for the `cargo test-fast` tier.

use dad_gui_core::{EventType, subscribe_events};
use protocol::{
    AgentEvent, AttachRequest, AttachResponse, BroadcastMsg, KIND_EVENT, KIND_REQ, KIND_RESP,
    read_frame, write_frame,
};
use tokio::net::UnixListener;

/// Accept one connection, assert the client subscribed, ack, then broadcast a
/// single `waiting_for_input` event for `agent-7`.
async fn stub_daemon(listener: UnixListener) {
    let (stream, _) = listener.accept().await.expect("accept");
    let (mut rd, mut wr) = stream.into_split();

    let (kind, payload) = read_frame(&mut rd)
        .await
        .expect("read subscribe")
        .expect("subscribe frame, not EOF");
    assert_eq!(kind, KIND_REQ, "first frame must be a REQ");
    let req: AttachRequest = serde_json::from_slice(&payload).expect("decode req");
    assert!(
        matches!(req, AttachRequest::SubscribeEvents),
        "expected SubscribeEvents, got {req:?}"
    );

    // Ack the subscription.
    let body = serde_json::to_vec(&AttachResponse::ok()).expect("encode resp");
    write_frame(&mut wr, KIND_RESP, &body)
        .await
        .expect("write resp");

    // Broadcast one hook event.
    let ev: AgentEvent = serde_json::from_str(
        r#"{
            "session_id": "sess-1",
            "agent_type": "claude_code",
            "event_type": "waiting_for_input",
            "timestamp": "2026-06-24T10:00:00Z",
            "agent_id": "agent-7"
        }"#,
    )
    .expect("build event");
    let body = serde_json::to_vec(&BroadcastMsg::Event(ev)).expect("encode event");
    write_frame(&mut wr, KIND_EVENT, &body)
        .await
        .expect("write event");
}

#[tokio::test]
async fn subscribe_receives_broadcast_event() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let listener = UnixListener::bind(&sock).expect("bind stub socket");
    let server = tokio::spawn(stub_daemon(listener));

    let mut stream = subscribe_events(&sock).await.expect("subscribe should ack");
    let ev = stream
        .next_event()
        .await
        .expect("read event")
        .expect("an event, not EOF");
    assert_eq!(ev.event_type, EventType::WaitingForInput);
    assert_eq!(ev.agent_id.as_deref(), Some("agent-7"));
    assert_eq!(ev.session_id, "sess-1");

    // After the stub closes, the stream reports a clean EOF rather than erroring.
    let after = stream.next_event().await.expect("clean read after close");
    assert!(after.is_none(), "stream should end with None on EOF");

    server.await.expect("stub daemon task");
}
