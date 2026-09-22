use chrono::{Duration, Utc};
use dot_agent_deck::event::{
    AgentEvent, AgentType, DISPLAY_NAME_METADATA_KEY, EventType, LiveTarget, TargetKind, Writable,
};
use dot_agent_deck::state::{AppState, SessionSnapshot, SessionStatus};

use spec::spec;

const PANE_ID: &str = "scheduler-handoff-pane";
/// A SECOND managed pane, for the one test that has to prove a frame cannot
/// reach across panes (`status/supersede/011`).
const OTHER_PANE_ID: &str = "scheduler-handoff-pane-b";
const TASK_NAME: &str = "morning-digest";

fn event(
    session_id: &str,
    agent_type: AgentType,
    event_type: EventType,
    agent_id: Option<&str>,
    timestamp: chrono::DateTime<Utc>,
) -> AgentEvent {
    event_on_pane(
        PANE_ID, session_id, agent_type, event_type, agent_id, timestamp,
    )
}

fn event_on_pane(
    pane_id: &str,
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
        pane_id: Some(pane_id.to_string()),
        agent_id: agent_id.map(str::to_string),
        agent_version: None,
        schema_version: None,
        live_target: None,
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

/// Scenario: A Pi pane reports under its stable `{pane_id}-session` id, then respawns
/// under a new registry agent id a minute later with no `SessionEnd` in between — the
/// `clear = true` shape, where the old child is SIGKILLed and never gets to say goodbye.
/// The one card left on the pane must still report the instant the FIRST generation
/// started, not the instant the replacement's first frame happened to arrive.
#[spec("status/supersede/009")]
#[test]
fn status_supersede_009_started_at_survives_a_same_key_respawn() {
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
        state.sessions[&stable_session_id].started_at, first_timestamp,
        "precondition: the first generation's card starts when its first frame arrived"
    );

    state.apply_event(event(
        &stable_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-3"),
        first_timestamp + Duration::seconds(60),
    ));

    assert_eq!(
        state.sessions[&stable_session_id].agent_id.as_deref(),
        Some("pi-agent-3"),
        "precondition: the respawn took the card, so this is the rebuild path under test"
    );
    assert_eq!(
        state.sessions[&stable_session_id].started_at, first_timestamp,
        "the pane's start time was re-derived from the respawn instead of carried across"
    );
}

/// Scenario: A pane's spawn-time card is replaced by a respawn reporting under a
/// DIFFERENT session id and a different registry agent id, again with no `SessionEnd`
/// in between. The surviving card must carry the pane's original start time, exactly as
/// the same-key respawn does — the two respawn shapes must not disagree about when the
/// pane started being used.
#[spec("status/supersede/010")]
#[test]
fn status_supersede_010_started_at_survives_a_cross_key_respawn() {
    let first_timestamp = Utc::now();

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.apply_event(event(
        "spawn-placeholder",
        AgentType::ClaudeCode,
        EventType::SessionStart,
        Some("agent-a"),
        first_timestamp,
    ));

    assert_eq!(
        state.sessions["spawn-placeholder"].started_at, first_timestamp,
        "precondition: the outgoing generation's card starts at its own first frame"
    );

    state.apply_event(event(
        "replacement-session",
        AgentType::ClaudeCode,
        EventType::SessionStart,
        Some("agent-b"),
        first_timestamp + Duration::seconds(60),
    ));

    assert_eq!(
        state.sessions.len(),
        1,
        "precondition: the replacement retired the outgoing card, so one card remains"
    );
    assert_eq!(
        state.sessions["replacement-session"].started_at, first_timestamp,
        "the pane's start time was re-derived from the replacement instead of carried across"
    );
}

