//! PRD #1542: the pending question on an agent's session — when the deck sets
//! it, what clears it, and how it survives a reconnect. Pure state: no daemon,
//! no PTY, no socket.

use std::collections::HashMap;
use std::sync::Arc;

#[path = "common/child_lifetime_bound.rs"]
mod child_lifetime_bound;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::event::{
    AgentEvent, AgentType, BroadcastMsg, EventType, QUESTION_RESOLVED_METADATA_KEY,
    SUBAGENT_ID_METADATA_KEY,
};
use dot_agent_deck::question::{
    AnswerChannel, OptionRole, PendingQuestion, Question, QuestionKind, QuestionOption,
    QuestionTool, ReleaseReason, ReplyOutcome,
};
use dot_agent_deck::state::{AppState, SessionStatus, SharedState};
use spec::spec;

const PANE: &str = "question-pane";
const SESSION: &str = "question-session";
const AGENT: &str = "7";

fn question(id: &str, tool: Option<(&str, Option<&str>)>) -> PendingQuestion {
    let option = |index: u32, label: &str, role: OptionRole| QuestionOption {
        index,
        label: label.to_string(),
        description: None,
        role,
        keyboard_only: false,
        scope: None,
    };
    PendingQuestion {
        id: id.to_string(),
        kind: QuestionKind::Permission,
        questions: vec![Question {
            prompt: "Allow Bash?".to_string(),
            header: None,
            options: vec![
                option(1, "Yes", OptionRole::AllowOnce),
                option(2, "No", OptionRole::Deny),
            ],
            multi_select: false,
        }],
        raised_at_ms: 1,
        tool: tool.map(|(name, use_id)| QuestionTool {
            name: name.to_string(),
            detail: None,
            use_id: use_id.map(str::to_string),
        }),
        channel: AnswerChannel::Held,
        subagent_id: None,
        revision: None,
    }
}

