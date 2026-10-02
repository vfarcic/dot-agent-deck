//! PR #1451 round 3, change 3 (PRD #1261) — a bare number said against the
//! numbered list on screen.
//!
//! While voice is on, the lists voice selects from — the dashboard's agents,
//! the Daemons screen's tiles, and the New agent dialog's daemons, directories
//! and modes — show a number beside each item. The webview DECLARES that list
//! with the utterance ([`VoiceNumberedList`]): its sections in on-screen order,
//! entry `i` of a section showing number `i + 1`, and a generation that changes
//! whenever the list does. A number ("three", "number three", "the third one",
//! "directory 13", "select daemon 1") is answered HERE, against that declaration, with no Commands backend call: no
//! model is asked anything, so no observed name can steer which item is chosen.
//!
//! # What the declaration is
//!
//! Exactly the numbered items on screen, nothing behind a dialog and nothing
//! scrolled into another page, in SECTIONS ([`VoiceNumberedSection`]): one
//! kind of list each — daemons, directories (with `..`), modes, agents — and
//! each numbered from 1 (round 4, D7, which superseded numbering several
//! visible lists continuously). The dashboard's agent rows are one section
//! across every daemon group. A listing that pages declares its CURRENT page
//! only, so its numbers restart at 1 on every page and a page turn is a new
//! generation.
//!
//! # The answer
//!
//! [`answer`] reads the utterance with the numbered choice's own ordinal rules
//! ([`super::choice::ordinal`]), after an optional leading verb ("select",
//! "choose", "open", "pick", "enter", "go to", "switch to") and section word
//! ("directory 13", "select daemon 1", "choose mode 2", "open agent 3"). A
//! section word picks its section; a bare number is the item showing it when
//! one section on screen shows it, and a choice between them when several do.
//! It refuses rather than guesses: a list that changed between the moment the
//! user looked at it and the answer ([`NumberAnswer::Stale`]), a section that
//! is not numbered on screen ([`NumberAnswer::NotNumbered`]), and a number its
//! section — or, said bare, every section — does not show
//! ([`NumberAnswer::OutOfRange`]). A number said as a count that some OTHER
//! item's name in the same section also is or ends in — "one" with an agent
//! called `orchestrator-1` further down — joins the choice
//! ([`NumberAnswer::Ambiguous`]), which the webview offers as the numbered
//! choice.

use serde::{Deserialize, Serialize};

use super::choice::{Ordinal, count_said, ordinal, said_as_count};
use super::outcome::whole_utterance;
use super::table::spoken_words;

/// What one numbered item is, which decides what choosing it does. The
/// webview maps each to the row a spoken name would have run: `agent` opens
/// it, `deck` chooses the New agent dialog's daemon, `deck_switch` switches
/// to the daemon in the open Daemon selector (PR #1451 round 3, change 4),
/// `directory` enters it, `parent` goes up, `mode` chooses the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberedKind {
    Agent,
    Deck,
    DeckSwitch,
    Directory,
    Parent,
    Mode,
}

/// One numbered item as the webview shows it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceNumberedEntry {
    pub kind: NumberedKind,
    /// Its identity on that list: an agent id, a deck id, a path, a mode id.
    /// Carried for the webview's dispatch; [`answer`] does not read it.
    pub value: String,
    /// For an agent, the deck it is on — the dashboard spans every deck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deck_id: Option<String>,
    /// The name shown beside the number.
    pub label: String,
    /// Every other name the item answers to (an agent's role, CLI, id), which
    /// a spoken number can collide with as surely as with the label.
    #[serde(default)]
    pub names: Vec<String>,
}

/// One kind of list on screen, numbered from 1 (round 4, D7). A daemon is
/// `deck` whichever list shows it — the New agent dialog's or the Daemon
/// selector's — and `..` is in the directories' section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    Agent,
    Deck,
    Directory,
    Mode,
}

/// Every section, for reading a section word.
const SECTIONS: [SectionKind; 4] = [
    SectionKind::Agent,
    SectionKind::Deck,
    SectionKind::Directory,
    SectionKind::Mode,
];

impl SectionKind {
    /// The words that name this section, singular and plural.
    fn words(self) -> &'static [&'static str] {
        match self {
            SectionKind::Agent => &["agent", "agents"],
            SectionKind::Deck => &["daemon", "daemons", "deck", "decks"],
            SectionKind::Directory => &[
                "directory",
                "directories",
                "folder",
                "folders",
                "dir",
                "dirs",
            ],
            SectionKind::Mode => &["mode", "modes"],
        }
    }

    /// The section `word` names, if any.
    fn named(word: &str) -> Option<Self> {
        SECTIONS
            .into_iter()
            .find(|section| section.words().contains(&word))
    }
}

