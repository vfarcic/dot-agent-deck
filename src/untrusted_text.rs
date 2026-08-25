//! Sanitizers for producer-supplied text that ends up rendered into the live
//! terminal.
//!
//! Several strings on this project's wire are written by a peer we do not
//! control — hook events arrive on a socket any agent on the deck can post to,
//! and `list_agents` records are echoed by a daemon that may be older or
//! malformed. Every one of those strings is eventually drawn into a ratatui
//! cell or written to the pre-alt-screen terminal, so a raw ESC, a NUL or a
//! Unicode bidi override in one of them can repaint, reorder or hide text the
//! user is relying on to make a decision.
//!
//! The policy here is to **strip**, not escape: these values land in
//! width-constrained regions (a dashboard card title gets whatever columns the
//! status badge leaves it), and `\x1b` expanding to four visible characters
//! would spend that budget on the attacker's behalf. Escaping is right for a
//! diagnostic line that wants to show what was there — see
//! `keybindings::sanitize_for_terminal`, which does exactly that for a warning
//! printed once to stderr — and wrong for a title that has to stay readable.
//!
//! This is now the single implementation of the control+bidi filter.
//! [`crate::build_version_handshake`] carried a byte-identical copy until issue
//! #670 and delegates here instead. [`crate::daemon_client`] still has its own
//! `strip_control_chars`, which drops control characters but NOT bidi — that is
//! a different seam (the `list_agents` wire boundary, on records the daemon has
//! already validated) with its own audit, and folding it in here is deliberately
//! not part of #670. Reach for this module rather than writing a third copy: it
//! is the divergence between those two that let the bidi half be present on one
//! untrusted path and absent on another.

use crate::agent_pty::DISPLAY_NAME_MAX_LEN;
use crate::prompt_delivery::truncate_on_char_boundary;

/// Returns `true` for Unicode bidirectional formatting / override codepoints.
///
/// [`char::is_control`] does **not** catch these — they are general category
/// `Cf`, not `Cc` — but a terminal honours them, so a `U+202E`
/// (RIGHT-TO-LEFT OVERRIDE) planted in an untrusted string visually reverses
/// the characters after it. That is enough to make a name read as something it
/// is not, or to swallow the text that follows it on the same line.
pub fn is_bidi_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}'   // LRE, RLE, PDF, LRO, RLO
            | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
            | '\u{200E}'              // LRM
            | '\u{200F}'              // RLM
            | '\u{061C}'              // ALM
    )
}

/// Drop every character from `s` that could perturb or spoof the terminal it is
/// rendered into:
///
/// - C0 control bytes (`< 0x20`) and `0x7F` (DEL),
/// - C1 controls (`0x80..=0x9F`, including U+0085 NEL) — both covered by
///   [`char::is_control`],
/// - the bidi formatting / override codepoints [`is_bidi_format_char`] names.
///
/// `keep_newlines` retains `\n` for callers whose own output is multi-line and
/// whose line structure is theirs rather than the producer's. Pass `false` for
/// any value that has to stay on one line — an embedded newline in a card title
/// or a prompt name is an extra line the producer did not get to ask for.
pub fn strip_control_and_bidi(s: &str, keep_newlines: bool) -> String {
    s.chars()
        .filter(|&c| {
            if keep_newlines && c == '\n' {
                return true;
            }
            !(c.is_control() || is_bidi_format_char(c))
        })
        .collect()
}

