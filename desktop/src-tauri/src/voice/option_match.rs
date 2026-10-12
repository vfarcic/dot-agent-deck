//! PR #1698 — matching what the user said to a waiting question's options,
//! here, with no model call, the way a person answers one out loud.
//!
//! On the keyboard an option is picked with the arrows and Enter; by voice the
//! user says it, and rarely word for word. So an utterance chooses an option
//! when it is:
//!
//! - **the whole label**, case, punctuation and edge filler aside ("um, yes
//!   please" is "Yes");
//! - **a number** — "2", "two", "second", "option two", "number two", "the
//!   second one", "the last one" — read as the number the agent draws beside
//!   the option ([`DesktopQuestionOption::index`]), which for an agent that
//!   draws no numbers is the option's place in the row;
//! - **part of one label**: words the label has, in its order, with at least
//!   one that is not a little word like "and" or "the" — "auto mode" picks
//!   "Yes, and switch to auto mode";
//! - on a question that takes several options, **several of those joined by
//!   "and"**: "one and three", "small and large".
//!
//! A word may differ from the label's by a plural "s", or, in words of five
//! letters or more, by one letter — the slips speech-to-text makes. Nothing
//! looser: this is meant to be predictable, and anything it does not take
//! goes to the Commands model ([`super::question`]), which knows synonyms.
//!
//! # Which question
//!
//! On a form, a number and a part of a label are read against the first
//! question not yet answered — the one the agent shows, since the deck sends
//! the whole form at once. A whole label is looked for there first, then on
//! every question. Once every question has an answer, a number goes to the
//! model, which sees the form, and labels are looked for on every question.
//!
//! # Ambiguity is refused, never guessed
//!
//! A whole label wins over a part of another ("Yes" is "Yes", not "Yes, and
//! switch to auto mode"). Otherwise words that fit more than one option choose
//! nothing: the refusal names the options they could be, with their numbers.
//! So does a number that names two options — the one drawn beside it, and one
//! whose label is that number — and a number no option has.
//!
//! Whatever this picks is still held to the question by
//! [`super::question::validate`], so a keyboard-only option it names is
//! refused there, with its own sentence.

use super::choice::{Ordinal, ordinal};
use super::question::{ModelAnswer, Selection};
use super::table::spoken_words;
use crate::dto::{DesktopPendingQuestion, DesktopQuestion, DesktopQuestionOption, safe_message};

/// What [`match_options`] made of an utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionMatch {
    /// The options it chose, as an answer for [`super::question`] to check.
    Chosen(ModelAnswer),
    /// It was about the options and chooses none: the sentence says why and
    /// what can be said.
    Refused(String),
    /// Nothing here: the model is asked.
    Unmatched,
}

/// Words said around an answer that carry nothing of it, removed from either
/// end before matching: hesitations, politeness, and the verbs of choosing.
/// Each entry is a run of words as [`spoken_words`] splits them, so "I'd" is
/// `i d`. An option whose label has these words is still matched whole
/// against the utterance as said.
const LEADING_FILLER: &[&[&str]] = &[
    &["um"],
    &["umm"],
    &["uh"],
    &["uhm"],
    &["er"],
    &["erm"],
    &["hmm"],
    &["mm"],
    &["ah"],
    &["oh"],
    &["okay"],
    &["ok"],
    &["so"],
    &["well"],
    &["please"],
    &["just"],
    &["i", "want"],
    &["i", "d", "like"],
    &["i", "would", "like"],
    &["i", "ll", "take"],
    &["i", "ll", "go", "with"],
    &["i", "ll", "pick"],
    &["i", "choose"],
    &["i", "pick"],
    &["i", "say"],
    &["let", "s", "go", "with"],
    &["let", "s", "do"],
    &["go", "with"],
    &["pick"],
    &["choose"],
    &["select"],
    &["take"],
    &["make", "it"],
    &["the", "answer", "is"],
    &["my", "answer", "is"],
];
const TRAILING_FILLER: &[&[&str]] = &[
    &["please"],
    &["thanks"],
    &["thank", "you"],
    &["do", "it"],
    &["then"],
    &["for", "now"],
    &["um"],
    &["uh"],
    &["okay"],
    &["ok"],
    &["it"],
];

