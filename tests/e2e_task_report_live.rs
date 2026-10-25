#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! A real interactive Haiku worker completes the delegated task it names
//! (PRD #1666 part 3, round 5): it reports with the `--task-id` its
//! instructions give, the orchestrator's inbox carries that task id, and the
//! worker is free for the next delegate.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use common::TuiDeck;
use dot_agent_deck::config;
use serde_json::{Value, json};
use spec::spec;
use std::time::Duration;

const ORCHESTRATION: &str = "task-report-live";
const WORKER_ROLE: &str = "worker";
const WORKER_COMMAND: &str =
    "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write";
const SENTINEL_FILE: &str = "task-report-037-c41e9a.txt";
const SENTINEL_CONTENT: &str = "TASK_REPORT_037_C41E9A";

/// The orchestrator stand-in: `cat`, which records its hook capability so the
/// test can delegate as the orchestrator.
fn orchestrator_command() -> String {
    common::capability_export_command("exec cat")
}

fn orchestration_config() -> String {
    format!(
        "[[orchestrations]]\n\
         name = \"{ORCHESTRATION}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = {}\n\
         start = true\n\n\
         [[orchestrations.roles]]\n\
         name = \"{WORKER_ROLE}\"\n\
         command = \"{WORKER_COMMAND}\"\n\
         clear = false\n",
        serde_json::to_string(&orchestrator_command()).unwrap()
    )
}

fn orchestration_session(project_dir: &str) -> String {
    let session = config::SavedSession {
        panes: vec![config::SavedPane {
            dir: project_dir.to_string(),
            name: "orchestrator".to_string(),
            command: orchestrator_command(),
            mode: None,
            orchestration: Some(config::OrchestrationSnapshot {
                version: 1,
                roles: vec!["orchestrator".to_string(), WORKER_ROLE.to_string()],
                start_role_index: 0,
                orchestrator_prompt: String::new(),
                config_name: ORCHESTRATION.to_string(),
                project_path: project_dir.to_string(),
                started_role_indices: vec![0],
                display_title: None,
            }),
        }],
        last_command: None,
        focus: None,
    };
    toml::to_string_pretty(&session).expect("serialize the orchestration session")
}

/// The orchestrator's identity on the hook socket: its pane, agent and token.
fn orchestrator(deck: &TuiDeck, project_dir: &std::path::Path) -> (String, String, String) {
    let record = std::cell::RefCell::new(Value::Null);
    assert!(
        common::wait_until(Duration::from_secs(60), || {
            *record.borrow_mut() =
                inbox::attach(deck, json!({"op":"list-agents"}))["agent_records"]
                    .as_array()
                    .and_then(|records| {
                        records
                            .iter()
                            .find(|r| r["tab_membership"]["role_name"] == "orchestrator")
                    })
                    .cloned()
                    .unwrap_or(Value::Null);
            record.borrow()["pane_id_env"].is_string()
        }),
        "the orchestrator never registered\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    let record = record.into_inner();
    let agent = record["id"].as_str().unwrap().to_string();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(project_dir, &agent))
        .expect("the orchestrator's hook capability");
    (
        record["pane_id_env"].as_str().unwrap().to_string(),
        agent,
        token,
    )
}

fn delegate(deck: &TuiDeck, orch: &(String, String, String), task: &str) -> Value {
    inbox::hook(
        deck,
        json!({"message_type":"delegate","pane_id":orch.0,"agent_id":orch.1,"token":orch.2,
            "task":task,"to":[WORKER_ROLE],"supersede":false,"timestamp":chrono::Utc::now()}),
    )
}

/// Scenario: Restore a two-role orchestration whose orchestrator is a `cat` stand-in and whose worker is an interactive Haiku agent, and delegate it a directive task naming a unique sentinel file. The worker creates the file and reports with `work-done --task-id` as its instructions say; the orchestrator's inbox then lists the report with the delegated task's id, and a plain follow-up delegate to the same worker is accepted because nothing is owed.
#[spec("orchestration/work-done/037")]
#[test]
fn work_done_037_a_real_worker_reports_the_task_it_was_given() {
    skip_unless!(common::check_claude_available());

    let root = common::harness_tempdir().expect("orchestration root tempdir");
    let project_dir = root.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create the project directory");
    let project_dir = project_dir
        .canonicalize()
        .expect("canonicalize the project directory");
    let project_str = project_dir
        .to_str()
        .expect("UTF-8 project path")
        .to_string();
    let _ = common::fixture_git(&project_dir, &project_dir)
        .arg("init")
        .arg("--quiet")
        .status();
    std::fs::write(
        project_dir.join(".dot-agent-deck.toml"),
        orchestration_config(),
    )
    .expect("write the orchestration config");
    let session_path = root.path().join("session.toml");
    std::fs::write(&session_path, orchestration_session(&project_str))
        .expect("write the orchestration session");

    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_imported_claude_credentials()
        .with_claude_project_trust(project_str.clone())
        .with_env("PATH", common::path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_path.to_str().expect("UTF-8 session path"),
        )
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_LOG", "task-report-live.log")
        .launch_with_fixture("minimal");
    assert!(
        deck.wait_for_grid_string_within(ORCHESTRATION, Duration::from_secs(45)),
        "the orchestration never surfaced\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    let orch = orchestrator(&deck, &project_dir);

    let task = format!(
        "Use the Bash tool to run exactly this one command: printf '%s\\n' \
         '{SENTINEL_CONTENT}' > '{SENTINEL_FILE}' Then report this task done exactly as the \
         'When done' section of your task instructions says, with the summary \
         'Created {SENTINEL_FILE}.', and end your turn."
    );
    let reply = delegate(&deck, &orch, &task);
    assert!(
        reply["error"].is_null(),
        "the delegate was refused: {reply}"
    );
    let task_id = reply["message_ids"][0]
        .as_str()
        .unwrap_or_else(|| panic!("the delegate names its message: {reply}"))
        .to_string();

    let sentinel = project_dir.join(SENTINEL_FILE);
    assert!(
        common::wait_for_path(&sentinel, Duration::from_secs(180)),
        "the Haiku worker never created {SENTINEL_FILE}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap().trim(),
        SENTINEL_CONTENT
    );

    let listed = std::cell::RefCell::new(Value::Null);
    let reported = common::wait_until(Duration::from_secs(120), || {
        *listed.borrow_mut() = inbox::hook(
            &deck,
            json!({"message_type":"inbox_list","pane_id":orch.0,"agent_id":orch.1,
                "token":orch.2,"all":true}),
        );
        listed.borrow()["messages"]
            .as_array()
            .is_some_and(|messages| {
                messages.iter().any(|m| {
                    m["provenance"]["kind"] == "task"
                        && m["provenance"]["task_id"] == task_id.as_str()
                })
            })
    });
    assert!(
        reported,
        "the orchestrator's inbox never listed a report of task {task_id}: {}\nFinal grid:\n{}",
        listed.borrow(),
        deck.snapshot_grid()
    );

    let follow_up = delegate(
        &deck,
        &orch,
        "Respond with exactly FOLLOW_UP_C41E9A and end your turn.",
    );
    assert!(
        follow_up["error"].is_null(),
        "the worker reported its task, so a follow-up delegate is accepted: {follow_up}"
    );
}
