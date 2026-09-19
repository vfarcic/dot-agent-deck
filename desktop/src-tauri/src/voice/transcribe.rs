//! PRD #802 M7: the transcription seam, and the one backend behind it.
//!
//! [`super::capture`] produces 16 kHz mono PCM; this turns it into a
//! [`super::Transcript`], which is what [`super::handle_utterance`] already
//! knows how to route. The shape deliberately mirrors
//! [`super::resolver::IntentResolver`]: an object-safe trait, a boxed future,
//! one `…_for` function that reads the settings at call time, and a stub every
//! test downstream drives.
//!
//! # `Off` is the default, and it is a product statement
//!
//! PRD #802 is explicit that transcription is the **one** stage with no no-key
//! trick — Siri matches declared App Intents rather than handing over a
//! transcript, `SFSpeechRecognizer` exists on one of three platforms, and local
//! whisper is deferred to D1 behind a decision to take a C/C++ toolchain into
//! `cargo test-fast`. So with nothing configured, the panel works from **typed
//! input** through the identical resolve → validate → execute → report path.
//!
//! That is why [`OffTranscriber`] answers [`TranscriptionError::NotConfigured`]
//! and why [`TranscriptionOutcome`] carries
//! [`TranscriptionOutcome::NotConfigured`] as its own variant rather than
//! folding it into a failure: *nothing is set up yet* and *the request timed
//! out* are different things for the user to do next, and a settings
//! instruction dressed as an error teaches people the feature is broken.
//!
//! # The credential is read here and never crosses into the webview
//!
//! Same rule and same mechanism as [`super::remote`]: the key is loaded from
//! [`SecretStore`] at call time, in this process, and goes straight into a
//! request header. There is no `desktop_load_secret` command and M7 does not
//! add one — a credential in the webview is one `JSON.stringify` from the
//! `localStorage` half of PRD #803's rule.
//!
//! # No utterance is persisted by this module, and that includes the audio
//!
//! PRD #802's Open Question 5. Nothing here writes a buffer or a transcript to
//! disk, and nothing logs one at any level: the request body is assembled in
//! memory, sent, and dropped. The error paths quote the API's own message and
//! the HTTP status and never the audio or the text.
//!
//! **Scoped to this module deliberately.** The claim holds without qualification
//! here — this is an HTTPS request and nothing else — but the feature-wide
//! version of it does not, because the agent-CLI intent backend hands its prompt
//! to another program that has storage of its own. [`super::agent_cli`] carries
//! that half, and [`super::Transcript`] carries the sentence covering both.
//! What the remote endpoint does with an upload is the endpoint's policy and
//! not a property this app can assert at all.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::secrets::{SecretId, SecretStore};

use super::Transcript;
use super::capture::Pcm16;

/// Where the request goes.
pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";

/// The model this backend asks.
///
/// **Whisper, and the reason is the deferred milestone rather than the price.**
/// PRD #802's D1 is local whisper, and it is described as the thing that
/// removes the credential requirement from transcription. Pinning the hosted
/// backend to the *same model family* is what makes D1 a swap rather than a
/// behaviour change: the phrases that work today go on working when the
/// credential goes away. A cheaper or more accurate hosted model would buy a
/// fraction of a cent an utterance and spend that property.
pub const DEFAULT_MODEL: &str = "whisper-1";

/// How long the request gets before the attempt is abandoned.
///
/// Generous on purpose, and for [`super::remote::REMOTE_TIMEOUT`]'s reason: a
/// backstop against a hung connection, not a latency budget. This one is longer
/// because the request carries up to [`super::capture::MAX_UTTERANCE`] of audio
/// — 960 KB — so the upload is part of the wait on a slow link, and a user who
/// has already spoken is better served by a slow answer than by a failure
/// sentence.
pub const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a transcriber could not answer.
///
/// Split the way [`super::resolver::IntentError`] is, and for the same reason:
/// nothing is configured yet (a settings instruction) against a backend that is
/// configured and failed (an error). **They are not the same sentence and must
/// not render as one.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionError {
    /// No transcription backend is configured, or the configured one has no
    /// credential.
    NotConfigured(String),
    /// The backend ran and did not produce a usable transcript — a timeout, a
    /// non-2xx reply, an unreadable body.
    Backend(String),
}

impl TranscriptionError {
    pub fn detail(&self) -> &str {
        match self {
            TranscriptionError::NotConfigured(detail) | TranscriptionError::Backend(detail) => {
                detail
            }
        }
    }
}

impl fmt::Display for TranscriptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for TranscriptionError {}