fn event(event_type: EventType) -> AgentEvent {
    AgentEvent {
        session_id: SESSION.to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some(PANE.to_string()),
        agent_id: Some(AGENT.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
    }
}

fn asking(event_type: EventType, q: &PendingQuestion) -> AgentEvent {
    let mut e = event(event_type);
    e.set_question(q);
    e
}

fn tool_end(tool: &str, use_id: Option<&str>) -> AgentEvent {
    let mut e = event(EventType::ToolEnd);
    e.tool_name = Some(tool.to_string());
    if let Some(id) = use_id {
        e.metadata.insert("tool_use_id".to_string(), id.to_string());
    }
    e
}

fn fresh() -> AppState {
    let mut state = AppState::default();
    state.register_pane(PANE.to_string());
    state.apply_event(event(EventType::SessionStart));
    state
}

fn pending(state: &AppState) -> Option<String> {
    state.sessions[SESSION]
        .pending_question
        .as_ref()
        .map(|q| q.id.clone())
}

/// Scenario: A permission request carrying a question sets the session's
/// pending question and the card reads Needs Input; so does a `ToolStart`
/// carrying one — Codex's `request_user_input` — which on its own would read
/// Working.
#[spec("question/state/001")]
#[test]
fn question_state_001_a_question_event_sets_the_question_and_needs_input() {
    let mut state = fresh();
    state.apply_event(asking(EventType::PermissionRequest, &question("q-a", None)));
    assert_eq!(pending(&state).as_deref(), Some("q-a"));
    assert_eq!(
        state.sessions[SESSION].status,
        SessionStatus::WaitingForInput
    );

    let mut state = fresh();
    let mut start = asking(
        EventType::ToolStart,
        &question("call_1", Some(("request_user_input", Some("call_1")))),
    );
    start.tool_name = Some("request_user_input".to_string());
    state.apply_event(start);
    assert_eq!(pending(&state).as_deref(), Some("call_1"));
    assert_eq!(
        state.sessions[SESSION].status,
        SessionStatus::WaitingForInput
    );

    // A question that does not survive sanitizing sets nothing.
    let mut state = fresh();
    let mut bad = question("q-a", None);
    bad.id = "not an id".to_string();
    state.apply_event(asking(EventType::WaitingForInput, &bad));
    assert_eq!(pending(&state), None);
}

/// Scenario: The question's own tool ending clears it — matched by
/// `tool_use_id` where the question has one, by tool name where it has none —
/// while a `ToolEnd` for a different tool running beside it does not.
#[spec("question/state/002")]
#[test]
fn question_state_002_its_tools_end_clears_another_tools_does_not() {
    let mut state = fresh();
    state.apply_event(asking(
        EventType::ToolStart,
        &question("call_1", Some(("request_user_input", Some("call_1")))),
    ));
    state.apply_event(tool_end("request_user_input", Some("call_other")));
    assert_eq!(pending(&state).as_deref(), Some("call_1"), "another call");
    state.apply_event(tool_end("Read", None));
    assert_eq!(pending(&state).as_deref(), Some("call_1"), "another tool");
    state.apply_event(tool_end("request_user_input", Some("call_1")));
    assert_eq!(pending(&state), None);

    let mut state = fresh();
    state.apply_event(asking(
        EventType::PermissionRequest,
        &question("q-bash", Some(("Bash", None))),
    ));
    state.apply_event(tool_end("Write", None));
    assert_eq!(pending(&state).as_deref(), Some("q-bash"));
    state.apply_event(tool_end("Bash", None));
    assert_eq!(
        pending(&state),
        None,
        "a keyboard yes runs the tool, and its end clears"
    );
}

/// Scenario: A new turn (a prompt), an idle (a `Stop`, or Codex's
/// `Interrupt`), a session start and an error each clear the pending question,
/// and a session end takes the session with it; Claude Code's `Notification`,
/// which fires six seconds after the question, does not.
#[spec("question/state/003")]
#[test]
fn question_state_003_the_agent_moving_on_clears_and_a_notification_does_not() {
    let raise = |state: &mut AppState| {
        state.apply_event(asking(EventType::PermissionRequest, &question("q-a", None)));
        assert_eq!(pending(state).as_deref(), Some("q-a"));
    };
    let mut prompt = event(EventType::Thinking);
    prompt.user_prompt = Some("do something else".to_string());
    for clearing in [
        prompt,
        event(EventType::Idle),
        event(EventType::SessionStart),
        event(EventType::Error),
    ] {
        let mut state = fresh();
        raise(&mut state);
        let kind = clearing.event_type.clone();
        state.apply_event(clearing);
        assert_eq!(pending(&state), None, "{kind:?} must clear the question");
    }

    let mut state = fresh();
    raise(&mut state);
    state.apply_event(event(EventType::WaitingForInput));
    assert_eq!(pending(&state).as_deref(), Some("q-a"), "Notification");
    state.apply_event(event(EventType::Thinking));
    assert_eq!(
        pending(&state).as_deref(),
        Some("q-a"),
        "a Thinking without a prompt is not a new turn"
    );
    state.apply_event(event(EventType::SessionEnd));
    assert!(
        state
            .sessions
            .get(SESSION)
            .is_none_or(|s| s.pending_question.is_none())
    );

    // A subagent's question ends with that subagent.
    let mut state = fresh();
    let mut from_sub = asking(EventType::PermissionRequest, &question("q-sub", None));
    from_sub
        .metadata
        .insert(SUBAGENT_ID_METADATA_KEY.to_string(), "sub-1".to_string());
    state.apply_event(from_sub);
    let mut stop = event(EventType::SubagentStop);
    stop.metadata
        .insert(SUBAGENT_ID_METADATA_KEY.to_string(), "sub-2".to_string());
    state.apply_event(stop.clone());
    assert_eq!(
        pending(&state).as_deref(),
        Some("q-sub"),
        "another subagent"
    );
    stop.metadata
        .insert(SUBAGENT_ID_METADATA_KEY.to_string(), "sub-1".to_string());
    state.apply_event(stop);
    assert_eq!(pending(&state), None);
}

/// Scenario: The agent reporting the pending question answered — OpenCode's
/// `permission.replied` names it — clears it; a report naming another question
/// does not.
#[spec("question/state/004")]
#[test]
fn question_state_004_the_agent_reporting_it_answered_clears_it() {
    let mut state = fresh();
    state.apply_event(asking(
        EventType::PermissionRequest,
        &question("per_1", None),
    ));
    let mut replied = event(EventType::Thinking);
    replied.metadata.insert(
        QUESTION_RESOLVED_METADATA_KEY.to_string(),
        "per_0".to_string(),
    );
    state.apply_event(replied.clone());
    assert_eq!(pending(&state).as_deref(), Some("per_1"));
    replied.metadata.insert(
        QUESTION_RESOLVED_METADATA_KEY.to_string(),
        "per_1".to_string(),
    );
    state.apply_event(replied);
    assert_eq!(pending(&state), None);
}

/// Scenario: Through the daemon's own ingest, a newer question replaces an
/// older one on the same pane, and the producer still holding the older one
/// is released as superseded; the newer one's hold is kept. A later idle
/// releases that one as cleared.
#[spec("question/state/005")]
#[tokio::test]
async fn question_state_005_a_newer_question_supersedes_and_releases_the_older() {
    child_lifetime_bound::arm();
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(fresh()));
    let (event_tx, _rx) = tokio::sync::broadcast::channel::<BroadcastMsg>(16);
    let registry = Arc::new(AgentPtyRegistry::new());

    // Registered and published through the daemon's own path, which stamps
    // each question with its own registration's generation (audit R1).
    let older = dot_agent_deck::daemon::register_and_publish_held(
        &state,
        &event_tx,
        &registry,
        PANE,
        "agent-1",
        asking(EventType::PermissionRequest, &question("q-old", None)),
    )
    .await
    .expect("the older question is pending")
    .rx;
    assert!(registry.question_holds().is_held(PANE, "q-old"));

    let newer = dot_agent_deck::daemon::register_and_publish_held(
        &state,
        &event_tx,
        &registry,
        PANE,
        "agent-1",
        asking(EventType::PermissionRequest, &question("q-new", None)),
    )
    .await
    .expect("the newer question is pending")
    .rx;
    assert_eq!(pending(&*state.read().await).as_deref(), Some("q-new"));
    let released = older.await.expect("the older hold is answered");
    assert_eq!(released.outcome, ReplyOutcome::Released);
    assert_eq!(released.reason, Some(ReleaseReason::Superseded));
    assert_eq!(released.question_id, "q-old");
    assert!(registry.question_holds().is_held(PANE, "q-new"));

    dot_agent_deck::daemon::ingest_event(&state, &event_tx, &registry, event(EventType::Idle))
        .await;
    let released = newer.await.expect("the newer hold is answered");
    assert_eq!(released.reason, Some(ReleaseReason::Cleared));
    assert!(!registry.question_holds().is_held(PANE, "q-new"));
}

