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
//! [`provider_permitted`] — which the speech command checks before every
//! request — refuses. A webview asking for audio directly gets the same
//! refusal.
//!
//! # The audio is fetched here and not in the webview
//!
//! For the reason every network hop is Rust-side: the webview's CSP permits no
//! network origin, and the key is in this process. The audio crosses the IPC
//! boundary as bytes and is decoded by the webview's Web Audio, which a CSP
//! does not restrict.

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
pub const PROVIDER_SPEECH_NOT_PERMITTED: &str = "the provider's speech is used only while Read turns aloud is on and the speech source is \
     the provider's";

/// Whether a sentence may be sent to the provider's speech: reading's opt-in
/// is on and [`plan_for`] names the provider. Checked before every request,
/// whoever asks.
pub fn provider_permitted(settings: &VoiceSettings) -> Result<(), String> {
    match plan_for(settings) {
        SpeechPlan::Provider { .. } => Ok(()),
        _ => Err(PROVIDER_SPEECH_NOT_PERMITTED.to_string()),
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
/// the request ([`provider_permitted`] against the settings as they are then):
/// the keychain can take as long as a prompt the user answers, and reading's
/// opt-in turned off during it sends nothing (PR #1617's review).
pub async fn synthesise<P, F>(
    intent: &IntentSettings,
    secrets: Arc<dyn SecretStore>,
    text: &str,
    still_permitted: P,
) -> Result<Vec<u8>, String>
where
    P: FnOnce() -> F,
    F: std::future::Future<Output = Result<(), String>>,
{
    let Some(endpoint) = speech_endpoint(intent) else {
        return Err(NO_PROVIDER_SPEECH.to_string());
    };
    if text.trim().is_empty() {
        return Err("there is nothing to say".to_string());
    }
    let secret = endpoint_credential(&endpoint, &secrets).await?;
    still_permitted().await?;
    let Some(client) = super::http::client() else {
        return Err("speech could not start a secure connection".to_string());
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
            if error.is_timeout() {
                format!(
                    "the speech service did not answer within {}s",
                    SPEECH_TIMEOUT.as_secs()
                )
            } else {
                "the speech request failed".to_string()
            }
        })?;
    let status = response.status();
    let audio_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_mp3_type);
    let body = super::http::capped_body(response, MAX_AUDIO_BYTES)
        .await
        .map_err(|error| match error {
            super::http::BodyError::TooLarge => {
                format!("the speech service answered with more than {MAX_AUDIO_BYTES} bytes")
            }
            super::http::BodyError::Transport => "the speech request failed".to_string(),
        })?;
    check_audio(status, audio_type, body)
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
        let body = request_body("The tester finished: done.");
        assert_eq!(body["model"], OPENAI_SPEECH_MODEL);
        assert_eq!(body["voice"], OPENAI_SPEECH_VOICE);
        assert_eq!(body["response_format"], "mp3");
        assert_eq!(body["input"], "The tester finished: done.");

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

    async fn permitted() -> Result<(), String> {
        Ok(())
    }

    #[tokio::test]
    async fn voice_speech_refuses_without_a_route_or_a_key_before_any_request() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
        assert_eq!(
            synthesise(&anthropic(), Arc::clone(&secrets), "hello", permitted).await,
            Err(NO_PROVIDER_SPEECH.to_string())
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
        assert!(error.contains("no key is stored"), "{error}");
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
            Err(PROVIDER_SPEECH_NOT_PERMITTED.to_string())
        );
    }
}
