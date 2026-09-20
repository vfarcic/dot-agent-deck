//! PRD #802 M7: the transcription seam, and the one backend behind it.
//!
//! [`super::capture`] produces 16 kHz mono PCM; this turns it into a
//! [`super::Transcript`], which is what [`super::handle_utterance`] already
//! knows how to route. The shape deliberately mirrors
//! [`super::resolver::IntentResolver`]: an object-safe trait, a boxed future,
//! one `…_for` function that reads the settings at call time, and a stub every
//! test downstream drives.
//!
//! # One backend, and the keyless one is the default
//!
//! PRD #802 shipped with transcription as the **one** stage it claimed had no
//! no-key trick — Siri matches declared App Intents rather than handing over a
//! transcript, `SFSpeechRecognizer` exists on one of three platforms, and local
//! whisper was deferred behind a decision to take a C/C++ toolchain into
//! `cargo test-fast`. The provider-selection work found the route that argument
//! had missed: **a container on loopback, over the same HTTP this file already
//! spoke.** Measured — `ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu` on
//! `127.0.0.1:18000`, the same three multipart parts, the same `text` field
//! back, 0.653 s median warm, and no credential anywhere.
//!
//! So there is **no `off` variant and no second implementation**.
//! [`HttpTranscriber`] is the whole of it, and the difference between local and
//! hosted is one field: [`HttpTranscriber::keyless`] holds no [`SecretStore`]
//! and sends no `Authorization` header, [`HttpTranscriber::keyed`] holds one
//! and does. The keyless path is not a credentialed path whose lookup happens
//! to return nothing — there is no lookup.
//!
//! [`TranscriptionOutcome::NotConfigured`] survives all of that, and earns its
//! keep on a better case than `off` ever was: **an endpoint nobody is
//! listening at**. *Start the container* and *the request timed out* are
//! different things to do next, and a prerequisite dressed as an error teaches
//! people the feature is broken. [`unreachable_detail`] is the sentence.
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
//! here — this is an HTTPS request and nothing else. It used to need a second
//! half: the agent-CLI intent backend handed its prompt to another program that
//! had storage of its own, and [`super::Transcript`] still carries the sentence
//! covering both because that is the rule a future backend inherits. That
//! backend is gone, so today every stage of this feature is an HTTPS request.
//! What the remote endpoint does with an upload is the endpoint's policy and
//! not a property this app can assert at all.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::model_service::{ModelId, ServiceUrl};
use crate::secrets::{SecretId, SecretStore, load_off_runtime};
use crate::settings::{
    KEYLESS_OFF_MACHINE, LOCAL_SPEECH_IMAGE, TranscriptionBackend, TranscriptionSettings,
};

use super::Transcript;
use super::capture::{MIN_SPEECH, Pcm16, SPEECH_FLOOR, SPEECH_WINDOW, SpeechMeasure};

/// How long the request gets before the attempt is abandoned.
///
/// Generous on purpose, and for [`super::remote::REMOTE_TIMEOUT`]'s reason: a
/// backstop against a hung connection, not a latency budget. This one is longer
/// because the request carries up to [`super::capture::MAX_UTTERANCE`] of audio
/// — 960 KB — so the upload is part of the wait on a slow link, and a user who
/// has already spoken is better served by a slow answer than by a failure
/// sentence.
pub const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(60);

/// The measurement clause and the sentence for a segment that held no speech.
///
/// # Two refusals, because there are two things to do about them
///
/// This used to be one frozen string — *"Nothing was said — still listening."*
/// — and PRD #802's product owner met it **with a real microphone, about words
/// he had said**. Two things were wrong with it and only one of them was the
/// gate. A sentence that says nothing was said tells the user they imagined
/// speaking; it names no threshold, no measurement and nothing to do next, so
/// the three causes it can have — a quiet input, a short utterance, or a bug —
/// are indistinguishable from outside the app. The owner had no way to tell
/// which he was looking at, and neither did anyone reading his report.
///
/// So the refusal is now a measurement, and [`SpeechMeasure`]'s two fields pick
/// which of the two it is:
///
/// * **nothing ever crossed [`SPEECH_FLOOR`]** — the input is too quiet for
///   this floor, and the user can act on that in a second by speaking up,
///   moving closer or raising the device's level. The number is printed
///   against the floor rather than described, because "quiet" is not
///   actionable and *"reached 410 where 600 counts as speech"* is.
/// * **it crossed the floor and there was not enough** — the audio was speech
///   and the utterance was too short or too sparse. Saying it again, a little
///   longer, is the fix.
///
/// Neither is phrased as a failure or as a fault in the user's hardware, which
/// is the rule [`TranscriptionOutcome::Silent`] exists to keep: nothing is
/// broken, voice is still on, and the microphone is still open.
///
/// The numbers go to the **user** rather than to a log because the only person
/// who can answer *"is my microphone quiet?"* is the one holding it — and on
/// the report this repo actually received, one refusal carrying these two
/// numbers would have separated the gate's defect from the floor's in one
/// utterance instead of a round trip.
fn not_enough_speech(measure: SpeechMeasure) -> (String, String) {
    if measure.peak_rms < SPEECH_FLOOR {
        let detail = format!(
            "too quiet — the loudest moment reached {} where {SPEECH_FLOOR} counts as speech",
            measure.peak_rms
        );
        let sentence = format!(
            "Too quiet to transcribe — the loudest moment reached {} where {SPEECH_FLOOR} counts \
             as speech. Move closer or turn the input up; still listening.",
            measure.peak_rms
        );
        return (detail, sentence);
    }
    let detail = format!(
        "only {} ms of speech inside the loudest {} ms, where {} ms is needed",
        measure.voiced.as_millis(),
        SPEECH_WINDOW.as_millis(),
        MIN_SPEECH.as_millis()
    );
    let sentence = format!(
        "I did not hear enough to transcribe — {} ms of speech inside the loudest {} ms, where {} \
         ms is needed. Say that again; still listening.",
        measure.voiced.as_millis(),
        SPEECH_WINDOW.as_millis(),
        MIN_SPEECH.as_millis()
    );
    (detail, sentence)
}

