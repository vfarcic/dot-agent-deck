#![cfg(unix)]

//! Issue #1354 — a tool call made by a Claude Code subagent (or a background
//! agent Claude Code runs after the turn) must not drive the top-level card's
//! status.
//!
//! The reported sequence, from one Claude session in `deck.log`: the main turn
//! runs a `Bash` call, finishes it, and goes `Idle`; three seconds later a
//! background agent under the same `session_id` starts a `Bash` call that
//! never gets a `PostToolUse`, and a `SubagentStop` follows. The card sat at
//! Working for ~15 hours.
//!
//! Each payload below goes through the REAL `dot-agent-deck hook --agent
//! claude-code` CLI (the same path Claude Code's installed hook takes), and
//! the `AgentEvent` it emits is fed to `AppState::apply_event` — the sink that
//! computes the status the card badges. The payload shapes are Claude Code's
//! own: its base hook input carries `agent_id` "only when the hook fires from
//! within a subagent … Absent for the main thread, even in --agent sessions"
//! (the schema description in Claude Code 2.1.283), and `SubagentStop` is only
//! ever emitted from a context that has one.
//!
//! Fast tier, and deliberately not linking `tests/common/mod.rs`: the
//! crate-internal temp-dir resolver is `#[path]`-included, the same shape as
//! `tests/codex_hook_ingestion.rs`. The rendered-grid half of this lives in
//! `tests/e2e_hook_delivery.rs` (`hooks/delivery/008`).

#[path = "../src/test_temp.rs"]
mod test_temp;

use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dot_agent_deck::event::{AgentEvent, EventType};
use dot_agent_deck::state::{AppState, SessionStatus};
use serde_json::{Value, json};
use spec::spec;

const PANE: &str = "subagent-status-pane";
const SESSION: &str = "subagent-status-session";
/// The background agent's id, as Claude Code stamps it on every hook that
/// fires inside that agent's context.
const SUBAGENT_ID: &str = "a7c1e0b2d9f34e18";

/// Run one hook payload through the real `hook --agent <agent>` CLI and return
/// the `AgentEvent` it posted to the socket.
fn invoke_hook(agent: &str, payload: &Value) -> AgentEvent {
    let temp = test_temp::tempdir().expect("create hook socket directory");
    let socket = temp.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).expect("bind hook socket");
    listener
        .set_nonblocking(true)
        .expect("make hook listener nonblocking");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hook", "--agent", agent])
        .env("DOT_AGENT_DECK_SOCKET", &socket)
        .env("DOT_AGENT_DECK_PANE_ID", PANE)
        .env_remove("DOT_AGENT_DECK_AGENT_ID")
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
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // PRD #1542: a Claude `PermissionRequest` arrives as a held
                // `question` envelope and waits for an answer on the same
                // connection, so read ONE line rather than to EOF, unwrap the
                // event, and release the hook.
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut stream), &mut line)
                    .expect("read emitted AgentEvent");
                let value: Value = serde_json::from_str(line.trim()).expect("parse emitted line");
                if value["message_type"] == "question" {
                    stream
                        .write_all(b"{\"question_id\":\"\",\"outcome\":\"released\",\"reason\":\"cleared\"}\n")
                        .expect("release the held hook");
                    let _ = child.wait();
                    return serde_json::from_value(value["event"].clone())
                        .expect("parse the held question's AgentEvent");
                }
                let output = child.wait_with_output().expect("wait for hook command");
                assert!(
                    output.status.success(),
                    "`hook --agent {agent}` rejected a payload: status={} stderr={}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
                return serde_json::from_value(value).expect("parse emitted AgentEvent");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "`hook --agent {agent}` exited successfully but emitted no AgentEvent for {payload}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept emitted AgentEvent: {error}"),
        }
    }
}

/// Fields Claude Code puts on every hook input, main thread or not.
fn base(event: &str) -> Value {
    json!({
        "session_id": SESSION,
        "transcript_path": format!("/home/user/.claude/projects/wt/{SESSION}.jsonl"),
        "cwd": "/home/user/code/wt",
        "permission_mode": "default",
        "hook_event_name": event,
    })
}