/// Sanitize a producer-supplied display name into something safe to store and
/// render, or `None` when nothing usable survives.
///
/// Strips control and bidi characters, trims surrounding whitespace, then
/// clamps to [`DISPLAY_NAME_MAX_LEN`] bytes on a character boundary via
/// [`truncate_on_char_boundary`] — the same truncator
/// `ui::render_session_card` already applies to the equally producer-controlled
/// `session_id`, so a clamped name is marked with the same trailing `…` the
/// user sees on a shortened id. `None` means "no usable name": the caller
/// should leave whatever name it already had in place rather than store an
/// empty title.
///
/// The byte ceiling is deliberately the daemon's own
/// [`DISPLAY_NAME_MAX_LEN`], so a name that reaches a card through the hook
/// socket cannot be longer than one that reaches it through
/// `agent_pty::is_valid_display_name` on the attach socket. The two differ in
/// what they do about a bad value — the daemon **rejects** the whole name,
/// this **repairs** it — because the daemon is validating a request a user just
/// typed and can retype, while this is scrubbing a field on an event whose
/// other contents we still want to apply.
///
/// Note the clamp counts BYTES, not display columns. That matches every other
/// length bound in this project and is not the render budget: the title's own
/// fit is decided later by `ui::truncate_styled_segments`, whose char-vs-column
/// accounting is issue #357.
pub fn sanitize_display_name(raw: &str) -> Option<String> {
    let stripped = strip_control_and_bidi(raw, false);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_on_char_boundary(trimmed, DISPLAY_NAME_MAX_LEN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_control_and_bidi_removes_escapes_del_c1_and_overrides() {
        // One string carrying every class at once: ESC, NUL, CR/LF, DEL, a C1
        // control that only `char::is_control` catches, and an RLO.
        let dirty = "ze\x1b[31mta\0-li\u{202e}ve\x7f-\u{0085}77\r\n";
        assert_eq!(strip_control_and_bidi(dirty, false), "ze[31mta-live-77");

        // `keep_newlines` is the ONLY exemption, and it is opt-in.
        assert_eq!(strip_control_and_bidi("a\nb", false), "ab");
        assert_eq!(strip_control_and_bidi("a\n\u{202e}b\x1b", true), "a\nb");

        // Text outside the ASCII range is untouched — names are UTF-8 and
        // legitimately contain accents, CJK and emoji.
        assert_eq!(
            strip_control_and_bidi("café-日本-🎉", false),
            "café-日本-🎉"
        );
    }

    #[test]
    fn strip_control_and_bidi_covers_every_bidi_codepoint() {
        // Enumerated rather than spot-checked: a range typo in
        // `is_bidi_format_char` would otherwise leave one override live, and one
        // is all a spoof needs.
        for c in ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}']
            .into_iter()
            .chain(['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'])
            .chain(['\u{200E}', '\u{200F}', '\u{061C}'])
        {
            assert!(
                is_bidi_format_char(c),
                "U+{:04X} must be recognised",
                c as u32
            );
            let input = format!("a{c}b");
            assert_eq!(
                strip_control_and_bidi(&input, false),
                "ab",
                "U+{:04X} survived the filter",
                c as u32
            );
        }
    }

    #[test]
    fn sanitize_display_name_scrubs_trims_and_drops_empties() {
        assert_eq!(
            sanitize_display_name("fix-auth-bug"),
            Some("fix-auth-bug".to_string())
        );
        assert_eq!(
            sanitize_display_name("  \x1b]0;pwn\x07 dispatch-670 \n"),
            Some("]0;pwn dispatch-670".to_string())
        );

        // Nothing usable survives → `None`, so the caller keeps the name it had
        // instead of blanking a card title. This is the case the old
        // `.filter(|n| !n.is_empty())` guard covered for the literal empty
        // string only.
        assert_eq!(sanitize_display_name(""), None);
        assert_eq!(sanitize_display_name("   "), None);
        assert_eq!(sanitize_display_name("\x1b\u{202e}\0\x7f"), None);
    }

    #[test]
    fn sanitize_display_name_clamps_on_a_char_boundary() {
        // ASCII: exactly at the ceiling passes through untouched, one over is
        // cut and marked.
        let at_cap = "a".repeat(DISPLAY_NAME_MAX_LEN);
        assert_eq!(sanitize_display_name(&at_cap), Some(at_cap.clone()));
        let over = "a".repeat(DISPLAY_NAME_MAX_LEN + 1);
        let clamped = sanitize_display_name(&over).expect("a long name is repaired, not dropped");
        assert_eq!(clamped, format!("{at_cap}…"));

        // Multi-byte: the cut must snap DOWN to a boundary rather than split a
        // character. Swept across widths and offsets because the surviving byte
        // count depends on where the ceiling lands inside the fixture, and a
        // single fixture that happens to align proves nothing.
        for filler in ['α', 'あ', '𝄞', '😀'] {
            for pad in 0..4usize {
                let raw = "x".repeat(pad) + &filler.to_string().repeat(DISPLAY_NAME_MAX_LEN);
                let out = sanitize_display_name(&raw).expect("non-empty input yields a name");
                let body = out.strip_suffix('…').expect("an over-long name is marked");
                assert!(
                    body.len() <= DISPLAY_NAME_MAX_LEN,
                    "clamp overshot the ceiling: {} bytes for filler {filler:?} pad {pad}",
                    body.len()
                );
                assert!(
                    body.ends_with(filler),
                    "the cut split a character for filler {filler:?} pad {pad}: {body:?}"
                );
            }
        }
    }

    #[test]
    fn sanitize_display_name_clamps_after_stripping_not_before() {
        // A name padded to well over the ceiling with control bytes is a REAL
        // name once scrubbed. Clamping first would cut it to 128 bytes of
        // padding and then scrub that to nothing, losing a name that fits.
        let padded = format!("{}{}", "\x1b".repeat(400), "nightly-sweep");
        assert_eq!(
            sanitize_display_name(&padded),
            Some("nightly-sweep".to_string())
        );
    }
}
