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

/// The daemon-authored opening clause of `compose_idle_worker_prompt`.
///
/// The bare `has not responded` this used to match was **not** proof of
/// provenance: a real orchestrator can write those words itself while
/// explaining why it is waiting on a worker, so the needle could be satisfied
/// by the very model whose input it is supposed to be verifying. The
/// parenthetical is the anchor — the prompt declares itself a daemon report
/// and explicitly not a message from a person or an agent, which a model
/// narrating its own state has no reason to emit verbatim.
const IDLE_DAEMON_CLAUSE: &str = "has not responded with work-done (dot-agent-deck daemon report, \
                                  not a message from a person or an agent)";

/// The second daemon-specific anchor: the role name is wrapped in unforgeable
/// untrusted-data markers (PRD #126 M1 audit finding 1), so matching the
/// WRAPPED form proves both that the daemon composed the line and that it
/// framed the role as data.
fn idle_role_label(role: &str) -> String {
    format!("[UNTRUSTED-ROLE-LABEL: {role} :END-UNTRUSTED-ROLE-LABEL]")
}

/// Drop every whitespace run and the vt100 box-drawing verticals from `text`.
///
/// The idle prompt is one long line, so on a rendered grid it is broken across
/// rows at whatever column the pane happens to be — and a needle straddling
/// that wrap column is absent from the row-joined snapshot even though every
/// character of it is on screen. Only the daemon clause *opens* the line and
/// is therefore safe to match raw; the untrusted-role label sits deep in the
/// text and can land anywhere. Squeezing both haystack and needle makes the
/// match independent of where the wrap fell (and of whether the renderer
/// re-flowed at a word boundary or hard-broke mid-token).
fn squeeze(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != '│')
        .collect()
}

/// Wrap-tolerant [`TuiDeck::wait_for_grid_string_within`].
fn wait_for_wrapped_grid_string(deck: &TuiDeck, needle: &str, timeout: Duration) -> bool {
    let needle = squeeze(needle);
    common::wait_until(timeout, || squeeze(&deck.snapshot_grid()).contains(&needle))
}

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

/// Scenario: Launch the real TUI and its lazy daemon with a tiny worker-response timeout, open the two-role `orch-deck` fixture, and inject a Delegate from the orchestrator pane to the live `cat` worker over the hook socket. The worker never sends work-done, so the rendered orchestration surface must visibly carry the daemon-report clause and the worker role inside its untrusted-role-label markers after the timeout.
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

    assert!(
        wait_for_wrapped_grid_string(&deck, IDLE_DAEMON_CLAUSE, Duration::from_secs(20)),
        "the daemon-authored idle prompt never became visible in the attached orchestration \
         pane\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        wait_for_wrapped_grid_string(&deck, &idle_role_label("worker"), Duration::from_secs(20)),
        "the idle prompt did not carry the silent role inside its untrusted-role-label \
         markers\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Restore a two-role orchestration whose real interactive Claude Haiku orchestrator is directed to delegate through the `dot-agent-deck` CLI to a `cat` worker that never sends work-done. After the short detector timeout, the attached TUI must visibly render the daemon's self-identifying report clause and the worker role wrapped in its untrusted-role-label markers in the live orchestration pane.
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

    // The daemon writes this file when it dispatches a delegate, so its
    // existence proves AT LEAST ONE delegate reached the daemon. It cannot
    // prove "exactly one": a repeated delegate overwrites the same path and
    // nothing counts invocations.
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
        wait_for_wrapped_grid_string(&deck, IDLE_DAEMON_CLAUSE, Duration::from_secs(60)),
        "the real orchestrator delegated, but the daemon-authored idle nudge never became \
         visible in the attached orchestration pane\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        wait_for_wrapped_grid_string(
            &deck,
            &idle_role_label(REAL_WORKER_ROLE),
            Duration::from_secs(30)
        ),
        "the visible nudge did not carry the silent role inside the daemon's \
         untrusted-role-label markers, so it was not provably the daemon's own \
         report\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}
