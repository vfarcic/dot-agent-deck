//! Tests for PRD #1184's chain path through the utterance pipeline: what an
//! answer of two or three steps comes to, with a scripted backend whose
//! meaning check is asked about each step's own words.

use super::fixtures::role_agent;
use super::outcome::{VoiceOutcome, handle_utterance};
use super::resolver::{IntentAnswer, IntentStep, StubResolver};
use super::table::{Screen, table};
use super::{DesktopAgent, Transcript, VoiceDeck};

fn fleet() -> Vec<DesktopAgent> {
    vec![role_agent("1", "tester"), role_agent("2", "reviewer")]
}

fn decks() -> Vec<VoiceDeck> {
    [
        ("deck-local", "Local deck", true),
        ("deck-build", "deploy@build-box", false),
    ]
    .into_iter()
    .map(|(id, label, local)| VoiceDeck {
        id: id.to_string(),
        label: label.to_string(),
        address: None,
        local,
        unavailable: None,
        holds_agents: local,
        agents: Default::default(),
    })
    .collect()
}

fn step(action: &str, said: &str, params: &[(&str, &str)]) -> IntentStep {
    IntentStep {
        action: action.to_string(),
        params: params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        said: said.to_string(),
    }
}

/// The answer a backend gives to a chain: the first step at the top level,
/// and every step listed.
fn chain(steps: Vec<IntentStep>) -> IntentAnswer {
    let first = steps[0].answer();
    first.with_steps(steps)
}

async fn run(resolver: &StubResolver, screen: Screen, said: &str) -> VoiceOutcome {
    handle_utterance(
        resolver,
        table(),
        screen,
        &fleet(),
        &decks(),
        None,
        None,
        Transcript::new(said),
    )
    .await
    .outcome
}

fn refused(outcome: &VoiceOutcome) -> &str {
    match outcome {
        VoiceOutcome::ChainRefused { sentence, .. } => sentence,
        other => panic!("expected the chain to be refused, got {other:?}"),
    }
}

/// Scenario: say "open the overview and then the tester" on the Daemons
/// screen, answered as two steps each asked for by its own words. Both steps
/// pass every check, and the chain is still refused because chains are not
/// enabled yet — nothing runs.
#[tokio::test]
async fn voice_chain_a_valid_chain_is_refused_until_chains_are_enabled() {
    let said = "open the overview and then the tester";
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step("open_overview", "open the overview", &[]),
                step("open_agent", "the tester", &[("agent", "tester")]),
            ]),
        )
        .judging("open the overview", "open_overview", true)
        .judging("the tester", "open_agent", true);
    let outcome = run(&resolver, Screen::Deck, said).await;
    assert!(refused(&outcome).contains("not enabled yet"), "{outcome:?}");
}

/// Scenario: the meaning check finds the whole sentence asks to open the
/// tester, but the second step's own words do not. The chain is refused at
/// that step, quoting its words, and nothing runs.
#[tokio::test]
async fn voice_chain_each_step_is_grounded_in_its_own_words() {
    let said = "open the overview and then the tester";
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step("open_overview", "open the overview", &[]),
                step("open_agent", "the tester", &[("agent", "tester")]),
            ]),
        )
        .judging(said, "open_agent", true)
        .judging("open the overview", "open_overview", true)
        .judging("the tester", "open_agent", false);
    let outcome = run(&resolver, Screen::Deck, said).await;
    let sentence = refused(&outcome);
    assert!(
        sentence.contains("step 2 (\u{201c}the tester\u{201d}) could not run"),
        "{sentence}"
    );
    assert!(sentence.ends_with("nothing was done."), "{sentence}");
}

/// Scenario: "open the tester and open the reviewer" on the dashboard. The
/// first step lands on the tester's pane, where opening another agent cannot
/// run, so the chain is refused at step 2 before anything runs.
#[tokio::test]
async fn voice_chain_a_step_is_judged_on_the_screen_the_earlier_steps_leave() {
    let said = "open the tester and open the reviewer";
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step("open_agent", "open the tester", &[("agent", "tester")]),
                step("open_agent", "open the reviewer", &[("agent", "reviewer")]),
            ]),
        )
        .judging("open the tester", "open_agent", true)
        .judging("open the reviewer", "open_agent", true);
    let outcome = run(&resolver, Screen::Overview, said).await;
    let sentence = refused(&outcome);
    assert!(
        sentence.contains("step 2 (\u{201c}open the reviewer\u{201d})"),
        "{sentence}"
    );
}

