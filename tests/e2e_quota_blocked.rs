#![cfg(all(feature = "e2e", unix))]

//! Attached-TUI coverage for quota-blocked agent cards and delegation.
//! These agent commands are local stand-ins; no provider credential is used.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

const CODEX_LINE: &str = "■ You’ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits";
const OPENCODE_LINE: &str = "Error: The usage limit has been reached";
const BLOCKED_NOTICE: &str =
    "delegated worker blocked by a provider usage limit (dot-agent-deck daemon report)";
const CONFIRM_MS: &str = "200";

fn write_agent(bin: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(bin).expect("create stand-in binary directory");
    let path = bin.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write agent stand-in");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make agent stand-in executable");
}

fn path_with_standins(bin: &Path) -> String {
    let deck_bin = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .parent()
        .expect("deck binary directory");
    format!(
        "{}:{}:{}",
        bin.display(),
        deck_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn status_document(deck: &TuiDeck) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["daemon", "status", "--json"])
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", deck.attach_socket_path())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run daemon status --json");
    assert!(
        output.status.success(),
        "daemon status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("daemon status JSON")
}

fn role_status(deck: &TuiDeck, role: &str) -> Option<String> {
    status_document(deck)["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .find(|agent| agent["role"] == role)
        .and_then(|agent| agent["status"].as_str().map(str::to_string))
}

fn role_agent(deck: &TuiDeck, role: &str) -> dot_agent_deck::agent_pty::AgentRecord {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| {
            matches!(
                &agent.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
            )
        })
        .unwrap_or_else(|| panic!("missing {role} role agent"))
}

fn role_pane_text(deck: &TuiDeck, role: &str) -> String {
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| {
            matches!(
                &agent.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
            )
        });
    agent
        .map(|agent| {
            common::strip_ansi(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &agent.id,
            ))
        })
        .unwrap_or_default()
}

fn role_agent_exists(deck: &TuiDeck, role: &str) -> bool {
    common::agent_records_on(deck.attach_socket_path())
        .iter()
        .any(|agent| {
            matches!(
                &agent.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
            )
        })
}

fn has_role_badge(grid: &str, role: &str, badge: &str) -> bool {
    let role_needle = format!("· {role}");
    grid.lines()
        .any(|line| line.contains(&role_needle) && line.contains(badge))
}

fn open_orchestration(deck: &TuiDeck) {
    deck.send_bytes(b"\x0e");
    deck.send_bytes(b" ");
    deck.wait_for_string("No mode");
    deck.send_bytes(b"\x1b[C");
    deck.send_bytes(b"\r");
    deck.send_bytes(b"\r");
    deck.wait_for_string("orchestrator");
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            common::agent_records_on(deck.attach_socket_path())
                .iter()
                .any(|agent| {
                    matches!(
                        &agent.tab_membership,
                        Some(TabMembership::Orchestration { role_name, is_start_role: true, .. })
                            if role_name == "orchestrator"
                    )
                })
        }),
        "orchestrator agent was not registered after opening its tab"
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains("ORCH-READY")
        }),
        "orchestrator did not appear ready in the attached TUI:\n{}",
        deck.snapshot_grid()
    );
}

fn write_orchestration(deck: &TuiDeck, workers: &[(&str, &str, &str)]) {
    write_agent(
        deck.workdir(),
        "quota-orchestrator.sh",
        "stty -echo -icanon -icrnl -opost min 1 time 0\nprintf ORCH-READY\nexec cat",
    );
    let mut config = String::from(
        "[[orchestrations]]\nname = \"quota-test\"\n\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./quota-orchestrator.sh\"\nstart = true\nclear = false\n",
    );
    for (role, command, agent) in workers {
        config.push_str(&format!(
            "\n[[orchestrations.roles]]\nname = \"{role}\"\ncommand = \"{command}\"\nagent = \"{agent}\"\nclear = false\n"
        ));
    }
    std::fs::write(deck.workdir().join(".dot-agent-deck.toml"), config)
        .expect("write quota orchestration config");
}

fn delegate(deck: &TuiDeck, role: &str, task: &str) -> Output {
    let orchestrator = role_agent(deck, "orchestrator");
    Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["delegate", "--to", role, "--task", task])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            orchestrator.pane_id_env.expect("orchestrator pane id"),
        )
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run delegate CLI")
}

