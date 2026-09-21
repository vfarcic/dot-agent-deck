//! Where the user's words stop being a command and start being their words.
//!
//! PRD #802 D6, rebuilt. What shipped first was a **mode**: *"type to the
//! tester"* aimed the microphone and every later utterance was typed until an
//! exit phrase was heard. The mode is gone, and with it the two failures D6
//! itself warned about — a missed exit typed the exit phrase into the agent, a
//! false positive truncated somebody mid-sentence. What replaces it is one
//! utterance at a time: open the agent's pane, then say what you want typed.
//!
//! # The one property this module exists to hold
//!
//! **The app supplies the typed text; the model may only say where it starts.**
//! A dictation command with a free-text *content* parameter would mean the
//! model rewriting the user's words on their way into an agent's prompt, which
//! is unacceptable in the one place fidelity matters most. Locating a boundary
//! is a different job from supplying the content, and the difference is
//! verifiable: whatever the model returns as the introducing words is checked
//! against **our** transcript before a single character is typed, and the text
//! that goes to the agent is a slice of that transcript and never of the
//! model's answer. [`strip_opening`] is the whole of that check, and it fails
//! closed — a prefix that is not genuinely a prefix types nothing.
//!
//! # Two paths, one rule
//!
//! * the **fast path**: an utterance that opens with one of
//!   [`DICTATION_OPENERS`] is dictation, decided here, with no backend call at
//!   all.
//! * the **model fallback**: everything else goes to the resolver as usual, and
//!   an answer of `dictate_to_agent` carries the introducing words as its
//!   `prefix` param — which is then verified by the same [`strip_opening`] the
//!   fast path uses.
//!
//! **The fast path is an optimisation and NOT the vocabulary.** A closed list
//! of openers is the guess-the-magic-word problem this feature exists to
//! prevent: *"let's write a prompt …"*, *"tell it to …"*, *"ask it to …"* are
//! all things a user says and none of them is in the list. They work, because
//! the model finds the boundary for them. The list only decides who pays for a
//! round trip, and since the resolver runs on every utterance anyway to decide
//! whether it is a command, the fallback costs **no extra** call — it is the
//! call that was always going to happen.

use std::ops::Range;

/// The openers an utterance may start with to be typed without asking anybody.
///
/// **Four verbs of producing text, and that is the whole test for membership.**
/// An utterance that begins *"type …"*, *"write …"*, *"say …"* or *"dictate
/// …"* is a request to put words somewhere; no row in `commands.toml` is about
/// producing text except the dictation one, so there is nothing for such an
/// utterance to be confused with. (`voice_dictation_no_opener_starts_another
/// _row_s_own_trigger_phrase` is the mechanical half of that claim: none of
/// these words opens a quoted trigger phrase in any other row's description.)
///
/// **What is deliberately NOT here matters more than what is.** *"submit"* was
/// named by the product owner as a plausible opener and is in
/// [`SUBMIT_PHRASES`] instead: *"submit"* on its own means press Enter, and a
/// word that means two different things is exactly the ambiguity the fast path
/// may not resolve by guessing. *"tell"*, *"ask"* and *"let's"* are absent for
/// the opposite reason — they are real openers, but where the content begins
/// after them varies (*"tell it to run the tests"* against *"tell the reviewer
/// that the build is green"*), and a fixed rule would cut in the wrong place.
/// The model cuts those, and the app checks the cut.
///
/// Matched as whole words against the normalised transcript, never as a
/// substring: *"typescript is confusing"* does not open with *"type"*, because
/// [`strip_opening`] compares token sequences and `typescript` is one token.
pub const DICTATION_OPENERS: [&str; 4] = ["type", "write", "say", "dictate"];

/// What presses Enter in the agent's prompt, said out loud.
///
/// **Matched against the WHOLE normalised transcript by equality, never as a
/// prefix and never as a suffix**, which is the entire defence against the
/// false positive that makes this dangerous. A trailing rule would submit
/// *"the meeting is at the"* when the user said *"type the meeting is at the
/// end"*, and no candidate phrase escapes that — *"tell him to send it"*
/// defeats *"send it"* identically. Submitting is the last thing that happens
/// to a prompt, so a half-finished instruction delivered to an agent is
/// unrecoverable. Whole-utterance equality means only an utterance that IS one
/// of these phrases submits anything.
///
/// The cost is stated rather than hidden: these six phrases are the words a
/// user cannot dictate *alone*. *"type end"* types `end`; a bare *"end"*
/// submits. That is the trade, and it is the one the product owner asked for.
///
/// **"stop" is deliberately absent**, though it was suggested. A bare *"stop"*
/// is the most likely thing somebody says when they want everything to stop,
/// and `voice_off`'s own description claims *"stop listening"*, *"stop voice"*
/// and *"stop voice control"*. Making a bare *"stop"* press Enter in an agent's
/// prompt would put the irreversible action on the word most likely to be said
/// in a panic.
///
/// **"go ahead" is absent from this list and is still a send phrase**, which is
/// worth saying because the two halves read like a contradiction.
/// `submit_prompt`'s own description names it, so a bare *"go ahead"* is
/// answered by the model rather than here — and the model is told it means
/// send. This list decides who answers an utterance, never what happens to it,
/// which is why check 13 runs one way only. The worry that kept it out — an
/// agent that has just asked *"shall I proceed?"* makes *"go ahead"* an answer
/// the user wants **typed** — is real, and keeping it out does not serve it:
/// [`strip_opening`] always strips at least one introducing token, so no
/// utterance is ever typed whole. *"type go ahead"* is how those words get
/// typed.
pub const SUBMIT_PHRASES: [&str; 6] = ["end", "send", "send it", "submit", "enter", "press enter"];

