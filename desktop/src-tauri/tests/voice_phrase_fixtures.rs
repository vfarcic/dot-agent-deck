//! Credentialed phrase-compatibility fixtures for PRD #802 M9.
//!
//! These fixtures are authoritative only for the shipping default intent
//! backend, which is now the **keyed API backend on this build's own preset**
//! — `test_support::api_preset()`, currently OpenAI chat-completions on
//! `gpt-5-mini`. A green run with another model, endpoint or protocol would
//! prove less than it appears to, so this module deliberately offers no backend
//! switch: it builds through `resolver_for(&IntentSettings::default())`, which
//! is the call production makes, and therefore follows the default wherever it
//! goes rather than naming a protocol of its own.
//!
//! **That is why `API_KEY_ENV` moved too.** The variable a developer has to set
//! is the one the default backend authenticates with, so it tracks the default
//! rather than being a separate decision — and a run with the wrong vendor's
//! key is a 401 on the first fixture, not a skip.
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
    NO_MATCH_ACTION, ParamKind, REMOTE_TIMEOUT, Screen, Transcript, VoiceOutcome,
    dictation::normalise,
    handle_utterance, table,
    test_support::{api_preset, api_resolver, role_agent_in_state, with_tool},
};
use serde::Deserialize;

const FIXTURES: &str = include_str!("../src/voice/phrase_fixtures.toml");
const REQUIRE_REAL_E2E_ENV: &str = "DOT_AGENT_DECK_REQUIRE_REAL_E2E";
/// The credential the keyed backend authenticates with, as the developer's own
/// shell already spells it. Rule 5's lane 2: a developer's key on a developer's
/// machine, and nothing registered on the repository.
///
/// **Keep this matched to the default backend.** It is `OPENAI_API_KEY` because
/// `IntentSettings::default()` is the OpenAI preset; a build whose default went
/// back to Anthropic would need `ANTHROPIC_API_KEY` here, and nothing
/// mechanical enforces the pairing — the preflight only knows whether the
/// variable it was told to read is set.
const API_KEY_ENV: &str = "OPENAI_API_KEY";
const MIN_FIXTURE_COUNT: usize = 30;
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
    /// The deck id a `deck_ref` param must resolve to (PRD #1223), checked
    /// against the planted fleet the way `resolved_agent` is against agents.
    #[serde(default)]
    resolved_deck: Option<String>,
    /// The introducing words a dictation fixture expects the model to MARK.
    ///
    /// **Not the text to type**, which is the whole design: the app takes that
    /// from the transcript itself, so there is nothing model-supplied for a
    /// fixture to assert about it. What a fixture can check — and what this
    /// column checks — is that the model put the boundary in the right place,
    /// which is the only thing it is trusted with. Compared through
    /// `voice::dictation::normalise`, because the model is quoting from prose
    /// and its casing and punctuation are its own.
    #[serde(default)]
    dictate_prefix: Option<String>,
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

/// The deck a dispatch's `deck_ref` param resolved to, if it carried one.
fn resolved_deck(outcome: &VoiceOutcome) -> Option<&str> {
    match outcome {
        VoiceOutcome::Dispatch { params, .. } => params
            .iter()
            .find(|param| param.kind == ParamKind::DeckRef)
            .map(|param| param.value.as_str()),
        _ => None,
    }
}

/// What the model marked as the introducing words, for a dictation dispatch.
///
/// Read off `spoken` rather than `value`: `value` is what the app resolved the
/// boundary TO — a slice of its own transcript — and asserting on it would
/// check this test's own arithmetic instead of the model's answer.
fn marked_prefix(outcome: &VoiceOutcome) -> Option<&str> {
    match outcome {
        VoiceOutcome::Dispatch { params, .. } => params
            .iter()
            .find(|param| param.kind == ParamKind::SpokenPrefix)
            .map(|param| param.spoken.as_str()),
        _ => None,
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
    // PRD #1223 — the fleet a `deck_ref` resolves against: this machine's deck
    // and one remote, labelled the way the overview labels them.
    let decks = vec![
        dot_agent_deck_desktop::voice::VoiceDeck {
            id: "deck-local".to_string(),
            label: "Local deck".to_string(),
            local: true,
        },
        dot_agent_deck_desktop::voice::VoiceDeck {
            id: "deck-build-box".to_string(),
            label: "deploy@build-box".to_string(),
            local: false,
        },
    ];
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
        if let Some(expected) = fixture.resolved_deck.as_deref() {
            assert!(
                decks.iter().any(|deck| deck.id == expected),
                "{}: fixture expects unknown deck id `{expected}`",
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
                &decks,
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
                let deck_matches = match fixture.resolved_deck.as_deref() {
                    Some(expected) => resolved_deck(&answer.outcome) == Some(expected),
                    None => true,
                };
                let prefix_matches = match fixture.dictate_prefix.as_deref() {
                    Some(expected) => marked_prefix(&answer.outcome)
                        .is_some_and(|marked| normalise(marked) == normalise(expected)),
                    None => true,
                };
                if action_matches
                    && actual_outcome == fixture.outcome
                    && agent_matches
                    && deck_matches
                    && prefix_matches
                {
                    Ok(())
                } else {
                    Err(format!(
                        "expected action={} outcome={} resolved_agent={:?} resolved_deck={:?} \
                         dictate_prefix={:?}, got action={:?} outcome={actual_outcome} \
                         resolved_agent={actual_agent:?} resolved_deck={:?} dictate_prefix={:?}",
                        fixture.action,
                        fixture.outcome,
                        fixture.resolved_agent,
                        fixture.resolved_deck,
                        fixture.dictate_prefix,
                        actual_action,
                        resolved_deck(&answer.outcome),
                        marked_prefix(&answer.outcome),
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
