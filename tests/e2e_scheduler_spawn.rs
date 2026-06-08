#![cfg(feature = "e2e")]

//! L2 spawn-primitive tests for the daemon-hosted scheduler (PRD #127 Phase 2A,
//! M2.1 + M2.3). A scheduled fire (cron tick or run-now) must call the spawn
//! primitive once: auto-create the working_dir, branch on the target dir's
//! `.dot-agent-deck.toml` (orchestration tab vs single-agent card), spawn the
//! task's required `command`, and deliver the prompt into the PTY.
//!
//! Per the task's LATITUDE CLAUSE, these tests observe ONLY externally-visible
//! behavior — the daemon's agent registry (`ListAgents` / `AttachResponse`),
//! the spawned PTY's output stream (prompt echo), on-disk effects (mkdir,
//! command side effects), the failure-surfacing notifier (daemon stderr), and
//! daemon liveness. They drive the existing `RunNow` control message and do NOT
//! reference any internal `spawn()` signature, so the coder is free to choose
//! the daemon-side integration. They are RED today because
//! `make_schedule_callback` is still a logging no-op (no agent is ever spawned).

mod common;

use std::time::Duration;

use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

const PROMPT_MARKER: &str = "SCHEDPROMPTMARKER";

/// Build one `[[scheduled_tasks]]` block. `command` is written into the TOML
/// when `Some` and omitted when `None`.
fn task_block(name: &str, working_dir: &str, command: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("[[scheduled_tasks]]\n");
    s.push_str(&format!("name = \"{name}\"\n"));
    // A schedule that will not fire on its own during the test window; the
    // tests trigger fires explicitly via run-now.
    s.push_str("cron = \"0 0 1 1 *\"\n");
    s.push_str(&format!("working_dir = \"{working_dir}\"\n"));
    if let Some(cmd) = command {
        s.push_str(&format!("command = \"{cmd}\"\n"));
    }
    s.push_str(&format!("prompt = \"{PROMPT_MARKER}\"\n"));
    s.push_str("enabled = true\n\n");
    s
}

/// Scenario: Register one task whose `working_dir` does not exist, one whose
/// `working_dir` is uncreatable (its parent is a regular file), and one control
/// task with a valid dir. Fire the missing-dir task via run-now and assert the
/// directory is created (`mkdir -p`) and an agent is spawned. Fire the
/// uncreatable task and assert the daemon does NOT crash, the path is not
/// created, a failure notification is surfaced (daemon stderr), and the control
/// task still spawns afterward — proving one bad fire doesn't wedge the daemon.
#[spec("scheduler/spawn/001")]
#[test]
fn spawn_001_mkdir_and_uncreatable_path() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let base = scratch.path();

    let missing_dir = base.join("created-on-fire");
    let good_dir = base.join("good");
    std::fs::create_dir_all(&good_dir).expect("create good dir");
    // A regular file whose "subdir" can never be created (ENOTDIR).
    let blocking_file = base.join("not-a-dir");
    std::fs::write(&blocking_file, b"x").expect("write blocking file");
    let uncreatable = blocking_file.join("sub");

    let mut toml = String::new();
    toml.push_str(&task_block(
        "mk",
        &missing_dir.to_string_lossy(),
        Some("cat"),
    ));
    toml.push_str(&task_block(
        "bad",
        &uncreatable.to_string_lossy(),
        Some("cat"),
    ));
    toml.push_str(&task_block(
        "good",
        &good_dir.to_string_lossy(),
        Some("cat"),
    ));

    let mut daemon = common::spawn_daemon_serve(Some(&toml), "0");

    // Missing dir: a fire creates it, then spawns.
    daemon.run_now("mk").expect("run-now mk");
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert!(
        missing_dir.is_dir(),
        "fire into a missing working_dir must create it (mkdir -p)"
    );
    assert!(
        !records.is_empty(),
        "fire into a missing working_dir must spawn an agent, got {records:?}"
    );

    let count_before_bad = daemon.agent_records().len();

    // Uncreatable dir: a fire surfaces a notification and does not crash.
    daemon.run_now("bad").expect("run-now bad");
    assert!(
        daemon.wait_for_stderr_contains("sub", Duration::from_secs(10)),
        "uncreatable working_dir should surface a failure notification mentioning the path"
    );
    assert!(
        !uncreatable.exists(),
        "the uncreatable path must not have been created"
    );
    assert!(
        daemon.is_alive_public(),
        "daemon must not crash on a working_dir creation failure"
    );

    // Other tasks keep working after the bad fire.
    daemon.run_now("good").expect("run-now good");
    let after = daemon.wait_for_agent_count(count_before_bad + 1, Duration::from_secs(10));
    assert!(
        after.len() > count_before_bad,
        "a healthy task must still spawn after a sibling task's mkdir failure"
    );
}