/// Why a transcriber could not answer.
///
/// Split the way [`super::resolver::IntentError`] is, and for the same reason:
/// something is not set up yet (an instruction) against a backend that is set
/// up and failed (an error). **They are not the same sentence and must not
/// render as one.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionError {
    /// The configured backend cannot run yet: nothing is listening at a
    /// loopback endpoint, or a keyed service has no key stored.
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

    /// Which backend this is, for the surface to name — `local`, `remote`,
    /// `stub`. It travels on every [`VoiceTranscription`] beside the latency,
    /// for [`super::resolver::IntentResolver::backend_name`]'s reason: the
    /// number alone is a complaint and the number with a name is a decision.
    fn backend_name(&self) -> &'static str;
}

/// Build the backend the settings name.
///
/// `secrets` is taken whichever backend is named even though only
/// [`TranscriptionBackend::Remote`] reads one —
/// [`super::resolver::resolver_for`]'s reason: passing the store rather than a
/// credential is what keeps the value Rust-side, read at the moment it is
/// needed and never handed around. What the keyless branch does with it is
/// **drop it**, which is the difference between "no key was found" and "no key
/// was asked for".
pub fn transcriber_for(
    settings: &TranscriptionSettings,
    secrets: Arc<dyn SecretStore>,
) -> Box<dyn Transcriber> {
    let endpoint = settings.endpoint.clone();
    let model = settings.model.clone();
    match settings.backend {
        // A refusal rather than a fallback, and never the keyed path: a
        // keyless backend pointed off this machine has no key to send, so
        // "try it anyway" IS the upload. See [`RefusedTranscriber`].
        TranscriptionBackend::Local => match HttpTranscriber::keyless(endpoint, model) {
            Ok(transcriber) => Box::new(transcriber),
            Err(refusal) => Box::new(RefusedTranscriber::new("local", refusal)),
        },
        TranscriptionBackend::Remote => Box::new(HttpTranscriber::keyed(secrets, endpoint, model)),
    }
}

/// A backend that answers one sentence and makes no request.
///
/// The shape a misconfiguration this module refuses to honour comes back as:
/// [`TranscriptionOutcome::NotConfigured`], which the surface renders as an
/// instruction rather than as a failure, carrying the rule that was broken.
///
/// It exists because the alternatives are both worse. Falling back to the
/// loopback preset would silently move the endpoint a user wrote, which is the
/// fold [`crate::settings::KEYLESS_OFF_MACHINE`] argues against; making
/// [`transcriber_for`] fallible would push the same decision onto its one
/// caller, which has no better answer available to it than this sentence.
///
/// `name` is the settings token rather than a word of its own, so the surface's
/// vocabulary — the document, the panel, the report — stays the one set.
struct RefusedTranscriber {
    name: &'static str,
    detail: &'static str,
}

impl RefusedTranscriber {
    fn new(name: &'static str, detail: &'static str) -> Self {
        Self { name, detail }
    }
}

impl Transcriber for RefusedTranscriber {
    fn transcribe<'a>(&'a self, _audio: &'a Pcm16) -> TranscribeFuture<'a> {
        Box::pin(async move { Err(TranscriptionError::NotConfigured(self.detail.to_string())) })
    }

    fn backend_name(&self) -> &'static str {
        self.name
    }
}

// -- the one HTTP backend --------------------------------------------------

/// Transcribe by posting the utterance to a speech service — on this machine or
/// hosted, which is the same request either way.
pub struct HttpTranscriber {
    /// `None` means **this endpoint takes no credential**, and the keychain is
    /// never consulted. It is not an absent store or a failed lookup: a keyless
    /// backend sends no `Authorization` header at all, which is what lets the
    /// local container — which has no notion of a key — be a first-class
    /// choice rather than a keyed path that happens to tolerate an empty one.
    secrets: Option<Arc<dyn SecretStore>>,
    /// `None` when no client could be built — see [`super::http::client`]. This
    /// backend reports that rather than falling back to a permissive one.
    client: Option<reqwest::Client>,
    endpoint: ServiceUrl,
    model: ModelId,
    /// What the surface names this — `local` or `remote`, from the settings
    /// token, so one vocabulary spans the document, the panel and the report.
    name: &'static str,
}

impl HttpTranscriber {
    /// A service that authenticates with a key of the app's own.
    pub fn keyed(secrets: Arc<dyn SecretStore>, endpoint: ServiceUrl, model: ModelId) -> Self {
        Self::build(Some(secrets), endpoint, model, "remote")
    }

