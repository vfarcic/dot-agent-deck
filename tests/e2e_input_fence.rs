#![cfg(all(feature = "e2e", target_os = "linux"))]

//! S-15/S-16: real attach connections and a native raw-terminal recorder, no agent API.
mod common;
#[path = "support/input_fence.rs"]
mod fixture;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[rustfmt::skip]
#[path = "fixtures/released-answer-client/client.rs"]
mod released;

pub use dot_agent_deck::{daemon_protocol, question};

use fixture::{CAP, Fixture, REPLY};
use spec::spec;

fn released_hashes() {
    use sha2::{Digest, Sha256};
    let provenance: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "fixtures/released-answer-client/provenance.json"
    ))
    .unwrap();
    for (name, source) in [
        (
            "client.rs",
            include_str!("fixtures/released-answer-client/client.rs"),
        ),
        (
            "lease.rs",
            include_str!("fixtures/released-answer-client/lease.rs"),
        ),
    ] {
        let expected = provenance
            .iter()
            .find(|entry| entry["compiled_file"] == name)
            .unwrap();
        assert_eq!(
            Sha256::digest(source.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            expected["sha256"].as_str().unwrap(),
            "frozen v0.49.2 {name} changed"
        );
    }
}

fn ask_recorder(f: &Fixture, id: &str) -> question::PendingQuestion {
    let q = serde_json::json!({
        "id": id, "kind": "choice", "channel": "keys", "raised_at_ms": 1,
        "questions": [{"prompt": "Which colour?", "multi_select": false,
            "options": [{"index": 1, "label": "Green", "role": "choice"},
                        {"index": 2, "label": "Blue", "role": "choice"}]}],
    });
    assert!(
        serde_json::from_value::<question::PendingQuestion>(q.clone())
            .unwrap()
            .sanitized()
            .is_some(),
        "the synthetic question must survive production validation"
    );
    f.hook_event("permission_request", Some(&q));
    assert!(
        common::wait_until(common::child_boot_budget(), || f
            .live()
            .is_some_and(|s| s.pending_question.is_some_and(|q| q.id == id))),
        "question hook did not reach the live snapshot: {:?}",
        f.live()
    );
    f.live().unwrap().pending_question.unwrap()
}

fn hook_during_answer(f: &Fixture) {
    let tool_name = f.hook_event("tool_start", None);
    assert!(
        common::wait_until(common::child_boot_budget(), || f
            .live()
            .is_some_and(|s| s.active_tool.is_some_and(|t| t.name == tool_name))),
        "a hook was blocked by the answer's lifecycle lock during the hold"
    );
    assert!(
        f.live().unwrap().pending_question.is_some(),
        "status hook must leave the question pending"
    );
}