fn with(mut value: Value, extra: Value) -> Value {
    let (Value::Object(map), Value::Object(extra)) = (&mut value, extra) else {
        unreachable!("both are JSON objects");
    };
    map.extend(extra);
    value
}

fn session_start() -> Value {
    with(base("SessionStart"), json!({"source": "startup"}))
}

fn main_pre_tool_use(tool_use_id: &str, command: &str) -> Value {
    with(
        base("PreToolUse"),
        json!({
            "tool_name": "Bash",
            "tool_use_id": tool_use_id,
            "tool_input": {"command": command},
        }),
    )
}

fn main_post_tool_use(tool_use_id: &str, command: &str) -> Value {
    with(
        base("PostToolUse"),
        json!({
            "tool_name": "Bash",
            "tool_use_id": tool_use_id,
            "tool_input": {"command": command},
            "tool_response": {"stdout": "", "stderr": "", "interrupted": false},
        }),
    )
}

fn stop() -> Value {
    with(
        base("Stop"),
        json!({"stop_hook_active": false, "last_assistant_message": "done"}),
    )
}

/// The background agent's call: the same base fields, plus the `agent_id` /
/// `agent_type` pair Claude Code adds inside a subagent's context.
fn subagent_pre_tool_use(tool_use_id: &str, command: &str) -> Value {
    with(
        main_pre_tool_use(tool_use_id, command),
        json!({"agent_id": SUBAGENT_ID, "agent_type": "general-purpose"}),
    )
}

fn subagent_stop() -> Value {
    with(
        base("SubagentStop"),
        json!({
            "stop_hook_active": false,
            "agent_id": SUBAGENT_ID,
            "agent_type": "general-purpose",
            "agent_transcript_path": format!("/home/user/.claude/projects/wt/{SESSION}/subagents/agent-{SUBAGENT_ID}.jsonl"),
            "last_assistant_message": "",
        }),
    )
}

const WORK_DONE: &str = "git status --short; dot-agent-deck work-done --task-file report.md";
const BACKGROUND_LS: &str = "ls .dot-agent-deck/worker-task-*.md 2>/dev/null";

/// Feed `payloads` in order through `hook --agent <agent>` and return the
/// resulting card, asserting each payload became the event type the sequence
/// expects.
fn apply_all(agent: &str, payloads: &[(Value, EventType)]) -> AppState {
    let mut state = AppState::default();
    state.register_pane(PANE.to_string());
    for (payload, want) in payloads {
        let event = invoke_hook(agent, payload);
        assert_eq!(
            &event.event_type, want,
            "`hook --agent {agent}` mapped {payload} to the wrong event type"
        );
        state.apply_event(event);
    }
    state
}

fn status_of(state: &AppState) -> SessionStatus {
    state
        .sessions
        .get(SESSION)
        .unwrap_or_else(|| panic!("no card for {SESSION:?}; sessions: {:?}", state.sessions))
        .status
        .clone()
}

/// Scenario: Replay issue #1354's exact hook sequence through the real `hook
/// --agent claude-code` CLI: the main turn's `Bash` starts and ends, the turn
/// stops, then a background agent under the same session starts a `Bash` call
/// that never gets a `PostToolUse`, and `SubagentStop` follows. The card must
/// read Idle — the turn is over — not Working on the background agent's call.
#[spec("status/subagent/001")]
#[test]
fn subagent_001_background_tool_call_after_idle_leaves_card_idle() {
    // Codex's hooks engine posts the same shape — `agent_id` only inside a
    // thread-spawned subagent, the root thread's `session_id` throughout —
    // so the same sequence must leave its card Idle too.
    for agent in ["claude-code", "codex"] {
        let state = apply_all(
            agent,
            &[
                (session_start(), EventType::SessionStart),
                (
                    main_pre_tool_use("toolu_main_1", WORK_DONE),
                    EventType::ToolStart,
                ),
                (
                    main_post_tool_use("toolu_main_1", WORK_DONE),
                    EventType::ToolEnd,
                ),
                (stop(), EventType::Idle),
                (
                    subagent_pre_tool_use("toolu_bg_1", BACKGROUND_LS),
                    EventType::ToolStart,
                ),
                (subagent_stop(), EventType::SubagentStop),
            ],
        );

        assert_eq!(
            status_of(&state),
            SessionStatus::Idle,
            "`{agent}`: the main turn went Idle and only a background agent's tool call \
         followed; the card must not read Working (issue #1354)"
        );
        assert!(
            state.sessions[SESSION].active_tool.is_none(),
            "`{agent}`: the background agent's unfinished call must not be the card's active tool: {:?}",
            state.sessions[SESSION].active_tool
        );
    }
}

