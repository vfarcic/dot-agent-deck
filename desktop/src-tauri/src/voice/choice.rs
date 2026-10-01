//! PRD #1261 — the local answer to a numbered voice choice.
//!
//! When a required param is a genuine tie, [`super::VoiceOutcome::ParamAmbiguous`]
//! carries its candidates and the voice panel offers them as a numbered list.
//! The next utterance is answered HERE, with no Commands backend call: no model
//! is asked anything, so no observed name can steer the answer.
//!
//! # The answer is checked against the OFFERED list
//!
//! Picking "two" is not a reference the transcript supports in the usual sense,
//! so it is validated against the list this app put on screen, not against the
//! utterance. **That is a different check from reference grounding** (removed
//! 2026-09-24 — `voice-first-design.md` §6) and from action grounding
//! (`action_grounded`): it asks *is this answer one of the entries offered, and
//! does it name exactly one?* It does not reinstate reference grounding, and it
//! must not be skipped because "grounding was removed".
//!
//! The original ACTION is not re-grounded here either: it was held to the
//! transcript when the first utterance resolved, and the answer supplies only
//! the value.

use serde::Serialize;

use super::outcome::{
    AgentRefMatch, ChoiceMatch, DeckRefMatch, DirRefMatch, ResolvedParam, agent_type_names,
    deck_spoken_names, dir_names, mode_names, resolve_agent_ref, resolve_agent_type_ref,
    resolve_deck_ref, resolve_dir_ref, resolve_mode_ref, resolve_orchestration_ref, spoken_names,
    whole_utterance,
};
use super::table::{ParamKind, spoken_words};
use super::{DesktopAgent, VoiceChoice, VoiceDeck, VoiceDirectories, VoiceNewAgent};
use crate::dto::DesktopTab;

/// The most candidates a tie is offered as a choice with. Single-digit
/// ordinals are what can be said reliably and shown in one row; a longer tie
/// keeps today's sentence, which already summarises "and N more". A starting
/// value, to revisit with use (PRD #1261, Open Question 3).
pub const MAX_CHOICES: usize = 9;

/// The whole utterances that cancel a choice. Closed and whole-utterance, so
/// "do not cancel the build" is a command rather than a cancel.
const CANCEL_PHRASES: [&str; 8] = [
    "cancel",
    "cancel that",
    "never mind",
    "nevermind",
    "none",
    "none of them",
    "neither",
    "no",
];

/// Words that introduce an ordinal without being one: "number two", "option
/// 2", "choice three".
const ORDINAL_LEADS: [&str; 5] = ["number", "option", "choice", "entry", "item"];

/// Words a NAME answer may carry around the name without saying anything
/// else: an article. [`kind_nouns`] adds the noun for what is being chosen.
const ARTICLES: [&str; 3] = ["the", "a", "an"];

/// The noun for what a choice of `kind` is choosing — "the review
/// orchestration", "the docs folder" — which an answer may say beside a name.
fn kind_nouns(kind: ParamKind) -> &'static [&'static str] {
    match kind {
        ParamKind::AgentRef => &["agent"],
        ParamKind::DeckRef => &["deck", "daemon"],
        ParamKind::DirRef => &["folder", "directory"],
        ParamKind::OrchestrationRef => &["orchestration", "run"],
        ParamKind::ModeRef => &["mode", "chip"],
        ParamKind::AgentTypeRef => &["agent", "type"],
        ParamKind::SpokenPrefix => &[],
    }
}

/// Whether the answer `words` say `name` and nothing else (PR #1451 review):
/// every word is a word of the name, an article or the kind's noun, and at
/// least one is the name's. "stop Planner" does not cover "Planner" — it is
/// a new command that happens to contain an offered name, and answering the
/// choice with it would run the ORIGINAL action on Planner instead of the
/// stop the user asked for. Neither does a name followed by extra words.
fn covers(words: &[String], name: &str, kind: ParamKind) -> bool {
    let name = spoken_words(name);
    let filler = |word: &String| {
        ARTICLES.contains(&word.as_str()) || kind_nouns(kind).contains(&word.as_str())
    };
    words.iter().any(|word| name.contains(word))
        && words.iter().all(|word| name.contains(word) || filler(word))
}

