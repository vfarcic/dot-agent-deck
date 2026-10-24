#![cfg(all(feature = "e2e", unix))]

//! Attached-TUI draft regressions retargeted from typed task delivery to the
//! inbox doorbell. Synthetic panes spend no real-agent credential.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use common::TuiDeck;
use inbox::*;
use spec::spec;
use std::time::Duration;

fn launch() -> (TuiDeck, Pane) {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
        .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
        // The tests fetch the stand-in's mail from the test process, outside
        // its pane; inbox/protocol/014 and /015 pin the binding itself.
        .with_env("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let pane = pane(&deck, "draft-worker");
    status(&deck, &pane, "session_start", "pi");
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    deck.wait_for_string("DOORBELL-STANDIN-READY");
    (deck, pane)
}

fn draft(deck: &TuiDeck, pane: &Pane) {
    deck.send_keys(DRAFT.as_bytes());
    assert!(
        common::wait_until(WAIT, || bytes(pane) == DRAFT.as_bytes()),
        "attached TUI must forward the human draft: {}",
        deck.snapshot_grid()
    );
    deck.wait_for_string(DRAFT);
}

/// Scenario: Type a human draft through the attached TUI and queue a task. Enter must leave the draft and fixed doorbell as separate submitted lines visibly in the pane, without typing the task content.
#[spec("inbox/doorbell/009")]
#[test]
fn doorbell_009_attached_draft_enter_releases_fixed_line() {
    let (deck, pane) = launch();
    draft(&deck, &pane);
    let id = enqueue(&deck, &pane, BODY);
    unchanged(&pane, Duration::from_millis(700));
    assert!(rings(&pane).is_empty());
    deck.send_keys(b"\r");
    ring(&deck, &pane, &id);
    deck.wait_for_string(&format!("SUBMITTED:{}", fixed_line(&id)));
    assert_eq!(
        bytes(&pane),
        format!("{DRAFT}\r{}\r", fixed_line(&id)).as_bytes()
    );
    assert!(!deck.snapshot_grid().contains(BODY));
}

/// Scenario: Leave a human draft unsent for a 1500 ms observation. The doorbell must stay absent with the message Queued; Ctrl+U clears the draft and permits the fixed line without ever submitting the draft.
#[spec("inbox/doorbell/010")]
#[test]
fn doorbell_010_draft_has_no_timeout_cap() {
    let (deck, pane) = launch();
    draft(&deck, &pane);
    let id = enqueue(&deck, &pane, BODY);
    unchanged(&pane, Duration::from_millis(1500));
    assert_eq!(
        bytes(&pane),
        DRAFT.as_bytes(),
        "legacy cap must never force-submit a user draft"
    );
    state(&deck, &pane, &id, "queued");
    deck.send_keys(b"\x15");
    ring(&deck, &pane, &id);
    deck.wait_for_string(&format!("SUBMITTED:{}", fixed_line(&id)));
    assert_eq!(
        bytes(&pane),
        format!("{DRAFT}\x15{}\r", fixed_line(&id)).as_bytes()
    );
    assert!(!turns(&pane).iter().any(|turn| turn["line"] == DRAFT));
}

/// Scenario: Receive and fetch a control task in the attached TUI's explicitly profiled synthetic editor, then press Left without typing a draft and wait for the editor to redraw its empty composer. A second doorbell must arrive without another keypress, and fetching it must return the new task's nonce.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_attached_left_in_empty_editor_does_not_strand_task() {
    let (deck, pane) = launch_composer(true);
    let control = enqueue(&deck, &pane, "empty-editor-control-7e91");
    ring(&deck, &pane, &control);
    assert!(fetch(&deck, &pane, true).to_string().contains(&control));
    let settled = format!("{}\r\r", fixed_line(&control));
    assert!(
        common::wait_until(WAIT, || bytes(&pane) == settled.as_bytes()),
        "control cleanup must settle before navigation: {:?}",
        bytes(&pane)
    );
    let revision = composer_revision(&screen(&deck, &pane)).unwrap();
    deck.send_keys(b"\x1b[D");
    assert!(
        common::wait_until(WAIT, || bytes(&pane).ends_with(b"\x1b[D")),
        "Left must reach the attached pane"
    );
    assert!(
        common::wait_until(WAIT, || {
            let grid = screen(&deck, &pane);
            grid.contains("COMPOSER: <empty>")
                && composer_revision(&grid).is_some_and(|current| current > revision)
        }),
        "stand-in must consume navigation and draw the empty composer"
    );
    let body = "empty-editor-task-7e91";
    let id = enqueue(&deck, &pane, body);
    ring(&deck, &pane, &id);
    let fetched = fetch(&deck, &pane, true).to_string();
    assert!(fetched.contains(&id) && fetched.contains(body), "{fetched}");
}

#[cfg(target_os = "linux")]
fn composer_revision(grid: &str) -> Option<u64> {
    grid.lines()
        .find_map(|line| line.trim().strip_prefix("PROCESSED:")?.parse().ok())
}

