//! Shared contract for "what bytes to send to an agent TUI to mean: this
//! prompt, then submit".
//!
//! Two writers need to agree on this encoding:
//!
//! * The local TUI's `EmbeddedPaneController::write_to_pane` (the
//!   user-typed-Enter path).
//! * The daemon's `AgentPtyRegistry::write_to_pane` (the orchestration
//!   dispatch path — PRD #93 round-5 moved delegate/work-done feedback
//!   into a direct PTY write from the daemon's async hook loop).
//!
//! Keeping the encoder + submit delay in one place ensures both writers
//! produce identical bytes for identical inputs. Drift would mean the
//! orchestration-dispatched prompts behave subtly differently from
//! user-typed prompts (e.g., multi-line tasks fragmenting into separate
//! submissions inside Claude Code).

/// Encode the payload portion of a pane input (content + bracketed paste
/// markers if multi-line) without the trailing submit byte. Trailing
/// whitespace is stripped so a one-line prompt doesn't accidentally submit
/// twice (once on the trailing `\n`, once on the explicit submit CR).
pub fn encode_pane_payload(text: &str) -> Vec<u8> {
    let trimmed = text.trim_end_matches(['\n', '\r', ' ', '\t']);
    let mut out = Vec::with_capacity(trimmed.len() + 16);
    if trimmed.contains('\n') {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(trimmed.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(trimmed.as_bytes());
    }
    out
}

/// Delay between writing input bytes and the submit CR. Agent TUIs like
/// claude treat a CR that arrives fused to the preceding text as
/// newline-in-input; only a CR that arrives as a separate event after a
/// pause is honored as Enter. The same applies after a bracketed-paste
/// close marker. 150ms tuned empirically.
pub const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_pane_payload_single_line() {
        assert_eq!(encode_pane_payload("ls -la"), b"ls -la");
    }

    #[test]
    fn encode_pane_payload_strips_trailing_whitespace() {
        assert_eq!(encode_pane_payload("ls -la\n"), b"ls -la");
        assert_eq!(encode_pane_payload("ls -la  \n\n"), b"ls -la");
        assert_eq!(encode_pane_payload("hello \n\r\t"), b"hello");
    }

    #[test]
    fn encode_pane_payload_wraps_multiline() {
        assert_eq!(
            encode_pane_payload("line1\nline2\nline3"),
            b"\x1b[200~line1\nline2\nline3\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_multiline_with_trailing_newline() {
        // Trailing newline is stripped, but embedded newlines still trigger paste wrapping.
        assert_eq!(
            encode_pane_payload("line1\nline2\n"),
            b"\x1b[200~line1\nline2\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_empty() {
        assert_eq!(encode_pane_payload(""), b"");
        // Edge case: trailing whitespace stripped to empty → no embedded newline → no markers.
        assert_eq!(encode_pane_payload("\n\n"), b"");
    }
}
