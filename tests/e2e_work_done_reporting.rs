#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use inbox::Endpoint;
use serde_json::{Value, json};
use spec::spec;
use std::io::{BufRead, Write};
use std::path::Path;

/// Scenario: Run the real work-done CLI from an uncommissioned synthetic worker. One bound Untrusted report is fetched and rendered in the orchestrator pane, labelled as a note that completes no task, with no commissioned summary file.
#[spec("orchestration/work-done/004")]
#[test]
fn work_done_004_unsolicited_completion_is_visibly_labelled_in_the_attached_tui() {
    route::worker_report(4);
}

/// Scenario: Send an unsolicited worker report larger than the inline budget through the real CLI. The orchestrator fetches its framed head and saved-copy path from the inbox; that saved file holds the entire Untrusted-fenced report.
#[spec("orchestration/work-done/009")]
#[test]
fn work_done_009_a_cut_unsolicited_report_names_its_saved_full_copy_in_the_attached_tui() {
    route::worker_report(9);
}

/// Scenario: Delegate a task and pass a worker-owned summary file to the real work-done CLI. The source survives byte for byte, a separately framed report copy is saved, and one bound report is fetched and rendered in the orchestrator pane.
#[spec("orchestration/work-done/012")]
#[test]
fn work_done_012_task_file_at_the_summary_path_survives_in_the_attached_tui() {
    route::worker_report(12);
}

// PRD #1666 part 3, round 5: the real CLI's task-scoped `work-done`, against
// the real daemon (`/033`–`/036`, `/040`) and against a stub
// (`inbox/protocol/029`).

/// The directory the daemon's work-done seams read, under its HOME
/// (`DOT_AGENT_DECK_TEST_WORK_DONE_PAUSE_DIR`).
const SEAMS: &str = "work-done-seams";

/// A daemon whose work-done seams the test can pause and fail.
fn seamed_daemon() -> common::DaemonProc {
    inbox::daemon_unbound(&[("DOT_AGENT_DECK_TEST_WORK_DONE_PAUSE_DIR", SEAMS)])
}

fn seam(daemon: &impl Endpoint, file: &str) -> std::path::PathBuf {
    let dir = daemon.home().join(SEAMS);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(file)
}

/// A team whose worker has fetched and acknowledged one delegated task; the
/// task's id, as the worker's instructions name it.
fn delegated(daemon: &impl Endpoint) -> (inbox::Pane, inbox::Pane, String) {
    let (orch, worker) = route::team(daemon, false);
    let id = route::delegate(daemon, &orch, route::TASK, false);
    let block = route::fetched(daemon, &worker, &id, route::TASK, "the orchestrator");
    let task_id = footer_task_id(daemon, &block);
    assert_eq!(task_id, id, "the footer names the delegated message's id");
    (orch, worker, task_id)
}

/// The id after `--task-id ` in the delegated message, or else in the task
/// file the deck wrote for the worker.
fn footer_task_id(daemon: &impl Endpoint, block: &str) -> String {
    let find = |text: &str| {
        text.split("--task-id ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .map(|id| id.trim_matches(['`', '\'', '"']).to_string())
    };
    find(block)
        .or_else(|| {
            std::fs::read_to_string(route::task_file(daemon))
                .ok()
                .and_then(|text| find(&text))
        })
        .unwrap_or_else(|| panic!("no `--task-id` in the delegated task: {block}"))
}

fn work_done(daemon: &impl Endpoint, worker: &inbox::Pane, args: &[&str]) -> std::process::Output {
    let mut argv = vec!["work-done"];
    argv.extend_from_slice(args);
    route::cli_output(daemon, worker, daemon.cwd(), &argv)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A plain delegate to the worker is refused because it still owes a task.
fn refused_as_busy(daemon: &impl Endpoint, orch: &inbox::Pane) {
    let mut req = inbox::request(orch, "delegate");
    req["task"] = json!("FOLLOW-UP-WHILE-OWED");
    req["to"] = json!(["coder"]);
    req["timestamp"] = json!(chrono::Utc::now());
    let reply = inbox::hook(daemon, req);
    assert!(
        reply["busy"]
            .as_array()
            .is_some_and(|busy| busy.iter().any(|w| w["role"] == "coder")),
        "the worker still owes its task, so a plain delegate is refused as busy: {reply}"
    );
}

/// Scenario: Delegate a task to a synthetic worker, which fetches it and runs the real `work-done` CLI with the `--task-id` its instructions name. The CLI prints that it reported that task, the orchestrator's inbox lists one report carrying that task id, and a plain follow-up delegate to the same worker is accepted.
#[spec("orchestration/work-done/033")]
#[test]
fn work_done_033_the_real_cli_reports_the_task_its_instructions_name() {
    let daemon = inbox::daemon_unbound(&[]);
    let (orch, worker, id) = delegated(&daemon);
    let out = work_done(
        &daemon,
        &worker,
        &["--task-id", &id, "--task", route::REPORT],
    );
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!("Reported task {id} to the orchestrator.\n")
    );
    let reports = route::count(&daemon, &orch, 1);
    assert_eq!(reports[0]["provenance"]["kind"], "task", "{}", reports[0]);
    assert_eq!(reports[0]["provenance"]["task_id"], id.as_str());
    route::delegate(&daemon, &orch, "FOLLOW-UP-AFTER-THE-REPORT", false);
}

/// Scenario: Delegate a task, then run the real `work-done` CLI without `--task-id`. The CLI says the report went to the orchestrator as a note that completes no task, the orchestrator's copy is labelled that way, and a plain follow-up delegate to the same worker is refused as busy because the task is still owed.
#[spec("orchestration/work-done/034")]
#[test]
fn work_done_034_a_report_without_a_task_id_is_a_note_and_the_task_stays_owed() {
    let daemon = inbox::daemon_unbound(&[]);
    let (orch, worker, _) = delegated(&daemon);
    let out = work_done(&daemon, &worker, &["--task", route::REPORT]);
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("as a note that completes no task"),
        "{}",
        text(&out.stdout)
    );
    let reports = route::count(&daemon, &orch, 1);
    assert_eq!(
        reports[0]["provenance"]["kind"], "uncorrelated",
        "{}",
        reports[0]
    );
    let block = inbox::fetch(&daemon, &orch, true).to_string();
    assert!(block.contains(route::NOTE_LABEL), "{block}");
    refused_as_busy(&daemon, &orch);
}

