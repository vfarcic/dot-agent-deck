//! Shared contract for "what bytes to send to an agent TUI to mean: this
//! prompt, then submit".
//!
//! Two writers need to agree on this encoding:
//!
//! * The local TUI's `EmbeddedPaneController::write_to_pane` (the
//!   user-typed-Enter path).
//! * The daemon's `AgentPtyRegistry::write_and_submit_guarded` (the orchestration
//!   dispatch path — PRD #93 round-5 moved delegate/work-done feedback
//!   into a direct PTY write from the daemon's async hook loop; issue #617
//!   bound it to an expected agent identity and #917 deleted the unguarded
//!   `write_to_pane_and_submit` it replaced).
//!
//! Keeping the encoder + submit delay in one place ensures both writers
//! produce identical bytes for identical inputs. Drift would mean the
//! orchestration-dispatched prompts behave subtly differently from
//! user-typed prompts (e.g., multi-line tasks fragmenting into separate
//! submissions inside Claude Code).

use std::borrow::Cow;

use thiserror::Error;

/// Errors that can arise when encoding a pane input payload.
///
/// PRD #93 round-8: the encoder refuses inputs that would corrupt the
/// bracketed-paste wrapper. A trimmed text containing a literal
/// `ESC[201~` byte sequence would prematurely terminate the outer paste
/// (an embedded `ESC[200~` is the symmetric case — it cannot nest), so
/// everything after the inner marker would be interpreted as raw
/// keystrokes by the receiving agent TUI. Callers handle the error by
/// logging and dropping the write — same behavior as a bad pane id.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PaneInputError {
    /// The trimmed payload contains an embedded bracketed-paste marker
    /// (`ESC[200~` or `ESC[201~`) and bracketed-paste wrapping is about
    /// to be applied. The variant carries the offending escape sequence
    /// in human-readable form so an operator scanning logs can correlate
    /// the dropped write with the input that triggered it.
    #[error("multi-line pane payload contains embedded bracketed-paste marker {0}")]
    EmbeddedPasteMarker(&'static str),
}

/// Encode the payload portion of a pane input (content + bracketed paste
/// markers if multi-line) without the trailing submit byte. Trailing
/// whitespace is stripped so a one-line prompt doesn't accidentally submit
/// twice (once on the trailing `\n`, once on the explicit submit CR).
///
/// Issue #1616: every line break is written as LF first
/// ([`normalize_line_breaks`]), so any text with more than one line goes in
/// as a bracketed paste and no bare CR is ever typed in the middle of it.
///
/// Returns `Err(PaneInputError::EmbeddedPasteMarker)` when the trimmed
/// payload is multi-line *and* contains a literal `ESC[200~` or
/// `ESC[201~` byte sequence — the inner marker would otherwise terminate
/// the outer bracketed paste early and let the rest of the payload
/// execute as live keystrokes inside the agent TUI. Single-line payloads
/// are never wrapped, so the same byte sequence in single-line text is
/// harmless and accepted (see
/// `encode_pane_payload_single_line_with_marker_still_passes`).
pub fn encode_pane_payload(text: &str) -> Result<Vec<u8>, PaneInputError> {
    let text = normalize_line_breaks(text);
    let trimmed = text.trim_end_matches(['\n', '\r', ' ', '\t']);
    let mut out = Vec::with_capacity(trimmed.len() + 16);
    if trimmed.contains('\n') {
        let bytes = trimmed.as_bytes();
        if contains_subslice(bytes, b"\x1b[201~") {
            return Err(PaneInputError::EmbeddedPasteMarker("ESC[201~"));
        }
        if contains_subslice(bytes, b"\x1b[200~") {
            return Err(PaneInputError::EmbeddedPasteMarker("ESC[200~"));
        }
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(trimmed.as_bytes());
    }
    Ok(out)
}

/// Issue #1616: `text` with every line break written as LF — CRLF and a bare
/// CR, and the vertical tab, form feed, NEL, line separator and paragraph
/// separator. A CRLF is one line break.
///
/// What an agent does with any of the others is not a line break. Unbracketed,
/// a bare CR is an Enter in the middle of the prompt. And Claude Code 2.1.294
/// removes each of them as an invisible character and then holds the prompt
/// in its composer with "review and press Enter to send", so the deck's own
/// Enter never submits it — measured with a bare CR, VT, FF, NEL, LS and PS.
/// LF is the one every agent's editor shows as a new line inside a paste, and
/// the one Claude Code reports a CRLF paste back as.
///
/// [`crate::prompt_delivery`] compares a reported prompt with the one written
/// under this same normalization, since the agent only ever saw LF.
pub fn normalize_line_breaks(text: &str) -> Cow<'_, str> {
    if !text.contains(is_line_break_other_than_lf) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' && chars.peek() == Some(&'\n') {
            continue;
        }
        out.push(if is_line_break_other_than_lf(c) {
            '\n'
        } else {
            c
        });
    }
    Cow::Owned(out)
}