/// Scenario: The pending question travels on the session's snapshot — what a
/// reconnecting client reads from `ListAgents` — and a client that hydrates a
/// card from that snapshot holds the same question.
#[spec("question/state/006")]
#[test]
fn question_state_006_the_snapshot_carries_it_and_hydration_restores_it() {
    let mut state = fresh();
    let q = question("q-a", Some(("Bash", None)));
    state.apply_event(asking(EventType::PermissionRequest, &q));
    let snapshot = state
        .live_session_for(AGENT, Some(PANE))
        .expect("a live session");
    assert_eq!(snapshot.pending_question.as_ref(), Some(&q));
    assert_eq!(
        state
            .pending_question_for(AGENT, Some(PANE))
            .map(|(_, q)| q.id),
        Some("q-a".to_string())
    );
    let wire: dot_agent_deck::state::SessionSnapshot =
        serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();

    let mut client = AppState::default();
    client.seed_hydrated_session(
        PANE.to_string(),
        None,
        Some(AgentType::ClaudeCode),
        Some(AGENT.to_string()),
        Some(&wire),
    );
    let restored: Vec<&PendingQuestion> = client
        .sessions
        .values()
        .filter_map(|s| s.pending_question.as_ref())
        .collect();
    assert_eq!(restored, vec![&q]);
}

/// Scenario: A client's event stream drops and resubscribes (issue #1520).
/// A question raised while it was not listening — revision and all — reaches
/// its card through the resync, the card reads Needs Input, and a question
/// answered while it was away is gone from the card after the next resync.
#[spec("question/state/007")]
#[test]
fn question_state_007_a_resync_after_an_event_gap_carries_the_question() {
    let records_from = |daemon: &AppState| {
        let mut records: Vec<dot_agent_deck::agent_pty::AgentRecord> = vec![
            serde_json::from_value(serde_json::json!({ "id": AGENT, "pane_id_env": PANE }))
                .unwrap(),
        ];
        daemon.attach_live_sessions(&mut records);
        records
    };

    let mut client = fresh();
    let mut daemon = client.clone();
    let mut q = question("q-a", Some(("Bash", None)));
    q.revision = Some(3);
    daemon.apply_event(asking(EventType::PermissionRequest, &q));
    assert_eq!(pending(&client), None, "the client missed the question");

    client.resync_after_event_gap(&records_from(&daemon));
    assert_eq!(
        client.sessions[SESSION].pending_question.as_ref(),
        Some(&q),
        "the resync must restore the question raised during the gap, revision included"
    );
    assert_eq!(
        client.sessions[SESSION].status,
        SessionStatus::WaitingForInput
    );

    // The question is answered while the client is away again.
    let mut resolved = event(EventType::ToolEnd);
    resolved.tool_name = Some("Bash".to_string());
    resolved.metadata.insert(
        QUESTION_RESOLVED_METADATA_KEY.to_string(),
        "q-a".to_string(),
    );
    daemon.apply_event(resolved);
    assert_eq!(pending(&daemon), None);
    assert_eq!(pending(&client).as_deref(), Some("q-a"));

    client.resync_after_event_gap(&records_from(&daemon));
    assert_eq!(
        pending(&client),
        None,
        "the resync must drop a question answered during the gap"
    );
}