#[cfg(target_os = "linux")]
fn launch_composer(profiled: bool) -> (TuiDeck, Pane) {
    use std::os::unix::fs::PermissionsExt;
    let python = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .expect("Linux synthetic composer requires python3 on PATH (no pip dependencies)");
    assert!(
        python.status.success(),
        "python3 preflight failed: {python:?}"
    );
    eprintln!(
        "synthetic composer interpreter: {}",
        String::from_utf8_lossy(&python.stdout).trim()
    );
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
        .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
        .with_env("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "2000")
        .with_env("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let binary = deck.home_dir().join("doorbell-composer");
    std::fs::write(&binary, include_str!("support/empty_composer.py")).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let base = deck.home_dir().join("composer-worker");
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
    let command =
        common::capability_export_command(&format!("exec '{}' '{}'", quote(&binary), quote(&base)));
    let pane = pane_with(
        &deck,
        "composer-worker",
        serde_json::json!({
            "command":command,
            "env":[["DOT_AGENT_DECK_TEST_COMPOSER_PROFILE", if profiled {"standin"} else {""}],
                   ["DOT_AGENT_DECK_TEST_COMPOSER_BINARY", binary]]
        }),
    );
    status(&deck, &pane, "session_start", "pi");
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    deck.wait_for_string("COMPOSER: <empty>");
    (deck, pane)
}

/// Scenario: Start the same empty synthetic editor without the explicit profile selector, then queue a task. The unrecognised editor must remain untouched and queued with zero deliveries and cannot_type, even after the accelerated stall threshold.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_unprofiled_composer_cannot_type() {
    let (deck, pane) = launch_composer(false);
    let id = enqueue(&deck, &pane, BODY);
    unchanged(&pane, Duration::from_secs(3));
    assert!(
        bytes(&pane).is_empty(),
        "unprofiled composer received {:?}",
        bytes(&pane)
    );
    let list = listing(&deck, &pane);
    let message = list["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap();
    assert_eq!(message["state"], "queued");
    assert_eq!(message["deliveries"], 0);
    let record = attach(&deck, serde_json::json!({"op":"list-agents"}))["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == pane.agent)
        .unwrap()
        .clone();
    assert!(
        record["inbox"]["wake_hold"]
            .to_string()
            .contains("cannot_type"),
        "unprofiled hold: {record}"
    );
}

/// Scenario: After a submitted doorbell, type a fresh human draft and fetch the task through another consumer. Cleanup must wait without submitting that draft; clearing it permits exactly one bare CR and no new doorbell.
#[spec("inbox/doorbell/011")]
#[test]
fn doorbell_011_cleanup_cr_respects_a_new_human_draft() {
    let (deck, pane) = launch();
    let id = enqueue(&deck, &pane, BODY);
    ring(&deck, &pane, &id);
    deck.send_keys(DRAFT.as_bytes());
    deck.wait_for_string(DRAFT);
    assert!(common::wait_until(WAIT, || bytes(&pane).ends_with(DRAFT.as_bytes())));
    fetch(&deck, &pane, true);
    state(&deck, &pane, &id, "acknowledged");
    unchanged(&pane, Duration::from_millis(700));
    assert!(
        bytes(&pane).ends_with(DRAFT.as_bytes()),
        "cleanup submitted the human draft"
    );
    deck.send_keys(b"\x15");
    let expected = format!("{}\r{DRAFT}\x15\r", fixed_line(&id));
    assert!(
        common::wait_until(WAIT, || bytes(&pane) == expected.as_bytes()),
        "clearing draft must release one cleanup CR: {:?}",
        bytes(&pane)
    );
    unchanged(&pane, Duration::from_millis(400));
    assert_eq!(rings(&pane).len(), 1);
    assert!(!turns(&pane).iter().any(|turn| turn["line"] == DRAFT));
}

/// Scenario: Hold two queued reports behind a draft in an attached pane. Clearing the draft must produce one coalesced fixed line; fetching must return both reports in arrival order with no report content typed into the pane.
#[spec("inbox/doorbell/012")]
#[test]
fn doorbell_012_drafted_batch_fetch_preserves_arrival_order() {
    let (deck, pane) = launch();
    draft(&deck, &pane);
    let first = enqueue(&deck, &pane, "first-worker-report-1666");
    let second = enqueue(&deck, &pane, "second-worker-report-1666");
    unchanged(&pane, Duration::from_millis(700));
    deck.send_keys(b"\x15");
    assert!(
        common::wait_until(WAIT, || !rings(&pane).is_empty()),
        "draft clear never released batch"
    );
    let reply = fetch(&deck, &pane, true).to_string();
    assert!(
        reply.find("first-worker-report-1666").unwrap()
            < reply.find("second-worker-report-1666").unwrap(),
        "fetch must preserve report arrival order: {reply}"
    );
    state(&deck, &pane, &first, "acknowledged");
    state(&deck, &pane, &second, "acknowledged");
    assert_eq!(rings(&pane).len(), 1);
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("first-worker-report-1666") && !grid.contains("second-worker-report-1666")
    );
}