    /// A service that takes no credential — the container on loopback.
    ///
    /// **Loopback is a precondition rather than an expectation**, and the
    /// refusal is why this is fallible where [`Self::keyed`] is not. No
    /// `Authorization` header is sent on this path, so an endpoint on another
    /// host is a captured utterance POSTed to a third party with no credential
    /// and no sign anything was wrong — the module's own invariant, *the audio
    /// never leaves the machine*, stated as a check instead of as prose.
    ///
    /// [`crate::settings::TranscriptionSettings::deserialize`] refuses the same
    /// pairing at the document and at the IPC seam, which is every route an
    /// untrusted value arrives by; this is the route Rust arrives by, and the
    /// two together are what make the invariant hold rather than hold usually.
    pub fn keyless(endpoint: ServiceUrl, model: ModelId) -> Result<Self, &'static str> {
        if !endpoint.is_loopback() {
            return Err(KEYLESS_OFF_MACHINE);
        }
        Ok(Self::build(None, endpoint, model, "local"))
    }

    fn build(
        secrets: Option<Arc<dyn SecretStore>>,
        endpoint: ServiceUrl,
        model: ModelId,
        name: &'static str,
    ) -> Self {
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
            // reason, and it applies to the keyless path too: a loopback
            // endpoint that redirects is still an endpoint sending the audio
            // somewhere the user did not write.
            client: super::http::client(),
            endpoint,
            model,
            name,
        }
    }

    async fn run(&self, audio: &Pcm16) -> Result<Transcript, TranscriptionError> {
        // A buffer with nothing said in it cannot produce words, and sending one
        // costs a round trip, a fraction of a cent, and — with a whisper-family
        // model — a training artefact presented to the user as a sentence they
        // said. See [`Pcm16::has_speech`].
        //
        // **The BACKSTOP rather than the gate**: [`handle_audio`] asks the same
        // question before it calls any transcriber, so in this app nothing
        // reaches here. It stays because [`Transcriber::transcribe`] is public
        // and a caller that went straight to it would otherwise post the audio.
        // Reported as a BACKEND failure rather than as a not-configured one: the
        // setup is fine, which is a different thing to do next.
        let measure = audio.measure_speech();
        if measure.voiced < MIN_SPEECH {
            return Err(TranscriptionError::Backend(not_enough_speech(measure).0));
        }

        // Read at call time, Rust-side, and dropped with this scope — and on
        // a blocking thread, because a keychain read is entitled to prompt and
        // this is an async fn on a shared runtime.
        //
        // `None` here is the KEYLESS backend, and the whole branch is skipped:
        // no keychain call, no prompt, no failure mode. That is the shape the
        // local container needed and the reason this is an `Option<Arc<…>>`
        // rather than a store that is asked and forgiven for answering nothing.
        let secret = match &self.secrets {
            None => None,
            Some(store) => {
                match load_off_runtime(Arc::clone(store), SecretId::VoiceTranscription).await {
                    Ok(Some(secret)) => Some(secret),
                    Ok(None) => {
                        return Err(TranscriptionError::NotConfigured(format!(
                            "no key is stored for {} — add one in Settings → Voice",
                            self.endpoint.host()
                        )));
                    }
                    // The keychain itself failed. PRD #802 M4's rule: "I could
                    // not find out" must not render as "nothing stored", or the
                    // user retypes a key into a store that cannot hold it.
                    Err(error) => return Err(TranscriptionError::NotConfigured(error.public())),
                }
            }
        };

        let Some(client) = self.client.as_ref() else {
            // Fail closed: never a fallback to a client that would re-send the
            // audio to a redirect target.
            return Err(TranscriptionError::Backend(
                "the transcription backend could not start a secure connection".into(),
            ));
        };

        let boundary = boundary();
        let body = multipart_body(audio, self.model.as_str(), &boundary);
        let mut request = client
            .post(self.endpoint.as_str())
            .timeout(TRANSCRIBE_TIMEOUT)
            .header("content-type", content_type(&boundary))
            .body(body);
        // Attached only where there is one to attach. The local container
        // tolerates the header, which is exactly why not sending it has to be
        // structural rather than incidental: a backend that sends an empty
        // `Bearer` to a service that ignores it works until the day it meets
        // one that does not.
        if let Some(secret) = &secret {
            request = request.header("authorization", format!("Bearer {}", secret.expose()));
        }
        let response = request
            .send()
            .await
            .map_err(|error| self.transport_error(&error))?;

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

    /// What a transport failure becomes, which depends on **where the endpoint
    /// points**.
    ///
    /// A connection refused by a hosted service and a connection refused by a
    /// container that is not running are the same `reqwest` error and entirely
    /// different situations: one is an outage to wait out, the other is a
    /// command to run. The product owner asked for this specifically — an
    /// unreachable endpoint must say what to start, not "transcription failed"
    /// — so a loopback endpoint that does not answer is classified as
    /// [`TranscriptionError::NotConfigured`] and carries
    /// [`unreachable_detail`]'s sentence, which the surface renders as an
    /// instruction rather than an error.
    fn transport_error(&self, error: &reqwest::Error) -> TranscriptionError {
        // Keyless AND loopback, not either alone. A keyed endpoint that happens
        // to be on this machine is a gateway the user runs their own way, and a
        // remote endpoint that is down is an outage — neither is fixed by
        // starting the speech container, and telling somebody to run docker at
        // an outage is worse than saying nothing.
        if error.is_connect() && self.secrets.is_none() && self.endpoint.is_loopback() {
            return TranscriptionError::NotConfigured(unreachable_detail(&self.endpoint));
        }
        TranscriptionError::Backend(transport_detail(error))
    }
}

impl Transcriber for HttpTranscriber {
    fn transcribe<'a>(&'a self, audio: &'a Pcm16) -> TranscribeFuture<'a> {
        Box::pin(self.run(audio))
    }