fn is_line_break_other_than_lf(c: char) -> bool {
    matches!(
        c,
        '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Delay between writing input bytes and the submit CR. Agent TUIs like
/// claude treat a CR that arrives fused to the preceding text as
/// newline-in-input; only a CR that arrives as a separate event after a
/// pause is honored as Enter. The same applies after a bracketed-paste
/// close marker. 150ms tuned empirically.
pub const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Render bytes for trace logging in a human-scannable, unambiguous
/// form. Rules:
///
/// * Printable ASCII (`0x20..=0x7E`) is emitted verbatim, **except**
///   for the backslash byte (`0x5C`), which is doubled to `\\`.
///   Without that escape, a literal `\x0a` typed into a user prompt
///   and a real `0x0A` byte would render identically — defeating the
///   point of an unambiguous byte-level trace.
/// * Every other byte (controls, ESC, CR, LF, all `0x80..=0xFF`,
///   including UTF-8 continuation bytes) is rendered as `\xNN` with
///   lowercase hex.
///
/// So the common framing bytes (`\x1b[200~`, `\x1b[201~`, `\r`, `\n`)
/// surface as the literal six-character strings `\x1b`, `\x0d`, `\x0a`
/// rather than as raw control codes terminals would interpret on
/// rendering, and a literal backslash in user input cannot forge a
/// fake `\xNN` escape.
///
/// PRD #128 (cherry-picked from PR #122): bracketed-paste framing and
/// `\r` vs `\n` are the two leading hypotheses for the orchestrator
/// spawn-time submit bug, so the pane-write trace must let an operator
/// distinguish them at a glance. Gated behind `RUST_LOG=trace` — the
/// helper itself does no logging; callers emit `tracing::trace!`.
pub fn escape_bytes_for_log(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                // Writing to a `String` cannot fail, hence the `let _`.
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_pane_payload_single_line() {
        assert_eq!(encode_pane_payload("ls -la").unwrap(), b"ls -la");
    }

    #[test]
    fn encode_pane_payload_strips_trailing_whitespace() {
        assert_eq!(encode_pane_payload("ls -la\n").unwrap(), b"ls -la");
        assert_eq!(encode_pane_payload("ls -la  \n\n").unwrap(), b"ls -la");
        assert_eq!(encode_pane_payload("hello \n\r\t").unwrap(), b"hello");
    }

    #[test]
    fn encode_pane_payload_wraps_multiline() {
        assert_eq!(
            encode_pane_payload("line1\nline2\nline3").unwrap(),
            b"\x1b[200~line1\nline2\nline3\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_multiline_with_trailing_newline() {
        // Trailing newline is stripped, but embedded newlines still trigger paste wrapping.
        assert_eq!(
            encode_pane_payload("line1\nline2\n").unwrap(),
            b"\x1b[200~line1\nline2\x1b[201~"
        );
    }

    /// Issue #1616: a line break that is not LF must not reach the agent as
    /// itself. Unbracketed, a bare CR is an Enter in the middle of the
    /// prompt; and Claude Code 2.1.294 treats a bare CR, VT, FF, NEL, LS or PS
    /// as an invisible character it removes, after which it holds the prompt
    /// in its composer ("review and press Enter to send") instead of
    /// submitting it. Every one becomes LF, so the payload is a bracketed
    /// paste whose lines the agent shows and submits.
    #[test]
    fn encode_pane_payload_writes_every_line_break_as_lf() {
        for separator in [
            "\r\n", "\r", "\x0b", "\x0c", "\u{85}", "\u{2028}", "\u{2029}",
        ] {
            assert_eq!(
                encode_pane_payload(&format!("line1{separator}line2")).unwrap(),
                b"\x1b[200~line1\nline2\x1b[201~",
                "{separator:?}"
            );
            assert_eq!(
                encode_pane_payload(&format!("line1{separator}{separator}line2{separator}"))
                    .unwrap(),
                b"\x1b[200~line1\n\nline2\x1b[201~",
                "{separator:?}"
            );
            assert_eq!(
                encode_pane_payload(&format!("one line{separator}")).unwrap(),
                b"one line",
                "{separator:?}"
            );
        }
        // A CRLF is ONE line break, not two.
        assert_eq!(
            encode_pane_payload("a\r\nb\r\n\r\nc").unwrap(),
            b"\x1b[200~a\nb\n\nc\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_empty() {
        assert_eq!(encode_pane_payload("").unwrap(), b"");
        // Edge case: trailing whitespace stripped to empty → no embedded newline → no markers.
        assert_eq!(encode_pane_payload("\n\n").unwrap(), b"");
    }

    /// PRD #93 round-8: an embedded END marker would terminate the outer
    /// bracketed paste early — the rest of the payload would land as raw
    /// keystrokes in the agent TUI. The encoder must refuse the write.
    #[test]
    fn encode_pane_payload_rejects_embedded_paste_end_marker() {
        // Multi-line because of the embedded \n — bracketed-paste wrap
        // would be applied without the check.
        let input = "first line\n\x1b[201~rest of payload";
        let err = encode_pane_payload(input).unwrap_err();
        assert_eq!(err, PaneInputError::EmbeddedPasteMarker("ESC[201~"));
    }

    /// Symmetric case: an embedded START marker would also corrupt the
    /// wrapper (bracketed paste cannot nest). Reject for parity.
    #[test]
    fn encode_pane_payload_rejects_embedded_paste_start_marker() {
        let input = "first line\nsecond line with \x1b[200~ in it";
        let err = encode_pane_payload(input).unwrap_err();
        assert_eq!(err, PaneInputError::EmbeddedPasteMarker("ESC[200~"));
    }

    /// Single-line payloads aren't bracketed-paste wrapped, so a literal
    /// marker in the input cannot escape an outer wrapper — there is no
    /// wrapper. Accept the write unchanged.
    #[test]
    fn encode_pane_payload_single_line_with_marker_still_passes() {
        let input = "hello \x1b[201~ world";
        let out = encode_pane_payload(input).unwrap();
        assert_eq!(out, input.as_bytes());
    }

    /// The trace-log helper must render bracketed-paste markers, CR, and
    /// LF unambiguously so an operator can tell at a glance whether the
    /// daemon emitted `\x1b[200~...\x1b[201~` framing and whether the
    /// submit terminator was `\r` (13), `\n` (10), or both.
    #[test]
    fn escape_bytes_for_log_renders_paste_framing_and_terminators() {
        let bytes = b"\x1b[200~hello\nworld\x1b[201~\r";
        assert_eq!(
            escape_bytes_for_log(bytes),
            "\\x1b[200~hello\\x0aworld\\x1b[201~\\x0d"
        );
        assert_eq!(escape_bytes_for_log(b""), "");
        assert_eq!(escape_bytes_for_log(b"ls -la"), "ls -la");
        assert_eq!(escape_bytes_for_log(b"\n"), "\\x0a");
        assert_eq!(escape_bytes_for_log(b"\r"), "\\x0d");
    }

    /// A literal backslash in user input must be doubled, otherwise an
    /// adversarial (or just unlucky) input like `\x0a` typed into a
    /// prompt would render identically to a real `0x0A` byte and the
    /// byte-level trace loses its unambiguity.
    #[test]
    fn escape_bytes_for_log_escapes_literal_backslash() {
        assert_eq!(escape_bytes_for_log(b"\\"), "\\\\");
        // Literal `\x0a` (six bytes) becomes `\\x0a` (seven chars,
        // distinguishable from a real 0x0A byte which renders as `\x0a`).
        assert_eq!(escape_bytes_for_log(b"\\x0a"), "\\\\x0a");
        // Literal `\\` (two bytes) becomes four backslashes.
        assert_eq!(escape_bytes_for_log(b"\\\\"), "\\\\\\\\");
    }

    /// Tab is a non-printable control byte and must be escaped.
    #[test]
    fn escape_bytes_for_log_escapes_tab() {
        assert_eq!(escape_bytes_for_log(b"\t"), "\\x09");
        assert_eq!(escape_bytes_for_log(b"a\tb"), "a\\x09b");
    }

    /// UTF-8 multi-byte sequences are escaped byte-by-byte. This is the
    /// correct behavior for a *byte-level* trace — the goal is to show
    /// what hit the PTY master, not to reconstruct the user-visible
    /// glyph. An 'é' (0xC3 0xA9) thus renders as `\xc3\xa9`.
    #[test]
    fn escape_bytes_for_log_escapes_utf8_multibyte_per_byte() {
        // 'é' in UTF-8 is 0xC3 0xA9.
        assert_eq!(escape_bytes_for_log("é".as_bytes()), "\\xc3\\xa9");
        // '€' in UTF-8 is 0xE2 0x82 0xAC.
        assert_eq!(escape_bytes_for_log("€".as_bytes()), "\\xe2\\x82\\xac");
        // Mixed ASCII + multibyte.
        assert_eq!(escape_bytes_for_log("aé".as_bytes()), "a\\xc3\\xa9");
    }
}
