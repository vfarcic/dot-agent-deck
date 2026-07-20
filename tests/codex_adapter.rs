//! Fast-tier contract tests for the Codex wrapper adapter (PRD #20 M7).
//!
//! These are pure registry and line-classification checks. They intentionally
//! compile RED until production adds the typed Codex identity and its ruleset.

use dot_agent_deck::agent_registry::{self, IntegrationStrategy};
use dot_agent_deck::event::AgentType;
use dot_agent_deck::wrap::{CODEX, DetectedEvent, classify_line_with};
use ratatui::style::Color;

/// Codex is a typed, detectable first-class agent whose complete metadata lives
/// in the registry and selects the stdout-wrapper integration strategy.
#[test]
fn codex_detect_001_registry_identity_is_complete() {
    assert_eq!(
        AgentType::from_command(Some("codex exec --model gpt-5.1-codex-mini")),
        Some(AgentType::Codex)
    );
    assert_eq!(
        AgentType::from_command(Some("/usr/local/bin/codex --full-auto")),
        Some(AgentType::Codex)
    );
    assert_eq!(format!("{}", AgentType::Codex), "Codex");

    let spec = agent_registry::spec(&AgentType::Codex);
    assert_eq!(spec.agent_type, AgentType::Codex);
    assert_eq!(spec.label, "Codex");
    assert_eq!(spec.default_command, Some("codex"));
    assert_eq!(spec.strategy, Some(IntegrationStrategy::Wrapper));
    assert!(spec.detect_basenames.contains(&"codex"));
    assert_ne!(
        spec.badge_color,
        Color::DarkGray,
        "Codex must have a non-neutral first-class badge color"
    );
}

/// Realistic `codex exec --json` lifecycle lines map to dashboard states. Both
/// reasoning and command execution are active work, errors win, and turn
/// completion makes the wrapped session idle without waiting for process exit.
#[test]
fn codex_wrap_001_jsonl_output_maps_to_dashboard_states() {
    let cases = [
        (
            r#"{"type":"turn.started"}"#,
            DetectedEvent::Working,
            "turn start",
        ),
        (
            r#"{"type":"item.started","item":{"type":"reasoning","text":"Inspecting files"}}"#,
            DetectedEvent::Working,
            "reasoning",
        ),
        (
            r#"{"type":"item.started","item":{"type":"command_execution","command":"ls"}}"#,
            DetectedEvent::Working,
            "tool execution",
        ),
        (
            r#"{"type":"error","message":"model request failed"}"#,
            DetectedEvent::Error,
            "error",
        ),
        (
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#,
            DetectedEvent::Idle,
            "turn completion",
        ),
    ];

    for (line, expected, label) in cases {
        assert_eq!(
            classify_line_with(line, &CODEX),
            Some(expected),
            "Codex {label} line was misclassified: {line}"
        );
    }
}
