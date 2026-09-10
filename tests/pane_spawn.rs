//! Issue #868: `pane spawn <role>` — a CLI-triggerable daemon verb that
//! spawns a role that is declared in `.dot-agent-deck.toml` but was never
//! spawned into this running orchestration instance (e.g. the operator added
//! a role to the config mid-session; today only a full tab restart picks up
//! a new role).
//!
//! Rides the same unversioned hook-socket `DaemonMessage` channel
//! `Delegate`/`RestartRole` already use. Unlike M2's
//! `handle_restart_role_with_state` (resolves under a short read guard, then
//! defers its one write-lock need into a detached task), EVERY successful
//! spawn here needs `register_orchestration_role` on the main path —
//! deferring it would let the response claim `spawned: true` before
//! `delegate` could actually reach the role. So `handle_spawn_role_with_state`
//! is a plain async free function taking the `SharedState` handle directly
//! and managing its own short-lived lock acquisitions, not an `AppState`
//! method.
//!
//! `pane/spawn/009` and `pane/spawn/010` (the `TabManager`-level dead-slot
//! replacement and cross-instance-isolation coverage) live in
//! `tests/orchestration_tab_growth.rs` instead, since they exercise
//! `TabManager::add_role_to_existing_orchestration`/`orchestration_tab_index_for`
//! directly with no daemon involved.

#![cfg(unix)]

use std::time::Duration;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership,
};
use dot_agent_deck::event::{SpawnRoleResponse, SpawnRoleSignal};
use dot_agent_deck::state::{OrchestrationIdentity, handle_spawn_role_with_state};
use spec::spec;

mod common;

const ORCH_PANE: &str = "spawn-orchestrator";
const CODER_PANE: &str = "spawn-coder";
const CODER_ROLE: &str = "coder";
const REVIEWER_ROLE: &str = "reviewer";
const UNKNOWN_ROLE: &str = "no-such-role";
const ORCHESTRATION: &str = "spawn-orchestration";
const ORCHESTRATION_ID: &str = "spawn-instance-1";

fn config() -> String {
    format!(
        "[[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n\
         [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
         [[orchestrations.roles]]\nname = \"{CODER_ROLE}\"\ncommand = \"cat\"\n\n\
         [[orchestrations.roles]]\nname = \"{REVIEWER_ROLE}\"\ncommand = \"cat\"\n"
    )
}

fn membership(role_index: usize, role_name: &str, is_start_role: bool, cwd: &str) -> TabMembership {
    TabMembership::Orchestration {
        name: ORCHESTRATION.to_string(),
        role_index,
        role_name: role_name.to_string(),
        is_start_role,
        orchestration_cwd: Some(cwd.to_string()),
        display_title: None,
        orchestration_id: Some(ORCHESTRATION_ID.to_string()),
    }
}

struct Fixture {
    daemon: common::InProcDaemon,
    _dir: tempfile::TempDir,
    coder_agent_id: String,
}

