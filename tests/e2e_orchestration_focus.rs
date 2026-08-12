#![cfg(feature = "e2e")]

//! L2 end-to-end coverage for the lock-governed focus contract — the
//! real-binary proof that focus follows the command-entry lock.
//! `orchestration/lock/*` (`e2e_orchestration_lock.rs`) and
//! `orchestration/focus/001`-`006` (`src/tab.rs`) each pin one mechanism in
//! isolation: the keystroke gate, the auto-focus chain, the lock-gated call
//! site's `TabManager`-level contract. Nothing else drives all of it together,
//! end to end, in a real pane, the way a human actually experiences it: locked
//! by default, a worker visibly pulling focus when it needs the human and
//! visibly releasing it once resolved, `Ctrl+e` handing control back, and a
//! manual focus choice actually sticking once it does.
//!
//! This contract is part of the experimental command-entry-lock surface, so
//! the deck launches with `DOT_AGENT_DECK_EXPERIMENTAL=1`. Without that flag,
//! the production gate intentionally disables both the lock and its focus
//! steering, and this test would assert on a surface that is not enabled.
//!
//! Uses the `orch-focus-lifecycle` fixture (`orchestrator` start role plus
//! `alpha` and `beta`, all plain `printf`+`sleep` stubs — no LLM tokens
//! spent). A 3-role fixture is required rather than the 2-role `orch-deck`:
//! the "manual focus sticks" half needs a role OTHER than the one going
//! `WaitingForInput`, because where the focused role and the waiting role are
//! the same pane a genuine stick is indistinguishable from
//! `auto_focus_waiting_pane`'s own no-flicker same-pane no-op.
//! `WaitingForInput` is driven synthetically over the hook socket, exactly as
//! `orchestration/lock/011` does, so `beta`'s checked-in placeholder script is
//! fine as is.
//!
//! Gated behind the `e2e` feature so `cargo test-fast` never compiles it.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use spec::spec;

/// Drive the new-pane dialog to open the (single) orchestration in the
/// `orch-focus-lifecycle` fixture. With no `[[modes]]` defined the Mode chip
/// row is `[No mode] [Orch: focus-lifecycle] [schedule]`, so ONE Right selects
/// the orchestration; selecting an orchestration hides the Command field, so a
/// second Enter submits the form. Lands with the orchestrator (start) role
/// focused in `PaneInput` mode, the deck's default LOCKED state untouched.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_bytes(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_bytes(b" "); // Space -> confirm current dir -> new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    deck.send_bytes(b"\x1b[C"); // Right -> [Orch: focus-lifecycle]
    deck.send_bytes(b"\r"); // Mode -> Name
    deck.send_bytes(b"\r"); // submit (Command hidden for an orchestration)
}

/// The rendered grid's expanded-pane top border fuses the pane's title
/// directly into the box-drawing corner as `┌<role>` in `PaneInput` mode
/// (`TerminalWidget`, `src/terminal_widget.rs`). Only the currently focused
/// role ever renders this way — every other role collapses to a small numbered
/// card with no live PTY body — so this string's presence on the settled grid
/// names which role currently holds focus with live content on screen: the
/// observable this whole test is built on.
fn expanded_header(role: &str) -> String {
    format!("┌{role}")
}

/// The `orch-focus-lifecycle` fixture's full daemon registry record for
/// `role`. Mirrors `e2e_orchestration_lock.rs::worker_agent_record`,
/// generalized from a single hardcoded role name to any role in this 3-role
/// fixture, since this test needs to target `alpha` and `beta` independently.
fn role_agent_record(
    socket: &std::path::Path,
    role: &str,
) -> dot_agent_deck::agent_pty::AgentRecord {
    common::agent_records_on(socket)
        .into_iter()
        .find(|r| {
            matches!(
                &r.tab_membership,
                Some(dot_agent_deck::agent_pty::TabMembership::Orchestration { role_name, .. })
                    if role_name == role
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "orch-focus-lifecycle fixture's {role} role pane must be registered with the daemon"
            )
        })
}

