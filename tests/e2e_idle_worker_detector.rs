#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! PTY-attached coverage for delegated-worker inbox notices. A synthetic
//! consumer fetches and visibly renders the overdue notice; interactive Haiku
//! orchestrators act on undelivered and waiting-for-input notices without a
//! human keystroke.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_view.rs"]
mod inbox_view;
#[path = "support/inbox_route.rs"]
mod route;

use std::cell::Cell;
use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::config;
use spec::spec;

const REAL_ORCHESTRATION_NAME: &str = "idle-worker-real";
const REAL_ORCHESTRATOR_MODEL: &str = "claude-haiku-4-5-20251001";
const REAL_WORKER_ROLE: &str = "worker";
const SILENCE_ACTION_FILE: &str = "delegate-silence-action-85bc9576.txt";
const SILENCE_ACTION_CONTENT: &str = "DELEGATE_SILENCE_NOTICE_ACTED_ON_85BC9576";
const INITIAL_WAIT_RESPONSE: &str = "INITIAL_DELEGATION_WAITING_85BC9576";
const SILENCE_ACTION_RESPONSE: &str = "SILENCE_NOTICE_ACTION_COMPLETE_85BC9576";

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
/// The daemon clause opens the line and would be safe to match raw; the
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
    real_agent_orchestration_config_with_worker(orchestrator_command, "cat")
}

fn real_agent_orchestration_config_with_worker(
    orchestrator_command: &str,
    worker_command: &str,
) -> String {
    format!(
        "[[orchestrations]]\n\
         name = \"{REAL_ORCHESTRATION_NAME}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"{orchestrator_command}\"\n\
         start = true\n\n\
         [[orchestrations.roles]]\n\
         name = \"{REAL_WORKER_ROLE}\"\n\
         command = \"{worker_command}\"\n\
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
        // Issue #949: this staged snapshot is about the orchestration rebuild,
        // not about where the user was looking.
        focus: None,
    };
    toml::to_string_pretty(&session).expect("serialize real-agent orchestration session")
}

