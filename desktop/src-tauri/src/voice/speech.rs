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
//! layout. Anthropic offers no text-to-speech at all. A server that speaks
//! chat completions but has no speech route (`llama.cpp`'s, for one) answers
//! the speech request with an error, and under **Auto** that falls back to the
//! system voice, so the guess costs one failed request rather than silence.
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
use crate::settings::{IntentBackend, IntentSettings, SpeechSource};

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
pub async fn synthesise(
    intent: &IntentSettings,
    secrets: Arc<dyn SecretStore>,
    text: &str,
) -> Result<Vec<u8>, String> {
    let Some(endpoint) = speech_endpoint(intent) else {
        return Err(NO_PROVIDER_SPEECH.to_string());
    };
    if text.trim().is_empty() {
        return Err("there is nothing to say".to_string());
    }
    let secret = endpoint_credential(&endpoint, &secrets).await?;
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
        .is_some_and(|value| value.starts_with("audio/"));
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

/// Accept a reply as audio only when it succeeded, says it is audio, and has
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

    #[tokio::test]
    async fn voice_speech_refuses_without_a_route_or_a_key_before_any_request() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
        assert_eq!(
            synthesise(&anthropic(), Arc::clone(&secrets), "hello").await,
            Err(NO_PROVIDER_SPEECH.to_string())
        );
        // Off this machine with no key stored: refused by the keychain read,
        // so no request is made (the endpoint is unroutable on purpose).
        let error = synthesise(
            &openai_at("https://voice-speech.invalid/v1/chat/completions"),
            secrets,
            "hello",
        )
        .await
        .expect_err("no key");
        assert!(error.contains("no key is stored"), "{error}");
    }
}
