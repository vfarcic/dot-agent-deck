#![cfg(all(feature = "e2e", unix))]

//! Attached-TUI self-check regression with the existing synthetic composer.
//! The e2e-only outcome driver is specified in fixtures/release-check/tui.md.
//! No runner or real agent is started by this scenario.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::*;
use serde_json::json;
use spec::spec;

/// Scenario: Queue a message for the synthetic composer while its on-machine release check is paused. The real PTY-attached TUI must show checking and the pane must receive no input; release a passing result, then see the submitted notification in that same pane, take the message through the stand-in integration and see the waiting notice disappear from its card.
#[spec("inbox/release-check/005")]
#[test]
fn release_check_005_checking_card_then_delivery_without_respawn() {
    let scratch = common::harness_tempdir().unwrap();
    let control = scratch.path().join("release-check-control");
    std::fs::create_dir(&control).unwrap();
    let target = "self-check-composer";
    std::fs::write(
        control.join("scenario.json"),
        serde_json::to_vec(&json!({
            "schema":1,"pane_id":target,"profile":"standin","agent":"claude",
            "release":"2.1.313","identity":"0123456789abcdef",
            "progress":"running","queue_position":1
        }))
        .unwrap(),
    )
    .unwrap();
    let daemon = daemon_unbound(&[
        (
            "DOT_AGENT_DECK_E2E_RELEASE_CHECK_CONTROL",
            control.to_str().unwrap(),
        ),
        ("DOT_AGENT_DECK_RELEASE_CHECK", "auto"),
    ]);
    let pane = pane_with(&daemon, target, json!({"agent_type":"claude_code"}));
    let deck = common::TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy(),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string(target);
    status(&daemon, &pane, "session_start", "claude_code");
    status(&daemon, &pane, "idle", "claude_code");
    let body = "SELF-CHECK-DELIVERY-SENTINEL-c540d371";
    let id = enqueue(&daemon, &pane, body);

    let checking = "waiting: checking Claude Code 2.1.313 on this machine.";
    assert!(
        deck.wait_for_grid_string_within(checking, WAIT),
        "SC-10: the waiting card must show checking before its result; received bytes={:?}; grid:\n{}",
        bytes(&pane),
        deck.snapshot_grid()
    );
    // A drain barrier acknowledges the fixture's input inventory, without a
    // sleep-shaped observation window. Passing is still held at result.json.
    drained(&pane);
    assert!(
        bytes(&pane).is_empty(),
        "a checking release received automatic input"
    );
    state(&daemon, &pane, &id, "queued");

    // A single rename is the result barrier. The e2e-only seam must pass this
    // outcome into the real queue/admission path, not synthesize an inbox ACK
    // or change the TUI summary directly. It grants no production authority.
    let next = control.join("result.next");
    std::fs::write(&next, br#"{"state":"passed"}"#).unwrap();
    std::fs::rename(&next, control.join("result.json")).unwrap();
    deck.send_keys(b"\r");
    assert!(
        deck.wait_for_grid_predicate_within(WAIT, |grid| grid.lines().any(|line| {
            line.contains("SUBMITTED:") && line.contains(&format!("message {id} waiting"))
        })),
        "the same synthetic pane must visibly receive the notification after pass:\n{}",
        deck.snapshot_grid()
    );
    ring(&daemon, &pane, &id);
    assert_eq!(rings(&pane).len(), 1, "one submitted notification");
    // This is the synthetic integration's fetch, after submission, not an
    // agent's receipt. A token appearing is insufficient to prove delivery.
    let delivered = fetch(&daemon, &pane, true);
    assert!(
        delivered.to_string().contains(body),
        "the queued body was not delivered: {delivered}"
    );
    state(&daemon, &pane, &id, "acknowledged");
    assert_eq!(
        std::fs::read_to_string(daemon.home.join(target).with_extension("pid"))
            .unwrap()
            .parse::<u32>()
            .unwrap(),
        pane.pid,
        "the original composer survives admission and delivery"
    );
    deck.send_keys(b"\x04");
    assert!(
        deck.wait_for_grid_predicate_within(WAIT, |grid| {
            grid.contains(target) && !grid.contains(checking) && !grid.contains("✉1 waiting")
        }),
        "delivery must clear the same card's waiting line:\n{}",
        deck.snapshot_grid()
    );
}