/// Scenario: Restore a Codex pane whose local stand-in prints the shipped
/// U+2019 quota message and stays alive. After confirmation, its attached TUI
/// card and daemon status JSON must both report Blocked.
#[spec("status/blocked/008")]
#[test]
fn status_blocked_008_codex_standin_printing_quota_line_shows_blocked_card() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    write_agent(
        &bin,
        "codex",
        &format!("printf '%s\\n' '{CODEX_LINE}'\nexec cat"),
    );
    let deck = TuiDeck::builder()
        .with_pty_size(160, 42)
        .with_env("PATH", path_with_standins(&bin))
        .with_env("DOT_AGENT_DECK_QUOTA_CONFIRM_MS", CONFIRM_MS)
        .with_continue_session("quota-codex", "codex")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"\x04");
    assert!(
        common::wait_until(Duration::from_secs(25), || {
            has_role_badge(&deck.snapshot_grid(), "quota-codex", "Blocked")
                || deck
                    .snapshot_grid()
                    .lines()
                    .any(|line| line.contains("quota-codex") && line.contains("Blocked"))
        }),
        "Codex quota card never showed Blocked:\n{}",
        deck.snapshot_grid()
    );
    let status = status_document(&deck);
    assert!(
        status["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .any(|agent| agent["status"] == "Blocked"),
        "daemon status did not publish Blocked: {status}"
    );
}

/// Scenario: Open two OpenCode stand-ins in one attached orchestration: one
/// prints a bare provider error and goes quiet, while the other repeatedly
/// prints the same sentence in quotes. Only the bare-error card may be Blocked.
#[spec("status/blocked/009")]
#[test]
fn status_blocked_009_opencode_standin_and_healthy_mention_are_distinguished() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    write_agent(
        &bin,
        "opencode",
        &format!(
            "if [ \"$1\" = quoted ]; then\n  while :; do printf '%s\\n' '\"The usage limit has been reached\"'; sleep 0.2; done\nfi\nprintf '%s\\n' '{OPENCODE_LINE}'\nexec cat"
        ),
    );
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_env("PATH", path_with_standins(&bin))
        .with_env("DOT_AGENT_DECK_QUOTA_CONFIRM_MS", CONFIRM_MS)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    write_orchestration(
        &deck,
        &[
            ("bare-quota", "opencode bare", "opencode"),
            ("quoted-output", "opencode quoted", "opencode"),
        ],
    );
    open_orchestration(&deck);
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            role_pane_text(&deck, "quoted-output").contains("\"The usage limit has been reached\"")
        }),
        "quoted OpenCode stand-in never printed its control line"
    );
    assert!(
        common::wait_until(Duration::from_secs(25), || {
            role_status(&deck, "bare-quota").as_deref() == Some("Blocked")
        }),
        "bare OpenCode error was not Blocked: {}",
        status_document(&deck)
    );
    let hold = Duration::from_millis(CONFIRM_MS.parse::<u64>().unwrap() * 2);
    assert!(
        !common::wait_until(hold, || {
            role_status(&deck, "quoted-output").as_deref() == Some("Blocked")
        }),
        "quoted, continuously active pane was falsely Blocked"
    );
}

