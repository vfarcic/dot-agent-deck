//! PRD #1184 — voice command chains: which of an answer's steps the app
//! accepts before anything runs.
//!
//! The Commands backend may answer an utterance that asks for two or three
//! things one after another with an ordered list of steps, each quoting the
//! words it came from ([`super::resolver::IntentStep`]). The model only MARKS
//! the steps. Everything a chain has to satisfy is decided here and in
//! `outcome.rs`'s chain path, and it fails closed: a chain that breaks any
//! rule runs nothing.
//!
//! # The rules, in the order they are applied
//!
//! 1. **At most [`MAX_STEPS`] steps.** More is refused with its own sentence;
//!    it never touches a one-step answer, which is the single answer the top
//!    level already carries.
//! 2. **A dictation anywhere is not a chain.** PRD #1184's M1 found dictation
//!    text split into steps ("type run the tests and then commit" as a
//!    dictation plus a start), and found the top-level answer right in every
//!    such case — so when the top level is the dictation, it is answered as
//!    the single dictation it is. A chain with a dictation step whose top
//!    level is something else is refused rather than run without its typing,
//!    since running only the first half would be a silent partial chain.
//! 3. **No word of contrast anywhere in the sentence** ([`CONTRAST_WORDS`]).
//!    "open settings, not the overview" is never two steps, and nor is "open
//!    the overview or the deck". Deliberately conservative: a false refusal
//!    costs one utterance, a wrong pair costs a user who cannot tell which
//!    half went wrong.
//! 4. **Every step's words are the user's own, and cover the sentence**
//!    ([`spans`]): each `said` is a run of the transcript's words, the runs
//!    are in order and do not overlap, only joining words ([`CONNECTIVES`])
//!    sit between them, and only a few filler words ([`EDGE_WORDS`]) before
//!    the first or after the last. Full coverage is load-bearing: without it
//!    M1 saw chains accepted that silently dropped their first step.
//! 5. **Each step's position is allowed** ([`position`]): only a step whose
//!    resulting screen can be predicted may come before another.
//!
//! What remains — each step held by the meaning check to its OWN words, and
//! each step valid on the screen the earlier ones would leave — needs the
//! meaning backend and the single-pick checks, and is `outcome.rs`'s.

use super::dictation::normalise;
use super::resolver::IntentStep;
use super::table::{CommandRow, CommandTable, Meaning, Screen};

/// The most steps one utterance may ask for. A starting value (PRD #1184's
/// open question): long enough for "make it a dispatcher, use codex and name
/// it triage bot", short enough to say back in one sentence.
pub const MAX_STEPS: usize = 3;

/// The words that may sit between two steps, as whole runs.
pub const CONNECTIVES: &[&[&str]] = &[
    &[],
    &["and"],
    &["then"],
    &["and", "then"],
    &["after", "that"],
    &["and", "after", "that"],
    &["next"],
    &["and", "next"],
];

/// The words that may come before the first step or after the last one
/// without belonging to either — politeness and filler. M1 measured that
/// requiring the rest of the sentence to be empty except for these refused
/// every chain that had silently dropped a step, at no cost to a correct one.
pub const EDGE_WORDS: &[&str] = &["please", "okay", "ok", "now", "just", "and", "then"];

/// Words that weigh one thing against another rather than ask for both. Any
/// of them anywhere in a chain's sentence refuses the chain — the closed
/// list PRD #1195 built for deck switching, as M1 scored it, normalised the
/// way [`normalise`] splits words (so "don't" is `don` and `t`).
pub const CONTRAST_WORDS: &[&str] = &[
    "not",
    "no",
    "nor",
    "never",
    "don",
    "dont",
    "doesn",
    "isn",
    "except",
    "but",
    "or",
    "instead",
    "rather",
    "than",
    "skip",
    "skipping",
    "avoid",
    "avoiding",
    "without",
    "exclude",
    "excluding",
    "besides",
];

