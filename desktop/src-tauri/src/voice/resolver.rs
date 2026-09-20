//! The intent-resolution seam.
//!
//! One model call carrying the command table, the live state the app already
//! has, and the transcript; one structured answer back. **The model returns a
//! situation, not a sentence** — the app renders every word the user reads,
//! from the table, in [`super::outcome`].
//!
//! M5 put two real backends behind this trait; PRD #802's provider work
//! removed one of them. The agent-CLI backend — which spawned the
//! pre-authenticated `claude` and so needed no key of the app's own — is gone,
//! and [`super::remote`] is what [`resolver_for`] can now choose.
//! [`StubResolver`] stays, and is what every test of everything downstream —
//! validation, param resolution, the rendered sentences — still drives, so none
//! of them needs a model, a credential or a network hop.

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
///
/// # A `null` param is an ABSENT param, and that is forced rather than lenient
///
/// OpenAI's strict structured outputs refuse a schema where a declared property
/// is not in `required`, so [`super::openai::response_schema`] cannot leave a
/// param optional the way a tool-use schema can: it enumerates every param the
/// table declares, requires all of them, and types each as `["string", "null"]`
/// — the documented way to spell *optional* under strict mode. The model is
/// therefore obliged to name a param the chosen action does not take, and the
/// only thing it can honestly say about it is `null`.
///
/// Dropping those here is what keeps that a detail of one envelope. A `null`
/// arriving as an empty string would resolve against nothing and render
/// `no agent here matches ""`; arriving as absent, it is
/// [`super::VoiceOutcome::ParamMissing`], which is the same sentence the other
/// protocol produces for the same situation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IntentAnswer {
    pub action: String,
    #[serde(default, deserialize_with = "params_without_nulls")]
    pub params: BTreeMap<String, String>,
}

/// [`IntentAnswer::params`], with the `null`s a strict schema forces removed.
///
/// A non-string, non-null value is still an error — `{"agent": 7}` is a backend
/// that did not honour the schema, and reading it as absent would turn a
/// malformed reply into a sentence blaming the user's phrasing.
fn params_without_nulls<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    Ok(
        BTreeMap::<String, Option<String>>::deserialize(deserializer)?
            .into_iter()
            .filter_map(|(name, value)| value.map(|value| (name, value)))
            .collect(),
    )
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

    /// Which backend this is, for the surface to name.
    ///
    /// One of `anthropic`, `openai`, `stub`. It travels on every
    /// [`super::VoiceResult`]
    /// beside the latency, because the two are only useful together: *4.2 s* on
    /// its own is a complaint, and a name beside it is a reason to change
    /// something. It is a `&'static str` and not the settings enum so a backend
    /// a later milestone adds can name itself without the settings token and
    /// the surface's vocabulary having to move in lockstep.
    fn backend_name(&self) -> &'static str;
}

