#![cfg(feature = "e2e")]

//! L2 CLI-writer test for the daemon-hosted scheduler.
//! PRD #127 catalog `scheduler/cli/002` (M1.5).
//!
//! `dot-agent-deck schedule add` is the single validated writer: it must write
//! to the fixed global `schedules.toml` regardless of cwd and then trigger a
//! daemon reload so the running daemon picks the new task up live.

mod common;

use spec::spec;

/// Scenario: Start `daemon serve` (idle disabled) with no schedules. From an
/// arbitrary cwd that is NOT the global config dir, run `dot-agent-deck
/// schedule add --name cli-task …` with `DOT_AGENT_DECK_SCHEDULES` pointing at
/// the global path. Assert the command succeeds, the entry lands in the global
/// `schedules.toml` (and nothing is written under the cwd), and the running
/// daemon registers the task via the add-triggered reload — probed by
/// `schedule run-now --name cli-task` exiting cleanly (it errors on an unknown
/// task).
#[spec("scheduler/cli/002")]
#[test]
fn cli_002_add_writes_global_and_daemon_reloads() {
    let daemon = common::spawn_daemon_serve(None, "0");

    // An arbitrary working directory, deliberately not the global config dir.
    let cwd = common::race_safe_tempdir();

    let out = daemon.run_schedule_cli_from(
        cwd.path(),
        &[
            "add",
            "--name",
            "cli-task",
            "--cron",
            "*/5 * * * * *",
            "--working-dir",
            "/tmp",
            "--prompt",
            "scheduled hello",
            "--enabled",
            "true",
        ],
    );
    assert!(
        out.status.success(),
        "schedule add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // (a) Entry written to the GLOBAL path, regardless of cwd.
    let global =
        std::fs::read_to_string(&daemon.schedules_path).expect("global schedules.toml exists");
    assert!(
        global.contains("cli-task"),
        "global schedules.toml should contain the new task, got:\n{global}"
    );
    assert!(
        !cwd.path().join("schedules.toml").exists(),
        "the writer must not drop a schedules.toml under the cwd"
    );

    // (b) Running daemon picked it up via the add-triggered reload.
    assert!(
        daemon.wait_for_schedule_registered("cli-task"),
        "daemon did not register cli-task via the add-triggered reload"
    );
}