/// Scenario: The control for `status/subagent/001`. Replay the same sequence,
/// but make the trailing `Bash` call the MAIN thread's — once plain, and once
/// from a session started with `--agent`, where Claude Code stamps
/// `agent_type` without `agent_id`. That is a real new turn starting, so the
/// card must read Working with the call as its active tool: what keeps
/// `/001`'s card Idle is the subagent attribution, not a `ToolStart` that has
/// stopped counting.
#[spec("status/subagent/002")]
#[test]
fn subagent_002_main_thread_tool_call_after_idle_still_reads_working() {
    let plain = main_pre_tool_use("toolu_main_2", BACKGROUND_LS);
    let agent_session = with(
        main_pre_tool_use("toolu_main_2", BACKGROUND_LS),
        json!({"agent_type": "reviewer"}),
    );
    for trailing in [plain, agent_session] {
        let state = apply_all(
            "claude-code",
            &[
                (session_start(), EventType::SessionStart),
                (
                    main_pre_tool_use("toolu_main_1", WORK_DONE),
                    EventType::ToolStart,
                ),
                (
                    main_post_tool_use("toolu_main_1", WORK_DONE),
                    EventType::ToolEnd,
                ),
                (stop(), EventType::Idle),
                (trailing.clone(), EventType::ToolStart),
            ],
        );

        assert_eq!(
            status_of(&state),
            SessionStatus::Working,
            "a main-thread tool call after Idle is a new turn and must read Working; payload={trailing}"
        );
        assert_eq!(
            state.sessions[SESSION]
                .active_tool
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Bash"),
            "the main thread's call must be the active tool; payload={trailing}"
        );
    }
}

fn subagent_post_tool_use(tool_use_id: &str, command: &str, response: Value) -> Value {
    with(
        base("PostToolUse"),
        json!({
            "tool_name": "Bash",
            "tool_use_id": tool_use_id,
            "tool_input": {"command": command},
            "tool_response": response,
            "agent_id": SUBAGENT_ID,
            "agent_type": "general-purpose",
        }),
    )
}

/// Scenario: The same shape as `status/subagent/001`, but the background
/// agent's call does finish — and fails, with a non-zero exit. A failed main-
/// thread call surfaces as Error on the card; a background agent's must not,
/// or the card that went Idle at the end of the turn is left on Error instead
/// of Working. The card must read Idle, and the call still counts as a tool.
#[spec("status/subagent/003")]
#[test]
fn subagent_003_background_failed_tool_call_after_idle_leaves_card_idle() {
    let failed =
        json!({"stdout": "", "stderr": "ls: cannot access", "exit_code": 2, "interrupted": false});
    let state = apply_all(
        "claude-code",
        &[
            (session_start(), EventType::SessionStart),
            (
                main_pre_tool_use("toolu_main_1", WORK_DONE),
                EventType::ToolStart,
            ),
            (
                main_post_tool_use("toolu_main_1", WORK_DONE),
                EventType::ToolEnd,
            ),
            (stop(), EventType::Idle),
            (
                subagent_pre_tool_use("toolu_bg_1", BACKGROUND_LS),
                EventType::ToolStart,
            ),
            (
                subagent_post_tool_use("toolu_bg_1", BACKGROUND_LS, failed.clone()),
                EventType::ToolEnd,
            ),
            (subagent_stop(), EventType::SubagentStop),
        ],
    );
    assert_eq!(
        status_of(&state),
        SessionStatus::Idle,
        "a background agent's failed call after the turn ended must not put the card on Error"
    );
    assert_eq!(state.sessions[SESSION].tool_count, 2);

    // Control: the same failed response on a MAIN-thread call is an Error.
    let mut main_failed = main_post_tool_use("toolu_main_2", BACKGROUND_LS);
    main_failed["tool_response"] = failed;
    let state = apply_all(
        "claude-code",
        &[
            (session_start(), EventType::SessionStart),
            (
                main_pre_tool_use("toolu_main_2", BACKGROUND_LS),
                EventType::ToolStart,
            ),
            (main_failed, EventType::Error),
        ],
    );
    assert_eq!(status_of(&state), SessionStatus::Error);
}

