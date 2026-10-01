//! PR #1451 round 3, change 3 (PRD #1261) — a bare number said against the
//! numbered list on screen.
//!
//! While voice is on, the lists voice selects from — the dashboard's agents,
//! the Daemons screen's tiles, and the New agent dialog's daemons, directories
//! and modes — show a number beside each item. The webview DECLARES that list
//! with the utterance ([`VoiceNumberedList`]): the entries in on-screen order,
//! entry `i` showing number `i + 1`, and a generation that changes whenever the
//! list does. A bare number ("three", "number three", "the third one") is
//! answered HERE, against that declaration, with no Commands backend call: no
//! model is asked anything, so no observed name can steer which item is chosen.
//!
//! # What the declaration is
//!
//! Exactly the numbered items on screen, nothing behind a dialog and nothing
//! scrolled into another page: several lists visible at once (the New agent
//! dialog's three) are one declaration numbered continuously in reading
//! order. A listing that pages declares its CURRENT page only, so its numbers
//! restart at 1 on every page and a page turn is a new generation.
//!
//! # The answer
//!
//! [`answer`] reads the utterance with the numbered choice's own ordinal rules
//! ([`super::choice::ordinal`]) and refuses rather than guesses: a list that
//! changed between the moment the user looked at it and the answer
//! ([`NumberAnswer::Stale`]), and a number no item on screen shows
//! ([`NumberAnswer::OutOfRange`]). A number said as a count that some OTHER
//! item's name also is or ends in — "one" with an agent called
//! `orchestrator-1` further down — is [`NumberAnswer::Ambiguous`], which the
//! webview offers as the numbered choice.

use serde::{Deserialize, Serialize};

use super::choice::{Ordinal, is_count_word, ordinal, said_as_count};
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

/// The numbered list on screen, as the webview declared it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceNumberedList {
    /// Changes whenever the list does. [`answer`] compares the generation the
    /// utterance was spoken against with the one on screen now.
    pub generation: u64,
    /// The items in on-screen order; entry `i` shows number `i + 1`.
    pub entries: Vec<VoiceNumberedEntry>,
}

/// What one utterance says about the numbered list. Numbers are 1-based, as
/// shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NumberAnswer {
    /// Not a bare number, or no numbered list is on screen: the utterance is
    /// resolved as an ordinary command.
    NotNumber,
    /// The item showing this number.
    Selected { number: usize },
    /// No item on screen shows this number.
    OutOfRange { number: usize },
    /// The list changed after the user looked at it, so the number may now
    /// name a different item. Nothing is chosen.
    Stale,
    /// The number names the item showing it AND, said as a count, is or ends
    /// another item's name. The item showing the number comes first.
    Ambiguous { numbers: Vec<usize> },
}

/// Answer `utterance` against `heard` — the list as it stood when the user
/// spoke — given the generation on screen `now`.
///
/// In order: anything that is not a whole-utterance ordinal, or a declaration
/// with no entries, is [`NumberAnswer::NotNumber`]; a list whose generation
/// moved is [`NumberAnswer::Stale`] whatever the number; a number past the end
/// (or "number zero") is [`NumberAnswer::OutOfRange`]; then a number said as a
/// count ("one", "1", "number one" — never "first" or "the last one", which
/// are positions by grammar) that is the trailing word of another item's
/// label or name is [`NumberAnswer::Ambiguous`]; otherwise
/// [`NumberAnswer::Selected`].
pub fn answer(utterance: &str, heard: &VoiceNumberedList, now: u64) -> NumberAnswer {
    let words = whole_utterance(utterance);
    let Some(said) = ordinal(&words) else {
        return NumberAnswer::NotNumber;
    };
    if heard.entries.is_empty() {
        return NumberAnswer::NotNumber;
    }
    if heard.generation != now {
        return NumberAnswer::Stale;
    }
    let Some(at) = said.index(heard.entries.len()) else {
        let number = match said {
            Ordinal::Nth(number) => number,
            Ordinal::Last => 0,
        };
        return NumberAnswer::OutOfRange { number };
    };
    let number = at + 1;
    let counted = matches!(said, Ordinal::Nth(_)) && said_as_count(&words);
    let mut numbers = vec![number];
    if counted {
        numbers.extend(
            heard
                .entries
                .iter()
                .enumerate()
                .filter(|&(other, entry)| other != at && ends_in(entry, number))
                .map(|(other, _)| other + 1),
        );
    }
    if numbers.len() > 1 {
        NumberAnswer::Ambiguous { numbers }
    } else {
        NumberAnswer::Selected { number }
    }
}

