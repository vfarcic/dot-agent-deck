#![cfg(all(feature = "e2e", unix))]

//! Attached-TUI coverage for quota-blocked agent cards and delegation.
//! These agent commands are local stand-ins; no provider credential is used.

mod common;

use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::event::{EventType, SUBAGENT_ID_METADATA_KEY};
use spec::spec;

const BLOCKED_NOTICE: &str =
    "delegated worker blocked by a provider usage limit (dot-agent-deck daemon report)";

/// Issue #714, after #708: the blocked-worker report's stable FINAL clause. It
/// ends in `.`, which the payload encoder's trailing-whitespace trim cannot
/// eat, so the byte that follows it in the pane is the terminator the daemon
/// wrote.
const BLOCKED_NOTICE_TAIL: &str = "daemon log names the role.";

/// The byte the daemon wrote right after the blocked-worker report — CR when it
/// SUBMITTED the report as a turn, LF when it left it as a line — or `None`
/// while the report or that byte has not arrived. Anchored to the report's
/// opening clause and then its final clause, so an unrelated line break in the
/// pane cannot pass for it. Exact because `quota-orchestrator.sh` runs `cat`
/// under `stty -echo -icanon -icrnl -opost` before printing its readiness
/// marker, so no CR/LF translation sits on either side of the pane.
fn blocked_notice_terminator(snapshot: &[u8]) -> Option<u8> {
    let start = snapshot
        .windows(BLOCKED_NOTICE.len())
        .position(|window| window == BLOCKED_NOTICE.as_bytes())?;
    let rest = &snapshot[start..];
    let end = rest
        .windows(BLOCKED_NOTICE_TAIL.len())
        .position(|window| window == BLOCKED_NOTICE_TAIL.as_bytes())?
        + BLOCKED_NOTICE_TAIL.len();
    rest.get(end).copied()
}

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

fn trigger_fifo(path: &Path) {
    let output = Command::new("mkfifo")
        .arg(path)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn release_standin(path: &Path, deck: &TuiDeck, role: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path);
            match opened {
                Ok(mut fifo) => fifo.write_all(b"go\n").is_ok(),
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => false,
                Err(error) => panic!("open stand-in FIFO {}: {error}", path.display()),
            }
        }),
        "stand-in never opened trigger FIFO {}; pane: {}; agents: {:?}",
        path.display(),
        if role_agent_exists(deck, role) {
            role_pane_text(deck, role)
        } else {
            "<role no longer registered>".to_string()
        },
        common::agent_records_on(deck.attach_socket_path())
    );
}

fn wait_for_role(deck: &TuiDeck, role: &str) {
    assert!(
        common::wait_until(Duration::from_secs(15), || role_agent_exists(deck, role)),
        "{role} never registered"
    );
}

fn assert_blocked(deck: &TuiDeck, role: &str, kind: &str) {
    assert!(
        common::wait_until(Duration::from_secs(20), || {
            role_status(deck, role).as_deref() == Some("Blocked")
                && has_role_badge(&deck.snapshot_grid(), role, "Blocked")
        }),
        "{role} never showed Blocked in daemon status and attached card: {}\n{}",
        status_document(deck),
        deck.snapshot_grid()
    );
    let label = match kind {
        "credits_depleted" => "Credits",
        "usage_limit" => "Usage",
        other => panic!("unexpected blocked kind: {other}"),
    };
    assert!(
        deck.snapshot_grid().contains(label),
        "{role} card omitted its {kind} reason:\n{}",
        deck.snapshot_grid()
    );
}

