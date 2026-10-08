#![cfg(feature = "e2e")]

//! L2 end-to-end hook-delivery tests. Each function spawns the real
//! `dot-agent-deck` binary inside an isolated PTY, writes a hook
//! payload to the per-test hook socket, and asserts on the rendered
//! grid through a `vt100` parser. PRD #77 Decision 2 + Decision 6.
//!
//! Decision 6: this file is gated behind the `e2e` feature so CI
//! (which runs only `cargo test-fast`) never compiles it.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// Scenario: Start a daemon-owned stand-in pane and send Claude hooks through the real hook CLI with that pane's own identity and capability. A client subscribing after the first completed turn must receive only later final replies, with the selected agent's identity and increasing sequence, without replaying the earlier reply or reading the terminal's sentinel.
#[spec("voice/reading-reply/001")]
#[cfg(unix)]
#[test]
fn reading_reply_001_subscription_delivers_only_future_selected_agent_replies() {
    use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
    use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery, StartAgentOptions};
    use dot_agent_deck::event::{AgentType, BroadcastMsg, EventType};
    use dot_agent_deck::hook_provenance::DOT_AGENT_DECK_PANE_CAPABILITY;
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    const SESSION: &str = "reading1497";
    const PANE: &str = "pane-reading-reply";
    const EARLIER: &str = "BEFORE_SUBSCRIPTION_1497_a31c";
    const REPLY: &str = "AFTER_SUBSCRIPTION_1497_b62d: all tests pass.";
    const SECOND: &str = "NEXT_TURN_REPLY_1497_c93e: two files changed.";
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("reply client runtime");
    let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
    let agent_id = runtime
        .block_on(client.start_agent(StartAgentOptions {
            command: Some(common::capability_export_command(
                "printf '%s\\n' 'TERMINAL_ONLY_1497_d24f'; cat",
            )),
            cwd: Some(deck.workdir().to_string_lossy().into_owned()),
            display_name: Some("reading-pane".into()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.into(), PANE.into())],
            agent_type: Some(AgentType::ClaudeCode),
            ..Default::default()
        }))
        .expect("daemon-owned synthetic pane");
    let token = runtime
        .block_on(common::recorded_hook_capability(deck.workdir(), &agent_id))
        .expect("stand-in's own capability");

    let send_hook = |event: &str, extra: serde_json::Value| {
        let mut payload = serde_json::json!({
            "session_id": SESSION,
            "hook_event_name": event,
            "cwd": deck.workdir(),
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["hook", "--agent", "claude-code"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", deck.home_dir())
            .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
            .env(DOT_AGENT_DECK_PANE_ID, PANE)
            .env(DOT_AGENT_DECK_AGENT_ID, &agent_id)
            .env(DOT_AGENT_DECK_PANE_CAPABILITY, &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Claude hook CLI");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "hook CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let mut status_stream = runtime
        .block_on(client.subscribe_events())
        .expect("hook ingestion barrier");
    send_hook("SessionStart", serde_json::json!({"source": "startup"}));
    send_hook(
        "UserPromptSubmit",
        serde_json::json!({"prompt": "first synthetic turn"}),
    );
    send_hook(
        "Stop",
        serde_json::json!({"last_assistant_message": EARLIER}),
    );
    // Await the earlier Stop on the client's existing hook stream, so it was
    // processed by the daemon before the new subscription is opened.
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = status_stream
                    .next_event()
                    .await
                    .unwrap()
                    .expect("status stream live");
                if matches!(event, BroadcastMsg::Event(ref event)
                    if event.session_id == SESSION && event.event_type == EventType::Idle)
                {
                    break;
                }
            }
        })
        .await
        .expect("earlier Stop reaches the client before reading on");
    });
    deck.wait_for_string("reading-pane");

    let GatedQuery::Answered(mut replies) = runtime
        .block_on(client.subscribe_turn_replies(&agent_id))
        .expect("subscribe through the client library")
    else {
        panic!("this daemon must advertise turn-replies");
    };
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(Duration::from_millis(300), replies.next_reply())
                .await
                .is_err(),
            "reading on must not replay a reply completed before subscription"
        );
    });
    send_hook(
        "UserPromptSubmit",
        serde_json::json!({"prompt": "second synthetic turn"}),
    );
    send_hook("Stop", serde_json::json!({"last_assistant_message": REPLY}));
    let first = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), replies.next_reply())
            .await
            .expect("final reply delivered promptly")
            .expect("decode final reply")
            .expect("reply stream live")
    });
    assert_eq!(first.agent_id, agent_id);
    assert_eq!(first.pane_id, PANE);
    assert_eq!(
        first.reply.text, REPLY,
        "deliver the hook's final reply, without terminal content or backlog"
    );
    assert!(!first.reply.failed);
    assert!(first.sequence > 0);

    send_hook(
        "UserPromptSubmit",
        serde_json::json!({"prompt": "third synthetic turn"}),
    );
    send_hook(
        "Stop",
        serde_json::json!({"last_assistant_message": SECOND}),
    );
    let second = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), replies.next_reply())
            .await
            .expect("next turn delivered promptly")
            .expect("decode next reply")
            .expect("reply stream live")
    });
    assert_eq!(second.agent_id, agent_id);
    assert_eq!(second.pane_id, PANE);
    assert_eq!(second.reply.text, SECOND);
    assert!(!second.reply.failed);
    assert!(
        second.sequence > first.sequence,
        "successive turns have increasing identifiers"
    );
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(Duration::from_millis(300), replies.next_reply())
                .await
                .is_err(),
            "each finished turn is delivered once"
        );
    });
}

