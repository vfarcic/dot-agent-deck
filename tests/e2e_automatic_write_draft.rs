#![cfg(feature = "e2e")]

//! Attached-TUI regressions for automatic daemon writes into a pane where a
//! person is still drafting input. The fixture uses live `cat` PTYs so the
//! submitted lines can be read without spending an agent credential.

mod common;

use std::cell::RefCell;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::TabMembership;
use dot_agent_deck::event::EventType;
use spec::spec;

const DRAFT: &str = "draft-544-sentinel";
const POINTER: &str = "Read .dot-agent-deck/worker-task-worker.md for your task.";
const FEEDBACK: &str = "work-done-draft-544-feedback";
const FIRST_REPORT: &str = "work-done-order-544-first";
const SECOND_REPORT: &str = "work-done-order-544-second";

fn launch_orchestration(cap_ms: &str) -> TuiDeck {
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(120, 40)
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0")
        .with_env("DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS", cap_ms)
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active agents");
    let config_path = deck.workdir().join(".dot-agent-deck.toml");
    let original = std::fs::read_to_string(&config_path).expect("read orchestration fixture");
    let configured = original.replace(
        "name = \"worker\"\ncommand = \"cat\"",
        "name = \"worker\"\ncommand = \"cat\"\nclear = false",
    );
    assert_ne!(
        configured, original,
        "worker must retain its pane on delegate"
    );
    std::fs::write(config_path, configured).expect("keep worker pane across delegation");
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("worker");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let records = common::agent_records_on(deck.attach_socket_path());
            ["orchestrator", "worker"].into_iter().all(|name| {
                records.iter().any(|record| {
                    matches!(
                        &record.tab_membership,
                        Some(TabMembership::Orchestration { role_name, .. }) if role_name == name
                    )
                })
            })
        }),
        "orchestration role panes were not registered"
    );
    deck
}

fn role(deck: &TuiDeck, name: &str) -> AgentRecord {
    let records = common::agent_records_on(deck.attach_socket_path());
    records
        .iter()
        .find(|record| {
            matches!(
                &record.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == name
            )
        })
        .unwrap_or_else(|| panic!("{name} role absent from registry: {records:?}"))
        .clone()
}

fn pane_text(deck: &TuiDeck, agent_id: &str) -> String {
    common::strip_ansi(&common::pane_snapshot_on(
        deck.attach_socket_path(),
        agent_id,
    ))
}

fn focus_worker(deck: &TuiDeck) {
    deck.send_keys(b"\x04"); // pane input -> command mode
    deck.send_keys(b"2"); // worker is the second role card
    deck.wait_until_grid("worker pane focused", |grid| grid.contains("┌worker"));
}

fn delegate(deck: &TuiDeck) -> Output {
    let orchestrator = role(deck, "orchestrator");
    Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "delegate",
            "--to",
            "worker",
            "--task",
            "Inspect the draft-deferral sentinel.",
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            orchestrator.pane_id_env.expect("orchestrator pane id"),
        )
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run delegate CLI")
}

fn assert_cli_success(output: &Output, name: &str) {
    assert!(
        output.status.success(),
        "{name} exited {:?}; stdout={:?}; stderr={:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Scenario: Type a draft into the focused worker pane, then delegate through
/// the real CLI. The pointer must stay absent until Enter, and the worker PTY
/// must show the user's draft and the pointer on separate submitted lines.
#[spec("orchestration/delegate/039")]
#[test]
fn orchestration_delegate_039_pointer_waits_for_attached_worker_draft() {
    let deck = launch_orchestration("60000");
    focus_worker(&deck);
    let worker = role(&deck, "worker");
    deck.send_keys(DRAFT.as_bytes());
    assert!(
        common::wait_until(Duration::from_secs(5), || pane_text(&deck, &worker.id)
            .contains(DRAFT)),
        "draft did not reach the worker PTY"
    );
    assert_cli_success(&delegate(&deck), "delegate");

    assert!(
        !common::wait_until(Duration::from_secs(3), || pane_text(&deck, &worker.id)
            .contains(POINTER)),
        "delegate pointer reached worker before the user submitted the draft: {:?}",
        pane_text(&deck, &worker.id)
    );
    deck.send_keys(b"\r");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let text = pane_text(&deck, &worker.id);
            text.contains(&format!("{DRAFT}\r\n")) && text.contains(&format!("{POINTER}\r\n"))
        }),
        "delegate pointer never arrived after Enter: {:?}",
        pane_text(&deck, &worker.id)
    );
    let text = pane_text(&deck, &worker.id);
    assert!(
        text.contains(&format!("{DRAFT}\r\n"))
            && text.contains(&format!("{POINTER}\r\n"))
            && !text.contains(&format!("{DRAFT}{POINTER}")),
        "draft and pointer were not separate submitted lines: {text:?}"
    );
}

