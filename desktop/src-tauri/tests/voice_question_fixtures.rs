//! Credentialed question fixtures for PRD #1542 (decision 4).
//!
//! What the user says to a pending question is matched onto its options by the
//! desktop's Commands model, not by a phrase list, so the unit tests in
//! `voice/question_tests.rs` (which drive a stub resolver) prove the plumbing
//! and the local validation but never that a real model maps "go ahead" onto
//! Allow once or "red for colour and large for size" onto a two-question form.
//! This file is that check, in the style of `voice_phrase_fixtures.rs`: it
//! drives the **shipping default intent backend** through
//! `test_support::api_resolver`, so it follows the default wherever it goes,
//! and offers no backend switch.
//!
//! The questions are built the way the daemon builds them — the root crate's
//! own builders over the captured payloads in `tests/fixtures/agent-questions`
//! — and projected through the desktop's production projection, so the model
//! sees the labels, roles and scopes a user's question would carry.
//!
//! The model now answers each selection with `evidence` — the user's words
//! that chose it, copied verbatim — and `voice::question::resolve` refuses any
//! answer whose evidence, or whose free text, is not a run of the utterance's
//! own words (audit A1). So a fixture passes only when the real model both
//! picks the right options AND quotes the utterance faithfully; a model that
//! paraphrases its evidence fails here as "refused", which is the regression
//! this file exists to catch.
//!
//! Some utterances below ("yes", "no", "blue") equal an option's label and
//! never reach the model: `voice::question` matches those locally. They are
//! kept because they are what a user says, and the fixture pins that they keep
//! landing where they should whichever path answers them.
//!
//! The test is local-only. It never reaches a model in CI or during an ordinary
//! `cargo test-fast`; set `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` to opt in and to
//! turn a missing credential into a failure instead of a green runtime skip.
//! `DOT_AGENT_DECK_VOICE_FIXTURE=<text>` runs only the fixtures whose name
//! contains it.

use std::time::{Duration, Instant};

use dot_agent_deck::question::{
    PendingQuestion, claude_permission_request, codex_permission_request, opencode_permission_asked,
};
use dot_agent_deck_desktop::voice::{
    REMOTE_TIMEOUT, Transcript,
    question::{QuestionVerdict, resolve},
    test_support::{api_preset, api_resolver, pending_question},
};
use serde_json::Value;

const REQUIRE_REAL_E2E_ENV: &str = "DOT_AGENT_DECK_REQUIRE_REAL_E2E";
const FIXTURE_FILTER_ENV: &str = "DOT_AGENT_DECK_VOICE_FIXTURE";
/// The credential the default backend authenticates with — keep it matched to
/// `IntentSettings::default()`, exactly as `voice_phrase_fixtures.rs` does.
const API_KEY_ENV: &str = "OPENAI_API_KEY";
const PER_FIXTURE_GRACE: Duration = Duration::from_secs(15);

const CLAUDE_BASH: &str =
    include_str!("../../../tests/fixtures/agent-questions/claude-permission-bash.json");
const CLAUDE_FORM: &str =
    include_str!("../../../tests/fixtures/agent-questions/claude-ask-user-question-form.json");
const CODEX_PERMISSION: &str =
    include_str!("../../../tests/fixtures/agent-questions/codex-permission-request.json");
const OPENCODE_PERMISSION: &str =
    include_str!("../../../tests/fixtures/agent-questions/opencode-permission-asked.json");

/// Which planted question a fixture is said to.
#[derive(Debug, Clone, Copy)]
enum Planted {
    /// Claude Code's Bash permission: 1 Yes, 2 always allow access to the
    /// directory, 3 No.
    ClaudeBash,
    /// Codex's command approval: 1 Yes, proceed, 2 don't ask again for the
    /// prefix, 3 No, and tell Codex what to do differently.
    CodexPermission,
    /// OpenCode's permission: 1 Allow once, 2 Allow always, 3 Reject.
    OpenCodePermission,
    /// Claude Code's two-question form: Colour (Red, Green, Blue — single
    /// choice) and Sizes (Small, Medium, Large — multiple choice), each with
    /// "Type something." and a keyboard-only "Chat about this" appended.
    ClaudeForm,
}

