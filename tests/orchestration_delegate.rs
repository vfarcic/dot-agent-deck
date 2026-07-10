//! PRD #201 M1.3 — the synthetic-agent harness proves the daemon's delegate
//! routing and pane-role guard hold for a **Pi identity** (test-plan rows 5 &
//! 6). Additive Pi coverage of the same contract `orchestration/delegate/001`
//! (delegate routes into the worker pane) and `orchestration/delegate/004`
//! (only the `start = true` role may delegate) already pin for claude/opencode.
//!
//! Fast tier (no `e2e` gate), mirroring the in-process `handle_delegate`
//! precedent `delegate_prompt_injection.rs`: the worker pane is a `cat` stub
//! whose PTY echoes whatever the daemon injects, so the registry snapshot
//! reflects the delivered (or, for the rejected case, never-delivered) bytes.
//! No LLM, no daemon socket.
//!
//! These are expected to be **green on write**: the daemon's routing/guard is
//! keyed on pane ROLE (`orchestrator_pane_ids` / `pane_role_map`), NOT on
//! agent type, so a Pi-identity orchestrator/worker exercises the identical
//! path. The value here is proving the *harness* drives that path for a Pi
//! identity, which is the reusable seam the companion PRD generalizes.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{AgentType, BroadcastMsg};
use dot_agent_deck::state::AppState;

use spec::spec;

mod common;

use common::synthetic_agent::SyntheticAgent;

const ORCH_PANE: &str = "pi-orchestrator-pane";
const WORKER_PANE: &str = "coder-worker-pane";
const WORKER_ROLE: &str = "coder";
const POINTER: &[u8] = b"Read .dot-agent-deck/worker-task-coder.md for your task.";

