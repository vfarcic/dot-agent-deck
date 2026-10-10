//! PRD #1497 M4 — where reading mode's voice comes from.
//!
//! Two sources (D9): the configured Commands connection's own text-to-speech,
//! when it offers one, and the operating system's voice. This module decides
//! which one a sentence is spoken with ([`plan`]) and fetches the provider's
//! audio ([`synthesise`]). It plays nothing: both sources play in the webview
//! (`desktop/src/lib/speech.ts`), which is also where the speech queue, the
//! interrupt and the "is speaking" state live, because that is where the audio
//! is.
//!
//! # Which connections offer speech
//!
//! An OpenAI-compatible connection whose endpoint is a `…/chat/completions`
//! URL is taken to offer `…/audio/speech` beside it, which is OpenAI's own
//! layout. Anthropic's API offers no text-to-speech endpoint today. A server that speaks
//! chat completions but has no speech route (`llama.cpp`'s, for one) answers
//! the speech request with an error, and under **Auto** that falls back to the
//! system voice, so the guess costs one failed request rather than silence.
//!
//! # Nothing goes to the provider's speech without consent
//!
//! The provider's speech is reading mode's, so it is gated on reading's
//! Settings opt-in (PRD #1497 D4) as well as on the speech source: with the
//! opt-in off, [`plan_for`] answers the system voice whatever the source, and
//! [`speech_permitted`] — which the speech command checks before every
//! request — refuses. Until reading's one-time notice is recorded as shown it
//! also refuses every sentence but that notice (re-audit R1). A webview asking
//! for audio directly gets the same refusal. The check is repeated after the keychain read against the
//! connection the request was prepared for ([`permitted_on`]), and a save
//! while the request is in flight cancels it when it no longer permits it
//! ([`SpeechRevocation`]).
//!
//! # The audio is fetched here and not in the webview
//!
//! For the reason every network hop is Rust-side: the webview's CSP permits no
//! network origin, and the key is in this process. The audio crosses the IPC
//! boundary as bytes and is decoded by the webview's Web Audio, which a CSP
//! does not restrict.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::model_service::ServiceUrl;
use crate::secrets::SecretStore;
use crate::settings::{IntentBackend, IntentSettings, ReadingConsent, SpeechSource, VoiceSettings};

use super::remote::{authorise, endpoint_credential};
use super::summary::protocol_for;

/// The text-to-speech model asked for on an OpenAI-compatible connection.
///
/// The connection's own model is a chat model and cannot speak; this is
/// OpenAI's current low-cost speech model.
pub const OPENAI_SPEECH_MODEL: &str = "gpt-4o-mini-tts";

/// The voice asked for.
pub const OPENAI_SPEECH_VOICE: &str = "alloy";

/// The audio format asked for. MP3 is the default of the API and the smallest
/// of the formats every webview engine decodes.
pub const OPENAI_SPEECH_FORMAT: &str = "mp3";

/// The longest text sent to be spoken, in characters. A summary is at most
/// [`super::summary::MAX_SUMMARY_CHARS`]; this leaves room for a permission
/// prompt or an error announcement and refuses an essay.
pub const MAX_SPEECH_INPUT_CHARS: usize = 1_000;

/// The most audio bytes accepted for one sentence: about two minutes of the
/// API's MP3, far past anything [`MAX_SPEECH_INPUT_CHARS`] produces.
pub const MAX_AUDIO_BYTES: usize = 2 * 1024 * 1024;

/// How long a speech request gets.
pub const SPEECH_TIMEOUT: Duration = Duration::from_secs(15);

/// How one sentence is to be spoken, as the webview reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SpeechPlan {
    /// Fetch the provider's audio ([`synthesise`]) and play it.
    #[serde(rename_all = "camelCase")]
    Provider {
        /// Whether a failed provider request falls back to the system voice:
        /// true under **Auto**, false when the user chose **Provider**.
        fallback_to_system: bool,
    },
    /// Speak with the operating system's voice.
    System,
    /// Nothing can speak, and why — a sentence for the user.
    Unavailable { reason: String },
}

/// The sentence for **Provider** chosen on a connection without speech.
pub const NO_PROVIDER_SPEECH: &str = "the Commands connection has no text-to-speech; \
    choose Auto or System for the reading voice under Settings → Voice";

/// Whether the Commands connection offers text-to-speech.
pub fn offers_speech(intent: &IntentSettings) -> bool {
    speech_endpoint(intent).is_some()
}

/// The connection's speech endpoint, or `None` when it offers none.
pub fn speech_endpoint(intent: &IntentSettings) -> Option<ServiceUrl> {
    match intent.backend {
        IntentBackend::Anthropic => None,
        IntentBackend::OpenaiCompatible => {
            let chat = intent.endpoint.as_str();
            let base = chat.strip_suffix("/chat/completions")?;
            ServiceUrl::parse(&format!("{base}/audio/speech")).ok()
        }
    }
}

/// Which source speaks, for the setting and the connection (D9).
pub fn plan(source: SpeechSource, intent: &IntentSettings) -> SpeechPlan {
    let offered = offers_speech(intent);
    match (source, offered) {
        (SpeechSource::System, _) | (SpeechSource::Auto, false) => SpeechPlan::System,
        (SpeechSource::Auto, true) => SpeechPlan::Provider {
            fallback_to_system: true,
        },
        (SpeechSource::Provider, true) => SpeechPlan::Provider {
            fallback_to_system: false,
        },
        (SpeechSource::Provider, false) => SpeechPlan::Unavailable {
            reason: NO_PROVIDER_SPEECH.to_string(),
        },
    }
}

