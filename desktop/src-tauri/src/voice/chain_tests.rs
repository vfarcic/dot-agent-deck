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
/// screen, answered as two steps each asked for by its own words. The answer
/// is a chain of both steps, each with the screen it was judged on and its
/// own report, and one sentence reporting both.
#[tokio::test]
async fn voice_chain_a_valid_chain_is_answered_with_its_steps() {
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
    let VoiceOutcome::Chain {
        steps, sentence, ..
    } = &outcome
    else {
        panic!("expected a chain, got {outcome:?}");
    };
    let shape: Vec<_> = steps
        .iter()
        .map(|step| (step.action.as_str(), step.screen, step.said.as_str()))
        .collect();
    assert_eq!(
        shape,
        [
            ("open_overview", Screen::Deck, "open the overview"),
            ("open_agent", Screen::Overview, "the tester"),
        ]
    );
    assert_eq!(steps[1].params[0].value, "1");
    assert!(sentence.contains(", then "), "{sentence}");
    assert!(sentence.ends_with('.'), "{sentence}");
}

/// Scenario: "make it a dispatcher, use codex and name it triage bot" with
/// the New agent form open: three form choices, each kept on the form, are a
/// chain of three steps judged on the same screen.
#[tokio::test]
async fn voice_chain_three_form_choices_are_one_chain() {
    let said = "make it a dispatcher, use codex and name it triage bot";
    let form: super::VoiceNewAgent = serde_json::from_value(serde_json::json!({ "form": {
        "deckId": "deck-local",
        "path": "/home/dev/code/billing",
        "modes": [{ "id": "none", "label": "No mode" }, { "id": "dispatcher", "label": "dispatcher" }],
        "agentTypes": [{ "id": "claude", "label": "Claude Code" }, { "id": "codex", "label": "Codex" }],
    }}))
    .expect("form");
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step(
                    "choose_mode",
                    "make it a dispatcher",
                    &[("mode", "dispatcher")],
                ),
                step("choose_agent_type", "use codex", &[("agent_type", "codex")]),
                step(
                    "name_new_agent",
                    "name it triage bot",
                    &[("prefix", "name it")],
                ),
            ]),
        )
        .judging("make it a dispatcher", "choose_mode", true)
        .judging("use codex", "choose_agent_type", true)
        .judging("name it triage bot", "name_new_agent", true);
    let outcome = handle_utterance(
        &resolver,
        table(),
        Screen::Overview,
        &fleet(),
        &decks(),
        None,
        Some(&form),
        Transcript::new(said),
    )
    .await
    .outcome;
    let VoiceOutcome::Chain { steps, .. } = &outcome else {
        panic!("expected a chain, got {outcome:?}");
    };
    assert_eq!(
        steps
            .iter()
            .map(|step| step.action.as_str())
            .collect::<Vec<_>>(),
        ["choose_mode", "choose_agent_type", "name_new_agent"]
    );
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
        dashboard: None,
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

/// Scenario: in the New agent dialog's directory listing, "select docs and
/// dot-agent-deck directories" is answered as the same directory step twice.
/// It is offered as one numbered choice between the two directories, not
/// run as two steps.
#[tokio::test]
async fn voice_chain_two_values_for_one_choice_are_offered_as_a_choice() {
    let said = "select docs and dot-agent-deck directories";
    let listing: super::VoiceDirectories = serde_json::from_value(serde_json::json!({
        "deckId": "deck-local", "path": "/home/dev/code", "hasParent": true,
        "entries": [
            { "name": "docs", "path": "/home/dev/code/docs" },
            { "name": "dot-agent-deck", "path": "/home/dev/code/dot-agent-deck" },
            { "name": "infra", "path": "/home/dev/code/infra" },
        ],
    }))
    .expect("listing");
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step("open_dir", "select docs", &[("dir", "docs")]),
                step(
                    "open_dir",
                    "dot-agent-deck directories",
                    &[("dir", "dot-agent-deck")],
                ),
            ]),
        )
        .judging("select docs", "open_dir", true)
        .judging("dot-agent-deck directories", "open_dir", true);
    let outcome = handle_utterance(
        &resolver,
        table(),
        Screen::Overview,
        &fleet(),
        &decks(),
        Some(&listing),
        None,
        Transcript::new(said),
    )
    .await
    .outcome;
    let VoiceOutcome::ParamAmbiguous {
        action,
        candidates,
        sentence,
        ..
    } = &outcome
    else {
        panic!("expected a choice, got {outcome:?}");
    };
    assert_eq!(action, "open_dir");
    assert_eq!(
        candidates
            .iter()
            .map(|value| value.label.as_str())
            .collect::<Vec<_>>(),
        ["docs", "dot-agent-deck"]
    );
    assert!(sentence.contains("which one"), "{sentence}");
}

/// Scenario: "open the overview and then the tester" with two agents called
/// tester. The second step matched both, so the chain is answered with that
/// step carrying the numbered choice it pauses on; the first step is
/// unchanged.
#[tokio::test]
async fn voice_chain_an_ambiguous_step_pauses_the_chain_on_a_choice() {
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
    let fleet = vec![role_agent("1", "tester"), role_agent("2", "tester")];
    let outcome = handle_utterance(
        &resolver,
        table(),
        Screen::Deck,
        &fleet,
        &decks(),
        None,
        None,
        Transcript::new(said),
    )
    .await
    .outcome;
    let VoiceOutcome::Chain { steps, .. } = &outcome else {
        panic!("expected a chain, got {outcome:?}");
    };
    assert!(steps[0].choice.is_none());
    let Some(choice) = steps[1].choice.as_deref() else {
        panic!("expected step 2 to carry a choice: {steps:?}");
    };
    assert!(
        matches!(choice, VoiceOutcome::ParamAmbiguous { candidates, .. } if candidates.len() == 2),
        "{choice:?}"
    );
}