/// Words too common to choose an option on their own, as part of a label.
const LITTLE_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "to", "of", "for", "in", "on", "at", "with", "this", "that",
    "it", "is", "be", "me", "my", "i", "you", "your", "do", "don", "t", "s",
];

/// The words that join several options of a multi-select answer.
const JOINERS: &[&str] = &["and", "plus"];

/// Match `text` against `question`'s options, with `form` the answer so far.
pub fn match_options(
    question: &DesktopPendingQuestion,
    form: &[Selection],
    text: &str,
) -> OptionMatch {
    let as_said = spoken_words(text);
    let said = without_filler(&as_said);
    if said.is_empty() {
        return OptionMatch::Unmatched;
    }
    let current = current_question(question, form);
    let others = || {
        question
            .questions
            .iter()
            .enumerate()
            .filter(move |(at, _)| Some(*at) != current)
    };

    // A whole label: the question on screen first, then any question.
    let whole = |q: &DesktopQuestion| -> Vec<u32> {
        q.options
            .iter()
            .filter(|option| {
                let label = spoken_words(&option.label);
                same_words(&label, &said) || same_words(&label, &as_said)
            })
            .map(|option| option.index)
            .collect()
    };
    if let Some(at) = current {
        let q = &question.questions[at];
        match decide(question, &said, &[(at, whole(q))]) {
            OptionMatch::Unmatched => {}
            decided => return decided,
        }
    }
    let elsewhere: Vec<(usize, Vec<u32>)> = others().map(|(at, q)| (at, whole(q))).collect();
    match decide(question, &said, &elsewhere) {
        OptionMatch::Unmatched => {}
        decided => return decided,
    }

    // A number, on the question on screen.
    if let Some(at) = current
        && let Some(said_number) = ordinal(&said)
    {
        let q = &question.questions[at];
        let named = numbered(q, said_number);
        if named.is_empty() {
            return OptionMatch::Refused(no_such_number(question, at, said_number));
        }
        return decide(question, &said, &[(at, named)]);
    }

    // Part of a label: the question on screen, then any other.
    let part = |q: &DesktopQuestion| -> Vec<u32> {
        q.options
            .iter()
            .filter(|option| has_part(&spoken_words(&option.label), &said))
            .map(|option| option.index)
            .collect()
    };
    if let Some(at) = current {
        let q = &question.questions[at];
        match decide(question, &said, &[(at, part(q))]) {
            OptionMatch::Unmatched => {}
            decided => return decided,
        }
        // Several options at once, on a question that takes several.
        if q.multi_select {
            match several(question, at, text) {
                OptionMatch::Unmatched => {}
                decided => return decided,
            }
        }
    }
    let elsewhere: Vec<(usize, Vec<u32>)> = others().map(|(at, q)| (at, part(q))).collect();
    decide(question, &said, &elsewhere)
}

/// The question a number or a part of a label is read against: the only one,
/// or the first a form has not answered. `None` once every question of a form
/// has an answer.
fn current_question(question: &DesktopPendingQuestion, form: &[Selection]) -> Option<usize> {
    if question.questions.len() == 1 {
        return Some(0);
    }
    (0..question.questions.len()).find(|at| !form.iter().any(|s| s.question_index as usize == *at))
}

/// One option chosen when `candidates` hold exactly one, across every
/// question listed; a refusal naming them when they hold several; nothing
/// when they hold none.
fn decide(
    question: &DesktopPendingQuestion,
    said: &[String],
    candidates: &[(usize, Vec<u32>)],
) -> OptionMatch {
    let all: Vec<(usize, u32)> = candidates
        .iter()
        .flat_map(|(at, indices)| indices.iter().map(move |index| (*at, *index)))
        .collect();
    match all.as_slice() {
        [] => OptionMatch::Unmatched,
        [(at, index)] => OptionMatch::Chosen(ModelAnswer::answer(*at as i64, &[i64::from(*index)])),
        several => OptionMatch::Refused(could_be(question, said, several)),
    }
}