/// Which source speaks under the whole voice settings: [`plan`], except that
/// with reading's opt-in off nothing is sent to the provider's speech, so the
/// system voice speaks whatever the source says.
pub fn plan_for(settings: &VoiceSettings) -> SpeechPlan {
    if settings.reading != ReadingConsent::On {
        return SpeechPlan::System;
    }
    plan(settings.speech, &settings.intent)
}

/// Why the provider's speech must not be asked for under the settings.
pub const PROVIDER_SPEECH_NOT_PERMITTED: &str = "the provider's speech is used only while Reading is on and the speech source is \
     the provider's";

/// Whether the provider's speech is the source under the settings at all:
/// reading's switch is on and [`plan_for`] names the provider. The source a
/// sentence is planned with; a request for one sentence's audio is checked
/// with [`speech_permitted`], which also asks about the notice.
pub fn provider_permitted(settings: &VoiceSettings) -> Result<(), String> {
    match plan_for(settings) {
        SpeechPlan::Provider { .. } => Ok(()),
        _ => Err(PROVIDER_SPEECH_NOT_PERMITTED.to_string()),
    }
}

/// Why a sentence other than the notice is not sent to the provider while
/// reading's one-time notice has not been shown.
pub const READING_NOTICE_NOT_SHOWN: &str =
    "the provider's speech says only where replies go until that notice has been shown";

/// Whether `text` may be sent to the provider's speech under `settings`:
/// [`provider_permitted`], and reading's one-time notice recorded as shown
/// ([`VoiceSettings::reading_permitted`], audit A5) — except for the notice
/// itself, which is what is said before it is recorded. The exception is the
/// notice's exact words for the configured Commands connection
/// ([`super::reading::reading_notice`]), so the webview cannot use it to send
/// anything else (re-audit R1). Checked before every request, whoever asks.
pub fn speech_permitted(settings: &VoiceSettings, text: &str) -> Result<(), String> {
    provider_permitted(settings)?;
    if settings.reading_permitted()
        || text == super::reading::reading_notice(&settings.intent.endpoint)
    {
        return Ok(());
    }
    Err(READING_NOTICE_NOT_SHOWN.to_string())
}

/// Why a sentence prepared for one Commands connection is not sent once the
/// settings name another.
pub const SPEECH_CONNECTION_CHANGED: &str =
    "the Commands connection changed while the sentence was being prepared";

/// [`speech_permitted`] for `text` under `settings`, and `settings` still name
/// the connection the request was prepared for (`intent`): the key is read
/// from one keychain slot for whichever connection is configured, so a
/// sentence must not go to the old endpoint, possibly with the new
/// connection's key, after the connection was changed (PR #1617's review).
pub fn permitted_on(
    settings: &VoiceSettings,
    intent: &IntentSettings,
    text: &str,
) -> Result<(), String> {
    speech_permitted(settings, text)?;
    if !settings.intent.same_connection(intent) {
        return Err(SPEECH_CONNECTION_CHANGED.to_string());
    }
    Ok(())
}

/// Why the provider's audio was not fetched, as the webview reads it.
///
/// A refusal — the settings no longer permit the provider's speech, or name
/// another connection — is told apart from a failure because the webview must
/// never answer a refusal with the system voice, even under **Auto**: that
/// fallback is for an outage (PR #1617's review).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SpeechError {
    Refused { message: String },
    Failed { message: String },
}

impl SpeechError {
    pub fn refused(message: impl Into<String>) -> Self {
        Self::Refused {
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }
}

/// The saves each request in flight is held to, so a save that turns
/// reading's opt-in off, leaves its notice not shown for a sentence other than
/// the notice (re-audit R1), or changes the Commands connection, cancels a
/// request that already passed its last check (PR #1617's review).
///
/// Each request registers when its command starts ([`Self::register`]),
/// before it reads the settings at all, and every save walks the registry
/// ([`Self::publish`]), cancelling each request the saved settings do not
/// permit — at the publish itself, so a later save that permits it again
/// cannot take the cancellation back (PR #1617's third review: a watch channel
/// kept only the latest value, so Off then On before the request next ran left
/// it alive). The save path does not have to know which connection a request
/// was prepared for.
///
/// Until the request knows its connection ([`Self::prepared_for`]), a save
/// that does not permit the provider's speech at all cancels it at once, and
/// every other save is kept and checked against the connection when it is
/// known (PR #1617's fourth review: a request that registered only after its
/// first settings read missed an Off-then-On, or B-then-A, pair of saves
/// landing between that read and the registration).
#[derive(Default)]
pub struct SpeechRevocation {
    registry: std::sync::Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    next: u64,
    in_flight: Vec<InFlightSpeech>,
}

struct InFlightSpeech {
    id: u64,
    /// The sentence being fetched, which [`speech_permitted`] is asked about.
    text: Arc<str>,
    prepared: Prepared,
    cancel: tokio::sync::oneshot::Sender<String>,
}

/// What a registered request was prepared for.
enum Prepared {
    /// Not known yet: the settings of every save since it registered that
    /// permit the provider's speech, to check its connection against.
    Unknown(Vec<VoiceSettings>),
    For(IntentSettings),
}

/// A request registered with [`SpeechRevocation::register`], not yet told
/// which connection it is for. Dropping it unregisters the request at the
/// next publish.
pub struct SpeechTicket {
    id: u64,
    cancelled: tokio::sync::oneshot::Receiver<String>,
}

impl SpeechRevocation {
    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A save wrote `settings`: every request in flight they do not permit is
    /// cancelled now, with why. Requests that finished are forgotten.
    pub fn publish(&self, settings: &VoiceSettings) {
        let mut registry = self.registry();
        for mut request in std::mem::take(&mut registry.in_flight) {
            if request.cancel.is_closed() {
                continue;
            }
            let verdict = match &mut request.prepared {
                Prepared::For(intent) => permitted_on(settings, intent, &request.text),
                Prepared::Unknown(seen) => {
                    speech_permitted(settings, &request.text).inspect(|()| {
                        seen.push(settings.clone());
                    })
                }
            };
            match verdict {
                Ok(()) => registry.in_flight.push(request),
                Err(refusal) => {
                    let _ = request.cancel.send(refusal);
                }
            }
        }
    }

