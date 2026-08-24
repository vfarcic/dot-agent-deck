use chrono::{Duration, Utc};
use dot_agent_deck::event::{AgentEvent, AgentType, DISPLAY_NAME_METADATA_KEY, EventType};
use dot_agent_deck::state::AppState;

use spec::spec;

const PANE_ID: &str = "scheduler-handoff-pane";
const TASK_NAME: &str = "morning-digest";

fn event(
    session_id: &str,
    agent_type: AgentType,
    event_type: EventType,
    agent_id: Option<&str>,
    timestamp: chrono::DateTime<Utc>,
) -> AgentEvent {
    AgentEvent {
        session_id: session_id.to_string(),
        agent_type,
        event_type,
        tool_name: None,
        tool_detail: None,
        cwd: Some("/tmp/runbox".to_string()),
        timestamp,
        user_prompt: None,
        metadata: Default::default(),
        pane_id: Some(PANE_ID.to_string()),
        agent_id: agent_id.map(str::to_string),
        agent_version: None,
        schema_version: None,
        live_target: None,
        agent_token: None,
    }
}

/// Scenario: A scheduler first surfaces a friendly `No agent` placeholder, then the real agent reports `SessionStart` on the same pane without display-name metadata. The handoff must leave one live card carrying both the real agent id and the scheduler's friendly task name.
#[spec("status/supersede/001")]
#[test]
fn status_supersede_001_real_session_replaces_placeholder_and_keeps_friendly_name() {
    let placeholder_timestamp = Utc::now();
    let mut placeholder = event(
        "scheduler-placeholder",
        AgentType::None,
        EventType::SessionStart,
        None,
        placeholder_timestamp,
    );
    placeholder
        .metadata
        .insert(DISPLAY_NAME_METADATA_KEY.to_string(), TASK_NAME.to_string());

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(placeholder);

    let placeholder_card = state
        .sessions
        .get("scheduler-placeholder")
        .expect("precondition: scheduler placeholder is visible");
    assert_eq!(placeholder_card.agent_id, None);
    assert_eq!(placeholder_card.display_name.as_deref(), Some(TASK_NAME));

    // A real hook may have been emitted before the scheduler's synthetic
    // surface frame was applied, so its event timestamp can be older even
    // though its Some(agent_id) identity authoritatively supersedes the
    // placeholder's None.
    let incoming_timestamp = placeholder_timestamp - Duration::seconds(1);
    let incoming = event(
        "real-agent-session",
        AgentType::ClaudeCode,
        EventType::SessionStart,
        Some("real-agent-id"),
        incoming_timestamp,
    );

    assert!(
        state.sessions["scheduler-placeholder"].pane_id == incoming.pane_id,
        "precondition: the replacement addresses the placeholder's pane"
    );
    assert_ne!(
        state.sessions["scheduler-placeholder"].agent_id, incoming.agent_id,
        "precondition: None placeholder identity differs from Some(real agent)"
    );
    assert!(
        incoming.timestamp < state.sessions["scheduler-placeholder"].last_activity,
        "precondition: only a timestamp guard can reject this authoritative handoff"
    );

    state.apply_event(incoming);

    assert_eq!(
        state.sessions.len(),
        1,
        "the real SessionStart must supersede the No-agent placeholder instead of stacking a second card on its pane"
    );
    let live = state
        .sessions
        .get("real-agent-session")
        .expect("the one surviving card must use the real session identity");
    assert_eq!(live.agent_id.as_deref(), Some("real-agent-id"));
    assert_eq!(
        live.display_name.as_deref(),
        Some(TASK_NAME),
        "the replacement must inherit the scheduler's friendly display name"
    );
}

