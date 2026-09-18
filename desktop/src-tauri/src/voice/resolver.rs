//! The intent-resolution seam.
//!
//! One model call carrying the command table, the live state the app already
//! has, and the transcript; one structured answer back. **The model returns a
//! situation, not a sentence** — the app renders every word the user reads,
//! from the table, in [`super::outcome`].
//!
//! M5 puts the real backends behind this trait: an agent-CLI one that needs no
//! key and no download, and a keyed remote one for the latency. Neither exists
//! here. What does is the shape they have to fit and a deterministic stub, so
//! everything downstream — validation, param resolution, the rendered sentences
//! — is exercised without a model.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use super::Transcript;
use super::schema::AnnotatedCommand;
use crate::dto::DesktopAgent;

/// Everything a backend is given for one utterance.
///
/// The command list arrives already annotated with `callable`, so a backend is
/// not handed the table and has no second answer about which screen is current:
/// the one it is given is the one validation will use.
pub struct IntentRequest<'a> {
    pub transcript: &'a Transcript,
    pub commands: &'a [AnnotatedCommand],
    /// The live agent snapshot the app already holds, for resolving a spoken
    /// reference like "the tester". Read-only here; a backend names an agent
    /// the way the user did and the app resolves it.
    pub agents: &'a [DesktopAgent],
}

/// What a backend answers with: an action id and the params as the user
/// referred to them.
///
/// The params are **unresolved** on purpose — `"tester"`, not an agent id. The
/// app owns resolution against live state, so a backend cannot assert that an
/// agent exists.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IntentAnswer {
    pub action: String,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl IntentAnswer {
    /// The "none of these" answer.
    pub fn none() -> Self {
        Self {
            action: super::table::NO_MATCH_ACTION.to_string(),
            params: BTreeMap::new(),
        }
    }

    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            params: BTreeMap::new(),
        }
    }

    pub fn with_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(name.into(), value.into());
        self
    }

    pub fn is_no_match(&self) -> bool {
        self.action == super::table::NO_MATCH_ACTION
    }
}

/// Why a backend could not answer.
///
/// Split in two because the user-facing situations differ: nothing is
/// configured yet (a settings instruction), against a backend that is
/// configured and failed (an error). Both become
/// [`super::VoiceOutcome::ResolutionFailed`]; M6 decides whether the settings
/// case also offers a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentError {
    /// No intent backend is configured, or the configured one is not installed.
    NotConfigured(String),
    /// The backend ran and did not produce a usable answer — a timeout, a
    /// non-zero exit, unparseable output.
    Backend(String),
}

impl IntentError {
    pub fn detail(&self) -> &str {
        match self {
            IntentError::NotConfigured(detail) | IntentError::Backend(detail) => detail,
        }
    }
}

impl fmt::Display for IntentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for IntentError {}

/// The future an [`IntentResolver`] returns.
///
/// Boxed rather than an `async fn` in the trait because the backend is chosen
/// from settings at runtime, so the trait has to be object-safe; an `async fn`
/// in a trait is not. Async rather than blocking because M5's backends spawn a
/// process and make an HTTPS request, and blocking either inside a Tauri
/// command is how an app stops repainting.
pub type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<IntentAnswer, IntentError>> + Send + 'a>>;

/// A backend that turns an utterance into a structured answer.
pub trait IntentResolver: Send + Sync {
    fn resolve<'a>(&'a self, request: IntentRequest<'a>) -> ResolveFuture<'a>;
}

/// A deterministic stand-in for a model.
///
/// It maps an utterance to a canned answer and falls back to "none of these",
/// which is what a real backend does when nothing fits. Tests own it: every
/// outcome below this seam is reachable by scripting one of these, so the
/// validation, the refusals and the rendered sentences are all asserted without
/// a model, a credential or a network hop.
#[derive(Debug, Clone, Default)]
pub struct StubResolver {
    answers: BTreeMap<String, IntentAnswer>,
    failure: Option<IntentError>,
}