/// The live state an answer is checked against — what the app observes NOW,
/// not what it observed when the choice was offered.
#[derive(Debug, Clone, Copy)]
pub struct ChoiceLive<'a> {
    pub agents: &'a [DesktopAgent],
    pub decks: &'a [VoiceDeck],
    pub directories: Option<&'a VoiceDirectories>,
    pub new_agent: Option<&'a VoiceNewAgent>,
}

/// What one utterance says about a pending choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "candidate", rename_all = "snake_case")]
// One answer per utterance, returned once and dropped: boxing the candidate
// would buy nothing but an allocation.
#[allow(clippy::large_enum_variant)]
pub enum ChoiceAnswer {
    /// This offered candidate, exactly as offered, and still live.
    Selected(ResolvedParam),
    /// An answer, but not one that can be acted on: an ordinal outside the
    /// list, a name matching several of the offered entries or only something
    /// that was not offered, an entry that is no longer live, or a bare
    /// ordinal or cancel phrase that is also an offered entry's label.
    Refused,
    /// Not an answer at all. The panel closes the choice and resolves the
    /// utterance as an ordinary one.
    NotAnswer,
    /// One of [`CANCEL_PHRASES`], said on its own.
    Cancelled,
}

/// Answer `utterance` against `offered`, in this order: a cancel phrase, a
/// whole-utterance ordinal, then a name said on its own ([`covers`]) that
/// resolves among the offered candidates alone, with the resolver their kind
/// already uses. Anything else — including a command that contains an offered
/// name — is [`ChoiceAnswer::NotAnswer`].
///
/// A bare control that is ALSO, word for word, an offered entry's label or one
/// of its other spoken names ([`names_of`]) — an agent named "two", or whose
/// role is "cancel" — is [`ChoiceAnswer::Refused`] before either
/// reading is taken: acting on the number would pick another entry, and
/// cancelling would drop the one the user named. An explicit ordinal with a
/// lead, "number two", is not the label and stays a number. The webview names
/// the collision in its refusal (`collidingChoiceEntry` in `voiceChoice.ts`).
///
/// A selected candidate is re-checked against `live` before it is returned, so
/// an entry whose agent, deck, orchestration, directory or chip has gone since
/// the offer is refused rather than acted on.
pub fn answer(utterance: &str, offered: &[ResolvedParam], live: &ChoiceLive) -> ChoiceAnswer {
    let words = whole_utterance(utterance);
    if words.is_empty() {
        return ChoiceAnswer::NotAnswer;
    }
    let cancels = CANCEL_PHRASES.contains(&words.join(" ").as_str());
    let ordinal = ordinal(&words);
    // The label, and every other name the entry answers to (an agent's role,
    // CLI name or id, a deck's host …): an agent labelled "Builder" whose
    // role is "two" collides with a bare "two" as surely as one labelled
    // "two" does (Qodo on PR #1451).
    if (cancels || ordinal.is_some())
        && offered.iter().any(|candidate| {
            spoken_words(&candidate.label) == words
                || names_of(candidate.kind, &candidate.value, live)
                    .iter()
                    .any(|name| spoken_words(name) == words)
        })
    {
        return ChoiceAnswer::Refused;
    }
    if cancels {
        return ChoiceAnswer::Cancelled;
    }
    if let Some(ordinal) = ordinal {
        return match ordinal.index(offered.len()).map(|at| &offered[at]) {
            Some(candidate) => still_live(candidate, live),
            None => ChoiceAnswer::Refused,
        };
    }
    by_name(utterance, &words, offered, live)
}

/// A whole-utterance ordinal. Shared beyond the choice (PR #1451, round 3)
/// so a bare number said against any numbered list on screen is read by the
/// same rules as an answer to a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ordinal {
    /// The 1-based position said: "two", "2", "the second".
    Nth(usize),
    /// "the last one".
    Last,
}

