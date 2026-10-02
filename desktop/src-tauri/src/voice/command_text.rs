//! PR #1451 round 4, decision D8 — the text a spoken "set the command to …"
//! puts in the New agent dialog's Command field, held against the transcript.
//!
//! # Why the model locates it, and why that is checked
//!
//! "Set the command to devbox run agent." has to set Command to `devbox run
//! agent`, and a sentence can carry more than the command ("make the command
//! npm run dev, please"), so the model supplies where the command is. This
//! module is where that value is held to the user's own words before anything
//! reaches the field: it is accepted only when it occurs in the transcript as
//! written, starting and ending on a word boundary, and what the field
//! receives is the TRANSCRIPT's slice — byte for byte as heard — never the
//! model's string. A command the model "fixed" or invented (a flag added, a
//! word corrected) is not in the transcript, so it is refused and the field
//! keeps what it had.
//!
//! Only presentation around the value is dropped: surrounding quotes and a
//! sentence's trailing full stop, which a transcriber adds to the end of
//! every sentence and which is never part of what the user meant to run.
//! Letter case is the one thing compared loosely, so a model that lowercased
//! "Devbox" still finds it — and the field still gets "Devbox", as heard.

/// The quote pairs that may wrap the model's value.
const QUOTES: [(char, char); 5] = [
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
    ('\u{201c}', '\u{201d}'),
    ('\u{2018}', '\u{2019}'),
];

/// The command `value` resolves to against `transcript`, or `None` when the
/// user did not say it (see the module docs). The `Some` is what the Command
/// field is set to and what the report quotes.
pub(super) fn grounded_command_text(transcript: &str, value: &str) -> Option<String> {
    let wanted = bare(value);
    if wanted.is_empty() || wanted.chars().any(char::is_control) {
        return None;
    }
    let slice = find_on_word_boundaries(transcript, wanted)?;
    // The transcript's own words, with the same presentation dropped — the
    // value matched them, so what is left is exactly what the user said.
    let said = bare(slice);
    (!said.is_empty()).then(|| said.to_string())
}

/// `value` without surrounding whitespace, a trailing sentence full stop, or
/// one pair of surrounding quotes — in either order, so `“npm run dev.”` and
/// `“npm run dev”.` both come out as `npm run dev`.
fn bare(value: &str) -> &str {
    let mut text = without_full_stop(value.trim());
    if let Some(inner) = unquoted(text) {
        text = without_full_stop(inner.trim());
    }
    text
}

/// `text` without the one full stop that ends a sentence: a single `.` after
/// a letter, a digit or a closing quote. `cd ..` and `ls ./` keep theirs.
fn without_full_stop(text: &str) -> &str {
    let Some(rest) = text.strip_suffix('.') else {
        return text;
    };
    match rest.chars().next_back() {
        Some(c) if c.is_alphanumeric() || QUOTES.iter().any(|&(_, close)| close == c) => {
            rest.trim_end()
        }
        _ => text,
    }
}

/// The inside of `text` when one quote pair wraps all of it — and only then:
/// `'a' && 'b'` opens and closes with a quote that is not one pair.
fn unquoted(text: &str) -> Option<&str> {
    QUOTES.iter().find_map(|&(open, close)| {
        let inner = text.strip_prefix(open)?.strip_suffix(close)?;
        (!inner.contains(open) && !inner.contains(close)).then_some(inner)
    })
}

