//! Credentialed phrase-compatibility fixtures for PRD #802 M9.
//!
//! These fixtures are authoritative only for the shipping default intent
//! backend, which is now the **keyed API backend on this build's own preset**
//! — `test_support::api_preset()`, currently Anthropic Messages on
//! `claude-haiku-4-5`. A green run with another model, endpoint or protocol
//! would prove less than it appears to, so this module deliberately offers no
//! backend switch: it reads the preset rather than taking one.
//!
//! **It used to drive `AgentCliResolver::claude()`, and that backend is gone.**
//! PRD #802's provider work removed the agent-CLI intent backend outright —
//! Commands is API-only, because a stage that needs a key has to let the user
//! choose whose key it is. These fixtures moved off it one commit ahead of the
//! removal, so it landed with the feature's only real-model verification intact
//! rather than reconstructed afterwards. That migration changed the resolver
//! under them and **no expectation in them**: the manifest, the planted fleet,
//! the outcome kinds and the pre-validation below are unchanged.
//!
//! The test is local-only. It never reaches a model in CI or during an ordinary
//! `cargo test-fast`; set `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` to opt in and to
//! turn a missing credential into a failure instead of a green runtime skip.
//!
//! # The ambiguous-name fixture shares a role, not a completable label
//!
//! *"zoom coder"* is heard against two agents both displayed as **Atlas** and
//! both carrying the visible role `coder`. Returning either model-visible name
//! therefore matches both agents and produces [`VoiceOutcome::ParamAmbiguous`],
//! so the app asks which was meant; `coder` offers no distinct full label that
//! names just one.
//!
//! This replaced the flaky **coder one** / **coder two** fleet. Across five
//! full suite runs on 2026-09-20 against `claude-haiku-4-5`, that version passed
//! only **2** times and failed 3, each failure resolving to `agent-coder-one`.
//! Seven prompt variants did not yield a fix: every wording that reliably
//! stopped the completion also degraded `open-agent-by-state` (*"show me the
//! one that's stuck"*). No prompt change shipped. The PRD's 2026-09-20 risk
//! records the product weakness that remains after making this fixture robust.
//! The isolated fleet then made this fixture pass 5 of 5 full-suite runs; four
//! suites were 24/24, while one was 23/24 when the untouched
//! `close-agent-view-unavailable` returned `none`.
//!
//! [`VoiceOutcome::ParamAmbiguous`]: dot_agent_deck_desktop::voice::VoiceOutcome::ParamAmbiguous

use std::fmt;
use std::time::{Duration, Instant};

use dot_agent_deck_desktop::voice::{
    NO_MATCH_ACTION, REMOTE_TIMEOUT, Screen, Transcript, VoiceOutcome, handle_utterance, table,
    test_support::{api_preset, api_resolver, role_agent_in_state, with_tool},
};
use serde::Deserialize;

const FIXTURES: &str = include_str!("../src/voice/phrase_fixtures.toml");
const REQUIRE_REAL_E2E_ENV: &str = "DOT_AGENT_DECK_REQUIRE_REAL_E2E";
/// The credential the keyed backend authenticates with, as the developer's own
/// shell already spells it. Rule 5's lane 2: a developer's key on a developer's
/// machine, and nothing registered on the repository.
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const MIN_FIXTURE_COUNT: usize = 24;
const PENDING_OPEN_SETTINGS_ACTION: &str = "open_settings";
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
    #[serde(default)]
    pending_action: bool,
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