impl Ordinal {
    /// The 0-based index this ordinal names in a list of `len` entries, or
    /// `None` when it names none of them: "zero", or past the end.
    pub(crate) fn index(self, len: usize) -> Option<usize> {
        let at = match self {
            Ordinal::Last => len.checked_sub(1),
            Ordinal::Nth(number) => number.checked_sub(1),
        };
        at.filter(|&at| at < len)
    }
}

/// `words` read as an ordinal, or `None` when they are anything else: "two",
/// "2", "number two", "option 2", "the second", "the second one", "2nd", "the
/// last one". Whole-utterance, so "open the second tab" is not one; `words`
/// are what [`whole_utterance`] makes of the transcript.
pub(crate) fn ordinal(words: &[String]) -> Option<Ordinal> {
    let mut rest: &[String] = words;
    if rest.first().is_some_and(|word| word == "the") {
        rest = &rest[1..];
    }
    if rest
        .first()
        .is_some_and(|word| ORDINAL_LEADS.contains(&word.as_str()))
    {
        rest = &rest[1..];
    }
    // "the second one": a trailing "one" after an ordinal word is filler. On
    // its own, "one" is the number.
    if rest.len() == 2 && rest[1] == "one" {
        rest = &rest[..1];
    }
    let [word] = rest else {
        return None;
    };
    const CARDINALS: [&str; 9] = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ];
    const ORDINALS: [&str; 9] = [
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
    ];
    if word == "last" {
        return Some(Ordinal::Last);
    }
    let named = |list: &[&str]| list.iter().position(|entry| entry == word);
    if let Some(at) = named(&CARDINALS).or_else(|| named(&ORDINALS)) {
        return Some(Ordinal::Nth(at + 1));
    }
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| word.strip_suffix(suffix))
        .unwrap_or(word);
    digits.parse::<usize>().ok().map(Ordinal::Nth)
}

/// The name half of [`answer`]: the utterance resolved among the offered
/// candidates that are still live AND that it [`covers`] — the whole utterance
/// is one of that entry's names, or part of one, with nothing else said — with
/// their kind's own resolver. One → selected; several → refused. None →
/// refused when the whole utterance IS the name of something on screen that
/// was not offered, or of an offered entry that has gone — an answer, just not
/// one that can be acted on — and otherwise not an answer at all.
///
/// The [`covers`] filter is what keeps a command from answering (PR #1451
/// review): each resolver's loose pass also accepts a reference that merely
/// CONTAINS a name, which is right for resolving a command's param and wrong
/// here, where "stop Planner" must close the choice and be resolved as the
/// stop it is.
fn by_name(
    utterance: &str,
    words: &[String],
    offered: &[ResolvedParam],
    live: &ChoiceLive,
) -> ChoiceAnswer {
    let Some(kind) = offered.first().map(|candidate| candidate.kind) else {
        return ChoiceAnswer::NotAnswer;
    };
    let offers = |value: &str| offered.iter().any(|candidate| candidate.value == value);
    let said = |names: Vec<String>| names.iter().any(|name| covers(words, name, kind));
    // An offered entry is said by its offered label as well as by its own
    // names.
    let said_offered = |value: &str, mut names: Vec<String>| {
        offered
            .iter()
            .find(|candidate| candidate.value == value)
            .is_some_and(|candidate| {
                names.push(candidate.label.clone());
                said(names)
            })
    };
    let found = match kind {
        ParamKind::AgentRef => {
            let agents: Vec<DesktopAgent> = live
                .agents
                .iter()
                .filter(|agent| said_offered(&agent.id, spoken_names(agent)))
                .cloned()
                .collect();
            match resolve_agent_ref(utterance, &agents) {
                AgentRefMatch::One { id, .. } => Found::One(id),
                AgentRefMatch::None => Found::None,
                AgentRefMatch::Ambiguous(_) => Found::Several,
            }
        }
        ParamKind::DeckRef => {
            let decks: Vec<VoiceDeck> = live
                .decks
                .iter()
                .filter(|deck| said_offered(&deck.id, deck_spoken_names(deck)))
                .cloned()
                .collect();
            match resolve_deck_ref(utterance, &decks) {
                DeckRefMatch::One { id, .. } => Found::One(id),
                DeckRefMatch::None => Found::None,
                DeckRefMatch::Ambiguous(_) => Found::Several,
            }
        }
        ParamKind::DirRef => {
            let listing = live.directories.map(|listing| VoiceDirectories {
                entries: listing
                    .entries
                    .iter()
                    .filter(|entry| said_offered(&entry.path, dir_names(&entry.name)))
                    .cloned()
                    .collect(),
                ..listing.clone()
            });
            match resolve_dir_ref(utterance, listing.as_ref()) {
                DirRefMatch::One { path, .. } => Found::One(path),
                DirRefMatch::None => Found::None,
                DirRefMatch::Ambiguous(_) => Found::Several,
            }
        }
        ParamKind::OrchestrationRef => {
            let agents: Vec<DesktopAgent> = live
                .agents
                .iter()
                .filter(|agent| {
                    (offers(&agent.id) || in_an_offered_run(agent, live, &offers))
                        && said(orchestration_names(agent, offered))
                })
                .cloned()
                .collect();
            choice_found(resolve_orchestration_ref(utterance, &agents))
        }
        ParamKind::ModeRef | ParamKind::AgentTypeRef => {
            let choices: Vec<VoiceChoice> = form_choices(kind, live)
                .iter()
                .filter(|choice| {
                    let names = if kind == ParamKind::ModeRef {
                        mode_names(choice)
                    } else {
                        agent_type_names(choice)
                    };
                    said_offered(&choice.id, names)
                })
                .cloned()
                .collect();
            choice_found(if kind == ParamKind::ModeRef {
                resolve_mode_ref(utterance, &choices)
            } else {
                resolve_agent_type_ref(utterance, &choices)
            })
        }
        ParamKind::SpokenPrefix => Found::None,
    };
    match found {
        Found::One(value) => match offered.iter().find(|candidate| candidate.value == value) {
            Some(candidate) => still_live(candidate, live),
            None => ChoiceAnswer::Refused,
        },
        Found::Several => ChoiceAnswer::Refused,
        Found::None if names_something(words, kind, offered, live) => ChoiceAnswer::Refused,
        Found::None => ChoiceAnswer::NotAnswer,
    }
}