/// Spawn an orchestrator + a `coder` worker running `coder_command`,
/// register both roles. The `.dot-agent-deck.toml` also declares a third
/// role, `reviewer`, that is deliberately never spawned or registered — the
/// "configured but unspawned role" M3 targets. `coder_command` mirrors
/// `tests/pane_restart.rs`'s own `fixture()` parameter: `"cat"` keeps the
/// coder healthy for the life of the test, a short-lived command (e.g.
/// `"sleep 0.2"`) lets it exit on its own so M1's naturally-exited-worker
/// marker (`crashed == Some(true)`) fires while its role registration is
/// left untouched — the precondition the crashed-vs-live refusal message
/// targets.
async fn fixture(coder_command: &str) -> Fixture {
    let daemon = common::spawn_inprocess_daemon().await;
    let dir = common::race_safe_tempdir();
    std::fs::write(dir.path().join(".dot-agent-deck.toml"), config())
        .expect("write orchestration config");
    let cwd = dir.path().to_string_lossy().into_owned();

    daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd),
            display_name: Some("orchestrator"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
            tab_membership: Some(membership(0, "orchestrator", true, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn orchestrator stand-in");
    let coder_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(coder_command),
            cwd: Some(&cwd),
            display_name: Some(CODER_ROLE),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), CODER_PANE.to_string())],
            tab_membership: Some(membership(1, CODER_ROLE, false, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn coder stand-in");

    {
        let mut state = daemon.state.write().await;
        let identity = OrchestrationIdentity::Instance {
            id: ORCHESTRATION_ID.to_string(),
            name: ORCHESTRATION.to_string(),
        };
        state.register_orchestration_role(
            ORCH_PANE,
            "orchestrator",
            true,
            identity.clone(),
            Some(&cwd),
        );
        state.register_orchestration_role(CODER_PANE, CODER_ROLE, false, identity, Some(&cwd));
    }

    Fixture {
        daemon,
        _dir: dir,
        coder_agent_id,
    }
}

/// Call `dot-agent-deck pane spawn <role>`'s daemon-side handler directly,
/// the same way `tests/pane_restart.rs`'s `restart_role()` helper calls
/// `handle_restart_role_with_state` — bypassing the CLI/socket layer, which
/// is mechanical wiring already covered by `main.rs`'s other CLI arms.
///
/// Unlike `restart_role()`, this does NOT go through a pre-held read guard:
/// `handle_spawn_role_with_state` is a free function that takes the
/// `SharedState` handle itself, because it must be free to take its own
/// short-lived write lock on the main path (see module docs).
async fn spawn_role(fx: &Fixture, caller_pane_id: &str, role: &str) -> SpawnRoleResponse {
    let signal = SpawnRoleSignal {
        pane_id: caller_pane_id.to_string(),
        role: role.to_string(),
        timestamp: chrono::Utc::now(),
    };
    handle_spawn_role_with_state(
        signal,
        &fx.daemon.state,
        &fx.daemon.registry,
        &fx.daemon.event_tx,
    )
    .await
}

/// Poll until `agent_id`'s registry record is `crashed == Some(true)`, the
/// same bounded-deadline idiom `tests/pane_restart.rs`'s own waiter uses.
async fn wait_for_crashed(registry: &AgentPtyRegistry, agent_id: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if registry
            .agent_record_any(agent_id)
            .is_some_and(|r| r.crashed == Some(true))
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Scenario: a role declared in `.dot-agent-deck.toml` (`reviewer`) was never
/// spawned into this orchestration instance. The orchestrator pane asks the
/// daemon to spawn it. The response must report success with no error, and
/// the role must actually be reachable afterward: `delegate_targets` from
/// the orchestrator resolved nothing for `reviewer` before the call, and
/// resolves exactly one pane for it after.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/spawn/001")]
async fn pane_spawn_001_spawns_a_configured_but_unspawned_role_and_it_becomes_reachable() {
    let fx = fixture("cat").await;

    let before = fx
        .daemon
        .state
        .read()
        .await
        .delegate_targets(ORCH_PANE, &[REVIEWER_ROLE.to_string()]);
    assert!(
        before.is_empty(),
        "precondition: reviewer must not be reachable before it is spawned; targets = {before:?}"
    );

    let response = spawn_role(&fx, ORCH_PANE, REVIEWER_ROLE).await;
    assert!(
        response.spawned,
        "spawning a configured-but-unspawned role must succeed; response = {response:?}"
    );
    assert!(
        response.error.is_none(),
        "a successful spawn must carry no error; response = {response:?}"
    );

    let after = fx
        .daemon
        .state
        .read()
        .await
        .delegate_targets(ORCH_PANE, &[REVIEWER_ROLE.to_string()]);
    assert_eq!(
        after.len(),
        1,
        "after spawning, delegate_targets must resolve exactly one pane for reviewer; \
         targets = {after:?}"
    );
    assert_eq!(
        after[0].0, REVIEWER_ROLE,
        "the resolved target must be for the reviewer role; targets = {after:?}"
    );
    let reviewer_pane_id = after[0].1.clone();
    assert!(
        fx.daemon
            .registry
            .pane_current_agent_id(&reviewer_pane_id)
            .is_some(),
        "the pane delegate_targets names for reviewer must have a live agent; pane = {reviewer_pane_id}"
    );
}

/// Scenario: the caller asks to spawn `coder`, a role that is already live
/// in this orchestration instance. The handler must refuse rather than spawn
/// a second agent for a role that already has one: `spawned` is false, an
/// error names that the role is already running, and nothing about the
/// existing coder pane/agent changes.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/spawn/002")]
async fn pane_spawn_002_refuses_a_role_already_live_in_this_instance() {
    let fx = fixture("cat").await;

    let records_before = fx.daemon.registry.agent_records().len();

    let response = spawn_role(&fx, ORCH_PANE, CODER_ROLE).await;
    assert!(
        !response.spawned,
        "spawning a role already live in this instance must be refused; response = {response:?}"
    );
    assert!(
        response.error.as_deref().is_some_and(
            |e| e.to_lowercase().contains("already") && e.to_lowercase().contains(CODER_ROLE)
        ),
        "the refusal must explain the role is already running; response = {response:?}"
    );

    let agent_id = fx
        .daemon
        .registry
        .pane_current_agent_id(CODER_PANE)
        .expect("coder's pane must still have its original agent");
    assert_eq!(
        agent_id, fx.coder_agent_id,
        "a refused spawn must not touch the already-live coder's agent"
    );
    assert_eq!(
        fx.daemon.registry.agent_records().len(),
        records_before,
        "a refused spawn must not create any new agent record"
    );
}

/// Scenario: the caller asks to spawn a role name that appears nowhere in
/// `.dot-agent-deck.toml`. The handler must refuse and name the unknown role
/// rather than silently doing nothing.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/spawn/003")]
async fn pane_spawn_003_refuses_a_role_not_in_config() {
    let fx = fixture("cat").await;

    let response = spawn_role(&fx, ORCH_PANE, UNKNOWN_ROLE).await;
    assert!(
        !response.spawned,
        "spawning an unconfigured role must be refused; response = {response:?}"
    );
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains(UNKNOWN_ROLE)),
        "the refusal must name the unknown role; response = {response:?}"
    );
}