/// Inject a synthetic `AgentEvent` for `role`'s real `(pane_id_env, agent_id)`
/// pair over the deck's hook socket — the SAME bare-`AgentEvent`,
/// no-`DaemonMessage`-envelope wire the real `dot-agent-deck agent-event
/// --type running|waiting|finished` CLI already rides for status reporting.
/// Mirrors `e2e_orchestration_lock.rs::inject_worker_status` (generalized to
/// any role) rather than writing a second injector — that file's version
/// documents why both `pane_id` AND `agent_id` must be the role's REAL values.
///
/// Blocks not on the daemon's broadcast (which fires whether or not
/// `apply_event` actually accepted the event) but on `ListAgents`'
/// `AgentRecord.live` join reporting the expected `SessionStatus` back for
/// `role`'s pane — proof the daemon's OWN state, not just its wire, reflects
/// the change before the caller starts asserting on focus movement driven by
/// it.
#[cfg(unix)]
fn inject_role_status(
    deck: &TuiDeck,
    socket: &std::path::Path,
    pane_id: &str,
    agent_id: &str,
    session_id: &str,
    event_type: EventType,
) {
    let expected_status = match event_type {
        EventType::WaitingForInput => dot_agent_deck::state::SessionStatus::WaitingForInput,
        EventType::Thinking => dot_agent_deck::state::SessionStatus::Thinking,
        other => {
            panic!("inject_role_status: no expected SessionStatus mapping wired up for {other:?}")
        }
    };
    let event = AgentEvent {
        session_id: session_id.to_string(),
        agent_type: AgentType::Pi,
        event_type: event_type.clone(),
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: std::collections::HashMap::new(),
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
    };
    let line = serde_json::to_string(&event).expect("serialize synthetic AgentEvent");
    common::write_hook_line(deck.hook_socket_path(), &line)
        .expect("inject synthetic AgentEvent over hook socket");

    let applied = common::wait_until(Duration::from_secs(10), || {
        common::agent_records_on(socket).into_iter().any(|r| {
            r.pane_id_env.as_deref() == Some(pane_id)
                && r.live.as_ref().map(|s| &s.status) == Some(&expected_status)
        })
    });
    assert!(
        applied,
        "the daemon's own ListAgents/live-status join never reported {event_type:?} \
         for pane {pane_id} (agent_id {agent_id}) within 10s — the hook socket write \
         was accepted, but AppState::apply_event may have rejected it or applied it \
         to the wrong session.",
    );
}