/// What a name resolved to among the offered, live candidates.
enum Found {
    One(String),
    Several,
    None,
}

fn choice_found(matched: ChoiceMatch) -> Found {
    match matched {
        ChoiceMatch::One { id, .. } => Found::One(id),
        ChoiceMatch::None => Found::None,
        ChoiceMatch::Ambiguous(_) => Found::Several,
    }
}

/// Whether `agent` belongs to the same orchestration as an offered card's
/// member — so a card resolves by its title with every member present, and
/// its `member_id` stays the one offered.
fn in_an_offered_run(
    agent: &DesktopAgent,
    live: &ChoiceLive,
    offers: &impl Fn(&str) -> bool,
) -> bool {
    let run_of = |agent: &DesktopAgent| match &agent.tab {
        DesktopTab::Orchestration {
            orchestration_id: Some(id),
            ..
        } => Some(id.clone()),
        _ => None,
    };
    let Some(run) = run_of(agent) else {
        return false;
    };
    live.agents
        .iter()
        .any(|member| offers(&member.id) && run_of(member).as_deref() == Some(run.as_str()))
}

/// Every name the live entry `value` of `kind` answers to, by the same per-kind
/// name function [`by_name`] matches an answer with — [`spoken_names`] for an
/// agent, [`deck_spoken_names`], [`dir_names`], [`run_names`],
/// [`mode_names`] and [`agent_type_names`] for the rest. This is what an
/// offered candidate carries as [`ResolvedParam::names`], so the webview's
/// fallback (`answerChoiceLocally`) reads these names rather than keeping a
/// second definition of them. Empty when nothing live has that value.
pub(super) fn names_of(kind: ParamKind, value: &str, live: &ChoiceLive) -> Vec<String> {
    let names = match kind {
        ParamKind::AgentRef => live
            .agents
            .iter()
            .find(|agent| agent.id == value)
            .map(spoken_names),
        ParamKind::DeckRef => live
            .decks
            .iter()
            .find(|deck| deck.id == value)
            .map(deck_spoken_names),
        ParamKind::DirRef => live
            .directories
            .and_then(|listing| listing.entries.iter().find(|entry| entry.path == value))
            .map(|entry| dir_names(&entry.name)),
        ParamKind::OrchestrationRef => live
            .agents
            .iter()
            .find(|agent| agent.id == value)
            .map(run_names),
        ParamKind::ModeRef => form_choices(kind, live)
            .iter()
            .find(|choice| choice.id == value)
            .map(mode_names),
        ParamKind::AgentTypeRef => form_choices(kind, live)
            .iter()
            .find(|choice| choice.id == value)
            .map(agent_type_names),
        ParamKind::SpokenPrefix => None,
    };
    names.unwrap_or_default()
}