/// One section of the numbered list: entry `i` shows number `i + 1`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceNumberedSection {
    pub kind: SectionKind,
    pub entries: Vec<VoiceNumberedEntry>,
}

/// The numbered list on screen, as the webview declared it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceNumberedList {
    /// Changes whenever the list does. [`answer`] compares the generation the
    /// utterance was spoken against with the one on screen now.
    pub generation: u64,
    /// The sections in on-screen order, each numbered from 1.
    pub sections: Vec<VoiceNumberedSection>,
}

impl VoiceNumberedList {
    /// Every item declared, across the sections.
    pub fn len(&self) -> usize {
        self.sections
            .iter()
            .map(|section| section.entries.len())
            .sum()
    }

    /// Whether nothing is numbered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The sections that number something, the first of each kind, in
    /// on-screen order.
    fn shown(&self) -> Vec<&VoiceNumberedSection> {
        let mut shown: Vec<&VoiceNumberedSection> = Vec::new();
        for section in &self.sections {
            if !section.entries.is_empty() && !shown.iter().any(|seen| seen.kind == section.kind) {
                shown.push(section);
            }
        }
        shown
    }
}

/// One numbered item, by its section and the number it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberRef {
    pub section: SectionKind,
    pub number: usize,
}

/// What one utterance says about the numbered list. Numbers are 1-based, as
/// shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NumberAnswer {
    /// Not a number, or no numbered list is on screen: the utterance is
    /// resolved as an ordinary command.
    NotNumber,
    /// The item showing this number in this section.
    Selected { section: SectionKind, number: usize },
    /// No item shows this number: in `section` when one was said, in any
    /// section when it was not. `elsewhere` names the OTHER sections that do
    /// show it ("select daemon 13" while directory 13 is on screen), so the
    /// refusal can say so; nothing is chosen there.
    OutOfRange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<SectionKind>,
        number: usize,
        elsewhere: Vec<SectionKind>,
    },
    /// The section said is not numbered on screen; `shown` is what is.
    NotNumbered {
        section: SectionKind,
        shown: Vec<SectionKind>,
    },
    /// The list changed after the user looked at it, so the number may now
    /// name a different item. Nothing is chosen.
    Stale,
    /// Several items the utterance could mean: the same number in several
    /// sections (a bare number), and within a section an item whose name, said
    /// as a count, is or ends in it. In on-screen order, each section's item
    /// showing the number before the names that collide with it.
    Ambiguous { choices: Vec<NumberRef> },
}

/// Verbs that may lead a section word: "select directory 13", "go to mode 2".
/// Only before a section word — "open the third one" stays a command.
const LEADING_VERBS: [&[&str]; 7] = [
    &["select"],
    &["choose"],
    &["pick"],
    &["open"],
    &["enter"],
    &["go", "to"],
    &["switch", "to"],
];

/// `words` split into the section said, if any — with the word that said it —
/// and the words that should be the number: a bare ordinal ("three", "the
/// third one") with no section, or an optional [`LEADING_VERBS`] entry, an
/// optional "the" and a section word before it ("select the directory 13").
/// `None` when a verb leads something other than a section word.
fn read(words: &[String]) -> Option<Said<'_>> {
    let verb = LEADING_VERBS
        .iter()
        .find(|verb| {
            words.len() >= verb.len() && verb.iter().zip(words).all(|(want, word)| word == want)
        })
        .map_or(0, |verb| verb.len());
    let mut rest = &words[verb..];
    if rest.first().is_some_and(|word| word == "the") {
        rest = &rest[1..];
    }
    if let Some((section, word)) = rest
        .first()
        .and_then(|word| SectionKind::named(word).map(|section| (section, word.as_str())))
    {
        return Some(Said {
            section: Some((section, word)),
            number: &rest[1..],
        });
    }
    (verb == 0).then_some(Said {
        section: None,
        number: words,
    })
}

/// What [`read`] makes of an utterance.
struct Said<'a> {
    /// The section said, with the word that said it ("folder" for
    /// [`SectionKind::Directory`]), if any.
    section: Option<(SectionKind, &'a str)>,
    /// The words that should be the number.
    number: &'a [String],
}

