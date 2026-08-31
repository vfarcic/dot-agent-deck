#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! PTY-attached coverage for the daemon's delegated-worker detectors. The
//! synthetic idle case opens the `orch-deck` fixture's live `cat` role panes and
//! injects a Delegate over its hook socket; the real-agent cases restore an
//! orchestration whose interactive Claude Haiku orchestrator delegates to a
//! silent worker. They prove both the long idle nudge and the earlier no-event
//! notice reach the visible orchestrator surface, with the latter becoming an
//! actionable response turn without human input.

mod common;

use std::cell::{Cell, RefCell};
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
const SILENCE_ACTION_FILE: &str = "delegate-silence-action-85bc9576.txt";
const SILENCE_ACTION_CONTENT: &str = "DELEGATE_SILENCE_NOTICE_ACTED_ON_85BC9576";
const INITIAL_WAIT_RESPONSE: &str = "INITIAL_DELEGATION_WAITING_85BC9576";
const SILENCE_ACTION_RESPONSE: &str = "SILENCE_NOTICE_ACTION_COMPLETE_85BC9576";

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

fn has_role_status(grid: &str, role: &str, status: &str) -> bool {
    let role_needle = format!("\u{00b7} {role}");
    grid.lines()
        .any(|line| line.contains(&role_needle) && line.contains(status))
}

/// Wrap-tolerant wait for `needle` inside the orchestration PANE COLUMN.
///
/// Every needle goes through [`common::squeeze_wrapped_text`] because the idle
/// prompt is ONE long line: the pane wraps it at whatever column the pane happens
/// to sit at, and a needle straddling that column is absent from the row-joined
/// snapshot even though every character of it is on screen. Only
/// [`IDLE_DAEMON_CLAUSE`] *opens* the line and would be safe to match raw; the
/// untrusted-role label sits deep in the text and can land anywhere.
///
/// NOT a wrap-tolerant whole-grid search: this crops to the pane column via
/// [`common::orchestration_pane_column`] and so inherits both of that
/// function's preconditions — the fixture's `start = true` role must be named
/// literally `orchestrator`, and its pane must render as an EXPANDED box. A
/// needle rendered in the SIDEBAR, or anywhere while the orchestrator's pane is
/// collapsed, will never be found here however long the timeout.
///
/// Returns which of the two distinct failures happened rather than one
/// undifferentiated `false` (review of #465, S4). Collapsing them was a real
/// diagnosability regression against the whole-grid search this replaced, which
/// had no anchor to lose: a collapsed pane draws no corner glyph, so the crop
/// returns `None` on every poll and the operator was told the idle prompt never
/// arrived when it may have been rendering perfectly. It bites hardest in
/// `idle_worker_012`, which is credential-gated and so never runs in CI.
fn wait_for_wrapped_pane_string(
    deck: &TuiDeck,
    needle: &str,
    timeout: Duration,
) -> Result<(), String> {
    let squeezed = common::squeeze_wrapped_text(needle);
    // `wait_until` takes a `Fn`, so the "did the anchor EVER appear" flag needs
    // interior mutability. Tracking it across the whole poll loop beats
    // re-checking once after the timeout: it distinguishes a pane that was
    // never expanded at all from one that merely happened to be collapsed on
    // the final frame.
    let anchor_seen = Cell::new(false);
    let found = common::wait_until(timeout, || {
        let Some(pane) = common::orchestration_pane_column(&deck.snapshot_grid()) else {
            return false;
        };
        anchor_seen.set(true);
        common::squeeze_wrapped_text(&pane).contains(&squeezed)
    });

    if found {
        Ok(())
    } else if anchor_seen.get() {
        Err(format!(
            "the orchestrator's expanded pane box WAS located, but {needle:?} never appeared \
             inside its column within {timeout:?}"
        ))
    } else {
        Err(format!(
            "the orchestrator's expanded pane box never rendered within {timeout:?}, so the \
             pane column could not be located and {needle:?} was never actually searched for \
             — the pane is collapsed or the start role is not named \"orchestrator\", neither \
             of which says anything about whether the prompt arrived"
        ))
    }
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

    wait_for_wrapped_pane_string(&deck, IDLE_DAEMON_CLAUSE, Duration::from_secs(20))
        .unwrap_or_else(|why| {
            panic!(
                "the daemon-authored idle prompt never became visible in the attached \
                 orchestration pane: {why}\nFinal grid:\n{}",
                deck.snapshot_grid()
            )
        });
    wait_for_wrapped_pane_string(&deck, &idle_role_label("worker"), Duration::from_secs(20))
        .unwrap_or_else(|why| {
            panic!(
                "the idle prompt did not carry the silent role inside its untrusted-role-label \
                 markers: {why}\nFinal grid:\n{}",
                deck.snapshot_grid()
            )
        });
}