/// The credential this run will spend, or why it cannot run.
///
/// The same shape the CLI preflight had — a question asked before any fixture
/// is dispatched, so "cannot run" is one sentence rather than 24 identical
/// failures. It reads the variable and checks it is not blank; whether the key
/// is *valid* is what the first fixture finds out, and an invalid key is a
/// failure rather than a skip.
fn preflight_key() -> Result<String, String> {
    match std::env::var(API_KEY_ENV) {
        Ok(key) if !key.trim().is_empty() => Ok(key),
        Ok(_) => Err(format!("{API_KEY_ENV} is set and blank")),
        Err(_) => Err(format!(
            "the keyed command backend needs {API_KEY_ENV} in the environment"
        )),
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

/// Scenario: Validate every checked-in phrase and the planted fleet before any
/// live-model gate. Skip CI unless a real-agent run is explicitly required;
/// with local opt-in, verify the full outcome through the shipping keyed API
/// backend on this build's own endpoint and model.
#[tokio::test]
async fn voice_phrase_fixtures_match_the_default_backend() {
    let fixtures = manifest();
    assert!(
        fixtures.fixtures.len() >= MIN_FIXTURE_COUNT,
        "voice phrase fixture manifest must contain at least {MIN_FIXTURE_COUNT} fixtures, found {}",
        fixtures.fixtures.len()
    );

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
        role_agent_in_state("agent-reviewer", "reviewer", "working"),
    ];
    let mut atlas_one = role_agent_in_state("agent-atlas-one", "coder", "working");
    atlas_one.display_name = Some("Atlas".to_string());
    let mut atlas_two = role_agent_in_state("agent-atlas-two", "coder", "working");
    atlas_two.display_name = Some("Atlas".to_string());
    let ambiguous_name_agents = vec![atlas_one, atlas_two];
    for fixture in &fixtures.fixtures {
        assert!(
            Screen::parse(&fixture.screen).is_some(),
            "{}: fixture names unknown screen `{}`",
            fixture.name,
            fixture.screen
        );
        if fixture.pending_action {
            assert_eq!(
                fixture.action, PENDING_OPEN_SETTINGS_ACTION,
                "{}: only `{PENDING_OPEN_SETTINGS_ACTION}` may be marked as a pending action",
                fixture.name
            );
            assert!(
                table().row(&fixture.action).is_none(),
                "{}: pending action `{}` now exists in commands.toml; remove its `pending_action` marker",
                fixture.name,
                fixture.action
            );
        } else {
            assert!(
                fixture.action == NO_MATCH_ACTION || table().row(&fixture.action).is_some(),
                "{}: fixture names unknown action `{}`",
                fixture.name,
                fixture.action
            );
        }
        if let Some(expected) = fixture.resolved_agent.as_deref() {
            assert!(
                agents.iter().any(|agent| agent.id == expected),
                "{}: fixture expects unknown agent id `{expected}`",
                fixture.name
            );
        }
    }

    let require_real_e2e = truthy_env(REQUIRE_REAL_E2E_ENV);
    if truthy_env("CI") {
        assert!(
            !require_real_e2e,
            "{REQUIRE_REAL_E2E_ENV} is set in CI, so this real-agent test must RUN, not skip"
        );
        skip("voice phrase fixtures never reach a real agent in CI");
        return;
    }
    if !require_real_e2e {
        skip("set DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 to run the voice phrase fixtures");
        return;
    }
    let key = match preflight_key() {
        Ok(key) => key,
        Err(reason) => panic!(
            "{REQUIRE_REAL_E2E_ENV} is set, so this real-agent test must RUN, not skip: {reason}"
        ),
    };

    let (endpoint, model) = api_preset();
    let resolver = api_resolver(&key);
    eprintln!(
        "backend: {} | {endpoint} | {model}",
        resolver.backend_name()
    );
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
        let fixture_agents = if fixture.name == "open-agent-ambiguous-name" {
            &ambiguous_name_agents
        } else {
            &agents
        };
        let resolved = tokio::time::timeout(
            REMOTE_TIMEOUT + PER_FIXTURE_GRACE,
            handle_utterance(
                resolver.as_ref(),
                table(),
                screen,
                fixture_agents,
                transcript,
            ),
        )
        .await;

        let result = match resolved {
            Err(_) => Err(format!(
                "timed out after {:?}",
                REMOTE_TIMEOUT + PER_FIXTURE_GRACE
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
