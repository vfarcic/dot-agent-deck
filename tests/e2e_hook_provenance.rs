#![cfg(feature = "e2e")]

//! L2 synthetic coverage for hook-event provenance. These tests drive the real
//! daemon and TUI binaries, but write deterministic JSON directly to the hook
//! socket instead of launching an LLM.

mod common;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::{DaemonProc, TuiDeck, spawn_daemon_serve, write_hook_line};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

fn launch_tui_against(daemon: &DaemonProc) -> TuiDeck {
    TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy().to_string(),
        )
        .launch_with_fixture("minimal")
}

/// Write exactly the supplied JSON and a newline, with no capability token
/// added by the harness. This path must remain token-less after the production
/// token is threaded through the ordinary synthetic-hook helper.
fn write_tokenless_hook_line(socket: &std::path::Path, event: &serde_json::Value) {
    let mut stream = UnixStream::connect(socket).expect("connect to hook socket");
    let line = event.to_string();
    stream
        .write_all(line.as_bytes())
        .expect("write token-less hook JSON");
    stream
        .write_all(b"\n")
        .expect("terminate token-less hook JSON line");
    stream.flush().expect("flush token-less hook JSON");
}

/// Scenario: Start two daemon-managed panes and prove pane A's token drives A when the hook correctly names A. After resetting A to Idle, send the same ToolStart with A's token but pane B's claimed pane and agent ids; A must return to Working while B remains Idle.
#[spec("hooks/provenance/001")]
#[test]
fn provenance_001_token_binding_overrides_claimed_pane_and_agent() {
    const PANE_A: &str = "provenance-token-pane-a";
    const PANE_B: &str = "provenance-token-pane-b";
    const LABEL_A: &str = "provenance-token-card-a";
    const LABEL_B: &str = "provenance-token-card-b";
    const SESSION_ID: &str = "tokenbound";

    let daemon = spawn_daemon_serve(None, "0");
    // Issue #318: this test IS the spawning peer, so each pane's capability
    // token comes straight off its own `StartAgent` reply — the one place the
    // daemon ever hands a token to a peer. There is deliberately no request that
    // fetches a token afterwards (an earlier revision had one; the security
    // audit rejected it, because unauthenticated `ListAgents` on the same socket
    // would then have made every pane's token available to any same-user
    // process).
    let mut tokens = std::collections::HashMap::new();
    for (pane_id, label, agent_type) in [
        (PANE_A, LABEL_A, AgentType::Pi),
        (PANE_B, LABEL_B, AgentType::Devin),
    ] {
        let response = daemon
            .send_attach_request(&AttachRequest::StartAgent {
                command: Some("/bin/sh".into()),
                cwd: None,
                rows: 24,
                cols: 80,
                env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane_id.into())],
                display_name: Some(label.into()),
                tab_membership: None,
                agent_type: Some(agent_type),
                seed: None,
            })
            .unwrap_or_else(|error| panic!("StartAgent for {pane_id}: {error}"));
        assert!(
            response.error.is_none(),
            "StartAgent for {pane_id} should succeed, got error: {:?}",
            response.error
        );
        let token = response.agent_token.clone().unwrap_or_else(|| {
            panic!("StartAgent for {pane_id} must return that agent's capability token")
        });
        tokens.insert(pane_id, token);
    }

    let records = daemon.wait_for_agent_count(2, Duration::from_secs(5));
    let agent_id = |pane_id: &str| {
        records
            .iter()
            .find(|record| record.pane_id_env.as_deref() == Some(pane_id))
            .unwrap_or_else(|| panic!("managed pane {pane_id} never registered: {records:?}"))
            .id
            .clone()
    };
    let agent_a = agent_id(PANE_A);
    let agent_b = agent_id(PANE_B);
    let token_a = tokens
        .remove(PANE_A)
        .expect("pane A's token was captured from its StartAgent reply");

    // One card per row keeps each type/status pair on its own rendered line,
    // so the two cards cannot satisfy each other's status assertions.
    let deck = TuiDeck::builder()
        .with_pty_size(70, 40)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy().to_string(),
        )
        .launch_with_fixture("minimal");
    deck.wait_until_grid("both managed cards begin Idle", |grid| {
        grid.lines()
            .any(|line| line.contains("Pi") && line.contains("Idle"))
            && grid
                .lines()
                .any(|line| line.contains("Devin") && line.contains("Idle"))
    });

    let correctly_named = serde_json::json!({
        "session_id": SESSION_ID,
        "agent_type": "pi",
        "event_type": "tool_start",
        "tool_name": "Bash",
        "tool_detail": "printf provenance",
        "timestamp": "2026-08-24T12:00:00Z",
        "pane_id": PANE_A,
        "agent_id": agent_a,
    });
    write_hook_line(
        &daemon.hook_socket,
        &correctly_named.to_string(),
        Some(&token_a),
    )
    .expect("write correctly named token-bearing ToolStart");
    deck.wait_until_grid("pane A's own token drives pane A", |grid| {
        grid.lines()
            .any(|line| line.contains("Pi") && line.contains("Working"))
            && grid
                .lines()
                .any(|line| line.contains("Devin") && line.contains("Idle"))
    });

    let idle = serde_json::json!({
        "session_id": SESSION_ID,
        "agent_type": "pi",
        "event_type": "idle",
        "timestamp": "2026-08-24T12:00:01Z",
        "pane_id": PANE_A,
        "agent_id": agent_a,
    });
    write_hook_line(&daemon.hook_socket, &idle.to_string(), Some(&token_a))
        .expect("reset pane A to Idle before the false-claim control");
    deck.wait_until_grid("both cards are Idle before the false claim", |grid| {
        grid.lines()
            .any(|line| line.contains("Pi") && line.contains("Idle"))
            && grid
                .lines()
                .any(|line| line.contains("Devin") && line.contains("Idle"))
    });

    let mut false_claim = correctly_named;
    false_claim["timestamp"] = serde_json::json!("2026-08-24T12:00:02Z");
    false_claim["pane_id"] = serde_json::json!(PANE_B);
    false_claim["agent_id"] = serde_json::json!(agent_b);
    write_hook_line(
        &daemon.hook_socket,
        &false_claim.to_string(),
        Some(&token_a),
    )
    .expect("write pane A's token with pane B's claimed identity");

    deck.wait_until_grid_then_hold(
        "pane A's token is rebound to A while pane B stays Idle",
        Duration::from_secs(1),
        |grid| {
            grid.lines()
                .any(|line| line.contains("Pi") && line.contains("Working"))
                && grid
                    .lines()
                    .any(|line| line.contains("Devin") && line.contains("Idle"))
        },
    );
}