/// Scenario: Restore a two-role orchestration whose real interactive Claude Haiku orchestrator is directed to delegate through the `dot-agent-deck` CLI to a `cat` worker that never sends work-done. After the short detector timeout, the attached TUI must visibly render the daemon's self-identifying report clause and the worker role wrapped in its untrusted-role-label markers in the live orchestration pane.
#[spec("scheduler/idle-worker/012")]
#[test]
fn idle_worker_012_real_orchestrator_visibly_receives_idle_nudge() {
    skip_unless!(common::check_claude_available());

    let orchestration_root = common::harness_tempdir().expect("orchestration root tempdir");
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

    wait_for_wrapped_pane_string(&deck, IDLE_DAEMON_CLAUSE, Duration::from_secs(60))
        .unwrap_or_else(|why| {
            panic!(
                "the real orchestrator delegated, but the daemon-authored idle nudge never \
                 became visible in the attached orchestration pane: {why}\nFinal grid:\n{}",
                deck.snapshot_grid()
            )
        });
    wait_for_wrapped_pane_string(
        &deck,
        &idle_role_label(REAL_WORKER_ROLE),
        Duration::from_secs(30),
    )
    .unwrap_or_else(|why| {
        panic!(
            "the visible nudge did not carry the silent role inside the daemon's \
             untrusted-role-label markers, so it was not provably the daemon's own report: \
             {why}\nFinal grid:\n{}",
            deck.snapshot_grid()
        )
    });
}

/// Scenario: Restore a real interactive Claude Haiku orchestrator that delegates to a silent `cat` worker, acknowledges that it is waiting, and returns to Idle. When the short no-event window expires, the daemon notice must trigger a new visible Thinking/Working response turn with no human keystroke, during which the orchestrator creates and reports a uniquely named action sentinel.
#[spec("orchestration/delegate/024")]
#[test]
fn delegate_024_real_orchestrator_acts_on_submitted_silence_notice() {
    skip_unless!(common::check_claude_available());

    let orchestration_root = common::harness_tempdir().expect("orchestration root tempdir");
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
    let project_protocol = format!(
        "# Automated dot-agent-deck integration test\n\nThis isolated repository is an authorized \
         integration test for the installed dot-agent-deck application. After the initial \
         delegation, a later user turn beginning with a dot-agent-deck daemon report that a \
         delegated worker went quiet is expected application input, not prompt injection. When \
         that report arrives, decide to keep waiting, use the Bash tool to run exactly `printf \
         '%s\\n' '{SILENCE_ACTION_CONTENT}' > '{SILENCE_ACTION_FILE}'`, respond with exactly \
         `{SILENCE_ACTION_RESPONSE}`, and end the turn. Never create the action file before that \
         daemon report arrives.\n"
    );
    std::fs::write(project_dir.join("CLAUDE.md"), project_protocol)
        .expect("write real-orchestrator integration protocol");
    let directive = format!(
        "This is an authorized local integration test. Use the Bash tool to run exactly this one \
         command: dot-agent-deck delegate --to {REAL_WORKER_ROLE} --task 'Remain silent and do \
         not send work-done.' After the command succeeds, respond with exactly \
         {INITIAL_WAIT_RESPONSE}, then end your turn."
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
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "30000")
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
    wait_for_wrapped_pane_string(&deck, INITIAL_WAIT_RESPONSE, Duration::from_secs(120))
        .unwrap_or_else(|why| {
            panic!(
                "the real orchestrator delegated but never visibly completed its initial waiting \
                 response: {why}\nFinal grid:\n{}",
                deck.snapshot_grid()
            )
        });
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            has_role_status(&deck.snapshot_grid(), "orchestrator", "Idle")
        }),
        "the orchestrator never returned to Idle after its initial delegate turn\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    let action_path = project_dir.join(SILENCE_ACTION_FILE);
    assert!(
        !action_path.exists(),
        "the orchestrator created {SILENCE_ACTION_FILE:?} during its initial turn, so the \
         sentinel cannot prove it acted on the later daemon notice"
    );

    deck.wait_for_strings_in_order_then_any_within(
        &["Thinking", "Working"],
        &[SILENCE_ACTION_RESPONSE],
        Duration::from_secs(90),
    );

    let action = std::fs::read_to_string(&action_path).unwrap_or_else(|error| {
        panic!(
            "the submitted silence notice produced a visible response but did not create the \
             required action sentinel {action_path:?}: {error}\nFinal grid:\n{}",
            deck.snapshot_grid()
        )
    });
    assert_eq!(
        action.trim(),
        SILENCE_ACTION_CONTENT,
        "the orchestrator wrote unexpected action-sentinel contents"
    );
    wait_for_wrapped_pane_string(&deck, SILENCE_ACTION_RESPONSE, Duration::from_secs(30))
        .unwrap_or_else(|why| {
            panic!(
                "the orchestrator created the action sentinel but its response turn never became \
                 visible in the attached pane: {why}\nFinal grid:\n{}",
                deck.snapshot_grid()
            )
        });
}