/// Scenario: Run the same real `work-done --task-id` command twice, as a worker retrying after a lost answer would. The orchestrator receives exactly one report, and the second run exits successfully saying the task was already reported and nothing new was sent.
#[spec("orchestration/work-done/035")]
#[test]
fn work_done_035_a_repeated_report_is_answered_once_and_delivered_once() {
    let daemon = inbox::daemon_unbound(&[]);
    let (orch, worker, id) = delegated(&daemon);
    let args = ["--task-id", id.as_str(), "--task", route::REPORT];
    let first = work_done(&daemon, &worker, &args);
    assert!(first.status.success(), "stderr: {}", text(&first.stderr));
    route::count(&daemon, &orch, 1);
    let second = work_done(&daemon, &worker, &args);
    assert!(second.status.success(), "stderr: {}", text(&second.stderr));
    assert_eq!(
        text(&second.stdout),
        format!("Task {id} was already reported; nothing new was sent.\n")
    );
    // The repeat was answered after the first report was listed, so a second
    // copy would already be listed too.
    route::count(&daemon, &orch, 1);
}

/// Scenario: With the deck's report save made to fail through its test seam, run the real `work-done --task-id`. The CLI exits 1 with the save-failed sentence, the orchestrator receives nothing, and a plain follow-up delegate is refused as busy because the task is still open; the same command run again once the fault is spent reports the task.
#[spec("orchestration/work-done/036")]
#[test]
fn work_done_036_a_failed_save_reports_nothing_and_keeps_the_task_open() {
    let daemon = seamed_daemon();
    let (orch, worker, id) = delegated(&daemon);
    std::fs::write(seam(&daemon, "work-done-fail-save.fault"), b"").unwrap();
    let args = ["--task-id", id.as_str(), "--task", route::REPORT];
    let failed = work_done(&daemon, &worker, &args);
    assert_eq!(
        failed.status.code(),
        Some(1),
        "stdout: {}",
        text(&failed.stdout)
    );
    assert!(
        text(&failed.stderr).contains("the deck could not save this report"),
        "{}",
        text(&failed.stderr)
    );
    assert!(text(&failed.stdout).is_empty(), "{}", text(&failed.stdout));
    assert!(route::messages(&daemon, &orch).is_empty());
    refused_as_busy(&daemon, &orch);
    let retried = work_done(&daemon, &worker, &args);
    assert!(
        retried.status.success(),
        "stderr: {}",
        text(&retried.stderr)
    );
    route::count(&daemon, &orch, 1);
}

fn wait_for(path: &Path) {
    assert!(
        common::wait_until(common::load_scaled(inbox::WAIT), || path.exists()),
        "the daemon never reached {}",
        path.display()
    );
}