/// Scenario: Leave a draft in the focused orchestrator pane and send a real
/// worker completion. Its feedback must wait, while a second daemon command
/// still completes promptly; Ctrl+U then releases the feedback by itself.
#[spec("orchestration/work-done/010")]
#[test]
fn orchestration_work_done_010_feedback_wait_does_not_stall_daemon() {
    let deck = launch_orchestration("60000");
    let orchestrator = role(&deck, "orchestrator");
    let worker = role(&deck, "worker");
    deck.send_keys(DRAFT.as_bytes());
    assert!(
        common::wait_until(Duration::from_secs(5), || pane_text(
            &deck,
            &orchestrator.id
        )
        .contains(DRAFT)),
        "draft did not reach the orchestrator PTY"
    );

    let events = deck.subscribe_events();

    let completion = RefCell::new(
        Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["work-done", "--task", FEEDBACK])
            .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
            .env(
                "DOT_AGENT_DECK_PANE_ID",
                worker.pane_id_env.as_deref().expect("worker pane id"),
            )
            .env("HOME", deck.home_dir())
            .current_dir(deck.workdir())
            .spawn()
            .expect("start work-done CLI"),
    );

    assert!(
        !common::wait_until(Duration::from_secs(3), || pane_text(
            &deck,
            &orchestrator.id
        )
        .contains(FEEDBACK)),
        "work-done feedback submitted the orchestrator's unsent draft: {:?}",
        pane_text(&deck, &orchestrator.id)
    );
    let start = Instant::now();
    let event = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["agent-event", "--type", "running"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            worker.pane_id_env.as_deref().expect("worker pane id"),
        )
        .env("DOT_AGENT_DECK_AGENT_ID", &worker.id)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("send worker status hook during feedback wait");
    assert_cli_success(&event, "agent-event");
    events.wait_for(
        |event| {
            event.pane_id.as_deref() == worker.pane_id_env.as_deref()
                && event.agent_id.as_deref() == Some(worker.id.as_str())
                && event.event_type == EventType::Thinking
        },
        Duration::from_secs(3),
    );
    let status = RefCell::new(
        Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["daemon", "status", "--json"])
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", deck.attach_socket_path())
            .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
            .env("HOME", deck.home_dir())
            .current_dir(deck.workdir())
            .spawn()
            .expect("query daemon status during feedback wait"),
    );
    let status_result = RefCell::new(None);
    let responsive = common::wait_until(Duration::from_secs(3), || {
        if let Some(result) = status.borrow_mut().try_wait().expect("poll daemon status") {
            *status_result.borrow_mut() = Some(result);
            true
        } else {
            false
        }
    });
    if !responsive {
        status
            .borrow_mut()
            .kill()
            .expect("stop stalled daemon status");
        panic!("daemon status stalled behind a deferred work-done write");
    }
    let status_result = status_result.into_inner().expect("completed daemon status");
    assert!(
        status_result.success() && start.elapsed() < Duration::from_secs(3),
        "daemon status failed or stalled while work-done feedback waited: status={status_result:?}, elapsed={:?}",
        start.elapsed(),
    );

    deck.send_keys(b"\x15");
    assert!(
        common::wait_until(Duration::from_secs(10), || pane_text(
            &deck,
            &orchestrator.id
        )
        .contains(FEEDBACK)),
        "work-done feedback never arrived after Ctrl+U: {:?}",
        pane_text(&deck, &orchestrator.id)
    );
    assert!(
        common::wait_until(Duration::from_secs(5), || completion
            .borrow_mut()
            .try_wait()
            .expect("poll work-done")
            .is_some()),
        "work-done CLI remained blocked after feedback was delivered"
    );
    let text = pane_text(&deck, &orchestrator.id);
    assert!(
        !text.contains(&format!("{DRAFT}Worker")),
        "work-done feedback submitted the cleared draft: {text:?}"
    );
}