/// The rows that run the dialog's or the pane's own irreversible step, or
/// type: never a step of a chain. A start stays a separate utterance (the
/// PRD's decision: the form would change and start in one breath, before the
/// user has seen the change), and every row the meaning check reads
/// [`Meaning::Whole`] is excluded by [`position`] as well, since a whole-
/// sentence row's evidence is that the sentence is nothing else.
pub const NEVER_IN_A_CHAIN: &[&str] = &["start_new_agent"];

/// The dictation rows, answered by rule 2 rather than as steps.
pub const DICTATION_ROWS: &[&str] = &["dictate_to_agent", "dictation_on", "dictation_off"];

/// Where the user stands after a step that may come before another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lands {
    /// The view moves to this screen. Predictable only with no New agent
    /// dialog open, which [`super::chain`]'s caller checks.
    View(Screen),
    /// Nothing moves: a choice in the New agent form, which stays open.
    Stays,
    /// The selected daemon changes. The screen stays, but the new daemon's
    /// agents are not loaded until it connects, so no later step may name an
    /// agent or an orchestration.
    DeckSwitched,
}

/// What a row may be in a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// Any step, including one with a step after it, landing as given.
    Before(Lands),
    /// Only the last step: what it leaves on screen cannot be predicted (a
    /// close, a listing that arrives from the daemon, Settings, after which
    /// "close" would mean Settings), or it ends at a confirmation.
    LastOnly,
    /// No step at all.
    Never,
}

/// A row's place in a chain.
pub fn position(row: &CommandRow) -> Position {
    if NEVER_IN_A_CHAIN.contains(&row.id.as_str())
        || DICTATION_ROWS.contains(&row.id.as_str())
        || row.meaning == Meaning::Whole
    {
        return Position::Never;
    }
    match row.id.as_str() {
        "open_overview" => Position::Before(Lands::View(Screen::Overview)),
        "open_deck" => Position::Before(Lands::View(Screen::Deck)),
        "open_agent" => Position::Before(Lands::View(Screen::Agent)),
        "switch_deck" => Position::Before(Lands::DeckSwitched),
        "choose_mode" | "choose_agent_type" | "name_new_agent" => Position::Before(Lands::Stays),
        _ => Position::LastOnly,
    }
}

/// The normalised words of `text`.
pub fn words(text: &str) -> Vec<String> {
    normalise(text)
        .split(' ')
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether the sentence carries a word of contrast (rule 3).
pub fn has_contrast(transcript: &str) -> bool {
    words(transcript)
        .iter()
        .any(|word| CONTRAST_WORDS.contains(&word.as_str()))
}

/// Why the steps' words could not be held to the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanError {
    /// A step's words are not a run of the transcript after the step before.
    NotSaid,
    /// Something other than joining words sits between two steps.
    Between,
    /// Words before the first step or after the last belong to no step.
    Uncovered,
}

/// Where each step's words are in the transcript, by word index, half-open
/// (rule 4).
pub fn spans(transcript: &str, steps: &[IntentStep]) -> Result<Vec<(usize, usize)>, SpanError> {
    let heard = words(transcript);
    let mut runs: Vec<(usize, usize)> = Vec::with_capacity(steps.len());
    let mut from = 0;
    for step in steps {
        let said = words(&step.said);
        if said.is_empty() {
            return Err(SpanError::NotSaid);
        }
        let at = (from..=heard.len().saturating_sub(said.len()))
            .find(|&at| heard.get(at..at + said.len()) == Some(&said[..]))
            .ok_or(SpanError::NotSaid)?;
        if let Some(&(_, previous_end)) = runs.last() {
            let between: Vec<&str> = heard[previous_end..at].iter().map(String::as_str).collect();
            if !CONNECTIVES.contains(&between.as_slice()) {
                return Err(SpanError::Between);
            }
        }
        runs.push((at, at + said.len()));
        from = at + said.len();
    }
    let (Some(&(first, _)), Some(&(_, last))) = (runs.first(), runs.last()) else {
        return Err(SpanError::NotSaid);
    };
    if heard[..first]
        .iter()
        .chain(&heard[last..])
        .any(|word| !EDGE_WORDS.contains(&word.as_str()))
    {
        return Err(SpanError::Uncovered);
    }
    Ok(runs)
}