    /// Register a request for `text` now — call it before the request's
    /// first settings read — so every save from this moment on is held
    /// against it.
    pub fn register(&self, text: &str) -> SpeechTicket {
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let mut registry = self.registry();
        registry
            .in_flight
            .retain(|request| !request.cancel.is_closed());
        let id = registry.next;
        registry.next += 1;
        registry.in_flight.push(InFlightSpeech {
            id,
            text: Arc::from(text),
            prepared: Prepared::Unknown(Vec::new()),
            cancel,
        });
        SpeechTicket { id, cancelled }
    }

    /// The request `ticket` registered was prepared for `intent`. Resolves,
    /// with why, once a save since it registered no longer permits it
    /// ([`permitted_on`]) — at once, if one already has — and stays pending
    /// otherwise. Dropping the future unregisters it at the next publish.
    pub fn prepared_for(
        &self,
        ticket: SpeechTicket,
        intent: IntentSettings,
    ) -> impl Future<Output = String> + Send + 'static {
        let SpeechTicket { id, cancelled } = ticket;
        let mut registry = self.registry();
        if let Some(at) = registry
            .in_flight
            .iter()
            .position(|request| request.id == id)
        {
            let request = &registry.in_flight[at];
            let refusal = match &request.prepared {
                Prepared::Unknown(seen) => seen
                    .iter()
                    .find_map(|settings| permitted_on(settings, &intent, &request.text).err()),
                Prepared::For(_) => None,
            };
            match refusal {
                Some(refusal) => {
                    let request = registry.in_flight.swap_remove(at);
                    let _ = request.cancel.send(refusal);
                }
                None => registry.in_flight[at].prepared = Prepared::For(intent),
            }
        }
        drop(registry);
        async move {
            match cancelled.await {
                Ok(refusal) => refusal,
                Err(_) => std::future::pending().await,
            }
        }
    }

    /// [`Self::register`] and [`Self::prepared_for`] at once, for a caller
    /// that already knows its connection: every save after this call is held
    /// against it.
    pub fn revoked(
        &self,
        intent: IntentSettings,
        text: &str,
    ) -> impl Future<Output = String> + Send + 'static {
        let ticket = self.register(text);
        self.prepared_for(ticket, intent)
    }

    /// How many requests are registered and not yet finished.
    #[cfg(test)]
    fn pending(&self) -> usize {
        self.registry()
            .in_flight
            .iter()
            .filter(|request| !request.cancel.is_closed())
            .count()
    }
}

/// `work`, unless `revoked` resolves first: then `work` is dropped — with it
/// any request it has in flight — and the answer is a refusal with
/// `revoked`'s reason.
pub async fn unless_revoked<T>(
    work: impl Future<Output = Result<T, SpeechError>>,
    revoked: impl Future<Output = String>,
) -> Result<T, SpeechError> {
    tokio::select! {
        biased;
        reason = revoked => Err(SpeechError::refused(reason)),
        result = work => result,
    }
}

/// `text` bounded to [`MAX_SPEECH_INPUT_CHARS`] at a word boundary.
pub fn bounded_input(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_SPEECH_INPUT_CHARS {
        return text.to_string();
    }
    let room: String = text.chars().take(MAX_SPEECH_INPUT_CHARS).collect();
    match room.rfind(char::is_whitespace) {
        Some(at) if at > 0 => room[..at].trim_end().to_string(),
        _ => room,
    }
}

/// The speech request body.
pub fn request_body(text: &str) -> Value {
    json!({
        "model": OPENAI_SPEECH_MODEL,
        "voice": OPENAI_SPEECH_VOICE,
        "input": bounded_input(text),
        "response_format": OPENAI_SPEECH_FORMAT,
    })
}