/// Scenario: A FOREGROUND subagent inside a live turn: the main thread calls
/// its `Agent` tool (the card reads Working on it), the subagent runs a `Bash`
/// call, raises a permission prompt the user answers, and finishes; the main
/// thread's `Agent` call ends and the turn stops. The card reads Working on
/// the `Agent` call throughout the subagent's own calls, reads Needs Input on
/// the subagent's prompt and leaves it when that call ends, and reads Idle at
/// the end — ignoring a subagent's tool calls for the badge costs none of that.
#[spec("status/subagent/004")]
#[test]
fn subagent_004_foreground_subagent_keeps_the_turn_working_and_its_prompt_answerable() {
    let agent_call = |event: &str| {
        with(
            base(event),
            json!({
                "tool_name": "Agent",
                "tool_use_id": "toolu_agent_1",
                "tool_input": {"description": "survey the tests", "prompt": "list them"},
            }),
        )
    };
    let permission = with(
        base("PermissionRequest"),
        json!({
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf target"},
            "agent_id": SUBAGENT_ID,
            "agent_type": "general-purpose",
        }),
    );
    let ok = json!({"stdout": "", "stderr": "", "interrupted": false});

    let mut state = AppState::default();
    state.register_pane(PANE.to_string());
    let active = |state: &AppState| {
        state.sessions[SESSION]
            .active_tool
            .as_ref()
            .map(|t| t.name.clone())
    };
    let steps: Vec<(Value, SessionStatus, Option<&str>)> = vec![
        (session_start(), SessionStatus::Idle, None),
        (
            agent_call("PreToolUse"),
            SessionStatus::Working,
            Some("Agent"),
        ),
        (
            subagent_pre_tool_use("toolu_sub_1", "ls tests"),
            SessionStatus::Working,
            Some("Agent"),
        ),
        (
            subagent_post_tool_use("toolu_sub_1", "ls tests", ok.clone()),
            SessionStatus::Working,
            Some("Agent"),
        ),
        (permission, SessionStatus::WaitingForInput, Some("Agent")),
        (
            subagent_post_tool_use("toolu_sub_2", "rm -rf target", ok),
            SessionStatus::Thinking,
            Some("Agent"),
        ),
        (subagent_stop(), SessionStatus::Thinking, Some("Agent")),
        (agent_call("PostToolUse"), SessionStatus::Thinking, None),
        (stop(), SessionStatus::Idle, None),
    ];
    for (payload, want_status, want_tool) in steps {
        state.apply_event(invoke_hook("claude-code", &payload));
        assert_eq!(status_of(&state), want_status, "after {payload}");
        assert_eq!(active(&state).as_deref(), want_tool, "after {payload}");
    }
}

/// Scenario: With the card's turn over (Idle), the daemon's shell-activity scan
/// sets a synthetic Working (`ShellBusy`) for a detached shell command, a
/// background subagent's tool call arrives, and the command then finishes
/// (`ShellIdle`). The card must return to Idle, because the subagent's call says
/// nothing about the main thread and must not take the synthetic Working over
/// as its own and strand it. The control — a main-thread call in the same spot —
/// is a real turn taking over, so the card stays Working through the `ShellIdle`.
#[spec("status/subagent/005")]
#[test]
fn subagent_005_subagent_call_does_not_strand_a_synthetic_shell_working() {
    for (trailing, from_subagent) in [
        (subagent_pre_tool_use("toolu_bg_1", BACKGROUND_LS), true),
        (main_pre_tool_use("toolu_main_2", BACKGROUND_LS), false),
    ] {
        let mut state = apply_all(
            "claude-code",
            &[
                (session_start(), EventType::SessionStart),
                (stop(), EventType::Idle),
            ],
        );
        let shell = |event_type: EventType| {
            let mut event = invoke_hook("claude-code", &stop());
            event.event_type = event_type;
            event
        };
        state.apply_event(shell(EventType::ShellBusy));
        assert_eq!(status_of(&state), SessionStatus::Working);

        let call = invoke_hook("claude-code", &trailing);
        assert_eq!(call.is_from_subagent(), from_subagent);
        state.apply_event(call);
        state.apply_event(shell(EventType::ShellIdle));

        let want = if from_subagent {
            SessionStatus::Idle
        } else {
            SessionStatus::Working
        };
        assert_eq!(
            status_of(&state),
            want,
            "after ShellBusy, {} ToolStart, ShellIdle",
            if from_subagent {
                "a subagent"
            } else {
                "a main-thread"
            }
        );
    }
}

