#![cfg(feature = "e2e")]

//! L2 end-to-end coverage for the "No agent on reconnect" fix (PRD-less
//! bugfix). Drives the real `dot-agent-deck` daemon binary over its hook and
//! attach sockets — no TUI render, no LLM tokens — to prove the daemon
//! persists the agent type it learns from a hook event into the registry that
//! `list_agents` serves. That `ListAgents` call is exactly what
//! `EmbeddedPaneController::hydrate_from_daemon` issues on a fresh
//! `dot-agent-deck connect`, so this pins the end-to-end path the in-process
//! `daemon::run_hook_loop` unit/integration tests cover only in isolation.
//!
//! Gated behind the `e2e` feature so CI (which runs only `cargo test-fast`)
//! never compiles it (PRD #77 Decision 6).

mod common;

use common::{spawn_daemon_serve, write_hook_line};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;
use std::time::Duration;

/// Scenario: Start a real daemon, then `StartAgent` a bare `/bin/sh` tagged
/// with `DOT_AGENT_DECK_PANE_ID=pane-recon` and no `agent_type` (the common
/// interactive case where the spawn command isn't a recognized agent, so the
/// registry records `None` — "No agent"). Confirm `ListAgents` reports that
/// agent with `agent_type == None`. Then write a synthetic Claude Code
/// `SessionStart` for `pane-recon` straight to the hook socket. A subsequent
/// `ListAgents` — the same call a reconnecting TUI's `hydrate_from_daemon`
/// makes — must now report `agent_type == ClaudeCode`, proving the daemon
/// learned and persisted the real type rather than waiting for a live event.
#[spec("hooks/delivery/007")]
#[test]
fn delivery_007_hook_teaches_daemon_agent_type_for_reconnect() {
    let daemon = spawn_daemon_serve(None, "0");

    // Start a shell agent whose command yields no inferable `AgentType`
    // (`from_command("/bin/sh") == None`), tagged with a known pane id so the
    // later hook event can be matched to it.
    let resp = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some("/bin/sh".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), "pane-recon".into())],
            display_name: None,
            tab_membership: None,
            agent_type: None,
        })
        .expect("StartAgent over the attach socket");
    assert!(
        resp.error.is_none(),
        "StartAgent should succeed, got error: {:?}",
        resp.error
    );

    // The agent registers, but with no known type yet — this is the
    // "No agent" state a reconnect would render before the fix.
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(5));
    assert_eq!(records.len(), 1, "the shell agent should be registered");
    assert_eq!(
        records[0].agent_type, None,
        "a shell-launched agent has no spawn-time type"
    );

    // A hook reveals the real agent type for that pane.
    let event = serde_json::json!({
        "session_id": "recon-sess",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-06-20T12:00:00Z",
        "pane_id": "pane-recon",
    });
    write_hook_line(&daemon.hook_socket, &event.to_string())
        .expect("write SessionStart hook to the per-test socket");

    // A fresh ListAgents (what hydrate_from_daemon issues on reconnect) now
    // reports the learned type — no live TUI event required.
    let learned = daemon.wait_for_agent_where(
        |r| r.agent_type == Some(AgentType::ClaudeCode),
        Duration::from_secs(5),
    );
    assert!(
        learned.is_some(),
        "list_agents must report the hook-learned agent type after reconnect, \
         got: {:?}",
        daemon.agent_records()
    );
}