/// The future a [`Transcriber`] returns.
///
/// Boxed rather than an `async fn` in the trait, for
/// [`super::resolver::ResolveFuture`]'s reason exactly: the backend is chosen
/// from settings at runtime, so the trait has to be object-safe, and an
/// `async fn` in a trait is not.
pub type TranscribeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Transcript, TranscriptionError>> + Send + 'a>>;

/// A backend that turns audio into text.
pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(&'a self, audio: &'a Pcm16) -> TranscribeFuture<'a>;

    /// Which backend this is, for the surface to name — `off`, `remote`,
    /// `stub`. It travels on every [`VoiceTranscription`] beside the latency,
    /// for [`super::resolver::IntentResolver::backend_name`]'s reason: the
    /// number alone is a complaint and the number with a name is a decision.
    fn backend_name(&self) -> &'static str;
}

/// Build the backend the settings name.
///
/// `secrets` is taken for every variant even though only
/// [`TranscriptionBackend::Remote`](crate::settings::TranscriptionBackend::Remote)
/// reads one — [`super::resolver::resolver_for`]'s reason: passing the store
/// rather than a credential is what keeps the value Rust-side, read at the
/// moment it is needed and never handed around.
pub fn transcriber_for(
    backend: crate::settings::TranscriptionBackend,
    secrets: Arc<dyn SecretStore>,
) -> Box<dyn Transcriber> {
    use crate::settings::TranscriptionBackend;
    match backend {
        TranscriptionBackend::Off => Box::new(OffTranscriber),
        TranscriptionBackend::Remote => Box::new(RemoteTranscriber::new(secrets)),
    }
}

/// No transcription backend.
///
/// The default, and **a first-class variant rather than a degraded mode**: it
/// answers [`TranscriptionError::NotConfigured`] with a sentence that names
/// where to fix it, and the surface renders that as an instruction beside a
/// working typed-input box.
pub struct OffTranscriber;

/// What the user is told when nothing is configured.
///
/// One string rather than one per call site, because it is the sentence that
/// has to read as an instruction: a second wording somewhere else is how it
/// drifts into sounding like a failure.
pub const NOT_CONFIGURED: &str =
    "no transcription backend is configured — type your command, or choose one in Settings → Voice";

impl Transcriber for OffTranscriber {
    fn transcribe<'a>(&'a self, _audio: &'a Pcm16) -> TranscribeFuture<'a> {
        Box::pin(async { Err(TranscriptionError::NotConfigured(NOT_CONFIGURED.into())) })
    }

    fn backend_name(&self) -> &'static str {
        "off"
    }
}

// -- the keyed remote backend ----------------------------------------------

/// Transcribe by uploading the utterance to a hosted speech model.
pub struct RemoteTranscriber {
    secrets: Arc<dyn SecretStore>,
    /// `None` when no client could be built — see [`super::http::client`]. This
    /// backend reports that rather than falling back to a permissive one.
    client: Option<reqwest::Client>,
    endpoint: String,
    model: String,
}

impl RemoteTranscriber {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            secrets,
            // Built once and reused, and the timeout set per REQUEST rather
            // than on a builder — `super::remote::RemoteResolver::new` spells
            // out why: `ClientBuilder::build` is fallible, and the obvious
            // `.timeout(..).build().unwrap_or_default()` silently yields a
            // client with no timeout on the failure path.
            //
            // The builder carries the REDIRECT policy, which has no per-request
            // spelling. This backend authenticates with the standard
            // `Authorization` header, which reqwest DOES strip across an origin
            // change — so the `x-api-key` leak PRD #802's audit found is not
            // this one's. What is this one's: a 307 or 308 re-sends the body,
            // and the body here is the user's voice. Same policy, different
            // reason.
            client: super::http::client(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Send somewhere else. PRD #802 M9's credentialed lane is what this is
    /// for; nothing in the merge-blocking tier opens a socket at all.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    async fn run(&self, audio: &Pcm16) -> Result<Transcript, TranscriptionError> {
        // An empty or silent buffer cannot produce words, and sending one costs
        // a round trip and a fraction of a cent to be told so. Reported as a
        // BACKEND failure rather than as a not-configured one: the setup is
        // fine and the microphone heard nothing, which is a different thing to
        // do next.
        if audio.is_empty() || audio.is_silent() {
            return Err(TranscriptionError::Backend(
                "the microphone heard nothing".into(),
            ));
        }

        // Read at call time, Rust-side, and dropped with this scope.
        let secret = match self.secrets.load(SecretId::VoiceTranscription) {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                return Err(TranscriptionError::NotConfigured(
                    "no key is stored for the remote transcription backend — add one in Settings → Voice"
                        .into(),
                ));
            }
            // The keychain itself failed. PRD #802 M4's rule: "I could not find
            // out" must not render as "nothing stored", or the user retypes a
            // key into a store that cannot hold it.
            Err(error) => return Err(TranscriptionError::NotConfigured(error.public())),
        };

