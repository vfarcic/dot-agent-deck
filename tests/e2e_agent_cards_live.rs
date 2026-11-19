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
/// What the agent's own suggested action creates: its text is the agent's,
/// so it reaches the agent framed as untrusted (audit S1).
const SUGGESTED: &str = "AGENT_CARD_LIVE_3E91D7_SUGGESTED.sentinel";
const PANE_ID: &str = "agent-card-live-pane";
/// Each phase's own bound. Their sum, with the harness's fixed waits, sets
/// the deck lifetime below and the nextest override for this binary
/// (`.config/nextest.toml`):
///
/// | phase | bound |
/// | --- | --- |
/// | the deck draws `No active agents`, Haiku leaves it, typing mode (harness `WAIT_TIMEOUT` x 3) | 90s |
/// | Haiku's `SessionStart` | [`START`] 90s |
/// | Haiku's pane settles | [`SETTLE`] 60s |
/// | the publish prompt is typed and submitted (`CLAUDE_PROMPT_SUBMIT_BASE` 20s, load-scaled up to x6) | 120s |
/// | the published card, the suggested file, the sentinel: one agent turn each | 3 x [`TURN`] 120s |
/// | the delivery recorded and the card done, after the sentinel | [`FOLLOW_UP`] 90s |
/// | the attached TUI shows the sentinel | [`SHOWN`] 30s |
/// | **worst case** | **840s** |
///
/// The 60s between that and the 900s lifetime is for what has no bound of its
/// own: spawning the deck and the `card list` / `card act` calls.
const START: Duration = Duration::from_secs(90);
const SETTLE: Duration = Duration::from_secs(60);
const TURN: Duration = Duration::from_secs(120);
const FOLLOW_UP: Duration = Duration::from_secs(90);
const SHOWN: Duration = Duration::from_secs(30);

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

fn wait_card(
    deck: &TuiDeck,
    key: &str,
    what: &str,
    within: Duration,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let last = RefCell::new(None);
    let ok = common::wait_until(within, || {
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
/// a question, that action and a suggested action of its own. `card list`
/// shows its report; the user presses the agent's own suggestion and the
/// agent creates the file it names, then runs `card act --action
/// touch-sentinel`, and the agent creates the uniquely named sentinel file,
/// the delivery is recorded as delivered, its card turns done, and the
/// attached TUI shows it working.
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
        // Above the phases' 840s worst case (the table at `START`), at
        // linkage-check rule 11's 900s ceiling; the nextest override for this
        // binary kills the test only past it, at 960s.
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "900")
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
    events.wait_for_session_start_on_pane(PANE_ID, &agent_id, START);
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    assert!(
        common::wait_until_panes_settled(
            deck.attach_socket_path(),
            std::slice::from_ref(&agent_id),
            Duration::from_millis(1500),
            Duration::from_secs(8),
            SETTLE,
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
    let report = format!(
        r#"{{"status":"waiting","summary":"Waiting for you to choose","questions":[{{"id":"go","text":"Create the sentinel?","options":[{{"id":"yes","label":"yes"}}]}}],"actions":[{{"name":"touch-sentinel"}},{{"name":"touch-suggested","label":"Touch suggested","prompt":"Create an empty file named {SUGGESTED} in the current directory with Bash, then stop and wait without doing anything else."}}]}}"#
    );
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
    let published = wait_card(
        &deck,
        &key,
        "showed the agent's waiting report",
        TURN,
        |e| {
            e["report"]["status"] == "waiting"
                && e["questions"][0]["id"] == "go"
                && e["actions"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["id"] == "touch-sentinel"))
        },
    );
    assert_eq!(published["report"]["source"], "agent");
    assert_eq!(published["needs_you"], true);
    assert!(
        !repo.join(SENTINEL).exists(),
        "no sentinel before the action"
    );
    assert!(
        !repo.join(SUGGESTED).exists(),
        "no suggested file before the user chose the suggestion"
    );
    let instance = published["instance_id"]
        .as_str()
        .expect("the card's instance id")
        .to_string();
    let token_of = |id: &str| {
        published["actions"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["id"] == id))
            .and_then(|a| a["token"].as_str())
            .unwrap_or_else(|| panic!("the listed action {id}'s token: {published:#}"))
            .to_string()
    };
    let act = |id: &str| {
        let token = token_of(id);
        let out = user_cli(
            &deck,
            &[
                "card",
                "act",
                "--entry",
                &key,
                "--action",
                id,
                "--token",
                &token,
                "--instance",
                &instance,
                "--json",
            ],
        );
        assert!(
            out.status.success(),
            "card act {id}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let result: Value = serde_json::from_slice(&out.stdout).expect("card act --json");
        assert_eq!(result["outcome"], "sent", "{result}");
        result["message_id"]
            .as_str()
            .expect("message id")
            .to_string()
    };

    // The agent's own suggestion arrives framed as untrusted text the user
    // chose (audit S1); the agent still does what the user chose.
    phase("invoking the agent's own suggestion");
    let suggested = published["actions"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["label"] == "Touch suggested"))
        .and_then(|a| a["id"].as_str())
        .expect("the agent's own action")
        .to_string();
    act(&suggested);
    phase("waiting for the suggested file");
    assert!(
        common::wait_for_path(&repo.join(SUGGESTED), TURN),
        "the agent never created {SUGGESTED} in {}\n{}",
        repo.display(),
        deck.snapshot_grid()
    );

    phase("invoking touch-sentinel");
    // `card act` names the version of the action the user read and the
    // daemon process that listed it, both from the card as listed.
    let message_id = act("touch-sentinel");

    phase("waiting for the sentinel");
    assert!(
        common::wait_for_path(&repo.join(SENTINEL), TURN),
        "the agent never created {SENTINEL} in {}\n{}",
        repo.display(),
        deck.snapshot_grid()
    );
    // What was sent stays on the card whatever the agent publishes next
    // (`recent_deliveries`); the action's prompt also has the agent keep
    // suggesting it, so the action shows the delivery as its own too.
    phase("waiting for delivered and done");
    let done = wait_card(
        &deck,
        &key,
        "recorded the delivery and turned done",
        FOLLOW_UP,
        |e| {
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
        },
    );
    assert_eq!(done["report"]["source"], "agent");
    assert!(
        deck.wait_for_grid_string_within(SENTINEL, SHOWN),
        "the attached TUI must show the agent working on the sentinel:\n{}",
        deck.snapshot_grid()
    );
    assert!(Path::new(&repo).join(SENTINEL).is_file());
}
