#![cfg(feature = "e2e")]

//! L2 synthetic coverage for hook-event provenance. These tests drive the real
//! daemon and TUI binaries, but write deterministic JSON directly to the hook
//! socket instead of launching an LLM.

mod common;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::{DaemonProc, TuiDeck, spawn_daemon_serve};
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
