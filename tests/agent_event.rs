//! PRD #201 M1.2 / M1.3 — fast-tier contract for the additive
//! `dot-agent-deck agent-event --type <state>` subcommand (test-plan rows
//! 4 & 7).
//!
//! These pin the seam the coder must build so a Pi pane reports status with
//! **no hook installed and no `~/.claude/settings.json` mutation**: a
//! lifecycle state emitted by the (bundled) extension routes an `AgentEvent`
//! into the EXISTING `EventType`/`AgentEvent` stream and drives the target
//! pane's card status — the same wire every other client already consumes.
//!
//! Fast tier by design (no `e2e` gate): the routing/status logic under test
//! is genuinely more reliable asserted deterministically in-process. The
//! full CLI → socket → daemon path (spawn the real binary, send over the
//! daemon socket) belongs to the later real-`pi` e2e milestone (M4). Here we
//! drive the two halves of the seam directly: the production state→EventType
//! mapping (`agent_event_type_from_state`) and `AppState::apply_event` (the
//! stream sink that computes the card status), which is the same in-process
//! seam the fast-tier delegate guard (`delegate_prompt_injection.rs`) uses.
//!
//! RED until M1.2 lands: `dot_agent_deck::event::agent_event_type_from_state`
//! does not exist yet, so this test binary fails to compile. That
//! compile-level RED is the point — it is the missing production seam.

use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::state::{AppState, SessionStatus};

use spec::spec;

mod common;

use common::synthetic_agent::SyntheticAgent;

/// The Pi orchestrator pane the daemon injected `DOT_AGENT_DECK_PANE_ID` /
/// `DOT_AGENT_DECK_AGENT_ID` for. The harness models those env vars as the
/// agent's `pane_id` / `agent_id`.
const PI_PANE: &str = "pi-orchestrator-pane";
const PI_AGENT_ID: &str = "pi-agent-1";

/// Read the card status the dashboard would badge for `session_id`.
fn status_of(state: &AppState, session_id: &str) -> SessionStatus {
    state
        .sessions
        .get(session_id)
        .unwrap_or_else(|| {
            panic!("no session {session_id:?} — the agent-event never created a card")
        })
        .status
        .clone()
}

/// Scenario: A Pi agent (the synthetic harness, `AgentType::Pi`) emits a
/// single `running` lifecycle event the way its extension would call
/// `dot-agent-deck agent-event --type running` from a pane carrying the
/// daemon's `DOT_AGENT_DECK_PANE_ID` / `DOT_AGENT_DECK_AGENT_ID` env vars.
/// Assert the state maps to an `EventType` via the production seam, that the
/// built frame rides the EXISTING raw-`AgentEvent` stream (no `message_type`
/// envelope, carries the pane/agent ids and the Pi identity), and that
/// feeding it through `AppState::apply_event` drives the target pane's card
/// status — with NO hook and NO `settings.json` involved anywhere in the path.
#[spec("status/agent-event/001")]
#[test]
fn agent_event_001_routes_into_the_event_stream_and_drives_status_no_hook() {
    let pi = SyntheticAgent::new(AgentType::Pi, PI_PANE).with_agent_id(PI_AGENT_ID);

    // (1) The additive subcommand's `--type <state>` maps a lifecycle state
    //     to an EventType via the production seam. RED: this function does
    //     not exist yet (M1.2).
    let event_type = dot_agent_deck::event::agent_event_type_from_state("running")
        .expect("agent-event --type running must map to an EventType");

    let event = pi.agent_event(event_type);

    // (2) Identity + addressing come from the injected env vars: the frame
    //     carries the pane id, the agent id, and the Pi agent type.
    assert_eq!(event.pane_id.as_deref(), Some(PI_PANE));
    assert_eq!(event.agent_id.as_deref(), Some(PI_AGENT_ID));
    assert_eq!(event.agent_type, AgentType::Pi);

    // (3) It rides the EXISTING AgentEvent stream — a bare AgentEvent with NO
    //     `message_type` envelope — so the daemon's raw-event fallback ingests
    //     it exactly like a hook event (zero new wire surface). Mirrors the
    //     `agent_event_not_parseable_as_daemon_message` guard in event.rs.
    let json = serde_json::to_string(&event).expect("serialize AgentEvent");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        value.get("message_type").is_none(),
        "agent-event must ride the raw AgentEvent stream, not a DaemonMessage envelope"
    );
    assert!(
        serde_json::from_str::<dot_agent_deck::event::DaemonMessage>(&json).is_err(),
        "agent-event frame must NOT parse as a DaemonMessage (it is an AgentEvent)"
    );

    // (4) Routed through the stream sink, it drives the target pane's status.
    //     No hook, no settings.json — purely the AgentEvent path.
    let mut state = AppState::default();
    state.register_pane(PI_PANE.to_string());
    state.apply_event(event);

    assert_eq!(
        status_of(&state, pi.session_id()),
        SessionStatus::Thinking,
        "a `running` agent-event must drive the Pi pane's card to a busy (Thinking) status"
    );
}

