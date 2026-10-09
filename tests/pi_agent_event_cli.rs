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

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::state::{AppState, SessionStatus};

const PANE: &str = "pi-cli-pane";
const AGENT: &str = "pi-cli-agent";

/// Run `dot-agent-deck agent-event <args>` from a pane and return the frame it
/// put on the hook socket.
fn agent_event(args: &[&str]) -> AgentEvent {
    serde_json::from_str(agent_event_line(args, None).trim()).expect("parse emitted AgentEvent")
}

/// Run `dot-agent-deck agent-event <args>` from a pane, writing `stdin` to it
/// (none: stdin is null), and return the raw line it put on the hook socket.
fn agent_event_line(args: &[&str], stdin: Option<&[u8]>) -> String {
    agent_event_line_writing(args, stdin).0
}

/// [`agent_event_line`], also returning how the write to stdin ended (`None`
/// when there was no stdin). Both ends run concurrently, as the deck and the
/// Pi extension do: stdin is written from a thread of its own and the socket
/// is accepted while the CLI runs, so neither a full pipe nor a hook line
/// larger than the socket buffer (macOS keeps 8 KiB, PR #1617) can leave the
/// CLI and this test waiting on each other.
fn agent_event_line_writing(
    args: &[&str],
    stdin: Option<&[u8]>,
) -> (String, Option<std::io::Result<()>>) {
    let temp = test_temp::tempdir().expect("create agent-event socket directory");
    let socket = temp.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).expect("bind agent-event socket");
    listener
        .set_nonblocking(true)
        .expect("make agent-event listener nonblocking");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .arg("agent-event")
        .args(args)
        .env("DOT_AGENT_DECK_SOCKET", &socket)
        .env("DOT_AGENT_DECK_PANE_ID", PANE)
        .env("DOT_AGENT_DECK_AGENT_ID", AGENT)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run agent-event");
    let writing = stdin.map(|bytes| {
        let mut pipe = child.stdin.take().expect("piped stdin");
        let bytes = bytes.to_vec();
        // Dropping the handle closes stdin, as the Pi extension does.
        std::thread::spawn(move || pipe.write_all(&bytes))
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let line = loop {
        // Polled before the accept, so an accept that finds nothing after the
        // CLI exited means it never connected.
        let exited = child.try_wait().expect("poll agent-event").is_some();
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("make the accepted stream blocking");
                let mut line = String::new();
                stream
                    .read_to_string(&mut line)
                    .expect("read emitted AgentEvent");
                break Some(line);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if exited || Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept emitted AgentEvent: {error}"),
        }
    };
    let output = child.wait_with_output().expect("wait for agent-event");
    assert!(
        output.status.success(),
        "`agent-event {args:?}` failed: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let line = line.unwrap_or_else(|| panic!("`agent-event {args:?}` exited 0 but sent nothing"));
    let written = writing.map(|thread| thread.join().expect("stdin writer thread"));
    (line, written)
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

/// Scenario: Report a settled Pi turn the way the bundled extension does since
/// PRD #1497 moved the reply off the command line: `--type finished
/// --turn-reply-stdin` (and `--turn-reply-failed`) with the reply written to
/// stdin. The line on the hook socket carries the reply under `turn_reply`
/// beside an unchanged `Idle` event; with nothing on stdin the line carries an
/// empty reply, the turn ending with nothing to read; on another `--type`, or
/// without the flag, the line carries no reply; and the old
/// `--turn-reply=<text>` form is refused as an unknown flag, so no reply can be
/// put on argv.
#[test]
fn a_settled_turns_reply_is_read_from_stdin_never_argv() {
    use dot_agent_deck::daemon_protocol::{FinalReply, MAX_TURN_REPLY_BYTES};
    use dot_agent_deck::turn_reply::reply_from_line;

    let reply = "All 42 tests pass.\n\n--not-a-flag é";
    let line = agent_event_line(
        &["--type", "finished", "--cwd=/w", "--turn-reply-stdin"],
        Some(reply.as_bytes()),
    );
    assert_eq!(
        reply_from_line(&line),
        Some(FinalReply {
            turn_id: None,
            text: reply.to_string(),
            failed: false,
        })
    );
    let event: AgentEvent = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(event.event_type, EventType::Idle);
    assert_eq!(event.cwd.as_deref(), Some("/w"));

    let line = agent_event_line(
        &[
            "--type",
            "finished",
            "--turn-reply-stdin",
            "--turn-reply-failed",
        ],
        Some(b"429 rate limited"),
    );
    assert!(reply_from_line(&line).expect("failed reply").failed);

    // A reply past what the deck keeps is cut to it, and a writer sending far
    // more than a pipe holds (64 KiB on Linux) still finishes its write: the
    // CLI reads what follows the bound and discards it.
    let long = "x".repeat(256 * 1024);
    let (line, written) = agent_event_line_writing(
        &["--type", "finished", "--turn-reply-stdin"],
        Some(long.as_bytes()),
    );
    assert_eq!(
        reply_from_line(&line).expect("long reply").text.len(),
        MAX_TURN_REPLY_BYTES
    );
    written
        .expect("stdin was written")
        .expect("the whole write is consumed, never refused");

    // A turn end with nothing on stdin is a turn that ended with nothing to
    // read: an empty reply (audit A2). A report that is not a turn end, and a
    // turn end without the flag, carry none.
    let line = agent_event_line(&["--type", "finished", "--turn-reply-stdin"], None);
    assert!(
        reply_from_line(&line)
            .expect("a flagged turn end is reported")
            .is_empty()
    );
    let line = agent_event_line(&["--type", "finished"], None);
    assert_eq!(reply_from_line(&line), None);
    let line = agent_event_line(
        &["--type", "running", "--turn-reply-stdin"],
        Some(b"not a turn end"),
    );
    assert_eq!(reply_from_line(&line), None);

    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["agent-event", "--type", "finished", "--turn-reply=secret"])
        .env("DOT_AGENT_DECK_PANE_ID", PANE)
        .env("DOT_AGENT_DECK_SOCKET", "/nonexistent/hook.sock")
        .output()
        .expect("run agent-event");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error: unexpected argument '--turn-reply"),
        "stderr: {stderr}"
    );
}
