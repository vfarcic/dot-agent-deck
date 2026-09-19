//! Credentialed phrase-compatibility fixtures for PRD #802 M9.
//!
//! These fixtures are authoritative only for the shipping default intent
//! backend: `AgentCliResolver::claude()`, currently Claude Haiku through the
//! pre-authenticated `claude` CLI. A green run with another model or backend
//! would prove less than it appears to, so this module deliberately offers no
//! backend switch.
//!
//! The test is local-only. It never reaches a model in CI or during an ordinary
//! `cargo test-fast`; set `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` to opt in and to
//! turn a missing CLI or login into a failure instead of a green runtime skip.

use std::fmt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dot_agent_deck_desktop::voice::{
    AGENT_CLI_TIMEOUT, AgentCliResolver, NO_MATCH_ACTION, Screen, Transcript, VoiceOutcome,
    handle_utterance, table,
    test_support::{role_agent_in_state, with_tool},
};
use serde::Deserialize;

const FIXTURES: &str = include_str!("../src/voice/phrase_fixtures.toml");
const REQUIRE_REAL_E2E_ENV: &str = "DOT_AGENT_DECK_REQUIRE_REAL_E2E";
const PER_FIXTURE_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifest {
    fixtures: Vec<PhraseFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhraseFixture {
    name: String,
    utterance: String,
    screen: String,
    action: String,
    outcome: OutcomeKind,
    #[serde(default)]
    resolved_agent: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutcomeKind {
    Dispatch,
    Unavailable,
    NoMatch,
    UnknownAction,
    ParamMissing,
    ParamUnresolved,
    ParamAmbiguous,
    ResolutionFailed,
    TranscriptionFailed,
}

impl fmt::Display for OutcomeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dispatch => "dispatch",
            Self::Unavailable => "unavailable",
            Self::NoMatch => "no_match",
            Self::UnknownAction => "unknown_action",
            Self::ParamMissing => "param_missing",
            Self::ParamUnresolved => "param_unresolved",
            Self::ParamAmbiguous => "param_ambiguous",
            Self::ResolutionFailed => "resolution_failed",
            Self::TranscriptionFailed => "transcription_failed",
        })
    }
}