/// Scenario: A Pi card on one pane accumulates a tool tally, then a frame arrives naming
/// the SAME producer session id but a different pane and a different registry agent id.
/// The card's identity and history belong to the first pane's agent, so the cross-pane
/// frame must not be allowed to take them over.
#[spec("status/supersede/011")]
#[test]
fn status_supersede_011_a_cross_pane_frame_cannot_refresh_the_card_identity() {
    let shared_session_id = format!("{PANE_ID}-session");
    let first_timestamp = Utc::now();

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.register_pane(OTHER_PANE_ID.to_string());

    state.apply_event(event_on_pane(
        PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-2"),
        first_timestamp,
    ));
    state.apply_event(event_on_pane(
        PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::ToolEnd,
        Some("pi-agent-2"),
        first_timestamp + Duration::seconds(1),
    ));

    assert_eq!(
        state.sessions[&shared_session_id].tool_count, 1,
        "precondition: the first pane's agent accumulated history on this card"
    );

    state.apply_event(event_on_pane(
        OTHER_PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-9"),
        first_timestamp + Duration::seconds(2),
    ));

    assert_eq!(
        state.sessions[&shared_session_id].agent_id.as_deref(),
        Some("pi-agent-2"),
        "a frame from another pane took over the card identity on a session-key match alone"
    );
    assert_eq!(
        state.sessions[&shared_session_id].tool_count, 1,
        "a frame from another pane rebuilt the card and discarded its accumulated history"
    );
    assert_eq!(
        state.sessions[&shared_session_id].started_at, first_timestamp,
        "a frame from another pane rebuilt the card and reset its start time"
    );
}

/// Scenario: A Pi card on pane A accumulates a tool tally under the producer's
/// `{pane_id}-session` key, then a DIFFERENT pane's agent reports under that same key.
/// The colliding frame must land on a card of its own pane rather than dragging pane A's
/// card across, leaving each pane with exactly one card carrying its own agent.
#[spec("status/supersede/012")]
#[test]
fn status_supersede_012_a_colliding_frame_gets_its_own_panes_card() {
    let shared_session_id = format!("{PANE_ID}-session");
    let first_timestamp = Utc::now();

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.register_pane(OTHER_PANE_ID.to_string());

    state.apply_event(event_on_pane(
        PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-2"),
        first_timestamp,
    ));
    state.apply_event(event_on_pane(
        PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::ToolEnd,
        Some("pi-agent-2"),
        first_timestamp + Duration::seconds(1),
    ));

    assert_eq!(
        state.sessions[&shared_session_id].tool_count, 1,
        "precondition: the first pane's agent accumulated history on this card"
    );

    state.apply_event(event_on_pane(
        OTHER_PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-9"),
        first_timestamp + Duration::seconds(2),
    ));

    let card_on = |pane: &str| {
        let mut found = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some(pane));
        let one = found.next();
        assert!(
            found.next().is_none(),
            "pane {pane} carries more than one card"
        );
        one
    };

    let pane_a = card_on(PANE_ID).expect("pane A lost its card to a frame from another pane");
    assert_eq!(
        pane_a.agent_id.as_deref(),
        Some("pi-agent-2"),
        "pane A's card no longer belongs to pane A's agent"
    );
    assert_eq!(
        pane_a.tool_count, 1,
        "pane A's card lost the history it had accumulated"
    );

    let pane_b = card_on(OTHER_PANE_ID)
        .expect("the colliding frame produced no card for the pane it actually named");
    assert_eq!(
        pane_b.agent_id.as_deref(),
        Some("pi-agent-9"),
        "the colliding frame's own agent did not get the card for its pane"
    );
}