/// The names an orchestration member's card answers to: its config name and
/// run title.
fn run_names(agent: &DesktopAgent) -> Vec<String> {
    let mut names = Vec::new();
    if let DesktopTab::Orchestration {
        name,
        display_title,
        ..
    } = &agent.tab
    {
        names.push(name.clone());
        names.extend(display_title.clone());
    }
    names
}

/// [`run_names`], plus the label its entry was offered under, when it is the
/// member offered.
fn orchestration_names(agent: &DesktopAgent, offered: &[ResolvedParam]) -> Vec<String> {
    let mut names = run_names(agent);
    names.extend(
        offered
            .iter()
            .filter(|candidate| candidate.value == agent.id)
            .map(|candidate| candidate.label.clone()),
    );
    names
}

/// The New agent form's Mode chips or agent entries, as declared now.
fn form_choices<'a>(kind: ParamKind, live: &ChoiceLive<'a>) -> &'a [VoiceChoice] {
    match live.new_agent.and_then(|dialog| dialog.form.as_ref()) {
        Some(form) if kind == ParamKind::ModeRef => &form.modes,
        Some(form) => &form.agent_types,
        None => &[],
    }
}

/// Whether the whole utterance is, word for word, the label of an offered
/// entry or of something of the same kind on screen now. Such an utterance is
/// an answer — naming what was not offered, or what has gone — and is refused;
/// a sentence that merely contains a name is not, and goes on to be resolved
/// as the ordinary command it probably is.
fn names_something(
    words: &[String],
    kind: ParamKind,
    offered: &[ResolvedParam],
    live: &ChoiceLive,
) -> bool {
    let is = |label: &str| spoken_words(label) == words;
    if offered.iter().any(|candidate| is(&candidate.label)) {
        return true;
    }
    match kind {
        ParamKind::AgentRef => live
            .agents
            .iter()
            .any(|agent| spoken_names(agent).iter().any(|name| is(name))),
        // A named deck (issue #1426) is still said by its address, which is
        // what its label was before it had a name.
        ParamKind::DeckRef => live
            .decks
            .iter()
            .any(|deck| is(&deck.label) || deck.address.as_deref().is_some_and(is)),
        ParamKind::DirRef => live
            .directories
            .is_some_and(|listing| listing.entries.iter().any(|entry| is(&entry.name))),
        ParamKind::OrchestrationRef => live.agents.iter().any(|agent| match &agent.tab {
            DesktopTab::Orchestration {
                name,
                display_title,
                ..
            } => is(name) || display_title.as_deref().is_some_and(is),
            _ => false,
        }),
        ParamKind::ModeRef | ParamKind::AgentTypeRef => form_choices(kind, live)
            .iter()
            .any(|choice| is(&choice.label)),
        ParamKind::SpokenPrefix => false,
    }
}