/// Scenario: "show me the overview, then stop the tester" on the Daemons
/// screen. The stop may be the last step, and it is answered as the same
/// confirmation opener a stop said on its own is, so nothing stops until the
/// user confirms by hand.
#[tokio::test]
async fn voice_chain_a_stop_as_the_last_step_ends_at_its_confirmation() {
    let said = "show me the overview, then stop the tester";
    let resolver = StubResolver::new()
        .answering(
            said,
            chain(vec![
                step("open_overview", "show me the overview", &[]),
                step("stop_agent", "stop the tester", &[("agent", "tester")]),
            ]),
        )
        .judging("show me the overview", "open_overview", true)
        .judging("stop the tester", "stop_agent", true);
    let outcome = run(&resolver, Screen::Deck, said).await;
    let VoiceOutcome::Chain { steps, .. } = &outcome else {
        panic!("expected a chain, got {outcome:?}");
    };
    assert_eq!(steps[1].invoke, "confirmStopAgent");
    assert_eq!(steps[1].screen, Screen::Overview);
}

/// Scenario (Qodo on PR #2417): a backend answers "open the overview and then
/// the tester" with `none` at the top level but two steps. The steps are
/// judged on their own, so the chain runs as if the top level had been the
/// first step, rather than being dropped as no match.
#[tokio::test]
async fn voice_chain_steps_under_a_none_top_level_are_still_judged() {
    let said = "open the overview and then the tester";
    let resolver = StubResolver::new()
        .answering(
            said,
            IntentAnswer::none().with_steps(vec![
                step("open_overview", "open the overview", &[]),
                step("open_agent", "the tester", &[("agent", "tester")]),
            ]),
        )
        .judging("open the overview", "open_overview", true)
        .judging("the tester", "open_agent", true);
    let outcome = run(&resolver, Screen::Deck, said).await;
    assert!(matches!(outcome, VoiceOutcome::Chain { .. }), "{outcome:?}");
}

/// Scenario (Qodo and Greptile on PR #2417): a chain whose daemon switch
/// pauses on a choice between two daemons. Addressing the outcome gives the
/// choice's candidates the Daemon selector's tokens, as a single switch's
/// are; when one candidate has no token, the choice is not offered at all.
#[test]
fn voice_chain_a_paused_switch_offers_selector_tokens() {
    use super::outcome::{ChainStep, ResolvedParam, VoiceDeckSelection, address_deck_switch};
    use super::table::ParamKind;
    let candidate = |value: &str, label: &str| ResolvedParam {
        name: "deck".to_string(),
        kind: ParamKind::DeckRef,
        spoken: "build".to_string(),
        value: value.to_string(),
        label: label.to_string(),
        deck_identity: None,
        deck_id: None,
        names: Vec::new(),
    };
    let chain_with = |candidates: Vec<ResolvedParam>| VoiceOutcome::Chain {
        transcript: Transcript::new("open the overview, then switch to build"),
        steps: vec![ChainStep {
            said: "switch to build".to_string(),
            action: "switch_deck".to_string(),
            invoke: "switchDeck".to_string(),
            params: Vec::new(),
            sentence: String::new(),
            screen: Screen::Overview,
            choice: Some(Box::new(VoiceOutcome::ParamAmbiguous {
                transcript: Transcript::new("switch to build"),
                action: "switch_deck".to_string(),
                invoke: "switchDeck".to_string(),
                param: "deck".to_string(),
                spoken: "build".to_string(),
                matches: vec!["build-one".to_string(), "build-two".to_string()],
                reports: vec![
                    "Showing build-one.".to_string(),
                    "Showing build-two.".to_string(),
                ],
                candidates,
                params: Vec::new(),
                sentence: "which daemon".to_string(),
            })),
        }],
        sentence: String::new(),
    };
    let tokens = |id: &str| match id {
        "deck-one" => Some(VoiceDeckSelection {
            token: "row-one".to_string(),
            identity: None,
        }),
        "deck-two" => Some(VoiceDeckSelection {
            token: "row-two".to_string(),
            identity: None,
        }),
        _ => None,
    };
    let choice_of = |outcome: &VoiceOutcome| match outcome {
        VoiceOutcome::Chain { steps, .. } => match steps[0].choice.as_deref() {
            Some(VoiceOutcome::ParamAmbiguous { candidates, .. }) => candidates
                .iter()
                .map(|value| value.value.clone())
                .collect::<Vec<_>>(),
            other => panic!("expected a choice, got {other:?}"),
        },
        other => panic!("expected a chain, got {other:?}"),
    };

    let mut addressed = chain_with(vec![
        candidate("deck-one", "build-one"),
        candidate("deck-two", "build-two"),
    ]);
    address_deck_switch(&mut addressed, tokens);
    assert_eq!(choice_of(&addressed), ["row-one", "row-two"]);

    let mut unlisted = chain_with(vec![
        candidate("deck-one", "build-one"),
        candidate("deck-gone", "gone"),
    ]);
    address_deck_switch(&mut unlisted, tokens);
    assert!(choice_of(&unlisted).is_empty());
}
