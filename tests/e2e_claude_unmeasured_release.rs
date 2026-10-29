#![cfg(all(feature = "e2e", unix))]

//! PRD #1666 part 3, round 5-wake (`inbox/doorbell/056`): a Claude Code
//! release the deck has not measured is held where the user can see it, and a
//! prompt recovers it, in a PTY-attached deck with a scripted Claude Code
//! screen. No agent executable or credential is used.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use common::TuiDeck;
use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::time::Duration;

/// A release no record measures, and the one the deck has measured.
const UNMEASURED: &str = "2.1.399";
const MEASURED: &str = "2.1.310";
const LOG: &str = "deck-056.log";
const TASK: &str = "UNMEASURED-RELEASE-TASK-056: write nothing, this is a fixture";

/// The command that runs the scripted Claude Code screen at `version` for
/// pane `name`.
fn claude_command(deck: &TuiDeck, name: &str, version: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let script = deck.workdir().join("claude-composer.py");
    std::fs::write(
        deck.workdir().join("composer_external_edit.py"),
        include_str!("support/composer_external_edit.py"),
    )
    .unwrap();
    std::fs::write(&script, include_str!("support/claude_composer.py")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let base = deck.workdir().join(format!(".claude-{name}"));
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
    common::capability_export_command(&format!(
        "exec python3 '{}' '{}' '{}' '{version}'",
        quote(&script),
        quote(&base),
        quote(common::contained_deck_binary())
    ))
}

/// Start the scripted Claude Code screen at `version` in pane `name`, with
/// `extra` start-agent fields.
fn claude(deck: &TuiDeck, name: &str, version: &str, extra: Value) -> Pane {
    let command = claude_command(deck, name, version);
    let base = deck.workdir().join(format!(".claude-{name}"));
    let mut start = json!({"op":"start-agent","command":command,"cwd":deck.workdir(),
        "rows":40,"cols":140,"agent_type":"claude_code",
        "env":[["DOT_AGENT_DECK_PANE_ID",name]],"display_name":name});
    start
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    let reply = attach(deck, start);
    assert_eq!(reply["ok"], true, "start the scripted Claude Code: {reply}");
    let agent = reply["id"].as_str().unwrap().to_string();
    let pid = base.with_extension("pid");
    assert!(
        common::wait_until(common::child_boot_budget(), || std::fs::read_to_string(
            &pid
        )
        .is_ok_and(|text| text.parse::<u32>().is_ok())),
        "the scripted Claude Code must start"
    );
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
        .expect("the pane's capability");
    Pane {
        id: name.into(),
        agent,
        token,
        pid: std::fs::read_to_string(pid).unwrap().parse().unwrap(),
        capture: base.with_extension("bytes"),
        attach_socket: deck.attach_socket_path().to_path_buf(),
    }
}

/// The agent's record, as `list-agents` and the desktop read it.
fn record(deck: &TuiDeck, pane: &Pane) -> Value {
    attach(deck, json!({"op":"list-agents"}))["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == pane.agent)
        .cloned()
        .unwrap_or_default()
}

/// What `daemon status --json` says the pane's WAKE is.
fn status_wake(deck: &TuiDeck, pane: &Pane) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["daemon", "status", "--json"])
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", deck.attach_socket_path())
        .output()
        .expect("run `daemon status --json`");
    assert!(output.status.success(), "{output:?}");
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    parsed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["pane_id"] == pane.id.as_str())
        .map(|agent| {
            agent["inbox"]["wake"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_else(|| panic!("no status row for {}: {parsed}", pane.id))
}

fn log(deck: &TuiDeck) -> String {
    std::fs::read_to_string(deck.workdir().join(LOG)).unwrap_or_default()
}

/// Scenario: In a PTY-attached deck, an orchestrator delegates a task to a scripted Claude Code worker whose start-up header names a release the deck has not measured; it is Idle at its empty composer with nothing armed. For longer than the stall window nothing is typed into it and no doorbell rings; the task stays queued, the worker's card on the screen and the agent record the desktop reads show the cannot_type words, `daemon status` shows WAKE none, and the orchestrator's overdue notice reads `Wake path: none` without promising the doorbell. The test then types a prompt into the worker's pane, as a user would: its prompt hook fetches the task, which is acknowledged, and the card clears. A second scripted worker drawing the measured release's header is the control: WAKE doorbell, one ring, and its task fetched.
#[spec("inbox/doorbell/056")]
#[test]
fn doorbell_056_an_unmeasured_claude_release_is_held_visibly_and_a_prompt_recovers_it() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
        .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
        .with_env("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "1000")
        .with_env(route::UNDELIVERED_MS, "1500")
        .with_env("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1")
        .with_env("DOT_AGENT_DECK_LOG", LOG)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    // The roles as the deck's config names them, each with the command its
    // pane runs.
    std::fs::write(
        deck.workdir().join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\nname = 'claude-release'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = {}\nstart = true\n[[orchestrations.roles]]\nname = 'coder'\ncommand = {}\nclear = false\n",
            serde_json::to_string(&route::command(deck.workdir())).unwrap(),
            serde_json::to_string(&claude_command(&deck, "release-coder", UNMEASURED)).unwrap()
        ),
    )
    .unwrap();
    let membership = |role: &str, index, start| {
        json!({"tab_membership": {
        "kind":"orchestration", "name":"claude-release", "role_name":role,
        "role_index":index, "is_start_role":start, "orchestration_id":"claude-release-056",
        "orchestration_cwd":deck.workdir()}})
    };
    let orch = route::start(
        &deck,
        "release-orchestrator",
        membership("orchestrator", 0, true),
    );
    let worker = claude(
        &deck,
        "release-coder",
        UNMEASURED,
        membership("coder", 1, false),
    );
    status(&deck, &worker, "session_start", "claude_code");
    status(&deck, &worker, "idle", "claude_code");
    let id = route::delegate(&deck, &orch, TASK, false);
    state(&deck, &worker, &id, "queued");

    assert!(
        common::wait_until(WAIT, || record(&deck, &worker)["inbox"]["wake_hold"]
            .to_string()
            .contains("cannot_type")),
        "the record holds the task as cannot_type: {}",
        record(&deck, &worker)
    );
    // The worker's card is on its orchestration's tab.
    let (col, row) = deck.wait_for_in_grid("claude-release");
    deck.click(col, row);
    assert!(
        deck.wait_for_grid_string_within(
            dot_agent_deck::inbox_display::WAKE_HOLD_CANNOT_TYPE,
            WAIT
        ),
        "the card on the screen shows the words: {}",
        deck.snapshot_grid()
    );
    assert_eq!(status_wake(&deck, &worker), "none");
    let notice = route::notice(&deck, &orch, "Wake path: none").to_string();
    assert!(
        !notice.contains("rings the doorbell"),
        "the notice promises no doorbell: {notice}"
    );
    unchanged(&worker, Duration::from_millis(1500));
    assert!(
        bytes(&worker).is_empty(),
        "nothing typed into the unmeasured release"
    );
    assert!(!log(&deck).contains("doorbell rang"), "no doorbell rang");
    assert!(
        log(&deck).lines().any(|line| line.contains("doorbell off")
            && line.contains("unmeasured_release")
            && line.contains(MEASURED)),
        "the log names the cause and the measured releases: {}",
        log(&deck)
    );
    state(&deck, &worker, &id, "queued");

    // The user sends it a prompt.
    let mut user = common::AttachInput::attach(deck.attach_socket_path(), &worker.agent);
    user.write("please check your messages\r");
    state(&deck, &worker, &id, "acknowledged");
    assert!(
        common::wait_until(WAIT, || record(&deck, &worker)["inbox"]["wake_hold"]
            .is_null()),
        "the card clears: {}",
        record(&deck, &worker)
    );

    // Control: the measured release is rung.
    let control = claude(&deck, "measured-coder", MEASURED, json!({}));
    status(&deck, &control, "session_start", "claude_code");
    status(&deck, &control, "idle", "claude_code");
    assert!(
        common::wait_until(WAIT, || status_wake(&deck, &control) == "doorbell"),
        "the measured release's WAKE"
    );
    let control_id = enqueue(&deck, &control, BODY);
    assert!(
        common::wait_until(WAIT, || rung_line(&control, &control_id).is_some()),
        "the measured release is rung: {:?}",
        String::from_utf8_lossy(&bytes(&control))
    );
    state(&deck, &control, &control_id, "acknowledged");
    assert_eq!(rings(&control).len(), 1, "one ring");
}