/// Scenario: the caller is the CODER's own pane, not the orchestrator.
/// Mirrors `pane_restart_005`'s anti-spoofing shape — only the orchestrator
/// pane of an orchestration may trigger a spawn within it.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/spawn/004")]
async fn pane_spawn_004_refuses_from_a_non_orchestrator_pane() {
    let fx = fixture("cat").await;

    let response = spawn_role(&fx, CODER_PANE, REVIEWER_ROLE).await;
    assert!(
        !response.spawned,
        "a spawn requested by a non-orchestrator pane must be refused; response = {response:?}"
    );
    assert!(
        response.error.is_some(),
        "the refusal must carry an error explaining the caller is not this orchestration's \
         orchestrator; response = {response:?}"
    );

    let after = fx
        .daemon
        .state
        .read()
        .await
        .delegate_targets(ORCH_PANE, &[REVIEWER_ROLE.to_string()]);
    assert!(
        after.is_empty(),
        "a refused spawn must not make reviewer reachable; targets = {after:?}"
    );
}

/// Scenario: `handle_spawn_role_with_state`'s "already live" check reuses
/// `delegate_targets`, which deliberately EXCLUDES orchestrator panes (so
/// restart can refuse `pane restart <start-role>`) — reused here it means
/// naming the orchestration's own start role always resolves empty, so an
/// "already running" refusal built on that oracle alone would never fire.
/// The orchestrator pane asks to spawn its OWN role. The handler must refuse
/// explicitly rather than launching a second orchestrator-command pane
/// registered as a worker.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/spawn/007")]
async fn pane_spawn_007_refuses_to_spawn_its_own_start_role() {
    let fx = fixture("cat").await;
    let records_before = fx.daemon.registry.agent_records().len();

    let response = spawn_role(&fx, ORCH_PANE, "orchestrator").await;

    assert!(
        !response.spawned,
        "spawning the orchestration's own start role must be refused; response = {response:?}"
    );
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains("orchestrator")
                && (e.to_lowercase().contains("start") || e.to_lowercase().contains("own"))),
        "the refusal must explain the named role is the orchestration's own start role; \
         response = {response:?}"
    );
    assert_eq!(
        fx.daemon.registry.agent_records().len(),
        records_before,
        "a refused spawn of the start role must not create a second orchestrator-command agent"
    );
}

