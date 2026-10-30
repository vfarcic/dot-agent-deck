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
    launch_with(&[("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")])
}

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

/// The head of the submitted row the attached TUI shows for the ring
/// `line` naming `id`: its token and the id, which fit the pane's width
/// where the whole token-led line may wrap.
fn shown_submitted(line: &str, id: &str) -> String {
    format!("SUBMITTED:{}{PREFIX}{id}", &line[..8])
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

/// Scenario: Type a human draft through the attached TUI and queue a task. Enter must leave the draft and the token-led doorbell as separate submitted lines visibly in the pane, without typing the task content.
#[spec("inbox/doorbell/009")]
#[test]
fn doorbell_009_attached_draft_enter_releases_the_doorbell() {
    let (deck, pane) = launch();
    draft(&deck, &pane);
    let id = enqueue(&deck, &pane, BODY);
    unchanged(&pane, Duration::from_millis(700));
    assert!(rings(&pane).is_empty());
    deck.send_keys(b"\r");
    ring(&deck, &pane, &id);
    let line = rung_line(&pane, &id).unwrap();
    deck.wait_for_string(&shown_submitted(&line, &id));
    assert_eq!(bytes(&pane), format!("{DRAFT}\r{line}\r").as_bytes());
    assert!(!deck.snapshot_grid().contains(BODY));
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

/// Scenario: Leave a human draft unsent for a 1500 ms observation. The doorbell must stay absent with the message Queued; Ctrl+U clears the draft and permits the token-led line without ever submitting the draft.
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
    let line = rung_line(&pane, &id).unwrap();
    deck.wait_for_string(&shown_submitted(&line, &id));
    assert_eq!(bytes(&pane), format!("{DRAFT}\x15{line}\r").as_bytes());
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
    // PRD #1666 part 3, round 5-wake: a submitted ring owes no cleanup CR,
    // so the control settles at its own line and Enter.
    let settled = format!("{}\r", rung_line(&pane, &control).unwrap());
    unchanged(&pane, Duration::from_millis(300));
    assert!(
        bytes(&pane) == settled.as_bytes(),
        "control must settle before navigation: {:?}",
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

/// Scenario: After a submitted doorbell, type a fresh human draft and fetch the task through another consumer, then clear the draft. A submitted ring owes the pane nothing more: no carriage return goes while the draft is there or after it is cleared, the draft is never submitted, and no new doorbell is typed.
#[spec("inbox/doorbell/011")]
#[test]
fn doorbell_011_a_submitted_ring_sends_nothing_after_a_new_human_draft() {
    let (deck, pane) = launch();
    let id = enqueue(&deck, &pane, BODY);
    ring(&deck, &pane, &id);
    let line = rung_line(&pane, &id).unwrap();
    deck.send_keys(DRAFT.as_bytes());
    deck.wait_for_string(DRAFT);
    assert!(common::wait_until(WAIT, || bytes(&pane).ends_with(DRAFT.as_bytes())));
    fetch(&deck, &pane, true);
    state(&deck, &pane, &id, "acknowledged");
    unchanged(&pane, Duration::from_millis(700));
    assert!(
        bytes(&pane).ends_with(DRAFT.as_bytes()),
        "something submitted the human draft"
    );
    deck.send_keys(b"\x15");
    let expected = format!("{line}\r{DRAFT}\x15");
    assert!(
        common::wait_until(WAIT, || bytes(&pane) == expected.as_bytes()),
        "Ctrl+U must reach the pane: {:?}",
        bytes(&pane)
    );
    unchanged(&pane, Duration::from_millis(700));
    assert_eq!(
        bytes(&pane),
        expected.as_bytes(),
        "nothing follows the cleared draft"
    );
    assert_eq!(rings(&pane).len(), 1);
    assert!(!turns(&pane).iter().any(|turn| turn["line"] == DRAFT));
}

/// Scenario: Hold two queued reports behind a draft in an attached pane. Clearing the draft must produce one coalesced token-led line; fetching must return both reports in arrival order with no report content typed into the pane.
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
