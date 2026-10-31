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

fn launch_with(extra: &[(&str, &str)]) -> (TuiDeck, Pane) {
    let mut builder = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
        // The tests fetch the stand-in's mail from the test process, outside
        // its pane; inbox/protocol/014 and /015 pin the binding itself.
        .with_env("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1");
    for (key, value) in extra {
        builder = builder.with_env(*key, *value);
    }
    let deck = builder.launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let pane = pane(&deck, "draft-worker");
    status(&deck, &pane, "session_start", "pi");
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    deck.wait_for_string("DOORBELL-STANDIN-READY");
    (deck, pane)
}

/// Scenario: Through the attached TUI, the user types while the deck's re-ring lands, their keys reaching the deck after its line and before its Enter. The deck submits its own line alone with one Enter, and the user's text then sits unsubmitted in the composer, exactly as typed.
#[spec("inbox/doorbell/039")]
#[test]
fn doorbell_039_typing_while_the_line_lands_keeps_the_users_text_apart() {
    let (deck, pane) = launch_with(&[
        ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "400"),
        ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE", "draft-worker"),
        ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
    ]);
    let root = deck.home_dir().join("ring-barriers");
    std::fs::create_dir_all(&root).unwrap();
    let _budget = InterleaveBudget::start(&root);
    std::fs::write(root.join("after-line.armed"), "").unwrap();
    let id = enqueue(&deck, &pane, BODY);
    ring(&deck, &pane, &id);
    let first = format!("{}\r", rung_line(&pane, &id).unwrap());
    assert!(
        common::wait_until(wait_budget(), || root.join("after-line.reached").exists()),
        "the re-ring must reach its line"
    );
    let retry = || {
        String::from_utf8_lossy(&bytes(&pane))
            .strip_prefix(first.as_str())
            .filter(|rest| is_ring_line(rest, &id))
            .map(str::to_string)
    };
    assert!(
        common::wait_until(wait_budget(), || retry().is_some()),
        "{:?}",
        String::from_utf8_lossy(&bytes(&pane))
    );
    let line = retry().unwrap();
    deck.send_keys(DRAFT.as_bytes());
    assert!(
        common::wait_until(wait_budget(), || std::fs::read_to_string(
            root.join("observations")
        )
        .unwrap_or_default()
        .contains("human-arrived draft-worker")),
        "the user's keys must arrive while the deck holds the writer"
    );
    std::fs::write(root.join("completed.armed"), "").unwrap();
    std::fs::write(root.join("after-line.release"), "").unwrap();
    let expected = format!("{first}{line}\r{DRAFT}");
    assert!(
        common::wait_until(wait_budget(), || bytes(&pane) == expected.as_bytes()),
        "{:?}",
        String::from_utf8_lossy(&bytes(&pane))
    );
    assert!(
        common::wait_until(wait_budget(), || screen(&deck, &pane)
            .contains(&format!("COMPOSER: {DRAFT}"))),
        "the user's text sits in the composer"
    );
    assert_eq!(
        ring_lines(&pane).len(),
        2,
        "each ring submitted its own line"
    );
    assert!(!turns(&pane).iter().any(|turn| {
        turn["line"]
            .as_str()
            .is_some_and(|text| text.contains(DRAFT))
    }));
    std::fs::write(root.join("completed.release"), "").unwrap();
}

#[cfg(target_os = "linux")]
fn launch_composer(profiled: bool) -> (TuiDeck, Pane) {
    launch_composer_as(if profiled { "standin" } else { "" }, Binary::Own)
}

/// What the pane's `DOT_AGENT_DECK_TEST_COMPOSER_BINARY` names.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum Binary {
    /// The composer the pane runs.
    Own,
    /// Nothing: the variable is not set.
    Missing,
    /// Another executable copy of the composer, which the pane does not run.
    Other,
}

#[cfg(target_os = "linux")]
fn launch_composer_as(selector: &str, named: Binary) -> (TuiDeck, Pane) {
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
    std::fs::write(
        deck.home_dir().join("composer_external_edit.py"),
        include_str!("support/composer_external_edit.py"),
    )
    .unwrap();
    std::fs::write(&binary, include_str!("support/empty_composer.py")).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let other = deck.home_dir().join("other-composer");
    std::fs::write(&other, include_str!("support/empty_composer.py")).unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut env = vec![serde_json::json!([
        "DOT_AGENT_DECK_TEST_COMPOSER_PROFILE",
        selector
    ])];
    match named {
        Binary::Own => env.push(serde_json::json!([
            "DOT_AGENT_DECK_TEST_COMPOSER_BINARY",
            binary
        ])),
        Binary::Other => env.push(serde_json::json!([
            "DOT_AGENT_DECK_TEST_COMPOSER_BINARY",
            other
        ])),
        Binary::Missing => {}
    }
    let base = deck.home_dir().join("composer-worker");
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
    let command =
        common::capability_export_command(&format!("exec '{}' '{}'", quote(&binary), quote(&base)));
    let pane = pane_with(
        &deck,
        "composer-worker",
        serde_json::json!({"command":command, "env":env}),
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
    assert_cannot_type(&deck, &pane);
}

/// Scenario: Start the empty synthetic editor with the profile selector but no executable path, then queue a task. Without the path the profile is not selected, so the editor must remain untouched and queued with zero deliveries and cannot_type.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_a_selector_without_its_executable_path_cannot_type() {
    let (deck, pane) = launch_composer_as("standin", Binary::Missing);
    assert_cannot_type(&deck, &pane);
}

/// Scenario: Start the empty synthetic editor with the profile selector and the path of another executable copy of it, which the pane does not run, then queue a task. A path the pane does not run selects nothing, so the editor must remain untouched and queued with zero deliveries and cannot_type.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_a_selector_naming_another_executable_cannot_type() {
    let (deck, pane) = launch_composer_as("standin", Binary::Other);
    assert_cannot_type(&deck, &pane);
}

/// The queued task is never typed into the pane: zero bytes past the stall
/// threshold, the message queued with no delivery, and `cannot_type` held.
#[cfg(target_os = "linux")]
fn assert_cannot_type(deck: &TuiDeck, pane: &Pane) {
    let id = enqueue(deck, pane, BODY);
    unchanged(pane, Duration::from_secs(3));
    assert!(
        bytes(pane).is_empty(),
        "unprofiled composer received {:?}",
        bytes(pane)
    );
    let list = listing(deck, pane);
    let message = list["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap();
    assert_eq!(message["state"], "queued");
    assert_eq!(message["deliveries"], 0);
    let record = attach(deck, serde_json::json!({"op":"list-agents"}))["agent_records"]
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