/// Scenario: A frozen v0.49.2 voice client answers the recorder's keyboard question with no fence. The released request completes as answered, its one digit reaches the terminal, and neither a client token nor a settlement entry appears; this controls the setup used by the held-answer cases.
#[spec("question/answer/023")]
#[test]
fn question_answer_023_released_client_open_control() {
    use std::sync::{Arc, Mutex};
    released_hashes();
    let f = Fixture::questions();
    let _attached = f.connect();
    f.hook_event("session_start", None);
    let q = ask_recorder(&f, "q-open-control");
    let client = released::Client {
        socket: f.answer_socket(),
        sent: Arc::new(Mutex::new(Vec::new())),
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    assert_eq!(
        runtime.block_on(client.voice_answer(f.agent_id(), &q.id, q.revision)),
        released::lease::Leased::Sent(dot_agent_deck::daemon_client::AnswerReport::Answered)
    );
    f.expect_input(b"1");
    f.expect_stable_input(b"1");
    assert!(f.live().unwrap().answer_settlements.is_empty());
    assert!(client.sent.lock().unwrap()[0].get("client_token").is_none());
}

/// Scenario: A frozen v0.49.2 voice client answers a synthetic keyboard question while the real daemon fences its input; status hooks still arrive and a repeat is refused immediately. Its real 15-second timeout displays the released unconfirmed warning, then release writes the answer exactly once without creating a token settlement.
#[spec("question/answer/023")]
#[test]
fn question_answer_023_released_voice_timeout_then_one_write() {
    use released::lease::Leased;
    use std::sync::{Arc, Mutex};
    released_hashes();
    let f = Fixture::questions();
    let _attached = f.connect();
    f.hook_event("session_start", None);
    let q = ask_recorder(&f, "q-old-client");
    f.fence();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = Arc::new(released::Client {
        socket: f.answer_socket(),
        sent: Arc::new(Mutex::new(Vec::new())),
    });
    let answer = {
        let client = client.clone();
        let id = f.agent_id().to_string();
        let q = q.clone();
        runtime.spawn(async move { client.voice_answer(&id, &q.id, q.revision).await })
    };
    f.wait_held(1, 0);
    f.expect_stable_input(b"");
    hook_during_answer(&f);
    let repeat = runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.voice_answer(f.agent_id(), &q.id, q.revision),
            )
            .await
        })
        .expect("repeat must refuse instead of queueing behind the first answer");
    assert_eq!(
        repeat,
        Leased::Sent(dot_agent_deck::daemon_client::AnswerReport::Refused(
            question::AnswerRefusal::AnswerInProgress
        ))
    );
    assert_eq!(
        released::progress_words("tester").1,
        "Another answer to tester is still being sent — nothing was sent."
    );
    let timed_out = runtime.block_on(answer).unwrap();
    assert_eq!(
        timed_out,
        Leased::Unconfirmed("the deck did not answer in time".into())
    );
    let report = released::timeout_outcome(timed_out, "tester");
    assert_eq!(report.kind, "unconfirmed");
    assert_eq!(
        report.sentence,
        "The answer may have been sent, but tester did not confirm it — check tester before answering again."
    );
    hook_during_answer(&f);
    assert!(f.live().unwrap().answer_settlements.is_empty());
    f.expect_stable_input(b"");
    f.release();
    f.expect_input(b"1");
    f.expect_stable_input(b"1");
    assert!(
        f.live().unwrap().answer_settlements.is_empty(),
        "a tokenless released client must not get a settlement entry"
    );
    let next = ask_recorder(&f, "q-after-timeout");
    assert_eq!(
        runtime.block_on(client.voice_answer(f.agent_id(), &next.id, next.revision)),
        Leased::Sent(dot_agent_deck::daemon_client::AnswerReport::Answered),
        "settling the timed-out consumer must release its answer reservation"
    );
    f.expect_input(b"11");
    f.expect_stable_input(b"11");
    assert!(f.live().unwrap().answer_settlements.is_empty());
    for request in client.sent.lock().unwrap().iter() {
        assert!(
            request.get("client_token").is_none(),
            "released request gained a token: {request}"
        );
    }
}

