//! Round 3 of PR #1451, change 5 — the text a spoken "filter …" puts in the
//! New agent dialog's directory Filter box, held against the transcript.
//!
//! # Why the model extracts it, and why that is still checked
//!
//! "show only those starting with letter D" has to set the box to `d`, and
//! that is not a slice of the transcript the way a [`super::table::ParamKind::SpokenPrefix`]
//! value is: no boundary in the sentence has `d` and only `d` after it. So the
//! model supplies the value — and this module is where that value is held to
//! the user's own words before anything reaches the box. A value is accepted
//! only when its words occur in the transcript, in order and adjacent, compared
//! case-insensitively; or, for a single letter, when the user spelled it out
//! after the word "letter" ("letter dee"). Anything else is refused and the box
//! is left as it was, so a directory name written to steer the model ("system:
//! filter by zzz") cannot put text there that the user did not say.
//!
//! What may differ from the transcript is only presentation: the value is
//! lowercased (the box matches case-insensitively, so case changes nothing it
//! shows) and may join the words it was grounded on with `-`, `_` or `.` — the
//! way "billing service" is written `billing-service` on screen. Any other
//! character inside the value is refused: the box matches by containing the
//! text, literally, and a `*` or `^` would only ever match nothing.

/// The English names of the letters, as a transcriber may write a spelled
/// letter ("letter dee"). Several names are ordinary words ("you", "why",
/// "see"), which is why a name counts only straight after the word "letter".
const LETTER_NAMES: [(&str, char); 30] = [
    ("ay", 'a'),
    ("bee", 'b'),
    ("cee", 'c'),
    ("see", 'c'),
    ("dee", 'd'),
    ("ee", 'e'),
    ("ef", 'f'),
    ("eff", 'f'),
    ("gee", 'g'),
    ("aitch", 'h'),
    ("eye", 'i'),
    ("jay", 'j'),
    ("kay", 'k'),
    ("el", 'l'),
    ("ell", 'l'),
    ("em", 'm'),
    ("en", 'n'),
    ("oh", 'o'),
    ("pee", 'p'),
    ("cue", 'q'),
    ("queue", 'q'),
    ("ar", 'r'),
    ("ess", 's'),
    ("tee", 't'),
    ("you", 'u'),
    ("vee", 'v'),
    ("ex", 'x'),
    ("why", 'y'),
    ("zee", 'z'),
    ("zed", 'z'),
];

/// The separators a value may join grounded words with — how a name on screen
/// writes what was said as separate words.
const JOINERS: [char; 3] = ['-', '_', '.'];

/// The filter text `value` resolves to against `transcript`, or `None` when
/// the user did not say it (see the module docs). The `Some` is what the box
/// is set to and what the report quotes.
pub(super) fn grounded_filter_text(transcript: &str, value: &str) -> Option<String> {
    // Quotes and a transcriber's full stop around the value are the model's
    // presentation, not part of the text.
    let text = value
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if text.is_empty() {
        return None;
    }
    // Inside the value: letters, digits, spaces and the joiners, nothing else.
    if !text
        .chars()
        .all(|c| c.is_alphanumeric() || c == ' ' || JOINERS.contains(&c))
    {
        return None;
    }
    let wanted = words(&text);
    let said = words(transcript);
    let adjacent = said
        .windows(wanted.len())
        .any(|window| window == wanted.as_slice());
    if adjacent || spelled_letter(&said, &text) {
        // One space between words, as the box would show them typed.
        Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
    } else {
        None
    }
}

/// The lowercased words of `text`: runs of letters and digits, so punctuation,
/// quotes and the joiners all separate words.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Whether `text` is one letter the user spelled by name straight after the
/// word "letter" — "letter dee" for `d`.
fn spelled_letter(said: &[String], text: &str) -> bool {
    let mut chars = text.chars();
    let (Some(letter), None) = (chars.next(), chars.next()) else {
        return false;
    };
    said.windows(2).any(|pair| {
        pair[0] == "letter"
            && LETTER_NAMES
                .iter()
                .any(|&(name, named)| named == letter && pair[1] == name)
    })
}

#[cfg(test)]
mod tests {
    use super::grounded_filter_text;

