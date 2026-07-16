#![cfg(feature = "e2e")]

//! PTY-attached real-Codex native-hook parity coverage for PRD #20 W1.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AgentType, EventType};
use spec::spec;

const HOOK_SENTINEL_NAME: &str = "codex_hooks_sentinel_f42e71.txt";
const HOOK_SENTINEL_CONTENT: &str = "CODEX_HOOKS_OK";

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = std::path::Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Scenario: Launch a real cheap-model Codex through the normal wrapped pane
/// seam, submit a directive that runs one shell command and creates a unique
/// sentinel, then detach without exiting Codex. Native hooks installed in the
/// isolated Codex home must make the dashboard show the prompt, shell tool name
/// and command detail, and finally Idle while the Codex pane remains alive.
#[spec("codex/hooks/001")]
#[test]
fn codex_hooks_001_real_interactive_turn_reaches_idle_without_exit() {
    skip_unless!(common::check_codex_available());

    let prompt = format!(
        "Run this exact command with the shell tool: printf {HOOK_SENTINEL_CONTENT} > {HOOK_SENTINEL_NAME}. Do not use apply_patch. After reporting completion, stay open and wait for another prompt."
    );
    let command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::CODEX_TEST_MODEL,
    );
    let config_dir = tempfile::tempdir().expect("Codex hooks new-pane config");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, format!("default_command = {command:?}\n"))
        .expect("write bare Codex hooks command");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_imported_codex_credentials()
        .launch_with_fixture("codex-live");

    deck.wait_for_string("No active sessions");
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("Tab: switch");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        deck.wait_for_grid_string_within(common::CODEX_TEST_MODEL, Duration::from_secs(30)),
        "the wrapped interactive Codex UI never became ready:\n{}",
        deck.snapshot_grid()
    );

    deck.send_keys(prompt.as_bytes());
    deck.wait_for_string(HOOK_SENTINEL_NAME);
    deck.send_keys(b"\r");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    assert!(
        deck.wait_for_grid_string_within("Thinking", Duration::from_secs(60)),
        "the dashboard card never showed Thinking after Codex prompt submission:\n{}",
        deck.snapshot_grid()
    );

    let prompt_event = events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::Thinking
                && event.user_prompt.as_deref() == Some(prompt.as_str())
        },
        Duration::from_secs(120),
    );
    assert_eq!(prompt_event.agent_type, AgentType::Codex);
    assert!(
        deck.wait_for_grid_string_within(HOOK_SENTINEL_NAME, Duration::from_secs(30)),
        "the Codex UserPromptSubmit detail never appeared on the dashboard card:\n{}",
        deck.snapshot_grid()
    );

    let tool_start = events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
                && event
                    .tool_detail
                    .as_deref()
                    .is_some_and(|value| value.contains(HOOK_SENTINEL_NAME))
        },
        Duration::from_secs(120),
    );
    assert_eq!(tool_start.tool_name.as_deref(), Some("Bash"));
    assert!(
        deck.wait_for_grid_string_within("Bash", Duration::from_secs(30))
            && deck.wait_for_grid_string_within(HOOK_SENTINEL_NAME, Duration::from_secs(30)),
        "the Codex Bash tool name and command detail never appeared on the dashboard card:\n{}",
        deck.snapshot_grid()
    );

    let tool_end = events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::ToolEnd
                && event.tool_name.as_deref() == Some("Bash")
                && event
                    .tool_detail
                    .as_deref()
                    .is_some_and(|value| value.contains(HOOK_SENTINEL_NAME))
        },
        Duration::from_secs(120),
    );
    assert!(
        tool_end
            .tool_detail
            .as_deref()
            .is_some_and(|value| value.contains(HOOK_SENTINEL_NAME)),
        "Codex PostToolUse lost the Bash command detail: {tool_end:?}"
    );

    let idle = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Idle,
        Duration::from_secs(120),
    );
    assert_eq!(idle.agent_type, AgentType::Codex);
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(30)),
        "the Codex card did not return to Idle at Stop-hook turn end:\n{}",
        deck.snapshot_grid()
    );

    let sentinel = deck.workdir().join(HOOK_SENTINEL_NAME);
    let sentinel_content = std::fs::read_to_string(&sentinel)
        .expect("real Codex did not create the requested hook sentinel");
    assert_eq!(
        sentinel_content, HOOK_SENTINEL_CONTENT,
        "real Codex did not complete the requested shell work"
    );
    assert!(
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .any(|record| record.agent_type == Some(AgentType::Codex)),
        "Stop-hook Idle was observed only after Codex exited; the pane must still be live"
    );
}