/// Scenario: A released voice answer waits behind a fence when a new question supersedes it, or the recorder exits and is replaced in the same pane. Releasing the old acceptance must type nothing into either recorder and must not create a token settlement; the successor's own fresh question remains answerable once.
#[spec("question/answer/023")]
#[test]
fn question_answer_023_released_answer_superseded_or_retired() {
    use released::lease::Leased;
    use std::sync::{Arc, Mutex};
    for retire in [false, true] {
        let mut f = Fixture::questions();
        let _attached = f.connect();
        f.hook_event("session_start", None);
        let q = ask_recorder(&f, "q-before");
        f.fence();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let client = Arc::new(released::Client {
            socket: f.answer_socket(),
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let answer = {
            let client = client.clone();
            let id = f.agent_id().to_string();
            runtime.spawn(async move { client.voice_answer(&id, &q.id, q.revision).await })
        };
        f.wait_held(1, 0);
        hook_during_answer(&f);
        let next = if retire {
            f.command("exit");
            f.wait_exit();
            f.replace();
            f.hook_event("session_start", None);
            let next = ask_recorder(&f, "q-successor");
            assert_ne!(
                f.old_release()["ok"],
                true,
                "a retired generation cannot release its successor"
            );
            assert!(f.previous_input().is_empty());
            next
        } else {
            let next = ask_recorder(&f, "q-replacement");
            f.release();
            next
        };
        let outcome = runtime.block_on(answer).unwrap();
        assert!(
            !matches!(
                outcome,
                Leased::Sent(dot_agent_deck::daemon_client::AnswerReport::Answered)
            ),
            "superseded/retired accepted answer must not report a write: {outcome:?}"
        );
        f.expect_stable_input(b"");
        assert!(f.live().unwrap().answer_settlements.is_empty());
        assert_eq!(
            runtime.block_on(client.voice_answer(f.agent_id(), &next.id, next.revision)),
            Leased::Sent(dot_agent_deck::daemon_client::AnswerReport::Answered)
        );
        f.expect_input(b"1");
        f.expect_stable_input(b"1");
        assert!(f.live().unwrap().answer_settlements.is_empty());
    }
}

/// Scenario: Attach to a raw-terminal recorder without opening a fence, send empty frames, split escape sequences, OSC replies, Ctrl+G encodings and a large paste, then detach and reattach. A resize must reach the terminal and the recorder must receive exactly the original bytes in order, once.
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
    // No installed probe: even candidates in split reports, key-release
    // forms and a paste must keep their original bytes without a hold.
    for bytes in [
        b"\x1b]10;rgb:1/2/3\x07".as_slice(),
        b"\x1b]11;rgb:a/b/c\x1b\\",
        b"\x1b[103;5u\x1b[71;5:3u\x1b[27;5;103~",
        b"\x1b[200~\x07\x1b[103;5u\x1b[201~",
    ] {
        for byte in bytes {
            first.send(&[*byte]);
        }
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

/// Scenario: Exit a fenced recorder with a full paste held and another frame waiting for capacity, then replace it in the same pane to retire its generation. Both old connections must finish or receive an honest refusal, no held or waiting byte may reach either terminal, and the replacement must accept its own fresh input.
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
    f.wait_exit();
    f.replace();
    held.wait_end();
    waiting.wait_end();
    let refused = f.old_release();
    assert_ne!(refused["ok"], true, "old generation release: {refused}");
    let mut successor = f.connect();
    successor.send(b"fresh-successor|");
    f.expect_input(b"fresh-successor|");
    f.expect_stable_input(b"fresh-successor|");
    assert_eq!(f.previous_input(), b"");
}

// tester: S-20 begin
/// Scenario: Hold typing and a tokened keyboard answer in a full fence while a third attach connection waits for capacity, then retire and replace the recorder. Each old connection must finish, the answer must return without claiming a write, no old byte or settlement may reach the successor, and the successor must accept only its own fresh keys.
#[spec("inbox/probe/036")]
#[test]
fn probe_036_retirement_settles_typing_answer_and_full_waiter_once() {
    let mut f = Fixture::questions();
    let mut typing = f.connect();
    let mut waiting = f.connect();
    f.hook_event("session_start", None);
    let q = ask_recorder(&f, "retirement-held-answer");
    f.fence();
    let socket = f.answer_socket();
    let id = f.agent_id().to_owned();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let answer = runtime.spawn(async move {
        dot_agent_deck::daemon_client::DaemonClient::new(socket)
            .answer_question_with_token_while(
                || true,
                &id,
                &q.id,
                q.revision,
                vec![question::QuestionAnswer {
                    question_index: 0,
                    option_indices: vec![1],
                    text: None,
                }],
                false,
                Some(3636),
            )
            .await
    });
    f.wait_held(1, 0);
    typing.send(&vec![b'H'; CAP - 1]);
    f.wait_held(2, 0);
    waiting.send(b"FULL-WAITER-NEVER");
    f.wait_held(2, 1);
    f.expect_stable_input(b"");
    f.command("exit");
    f.wait_exit();
    f.replace();
    let reply = runtime
        .block_on(async { tokio::time::timeout(common::child_boot_budget(), answer).await })
        .expect("accepted answer must finish when its generation retires")
        .expect("one answer completion")
        .expect("retirement answer response");
    assert_ne!(
        reply,
        dot_agent_deck::daemon_client::AnswerReport::Answered,
        "retirement cannot report a written answer: {reply:?}"
    );
    assert!(
        f.live().is_none_or(|s| s.answer_settlements.is_empty()),
        "a predecessor settlement must not appear on the successor"
    );
    typing.wait_end();
    waiting.wait_end();
    assert_ne!(f.old_release()["ok"], true);
    assert!(f.previous_input().is_empty());
    let mut successor = f.connect();
    successor.send(b"SUCCESSOR-ONLY");
    f.expect_input(b"SUCCESSOR-ONLY");
    f.expect_stable_input(b"SUCCESSOR-ONLY");
}

/// Scenario: Accept a tokened keyboard answer behind a full fence, then disconnect the requesting client after acceptance. Releasing the hold must still write the accepted answer once and publish exactly one matching Written settlement, preserving the detached daemon task's ownership.
#[spec("question/answer/023")]
#[test]
fn question_answer_023_tokened_disconnect_keeps_accepted_answer_owned() {
    let f = Fixture::questions();
    let attached = f.connect();
    f.hook_event("session_start", None);
    let q = ask_recorder(&f, "accepted-disconnect");
    f.fence();
    let socket = f.answer_socket();
    let id = f.agent_id().to_owned();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let answer = runtime.spawn(async move {
        dot_agent_deck::daemon_client::DaemonClient::new(socket)
            .answer_question_with_token_while(
                || true,
                &id,
                &q.id,
                q.revision,
                vec![question::QuestionAnswer {
                    question_index: 0,
                    option_indices: vec![1],
                    text: None,
                }],
                false,
                Some(3623),
            )
            .await
    });
    f.wait_held(1, 0);
    answer.abort();
    assert!(runtime.block_on(answer).unwrap_err().is_cancelled());
    f.release();
    f.expect_input(b"1");
    f.expect_stable_input(b"1");
    assert!(
        common::wait_until(common::child_boot_budget(), || f.live().is_some_and(|s| s
            .answer_settlements
            .iter()
            .any(|s| s.client_token == 3623))),
        "accepted disconnected answer must settle after physical completion"
    );
    let own: Vec<_> = f
        .live()
        .unwrap()
        .answer_settlements
        .into_iter()
        .filter(|s| s.client_token == 3623)
        .collect();
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].outcome, question::SettlementOutcome::Written);
    drop(attached);
}

