//! Tests for PRD #1261's local answer to a numbered voice choice.

use super::choice::{ChoiceAnswer, ChoiceLive, answer};
use super::outcome::ResolvedParam;
use super::table::ParamKind;
use super::{VoiceDeck, VoiceDirectories, VoiceDirectoryEntry};
use crate::voice::fixtures::role_agent;

fn candidate(kind: ParamKind, value: &str, label: &str) -> ResolvedParam {
    ResolvedParam {
        name: match kind {
            ParamKind::AgentRef => "agent",
            ParamKind::DeckRef => "deck",
            ParamKind::DirRef => "dir",
            ParamKind::OrchestrationRef => "orchestration",
            ParamKind::ModeRef => "mode",
            ParamKind::AgentTypeRef => "agent_type",
            ParamKind::SpokenPrefix => panic!("a spoken prefix cannot be an offered choice"),
        }
        .to_string(),
        kind,
        spoken: "several".to_string(),
        value: value.to_string(),
        label: label.to_string(),
        deck_identity: None,
    }
}

fn listing() -> VoiceDirectories {
    VoiceDirectories {
        deck_id: "local".to_string(),
        path: "/code".to_string(),
        has_parent: true,
        entries: ["docs-site", "docs-api", "src"]
            .into_iter()
            .map(|name| VoiceDirectoryEntry {
                name: name.to_string(),
                path: format!("/code/{name}"),
            })
            .collect(),
    }
}

fn directories() -> Vec<ResolvedParam> {
    ["docs-site", "docs-api", "src"]
        .into_iter()
        .map(|name| candidate(ParamKind::DirRef, &format!("/code/{name}"), name))
        .collect()
}

/// Scenario: whole ordinals select only a numbered entry that was offered.
/// A sentence containing an ordinal and an out-of-range number never select.
#[test]
fn choice_answers_whole_ordinals_and_refuses_out_of_range() {
    let offered = directories();
    let listing = listing();
    let live = ChoiceLive {
        agents: &[],
        decks: &[],
        directories: Some(&listing),
        new_agent: None,
    };
    for (said, index) in [
        ("one", 0),
        ("1", 0),
        ("two", 1),
        ("the second one", 1),
        ("number 3", 2),
        ("the last one", 2),
    ] {
        assert_eq!(
            answer(said, &offered, &live),
            ChoiceAnswer::Selected(offered[index].clone()),
            "{said}"
        );
    }
    assert_eq!(answer("number 4", &offered, &live), ChoiceAnswer::Refused);
    assert_eq!(
        answer("open the second tab", &offered, &live),
        ChoiceAnswer::NotAnswer
    );
}

/// Scenario: a spoken name selects one of the offered entries only. A live but
/// unoffered directory or a shared name is refused, and a command containing
/// an offered agent's name remains a new command rather than an answer.
#[test]
fn choice_names_are_checked_against_the_offered_list_only() {
    let listing = listing();
    let live = ChoiceLive {
        agents: &[],
        decks: &[],
        directories: Some(&listing),
        new_agent: None,
    };
    let offered = directories()[..2].to_vec();
    assert_eq!(
        answer("docs-api", &offered, &live),
        ChoiceAnswer::Selected(offered[1].clone())
    );
    assert_eq!(answer("src", &offered, &live), ChoiceAnswer::Refused);
    assert_eq!(answer("docs", &offered, &live), ChoiceAnswer::Refused);

    let agents = [
        role_agent("planner", "Planner"),
        role_agent("builder", "Desktop implementation"),
    ];
    let live_agents = ChoiceLive {
        agents: &agents,
        decks: &[],
        directories: None,
        new_agent: None,
    };
    let agent_offers = [
        candidate(ParamKind::AgentRef, "planner", "Planner"),
        candidate(ParamKind::AgentRef, "builder", "Desktop implementation"),
    ];
    assert_eq!(
        answer("Planner", &agent_offers, &live_agents),
        ChoiceAnswer::Selected(agent_offers[0].clone())
    );
    assert_eq!(
        [
            answer("stop Planner", &agent_offers, &live_agents),
            answer("desktop implementation extra", &agent_offers, &live_agents),
        ],
        [ChoiceAnswer::NotAnswer, ChoiceAnswer::NotAnswer]
    );
}

