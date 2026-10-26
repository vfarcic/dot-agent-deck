#![cfg(all(feature = "e2e", unix))]

//! Truthful user-facing termination notices from synthetic pane commands.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use inbox::*;
use serde_json::json;
use spec::spec;
use std::path::PathBuf;
use std::time::Duration;

struct Descendant(PathBuf);
impl Drop for Descendant {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.0)
            && pid.trim().parse::<u32>().is_ok()
        {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
    }
}

fn fixture(mode: &str, wrapped: bool) -> (common::DaemonProc, Pane, Pane, String, Descendant) {
    let daemon = daemon_unbound(&[]);
    let successor_command = route::command(daemon.cwd());
    std::fs::write(daemon.cwd().join(".dot-agent-deck.toml"), format!(
        "[[orchestrations]]\nname = 'route'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = {}\nstart = true\n[[orchestrations.roles]]\nname = 'coder'\ncommand = {}\nclear = false\n",
        serde_json::to_string(&successor_command).unwrap(), serde_json::to_string(&successor_command).unwrap()
    )).unwrap();
    let orch = route::start(
        &daemon,
        "route-orchestrator",
        json!({"tab_membership": {
            "kind":"orchestration", "name":"route", "role_name":"orchestrator", "role_index":0,
            "is_start_role":true,"orchestration_id":"route-fixture","orchestration_cwd":daemon.cwd()
        }}),
    );
    // Exercise shell quoting with an apostrophe in the executable's path too.
    let script = daemon.home.join("terminal'command.py");
    std::fs::write(&script, r#"import os, sys, time, signal, subprocess
base, mode = sys.argv[1:]
open(base + '.pid', 'w').write(str(os.getpid()))
os.write(1, b'DOORBELL-STANDIN-READY\r\n')
while not os.path.exists(base + '.finish'): time.sleep(.01)
if mode == 'eof':
    for fd in (0, 1, 2): os.close(fd)
    while True: time.sleep(1)
if mode == 'keeps-terminal':
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    if os.fork() == 0:
        open(base + '.descendant', 'w').write(str(os.getpid()))
        while not os.path.exists(base + '.release'): time.sleep(.01)
        os._exit(0)
    while not os.path.exists(base + '.descendant'): time.sleep(.01)
    open(base + '.launcher-ended', 'w').close()
    os._exit(0)
if mode == 'detached':
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'],
        start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    open(base + '.descendant', 'w').write(str(child.pid))
sys.exit(7 if mode == 'nonzero' else 0)
"#).unwrap();
    let base = daemon.home.join("terminal-worker");
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
    let command = format!(
        "exec python3 '{}' '{}' '{mode}'",
        quote(&script),
        quote(&base)
    );
    let worker = pane_with(
        &daemon,
        "terminal-worker",
        json!({
            "profile":false, "command":common::capability_export_command(&command),
            // Ask the real spawn seam to construct the wrapper, so the deck
            // knows its child is a wrapper and must use its child's report.
            "agent_type": if wrapped { json!("codex") } else { json!(null) },
            "tab_membership":{"kind":"orchestration", "name":"route", "role_name":"coder",
                "role_index":1,"is_start_role":false,"orchestration_id":"route-fixture", "orchestration_cwd":daemon.cwd()}
        }),
    );
    status(&daemon, &orch, "session_start", "codex");
    status(&daemon, &orch, "thinking", "codex");
    let task = route::delegate(&daemon, &orch, "TERMINATION-WAITING-TASK-4c91", false);
    state(&daemon, &worker, &task, "queued");
    assert!(
        route::messages(&daemon, &orch).is_empty(),
        "control: no notice before termination"
    );
    let guard = Descendant(base.with_extension("descendant"));
    (daemon, orch, worker, task, guard)
}

fn finish(worker: &Pane) {
    std::fs::write(worker.capture.with_extension("finish"), "").unwrap();
}

fn notice(daemon: &impl Endpoint, orch: &Pane, task: &str) -> String {
    let notice = route::notice(daemon, orch, task).to_string();
    assert!(
        !notice.contains("agent exited")
            && !notice.contains("worker exited")
            && !notice.contains("worker it was addressed to exited")
            && !notice.contains("agent's process exited"),
        "a pane command ending is not evidence that the agent's process exited: {notice}"
    );
    notice
}

fn exited(mode: &str, code: i32, wrapped: bool) {
    let (daemon, orch, worker, task, _descendant) = fixture(mode, wrapped);
    finish(&worker);
    state(&daemon, &worker, &task, "undeliverable");
    if mode == "detached" {
        let pid = std::fs::read_to_string(worker.capture.with_extension("descendant")).unwrap();
        assert!(
            common::process_running(pid.parse().unwrap()),
            "control: detached descendant still runs when the notice is composed"
        );
    }
    let message = notice(&daemon, &orch, &task);
    let expected = format!(
        "the command started in the agent's pane exited (code {code}) before taking this message"
    );
    assert!(
        message.contains(&expected),
        "pane-command exit (wrapped={wrapped}) must say {expected:?}: {message}"
    );
}

/// Scenario: A wrapped synthetic pane command exits successfully before taking its delegated message. The orchestrator's notice must report the pane command's code zero without claiming that the agent process exited.
#[spec("inbox/status/032")]
#[test]
fn status_032_wrapped_zero_exit_names_the_pane_command() {
    exited("zero", 0, true);
}

/// Scenario: A wrapped synthetic pane command exits with code seven before taking its delegated message. The orchestrator's notice must report seven without claiming an agent-process exit; this public wrapper also exits seven, so distinguishing the two evidence sources requires a coder-owned capture test.
#[spec("inbox/status/032")]
#[test]
fn status_032_wrapped_nonzero_exit_names_the_child_code() {
    exited("nonzero", 7, true);
}

/// Scenario: A synthetic command started directly in its pane exits zero before taking its delegated message. The orchestrator's notice must name the pane command and its observed code, without a wrapper report or a claim about an agent process.
#[spec("inbox/status/032")]
#[test]
fn status_032_unwrapped_zero_exit_names_the_pane_command() {
    exited("zero", 0, false);
}

/// Scenario: A synthetic command started directly in its pane exits seven before taking its delegated message. The orchestrator's notice must name that pane command's observed code seven without depending on a wrapper report.
#[spec("inbox/status/032")]
#[test]
fn status_032_unwrapped_nonzero_exit_names_the_pane_command() {
    exited("nonzero", 7, false);
}

/// Scenario: A live pane command closes its terminal descriptors without exiting or reporting a child status. Its waiting task becomes undeliverable and the orchestrator is told that the connection ended and whether the agent still runs is unknown.
#[spec("inbox/status/032")]
#[test]
fn status_032_terminal_eof_without_status_is_connection_ended() {
    let (daemon, orch, worker, task, _descendant) = fixture("eof", false);
    finish(&worker);
    state(&daemon, &worker, &task, "undeliverable");
    assert!(
        common::process_running(worker.pid as i32),
        "control: terminal descriptors closed while the pane command is still alive"
    );
    let message = notice(&daemon, &orch, &task);
    assert!(message.contains("the deck's connection to the agent's pane ended before it took this message; whether the agent is still running is unknown"), "no child status means connection ended: {message}");
}

/// Scenario: Close a pane while its delegated task is still queued. The orchestrator's notice must say the pane was closed before the agent took the message, without claiming a process exit.
#[spec("inbox/status/032")]
#[test]
fn status_032_closed_pane_says_closed() {
    let (daemon, orch, worker, task, _descendant) = fixture("zero", true);
    assert_eq!(
        attach(&daemon, json!({"op":"stop-agent","id":worker.agent}))["ok"],
        true
    );
    let message = notice(&daemon, &orch, &task);
    assert!(
        message.contains("the pane was closed before the agent took this message"),
        "close notice: {message}"
    );
}

/// Scenario: Start a successor in the same pane while the previous recipient still has a delegated task queued. The orchestrator must be told the pane was replaced and the task stayed with the previous agent, and the successor must receive none of it.
#[spec("inbox/status/032")]
#[test]
fn status_032_replaced_pane_keeps_the_previous_agents_message() {
    let (daemon, orch, worker, task, _descendant) = fixture("zero", true);
    route::cli(
        &daemon,
        &orch,
        daemon.cwd(),
        &["pane", "restart", "coder", "--force"],
    );
    let successor_id = common::agent_records_on(daemon.attach_socket())
        .into_iter()
        .find(|r| r.pane_id_env.as_deref() == Some(&worker.id) && r.id != worker.agent)
        .expect("forced restart published the successor")
        .id;
    let successor = route::observe(&daemon, &successor_id);
    assert_ne!(successor.agent, worker.agent);
    assert!(
        !fetch(&daemon, &successor, true)
            .to_string()
            .contains("TERMINATION-WAITING-TASK-4c91")
    );
    let message = notice(&daemon, &orch, &task);
    assert!(message.contains("the agent's pane was replaced before it took this message; the message stays with the previous agent"), "replacement notice: {message}");
}

/// Scenario: A launcher exits while a forked descendant keeps its terminal open and the delegated task is still queued. No exit notice may reach the orchestrator before that descendant releases the terminal.
#[spec("inbox/status/032")]
#[test]
fn status_032_launcher_exit_with_terminal_descendant_sends_no_notice() {
    let (daemon, orch, worker, task, _descendant) = fixture("keeps-terminal", false);
    finish(&worker);
    assert!(
        common::wait_until(WAIT, || worker
            .capture
            .with_extension("launcher-ended")
            .exists()),
        "launcher reached its exit"
    );
    assert!(
        common::wait_until(WAIT, || !common::process_running(worker.pid as i32)),
        "control: the launcher really exited before the negative observation"
    );
    let descendant = std::fs::read_to_string(worker.capture.with_extension("descendant")).unwrap();
    assert!(
        common::process_running(descendant.parse().unwrap()),
        "control: its descendant still holds the terminal after the launcher exited"
    );
    assert!(
        !common::wait_until(Duration::from_millis(700), || !route::messages(
            &daemon, &orch
        )
        .is_empty()),
        "a descendant keeps the terminal open: no exit notice is justified"
    );
    state(&daemon, &worker, &task, "queued");
    std::fs::write(worker.capture.with_extension("release"), "").unwrap();
}

/// Scenario: A wrapped launcher starts a detached descendant, then exits zero with a task still waiting. The notice must stop at the pane command's exit even while that descendant demonstrably remains alive.
#[spec("inbox/status/032")]
#[test]
fn status_032_detached_descendant_does_not_justify_agent_exit_words() {
    exited("detached", 0, true);
}