/// Scenario: Fill a fence with held typing, then raise a fresh keyboard question and answer it with a client token. The answer must refuse promptly with the input-full explanation, never enqueue a digit or claim a written settlement, and releasing the typing must preserve the original paste exactly.
#[spec("question/answer/023")]
#[test]
fn question_answer_023_a_full_fence_refuses_the_answer_without_a_digit() {
    let f = Fixture::questions();
    let mut human = f.connect();
    f.hook_event("session_start", None);
    f.fence();
    let paste = vec![b'P'; CAP];
    human.send(&paste);
    f.wait_held(1, 0);
    let q = ask_recorder(&f, "answer-full");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = dot_agent_deck::daemon_client::DaemonClient::new(f.answer_socket());
    let outcome = runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                client.answer_question_with_token_while(
                    || true,
                    f.agent_id(),
                    &q.id,
                    q.revision,
                    vec![question::QuestionAnswer {
                        question_index: 0,
                        option_indices: vec![1],
                        text: None,
                    }],
                    false,
                    Some(3624),
                ),
            )
            .await
        })
        .expect("a Full answer must refuse rather than park a client")
        .unwrap();
    match outcome {
        dot_agent_deck::daemon_client::AnswerReport::Refused(
            question::AnswerRefusal::WriteFailed { detail },
        ) => assert!(
            detail.contains("no room for the answer"),
            "honest Full refusal: {detail}"
        ),
        other => panic!("expected the answer-specific Full refusal, got {other:?}"),
    }
    f.wait_held(1, 0);
    f.expect_stable_input(b"");
    f.release();
    f.expect_input(&paste);
    f.expect_stable_input(&paste);
    assert!(
        !f.live()
            .unwrap()
            .answer_settlements
            .iter()
            .any(|s| s.client_token == 3624 && s.outcome == question::SettlementOutcome::Written)
    );
}

