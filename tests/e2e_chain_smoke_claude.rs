#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! Real interactive Claude chain smoke through the native inbox and live card.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

const PINNED_MODEL: &str = "claude-haiku-4-5-20251001";
const SENTINEL: &str = "claude_chain_sentinel_8d24.txt";
const PROOF: &str = "claude_chain_proof_8d24.txt";
const BUDGET: Duration = Duration::from_secs(120);

/// Scenario: Restore an interactive Claude Haiku pane with isolated credentials
/// and pre-seeded project trust, then post one inbox task asking it to copy a
/// uniquely named sentinel through Bash. On the dashboard, require Thinking →
/// Working with Bash → Idle, the exact proof file, and a still-live Claude pane.
#[spec("chain-smoke/claude/001")]
#[test]
fn claude_001_thinking_working_idle() {
    skip_unless!(common::check_claude_available());
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_continue_session(
            "claude-smoke",
            format!("claude --model {PINNED_MODEL} --allowedTools Bash Read Write"),
        )
        .launch_with_fixture("chain-smoke-claude");
    std::fs::write(deck.workdir().join(SENTINEL), "CLAUDE_CHAIN_OK\n")
        .expect("write isolated sentinel");
    deck.wait_for_string("Claude Code v");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    deck.wait_for_string("claude-smoke");

    let agents = common::attach_request_on(deck.attach_socket_path(), &AttachRequest::ListAgents)
        .expect("list the sandbox's agents")
        .agent_records
        .expect("agent records");
    let agent = agents
        .iter()
        .find(|agent| agent.display_name.as_deref() == Some("claude-smoke"))
        .expect("restored interactive Claude");
    let pane = agent.pane_id_env.as_deref().expect("managed pane identity");
    let start = deck.stream_text().len();
    let directive = format!(
        "Use Bash exactly once to run: sleep 3; cat '{}/{SENTINEL}' > '{}/{PROOF}'. \
         Do not modify any other file. Then reply CLAUDE_CHAIN_DONE and end your turn.",
        deck.workdir().display(),
        deck.workdir().display(),
    );
    common::post_message_on(deck.attach_socket_path(), pane, &agent.id, &directive)
        .expect("post the sentinel task through Claude's inbox");

    // Inspect bytes painted after the post boundary, so an initial Idle card
    // cannot count as completion. Keeping Bash alive for three seconds makes
    // its Working frame visible without a second model turn.
    assert!(
        common::wait_until(BUDGET, || {
            let stream = deck.stream_text();
            let turn = stream.get(start..).unwrap_or_default();
            common::match_needles_in_order(
                turn.as_bytes(),
                &["Thinking", "Working", "Bash", "Idle"],
            ) == 4
        }),
        "the interactive Claude card did not complete Thinking → Working/Bash → Idle:\n{}",
        deck.snapshot_grid()
    );
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        "CLAUDE_CHAIN_OK",
        Duration::from_secs(10),
    )
    .expect("real Claude must copy the fixture sentinel exactly");
    assert!(
        deck.snapshot_grid().contains("Idle"),
        "{}",
        deck.snapshot_grid()
    );
    let agents = common::attach_request_on(deck.attach_socket_path(), &AttachRequest::ListAgents)
        .expect("list surviving interactive agent")
        .agent_records
        .expect("agent records");
    assert!(agents.iter().any(|record| record.id == agent.id));
}