/// A permission request raised inside the subagent `subagent_id`.
fn subagent_permission_request(subagent_id: &str) -> Value {
    with(
        base("PermissionRequest"),
        json!({
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf target"},
            "agent_id": subagent_id,
            "agent_type": "general-purpose",
        }),
    )
}

/// `subagent_id`'s terminal event: `SubagentStop`, or — Claude Code only — the
/// `StopFailure` an API error ends it with (issue #714), which the hook CLI
/// turns into the `SubagentStop` it stands in for.
fn subagent_end(terminal: &str, subagent_id: &str) -> Value {
    let extra = match terminal {
        "StopFailure" => json!({
            "error": "rate_limit",
            "last_assistant_message": "API Error: rate limit",
            "agent_id": subagent_id,
            "agent_type": "general-purpose",
        }),
        _ => json!({
            "stop_hook_active": false,
            "agent_id": subagent_id,
            "agent_type": "general-purpose",
            "last_assistant_message": "",
        }),
    };
    with(base(terminal), extra)
}

/// One hook payload and the event type it must become.
type Step = (Value, EventType);

/// Scenario: A subagent raises a permission request — the card reads Needs Input — and then ends without the call it asked about ever running: a plain `SubagentStop`, or for Claude Code a `StopFailure`. For a background subagent after the turn ended the card must return to Idle, and inside a live turn to Thinking, never Working, Error or Blocked; the controls keep Needs Input while the prompt is still someone's — a different subagent stopping, a second waiting subagent still running, and a wait the main thread raised.
#[spec("status/subagent/006")]
#[test]
fn subagent_006_a_subagent_that_ends_takes_its_permission_prompt_with_it() {
    let agent_call = with(
        base("PreToolUse"),
        json!({
            "tool_name": "Agent",
            "tool_use_id": "toolu_agent_1",
            "tool_input": {"description": "survey the tests", "prompt": "list them"},
        }),
    );
    let main_permission = with(
        base("PermissionRequest"),
        json!({"tool_name": "Bash", "tool_input": {"command": "cargo clean"}}),
    );
    for (agent, terminal) in [
        ("claude-code", "SubagentStop"),
        ("claude-code", "StopFailure"),
        ("codex", "SubagentStop"),
    ] {
        let end =
            |subagent_id: &str| (subagent_end(terminal, subagent_id), EventType::SubagentStop);
        let cases: Vec<(&str, Vec<Step>, SessionStatus)> = vec![
            (
                "a background subagent's prompt, after the turn went Idle",
                vec![
                    (stop(), EventType::Idle),
                    (
                        subagent_permission_request(SUBAGENT_ID),
                        EventType::PermissionRequest,
                    ),
                    end(SUBAGENT_ID),
                ],
                SessionStatus::Idle,
            ),
            (
                "a foreground subagent's prompt, inside the main thread's Agent call",
                vec![
                    (agent_call.clone(), EventType::ToolStart),
                    (
                        subagent_permission_request(SUBAGENT_ID),
                        EventType::PermissionRequest,
                    ),
                    end(SUBAGENT_ID),
                ],
                SessionStatus::Thinking,
            ),
            (
                "both waiting subagents ended",
                vec![
                    (stop(), EventType::Idle),
                    (
                        subagent_permission_request(SUBAGENT_ID),
                        EventType::PermissionRequest,
                    ),
                    (
                        subagent_permission_request("b9d0"),
                        EventType::PermissionRequest,
                    ),
                    end("b9d0"),
                    end(SUBAGENT_ID),
                ],
                SessionStatus::Idle,
            ),
            (
                "control: a DIFFERENT subagent ended",
                vec![
                    (stop(), EventType::Idle),
                    (
                        subagent_permission_request(SUBAGENT_ID),
                        EventType::PermissionRequest,
                    ),
                    end("b9d0"),
                ],
                SessionStatus::WaitingForInput,
            ),
            (
                "control: one of two waiting subagents ended",
                vec![
                    (stop(), EventType::Idle),
                    (
                        subagent_permission_request(SUBAGENT_ID),
                        EventType::PermissionRequest,
                    ),
                    (
                        subagent_permission_request("b9d0"),
                        EventType::PermissionRequest,
                    ),
                    end(SUBAGENT_ID),
                ],
                SessionStatus::WaitingForInput,
            ),
            (
                "control: the MAIN thread's prompt, which a subagent then joined and left",
                vec![
                    (stop(), EventType::Idle),
                    (main_permission.clone(), EventType::PermissionRequest),
                    (
                        subagent_permission_request(SUBAGENT_ID),
                        EventType::PermissionRequest,
                    ),
                    end(SUBAGENT_ID),
                ],
                SessionStatus::WaitingForInput,
            ),
        ];
        for (case, sequence, want) in cases {
            let mut payloads = vec![(session_start(), EventType::SessionStart)];
            payloads.extend(sequence);
            let state = apply_all(agent, &payloads);
            assert_eq!(
                status_of(&state),
                want,
                "`{agent}`, ended by {terminal}: {case} (issue #1364)"
            );
            assert!(
                state.sessions[SESSION].blocked.is_none(),
                "`{agent}`, ended by {terminal}: {case} left a quota reason on the parent"
            );
        }
    }
}