/// Scenario: The Pi synthetic agent emits the full lifecycle sequence
/// `running` → `waiting` → `finished` via `agent-event`, and the card badge
/// follows each transition. Each state is resolved to an `EventType` through
/// the production seam and routed through `AppState::apply_event`; the
/// derived `SessionStatus` (the badge source) must move
/// Thinking → WaitingForInput → Idle in lock-step — again with no hook and no
/// `settings.json` mutation.
#[spec("status/agent-event/002")]
#[test]
fn agent_event_002_status_badge_follows_running_waiting_finished_sequence() {
    let pi = SyntheticAgent::new(AgentType::Pi, PI_PANE).with_agent_id(PI_AGENT_ID);

    let mut state = AppState::default();
    state.register_pane(PI_PANE.to_string());

    // The lifecycle sequence the extension reports, and the card status each
    // must produce. RED until `agent_event_type_from_state` exists (M1.2).
    let sequence = [
        ("running", EventType::Thinking, SessionStatus::Thinking),
        (
            "waiting",
            EventType::WaitingForInput,
            SessionStatus::WaitingForInput,
        ),
        ("finished", EventType::Idle, SessionStatus::Idle),
    ];

    for (state_name, want_event, want_status) in sequence {
        let event_type = dot_agent_deck::event::agent_event_type_from_state(state_name)
            .unwrap_or_else(|| panic!("agent-event --type {state_name} must map to an EventType"));
        assert_eq!(
            event_type, want_event,
            "`{state_name}` must map to {want_event:?} so apply_event renders {want_status:?}"
        );

        state.apply_event(pi.agent_event(event_type));

        assert_eq!(
            status_of(&state, pi.session_id()),
            want_status,
            "after agent-event --type {state_name}, the Pi card badge must read {want_status:?}"
        );
    }
}

/// Scenario: A synthetic Codex wrapper emits the same observable lifecycle its
/// stdout detector produces: session start, active work, an error, recovery,
/// and turn completion. Applying those typed events must keep one Codex card
/// and move its badge Thinking → Error → Thinking → Idle.
#[spec("status/agent-event/004")]
#[test]
fn agent_event_004_codex_wrapper_lifecycle_drives_one_card() {
    let codex = SyntheticAgent::new(AgentType::Codex, "codex-wrapper-pane")
        .with_agent_id("codex-wrapper-agent");
    let mut state = AppState::default();
    state.register_pane(codex.pane_id.clone());

    for (event_type, expected) in [
        (EventType::Thinking, SessionStatus::Thinking),
        (EventType::Error, SessionStatus::Error),
        (EventType::Thinking, SessionStatus::Thinking),
        (EventType::Idle, SessionStatus::Idle),
    ] {
        state.apply_event(codex.agent_event(event_type));
        assert_eq!(status_of(&state, codex.session_id()), expected);
        assert_eq!(
            state.sessions[codex.session_id()].agent_type,
            AgentType::Codex
        );
        assert_eq!(
            state.sessions.len(),
            1,
            "one wrapped run must update one card"
        );
    }
}