fn quota_deck(bin: &Path, fifo: &Path, transcript: &Path) -> TuiDeck {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .impersonating_pane_signals()
        .with_env("PATH", path_with_standins(bin))
        .with_env("QUOTA_TRIGGER_FIFO", fifo.to_string_lossy())
        .with_env("QUOTA_TRANSCRIPT_PATH", transcript.to_string_lossy())
        .launch_with_fixture("minimal");
    // The isolated HOME deliberately has no installed agent directories.
    // Exercise the same deck installers explicitly before spawning stand-ins.
    for agent in ["claude-code", "opencode"] {
        let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["hooks", "install", "--agent", agent])
            .env("HOME", deck.home_dir())
            .env("XDG_CONFIG_HOME", deck.home_dir().join(".config"))
            .env("PATH", path_with_standins(bin))
            .current_dir(deck.workdir())
            .output()
            .expect("install synthetic agent hook");
        assert!(
            output.status.success(),
            "{agent} hook install failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    deck
}

fn write_claude_record(path: &Path, transient: bool) {
    let record = if transient {
        json!({"type":"assistant","message":{"role":"assistant","content":[]},
            "error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,
            "apiErrorIsTransient":true})
    } else {
        json!({"type":"assistant","message":{"role":"assistant","content":[]},
            "error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,
            "quotaLimits":{"status":"rejected",
                "resetsAt":chrono::Utc::now().timestamp() + 3600,
                "rateLimitType":"five_hour"}})
    };
    std::fs::write(path, format!("{record}\n")).expect("write structured Claude transcript");
}

fn install_structured_standin(bin: &Path, name: &str) {
    std::fs::create_dir_all(bin).expect("create bin");
    let script = r#"import json, os, subprocess, sys
agent = sys.argv[1]
if agent == 'codex' and len(sys.argv) > 2:
    sys.exit(0)  # Startup's app-server trust probe is not the agent pane.
home = os.environ['HOME']
if agent == 'claude':
    settings = os.path.join(home, '.claude', 'settings.json')
    hooks = json.load(open(settings))['hooks']
    suffix = 'hook --agent claude-code'
else:
    codex_home = os.environ.get('CODEX_HOME') or os.path.join(home, '.codex')
    hooks = json.load(open(os.path.join(codex_home, 'hooks.json')))['hooks']
    suffix = 'hook --agent codex'
def send(event, **fields):
    commands = [h['command'] for rule in hooks[event] for h in rule['hooks']
                if h['command'].endswith(suffix)]
    assert len(commands) == 1, (event, commands)
    payload = {'hook_event_name':event,'session_id':'quota-' + agent,**fields}
    subprocess.run(commands[0], shell=True, input=json.dumps(payload), text=True,
                   check=True, stdout=subprocess.DEVNULL)
with open(os.environ['QUOTA_TRIGGER_FIFO']) as trigger:
    trigger.readline()
path = os.environ['QUOTA_TRANSCRIPT_PATH']
if agent == 'claude':
    send('SessionStart')
    send('StopFailure', error='rate_limit', transcript_path=path,
         last_assistant_message='structured-provider-detail-sentinel')
else:
    send('SessionStart', transcript_path=path)
    send('UserPromptSubmit', transcript_path=path, turn_id='turn-714', prompt='work')
    records = [
        {'type':'event_msg','payload':{'type':'task_started','turn_id':'turn-714'}},
        {'type':'event_msg','payload':{'type':'token_count','turn_id':'turn-714',
            'rate_limits':{'rate_limit_reached_type':'workspace_member_credits_depleted'}}},
        {'type':'event_msg','payload':{'type':'task_complete','turn_id':'turn-714',
            'error':{'codex_error_info':'usage_limit_exceeded',
                     'message':'structured-provider-detail-sentinel'}}}
    ]
    with open(path, 'a') as rollout:
        for record in records:
            rollout.write(json.dumps(record) + '\n')
        rollout.flush()
sys.stdin.buffer.read()
"#;
    std::fs::write(bin.join("quota-standin.py"), script).expect("write structured stand-in");
    let version = if name == "claude" {
        r#"if [ "$1" = --version ]; then printf '2.1.283 (Claude Code)\n'; exit 0; fi
"#
    } else {
        ""
    };
    write_agent(
        bin,
        name,
        &format!(
            "{version}exec python3 '{}/quota-standin.py' {name}",
            bin.display()
        ),
    );
}

fn claude_hook(deck: &TuiDeck, role: &str, event: &str) {
    claude_hook_with_fields(deck, role, event, json!({}));
}

fn claude_hook_with_fields(deck: &TuiDeck, role: &str, event: &str, fields: Value) {
    use std::io::Write as _;
    use std::process::Stdio;
    let settings: Value = serde_json::from_slice(
        &std::fs::read(deck.home_dir().join(".claude/settings.json")).expect("Claude settings"),
    )
    .expect("settings JSON");
    let command = settings["hooks"][event][0]["hooks"][0]["command"]
        .as_str()
        .expect("installed Claude command");
    let agent = role_agent(deck, role);
    let mut child = Command::new("sh")
        .args(["-c", command])
        .env("HOME", deck.home_dir())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            agent.pane_id_env.expect("pane id"),
        )
        .env("DOT_AGENT_DECK_AGENT_ID", &agent.id)
        .stdin(Stdio::piped())
        .spawn()
        .expect("run installed hook");
    let mut payload = json!({"hook_event_name":event,"session_id":"quota-claude"});
    payload
        .as_object_mut()
        .expect("hook payload object")
        .extend(fields.as_object().expect("hook fields object").clone());
    writeln!(child.stdin.take().expect("stdin"), "{payload}").expect("send hook payload");
    assert!(child.wait().expect("wait hook").success());
}

