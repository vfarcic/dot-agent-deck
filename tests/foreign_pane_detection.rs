//! `has_live_pane` — the predicate behind the daemon's foreign-agent warning.
//!
//! A `SessionStart` naming a pane the daemon never spawned registers a card that
//! no local pane backs: it surfaces on the dashboard and is retired again, which
//! is the "ghost agent appeared and disappeared" report. The usual cause is
//! another deck's agent posting here — most often a test child that inherited an
//! ambient `DOT_AGENT_DECK_SOCKET`. The daemon cannot refuse such events (a pane
//! may legitimately belong to a client whose agent it does not own), so it warns,
//! and this predicate is what decides.
//!
//! Fast tier: a `cat` stub, no LLM and no daemon socket.

use std::sync::Arc;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};

dot_agent_deck_test_harness::link!();

/// Scenario: Spawn one `cat`-backed agent carrying a known pane id, then ask the
/// registry about that pane, about a pane id no agent was spawned with (the
/// fixture-shaped ids seen leaking from test runs), and about an empty id.
/// Only the spawned pane is reported live.
#[test]
fn has_live_pane_distinguishes_own_panes_from_foreign_ones() {
    common::init_test_env();

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("tempdir path is UTF-8")
        .to_string();

    let registry = Arc::new(AgentPtyRegistry::new());
    registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(cwd_str.as_str()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "own-pane".to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn stub");

    assert!(
        registry.has_live_pane("own-pane"),
        "a pane this registry spawned must be reported live"
    );

    // The shapes actually observed arriving at a live daemon from test runs.
    for foreign in [
        "worker-pane",
        "codex-trust-test-pane",
        "pane-live-transition",
    ] {
        assert!(
            !registry.has_live_pane(foreign),
            "{foreign} was never spawned here and must not be reported live — \
             the daemon would then stay silent about a foreign agent"
        );
    }

    assert!(
        !registry.has_live_pane(""),
        "an empty pane id is not a live pane"
    );

    registry.shutdown_all();
}

/// Scenario: Spawn a `cat` stub, shut the registry down so its agent is no
/// longer live, and confirm the pane stops being reported as live — otherwise a
/// stale entry would suppress the warning for a genuinely foreign event reusing
/// that pane id.
#[test]
fn has_live_pane_excludes_agents_that_have_exited() {
    common::init_test_env();

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("tempdir path is UTF-8")
        .to_string();

    let registry = Arc::new(AgentPtyRegistry::new());
    registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(cwd_str.as_str()),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "doomed-pane".to_string(),
            )],
            ..SpawnOptions::default()
        })
        .expect("spawn stub");
    assert!(registry.has_live_pane("doomed-pane"));

    registry.shutdown_all();

    // `shutdown_all` removes the entries outright, so the pane is gone either
    // way — the point is that it is NOT still reported live.
    assert!(
        !registry.has_live_pane("doomed-pane"),
        "a torn-down pane must not keep suppressing the foreign-agent warning"
    );
}
