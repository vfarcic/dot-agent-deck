//! Fast wire-contract tests for PRD #20 live-target and send-result semantics.

use dot_agent_deck::daemon_protocol::AttachResponse;
use std::collections::HashMap;

use chrono::{Duration, Utc};
use dot_agent_deck::event::{AgentEvent, AgentType, EventType, Writable};
use dot_agent_deck::state::AppState;
use serde_json::{Value, json};
use spec::spec;

fn event_payload() -> Value {
    json!({
        "session_id": "wrapped-codex-01",
        "agent_type": "codex",
        "event_type": "session_start",
        "timestamp": "2026-07-15T12:00:00Z"
    })
}

/// Scenario: Round-trip every live-target kind and writable value through the
/// public `AgentEvent` JSON payload. Then deserialize a legacy event with no
/// `live_target` and confirm serializing it again still omits the optional field.
#[spec("protocol/live-target/001")]
#[test]
fn live_target_001_descriptor_round_trip_and_legacy_omission() {
    for kind in ["process", "pty", "tmux", "sdk", "none"] {
        for writable in ["live", "history-only", "none"] {
            let mut payload = event_payload();
            payload["live_target"] = json!({
                "kind": kind,
                "writable": writable,
            });

            let decoded: AgentEvent = serde_json::from_value(payload.clone())
                .unwrap_or_else(|e| panic!("deserialize {kind}/{writable} live target: {e}"));
            let encoded = serde_json::to_value(decoded).expect("serialize AgentEvent");
            assert_eq!(
                encoded.get("live_target"),
                payload.get("live_target"),
                "AgentEvent must preserve live_target kind={kind}, writable={writable}"
            );
        }
    }

    let decoded: AgentEvent =
        serde_json::from_value(event_payload()).expect("deserialize legacy AgentEvent");
    let encoded = serde_json::to_value(decoded).expect("serialize legacy AgentEvent");
    assert!(
        encoded.get("live_target").is_none(),
        "a legacy AgentEvent without live_target must deserialize as None and omit the field when reserialized: {encoded}"
    );
}

/// Scenario: Round-trip each honest input-delivery result through the daemon's
/// public response payload. Every result must retain its distinct wire value so
/// callers can distinguish accepted input from stale, wrong, or unwritable targets.
#[spec("protocol/send-result/001")]
#[test]
fn send_result_001_all_variants_round_trip() {
    for result in [
        "applied",
        "queued",
        "stale",
        "wrong-session",
        "history-only",
        "no-live-target",
    ] {
        let payload = json!({
            "ok": true,
            "send_result": result,
        });
        let decoded: AttachResponse = serde_json::from_value(payload.clone())
            .unwrap_or_else(|e| panic!("deserialize send result {result}: {e}"));
        let encoded = serde_json::to_value(decoded).expect("serialize AttachResponse");
        assert_eq!(
            encoded.get("send_result"),
            payload.get("send_result"),
            "AttachResponse must preserve SendResult::{result}"
        );
    }
}

/// Scenario: Declare a Codex session history-only, then apply more than the
/// bounded activity journal's capacity of events that omit `live_target`. The
/// session must remain history-only because writability is durable state, not a
/// property that disappears when the declaring event ages out of recent history.
#[spec("protocol/live-target/002")]
#[test]
fn live_target_002_writability_survives_recent_event_eviction() {
    let mut state = AppState::default();
    state.register_pane("pane-durable".to_string());
    let started = Utc::now();
    state.apply_event(AgentEvent {
        session_id: "durable-codex".to_string(),
        agent_type: AgentType::Codex,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: started,
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some("pane-durable".to_string()),
        agent_id: Some("agent-durable".to_string()),
        agent_version: None,
        schema_version: None,
        live_target: Some(dot_agent_deck::event::LiveTarget {
            kind: dot_agent_deck::event::TargetKind::Process,
            writable: Writable::HistoryOnly,
        }),
    });

    for offset in 1..=51 {
        state.apply_event(AgentEvent {
            session_id: "durable-codex".to_string(),
            agent_type: AgentType::Codex,
            event_type: EventType::Thinking,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: started + Duration::seconds(offset),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: Some("pane-durable".to_string()),
            agent_id: Some("agent-durable".to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        });
    }

    let session = state
        .sessions
        .get("durable-codex")
        .expect("the durable Codex session remains present");
    assert_eq!(
        session.writable(),
        Writable::HistoryOnly,
        "evicting the declaring event from the 50-entry recent-events journal must not promote a history-only session to Live"
    );
}
