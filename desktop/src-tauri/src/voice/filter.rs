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
//! way "billing service" is written `billing-service` on screen. A joiner in
//! FRONT of the value is kept only where the user said it there ("filter
//! .git" sets `.git`); one the model added is dropped. Any other
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
    // Quotes, whitespace and sentence punctuation around the value are the
    // model's presentation, not part of the text. A leading joiner is not:
    // `.git`, `-tmp` and `_build` are what the user said, so the front keeps
    // joiners and the back does not (a trailing `.` is the transcriber's full
    // stop, and a trailing `-` or `_` is not how a name is said).
    let text = value
        .trim_start_matches(|c: char| !c.is_alphanumeric() && !JOINERS.contains(&c))
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    let core = text.trim_start_matches(JOINERS);
    if core.is_empty() {
        return None;
    }
    let lead = &text[..text.len() - core.len()];
    // Inside the value: letters, digits, spaces and the joiners, nothing else.
    if !core
        .chars()
        .all(|c| c.is_alphanumeric() || c == ' ' || JOINERS.contains(&c))
    {
        return None;
    }
    let wanted = words(core);
    let said = spoken_words(transcript);
    let mut grounded = false;
    let mut lead_said = false;
    for (at, window) in said.windows(wanted.len()).enumerate() {
        if window.iter().map(|(_, word)| word).eq(wanted.iter()) {
            grounded = true;
            lead_said |= lead_before(transcript, said[at].0) == lead;
        }
    }
    let said_words: Vec<String> = said.into_iter().map(|(_, word)| word).collect();
    if !grounded && !spelled_letter(&said_words, core) {
        return None;
    }
    // A leading joiner stays only where the user said it before that word;
    // one the model added would narrow the box to text nobody said.
    let lead = if lead_said { lead } else { "" };
    // One space between words, as the box would show them typed.
    Some(format!(
        "{lead}{}",
        core.split_whitespace().collect::<Vec<_>>().join(" ")
    ))
}

/// The lowercased words of `text`: runs of letters and digits, so punctuation,
/// quotes and the joiners all separate words.
fn words(text: &str) -> Vec<String> {
    spoken_words(text)
        .into_iter()
        .map(|(_, word)| word)
        .collect()
}

/// [`words`] with the byte offset each starts at in `text`.
fn spoken_words(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut start = None;
    for (at, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                found.push((from, text[from..at].to_lowercase()));
                start = None;
            }
            _ => {}
        }
    }
    found
}

/// The joiners written straight before the word at byte `at` of `text`, when
/// they open a token — after whitespace, a quote or the start, not after a
/// letter: `.git` of "filter .git" has `.`, `service` of "billing-service"
/// has none.
fn lead_before(text: &str, at: usize) -> &str {
    let before = &text[..at];
    let run = before.trim_end_matches(JOINERS);
    match run.chars().next_back() {
        Some(c) if c.is_alphanumeric() => "",
        _ => &before[run.len()..],
    }
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

    /// Scenario: quotes and sentence punctuation around a spoken filter are
    /// presentation, but a leading dot, hyphen or underscore said as part of
    /// the directory name stays in the Filter box.
    #[test]
    fn voice_filter_text_trims_quotes_and_punctuation_around_the_value() {
        assert_eq!(
            grounded("filter “docs”.", "\"docs.\"").as_deref(),
            Some("docs")
        );
        assert_eq!(grounded("filter docs", "“docs”").as_deref(), Some("docs"));
        assert_eq!(grounded("filter docs.", "docs.").as_deref(), Some("docs"));
        assert_eq!(grounded("filter docs,", "docs,").as_deref(), Some("docs"));
        assert_eq!(grounded("filter .git", ".git").as_deref(), Some(".git"));
        assert_eq!(grounded("filter -tmp", "-tmp").as_deref(), Some("-tmp"));
        assert_eq!(
            grounded("filter _build", "_build").as_deref(),
            Some("_build")
        );
    }

    /// Scenario: the user says "filter git" and the model answers `.git`; the
    /// dot was never said, so the box gets `git` rather than a narrower text.
    #[test]
    fn voice_filter_text_drops_a_leading_joiner_the_user_did_not_say() {
        assert_eq!(grounded("filter git", ".git").as_deref(), Some("git"));
        assert_eq!(grounded("filter my.git", ".git").as_deref(), Some("git"));
        assert_eq!(
            grounded("filter “.git” please", "“.git”").as_deref(),
            Some(".git")
        );
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