/// Scenario: Launch the deck against the `minimal` fixture, wait
/// for the empty dashboard to render, then write a synthetic
/// Claude Code `SessionStart` hook payload (with `pane_id =
/// pane-m2-001`, `session_id = m2demo`, `agent_type = claude_code`)
/// directly to the per-test hook socket. The deck's daemon auto-
/// registers the unknown pane on its first `SessionStart` event,
/// so a card titled `m2demo` should appear on the dashboard within
/// the test budget. No real LLM tokens are spent — the harness
/// injects the event in-process.
#[spec("hooks/delivery/001")]
#[test]
fn delivery_001_session_start_creates_card() {
    // PRD #77 catalog: hooks/delivery/001 — A Claude Code SessionStart
    // hook arriving at the daemon's hook socket creates a session entry
    // on the dashboard. The harness redirects `DOT_AGENT_DECK_SOCKET`
    // to a per-test path so the deck-spawned daemon binds there;
    // `write_hook_line` then injects the JSON payload that the daemon
    // already accepts on the hook socket (see `run_hook_loop` in
    // `src/daemon.rs`).
    let deck = TuiDeck::launch_with_fixture("minimal");

    // Wait for the deck to finish painting its initial dashboard so the
    // attach-side `subscribe_events` connection is live before we inject
    // — otherwise a fast write can land before the TUI subscribes. The
    // empty-state line is sufficient evidence the dashboard rendered;
    // wait_until_quiescent would race the TUI's periodic redraw tick.
    deck.wait_for_string("No active agents");

    // The hook event uses a session_id short enough to render in full
    // (the dashboard truncates to 11 chars), and a fresh pane_id that
    // the deck has not seen — `apply_event`'s SessionStart auto-register
    // branch will adopt it and a card will appear.
    let event = serde_json::json!({
        "session_id": "m2demo",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-05-26T12:00:00Z",
        "pane_id": "pane-m2-001",
    });

    write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write SessionStart hook to per-test socket");

    // Asserting via `wait_for_string` against the rendered grid — the
    // catalog explicitly says "loose substring match on the session_id
    // or display_name".
    deck.wait_for_string("m2demo");
}