/// The card a TUI holds for `PANE`, whichever session it is keyed under — a
/// hydrated card starts under a placeholder id.
fn pane_status_of(state: &AppState) -> SessionStatus {
    state
        .sessions
        .values()
        .filter(|session| session.pane_id.as_deref() == Some(PANE))
        .max_by_key(|session| session.last_activity)
        .unwrap_or_else(|| panic!("no card for {PANE:?}; sessions: {:?}", state.sessions))
        .status
        .clone()
}

/// Scenario: A background subagent raises a permission request after the turn ended, and only then does a TUI attach, restoring the card from the daemon's live snapshot as it arrives over the wire. When that subagent then stops, the attached TUI's card must leave Needs Input for Idle, as the daemon's does; the control is a snapshot from a daemon that sends no subagent attribution, whose card keeps today's Needs Input.
#[spec("status/subagent/007")]
#[test]
fn subagent_007_a_tui_that_attaches_mid_wait_still_ends_it_with_the_subagent() {
    let daemon = apply_all(
        "claude-code",
        &[
            (session_start(), EventType::SessionStart),
            (stop(), EventType::Idle),
            (
                subagent_permission_request(SUBAGENT_ID),
                EventType::PermissionRequest,
            ),
        ],
    );
    assert_eq!(status_of(&daemon), SessionStatus::WaitingForInput);
    let wire = serde_json::to_value(daemon.sessions[SESSION].live_snapshot())
        .expect("serialize the live snapshot");
    let stop_event = invoke_hook("claude-code", &subagent_end("SubagentStop", SUBAGENT_ID));

    for (case, snapshot, want) in [
        ("this daemon's snapshot", wire.clone(), SessionStatus::Idle),
        (
            "control: a snapshot without the attribution, as an older daemon sends",
            {
                let mut older = wire.clone();
                older
                    .as_object_mut()
                    .expect("a snapshot is a JSON object")
                    .remove("subagent_wait");
                older
            },
            SessionStatus::WaitingForInput,
        ),
    ] {
        let snapshot: dot_agent_deck::state::SessionSnapshot =
            serde_json::from_value(snapshot).expect("decode the live snapshot");
        let mut tui = AppState::default();
        tui.register_pane(PANE.to_string());
        tui.seed_hydrated_session(PANE.to_string(), None, None, None, Some(&snapshot));
        assert_eq!(
            pane_status_of(&tui),
            SessionStatus::WaitingForInput,
            "precondition ({case}): the hydrated card reads Needs Input"
        );
        tui.apply_event(stop_event.clone());
        assert_eq!(
            pane_status_of(&tui),
            want,
            "{case}: the subagent that raised the wait stopped (issue #1364)"
        );
    }
}