impl StubResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `action` for `utterance`, matched after trimming and lowercasing.
    pub fn answering(mut self, utterance: &str, answer: IntentAnswer) -> Self {
        self.answers.insert(normalize(utterance), answer);
        self
    }

    /// Fail every request, whatever was said.
    pub fn failing(error: IntentError) -> Self {
        Self {
            answers: BTreeMap::new(),
            failure: Some(error),
        }
    }
}

impl IntentResolver for StubResolver {
    fn resolve<'a>(&'a self, request: IntentRequest<'a>) -> ResolveFuture<'a> {
        let outcome = match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(self
                .answers
                .get(&normalize(request.transcript.text()))
                .cloned()
                .unwrap_or_else(IntentAnswer::none)),
        };
        Box::pin(async move { outcome })
    }
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::schema::annotate;
    use crate::voice::table::{Screen, table};

    fn request<'a>(
        transcript: &'a Transcript,
        commands: &'a [AnnotatedCommand],
    ) -> IntentRequest<'a> {
        IntentRequest {
            transcript,
            commands,
            agents: &[],
        }
    }

    #[tokio::test]
    async fn voice_resolver_stub_answers_a_scripted_utterance() {
        let resolver = StubResolver::new().answering(
            "show me the tester",
            IntentAnswer::new("open_agent").with_param("agent", "tester"),
        );
        let commands = annotate(table(), Screen::Deck);
        let transcript = Transcript::new("Show me the tester");
        let answer = resolver
            .resolve(request(&transcript, &commands))
            .await
            .expect("answers");
        assert_eq!(answer.action, "open_agent");
        assert_eq!(answer.params.get("agent"), Some(&"tester".to_string()));
    }

    #[tokio::test]
    async fn voice_resolver_stub_falls_back_to_none_of_these() {
        let resolver = StubResolver::new();
        let commands = annotate(table(), Screen::Deck);
        let transcript = Transcript::new("what time is it");
        let answer = resolver
            .resolve(request(&transcript, &commands))
            .await
            .expect("answers");
        assert!(answer.is_no_match());
        assert_eq!(answer, IntentAnswer::none());
    }

    #[tokio::test]
    async fn voice_resolver_stub_can_fail() {
        let resolver = StubResolver::failing(IntentError::NotConfigured(
            "no backend is configured".into(),
        ));
        let commands = annotate(table(), Screen::Deck);
        let transcript = Transcript::new("show me the tester");
        let error = resolver
            .resolve(request(&transcript, &commands))
            .await
            .expect_err("fails");
        assert_eq!(error.detail(), "no backend is configured");
    }

    #[tokio::test]
    async fn voice_resolver_is_object_safe() {
        // The property M5 depends on: the backend is chosen from settings, so
        // it has to be storable behind a `dyn`.
        let resolver: Box<dyn IntentResolver> = Box::new(
            StubResolver::new().answering("go to the overview", IntentAnswer::new("open_overview")),
        );
        let commands = annotate(table(), Screen::Deck);
        let transcript = Transcript::new("go to the overview");
        let answer = resolver
            .resolve(request(&transcript, &commands))
            .await
            .expect("answers");
        assert_eq!(answer.action, "open_overview");
    }

    #[test]
    fn voice_resolver_answer_round_trips_through_json() {
        // The wire shape a backend's reply is parsed into.
        let answer: IntentAnswer =
            serde_json::from_str(r#"{"action":"open_agent","params":{"agent":"tester"}}"#)
                .expect("parses");
        assert_eq!(answer.action, "open_agent");
        assert_eq!(answer.params.get("agent"), Some(&"tester".to_string()));
        let back = serde_json::to_value(&answer).expect("serializes");
        assert_eq!(back["action"], "open_agent");
    }

    #[test]
    fn voice_resolver_answer_tolerates_absent_params() {
        let answer: IntentAnswer = serde_json::from_str(r#"{"action":"none"}"#).expect("parses");
        assert!(answer.is_no_match());
        assert!(answer.params.is_empty());
    }
}
