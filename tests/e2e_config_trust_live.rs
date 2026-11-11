#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! PRD #1885, lane 2: the trust prompt as a user meets it with a REAL agent.
//! A fresh project whose single role is an interactive Claude Code on Haiku:
//! Ctrl+n shows the role's real command in the `Review project commands`
//! prompt, nothing runs until `a`, and once approved the agent does the work
//! its prompt directs — creating a uniquely named sentinel file.
//!
//! Runs on a developer's machine only (CLAUDE.md rule 5); `config-trust/*` in
//! `e2e_config_trust.rs` covers the same surface with stand-ins in lane 1.

dot_agent_deck_test_harness::link!();

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

/// Kept byte-identical to the fixture's `command`
/// (`tests/fixtures/config-trust-live/.dot-agent-deck.toml`).
const COORDINATOR_COMMAND: &str =
    "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read";

/// The file the fixture's `prompt_template` tells the agent to create.
const SENTINEL: &str = "config_trust_live_5e81c3.txt";

/// Scenario: Launch the deck on a never-approved project whose one role is a
/// real interactive Claude Code on Haiku, press Ctrl+n and choose the
/// orchestration. The `Review project commands` prompt shows the role's
/// `claude …` command and nothing is spawned; pressing `a` starts the agent,
/// which creates the uniquely named sentinel file its prompt asks for.
#[spec("config-trust/live/001")]
#[test]
fn live_001_a_real_agent_starts_only_after_the_trust_prompt_is_approved() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .without_trust_preapproval()
        .with_pty_size(140, 40)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        // Outlast the agent wait below (linkage-check rule 11 caps this).
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "720")
        .launch_with_fixture("config-trust-live");
    deck.wait_for_string("No active agents");
    let attach = deck.attach_socket_path().to_path_buf();
    let roles = || {
        common::agent_records_on(&attach)
            .into_iter()
            .filter(|r| matches!(r.tab_membership, Some(TabMembership::Orchestration { .. })))
            .count()
    };

    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> New Agent form
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // Right -> [Orch: trust-live]
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // submit

    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
            deck.snapshot_grid().contains("Review project commands")
        }),
        "the trust prompt never appeared.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains(COORDINATOR_COMMAND),
        "the prompt must show the real agent's command.\nGrid:\n{grid}"
    );
    assert_eq!(roles(), 0, "nothing runs while the prompt is open");

    deck.send_keys(b"a");
    let sentinel = deck.workdir().join(SENTINEL);
    assert!(
        common::wait_for_path(&sentinel, Duration::from_secs(300)),
        "the approved agent never created {SENTINEL}; roles = {}\nGrid:\n{}",
        roles(),
        deck.snapshot_grid()
    );
}
