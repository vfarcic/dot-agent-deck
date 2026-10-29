#![cfg(all(feature = "e2e", target_os = "linux"))]

//! S-15/S-16: real attach connections and a native raw-terminal recorder, no agent API.
mod common;
#[path = "support/input_fence.rs"]
mod fixture;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use fixture::{CAP, Fixture, REPLY};
use spec::spec;

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
