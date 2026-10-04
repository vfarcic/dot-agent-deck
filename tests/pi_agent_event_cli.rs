#![cfg(unix)]

//! Fast subprocess coverage for the card detail `dot-agent-deck agent-event`
//! carries (issue #622).
//!
//! The bundled Pi extension reports through this verb, and before #622 the
//! verb could only say `running` / `waiting` / `finished`: a Pi card kept no
//! prompt, never showed the tool Pi was running and always read `Tools: 0`.
//! These run the real CLI against a listening socket, then feed what it sends
//! through `AppState::apply_event` — the seam the TUI's subscriber uses — so
//! they pin both halves: the flags reach the wire, and the wire fills the card.
//! The real-Pi end of the path is `pi/live/001` (lane 2).

// Fast-tier, does NOT link `tests/common/mod.rs`; see
// `tests/devin_hook_ingestion.rs` and `docs/develop/e2e-temp-dirs.md`.
#[path = "../src/test_temp.rs"]
mod test_temp;

use std::io::Read as _;
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::{Duration, Instant};

use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::prompt_delivery::{ConfirmationCapability, pane_confirmation_capability};
use dot_agent_deck::state::{AppState, SessionState, SessionStatus};

const PANE: &str = "pi-cli-pane";
const AGENT: &str = "pi-cli-agent";

/// Run `dot-agent-deck agent-event <args>` from a pane and return the frame it
/// put on the hook socket.
fn agent_event(args: &[&str]) -> AgentEvent {
    let temp = test_temp::tempdir().expect("create agent-event socket directory");
    let socket = temp.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).expect("bind agent-event socket");
    listener
        .set_nonblocking(true)
        .expect("make agent-event listener nonblocking");

    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .arg("agent-event")
        .args(args)
        .env("DOT_AGENT_DECK_SOCKET", &socket)
        .env("DOT_AGENT_DECK_PANE_ID", PANE)
        .env("DOT_AGENT_DECK_AGENT_ID", AGENT)
        .output()
        .expect("run agent-event");
    assert!(
        output.status.success(),
        "`agent-event {args:?}` failed: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut line = String::new();
                stream
                    .read_to_string(&mut line)
                    .expect("read emitted AgentEvent");
                return serde_json::from_str(line.trim()).expect("parse emitted AgentEvent");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "`agent-event {args:?}` exited 0 but sent nothing"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept emitted AgentEvent: {error}"),
        }
    }
}

/// Scenario: Report a Pi turn the way the bundled extension does — session
/// start with its directory, the submitted prompt, a `bash` tool starting and
/// ending, then the turn settling — through the real `agent-event` CLI, and
/// apply each frame to a card. The card ends `Idle` with its directory, the
/// prompt, the `bash` call in its history and one completed tool, and showed
/// the tool as active while it ran.
#[test]
fn a_reported_pi_turn_fills_the_card_like_any_native_integration() {
    let mut state = AppState::default();
    state.register_pane(PANE.to_string());
    let session = format!("{PANE}-session");
    let mut apply = |args: &[&str]| {
        let event = agent_event(args);
        assert_eq!(event.agent_type, AgentType::Pi);
        assert_eq!(event.pane_id.as_deref(), Some(PANE));
        assert_eq!(event.agent_id.as_deref(), Some(AGENT));
        state.apply_event(event.clone());
        (event, state.sessions.get(&session).cloned().expect("card"))
    };

    let (_, card) = apply(&["--type", "finished", "--cwd", "/work/repo"]);
    assert_eq!(card.status, SessionStatus::Idle);
    assert_eq!(card.cwd.as_deref(), Some("/work/repo"));
    assert!(card.last_user_prompt.is_none(), "nothing submitted yet");
    assert_eq!(card.tool_count, 0, "nothing executed yet");

    let (event, card) = apply(&[
        "--type",
        "prompt",
        "--cwd",
        "/work/repo",
        "--prompt",
        "list the files",
    ]);
    assert_eq!(event.event_type, EventType::Thinking);
    assert_eq!(card.status, SessionStatus::Thinking);
    assert_eq!(card.last_user_prompt.as_deref(), Some("list the files"));

    let (event, card) = apply(&[
        "--type",
        "tool-start",
        "--cwd",
        "/work/repo",
        "--tool-name",
        "bash",
        "--tool-detail",
        "ls -la",
    ]);
    assert_eq!(event.event_type, EventType::ToolStart);
    assert_eq!(card.status, SessionStatus::Working);
    let active = card.active_tool.expect("active tool while bash runs");
    assert_eq!(active.name, "bash");
    assert_eq!(active.detail.as_deref(), Some("ls -la"));

    let (event, card) = apply(&["--type", "tool-end", "--tool-name", "bash"]);
    assert_eq!(event.event_type, EventType::ToolEnd);
    assert_eq!(card.tool_count, 1);
    assert!(card.active_tool.is_none());

    let (_, card) = apply(&["--type", "finished", "--cwd", "/work/repo"]);
    assert_eq!(card.status, SessionStatus::Idle);
    assert_eq!(card.cwd.as_deref(), Some("/work/repo"));
    assert_eq!(card.last_user_prompt.as_deref(), Some("list the files"));
    assert_eq!(card.tool_count, 1);
    assert!(
        card.recent_events
            .iter()
            .any(|e| e.event_type == EventType::ToolStart
                && e.tool_name.as_deref() == Some("bash")
                && e.tool_detail.as_deref() == Some("ls -la")),
        "the settled card keeps the bash call in its tool history"
    );
}