/// Answer `utterance` against `heard` — the list as it stood when the user
/// spoke — given the generation on screen `now`.
///
/// In order: anything that is not a whole-utterance ordinal (after an
/// optional verb and section word), or a declaration with nothing numbered,
/// is [`NumberAnswer::NotNumber`]; a list whose generation moved is
/// [`NumberAnswer::Stale`] whatever the number; a section said that numbers
/// nothing on screen is [`NumberAnswer::NotNumbered`]; a number its section
/// (or, said bare, every section) does not show — or "number zero" — is
/// [`NumberAnswer::OutOfRange`]. Then the items it names: in the section said,
/// or in every section showing it when none was, each joined by the items of
/// its own section whose label or name ends in the number when the number is
/// said as a count ("one", "1", "number one" — never "first" or "the last
/// one", which are positions by grammar). One item is
/// [`NumberAnswer::Selected`]; several are [`NumberAnswer::Ambiguous`].
pub fn answer(utterance: &str, heard: &VoiceNumberedList, now: u64) -> NumberAnswer {
    let words = whole_utterance(utterance);
    let Some(Said {
        section,
        number: said_words,
    }) = read(&words)
    else {
        return NumberAnswer::NotNumber;
    };
    let Some(said) = ordinal(said_words) else {
        return NumberAnswer::NotNumber;
    };
    let shown = heard.shown();
    if shown.is_empty() {
        return NumberAnswer::NotNumber;
    }
    if heard.generation != now {
        return NumberAnswer::Stale;
    }
    let counted = matches!(said, Ordinal::Nth(_)) && said_as_count(said_words);
    // A bare count collides with any name ending in it; after a section word
    // only with a name that IS what was said ("folder 13" and `folder-13`).
    let collides = match section {
        _ if !counted => Collides::Nothing,
        None => Collides::EndsIn,
        Some((_, word)) => Collides::Named(word),
    };
    let section = section.map(|(kind, _)| kind);
    let number = match said {
        Ordinal::Nth(number) => number,
        Ordinal::Last => 0,
    };
    let choices: Vec<NumberRef> = match section {
        Some(kind) => {
            let Some(listed) = shown.iter().find(|listed| listed.kind == kind) else {
                return NumberAnswer::NotNumbered {
                    section: kind,
                    shown: shown.iter().map(|listed| listed.kind).collect(),
                };
            };
            let Some(at) = said.index(listed.entries.len()) else {
                let elsewhere = match said {
                    Ordinal::Nth(_) => shown
                        .iter()
                        .filter(|other| {
                            other.kind != kind && said.index(other.entries.len()).is_some()
                        })
                        .map(|other| other.kind)
                        .collect(),
                    Ordinal::Last => Vec::new(),
                };
                return NumberAnswer::OutOfRange {
                    section: Some(kind),
                    number,
                    elsewhere,
                };
            };
            named(listed, at, collides)
        }
        None => shown
            .iter()
            .filter_map(|listed| {
                said.index(listed.entries.len())
                    .map(|at| named(listed, at, collides))
            })
            .flatten()
            .collect(),
    };
    match choices.as_slice() {
        [] => NumberAnswer::OutOfRange {
            section: None,
            number,
            elsewhere: Vec::new(),
        },
        [one] => NumberAnswer::Selected {
            section: one.section,
            number: one.number,
        },
        _ => NumberAnswer::Ambiguous { choices },
    }
}

/// Which other items of its section a number said as a count can also mean.
#[derive(Debug, Clone, Copy)]
enum Collides<'a> {
    /// None: the number was said as a position ("the third one").
    Nothing,
    /// Every item whose label or name is, or ends in, the number: a bare
    /// "one" and `orchestrator-1`.
    EndsIn,
    /// Every item whose label or name is the section word said and the
    /// number: "folder 13" and `folder-13`, "agent 1" and `agent-1`. "Select
    /// directory 13" names row 13 even when `folder-13` is another row.
    Named(&'a str),
}

/// The items of `listed` the number at `at` names: the item showing it, then
/// the other items of the same section it [`Collides`] with.
fn named(listed: &VoiceNumberedSection, at: usize, collides: Collides) -> Vec<NumberRef> {
    let number = at + 1;
    let item = |other: usize| NumberRef {
        section: listed.kind,
        number: other + 1,
    };
    let collides_with = |entry: &VoiceNumberedEntry| match collides {
        Collides::Nothing => false,
        Collides::EndsIn => ends_in(entry, number),
        Collides::Named(word) => is_named(entry, word, number),
    };
    let mut items = vec![item(at)];
    items.extend(
        listed
            .entries
            .iter()
            .enumerate()
            .filter(|&(other, entry)| other != at && collides_with(entry))
            .map(|(other, _)| item(other)),
    );
    items
}