/// The options `said` names by number on `q`: the one drawn with that number,
/// and any whose whole label is that number.
fn numbered(q: &DesktopQuestion, said: Ordinal) -> Vec<u32> {
    let mut named: Vec<u32> = match said {
        Ordinal::Last => q.options.last().map(|o| o.index).into_iter().collect(),
        Ordinal::Nth(number) => q
            .options
            .iter()
            .filter(|option| option.index as usize == number)
            .map(|option| option.index)
            .collect(),
    };
    if let Ordinal::Nth(number) = said {
        for option in &q.options {
            let label = spoken_words(&option.label);
            if ordinal(&label) == Some(Ordinal::Nth(number)) && !named.contains(&option.index) {
                named.push(option.index);
            }
        }
    }
    named.sort_unstable();
    named
}

/// Several options of a multi-select question, joined by "and" or a comma:
/// each part must name exactly one option of question `at`, by its whole
/// label, its number or a part of its label.
fn several(question: &DesktopPendingQuestion, at: usize, text: &str) -> OptionMatch {
    let parts: Vec<Vec<String>> = text
        .split(',')
        .flat_map(|chunk| {
            spoken_words(chunk)
                .split(|word| JOINERS.contains(&word.as_str()))
                .map(<[String]>::to_vec)
                .collect::<Vec<_>>()
        })
        .map(|part| without_filler(&part))
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 {
        return OptionMatch::Unmatched;
    }
    let q = &question.questions[at];
    let mut chosen: Vec<i64> = Vec::new();
    for part in &parts {
        let mut named: Vec<u32> = q
            .options
            .iter()
            .filter(|option| same_words(&spoken_words(&option.label), part))
            .map(|option| option.index)
            .collect();
        if named.is_empty()
            && let Some(number) = ordinal(part)
        {
            named = numbered(q, number);
            if named.is_empty() {
                return OptionMatch::Refused(no_such_number(question, at, number));
            }
        }
        if named.is_empty() {
            named = q
                .options
                .iter()
                .filter(|option| has_part(&spoken_words(&option.label), part))
                .map(|option| option.index)
                .collect();
        }
        match named.as_slice() {
            [] => return OptionMatch::Unmatched,
            [index] => {
                if !chosen.contains(&i64::from(*index)) {
                    chosen.push(i64::from(*index));
                }
            }
            several => {
                let pairs: Vec<(usize, u32)> = several.iter().map(|index| (at, *index)).collect();
                return OptionMatch::Refused(could_be(question, part, &pairs));
            }
        }
    }
    OptionMatch::Chosen(ModelAnswer::answer(at as i64, &chosen))
}

/// `words` without [`LEADING_FILLER`] and [`TRAILING_FILLER`] at their ends,
/// for as long as something is left.
fn without_filler(words: &[String]) -> Vec<String> {
    let mut rest: &[String] = words;
    loop {
        let before = rest.len();
        for filler in LEADING_FILLER {
            if rest.len() > filler.len() && rest.iter().zip(filler.iter()).all(|(w, f)| w == f) {
                rest = &rest[filler.len()..];
            }
        }
        for filler in TRAILING_FILLER {
            if rest.len() > filler.len()
                && rest[rest.len() - filler.len()..]
                    .iter()
                    .zip(filler.iter())
                    .all(|(w, f)| w == f)
            {
                rest = &rest[..rest.len() - filler.len()];
            }
        }
        if rest.len() == before {
            return rest.to_vec();
        }
    }
}

/// Whether two runs of words are the same, word for word, allowing each
/// [`same_word`] slip.
fn same_words(label: &[String], said: &[String]) -> bool {
    !label.is_empty()
        && label.len() == said.len()
        && label.iter().zip(said).all(|(l, s)| same_word(l, s))
}

/// Whether `said` is part of `label`: every word of it in the label, in the
/// label's order, with a leading article dropped, and at least one word that
/// is not one of [`LITTLE_WORDS`].
fn has_part(label: &[String], said: &[String]) -> bool {
    let said = match said {
        [article, rest @ ..]
            if !rest.is_empty() && matches!(article.as_str(), "the" | "a" | "an") =>
        {
            rest
        }
        _ => said,
    };
    if !said
        .iter()
        .any(|word| !LITTLE_WORDS.contains(&word.as_str()))
    {
        return false;
    }
    let mut label = label.iter();
    said.iter()
        .all(|word| label.by_ref().any(|l| same_word(l, word)))
}

