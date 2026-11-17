#![cfg(all(unix, feature = "e2e", feature = "e2e-live"))]

//! PRD #2374: an interactive Haiku publishes its agent card, and a card action
//! the user invokes makes it do real work (lane 2, real agent).
//!
//! The user's side is the real `card list` / `card act` CLI, which sends the
//! same daemon requests the desktop app's cards send; the TUI is attached so
//! the agent is visibly working in its pane.

dot_agent_deck_test_harness::link!();

use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use serde_json::Value;
use spec::spec;

const SENTINEL: &str = "AGENT_CARD_LIVE_3E91D7_SENTINEL.sentinel";
const PANE_ID: &str = "agent-card-live-pane";
const WAIT: Duration = Duration::from_secs(150);

/// Name the phase a timed-out run was in: nextest's timeout kill prints the
/// captured stderr and nothing else.
fn phase(name: &str) {
    eprintln!("[agent-card-live] phase: {name}");
}

/// `card list`/`card act` as the user runs them: the deck's sockets, no pane
/// identity.
fn user_cli(deck: &TuiDeck, args: &[&str]) -> std::process::Output {
    std::process::Command::new(common::contained_deck_binary())
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", deck.home_dir())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", deck.attach_socket_path())
        .current_dir(deck.workdir())
        .output()
        .expect("run the real deck CLI")
}

