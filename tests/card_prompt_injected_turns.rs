//! L1 tests for issue #1993: the prompt a card shows is one a person typed.
//!
//! An agent's harness can submit a turn of its own through the same prompt
//! hook a typed prompt uses — Claude Code's `<task-notification>` when a
//! background command or subagent finishes. These feed such a turn through
//! [`AppState::apply_event`] after a real prompt and read the two places a
//! user sees the card's prompt: the snapshot's `last_user_prompt`, which is
//! the desktop's Last prompt column, and the TUI card's `Prmt:` lines.

use chrono::{Duration, Utc};
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::state::AppState;
use dot_agent_deck::ui::{CardDensityKind, render_card_to_buffer};
use spec::spec;

const PANE_ID: &str = "injected-turn-pane";
const SESSION_ID: &str = "injected-turn-session";
const TYPED_PROMPT: &str = "fix the login bug";

/// The turn as the issue reported it reaching the card: Claude Code's
/// notification for a finished background command.
const TASK_NOTIFICATION: &str = "<task-notification><task-id>byn96iho8</task-id><tool-use-id>toolu_012gy7Ns2WZzerY5URQwk2uh</tool-use-id><output-file>/tmp/claude-1000/-home-vfarcic-code-dot-agent-deck/tasks/byn96iho8.output</output-file><status>completed</status><summary>Background command \"cargo test\" completed (exit code 0)</summary></task-notification>";

fn prompt_event(prompt: &str, at: chrono::DateTime<Utc>) -> AgentEvent {
    AgentEvent {
        session_id: SESSION_ID.to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::Thinking,
        tool_name: None,
        tool_detail: None,
        cwd: Some("/home/dev/api-svc".to_string()),
        timestamp: at,
        user_prompt: Some(prompt.to_string()),
        metadata: Default::default(),
        pane_id: Some(PANE_ID.to_string()),
        agent_id: None,
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    }
}

fn state_after(prompts: &[&str]) -> AppState {
    let mut state = AppState::default();
    state.register_pane(PANE_ID.to_string());
    let start = Utc::now();
    for (i, prompt) in prompts.iter().enumerate() {
        state.apply_event(prompt_event(prompt, start + Duration::seconds(i as i64)));
    }
    state
}

fn card_text(state: &AppState, density: CardDensityKind) -> String {
    let session = state.sessions.get(SESSION_ID).expect("the card exists");
    let buffer = render_card_to_buffer(
        session,
        Some("api-svc"),
        Some(1),
        density,
        0,
        Utc::now(),
        false,
        80,
        density.rendered_height(),
    );
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Scenario: A Claude Code card receives the prompt "fix the login bug", then
/// the `<task-notification>` turn Claude Code submits itself when a background
/// command finishes. The desktop's Last prompt and the TUI card's prompt line
/// still read "fix the login bug" and show no part of the notification.
#[spec("dashboard/prompt/001")]
#[test]
fn prompt_001_injected_task_notification_keeps_the_typed_prompt() {
    let state = state_after(&[TYPED_PROMPT, TASK_NOTIFICATION]);
    let session = state.sessions.get(SESSION_ID).expect("the card exists");

    assert_eq!(
        session.live_snapshot().last_user_prompt.as_deref(),
        Some(TYPED_PROMPT),
        "the desktop's Last prompt must stay on what the user typed"
    );
    assert_eq!(session.first_prompts, vec![TYPED_PROMPT.to_string()]);

    for density in [CardDensityKind::Normal, CardDensityKind::Spacious] {
        let card = card_text(&state, density);
        assert!(
            card.contains(&format!("Prmt: {TYPED_PROMPT}")),
            "{density:?} card must show the typed prompt:\n{card}"
        );
        assert!(
            !card.contains("task-notification") && !card.contains("task-id"),
            "{density:?} card must not show the injected turn:\n{card}"
        );
    }
}

/// Scenario: After an injected `<task-notification>` turn, the user types the
/// slash command "/review 42" and then an ordinary prompt. Each one the user
/// typed replaces the card's prompt in turn, so the filter holds back only the
/// harness's own turn.
#[spec("dashboard/prompt/002")]
#[test]
fn prompt_002_prompts_typed_after_an_injected_turn_still_reach_the_card() {
    let state = state_after(&[TYPED_PROMPT, TASK_NOTIFICATION, "/review 42"]);
    let session = state.sessions.get(SESSION_ID).expect("the card exists");
    assert_eq!(session.last_user_prompt.as_deref(), Some("/review 42"));
    let card = card_text(&state, CardDensityKind::Normal);
    assert!(card.contains("Prmt: /review 42"), "{card}");

    let state = state_after(&[TASK_NOTIFICATION, "ship it"]);
    let session = state.sessions.get(SESSION_ID).expect("the card exists");
    assert_eq!(session.last_user_prompt.as_deref(), Some("ship it"));
    assert_eq!(session.first_prompts, vec!["ship it".to_string()]);
}