/// What the utterance must come to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    /// These selections, `(question_index, option_indices)`, in question
    /// order; `always` whether the "always allow" confirmation is raised.
    Answer {
        form: Vec<(u32, Vec<u32>)>,
        always: bool,
    },
    /// One free-text option chosen, with exactly these words — the
    /// transcript's own, never the model's rendering of them.
    Text {
        question: u32,
        option: u32,
        text: &'static str,
    },
    /// Not about the question — on to ordinary command handling.
    NotAnswer,
}

struct Fixture {
    name: &'static str,
    planted: Planted,
    utterance: &'static str,
    expect: Expect,
}

fn answer(form: &[(u32, &[u32])]) -> Expect {
    Expect::Answer {
        form: form
            .iter()
            .map(|(question, options)| (*question, options.to_vec()))
            .collect(),
        always: false,
    }
}

fn always(question: u32, option: u32) -> Expect {
    Expect::Answer {
        form: vec![(question, vec![option])],
        always: true,
    }
}

fn fixtures() -> Vec<Fixture> {
    use Planted::*;
    let f = |name, planted, utterance, expect| Fixture {
        name,
        planted,
        utterance,
        expect,
    };
    vec![
        f(
            "claude-permission-yes",
            ClaudeBash,
            "yes",
            answer(&[(0, &[1])]),
        ),
        f(
            "claude-permission-go-ahead",
            ClaudeBash,
            "go ahead",
            answer(&[(0, &[1])]),
        ),
        f(
            "claude-permission-sure-do-it",
            ClaudeBash,
            "sure, do it",
            answer(&[(0, &[1])]),
        ),
        f(
            "claude-permission-no",
            ClaudeBash,
            "no",
            answer(&[(0, &[3])]),
        ),
        f(
            "claude-permission-dont-run-that",
            ClaudeBash,
            "no, don't run that",
            answer(&[(0, &[3])]),
        ),
        f(
            "claude-permission-always-allow",
            ClaudeBash,
            "always allow",
            always(0, 2),
        ),
        f(
            "claude-permission-unrelated-command",
            ClaudeBash,
            "open the dashboard",
            Expect::NotAnswer,
        ),
        f(
            "codex-permission-yes",
            CodexPermission,
            "yes",
            answer(&[(0, &[1])]),
        ),
        f(
            "codex-permission-go-ahead",
            CodexPermission,
            "go ahead",
            answer(&[(0, &[1])]),
        ),
        f(
            "codex-permission-no",
            CodexPermission,
            "no",
            answer(&[(0, &[3])]),
        ),
        f(
            "codex-permission-always-allow",
            CodexPermission,
            "always allow",
            always(0, 2),
        ),
        f(
            "codex-permission-unrelated-command",
            CodexPermission,
            "open the dashboard",
            Expect::NotAnswer,
        ),
        f(
            "opencode-permission-go-ahead",
            OpenCodePermission,
            "go ahead",
            answer(&[(0, &[1])]),
        ),
        f(
            "opencode-permission-no",
            OpenCodePermission,
            "no",
            answer(&[(0, &[3])]),
        ),
        f(
            "opencode-permission-always-allow",
            OpenCodePermission,
            "always allow",
            always(0, 2),
        ),
        f(
            "form-two-questions-one-utterance",
            ClaudeForm,
            "red for colour and large for size",
            answer(&[(0, &[1]), (1, &[3])]),
        ),
        f(
            "form-multi-select",
            ClaudeForm,
            "small and large",
            answer(&[(1, &[1, 3])]),
        ),
        f("form-label", ClaudeForm, "blue", answer(&[(0, &[3])])),
        f(
            "form-free-text",
            ClaudeForm,
            "type my own colour: a hamster named Bob",
            Expect::Text {
                question: 0,
                option: 4,
                text: "a hamster named Bob",
            },
        ),
        f(
            "form-unrelated-command",
            ClaudeForm,
            "open the dashboard",
            Expect::NotAnswer,
        ),
    ]
}

