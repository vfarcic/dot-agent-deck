//! PR #1451 round 4, decision D8 — the text a spoken "set the command to …"
//! puts in the New agent dialog's Command field, held against the transcript.
//!
//! # Why the model locates it, and why that is checked
//!
//! "Set the command to devbox run agent." has to set Command to `devbox run
//! agent`, and a sentence can carry more than the command ("make the command
//! npm run dev please"), so the model supplies where the command is. This
//! module is where that value is held to the user's own words before anything
//! reaches the field: it is accepted only when it occurs in the transcript as
//! written, made of WHOLE spoken tokens, and what the field receives is the
//! TRANSCRIPT's slice — byte for byte as heard — never the model's string. A
//! command the model "fixed" or invented (a flag added, a word corrected) is
//! not in the transcript, so it is refused and the field keeps what it had.
//!
//! "Whole tokens" is whitespace, not word characters (PR #1451 round 4, audit
//! A3): `run.sh` out of "./run.sh" or `model=haiku` out of "--model=haiku"
//! starts on a word boundary yet drops what makes it a path or a flag, so the
//! slice must start and end at whitespace or an end of the transcript, past
//! nothing but a surrounding quote and a sentence's full stop.
//!
//! **No invisible characters** (audit A2): a C0/C1 control, or any Unicode
//! format character (`Cf` — the bidi overrides and isolates, zero-width
//! spaces and joiners, the word joiner, the BOM), or a line or paragraph
//! separator refuses the command, in the model's value and in the slice. A
//! right-to-left override makes the field READ differently from what Start
//! runs, and the field is what the user reviews before starting.
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
    if wanted.is_empty() || has_invisible(wanted) {
        return None;
    }
    let slice = find_whole_tokens(transcript, wanted)?;
    // The transcript's own words, with the same presentation dropped — the
    // value matched them, so what is left is exactly what the user said.
    let said = bare(slice);
    (!said.is_empty() && !has_invisible(said)).then(|| said.to_string())
}

/// Whether `text` holds a character the field would not show as itself: a
/// control (`Cc`), a format character (`Cf`), or a line or paragraph
/// separator. The bidi half is `untrusted_text::is_bidi_format_char`, the
/// policy every display scrub in the app uses; the rest of `Cf` is enumerated
/// beside it, because a scrub can DROP a zero-width character where this has
/// to refuse the whole command.
fn has_invisible(text: &str) -> bool {
    text.chars().any(|c| {
        c.is_control()
            || dot_agent_deck::untrusted_text::is_bidi_format_char(c)
            || is_other_format_char(c)
    })
}

/// General category `Cf` outside what `is_bidi_format_char` names, plus the
/// line and paragraph separators (`Zl`, `Zp`), which break a line as `\n`
/// does.
fn is_other_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200D}'
            | '\u{2028}'..='\u{2029}'
            | '\u{2060}'..='\u{2064}'
            | '\u{206A}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
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
/// ignoring ASCII letter case — that is made of whole spoken tokens
/// ([`starts_a_token`], [`ends_a_token`]): "dev" of "devbox" and `run.sh` of
/// "./run.sh" are parts of something the user said, not what they said.
fn find_whole_tokens<'a>(transcript: &'a str, wanted: &str) -> Option<&'a str> {
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
            if equal && starts_a_token(&transcript[..at]) && ends_a_token(slice, &transcript[end..])
            {
                return Some(slice);
            }
        }
    }
    None
}

/// Whether a slice preceded by `before` starts a token: `before` is empty or
/// ends in whitespace, once one opening quote is set aside.
fn starts_a_token(before: &str) -> bool {
    let mut rest = before;
    if let Some(open) = rest.chars().next_back()
        && QUOTES.iter().any(|&(quote, _)| quote == open)
    {
        rest = &rest[..rest.len() - open.len_utf8()];
    }
    rest.chars().next_back().is_none_or(char::is_whitespace)
}