/// Scenario: A Claude stand-in uses the installed StopFailure hook and a rejected
/// quota transcript to block its attached card and daemon status. A later
/// UserPromptSubmit hook resumes it, clearing Blocked to Thinking.
#[spec("status/blocked/017")]
#[test]
fn status_blocked_017_claude_stop_failure_hook_shows_blocked_card() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("claude-trigger");
    let transcript = fixture.path().join("claude-quota.jsonl");
    trigger_fifo(&fifo);
    write_claude_record(&transcript, false);
    install_structured_standin(&bin, "claude");
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "claude", "claude")]);
    open_orchestration(&deck);
    wait_for_role(&deck, "worker");
    release_standin(&fifo, &deck, "worker");
    assert_blocked(&deck, "worker", "usage_limit");
    assert!(
        deck.snapshot_grid().contains("resets"),
        "Claude card omitted provider reset: \n{}",
        deck.snapshot_grid()
    );
    claude_hook(&deck, "worker", "UserPromptSubmit");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            role_status(&deck, "worker").as_deref() == Some("Thinking")
                && has_role_badge(&deck.snapshot_grid(), "worker", "Thinking")
        }),
        "new Claude prompt did not clear Blocked: {}\n{}",
        status_document(&deck),
        deck.snapshot_grid()
    );
    assert_transient_claude_429_is_error();
}

