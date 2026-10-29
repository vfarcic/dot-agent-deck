#![cfg(all(feature = "e2e", target_os = "linux"))]

//! S-15: real attach connections and a native raw-terminal recorder, no agent API.
mod common;
#[path = "support/input_fence.rs"]
mod fixture;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use fixture::{CAP, Fixture, REPLY};
use spec::spec;

/// Scenario: Attach to a raw-terminal recorder without opening a fence, send empty frames, split escape sequences and a large paste, then detach and reattach. A resize must reach the terminal and the recorder must receive exactly the original bytes in order, once.
#[spec("inbox/probe/015")]
#[test]
fn probe_015_open_attach_keeps_bytes_detach_resize_and_empty_frames() {
    if fixture::agent_process() {
        return;
    }
    let f = Fixture::start(false);
    let mut first = f.connect();
    let mut expected = Vec::new();
    for bytes in [
        b"".as_slice(),
        b"alpha",
        b"\x1b[20",
        b"0~paste\0\x07\x1b[201~",
        b"",
    ] {
        first.send(bytes);
        expected.extend_from_slice(bytes);
    }
    let large = vec![b'x'; CAP + 17];
    first.send(&large);
    expected.extend_from_slice(&large);
    f.expect_input(&expected);
    drop(first);
    let mut second = f.connect();
    f.resize(33, 109);
    assert_eq!(f.command("size")["size"], serde_json::json!([33, 109]));
    second.send(b"reattached\r");
    expected.extend_from_slice(b"reattached\r");
    f.query(&second);
    second.send(REPLY);
    expected.extend_from_slice(REPLY);
    f.expect_input(&expected);
    f.expect_stable_input(&expected);
}

/// Scenario: An unfenced recorder exits with an old attach connection still open, then a new recorder starts in the same pane. Input on the old connection may be refused or end with that stream, but only new-connection input may reach the replacement, whose capture must contain exactly its own sentinel.
#[spec("inbox/probe/015")]
#[test]
fn probe_015_open_replacement_refuses_the_predecessors_input() {
    let mut f = Fixture::start(false);
    let mut old = f.connect();
    old.send(b"predecessor");
    f.expect_input(b"predecessor");
    f.command("exit");
    f.wait_exit();
    f.replace();
    old.try_send(b"stale-generation");
    old.wait_end();
    let mut new = f.connect();
    new.send(b"successor-only");
    f.expect_input(b"successor-only");
    f.expect_stable_input(b"successor-only");
    assert_eq!(f.previous_input(), b"predecessor");
}

/// Scenario: Open a test fence and type from two attached clients, waiting for each handoff before typing the next frame. Nothing reaches the raw terminal while held; release must write both clients' bytes in that acceptance order.
#[spec("inbox/probe/023")]
#[test]
fn probe_023_two_clients_keep_acceptance_order() {
    let f = Fixture::start(true);
    let mut a = f.connect();
    let mut b = f.connect();
    f.fence();
    a.send(b"a-first|");
    f.wait_held(1, 0);
    b.send(b"b-second|");
    f.wait_held(2, 0);
    a.send(b"a-third|");
    f.wait_held(3, 0);
    f.expect_stable_input(b"");
    f.release();
    f.expect_input(b"a-first|b-second|a-third|");
    f.expect_stable_input(b"a-first|b-second|a-third|");
}

/// Scenario: Hold a client's typing, then have the recorder emit a device-attributes query and answer it on that same connection. The complete solicited reply must reach the terminal ahead of the held typing; releasing the fence then delivers that typing exactly once.
#[spec("inbox/probe/023")]
#[test]
fn probe_023_same_connection_can_reply_after_an_accepted_key() {
    let f = Fixture::start(true);
    let mut client = f.connect();
    f.fence();
    client.send(b"held-key|");
    f.wait_held(1, 0);
    f.query(&client);
    client.send(REPLY);
    f.expect_input(REPLY);
    f.release();
    f.expect_input(&[REPLY, b"held-key|"].concat());
    f.expect_stable_input(&[REPLY, b"held-key|"].concat());
}