/// Whether one of `entry`'s names is, or ends in, `number`: "orchestrator-1",
/// "worker 2", an agent whose role is "three".
fn ends_in(entry: &VoiceNumberedEntry, number: usize) -> bool {
    std::iter::once(&entry.label)
        .chain(&entry.names)
        .any(|name| {
            spoken_words(name).last().is_some_and(|last| {
                is_count_word(last)
                    && ordinal(std::slice::from_ref(last)) == Some(Ordinal::Nth(number))
            })
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
            entries: labels.iter().map(|label| entry(label)).collect(),
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
            assert_eq!(
                answer(said, &heard, 7),
                NumberAnswer::Selected { number: 3 },
                "{said}"
            );
        }
        assert_eq!(
            answer("the last one", &heard, 7),
            NumberAnswer::Selected { number: 4 }
        );
    }

    #[test]
    fn anything_but_a_bare_number_is_left_to_the_resolver() {
        let heard = list(7, &DASHBOARD);
        for said in [
            "open docs",
            "three agents",
            "open the third one",
            "",
            "zero",
        ] {
            assert_eq!(answer(said, &heard, 7), NumberAnswer::NotNumber, "{said}");
        }
        // No numbered list on screen: a number is an ordinary utterance.
        assert_eq!(answer("three", &list(7, &[]), 7), NumberAnswer::NotNumber);
    }

    #[test]
    fn a_number_no_item_shows_is_refused() {
        let heard = list(7, &DASHBOARD);
        assert_eq!(
            answer("seven", &heard, 7),
            NumberAnswer::OutOfRange { number: 7 }
        );
        assert_eq!(
            answer("number 12", &heard, 7),
            NumberAnswer::OutOfRange { number: 12 }
        );
        assert_eq!(
            answer("number 0", &heard, 7),
            NumberAnswer::OutOfRange { number: 0 }
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
        assert_eq!(
            answer("one", &heard, 7),
            NumberAnswer::Ambiguous {
                numbers: vec![1, 2]
            }
        );
        assert_eq!(
            answer("1", &heard, 7),
            NumberAnswer::Ambiguous {
                numbers: vec![1, 2]
            }
        );
        assert_eq!(
            answer("number one", &heard, 7),
            NumberAnswer::Ambiguous {
                numbers: vec![1, 2]
            }
        );
        // A position by grammar names no agent.
        assert_eq!(
            answer("the first one", &heard, 7),
            NumberAnswer::Selected { number: 1 }
        );
        assert_eq!(
            answer("first", &heard, 7),
            NumberAnswer::Selected { number: 1 }
        );
        // A name that only CONTAINS the number elsewhere does not collide.
        assert_eq!(
            answer("two", &list(7, &["1 builder", "b", "c"]), 7),
            NumberAnswer::Selected { number: 2 }
        );
        // A name that IS the number, in words.
        assert_eq!(
            answer("two", &list(7, &["a", "b", "two"]), 7),
            NumberAnswer::Ambiguous {
                numbers: vec![2, 3]
            }
        );
    }

    #[test]
    fn a_collision_counts_every_name_but_not_the_item_showing_the_number() {
        let mut heard = list(7, &["agent-1", "Builder", "c"]);
        // The item showing 1 is itself called "…-1": nothing to choose between.
        assert_eq!(
            answer("one", &heard, 7),
            NumberAnswer::Selected { number: 1 }
        );
        // Another item whose ROLE ends in 2 collides as its label would.
        heard.entries[2].names = vec!["worker 2".to_string()];
        assert_eq!(
            answer("two", &heard, 7),
            NumberAnswer::Ambiguous {
                numbers: vec![2, 3]
            }
        );
    }

    /// A page longer than nine items, labelled so no name ends in a number.
    fn long_page(len: usize) -> VoiceNumberedList {
        VoiceNumberedList {
            generation: 7,
            entries: (0..len)
                .map(|at| {
                    entry(&format!(
                        "dir {}{}",
                        char::from(b'a' + (at / 26) as u8),
                        char::from(b'a' + (at % 26) as u8)
                    ))
                })
                .collect(),
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
            assert_eq!(
                answer(said, &heard, 7),
                NumberAnswer::Selected { number },
                "{said}"
            );
        }
        assert_eq!(
            answer("ninety-nine", &heard, 7),
            NumberAnswer::OutOfRange { number: 99 }
        );
        assert_eq!(
            answer("fiftieth", &heard, 7),
            NumberAnswer::OutOfRange { number: 50 }
        );
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
        heard.entries[13].label = "worker 12".to_string();
        assert_eq!(
            answer("twelve", &heard, 7),
            NumberAnswer::Ambiguous {
                numbers: vec![12, 14]
            }
        );
        assert_eq!(
            answer("the twelfth", &heard, 7),
            NumberAnswer::Selected { number: 12 }
        );
        heard.entries[13].label = "worker twelve".to_string();
        assert_eq!(
            answer("12", &heard, 7),
            NumberAnswer::Ambiguous {
                numbers: vec![12, 14]
            }
        );
    }

    #[test]
    fn the_declaration_reads_the_webviews_shape() {
        let heard: VoiceNumberedList = serde_json::from_str(
            r#"{"generation":3,"entries":[{"kind":"parent","value":"/home","label":".."},{"kind":"agent","value":"a1","deckId":"deck-x","label":"Docs","names":["docs","claude"]}]}"#,
        )
        .expect("the webview's declaration parses");
        assert_eq!(heard.entries[0].kind, NumberedKind::Parent);
        assert_eq!(heard.entries[1].deck_id.as_deref(), Some("deck-x"));
        // The Daemon selector's menu, numbered while it is open.
        let menu: VoiceNumberedEntry = serde_json::from_str(
            r#"{"kind":"deck_switch","value":"deck-build","label":"build box"}"#,
        )
        .expect("a selector entry parses");
        assert_eq!(menu.kind, NumberedKind::DeckSwitch);
        assert!(
            serde_json::from_str::<VoiceNumberedList>(r#"{"generation":3,"entries":[],"extra":1}"#)
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(NumberAnswer::Ambiguous {
                numbers: vec![1, 3]
            })
            .expect("serialises"),
            serde_json::json!({"kind": "ambiguous", "numbers": [1, 3]}),
        );
        assert_eq!(
            serde_json::to_value(NumberAnswer::Stale).expect("serialises"),
            serde_json::json!({"kind": "stale"})
        );
    }
}
