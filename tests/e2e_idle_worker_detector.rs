#![cfg(feature = "e2e")]

//! PTY-attached coverage for the daemon idle-worker detector. The synthetic
//! case opens the `orch-deck` fixture's live `cat` role panes and injects a
//! Delegate over its hook socket; the real-agent case restores an orchestration
//! whose interactive Claude Haiku orchestrator delegates to a silent worker.
//! Both must render the daemon's idle prompt in the orchestrator surface.

mod common;

use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::config;
use dot_agent_deck::daemon_protocol::TabMembership;
use dot_agent_deck::event::{DaemonMessage, DelegateSignal};
use spec::spec;

const REAL_ORCHESTRATION_NAME: &str = "idle-worker-real";
const REAL_ORCHESTRATOR_MODEL: &str = "claude-haiku-4-5-20251001";
const REAL_WORKER_ROLE: &str = "worker";

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent directory");
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn real_agent_orchestration_config(orchestrator_command: &str) -> String {
    format!(
        "[[orchestrations]]\n\
         name = \"{REAL_ORCHESTRATION_NAME}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"{orchestrator_command}\"\n\
         start = true\n\n\
         [[orchestrations.roles]]\n\
         name = \"{REAL_WORKER_ROLE}\"\n\
         command = \"cat\"\n\
         clear = false\n"
    )
}

fn real_agent_orchestration_session(
    project_dir: &str,
    orchestrator_command: &str,
    directive: &str,
) -> String {
    let session = config::SavedSession {
        panes: vec![config::SavedPane {
            dir: project_dir.to_string(),
            name: "orchestrator".to_string(),
            command: orchestrator_command.to_string(),
            mode: None,
            orchestration: Some(config::OrchestrationSnapshot {
                version: 1,
                roles: vec!["orchestrator".to_string(), REAL_WORKER_ROLE.to_string()],
                start_role_index: 0,
                orchestrator_prompt: directive.to_string(),
                config_name: REAL_ORCHESTRATION_NAME.to_string(),
                project_path: project_dir.to_string(),
                started_role_indices: vec![0],
                display_title: None,
            }),
        }],
        last_command: None,
    };
    toml::to_string_pretty(&session).expect("serialize real-agent orchestration session")
}

fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> new-pane form
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // select [Orch: demo-orch]
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // submit (Command is hidden)
}

fn orchestration_panes(deck: &TuiDeck) -> (String, String) {
    let panes = RefCell::new(None);
    let ready = common::wait_until(Duration::from_secs(10), || {
        let records = common::agent_records_on(deck.attach_socket_path());
        let orchestrator = records
            .iter()
            .find_map(|record| match &record.tab_membership {
                Some(TabMembership::Orchestration {
                    role_name,
                    is_start_role: true,
                    ..
                }) if role_name == "orchestrator" => record.pane_id_env.clone(),
                _ => None,
            });
        let worker = records
            .iter()
            .find_map(|record| match &record.tab_membership {
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == "worker" => {
                    record.pane_id_env.clone()
                }
                _ => None,
            });
        if let (Some(orchestrator), Some(worker)) = (orchestrator, worker) {
            *panes.borrow_mut() = Some((orchestrator, worker));
            return true;
        }
        false
    });
    assert!(
        ready,
        "orchestration role panes were not registered within 10s; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
    panes
        .into_inner()
        .expect("ready role-pane poll stores both pane ids")
}

/// Scenario: Launch the real TUI and its lazy daemon with a tiny worker-response timeout, open the two-role `orch-deck` fixture, and inject a Delegate from the orchestrator pane to the live `cat` worker over the hook socket. The worker never sends work-done, so the rendered orchestration surface must visibly contain "has not responded" after the timeout.
#[spec("scheduler/idle-worker/011")]
#[test]
fn idle_worker_011_silent_worker_prompt_is_visible_in_attached_tui() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "1500")
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");
    open_orchestration(&deck);
    deck.wait_for_string("worker");

    let (orchestrator_pane, _worker_pane) = orchestration_panes(&deck);
    let message = DaemonMessage::Delegate(DelegateSignal {
        pane_id: orchestrator_pane,
        task: "Remain silent so the idle detector can surface its prompt.".to_string(),
        to: vec!["worker".to_string()],
        timestamp: chrono::Utc::now(),
    });
    let line = serde_json::to_string(&message).expect("serialize Delegate hook message");
    common::write_hook_line(deck.hook_socket_path(), &line)
        .expect("inject Delegate over hook socket");

    deck.wait_for_string("has not responded");
}

/// Scenario: Restore a two-role orchestration whose real interactive Claude Haiku orchestrator is directed to delegate once through the `dot-agent-deck` CLI to a `cat` worker that never sends work-done. After the short detector timeout, the attached TUI must visibly render the daemon-authored `has not responded` nudge in the live orchestration pane.
#[spec("scheduler/idle-worker/012")]
#[test]
fn idle_worker_012_real_orchestrator_visibly_receives_idle_nudge() {
    skip_unless!(common::check_claude_available());

    let orchestration_root = tempfile::tempdir().expect("orchestration root tempdir");
    let project_dir = orchestration_root.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create orchestration project directory");
    let project_dir = project_dir
        .canonicalize()
        .expect("canonicalize orchestration project directory");
    let project_str = project_dir
        .to_str()
        .expect("orchestration project directory is UTF-8")
        .to_string();
    let _ = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(&project_dir)
        .status();

    let orchestrator_command =
        format!("claude --model {REAL_ORCHESTRATOR_MODEL} --allowedTools Bash");
    let directive = format!(
        "You are the orchestrator in a dot-agent-deck orchestration. Use the Bash tool to run \
         this exact command once: dot-agent-deck delegate --to {REAL_WORKER_ROLE} --task \
         'Remain silent and do not send work-done.' Do not do the worker task yourself and do \
         not run work-done. After the delegate command succeeds, say that you are waiting for \
         the worker, then stop."
    );

    std::fs::write(
        project_dir.join(".dot-agent-deck.toml"),
        real_agent_orchestration_config(&orchestrator_command),
    )
    .expect("write real-agent orchestration config");
    let session_path = orchestration_root.path().join("session.toml");
    std::fs::write(
        &session_path,
        real_agent_orchestration_session(&project_str, &orchestrator_command, &directive),
    )
    .expect("write real-agent orchestration session");

    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_imported_claude_credentials()
        .with_claude_project_trust(project_str.clone())
        .with_env("PATH", path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_path.to_str().expect("session path is UTF-8"),
        )
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "10000")
        .launch_with_fixture("minimal");

    assert!(
        deck.wait_for_grid_string_within(REAL_ORCHESTRATION_NAME, Duration::from_secs(45)),
        "the restored real-agent orchestration never surfaced within 45s\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    let worker_task = project_dir
        .join(".dot-agent-deck")
        .join(format!("worker-task-{REAL_WORKER_ROLE}.md"));
    assert!(
        common::wait_for_path(&worker_task, Duration::from_secs(120)),
        "the real Claude orchestrator never delegated to {REAL_WORKER_ROLE:?}; expected the \
         daemon to create {worker_task:?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    assert!(
        deck.wait_for_grid_string_within("has not responded", Duration::from_secs(60)),
        "the real orchestrator delegated, but the daemon-authored idle nudge never became \
         visible in the attached orchestration pane\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}
