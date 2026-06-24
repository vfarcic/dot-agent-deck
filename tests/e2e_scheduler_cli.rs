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
            "--command",
            "claude",
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

/// Scenario: Start `daemon serve` (idle disabled) with no schedules. From an
/// arbitrary cwd, run `dot-agent-deck schedule add` with the PRD #120
/// issue-dispatch flags — `--repo acme/widgets --max-per-run 2 --label
/// agent-eligible --query "is:open label:bug"` plus name/cron/working-dir/prompt
/// — and with `--command` deliberately OMITTED (optional for this task type: the
/// per-issue command comes from each cloned repo's config). Assert the command
/// succeeds, the global `schedules.toml` gains a `[scheduled_tasks.issue_dispatch]`
/// sub-table whose repo/max_per_run/label/query round-trip back into an
/// `IssueDispatchConfig` through the loader, and the running daemon registers the
/// task via the add-triggered reload. Then assert a malformed `--repo` (not
/// `owner/name`) exits non-zero with a clear error. RED today: `schedule add` has
/// no `--repo`/`--max-per-run`/`--label`/`--query` flags, so clap rejects the
/// unknown `--repo` argument and the add exits non-zero before any file write.
#[spec("scheduler/cli/004")]
#[test]
fn cli_004_add_issue_dispatch_writes_table_and_reloads() {
    let daemon = common::spawn_daemon_serve(None, "0");

    // An arbitrary working directory, deliberately not the global config dir.
    let cwd = common::race_safe_tempdir();

    // The happy path: an issue-dispatch schedule, authored WITHOUT --command.
    let out = daemon.run_schedule_cli_from(
        cwd.path(),
        &[
            "add",
            "--name",
            "issues-task",
            "--cron",
            "0 9 * * *",
            "--working-dir",
            "/tmp",
            "--prompt",
            "fix {{issue_number}}",
            "--repo",
            "acme/widgets",
            "--max-per-run",
            "2",
            "--label",
            "agent-eligible",
            "--query",
            "is:open label:bug",
        ],
    );
    assert!(
        out.status.success(),
        "schedule add with the issue-dispatch flags (and no --command) should succeed, but it \
         exited non-zero.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // (a) The written TOML carries a [scheduled_tasks.issue_dispatch] sub-table...
    let global =
        std::fs::read_to_string(&daemon.schedules_path).expect("global schedules.toml exists");
    assert!(
        global.contains("[scheduled_tasks.issue_dispatch]"),
        "the written schedules.toml must carry an [scheduled_tasks.issue_dispatch] sub-table, \
         got:\n{global}"
    );

    // ...and every issue-dispatch field round-trips back through the loader into
    // an `IssueDispatchConfig` (repo / max_per_run / label / query).
    let loaded = dot_agent_deck::config::LoadedSchedules::parse(&global);
    assert!(
        loaded.errors.is_empty(),
        "the written issue-dispatch task must round-trip with no load errors, got: {:?}",
        loaded.errors
    );
    let task = loaded
        .tasks
        .iter()
        .find(|t| t.name == "issues-task")
        .expect("the written issues-task must be present after round-trip");
    let disp = task
        .issue_dispatch
        .as_ref()
        .expect("issue_dispatch table present → issue-dispatch task");
    assert_eq!(disp.repo, "acme/widgets", "repo must round-trip");
    assert_eq!(disp.max_per_run, 2, "max_per_run must round-trip");
    assert_eq!(
        disp.label.as_deref(),
        Some("agent-eligible"),
        "label must round-trip"
    );
    assert_eq!(
        disp.query.as_deref(),
        Some("is:open label:bug"),
        "query must round-trip"
    );

    // (b) The running daemon registered the task via the add-triggered reload.
    assert!(
        daemon.wait_for_schedule_registered("issues-task"),
        "daemon did not register issues-task via the add-triggered reload"
    );

    // (c) A malformed --repo (not `owner/name`) is rejected with a clear error.
    let bad = daemon.run_schedule_cli_from(
        cwd.path(),
        &[
            "add",
            "--name",
            "bad-repo-task",
            "--cron",
            "0 9 * * *",
            "--working-dir",
            "/tmp",
            "--prompt",
            "fix {{issue_number}}",
            "--repo",
            "not-a-valid-slug",
            "--max-per-run",
            "1",
        ],
    );
    assert!(
        !bad.status.success(),
        "schedule add with a malformed --repo should FAIL with a non-zero exit, but it \
         succeeded.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr),
    );
    let bad_stderr = String::from_utf8_lossy(&bad.stderr).to_lowercase();
    assert!(
        bad_stderr.contains("repo")
            && (bad_stderr.contains("owner/name")
                || bad_stderr.contains("slug")
                || bad_stderr.contains("owner")),
        "stderr should clearly state that --repo must be a GitHub owner/name slug, got:\n{}",
        String::from_utf8_lossy(&bad.stderr),
    );
}