/// Pipe one Claude Code hook payload through the REAL `dot-agent-deck hook
/// --agent claude-code` CLI at the deck's per-test hook socket, the way
/// Claude Code's installed hook command delivers it.
fn claude_hook_via_cli(deck: &TuiDeck, pane_id: &str, payload: &serde_json::Value) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hook", "--agent", "claude-code"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", deck.home_dir())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", pane_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `dot-agent-deck hook --agent claude-code`");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write hook payload");
    let output = child.wait_with_output().expect("wait for hook CLI");
    assert!(
        output.status.success(),
        "hook CLI refused {payload}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Scenario: Launch the deck, then replay issue #1354's hook sequence for one
/// Claude session through the real `hook --agent claude-code` CLI: the main
/// turn's `Bash` starts (the card reads Working) and ends, the turn stops (the
/// card reads Idle), then a background agent under the same session starts a
/// `Bash` call that never ends and `SubagentStop` follows. The card must still
/// read Idle, and keep reading it, rather than flipping back to Working. Then
/// another background subagent raises a permission request (the card reads
/// Needs Input) and fails with a `StopFailure`: the card must go back to Idle
/// (issue #1364).
#[spec("hooks/delivery/008")]
#[test]
fn delivery_008_background_subagent_tool_call_does_not_flip_idle_card_to_working() {
    const PANE: &str = "pane-sub-1354";
    const SESSION: &str = "sub1354";
    const SUBAGENT_ID: &str = "a7c1e0b2d9f34e18";
    let payload = |event: &str, extra: serde_json::Value| {
        let mut value = serde_json::json!({
            "session_id": SESSION,
            "transcript_path": format!("/home/user/.claude/projects/wt/{SESSION}.jsonl"),
            "cwd": "/home/user/code/wt",
            "permission_mode": "default",
            "hook_event_name": event,
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        value
    };
    let bash = |id: &str, command: &str| {
        serde_json::json!({
            "tool_name": "Bash",
            "tool_use_id": id,
            "tool_input": {"command": command},
        })
    };
    // Only the one card is on screen, so a status word anywhere in the grid
    // is that card's badge.
    let card_reads =
        |status: &'static str| move |grid: &str| grid.contains(SESSION) && grid.contains(status);

    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    claude_hook_via_cli(
        &deck,
        PANE,
        &payload("SessionStart", serde_json::json!({"source": "startup"})),
    );
    deck.wait_for_string(SESSION);

    let work_done = "git status --short; dot-agent-deck work-done --task-file report.md";
    claude_hook_via_cli(
        &deck,
        PANE,
        &payload("PreToolUse", bash("toolu_main_1", work_done)),
    );
    // Proves the needle can appear at all, so the Idle hold below is not
    // passing on a card that never shows Working.
    deck.wait_until_grid(
        "card reads Working on the main turn's Bash",
        card_reads("Working"),
    );

    let mut end = bash("toolu_main_1", work_done);
    end["tool_response"] = serde_json::json!({"stdout": "", "stderr": "", "interrupted": false});
    claude_hook_via_cli(&deck, PANE, &payload("PostToolUse", end));
    claude_hook_via_cli(
        &deck,
        PANE,
        &payload("Stop", serde_json::json!({"stop_hook_active": false})),
    );
    deck.wait_until_grid("card reads Idle after the turn stops", |g| {
        card_reads("Idle")(g) && !g.contains("Working")
    });

    // The background agent: Claude Code stamps `agent_id` on every hook that
    // fires inside a subagent's context. No PostToolUse ever arrives.
    let mut background = bash(
        "toolu_bg_1",
        "ls .dot-agent-deck/worker-task-*.md 2>/dev/null",
    );
    background["agent_id"] = SUBAGENT_ID.into();
    background["agent_type"] = "general-purpose".into();
    claude_hook_via_cli(&deck, PANE, &payload("PreToolUse", background));
    claude_hook_via_cli(
        &deck,
        PANE,
        &payload(
            "SubagentStop",
            serde_json::json!({
                "stop_hook_active": false,
                "agent_id": SUBAGENT_ID,
                "agent_type": "general-purpose",
                "agent_transcript_path": "/home/user/.claude/projects/wt/agent.jsonl",
            }),
        ),
    );
    // The background call reaches the card's tool history either way — it
    // is work the session did — which is also the barrier: once it is on
    // screen, the event that used to flip the badge has been applied.
    deck.wait_for_string("worker-task");

    deck.wait_until_grid_then_hold(
        "card still reads Idle after the background agent's unfinished call",
        std::time::Duration::from_secs(2),
        |g| card_reads("Idle")(g) && !g.contains("Working"),
    );

    // Issue #1364: a background subagent asks for permission — the card reads
    // Needs Input, which also proves that needle can appear — and then fails
    // (`StopFailure`, which the hook CLI turns into the `SubagentStop` it
    // stands in for). Its prompt is gone, so the card must return to Idle,
    // not keep asking, and not turn Working, Error or Blocked.
    const PROMPTING_SUBAGENT: &str = "b9d0f1e2a3c4d5e6";
    claude_hook_via_cli(
        &deck,
        PANE,
        &payload(
            "PermissionRequest",
            serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "rm -rf target"},
                "agent_id": PROMPTING_SUBAGENT,
                "agent_type": "general-purpose",
            }),
        ),
    );
    deck.wait_until_grid(
        "card reads Needs Input on the background subagent's permission request",
        card_reads("Needs Input"),
    );
    claude_hook_via_cli(
        &deck,
        PANE,
        &payload(
            "StopFailure",
            serde_json::json!({
                "error": "rate_limit",
                "last_assistant_message": "API Error: rate limit",
                "agent_id": PROMPTING_SUBAGENT,
                "agent_type": "general-purpose",
            }),
        ),
    );
    deck.wait_until_grid_then_hold(
        "card leaves Needs Input for Idle once the prompting subagent has ended",
        std::time::Duration::from_secs(2),
        |g| {
            card_reads("Idle")(g)
                && !g.contains("Needs Input")
                && !g.contains("Working")
                && !g.contains("Blocked")
                && !g.contains("Error")
        },
    );
}