/// Poll the agent's PTY snapshot until `needle` appears or `timeout` elapses,
/// returning the final snapshot either way.
async fn wait_for_needle(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &[u8],
    timeout: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(snap) = registry.snapshot(agent_id)
            && snap.windows(needle.len()).any(|w| w == needle)
        {
            return snap;
        }
        if tokio::time::Instant::now() >= deadline {
            return registry.snapshot(agent_id).unwrap_or_default();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Spawn a `cat`-backed worker pane in `cwd` under `pane_env`, returning its
/// registry agent id. The PTY echoes injected bytes for snapshot assertions.
fn spawn_worker_stub(registry: &AgentPtyRegistry, cwd: &str, pane_env: &str) -> String {
    registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(cwd),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_env.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn worker stub")
}

/// Scenario: A Pi-identity orchestrator (the synthetic harness,
/// `AgentType::Pi`, the `start = true` role) calls `delegate --to coder` from
/// its pane; the daemon's real `handle_delegate` routes the task into the
/// `coder` worker pane's PTY. Assert the worker `cat` stub received the
/// single-line task pointer — the same contract `orchestration/delegate/001`
/// proves for claude/opencode, now driven by a Pi orchestrator through the
/// harness.
// The linkage-check scanner links `#[spec(...)]` to the next plain `fn`
// (not `async fn`), so the spec'd entry point is a sync `#[test]` that
// `block_on`s the async body (mirrors `session/live/007` in rehydration.rs).
#[spec("orchestration/delegate/005")]
#[test]
fn delegate_005_pi_orchestrator_delegate_routes_to_worker() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime")
        .block_on(delegate_005_pi_orchestrator_delegate_routes_to_worker_inner());
}

async fn delegate_005_pi_orchestrator_delegate_routes_to_worker_inner() {
    common::init_test_env();

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("tempdir path is UTF-8")
        .to_string();

    let registry = Arc::new(AgentPtyRegistry::new());
    let worker_agent_id = spawn_worker_stub(&registry, &cwd_str, WORKER_PANE);

    let (event_tx, _rx) = broadcast::channel::<BroadcastMsg>(64);
    let orchestration = ("pi-orchestration".to_string(), cwd_str.clone());

    // The Pi orchestrator is the ONLY valid delegate source; the coder worker
    // shares the orchestration. Registration mirrors the StartAgent path.
    let pi = SyntheticAgent::new(AgentType::Pi, ORCH_PANE);
    let mut state = AppState::default();
    pi.register_role(
        &mut state,
        "orchestrator",
        true,
        orchestration.clone(),
        &cwd_str,
    );
    state
        .pane_role_map
        .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
    state
        .pane_orchestration_map
        .insert(WORKER_PANE.to_string(), orchestration);
    state
        .pane_cwd_map
        .insert(WORKER_PANE.to_string(), cwd_str.clone());

    let signal = pi.delegate("List the files in the current directory.", &[WORKER_ROLE]);
    state.handle_delegate(signal, &registry, &event_tx).await;

    let snap = wait_for_needle(&registry, &worker_agent_id, POINTER, Duration::from_secs(5)).await;
    assert!(
        snap.windows(POINTER.len()).any(|w| w == POINTER),
        "Pi orchestrator's delegate never reached the coder worker pane; snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );

    registry.shutdown_all();
}

/// Scenario: A Pi-identity WORKER (role `worker`, NOT the `start = true`
/// role) calls `delegate --to coder`; the daemon's `handle_delegate` rejects
/// it — the pane-role guard admits only the orchestrator. Set up an
/// orchestration where an orchestrator delegate WOULD deliver (a `coder`
/// worker `cat` stub in the same orchestration), then send the delegate from
/// the Pi worker instead and assert the `coder` stub receives NOTHING. Same
/// guard `orchestration/delegate/004` proves for claude/opencode, now for a
/// Pi identity.
#[spec("orchestration/delegate/006")]
#[test]
fn delegate_006_pi_worker_delegate_is_rejected_by_role_guard() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime")
        .block_on(delegate_006_pi_worker_delegate_is_rejected_by_role_guard_inner());
}

async fn delegate_006_pi_worker_delegate_is_rejected_by_role_guard_inner() {
    common::init_test_env();

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("tempdir path is UTF-8")
        .to_string();

    let registry = Arc::new(AgentPtyRegistry::new());
    let worker_agent_id = spawn_worker_stub(&registry, &cwd_str, WORKER_PANE);

    let (event_tx, _rx) = broadcast::channel::<BroadcastMsg>(64);
    let orchestration = ("pi-orchestration".to_string(), cwd_str.clone());

    // The Pi agent is a WORKER — registered in the role map but deliberately
    // NOT in orchestrator_pane_ids, so it is not the `start = true` role.
    let pi_worker = SyntheticAgent::new(AgentType::Pi, "pi-worker-pane");
    let mut state = AppState::default();
    pi_worker.register_role(&mut state, "worker", false, orchestration.clone(), &cwd_str);
    // The coder target exists and shares the orchestration — so an
    // ORCHESTRATOR's delegate would deliver; only the guard stops this one.
    state
        .pane_role_map
        .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
    state
        .pane_orchestration_map
        .insert(WORKER_PANE.to_string(), orchestration);
    state
        .pane_cwd_map
        .insert(WORKER_PANE.to_string(), cwd_str.clone());

    let signal = pi_worker.delegate("Escalate: do the orchestrator's job.", &[WORKER_ROLE]);
    state.handle_delegate(signal, &registry, &event_tx).await;

    // handle_delegate rejects a non-orchestrator sender synchronously (before
    // spawning any dispatch task), so a bounded grace window with the pointer
    // still absent is a strong "never delivered" signal.
    let snap = wait_for_needle(
        &registry,
        &worker_agent_id,
        POINTER,
        Duration::from_millis(1500),
    )
    .await;
    assert!(
        !snap.windows(POINTER.len()).any(|w| w == POINTER),
        "a Pi WORKER's delegate must be rejected by the role guard, but the coder pane \
         received the task; snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );

    registry.shutdown_all();
}