/// One transcript, reduced to the form the phrase lists are compared against.
///
/// Case, punctuation and spacing are all things a transcriber decides for
/// itself — *"Send it."*, *"send it"* and *"Send, it"* are the same two words —
/// so comparing raw text would make every phrase here a lottery on the
/// backend's punctuation model. Everything that is not a letter or a digit is a
/// separator, and what comes back is the tokens lowercased and joined by one
/// space.
pub fn normalise(text: &str) -> String {
    tokens(text)
        .into_iter()
        .map(|token| token.word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the whole utterance IS one of `phrases`.
///
/// Equality against the normalised form, which is what makes this safe to call
/// on text the user meant to have typed. See [`SUBMIT_PHRASES`] for why it is
/// never a prefix or suffix test.
pub fn whole_utterance_is(text: &str, phrases: &[&str]) -> bool {
    let phrase = normalise(text);
    phrases.contains(&phrase.as_str())
}

/// The opener this utterance begins with, if it begins with one.
///
/// Longest match first, so a multi-word opener is not shadowed by a one-word
/// one that happens to start it. There are no multi-word openers today; the
/// ordering is here so adding one is a list edit rather than a bug.
pub fn opening_with<'a>(text: &str, openers: &'a [&'a str]) -> Option<&'a str> {
    openers
        .iter()
        .copied()
        .filter(|opener| strip_opening(text, opener).is_some())
        .max_by_key(|opener| tokens(opener).len())
}

/// Everything after `opening`, verbatim — or `None` if `opening` is not
/// genuinely how this transcript starts.
///
/// **This is the fidelity guarantee, and it is a function rather than a
/// comment.** The model may mark a boundary; it may not supply text. So the
/// words it marked are compared, token for token, against the front of *our*
/// transcript, and what is returned is a slice of **that** transcript. A model
/// that answers with a prefix nobody said gets `None`, and `None` types
/// nothing — there is deliberately no path that falls back to the model's own
/// string.
///
/// # Token-wise, not byte-wise
///
/// The comparison is over normalised tokens because the model is answering from
/// a transcript it was shown as prose and will differ from it in case and
/// punctuation — *"Let's write a prompt"* against *"let's write a prompt,"* is
/// the same boundary. Comparing tokens also makes a partial-word match
/// impossible: *"type"* does not open *"typescript is confusing"*, because
/// `typescript` is one token and `type` is another.
///
/// # What comes back is the raw remainder
///
/// The returned slice is the transcript itself from the end of the last matched
/// token, with the whitespace after it removed and — at most once — a single
/// separating `:`, `,` or dash removed after that. A transcriber writes *"Type:
/// run the login tests"* as readily as *"Type run the login tests"*, and typing
/// a leading colon into an agent's prompt is a transcription artefact rather
/// than the user's word. Nothing else is touched: quotes, inner punctuation,
/// casing and spacing are the user's and survive to the agent byte for byte.
pub fn strip_opening<'a>(text: &'a str, opening: &str) -> Option<&'a str> {
    let opening = tokens(opening);
    if opening.is_empty() {
        return None;
    }
    let spoken = tokens(text);
    if spoken.len() < opening.len() {
        return None;
    }
    if !spoken
        .iter()
        .zip(opening.iter())
        .all(|(said, marked)| said.word == marked.word)
    {
        return None;
    }
    let end = spoken[opening.len() - 1].at.end;
    let rest = text[end..].trim_start();
    let rest = rest
        .strip_prefix(|character: char| {
            matches!(character, ':' | ',' | '-' | '\u{2013}' | '\u{2014}')
        })
        .unwrap_or(rest);
    Some(rest.trim_start())
}

/// One word of a transcript: where it is, and what it normalises to.
struct Token {
    at: Range<usize>,
    word: String,
}

/// Every maximal run of letters and digits, in order.
///
/// The same character class [`normalise`] describes, computed once so the
/// normalised form and the byte offsets can never disagree about where a word
/// ended — which is the only reason [`strip_opening`] can return a slice of the
/// original text after matching against a normalised one.
fn tokens(text: &str) -> Vec<Token> {
    let mut found = Vec::new();
    let mut start: Option<usize> = None;
    for (at, character) in text.char_indices() {
        if character.is_alphanumeric() {
            start.get_or_insert(at);
        } else if let Some(from) = start.take() {
            found.push(Token {
                at: from..at,
                word: text[from..at].to_lowercase(),
            });
        }
    }
    if let Some(from) = start {
        found.push(Token {
            at: from..text.len(),
            word: text[from..].to_lowercase(),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::table::table;

    #[test]
    fn voice_dictation_normalise_folds_case_punctuation_and_spacing() {
        assert_eq!(normalise("  Send,  IT.  "), "send it");
        assert_eq!(normalise("Let's write a prompt"), "let s write a prompt");
        assert_eq!(normalise(""), "");
        assert_eq!(normalise("!!!"), "");
    }

    #[test]
    fn voice_dictation_a_submit_phrase_is_matched_whole_and_never_as_a_part() {
        assert!(whole_utterance_is("End.", &SUBMIT_PHRASES));
        assert!(whole_utterance_is("send it", &SUBMIT_PHRASES));
        // The false positive the whole-utterance rule exists to make
        // impossible. Both of these were typed by a user who meant every word.
        assert!(!whole_utterance_is(
            "the meeting is at the end",
            &SUBMIT_PHRASES
        ));
        assert!(!whole_utterance_is("tell him to send it", &SUBMIT_PHRASES));
        assert!(!whole_utterance_is("end of file", &SUBMIT_PHRASES));
    }

    #[test]
    fn voice_dictation_an_opener_is_a_whole_word() {
        assert_eq!(
            opening_with("type run the tests", &DICTATION_OPENERS),
            Some("type")
        );
        // `typescript` is one token, so nothing opens with `type` here.
        assert_eq!(
            opening_with("typescript is confusing", &DICTATION_OPENERS),
            None
        );
        assert_eq!(opening_with("open the tester", &DICTATION_OPENERS), None);
    }

    #[test]
    fn voice_dictation_the_remainder_is_the_transcript_byte_for_byte() {
        // The whole point: what reaches the agent is a slice of what was heard.
        let heard = "type Run the login tests, then report \"DONE\"";
        let rest = strip_opening(heard, "type").expect("a genuine prefix");
        assert_eq!(rest, "Run the login tests, then report \"DONE\"");
        assert_eq!(rest, &heard[heard.len() - rest.len()..]);
    }

    #[test]
    fn voice_dictation_an_unusual_opener_is_stripped_by_the_marked_boundary() {
        let heard = "let's write a prompt run the tests";
        assert_eq!(
            strip_opening(heard, "let's write a prompt"),
            Some("run the tests")
        );
        // The model's own casing and punctuation differ from the transcript's
        // and must not matter — it is marking a boundary, not quoting.
        assert_eq!(
            strip_opening(heard, "Let us"),
            None,
            "different words are a different boundary"
        );
        assert_eq!(
            strip_opening(
                "Let's write a prompt: run the tests",
                "let's write a prompt"
            ),
            Some("run the tests"),
            "a transcriber's separating colon is not the user's word"
        );
    }

    #[test]
    fn voice_dictation_a_prefix_nobody_said_is_refused_rather_than_trusted() {
        // The fidelity guarantee, pinned. A model that answers with words the
        // user did not say gets nothing, and there is no path that types its
        // string instead of ours.
        assert_eq!(strip_opening("run the login tests", "please type"), None);
        assert_eq!(strip_opening("type the tests", "write"), None);
        assert_eq!(
            strip_opening("type", "type the tests"),
            None,
            "a prefix longer than the transcript is not a prefix"
        );
        assert_eq!(strip_opening("type the tests", ""), None);
        assert_eq!(
            strip_opening("run the tests", "run the tests"),
            Some(""),
            "a prefix that swallows the utterance leaves nothing to type"
        );
    }

    #[test]
    fn voice_dictation_the_two_phrase_lists_are_disjoint() {
        // The precedence between them is load-bearing — submit is checked
        // first — and the panel's own comment says the order decides nothing
        // BECAUSE they share no phrase. A shared phrase would make that
        // silently untrue.
        for opener in DICTATION_OPENERS {
            assert!(
                !SUBMIT_PHRASES.contains(&opener),
                "`{opener}` opens a dictation and submits one"
            );
        }
    }

    #[test]
    fn voice_dictation_no_opener_starts_another_row_s_own_trigger_phrase() {
        // The mechanical half of "unambiguous": a row's description offers its
        // triggers as quoted phrases, and none of those phrases begins with an
        // opener. A row that started claiming `"type ..."` would make the fast
        // path steal utterances the model should have judged.
        for row in table().rows() {
            if row.id == "dictate_to_agent" {
                continue;
            }
            for quoted in row.description.split('"').skip(1).step_by(2) {
                let claimed = normalise(quoted);
                for opener in DICTATION_OPENERS {
                    assert!(
                        opening_with(&claimed, &[opener]).is_none(),
                        "`{}` claims the phrase {quoted:?}, which the fast path would swallow",
                        row.id
                    );
                }
            }
        }
    }
}
