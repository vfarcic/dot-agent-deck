//! upstream #331 — the CLI-side half of the work-done output-path collision.
//!
//! `handle_work_done` (`src/state.rs`) writes its per-role report
//! unconditionally to a path derived from role name + cwd, with no existence
//! check. Issue #331's own proposed fix is narrower than the fix required for
//! the daemon-side clobber (see `delegate_prompt_injection.rs`'s
//! `delegate_020_existing_report_survives_and_collision_is_announced`, which
//! pins the general case), but it is still worth having on its own: a
//! `dot-agent-deck work-done --task-file <path>` invocation whose `--path`
//! resolves into the daemon's own `.dot-agent-deck/work-done-*.md`
//! namespace is a literal, mechanically-preventable setup for exactly that
//! clobber — the CLI can refuse it client-side, before ever reading the file
//! or contacting the daemon.
//!
//! Real subprocess technique (not an in-process `handle_work_done` call):
//! this behavior lives in the CLI argument-handling arm in `src/main.rs`, so
//! the only way to observe it is to run the actual compiled binary. A real
//! daemon is spawned so the two cases are told apart by EXIT STATUS alone,
//! not by wording: if the CLI does not refuse client-side, the reachable
//! daemon accepts the message and the process exits 0; a real refusal exits
//! non-zero before ever touching the socket.

mod common;

use std::process::{Command, Output};

use dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID;
use spec::spec;

/// Run `dot-agent-deck work-done --task-file <relative_task_file>` from
/// `cwd`, wired at the real daemon's socket so a non-refusing CLI would
/// succeed. Env is intentionally minimal (PATH, a fresh HOME, the pane id,
/// and the daemon socket) so nothing outside the check under test can
/// influence the outcome.
#[cfg(unix)]
fn run_work_done_task_file(
    daemon: &common::DaemonProc,
    cwd: &std::path::Path,
    home: &std::path::Path,
    relative_task_file: &str,
) -> Output {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let mut cmd = Command::new(bin);
    cmd.arg("work-done");
    cmd.arg("--task-file").arg(relative_task_file);
    cmd.current_dir(cwd);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", home);
    cmd.env(DOT_AGENT_DECK_PANE_ID, "cli-refusal-test-pane");
    cmd.env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket);
    cmd.output()
        .expect("run `dot-agent-deck work-done` subprocess")
}

/// Scenario: Start a real daemon, then run `dot-agent-deck work-done
/// --task-file .dot-agent-deck/work-done-coder.md` from a cwd where that
/// exact file already exists — a `--task-file` argument that resolves
/// straight into the daemon's own output namespace. The CLI must refuse and
/// exit non-zero WITHOUT ever reaching the (reachable) daemon; a second,
/// otherwise-identical call whose `--task-file` points outside that
/// namespace must still succeed, proving the refusal is scoped rather than a
/// blanket rejection of `--task-file`.
#[cfg(unix)]
#[spec("orchestration/delegate/021")]
#[test]
fn delegate_021_work_done_refuses_task_file_inside_own_output_namespace() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let cwd = common::race_safe_tempdir();
    let home = cwd.path().join("home");
    std::fs::create_dir_all(&home).expect("create fresh HOME for the CLI subprocess");
    let dot_dir = cwd.path().join(".dot-agent-deck");
    std::fs::create_dir_all(&dot_dir).expect("create .dot-agent-deck");

    // A file sitting exactly where the daemon's own work-done output lives
    // today (role-keyed: `work-done-<role>.md`).
    std::fs::write(dot_dir.join("work-done-coder.md"), "some report content")
        .expect("seed collision file");

    let refused = run_work_done_task_file(
        &daemon,
        cwd.path(),
        &home,
        ".dot-agent-deck/work-done-coder.md",
    );
    assert!(
        !refused.status.success(),
        "`work-done --task-file` pointing INSIDE the daemon's own \
         `.dot-agent-deck/work-done-*.md` output namespace exited successfully instead of \
         being refused — the daemon IS reachable in this test, so a non-refusing CLI \
         forwards the message and exits 0. This --task-file was set up to be overwritten \
         by the daemon's own next work-done write.\nstdout: {:?}\nstderr: {:?}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr),
    );

    // Control: an otherwise-identical call whose --task-file points OUTSIDE
    // the daemon's own namespace must still be accepted.
    std::fs::write(
        cwd.path().join("worker-report.md"),
        "an ordinary worker report",
    )
    .expect("seed non-collision file");
    let allowed = run_work_done_task_file(&daemon, cwd.path(), &home, "worker-report.md");
    assert!(
        allowed.status.success(),
        "a --task-file OUTSIDE the daemon's own output namespace must still be accepted; \
         the refusal must be scoped to the collision path, not a blanket rejection of \
         --task-file.\nstdout: {:?}\nstderr: {:?}",
        String::from_utf8_lossy(&allowed.stdout),
        String::from_utf8_lossy(&allowed.stderr),
    );
}