/// Scenario: A close confirmation is armed against a session id, then a different agent generation reports `SessionStart` on the same pane. The armed identity must disappear from state so confirmation resolves it as vanished and cannot close the replacement.
#[spec("status/supersede/002")]
#[test]
fn status_supersede_002_replaced_armed_session_identity_resolves_as_vanished() {
    let original_timestamp = Utc::now();
    let original = event(
        "armed-session",
        AgentType::ClaudeCode,
        EventType::SessionStart,
        Some("outgoing-agent-id"),
        original_timestamp,
    );

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(original);

    // CloseTarget::Session stores this stable session identity at arm time.
    let armed_session_id = "armed-session".to_string();
    assert!(state.sessions.contains_key(&armed_session_id));

    // Delivery order, not the producer timestamp, determines that this real
    // SessionStart is the replacement. The older stamp exercises the exact
    // case where the naive monotonicity guard retains the armed target.
    let replacement = event(
        "replacement-session",
        AgentType::ClaudeCode,
        EventType::SessionStart,
        Some("incoming-agent-id"),
        original_timestamp - Duration::seconds(1),
    );
    state.apply_event(replacement);

    assert!(
        !state.sessions.contains_key(&armed_session_id),
        "the session identity captured by close confirmation must vanish when another generation takes over the pane"
    );
    assert!(
        state.sessions.contains_key("replacement-session"),
        "the incoming generation must remain visible but must not inherit the armed session id"
    );
    assert_eq!(
        state.sessions.len(),
        1,
        "session replacement must not leave the armed generation beside the live one"
    );
}

/// Scenario: A live agent B owns a pane when a delayed `SessionEnd` arrives from outgoing agent A with a newer timestamp. Because a terminal event announces a generation ending rather than taking over, B's live card must remain visible on the pane.
#[spec("status/supersede/003")]
#[test]
fn status_supersede_003_outgoing_session_end_keeps_the_live_card() {
    let live_timestamp = Utc::now();
    let live = event(
        "live-agent-session",
        AgentType::ClaudeCode,
        EventType::Thinking,
        Some("live-agent-id"),
        live_timestamp,
    );

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(live);

    assert!(
        state.sessions.contains_key("live-agent-session"),
        "precondition: agent B has a live card on the pane"
    );

    let outgoing_end = event(
        "outgoing-agent-session",
        AgentType::ClaudeCode,
        EventType::SessionEnd,
        Some("outgoing-agent-id"),
        live_timestamp + Duration::seconds(1),
    );
    state.apply_event(outgoing_end);

    assert!(
        state.sessions.contains_key("live-agent-session"),
        "a SessionEnd from outgoing agent A removed live agent B's card, leaving zero cards on a live pane — the inverse of the two-cards bug"
    );
}

/// Scenario: A live agent B card established at T=30 receives its own delayed T=10 event because hook sends use separate accepted connections and spawned tasks, so production delivery can reorder. An outgoing agent A straggler at T=20 must not retire B after that same-session delay.
#[spec("status/supersede/004")]
#[test]
fn status_supersede_004_reordered_same_session_event_cannot_weaken_the_guard() {
    let t30 = Utc::now();
    let live = event(
        "live-agent-session",
        AgentType::Pi,
        EventType::Thinking,
        Some("live-agent-id"),
        t30,
    );

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(live);

    let delayed_same_session = event(
        "live-agent-session",
        AgentType::Pi,
        EventType::Idle,
        Some("live-agent-id"),
        t30 - Duration::seconds(20),
    );
    state.apply_event(delayed_same_session);

    let outgoing_straggler = event(
        "outgoing-agent-session",
        AgentType::Pi,
        EventType::Idle,
        Some("outgoing-agent-id"),
        t30 - Duration::seconds(10),
    );
    state.apply_event(outgoing_straggler);

    assert!(
        state.sessions.contains_key("live-agent-session"),
        "a reordered same-session event moved last_activity backward and let an outgoing-agent straggler retire the LIVE card"
    );
}

/// Scenario: Pi reports two successive respawn generations through its pane-derived stable session id. The second generation must replace the first card's agent identity without creating a duplicate card.
#[spec("status/supersede/005")]
#[test]
fn status_supersede_005_repeated_pi_respawn_refreshes_the_stable_card_identity() {
    let stable_session_id = format!("{PANE_ID}-session");
    let first_timestamp = Utc::now();

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(event(
        &stable_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-2"),
        first_timestamp,
    ));

    assert_eq!(
        state.sessions[&stable_session_id].agent_id.as_deref(),
        Some("pi-agent-2"),
        "precondition: the first respawn generation owns the stable Pi card"
    );

    state.apply_event(event(
        &stable_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-3"),
        first_timestamp + Duration::seconds(1),
    ));

    assert_eq!(
        state.sessions.len(),
        1,
        "repeated Pi respawn must keep exactly one card on the pane"
    );
    assert_eq!(
        state.sessions[&stable_session_id].agent_id.as_deref(),
        Some("pi-agent-3"),
        "the stable Pi card kept the stale agent identity from the previous respawn generation"
    );
}