/// Scenario: Two worker completions arrive while the orchestrator has an
/// unsent draft. Clearing the draft must deliver both reports as separate
/// turns in the order their hook requests arrived.
#[spec("orchestration/work-done/010")]
#[test]
fn orchestration_work_done_010_deferred_reports_keep_arrival_order() {
    let deck = launch_orchestration("60000");
    let orchestrator = role(&deck, "orchestrator");
    let worker = role(&deck, "worker");
    deck.send_keys(DRAFT.as_bytes());
    assert!(
        common::wait_until(Duration::from_secs(5), || pane_text(
            &deck,
            &orchestrator.id
        )
        .contains(DRAFT)),
        "draft did not reach the orchestrator PTY"
    );

    for report in [FIRST_REPORT, SECOND_REPORT] {
        let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["work-done", "--task", report])
            .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
            .env(
                "DOT_AGENT_DECK_PANE_ID",
                worker.pane_id_env.as_deref().expect("worker pane id"),
            )
            .env("HOME", deck.home_dir())
            .current_dir(deck.workdir())
            .output()
            .expect("run work-done CLI");
        assert_cli_success(&output, "work-done");
    }
    assert!(
        !common::wait_until(Duration::from_millis(800), || {
            let text = pane_text(&deck, &orchestrator.id);
            text.contains(FIRST_REPORT) || text.contains(SECOND_REPORT)
        }),
        "completion reached the orchestrator before the draft was cleared: {:?}",
        pane_text(&deck, &orchestrator.id)
    );

    deck.send_keys(b"\x15");
    assert!(
        common::wait_until(Duration::from_secs(10), || pane_text(
            &deck,
            &orchestrator.id
        )
        .contains(SECOND_REPORT)),
        "second completion never arrived after Ctrl+U: {:?}",
        pane_text(&deck, &orchestrator.id)
    );
    let text = pane_text(&deck, &orchestrator.id);
    let first = text.find(FIRST_REPORT).expect("first completion was lost");
    let second = text
        .find(SECOND_REPORT)
        .expect("second completion was lost");
    assert!(
        first < second && text[first..second].contains("\r\n"),
        "completion reports were reordered or joined into one turn: {text:?}"
    );
    assert!(
        !text.contains(&format!("{DRAFT}Worker")),
        "completion report submitted the cleared user draft: {text:?}"
    );
}

/// Scenario: Leave a worker draft unsent beyond a short two-second deferral
/// cap. The real delegate pointer must eventually arrive despite the draft,
/// and the worker's card must explain that the draft may have been submitted.
#[spec("orchestration/delegate/040")]
#[test]
fn orchestration_delegate_040_cap_delivers_instead_of_dropping_pointer() {
    let deck = launch_orchestration("2000");
    focus_worker(&deck);
    let worker = role(&deck, "worker");
    deck.send_keys(DRAFT.as_bytes());
    assert!(
        common::wait_until(Duration::from_secs(5), || pane_text(&deck, &worker.id)
            .contains(DRAFT)),
        "draft did not reach the worker PTY"
    );

    let start = Instant::now();
    assert_cli_success(&delegate(&deck), "delegate");
    assert!(
        !common::wait_until(Duration::from_millis(900), || pane_text(&deck, &worker.id)
            .contains(POINTER)),
        "pointer arrived before the draft deferral cap: {:?}",
        pane_text(&deck, &worker.id)
    );
    assert!(
        common::wait_until(Duration::from_secs(8), || pane_text(&deck, &worker.id)
            .contains(POINTER)),
        "pointer was lost after the draft deferral cap: {:?}",
        pane_text(&deck, &worker.id)
    );
    assert!(
        start.elapsed() >= Duration::from_millis(1800),
        "pointer arrived before the configured two-second cap: {:?}",
        start.elapsed()
    );
    let card_column = || {
        deck.snapshot_grid()
            .lines()
            .map(|line| line.chars().take(40).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            card_column().contains("a deck prompt waited")
        }),
        "worker card did not render the capped-draft DeliveryNotice: {}",
        card_column()
    );
}
