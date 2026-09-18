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
    AGENT_CLI_TIMEOUT, AgentCliResolver, IntentRequest, IntentResolver, NO_MATCH_ACTION, Screen,
    Transcript, annotate, table,
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
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutcomeKind {
    Dispatch,
    Unavailable,
    NoMatch,
    UnknownAction,
}

impl fmt::Display for OutcomeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dispatch => "dispatch",
            Self::Unavailable => "unavailable",
            Self::NoMatch => "no_match",
            Self::UnknownAction => "unknown_action",
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

fn classify(action: &str, screen: Screen) -> OutcomeKind {
    if action == NO_MATCH_ACTION {
        return OutcomeKind::NoMatch;
    }
    match table().row(action) {
        Some(row) if row.callable_on(screen) => OutcomeKind::Dispatch,
        Some(_) => OutcomeKind::Unavailable,
        None => OutcomeKind::UnknownAction,
    }
}

/// Scenario: With an explicit local real-agent opt-in, feed every checked-in
/// phrase to the shipping Claude CLI resolver and verify both the chosen action
/// and whether that action is callable on the fixture's current screen.
#[tokio::test]
async fn voice_phrase_fixtures_match_the_default_backend() {
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
    let fixtures = manifest();
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
        let commands = annotate(table(), screen);
        let transcript = Transcript::new(&fixture.utterance);
        let started = Instant::now();
        let resolved = tokio::time::timeout(
            AGENT_CLI_TIMEOUT + PER_FIXTURE_GRACE,
            resolver.resolve(IntentRequest {
                transcript: &transcript,
                commands: &commands,
                agents: &[],
            }),
        )
        .await;

        let result = match resolved {
            Err(_) => Err(format!(
                "timed out after {:?}",
                AGENT_CLI_TIMEOUT + PER_FIXTURE_GRACE
            )),
            Ok(Err(error)) => Err(format!("backend failed: {error}")),
            Ok(Ok(answer)) => {
                let actual_outcome = classify(&answer.action, screen);
                if answer.action == fixture.action && actual_outcome == fixture.outcome {
                    Ok(())
                } else {
                    Err(format!(
                        "expected action={} outcome={}, got action={} outcome={actual_outcome}",
                        fixture.action, fixture.outcome, answer.action
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