/// Whether one of `entry`'s names is `word` followed by `number` as a count:
/// `folder-13` for "folder thirteen", `agent-1` for "agent 1", "agent
/// twenty-three" for "agent 23".
fn is_named(entry: &VoiceNumberedEntry, word: &str, number: usize) -> bool {
    std::iter::once(&entry.label)
        .chain(&entry.names)
        .any(|name| match spoken_words(name).as_slice() {
            [first, rest @ ..] if (1..=2).contains(&rest.len()) => {
                first == word && count_said(rest) == Some(number)
            }
            _ => false,
        })
}

/// Whether one of `entry`'s names is, or ends in, `number`: "orchestrator-1",
/// "worker 2", an agent whose role is "three", "worker twenty three".
fn ends_in(entry: &VoiceNumberedEntry, number: usize) -> bool {
    std::iter::once(&entry.label)
        .chain(&entry.names)
        .any(|name| trailing_count(&spoken_words(name)) == Some(number))
}

/// The count a name ends in: its last two words when they are one number
/// ("worker twenty three" is 23, not 3), else its last word.
fn trailing_count(words: &[String]) -> Option<usize> {
    words
        .len()
        .checked_sub(2)
        .and_then(|at| count_said(&words[at..]))
        .or_else(|| {
            words
                .last()
                .and_then(|last| count_said(std::slice::from_ref(last)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str) -> VoiceNumberedEntry {
        VoiceNumberedEntry {
            kind: NumberedKind::Agent,
            value: label.to_lowercase(),
            deck_id: Some("deck-local".to_string()),
            label: label.to_string(),
            names: Vec::new(),
        }
    }

    fn list(generation: u64, labels: &[&str]) -> VoiceNumberedList {
        VoiceNumberedList {
            generation,
            sections: vec![section(
                SectionKind::Agent,
                labels.iter().map(|label| entry(label)).collect(),
            )],
        }
    }

    fn section(kind: SectionKind, entries: Vec<VoiceNumberedEntry>) -> VoiceNumberedSection {
        VoiceNumberedSection { kind, entries }
    }

    /// An agent picked by number, the way the dashboard's one section answers.
    fn agent(number: usize) -> NumberAnswer {
        NumberAnswer::Selected {
            section: SectionKind::Agent,
            number,
        }
    }

    /// The agents a number could mean, in the order offered.
    fn agents(numbers: &[usize]) -> NumberAnswer {
        NumberAnswer::Ambiguous {
            choices: numbers
                .iter()
                .map(|&number| NumberRef {
                    section: SectionKind::Agent,
                    number,
                })
                .collect(),
        }
    }

    fn out_of_range(number: usize) -> NumberAnswer {
        NumberAnswer::OutOfRange {
            section: None,
            number,
            elsewhere: Vec::new(),
        }
    }

    const DASHBOARD: [&str; 4] = [
        "Plan / architecture",
        "Desktop implementation",
        "Contract review",
        "Docs",
    ];

    #[test]
    fn a_bare_number_selects_the_item_showing_it() {
        let heard = list(7, &DASHBOARD);
        for said in [
            "three",
            "3",
            "number three",
            "Number 3.",
            "the third one",
            "third",
            "3rd",
            "okay three please",
        ] {
            assert_eq!(answer(said, &heard, 7), agent(3), "{said}");
        }
        assert_eq!(answer("the last one", &heard, 7), agent(4));
    }

    #[test]
    fn anything_but_a_bare_number_is_left_to_the_resolver() {
        let heard = list(7, &DASHBOARD);
        for said in ["open docs", "three agents", "open the third one", ""] {
            assert_eq!(answer(said, &heard, 7), NumberAnswer::NotNumber, "{said}");
        }
        // No numbered list on screen: a number is an ordinary utterance.
        assert_eq!(answer("three", &list(7, &[]), 7), NumberAnswer::NotNumber);
    }

    #[test]
    fn a_number_no_item_shows_is_refused() {
        let heard = list(7, &DASHBOARD);
        assert_eq!(answer("seven", &heard, 7), out_of_range(7));
        assert_eq!(answer("number 12", &heard, 7), out_of_range(12));
        assert_eq!(answer("number 0", &heard, 7), out_of_range(0));
    }

    /// Qodo #16 on PR #1451: "zero" said as a word is the same number as
    /// "number 0", and no item shows it, so it is refused here rather than
    /// sent to the Commands backend. So is a number too large to read.
    #[test]
    fn zero_and_a_number_too_large_to_read_are_refused() {
        let heard = list(7, &DASHBOARD);
        for said in ["zero", "number zero", "Number zero.", "option zero"] {
            assert_eq!(answer(said, &heard, 7), out_of_range(0), "{said}");
        }
        for said in ["99999999999999999999", "number 99999999999999999999"] {
            assert_eq!(answer(said, &heard, 7), out_of_range(usize::MAX), "{said}");
        }
        assert_eq!(
            answer("select agent zero", &heard, 7),
            NumberAnswer::OutOfRange {
                section: Some(SectionKind::Agent),
                number: 0,
                elsewhere: Vec::new(),
            }
        );
    }

    #[test]
    fn an_answer_about_a_list_that_changed_is_refused() {
        let heard = list(7, &DASHBOARD);
        assert_eq!(answer("three", &heard, 8), NumberAnswer::Stale);
        // Whatever the number: it was said about a list that is not on screen.
        assert_eq!(answer("seven", &heard, 8), NumberAnswer::Stale);
        // Not a number at all is still not one, changed list or not.
        assert_eq!(answer("open docs", &heard, 8), NumberAnswer::NotNumber);
    }

    #[test]
    fn a_count_that_is_also_another_items_name_offers_both() {
        let heard = list(
            7,
            &["Plan / architecture", "orchestrator-1", "Contract review"],
        );
        assert_eq!(answer("one", &heard, 7), agents(&[1, 2]));
        assert_eq!(answer("1", &heard, 7), agents(&[1, 2]));
        assert_eq!(answer("number one", &heard, 7), agents(&[1, 2]));
        // A position by grammar names no agent.
        assert_eq!(answer("the first one", &heard, 7), agent(1));
        assert_eq!(answer("first", &heard, 7), agent(1));
        // A name that only CONTAINS the number elsewhere does not collide.
        assert_eq!(
            answer("two", &list(7, &["1 builder", "b", "c"]), 7),
            agent(2)
        );
        // A name that IS the number, in words.
        assert_eq!(
            answer("two", &list(7, &["a", "b", "two"]), 7),
            agents(&[2, 3])
        );
    }

    #[test]
    fn a_collision_counts_every_name_but_not_the_item_showing_the_number() {
        let mut heard = list(7, &["agent-1", "Builder", "c"]);
        // The item showing 1 is itself called "…-1": nothing to choose between.
        assert_eq!(answer("one", &heard, 7), agent(1));
        // Another item whose ROLE ends in 2 collides as its label would.
        heard.sections[0].entries[2].names = vec!["worker 2".to_string()];
        assert_eq!(answer("two", &heard, 7), agents(&[2, 3]));
    }

    /// A page longer than nine items, labelled so no name ends in a number.
    fn long_page(len: usize) -> VoiceNumberedList {
        VoiceNumberedList {
            generation: 7,
            sections: vec![section(
                SectionKind::Agent,
                (0..len)
                    .map(|at| {
                        entry(&format!(
                            "dir {}{}",
                            char::from(b'a' + (at / 26) as u8),
                            char::from(b'a' + (at % 26) as u8)
                        ))
                    })
                    .collect(),
            )],
        }
    }

    /// Scenario: a page shows more than nine numbers, so every number on it can
    /// be said in words as well as digits: "twelve", "number twenty-three",
    /// "the twelfth", "twentieth", "thirty-fifth".
    #[test]
    fn numbers_past_nine_are_understood_in_words() {
        let heard = long_page(40);
        for (said, number) in [
            ("ten", 10),
            ("eleven", 11),
            ("twelve", 12),
            ("number twelve", 12),
            ("nineteen", 19),
            ("twenty", 20),
            ("twenty one", 21),
            ("twenty three", 23),
            ("twenty-three", 23),
            ("Number twenty-three.", 23),
            ("the twenty-three", 23),
            ("23", 23),
            ("number 23", 23),
            ("23rd", 23),
            ("tenth", 10),
            ("the twelfth", 12),
            ("the twelfth one", 12),
            ("nineteenth", 19),
            ("twentieth", 20),
            ("the twentieth one", 20),
            ("twenty first", 21),
            ("the twenty-first one", 21),
            ("thirty", 30),
            ("thirty-fifth", 35),
            ("forty", 40),
        ] {
            assert_eq!(answer(said, &heard, 7), agent(number), "{said}");
        }
        assert_eq!(answer("ninety-nine", &heard, 7), out_of_range(99));
        assert_eq!(answer("fiftieth", &heard, 7), out_of_range(50));
        for said in [
            "twenty agents",
            "three twenty",
            "twenty twenty",
            "ten three",
            "twenty tenth",
            "one hundred",
        ] {
            assert_eq!(answer(said, &heard, 7), NumberAnswer::NotNumber, "{said}");
        }
    }

    /// Scenario: "twelve" said as a count collides with another item whose
    /// name ends in 12, as "one" does with `orchestrator-1`; "the twelfth" is a
    /// position and does not.
    #[test]
    fn a_count_past_nine_collides_with_a_name_ending_in_it() {
        let mut heard = long_page(14);
        heard.sections[0].entries[13].label = "worker 12".to_string();
        assert_eq!(answer("twelve", &heard, 7), agents(&[12, 14]));
        assert_eq!(answer("the twelfth", &heard, 7), agent(12));
        heard.sections[0].entries[13].label = "worker twelve".to_string();
        assert_eq!(answer("12", &heard, 7), agents(&[12, 14]));
    }

    /// The New agent dialog's three sections: two daemons, four directory
    /// rows (`..` first), three Mode chips.
    fn dialog(generation: u64) -> VoiceNumberedList {
        let item = |kind: NumberedKind, label: &str| VoiceNumberedEntry {
            kind,
            value: label.to_lowercase(),
            deck_id: None,
            label: label.to_string(),
            names: Vec::new(),
        };
        VoiceNumberedList {
            generation,
            sections: vec![
                section(
                    SectionKind::Deck,
                    vec![
                        item(NumberedKind::Deck, "dev box"),
                        item(NumberedKind::Deck, "build box"),
                    ],
                ),
                section(
                    SectionKind::Directory,
                    vec![
                        item(NumberedKind::Parent, ".."),
                        item(NumberedKind::Directory, "demo-project"),
                        item(NumberedKind::Directory, "scratch"),
                        item(NumberedKind::Directory, "notes"),
                    ],
                ),
                section(
                    SectionKind::Mode,
                    vec![
                        item(NumberedKind::Mode, "No mode"),
                        item(NumberedKind::Mode, "Schedule"),
                        item(NumberedKind::Mode, "Dispatcher"),
                    ],
                ),
            ],
        }
    }

    fn selected(section: SectionKind, number: usize) -> NumberAnswer {
        NumberAnswer::Selected { section, number }
    }

    fn at(section: SectionKind, number: usize) -> NumberRef {
        NumberRef { section, number }
    }

    /// Scenario: round 4's defect — "Select directory 13", "Select daemon 1",
    /// "choose mode 2" and "open agent 3" name a section and a number. Each is
    /// answered against that section here, never left to the Commands backend.
    #[test]
    fn a_section_word_and_a_number_select_in_that_section() {
        let heard = dialog(7);
        for (said, section, number) in [
            ("directory 3", SectionKind::Directory, 3),
            ("Select directory 3.", SectionKind::Directory, 3),
            ("select the directory 3", SectionKind::Directory, 3),
            ("directory number four", SectionKind::Directory, 4),
            ("open folder three", SectionKind::Directory, 3),
            ("go to directories 2", SectionKind::Directory, 2),
            ("enter directory 1", SectionKind::Directory, 1),
            ("Select daemon 1", SectionKind::Deck, 1),
            ("daemon two", SectionKind::Deck, 2),
            ("pick deck 2", SectionKind::Deck, 2),
            ("switch to daemon 2", SectionKind::Deck, 2),
            ("choose mode 2", SectionKind::Mode, 2),
            ("Mode two.", SectionKind::Mode, 2),
            ("modes 3", SectionKind::Mode, 3),
            ("mode the third one", SectionKind::Mode, 3),
        ] {
            assert_eq!(answer(said, &heard, 7), selected(section, number), "{said}");
        }
        let dashboard = list(7, &DASHBOARD);
        for said in ["open agent 3", "agent three", "select agent number 3"] {
            assert_eq!(answer(said, &dashboard, 7), agent(3), "{said}");
        }
    }

    /// Scenario: every section is numbered from 1, so a bare number shown by
    /// one section acts there, and one shown by several is a choice between
    /// exactly those, in on-screen order.
    #[test]
    fn a_bare_number_acts_where_one_section_shows_it_and_asks_where_several_do() {
        let heard = dialog(7);
        assert_eq!(
            answer("four", &heard, 7),
            selected(SectionKind::Directory, 4)
        );
        assert_eq!(
            answer("three", &heard, 7),
            NumberAnswer::Ambiguous {
                choices: vec![at(SectionKind::Directory, 3), at(SectionKind::Mode, 3)]
            }
        );
        assert_eq!(
            answer("number two", &heard, 7),
            NumberAnswer::Ambiguous {
                choices: vec![
                    at(SectionKind::Deck, 2),
                    at(SectionKind::Directory, 2),
                    at(SectionKind::Mode, 2),
                ]
            }
        );
        assert_eq!(answer("five", &heard, 7), out_of_range(5));
    }

    /// Scenario: a section word with a number that section does not show is
    /// refused, naming any other section that does show it — never acted on
    /// there. A section that numbers nothing on screen is refused too.
    #[test]
    fn a_number_its_section_does_not_show_is_refused() {
        let heard = dialog(7);
        assert_eq!(
            answer("select daemon 4", &heard, 7),
            NumberAnswer::OutOfRange {
                section: Some(SectionKind::Deck),
                number: 4,
                elsewhere: vec![SectionKind::Directory],
            }
        );
        assert_eq!(
            answer("select daemon 3", &heard, 7),
            NumberAnswer::OutOfRange {
                section: Some(SectionKind::Deck),
                number: 3,
                elsewhere: vec![SectionKind::Directory, SectionKind::Mode],
            }
        );
        assert_eq!(
            answer("select directory 99", &heard, 7),
            NumberAnswer::OutOfRange {
                section: Some(SectionKind::Directory),
                number: 99,
                elsewhere: Vec::new(),
            }
        );
        assert_eq!(
            answer("mode 2", &list(7, &DASHBOARD), 7),
            NumberAnswer::NotNumbered {
                section: SectionKind::Mode,
                shown: vec![SectionKind::Agent],
            }
        );
        // A section declared empty numbers nothing.
        let mut empty_modes = dialog(7);
        empty_modes.sections[2].entries.clear();
        assert_eq!(
            answer("mode 1", &empty_modes, 7),
            NumberAnswer::NotNumbered {
                section: SectionKind::Mode,
                shown: vec![SectionKind::Deck, SectionKind::Directory],
            }
        );
        // Changed since: stale, whichever section was said.
        assert_eq!(answer("directory 3", &heard, 8), NumberAnswer::Stale);
        assert_eq!(answer("mode 9", &heard, 8), NumberAnswer::Stale);
    }

    /// Scenario: a verb leads a section word or nothing at all — "open the
    /// third one", "select 3" and "open docs" stay commands for the resolver.
    #[test]
    fn a_verb_without_a_section_word_is_left_to_the_resolver() {
        let heard = dialog(7);
        for said in [
            "open the third one",
            "select 3",
            "open three",
            "go to 2",
            "open docs",
            "directory scratch",
            "select directory",
            "directory 3 please open",
            "mode 2 3",
        ] {
            assert_eq!(answer(said, &heard, 7), NumberAnswer::NotNumber, "{said}");
        }
    }

    /// Scenario: the collision rule holds within a section. A Mode chip
    /// ending in 1 joins "one" said bare (both sections show 1), and does not
    /// join "directory 1", whose section has no name ending in 1, nor "mode
    /// one", which names the chip showing 1 by its section.
    #[test]
    fn a_collision_is_counted_within_its_own_section() {
        let mut heard = dialog(7);
        heard.sections[2].entries[1].label = "Orch: build 1".to_string();
        assert_eq!(
            answer("directory 1", &heard, 7),
            selected(SectionKind::Directory, 1)
        );
        assert_eq!(
            answer("mode one", &heard, 7),
            selected(SectionKind::Mode, 1)
        );
        assert_eq!(
            answer("one", &heard, 7),
            NumberAnswer::Ambiguous {
                choices: vec![
                    at(SectionKind::Deck, 1),
                    at(SectionKind::Directory, 1),
                    at(SectionKind::Mode, 1),
                    at(SectionKind::Mode, 2),
                ]
            }
        );
        let dashboard = list(7, &["Plan / architecture", "orchestrator-1", "c"]);
        assert_eq!(answer("agent 1", &dashboard, 7), agent(1));
        assert_eq!(
            answer("the first agent", &dashboard, 7),
            NumberAnswer::NotNumber
        );
        assert_eq!(answer("agent the first one", &dashboard, 7), agent(1));
    }

    /// Scenario: round 4's crowded page — row 13 is `folder-12` and row 14 is
    /// `folder-13`. "Select directory 13" enters row 13; "folder 13" is also
    /// row 14's name, so it offers both, as "agent 1" does with `agent-1`.
    /// Qodo on PR #1451: a name ending in a number said in two words —
    /// "worker twenty three" — is that whole number, so "twenty three" offers
    /// it beside row 23 and a bare "three" does not.
    #[test]
    fn a_name_ending_in_a_compound_number_collides_with_that_number() {
        let mut labels: Vec<String> = (1..=30).map(|at| format!("Task {at}")).collect();
        labels[4] = "worker twenty three".to_string();
        labels[6] = "agent twenty-three".to_string();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let heard = list(7, &labels);
        assert_eq!(answer("twenty three", &heard, 7), agents(&[23, 5, 7]));
        assert_eq!(answer("number 23", &heard, 7), agents(&[23, 5, 7]));
        assert_eq!(answer("three", &heard, 7), agent(3));
        // After a section word, only the name that is the word and the number.
        assert_eq!(answer("agent twenty three", &heard, 7), agents(&[23, 7]));
        assert_eq!(answer("agent three", &heard, 7), agent(3));
    }

    #[test]
    fn a_section_word_collides_only_with_a_name_that_is_what_was_said() {
        let folder = |label: String| VoiceNumberedEntry {
            kind: NumberedKind::Directory,
            value: format!("/home/dev/{label}"),
            deck_id: None,
            label,
            names: Vec::new(),
        };
        let mut entries = vec![VoiceNumberedEntry {
            kind: NumberedKind::Parent,
            value: "/home".to_string(),
            deck_id: None,
            label: "..".to_string(),
            names: Vec::new(),
        }];
        entries.extend((1..=20).map(|at| folder(format!("folder-{at}"))));
        let heard = VoiceNumberedList {
            generation: 7,
            sections: vec![section(SectionKind::Directory, entries)],
        };
        for said in [
            "Select directory 13",
            "directory thirteen",
            "go to directory number 13",
        ] {
            assert_eq!(
                answer(said, &heard, 7),
                selected(SectionKind::Directory, 13),
                "{said}"
            );
        }
        let both = NumberAnswer::Ambiguous {
            choices: vec![
                at(SectionKind::Directory, 13),
                at(SectionKind::Directory, 14),
            ],
        };
        assert_eq!(answer("open folder 13", &heard, 7), both);
        assert_eq!(answer("folder thirteen", &heard, 7), both);
        // A position is never a name.
        assert_eq!(
            answer("folder the thirteenth", &heard, 7),
            selected(SectionKind::Directory, 13)
        );
        // Bare, the count still collides with every name ending in it.
        assert_eq!(answer("thirteen", &heard, 7), both);
        let dashboard = list(7, &["Plan", "Docs", "agent-1"]);
        assert_eq!(answer("open agent 1", &dashboard, 7), agents(&[1, 3]));
    }

    #[test]
    fn the_declaration_reads_the_webviews_shape() {
        let heard: VoiceNumberedList = serde_json::from_str(
            r#"{"generation":3,"sections":[{"kind":"directory","entries":[{"kind":"parent","value":"/home","label":".."}]},{"kind":"agent","entries":[{"kind":"agent","value":"a1","deckId":"deck-x","label":"Docs","names":["docs","claude"]}]}]}"#,
        )
        .expect("the webview's declaration parses");
        assert_eq!(heard.sections[0].kind, SectionKind::Directory);
        assert_eq!(heard.sections[0].entries[0].kind, NumberedKind::Parent);
        assert_eq!(
            heard.sections[1].entries[0].deck_id.as_deref(),
            Some("deck-x")
        );
        assert_eq!(heard.len(), 2);
        // The Daemon selector's menu, numbered while it is open.
        let menu: VoiceNumberedEntry = serde_json::from_str(
            r#"{"kind":"deck_switch","value":"deck-build","label":"build box"}"#,
        )
        .expect("a selector entry parses");
        assert_eq!(menu.kind, NumberedKind::DeckSwitch);
        assert!(
            serde_json::from_str::<VoiceNumberedList>(
                r#"{"generation":3,"sections":[],"extra":1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<VoiceNumberedList>(r#"{"generation":3,"entries":[]}"#).is_err()
        );
        assert_eq!(
            serde_json::to_value(NumberAnswer::Ambiguous {
                choices: vec![at(SectionKind::Directory, 3), at(SectionKind::Mode, 3)]
            })
            .expect("serialises"),
            serde_json::json!({"kind": "ambiguous", "choices": [
                {"section": "directory", "number": 3},
                {"section": "mode", "number": 3},
            ]}),
        );
        assert_eq!(
            serde_json::to_value(selected(SectionKind::Deck, 1)).expect("serialises"),
            serde_json::json!({"kind": "selected", "section": "deck", "number": 1}),
        );
        assert_eq!(
            serde_json::to_value(out_of_range(9)).expect("serialises"),
            serde_json::json!({"kind": "out_of_range", "number": 9, "elsewhere": []}),
        );
        assert_eq!(
            serde_json::to_value(NumberAnswer::NotNumbered {
                section: SectionKind::Mode,
                shown: vec![SectionKind::Agent],
            })
            .expect("serialises"),
            serde_json::json!({"kind": "not_numbered", "section": "mode", "shown": ["agent"]}),
        );
        assert_eq!(
            serde_json::to_value(NumberAnswer::Stale).expect("serialises"),
            serde_json::json!({"kind": "stale"})
        );
    }
}