/// Scenario: Start a daemon-managed pane and attach the real TUI so its Idle card is visible, then write a deliberately token-less ToolStart naming that pane and agent directly to the hook socket. The managed card must remain Idle rather than moving to Working.
#[spec("hooks/provenance/002")]
#[test]
fn provenance_002_tokenless_event_cannot_drive_managed_card() {
    const PANE_ID: &str = "managed-provenance-pane";
    const LABEL: &str = "managed-provenance-card";

    let daemon = spawn_daemon_serve(None, "0");
    let response = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some("sh -c 'sleep 600'".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), PANE_ID.into())],
            display_name: Some(LABEL.into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            seed: None,
        })
        .expect("StartAgent managed provenance pane over the attach socket");
    assert!(
        response.error.is_none(),
        "StartAgent should succeed, got error: {:?}",
        response.error
    );
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(5));
    let agent_id = records
        .first()
        .unwrap_or_else(|| panic!("managed pane never registered: {records:?}"))
        .id
        .clone();

    let deck = launch_tui_against(&daemon);
    deck.wait_until_grid("managed card begins Idle", |grid| {
        grid.contains(LABEL)
            && grid
                .lines()
                .any(|line| line.contains("ClaudeCode") && line.contains("Idle"))
    });

    let event = serde_json::json!({
        "session_id": "forged-managed-session",
        "agent_type": "claude_code",
        "event_type": "tool_start",
        "tool_name": "Bash",
        "tool_detail": "printf forged",
        "timestamp": "2026-08-24T12:00:00Z",
        "pane_id": PANE_ID,
        "agent_id": agent_id,
    });
    write_tokenless_hook_line(&daemon.hook_socket, &event);

    deck.wait_until_grid_then_hold(
        "managed card remains Idle after a token-less hook",
        Duration::from_secs(1),
        |grid| {
            grid.contains(LABEL)
                && grid
                    .lines()
                    .any(|line| line.contains("ClaudeCode") && line.contains("Idle"))
        },
    );
}