/// Scenario: Pane A and pane B each hold a live card, and pane B's agent then ends its
/// conversation under a session key that pane A's card also happens to carry. The
/// terminal frame must end pane B's own card and leave pane A's card, agent and tally
/// exactly where they were.
#[spec("status/supersede/013")]
#[test]
fn status_supersede_013_a_colliding_terminal_frame_ends_only_its_own_pane() {
    let shared_session_id = format!("{PANE_ID}-session");
    let first_timestamp = Utc::now();

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.register_pane(OTHER_PANE_ID.to_string());

    state.apply_event(event_on_pane(
        PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-2"),
        first_timestamp,
    ));
    state.apply_event(event_on_pane(
        PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::ToolEnd,
        Some("pi-agent-2"),
        first_timestamp + Duration::seconds(1),
    ));
    state.apply_event(event_on_pane(
        OTHER_PANE_ID,
        "other-pane-own-session",
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-9"),
        first_timestamp + Duration::seconds(2),
    ));

    assert_eq!(
        state.sessions.len(),
        2,
        "precondition: each pane holds exactly one card of its own"
    );

    state.apply_event(event_on_pane(
        OTHER_PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::SessionEnd,
        Some("pi-agent-9"),
        first_timestamp + Duration::seconds(3),
    ));

    let pane_a = state
        .sessions
        .values()
        .find(|s| s.pane_id.as_deref() == Some(PANE_ID))
        .expect("another pane's SessionEnd removed pane A's live card");
    assert_eq!(
        pane_a.agent_id.as_deref(),
        Some("pi-agent-2"),
        "pane A's card no longer belongs to pane A's agent"
    );
    assert_eq!(
        pane_a.tool_count, 1,
        "pane A's card lost the history it had accumulated"
    );
    assert!(
        !state.sessions.contains_key("other-pane-own-session"),
        "the terminal frame did not end the card on the pane it actually named"
    );
}

/// Scenario: A pane-less event creates a card, the pane it turns out to belong to is
/// registered, and a later frame from the same agent names that pane. The card must
/// learn its pane and keep its accumulated history rather than being split in two.
#[spec("status/supersede/014")]
#[test]
fn status_supersede_014_a_pane_less_card_still_learns_its_pane() {
    let roaming_session_id = "roaming-session";
    let first_timestamp = Utc::now();

    // No managed panes yet, which is what admits a pane-less event at all.
    let mut state = AppState::default();
    let mut paneless = event_on_pane(
        PANE_ID,
        roaming_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-2"),
        first_timestamp,
    );
    paneless.pane_id = None;
    state.apply_event(paneless);

    assert_eq!(
        state.sessions[roaming_session_id].pane_id, None,
        "precondition: the card was created without a pane"
    );

    state.register_pane(PANE_ID.to_string());
    state.apply_event(event_on_pane(
        PANE_ID,
        roaming_session_id,
        AgentType::Pi,
        EventType::ToolEnd,
        Some("pi-agent-2"),
        first_timestamp + Duration::seconds(1),
    ));

    assert_eq!(
        state.sessions.len(),
        1,
        "the pane-naming frame minted a second card instead of binding the existing one"
    );
    assert_eq!(
        state.sessions[roaming_session_id].pane_id.as_deref(),
        Some(PANE_ID),
        "the card never learned the pane its own agent reported from"
    );
    assert_eq!(
        state.sessions[roaming_session_id].tool_count, 1,
        "the card lost its history on the way to learning its pane"
    );
}