/// Scenario: A Claude stand-in blocks its attached card through a rejected-quota
/// StopFailure hook, then subagent tool and failure hooks leave both the card and
/// daemon status Blocked. A main-thread prompt clears the card to Thinking.
#[spec("status/blocked/025")]
#[test]
fn status_blocked_025_subagent_hooks_keep_parent_blocked() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("claude-trigger");
    let transcript = fixture.path().join("claude-quota.jsonl");
    trigger_fifo(&fifo);
    write_claude_record(&transcript, false);
    install_structured_standin(&bin, "claude");
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "claude", "claude")]);
    open_orchestration(&deck);
    wait_for_role(&deck, "worker");
    release_standin(&fifo, &deck, "worker");
    assert_blocked(&deck, "worker", "usage_limit");

    let events = deck.subscribe_events();
    let subagent = "quota-subagent-025";
    for (hook, expected, fields) in [
        (
            "SubagentStart",
            EventType::SubagentStart,
            json!({"agent_id":subagent}),
        ),
        (
            "PreToolUse",
            EventType::ToolStart,
            json!({"agent_id":subagent,"tool_name":"Bash","tool_use_id":"quota-tool-025",
                "tool_input":{"command":"echo subagent-only"}}),
        ),
        (
            "PostToolUse",
            EventType::ToolEnd,
            json!({"agent_id":subagent,"tool_name":"Bash","tool_use_id":"quota-tool-025",
                "tool_response":{"stdout":"subagent-only","stderr":"","interrupted":false}}),
        ),
        (
            "StopFailure",
            EventType::SubagentStop,
            json!({"agent_id":subagent,"error":"billing_error",
                "transcript_path":transcript}),
        ),
    ] {
        claude_hook_with_fields(&deck, "worker", hook, fields);
        // The broadcast confirms this hook reached the daemon before checking
        // the card. Checking each step prevents a later hook from hiding a
        // brief, incorrect transition to Working or Error.
        events.wait_for(
            |event| {
                event.event_type == expected
                    && event
                        .metadata
                        .get(SUBAGENT_ID_METADATA_KEY)
                        .map(String::as_str)
                        == Some(subagent)
            },
            Duration::from_secs(10),
        );
        deck.wait_until_grid_then_hold(
            &format!("{hook} leaves the worker card Blocked"),
            Duration::from_millis(250),
            |grid| {
                has_role_badge(grid, "worker", "Blocked")
                    && !has_role_badge(grid, "worker", "Error")
            },
        );
        assert_eq!(
            role_status(&deck, "worker").as_deref(),
            Some("Blocked"),
            "{hook} changed daemon status"
        );
    }
    assert!(
        deck.snapshot_grid().contains("Usage"),
        "subagent hooks removed the quota reason:\n{}",
        deck.snapshot_grid()
    );

    claude_hook(&deck, "worker", "UserPromptSubmit");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            role_status(&deck, "worker").as_deref() == Some("Thinking")
                && has_role_badge(&deck.snapshot_grid(), "worker", "Thinking")
        }),
        "main-thread prompt did not clear Blocked: {}\n{}",
        status_document(&deck),
        deck.snapshot_grid()
    );
}

/// Scenario: A transient Claude 429 reaches StopFailure without quotaLimits.
/// Its card ends in Error, never Blocked, even though the HTTP status is 429.
fn assert_transient_claude_429_is_error() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("claude-trigger");
    let transcript = fixture.path().join("claude-transient.jsonl");
    trigger_fifo(&fifo);
    write_claude_record(&transcript, true);
    install_structured_standin(&bin, "claude");
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "claude", "claude")]);
    open_orchestration(&deck);
    wait_for_role(&deck, "worker");
    release_standin(&fifo, &deck, "worker");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            role_status(&deck, "worker").as_deref() == Some("Error")
                && has_role_badge(&deck.snapshot_grid(), "worker", "Error")
        }),
        "transient 429 did not end in Error: {}\n{}",
        status_document(&deck),
        deck.snapshot_grid()
    );
}

/// Scenario: A launcher starts a Codex stand-in that announces its rollout
/// path through the deck-installed hooks, then appends a quota task_complete.
/// The attached card and daemon status show depleted credits.
#[spec("status/blocked/018")]
#[test]
fn status_blocked_018_codex_rollout_blocks_a_launcher_started_codex() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("codex-trigger");
    let transcript = fixture.path().join("rollout-714.jsonl");
    trigger_fifo(&fifo);
    std::fs::write(&transcript, "").expect("create rollout");
    install_structured_standin(&bin, "codex");
    write_agent(&bin, "launch-codex", "exec codex");
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "launch-codex", "codex")]);
    open_orchestration(&deck);
    wait_for_role(&deck, "worker");
    release_standin(&fifo, &deck, "worker");
    assert_blocked(&deck, "worker", "credits_depleted");
}