/// Fetch the provider's audio for `text`: MP3 bytes, or a sentence saying why
/// not.
///
/// `still_permitted` is asked after the keychain read and immediately before
/// the request ([`permitted_on`] against the settings as they are then): the
/// keychain can take as long as a prompt the user answers, and reading's
/// opt-in turned off, or the connection changed, during it sends nothing (PR
/// #1617's review). A save after that check is [`SpeechRevocation`]'s.
pub async fn synthesise<P, F>(
    intent: &IntentSettings,
    secrets: Arc<dyn SecretStore>,
    text: &str,
    still_permitted: P,
) -> Result<Vec<u8>, SpeechError>
where
    P: FnOnce() -> F,
    F: std::future::Future<Output = Result<(), String>>,
{
    let Some(endpoint) = speech_endpoint(intent) else {
        return Err(SpeechError::failed(NO_PROVIDER_SPEECH));
    };
    if text.trim().is_empty() {
        return Err(SpeechError::failed("there is nothing to say"));
    }
    let secret = endpoint_credential(&endpoint, &secrets)
        .await
        .map_err(SpeechError::failed)?;
    still_permitted().await.map_err(SpeechError::refused)?;
    let Some(client) = super::http::client() else {
        return Err(SpeechError::failed(
            "speech could not start a secure connection",
        ));
    };
    let post = client
        .post(endpoint.as_str())
        .timeout(SPEECH_TIMEOUT)
        .header("content-type", "application/json");
    let response = authorise(post, protocol_for(intent), secret.as_ref())
        .json(&request_body(text))
        .send()
        .await
        .map_err(|error| {
            SpeechError::failed(if error.is_timeout() {
                format!(
                    "the speech service did not answer within {}s",
                    SPEECH_TIMEOUT.as_secs()
                )
            } else {
                "the speech request failed".to_string()
            })
        })?;
    let status = response.status();
    let audio_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_mp3_type);
    let body = super::http::capped_body(response, MAX_AUDIO_BYTES)
        .await
        .map_err(|error| {
            SpeechError::failed(match error {
                super::http::BodyError::TooLarge => {
                    format!("the speech service answered with more than {MAX_AUDIO_BYTES} bytes")
                }
                super::http::BodyError::Transport => "the speech request failed".to_string(),
            })
        })?;
    check_audio(status, audio_type, body).map_err(SpeechError::failed)
}

/// Whether a `Content-Type` value names MP3 audio — the format asked for
/// ([`OPENAI_SPEECH_FORMAT`]). Parameters are tolerated (`audio/mpeg;
/// charset=…`); any other audio type is not, since the webview hands the bytes
/// to a decoder and only MP3 was requested.
pub fn is_mp3_type(value: &str) -> bool {
    let essence = value.split(';').next().unwrap_or_default().trim();
    essence.eq_ignore_ascii_case("audio/mpeg") || essence.eq_ignore_ascii_case("audio/mp3")
}