/// Scenario: "switch to the build box and then the tester". After a daemon
/// switch the next daemon's agents are not known yet, so a step naming an
/// agent is refused rather than resolved against the old daemon's agents.
#[tokio::test]
async fn voice_chain_no_agent_is_named_after_a_daemon_switch() {
    let said = "switch to the build box and then the tester";
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step(
                    "switch_deck",
                    "switch to the build box",
                    &[("deck", "build box")],
                ),
                step("open_agent", "the tester", &[("agent", "tester")]),
            ]),
        )
        .judging("switch to the build box", "switch_deck", true)
        .judging("the tester", "open_agent", true);
    let outcome = run(&resolver, Screen::Deck, said).await;
    let sentence = refused(&outcome);
    assert!(
        sentence.contains("has not shown its agents yet"),
        "{sentence}"
    );
}

/// Scenario: "open the overview or the deck" answered as two steps is
/// refused for its contrast word, whatever the meaning check says.
#[tokio::test]
async fn voice_chain_a_contrast_word_never_yields_two_steps() {
    let said = "open the overview or the deck";
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step("open_overview", "open the overview", &[]),
                step("open_deck", "or the deck", &[]),
            ]),
        )
        .judging_everything(true);
    let outcome = run(&resolver, Screen::Overview, said).await;
    assert!(
        refused(&outcome).contains("say one command at a time"),
        "{outcome:?}"
    );
}

/// Scenario: "type run the tests and then commit" over an agent's pane,
/// split by the model into a dictation and a start with the dictation at the
/// top level. It is answered as the single dictation it is.
#[tokio::test]
async fn voice_chain_a_split_dictation_is_the_single_dictation() {
    let said = "type run the tests and then commit";
    let resolver = StubResolver::new().answering(
        said,
        IntentAnswer::new("dictate_to_agent")
            .with_param("prefix", "type")
            .with_steps(vec![
                step(
                    "dictate_to_agent",
                    "type run the tests",
                    &[("prefix", "type")],
                ),
                step("start_new_agent", "and then commit", &[]),
            ]),
    );
    let outcome = run(&resolver, Screen::Agent, said).await;
    assert!(
        !matches!(outcome, VoiceOutcome::ChainRefused { .. }),
        "{outcome:?}"
    );
}

/// Scenario: an answer that lists one step is the single command at its top
/// level, exactly as an answer with no steps.
#[tokio::test]
async fn voice_chain_one_listed_step_is_a_single_command() {
    let said = "open the overview";
    let resolver = StubResolver::new().answering(
        said,
        IntentAnswer::new("open_overview").with_steps(vec![step("open_overview", said, &[])]),
    );
    let outcome = run(&resolver, Screen::Deck, said).await;
    assert!(
        matches!(&outcome, VoiceOutcome::Dispatch { action, .. } if action == "open_overview"),
        "{outcome:?}"
    );
}

/// Scenario: build the request each backend protocol sends and check that
/// the chain rules ride only as the description of the answer's `steps`
/// field — never in the system prompt, where M1 measured them regressing
/// single commands — and that `steps` may be `null`.
#[test]
fn voice_chain_the_rules_live_in_the_answer_schema_only() {
    use super::schema::{CHAIN_INSTRUCTIONS, annotate};
    let commands = annotate(table(), Screen::Overview);
    let transcript = Transcript::new("open the overview");
    let request = super::resolver::IntentRequest {
        transcript: &transcript,
        commands: &commands,
        agents: &[],
        decks: &[],
        directories: None,
        new_agent: None,
    };

    let openai = super::openai::request_body(
        &request,
        "a-model",
        crate::model_service::TokenCeiling::default(),
        None,
    );
    let schema = &openai["response_format"]["json_schema"]["schema"];
    assert_eq!(
        schema["properties"]["steps"]["description"],
        CHAIN_INSTRUCTIONS
    );
    assert_eq!(
        schema["properties"]["steps"]["type"],
        serde_json::json!(["array", "null"])
    );
    let system = openai["messages"][0]["content"]
        .as_str()
        .expect("system turn");
    assert!(!system.contains("`steps`"), "{system}");

    let anthropic = super::remote::request_body(
        &request,
        "a-model",
        crate::model_service::TokenCeiling::default(),
    );
    let tool = &anthropic["tools"][0];
    assert_eq!(
        tool["input_schema"]["properties"]["steps"]["description"],
        CHAIN_INSTRUCTIONS
    );
    assert!(
        !tool["description"]
            .as_str()
            .unwrap_or_default()
            .contains("`steps`")
    );
    assert!(
        !anthropic["system"]
            .as_str()
            .unwrap_or_default()
            .contains("`steps`")
    );
}