/// Scenario: Pause the deck after it saved a worker's report and again once the report is committed, then run the real `work-done --task-id`. While paused the CLI has printed nothing and is still running, and at the second pause the orchestrator's inbox already lists the report with that task id; released, the CLI prints one `Reported task …` line. For a second task, a save failure injected while the save is paused makes the CLI exit 1 with the save-failed sentence only.
#[spec("orchestration/work-done/040")]
#[test]
fn work_done_040_the_answer_follows_the_saved_and_committed_report() {
    let daemon = seamed_daemon();
    let (orch, worker, id) = delegated(&daemon);
    std::fs::write(seam(&daemon, "work-done-saved.armed"), b"").unwrap();
    std::fs::write(seam(&daemon, "work-done-committed.armed"), b"").unwrap();
    let mut child = route::cli_command(
        &daemon,
        &worker,
        daemon.cwd(),
        &["work-done", "--task-id", &id, "--task", route::REPORT],
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .unwrap();

    wait_for(&seam(&daemon, "work-done-saved.reached"));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the CLI answered before the commit"
    );
    std::fs::write(seam(&daemon, "work-done-saved.release"), b"").unwrap();
    wait_for(&seam(&daemon, "work-done-committed.reached"));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the CLI answered before the daemon wrote its answer"
    );
    let listed = route::messages(&daemon, &orch);
    assert_eq!(listed.len(), 1, "committed before the answer: {listed:?}");
    assert_eq!(listed[0]["provenance"]["task_id"], id.as_str());
    std::fs::write(seam(&daemon, "work-done-committed.release"), b"").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!("Reported task {id} to the orchestrator.\n")
    );
    assert!(text(&out.stderr).is_empty(), "{}", text(&out.stderr));

    // A second task, whose save fails while paused.
    let second = route::delegate(&daemon, &orch, "ROUTE-SECOND-TASK", false);
    route::fetched(
        &daemon,
        &worker,
        &second,
        "ROUTE-SECOND-TASK",
        "the orchestrator",
    );
    std::fs::write(seam(&daemon, "work-done-saved.armed"), b"").unwrap();
    let _ = std::fs::remove_file(seam(&daemon, "work-done-saved.reached"));
    let _ = std::fs::remove_file(seam(&daemon, "work-done-saved.release"));
    let child = route::cli_command(
        &daemon,
        &worker,
        daemon.cwd(),
        &["work-done", "--task-id", &second, "--task", "SECOND-REPORT"],
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    wait_for(&seam(&daemon, "work-done-saved.reached"));
    std::fs::write(seam(&daemon, "work-done-fail-save.fault"), b"").unwrap();
    std::fs::write(seam(&daemon, "work-done-saved.release"), b"").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).is_empty(), "{}", text(&out.stdout));
    let stderr = text(&out.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains("the deck could not save this report"),
        "{stderr}"
    );
    assert_eq!(
        route::messages(&daemon, &orch).len(),
        1,
        "nothing new was posted"
    );
}

/// The real CLI run against a stub hook socket at `socket`.
fn cli_against(socket: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(common::contained_deck_binary())
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("DOT_AGENT_DECK_SOCKET", socket)
        .env("DOT_AGENT_DECK_PANE_ID", "stub-pane")
        .output()
        .unwrap()
}

fn read_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(n) if n > 0 => Some(line),
        _ => None,
    }
}

/// Scenario: Run the real `work-done --task-id` binary against a stub hook socket. A stub that reads the capability question and never answers gets no report, and the CLI exits 1 saying the daemon did not confirm it supports task reports; a stub that confirms version 1 receives the report on that same connection, opens no second one, and its accepted answer makes the CLI print `Reported task …` and exit 0.
#[spec("inbox/protocol/029")]
#[test]
fn protocol_029_the_real_cli_asks_first_and_reports_on_the_confirming_connection() {
    let dir = common::harness_tempdir().unwrap();
    let args = ["work-done", "--task-id", "m-029", "--task", "STUB-REPORT"];

    // A daemon that never answers the question.
    let socket = dir.path().join("silent.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let stub = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream);
        std::iter::from_fn(|| read_line(&mut reader)).collect::<Vec<_>>()
    });
    let out = cli_against(&socket, dir.path(), &args);
    let lines = stub.join().unwrap();
    assert_eq!(out.status.code(), Some(1), "stdout: {}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("did not confirm"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(lines.len(), 1, "only the question: {lines:?}");
    assert!(lines[0].contains("\"hook_capabilities\""), "{lines:?}");

    // A daemon that confirms.
    let socket = dir.path().join("confirming.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let stub = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let question = read_line(&mut reader).unwrap();
        writer
            .write_all(b"{\"task_scoped_completion\":1}\n")
            .unwrap();
        let report = read_line(&mut reader).unwrap();
        writer
            .write_all(
                b"{\"disposition\":\"accepted\",\"task_id\":\"m-029\",\"destination\":\"inbox\"}\n",
            )
            .unwrap();
        let rest: Vec<String> = std::iter::from_fn(|| read_line(&mut reader)).collect();
        listener.set_nonblocking(true).unwrap();
        let second = listener.accept().is_ok();
        (question, report, rest, second)
    });
    let out = cli_against(&socket, dir.path(), &args);
    let (question, report, rest, second) = stub.join().unwrap();
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "Reported task m-029 to the orchestrator.\n"
    );
    assert!(question.contains("\"hook_capabilities\""), "{question}");
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["message_type"], "work_done_task", "{report}");
    assert_eq!(
        report["mode"],
        json!({"kind":"task","task_id":"m-029"}),
        "{report}"
    );
    assert_eq!(report["report"], "STUB-REPORT");
    assert!(rest.is_empty(), "nothing after the report: {rest:?}");
    assert!(!second, "the CLI opened a second connection");
}
