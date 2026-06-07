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

/// Scenario: Start `daemon serve` (idle disabled) with no schedules. From an
/// arbitrary cwd, run `dot-agent-deck schedule add` with a complete, valid set
/// of flags (--name, --cron, --working-dir, --prompt, --enabled) but
/// deliberately NO --command. Assert the writer REJECTS it: the process exits
/// non-zero and prints a clear error to stderr indicating that --command is
/// required (a scheduled task needs an agent command to act on its prompt — a
/// silent fallback to a bare $SHELL cannot). This is the exact invocation
/// `cli_002` proves succeeds today, so asserting failure pins the new
/// required-command contract.
#[spec("scheduler/cli/003")]
#[test]
fn cli_003_add_requires_command() {
    let daemon = common::spawn_daemon_serve(None, "0");

    // An arbitrary working directory, deliberately not the global config dir.
    let cwd = common::race_safe_tempdir();

    // Same complete invocation as cli_002, MINUS --command.
    let out = daemon.run_schedule_cli_from(
        cwd.path(),
        &[
            "add",
            "--name",
            "needs-command",
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

    // (a) Non-zero exit: the writer must reject a missing --command instead of
    // silently accepting it (which later falls back to a bare $SHELL).
    assert!(
        !out.status.success(),
        "schedule add without --command should FAIL with a non-zero exit, but it \
         exited successfully.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // (b) Clear, observable error naming --command as the required field.
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("command") && stderr.contains("required"),
        "stderr should clearly state that --command is required, got:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}