/// What the rules before the meaning check make of an answer's steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan<'t> {
    /// Not a chain: answer the top-level single action as before. One step,
    /// or a dictation whose top level is the dictation (rule 2).
    Single,
    /// A chain this module refuses, with the reason the user is told.
    Refused(ChainRefusal<'t>),
    /// Steps to check one by one, each with its row.
    Steps(Vec<(&'t IntentStep, &'t CommandRow)>),
    /// The same single-valued choice named for every step: not steps but one
    /// choice between their values, offered when each resolves on screen.
    OneOf(Vec<(&'t IntentStep, &'t CommandRow)>),
}

/// Why a chain was refused before any step was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainRefusal<'t> {
    TooMany,
    /// A dictation step in a chain whose top level is not that dictation.
    Dictation,
    Contrast,
    Span(SpanError),
    /// A step names an action the table does not have.
    Unknown,
    /// The same single-valued choice twice beside a different step: two
    /// values for one choice inside a chain, which no step order can run.
    SameChoiceTwice(&'t CommandRow),
    /// The row may not be a step at all.
    Never(&'t CommandRow),
    /// The row may only be the last step and is not.
    NotLast(&'t CommandRow),
}

impl ChainRefusal<'_> {
    /// What the user is told, after "Heard: “…” — ".
    pub fn why(&self) -> String {
        const ONE: &str = "say one command at a time";
        match self {
            ChainRefusal::TooMany => format!(
                "that asks for more than {MAX_STEPS} things at once, so nothing was done; say up to {MAX_STEPS}"
            ),
            ChainRefusal::Dictation => {
                format!("typing into an agent is said on its own, so nothing was done; {ONE}")
            }
            ChainRefusal::Contrast => format!(
                "that weighs one thing against another rather than asking for steps, so nothing was done; {ONE}"
            ),
            ChainRefusal::Span(_) | ChainRefusal::Unknown => format!(
                "I could not tell which words asked for which step, so nothing was done; {ONE}"
            ),
            ChainRefusal::SameChoiceTwice(row) => format!(
                "that names two at once where only one can be chosen, so nothing was done; ask to {} once",
                row.asks_to
            ),
            ChainRefusal::Never(row) => format!(
                "asking to {} is said on its own, never with other steps, so nothing was done",
                row.asks_to
            ),
            ChainRefusal::NotLast(row) => format!(
                "asking to {} can only be the last thing in one sentence, so nothing was done",
                row.asks_to
            ),
        }
    }
}

/// The rows that choose ONE of the things on screen, where the same row for
/// every step is one choice between those things rather than steps — offered
/// as PRD #1261's numbered choice, which can be answered by voice only for a
/// value it can check is still on screen.
const SINGLE_VALUED: &[&str] = &[
    "open_dir",
    "use_this_directory",
    "choose_mode",
    "choose_agent_type",
    "choose_deck",
    "switch_deck",
];

/// The rows that set ONE value from the user's own words (the Name, the
/// Command): the same row twice is two values for one field, which no step
/// order can run and a numbered choice cannot be answered by voice for
/// (Greptile on PR #2417), so it is refused.
const TEXT_VALUED: &[&str] = &["name_new_agent", "set_new_agent_command"];

/// Apply rules 1–5 to `top_action` and its `steps`, as answered for
/// `transcript`.
pub fn plan<'t>(
    table: &'t CommandTable,
    transcript: &str,
    top_action: &str,
    steps: &'t [IntentStep],
) -> Plan<'t> {
    if steps.len() < 2 {
        return Plan::Single;
    }
    if steps.len() > MAX_STEPS {
        return Plan::Refused(ChainRefusal::TooMany);
    }
    if steps
        .iter()
        .any(|step| DICTATION_ROWS.contains(&step.action.as_str()))
    {
        return if DICTATION_ROWS.contains(&top_action) {
            Plan::Single
        } else {
            Plan::Refused(ChainRefusal::Dictation)
        };
    }
    if has_contrast(transcript) {
        return Plan::Refused(ChainRefusal::Contrast);
    }
    if let Err(error) = spans(transcript, steps) {
        return Plan::Refused(ChainRefusal::Span(error));
    }
    let mut rows = Vec::with_capacity(steps.len());
    for step in steps {
        let Some(row) = table.row(&step.action) else {
            return Plan::Refused(ChainRefusal::Unknown);
        };
        rows.push((step, row));
    }
    // The same single-valued choice named for every step ("select docs and
    // dot-agent-deck directories"): one choice between the values, offered
    // as a numbered choice when each value resolves on screen.
    if SINGLE_VALUED.contains(&rows[0].1.id.as_str())
        && rows.iter().all(|(_, row)| row.id == rows[0].1.id)
    {
        return Plan::OneOf(rows);
    }
    if let Some(&(_, row)) = rows.iter().enumerate().find_map(|(at, (_, row))| {
        ((SINGLE_VALUED.contains(&row.id.as_str()) || TEXT_VALUED.contains(&row.id.as_str()))
            && rows[..at].iter().any(|(_, earlier)| earlier.id == row.id))
        .then_some(&rows[at])
    }) {
        return Plan::Refused(ChainRefusal::SameChoiceTwice(row));
    }
    let last = rows.len() - 1;
    for (at, &(_, row)) in rows.iter().enumerate() {
        match position(row) {
            Position::Never => return Plan::Refused(ChainRefusal::Never(row)),
            Position::LastOnly if at != last => {
                return Plan::Refused(ChainRefusal::NotLast(row));
            }
            _ => {}
        }
    }
    Plan::Steps(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::table::table;

    fn step(action: &str, said: &str) -> IntentStep {
        IntentStep {
            action: action.to_string(),
            params: Default::default(),
            said: said.to_string(),
        }
    }

    fn actions(plan: &Plan<'_>) -> Vec<String> {
        match plan {
            Plan::Steps(rows) => rows.iter().map(|(_, row)| row.id.clone()).collect(),
            other => panic!("expected steps, got {other:?}"),
        }
    }

    /// Scenario: answer "open the overview and then the tester" with two
    /// steps whose words are the sentence's own, and check that both are
    /// accepted for the per-step checks, in order.
    #[test]
    fn voice_chain_two_navigation_steps_are_planned_in_order() {
        let steps = [
            step("open_overview", "open the overview"),
            step("open_agent", "the tester"),
        ];
        let plan = plan(
            table(),
            "Open the overview and then the tester.",
            "open_overview",
            &steps,
        );
        assert_eq!(actions(&plan), ["open_overview", "open_agent"]);
    }

    /// Scenario: answer with one step, or with none, and check the answer is
    /// treated as the single command it always was.
    #[test]
    fn voice_chain_one_step_is_a_single_command() {
        let one = [step("open_overview", "open the overview")];
        assert_eq!(
            plan(table(), "open the overview", "open_overview", &one),
            Plan::Single
        );
        assert_eq!(
            plan(table(), "open the overview", "open_overview", &[]),
            Plan::Single
        );
    }

    /// Scenario: answer with four steps and check the chain is refused with
    /// the "up to three" sentence.
    #[test]
    fn voice_chain_more_than_three_steps_is_refused() {
        let steps = [
            step("open_overview", "a"),
            step("open_deck", "b"),
            step("open_overview", "c"),
            step("open_deck", "d"),
        ];
        let plan = plan(table(), "a and b and c and d", "open_overview", &steps);
        assert_eq!(plan, Plan::Refused(ChainRefusal::TooMany));
        let Plan::Refused(refusal) = plan else {
            unreachable!()
        };
        assert!(refusal.why().contains("say up to 3"), "{}", refusal.why());
    }

    /// Scenario: "type run the tests and then commit" split by the model into
    /// a dictation and a start, with the dictation at the top level, is
    /// answered as the single dictation; the same split under a top level
    /// that is not the dictation is refused.
    #[test]
    fn voice_chain_a_dictation_step_falls_back_or_refuses() {
        let steps = [
            step("dictate_to_agent", "type run the tests"),
            step("start_new_agent", "and then commit"),
        ];
        let said = "type run the tests and then commit";
        assert_eq!(
            plan(table(), said, "dictate_to_agent", &steps),
            Plan::Single
        );
        let steps = [
            step("open_agent", "open the tester"),
            step("dictation_on", "type run the tests"),
        ];
        assert_eq!(
            plan(
                table(),
                "open the tester and type run the tests",
                "open_agent",
                &steps
            ),
            Plan::Refused(ChainRefusal::Dictation)
        );
    }

    /// Scenario: a contrast word anywhere — "or", "not", "rather than",
    /// "instead" — refuses the chain, even inside a step's own words.
    #[test]
    fn voice_chain_a_contrast_word_refuses_the_chain() {
        for (said, steps) in [
            (
                "open the overview or the deck",
                vec![
                    step("open_overview", "open the overview"),
                    step("open_deck", "or the deck"),
                ],
            ),
            (
                "open settings, not the overview",
                vec![
                    step("open_settings", "open settings"),
                    step("open_overview", "the overview"),
                ],
            ),
            (
                "make it a dispatcher or a schedule",
                vec![
                    step("choose_mode", "make it a dispatcher"),
                    step("choose_mode", "or a schedule"),
                ],
            ),
            (
                "show the reviewer instead of the tester",
                vec![
                    step("open_agent", "show the reviewer"),
                    step("open_agent", "the tester"),
                ],
            ),
        ] {
            assert_eq!(
                plan(table(), said, &steps[0].action, &steps),
                Plan::Refused(ChainRefusal::Contrast),
                "{said}"
            );
        }
    }

    /// Scenario: a step whose words the user did not say, steps out of order,
    /// other words between two steps, and a sentence whose first words belong
    /// to no step are each refused before anything is checked.
    #[test]
    fn voice_chain_steps_must_quote_and_cover_the_sentence() {
        let said = "open the overview and then the tester";
        fn refused<'t>(said: &str, steps: &'t [IntentStep]) -> Plan<'t> {
            plan(table(), said, "open_overview", steps)
        }
        assert_eq!(
            refused(
                said,
                &[
                    step("open_overview", "open the overview"),
                    step("open_agent", "open the tester")
                ]
            ),
            Plan::Refused(ChainRefusal::Span(SpanError::NotSaid))
        );
        assert_eq!(
            refused(
                said,
                &[
                    step("open_agent", "the tester"),
                    step("open_overview", "open the overview")
                ]
            ),
            Plan::Refused(ChainRefusal::Span(SpanError::NotSaid))
        );
        assert_eq!(
            spans(
                "open the overview quickly and the tester",
                &[
                    step("open_overview", "open the overview"),
                    step("open_agent", "the tester"),
                ]
            ),
            Err(SpanError::Between)
        );
        // "make it a dispatcher, use codex and name it triage bot" answered
        // without its first step — M1's dropped-step case.
        assert_eq!(
            spans(
                "make it a dispatcher, use codex and name it triage bot",
                &[
                    step("choose_agent_type", "use codex"),
                    step("name_new_agent", "name it triage bot"),
                ]
            ),
            Err(SpanError::Uncovered)
        );
        assert!(
            spans(
                "please open the overview and then the tester now",
                &[
                    step("open_overview", "open the overview"),
                    step("open_agent", "the tester"),
                ]
            )
            .is_ok()
        );
    }

    /// Scenario: "select docs and dot-agent-deck directories" answered as the
    /// same directory step twice is one choice between the two values, not
    /// two steps; the same choice twice beside another step is refused.
    #[test]
    fn voice_chain_the_same_choice_twice_is_not_two_steps() {
        let steps = [
            step("open_dir", "select docs"),
            step("open_dir", "dot-agent-deck directories"),
        ];
        let planned = plan(
            table(),
            "select docs and dot-agent-deck directories",
            "open_dir",
            &steps,
        );
        assert!(matches!(&planned, Plan::OneOf(rows) if rows.len() == 2));
        let steps = [
            step("choose_mode", "make it a dispatcher"),
            step("choose_mode", "a schedule"),
            step("name_new_agent", "name it nightly"),
        ];
        let planned = plan(
            table(),
            "make it a dispatcher and a schedule and name it nightly",
            "choose_mode",
            &steps,
        );
        assert!(
            matches!(planned, Plan::Refused(ChainRefusal::SameChoiceTwice(row)) if row.id == "choose_mode")
        );
    }

    /// Scenario: "name it alpha and name it beta" answered as two Name steps
    /// is refused, not offered as a choice: a spoken answer cannot pick
    /// between two names (Greptile on PR #2417).
    #[test]
    fn voice_chain_two_names_are_refused_not_offered() {
        let steps = [
            step("name_new_agent", "name it alpha"),
            step("name_new_agent", "name it beta"),
        ];
        let planned = plan(
            table(),
            "name it alpha and name it beta",
            "name_new_agent",
            &steps,
        );
        assert!(
            matches!(planned, Plan::Refused(ChainRefusal::SameChoiceTwice(row)) if row.id == "name_new_agent")
        );
    }

    /// Scenario: a start, a whole-sentence row and a dictation row are never
    /// a step; a close and a stop may only be last; a view move, a deck
    /// switch and a form choice may come before another step.
    #[test]
    fn voice_chain_positions_follow_the_table() {
        let table = table();
        let of = |id: &str| position(table.row(id).expect(id));
        for id in [
            "start_new_agent",
            "submit_prompt",
            "discard_new_agent",
            "dictation_on",
            "dictate_to_agent",
        ] {
            assert_eq!(of(id), Position::Never, "{id}");
        }
        for id in [
            "close",
            "stop_agent",
            "close_orchestration",
            "open_settings",
            "open_new_agent",
            "open_dir",
            "list_commands",
            "voice_off",
        ] {
            assert_eq!(of(id), Position::LastOnly, "{id}");
        }
        assert_eq!(
            of("open_overview"),
            Position::Before(Lands::View(Screen::Overview))
        );
        assert_eq!(
            of("open_agent"),
            Position::Before(Lands::View(Screen::Agent))
        );
        assert_eq!(of("switch_deck"), Position::Before(Lands::DeckSwitched));
        assert_eq!(of("choose_mode"), Position::Before(Lands::Stays));

        let steps = [
            step("close", "close this"),
            step("open_agent", "open the reviewer"),
        ];
        assert!(matches!(
            plan(table, "close this and open the reviewer", "close", &steps),
            Plan::Refused(ChainRefusal::NotLast(row)) if row.id == "close"
        ));
        let steps = [
            step("choose_mode", "make it a dispatcher"),
            step("start_new_agent", "start it"),
        ];
        assert!(matches!(
            plan(table, "make it a dispatcher and start it", "choose_mode", &steps),
            Plan::Refused(ChainRefusal::Never(row)) if row.id == "start_new_agent"
        ));
        let steps = [
            step("open_overview", "show me the overview"),
            step("stop_agent", "stop the coder one"),
        ];
        assert_eq!(
            actions(&plan(
                table,
                "show me the overview, then stop the coder one",
                "open_overview",
                &steps
            )),
            ["open_overview", "stop_agent"]
        );
    }

    /// Scenario: every id this module names is a row of the shipped table, so
    /// a renamed row cannot silently fall out of a rule.
    #[test]
    fn voice_chain_every_named_row_exists() {
        let table = table();
        for id in NEVER_IN_A_CHAIN
            .iter()
            .chain(DICTATION_ROWS)
            .chain(SINGLE_VALUED)
            .chain(TEXT_VALUED)
            .chain(&[
                "open_overview",
                "open_deck",
                "open_agent",
                "switch_deck",
                "choose_mode",
                "choose_agent_type",
                "name_new_agent",
            ])
        {
            assert!(table.row(id).is_some(), "{id} is not a row");
        }
    }
}
