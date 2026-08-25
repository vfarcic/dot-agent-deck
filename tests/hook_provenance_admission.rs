#![cfg(unix)]

//! Hook provenance, end to end: `hook_ingest::admit` followed by
//! `AppState::apply_event` — the two steps the daemon's hook loop runs, in that
//! order, against a REAL `AgentPtyRegistry`.
//!
//! # Why this exists and the registry unit tests are not enough
//!
//! Issue #318's re-audit, finding 1. The first fix for "a natural exit does not
//! revoke authority" was checked by a unit test that asserted `!manages_pane`
//! and `resolve_agent_token(..).is_none()` — two registry predicates agreeing
//! with each other. Neither is a refusal. In the classifier `!manages_pane` is
//! precisely what SELECTS `Provenance::Foreign`, on which `admit` leaves the
//! payload's claimed pane and agent untouched; the daemon then broadcasts the
//! event and applies it, and the OLDER ownership layer positively accepts it,
//! because a retired generation keeps its pane until another claims it. So the
//! test passed while the attack — a token-less event naming an exited pane,
//! driving its card indefinitely — worked.
//!
//! The lesson is the shape of the test, not the finding: provenance can only be
//! shown to refuse by running the whole admission path and looking at the card.
//! Every case here therefore ends at `AppState`, and the token-less case also
//! proves it is not vacuous by applying the same forged event to a CLONE of the
//! state with provenance bypassed, and showing that the card moves there.
//!
//! Fast tier: `/usr/bin/true` and `/bin/sh` stand-ins, no LLM and no daemon
//! socket. Unix-gated for the same reason the in-crate `spawn_tests` module is
//! (`#[cfg(all(test, unix))]`): those two stand-ins are real PTY spawns of paths
//! that do not exist on Windows, so without the gate the file would compile
//! there — `tests/common` is deliberately ungated — and then fail at RUNTIME on
//! a Windows contributor's `cargo test-fast`, which CI would never catch because
//! `build-windows` is clippy-only.

use std::sync::Arc;
use std::time::Duration;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::hook_ingest::{AgentToken, Provenance, RefusalReason, admit};
use dot_agent_deck::state::{AgentOwnership, AppState, SessionStatus};

mod common;

const PANE: &str = "provenance-admission-pane";
const SESSION: &str = "provenance-admission-session";

/// One hook payload as a peer would put it on the wire: the pane, the session
/// and the agent id are all *claims*, and the token is the only evidence.
fn hook_event(event_type: EventType, pane: &str, token: Option<&AgentToken>) -> AgentEvent {
    AgentEvent {
        session_id: SESSION.to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: std::collections::HashMap::new(),
        pane_id: Some(pane.to_string()),
        // Omitted on purpose in the forged case: `generation_ownership`'s
        // pane-only arm is the broader of the two, so leaving the agent id out
        // is what an attacker would do.
        agent_id: None,
        agent_version: None,
        schema_version: None,
        live_target: None,
        agent_token: token.cloned(),
    }
}

/// Exactly what `daemon::run_hook_loop` does with a decoded event: admit it,
/// and apply it only if the verdict was not a refusal.
fn daemon_ingest(
    registry: &AgentPtyRegistry,
    state: &mut AppState,
    mut event: AgentEvent,
) -> Provenance {
    let verdict = admit(registry, &mut event);
    if !matches!(verdict, Provenance::Refused(_)) {
        state.apply_event(event);
    }
    verdict
}

