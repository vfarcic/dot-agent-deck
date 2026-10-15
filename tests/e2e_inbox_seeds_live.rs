#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Spawn seeds remain queued through boot and are acknowledged when consumed.
//! The real-agent cases run interactive Haiku in attached deck panes.
mod common;
#[path = "support/inbox_view.rs"]
mod inbox_view;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::{AttachRequest, PostOrigin};
use dot_agent_deck::inbox::{MessageSummary, StateName};
use spec::spec;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const REAL_AGENT_COMMAND: &str =
    "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write";
const WAIT: Duration = Duration::from_secs(120);

struct SiblingWorktreeGuards(Vec<PathBuf>);
impl Drop for SiblingWorktreeGuards {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn dispatch_worktree_of(deck: &TuiDeck, name: &str) -> PathBuf {
    deck.workdir().parent().unwrap().join(format!(
        "{}-dispatch-{name}",
        deck.workdir().file_name().unwrap().to_string_lossy()
    ))
}

fn configure(command: &str) -> tempfile::TempDir {
    let config = common::harness_tempdir().unwrap();
    std::fs::write(
        config.path().join("config.toml"),
        format!("default_command = {command:?}\n"),
    )
    .unwrap();
    config
}

/// Read the visible value, excluding the form's border and padding.
fn form_field_value(grid: &str, label: &str) -> Option<String> {
    grid.lines().find_map(|line| {
        line.split_once(label).map(|(_, value)| {
            value
                .split('│')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
    })
}

/// Wait for both edited values and the visible cursor in the blank Command field.
fn wait_for_edited_form(deck: &TuiDeck, name: &str) {
    deck.capture_screen_when("edited Name and blank, focused Command", |screen| {
        let grid = screen.contents();
        let (row, line) = grid
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Command:"))?;
        let col = line.find("Command:")?;
        let value_col = line[..col].chars().count() + "Command: ".len();
        (form_field_value(&grid, "Name:").as_deref() == Some(name)
            && form_field_value(&grid, "Command:").as_deref() == Some("")
            && !screen.hide_cursor()
            && screen.cursor_position() == (row as u16, value_col as u16))
            .then_some(())
    });
}

fn open_shell_caller_pane(deck: &TuiDeck, configured_command: &str) -> String {
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    let initial_name = deck.capture_screen_when("initial Name value", |screen| {
        form_field_value(&screen.contents(), "Name:")
    });
    deck.send_keys(b"\t");
    deck.send_keys(&vec![0x7f; initial_name.chars().count()]);
    deck.send_keys(b"caller");
    deck.wait_until_grid("Name changed to caller", |grid| {
        form_field_value(grid, "Name:").as_deref() == Some("caller")
    });
    deck.send_keys(b"\t");
    deck.send_keys(&vec![0x7f; configured_command.chars().count()]);
    wait_for_edited_form(deck, "caller");
    // Seed delivery needs a caller shell; mouse parity is covered by mouse/form/001.
    deck.send_keys(b"\r");
    deck.wait_for_absence("┌ New Agent");
    let found = std::cell::RefCell::new(None);
    assert!(common::wait_until(WAIT, || {
        *found.borrow_mut() = common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find_map(|r| r.pane_id_env);
        found.borrow().is_some()
    }));
    found.into_inner().unwrap()
}

fn prepare(
    deck: &TuiDeck,
    names: &[&str],
    configured_command: &str,
) -> (String, SiblingWorktreeGuards) {
    common::commit_fixture_repo(deck.workdir());
    let worktrees: Vec<_> = names
        .iter()
        .map(|name| dispatch_worktree_of(deck, name))
        .collect();
    let mut trust: Vec<_> = worktrees
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    trust.push(deck.workdir().to_string_lossy().into_owned());
    common::seed_claude_trust_in_home(deck.home_dir(), &trust).unwrap();
    (
        open_shell_caller_pane(deck, configured_command),
        SiblingWorktreeGuards(worktrees),
    )
}

fn dispatch(deck: &TuiDeck, caller: &str, cases: &[(&str, String)]) {
    let mut children: Vec<_> = cases
        .iter()
        .map(|(name, prompt)| {
            Command::new(common::contained_deck_binary())
                .args(["dispatch", name, "--single", "--task", prompt])
                .env("HOME", deck.home_dir())
                .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
                .env("DOT_AGENT_DECK_PANE_ID", caller)
                .current_dir(deck.workdir())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children.drain(..) {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "dispatch: {output:?}");
    }
}

fn record(deck: &TuiDeck, name: &str) -> Option<dot_agent_deck::agent_pty::AgentRecord> {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|r| r.display_name.as_deref() == Some(format!("dispatch-{name}").as_str()))
}

fn task(deck: &TuiDeck, name: &str, needle: &str) -> Option<MessageSummary> {
    let pane = record(deck, name)?.pane_id_env?;
    inbox_view::messages(deck.attach_socket_path(), &pane)
        .into_iter()
        .find(|m| m.text.as_deref().is_some_and(|t| t.contains(needle)))
}

fn diagnostics(deck: &TuiDeck) -> String {
    format!(
        "inboxes={:#?}\nGrid:\n{}\nLog:\n{}",
        inbox_view::all_messages(deck.attach_socket_path()),
        deck.snapshot_grid(),
        std::fs::read_to_string(deck.workdir().join("inbox-seeds.log")).unwrap_or_default()
    )
}

fn acknowledged(deck: &TuiDeck, name: &str, needle: &str) -> MessageSummary {
    assert!(
        common::wait_until(WAIT, || task(deck, name, needle)
            .is_some_and(|m| m.state == StateName::Acknowledged)),
        "{name} did not acknowledge {needle}; {}",
        diagnostics(deck)
    );
    let message = task(deck, name, needle).unwrap();
    assert_eq!(message.deliveries, 1, "one delivery per seed");
    let record = record(deck, name).unwrap();
    assert_eq!(message.bind_agent.as_deref(), Some(record.id.as_str()));
    assert!(
        !record
            .live
            .and_then(|l| l.last_user_prompt)
            .unwrap_or_default()
            .contains(needle),
        "seed must not become human prompt history"
    );
    message
}

/// Scenario: Dispatch three synthetic agents which hold at bootstrap without reading a task from the PTY. Each seed must remain queued until its own recorder is released, then the real inbox CLI consumes and acknowledges it once without a typed content payload.
#[spec("scheduler/dispatch/014")]
#[test]
fn dispatch_014_concurrent_swallowed_seeds_retry_until_confirmed() {
    let staging = common::harness_tempdir().unwrap();
    let launcher = staging.path().join("seed-recorder.py");
    std::fs::write(
        &launcher,
        format!(
            r#"#!/usr/bin/python3
import os, tty, termios, select, subprocess
tty.setraw(0,termios.TCSANOW)
open('bootstrap-entered','w').close()
capture=open('bootstrap.bytes','ab',buffering=0)
while not os.path.exists('release-bootstrap'):
    if select.select([0],[],[],0.025)[0]: capture.write(os.read(0,4096))
result=subprocess.run(['{}','inbox','fetch'],capture_output=True,check=True)
with open('consumed-inbox.txt','wb') as f: f.write(result.stdout)
while True:
    if select.select([0],[],[],0.025)[0]:
        chunk=os.read(0,4096)
        if not chunk: break
        capture.write(chunk)
"#,
            common::contained_deck_binary().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = configure(launcher.to_str().unwrap());
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_CONFIG",
            config.path().join("config.toml").to_string_lossy(),
        )
        .with_env("DOT_AGENT_DECK_LOG", "inbox-seeds.log")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    common::commit_fixture_repo(deck.workdir());
    let names = ["synthetic-alpha", "synthetic-beta", "synthetic-gamma"];
    let _guards = SiblingWorktreeGuards(
        names
            .iter()
            .map(|name| dispatch_worktree_of(&deck, name))
            .collect(),
    );
    let caller = open_shell_caller_pane(&deck, launcher.to_str().unwrap());
    let cases: Vec<_> = names
        .iter()
        .map(|name| (*name, format!("BOOT-QUEUE-{name}-cc17")))
        .collect();
    dispatch(&deck, &caller, &cases);
    for (name, prompt) in &cases {
        assert!(
            common::wait_until(WAIT, || task(&deck, name, prompt).is_some()),
            "seed must queue; {}",
            diagnostics(&deck)
        );
        assert_eq!(task(&deck, name, prompt).unwrap().state, StateName::Queued);
        let work = dispatch_worktree_of(&deck, name);
        assert!(common::wait_for_path(&work.join("bootstrap-entered"), WAIT));
        assert!(
            std::fs::read(work.join("bootstrap.bytes"))
                .unwrap_or_default()
                .is_empty(),
            "boot must not receive typed content"
        );
        std::fs::write(work.join("release-bootstrap"), "").unwrap();
        acknowledged(&deck, name, prompt);
        assert!(common::wait_until(WAIT, || std::fs::read_to_string(
            work.join("consumed-inbox.txt")
        )
        .is_ok_and(|t| t.contains(prompt))));
    }
}

/// Scenario: Dispatch three interactive Haiku agents concurrently behind launchers which delay their native startup. Each queued seed must wake its own agent natively, append one unique sentinel line and be acknowledged exactly once, with no typed doorbell or user-history entry.
#[spec("scheduler/dispatch/015")]
#[test]
fn dispatch_015_three_real_claude_seeds_are_genuinely_confirmed() {
    skip_unless!(common::check_claude_available());
    let staging = common::harness_tempdir().unwrap();
    let launcher = staging.path().join("delayed-haiku.sh");
    std::fs::write(
        &launcher,
        format!("#!/bin/sh\nprintf boot > bootstrap-entered\nsleep 2\nexec {REAL_AGENT_COMMAND}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = configure(launcher.to_str().unwrap());
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(200, 55)
        .with_imported_claude_credentials()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_CONFIG",
            config.path().join("config.toml").to_string_lossy(),
        )
        .with_env("DOT_AGENT_DECK_LOG", "inbox-seeds.log")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let names = ["real-seed-alpha", "real-seed-beta", "real-seed-gamma"];
    let (caller, _guards) = prepare(&deck, &names, launcher.to_str().unwrap());
    let cases:Vec<_>=names.iter().map(|name|(*name,format!("Use Bash to run pwd, then run exactly this in the directory it prints: printf '%s\\n' 'seed-ok-{name}' >> 'native-seed-{name}-7f31.txt'. Then end your turn without running work-done."))).collect();
    dispatch(&deck, &caller, &cases);
    for (name, prompt) in &cases {
        acknowledged(&deck, name, prompt);
        let sentinel =
            dispatch_worktree_of(&deck, name).join(format!("native-seed-{name}-7f31.txt"));
        common::wait_for_file_trimmed_eq(&sentinel, &format!("seed-ok-{name}"), WAIT)
            .unwrap_or_else(|e| {
                panic!(
                    "Haiku must act on its seed once: {e}; {}",
                    diagnostics(&deck)
                )
            });
        assert_eq!(
            std::fs::read_to_string(sentinel).unwrap(),
            format!("seed-ok-{name}\n")
        );
    }
    assert!(
        !std::fs::read_to_string(deck.workdir().join("inbox-seeds.log"))
            .unwrap_or_default()
            .contains("doorbell rang"),
        "native seeds require no PTY ring; {}",
        diagnostics(&deck)
    );
    assert!(
        deck.wait_for_grid_string_within("✉", Duration::from_secs(15)),
        "seed cards must carry deck labels"
    );
}

/// Scenario: Dispatch interactive Haiku using a task file with CRLF and blank lines and require one acknowledged inbox message and one sentinel append. Once idle, post a bare-CR follow-up through the client PostMessage seam; it must be consumed once, append the second sentinel and stay out of human prompt history.
#[spec("scheduler/dispatch/028")]
#[test]
fn dispatch_028_prompts_with_cr_line_breaks_submit_exactly_once_on_real_claude() {
    skip_unless!(common::check_claude_available());
    let config = configure(REAL_AGENT_COMMAND);
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_imported_claude_credentials()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_CONFIG",
            config.path().join("config.toml").to_string_lossy(),
        )
        .with_env("DOT_AGENT_DECK_LOG", "inbox-seeds.log")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let name = "line-breaks";
    let (caller, _guards) = prepare(&deck, &[name], REAL_AGENT_COMMAND);
    let staging = common::harness_tempdir().unwrap();
    let task_file = staging.path().join("crlf-task.md");
    let body = "This is a Windows CRLF task.\r\n\r\nUse Bash to run pwd, then run exactly this in the directory it prints: printf '%s\\n' first-ok >> line-break-first-91f3.txt. Then end your turn without running work-done.\r\n";
    std::fs::write(&task_file, body).unwrap();
    let output = Command::new(common::contained_deck_binary())
        .args(["dispatch", name, "--single", "--task-file"])
        .arg(&task_file)
        .env("HOME", deck.home_dir())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", caller)
        .current_dir(deck.workdir())
        .output()
        .unwrap();
    assert!(output.status.success(), "dispatch task file: {output:?}");
    let first = acknowledged(&deck, name, "line-break-first-91f3.txt");
    assert!(
        first.text.as_deref().unwrap().contains(body.trim()),
        "inbox preserves interior task-file line breaks: {:?}",
        first.text
    );
    let work = dispatch_worktree_of(&deck, name);
    common::wait_for_file_trimmed_eq(&work.join("line-break-first-91f3.txt"), "first-ok", WAIT)
        .unwrap();
    assert!(common::wait_until(WAIT, || record(&deck, name)
        .and_then(|r| r.live)
        .is_some_and(
            |l| l.status == dot_agent_deck::state::SessionStatus::Idle
        )));
    let agent = record(&deck, name).unwrap();
    let follow_up = "One more check with a bare carriage return.\rUse Bash to run pwd, then run exactly this in the directory it prints: printf '%s\\n' second-ok >> line-break-second-4c7a.txt. Then end your turn.";
    let reply = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::PostMessage {
            pane_id: agent.pane_id_env.unwrap(),
            expected_agent_id: Some(agent.id),
            subject: "line break follow-up".into(),
            body: follow_up.into(),
            spill_path: None,
            origin: PostOrigin::Seed,
        },
    )
    .unwrap();
    assert!(reply.ok);
    let second = acknowledged(&deck, name, "line-break-second-4c7a.txt");
    assert_eq!(Some(second.id.to_string()), reply.id);
    assert!(second.text.unwrap().contains(follow_up));
    common::wait_for_file_trimmed_eq(&work.join("line-break-second-4c7a.txt"), "second-ok", WAIT)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(work.join("line-break-first-91f3.txt")).unwrap(),
        "first-ok\n"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("line-break-second-4c7a.txt")).unwrap(),
        "second-ok\n"
    );
    assert!(
        !std::fs::read_to_string(deck.workdir().join("inbox-seeds.log"))
            .unwrap_or_default()
            .contains("doorbell rang")
    );
}