/// Scenario: an offered agent is literally named like a bare ordinal or a
/// cancel phrase. Those readings are ambiguous and refuse rather than picking
/// another agent or cancelling; an explicit numbered choice still selects.
#[test]
fn choice_refuses_labels_that_collide_with_bare_controls() {
    let agents = [
        role_agent("agent-two", "two"),
        role_agent("agent-other", "Other"),
    ];
    let live = ChoiceLive {
        agents: &agents,
        decks: &[],
        directories: None,
        new_agent: None,
    };
    let offered = [
        candidate(ParamKind::AgentRef, "agent-two", "two"),
        candidate(ParamKind::AgentRef, "agent-other", "Other"),
    ];
    assert_eq!(
        answer("number one", &offered, &live),
        ChoiceAnswer::Selected(offered[0].clone())
    );

    let cancel_agent = role_agent("agent-cancel", "cancel");
    let cancel_live = ChoiceLive {
        agents: std::slice::from_ref(&cancel_agent),
        decks: &[],
        directories: None,
        new_agent: None,
    };
    let cancel_offer = [candidate(ParamKind::AgentRef, "agent-cancel", "cancel")];
    assert_eq!(
        [
            answer("two", &offered, &live),
            answer("cancel", &cancel_offer, &cancel_live),
        ],
        [ChoiceAnswer::Refused, ChoiceAnswer::Refused]
    );
}

/// Scenario: whole cancel phrases dismiss an offer, while ordinary commands
/// remain ordinary utterances for the panel to resolve after closing it.
#[test]
fn choice_cancels_only_closed_whole_phrases() {
    let listing = listing();
    let offered = directories();
    let live = ChoiceLive {
        agents: &[],
        decks: &[],
        directories: Some(&listing),
        new_agent: None,
    };
    for said in ["cancel", "never mind", "none", "none of them", "no"] {
        assert_eq!(
            answer(said, &offered, &live),
            ChoiceAnswer::Cancelled,
            "{said}"
        );
    }
    assert_eq!(
        answer("do not cancel the build", &offered, &live),
        ChoiceAnswer::NotAnswer
    );
    assert_eq!(answer("type on", &offered, &live), ChoiceAnswer::NotAnswer);
}

/// Scenario: an ordinal pointing at a former agent, deck or orchestration is
/// refused after that value disappears; the old list cannot act on new state.
#[test]
fn choice_refuses_an_offered_value_that_is_no_longer_live() {
    let agent = candidate(ParamKind::AgentRef, "agent-1", "Atlas");
    let deck = candidate(ParamKind::DeckRef, "deck-1", "ops@build-box");
    let run = candidate(ParamKind::OrchestrationRef, "run-member-1", "Review");
    let absent = ChoiceLive {
        agents: &[],
        decks: &[],
        directories: None,
        new_agent: None,
    };
    for offered in [&agent, &deck, &run] {
        assert_eq!(
            answer("one", std::slice::from_ref(offered), &absent),
            ChoiceAnswer::Refused,
            "{}",
            offered.label
        );
    }

    let agents = [role_agent("agent-1", "Atlas")];
    let decks = [VoiceDeck {
        id: "deck-1".to_string(),
        label: "ops@build-box".to_string(),
        local: false,
        unavailable: None,
    }];
    let live = ChoiceLive {
        agents: &agents,
        decks: &decks,
        directories: None,
        new_agent: None,
    };
    assert_eq!(
        answer("one", std::slice::from_ref(&agent), &live),
        ChoiceAnswer::Selected(agent)
    );
    assert_eq!(
        answer("one", std::slice::from_ref(&deck), &live),
        ChoiceAnswer::Selected(deck)
    );
}
