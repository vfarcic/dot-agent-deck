#![cfg(all(feature = "e2e", unix))]

//! PRD #1666 part 3 (`inbox/doorbell/056`): a Claude Code whose doorbell the
//! user turned off (`doorbell.claude = false`) is held where the user can see
//! it, and a prompt recovers it; with the switch back on, a Claude Code
//! release nobody measured is rung on Claude Code's facts. In a PTY-attached
//! deck with a scripted Claude Code screen. No agent executable or credential
//! is used.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use common::TuiDeck;
use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::time::Duration;

/// A release nobody measured: the deck types into it on Claude Code's facts.
const UNMEASURED: &str = "2.1.399";
const LOG: &str = "deck-056.log";
const TASK: &str = "TURNED-OFF-DOORBELL-TASK-056: write nothing, this is a fixture";

/// Write the deck's `config.toml` with the Claude Code doorbell `on` or off,
/// as `dot-agent-deck config set doorbell.claude <on>` would. The daemon reads
/// it at each agent's start.
fn set_claude_doorbell(deck: &TuiDeck, on: bool) {
    let dir = deck.home_dir().join(".config/dot-agent-deck");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!("[doorbell]\nclaude = {on}\n"),
    )
    .unwrap();
}

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

/// Scenario: In a PTY-attached deck whose config turns the Claude Code doorbell off, an orchestrator delegates a task to a scripted Claude Code worker whose start-up header names a release nobody measured; it is Idle at its empty composer with nothing armed. For longer than the stall window nothing is typed into it and no doorbell rings; the task stays queued, the worker's card on the screen and the agent record the desktop reads show the cannot_type words, `daemon status` shows WAKE none, the orchestrator's overdue notice reads `Wake path: none` without promising the doorbell, and the deck log's `doorbell off` line names turned_off. The test then types a prompt into the worker's pane, as a user would: its prompt hook fetches the task, which is acknowledged, and the card clears. With the switch turned back on, a second scripted worker drawing the same unmeasured release's header is rung on Claude Code's facts: WAKE doorbell, one ring, and its task fetched.
#[spec("inbox/doorbell/056")]
#[test]
fn doorbell_056_a_turned_off_claude_doorbell_is_held_visibly_and_a_prompt_recovers_it() {
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
    set_claude_doorbell(&deck, false);
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
        "nothing typed into the turned-off Claude Code"
    );
    assert!(!log(&deck).contains("doorbell rang"), "no doorbell rang");
    assert!(
        log(&deck)
            .lines()
            .any(|line| line.contains("doorbell off") && line.contains("turned_off")),
        "the log names the cause: {}",
        log(&deck)
    );
    assert!(
        !log(&deck).contains("unmeasured"),
        "no unmeasured-release wording reaches the log: {}",
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

    // Switched back on, the same unmeasured release is rung on Claude Code's
    // facts: the switch applies to agents started after the change.
    set_claude_doorbell(&deck, true);
    let control = claude(&deck, "unmeasured-coder", UNMEASURED, json!({}));
    status(&deck, &control, "session_start", "claude_code");
    status(&deck, &control, "idle", "claude_code");
    assert!(
        common::wait_until(WAIT, || status_wake(&deck, &control) == "doorbell"),
        "the unmeasured release's WAKE"
    );
    let control_id = enqueue(&deck, &control, BODY);
    assert!(
        common::wait_until(WAIT, || rung_line(&control, &control_id).is_some()),
        "the unmeasured release is rung: {:?}",
        String::from_utf8_lossy(&bytes(&control))
    );
    state(&deck, &control, &control_id, "acknowledged");
    assert_eq!(rings(&control).len(), 1, "one ring");
}

/// Scenario: In a PTY-attached deck whose `config.toml` is a symlink to a file that is not there (a dotfiles target that moved, or a volume that is not mounted), a scripted Claude Code drawing a release nobody measured is started and a message is queued for it while it is Idle at its empty composer. Its doorbell is held off as if the config had turned it off: for longer than the stall window nothing is typed, no doorbell rings, the agent record and its card on the screen show the cannot_type words, `daemon status` shows WAKE none, the deck log names the link whose target is missing and a `doorbell off` line naming turned_off, and the message stays queued. With the link removed so nothing is at the config path, a second scripted worker drawing the same header is rung on Claude Code's facts: WAKE doorbell, one ring, and its message fetched.
#[spec("inbox/doorbell/062")]
#[test]
fn doorbell_062_a_config_link_whose_target_is_missing_holds_a_started_claude_off() {
    const LOG_062: &str = "deck-062.log";
    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
        .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
        .with_env("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "1000")
        .with_env("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1")
        .with_env("DOT_AGENT_DECK_LOG", LOG_062)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let log = || std::fs::read_to_string(deck.workdir().join(LOG_062)).unwrap_or_default();

    // The deck's config is a link whose target is missing.
    let dir = deck.home_dir().join(".config/dot-agent-deck");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::os::unix::fs::symlink(deck.workdir().join("unmounted/deck.toml"), &config).unwrap();
    assert!(
        config.symlink_metadata().is_ok() && !config.exists(),
        "the config path is a dangling link"
    );

    let held = claude(&deck, "dangling-coder", UNMEASURED, json!({}));
    status(&deck, &held, "session_start", "claude_code");
    status(&deck, &held, "idle", "claude_code");
    let id = enqueue(&deck, &held, BODY);
    assert!(
        common::wait_until(WAIT, || record(&deck, &held)["inbox"]["wake_hold"]
            .to_string()
            .contains("cannot_type")),
        "the record holds the message as cannot_type: {}",
        record(&deck, &held)
    );
    assert!(
        deck.wait_for_grid_string_within(
            dot_agent_deck::inbox_display::WAKE_HOLD_CANNOT_TYPE,
            WAIT
        ),
        "the card on the screen shows the words: {}",
        deck.snapshot_grid()
    );
    assert_eq!(status_wake(&deck, &held), "none");
    unchanged(&held, Duration::from_millis(1500));
    assert!(
        bytes(&held).is_empty(),
        "nothing typed into the held Claude Code"
    );
    assert!(!log().contains("doorbell rang"), "no doorbell rang");
    assert!(
        log().contains("it is a link whose target is missing"),
        "the daemon read the dangling link at the agent's start: {}",
        log()
    );
    assert!(
        log()
            .lines()
            .any(|line| line.contains("doorbell off") && line.contains("turned_off")),
        "the log names the cause: {}",
        log()
    );
    state(&deck, &held, &id, "queued");

    // The control: nothing at the config path, so the doorbell is on for an
    // agent started now.
    std::fs::remove_file(&config).unwrap();
    assert!(
        config
            .symlink_metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "nothing at the config path"
    );
    let control = claude(&deck, "absent-config-coder", UNMEASURED, json!({}));
    status(&deck, &control, "session_start", "claude_code");
    status(&deck, &control, "idle", "claude_code");
    assert!(
        common::wait_until(WAIT, || status_wake(&deck, &control) == "doorbell"),
        "with no config the unmeasured release's WAKE is the doorbell"
    );
    let control_id = enqueue(&deck, &control, BODY);
    assert!(
        common::wait_until(WAIT, || rung_line(&control, &control_id).is_some()),
        "with no config the unmeasured release is rung: {:?}",
        String::from_utf8_lossy(&bytes(&control))
    );
    state(&deck, &control, &control_id, "acknowledged");
    assert_eq!(rings(&control).len(), 1, "one ring");
    assert!(
        bytes(&held).is_empty(),
        "still nothing typed into the held Claude Code"
    );
}