/// Scenario: Pane A holds a card under key `K` and a third pane already holds one under
/// the very key that re-keying `K` for pane B produces. A frame naming pane B under `K`
/// must still end up on a card of its own, leaving both other panes' cards untouched.
#[spec("status/supersede/015")]
#[test]
fn status_supersede_015_a_stacked_key_collision_still_lands_on_its_own_pane() {
    const THIRD_PANE_ID: &str = "scheduler-handoff-pane-c";
    let shared_session_id = format!("{PANE_ID}-session");
    // Exactly the spelling the re-key derives for `OTHER_PANE_ID`. Nothing stops
    // a producer emitting it: session ids arrive verbatim on producer payloads.
    let derived_session_id = format!("{OTHER_PANE_ID}::{shared_session_id}");
    let first_timestamp = Utc::now();

    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    state.register_pane(OTHER_PANE_ID.to_string());
    state.register_pane(THIRD_PANE_ID.to_string());

    state.apply_event(event_on_pane(
        PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::ToolEnd,
        Some("pi-agent-2"),
        first_timestamp,
    ));
    state.apply_event(event_on_pane(
        THIRD_PANE_ID,
        &derived_session_id,
        AgentType::Pi,
        EventType::ToolEnd,
        Some("pi-agent-3"),
        first_timestamp + Duration::seconds(1),
    ));

    assert_eq!(
        state.sessions.len(),
        2,
        "precondition: pane A and the third pane each hold a card of their own"
    );

    state.apply_event(event_on_pane(
        OTHER_PANE_ID,
        &shared_session_id,
        AgentType::Pi,
        EventType::Thinking,
        Some("pi-agent-9"),
        first_timestamp + Duration::seconds(2),
    ));

    let card_on = |pane: &str| {
        let mut found = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref() == Some(pane));
        let one = found.next();
        assert!(
            found.next().is_none(),
            "pane {pane} carries more than one card"
        );
        one
    };

    for (pane, agent) in [(PANE_ID, "pi-agent-2"), (THIRD_PANE_ID, "pi-agent-3")] {
        let card = card_on(pane).unwrap_or_else(|| panic!("pane {pane} lost its card"));
        assert_eq!(
            card.agent_id.as_deref(),
            Some(agent),
            "pane {pane}'s card no longer belongs to its own agent"
        );
        assert_eq!(card.tool_count, 1, "pane {pane}'s card lost its history");
    }

    let pane_b = card_on(OTHER_PANE_ID)
        .expect("the colliding frame produced no card for the pane it actually named");
    assert_eq!(
        pane_b.agent_id.as_deref(),
        Some("pi-agent-9"),
        "the colliding frame's own agent did not get the card for its pane"
    );
}

/// The generation a reconnecting TUI seeds a card for in `status/supersede/016`
/// and `017`.
const RECONNECT_AGENT_ID: &str = "reconnect-agent";

/// An instant `offset` from now, truncated to a whole millisecond.
/// `SessionSnapshot` carries `last_activity` in epoch milliseconds, so the
/// reconnect tests stamp their frames on whole milliseconds and a comparison
/// between the daemon's value and the seeded one is exact rather than off by
/// the truncation.
fn whole_ms(offset: Duration) -> chrono::DateTime<Utc> {
    let at = Utc::now() + offset;
    chrono::DateTime::from_timestamp_millis(at.timestamp_millis()).expect("in range")
}

/// Seed `tui`'s card for `PANE_ID` from `snapshot`, exactly as the TUI's
/// reconnect hydration does (`ui.rs`: `register_pane`, then
/// `seed_hydrated_session` under the same lock).
fn reconnect(tui: &mut AppState, snapshot: Option<&SessionSnapshot>) {
    tui.register_pane(PANE_ID.to_string());
    tui.seed_hydrated_session(
        PANE_ID.to_string(),
        Some("/tmp/runbox".to_string()),
        Some(AgentType::Pi),
        Some(RECONNECT_AGENT_ID.to_string()),
        snapshot,
    );
}

/// The key `seed_hydrated_session` stores `PANE_ID`'s card under.
fn seeded_key() -> String {
    format!("pane-{PANE_ID}")
}

/// Which generation a pane-keyed pick landed on. The daemon and a reconnected
/// TUI key the same generation differently (the TUI's seeded card is
/// `pane-<id>`), so the comparison is by the agent that owns the session.
fn owner_of(state: &AppState, session_id: Option<String>) -> Option<String> {
    session_id
        .and_then(|id| state.sessions.get(&id))
        .and_then(|session| session.agent_id.clone())
}

/// The `ListAgents` join's answer for `agent_id` on `PANE_ID`, as JSON so the
/// whole snapshot — `last_activity_ms` included — compares at once.
fn join(state: &AppState, agent_id: &str) -> Option<serde_json::Value> {
    state
        .live_session_for(agent_id, Some(PANE_ID))
        .map(|snapshot| serde_json::to_value(snapshot).expect("a snapshot serializes"))
}

/// A history-only live-target declaration, so `pane_writable` has a visible
/// answer that tells a seeded card apart from any session that declared none.
fn history_only() -> LiveTarget {
    LiveTarget {
        kind: TargetKind::Process,
        writable: Writable::HistoryOnly,
    }
}

