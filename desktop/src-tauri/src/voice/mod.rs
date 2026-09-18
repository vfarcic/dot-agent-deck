//! PRD #802 M1 — the voice command table and its Rust-side consumers.
//!
//! The pipeline is capture → transcribe → resolve → validate → execute →
//! report. This module owns the middle: it takes a [`Transcript`] plus the live
//! state the app already has, asks an [`IntentResolver`] what the user meant,
//! refuses anything the table does not sanction, and returns one
//! [`VoiceOutcome`] carrying the sentence to show. It captures nothing and
//! renders no UI — M6 brings the surface and M7 the microphone.
//!
//! **M7 brought the microphone.** [`capture`] owns the device — Rust-side,
//! because `wry` grants webview capture on one of the three platforms this app
//! ships to — and [`transcribe`] owns the seam that turns its PCM into a
//! [`Transcript`]. `off` is the default there and is a product statement, not a
//! degraded mode: with it, the panel works from typed input through the
//! identical path below.
//!
//! **M5 brought the real backends.** [`agent_cli`] spawns the pre-authenticated
//! CLI the user already has — no key of the app's own, no download, and slow;
//! [`remote`] makes one keyed HTTPS request with a constrained enum, and is
//! roughly four times faster. [`resolver_for`] is where `VoiceSettings.intent`
//! picks one. Every [`VoiceResult`] carries the latency it cost, because PRD
//! #802's mitigation for *slow enough to feel broken* is to show the number
//! rather than hide it.
//!
//! **Voice gets no execution path of its own.** A [`VoiceOutcome::Dispatch`]
//! names an `invoke` target in the frontend action registry (M2) and the
//! frontend dispatches it where a click dispatches one. Nothing here runs an
//! action.
//!
//! The module is `pub` because most of it has no in-crate consumer yet: M6 owns
//! the surface, and until then a private module of unreferenced items is dead
//! code. M7 gave part of it one — `lib.rs`'s four `desktop_voice_*` commands
//! are what the panel will call.

pub mod agent_cli;
pub mod capture;
pub mod outcome;
pub mod prompt;
pub mod remote;
pub mod resolver;
pub mod schema;
pub mod table;
pub mod transcribe;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Live state, as the DTO the app already projects from the daemon.
///
/// Re-exported rather than mirrored: PRD #802 has no daemon-side change in it
/// at all, so the resolver reads the snapshot the agent overview already
/// renders. A parallel shape here would be a second answer to "what agents are
/// there" with nothing keeping the two in step.
pub use crate::dto::DesktopAgent;

pub use agent_cli::{AGENT_CLI_TIMEOUT, AgentCli, AgentCliResolver};
pub use capture::{
    AudioFormat, AudioSource, AudioStream, Capture, CaptureError, CaptureSession, CaptureState,
    CaptureStatus, CaptureTicket, CpalSource, MAX_UTTERANCE, Pcm16, PcmSink, StubSource,
    TARGET_SAMPLE_RATE,
};
pub use outcome::{ResolvedParam, VoiceOutcome, VoiceResult, handle_utterance};
pub use remote::{REMOTE_TIMEOUT, RemoteResolver};
pub use resolver::{
    IntentAnswer, IntentError, IntentRequest, IntentResolver, StubResolver, resolver_for,
};
pub use schema::{
    AnnotatedCommand, AnnotatedParam, TOOL_INSTRUCTIONS, TOOL_NAME, annotate, tool_schema,
};
pub use table::{CommandRow, CommandTable, NO_MATCH_ACTION, ParamKind, ParamSpec, Screen, table};
pub use transcribe::{
    OffTranscriber, RemoteTranscriber, StubTranscriber, TRANSCRIBE_TIMEOUT, Transcriber,
    TranscriptionError, TranscriptionOutcome, VoiceTranscription, handle_audio, transcriber_for,
};

/// What the user said, as text — from the microphone through the `Transcriber`
/// seam (M7), or typed into the same box.
///
/// **The rule this feature adopts: no transcript, utterance or audio buffer is
/// written to a log or to disk.** That covers `eprintln!`, which is how this
/// crate logs today (`lib.rs` and `settings.rs` between them are its only log
/// calls, and neither `tracing` nor `log` is a dependency of it), and it covers
/// whatever replaces it. A transcript lives in memory for the session's UI, and
/// the one place it is meant to appear is the sentence the app renders back to
/// the user.
///
/// It is a **rule**, not a property M1 can assert: the code that could break it
/// — the backends (M5), the surface (M6), the microphone (M7) — is not written
/// yet. PRD #802's Open Question 5 asks whether any part of an utterance is
/// persisted; this is the answer being proposed, and M6 owes it to the docs
/// either way.
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

/// Snapshot-agent fixtures, shared by every test module under `voice`.
///
/// One construction site for [`DesktopAgent`]'s fifteen fields rather than one
/// per module: `outcome`, `prompt` and `remote` all need agents to resolve a
/// spoken reference against, and three literals would be three things to edit
/// when the DTO gains a field.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::DesktopAgent;
    use crate::dto::DesktopTab;

    /// A snapshot agent. Only the fields a spoken reference can reach are
    /// interesting; the rest are what the daemon would have reported.
    pub(crate) fn agent(id: &str, display_name: Option<&str>, agent_type: &str) -> DesktopAgent {
        DesktopAgent {
            id: id.to_string(),
            pane_id: None,
            display_name: display_name.map(str::to_string),
            cwd: None,
            rows: 24,
            cols: 80,
            agent_type: agent_type.to_string(),
            cli_name: None,
            status: "running".to_string(),
            active_tool: None,
            tool_count: 0,
            last_user_prompt: None,
            write_lease: None,
            last_activity_ms: None,
            spawned_at_ms: None,
            tab: DesktopTab::Dashboard,
        }
    }

    /// An agent the deck names by its orchestration ROLE, which is how a user
    /// refers to one out loud.
    pub(crate) fn role_agent(id: &str, role: &str) -> DesktopAgent {
        let mut agent = agent(id, None, "claude_code");
        agent.tab = DesktopTab::Orchestration {
            name: "build".to_string(),
            role_index: 0,
            role_name: role.to_string(),
            is_start_role: false,
            cwd: None,
            display_title: None,
            orchestration_id: None,
        };
        agent
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
