#![cfg(unix)]

//! Issue #1835 — a Claude Code tool call that FAILS (a Bash command exiting
//! non-zero, say) fires `PostToolUseFailure` instead of `PostToolUse`
//! [observed on 2.1.78 and 2.1.314 with Haiku and `claude -p`: `PreToolUse`,
//! then `PostToolUseFailure` carrying the same `tool_use_id`, and no
//! `PostToolUse`]. The deck installed no such hook, so it never heard that
//! the call ended: the card kept the failed command as its running tool, and
//! the call's `agent.operation` span stayed open until the run ended.
//!
//! Two halves, both fast tier: the install writes the hook for a Claude Code
//! that accepts it (and only then), and a `PostToolUseFailure` posted through
//! the real `dot-agent-deck hook --agent claude-code` CLI ends the call on the
//! card the way a `PostToolUse` does, without turning the card to Error. The
//! exported-span half lives in `tests/e2e_telemetry_failed_operation.rs`.
//!
//! Deliberately not linking `tests/common/mod.rs`: the crate-internal
//! temp-dir resolver is `#[path]`-included, the same shape as
//! `tests/subagent_tool_status.rs`.

#[path = "../src/test_temp.rs"]
mod test_temp;

use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dot_agent_deck::event::{AgentEvent, EventType};
use dot_agent_deck::hooks_manage::{
    ClaudeHookGates, STOP_FAILURE_MIN_CLAUDE_VERSION, install_to_with_gates, uninstall_from,
};
use dot_agent_deck::state::{AppState, SessionStatus};
use serde_json::{Value, json};
use spec::spec;

const HOOK: &str = "PostToolUseFailure";
const BINARY: &str = "/opt/deck/dot-agent-deck";
const PANE: &str = "tool-failure-pane";
const SESSION: &str = "tool-failure-session";

fn read_settings(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read settings.json"))
        .expect("parse settings.json")
}

/// The deck's own commands under `hook_type`.
fn deck_commands(settings: &Value, hook_type: &str) -> Vec<String> {
    settings["hooks"][hook_type]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|rule| rule["hooks"].as_array().into_iter().flatten())
        .filter_map(|handler| handler["command"].as_str())
        .filter(|command| command.contains(&format!("{BINARY} hook --agent claude-code")))
        .map(str::to_string)
        .collect()
}

/// Scenario: Install the deck's Claude Code hooks into a `settings.json` for a Claude Code reporting 2.1.78, then again for one reporting 2.1.77 and for one whose version is unknown, then uninstall. The 2.1.78 install must write one deck `PostToolUseFailure` rule with no matcher beside a user's own rule; the older and unknown installs must remove the deck's rule and keep the user's, and the uninstall must leave no deck rule under that key.
#[spec("hooks/install/024")]
#[test]
fn hooks_install_024_post_tool_use_failure_installed_for_claude_that_accepts_it() {
    let dir = test_temp::tempdir().expect("settings directory");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        json!({
            "model": "opus",
            "hooks": {HOOK: [{"hooks": [{"type": "command", "command": "logger tool failed"}]}]}
        })
        .to_string(),
    )
    .expect("seed settings.json");

    // The floor is shared with `StopFailure`, the gate that keeps a key an
    // older Claude Code does not know out of the file (issue #714).
    install_to_with_gates(
        &path,
        BINARY,
        ClaudeHookGates::for_version(Some(STOP_FAILURE_MIN_CLAUDE_VERSION)),
    )
    .expect("install for 2.1.78");
    let settings = read_settings(&path);
    assert_eq!(
        deck_commands(&settings, HOOK).len(),
        1,
        "a Claude Code that fires {HOOK} for a failed tool call must get the deck's hook, \
         or the deck never hears that the call ended (issue #1835): {}",
        settings["hooks"]
    );
    let deck_rule = settings["hooks"][HOOK]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| {
            rule["hooks"][0]["command"].as_str() == Some(&deck_commands(&settings, HOOK)[0])
        })
        .expect("the deck's rule");
    assert_eq!(
        deck_rule.get("matcher"),
        None,
        "no matcher: a failed call of every tool must end on time"
    );
    assert!(
        settings["hooks"][HOOK]
            .to_string()
            .contains("logger tool failed"),
        "the user's own rule survives the install: {}",
        settings["hooks"][HOOK]
    );
    assert_eq!(settings["model"], "opus", "unrelated settings survive");

    for version in [Some((2, 1, 77)), None] {
        install_to_with_gates(&path, BINARY, ClaudeHookGates::for_version(version))
            .expect("install for an older or unknown Claude Code");
        let settings = read_settings(&path);
        assert!(
            deck_commands(&settings, HOOK).is_empty(),
            "{version:?}: the deck's {HOOK} rule must not stay in a file read by a Claude Code \
             below the floor: {}",
            settings["hooks"]
        );
        assert!(
            settings["hooks"][HOOK]
                .to_string()
                .contains("logger tool failed"),
            "{version:?}: the user's own rule is theirs to keep: {}",
            settings["hooks"]
        );
    }

    install_to_with_gates(
        &path,
        BINARY,
        ClaudeHookGates::for_version(Some(STOP_FAILURE_MIN_CLAUDE_VERSION)),
    )
    .expect("reinstall");
    uninstall_from(&path).expect("uninstall");
    let settings = read_settings(&path);
    assert!(
        deck_commands(&settings, HOOK).is_empty(),
        "uninstall must sweep the deck's {HOOK} rule: {}",
        settings["hooks"]
    );
}