        let Some(client) = self.client.as_ref() else {
            // Fail closed: never a fallback to a client that would re-send the
            // audio to a redirect target.
            return Err(TranscriptionError::Backend(
                "the transcription backend could not start a secure connection".into(),
            ));
        };

        let boundary = boundary();
        let body = multipart_body(audio, &self.model, &boundary);
        let response = client
            .post(&self.endpoint)
            .timeout(TRANSCRIBE_TIMEOUT)
            .header("content-type", content_type(&boundary))
            .header("authorization", format!("Bearer {}", secret.expose()))
            .body(body)
            .send()
            .await
            .map_err(|error| TranscriptionError::Backend(transport_detail(&error)))?;

        let status = response.status();
        // Bounded BEFORE the bytes become text or JSON, on the success and the
        // failure path alike — PRD #802's audit. `Response::json` collected the
        // whole body first, and a request timeout bounds elapsed time rather
        // than bytes.
        let body = match super::http::capped_body(response, super::http::MAX_BODY_BYTES).await {
            Ok(body) => body,
            Err(super::http::BodyError::TooLarge) => {
                return Err(TranscriptionError::Backend(format!(
                    "the transcription backend answered {status} with more than {} bytes",
                    super::http::MAX_BODY_BYTES
                )));
            }
            Err(super::http::BodyError::Transport) => {
                return Err(TranscriptionError::Backend(
                    "the request to the transcription backend failed".into(),
                ));
            }
        };
        let payload: Value = serde_json::from_slice(&body).map_err(|_| {
            TranscriptionError::Backend(format!(
                "the transcription backend answered {status} unreadably"
            ))
        })?;
        if !status.is_success() {
            return Err(TranscriptionError::Backend(api_error_detail(
                status, &payload,
            )));
        }
        parse_response(&payload)
    }
}

impl Transcriber for RemoteTranscriber {
    fn transcribe<'a>(&'a self, audio: &'a Pcm16) -> TranscribeFuture<'a> {
        Box::pin(self.run(audio))
    }

    fn backend_name(&self) -> &'static str {
        "remote"
    }
}

/// The multipart separator for one request.
///
/// Random rather than fixed, because a boundary that appeared inside the body
/// would truncate the upload — and the body is a WAV, whose sample bytes are
/// arbitrary. `getrandom` is already in this workspace's graph for the hook
/// capability token, so this costs nothing new.
fn boundary() -> String {
    let mut bytes = [0u8; 16];
    // A failure here is a machine with no OS randomness, which is not a state
    // this process can realistically be in — but the alternative spelling is an
    // `.expect()` on the audio path, and that is a panic inside a Tauri
    // command. So it falls back to the zeroes the array was built with. The
    // consequence of a predictable boundary is a truncated upload IF the audio
    // happens to contain those exact bytes, which is a bad transcript; it is
    // not a security property and must not be read as one.
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("dot-agent-deck-{hex}")
}

/// The `Content-Type` a [`multipart_body`] is sent with.
pub fn content_type(boundary: &str) -> String {
    format!("multipart/form-data; boundary={boundary}")
}

