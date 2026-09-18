//! PRD #802 M1 — the voice command table and its Rust-side consumers.
//!
//! The pipeline is capture → transcribe → resolve → validate → execute →
//! report. This module owns the middle: it takes a [`Transcript`] plus the live
//! state the app already has, asks an [`IntentResolver`] what the user meant,
//! refuses anything the table does not sanction, and returns one
//! [`VoiceOutcome`] carrying the sentence to show. It captures nothing, calls
//! no model, and renders no UI — M5 brings the real resolvers, M6 the surface,
//! M7 the microphone.
//!
//! **Voice gets no execution path of its own.** A [`VoiceOutcome::Dispatch`]
//! names an `invoke` target in the frontend action registry (M2) and the
//! frontend dispatches it where a click dispatches one. Nothing here runs an
//! action.
//!
//! The module is `pub` because the crate's lib target has no other consumer for
//! it yet: M6 owns the IPC seam and will decide its shape, and until then a
//! private module of unreferenced items is dead code.

pub mod outcome;
pub mod resolver;
pub mod schema;
pub mod table;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Live state, as the DTO the app already projects from the daemon.
///
/// Re-exported rather than mirrored: PRD #802 has no daemon-side change in it
/// at all, so the resolver reads the snapshot the agent overview already
/// renders. A parallel shape here would be a second answer to "what agents are
/// there" with nothing keeping the two in step.
pub use crate::dto::DesktopAgent;

pub use outcome::{ResolvedParam, VoiceOutcome, handle_utterance};
pub use resolver::{IntentAnswer, IntentError, IntentRequest, IntentResolver, StubResolver};
pub use schema::{AnnotatedCommand, AnnotatedParam, TOOL_NAME, annotate, tool_schema};
pub use table::{CommandRow, CommandTable, NO_MATCH_ACTION, ParamKind, ParamSpec, Screen, table};

/// What the user said, as text — from the microphone through the `Transcriber`
/// seam (M7), or typed into the same box.
///
/// **The rule: nothing in this feature hands a transcript, an utterance or an
/// audio buffer to a `tracing`/`log` call — not as a field, not inside a
/// message, not at `debug` and not at `trace`.** (PRD #802 Open Question 5,
/// answered: no part of an utterance is persisted or logged, anywhere, at any
/// level.) It lives in memory for the session's UI, and the one place it is
/// meant to appear is the no-match sentence the app renders back to the user.
///
/// [`fmt::Debug`] is written by hand and prints no content, so a DERIVED
/// `{:?}` on a type containing a transcript prints none either. That closes the
/// accidental route and only that route — a hand-written `Debug` calling
/// [`Transcript::text`] would still print it, and `text` is public because the
/// renderer needs it. The rule above is what governs a deliberate route; the
/// type only covers the careless one.
///
/// [`Serialize`] deliberately does emit the text — the webview has to show what
/// was heard, which is the whole point of the no-match sentence.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Transcript(String);

impl Transcript {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text, verbatim. Rendered into the no-match sentence unchanged: a
    /// transcription failure is corrected by seeing exactly what was heard, so
    /// this is not the seam that bounds or scrubs it. The webview scrubs its
    /// own display copy at the render seam, as it does for every other
    /// free-form string it prints.
    pub fn text(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Transcript {
    /// Prints the length and no content. See the type's own docs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Transcript(<{} chars, not printed>)",
            self.0.chars().count()
        )
    }
}

impl From<&str> for Transcript {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Transcript {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_transcript_debug_prints_no_content() {
        let transcript = Transcript::new("open the tester");
        let rendered = format!("{transcript:?}");
        assert!(
            !rendered.contains("tester"),
            "Debug spilled the transcript: {rendered}"
        );
        assert_eq!(rendered, "Transcript(<15 chars, not printed>)");
    }

    #[test]
    fn voice_transcript_debug_hides_content_through_a_container() {
        // The route that matters: a `{:?}` on something that HOLDS a
        // transcript, which is how one would reach a log line by accident.
        #[derive(Debug)]
        struct Holder {
            #[allow(dead_code)]
            transcript: Transcript,
        }
        let rendered = format!(
            "{:?}",
            Holder {
                transcript: Transcript::new("go beck"),
            }
        );
        assert!(
            !rendered.contains("beck"),
            "container Debug spilled: {rendered}"
        );
    }

    #[test]
    fn voice_transcript_text_is_verbatim() {
        let transcript = Transcript::new("  Open   the tester  ");
        assert_eq!(transcript.text(), "  Open   the tester  ");
    }

    #[test]
    fn voice_transcript_serializes_as_a_plain_string() {
        let json = serde_json::to_string(&Transcript::new("go beck")).expect("serialize");
        assert_eq!(json, "\"go beck\"");
    }

    #[test]
    fn voice_transcript_is_empty_ignores_whitespace() {
        assert!(Transcript::new("   \t\n").is_empty());
        assert!(!Transcript::new(" a ").is_empty());
    }
}