/// Run one hook payload through the real `hook --agent claude-code` CLI and
/// return the `AgentEvent` it posted, or `None` when it posted nothing.
fn invoke_hook(payload: &Value) -> Option<AgentEvent> {
    let temp = test_temp::tempdir().expect("create hook socket directory");
    let socket = temp.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).expect("bind hook socket");
    listener
        .set_nonblocking(true)
        .expect("make hook listener nonblocking");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hook", "--agent", "claude-code"])
        .env("DOT_AGENT_DECK_SOCKET", &socket)
        .env("DOT_AGENT_DECK_PANE_ID", PANE)
        .env_remove("DOT_AGENT_DECK_AGENT_ID")
        // No token: from inside a deck pane an inherited one would make the
        // SessionStart hook also send its inbox announce (PRD #1666) on the
        // connection this reads as one event.
        .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook ingestion command");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write hook payload");
    let output = child.wait_with_output().expect("wait for hook command");
    assert!(
        output.status.success(),
        "`hook --agent claude-code` rejected {payload}: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    // The hook has exited, so anything it sent is already queued.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // On macOS an accepted socket inherits the listener's
                // O_NONBLOCK; the peer is gone, so a blocking read ends at EOF.
                stream
                    .set_nonblocking(false)
                    .expect("make the accepted stream blocking");
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut line)
                    .expect("read emitted AgentEvent");
                return Some(serde_json::from_str(line.trim()).expect("parse emitted AgentEvent"));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept emitted AgentEvent: {error}"),
        }
    }
}

/// Fields Claude Code puts on every hook input.
fn claude_payload(event: &str, extra: Value) -> Value {
    let mut payload = json!({
        "session_id": SESSION,
        "transcript_path": format!("/home/user/.claude/projects/wt/{SESSION}.jsonl"),
        "cwd": "/home/user/code/wt",
        "permission_mode": "default",
        "hook_event_name": event,
        "tool_name": "Bash",
        "tool_use_id": "toolu_failing_1",
        "tool_input": {"command": "false", "description": "Run the false command"},
    });
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    payload
}

/// Replay `SessionStart`, the failing call's `PreToolUse` and then `end`
/// through the real CLI, and return the card plus the event `end` became.
fn card_after(end: Value) -> (AppState, Option<AgentEvent>) {
    let mut state = AppState::default();
    state.register_pane(PANE.to_string());
    for (payload, want) in [
        (
            claude_payload("SessionStart", json!({"source": "startup"})),
            EventType::SessionStart,
        ),
        (
            claude_payload("PreToolUse", json!({})),
            EventType::ToolStart,
        ),
    ] {
        let event = invoke_hook(&payload).expect("the CLI posts the call's start");
        assert_eq!(event.event_type, want, "{payload}");
        state.apply_event(event);
    }
    assert!(
        state.sessions[SESSION].active_tool.is_some(),
        "the failing command must be the card's running tool before it ends"
    );
    let end = invoke_hook(&end);
    if let Some(event) = &end {
        state.apply_event(event.clone());
    }
    (state, end)
}

/// Scenario: Through the real `hook --agent claude-code` CLI, start a session and a Bash call to `false`, then send the `PostToolUseFailure` Claude Code fires for it (the payload shape recorded from 2.1.314). The CLI must post a ToolEnd for that `tool_use_id`, and the card must drop the command as its running tool, count it, and stay Working rather than Error — exactly as the same call ended by a `PostToolUse` (the control) leaves it.
#[spec("status/tool_end/001")]
#[test]
fn status_tool_end_001_failed_claude_tool_call_ends_without_error() {
    let (control, control_end) = card_after(claude_payload(
        "PostToolUse",
        json!({"tool_response": {"stdout": "", "stderr": "", "interrupted": false}}),
    ));
    let control_end = control_end.expect("control: a PostToolUse posts its call's end");
    assert_eq!(control_end.event_type, EventType::ToolEnd);

    let (state, end) = card_after(claude_payload(
        "PostToolUseFailure",
        json!({"error": "Exit code 1", "is_interrupt": false, "duration_ms": 104}),
    ));
    let end = end.expect(
        "the deck must hear that a failed Claude Code tool call ended: \
         `hook --agent claude-code` posted nothing for PostToolUseFailure (issue #1835)",
    );
    assert_eq!(
        end.event_type,
        EventType::ToolEnd,
        "a failed command is ordinary agent work: its end is a ToolEnd, not an Error"
    );
    assert_eq!(
        end.metadata.get("tool_use_id").map(String::as_str),
        Some("toolu_failing_1"),
        "the end must name the call it ends, as a PostToolUse does"
    );

    let card = &state.sessions[SESSION];
    let control = &control.sessions[SESSION];
    assert!(
        card.active_tool.is_none(),
        "the failed command must no longer be the card's running tool: {:?}",
        card.active_tool
    );
    assert_ne!(card.status, SessionStatus::Error);
    assert_eq!(
        card.status, control.status,
        "same card as a PostToolUse leaves"
    );
    assert_eq!(card.tool_count, control.tool_count);
}