/// Scenario: A scheduler placeholder with a friendly name lands before an older-stamped Pi frame, so the first Pi frame creates a sibling card without retiring it. A later Pi status retires the placeholder and must transfer its friendly name onto the already-existing Pi card.
#[spec("status/supersede/007")]
#[test]
fn status_supersede_007_existing_pi_session_inherits_the_retired_placeholder_name() {
    let stable_session_id = format!("{PANE_ID}-session");
    let placeholder_timestamp = Utc::now();
    let mut placeholder = event(
        "scheduler-placeholder",
        AgentType::None,
        EventType::SessionStart,
        None,
        placeholder_timestamp,
    );
    placeholder
        .metadata
        .insert(DISPLAY_NAME_METADATA_KEY.to_string(), TASK_NAME.to_string());

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(placeholder);

    state.apply_event(event(
        &stable_session_id,
        AgentType::Pi,
        EventType::Idle,
        Some("pi-agent-2"),
        placeholder_timestamp - Duration::seconds(1),
    ));
    assert_eq!(
        state.sessions.len(),
        2,
        "precondition: the older first Pi frame cannot yet retire the newer scheduler placeholder"
    );

    state.apply_event(event(
        &stable_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-2"),
        placeholder_timestamp + Duration::seconds(1),
    ));

    assert_eq!(
        state.sessions.len(),
        1,
        "the newer Pi frame must retire the scheduler placeholder"
    );
    assert_eq!(
        state.sessions[&stable_session_id].display_name.as_deref(),
        Some(TASK_NAME),
        "the existing Pi card dropped the friendly name inherited from the retired scheduler placeholder"
    );
}

/// Scenario: A dispatched orchestration's role card is named `morning-digest`, then a
/// `clear = true` delegate SIGTERMs that agent — its `SessionEnd` lands and the
/// replacement reports a brand-new `SessionStart` with a different agent id. The one
/// card left on the pane must still carry the friendly name, not the replacement's
/// session id.
#[spec("status/supersede/008")]
#[test]
fn status_supersede_008_a_respawn_across_session_end_keeps_the_friendly_name() {
    let placeholder_timestamp = Utc::now();
    let mut placeholder = event(
        "spawn-placeholder",
        AgentType::None,
        EventType::SessionStart,
        None,
        placeholder_timestamp,
    );
    placeholder
        .metadata
        .insert(DISPLAY_NAME_METADATA_KEY.to_string(), TASK_NAME.to_string());

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(placeholder);

    // The first real agent takes the pane and inherits the friendly name.
    state.apply_event(event(
        "first-generation",
        AgentType::ClaudeCode,
        EventType::SessionStart,
        Some("agent-1"),
        placeholder_timestamp + Duration::seconds(1),
    ));
    assert_eq!(
        state.sessions["first-generation"].display_name.as_deref(),
        Some(TASK_NAME),
        "precondition: the first real generation inherits the spawn-time friendly name"
    );

    // The `clear = true` respawn: the outgoing agent's SessionEnd, then the
    // replacement's own SessionStart under a fresh registry id.
    state.apply_event(event(
        "first-generation",
        AgentType::ClaudeCode,
        EventType::SessionEnd,
        Some("agent-1"),
        placeholder_timestamp + Duration::seconds(2),
    ));
    state.apply_event(event(
        "second-generation",
        AgentType::ClaudeCode,
        EventType::SessionStart,
        Some("agent-2"),
        placeholder_timestamp + Duration::seconds(3),
    ));

    assert_eq!(
        state.sessions.len(),
        1,
        "the replacement must leave exactly one card on the pane"
    );
    assert_eq!(
        state.sessions["second-generation"].display_name.as_deref(),
        Some(TASK_NAME),
        "the replacement card dropped the pane's friendly name — a `clear = true` \
         delegate's worker then renders as `ClaudeCode · <session-uuid>` instead of \
         its role (issue #663)"
    );
}