/// Scenario: A Claude worker reports a blocked provider through its installed
/// StopFailure hook before delegation. Delegate still delivers the task, warns
/// about Blocked, and sends exactly one fixed notice to the orchestrator.
#[spec("orchestration/delegate/037")]
#[test]
fn orchestration_delegate_037_delegate_to_blocked_worker_warns_and_delivers() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("claude-trigger");
    let transcript = fixture.path().join("claude-quota.jsonl");
    trigger_fifo(&fifo);
    write_claude_record(&transcript, false);
    install_structured_standin(&bin, "claude");
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "claude", "claude")]);
    open_orchestration(&deck);
    wait_for_role(&deck, "worker");
    release_standin(&fifo, &deck, "worker");
    assert_blocked(&deck, "worker", "usage_limit");
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
    assert_eq!(
        notice_text().matches(BLOCKED_NOTICE).count(),
        1,
        "blocked notice repeated"
    );
}

/// Scenario: A delegated Codex worker announces its rollout path through the
/// installed hook and appends a structured quota failure while work is owed.
/// The orchestrator receives one fixed notice and the ledger stays busy.
#[spec("scheduler/idle-worker/021")]
#[test]
fn scheduler_idle_worker_021_blocked_worker_notices_orchestrator_once() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("codex-trigger");
    let transcript = fixture.path().join("rollout-714.jsonl");
    trigger_fifo(&fifo);
    std::fs::write(&transcript, "").expect("create rollout");
    install_structured_standin(&bin, "codex");
    write_agent(&bin, "launch-codex", "exec codex");
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "launch-codex", "codex")]);
    open_orchestration(&deck);
    wait_for_role(&deck, "worker");
    let first = delegate(&deck, "worker", "remain-owed-after-quota");
    assert!(
        first.status.success(),
        "delegate failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            role_pane_text(&deck, "worker").contains("worker-task-worker")
        }),
        "task pointer did not reach worker PTY"
    );
    release_standin(&fifo, &deck, "worker");
    assert_blocked(&deck, "worker", "credits_depleted");
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
        !text.contains("structured-provider-detail-sentinel"),
        "detail leaked: {text}"
    );
    // Issue #714, after #708: SUBMITTED, not written. An unsubmitted line
    // reaches nobody in an unattended dispatched unit, so the orchestrator
    // would never learn to reassign work its blocked worker cannot finish.
    let raw_snapshot = || common::pane_snapshot_on(deck.attach_socket_path(), &orchestrator.id);
    common::wait_until(Duration::from_secs(5), || {
        blocked_notice_terminator(&raw_snapshot()).is_some()
    });
    let terminator = blocked_notice_terminator(&raw_snapshot());
    assert_eq!(
        terminator,
        Some(b'\r'),
        "the blocked-worker report must be SUBMITTED (CR), not left as an LF-terminated \
         line in the orchestrator's pane: {text}"
    );
    let second = delegate(&deck, "worker", "busy-ledger-still-owed");
    assert!(
        !second.status.success(),
        "blocked notice retired outstanding delegation"
    );
    assert_eq!(notice_text().matches(BLOCKED_NOTICE).count(), 1);
}

fn install_opencode_standin(bin: &Path) {
    std::fs::create_dir_all(bin).expect("create bin");
    let script = r#"import fs from 'node:fs';
import path from 'node:path';
const home = process.env.HOME;
const candidates = [path.join(home, '.config/opencode/plugin/dot-agent-deck.js'),
                    path.join(home, '.opencode/plugin/dot-agent-deck.js')];
const variant = process.argv[2];
fs.readFileSync(process.env.QUOTA_TRIGGER_FIFO + '-' + variant, 'utf8');
const pluginPath = candidates.find(p => fs.existsSync(p));
if (!pluginPath) throw new Error('deck-installed OpenCode plugin missing');
const source = fs.readFileSync(pluginPath, 'utf8');
const module = await import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
const plugin = await module.DotAgentDeckPlugin({directory: process.cwd()});
const sessionID = 'quota-opencode-' + variant;
await plugin.event({event:{type:'session.created',properties:{info:{id:sessionID}}}});
const body = variant === 'marker'
    ? JSON.stringify({error:{type:'usage_limit_reached',
        resets_at:Math.floor(Date.now()/1000)+3600}})
    : JSON.stringify({error:{type:'rate_limit_error'}});
await plugin.event({event:{type:'session.error',properties:{sessionID,
    error:{name:'APIError',data:{statusCode:429,responseBody:body,
        message:'structured-provider-detail-sentinel'}}}}});
process.stdin.resume();
"#;
    std::fs::write(bin.join("quota-opencode.mjs"), script).expect("write OpenCode stand-in");
    write_agent(
        bin,
        "opencode",
        &format!("exec node '{}/quota-opencode.mjs' \"$@\"", bin.display()),
    );
}