fn card(deck: &TuiDeck, key: &str) -> Option<Value> {
    let out = user_cli(deck, &["card", "list"]);
    assert!(
        out.status.success(),
        "card list: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("card list JSON");
    doc["entries"]
        .as_array()?
        .iter()
        .find(|e| e["key"] == key)
        .cloned()
}

fn wait_card(deck: &TuiDeck, key: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let last = RefCell::new(None);
    let ok = common::wait_until(WAIT, || {
        let found = card(deck, key);
        let hit = found.as_ref().is_some_and(&pred);
        *last.borrow_mut() = found;
        hit
    });
    assert!(
        ok,
        "the card never {what}: {:#?}\n{}",
        last.borrow(),
        deck.snapshot_grid()
    );
    last.into_inner().unwrap()
}

/// Scenario: Start an interactive Haiku in a deck pane under a user catalog
/// with a `touch-sentinel` action, and tell it to publish a waiting card with
/// a question that suggests that action. `card list` shows its report; the
/// user then runs `card act --action touch-sentinel`, and the agent creates
/// the uniquely named sentinel file, the delivery is recorded as delivered,
/// its card turns done, and the attached TUI shows it working.
#[spec("agent-report/live/001")]
#[test]
fn live_001_a_real_agent_publishes_its_card_and_acts_on_a_card_action() {
    skip_unless!(common::check_claude_available());

    let scratch = common::harness_tempdir().expect("live agent card scratch");
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().expect("canonical fixture dir");
    // The binary under test, by absolute path: the agent's PATH can reach an
    // installed deck of another version first, as other live tests note.
    let bin = common::contained_deck_binary().display().to_string();
    let config = scratch.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[[catalog.actions]]\nname = \"touch-sentinel\"\ndescription = \"Offer when the user may want the sentinel file.\"\nlabel = \"Touch sentinel\"\nprompt = {}\n",
            toml::Value::String(format!(
                "Create an empty file named {SENTINEL} in the current directory with Bash, then run this command with Bash to publish your card with status done: {bin} card publish --status done --summary 'Sentinel created' --action touch-sentinel"
            ))
        ),
    )
    .unwrap();
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .with_claude_project_trust(repo.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        // Four real-agent phases outlast the harness's 300s default; the
        // nextest override for this binary allows nine minutes.
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "600")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    phase("starting Haiku");
    let events = deck.subscribe_events();
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StartAgent {
            command: Some(
                "claude --ax-screen-reader --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write"
                    .into(),
            ),
            cwd: Some(repo.to_string_lossy().into_owned()),
            rows: 36,
            cols: 156,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), PANE_ID.into())],
            display_name: Some("card Haiku".into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        },
    )
    .expect("start interactive Haiku");
    assert!(response.ok, "StartAgent: {response:?}");
    let agent_id = response.id.expect("started Haiku agent id");
    let key = format!("{PANE_ID}/{agent_id}");
    events.wait_for_session_start_on_pane(PANE_ID, &agent_id, WAIT);
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    assert!(
        common::wait_until_panes_settled(
            deck.attach_socket_path(),
            std::slice::from_ref(&agent_id),
            Duration::from_millis(1500),
            Duration::from_secs(8),
            WAIT,
        ),
        "interactive Claude startup must settle before typing:\n{}",
        deck.snapshot_grid()
    );
    if deck
        .snapshot_grid()
        .lines()
        .any(|line| line.starts_with(" COMMAND "))
    {
        deck.send_keys(b"\x04");
    }
    deck.wait_until_grid("interactive Haiku typing mode", |grid| {
        grid.lines().any(|line| line.starts_with(" TYPING "))
    });
    let report = r#"{"status":"waiting","summary":"Waiting for you to choose","questions":[{"id":"go","text":"Create the sentinel?","options":[{"id":"yes","label":"yes"}]}],"actions":[{"name":"touch-sentinel"}]}"#;
    phase("prompting the publish");
    deck.submit_claude_prompt(
        &events,
        &agent_id,
        &format!(
            "AGENT_CARD_PUBLISH. Use Bash to run exactly this one command, then stop and wait without doing anything else: printf '%s' '{report}' | {bin} card publish --file -"
        ),
        "AGENT_CARD_PUBLISH",
    );

    phase("waiting for the published card");
    let published = wait_card(&deck, &key, "showed the agent's waiting report", |e| {
        e["report"]["status"] == "waiting"
            && e["questions"][0]["id"] == "go"
            && e["actions"]
                .as_array()
                .is_some_and(|a| a.iter().any(|a| a["id"] == "touch-sentinel"))
    });
    assert_eq!(published["report"]["source"], "agent");
    assert_eq!(published["needs_you"], true);
    assert!(
        !repo.join(SENTINEL).exists(),
        "no sentinel before the action"
    );

    phase("invoking touch-sentinel");
    // `card act` names the version of the action the user read and the
    // daemon process that listed it, both from the card as listed.
    let token = published["actions"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["id"] == "touch-sentinel"))
        .and_then(|a| a["token"].as_str())
        .expect("the listed action's token")
        .to_string();
    let instance = published["instance_id"]
        .as_str()
        .expect("the card's instance id")
        .to_string();
    let out = user_cli(
        &deck,
        &[
            "card",
            "act",
            "--entry",
            &key,
            "--action",
            "touch-sentinel",
            "--token",
            &token,
            "--instance",
            &instance,
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "card act: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).expect("card act --json");
    assert_eq!(result["outcome"], "sent", "{result}");
    let message_id = result["message_id"]
        .as_str()
        .expect("message id")
        .to_string();

    phase("waiting for the sentinel");
    assert!(
        common::wait_for_path(&repo.join(SENTINEL), WAIT),
        "the agent never created {SENTINEL} in {}\n{}",
        repo.display(),
        deck.snapshot_grid()
    );
    // What was sent stays on the card whatever the agent publishes next
    // (`recent_deliveries`); the action's prompt also has the agent keep
    // suggesting it, so the action shows the delivery as its own too.
    phase("waiting for delivered and done");
    let done = wait_card(&deck, &key, "recorded the delivery and turned done", |e| {
        e["report"]["status"] == "done"
            && e["recent_deliveries"].as_array().is_some_and(|d| {
                d.iter().any(|d| {
                    d["action_id"] == "touch-sentinel"
                        && d["message_id"] == message_id.as_str()
                        && d["state"] == "delivered"
                })
            })
            && e["actions"].as_array().is_some_and(|a| {
                a.iter().any(|a| {
                    a["id"] == "touch-sentinel"
                        && a["last_delivery"]["message_id"] == message_id.as_str()
                })
            })
    });
    assert_eq!(done["report"]["source"], "agent");
    assert!(
        deck.wait_for_grid_string_within(SENTINEL, WAIT),
        "the attached TUI must show the agent working on the sentinel:\n{}",
        deck.snapshot_grid()
    );
    assert!(Path::new(&repo).join(SENTINEL).is_file());
}
