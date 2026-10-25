#![cfg(all(feature = "e2e", unix))]

//! Attached-client tests of live inbox summaries, state-change broadcasts
//! and persisted deck identity. Synthetic panes spend no agent credential.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use dot_agent_deck::daemon_protocol::{KIND_EVENT, KIND_REQ, KIND_RESP, PROTOCOL_VERSION};
use dot_agent_deck::event::BroadcastMsg;
use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

fn summary(daemon: &impl Endpoint, pane: &Pane) -> Value {
    let reply = attach(daemon, json!({"op":"list-agents"}));
    assert_eq!(reply["ok"], true, "ListAgents: {reply}");
    let record = reply["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == pane.agent)
        .unwrap();
    assert!(
        record["inbox"].is_object(),
        "ListAgents must carry AgentRecord.inbox: {record}"
    );
    record["inbox"].clone()
}

fn pending(daemon: &impl Endpoint, pane: &Pane, queued: u64, fetched: u64, wake: &str) {
    let value = summary(daemon, pane);
    assert_eq!(value["queued"], queued, "queued count: {value}");
    assert_eq!(value["fetched"], fetched, "fetched count: {value}");
    assert_eq!(value["undeliverable"], 0, "undeliverable count: {value}");
    assert_eq!(value["wake"], wake, "current wake path: {value}");
}

/// Scenario: Queue and fetch work for one synthetic agent while another remains empty, then announce Pi's extension without a watcher. ListAgents must show doorbell fallback until Pi's receipt stream registers, native while it is registered, and doorbell again after EOF; acknowledgements and other panes' counts remain separate.
#[spec("inbox/status/003")]
#[test]
fn status_003_list_agents_reports_counts_and_armed_wake() {
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_TEST_PAUSE_WAITER_DELIVERY", "1")]);
    let worker = pane(&daemon, "status-worker");
    let control = pane(&daemon, "status-control");
    status(&daemon, &worker, "idle", "codex");
    status(&daemon, &control, "idle", "codex");
    let first = enqueue(&daemon, &worker, "status-first");
    fetch(&daemon, &worker, false);
    state(&daemon, &worker, &first, "fetched");
    enqueue(&daemon, &worker, "status-second");
    enqueue(&daemon, &worker, "status-third");
    pending(&daemon, &worker, 2, 1, "doorbell");
    pending(&daemon, &control, 0, 0, "doorbell");
    let mut ack = request(&worker, "ack");
    ack["delivery_id"] = json!(first);
    assert_eq!(hook(&daemon, ack)["accepted"], true);
    pending(&daemon, &worker, 2, 0, "doorbell");
    assert!(
        !summary(&daemon, &worker)["last_acked_at"].is_null(),
        "ack time must be reported"
    );
    let mut pi_announce = request(&worker, "inbox_announce");
    pi_announce["component"] = json!("pi_extension");
    pi_announce["component_version"] = json!("synthetic-pi");
    pi_announce["wake"] = json!("native");
    pi_announce["pid"] = json!(worker.pid);
    assert_eq!(hook(&daemon, pi_announce)["accepted"], true);
    pending(&daemon, &worker, 2, 0, "doorbell");
    assert_eq!(summary(&daemon, &worker)["component"], "pi_extension");
    // Pi's extension arms its receipt stream (PRD #1666).
    let wait = receipt_stream_as(&daemon, &worker, "pi_extension");
    pending(&daemon, &worker, 2, 0, "native");
    assert_eq!(summary(&daemon, &worker)["component"], "pi_extension");
    drop(wait);
    let listing_after = RefCell::new(Value::Null);
    assert!(
        common::wait_until(WAIT, || {
            *listing_after.borrow_mut() = summary(&daemon, &worker);
            listing_after.borrow()["wake"] == "doorbell"
        }),
        "the closed stream leaves the doorbell: {}",
        listing_after.borrow()
    );
    announce(&daemon, &worker, "turn_end");
    pending(&daemon, &worker, 2, 0, "turn_end");
}

fn frame(socket: &mut UnixStream) -> (u8, Value) {
    let mut header = [0; 5];
    socket.read_exact(&mut header).unwrap();
    let size = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(size < 1024 * 1024);
    let mut body = vec![0; size];
    socket.read_exact(&mut body).unwrap();
    (header[0], serde_json::from_slice(&body).unwrap())
}

fn subscribe(daemon: &impl Endpoint) -> UnixStream {
    let mut socket = UnixStream::connect(daemon.attach_socket()).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    let payload = serde_json::to_vec(&json!({"op":"subscribe-events"})).unwrap();
    socket.write_all(&[KIND_REQ]).unwrap();
    socket
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&payload).unwrap();
    let (kind, reply) = frame(&mut socket);
    assert_eq!(kind, KIND_RESP);
    assert_eq!(reply["ok"], true, "subscription barrier: {reply}");
    socket
}