/// Scenario: Two OpenCode stand-ins are released independently, load the
/// deck-installed plugin, and send session.error events. A structured usage
/// marker blocks the first card; a bare 429 leaves the other in Error.
#[spec("status/blocked/019")]
#[test]
fn status_blocked_019_opencode_plugin_error_blocks_and_bare_429_does_not() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("SKIP: node is unavailable");
        return;
    }
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("opencode-trigger");
    let marker_fifo = fixture.path().join("opencode-trigger-marker");
    let bare_fifo = fixture.path().join("opencode-trigger-bare");
    let transcript = fixture.path().join("unused.jsonl");
    trigger_fifo(&marker_fifo);
    trigger_fifo(&bare_fifo);
    install_opencode_standin(&bin);
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(
        &deck,
        &[
            ("marker", "opencode marker", "opencode"),
            ("bare", "opencode bare", "opencode"),
        ],
    );
    open_orchestration(&deck);
    wait_for_role(&deck, "marker");
    wait_for_role(&deck, "bare");
    release_standin(&marker_fifo, &deck, "marker");
    release_standin(&bare_fifo, &deck, "bare");
    assert_blocked(&deck, "marker", "usage_limit");
    assert!(
        common::wait_until(Duration::from_secs(20), || {
            role_status(&deck, "bare").as_deref() == Some("Error")
                && has_role_badge(&deck.snapshot_grid(), "bare", "Error")
        }),
        "bare OpenCode 429 did not end in Error: {}\n{}",
        status_document(&deck),
        deck.snapshot_grid()
    );
}

/// Scenario: A Claude stand-in blocks through StopFailure, then its role pane
/// restarts. The new card does not retain the old Blocked state.
#[spec("status/blocked/021")]
#[test]
fn status_blocked_021_pane_restart_clears_a_blocked_card() {
    let fixture = common::race_safe_tempdir();
    let bin = fixture.path().join("bin");
    let fifo = fixture.path().join("claude-trigger");
    let transcript = fixture.path().join("claude-quota.jsonl");
    trigger_fifo(&fifo);
    write_claude_record(&transcript, false);
    install_structured_standin(&bin, "claude");
    let deck = quota_deck(&bin, &fifo, &transcript);
    deck.wait_for_string("No active sessions");
    write_orchestration(&deck, &[("worker", "claude", "claude")]);
    open_orchestration(&deck);
    wait_for_role(&deck, "worker");
    release_standin(&fifo, &deck, "worker");
    assert_blocked(&deck, "worker", "usage_limit");
    let old_id = role_agent(&deck, "worker").id;
    let orchestrator_pane = role_agent(&deck, "orchestrator")
        .pane_id_env
        .expect("orchestrator pane id");
    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["pane", "restart", "worker", "--force"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", orchestrator_pane)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("restart blocked worker");
    assert!(
        output.status.success(),
        "restart refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            role_agent_exists(&deck, "worker")
                && role_agent(&deck, "worker").id != old_id
                && role_status(&deck, "worker").as_deref() != Some("Blocked")
                && !has_role_badge(&deck.snapshot_grid(), "worker", "Blocked")
        }),
        "restarted worker retained Blocked: {}\n{}",
        status_document(&deck),
        deck.snapshot_grid()
    );
}