/// Scenario: Launch a real Orchestration tab with the experimental command-entry-lock surface enabled, so LOCKED is that enabled surface's initial state, and verify waiting `alpha` visibly takes focus before all-clear returns it to `orchestrator`. Unlock through `Ctrl+d` then `Ctrl+e`, manually focus `beta`, and inject a fresh `alpha` waiting/all-clear pair; `beta`'s expanded pane and typed sentinel both remain visible because while unlocked the per-frame call site never even calls `observe_waiting_panes`, so no auto-focus branch is left to fight the human's manual choice.
#[spec("orchestration/focus/007")]
#[test]
fn focus_007_lock_governed_focus_contract_on_real_binary() {
    const BETA_STICK_SENTINEL: &str = "FOCUS007_BETA_STICK_6d4e";

    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .launch_with_fixture("orch-focus-lifecycle");
    deck.wait_for_string("No active sessions");

    open_orchestration(&deck);
    deck.wait_for_absence("New Agent"); // form closed -> tab up, orchestrator focused

    let socket = deck.attach_socket_path().to_path_buf();
    let alpha_record = role_agent_record(&socket, "alpha");
    let alpha_id = alpha_record.id.clone();
    let alpha_pane_id = alpha_record
        .pane_id_env
        .clone()
        .expect("alpha role pane must have a DOT_AGENT_DECK_PANE_ID recorded");
    let alpha_session_id = format!("{alpha_id}-focus007-session");

    // --- Step 1: LOCKED by default, focus sits on the orchestrator pane. ---
    let orch_expanded = expanded_header("orchestrator");
    let alpha_expanded = expanded_header("alpha");
    let beta_expanded = expanded_header("beta");
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
            grid.contains(&orch_expanded)
        }),
        "a freshly opened orchestration tab never showed the orchestrator \
         role's expanded pane box (needle {orch_expanded:?}) — expected the \
         default LOCKED state to leave focus on the orchestrator.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );

    // --- Step 2: alpha goes WaitingForInput and visibly pulls focus. ---
    inject_role_status(
        &deck,
        &socket,
        &alpha_pane_id,
        &alpha_id,
        &alpha_session_id,
        EventType::WaitingForInput,
    );
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(10), |grid| {
            grid.contains(&alpha_expanded)
        }),
        "injecting WaitingForInput for the non-orchestrator alpha role never \
         steered focus onto its expanded pane box (needle {alpha_expanded:?}) — \
         expected the locked deck's auto-focus chain to pull focus onto a \
         waiting role.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );

    // --- Step 3: alpha resolves; focus returns to the orchestrator on the
    // all-clear edge. ---
    inject_role_status(
        &deck,
        &socket,
        &alpha_pane_id,
        &alpha_id,
        &alpha_session_id,
        EventType::Thinking,
    );
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(10), |grid| {
            grid.contains(&orch_expanded)
        }),
        "alpha's status clearing from WaitingForInput never returned focus to \
         the orchestrator's expanded pane box (needle {orch_expanded:?}) — \
         expected the locked deck's all-clear edge to fire.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );

    // --- Step 4: Ctrl+d then Ctrl+e unlocks. ---
    deck.send_bytes(b"\x04"); // Ctrl+d -> Normal (command) mode
    deck.send_bytes(b"\x05"); // Ctrl+e -> Action::ToggleOrchestrationLock
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
            grid.contains("Pane entry: unlocked")
        }),
        "Ctrl+d then Ctrl+e never surfaced the 'Pane entry: unlocked' status \
         message — expected the command-mode chord to flip \
         ui.command_entry_locked to unlocked.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );

    // --- Step 5: manual focus on beta (a NON-orchestrator role, deliberately
    // NOT alpha — see the doc comment) sticks across both a fresh waiting
    // episode and its all-clear, because unlocked runs no auto-focus branch at
    // all. Still in Normal mode from the Ctrl+e above: digit '3' -> Jump3 ->
    // Action::FocusCard(2) -> role_pane_ids[2] == beta. `focus_deck` re-enters
    // PaneInput mode on success, so no extra Ctrl+d is needed before the
    // sentinel below. ---
    deck.send_bytes(b"3");
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
            grid.contains(&beta_expanded)
        }),
        "manually jumping to the beta role (digit '3') never showed its \
         expanded pane box (needle {beta_expanded:?}) — cannot proceed with the \
         manual-focus-sticks assertion below without confirming the initial \
         focus target first.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );

    // A fresh waiting episode on alpha — a DIFFERENT role than the one manually
    // focused. Under the LOCKED contract (steps 1-3 above) this would steer
    // focus onto alpha; unlocked, it must not.
    inject_role_status(
        &deck,
        &socket,
        &alpha_pane_id,
        &alpha_id,
        &alpha_session_id,
        EventType::WaitingForInput,
    );
    // No in-process signal for "the deck observed the event and chose not to
    // move focus" — poll for a bounded stretch and confirm beta's expanded box
    // is what's there throughout.
    let alpha_stole_focus = deck.wait_for_grid_predicate_within(Duration::from_secs(3), |grid| {
        grid.contains(&alpha_expanded)
    });
    assert!(
        !alpha_stole_focus,
        "while UNLOCKED, injecting WaitingForInput for alpha steered focus onto \
         its expanded pane box (needle {alpha_expanded:?}) anyway — expected the \
         unlocked deck to run no auto-focus branch at all, leaving the human's \
         manual focus on beta untouched.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );
    assert!(
        deck.snapshot_grid().contains(&beta_expanded),
        "beta's expanded pane box was gone after a waiting episode arrived on \
         alpha while unlocked — manual focus must survive untouched.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );

    // alpha resolves; under the LOCKED contract this would fire the all-clear
    // move back to the orchestrator. Unlocked, it must not either — beta stays
    // focused.
    inject_role_status(
        &deck,
        &socket,
        &alpha_pane_id,
        &alpha_id,
        &alpha_session_id,
        EventType::Thinking,
    );
    let snapped_to_orchestrator = deck
        .wait_for_grid_predicate_within(Duration::from_secs(3), |grid| {
            grid.contains(&orch_expanded)
        });
    assert!(
        !snapped_to_orchestrator,
        "while UNLOCKED, alpha's status clearing fired an all-clear move back to \
         the orchestrator's expanded pane box (needle {orch_expanded:?}) anyway \
         — expected no auto-focus branch to run at all while unlocked.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );

    // The final proof, not just that beta's box is still drawn but that it
    // genuinely still holds live PTY focus: a keystroke typed now must appear
    // inside beta's own expanded box.
    deck.send_keys(format!("{BETA_STICK_SENTINEL}\r").as_bytes());
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
            grid.contains(&beta_expanded) && grid.contains(BETA_STICK_SENTINEL)
        }),
        "after surviving both a waiting episode and its all-clear while \
         unlocked, a keystroke typed at the end never showed up inside beta's \
         own expanded pane box — manual focus must have stuck all the way \
         through.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );
}
