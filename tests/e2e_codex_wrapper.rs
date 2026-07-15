#![cfg(feature = "e2e")]

//! PTY-attached Codex wrapper coverage for PRD #20 M7. The synthetic case pins
//! deterministic plumbing; the real case runs a cheap Codex model against a
//! uniquely named fixture sentinel. Both assert the user-visible dashboard.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AGENT_EVENT_SCHEMA_VERSION, AgentType, EventType};
use spec::spec;

const SENTINEL_NAME: &str = "codex_sentinel_a7c91f.txt";
const LAST_MESSAGE_NAME: &str = "codex-last-message.txt";

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = std::path::Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Scenario: Restore a pane running `dot-agent-deck wrap --agent codex` around
/// a deterministic shell stand-in that emits realistic Codex JSONL turn-start
/// and turn-completed records. Subscribe to the real daemon event stream and
/// detach to the dashboard; events must carry the Codex identity and schema
/// version while the visible card moves Thinking → Idle and reads `Codex`.
#[spec("codex/wrap/001")]
#[test]
fn codex_wrap_001_synthetic_jsonl_reaches_dashboard() {
    let command = "dot-agent-deck wrap --agent codex -- /bin/sh codex-standin.sh";
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_continue_session("", command)
        .launch_with_fixture("codex-synthetic");

    deck.wait_for_string("[Command Mode Ctrl+D]");
    let events = deck.subscribe_events();
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");

    let working = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Thinking,
        Duration::from_secs(15),
    );
    assert_eq!(working.schema_version, Some(AGENT_EVENT_SCHEMA_VERSION));
    assert_eq!(working.agent_type, AgentType::Codex);
    assert!(
        deck.wait_for_stream_string_within("Thinking", Duration::from_secs(10)),
        "the wrapped Codex card never visibly entered Thinking:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(10)),
        "the live dashboard card did not show the Codex identity:\n{}",
        deck.snapshot_grid()
    );

    let idle = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Idle,
        Duration::from_secs(15),
    );
    assert_eq!(idle.schema_version, Some(AGENT_EVENT_SCHEMA_VERSION));
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(10)),
        "the wrapped Codex card never visibly completed its turn:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: With Codex authentication proven and copied into an isolated HOME,
/// restore a PTY pane running real `codex exec` on the cheap mini model under
/// `dot-agent-deck wrap`. Codex must list the fixture directory and report the
/// unique sentinel in its persisted final response while the visible dashboard
/// shows a live Codex card transition from Thinking to Idle.
#[spec("codex/live/001")]
#[test]
fn codex_live_001_real_model_lists_sentinel_in_wrapped_pane() {
    skip_unless!(common::check_codex_available());

    let prompt = format!(
        "Use the shell tool to list the files in the current directory. Then finish with one short sentence that includes the exact filename {SENTINEL_NAME}. Do not create, edit, or delete files."
    );
    let command = format!(
        "dot-agent-deck wrap --agent codex -- codex exec --ephemeral --ignore-user-config \
         --skip-git-repo-check --sandbox read-only --model {} \
         -c 'model_reasoning_effort=\"low\"' --json \
         --output-last-message {LAST_MESSAGE_NAME} '{prompt}'",
        common::CODEX_TEST_MODEL,
    );
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_imported_codex_credentials()
        .with_continue_session("", command)
        .launch_with_fixture("codex-live");

    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");

    assert!(
        deck.wait_for_stream_string_within("Thinking", Duration::from_secs(120)),
        "the real wrapped Codex run never visibly entered Thinking:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(30)),
        "the real wrapped session never rendered a Codex card:\n{}",
        deck.snapshot_grid()
    );

    let last_message = deck.workdir().join(LAST_MESSAGE_NAME);
    assert!(
        common::wait_for_path(&last_message, Duration::from_secs(180)),
        "real Codex never produced its final-message file; final grid:\n{}",
        deck.snapshot_grid()
    );
    let response = std::fs::read_to_string(&last_message).expect("read Codex final response");
    assert!(
        response.contains(SENTINEL_NAME),
        "Codex reached the model but did not report the fixture sentinel; response: {response:?}"
    );
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(30)),
        "the real wrapped Codex card never visibly returned to Idle:\n{}",
        deck.snapshot_grid()
    );
}