/// Accept a reply as audio only when it succeeded, says it is MP3, and has
/// some.
fn check_audio(
    status: reqwest::StatusCode,
    audio_type: bool,
    body: Vec<u8>,
) -> Result<Vec<u8>, String> {
    if !status.is_success() {
        return Err(format!("the speech service refused ({status})"));
    }
    if !audio_type || body.is_empty() {
        return Err("the speech service answered without audio".to_string());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecretStore;
    use crate::settings::ReadingNotice;

    fn openai() -> IntentSettings {
        IntentSettings::for_backend(IntentBackend::OpenaiCompatible)
    }

    fn anthropic() -> IntentSettings {
        IntentSettings::for_backend(IntentBackend::Anthropic)
    }

    fn openai_at(endpoint: &str) -> IntentSettings {
        IntentSettings {
            endpoint: ServiceUrl::parse(endpoint).expect("valid"),
            ..openai()
        }
    }

    #[test]
    fn voice_speech_source_for_each_setting_and_provider_kind() {
        let auto_provider = SpeechPlan::Provider {
            fallback_to_system: true,
        };
        let chosen_provider = SpeechPlan::Provider {
            fallback_to_system: false,
        };
        let unavailable = SpeechPlan::Unavailable {
            reason: NO_PROVIDER_SPEECH.to_string(),
        };
        for (source, intent, expected) in [
            (SpeechSource::Auto, openai(), auto_provider.clone()),
            (SpeechSource::Auto, anthropic(), SpeechPlan::System),
            (SpeechSource::Provider, openai(), chosen_provider),
            (SpeechSource::Provider, anthropic(), unavailable.clone()),
            (SpeechSource::System, openai(), SpeechPlan::System),
            (SpeechSource::System, anthropic(), SpeechPlan::System),
            // An OpenAI-compatible endpoint that is not a chat-completions URL
            // offers no speech route this build can find.
            (
                SpeechSource::Auto,
                openai_at("https://gateway.example/v1/route"),
                SpeechPlan::System,
            ),
            (
                SpeechSource::Provider,
                openai_at("https://gateway.example/v1/route"),
                unavailable,
            ),
        ] {
            assert_eq!(plan(source, &intent), expected, "{source:?} on {intent:?}");
        }
    }

    #[test]
    fn voice_speech_endpoint_sits_beside_chat_completions() {
        assert_eq!(
            speech_endpoint(&openai()).map(|url| url.as_str().to_string()),
            Some("https://api.openai.com/v1/audio/speech".to_string())
        );
        assert_eq!(
            speech_endpoint(&openai_at("http://127.0.0.1:8080/v1/chat/completions"))
                .map(|url| url.as_str().to_string()),
            Some("http://127.0.0.1:8080/v1/audio/speech".to_string())
        );
        assert_eq!(speech_endpoint(&anthropic()), None);
    }

    #[test]
    fn voice_speech_plan_serialises_as_the_webview_reads_it() {
        assert_eq!(
            serde_json::to_value(SpeechPlan::Provider {
                fallback_to_system: true
            })
            .unwrap(),
            json!({ "kind": "provider", "fallbackToSystem": true })
        );
        assert_eq!(
            serde_json::to_value(SpeechPlan::System).unwrap(),
            json!({ "kind": "system" })
        );
        assert_eq!(
            serde_json::to_value(SpeechPlan::Unavailable { reason: "x".into() }).unwrap(),
            json!({ "kind": "unavailable", "reason": "x" })
        );
    }

    #[test]
    fn voice_speech_request_is_bounded() {
        let body = request_body("Tester: done.");
        assert_eq!(body["model"], OPENAI_SPEECH_MODEL);
        assert_eq!(body["voice"], OPENAI_SPEECH_VOICE);
        assert_eq!(body["response_format"], "mp3");
        assert_eq!(body["input"], "Tester: done.");

        let long = "word ".repeat(1_000);
        let bounded = bounded_input(&long);
        assert!(bounded.chars().count() <= MAX_SPEECH_INPUT_CHARS);
        assert!(bounded.ends_with("word"));
    }

    #[test]
    fn voice_speech_reply_must_be_successful_audio() {
        use reqwest::StatusCode;
        assert_eq!(
            check_audio(StatusCode::OK, true, vec![1, 2]),
            Ok(vec![1, 2])
        );
        assert!(check_audio(StatusCode::NOT_FOUND, true, vec![1]).is_err());
        assert!(check_audio(StatusCode::OK, false, vec![1]).is_err());
        assert!(check_audio(StatusCode::OK, true, Vec::new()).is_err());
    }

    /// Scenario (audit A-S2): only an MP3 media type is accepted for the
    /// provider's audio, with or without parameters; another audio type, or a
    /// type that merely starts like one, is refused.
    #[test]
    fn voice_speech_accepts_only_the_mp3_media_type() {
        for accepted in [
            "audio/mpeg",
            "Audio/MPEG",
            "audio/mpeg; charset=binary",
            "audio/mp3",
        ] {
            assert!(is_mp3_type(accepted), "{accepted}");
        }
        for refused in [
            "audio/wav",
            "audio/ogg",
            "audio/mpegurl",
            "audio/",
            "text/html",
            "application/octet-stream",
            "",
        ] {
            assert!(!is_mp3_type(refused), "{refused}");
        }
    }

    fn voice(
        source: SpeechSource,
        intent: IntentSettings,
        reading: ReadingConsent,
    ) -> VoiceSettings {
        VoiceSettings {
            intent,
            speech: source,
            reading,
            reading_notice: ReadingNotice::Shown,
            ..VoiceSettings::default()
        }
    }

    /// Scenario (audit A-S1): the provider's speech is permitted only while
    /// reading's opt-in is on and the source resolves to the provider — Auto
    /// on a connection with speech, or Provider. With the opt-in off the plan
    /// is the system voice whatever the source, and a direct request for
    /// audio is refused.
    #[test]
    fn voice_speech_provider_needs_consent_and_a_provider_source() {
        use ReadingConsent::{Off, On};
        for (source, intent, reading, permitted) in [
            (SpeechSource::Auto, openai(), On, true),
            (SpeechSource::Provider, openai(), On, true),
            (SpeechSource::System, openai(), On, false),
            (SpeechSource::Auto, anthropic(), On, false),
            (SpeechSource::Provider, anthropic(), On, false),
            (SpeechSource::Auto, openai(), Off, false),
            (SpeechSource::Provider, openai(), Off, false),
            (SpeechSource::System, openai(), Off, false),
        ] {
            let settings = voice(source, intent.clone(), reading);
            assert_eq!(
                provider_permitted(&settings).is_ok(),
                permitted,
                "{source:?} on {intent:?} with reading {reading:?}"
            );
            if reading == Off {
                assert_eq!(plan_for(&settings), SpeechPlan::System);
            } else {
                assert_eq!(plan_for(&settings), plan(source, &intent));
            }
        }
    }

    /// `voice`, with reading's one-time notice not yet recorded as shown.
    fn notice_pending(source: SpeechSource, intent: IntentSettings) -> VoiceSettings {
        VoiceSettings {
            reading_notice: ReadingNotice::Pending,
            ..voice(source, intent, ReadingConsent::On)
        }
    }

    /// Scenario (re-audit R1): Reading is on with the provider's speech and
    /// its one-time notice not yet shown. A request to speak an agent's reply
    /// — or any text but the notice — is refused before anything is sent;
    /// the notice's exact words for the configured connection are permitted,
    /// and the notice for another connection is not. Once the notice is
    /// shown, any sentence is permitted again, and with the switch off not
    /// even the notice is.
    #[tokio::test]
    async fn voice_speech_before_the_notice_is_shown_only_the_notice_is_sent() {
        let intent = openai_at("https://voice-speech.invalid/v1/chat/completions");
        let notice = super::super::reading::reading_notice(&intent.endpoint);
        let elsewhere = super::super::reading::reading_notice(
            &ServiceUrl::parse("https://other.invalid/v1/chat/completions").expect("valid"),
        );
        for source in [SpeechSource::Auto, SpeechSource::Provider] {
            let pending = notice_pending(source, intent.clone());
            for text in ["All 42 tests pass.", "", elsewhere.as_str()] {
                assert_eq!(
                    speech_permitted(&pending, text),
                    Err(READING_NOTICE_NOT_SHOWN.to_string()),
                    "{source:?}: {text:?}"
                );
            }
            assert_eq!(speech_permitted(&pending, &notice), Ok(()), "{source:?}");
            assert_eq!(permitted_on(&pending, &intent, &notice), Ok(()));
            assert_eq!(
                speech_permitted(
                    &voice(source, intent.clone(), ReadingConsent::On),
                    "anything"
                ),
                Ok(())
            );
            assert_eq!(
                speech_permitted(&voice(source, intent.clone(), ReadingConsent::Off), &notice),
                Err(PROVIDER_SPEECH_NOT_PERMITTED.to_string())
            );
        }

        // Through the request itself: refused after the keychain read, with
        // nothing sent (the endpoint is unroutable on purpose).
        let secrets: Arc<dyn SecretStore> = Arc::new(RevokedWhileReading(Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        )));
        let pending = notice_pending(SpeechSource::Provider, intent.clone());
        let reply = "The secret is 42.";
        assert_eq!(
            synthesise(&intent, secrets, reply, || async {
                permitted_on(&pending, &intent, reply)
            })
            .await,
            Err(SpeechError::refused(READING_NOTICE_NOT_SHOWN))
        );
    }

    /// Scenario (re-audit R1): two provider speech requests are in flight —
    /// the notice, and another sentence — when a save leaves the notice not
    /// shown (Reading turned off and on again resets it). The other sentence
    /// is cancelled with the notice-not-shown refusal; the notice is left to
    /// finish, through that save and through the one recording it as shown.
    #[tokio::test]
    async fn voice_speech_a_save_leaving_the_notice_pending_cancels_all_but_the_notice() {
        let intent = openai();
        let notice = super::super::reading::reading_notice(&intent.endpoint);
        let revocation = SpeechRevocation::default();
        let reply_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notice_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut reply = Box::pin(unless_revoked(
            in_flight(&reply_dropped),
            revocation.revoked(intent.clone(), "All tests pass."),
        ));
        let mut said = Box::pin(unless_revoked(
            in_flight(&notice_dropped),
            revocation.revoked(intent.clone(), &notice),
        ));
        assert_eq!(settle(&mut reply).await, None);
        assert_eq!(settle(&mut said).await, None);
        revocation.publish(&notice_pending(SpeechSource::Auto, intent.clone()));
        assert_eq!(
            settle(&mut reply).await,
            Some(Err(SpeechError::refused(READING_NOTICE_NOT_SHOWN)))
        );
        drop(reply);
        assert!(reply_dropped.load(std::sync::atomic::Ordering::SeqCst));
        revocation.publish(&voice(
            SpeechSource::Auto,
            intent.clone(),
            ReadingConsent::On,
        ));
        assert_eq!(settle(&mut said).await, None, "the notice was cancelled");
        assert!(!notice_dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(revocation.pending(), 1);

        // Registered before its connection is known: the pending-notice save
        // is held against the sentence, and refuses it once it is known.
        let revocation = SpeechRevocation::default();
        let ticket = revocation.register("Finished.");
        revocation.publish(&notice_pending(SpeechSource::Auto, intent.clone()));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut request = Box::pin(unless_revoked(
            in_flight(&dropped),
            revocation.prepared_for(ticket, intent.clone()),
        ));
        assert_eq!(
            settle(&mut request).await,
            Some(Err(SpeechError::refused(READING_NOTICE_NOT_SHOWN)))
        );
    }

    async fn permitted() -> Result<(), String> {
        Ok(())
    }

    #[tokio::test]
    async fn voice_speech_refuses_without_a_route_or_a_key_before_any_request() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
        assert_eq!(
            synthesise(&anthropic(), Arc::clone(&secrets), "hello", permitted).await,
            Err(SpeechError::failed(NO_PROVIDER_SPEECH))
        );
        // Off this machine with no key stored: refused by the keychain read,
        // so no request is made (the endpoint is unroutable on purpose).
        let error = synthesise(
            &openai_at("https://voice-speech.invalid/v1/chat/completions"),
            secrets,
            "hello",
            permitted,
        )
        .await
        .expect_err("no key");
        assert!(
            matches!(&error, SpeechError::Failed { message } if message.contains("no key is stored")),
            "{error:?}"
        );
    }

    /// A keychain whose read is where the user turns reading off: answering
    /// it revokes the opt-in, as a save during a keychain prompt would.
    struct RevokedWhileReading(Arc<std::sync::atomic::AtomicBool>);

    impl SecretStore for RevokedWhileReading {
        fn store(
            &self,
            _: crate::secrets::SecretId,
            _: &crate::secrets::Secret,
        ) -> Result<(), crate::secrets::SecretError> {
            Ok(())
        }

        fn load(
            &self,
            _: crate::secrets::SecretId,
        ) -> Result<Option<crate::secrets::Secret>, crate::secrets::SecretError> {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(crate::secrets::Secret::new("sk-test")))
        }

        fn delete(&self, _: crate::secrets::SecretId) -> Result<(), crate::secrets::SecretError> {
            Ok(())
        }
    }

    /// Scenario (PR #1617 review): reading is on when a sentence is asked
    /// for, and is turned off while the key is being read from the keychain.
    /// Consent is checked again after the read, so nothing is sent: the
    /// answer is the not-permitted sentence, never a request to the provider
    /// (whose endpoint is unroutable on purpose).
    #[tokio::test]
    async fn voice_speech_consent_revoked_during_the_keychain_read_sends_nothing() {
        let consent = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let secrets: Arc<dyn SecretStore> = Arc::new(RevokedWhileReading(Arc::clone(&consent)));
        let intent = openai_at("https://voice-speech.invalid/v1/chat/completions");
        let settings_now = || {
            let reading = if consent.load(std::sync::atomic::Ordering::SeqCst) {
                ReadingConsent::On
            } else {
                ReadingConsent::Off
            };
            voice(SpeechSource::Provider, intent.clone(), reading)
        };
        assert!(provider_permitted(&settings_now()).is_ok());
        assert_eq!(
            synthesise(&intent, secrets, "hello", || async {
                provider_permitted(&settings_now())
            })
            .await,
            Err(SpeechError::refused(PROVIDER_SPEECH_NOT_PERMITTED))
        );
    }

    /// A keychain whose read is where the user changes the Commands
    /// connection, as a save during a keychain prompt would.
    struct ConnectionChangedWhileReading(Arc<std::sync::atomic::AtomicBool>);

    impl SecretStore for ConnectionChangedWhileReading {
        fn store(
            &self,
            _: crate::secrets::SecretId,
            _: &crate::secrets::Secret,
        ) -> Result<(), crate::secrets::SecretError> {
            Ok(())
        }

        fn load(
            &self,
            _: crate::secrets::SecretId,
        ) -> Result<Option<crate::secrets::Secret>, crate::secrets::SecretError> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(crate::secrets::Secret::new("sk-new-connection")))
        }

        fn delete(&self, _: crate::secrets::SecretId) -> Result<(), crate::secrets::SecretError> {
            Ok(())
        }
    }

    /// Scenario (PR #1617 review): reading is on with the provider's speech
    /// when a sentence is asked for, and the Commands connection is changed to
    /// another endpoint while the key is being read. The settings still permit
    /// the provider's speech, but they name a different connection, so nothing
    /// is sent: the answer is the connection-changed sentence, never a request
    /// to the old endpoint (unroutable on purpose) with the new key.
    #[tokio::test]
    async fn voice_speech_connection_changed_during_the_keychain_read_sends_nothing() {
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let secrets: Arc<dyn SecretStore> =
            Arc::new(ConnectionChangedWhileReading(Arc::clone(&changed)));
        let old = openai_at("https://voice-speech-old.invalid/v1/chat/completions");
        let new = openai_at("https://voice-speech-new.invalid/v1/chat/completions");
        let settings_now = || {
            let intent = if changed.load(std::sync::atomic::Ordering::SeqCst) {
                new.clone()
            } else {
                old.clone()
            };
            voice(SpeechSource::Provider, intent, ReadingConsent::On)
        };
        assert!(permitted_on(&settings_now(), &old, "hello").is_ok());
        assert_eq!(
            synthesise(&old, secrets, "hello", || async {
                permitted_on(&settings_now(), &old, "hello")
            })
            .await,
            Err(SpeechError::refused(SPEECH_CONNECTION_CHANGED))
        );
        // Another model at the same endpoint is the same connection.
        let other_model = IntentSettings {
            model: crate::model_service::ModelId::parse("gpt-5").expect("valid"),
            ..old.clone()
        };
        assert!(
            permitted_on(
                &voice(SpeechSource::Provider, other_model, ReadingConsent::On),
                &old,
                "hello"
            )
            .is_ok()
        );
    }

    /// Stands in for a speech request in flight: never finishes, and records
    /// being dropped, which is what cancels a real request.
    struct InFlight(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for InFlight {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn in_flight(
        dropped: &Arc<std::sync::atomic::AtomicBool>,
    ) -> impl Future<Output = Result<Vec<u8>, SpeechError>> {
        let guard = InFlight(Arc::clone(dropped));
        async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            Ok(Vec::new())
        }
    }

    async fn settle<F: Future + Unpin>(future: &mut F) -> Option<F::Output> {
        tokio::time::timeout(Duration::from_millis(50), future)
            .await
            .ok()
    }

    /// Scenario (PR #1617 review): a speech request is in flight when the
    /// settings are saved with reading's opt-in turned off. The request is
    /// dropped — cancelling it — and the command answers the not-permitted
    /// sentence; a save that keeps the opt-in and the connection leaves an
    /// in-flight request alone, and one that changes the connection cancels it
    /// too.
    #[tokio::test]
    async fn voice_speech_a_save_revoking_consent_cancels_the_request_in_flight() {
        let intent = openai();
        let revocation = SpeechRevocation::default();

        // Consent off while the request is pending.
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut request = Box::pin(unless_revoked(
            in_flight(&dropped),
            revocation.revoked(intent.clone(), "hello"),
        ));
        assert_eq!(settle(&mut request).await, None, "the request is pending");
        // A save that changes nothing that matters to it does not cancel it.
        revocation.publish(&voice(
            SpeechSource::Auto,
            intent.clone(),
            ReadingConsent::On,
        ));
        assert_eq!(
            settle(&mut request).await,
            None,
            "an unrelated save cancelled it"
        );
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        revocation.publish(&voice(
            SpeechSource::Auto,
            intent.clone(),
            ReadingConsent::Off,
        ));
        assert_eq!(
            settle(&mut request).await,
            Some(Err(SpeechError::refused(PROVIDER_SPEECH_NOT_PERMITTED)))
        );
        drop(request);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));

        // The connection replaced while the request is pending.
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut request = Box::pin(unless_revoked(
            in_flight(&dropped),
            revocation.revoked(intent.clone(), "hello"),
        ));
        assert_eq!(
            settle(&mut request).await,
            None,
            "a save before it began cancelled it"
        );
        revocation.publish(&voice(
            SpeechSource::Auto,
            openai_at("https://gateway.example/v1/chat/completions"),
            ReadingConsent::On,
        ));
        assert_eq!(
            settle(&mut request).await,
            Some(Err(SpeechError::refused(SPEECH_CONNECTION_CHANGED)))
        );
        drop(request);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Scenario (PR #1617's third review): a speech request is in flight when
    /// two saves land before it runs again — reading's opt-in turned off and
    /// straight back on, or the connection changed and changed back. The
    /// request is still cancelled: a save that does not permit it cancels it
    /// at the save, and a later one that would permit it cannot take that
    /// back. A request still permitted by both saves is left alone, and a
    /// finished request is forgotten.
    #[tokio::test]
    async fn voice_speech_a_revoking_save_is_not_undone_by_a_later_one() {
        let intent = openai();
        let other = openai_at("https://gateway.example/v1/chat/completions");
        for (name, first) in [
            (
                "opt-in off then on",
                voice(SpeechSource::Auto, intent.clone(), ReadingConsent::Off),
            ),
            (
                "connection away and back",
                voice(SpeechSource::Auto, other.clone(), ReadingConsent::On),
            ),
            (
                "provider, system, provider",
                voice(SpeechSource::System, intent.clone(), ReadingConsent::On),
            ),
        ] {
            let revocation = SpeechRevocation::default();
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut request = Box::pin(unless_revoked(
                in_flight(&dropped),
                revocation.revoked(intent.clone(), "hello"),
            ));
            assert_eq!(settle(&mut request).await, None, "{name}: pending");
            // Both saves before the request is polled again.
            revocation.publish(&first);
            revocation.publish(&voice(
                SpeechSource::Auto,
                intent.clone(),
                ReadingConsent::On,
            ));
            assert!(
                matches!(
                    settle(&mut request).await,
                    Some(Err(SpeechError::Refused { .. }))
                ),
                "{name}: the revoking save was coalesced away"
            );
            drop(request);
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "{name}");
            assert_eq!(revocation.pending(), 0, "{name}");
        }

        // Permitted by every save: left alone, and forgotten once it ends.
        let revocation = SpeechRevocation::default();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut request = Box::pin(unless_revoked(
            in_flight(&dropped),
            revocation.revoked(intent.clone(), "hello"),
        ));
        let permitting = voice(SpeechSource::Provider, intent.clone(), ReadingConsent::On);
        revocation.publish(&permitting);
        revocation.publish(&permitting);
        assert_eq!(settle(&mut request).await, None);
        assert_eq!(revocation.pending(), 1);
        drop(request);
        assert_eq!(revocation.pending(), 0);
        revocation.publish(&permitting);
        assert!(revocation.registry().in_flight.is_empty());
    }

    /// Scenario (PR #1617's fourth review): the speech command registers at
    /// entry, and two saves land before its first settings read — reading's
    /// opt-in turned off and straight back on, or the connection changed and
    /// changed back. Its read then sees the settings as they are again, and
    /// the request is still refused once its connection is known: every save
    /// since the command started is held against it. Saves that permit it
    /// throughout leave it alone.
    #[tokio::test]
    async fn voice_speech_a_save_before_the_first_read_is_not_missed() {
        let intent = openai();
        let other = openai_at("https://gateway.example/v1/chat/completions");
        for (name, first, expected) in [
            (
                "opt-in off then on",
                voice(SpeechSource::Auto, intent.clone(), ReadingConsent::Off),
                PROVIDER_SPEECH_NOT_PERMITTED,
            ),
            (
                "connection away and back",
                voice(SpeechSource::Auto, other.clone(), ReadingConsent::On),
                SPEECH_CONNECTION_CHANGED,
            ),
        ] {
            let revocation = SpeechRevocation::default();
            let ticket = revocation.register("hello");
            revocation.publish(&first);
            revocation.publish(&voice(
                SpeechSource::Auto,
                intent.clone(),
                ReadingConsent::On,
            ));
            // The first settings read now sees `intent` with reading on.
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut request = Box::pin(unless_revoked(
                in_flight(&dropped),
                revocation.prepared_for(ticket, intent.clone()),
            ));
            assert_eq!(
                settle(&mut request).await,
                Some(Err(SpeechError::refused(expected))),
                "{name}: the save before the first read was missed"
            );
            drop(request);
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "{name}");
            assert_eq!(revocation.pending(), 0, "{name}");
        }

        // Permitted by every save since it registered: left alone, and still
        // held against the saves after it knows its connection.
        let revocation = SpeechRevocation::default();
        let ticket = revocation.register("hello");
        revocation.publish(&voice(
            SpeechSource::Provider,
            intent.clone(),
            ReadingConsent::On,
        ));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut request = Box::pin(unless_revoked(
            in_flight(&dropped),
            revocation.prepared_for(ticket, intent.clone()),
        ));
        assert_eq!(
            settle(&mut request).await,
            None,
            "a permitting save cancelled it"
        );
        revocation.publish(&voice(
            SpeechSource::Auto,
            intent.clone(),
            ReadingConsent::Off,
        ));
        assert_eq!(
            settle(&mut request).await,
            Some(Err(SpeechError::refused(PROVIDER_SPEECH_NOT_PERMITTED)))
        );

        // A ticket dropped before its connection was known is forgotten.
        let revocation = SpeechRevocation::default();
        drop(revocation.register("hello"));
        assert_eq!(revocation.pending(), 0);
    }

    /// Scenario (PR #1617's third review): the webview reads a refusal and a
    /// failure apart, so a refused request is never answered with the system
    /// voice.
    #[test]
    fn voice_speech_error_serialises_as_the_webview_reads_it() {
        assert_eq!(
            serde_json::to_value(SpeechError::refused("no")).unwrap(),
            json!({ "kind": "refused", "message": "no" })
        );
        assert_eq!(
            serde_json::to_value(SpeechError::failed("down")).unwrap(),
            json!({ "kind": "failed", "message": "down" })
        );
    }
}