/// The whole request body: the WAV, the model, and nothing else.
///
/// **Assembled by hand rather than through `reqwest`'s `multipart` feature**,
/// and that is a dependency decision rather than a preference. Turning the
/// feature on adds `mime_guess` and a `futures-util` streaming body to a crate
/// whose reqwest spec is copied verbatim from the root package's — so cargo
/// would resolve one feature-unified build for both, and the root crate's
/// `src/version.rs` would start carrying a multipart encoder it has no use for.
/// What it would buy is the forty lines below, for a body with two parts, no
/// nested multipart and no filename this build does not choose itself.
///
/// `\r\n` throughout, and not `\n`: RFC 2046 says CRLF, and a server that
/// tolerates bare LF is not one this can rely on.
pub fn multipart_body(audio: &Pcm16, model: &str, boundary: &str) -> Vec<u8> {
    let wav = audio.to_wav();
    let mut body = Vec::with_capacity(wav.len() + 512);
    let mut part = |header: &str| body.extend_from_slice(header.as_bytes());

    part(&format!("--{boundary}\r\n"));
    part("content-disposition: form-data; name=\"model\"\r\n\r\n");
    part(model);
    part("\r\n");

    // `response_format` is named explicitly rather than left to the default:
    // the default is a JSON object today, and `parse_response` reads one, so a
    // server-side default that moved would turn a working build into an
    // unreadable-answer error.
    part(&format!("--{boundary}\r\n"));
    part("content-disposition: form-data; name=\"response_format\"\r\n\r\n");
    part("json");
    part("\r\n");

    // The filename is what tells the service the container, and `.wav` is what
    // `Pcm16::to_wav` produced. It names no user, no path and no session — it
    // is a constant, which is the point: a filename derived from anything would
    // be an utterance detail leaving this process.
    part(&format!("--{boundary}\r\n"));
    part("content-disposition: form-data; name=\"file\"; filename=\"utterance.wav\"\r\n");
    part("content-type: audio/wav\r\n\r\n");
    body.extend_from_slice(&wav);
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// The transcript, out of the reply.
pub fn parse_response(payload: &Value) -> Result<Transcript, TranscriptionError> {
    match payload["text"].as_str() {
        Some(text) => Ok(Transcript::new(text.trim())),
        None => Err(TranscriptionError::Backend(
            "the transcription backend answered without a transcript".into(),
        )),
    }
}

/// What a non-2xx reply becomes.
///
/// The API's own `error.message` is quoted for [`super::remote`]'s reason: it
/// is the difference between "your key is wrong" and "you are rate limited",
/// which the user must be able to act on. The status is always included so a
/// reply with no readable body still says something.
fn api_error_detail(status: reqwest::StatusCode, payload: &Value) -> String {
    match payload["error"]["message"].as_str() {
        Some(message) if !message.trim().is_empty() => {
            format!(
                "the transcription backend refused ({status}): {}",
                message.trim()
            )
        }
        _ => format!("the transcription backend refused ({status})"),
    }
}

/// What a transport failure becomes.
///
/// Classified rather than stringified, because `reqwest`'s own `Display`
/// carries the URL and a chain of source errors — debugging output, not a
/// sentence for someone who just spoke.
fn transport_detail(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        format!(
            "the transcription backend did not answer within {}s",
            TRANSCRIBE_TIMEOUT.as_secs()
        )
    } else if error.is_connect() {
        "the transcription backend could not be reached".to_string()
    } else {
        "the request to the transcription backend failed".to_string()
    }
}

// -- the stub --------------------------------------------------------------

/// A deterministic stand-in for a speech model.
///
/// [`super::StubResolver`]'s counterpart, and there for the same reason: every
/// outcome below this seam is reachable by scripting one of these, so the IPC
/// layer, the state machine and the rendered sentences are all asserted with no
/// model, no credential, no microphone and no network hop.
pub struct StubTranscriber {
    answer: Result<Transcript, TranscriptionError>,
}

impl StubTranscriber {
    pub fn hearing(text: impl Into<String>) -> Self {
        Self {
            answer: Ok(Transcript::new(text)),
        }
    }

    pub fn failing(error: TranscriptionError) -> Self {
        Self { answer: Err(error) }
    }
}

impl Transcriber for StubTranscriber {
    fn transcribe<'a>(&'a self, _audio: &'a Pcm16) -> TranscribeFuture<'a> {
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }

    fn backend_name(&self) -> &'static str {
        "stub"
    }
}

// -- the outcome -----------------------------------------------------------

/// What one utterance's transcription ended in, plus what it cost.
///
/// Mirrors [`super::VoiceResult`], which carries the intent half: the outcome,
/// the milliseconds it took and the backend that took them. PRD #802's
/// mitigation for *slow enough to feel broken* is to surface the number rather
/// than hide it, and transcription is on the same path as resolution — a user
/// waiting six seconds is owed the split between the two.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceTranscription {
    pub outcome: TranscriptionOutcome,
    /// Milliseconds spent in the backend, or `None` when none was called.
    pub transcribe_ms: Option<u32>,
    /// Which backend answered — `off`, `remote`, `stub`. Present even when no
    /// call was made, because it names what *would* have answered, which is
    /// what a settings-facing sentence is about.
    pub backend: &'static str,
    /// How much audio was captured, in milliseconds.
    pub audio_ms: u32,
}

impl VoiceTranscription {
    /// The sentence to show the user.
    pub fn sentence(&self) -> &str {
        self.outcome.sentence()
    }

    /// The text, when there is some.
    pub fn transcript(&self) -> Option<&Transcript> {
        match &self.outcome {
            TranscriptionOutcome::Heard { transcript, .. } => Some(transcript),
            _ => None,
        }
    }
}