/// Scenario: Register one task whose `working_dir` contains a
/// `.dot-agent-deck.toml` with an `[[orchestrations]]` block (orchestrator role
/// runs `cat`), and one task whose `working_dir` has no config. Fire both via
/// run-now. Assert the orchestration fire produces an agent in the registry
/// tagged as the `orchestrator` role of an orchestration tab and that the
/// prompt is delivered to it (echoed by its PTY); assert the plain fire
/// produces a non-orchestration single-agent card with the prompt delivered.
#[spec("scheduler/spawn/002")]
#[test]
fn spawn_002_orchestration_vs_single_agent() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let base = scratch.path();

    // Orchestration target dir.
    let orch_dir = base.join("orch");
    std::fs::create_dir_all(&orch_dir).expect("create orch dir");
    std::fs::write(
        orch_dir.join(".dot-agent-deck.toml"),
        "[[orchestrations]]\nname = \"digest\"\n\n\
         [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n",
    )
    .expect("write orchestration config");

    // Plain single-agent target dir (no .dot-agent-deck.toml).
    let single_dir = base.join("single");
    std::fs::create_dir_all(&single_dir).expect("create single dir");

    let mut toml = String::new();
    // `command` is required for every task to LOAD, even an orchestration
    // target whose fire is driven by the target dir's role command (so this
    // value is ignored at fire time). Without it the loader skips the entry and
    // `run-now orch` would error on an unknown task.
    toml.push_str(&task_block(
        "orch",
        &orch_dir.to_string_lossy(),
        Some("cat"),
    ));
    toml.push_str(&task_block(
        "single",
        &single_dir.to_string_lossy(),
        Some("cat"),
    ));

    let daemon = common::spawn_daemon_serve(Some(&toml), "0");

    // Orchestration fire → orchestrator-role agent in an orchestration tab.
    daemon.run_now("orch").expect("run-now orch");
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    let orchestrator = records.iter().find(|r| {
        matches!(
            &r.tab_membership,
            Some(TabMembership::Orchestration { role_name, .. }) if role_name == "orchestrator"
        )
    });
    let orchestrator = orchestrator.unwrap_or_else(|| {
        panic!("orchestration fire must spawn an `orchestrator` role pane, got {records:?}")
    });
    assert!(
        daemon.attach_and_wait_for_output(&orchestrator.id, PROMPT_MARKER, Duration::from_secs(10)),
        "the prompt must be delivered to the orchestrator role pane"
    );

    // Plain fire → single-agent (non-orchestration) card.
    daemon.run_now("single").expect("run-now single");
    let single = daemon
        .wait_for_agent_where(
            |r| !matches!(r.tab_membership, Some(TabMembership::Orchestration { .. })),
            Duration::from_secs(10),
        )
        .expect("plain fire must spawn a non-orchestration single-agent card");
    assert!(
        daemon.attach_and_wait_for_output(&single.id, PROMPT_MARKER, Duration::from_secs(10)),
        "the prompt must be delivered to the single-agent card"
    );
}

/// Scenario: Register one task whose explicit `command` touches a unique marker
/// file on startup. Fire it via run-now and assert the marker appears — proving
/// the scheduler spawns the configured `command` itself. (The former
/// omitted-command -> `$SHELL` fallback no longer exists: `command` is now a
/// required field, so there is no implicit-shell case left to exercise.)
#[spec("scheduler/spawn/003")]
#[test]
fn spawn_003_command_is_honored() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let base = scratch.path();

    let cmd_dir = base.join("cmd");
    std::fs::create_dir_all(&cmd_dir).expect("create cmd dir");

    let cmd_marker = base.join("CMD_RAN");

    let explicit_command = format!(
        "touch \\\"{}\\\"; exec sleep 30",
        cmd_marker.to_string_lossy()
    );

    let toml = task_block(
        "with-cmd",
        &cmd_dir.to_string_lossy(),
        Some(&explicit_command),
    );
    let daemon = common::spawn_daemon_serve(Some(&toml), "0");

    daemon.run_now("with-cmd").expect("run-now with-cmd");

    assert!(
        common::wait_for_path(&cmd_marker, Duration::from_secs(10)),
        "the configured command must be the process spawned (CMD_RAN marker)"
    );
}

/// Scenario: Register a single task and fire it once via run-now. Assert
/// exactly one agent is spawned (no double-spawn) and the configured prompt is
/// delivered to it (echoed by its PTY).
#[spec("scheduler/spawn/004")]
#[test]
fn spawn_004_fires_spawn_exactly_once_and_delivers() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).expect("create work dir");

    let toml = task_block("once", &work.to_string_lossy(), Some("cat"));
    let daemon = common::spawn_daemon_serve(Some(&toml), "0");

    daemon.run_now("once").expect("run-now once");

    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(
        records.len(),
        1,
        "a single fire must spawn exactly one agent, got {records:?}"
    );
    // Hold the line: no second tab appears shortly after.
    daemon.assert_agent_count_stays_at_most(1, Duration::from_secs(2));

    let agent = &daemon.agent_records()[0];
    assert!(
        daemon.attach_and_wait_for_output(&agent.id, PROMPT_MARKER, Duration::from_secs(10)),
        "the configured prompt must be delivered to the spawned agent"
    );
}