fn transition(socket: &mut UnixStream, pane: &Pane, id: &str, expected: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        socket
            .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
            .unwrap();
        let mut first = [0];
        socket.read_exact(&mut first).unwrap_or_else(|error| {
            panic!("attached client must receive Inbox broadcast for {id} -> {expected}: {error}")
        });
        let mut size = [0; 4];
        socket.read_exact(&mut size).unwrap();
        let size = u32::from_be_bytes(size) as usize;
        assert!(size < 1024 * 1024);
        let mut payload = vec![0; size];
        socket.read_exact(&mut payload).unwrap();
        assert_eq!(first[0], KIND_EVENT);
        let event: Value = serde_json::from_slice(&payload).unwrap();
        if event["kind"] != "inbox" {
            continue;
        }
        let typed: BroadcastMsg = serde_json::from_value(event.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(typed).unwrap(),
            event,
            "Inbox must be a recognized BroadcastMsg variant"
        );
        if event["message_id"] != id {
            continue;
        }
        assert_eq!(event["pane_id"], pane.id);
        assert_eq!(event["subject"], "doorbell fixture");
        assert!(
            event["from_label"]
                .as_str()
                .is_some_and(|label| !label.is_empty()),
            "sender label: {event}"
        );
        assert_eq!(event["state"], expected, "broadcast ordering: {event}");
        return;
    }
}

/// Scenario: Subscribe an attached client before posting a message, then fetch it with an `inbox fetch` that prints nothing, so the deck takes it back, fetch it again and acknowledge it, and stop an agent with another queued message. The client must observe each Queued, Fetched, Queued, Fetched, Acknowledged and Undeliverable transition in order with the message identity, subject and sender label.
#[spec("inbox/status/004")]
#[test]
fn status_004_attached_client_receives_each_inbox_state_change() {
    let daemon = daemon_unbound(&[]);
    let worker = pane(&daemon, "broadcast-worker");
    let mut events = subscribe(&daemon);
    let id = enqueue(&daemon, &worker, "broadcast task");
    transition(&mut events, &worker, &id, "queued");
    fetch_not_handed(&daemon, &worker);
    transition(&mut events, &worker, &id, "fetched");
    transition(&mut events, &worker, &id, "queued");
    fetch(&daemon, &worker, false);
    transition(&mut events, &worker, &id, "fetched");
    let mut ack = request(&worker, "ack");
    ack["delivery_id"] = json!(id);
    assert_eq!(hook(&daemon, ack)["accepted"], true);
    transition(&mut events, &worker, &id, "acknowledged");
    let lost = enqueue(&daemon, &worker, "lost task");
    transition(&mut events, &worker, &lost, "queued");
    assert_eq!(
        attach(&daemon, json!({"op":"stop-agent","id":worker.agent}))["ok"],
        true
    );
    transition(&mut events, &worker, &lost, "undeliverable");
}

/// Scenario: Connect twice to the same daemon and request Hello, then post a message. Both replies must carry the same persisted deck_id, matching the deck token in that message's globally addressed id.
#[spec("inbox/status/005")]
#[test]
fn status_005_hello_names_the_inbox_deck() {
    let daemon = daemon_unbound(&[]);
    let worker = pane(&daemon, "hello-worker");
    let hello = || {
        attach(
            &daemon,
            json!({"op":"hello","client_version":PROTOCOL_VERSION}),
        )
    };
    let first = hello();
    assert_eq!(first["ok"], true, "Hello: {first}");
    let deck = first["deck_id"]
        .as_str()
        .unwrap_or_else(|| panic!("Hello must carry deck_id: {first}"));
    assert_eq!(deck.len(), 8, "deck id token: {deck}");
    assert_eq!(
        hello()["deck_id"],
        deck,
        "deck identity must be stable across clients"
    );
    let id = enqueue(&daemon, &worker, "hello identity");
    assert!(
        id.starts_with(&format!("m-{deck}-")),
        "Hello and inbox address must name the same deck: {id}"
    );
}