/// The closed set of situations transcribing one utterance can end in.
///
/// Three rather than two, and the third is the point: *nothing is configured*
/// is neither a transcript nor a failure, and rendering it as either is the
/// mistake PRD #802 names when it calls `off` a product statement.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TranscriptionOutcome {
    /// Speech became text.
    ///
    /// The text may still be empty — a user who pressed the button and said
    /// nothing gets the no-match sentence from downstream, which already knows
    /// how to phrase it, rather than a failure from here.
    Heard {
        transcript: Transcript,
        sentence: String,
    },
    /// No transcription backend is configured. **Not a failure**: the panel
    /// works from typed input and the sentence is a settings instruction.
    NotConfigured { detail: String, sentence: String },
    /// Speech could not be turned into text.
    Failed { detail: String, sentence: String },
}

impl TranscriptionOutcome {
    pub fn sentence(&self) -> &str {
        match self {
            TranscriptionOutcome::Heard { sentence, .. }
            | TranscriptionOutcome::NotConfigured { sentence, .. }
            | TranscriptionOutcome::Failed { sentence, .. } => sentence,
        }
    }

    /// Whether anything downstream has a transcript to work with.
    pub fn is_heard(&self) -> bool {
        matches!(self, TranscriptionOutcome::Heard { .. })
    }
}