// tester: S-20 end

// S-16's oracle is OSC syntax plus the script's query history, not the
// production tokenizer or the paste-mode tracker's parity with vt100.
// It observes actual PTY input and the native sequence exposed to helpers.
// Q35-19's admission/FIFO-expiry assertions belong to S-19.
#[derive(Clone, Copy, Debug)]
enum History {
    Retained,
    Unsolicited,
    SaturatedRetained,
    SaturatedUnretained,
}

fn colour_query(slot: u8) -> Vec<u8> {
    format!("\x1b]{slot};?\x1b\\").into_bytes()
}

fn colour_reply(slot: u8, bel: bool) -> Vec<u8> {
    let mut bytes = format!("\x1b]{slot};rgb:1/22/333").into_bytes();
    bytes.extend_from_slice(if bel { b"\x07" } else { b"\x1b\\" });
    bytes
}

fn saturate(f: &Fixture, client: &fixture::Client) {
    // The contractual ledger cap is eight. These nine unanswered DA1
    // queries saturate it without introducing a CPR backlog or a BEL.
    f.emit(client, &b"\x1b[c".repeat(9));
}

fn history(f: &Fixture, client: &fixture::Client, slot: u8, history: History) {
    match history {
        History::Retained => f.emit(client, &colour_query(slot)),
        History::Unsolicited => {}
        History::SaturatedRetained => {
            f.emit(client, &colour_query(slot));
            saturate(f, client);
        }
        History::SaturatedUnretained => {
            saturate(f, client);
            f.emit(client, &colour_query(slot));
        }
    }
}

fn native_report_case(slot: u8, bel: bool, split: bool, prior: History, native: u64) {
    let f = Fixture::start(true);
    let mut client = f.connect();
    assert_eq!(f.native_seq(), 0, "fresh installed helper publication");
    history(&f, &client, slot, prior);
    let reply = colour_reply(slot, bel);
    if split {
        // The final BEL is its own op: a whole report exemption cannot be
        // reconstructed by concatenating previously accepted operations.
        for (offset, byte) in reply.iter().enumerate() {
            client.send(&[*byte]);
            f.expect_input(&reply[..=offset]);
            if offset + 1 < reply.len() {
                assert_eq!(f.native_seq(), 0, "no candidate before the terminator");
            }
        }
    } else {
        client.send(&reply);
        f.expect_input(&reply);
    }
    client.send(b"native-tail|");
    let expected = [&reply, b"native-tail|".as_slice()].concat();
    f.expect_input(&expected);
    f.expect_stable_input(&expected);
    assert_eq!(
        f.native_seq(),
        native,
        "helper-visible native sequence: slot={slot}, BEL={bel}, split={split}, history={prior:?}"
    );
}

/// Scenario: With no check open, answer retained OSC 10 and 11 queries in one frame, using BEL and ST terminators, both before and after the query ledger saturates. The raw recorder receives each reply and following typing unchanged, and its helper-visible native sequence stays zero because these whole retained replies contain no native Ctrl+G.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_whole_retained_reports_are_not_native_even_after_saturation() {
    for prior in [History::Retained, History::SaturatedRetained] {
        for slot in [10, 11] {
            for bel in [false, true] {
                native_report_case(slot, bel, false, prior, 0);
            }
        }
    }
}