fn manifest() -> FixtureManifest {
    toml_edit::de::from_str(FIXTURES).expect("voice phrase fixture manifest must parse")
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

fn skip(reason: &str) {
    eprintln!("SKIP: [e2e] {reason}");
}

fn preflight_claude() -> Result<(), String> {
    let status = Command::new("claude")
        .args(["auth", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("the default `claude` backend is not available: {error}"))?;
    if !status.status.success() {
        return Err("the default `claude` backend is installed but not authenticated".to_string());
    }
    let auth: serde_json::Value = serde_json::from_slice(&status.stdout)
        .map_err(|_| "`claude auth status --json` returned unreadable output".to_string())?;
    match auth.get("loggedIn").and_then(serde_json::Value::as_bool) {
        Some(true) => Ok(()),
        _ => Err("the default `claude` backend is not logged in".to_string()),
    }
}

fn observed(outcome: &VoiceOutcome) -> (Option<&str>, OutcomeKind, Option<&str>) {
    match outcome {
        VoiceOutcome::Dispatch { action, params, .. } => (
            Some(action),
            OutcomeKind::Dispatch,
            params
                .iter()
                .find(|param| param.name == "agent")
                .map(|param| param.value.as_str()),
        ),
        VoiceOutcome::Unavailable { action, .. } => (Some(action), OutcomeKind::Unavailable, None),
        VoiceOutcome::NoMatch { .. } => (Some(NO_MATCH_ACTION), OutcomeKind::NoMatch, None),
        VoiceOutcome::UnknownAction { action, .. } => {
            (Some(action), OutcomeKind::UnknownAction, None)
        }
        VoiceOutcome::ParamMissing { action, .. } => {
            (Some(action), OutcomeKind::ParamMissing, None)
        }
        VoiceOutcome::ParamUnresolved { action, .. } => {
            (Some(action), OutcomeKind::ParamUnresolved, None)
        }
        VoiceOutcome::ParamAmbiguous { action, .. } => {
            (Some(action), OutcomeKind::ParamAmbiguous, None)
        }
        VoiceOutcome::ResolutionFailed { .. } => (None, OutcomeKind::ResolutionFailed, None),
        VoiceOutcome::TranscriptionFailed { .. } => (None, OutcomeKind::TranscriptionFailed, None),
    }
}

/// Scenario: With an explicit local real-agent opt-in, feed every checked-in
/// phrase and a live fleet to the shipping Claude CLI resolver. Verify the full
/// outcome, including the resolved agent id or an intentional ambiguity.
#[tokio::test]
async fn voice_phrase_fixtures_match_the_default_backend() {
    let fixtures = manifest();
    let agents = vec![
        role_agent_in_state("agent-tester", "tester", "waiting_for_input"),
        with_tool(
            role_agent_in_state("agent-coder-one", "coder one", "working"),
            "Bash",
            Some("cargo test-fast"),
        ),
        with_tool(
            role_agent_in_state("agent-coder-two", "coder two", "working"),
            "Edit",
            Some("src/lib.rs"),
        ),
        role_agent_in_state("agent-reviewer", "reviewer", "idle"),
    ];
    for fixture in &fixtures.fixtures {
        if let Some(expected) = fixture.resolved_agent.as_deref() {
            assert!(
                agents.iter().any(|agent| agent.id == expected),
                "{}: fixture expects unknown agent id `{expected}`",
                fixture.name
            );
        }
    }

    if truthy_env("CI") {
        skip("voice phrase fixtures never reach a real agent in CI");
        return;
    }
    if !truthy_env(REQUIRE_REAL_E2E_ENV) {
        skip("set DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 to run the voice phrase fixtures");
        return;
    }
    if let Err(reason) = preflight_claude() {
        panic!(
            "{REQUIRE_REAL_E2E_ENV} is set, so this real-agent test must RUN, not skip: {reason}"
        );
    }

    let resolver = AgentCliResolver::claude();
    let suite_started = Instant::now();
    let mut failures = Vec::new();

    for fixture in fixtures.fixtures {
        let Some(screen) = Screen::parse(&fixture.screen) else {
            failures.push(format!(
                "{}: fixture names unknown screen `{}`",
                fixture.name, fixture.screen
            ));
            continue;
        };
        let transcript = Transcript::new(&fixture.utterance);
        let started = Instant::now();
        let resolved = tokio::time::timeout(
            AGENT_CLI_TIMEOUT + PER_FIXTURE_GRACE,
            handle_utterance(&resolver, table(), screen, &agents, transcript),
        )
        .await;

        let result = match resolved {
            Err(_) => Err(format!(
                "timed out after {:?}",
                AGENT_CLI_TIMEOUT + PER_FIXTURE_GRACE
            )),
            Ok(answer) => {
                let (actual_action, actual_outcome, actual_agent) = observed(&answer.outcome);
                let action_matches = actual_action == Some(fixture.action.as_str());
                let agent_matches = match fixture.resolved_agent.as_deref() {
                    Some(expected) => actual_agent == Some(expected),
                    None => true,
                };
                if action_matches && actual_outcome == fixture.outcome && agent_matches {
                    Ok(())
                } else {
                    Err(format!(
                        "expected action={} outcome={} resolved_agent={:?}, got action={:?} \
                         outcome={actual_outcome} resolved_agent={actual_agent:?}",
                        fixture.action, fixture.outcome, fixture.resolved_agent, actual_action
                    ))
                }
            }
        };

        match result {
            Ok(()) => eprintln!(
                "PASS: {} | {:?} | {} | {} -> {} ({:.2?})",
                fixture.name,
                fixture.utterance,
                fixture.screen,
                fixture.action,
                fixture.outcome,
                started.elapsed()
            ),
            Err(error) => {
                eprintln!(
                    "FAIL: {} | {:?} | {} | {} ({:.2?})",
                    fixture.name,
                    fixture.utterance,
                    fixture.screen,
                    error,
                    started.elapsed()
                );
                failures.push(format!("{}: {error}", fixture.name));
            }
        }
    }

    eprintln!(
        "voice phrase fixture wall clock: {:.2?}",
        suite_started.elapsed()
    );
    assert!(
        failures.is_empty(),
        "voice phrase fixture failures:\n{}",
        failures.join("\n")
    );
}