/// Scenario: Start an orchestration whose Codex worker prints a quota error
/// and remains alive. Once its card is Blocked, delegating to that worker must
/// succeed with an explicit warning, still deliver the task pointer, and send
/// the orchestrator exactly one blocked-worker notice for that new delegation.
#[spec("orchestration/delegate/037")]
#[test]
fn orchestration_delegate_037_delegate_to_blocked_worker_warns_and_delivers() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    write_agent(
        &bin,
        "codex",
        &format!("printf '%s\\n' '{CODEX_LINE}'\nstty -echo -icanon\nexec cat"),
    );
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .impersonating_pane_signals()
        .with_env("PATH", path_with_standins(&bin))
        .with_env("DOT_AGENT_DECK_QUOTA_CONFIRM_MS", CONFIRM_MS)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "codex", "codex")]);
    open_orchestration(&deck);
    assert!(
        common::wait_until(Duration::from_secs(25), || {
            has_role_badge(&deck.snapshot_grid(), "worker", "Blocked")
        }),
        "worker card never showed Blocked:\n{}",
        deck.snapshot_grid()
    );
    let output = delegate(&deck, "worker", "quota-warning-delivery-sentinel");
    assert!(
        output.status.success(),
        "delegate refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("appear BLOCKED"),
        "missing blocked warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let worker = role_agent(&deck, "worker");
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            common::strip_ansi(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &worker.id,
            ))
            .contains("worker-task-worker")
        }),
        "accepted delegation did not reach worker PTY"
    );
    // Issue #714 (review): the block was published before this delegation
    // existed, and an unchanged blocked screen never publishes again — the
    // dispatch that delivered the task is what reports it.
    let orchestrator = role_agent(&deck, "orchestrator");
    let notice_text = || {
        common::strip_ansi(&common::pane_snapshot_on(
            deck.attach_socket_path(),
            &orchestrator.id,
        ))
    };
    assert!(
        common::wait_until(Duration::from_secs(10), || notice_text()
            .contains(BLOCKED_NOTICE)),
        "orchestrator was not told the new delegation's worker is blocked: {}",
        notice_text()
    );
    assert!(
        !common::wait_until(Duration::from_secs(1), || notice_text()
            .matches(BLOCKED_NOTICE)
            .count()
            > 1),
        "blocked notice repeated: {}",
        notice_text()
    );
}

/// Scenario: Delegate to a live worker, then make its Codex stand-in print a
/// quota error while the task remains owed. The orchestrator must receive one
/// fixed daemon notice, and a second delegation must still see the busy ledger.
#[spec("scheduler/idle-worker/021")]
#[test]
fn scheduler_idle_worker_021_blocked_worker_notices_orchestrator_once() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let trigger = fixture.path().join("print-quota-now");
    write_agent(
        &bin,
        "codex",
        &format!(
            "(while [ ! -e '{}' ]; do sleep 0.1; done; printf '\\n%s\\n' '{CODEX_LINE}') &\nstty -echo -icanon\nexec cat",
            trigger.display()
        ),
    );
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .impersonating_pane_signals()
        .with_env("PATH", path_with_standins(&bin))
        .with_env("DOT_AGENT_DECK_QUOTA_CONFIRM_MS", CONFIRM_MS)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "codex", "codex")]);
    open_orchestration(&deck);
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            role_agent_exists(&deck, "worker")
        }),
        "worker pane did not start before the first delegation"
    );
    let first = delegate(&deck, "worker", "remain-owed-after-quota");
    assert!(
        first.status.success(),
        "initial delegate failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            role_pane_text(&deck, "worker").contains("worker-task-worker")
        }),
        "first task pointer did not reach the worker PTY"
    );
    std::fs::write(&trigger, b"go").expect("trigger worker quota line");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            role_pane_text(&deck, "worker").contains(CODEX_LINE)
        }),
        "triggered Codex stand-in never printed its provider error"
    );
    assert!(
        common::wait_until(Duration::from_secs(25), || {
            role_status(&deck, "worker").as_deref() == Some("Blocked")
        }),
        "worker did not become Blocked: {}",
        status_document(&deck)
    );
    let orchestrator = role_agent(&deck, "orchestrator");
    let notice_text = || {
        common::strip_ansi(&common::pane_snapshot_on(
            deck.attach_socket_path(),
            &orchestrator.id,
        ))
    };
    assert!(
        common::wait_until(Duration::from_secs(10), || notice_text()
            .contains(BLOCKED_NOTICE)),
        "orchestrator did not receive blocked notice: {}",
        notice_text()
    );
    let text = notice_text();
    assert_eq!(
        text.matches(BLOCKED_NOTICE).count(),
        1,
        "blocked notice repeated: {text}"
    );
    let worker_pane = role_agent(&deck, "worker")
        .pane_id_env
        .expect("worker pane id");
    assert!(
        text.contains(&worker_pane),
        "notice omitted worker pane id: {text}"
    );
    assert!(
        !text.contains(CODEX_LINE),
        "agent-controlled quota detail leaked into notice: {text}"
    );
    let second = delegate(&deck, "worker", "busy-ledger-still-owed");
    assert!(
        !second.status.success(),
        "blocked notice incorrectly retired the outstanding delegation"
    );
    assert_eq!(notice_text().matches(BLOCKED_NOTICE).count(), 1);
}