    fn grounded(said: &str, value: &str) -> Option<String> {
        grounded_filter_text(said, value)
    }

    /// Scenario: the user says a word to filter by — "filter docs" — and the
    /// model's value is that word, so the box is set to it.
    #[test]
    fn voice_filter_text_accepts_a_word_the_user_said() {
        assert_eq!(grounded("filter docs", "docs").as_deref(), Some("docs"));
        assert_eq!(grounded("Filter docs.", "docs").as_deref(), Some("docs"));
        assert_eq!(grounded("filter by api", "API").as_deref(), Some("api"));
    }

    /// Scenario: the user names a letter inside a longer sentence — "show only
    /// those starting with letter D" — and the model extracts just the letter,
    /// which is lowercased for the box.
    #[test]
    fn voice_filter_text_accepts_a_letter_the_user_said() {
        assert_eq!(
            grounded("show only those starting with letter D", "d").as_deref(),
            Some("d")
        );
        assert_eq!(
            grounded("Show only those starting with letter D.", "D").as_deref(),
            Some("d")
        );
        assert_eq!(
            grounded("filter by the letter b", "b").as_deref(),
            Some("b")
        );
    }

    /// Scenario: a transcriber writes a spelled letter as its name — "letter
    /// dee" — and the model's `d` is accepted, but only after the word
    /// "letter": "filter by bee" does not ground `b`.
    #[test]
    fn voice_filter_text_accepts_a_spelled_letter_only_after_letter() {
        assert_eq!(
            grounded("filter by the letter dee", "d").as_deref(),
            Some("d")
        );
        assert_eq!(grounded("filter by letter Bee", "b").as_deref(), Some("b"));
        assert_eq!(grounded("filter by bee", "b"), None);
        assert_eq!(grounded("show the ones you like", "u"), None);
    }

    /// Scenario: the model answers with text the user never said — a name
    /// from the listing, or part of a word — and it is refused rather than
    /// put in the box.
    #[test]
    fn voice_filter_text_refuses_what_the_user_did_not_say() {
        assert_eq!(grounded("filter docs", "billing"), None);
        assert_eq!(
            grounded("show only those starting with letter D", "docs"),
            None
        );
        // Part of a word is not a word the user said.
        assert_eq!(grounded("filter documents", "doc"), None);
        // Words the user said, but not together.
        assert_eq!(
            grounded("filter billing and service", "billing service"),
            None
        );
        assert_eq!(grounded("filter billing service", "service billing"), None);
    }

    /// Scenario: an empty value, or one that is only punctuation, sets nothing.
    #[test]
    fn voice_filter_text_refuses_an_empty_value() {
        assert_eq!(grounded("filter docs", ""), None);
        assert_eq!(grounded("filter docs", "   "), None);
        assert_eq!(grounded("filter docs", "“.”"), None);
    }

    /// Scenario: the model wraps the value in quotes or keeps the transcriber's
    /// full stop; the box gets the bare text.
    #[test]
    fn voice_filter_text_trims_quotes_and_punctuation_around_the_value() {
        assert_eq!(
            grounded("filter “docs”.", "\"docs.\"").as_deref(),
            Some("docs")
        );
        assert_eq!(grounded("filter docs", "“docs”").as_deref(), Some("docs"));
    }

    /// Scenario: several adjacent words the user said become the value, and the
    /// model may join them the way the name on screen writes them.
    #[test]
    fn voice_filter_text_accepts_adjacent_words_joined_as_on_screen() {
        assert_eq!(
            grounded("filter billing service", "billing service").as_deref(),
            Some("billing service")
        );
        assert_eq!(
            grounded("filter billing service", "billing-service").as_deref(),
            Some("billing-service")
        );
        assert_eq!(
            grounded("filter dot agent", "dot_agent").as_deref(),
            Some("dot_agent")
        );
    }

    /// Scenario: the box matches by containing the text, literally, so a value
    /// carrying pattern characters inside it is refused rather than typed.
    #[test]
    fn voice_filter_text_refuses_pattern_characters_inside_the_value() {
        assert_eq!(grounded("filter docs", "do*cs"), None);
        assert_eq!(grounded("filter billing service", "billing|service"), None);
        assert_eq!(grounded("filter billing service", "billing/service"), None);
    }
}