fn payload(raw: &str) -> Value {
    serde_json::from_str(raw).expect("a captured question payload must parse")
}

/// The question the daemon would hold for `planted`, built by the daemon's own
/// builders from the captured payload.
fn build(planted: Planted) -> PendingQuestion {
    let id = "q-voice-fixture".to_string();
    let claude = |raw: &str| {
        let payload = payload(raw);
        let tool_name = payload["tool_name"].as_str().unwrap_or_default();
        let detail = payload["tool_input"]["command"]
            .as_str()
            .map(str::to_string);
        claude_permission_request(
            id.clone(),
            tool_name,
            payload.get("tool_input"),
            detail,
            payload.get("permission_suggestions"),
            1,
            None,
        )
        .question
    };
    match planted {
        Planted::ClaudeBash => claude(CLAUDE_BASH),
        Planted::ClaudeForm => claude(CLAUDE_FORM),
        Planted::CodexPermission => {
            let payload = payload(CODEX_PERMISSION);
            codex_permission_request(
                id,
                payload["tool_name"].as_str().unwrap_or_default(),
                payload["tool_input"]["command"]
                    .as_str()
                    .map(str::to_string),
                payload["tool_input"]["command"].as_str(),
                1,
                None,
            )
        }
        Planted::OpenCodePermission => opencode_permission_asked(&payload(OPENCODE_PERMISSION), 1)
            .expect("the captured OpenCode permission must build a question"),
    }
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

fn preflight_key() -> Result<String, String> {
    match std::env::var(API_KEY_ENV) {
        Ok(key) if !key.trim().is_empty() => Ok(key),
        Ok(_) => Err(format!("{API_KEY_ENV} is set and blank")),
        Err(_) => Err(format!(
            "the keyed command backend needs {API_KEY_ENV} in the environment"
        )),
    }
}

fn observed(verdict: &QuestionVerdict) -> String {
    match verdict {
        QuestionVerdict::Answered { form, always, .. } => format!(
            "answer {:?} always={}",
            form.iter()
                .map(|selection| (
                    selection.question_index,
                    selection.option_indices.clone(),
                    selection.text.clone()
                ))
                .collect::<Vec<_>>(),
            always.is_some()
        ),
        QuestionVerdict::NotAnswer => "not_answer".to_string(),
        QuestionVerdict::Cancelled { sentence } => format!("cancelled ({sentence})"),
        QuestionVerdict::Refused { sentence } => format!("refused ({sentence})"),
    }
}

fn matches(expect: &Expect, verdict: &QuestionVerdict) -> bool {
    match (expect, verdict) {
        (Expect::NotAnswer, QuestionVerdict::NotAnswer) => true,
        (
            Expect::Answer {
                form: expected,
                always: expected_always,
            },
            QuestionVerdict::Answered { form, always, .. },
        ) => {
            let got: Vec<(u32, Vec<u32>)> = form
                .iter()
                .map(|selection| {
                    let mut options = selection.option_indices.clone();
                    options.sort_unstable();
                    (selection.question_index, options)
                })
                .collect();
            got == *expected && always.is_some() == *expected_always
        }
        (
            Expect::Text {
                question,
                option,
                text,
            },
            QuestionVerdict::Answered { form, .. },
        ) => matches!(form.as_slice(), [selection]
            if selection.question_index == *question
                && selection.option_indices == [*option]
                && selection.text.as_deref() == Some(*text)),
        _ => false,
    }
}

/// Scenario: Each planted question — a Claude Code, Codex and OpenCode
/// permission prompt and Claude Code's two-question form, built from captured
/// payloads by the daemon's own builders — is given what a user would say to
/// it. The shipping default Commands backend maps every utterance onto the
/// expected options (with the "always allow" confirmation raised where it
/// must be), citing words that are really in the utterance, takes a free-text
/// answer as the user's own words, and leaves an unrelated command to
/// ordinary command handling.
#[tokio::test]
async fn voice_question_fixtures_match_the_default_backend() {
    let fixtures = fixtures();
    // Pre-validate every fixture against the planted question before any
    // model call, so a fixture naming an option that does not exist is a
    // fixture bug rather than a model failure.
    for fixture in &fixtures {
        let question = pending_question(&build(fixture.planted));
        assert!(
            question.answerable,
            "{}: the planted question must be answerable",
            fixture.name
        );
        let form = match &fixture.expect {
            Expect::Answer { form, .. } => form.clone(),
            Expect::Text {
                question, option, ..
            } => vec![(*question, vec![*option])],
            Expect::NotAnswer => Vec::new(),
        };
        {
            for (at, options) in &form {
                let q = question
                    .question(*at)
                    .unwrap_or_else(|| panic!("{}: no question {at}", fixture.name));
                for index in options {
                    let option = q.option(*index).unwrap_or_else(|| {
                        panic!("{}: question {at} has no option {index}", fixture.name)
                    });
                    assert!(
                        option.answerable,
                        "{}: option {index} is not answerable",
                        fixture.name
                    );
                }
            }
        }
    }

    let require_real_e2e = truthy_env(REQUIRE_REAL_E2E_ENV);
    if truthy_env("CI") {
        assert!(
            !require_real_e2e,
            "{REQUIRE_REAL_E2E_ENV} is set in CI, so this real-agent test must RUN, not skip"
        );
        skip("voice question fixtures never reach a real agent in CI");
        return;
    }
    if !require_real_e2e {
        skip("set DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 to run the voice question fixtures");
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
    let filter = std::env::var(FIXTURE_FILTER_ENV)
        .ok()
        .filter(|filter| !filter.trim().is_empty());
    let suite_started = Instant::now();
    let mut failures = Vec::new();
    let mut ran = 0usize;
    for fixture in fixtures {
        if filter
            .as_deref()
            .is_some_and(|filter| !fixture.name.contains(filter))
        {
            continue;
        }
        ran += 1;
        let question = pending_question(&build(fixture.planted));
        let started = Instant::now();
        let resolved = tokio::time::timeout(
            REMOTE_TIMEOUT + PER_FIXTURE_GRACE,
            resolve(
                resolver.as_ref(),
                "agent",
                &question,
                &[],
                None,
                Transcript::new(fixture.utterance),
            ),
        )
        .await;
        let elapsed = started.elapsed();
        match resolved {
            Err(_) => failures.push(format!("{}: timed out after {elapsed:?}", fixture.name)),
            Ok(result) => {
                let seen = observed(&result.verdict);
                let model_ms = result
                    .resolve_ms
                    .map_or_else(|| "local".to_string(), |ms| format!("{ms}ms"));
                if matches(&fixture.expect, &result.verdict) {
                    eprintln!("PASS {} ({model_ms}): {seen}", fixture.name);
                } else {
                    eprintln!("FAIL {} ({model_ms}): {seen}", fixture.name);
                    failures.push(format!(
                        "{}: said {:?}, expected {:?}, got {seen}",
                        fixture.name, fixture.utterance, fixture.expect
                    ));
                }
            }
        }
    }
    eprintln!(
        "{ran} question fixtures in {:?}, {} failed",
        suite_started.elapsed(),
        failures.len()
    );
    assert!(ran > 0, "the fixture filter matched no question fixture");
    assert!(
        failures.is_empty(),
        "question fixtures failed:\n{}",
        failures.join("\n")
    );
}