/// Scenario: A daemon holds an agent whose history-only card went quiet an hour ago, and a TUI reconnects and seeds its card from the daemon's snapshot. A different agent's frame then reaches both — stamped half an hour after the quiet instant in one run, half an hour before it in another. The TUI must retire or keep its seeded card exactly where the daemon retires or keeps its own, so `pane_writable`, `pane_session_id` and the `ListAgents` join pick the same generation on both sides.
#[spec("status/supersede/016")]
#[test]
fn status_supersede_016_a_reconnected_card_is_superseded_exactly_where_the_daemons_is() {
    let quiet_since = whole_ms(-Duration::hours(1));
    for (straggler_at, daemon_retires_incumbent) in [
        (quiet_since + Duration::minutes(30), true),
        (quiet_since - Duration::minutes(30), false),
    ] {
        // The daemon's own state, built the way hook frames build it.
        let mut daemon = AppState::default();
        daemon.register_pane(PANE_ID.to_string());
        let mut announced = event(
            "incumbent-session",
            AgentType::Pi,
            EventType::SessionStart,
            Some(RECONNECT_AGENT_ID),
            quiet_since - Duration::minutes(5),
        );
        announced.live_target = Some(history_only());
        daemon.apply_event(announced);
        daemon.apply_event(event(
            "incumbent-session",
            AgentType::Pi,
            EventType::Idle,
            Some(RECONNECT_AGENT_ID),
            quiet_since,
        ));

        // What `ListAgents` hands the reconnecting TUI for that record.
        let snapshot = daemon
            .live_session_for(RECONNECT_AGENT_ID, Some(PANE_ID))
            .expect("the daemon holds a live session for the agent");
        let mut tui = AppState::default();
        reconnect(&mut tui, Some(&snapshot));

        assert_eq!(
            tui.sessions[&seeded_key()].last_activity,
            quiet_since,
            "the reconnected card must carry the daemon's quiet instant, not the moment it was seeded"
        );
        assert_eq!(
            join(&tui, RECONNECT_AGENT_ID),
            join(&daemon, RECONNECT_AGENT_ID),
            "right after the reconnect the join must answer exactly what the daemon answers"
        );

        // Both sides receive the same broadcast frame: a different agent on the
        // pane, whose takeover is only INFERRED, so `supersedes_generation`
        // weighs its stamp against the incumbent's `last_activity`.
        let straggler = event(
            "successor-session",
            AgentType::Pi,
            EventType::Thinking,
            Some("successor-agent"),
            straggler_at,
        );
        daemon.apply_event(straggler.clone());
        tui.apply_event(straggler);

        let holds_incumbent = |state: &AppState| {
            state
                .sessions
                .values()
                .any(|session| session.agent_id.as_deref() == Some(RECONNECT_AGENT_ID))
        };
        assert_eq!(
            holds_incumbent(&daemon),
            !daemon_retires_incumbent,
            "precondition: the daemon's own supersession decision"
        );
        assert_eq!(
            holds_incumbent(&tui),
            holds_incumbent(&daemon),
            "a frame stamped {straggler_at} must retire the reconnected card exactly where it retires \
             the daemon's (quiet since {quiet_since})"
        );
        assert_eq!(
            tui.pane_writable(PANE_ID),
            daemon.pane_writable(PANE_ID),
            "pane_writable must pick the same generation on the TUI as on the daemon"
        );
        assert_eq!(
            owner_of(&tui, tui.pane_session_id(PANE_ID)),
            owner_of(&daemon, daemon.pane_session_id(PANE_ID)),
            "pane_session_id must pick the same generation on the TUI as on the daemon"
        );
        for agent in [RECONNECT_AGENT_ID, "successor-agent"] {
            assert_eq!(
                join(&tui, agent),
                join(&daemon, agent),
                "the ListAgents join must answer the same for {agent} on both sides"
            );
        }
    }
}

