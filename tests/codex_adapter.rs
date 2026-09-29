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

/// Issue #540: the Codex rule set classifies what the interactive `codex` TUI
/// actually prints — ANSI redraw text, never a `"type":…` JSON record — and it
/// reads every such line as activity and nothing else. In particular a redrawn
/// reply that merely mentions "error" must not flip a working card to `Error`,
/// which is the one thing that keeps this set distinct from `GENERIC`. And the
/// `codex exec --json` records the set used to key on are activity too now:
/// matching them was a classification path the spawned process never reached.
#[test]
fn codex_wrap_001_codex_output_is_activity_only() {
    let cases = [
        (
            "\u{1b}[2K\u{1b}[1G› Ask Codex to do anything",
            "idle composer redraw",
        ),
        (
            "• Ran cargo test -- error handling passes",
            "reply mentioning error",
        ),
        (
            "  └ error: could not compile `demo`",
            "rendered tool output",
        ),
        (
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#,
            "exec turn end",
        ),
        (
            r#"{"type":"error","message":"model request failed"}"#,
            "exec error record",
        ),
    ];
    for (line, label) in cases {
        assert_eq!(
            classify_line_with(line, &CODEX),
            Some(DetectedEvent::Working),
            "Codex {label} line must classify as activity only: {line}"
        );
    }
    assert_eq!(
        classify_line_with("   ", &CODEX),
        None,
        "a blank line signals nothing"
    );
    assert!(
        CODEX.error_markers.is_empty() && CODEX.idle_markers.is_empty(),
        "the Codex set must not grow markers the interactive TUI never prints (issue #540)"
    );
}