/// `candidate`, selected — if what it names is still on screen. A deck is
/// looked up by the value offered, which for a switch is the Deck selector's
/// token: the caller hands in decks keyed the same way.
fn still_live(candidate: &ResolvedParam, live: &ChoiceLive) -> ChoiceAnswer {
    let value = candidate.value.as_str();
    let present = match candidate.kind {
        ParamKind::AgentRef => live.agents.iter().any(|agent| agent.id == value),
        ParamKind::DeckRef => live.decks.iter().any(|deck| deck.id == value),
        ParamKind::OrchestrationRef => live.agents.iter().any(|agent| {
            agent.id == value && matches!(agent.tab, DesktopTab::Orchestration { .. })
        }),
        ParamKind::DirRef => live
            .directories
            .is_some_and(|listing| listing.entries.iter().any(|entry| entry.path == value)),
        ParamKind::ModeRef | ParamKind::AgentTypeRef => form_choices(candidate.kind, live)
            .iter()
            .any(|choice| choice.id == value),
        ParamKind::SpokenPrefix => false,
    };
    if present {
        ChoiceAnswer::Selected(candidate.clone())
    } else {
        ChoiceAnswer::Refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fixtures::role_agent;

    fn agent_candidate(id: &str, label: &str) -> ResolvedParam {
        ResolvedParam {
            name: "agent".to_string(),
            kind: ParamKind::AgentRef,
            spoken: "atlas".to_string(),
            value: id.to_string(),
            label: label.to_string(),
            deck_identity: None,
            names: Vec::new(),
        }
    }

    /// Scenario: the webview reads `{kind, candidate}`, and a selected answer
    /// carries the offered entry in the same camelCase shape it was sent in.
    #[test]
    fn choice_answer_serializes_the_shape_the_webview_reads() {
        let chosen = agent_candidate("atlas-b", "Atlas");
        assert_eq!(
            serde_json::to_value(ChoiceAnswer::Selected(chosen)).expect("serializes"),
            serde_json::json!({
                "kind": "selected",
                "candidate": {
                    "name": "agent", "kind": "agent_ref", "spoken": "atlas",
                    "value": "atlas-b", "label": "Atlas"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(ChoiceAnswer::NotAnswer).expect("serializes"),
            serde_json::json!({ "kind": "not_answer" })
        );
    }

    /// Scenario: two agents both shown as Atlas are offered. "two", "option
    /// 2", "2nd" and "okay the second one please" each pick the second VALUE;
    /// "Atlas" by name names both and is refused rather than guessed.
    #[test]
    fn choice_ordinals_pick_values_where_labels_collide() {
        let agents = [
            role_agent("atlas-a", "Atlas"),
            role_agent("atlas-b", "Atlas"),
        ];
        let offered = [
            agent_candidate("atlas-a", "Atlas"),
            agent_candidate("atlas-b", "Atlas"),
        ];
        let live = ChoiceLive {
            agents: &agents,
            decks: &[],
            directories: None,
            new_agent: None,
        };
        for said in [
            "two",
            "option 2",
            "2nd",
            "okay the second one please",
            "last",
        ] {
            assert_eq!(
                answer(said, &offered, &live),
                ChoiceAnswer::Selected(offered[1].clone()),
                "{said}"
            );
        }
        assert_eq!(answer("Atlas", &offered, &live), ChoiceAnswer::Refused);
        assert_eq!(answer("zero", &offered, &live), ChoiceAnswer::NotAnswer);
        assert_eq!(answer("number 0", &offered, &live), ChoiceAnswer::Refused);
        assert_eq!(answer("", &offered, &live), ChoiceAnswer::NotAnswer);
    }

    /// Scenario: a sentence that merely contains an agent's name — "open the
    /// tester" while two Atlas agents are offered — is not an answer: it
    /// closes the choice and is resolved as the command it is. Saying just
    /// "tester" is an answer naming what was not offered, and is refused.
    #[test]
    fn choice_a_command_naming_an_unoffered_agent_is_not_an_answer() {
        let agents = [
            role_agent("atlas-a", "Atlas"),
            role_agent("atlas-b", "Atlas"),
            role_agent("tester", "tester"),
        ];
        let offered = [
            agent_candidate("atlas-a", "Atlas"),
            agent_candidate("atlas-b", "Atlas"),
        ];
        let live = ChoiceLive {
            agents: &agents,
            decks: &[],
            directories: None,
            new_agent: None,
        };
        assert_eq!(
            answer("open the tester", &offered, &live),
            ChoiceAnswer::NotAnswer
        );
        assert_eq!(answer("tester", &offered, &live), ChoiceAnswer::Refused);
    }
}