/// Scenario: A TUI already holds evidence for the card it is about to seed — a `SessionStart` that landed before hydration and ties the snapshot, an older one, a future-stamped session from another agent, a newer report the same agent sent without a pane, or the card itself after it saw a newer frame than the snapshot it is re-seeded from. Seeding the card from the daemon's snapshot must change no newest-wins pick: the seeded card wins exactly where a freshly minted one won, never ties, and its `last_activity` never drops below evidence the state already held.
#[spec("status/supersede/017")]
#[test]
fn status_supersede_017_the_reconnect_overlay_changes_no_newest_wins_pick() {
    let quiet_since = whole_ms(-Duration::hours(1));
    let snapshot_at = |at: chrono::DateTime<Utc>| SessionSnapshot {
        status: SessionStatus::Working,
        agent_type: Some(AgentType::Pi),
        active_tool: None,
        // Distinct from anything a bare `SessionStart` card carries, so the
        // join's answer says which card it picked.
        tool_count: 7,
        first_prompts: Vec::new(),
        last_user_prompt: None,
        live_target: Some(history_only()),
        last_activity_ms: Some(at.timestamp_millis()),
    };
    // A key that sorts AFTER `pane-<id>`: the join breaks an exact tie on the
    // larger session id, so a tie with this card is observable rather than
    // resolved in the seeded card's favour by the alphabet.
    let early_key = "zz-early-session";

    // (a) A `SessionStart` reached the TUI before hydration seeded the pane (its
    // event subscriber starts first), and it was the daemon's newest frame, so
    // the snapshot's instant EQUALS its stamp.
    // (b) The same, with the snapshot strictly newer than it.
    for (early_at, overlay_applies) in [
        (quiet_since, false),
        (quiet_since - Duration::minutes(10), true),
    ] {
        let mut tui = AppState::default();
        tui.apply_event(event(
            early_key,
            AgentType::Pi,
            EventType::SessionStart,
            Some(RECONNECT_AGENT_ID),
            early_at,
        ));
        let before_seed = Utc::now();
        reconnect(&mut tui, Some(&snapshot_at(quiet_since)));

        let seeded = &tui.sessions[&seeded_key()];
        if overlay_applies {
            assert_eq!(
                seeded.last_activity, quiet_since,
                "a snapshot newer than everything the state holds must be taken"
            );
        } else {
            assert!(
                seeded.last_activity >= before_seed,
                "a snapshot that only TIES the pane's newest evidence must leave the freshly minted \
                 value, not create a tie; got {}",
                seeded.last_activity
            );
        }
        assert_eq!(
            tui.pane_session_id(PANE_ID),
            Some(seeded_key()),
            "pane_session_id must still pick the seeded card (early frame at {early_at})"
        );
        assert_eq!(
            tui.pane_writable(PANE_ID),
            Writable::HistoryOnly,
            "pane_writable must still pick the seeded card (early frame at {early_at})"
        );
        assert_eq!(
            tui.agent_writable(RECONNECT_AGENT_ID),
            Writable::HistoryOnly,
            "agent_writable must still pick the seeded card (early frame at {early_at})"
        );
        assert_eq!(
            tui.live_session_for(RECONNECT_AGENT_ID, Some(PANE_ID))
                .map(|live| live.tool_count),
            Some(7),
            "the ListAgents join must still pick the seeded card (early frame at {early_at})"
        );
    }

    // (c) A session from another agent on the pane carries a stamp in the
    // future, so a freshly minted card never beat it. A snapshot stamped even
    // further out must not flip that.
    let mut tui = AppState::default();
    tui.apply_event(event(
        "future-session",
        AgentType::Pi,
        EventType::SessionStart,
        Some("future-agent"),
        whole_ms(Duration::hours(1)),
    ));
    reconnect(&mut tui, Some(&snapshot_at(whole_ms(Duration::hours(2)))));
    assert_eq!(
        owner_of(&tui, tui.pane_session_id(PANE_ID)).as_deref(),
        Some("future-agent"),
        "a card that lost to a future-stamped session before the overlay must still lose to it"
    );
    assert_eq!(
        tui.pane_writable(PANE_ID),
        Writable::Live,
        "pane_writable must still pick the future-stamped session"
    );

    // (d) The same agent reported without a pane before hydration, later than
    // the snapshot's instant. `agent_writable` weighs that session against the
    // seeded card, so it is evidence too, even though it is on no pane.
    let mut tui = AppState::default();
    let mut paneless = event(
        "paneless-session",
        AgentType::Pi,
        EventType::Thinking,
        Some(RECONNECT_AGENT_ID),
        quiet_since + Duration::minutes(1),
    );
    paneless.pane_id = None;
    tui.apply_event(paneless);
    reconnect(&mut tui, Some(&snapshot_at(quiet_since)));
    assert_eq!(
        tui.agent_writable(RECONNECT_AGENT_ID),
        Writable::HistoryOnly,
        "agent_writable must still pick the seeded card over the agent's paneless session"
    );

    // (e) The card saw a frame newer than the snapshot it is later re-seeded
    // from. The re-seed must not move its evidence backward — the same
    // high-water mark `apply_event` keeps (`status/supersede/004`).
    let mut tui = AppState::default();
    let stale = snapshot_at(quiet_since);
    reconnect(&mut tui, Some(&stale));
    let newer = quiet_since + Duration::minutes(30);
    tui.apply_event(event(
        "later-session",
        AgentType::Pi,
        EventType::Thinking,
        Some(RECONNECT_AGENT_ID),
        newer,
    ));
    assert_eq!(
        tui.sessions[&seeded_key()].last_activity,
        newer,
        "precondition: the newer frame landed on the seeded card"
    );
    reconnect(&mut tui, Some(&stale));
    assert!(
        tui.sessions[&seeded_key()].last_activity >= newer,
        "a re-seed from a snapshot older than the card's own evidence moved last_activity backward to {}",
        tui.sessions[&seeded_key()].last_activity
    );
}

