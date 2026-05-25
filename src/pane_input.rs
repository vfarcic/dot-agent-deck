//! Shared contract for "what bytes to send to an agent TUI to mean: this
//! prompt, then submit".
//!
//! Two writers need to agree on this encoding:
//!
//! * The local TUI's `EmbeddedPaneController::write_to_pane` (the
//!   user-typed-Enter path).
//! * The daemon's `AgentPtyRegistry::write_to_pane_and_submit` (the orchestration
//!   dispatch path — PRD #93 round-5 moved delegate/work-done feedback
//!   into a direct PTY write from the daemon's async hook loop).
//!
//! Keeping the encoder + submit delay in one place ensures both writers
//! produce identical bytes for identical inputs. Drift would mean the
//! orchestration-dispatched prompts behave subtly differently from
//! user-typed prompts (e.g., multi-line tasks fragmenting into separate
//! submissions inside Claude Code).

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
/// Returns `Err(PaneInputError::EmbeddedPasteMarker)` when the trimmed
/// payload is multi-line *and* contains a literal `ESC[200~` or
/// `ESC[201~` byte sequence — the inner marker would otherwise terminate
/// the outer bracketed paste early and let the rest of the payload
/// execute as live keystrokes inside the agent TUI. Single-line payloads
/// are never wrapped, so the same byte sequence in single-line text is
/// harmless and accepted (see
/// `encode_pane_payload_single_line_with_marker_still_passes`).
pub fn encode_pane_payload(text: &str) -> Result<Vec<u8>, PaneInputError> {
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

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Delay between writing input bytes and the submit CR. Agent TUIs like
/// claude treat a CR that arrives fused to the preceding text as
/// newline-in-input; only a CR that arrives as a separate event after a
/// pause is honored as Enter. The same applies after a bracketed-paste
/// close marker. 150ms tuned empirically.
pub const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Render bytes for trace logging in a human-scannable form: printable
/// ASCII as-is, everything else as `\xNN`. The common framing bytes
/// (`\x1b[200~`, `\x1b[201~`, `\r`, `\n`) thus surface as
/// `\x1b[200~`, `\r`, `\n` rather than raw control codes that
/// terminals would interpret on rendering.
///
/// PRD #100 M1.1: bracketed-paste framing and `\r` vs `\n` are the two
/// leading hypotheses for the orchestrator Enter-submit bug, so the
/// pane-write trace must let an operator distinguish them at a glance.
/// Gated behind `RUST_LOG=trace` (or per-target `dot_agent_deck::pane_input=trace`)
/// — the helper itself does no logging; callers emit `tracing::trace!`.
pub fn escape_bytes_for_log(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
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

    /// PRD #100 M1.1: the trace-log helper must render bracketed-paste
    /// markers, CR, and LF unambiguously so an operator can tell at a
    /// glance whether the daemon emitted `\x1b[200~...\x1b[201~` framing
    /// and whether the submit terminator was `\r` (13), `\n` (10), or
    /// both.
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
}