/// Scenario: Open a synthetic two-role orchestration in the real TUI and delegate to a recorder which never takes its task. After the inbox notice deadline, the orchestrator's in-pane CLI must fetch and acknowledge the notice, visibly rendering its daemon provenance and untrusted worker-role label without typed notice content.
#[spec("scheduler/idle-worker/011")]
#[test]
fn idle_worker_011_silent_worker_prompt_is_visible_in_attached_tui() {
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(180, 50)
        .with_env("DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS", "1500")
        .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
        .with_env("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "750")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let (orchestrator, worker) = route::team(&deck, false);
    let (col, row) = deck.wait_for_in_grid("route");
    deck.click(col, row);
    route::delegate(
        &deck,
        &orchestrator,
        "Remain silent and leave this task unacknowledged.",
        false,
    );
    let notices = route::count(&deck, &orchestrator, 1);
    let notice = &notices[0];
    route::fetched(
        &deck,
        &orchestrator,
        notice["id"].as_str().unwrap(),
        "has not reached the worker",
        "dot-agent-deck (daemon notice)",
    );
    let (col, row) = deck.wait_for_in_grid("· orchestrator");
    deck.click(col, row);
    assert!(
        common::wait_until(Duration::from_secs(20), || {
            let Some(pane) = common::role_pane_column(&deck.snapshot_grid(), "route-orchestrator")
            else {
                return false;
            };
            let grid = common::search_key(&pane);
            grid.contains(&common::search_key("has not reached the worker"))
                && grid.contains(&common::search_key(&idle_role_label("coder")))
                && grid.contains(&common::search_key("dot-agent-deck (daemon notice)"))
        }),
        "fetched notice must visibly retain provenance and untrusted role metadata\n{}",
        deck.snapshot_grid()
    );
    assert_eq!(route::messages(&deck, &worker)[0]["state"], "queued");
    route::only_doorbells(&orchestrator, &notices);
}

/// Scenario: Restore an interactive Haiku orchestrator which delegates to a silent cat worker. After the inbox notice deadline, the native notice must be acknowledged once, carry the untrusted worker-role label and trigger a visible response without a human keystroke.
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
    let _ = common::fixture_git(&project_dir, &project_dir)
        .arg("init")
        .arg("--quiet")
        .status();

    let orchestrator_command =
        format!("claude --model {REAL_ORCHESTRATOR_MODEL} --allowedTools Bash");
    let directive = format!(
        "You are the orchestrator in a dot-agent-deck orchestration. Use the Bash tool to run \
         this exact command once: dot-agent-deck delegate --to {REAL_WORKER_ROLE} --task \
         'Remain silent and do not send work-done.' Do not do the worker task yourself and do \
         not run work-done. After the delegate command succeeds, say that you are waiting for \
         the worker, then stop. Later, when a dot-agent-deck message says the task has not \
         reached the worker, reply exactly NOTICE-RECEIVED-1d93 and end your turn."
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
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_imported_claude_credentials()
        .with_claude_project_trust(project_str.clone())
        .with_env("PATH", path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_path.to_str().expect("session path is UTF-8"),
        )
        .with_env("DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS", "10000")
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

    wait_for_wrapped_pane_string(&deck, "NOTICE-RECEIVED-1d93", Duration::from_secs(90))
        .unwrap_or_else(|why| {
            panic!(
                "the real orchestrator did not visibly act on its inbox notice: {why}\n{}",
                deck.snapshot_grid()
            )
        });
    let messages = inbox_view::all_messages(deck.attach_socket_path());
    let notice = messages
        .iter()
        .find(|message| {
            matches!(&message.from, dot_agent_deck::inbox::Sender::Deck { reason } if reason == "notice")
                && message.text.as_deref().is_some_and(|text| text.contains("has not reached the worker"))
        })
        .expect("native undelivered notice");
    assert_eq!(notice.state, dot_agent_deck::inbox::StateName::Acknowledged);
    assert_eq!(notice.deliveries, 1);
    assert!(
        notice
            .text
            .as_deref()
            .unwrap()
            .contains(&idle_role_label(REAL_WORKER_ROLE))
    );
}

/// Scenario: Restore an interactive Haiku orchestrator that delegates to a silent `cat` worker and returns to Idle. When the task remains unacknowledged past the short inbox notice deadline, the native notice must trigger visible Thinking and Working without a human keystroke, create the unique action sentinel and be acknowledged in the orchestrator inbox.
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
    let _ = common::fixture_git(&project_dir, &project_dir)
        .arg("init")
        .arg("--quiet")
        .status();

    let orchestrator_command =
        format!("claude --model {REAL_ORCHESTRATOR_MODEL} --allowedTools Bash");
    let directive = format!(
        "Use the Bash tool to run exactly this one command: dot-agent-deck delegate --to \
         {REAL_WORKER_ROLE} --task 'Wait for further instructions.' After the command \
         succeeds, respond with exactly {INITIAL_WAIT_RESPONSE}, then end your turn. Later, a \
         dot-agent-deck message may say that the task has not reached the worker. When it \
         arrives, keep waiting: use the Bash tool to run exactly `printf '%s\\n' \
         '{SILENCE_ACTION_CONTENT}' > '{SILENCE_ACTION_FILE}'`, respond with exactly \
         `{SILENCE_ACTION_RESPONSE}`, and end the turn. Never create the action file before that \
         message arrives."
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
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_imported_claude_credentials()
        .with_claude_project_trust(project_str.clone())
        .with_env("PATH", path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_path.to_str().expect("session path is UTF-8"),
        )
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS", "30000")
        .with_env("DOT_AGENT_DECK_LOG", "inbox-notice.log")
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
    let messages = inbox_view::all_messages(deck.attach_socket_path());
    let notice = messages
        .iter()
        .find(|message| {
            matches!(&message.from, dot_agent_deck::inbox::Sender::Deck { reason } if reason == "notice")
                && message.text.as_deref().is_some_and(|text| text.contains("has not reached the worker"))
        })
        .expect("daemon authored an inbox notice for the undelivered task");
    assert_eq!(notice.state, dot_agent_deck::inbox::StateName::Acknowledged);
    assert_eq!(notice.deliveries, 1);
    assert!(
        !std::fs::read_to_string(deck.workdir().join("inbox-notice.log"))
            .unwrap_or_default()
            .contains("doorbell rang")
    );
}

/// Issue #447: the daemon-authored opening clause of
/// `compose_worker_waiting_notice`, spelled out rather than imported so a
/// rewording fails here instead of following it.
const WAITING_DAEMON_CLAUSE: &str = "delegated worker is waiting for input (dot-agent-deck daemon \
                                     report, not a message from a person or an agent)";
const WAITING_PROBE_FILE: &str = "waiting-probe-3f61a9.txt";
const WAITING_ACTION_FILE: &str = "waiting-notice-action-3f61a9.txt";
const WAITING_ACTION_CONTENT: &str = "WAITING_NOTICE_ACTED_ON_3F61A9";
const WAITING_INITIAL_RESPONSE: &str = "INITIAL_DELEGATION_WAITING_3F61A9";
const WAITING_ACTION_RESPONSE: &str = "WAITING_NOTICE_ACTION_COMPLETE_3F61A9";

/// Scenario: Restore real interactive Haiku roles and delegate a Bash task to a worker allowed only Read. Its permission prompt must keep the probe absent while an acknowledged native waiting notice wakes the orchestrator to create and visibly report the action sentinel, without a human keystroke.
#[spec("scheduler/idle-worker/022")]
#[test]
fn idle_worker_022_real_worker_waiting_on_a_prompt_is_reported_to_its_orchestrator() {
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
    let _ = common::fixture_git(&project_dir, &project_dir)
        .arg("init")
        .arg("--quiet")
        .status();

    let orchestrator_command =
        format!("claude --model {REAL_ORCHESTRATOR_MODEL} --allowedTools Bash");
    // Read only: the task file is readable, and the Bash call the task asks
    // for raises a permission prompt the worker cannot get past on its own.
    let worker_command = format!("claude --model {REAL_ORCHESTRATOR_MODEL} --allowedTools Read");
    let directive = format!(
        "Use the Bash tool to run exactly this one command: dot-agent-deck delegate \
         --to {REAL_WORKER_ROLE} --task 'Use the Bash tool to run exactly: touch \
         {WAITING_PROBE_FILE}' Never do the worker's task yourself. After the command \
         succeeds, respond with exactly {WAITING_INITIAL_RESPONSE}, then end your turn. Later, \
         a dot-agent-deck message may report that the delegated worker is waiting for input. \
         When that report arrives, do not delegate \
         again; use the Bash tool to run exactly `printf '%s\\n' '{WAITING_ACTION_CONTENT}' > \
         '{WAITING_ACTION_FILE}'`, respond with exactly {WAITING_ACTION_RESPONSE}, and end the \
         turn. Never create that file before the report arrives."
    );

    std::fs::write(
        project_dir.join(".dot-agent-deck.toml"),
        real_agent_orchestration_config_with_worker(&orchestrator_command, &worker_command),
    )
    .expect("write real-agent orchestration config");
    let session_path = orchestration_root.path().join("session.toml");
    std::fs::write(
        &session_path,
        real_agent_orchestration_session(&project_str, &orchestrator_command, &directive),
    )
    .expect("write real-agent orchestration session");

    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_imported_claude_credentials()
        .with_claude_project_trust(project_str.clone())
        .with_env("PATH", path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_path.to_str().expect("session path is UTF-8"),
        )
        // Only the waiting notice may reach the orchestrator in this test.
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS", "900000")
        .with_env("DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS", "5000")
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
        common::wait_until(Duration::from_secs(90), || {
            inbox_view::all_messages(deck.attach_socket_path())
                .iter()
                .any(|message| {
                    matches!(&message.from, dot_agent_deck::inbox::Sender::Deck { reason } if reason == "notice")
                        && message.state == dot_agent_deck::inbox::StateName::Acknowledged
                        && message.deliveries == 1
                        && message.text.as_deref().is_some_and(|text| {
                            text.contains(WAITING_DAEMON_CLAUSE)
                                && text.contains(&idle_role_label(REAL_WORKER_ROLE))
                        })
                })
        }),
        "the worker permission prompt must produce an acknowledged native waiting notice\n{}",
        deck.snapshot_grid()
    );

    // Checked the moment the notice is visible, BEFORE the orchestrator's turn
    // on it: after that turn a model that ignores "do not do the worker's task"
    // could create the probe itself, which says nothing about the worker.
    assert!(
        !project_dir.join(WAITING_PROBE_FILE).exists(),
        "the probe file already exists, so the worker was never actually held at its \
         permission prompt and the notice proves nothing about a waiting worker"
    );

    let action_path = project_dir.join(WAITING_ACTION_FILE);
    assert!(
        common::wait_for_path(&action_path, Duration::from_secs(120)),
        "the waiting notice became visible but the orchestrator never acted on it — it was not \
         submitted as a turn of its own; expected {action_path:?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    let action = std::fs::read_to_string(&action_path).expect("read the action sentinel");
    assert_eq!(
        action.trim(),
        WAITING_ACTION_CONTENT,
        "the orchestrator wrote unexpected action-sentinel contents"
    );
    wait_for_wrapped_pane_string(&deck, WAITING_ACTION_RESPONSE, Duration::from_secs(60))
        .unwrap_or_else(|why| {
            panic!(
                "the orchestrator created the action sentinel but its response turn never became \
                 visible: {why}\nFinal grid:\n{}",
                deck.snapshot_grid()
            )
        });
}