/// Scenario: Run the lifecycle-only invocation an older extension sends
/// (`--type running`, no other flag). It still succeeds and sends the same
/// bare frame as before, with no directory, prompt or tool on it.
#[test]
fn a_lifecycle_only_report_is_unchanged() {
    let event = agent_event(&["--type", "running"]);
    assert_eq!(event.event_type, EventType::Thinking);
    assert_eq!(event.session_id, format!("{PANE}-session"));
    assert!(event.cwd.is_none());
    assert!(event.user_prompt.is_none());
    assert!(event.tool_name.is_none());
    assert!(event.tool_detail.is_none());
}

/// The confirmation capability the deck reads for `PANE` after `state` applied
/// what the CLI sent — the input every delivery path re-submits from.
fn pane_capability(state: &AppState) -> ConfirmationCapability {
    pane_confirmation_capability(
        state
            .sessions
            .values()
            .filter(|session| session.pane_id.as_deref() == Some(PANE))
            .map(SessionState::confirmation_producer),
    )
}

/// Scenario: Report a Pi session start and a submitted prompt the way the
/// bundled extension does since issue #1567, with `--reports-prompts` on every
/// report, and apply each frame to a card. From the first frame on, the pane
/// counts as one that confirms a delivered prompt itself, so the deck may
/// re-submit a prompt Pi never reported. The same reports without the flag —
/// what an older extension sends — leave the pane as one that cannot confirm,
/// so nothing typed into it is ever typed again.
#[test]
fn only_a_report_declaring_prompt_reports_makes_a_pi_pane_confirm_its_prompts() {
    for declared in [false, true] {
        let mut state = AppState::default();
        state.register_pane(PANE.to_string());
        let flag: &[&str] = if declared {
            &["--reports-prompts"]
        } else {
            &[]
        };
        let start = agent_event(&[&["--type", "finished", "--cwd", "/work/repo"], flag].concat());
        state.apply_event(start);
        let expected = if declared {
            ConfirmationCapability::Reports
        } else {
            ConfirmationCapability::CannotReport
        };
        assert_eq!(
            pane_capability(&state),
            expected,
            "declared={declared}: the session-start report alone decides the pane's capability"
        );
        let prompt =
            agent_event(&[&["--type", "prompt", "--prompt", "list the files"], flag].concat());
        assert_eq!(prompt.user_prompt.as_deref(), Some("list the files"));
        state.apply_event(prompt);
        assert_eq!(
            pane_capability(&state),
            expected,
            "declared={declared}: a reported prompt does not change what the producer declared"
        );
    }
}

/// Scenario: Report a prompt and a tool call whose text starts with a dash
/// (`--help me`, `-rf build`), in both the `--flag=value` form the extension
/// sends and the separate-argument form. Each is delivered as text rather than
/// refused as an unknown flag.
#[test]
fn a_detail_starting_with_a_dash_is_text_not_a_flag() {
    let event = agent_event(&["--type", "prompt", "--prompt=--help me"]);
    assert_eq!(event.user_prompt.as_deref(), Some("--help me"));
    let event = agent_event(&["--type", "prompt", "--prompt", "- item one"]);
    assert_eq!(event.user_prompt.as_deref(), Some("- item one"));
    let event = agent_event(&[
        "--type",
        "tool-start",
        "--tool-name",
        "bash",
        "--tool-detail",
        "-rf build",
        "--cwd",
        "-odd-dir",
    ]);
    assert_eq!(event.tool_detail.as_deref(), Some("-rf build"));
    assert_eq!(event.cwd.as_deref(), Some("-odd-dir"));
}

/// Scenario: Run `agent-event --type tool_start` (a misspelling). The CLI
/// exits non-zero and its error lists every type it accepts.
#[test]
fn an_unknown_type_is_refused_with_the_full_vocabulary() {
    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["agent-event", "--type", "tool_start"])
        .env("DOT_AGENT_DECK_PANE_ID", PANE)
        .env("DOT_AGENT_DECK_SOCKET", "/nonexistent/hook.sock")
        .output()
        .expect("run agent-event");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("running, waiting, finished, prompt, tool-start, tool-end"),
        "stderr: {stderr}"
    );
}