    fn backend_name(&self) -> &'static str {
        self.name
    }
}

/// What a user is told when nothing answers at a speech endpoint on their own
/// machine.
///
/// **Names the command, not the symptom.** The whole point of the keyless
/// default is that nobody has to paste a credential to try voice; the cost of
/// that default is a prerequisite the user has to start, so the one place that
/// cost is met has to carry the fix. The port comes from the endpoint the user
/// actually configured rather than from the preset, because a person who moved
/// it is exactly the person a hardcoded `18000` would mislead.
pub fn unreachable_detail(endpoint: &ServiceUrl) -> String {
    let published = endpoint.port().unwrap_or(18000);
    format!(
        "nothing is listening at {origin} — start the speech service with \
         `docker run -d -p {published}:8000 {LOCAL_SPEECH_IMAGE}` and press Voice again, \
         or pick a hosted service under Settings → Voice",
        origin = endpoint.origin(),
    )
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
    /// Which backend answered — `local`, `remote`, `stub`. Present even when
    /// no call was made, because it names what *would* have answered, which is
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
/// Four rather than two, and the two additions are the point: *a prerequisite
/// is not running* is neither a transcript nor a failure, and neither is
/// *nothing was said*. Rendering either as an error is the mistake — one
/// dressed as a failure teaches people the feature is broken, and the other
/// sends them looking for a fault in a microphone that is working.
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
    /// The configured backend cannot be used yet — nothing is listening at a
    /// loopback endpoint, or a keyed service has no key. **Not a failure**: the
    /// sentence is an instruction naming what to start or what to paste.
    NotConfigured { detail: String, sentence: String },
    /// The segment held no speech, so no backend was called.
    ///
    /// **Not a failure either, and the distinction is the whole of the fix PRD
    /// #802's product owner asked for.** A noise ends a segment
    /// ([`super::Vad::heard_speech`] flips on one frame over the floor) far
    /// more often than a sentence does, and what arrives here is then thirty
    /// seconds of a quiet room. Transcribing it is worse than useless:
    /// whisper-family models emit their captioned-video training artefacts on
    /// near-silence, so the report said the app had heard "Don't forget to
    /// subscribe".
    ///
    /// **It carries a `detail` and it used to carry none**, on the argument
    /// that "there is nothing to diagnose — this is the ordinary outcome of a
    /// quiet room". That argument was wrong in the case that matters. This is
    /// also what a user sees when they *did* speak and the audio did not clear
    /// the gate, and then it is the only thing standing between them and a
    /// feature that looks broken for a reason nobody can name. The `detail` is
    /// the measurement [`not_enough_speech`] renders, and it is a statement
    /// about the AUDIO — never about the device, which is the rule that has not
    /// changed.
    Silent { detail: String, sentence: String },
    /// Speech could not be turned into text.
    Failed { detail: String, sentence: String },
}

