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

/// The file the retried delegate's task tells the real worker to create.
const REQUEST_SENTINEL: &str = "config_trust_request_live_9d27a4.txt";

/// Issue #2330: a stand-in coordinator and a REAL interactive Claude Code
/// worker on Haiku, with the worker's description set to `description` (an
/// approved field, so changing it revokes the approval).
fn write_request_live_config(dir: &std::path::Path, description: &str) {
    std::fs::write(
        dir.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\n\
             name = \"trust-request-live\"\n\n\
             [[orchestrations.roles]]\n\
             name = \"orchestrator\"\n\
             command = \"cat\"\n\
             start = true\n\
             description = \"Stand-in coordinator\"\n\n\
             [[orchestrations.roles]]\n\
             name = \"writer\"\n\
             command = \"{COORDINATOR_COMMAND}\"\n\
             description = \"{description}\"\n\
             prompt_template = \"You are the writer. Do every task you are given yourself with the Bash tool, then reply DONE. Do not ask what to do or wait for further instructions.\"\n"
        ),
    )
    .expect("write the request-live config");
}

/// The daemon's record of orchestration role `role`.
fn role_record(
    attach: &std::path::Path,
    role: &str,
) -> Option<dot_agent_deck::agent_pty::AgentRecord> {
    common::agent_records_on(attach).into_iter().find(|r| {
        matches!(
            &r.tab_membership,
            Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
        )
    })
}

/// Run the real deck CLI `args` from `caller_pane`'s identity, as that pane's
/// agent would.
fn agent_verb(deck: &TuiDeck, caller_pane: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new(common::contained_deck_binary())
        .args(args)
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", caller_pane)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run the agent verb")
}

/// Scenario: Open an approved orchestration whose worker is a real interactive
/// Claude Code on Haiku, then change the worker's description on disk. The
/// coordinator's `delegate` to the worker is refused, saying the approval was
/// requested in the deck, and the TUI opens `Review project commands` on its
/// own with `Asked by: delegate`. Pressing `a` approves it; the retried
/// delegate is accepted and the real worker creates the uniquely named
/// sentinel file the task asks for.
#[spec("config-trust/live/002")]
#[test]
fn live_002_a_refused_delegate_is_approved_in_the_deck_and_the_retry_reaches_a_real_agent() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(160, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        // Outlast the agent wait below (linkage-check rule 11 caps this).
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "720")
        .launch_with_fixture("config-trust-live");
    deck.wait_for_string("No active agents");
    write_request_live_config(deck.workdir(), "Writes files V1");
    deck.approve_project_trust(deck.workdir());
    let attach = deck.attach_socket_path().to_path_buf();

    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> New Agent form
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // Right -> [Orch: trust-request-live]
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // submit
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(30)), || {
            role_record(&attach, "orchestrator").is_some()
                && role_record(&attach, "writer").is_some()
        }),
        "precondition: the approved orchestration opens with no prompt.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    let orchestrator = role_record(&attach, "orchestrator")
        .and_then(|r| r.pane_id_env)
        .expect("the orchestrator has a pane id");

    write_request_live_config(deck.workdir(), "Writes files V2");
    let task = format!(
        "Use the Bash tool to run `touch {REQUEST_SENTINEL}` in the current directory, then reply DONE."
    );
    let delegate = || {
        agent_verb(
            &deck,
            &orchestrator,
            &["delegate", "--to", "writer", "--task", &task],
        )
    };
    let refused = delegate();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !refused.status.success() && said.contains("requested in the deck"),
        "the delegate must be refused, saying the approval was requested in the deck: {said}"
    );
    deck.wait_until_grid("the review the deck opened on its own", |grid| {
        [
            "Review project commands",
            "Asked by: delegate",
            "Writes files V2",
        ]
        .iter()
        .all(|needle| grid.contains(needle))
    });
    deck.wait_for_string("a approve · Esc deny");
    deck.wait_until_grid_then_hold(
        "the deck's review, while its keys arm",
        dot_agent_deck::trust_prompt::REQUEST_KEYS_ARM + Duration::from_millis(300),
        |grid| grid.contains("a approve · Esc deny"),
    );
    deck.send_keys(b"a");
    deck.wait_for_string("Approved orchestration 'trust-request-live'");

    let accepted = delegate();
    assert!(
        accepted.status.success(),
        "the retried delegate is accepted: {}{}",
        String::from_utf8_lossy(&accepted.stdout),
        String::from_utf8_lossy(&accepted.stderr)
    );
    let sentinel = deck.workdir().join(REQUEST_SENTINEL);
    assert!(
        common::wait_for_path(&sentinel, Duration::from_secs(300)),
        "the real worker never created {REQUEST_SENTINEL}.\nGrid:\n{}",
        deck.snapshot_grid()
    );
}