/// Scenario: Launch the real deck with no managed panes, then write a deliberately token-less SessionStart for an unknown pane directly to the hook socket. The foreign card must still register and render, preserving the compatibility path intentionally left open by issue #601.
#[spec("hooks/provenance/003")]
#[test]
fn provenance_003_tokenless_event_still_registers_foreign_card() {
    const SESSION_ID: &str = "foreignok";

    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let event = serde_json::json!({
        "session_id": SESSION_ID,
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-08-24T12:00:00Z",
        "pane_id": "foreign-unmanaged-pane",
    });
    write_tokenless_hook_line(deck.hook_socket_path(), &event);

    deck.wait_until_grid("token-less foreign card is registered", |grid| {
        grid.contains(SESSION_ID)
            && grid
                .lines()
                .any(|line| line.contains("ClaudeCode") && line.contains("Idle"))
    });
}

/// Write `line` verbatim, with a trailing newline and no harness-added token.
///
/// Distinct from [`write_tokenless_hook_line`] because that one takes a
/// `serde_json::Value`, and re-serializing a `Value` normalizes an escaped
/// member name back to its plain spelling — which is precisely the property
/// under test here.
fn write_raw_hook_line(socket: &std::path::Path, line: &str) {
    let mut stream = UnixStream::connect(socket).expect("connect to hook socket");
    stream
        .write_all(line.as_bytes())
        .expect("write raw hook JSON");
    stream
        .write_all(b"\n")
        .expect("terminate raw hook JSON line");
    stream.flush().expect("flush raw hook JSON");
}

/// Read the daemon's log file, tolerating "not created yet".
fn read_log(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Scenario: Start a headless daemon with its log redirected into the test's own
/// temp dir, then write one hook line whose `agent_token` member name is spelled
/// with a JSON escape (`agent_\u0074oken`) and whose `event_type` is a typo, plus
/// a second copy of the same secret under a member the event does not have. The
/// daemon decodes it, takes the escaped member as the capability field, and logs
/// its "unrecognized event_type" diagnostic — the log must name the bad
/// `event_type` and must contain neither copy of the secret.
#[spec("hooks/provenance/004")]
#[test]
fn provenance_004_no_spelling_of_the_token_field_reaches_the_daemon_log() {
    // A value that could not occur by chance, so finding it in the log is proof
    // and not coincidence.
    const SECRET: &str = "provenance004-secret-capability-value";
    const BAD_EVENT_TYPE: &str = "provenance004-not-an-event-type";

    let logdir = common::race_safe_tempdir();
    let log_path = logdir.path().join("deck.log");
    let daemon = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[(
            "DOT_AGENT_DECK_LOG",
            log_path.to_str().expect("the log path is UTF-8"),
        )],
    );

    // `\u0074` is `t`. `serde_json` decodes this member name to `agent_token`,
    // so the daemon honours it as the capability field — while a textual scan
    // for the literal `"agent_token"` finds nothing at all to redact. `stash`
    // carries the same value under a member the event does not have.
    let line = format!(
        "{{\"session_id\":\"provenance004\",\"agent_type\":\"claude_code\",\
         \"event_type\":\"{BAD_EVENT_TYPE}\",\"timestamp\":\"2026-08-24T12:00:00Z\",\
         \"pane_id\":\"provenance-004-unmanaged-pane\",\
         \"agent_\\u0074oken\":\"{SECRET}\",\"stash\":\"{SECRET}\"}}"
    );
    assert!(
        !line.contains("\"agent_token\""),
        "precondition: the textual redaction has nothing to match on"
    );
    // Anti-vacuity, and it is what makes the log assertion below a proof rather
    // than a hope: the textual redaction the daemon USED to apply on this branch
    // leaks this exact line. So if the daemon were still calling it, the secret
    // would be in the log; the assertion that it is not therefore establishes
    // that the branch redacts structurally, without needing to inspect which
    // function it called.
    assert!(
        dot_agent_deck::hook_ingest::redact_for_log(&line).contains(SECRET),
        "precondition: a textual scan for the literal field name cannot redact \
         this line at all"
    );
    write_raw_hook_line(&daemon.hook_socket, &line);

    common::wait_until(Duration::from_secs(10), || {
        read_log(&log_path).contains(BAD_EVENT_TYPE)
    });
    let log = read_log(&log_path);
    assert!(
        log.contains(BAD_EVENT_TYPE),
        "the daemon must still report the unrecognized event_type — that is the \
         whole reason this branch logs the payload at all. Log was:\n{log}"
    );
    assert!(
        !log.contains(SECRET),
        "no spelling of the capability field, and no unexpected member, may reach \
         the daemon log. Log was:\n{log}"
    );
}