/// Scenario: With no check open, send whole OSC 10 and 11 colour replies without a query, or after saturation dropped their query. ST leaves the native sequence unchanged, while a final BEL numbers one native candidate; in either case the reply and following typing reach the raw terminal once without a segment hold.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_unretained_bel_reports_number_one_native_candidate() {
    for prior in [History::Unsolicited, History::SaturatedUnretained] {
        for slot in [10, 11] {
            for bel in [false, true] {
                native_report_case(slot, bel, false, prior, u64::from(bel));
            }
        }
    }
}

/// Scenario: With no check open, send OSC 10 and 11 replies one byte per attach frame, to retained and dropped queries, with BEL and ST endings. The recorder receives every prefix and subsequent typing unchanged, and only the final BEL advances the helper-visible native sequence, once, because split replies are not whole solicited operations.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_split_bel_reports_number_one_native_candidate() {
    for prior in [History::Retained, History::SaturatedUnretained] {
        for slot in [10, 11] {
            for bel in [false, true] {
                native_report_case(slot, bel, true, prior, u64::from(bel));
            }
        }
    }
}

/// Scenario: Hold typing behind a check, then answer newly emitted OSC 10 and 11 queries with complete BEL- or ST-ended replies on that same connection. Each solicited reply must pass ahead of the typing without opening a new hold, and releasing the original check delivers the held typing once.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_whole_solicited_colour_reports_pass_ahead_during_a_check() {
    for slot in [10, 11] {
        for bel in [false, true] {
            let f = Fixture::start(true);
            let mut client = f.connect();
            f.check(&client);
            client.send(b"held-before|");
            f.wait_held(1, 0);
            f.emit(&client, &colour_query(slot));
            let reply = colour_reply(slot, bel);
            client.send(&reply);
            f.expect_input(&reply);
            client.send(b"held-after|");
            f.wait_held(2, 0);
            f.expect_stable_input(&reply);
            f.release();
            let expected = [&reply, b"held-before|held-after|".as_slice()].concat();
            f.expect_input(&expected);
            f.expect_stable_input(&expected);
            assert_eq!(f.native_seq(), 0, "solicited report is not a native key");
        }
    }
}

fn held_report_case(slot: u8, bel: bool, split: bool, prior: History, candidate: bool) {
    let mut f = Fixture::start(true);
    let mut client = f.connect();
    // Even a retained whole reply cannot pass ahead: its query precedes
    // the fence trigger. Saturation also removes pass-ahead permission.
    history(&f, &client, slot, prior);
    f.check(&client);
    let reply = colour_reply(slot, bel);
    let frames = if split {
        for (index, byte) in reply.iter().enumerate() {
            client.send(&[*byte]);
            f.wait_held(index as u64 + 1, 0);
        }
        reply.len() as u64
    } else {
        client.send(&reply);
        f.wait_held(1, 0);
        1
    };
    client.send(b"held-tail|");
    f.wait_held(frames + 1, 0);
    f.expect_stable_input(b"");
    assert_eq!(
        f.native_seq(),
        0,
        "acceptance alone writes no native candidate"
    );
    f.release();
    if candidate {
        f.expect_input(&reply);
        f.expect_stable_input(&reply);
        assert_eq!(
            f.native_seq(),
            0,
            "the check's candidate is HumanG, not native"
        );
        // No automatic completion exists for the stand-in. Retirement
        // refuses the held tail instead of manufacturing release authority.
        f.command("exit");
        f.wait_exit();
        f.replace();
        let mut successor = f.connect();
        successor.send(b"fresh-after-report|");
        f.expect_input(b"fresh-after-report|");
        f.expect_stable_input(b"fresh-after-report|");
        assert_eq!(f.previous_input(), reply);
    } else {
        let expected = [&reply, b"held-tail|".as_slice()].concat();
        f.expect_input(&expected);
        f.expect_stable_input(&expected);
        assert_eq!(
            f.native_seq(),
            0,
            "a report without a candidate stays non-native"
        );
    }
}