/// Whether `slice` followed by `after` ends a token: `after` is empty or
/// starts with whitespace, once a sentence's full stop and one closing quote
/// are set aside — in either order, as [`bare`] drops them. A `.` is a full
/// stop only after a letter, a digit or a closing quote, so "cd ." is not a
/// token of "cd ..".
fn ends_a_token(slice: &str, after: &str) -> bool {
    let closes = |c: char| QUOTES.iter().any(|&(_, close)| close == c);
    let mut last = slice.chars().next_back();
    let mut rest = after;
    let mut stopped = false;
    let mut quoted = false;
    loop {
        match rest.chars().next() {
            Some('.') if !stopped && last.is_some_and(|c| c.is_alphanumeric() || closes(c)) => {
                stopped = true;
            }
            Some(c) if !quoted && closes(c) => quoted = true,
            next => return next.is_none_or(char::is_whitespace),
        }
        last = rest.chars().next();
        rest = &rest[last.map_or(0, char::len_utf8)..];
    }
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

    /// Scenario: a spoken command containing a Unicode bidi format character
    /// is refused so the visible field cannot differ from what Start runs.
    #[test]
    fn voice_command_text_refuses_bidi_format_controls() {
        let said = "set the command to echo \u{202e}safe";
        assert_eq!(grounded(said, "echo \u{202e}safe"), None);
    }

    /// Scenario: the model supplies only part of a path or option token the
    /// user said. The field must not silently lose its leading punctuation.
    #[test]
    fn voice_command_text_refuses_partial_path_and_option_tokens() {
        let cases = [
            ("set the command to ./run.sh", "run.sh"),
            ("set the command to ../run.sh", "run.sh"),
            ("set the command to claude --model=haiku", "model=haiku"),
            ("set the command to claude -model=haiku", "model=haiku"),
        ];
        assert_eq!(
            cases.map(|(said, value)| grounded(said, value)),
            [None, None, None, None]
        );
    }

    /// Scenario: a whole path or option token is kept as said — "./run.sh",
    /// "../run.sh", "--model=haiku", a quoted argument — and a `.` that is
    /// part of the command is never taken for a full stop ("cd ." is not
    /// what "cd .." said).
    #[test]
    fn voice_command_text_keeps_whole_path_and_option_tokens() {
        let cases = [
            ("set the command to ./run.sh", "./run.sh", Some("./run.sh")),
            (
                "set the command to ../run.sh.",
                "../run.sh",
                Some("../run.sh"),
            ),
            (
                "set the command to claude --model=haiku",
                "claude --model=haiku",
                Some("claude --model=haiku"),
            ),
            (
                "set the command to echo \"quoted args\"",
                "echo \"quoted args\"",
                Some("echo \"quoted args\""),
            ),
            (
                "set the command to “npm run dev.”",
                "npm run dev",
                Some("npm run dev"),
            ),
            ("set the command to cd ..", "cd .", None),
            (
                "set the command to npm run dev, please",
                "npm run dev",
                None,
            ),
        ];
        assert_eq!(
            cases.map(|(said, value, _)| grounded(said, value)),
            cases.map(|(_, _, expected)| expected.map(str::to_string))
        );
    }

    /// Scenario: every invisible character class is refused, not only the
    /// bidi overrides: a C1 control, zero-width space and joiner, word
    /// joiner, byte-order mark, soft hyphen, and a line separator.
    #[test]
    fn voice_command_text_refuses_every_invisible_character() {
        for invisible in [
            '\u{0085}',
            '\u{200B}',
            '\u{200D}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{2028}',
            '\u{2067}',
            '\u{E0041}',
        ] {
            let value = format!("echo {invisible}safe");
            assert_eq!(
                grounded(&format!("set the command to {value}"), &value),
                None,
                "U+{:04X}",
                invisible as u32
            );
        }
    }
}