/// Spawn a stand-in that exits immediately, and wait for the registry to see its
/// PTY EOF **without reaping the record** — the lingering-record state every
/// case below is about.
fn spawn_and_let_it_exit(registry: &Arc<AgentPtyRegistry>, pane: &str) -> String {
    let id = registry
        .spawn_agent(SpawnOptions {
            command: Some("/usr/bin/true"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn a stand-in that exits on its own");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && registry.live_count() != 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        registry.live_count(),
        0,
        "test prerequisite: /usr/bin/true must have exited"
    );
    assert_eq!(
        registry.len(),
        1,
        "test prerequisite: the record must be UNREAPED — a removed record would \
         make every assertion below pass for the wrong reason"
    );
    id
}

fn status_of(state: &AppState, session_id: &str) -> SessionStatus {
    state
        .sessions
        .get(session_id)
        .unwrap_or_else(|| panic!("no session {session_id:?} — no card was ever created"))
        .status
        .clone()
}

/// Scenario: Start an agent on a pane and let its process exit on its own, so
/// the registry keeps the record but nothing is running. Its own late final
/// event — carrying the token the daemon minted for it — is ingested and drives
/// the card to `Thinking`. Then a forged event naming the same pane with NO
/// token, no agent id and the session id a peer could have read from the log
/// tries to move the card to `Error`: it must be refused and the card must not
/// move, while the same forged event applied with provenance bypassed DOES move
/// it — which is what makes the refusal, and not some accident of the fixture,
/// the thing protecting the card.
#[test]
fn a_tokenless_event_cannot_drive_a_pane_whose_agent_has_exited() {
    common::init_test_env();

    let registry = Arc::new(AgentPtyRegistry::new());
    let id = spawn_and_let_it_exit(&registry, PANE);
    let token = registry
        .agent_hook_token(&id)
        .expect("a retired generation still holds the token for its own pane");

    let mut state = AppState::default();
    // Same coercion the daemon does at start-up: the oracle is held as a
    // `Weak<dyn AgentOwnership>` so the registry can own the state back.
    let oracle: Arc<dyn AgentOwnership> = registry.clone();
    state.set_agent_ownership(Arc::downgrade(&oracle));

    // The tension the fix must not break: the agent's own final report, written
    // just before exit and racing its PTY EOF, still lands.
    let verdict = daemon_ingest(
        &registry,
        &mut state,
        hook_event(EventType::Thinking, PANE, Some(&token)),
    );
    assert!(
        matches!(verdict, Provenance::Bound(_)),
        "an exiting agent's own late event must still bind to its pane, got {verdict:?}"
    );
    assert_eq!(status_of(&state, SESSION), SessionStatus::Thinking);

    // The attack: no token at all, naming the exited pane.
    let forged = hook_event(EventType::Error, PANE, None);
    let bypassed = {
        let mut shadow = state.clone();
        shadow.apply_event(forged.clone());
        status_of(&shadow, SESSION)
    };
    assert_eq!(
        bypassed,
        SessionStatus::Error,
        "anti-vacuity: with provenance bypassed the older ownership layer DOES \
         accept this event, which is the whole of re-audit finding 1"
    );

    let verdict = daemon_ingest(&registry, &mut state, forged);
    assert_eq!(
        verdict,
        Provenance::Refused(RefusalReason::MissingToken),
        "a pane whose record still holds it is protected, token-less or not"
    );
    assert_eq!(
        status_of(&state, SESSION),
        SessionStatus::Thinking,
        "the card must not have moved"
    );
}

/// Scenario: Start an agent on a pane, let it exit, then start a second agent on
/// the same pane so the pane changes hands. Replaying the first agent's token
/// against that pane must be refused as an unrecognized capability, and the
/// card the live successor owns must not move — the replay the token's
/// revocation rule exists to forbid.
#[test]
fn a_replayed_token_cannot_drive_a_pane_a_later_generation_took() {
    common::init_test_env();

    let registry = Arc::new(AgentPtyRegistry::new());
    let first = spawn_and_let_it_exit(&registry, PANE);
    let stale = registry
        .agent_hook_token(&first)
        .expect("the retired generation's token, as a peer would have captured it");

    let mut state = AppState::default();
    // Same coercion the daemon does at start-up: the oracle is held as a
    // `Weak<dyn AgentOwnership>` so the registry can own the state back.
    let oracle: Arc<dyn AgentOwnership> = registry.clone();
    state.set_agent_ownership(Arc::downgrade(&oracle));
    let verdict = daemon_ingest(
        &registry,
        &mut state,
        hook_event(EventType::Thinking, PANE, Some(&stale)),
    );
    assert!(
        matches!(verdict, Provenance::Bound(_)),
        "precondition: before the handover the token still speaks for its pane"
    );
    assert_eq!(status_of(&state, SESSION), SessionStatus::Thinking);

    // A successor publishes onto the pane. The pane has changed hands.
    registry
        .spawn_agent(SpawnOptions {
            command: Some("/bin/sh"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("the pane is free for a successor once its occupant exited");

    let verdict = daemon_ingest(
        &registry,
        &mut state,
        hook_event(EventType::Error, PANE, Some(&stale)),
    );
    assert_eq!(
        verdict,
        Provenance::Refused(RefusalReason::UnknownToken),
        "a token whose generation lost the pane resolves to nothing, and the pane \
         is protected by its new occupant"
    );
    assert_eq!(
        status_of(&state, SESSION),
        SessionStatus::Thinking,
        "the card must not have moved"
    );

    registry.shutdown_all();
}

/// Scenario: Send a token-less event naming a pane this daemon never spawned.
/// It must still take the pre-existing foreign-agent path and register its card,
/// because protecting exited panes must not narrow #601's named remainder —
/// external agents posting into a deck they were not spawned by keep working.
#[test]
fn a_pane_this_daemon_never_spawned_is_still_the_foreign_path() {
    common::init_test_env();

    let registry = Arc::new(AgentPtyRegistry::new());
    // A lingering record for OUR pane, so the registry is not trivially empty
    // and `manages_any_pane`-style answers are exercised alongside.
    spawn_and_let_it_exit(&registry, PANE);

    let mut state = AppState::default();
    // Same coercion the daemon does at start-up: the oracle is held as a
    // `Weak<dyn AgentOwnership>` so the registry can own the state back.
    let oracle: Arc<dyn AgentOwnership> = registry.clone();
    state.set_agent_ownership(Arc::downgrade(&oracle));
    state.register_pane("someone-elses-pane".to_string());

    let verdict = daemon_ingest(
        &registry,
        &mut state,
        hook_event(EventType::Thinking, "someone-elses-pane", None),
    );
    assert_eq!(
        verdict,
        Provenance::Foreign,
        "a pane no generation of this registry has ever held is not protected"
    );
    assert_eq!(status_of(&state, SESSION), SessionStatus::Thinking);
}
