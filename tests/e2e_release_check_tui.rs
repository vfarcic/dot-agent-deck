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

/// Scenario: Queue a message for the synthetic composer while its on-machine release check is paused, and wait for a completed policy tick before asserting no input. Focus the pane with Enter only after visibly confirming command mode, keep the check held through another policy tick, then release a pass and see delivery, acknowledgement and the cleared card in the same pane.
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
        ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
    ]);
    let policy_controls = daemon.home.join("ring-barriers");
    std::fs::create_dir_all(&policy_controls).unwrap();
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
    // This acknowledges a completed policy tick AND its spawned attempts,
    // then drains the recorder. A rendered running summary alone is weaker.
    clock_step(&daemon, &pane, 5_000);
    state(&daemon, &pane, &id, "queued");

    // Ctrl+D toggles modes, so only send it if the current frame isn't already
    // in command mode. Wait for that mode before Enter: in pane mode Enter
    // would be a human byte and would touch this no-observer stand-in.
    if !deck.snapshot_grid().contains("COMMAND") {
        deck.send_keys(b"\x04");
    }
    assert!(
        deck.wait_for_grid_string_within("COMMAND", WAIT),
        "Enter requires command mode:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"\r");
    assert!(
        deck.wait_for_grid_string_within("[Command Mode Ctrl+D]", WAIT),
        "Enter must focus the pane:\n{}",
        deck.snapshot_grid()
    );
    clock_step(&daemon, &pane, 10_000);
    state(&daemon, &pane, &id, "queued");

    // A single rename is the result barrier. The e2e-only seam must pass this
    // outcome into the real queue/admission path, not synthesize an inbox ACK
    // or change the TUI summary directly. It grants no production authority.
    let next = control.join("result.next");
    std::fs::write(&next, br#"{"state":"passed"}"#).unwrap();
    std::fs::rename(&next, control.join("result.json")).unwrap();
    // Keep policy time monotonic after the held decisions. This step permits
    // the expected write, so unlike clock_step it does not assert zero bytes.
    std::fs::write(policy_controls.join("clock-ms"), "60000").unwrap();
    assert!(
        common::wait_until(WAIT, || {
            std::fs::read_to_string(policy_controls.join("clock-observed"))
                .is_ok_and(|age| age == "60000")
        }),
        "the policy tick after pass must complete"
    );
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

/// Scenario: Offer a synthetic passing check outcome for a named pane whose composer has no stand-in profile. The driver must visibly refuse that fixture, leave its message queued through completed policy decisions and send no bytes, even after the result file appears.
#[spec("inbox/release-check/005")]
#[test]
fn release_check_005_driver_refuses_a_non_standin_pane() {
    let scratch = common::harness_tempdir().unwrap();
    let control = scratch.path().join("release-check-control");
    std::fs::create_dir(&control).unwrap();
    let target = "unprofiled-check-composer";
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
        ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
    ]);
    std::fs::create_dir_all(daemon.home.join("ring-barriers")).unwrap();
    // This is still our harmless Python composer, never a real agent. It has
    // no stand-in profile and must not gain one through the outcome driver.
    // It already cannot receive doorbells: refusal plus no input here does
    // not establish that the driver withheld record publication/authority.
    let pane = pane_with(
        &daemon,
        target,
        json!({"agent_type":"claude_code","profile":false}),
    );
    status(&daemon, &pane, "session_start", "claude_code");
    status(&daemon, &pane, "idle", "claude_code");
    let id = enqueue(&daemon, &pane, "NON-STANDIN-REFUSAL-SENTINEL");
    clock_step(&daemon, &pane, 5_000);
    assert!(
        common::wait_until(WAIT, || control.join("refused.json").exists()),
        "SC-10 driver must report refusal of a non-stand-in pane; hold={}",
        wake_hold(&daemon, &pane)
    );
    let refused: serde_json::Value =
        serde_json::from_slice(&std::fs::read(control.join("refused.json")).unwrap()).unwrap();
    assert_eq!(refused, json!({"reason":"not_standin","pane_id":target}));
    std::fs::write(control.join("result.json"), br#"{"state":"passed"}"#).unwrap();
    clock_step(&daemon, &pane, 10_000);
    state(&daemon, &pane, &id, "queued");
}

fn wake_hold(daemon: &impl Endpoint, pane: &inbox::Pane) -> serde_json::Value {
    attach(daemon, json!({"op":"list-agents"}))["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == pane.agent)
        .map(|record| record["inbox"]["wake_hold"].clone())
        .unwrap_or_default()
}
