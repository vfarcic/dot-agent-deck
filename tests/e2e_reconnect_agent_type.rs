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

use common::{DaemonProc, TuiDeck, spawn_daemon_serve, write_hook_line};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use dot_agent_deck::state::SessionStatus;
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
            seed: None,
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

/// Launch the real TUI binary in a PTY pointed at `daemon`'s hook + attach
/// sockets so `ensure_external_daemon_or_die` reuses that already-running
/// daemon instead of lazy-spawning its own. No `DOT_AGENT_DECK_BUILD_ID_OVERRIDE`
/// is set, so the TUI and the same-binary daemon report identical build ids and
/// the version-mismatch prompt never fires — the deck drops straight into the
/// dashboard and hydrates the daemon's live agents.
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

/// Scenario: Start a real daemon and `StartAgent` a bare `sh -c 'sleep 600'`
/// tagged `DOT_AGENT_DECK_PANE_ID=pane-recon` with display name `recon-live-77`
/// and no `agent_type`. With NO TUI attached (the daemon owns the agent), drive
/// the agent to a live `Working` status by writing two synthetic Claude Code
/// hooks straight to the hook socket — a `session_start` then a `tool_start`
/// (`Read src/main.rs`) — both carrying the registry agent id so the daemon's
/// `AppState` session matches the `ListAgents` join. Then launch a FRESH TUI
/// against the same daemon and, WITHOUT writing any further hook, assert the
/// rebuilt dashboard card shows the live `Working` status (and the agent's
/// display name) immediately on reconnect — not the bare `Idle` / `No agent`
/// placeholder the pre-PRD-162 reconnect path rendered until the next event.
/// RED until M2.1/M2.2 thread the snapshot through `HydratedPane` and seed the
/// hydrated session from it: today the rebuilt card seeds `agent_type = None`
/// (spawn-time) → renders `No agent`, never `Working`.
#[spec("session/live/006")]
#[test]
fn live_006_fresh_tui_renders_live_working_status_on_reconnect() {
    let daemon = spawn_daemon_serve(None, "0");

    // A shell agent with no inferable type (`from_command("sh …") == None`),
    // tagged with a known pane id and a distinctive display name.
    let resp = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some("sh -c 'sleep 600'".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), "pane-recon".into())],
            display_name: Some("recon-live-77".into()),
            tab_membership: None,
            agent_type: None,
            seed: None,
        })
        .expect("StartAgent over the attach socket");
    assert!(
        resp.error.is_none(),
        "StartAgent should succeed, got error: {:?}",
        resp.error
    );

    // Capture the daemon-assigned registry id — the `ListAgents` live-snapshot
    // join (PRD #162 M1.2) matches the session on agent_id AND pane_id, so the
    // hook events below must carry this id to populate the snapshot.
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(5));
    assert_eq!(records.len(), 1, "the shell agent should be registered");
    let agent_id = records[0].id.clone();

    // Drive the daemon's live session to `Working` with an active tool — the
    // exact state a user would see before disconnecting. No TUI is attached;
    // the daemon's `apply_event` builds the authoritative `SessionState`.
    //
    // The two hooks MUST be applied in order (`session_start` then
    // `tool_start`), so we write the second only after the daemon has applied
    // the first. The daemon handles each hook connection on its own
    // `tokio::spawn`ed task (see `run_hook_loop`), so two back-to-back writes
    // over separate connections have NO cross-connection ordering guarantee —
    // and `apply_event`'s `SessionStart` arm resets the session to `Idle` /
    // clears the active tool. If `session_start` raced in AFTER `tool_start`
    // under parallel load it would clobber the `Working` state straight back
    // to `Idle`, and the snapshot could never reach `Working` (the exact
    // failure a single back-to-back-then-wait guard hit). A real agent always
    // emits `session_start` at launch and its first tool hook well afterward,
    // so serializing here models real usage and removes the load-induced race.

    // Hook #1 — `session_start`: establishes the session and teaches the
    // event-derived agent type. Wait until the daemon's registry reflects it
    // (a live snapshot exists and carries the learned `ClaudeCode` type; the
    // `SessionStart` arm leaves the status at `Idle`) before sending the tool
    // hook, so the tool hook can only move the status forward.
    let session_start = serde_json::json!({
        "session_id": "recon-sess",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-06-20T12:00:00Z",
        "pane_id": "pane-recon",
        "agent_id": agent_id,
    });
    write_hook_line(&daemon.hook_socket, &session_start.to_string())
        .expect("write SessionStart hook");
    let started = daemon.wait_for_agent_where(
        |r| {
            r.agent_type == Some(AgentType::ClaudeCode)
                && r.live
                    .as_ref()
                    .is_some_and(|s| s.status == SessionStatus::Idle)
        },
        Duration::from_secs(5),
    );
    assert!(
        started.is_some(),
        "daemon must apply SessionStart (live snapshot present, type learned) \
         before the tool hook is sent, got: {:?}",
        daemon.agent_records()
    );

    // Hook #2 — `tool_start`: with the session already established, this can
    // only drive the live snapshot to `Working` (no later `SessionStart`
    // arrives to reset it).
    let tool_start = serde_json::json!({
        "session_id": "recon-sess",
        "agent_type": "claude_code",
        "event_type": "tool_start",
        "tool_name": "Read",
        "tool_detail": "src/main.rs",
        "timestamp": "2026-06-20T12:00:01Z",
        "pane_id": "pane-recon",
        "agent_id": agent_id,
    });
    write_hook_line(&daemon.hook_socket, &tool_start.to_string()).expect("write ToolStart hook");

    // The daemon now holds a `Working` session; a reconnecting TUI's
    // `hydrate_from_daemon` → `ListAgents` must carry it. Gate on the
    // snapshot's `Working` status — not just the learned type — so the tool
    // hook is guaranteed applied to the snapshot before the fresh TUI
    // reconnects.
    let learned = daemon.wait_for_agent_where(
        |r| {
            r.agent_type == Some(AgentType::ClaudeCode)
                && r.live
                    .as_ref()
                    .is_some_and(|s| s.status == SessionStatus::Working)
        },
        Duration::from_secs(5),
    );
    assert!(
        learned.is_some(),
        "daemon must have ingested both hooks (live snapshot == Working) before \
         the fresh TUI reconnects, got: {:?}",
        daemon.agent_records()
    );

    // Reconnect a FRESH TUI. No further hook is written, so the card's status
    // can only come from the reconnect snapshot — not a live event.
    let deck = launch_tui_against(&daemon);

    // The rebuilt card must show the LIVE status (`Working`) alongside the
    // agent's display name, immediately on reconnect. Pre-PRD-162 the card
    // seeds `agent_type = None` → renders `No agent`, so this wait times out:
    // the RED signal.
    deck.wait_until_grid(
        "reconnected card shows live Working status + display name",
        |g| g.contains("recon-live-77") && g.contains("Working"),
    );

    // And it must NOT regress to the placeholder label for that live agent.
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("No agent"),
        "a reconnected live agent must not render the 'No agent' placeholder; \
         grid was:\n{grid}"
    );
}