/// Scenario: During a check, accept whole and byte-split OSC 10 and 11 replies after the ledger saturates, covering queries retained before saturation and queries dropped afterwards. Release writes a whole retained reply or an ST-ended split without holding its tail; an unretained or split BEL opens a new hold, whose tail must remain absent through retirement and replacement.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_saturated_and_split_reports_preserve_the_check_cut() {
    for prior in [History::SaturatedRetained, History::SaturatedUnretained] {
        for slot in [10, 11] {
            for split in [false, true] {
                for bel in [false, true] {
                    let candidate = bel && (split || matches!(prior, History::SaturatedUnretained));
                    held_report_case(slot, bel, split, prior, candidate);
                }
            }
        }
    }
}

/// Scenario: Accept a whole retained OSC colour reply and its tail behind a check, then saturate the ledger before releasing the bytes to the PTY. Both terminators and colour slots must drain with the tail once, without a new hold or native number, because write-time saturation cannot change the acceptance-time classification.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_saturation_after_acceptance_does_not_reclassify_a_report() {
    for slot in [10, 11] {
        for bel in [false, true] {
            let f = Fixture::start(true);
            let mut client = f.connect();
            f.emit(&client, &colour_query(slot));
            f.check(&client);
            let reply = colour_reply(slot, bel);
            client.send(&reply);
            f.wait_held(1, 0);
            client.send(b"accepted-before-saturation|");
            f.wait_held(2, 0);
            f.expect_stable_input(b"");
            saturate(&f, &client);
            f.release();
            let expected = [&reply, b"accepted-before-saturation|".as_slice()].concat();
            f.expect_input(&expected);
            f.expect_stable_input(&expected);
            assert_eq!(
                f.native_seq(),
                0,
                "the physical tracker keeps the planner's report classification"
            );
        }
    }
}

/// Scenario: Accept an unsolicited BEL-ended OSC 10 or 11 reply and following typing behind a check, then emit its query before the write. Releasing the original check must write only through BEL and hold the tail: a later query cannot retroactively make that accepted candidate a solicited report.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_a_query_after_acceptance_does_not_remove_a_check_cut() {
    for slot in [10, 11] {
        let f = Fixture::start(true);
        let mut client = f.connect();
        f.check(&client);
        let reply = colour_reply(slot, true);
        client.send(&reply);
        f.wait_held(1, 0);
        client.send(b"must-remain-held|");
        f.wait_held(2, 0);
        f.expect_stable_input(b"");
        f.emit(&client, &colour_query(slot));
        f.release();
        f.expect_input(&reply);
        f.expect_stable_input(&reply);
        assert_eq!(f.native_seq(), 0, "a held HumanG is not a native candidate");
    }
}

/// Scenario: With a check open and an OSC colour query dropped by saturation, send a BEL-ended reply and a sentinel tail together in one attach frame. Releasing the original check must write exactly through BEL, keep the same frame's tail held, and discard that tail on replacement without leaking it into the successor.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_a_candidate_in_one_frame_holds_that_frames_tail() {
    for slot in [10, 11] {
        let mut f = Fixture::start(true);
        let mut client = f.connect();
        history(&f, &client, slot, History::SaturatedUnretained);
        f.check(&client);
        let reply = colour_reply(slot, true);
        client.send(&[&reply, b"same-frame-tail|".as_slice()].concat());
        f.wait_any_held();
        f.expect_stable_input(b"");
        f.release();
        f.expect_input(&reply);
        f.expect_stable_input(&reply);
        assert_eq!(f.native_seq(), 0, "the check cut is not a native key");
        f.command("exit");
        f.wait_exit();
        f.replace();
        let mut successor = f.connect();
        successor.send(b"new-frame|");
        f.expect_input(b"new-frame|");
        f.expect_stable_input(b"new-frame|");
        assert_eq!(f.previous_input(), reply);
    }
}
