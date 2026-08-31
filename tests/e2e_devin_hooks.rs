#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! PTY-attached real-Devin native-hook coverage.
//!
//! Devin is a `NativeHooks` agent, so nothing about it is wrapped or synthesized
//! at runtime: the deck writes hook definitions into Devin's own config, and
//! then Devin — a third-party binary this project does not control — decides
//! whether to run them. Every cheaper tier stops short of proving that. The unit
//! tests prove the deck writes the JSON it intended; `devin_hook_ingestion`
//! proves the deck parses payloads of the documented shape. Neither can tell you
//! that Devin reads that file, accepts that shape, or ever invokes the command —
//! and getting any of those wrong fails SILENTLY, with a green suite and a card
//! that simply never updates.
//!
//! So this is the test that runs the real CLI against a deck-written config and
//! watches the dashboard.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AgentType, EventType};
use spec::spec;

/// A fixture-unique filename the agent must read back, so the assertion survives
/// LLM phrasing variance: any sensible model listing this directory echoes this
/// exact string, and no other test's output contains it.
const SENTINEL_NAME: &str = "devin_live_sentinel_4c81de.txt";

/// Scenario: Launch the deck with a real interactive Devin running in a pane,
/// type a directive asking it to list the working directory and name a sentinel
/// file, then leave the pane. The dashboard card must show live status driven by
/// Devin's OWN deck-installed hooks — Thinking on submit, an `exec` tool event,
/// Idle when the turn ends — and the pane must show the sentinel filename.
#[spec("devin/live/001")]
#[test]
#[cfg(unix)]
fn devin_live_001_real_interactive_turn_drives_the_card_live() {
    skip_unless!(common::check_devin_available());

    // `--respect-workspace-trust false` is mandatory, not defensive: Devin
    // refuses to start in an untrusted directory and the per-test fixture dir is
    // always untrusted. `--permission-mode auto` auto-approves the read-only
    // `exec` listing so no permission prompt swallows the turn.
    //
    // No `--model` is pinned ON PURPOSE. Devin rejects every explicit model with
    // "/upgrade to access this model" on a free account, so pinning one would
    // make this test pass only on paid plans. The account default is used
    // instead, which is the cheap SWE family.
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_imported_devin_credentials()
        .with_continue_session(
            "devin-live",
            "devin --respect-workspace-trust false --permission-mode auto",
        )
        .launch_with_fixture("minimal");

    std::fs::write(deck.workdir().join(SENTINEL_NAME), "DEVIN_LIVE_OK\n")
        .expect("write the sentinel the agent must read back");

    deck.wait_for_string("[Command Mode Ctrl+D]");

    // Wait for Devin's OWN TUI to finish booting before typing. The deck's
    // command-mode footer appears as soon as the pane exists, which is well
    // before Devin can accept input — keystrokes sent in that window are simply
    // dropped and the turn never starts.
    assert!(
        deck.wait_for_grid_string_within("Ask Devin", Duration::from_secs(120)),
        "Devin's interactive UI never became ready in the pane:\n{}",
        deck.snapshot_grid()
    );

    // Subscribe only once the daemon is up — its attach socket does not exist
    // before that. Devin's SessionStart has therefore already fired by now, so
    // this test asserts on the turn events instead; Thinking alone is proof the
    // deck-written hook config was read AND executed by the real binary.
    let events = deck.subscribe_events();

    deck.send_keys(
        b"List the files in this directory with the exec tool, then reply with the exact \
          filename that starts with devin_live_sentinel." as &[u8],
    );
    // Confirm the text actually landed in Devin's input box before submitting,
    // rather than pressing Enter on an empty prompt.
    assert!(
        deck.wait_for_grid_string_within("devin_live_sentinel", Duration::from_secs(30)),
        "the directive never appeared in Devin's input box:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"\r");

    // Leave the pane so the dashboard card is on screen for the status asserts.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");

    // Status is driven entirely by Devin's hooks: UserPromptSubmit -> Thinking.
    assert!(
        deck.wait_for_grid_string_within("Thinking", Duration::from_secs(60)),
        "the card never showed Thinking, so Devin did not run the deck-installed \
         UserPromptSubmit hook:\n{}",
        deck.snapshot_grid()
    );

    // The real `exec` tool payload exercises hook.rs's `exec` tool-detail arm
    // against Devin's actual shape rather than a synthesized one.
    let tool = events.wait_for(
        |event| event.agent_type == AgentType::Devin && event.event_type == EventType::ToolStart,
        Duration::from_secs(45),
    );
    assert_eq!(
        tool.tool_name.as_deref(),
        Some("exec"),
        "Devin's shell tool should surface as `exec`; event={tool:?}"
    );
    assert!(
        tool.tool_detail.is_some(),
        "a real Devin exec payload must yield a tool detail; event={tool:?}"
    );

    assert!(
        deck.wait_for_grid_string_within(SENTINEL_NAME, Duration::from_secs(60)),
        "the real Devin turn never reported the sentinel file:\n{}",
        deck.snapshot_grid()
    );

    // Stop -> Idle: the turn completes and the card settles, rather than the
    // agent exiting or hanging mid-tool.
    let idle = events.wait_for(
        |event| event.agent_type == AgentType::Devin && event.event_type == EventType::Idle,
        Duration::from_secs(30),
    );
    assert_eq!(idle.agent_type, AgentType::Devin);
}