/// The first slice of `transcript` equal to `wanted` — exactly, or else
/// ignoring ASCII letter case — that starts and ends on a word boundary, so a
/// part of a word the user said ("dev" of "devbox") is not a command they said.
fn find_on_word_boundaries<'a>(transcript: &'a str, wanted: &str) -> Option<&'a str> {
    let joins = |outside: Option<char>, inside: Option<char>| matches!((outside, inside), (Some(o), Some(i)) if o.is_alphanumeric() && i.is_alphanumeric());
    let bounded = |at: usize, end: usize| {
        !joins(transcript[..at].chars().next_back(), wanted.chars().next())
            && !joins(transcript[end..].chars().next(), wanted.chars().next_back())
    };
    for exact in [true, false] {
        for (at, _) in transcript.char_indices() {
            let end = at + wanted.len();
            let Some(slice) = transcript.get(at..end) else {
                continue;
            };
            let equal = if exact {
                slice == wanted
            } else {
                slice.eq_ignore_ascii_case(wanted)
            };
            if equal && bounded(at, end) {
                return Some(slice);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::grounded_command_text;

    fn grounded(said: &str, value: &str) -> Option<String> {
        grounded_command_text(said, value)
    }

    /// Scenario: the maintainer's report — "Set the command to devbox run
    /// agent." — and the model's value is those three words, with or without
    /// the sentence's full stop; the field gets exactly them.
    #[test]
    fn voice_command_text_accepts_the_words_the_user_said() {
        let said = "Set the command to devbox run agent.";
        assert_eq!(
            grounded(said, "devbox run agent").as_deref(),
            Some("devbox run agent")
        );
        assert_eq!(
            grounded(said, "devbox run agent.").as_deref(),
            Some("devbox run agent")
        );
        assert_eq!(
            grounded("make the command npm run dev", "npm run dev").as_deref(),
            Some("npm run dev")
        );
    }

    /// Scenario: the model wraps the command in quotes, or the user quoted it;
    /// the field gets the bare command.
    #[test]
    fn voice_command_text_drops_surrounding_quotes() {
        assert_eq!(
            grounded("Set the command to “npm run dev”.", "“npm run dev”").as_deref(),
            Some("npm run dev")
        );
        assert_eq!(
            grounded("set the command to npm run dev", "\"npm run dev.\"").as_deref(),
            Some("npm run dev")
        );
        assert_eq!(
            grounded("Set the command to “npm run dev.”", "npm run dev").as_deref(),
            Some("npm run dev")
        );
    }

    /// Scenario: everything but the surrounding presentation is kept byte for
    /// byte — flags, paths and dots inside the command, and the user's case
    /// even when the model's value lowercased it.
    #[test]
    fn voice_command_text_keeps_the_command_as_said() {
        assert_eq!(
            grounded(
                "set the command to claude --model haiku.",
                "claude --model haiku"
            )
            .as_deref(),
            Some("claude --model haiku")
        );
        assert_eq!(
            grounded("set the command to cd .. && make", "cd .. && make").as_deref(),
            Some("cd .. && make")
        );
        assert_eq!(
            grounded("set the command to cd ..", "cd ..").as_deref(),
            Some("cd ..")
        );
        assert_eq!(
            grounded("Set the command to Devbox Run agent.", "devbox run agent").as_deref(),
            Some("Devbox Run agent")
        );
    }

    /// Scenario: the model "fixes" or invents the command — a flag added, a
    /// word changed, a command the user never said — and it is refused rather
    /// than put in the field.
    #[test]
    fn voice_command_text_refuses_what_the_user_did_not_say() {
        let said = "Set the command to devbox run agent.";
        assert_eq!(grounded(said, "devbox run agent --verbose"), None);
        assert_eq!(grounded(said, "devbox run agents"), None);
        assert_eq!(grounded(said, "npm run dev"), None);
        assert_eq!(grounded(said, "devbox  run agent"), None);
        // Part of a word is not a word the user said.
        assert_eq!(grounded(said, "dev"), None);
        assert_eq!(grounded(said, "box run"), None);
    }

    /// Scenario: an empty value, or one that is only quotes and a full stop,
    /// sets nothing; neither does one carrying a line break.
    #[test]
    fn voice_command_text_refuses_an_empty_or_multiline_value() {
        assert_eq!(grounded("set the command", ""), None);
        assert_eq!(grounded("set the command", "   "), None);
        assert_eq!(grounded("set the command", "“”."), None);
        assert_eq!(grounded("set the command to a\nb", "a\nb"), None);
    }
}