/// Scenario: Fill the fence's byte budget from one connection and park its next frame as Full, then answer a newly emitted device-attributes query from a second connection. The second client's reply must reach the raw terminal before release; afterwards every held and waiting byte must follow it once in order.
#[spec("inbox/probe/023")]
#[test]
fn probe_023_a_full_producer_does_not_block_another_clients_reply() {
    let f = Fixture::start(true);
    let mut producer = f.connect();
    let mut replying = f.connect();
    f.fence();
    let payload = vec![b'A'; CAP];
    producer.send(&payload);
    f.wait_held(1, 0);
    producer.send(b"full-waiter|");
    f.wait_held(1, 1);
    f.query(&replying);
    replying.send(REPLY);
    f.expect_input(REPLY);
    f.release();
    let expected = [REPLY, &payload, b"full-waiter|"].concat();
    f.expect_input(&expected);
    f.expect_stable_input(&expected);
}

/// Scenario: Fill the fence and park another frame on the same connection that will answer a device-attributes query. Its reply must remain behind its own backpressure until explicit release, after which the recorder receives the held paste, waiting frame and reply in order without loss.
#[spec("inbox/probe/023")]
#[test]
fn probe_023_a_reply_behind_its_own_full_frame_waits_without_loss() {
    let f = Fixture::start(true);
    let mut client = f.connect();
    f.fence();
    let payload = vec![b'B'; CAP];
    client.send(&payload);
    f.wait_held(1, 0);
    client.send(b"waiting|");
    f.wait_held(1, 1);
    f.query(&client);
    client.send(REPLY);
    f.expect_stable_input(b"");
    f.release();
    let expected = [&payload, b"waiting|".as_slice(), REPLY].concat();
    f.expect_input(&expected);
    f.expect_stable_input(&expected);
}

/// Scenario: Disconnect after the fence accepts typing, reattach and resize the same recorder, and type again. Detaching and resizing must release no held byte; explicit release delivers both accepted frames once while the recorder has the new geometry.
#[spec("inbox/probe/023")]
#[test]
fn probe_023_detach_reattach_and_resize_preserve_held_input() {
    let f = Fixture::start(true);
    let mut first = f.connect();
    f.fence();
    first.send(b"before-detach|");
    f.wait_held(1, 0);
    drop(first);
    let mut second = f.connect();
    f.resize(31, 101);
    assert_eq!(f.command("size")["size"], serde_json::json!([31, 101]));
    second.send(b"after-reattach|");
    f.wait_held(2, 0);
    f.expect_stable_input(b"");
    f.release();
    f.expect_input(b"before-detach|after-reattach|");
    f.expect_stable_input(b"before-detach|after-reattach|");
}

/// Scenario: Retire a fenced recorder with a full paste held and another frame waiting for capacity, then replace it in the same pane. Both old connections must finish or receive an honest refusal, no held or waiting byte may reach either terminal, and the replacement must accept its own fresh input.
#[spec("inbox/probe/023")]
#[test]
fn probe_023_retirement_discards_held_and_full_waiting_bytes() {
    let mut f = Fixture::start(true);
    let mut held = f.connect();
    let mut waiting = f.connect();
    f.fence();
    held.send(&vec![b'R'; CAP]);
    f.wait_held(1, 0);
    waiting.send(b"retired-waiter|");
    f.wait_held(1, 1);
    f.expect_stable_input(b"");
    f.command("exit");
    held.wait_end();
    waiting.wait_end();
    f.replace();
    let refused = f.old_release();
    assert_ne!(refused["ok"], true, "old generation release: {refused}");
    let mut successor = f.connect();
    successor.send(b"fresh-successor|");
    f.expect_input(b"fresh-successor|");
    f.expect_stable_input(b"fresh-successor|");
    assert_eq!(f.previous_input(), b"");
}