/// One word of a label and one said: the same, or the same but for a plural
/// "s", or — both five letters or more — one letter apart.
fn same_word(label: &str, said: &str) -> bool {
    if label == said {
        return true;
    }
    let stem = |word: &str| word.strip_suffix('s').unwrap_or(word).to_string();
    if label.len() > 3 && said.len() > 3 && stem(label) == stem(said) {
        return true;
    }
    label.chars().count() >= 5 && said.chars().count() >= 5 && one_letter_apart(label, said)
}

/// Whether `a` becomes `b` by changing, adding or removing one letter.
fn one_letter_apart(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }
    let first = short
        .iter()
        .zip(long.iter())
        .position(|(x, y)| x != y)
        .unwrap_or(short.len());
    if short.len() == long.len() {
        short[first + 1..] == long[first + 1..]
    } else {
        short[first..] == long[first + 1..]
    }
}

/// "1 “Yes”" — an option with the number the agent draws beside it.
pub fn numbered_option(option: &DesktopQuestionOption) -> String {
    format!("{} \u{201c}{}\u{201d}", option.index, option.label)
}

/// The options the deck can answer on `q`, numbered: "1 “Yes”, 3 “No”".
pub fn numbered_options(q: &DesktopQuestion) -> String {
    q.options
        .iter()
        .filter(|option| option.answerable)
        .map(numbered_option)
        .collect::<Vec<_>>()
        .join(", ")
}

/// "a, b or c".
fn or_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// The refusal for words that fit several options.
fn could_be(
    question: &DesktopPendingQuestion,
    said: &[String],
    candidates: &[(usize, u32)],
) -> String {
    let several_questions = candidates.iter().any(|(at, _)| *at != candidates[0].0);
    let named: Vec<String> = candidates
        .iter()
        .filter_map(|(at, index)| {
            let q = question.question(*at as u32)?;
            let option = q.option(*index)?;
            Some(if several_questions {
                format!(
                    "{} {}",
                    q.header.as_deref().unwrap_or(&q.prompt),
                    numbered_option(option)
                )
            } else {
                numbered_option(option)
            })
        })
        .collect();
    format!(
        "\u{201c}{}\u{201d} could be {} — say its number or more of its words. Nothing was chosen.",
        safe_message(said.join(" ")),
        or_list(&named)
    )
}

/// The refusal for a number no option of question `at` has.
fn no_such_number(question: &DesktopPendingQuestion, at: usize, said: Ordinal) -> String {
    let number = match said {
        Ordinal::Nth(number) => number.to_string(),
        Ordinal::Last => "last".to_string(),
    };
    let options = question
        .question(at as u32)
        .map(numbered_options)
        .unwrap_or_default();
    if options.is_empty() {
        return format!("There is no option {number}, so nothing was chosen.");
    }
    format!("There is no option {number}, so nothing was chosen. The options are {options}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        spoken_words(text)
    }

    #[test]
    fn filler_comes_off_both_ends_but_never_empties_the_utterance() {
        assert_eq!(without_filler(&words("um, yes please")), ["yes"]);
        assert_eq!(without_filler(&words("Yes, do it.")), ["yes"]);
        assert_eq!(
            without_filler(&words("I'd like the second one")),
            ["the", "second", "one"]
        );
        assert_eq!(without_filler(&words("please")), ["please"]);
        assert_eq!(without_filler(&words("okay")), ["okay"]);
    }

    #[test]
    fn a_word_may_slip_by_a_plural_or_one_letter_in_a_long_word() {
        assert!(same_word("sizes", "size"));
        assert!(same_word("switch", "swich"));
        assert!(same_word("large", "larges"));
        assert!(!same_word("yes", "ye"));
        assert!(!same_word("deny", "dent"), "short words must match exactly");
        assert!(!same_word("allow", "below"));
    }

    #[test]
    fn a_part_is_the_label_s_words_in_its_order_with_something_more_than_little_words() {
        let label = words("Yes, and switch to auto mode");
        assert!(has_part(&label, &words("auto mode")));
        assert!(has_part(&label, &words("switch to auto mode")));
        assert!(has_part(&label, &words("yes switch to auto mode")));
        assert!(has_part(&label, &words("the auto mode")));
        assert!(!has_part(&label, &words("mode auto")), "order matters");
        assert!(
            !has_part(&label, &words("and")),
            "a little word alone chooses nothing"
        );
        assert!(!has_part(&label, &words("auto mode please now")));
    }
}