/// Build the backend the settings name.
///
/// The one place `IntentBackend` becomes an implementation, and the reason the
/// trait is object-safe: M1 built the `dyn` seam for exactly this, and the
/// choice is made at call time rather than at startup so a user who changes the
/// setting does not restart the app to use it.
///
/// Passing the store rather than a credential is what keeps the value
/// Rust-side: the resolver asks the keychain itself, at the moment it needs
/// one, and nothing hands a secret around in the hope that whoever holds it
/// will not serialize it.
///
/// The endpoint and the model travel with the settings rather than being
/// constants in [`super::remote`] — PRD #802's provider work, and the reason
/// every coordinate a user can choose is read here rather than compiled in.
///
/// **The settings token is a PROTOCOL, and this is where that becomes one.**
/// Both arms build the same resolver over the same transport; what differs is
/// the dialect it speaks. An exhaustive `match` is what makes a new
/// `IntentBackend` variant with no adapter a compile error instead of a
/// settings value the app cannot honour.
pub fn resolver_for(
    settings: &crate::settings::IntentSettings,
    secrets: std::sync::Arc<dyn crate::secrets::SecretStore>,
) -> Box<dyn IntentResolver> {
    use super::remote::Protocol;
    use crate::settings::IntentBackend;
    let protocol = match settings.backend {
        IntentBackend::Anthropic => Protocol::Anthropic,
        // The dialect's own parameter, decided here from the coordinates rather
        // than sent unconditionally — `IntentSettings::reasoning_effort` is the
        // gate and says why an `openai_compatible` endpoint that is not this
        // build's preset must not receive it.
        IntentBackend::OpenaiCompatible => Protocol::OpenAiCompatible {
            reasoning_effort: settings.reasoning_effort(),
        },
    };
    Box::new(super::remote::RemoteResolver::new(
        protocol,
        secrets,
        settings.endpoint.clone(),
        settings.model.clone(),
        settings.max_tokens,
    ))
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

    fn backend_name(&self) -> &'static str {
        "stub"
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

    /// The command stage with one backend chosen and this build's presets for
    /// the coordinates — which is what the panel writes when a user picks one.
    fn stage(backend: crate::settings::IntentBackend) -> crate::settings::IntentSettings {
        // `for_backend` rather than the default with the backend overwritten:
        // the coordinates belong to the backend, so the latter would build an
        // Anthropic stage pointing at the OpenAI preset — a pairing the panel
        // cannot produce and the deserializer refuses to construct.
        crate::settings::IntentSettings::for_backend(backend)
    }

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
    fn voice_resolver_selects_the_backend_the_settings_name() {
        use crate::secrets::MemorySecretStore;
        use crate::settings::IntentBackend;
        let store = || std::sync::Arc::new(MemorySecretStore::new()) as std::sync::Arc<_>;
        // Named by `backend_name`, which is what the surface renders — so this
        // asserts the thing a user would see rather than a type the compiler
        // already knows.
        assert_eq!(
            resolver_for(&stage(IntentBackend::Anthropic), store()).backend_name(),
            "anthropic"
        );
        assert_eq!(
            resolver_for(&stage(IntentBackend::OpenaiCompatible), store()).backend_name(),
            "openai"
        );
    }

    /// Scenario: the resolver built from this build's own preset asks for
    /// minimal reasoning; one built from any coordinate a user could edit asks
    /// for none.
    ///
    /// `reasoning_effort` is an **OpenAI-family** parameter, so a request
    /// carrying it unconditionally is a 400 waiting on somebody's gateway or
    /// local server — and on `api.openai.com` itself for a model that does not
    /// reason. This is the seam where that decision is made, so it is the seam
    /// where the scoping is pinned: the `Protocol` variant carries the value,
    /// and `IntentSettings::reasoning_effort` decides it against the endpoint
    /// and the model together.
    #[test]
    fn voice_resolver_sends_the_reasoning_parameter_only_on_this_builds_preset() {
        use crate::model_service::{ModelId, ServiceUrl};
        use crate::settings::{IntentBackend, IntentSettings, OPENAI_COMMAND_REASONING_EFFORT};

        let preset = IntentSettings::for_backend(IntentBackend::OpenaiCompatible);
        assert_eq!(
            preset.reasoning_effort(),
            Some(OPENAI_COMMAND_REASONING_EFFORT)
        );
        // And the shipping default IS that preset, so a fresh install gets it.
        assert_eq!(
            IntentSettings::default().reasoning_effort(),
            Some(OPENAI_COMMAND_REASONING_EFFORT)
        );

        // Every edit a user can make takes it off. A gateway in front of
        // OpenAI, a server on this machine, and the same endpoint with a
        // different model — the last is the one a per-endpoint rule would
        // have missed.
        for endpoint in [
            "https://gateway.example.com/v1/chat/completions",
            "http://127.0.0.1:8080/v1/chat/completions",
            // The preset host with a different path: still not the preset.
            "https://api.openai.com/v1/responses",
        ] {
            let edited = IntentSettings {
                endpoint: ServiceUrl::parse(endpoint).expect("valid"),
                ..preset.clone()
            };
            assert_eq!(edited.reasoning_effort(), None, "{endpoint}");
        }
        for model in ["gpt-4.1-mini", "llama3.1:8b"] {
            let edited = IntentSettings {
                model: ModelId::parse(model).expect("valid"),
                ..preset.clone()
            };
            assert_eq!(edited.reasoning_effort(), None, "{model}");
        }
        // Raising the ceiling is NOT an edit that takes it off: the parameter
        // is about what the endpoint accepts, and the ceiling is not.
        let raised = IntentSettings {
            max_tokens: crate::model_service::TokenCeiling::parse(8192).expect("in range"),
            ..preset.clone()
        };
        assert_eq!(
            raised.reasoning_effort(),
            Some(OPENAI_COMMAND_REASONING_EFFORT)
        );

        // The other dialect has no such parameter and no way to hold one —
        // `Protocol::Anthropic` is a unit variant — so this is belt and braces
        // for a caller that asks anyway.
        assert_eq!(
            IntentSettings::for_backend(IntentBackend::Anthropic).reasoning_effort(),
            None
        );
    }

    #[test]
    fn voice_resolver_selection_is_total_over_the_settings_enum() {
        // A variant added to `IntentBackend` without an adapter would be a
        // settings value the app cannot honour, which `VoiceSettings`' own docs
        // call a lie in a file the user can read. The `match` in `resolver_for`
        // is exhaustive, so the compiler catches it — this asserts the set is
        // the one that was mapped, which the compiler cannot.
        use crate::settings::IntentBackend;
        let names: Vec<&str> = [IntentBackend::Anthropic, IntentBackend::OpenaiCompatible]
            .into_iter()
            .map(|backend| {
                resolver_for(
                    &stage(backend),
                    std::sync::Arc::new(crate::secrets::MemorySecretStore::new()),
                )
                .backend_name()
            })
            .collect();
        assert_eq!(names, vec!["anthropic", "openai"]);
        // Every token the settings can hold has an adapter above. The token set
        // is pinned on the settings side by
        // `the_voice_tokens_match_the_frontends_copy`; this side pins that the
        // mapping covers it — and that the two do NOT collapse to one name,
        // which is what a `Protocol` wired to the wrong arm would look like.
        assert_eq!(names.len(), 2);
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
    fn voice_resolver_answer_reads_a_null_param_as_absent() {
        // Forced by `openai`'s strict schema, which has to declare and require
        // every param name. See `IntentAnswer`'s own docs.
        let answer: IntentAnswer =
            serde_json::from_str(r#"{"action":"open_deck","params":{"agent":null}}"#)
                .expect("parses");
        assert_eq!(answer.action, "open_deck");
        assert!(answer.params.is_empty(), "{:?}", answer.params);

        // A mixed object keeps what is there and drops only the nulls.
        let answer: IntentAnswer = serde_json::from_str(
            r#"{"action":"open_agent","params":{"agent":"tester","other":null}}"#,
        )
        .expect("parses");
        assert_eq!(answer.params.get("agent"), Some(&"tester".to_string()));
        assert_eq!(answer.params.len(), 1);

        // A non-string, non-null value is still a malformed answer.
        assert!(
            serde_json::from_str::<IntentAnswer>(r#"{"action":"open_agent","params":{"agent":7}}"#)
                .is_err()
        );
    }

    #[test]
    fn voice_resolver_answer_tolerates_absent_params() {
        let answer: IntentAnswer = serde_json::from_str(r#"{"action":"none"}"#).expect("parses");
        assert!(answer.is_no_match());
        assert!(answer.params.is_empty());
    }
}