impl TranscriptionOutcome {
    pub fn sentence(&self) -> &str {
        match self {
            TranscriptionOutcome::Heard { sentence, .. }
            | TranscriptionOutcome::NotConfigured { sentence, .. }
            | TranscriptionOutcome::Silent { sentence, .. }
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

    // The eligibility gate, in front of EVERY backend rather than inside one of
    // them. It lives here and not in [`HttpTranscriber::run`] because "was
    // anything said" is a question about the audio and not about a transport:
    // a stub, a second HTTP backend or whatever replaces them would each have
    // had to remember to ask it, and the one that forgot would be the one that
    // reported a model's hallucination as a sentence the user said.
    //
    // `transcribe_ms` is `None` for [`VoiceTranscription::transcribe_ms`]'s own
    // rule — no call was made, and a number here would claim a measurement
    // nobody took — while `backend` still names what would have answered.
    let measure = audio.measure_speech();
    if measure.voiced < MIN_SPEECH {
        let (detail, sentence) = not_enough_speech(measure);
        return VoiceTranscription {
            outcome: TranscriptionOutcome::Silent { detail, sentence },
            transcribe_ms: None,
            backend,
            audio_ms,
        };
    }

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
    use crate::secrets::{MemorySecretStore, Secret, SecretErrorKind, ThreadRecordingStore};
    use crate::settings::{
        HOSTED_SPEECH_MODEL, LOCAL_SPEECH_ENDPOINT, LOCAL_SPEECH_MODEL, TranscriptionBackend,
    };
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

    /// A keyed transcriber pointed somewhere unroutable on purpose: reaching
    /// it would be a failure of the test's premise, not a flake.
    fn keyed(secrets: Arc<dyn SecretStore>) -> HttpTranscriber {
        HttpTranscriber::keyed(
            secrets,
            ServiceUrl::parse("https://voice-transcription.invalid/never").expect("valid"),
            ModelId::parse(HOSTED_SPEECH_MODEL).expect("valid"),
        )
    }

    fn stage(backend: TranscriptionBackend) -> TranscriptionSettings {
        TranscriptionSettings {
            backend,
            ..TranscriptionSettings::default()
        }
    }

    #[test]
    fn voice_transcribe_selects_the_backend_the_settings_name() {
        assert_eq!(
            transcriber_for(&stage(TranscriptionBackend::Local), store()).backend_name(),
            "local"
        );
        assert_eq!(
            transcriber_for(&stage(TranscriptionBackend::Remote), store()).backend_name(),
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
        let names: Vec<&str> = [TranscriptionBackend::Local, TranscriptionBackend::Remote]
            .into_iter()
            .map(|backend| transcriber_for(&stage(backend), store()).backend_name())
            .collect();
        assert_eq!(names, vec!["local", "remote"]);
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn voice_transcribe_is_object_safe() {
        // The property the settings choice depends on: the backend is picked at
        // call time, so it has to be storable behind a `dyn`.
        let transcriber: Box<dyn Transcriber> =
            transcriber_for(&stage(TranscriptionBackend::Local), store());
        assert_eq!(transcriber.backend_name(), "local");
    }

    /// The module's own invariant, asserted rather than written down: a
    /// keyless backend pointed off this machine makes **no request at all**.
    ///
    /// `TranscriptionBackend::Local` means *no `Authorization` header*, and
    /// `ServiceUrl` accepts any `https://host` — so the two together are a
    /// pairing nothing else checks, and the reachable version of it is a user
    /// picking the keyless backend and then editing the Endpoint field to a
    /// hosted service. Their captured WAV is then POSTed to a third party with
    /// no credential on it. `transport_error` gating the docker hint on
    /// `is_loopback` reads as if it covered this; it runs after the upload.
    ///
    /// The store is a recorder for the same reason
    /// [`voice_transcribe_local_reads_no_secret_at_all`] uses one: the refusal
    /// must not quietly become the KEYED path, which would send a key to
    /// somewhere the user picked a keyless backend to avoid.
    #[tokio::test]
    async fn voice_transcribe_keyless_refuses_an_endpoint_off_this_machine() {
        assert!(
            HttpTranscriber::keyless(
                ServiceUrl::parse("https://api.openai.com/v1/audio/transcriptions").expect("valid"),
                ModelId::parse(HOSTED_SPEECH_MODEL).expect("valid"),
            )
            .is_err(),
            "a keyless transcriber may not be built for another host"
        );

        let recorder = Arc::new(ThreadRecordingStore::new());
        let settings = TranscriptionSettings {
            backend: TranscriptionBackend::Local,
            endpoint: ServiceUrl::parse("https://api.openai.com/v1/audio/transcriptions")
                .expect("valid"),
            model: ModelId::parse(HOSTED_SPEECH_MODEL).expect("valid"),
        };
        let transcriber = transcriber_for(&settings, Arc::clone(&recorder) as Arc<dyn SecretStore>);
        // Still `local` to the surface: the vocabulary is the settings token,
        // so the report names the backend the user chose.
        assert_eq!(transcriber.backend_name(), "local");

        let error = transcriber
            .transcribe(&audio(16))
            .await
            .expect_err("the pairing is refused");
        assert!(
            matches!(error, TranscriptionError::NotConfigured(_)),
            "a misconfiguration is an instruction, not a failure: {error:?}"
        );
        assert_eq!(error.detail(), crate::settings::KEYLESS_OFF_MACHINE);
        assert_eq!(
            recorder.read_on(),
            None,
            "the refusal must not fall back to the keyed path"
        );
    }

    // -- the keyless local backend -----------------------------------------

    /// The keyless path is structural, not a lookup that happens to find
    /// nothing: `transcriber_for` drops the store for a local backend, so this
    /// records **zero** reads against one that counts them.
    ///
    /// It is the difference the module docs claim, asserted rather than argued
    /// — a backend that asked and forgave an empty answer would pass every
    /// other test here and still prompt a macOS keychain on every utterance.
    #[tokio::test]
    async fn voice_transcribe_local_reads_no_secret_at_all() {
        let recorder = Arc::new(ThreadRecordingStore::new());
        let settings = TranscriptionSettings {
            backend: TranscriptionBackend::Local,
            endpoint: ServiceUrl::parse("http://127.0.0.1:1/v1/audio/transcriptions")
                .expect("valid"),
            ..TranscriptionSettings::default()
        };
        let transcriber = transcriber_for(&settings, Arc::clone(&recorder) as Arc<dyn SecretStore>);
        // The request cannot succeed — nothing is listening on port 1 — but
        // whether a secret was read is decided before the socket.
        let _ = transcriber.transcribe(&audio(16_000)).await;
        assert_eq!(
            recorder.read_on(),
            None,
            "the keyless backend consulted the keychain"
        );
    }

    /// The unreachable **local** endpoint is a prerequisite, not a failure —
    /// the product owner asked for this specifically: it must say what to
    /// start.
    ///
    /// Driven against a port nothing listens on, which is a connect refused on
    /// loopback and costs no network. This is the one test here that opens a
    /// socket, and it opens it to a closed port on this machine.
    #[tokio::test]
    async fn voice_transcribe_an_unreachable_local_endpoint_says_what_to_start() {
        let settings = TranscriptionSettings {
            backend: TranscriptionBackend::Local,
            // Port 1 on loopback: privileged, unbound, refused immediately.
            endpoint: ServiceUrl::parse("http://127.0.0.1:1/v1/audio/transcriptions")
                .expect("valid"),
            ..TranscriptionSettings::default()
        };
        let result =
            handle_audio(transcriber_for(&settings, store()).as_ref(), &audio(16_000)).await;

        assert!(
            matches!(&result.outcome, TranscriptionOutcome::NotConfigured { .. }),
            "an unreachable container rendered as a failure: {:?}",
            result.outcome
        );
        let sentence = result.sentence();
        assert!(
            sentence.contains("nothing is listening at http://127.0.0.1:1"),
            "{sentence}"
        );
        assert!(sentence.contains("docker run"), "{sentence}");
        assert!(sentence.contains(LOCAL_SPEECH_IMAGE), "{sentence}");
        // The whole point of the classification: it must not read as a break.
        assert!(
            !sentence.contains("Could not turn that into text"),
            "{sentence}"
        );
    }

    /// The same sentence, built directly, so the words the owner asked to see
    /// are pinned rather than inferred — including the PORT coming from the
    /// user's own endpoint rather than from the preset.
    #[test]
    fn voice_transcribe_the_unreachable_sentence_names_the_users_own_port() {
        let moved =
            ServiceUrl::parse("http://127.0.0.1:9123/v1/audio/transcriptions").expect("valid");
        let detail = unreachable_detail(&moved);
        assert!(
            detail.contains("nothing is listening at http://127.0.0.1:9123"),
            "{detail}"
        );
        assert!(detail.contains("-p 9123:8000"), "{detail}");
        assert!(detail.contains("press Voice again"), "{detail}");
    }

    /// A KEYED endpoint that refuses a connection is an outage or a gateway the
    /// user runs their own way — never a missing speech container — so it must
    /// not be told to run docker, even when it is on this machine.
    ///
    /// Driven against the same refused loopback port as the test above, so the
    /// only thing that differs is which backend was chosen.
    #[tokio::test]
    async fn voice_transcribe_a_keyed_endpoint_is_never_a_docker_instruction() {
        let keyed = store();
        keyed
            .store(SecretId::VoiceTranscription, &Secret::new("sk-test"))
            .expect("stores");
        let settings = TranscriptionSettings {
            backend: TranscriptionBackend::Remote,
            endpoint: ServiceUrl::parse("http://127.0.0.1:1/v1/audio/transcriptions")
                .expect("valid"),
            ..TranscriptionSettings::default()
        };
        let result = handle_audio(
            transcriber_for(&settings, keyed as Arc<dyn SecretStore>).as_ref(),
            &audio(16_000),
        )
        .await;
        assert!(
            !result.sentence().contains("docker run"),
            "a keyed endpoint was reported as a missing container: {}",
            result.sentence()
        );
        assert!(
            matches!(&result.outcome, TranscriptionOutcome::Failed { .. }),
            "{:?}",
            result.outcome
        );
    }

    // -- the outcome rendering ---------------------------------------------

    #[tokio::test]
    async fn voice_transcribe_reports_what_was_heard() {
        let result = handle_audio(
            &StubTranscriber::hearing("show me the tester"),
            &audio(16_000),
        )
        .await;
        assert!(result.outcome.is_heard());
        assert_eq!(
            result.transcript().map(Transcript::text),
            Some("show me the tester")
        );
        assert_eq!(result.backend, "stub");
        assert_eq!(result.audio_ms, 1_000);
        assert!(result.transcribe_ms.is_some());
    }

    #[tokio::test]
    async fn voice_transcribe_a_backend_failure_is_its_own_outcome() {
        let result = handle_audio(
            &StubTranscriber::failing(TranscriptionError::Backend("it timed out".into())),
            &audio(16_000),
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
            &audio(16_000),
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
            detail: "nothing is listening".into(),
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
        multipart_body(audio, HOSTED_SPEECH_MODEL, "TESTBOUNDARY")
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
        let transcriber = keyed(store());
        let error = transcriber
            .transcribe(&audio(16_000))
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, TranscriptionError::NotConfigured(detail) if detail.contains("no key is stored")),
            "got {error:?}"
        );
        // Named by its HOST, so a user holding several keys knows which one is
        // wanted — the old wording said "the remote transcription backend".
        assert!(
            error.detail().contains("voice-transcription.invalid"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn voice_transcribe_no_key_renders_as_an_instruction_not_a_break() {
        let transcriber = keyed(store());
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
        let transcriber = keyed(Arc::new(store));
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

    /// Scenario: transcribe one buffer and note which thread the keychain was
    /// read on. It is not the one the transcriber is running on.
    ///
    /// The sibling of `voice_remote_reads_the_keychain_off_the_runtime`, and
    /// the same regression: `run` read the store inline in its own `async
    /// fn`, so an unlock prompt parked a shared runtime worker.
    #[tokio::test]
    async fn voice_transcribe_reads_the_keychain_off_the_runtime() {
        let store = Arc::new(ThreadRecordingStore::new());
        let transcriber = keyed(Arc::clone(&store) as Arc<dyn SecretStore>);
        let error = transcriber
            .transcribe(&audio(16_000))
            .await
            .expect_err("no key is stored");
        assert!(
            matches!(&error, TranscriptionError::NotConfigured(detail) if detail.contains("no key is stored")),
            "got {error:?}"
        );
        assert_ne!(
            store.read_on().expect("the store was read"),
            std::thread::current().id(),
            "the keychain read ran on the runtime thread transcribing the audio"
        );
    }

    #[tokio::test]
    async fn voice_transcribe_silence_never_reaches_the_backend() {
        // Short-circuited BEFORE the credential is read, so a muted microphone
        // costs neither a keychain prompt nor a round trip. The store here
        // would panic the assertion below if it were consulted, because a
        // missing key reports a different sentence.
        let transcriber = keyed(store());
        let error = transcriber
            .transcribe(&Pcm16::new(vec![0; 16_000]))
            .await
            .expect_err("fails");
        // Digital zero, so the refusal is the LEVEL one and it names the floor
        // it measured against rather than asserting the room was quiet.
        assert!(
            matches!(&error, TranscriptionError::Backend(detail) if detail.contains("too quiet")),
            "got {error:?}"
        );
        assert!(
            error.detail().contains(&SPEECH_FLOOR.to_string()),
            "the refusal names no floor to compare against: {}",
            error.detail()
        );
        let error = transcriber
            .transcribe(&Pcm16::new(Vec::new()))
            .await
            .expect_err("fails");
        assert!(error.detail().contains("too quiet"), "{}", error.detail());
    }

    /// The defect PRD #802's product owner met: a buffer of room tone with one
    /// impulse in it reached whisper, which answered with a training artefact.
    ///
    /// One sample at full scale clears `Pcm16::is_silent` — `all()` needs every
    /// sample under the floor — which is why the old guard let this through.
    #[tokio::test]
    async fn voice_transcribe_near_silence_with_one_loud_sample_is_never_sent() {
        let mut samples = vec![0i16; 16_000];
        samples[8_000] = i16::MAX;
        let audio = Pcm16::new(samples);
        assert!(
            !audio.is_silent(),
            "the fixture must be one the OLD guard passed, or this proves nothing"
        );

        let error = keyed(store())
            .transcribe(&audio)
            .await
            .expect_err("a room with one tap in it is not an utterance");
        // The LENGTH branch, not the level one: a full-scale sample puts its
        // own frame's RMS well over `SPEECH_FLOOR`, so the honest refusal is
        // that there was 20 ms of it and not that the room was quiet.
        assert!(
            error.detail().contains("of speech inside the loudest"),
            "got {}",
            error.detail()
        );
        assert!(
            !error.detail().contains("too quiet"),
            "a tap at full scale reported as too quiet: {}",
            error.detail()
        );
    }

    /// The same buffer through the gate every backend sits behind: no call, no
    /// latency claimed, and a sentence that says nothing was said rather than
    /// that something failed.
    #[tokio::test]
    async fn voice_transcribe_a_segment_with_no_speech_in_it_is_its_own_outcome() {
        let mut samples = vec![0i16; 16_000];
        samples[8_000] = i16::MAX;
        // A transcriber that would PANIC the assertions below if it were asked,
        // because "the tester" is neither the sentence nor the kind expected.
        let result = handle_audio(
            &StubTranscriber::hearing("Don't forget to subscribe"),
            &Pcm16::new(samples),
        )
        .await;

        assert!(
            matches!(&result.outcome, TranscriptionOutcome::Silent { .. }),
            "{:?}",
            result.outcome
        );
        assert!(
            !result.outcome.is_heard(),
            "nothing goes on to the resolver"
        );
        assert_eq!(result.transcript(), None);
        // The sentence a user actually reads. It used to be "Nothing was
        // said", which tells somebody who DID speak that they imagined it; the
        // measurement is what lets them tell a quiet microphone from a short
        // utterance from a bug.
        assert!(
            result
                .sentence()
                .starts_with("I did not hear enough to transcribe"),
            "{}",
            result.sentence()
        );
        assert!(
            result.sentence().contains("still listening"),
            "the refusal must not read as a stop: {}",
            result.sentence()
        );
        assert!(
            result
                .sentence()
                .contains("20 ms of speech inside the loudest 200 ms"),
            "the refusal carries no measurement: {}",
            result.sentence()
        );
        assert!(
            !result.sentence().contains("Nothing was said"),
            "{}",
            result.sentence()
        );
        // Not a failure and not a fault in the user's hardware, which is the
        // whole reason this is a fourth variant rather than a `Failed`.
        assert!(
            !result.sentence().contains("Could not"),
            "{}",
            result.sentence()
        );
        assert!(
            !result.sentence().contains("microphone"),
            "{}",
            result.sentence()
        );
        assert_eq!(result.transcribe_ms, None, "no backend was called");
        assert_eq!(
            result.backend, "stub",
            "it still names what would have answered"
        );
        assert_eq!(result.audio_ms, 1_000);
    }

    /// The other direction, which is what stops the guard above being a mute
    /// button: a buffer with somebody speaking in it still reaches the backend.
    #[tokio::test]
    async fn voice_transcribe_real_speech_still_reaches_the_backend() {
        let result = handle_audio(
            &StubTranscriber::hearing("go back"),
            // Loud enough to be speech, and only 200 ms of it (3 200 samples at
            // 16 kHz) — a bare "back" on the overview is a real command and
            // must not be refused.
            &audio(3_200),
        )
        .await;

        assert!(result.outcome.is_heard(), "{:?}", result.outcome);
        assert_eq!(result.transcript().map(Transcript::text), Some("go back"));
        assert!(result.transcribe_ms.is_some(), "the backend was called");
    }

    #[test]
    fn voice_transcribe_silence_serializes_a_kind_the_webview_reads() {
        let (detail, sentence) = not_enough_speech(SpeechMeasure {
            voiced: Duration::from_millis(40),
            peak_rms: 5_000,
        });
        let silent = TranscriptionOutcome::Silent { detail, sentence };
        let json = serde_json::to_value(&silent).expect("serializes");
        assert_eq!(json["kind"], "silent");
        // `detail` is carried and the webview's union declares it. It used to
        // be absent on the argument that there was nothing to diagnose — see
        // `TranscriptionOutcome::Silent` for why that stopped being true.
        assert_eq!(
            json["detail"],
            "only 40 ms of speech inside the loudest 200 ms, where 120 ms is needed"
        );
        assert!(
            json["sentence"]
                .as_str()
                .expect("a sentence")
                .starts_with("I did not hear enough"),
            "{json}"
        );
    }

    /// The two refusals, pinned as the two different things a user has to DO.
    ///
    /// The branch is on whether anything crossed [`SPEECH_FLOOR`], because that
    /// is what separates "your input is too quiet for this floor" from "that
    /// was speech and there was not enough of it" — and PRD #802's product
    /// owner had neither number.
    #[test]
    fn voice_transcribe_the_refusal_names_which_of_the_two_causes_it_was() {
        let (detail, sentence) = not_enough_speech(SpeechMeasure {
            voiced: Duration::ZERO,
            peak_rms: 410,
        });
        assert_eq!(
            detail,
            "too quiet — the loudest moment reached 410 where 600 counts as speech"
        );
        assert!(
            sentence.contains("Move closer or turn the input up"),
            "{sentence}"
        );
        assert!(sentence.contains("still listening"), "{sentence}");

        let (detail, sentence) = not_enough_speech(SpeechMeasure {
            voiced: Duration::from_millis(100),
            peak_rms: 5_000,
        });
        assert_eq!(
            detail,
            "only 100 ms of speech inside the loudest 200 ms, where 120 ms is needed"
        );
        assert!(sentence.contains("Say that again"), "{sentence}");

        // Exactly at the floor is the LENGTH branch, not the level one: a frame
        // at `SPEECH_FLOOR` counts as speech everywhere else in this module.
        let (detail, _) = not_enough_speech(SpeechMeasure {
            voiced: Duration::ZERO,
            peak_rms: SPEECH_FLOOR,
        });
        assert!(detail.starts_with("only 0 ms"), "{detail}");
    }

    /// Neither refusal blames the user's hardware, whichever branch it took.
    ///
    /// The rule `TranscriptionOutcome::Silent` exists to keep: the backstop's
    /// detail used to read "the microphone heard nothing", which sent PRD
    /// #802's product owner looking for a fault in a device that was working.
    #[test]
    fn voice_transcribe_neither_refusal_blames_the_device() {
        for measure in [
            SpeechMeasure {
                voiced: Duration::ZERO,
                peak_rms: 0,
            },
            SpeechMeasure {
                voiced: Duration::from_millis(80),
                peak_rms: 9_000,
            },
        ] {
            let (detail, sentence) = not_enough_speech(measure);
            for text in [&detail, &sentence] {
                for banned in ["microphone", "Nothing was said", "failed", "error"] {
                    assert!(!text.contains(banned), "`{banned}` in: {text}");
                }
            }
        }
    }

    /// Scenario: the built code, on this build's own default settings, posts a
    /// real utterance to a real speech container on loopback and gets words
    /// back.
    ///
    /// **The one test here that reaches a running service, and it is
    /// `#[ignore]`d for that reason** — every other test in this module is
    /// bytes in, bytes out, which is PRD #802 M5's rule and what keeps the
    /// merge-blocking tier free of sockets. This one exists because a suite of
    /// stubs passes identically whether or not the local backend can actually
    /// talk to a container: it asserts the whole seam the settings name, from
    /// `TranscriptionSettings` through `transcriber_for` to a parsed
    /// [`Transcript`], with no credential anywhere.
    ///
    /// To run it, start the container and hand it an utterance as raw
    /// 16 kHz mono little-endian i16 — which is exactly what
    /// [`super::capture`] produces, so no WAV parser is needed here:
    ///
    /// ```text
    /// docker run -d -p 18000:8000 ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu
    /// espeak-ng -v en-us -w say.wav "show me the tester"
    /// ffmpeg -i say.wav -ac 1 -ar 16000 -f s16le utterance.pcm
    /// DAD_LOCAL_SPEECH_PCM=$PWD/utterance.pcm \
    ///   cargo test -p dot-agent-deck-desktop --lib \
    ///   voice_transcribe_local_container -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs a speech container on loopback; the doc comment has the two commands"]
    async fn voice_transcribe_local_container_transcribes_a_real_utterance() {
        let path = std::env::var("DAD_LOCAL_SPEECH_PCM").expect(
            "set DAD_LOCAL_SPEECH_PCM to raw 16 kHz mono LE i16 audio — see the doc comment",
        );
        let raw = std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        let samples: Vec<i16> = raw
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let audio = Pcm16::new(samples);
        assert!(
            !audio.is_silent(),
            "{path} is silence; nothing would be sent"
        );

        // This build's own defaults, untouched: the keyless local backend at
        // the preset loopback endpoint, asking for the preset model.
        let settings = TranscriptionSettings::default();
        assert_eq!(settings.backend, TranscriptionBackend::Local);
        let transcriber = transcriber_for(&settings, store());
        let result = handle_audio(transcriber.as_ref(), &audio).await;

        println!(
            "backend={} audio={}ms transcribe={:?}ms outcome={:?}",
            result.backend, result.audio_ms, result.transcribe_ms, result.outcome
        );
        let transcript = result
            .transcript()
            .unwrap_or_else(|| panic!("no transcript: {}", result.sentence()));
        assert!(
            !transcript.text().trim().is_empty(),
            "the container answered with no words"
        );
        assert_eq!(result.backend, "local");
    }

    #[test]
    fn voice_transcribe_names_itself_for_the_surface() {
        assert_eq!(keyed(store()).backend_name(), "remote");
        assert_eq!(
            HttpTranscriber::keyless(
                ServiceUrl::parse(LOCAL_SPEECH_ENDPOINT).expect("valid"),
                ModelId::parse(LOCAL_SPEECH_MODEL).expect("valid"),
            )
            .expect("the preset endpoint is loopback")
            .backend_name(),
            "local"
        );
    }
}