/// Scenario: `handle_spawn_role_with_state` resolves its "already live"
/// check under a READ guard that is dropped BEFORE `registry.spawn_agent`,
/// then re-takes a fresh WRITE guard only to register — nothing serializes
/// two concurrent spawns of the SAME not-yet-live role in that window unless
/// the re-check under the write guard closes it. Fire two concurrent spawns
/// of `reviewer` at once and assert exactly ONE live pane ends up registered
/// for `reviewer` once both calls resolve — never two.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/spawn/008")]
async fn pane_spawn_008_concurrent_spawns_of_the_same_role_never_leave_two_live_panes() {
    let fx = fixture("cat").await;

    let signal_a = SpawnRoleSignal {
        pane_id: ORCH_PANE.to_string(),
        role: REVIEWER_ROLE.to_string(),
        timestamp: chrono::Utc::now(),
    };
    let signal_b = signal_a.clone();

    // `tokio::spawn` onto the multi-thread runtime's own worker threads,
    // NOT `tokio::join!` on two unspawned futures polled on this one task —
    // `join!` only interleaves at `.await` points on a single task, and
    // `handle_spawn_role_with_state`'s "already live" check → `spawn_agent`
    // window is a plain synchronous span with no `.await` inside it, so a
    // same-task `join!` could never actually overlap the two calls there.
    // Real concurrent OS threads are what actually races the TOCTOU window.
    let state_a = fx.daemon.state.clone();
    let registry_a = fx.daemon.registry.clone();
    let event_tx_a = fx.daemon.event_tx.clone();
    let handle_a = tokio::spawn(async move {
        handle_spawn_role_with_state(signal_a, &state_a, &registry_a, &event_tx_a).await
    });
    let state_b = fx.daemon.state.clone();
    let registry_b = fx.daemon.registry.clone();
    let event_tx_b = fx.daemon.event_tx.clone();
    let handle_b = tokio::spawn(async move {
        handle_spawn_role_with_state(signal_b, &state_b, &registry_b, &event_tx_b).await
    });
    let (response_a, response_b) =
        tokio::try_join!(handle_a, handle_b).expect("neither concurrent spawn task should panic");

    let targets = fx
        .daemon
        .state
        .read()
        .await
        .delegate_targets(ORCH_PANE, &[REVIEWER_ROLE.to_string()]);
    assert_eq!(
        targets.len(),
        1,
        "two concurrent `pane spawn reviewer` calls must never leave more than one live pane \
         registered for the role — response_a = {response_a:?}, response_b = {response_b:?}, \
         targets = {targets:?}"
    );
}

/// Scenario: the `coder` worker exits on its own shortly after boot (M1's
/// naturally-exited-worker marker fires, `crashed == Some(true)`) while its
/// role registration is left in place — the "already live" oracle still
/// resolves it. The orchestrator asks to `pane spawn coder`. The refusal
/// must name the crashed role AND point the operator at `pane restart` as
/// the remedy, not the flat "already running" wording that fires for a
/// genuinely healthy pane.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/spawn/011")]
async fn pane_spawn_011_refuses_a_crashed_roles_spawn_with_a_pane_restart_pointer() {
    let fx = fixture("sleep 0.2").await;

    let crashed = wait_for_crashed(
        &fx.daemon.registry,
        &fx.coder_agent_id,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        crashed,
        "precondition: the coder stand-in never got marked crashed; record = {:?}",
        fx.daemon.registry.agent_record_any(&fx.coder_agent_id)
    );

    let response = spawn_role(&fx, ORCH_PANE, CODER_ROLE).await;

    assert!(
        !response.spawned,
        "spawning an already-registered-but-crashed role must be refused; response = {response:?}"
    );
    let error = response
        .error
        .as_deref()
        .expect("a refused spawn must carry an error message");
    assert!(
        error.contains(CODER_ROLE),
        "the refusal must name the crashed role; error = {error:?}"
    );
    assert!(
        error.contains("pane restart"),
        "the refusal must point the operator at `pane restart` as the remedy, not the flat \
         'already running' wording; error = {error:?}"
    );
}