/// Scenario: A TUI reconnects to a daemon whose agent reported a stamp an hour in the future, and seeds its card from that snapshot. A different agent then takes the pane and reports a minute after the reconnect. The card must not import the future stamp, so that honest successor still retires it; imported, the stamp would pin the stale card against every successor until the wall clock caught up, because the daemon's registry-ownership ground has no counterpart in a client.
#[spec("status/supersede/018")]
#[test]
fn status_supersede_018_a_future_stamped_snapshot_cannot_pin_the_reconnected_card() {
    let future = whole_ms(Duration::hours(1));
    let snapshot = SessionSnapshot {
        status: SessionStatus::Working,
        agent_type: Some(AgentType::Pi),
        active_tool: None,
        tool_count: 7,
        first_prompts: Vec::new(),
        last_user_prompt: None,
        live_target: Some(history_only()),
        last_activity_ms: Some(future.timestamp_millis()),
    };
    let mut tui = AppState::default();
    let before_seed = Utc::now();
    reconnect(&mut tui, Some(&snapshot));
    let after_seed = Utc::now();

    let seeded = tui.sessions[&seeded_key()].last_activity;
    assert!(
        seeded >= before_seed && seeded <= after_seed,
        "a snapshot stamped in the future must leave the freshly minted value, got {seeded}"
    );

    tui.apply_event(event(
        "successor-session",
        AgentType::Pi,
        EventType::Thinking,
        Some("successor-agent"),
        after_seed + Duration::minutes(1),
    ));
    assert!(
        !tui.sessions.contains_key(&seeded_key()),
        "an honest successor's frame must still retire the reconnected card"
    );
    assert_eq!(
        owner_of(&tui, tui.pane_session_id(PANE_ID)).as_deref(),
        Some("successor-agent"),
        "pane_session_id must follow the successor, not a card pinned by a future stamp"
    );
    assert_eq!(
        tui.pane_writable(PANE_ID),
        Writable::Live,
        "pane_writable must follow the successor, not the pinned history-only card"
    );
}