/// Transcribe one utterance and render its situation.
///
/// The transcription counterpart of [`super::handle_utterance`], and the
/// function the IPC layer calls: it times the backend, classifies the error and
/// produces the sentence, so no surface has to know how to phrase one and two
/// surfaces cannot phrase the same situation differently.
pub async fn handle_audio(transcriber: &dyn Transcriber, audio: &Pcm16) -> VoiceTranscription {
    let backend = transcriber.backend_name();
    let audio_ms = audio.duration().as_millis().min(u128::from(u32::MAX)) as u32;
    let started = std::time::Instant::now();
    let answered = transcriber.transcribe(audio).await;
    let transcribe_ms = Some(millis(started.elapsed()));

    let outcome = match answered {
        Ok(transcript) => TranscriptionOutcome::Heard {
            sentence: format!("Heard “{}”.", transcript.text()),
            transcript,
        },
        Err(TranscriptionError::NotConfigured(detail)) => {
            let detail = crate::dto::safe_message(detail);
            TranscriptionOutcome::NotConfigured {
                sentence: format!("Nothing to listen with — {detail}."),
                detail,
            }
        }
        Err(TranscriptionError::Backend(detail)) => {
            let detail = crate::dto::safe_message(detail);
            // The same wording `VoiceOutcome::transcription_failed` renders, so
            // a failure reads identically whether it arrives from here or from
            // the pipeline's own seam.
            TranscriptionOutcome::Failed {
                sentence: format!("Could not turn that into text ({detail})."),
                detail,
            }
        }
    };

    VoiceTranscription {
        // `None` is a real answer and not a missing one, which is
        // [`super::VoiceResult::resolve_ms`]'s rule: nothing was transcribed on
        // the not-configured path, and rendering the microseconds it took to
        // say so would claim a measurement nobody made.
        transcribe_ms: match &outcome {
            TranscriptionOutcome::NotConfigured { .. } => None,
            _ => transcribe_ms,
        },
        outcome,
        backend,
        audio_ms,
    }
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{MemorySecretStore, Secret, SecretErrorKind};
    use crate::settings::TranscriptionBackend;
    use serde_json::json;

    // Every test here is pure: bytes in, a `Value` in, a `Value` or an outcome
    // out. PRD #802 M5's rule carried forward — nothing in the merge-blocking
    // tier needs a credential, a microphone or the network, and the strongest
    // form of that is a suite that opens no socket at all rather than one that
    // opens a loopback one.

    fn audio(samples: usize) -> Pcm16 {
        // Loud enough not to be mistaken for silence, which the remote backend
        // short-circuits on.
        Pcm16::new(
            (0..samples)
                .map(|i| if i % 2 == 0 { 9_000 } else { -9_000 })
                .collect(),
        )
    }

    fn store() -> Arc<MemorySecretStore> {
        Arc::new(MemorySecretStore::new())
    }

    // -- selection ---------------------------------------------------------

    #[test]
    fn voice_transcribe_selects_the_backend_the_settings_name() {
        assert_eq!(
            transcriber_for(TranscriptionBackend::Off, store()).backend_name(),
            "off"
        );
        assert_eq!(
            transcriber_for(TranscriptionBackend::Remote, store()).backend_name(),
            "remote"
        );
    }

    #[test]
    fn voice_transcribe_selection_is_total_over_the_settings_enum() {
        // A variant added to `TranscriptionBackend` without an adapter would be
        // a settings value the app cannot honour, which `VoiceSettings`' own
        // docs call a lie in a file the user can read. The `match` is
        // exhaustive so the compiler catches it; this asserts the set that was
        // mapped, which the compiler cannot.
        let names: Vec<&str> = [TranscriptionBackend::Off, TranscriptionBackend::Remote]
            .into_iter()
            .map(|backend| transcriber_for(backend, store()).backend_name())
            .collect();
        assert_eq!(names, vec!["off", "remote"]);
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn voice_transcribe_is_object_safe() {
        // The property the settings choice depends on: the backend is picked at
        // call time, so it has to be storable behind a `dyn`.
        let transcriber: Box<dyn Transcriber> = Box::new(OffTranscriber);
        assert_eq!(transcriber.backend_name(), "off");
    }

    // -- off ---------------------------------------------------------------

    #[tokio::test]
    async fn voice_transcribe_off_is_not_configured_rather_than_a_failure() {
        let result = handle_audio(&OffTranscriber, &audio(16)).await;
        assert!(
            matches!(&result.outcome, TranscriptionOutcome::NotConfigured { .. }),
            "{:?}",
            result.outcome
        );
        assert!(!result.outcome.is_heard());
        assert_eq!(result.backend, "off");
        // Distinguishable in the rendered sentence too, not only in the type:
        // the user reads a sentence, and this one must not sound like a break.
        assert!(
            result.sentence().contains("Nothing to listen with"),
            "{result:?}"
        );
        assert!(result.sentence().contains("Settings"), "{result:?}");
        assert!(
            !result.sentence().contains("Could not"),
            "the not-configured sentence read as a failure: {result:?}"
        );
    }

    #[tokio::test]
    async fn voice_transcribe_off_claims_no_measurement_it_did_not_take() {
        let result = handle_audio(&OffTranscriber, &audio(16)).await;
        assert_eq!(result.transcribe_ms, None);
    }

    #[tokio::test]
    async fn voice_transcribe_off_opens_no_socket_and_reads_no_secret() {
        // `OffTranscriber` holds no store at all, which is the structural
        // version of this claim; the type is what the assertion is on.
        let transcriber = transcriber_for(TranscriptionBackend::Off, store());
        let error = transcriber
            .transcribe(&audio(16))
            .await
            .expect_err("is not configured");
        assert!(
            matches!(&error, TranscriptionError::NotConfigured(_)),
            "{error:?}"
        );
        assert_eq!(error.detail(), NOT_CONFIGURED);
    }

    // -- the outcome rendering ---------------------------------------------

    #[tokio::test]
    async fn voice_transcribe_reports_what_was_heard() {
        let result =
            handle_audio(&StubTranscriber::hearing("show me the tester"), &audio(32)).await;
        assert!(result.outcome.is_heard());
        assert_eq!(
            result.transcript().map(Transcript::text),
            Some("show me the tester")
        );
        assert_eq!(result.backend, "stub");
        assert_eq!(result.audio_ms, 2);
        assert!(result.transcribe_ms.is_some());
    }

    #[tokio::test]
    async fn voice_transcribe_a_backend_failure_is_its_own_outcome() {
        let result = handle_audio(
            &StubTranscriber::failing(TranscriptionError::Backend("it timed out".into())),
            &audio(32),
        )
        .await;
        assert!(
            matches!(&result.outcome, TranscriptionOutcome::Failed { .. }),
            "{:?}",
            result.outcome
        );
        assert_eq!(
            result.sentence(),
            "Could not turn that into text (it timed out)."
        );
    }

    #[tokio::test]
    async fn voice_transcribe_scrubs_a_backend_detail_before_rendering_it() {
        // A backend's own message reaches a sentence, so it goes through the
        // same seam every other free-form string does.
        let result = handle_audio(
            &StubTranscriber::failing(TranscriptionError::Backend("a\u{1b}[2Jb".into())),
            &audio(32),
        )
        .await;
        assert!(!result.sentence().contains('\u{1b}'), "{result:?}");
    }

    #[test]
    fn voice_transcribe_outcome_serializes_the_tokens_the_webview_reads() {
        let heard = TranscriptionOutcome::Heard {
            transcript: Transcript::new("go back"),
            sentence: "Heard “go back”.".into(),
        };
        let json = serde_json::to_value(&heard).expect("serializes");
        assert_eq!(json["kind"], "heard");
        assert_eq!(json["transcript"], "go back");

        let off = TranscriptionOutcome::NotConfigured {
            detail: NOT_CONFIGURED.into(),
            sentence: "…".into(),
        };
        assert_eq!(
            serde_json::to_value(&off).expect("serializes")["kind"],
            "not_configured"
        );

        let failed = TranscriptionOutcome::Failed {
            detail: "x".into(),
            sentence: "…".into(),
        };
        assert_eq!(
            serde_json::to_value(&failed).expect("serializes")["kind"],
            "failed"
        );
    }

    #[tokio::test]
    async fn voice_transcribe_result_serializes_the_shape_m6_reads() {
        let result = handle_audio(&StubTranscriber::hearing("go back"), &audio(16_000)).await;
        let json = serde_json::to_value(&result).expect("serializes");
        assert_eq!(json["outcome"]["kind"], "heard");
        assert_eq!(json["outcome"]["transcript"], "go back");
        assert_eq!(json["backend"], "stub");
        assert_eq!(json["audioMs"], 1000);
        assert!(json["transcribeMs"].is_number());
    }

    // -- the request shape -------------------------------------------------

    fn body(audio: &Pcm16) -> Vec<u8> {
        multipart_body(audio, DEFAULT_MODEL, "TESTBOUNDARY")
    }

    fn text_of(body: &[u8]) -> String {
        String::from_utf8_lossy(body).into_owned()
    }

    #[test]
    fn voice_transcribe_request_names_the_model_and_the_json_response_format() {
        let rendered = text_of(&body(&audio(8)));
        assert!(
            rendered.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("name=\"response_format\"\r\n\r\njson\r\n"),
            "{rendered}"
        );
    }

    #[test]
    fn voice_transcribe_request_carries_a_wav_named_by_a_constant() {
        let pcm = audio(8);
        let body = body(&pcm);
        let rendered = text_of(&body);
        assert!(
            rendered.contains("name=\"file\"; filename=\"utterance.wav\""),
            "{rendered}"
        );
        assert!(rendered.contains("content-type: audio/wav"), "{rendered}");
        // The WAV itself is in there verbatim, header and all.
        let wav = pcm.to_wav();
        assert!(
            body.windows(wav.len()).any(|window| window == wav),
            "the audio did not reach the body"
        );
    }

    #[test]
    fn voice_transcribe_request_is_well_formed_multipart() {
        let pcm = audio(4);
        let raw = body(&pcm);
        let rendered = text_of(&raw);
        assert!(rendered.starts_with("--TESTBOUNDARY\r\n"), "{rendered}");
        assert!(rendered.ends_with("\r\n--TESTBOUNDARY--\r\n"), "{rendered}");
        // Three parts: model, response_format, file.
        assert_eq!(
            rendered.matches("content-disposition: form-data").count(),
            3
        );

        // CRLF everywhere — a bare LF is the mistake a hand-written encoder
        // makes. Asserted on the ENVELOPE with the audio cut out, because a WAV
        // is arbitrary bytes and a sample that happened to be 0x0A would make
        // this assertion a coin toss rather than a check.
        let wav = pcm.to_wav();
        let at = raw
            .windows(wav.len())
            .position(|window| window == wav)
            .expect("the audio is in the body");
        let mut envelope = raw[..at].to_vec();
        envelope.extend_from_slice(&raw[at + wav.len()..]);
        let envelope = String::from_utf8(envelope).expect("the envelope is text");
        assert_eq!(
            envelope.matches('\n').count(),
            envelope.matches("\r\n").count(),
            "a bare LF in {envelope:?}"
        );
    }

    #[test]
    fn voice_transcribe_content_type_names_the_boundary_the_body_used() {
        assert_eq!(
            content_type("TESTBOUNDARY"),
            "multipart/form-data; boundary=TESTBOUNDARY"
        );
        // And a generated boundary is long enough not to collide with audio by
        // accident, and is not the same twice.
        let first = boundary();
        assert!(first.len() >= 32, "{first}");
        assert_ne!(first, boundary());
    }

    #[test]
    fn voice_transcribe_request_never_carries_the_credential() {
        // The key travels in a header and nowhere else. A body carrying it
        // would reach any logging or error path that prints a request.
        let store = MemorySecretStore::new();
        store
            .store(
                SecretId::VoiceTranscription,
                &Secret::new("sk-not-a-real-key-0123"),
            )
            .expect("stores");
        let rendered = text_of(&body(&audio(8)));
        assert!(!rendered.contains("sk-not-a-real-key"), "{rendered}");
        assert!(!rendered.contains("authorization"), "{rendered}");
    }

    // -- the response shape ------------------------------------------------

    #[test]
    fn voice_transcribe_parses_a_transcript() {
        let transcript =
            parse_response(&json!({"text": "  show me the tester  "})).expect("parses");
        assert_eq!(transcript.text(), "show me the tester");
    }

    #[test]
    fn voice_transcribe_parses_an_empty_transcript_rather_than_failing() {
        // Silence that reached the model is a no-match downstream, not a
        // transcription failure — the sentences are different and only the
        // pipeline can phrase the first one.
        let transcript = parse_response(&json!({"text": ""})).expect("parses");
        assert!(transcript.is_empty());
    }

    #[test]
    fn voice_transcribe_reports_a_reply_with_no_transcript() {
        let error = parse_response(&json!({"duration": 1.2})).expect_err("fails");
        assert!(error.detail().contains("without a transcript"), "{error}");
        let error = parse_response(&json!({"text": 7})).expect_err("fails");
        assert!(error.detail().contains("without a transcript"), "{error}");
    }

    #[test]
    fn voice_transcribe_reports_an_api_error_with_its_message() {
        let detail = api_error_detail(
            reqwest::StatusCode::UNAUTHORIZED,
            &json!({"error": {"message": "Incorrect API key provided"}}),
        );
        assert!(detail.contains("401"), "{detail}");
        assert!(detail.contains("Incorrect API key"), "{detail}");
    }

    #[test]
    fn voice_transcribe_reports_an_api_error_with_no_readable_body() {
        let detail = api_error_detail(reqwest::StatusCode::BAD_GATEWAY, &json!({}));
        assert!(detail.contains("502"), "{detail}");
    }

    #[test]
    fn voice_transcribe_transport_failures_carry_no_debugging_output() {
        // `reqwest::Error`'s own Display carries the URL and a source chain.
        // What reaches a sentence is this file's wording. A malformed URL
        // yields `is_builder`, which lands in the catch-all arm — the arm most
        // likely to leak.
        let error = reqwest::Client::new()
            .post("not a url")
            .build()
            .expect_err("a builder error");
        assert_eq!(
            transport_detail(&error),
            "the request to the transcription backend failed"
        );
    }

    // -- the credential ----------------------------------------------------

    #[tokio::test]
    async fn voice_transcribe_without_a_key_is_not_configured_and_opens_no_socket() {
        // The endpoint is unroutable on purpose: reaching it would be a failure
        // of this test's premise, not a flake.
        let transcriber = RemoteTranscriber::new(store())
            .with_endpoint("https://voice-transcription.invalid/never");
        let error = transcriber
            .transcribe(&audio(16_000))
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, TranscriptionError::NotConfigured(detail) if detail.contains("no key is stored")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn voice_transcribe_no_key_renders_as_an_instruction_not_a_break() {
        let transcriber = RemoteTranscriber::new(store())
            .with_endpoint("https://voice-transcription.invalid/never");
        let result = handle_audio(&transcriber, &audio(16_000)).await;
        assert!(
            matches!(&result.outcome, TranscriptionOutcome::NotConfigured { .. }),
            "{:?}",
            result.outcome
        );
        assert!(result.sentence().contains("Settings"), "{result:?}");
    }

    #[tokio::test]
    async fn voice_transcribe_reports_an_unreachable_keychain_rather_than_a_missing_key() {
        // PRD #802 M4's rule, one layer up: "I could not find out" must not
        // render as "nothing stored", or the user retypes their key into a
        // store that cannot hold it.
        let store = MemorySecretStore::failing(SecretErrorKind::Unavailable, "test");
        let transcriber = RemoteTranscriber::new(Arc::new(store))
            .with_endpoint("https://voice-transcription.invalid/never");
        let error = transcriber
            .transcribe(&audio(16_000))
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, TranscriptionError::NotConfigured(_)),
            "got {error:?}"
        );
        assert!(!error.detail().contains("no key is stored"), "{error}");
    }

    #[tokio::test]
    async fn voice_transcribe_silence_never_reaches_the_backend() {
        // Short-circuited BEFORE the credential is read, so a muted microphone
        // costs neither a keychain prompt nor a round trip. The store here
        // would panic the assertion below if it were consulted, because a
        // missing key reports a different sentence.
        let transcriber = RemoteTranscriber::new(store())
            .with_endpoint("https://voice-transcription.invalid/never");
        let error = transcriber
            .transcribe(&Pcm16::new(vec![0; 16_000]))
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, TranscriptionError::Backend(detail) if detail.contains("heard nothing")),
            "got {error:?}"
        );
        let error = transcriber
            .transcribe(&Pcm16::new(Vec::new()))
            .await
            .expect_err("fails");
        assert!(error.detail().contains("heard nothing"), "{error}");
    }

    #[test]
    fn voice_transcribe_names_itself_for_the_surface() {
        assert_eq!(RemoteTranscriber::new(store()).backend_name(), "remote");
        assert_eq!(OffTranscriber.backend_name(), "off");
    }
}
