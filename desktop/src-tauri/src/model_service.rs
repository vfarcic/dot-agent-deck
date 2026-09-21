//! Where a model service is, which model to ask it for, and how much answer to
//! allow — as validating newtypes rather than as `String`s and a bare integer.
//!
//! PRD #802 made both voice stages user-selectable, which is the moment the
//! endpoint and the model stop being `const`s this build picked and become
//! values a user can write into `desktop.toml`. `ALLOWED_FIELD_TYPES` in
//! `xtask/linkage-check/src/desktop_settings_secrets.rs` refuses a `String` in a
//! settings struct, and that refusal is the friction working: a free-text field
//! beside a stage whose credential lives in the OS keychain is exactly the shape
//! a credential ends up pasted into. So each of these says, in its constructor
//! and in its `Deserialize` alike, what it can and cannot be.
//!
//! # The claims, narrowly
//!
//! [`ServiceUrl`] is an absolute `http`/`https` URL with no userinfo, at most
//! [`MAX_SERVICE_URL_BYTES`], every byte of it printable ASCII with no space.
//! **`http` is permitted only for a loopback host**, so the one keyless backend
//! PRD #802 ships — a container on `127.0.0.1` — is expressible and a plaintext
//! hop to somebody else's box is not. What that makes unrepresentable: a
//! `user:password@host` authority, which is the one place a URL carries a
//! credential; and any multi-line or whitespace-bearing blob, which is the shape
//! of a PEM block or a wrapped token. It is **not** a claim that no single-line
//! secret could be typed into a path segment — what rules that out is that the
//! value is handed to `reqwest` as a destination and reaches no authentication
//! surface as text, the same argument `KeyPath` makes about `ssh -i`.
//!
//! [`ModelId`] is a non-empty ASCII identifier of at most [`MAX_MODEL_ID_BYTES`]
//! drawn from [`MODEL_ID_CHARS`] plus alphanumerics — so no control byte, no
//! whitespace, no non-ASCII byte, and nothing long enough to be a payload.
//!
//! [`TokenCeiling`] is an integer between [`MIN_TOKEN_CEILING`] and
//! [`MAX_TOKEN_CEILING`], and it is the one of the three whose newtype is **not**
//! about credentials at all: `u32` is already on `ALLOWED_FIELD_TYPES`, so a
//! bare integer would have passed that guard and then happily held `0`. What
//! the type buys is the range — a floor under which no answer can be written
//! and a cap over which one utterance stops being a routing decision and starts
//! being a bill.
//!
//! # A rejected value is an ERROR, not a fold
//!
//! The `[voice]` *token* enums fold an unrecognised value to their default
//! (`VoiceToken::from_str_lossy`), because a newer build may name a backend this
//! one has never heard of and losing the whole document over it is worse. These
//! three deliberately do not, and the reason is not symmetry with
//! `crate::settings::EndpointId` — it is that **folding an endpoint is not a
//! safe default**. Fold a refused `endpoint` to the preset and a user who wrote
//! a URL pointing at their own machine has their voice uploaded to a hosted
//! service instead, silently. An error puts *that section* on defaults **with a
//! diagnostic the settings surface shows**, which is the only version of that
//! outcome the user can act on.
//!
//! **"That section", and it used to say "the document", which was true and was
//! the defect.** `toml_edit::de::from_str` is all-or-nothing, so one endpoint
//! refused here took the user's `[endpoints]` — their remote decks — their
//! appearance and their zoom with it, for a value typed in a section that has
//! nothing to do with any of them. The cost of erring rather than folding is
//! meant to be a re-typed endpoint, not a fleet that is not there any more.
//! `crate::settings::sections_this_build_can_read` is what narrows it to the
//! section the refused value is in; nothing about the choice above changed.
//!
//! Neither error message quotes the offending value, for
//! `SettingsDocumentProblem`'s reason (issue #827): the messages describe the
//! rule that was broken and the bound that was crossed, and never a byte of the
//! document.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The longest endpoint this build will accept.
///
/// 2048 is the de-facto interoperable URL ceiling (the oldest and smallest
/// limit any common HTTP stack imposes), so it refuses nothing a real endpoint
/// needs while making a payload-shaped value impossible. It bounds what this
/// build will **hold**, which is a narrower claim than
/// [`crate::settings::MAX_VOICE_TOKEN_BYTES`]'s: that one is judged on borrowed
/// input before anything allocates, and this one cannot be, because both TOML
/// and JSON hand a deserializer an already-decoded string. What it still buys
/// is that no value past this length reaches the document, the panel or a
/// request.
pub const MAX_SERVICE_URL_BYTES: usize = 2048;

/// The longest model identifier this build will accept.
///
/// The longest real one measured for PRD #802 is 38 bytes
/// (`Systran/faster-whisper-tiny.en` is 30, `deepdml/faster-whisper-large-v3-turbo-ct2`
/// is 40). 128 leaves a comfortable multiple of that while staying far below
/// anything that could carry a key.
pub const MAX_MODEL_ID_BYTES: usize = 128;

/// The punctuation a model identifier may use, beside ASCII alphanumerics.
///
/// Drawn from the identifiers the three ecosystems PRD #802 touches actually
/// publish: `whisper-1` and `claude-haiku-4-5` need `-`; `Systran/faster-whisper-tiny.en`
/// needs `/` and `.`; `_` and `+` appear in Hugging Face repository names; `:`
/// is how a locally served model names its tag (`llama3.1:8b`). Everything else
/// — every control byte, every whitespace byte, every non-ASCII byte — is
/// refused.
pub const MODEL_ID_CHARS: &str = "./-_:+";

/// Why a [`ServiceUrl`] was refused.
///
/// One variant per rule rather than one string, so the panel and the tests name
/// the same rule and a reason cannot drift between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceUrlError {
    /// Nothing, or only whitespace.
    Empty,
    /// Longer than [`MAX_SERVICE_URL_BYTES`].
    TooLong,
    /// A byte outside printable ASCII, or a space.
    Charset,
    /// Not a URL this build can read at all.
    Unparseable,
    /// A scheme other than `http` or `https`.
    Scheme,
    /// A `user@` or `user:password@` authority.
    Credentials,
    /// No host — `http:///v1`, or a scheme-only string.
    NoHost,
    /// Plaintext `http` to a host that is not loopback.
    InsecureNonLoopback,
}

impl ServiceUrlError {
    /// One complete sentence, naming the rule and never the value.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Empty => "an endpoint is required",
            Self::TooLong => "an endpoint is at most 2048 bytes",
            Self::Charset => {
                "an endpoint may hold only printable ASCII, with no spaces — percent-encode anything else"
            }
            Self::Unparseable => "an endpoint must be an absolute URL, such as https://host/path",
            Self::Scheme => "an endpoint must use http or https",
            Self::Credentials => {
                "an endpoint may not carry a user or password — keys belong in the OS keychain"
            }
            Self::NoHost => "an endpoint must name a host",
            Self::InsecureNonLoopback => {
                "http is allowed only for a loopback endpoint; use https to reach another machine"
            }
        }
    }
}

impl fmt::Display for ServiceUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for ServiceUrlError {}

/// Where a request goes: an absolute `http`/`https` URL carrying no credential.
///
/// Holds the **trimmed input** rather than a re-serialised parse, so a document
/// round-trips byte for byte and a user's endpoint reads back the way they wrote
/// it. That is safe because the charset check runs on the raw bytes *before* the
/// parse: `Url::parse` strips ASCII tab and newline per the URL standard, so a
/// value whose parse differs from its text cannot get this far to begin with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceUrl(String);

impl ServiceUrl {
    /// Validate one endpoint, applying every rule in [`ServiceUrlError`]'s
    /// order: cheap byte checks first, then the parse, then the parsed
    /// predicates.
    pub fn parse(raw: &str) -> Result<Self, ServiceUrlError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ServiceUrlError::Empty);
        }
        // Bounded before the parse, which is the expensive half.
        if trimmed.len() > MAX_SERVICE_URL_BYTES {
            return Err(ServiceUrlError::TooLong);
        }
        // `is_ascii_graphic` is 0x21..=0x7E: printable, and deliberately NOT
        // including 0x20, so a space is refused rather than percent-encoded on
        // the user's behalf. This is also what makes holding the raw string
        // sound — see the type's doc comment.
        if !trimmed.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ServiceUrlError::Charset);
        }
        // `reqwest::Url` is `url::Url`, re-exported — the same parser reqwest
        // will run on this string when the request is made, so what is checked
        // here is what is sent rather than an approximation of it.
        let url = reqwest::Url::parse(trimmed).map_err(|_| ServiceUrlError::Unparseable)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ServiceUrlError::Scheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ServiceUrlError::Credentials);
        }
        if url.host_str().is_none() {
            return Err(ServiceUrlError::NoHost);
        }
        if url.scheme() == "http" && !host_is_loopback(&url) {
            return Err(ServiceUrlError::InsecureNonLoopback);
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this points at this machine.
    ///
    /// What the transcription backend reads to decide between *the service
    /// refused you* and *the container is not running* — two situations with
    /// entirely different things to do next.
    pub fn is_loopback(&self) -> bool {
        reqwest::Url::parse(&self.0).is_ok_and(|url| host_is_loopback(&url))
    }

    /// The port a request to this endpoint connects to, with the scheme's
    /// default filled in — so an endpoint written without one still names the
    /// port a user has to publish.
    pub fn port(&self) -> Option<u16> {
        reqwest::Url::parse(&self.0)
            .ok()
            .and_then(|url| url.port_or_known_default())
    }

    /// The host alone, for a sentence that names WHICH service a key is for.
    ///
    /// "no key is stored for the remote command backend" told a person holding
    /// three API keys nothing about which one to paste; `api.anthropic.com` is
    /// the word they recognise, because they either typed it or picked the
    /// preset that carries it.
    pub fn host(&self) -> String {
        match reqwest::Url::parse(&self.0) {
            Ok(url) => url.host_str().unwrap_or(&self.0).to_string(),
            Err(_) => self.0.clone(),
        }
    }

    /// `scheme://host:port`, for a sentence that names where nothing answered.
    ///
    /// The path is dropped deliberately: what the user starts is a server on a
    /// port, and the path is this build's business rather than theirs.
    pub fn origin(&self) -> String {
        match reqwest::Url::parse(&self.0) {
            Ok(url) => match (url.host_str(), url.port()) {
                (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
                (Some(host), None) => format!("{}://{host}", url.scheme()),
                (None, _) => self.0.clone(),
            },
            Err(_) => self.0.clone(),
        }
    }
}

/// Whether a parsed URL's host is this machine.
///
/// Written against `host_str` rather than `Url::host`, so the `url` crate does
/// not have to become a direct dependency of this crate for one enum. IPv6
/// arrives bracketed — `[::1]` — which is why the brackets are stripped before
/// the address parse.
///
/// **A name other than `localhost` is not resolved**, and that is deliberate:
/// resolution is a network operation whose answer can change between the check
/// and the request, so a rule built on it would be one this type cannot keep.
/// The cost is that a user with `mybox.local` pointing at `127.0.0.1` writes
/// `https` or writes the address.
fn host_is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

impl fmt::Display for ServiceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ServiceUrl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ServiceUrl {
    /// Runs the constructor's own check, so a hand-edited `desktop.toml` cannot
    /// smuggle past what the settings panel applies.
    ///
    /// Takes a `String` and not a `&'de str`: `toml_edit`'s deserializer hands
    /// out owned strings, so the borrowed spelling compiles and then fails
    /// **at run time on the TOML path only** — which is the path the document
    /// actually arrives on. Measured, not reasoned about: it reported
    /// `line 8, column 12 could not be read as settings` on a document whose
    /// endpoint was perfectly valid, while every JSON test went on passing.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(|error| serde::de::Error::custom(error.detail()))
    }
}

/// Why a [`ModelId`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelIdError {
    /// Nothing, or only whitespace.
    Empty,
    /// Longer than [`MAX_MODEL_ID_BYTES`].
    TooLong,
    /// A byte outside alphanumerics and [`MODEL_ID_CHARS`] — which includes
    /// every control byte, every whitespace byte and every non-ASCII byte.
    Charset,
}

impl ModelIdError {
    /// One complete sentence, naming the rule and never the value.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Empty => "a model is required",
            Self::TooLong => "a model identifier is at most 128 bytes",
            Self::Charset => "a model identifier may hold only letters, digits and . / - _ : +",
        }
    }
}

impl fmt::Display for ModelIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for ModelIdError {}

/// Which model a service is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelId(String);

impl ModelId {
    pub fn parse(raw: &str) -> Result<Self, ModelIdError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ModelIdError::Empty);
        }
        if trimmed.len() > MAX_MODEL_ID_BYTES {
            return Err(ModelIdError::TooLong);
        }
        if !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || MODEL_ID_CHARS.contains(ch))
        {
            return Err(ModelIdError::Charset);
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ModelId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModelId {
    /// A `String` rather than a `&'de str`, for [`ServiceUrl`]'s measured
    /// reason.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(|error| serde::de::Error::custom(error.detail()))
    }
}

/// The smallest answer ceiling this build will accept.
///
/// A ceiling below the shortest answer the feature can produce is a setting
/// whose only effect is permanent truncation, so the floor is drawn above the
/// largest *complete* answer PRD #802 has measured: 58 completion tokens on the
/// Anthropic dialect and 28 on the OpenAI one with reasoning suppressed. 64 is
/// the next power of two above both. It is **not** a claim that 64 is enough
/// for every model — a reasoning model that is not told to stop thinking will
/// spend far more than that before it writes anything — only that nothing below
/// it can ever work, which is the question a floor answers.
pub const MIN_TOKEN_CEILING: u32 = 64;

/// The largest answer ceiling this build will accept.
///
/// The value bounds what the provider will generate and therefore what one
/// utterance can cost, so an unbounded field is a way to turn a routing
/// decision into a runaway bill. The largest response measured for PRD #802 is
/// **796 completion tokens** — `gpt-5-mini` at provider-default reasoning — so
/// this is roughly forty times the worst observed case, which leaves room for a
/// model that thinks harder than any measured here while keeping a typo three
/// digits short of a surprise.
pub const MAX_TOKEN_CEILING: u32 = 32_768;

/// The ceiling one command request carries when the document says nothing.
///
/// **4096, and the number it replaced was 256** — a constant sized from the
/// Anthropic preset's 33–58-token answers, which was correct for that model and
/// wrong for the field it became. `max_completion_tokens` counts **reasoning**
/// tokens as well as written ones, so any model that reasons without being told
/// not to spends the whole ceiling before it emits a character and the reply
/// comes back `finish_reason: "length"`. Measured on `gpt-5-mini` against the
/// 24 phrase fixtures: at 256 the sweep scored **18/24**, and all six failures
/// had spent exactly 256 completion tokens, every one of them reasoning.
///
/// **A ceiling is not a reservation.** It costs nothing unless it is used, so
/// the number to pick is one that clears every model a user might point the
/// generic `openai_compatible` endpoint at, not one sized to this build's own
/// preset. 1024 was considered and rejected: it clears the 796-token worst case
/// measured here by 228 tokens, which is a margin from a single sweep rather
/// than a cross-provider ceiling.
pub const DEFAULT_TOKEN_CEILING: u32 = 4096;

/// Why a [`TokenCeiling`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCeilingError {
    /// Below [`MIN_TOKEN_CEILING`], including zero and every negative number a
    /// hand-edited document can hold.
    TooSmall,
    /// Above [`MAX_TOKEN_CEILING`].
    TooLarge,
}

impl TokenCeilingError {
    /// One complete sentence, naming the rule and never the value.
    pub fn detail(self) -> &'static str {
        match self {
            Self::TooSmall => "an answer ceiling is at least 64 tokens",
            Self::TooLarge => "an answer ceiling is at most 32768 tokens",
        }
    }
}

impl fmt::Display for TokenCeilingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for TokenCeilingError {}

/// How many tokens one answer may cost, reasoning included.
///
/// A newtype over `u32` rather than a bare one, and the reason is the **range**
/// rather than the `ALLOWED_FIELD_TYPES` guard — `u32` is on that list already,
/// so a raw integer would have passed it. What a raw integer would not have is
/// a deserializer: `max_tokens = 0` and `max_tokens = 4000000000` are both
/// perfectly good `u32`s, and the first is a stage that can never answer while
/// the second is a bill. The bounds are [`MIN_TOKEN_CEILING`] and
/// [`MAX_TOKEN_CEILING`], checked in the constructor and again in
/// `Deserialize`, so a hand-edited `desktop.toml` cannot smuggle past what the
/// settings panel applies.
///
/// **Refused rather than clamped**, for [`ServiceUrl`]'s reason: a value
/// silently moved is a setting the user believes they have. A refusal puts the
/// `[voice]` section on its defaults with a diagnostic the settings surface
/// shows, and `crate::settings::sections_this_build_can_read` is what keeps the
/// rest of the document.
///
/// It is one value spelled two ways on the wire — `max_tokens` on the Anthropic
/// dialect and `max_completion_tokens` on the OpenAI-compatible one — because
/// it is the same knob and the two protocols disagree only about its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenCeiling(u32);

impl TokenCeiling {
    /// Validate one ceiling.
    ///
    /// Takes an `i64` because that is what both formats hand a deserializer for
    /// an integer, and because a negative number is a value this type has to
    /// have a *sentence* about rather than a type error from serde naming a
    /// Rust primitive the user has never heard of.
    pub fn parse(raw: i64) -> Result<Self, TokenCeilingError> {
        if raw < i64::from(MIN_TOKEN_CEILING) {
            return Err(TokenCeilingError::TooSmall);
        }
        if raw > i64::from(MAX_TOKEN_CEILING) {
            return Err(TokenCeilingError::TooLarge);
        }
        Ok(Self(raw as u32))
    }

    /// The ceiling as the wire carries it.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for TokenCeiling {
    fn default() -> Self {
        Self(DEFAULT_TOKEN_CEILING)
    }
}

impl fmt::Display for TokenCeiling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for TokenCeiling {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for TokenCeiling {
    /// Runs the constructor's own check, so a hand-edited `desktop.toml` cannot
    /// smuggle past what the settings panel applies.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = i64::deserialize(deserializer)?;
        Self::parse(raw).map_err(|error| serde::de::Error::custom(error.detail()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_url_accepts_https_and_loopback_http() {
        for accepted in [
            "https://api.openai.com/v1/audio/transcriptions",
            "https://api.anthropic.com/v1/messages",
            "http://127.0.0.1:18000/v1/audio/transcriptions",
            "http://localhost:8000/v1/audio/transcriptions",
            "http://LOCALHOST:8000/v1",
            "http://[::1]:18000/v1/audio/transcriptions",
            "http://127.9.9.9:18000/v1",
            "https://example.com:8443/v1/messages?beta=1",
        ] {
            assert!(
                ServiceUrl::parse(accepted).is_ok(),
                "should accept {accepted}"
            );
        }
    }

    #[test]
    fn a_service_url_round_trips_the_text_it_was_given() {
        // Held raw rather than re-serialised, so a document reads back the way
        // it was written — the property the type's doc comment claims.
        let url =
            ServiceUrl::parse("  https://api.openai.com/v1/audio/transcriptions  ").expect("valid");
        assert_eq!(
            url.as_str(),
            "https://api.openai.com/v1/audio/transcriptions"
        );
    }

    #[test]
    fn a_service_url_refuses_plaintext_to_another_machine() {
        assert_eq!(
            ServiceUrl::parse("http://api.openai.com/v1/audio/transcriptions"),
            Err(ServiceUrlError::InsecureNonLoopback)
        );
        assert_eq!(
            ServiceUrl::parse("http://192.168.1.10:18000/v1"),
            Err(ServiceUrlError::InsecureNonLoopback)
        );
        // A name this build cannot resolve is not loopback, whatever it
        // resolves to on the user's machine — see `host_is_loopback`.
        assert_eq!(
            ServiceUrl::parse("http://mybox.local:18000/v1"),
            Err(ServiceUrlError::InsecureNonLoopback)
        );
    }

    #[test]
    fn a_service_url_refuses_an_embedded_credential() {
        assert_eq!(
            ServiceUrl::parse("https://sk-abcdef:secret@api.openai.com/v1"),
            Err(ServiceUrlError::Credentials)
        );
        assert_eq!(
            ServiceUrl::parse("https://sk-abcdef@api.openai.com/v1"),
            Err(ServiceUrlError::Credentials)
        );
    }

    #[test]
    fn a_service_url_refuses_a_scheme_that_is_not_http() {
        for refused in [
            "file:///etc/passwd",
            "ftp://example.com/v1",
            "javascript:alert(1)",
            "data:text/plain,hello",
        ] {
            assert_eq!(
                ServiceUrl::parse(refused),
                Err(ServiceUrlError::Scheme),
                "should refuse {refused}"
            );
        }
    }

    #[test]
    fn a_service_url_refuses_a_relative_or_hostless_value() {
        assert_eq!(
            ServiceUrl::parse("/v1/audio/transcriptions"),
            Err(ServiceUrlError::Unparseable)
        );
        assert_eq!(
            ServiceUrl::parse("api.openai.com/v1"),
            Err(ServiceUrlError::Unparseable)
        );
        assert_eq!(
            ServiceUrl::parse("https://"),
            Err(ServiceUrlError::Unparseable)
        );
    }

    #[test]
    fn a_service_url_refuses_whitespace_control_bytes_and_non_ascii() {
        // Each of these is refused on the BYTES, before the parse — which is
        // what stops `Url::parse`'s tab/newline stripping from turning a value
        // whose text says one thing into a request that goes somewhere else.
        for refused in [
            "https://api.openai.com/v1 with a space",
            "https://api.openai.com/\tv1",
            "https://api.openai.com/\nv1",
            "https://api.openai.com/v1\u{0}",
            "https://api.öpenai.com/v1",
        ] {
            assert_eq!(
                ServiceUrl::parse(refused),
                Err(ServiceUrlError::Charset),
                "should refuse {refused:?}"
            );
        }
    }

    #[test]
    fn a_service_url_is_bounded_before_it_is_parsed() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_SERVICE_URL_BYTES));
        assert_eq!(ServiceUrl::parse(&long), Err(ServiceUrlError::TooLong));
        // Exactly at the bound is accepted: the cap is the size this build is
        // willing to hold, so a value that fills it is still a value.
        let exact = format!(
            "https://example.com/{}",
            "a".repeat(MAX_SERVICE_URL_BYTES - "https://example.com/".len())
        );
        assert_eq!(exact.len(), MAX_SERVICE_URL_BYTES);
        assert!(ServiceUrl::parse(&exact).is_ok());
    }

    #[test]
    fn a_service_url_refuses_an_empty_value() {
        assert_eq!(ServiceUrl::parse(""), Err(ServiceUrlError::Empty));
        assert_eq!(ServiceUrl::parse("   "), Err(ServiceUrlError::Empty));
    }

    #[test]
    fn a_service_url_knows_whether_it_is_loopback() {
        assert!(
            ServiceUrl::parse("http://127.0.0.1:18000/v1")
                .expect("valid")
                .is_loopback()
        );
        assert!(
            ServiceUrl::parse("http://localhost:18000/v1")
                .expect("valid")
                .is_loopback()
        );
        assert!(
            ServiceUrl::parse("https://[::1]/v1")
                .expect("valid")
                .is_loopback()
        );
        assert!(
            !ServiceUrl::parse("https://api.openai.com/v1")
                .expect("valid")
                .is_loopback()
        );
    }

    #[test]
    fn a_service_url_names_its_host_for_a_which_key_sentence() {
        assert_eq!(
            ServiceUrl::parse("https://api.anthropic.com/v1/messages")
                .expect("valid")
                .host(),
            "api.anthropic.com"
        );
        assert_eq!(
            ServiceUrl::parse("http://127.0.0.1:18000/v1")
                .expect("valid")
                .host(),
            "127.0.0.1"
        );
    }

    #[test]
    fn a_service_url_names_the_port_a_request_would_connect_to() {
        assert_eq!(
            ServiceUrl::parse("http://127.0.0.1:18000/v1")
                .expect("valid")
                .port(),
            Some(18000)
        );
        // Written without one, so the scheme's default is what a user has to
        // publish — the sentence that names it would otherwise say nothing.
        assert_eq!(
            ServiceUrl::parse("http://localhost/v1")
                .expect("valid")
                .port(),
            Some(80)
        );
        assert_eq!(
            ServiceUrl::parse("https://api.openai.com/v1")
                .expect("valid")
                .port(),
            Some(443)
        );
    }

    #[test]
    fn a_service_url_names_its_origin_without_the_path() {
        assert_eq!(
            ServiceUrl::parse("http://127.0.0.1:18000/v1/audio/transcriptions")
                .expect("valid")
                .origin(),
            "http://127.0.0.1:18000"
        );
        assert_eq!(
            ServiceUrl::parse("https://api.openai.com/v1/audio/transcriptions")
                .expect("valid")
                .origin(),
            "https://api.openai.com"
        );
    }

    #[test]
    fn a_service_url_deserializes_through_its_own_check() {
        let accepted: ServiceUrl =
            serde_json::from_str(r#""https://api.openai.com/v1""#).expect("parses");
        assert_eq!(accepted.as_str(), "https://api.openai.com/v1");

        let refused = serde_json::from_str::<ServiceUrl>(r#""http://api.openai.com/v1""#)
            .expect_err("a hand-edited document cannot smuggle past the constructor");
        let message = refused.to_string();
        assert!(
            message.contains("http is allowed only for a loopback endpoint"),
            "unexpected message: {message}"
        );
        // Issue #827's rule: the reason, never a byte of the value.
        assert!(
            !message.contains("api.openai.com"),
            "the message quotes the document: {message}"
        );
    }

    #[test]
    fn a_service_url_serializes_as_one_string() {
        let url = ServiceUrl::parse("https://api.anthropic.com/v1/messages").expect("valid");
        assert_eq!(
            serde_json::to_string(&url).expect("serializes"),
            r#""https://api.anthropic.com/v1/messages""#
        );
    }

    #[test]
    fn a_model_id_accepts_what_the_three_ecosystems_publish() {
        for accepted in [
            "whisper-1",
            "claude-haiku-4-5",
            "Systran/faster-whisper-tiny.en",
            "deepdml/faster-whisper-large-v3-turbo-ct2",
            "gpt-4o-transcribe",
            "llama3.1:8b",
            "some_model+v2",
        ] {
            assert!(ModelId::parse(accepted).is_ok(), "should accept {accepted}");
        }
    }

    #[test]
    fn a_model_id_refuses_control_bytes_whitespace_and_non_ascii() {
        for refused in [
            "whisper 1",
            "whisper\n1",
            "whisper\t1",
            "whisper\u{0}1",
            "whisper—1",
            "wh*sper",
            "whisper;rm -rf /",
        ] {
            assert_eq!(
                ModelId::parse(refused),
                Err(ModelIdError::Charset),
                "should refuse {refused:?}"
            );
        }
    }

    #[test]
    fn a_model_id_is_bounded_and_non_empty() {
        assert_eq!(ModelId::parse(""), Err(ModelIdError::Empty));
        assert_eq!(ModelId::parse("  "), Err(ModelIdError::Empty));
        assert_eq!(
            ModelId::parse(&"a".repeat(MAX_MODEL_ID_BYTES + 1)),
            Err(ModelIdError::TooLong)
        );
        assert!(ModelId::parse(&"a".repeat(MAX_MODEL_ID_BYTES)).is_ok());
    }

    #[test]
    fn a_model_id_round_trips_through_serde_and_refuses_the_rest() {
        let accepted: ModelId = serde_json::from_str(r#""whisper-1""#).expect("parses");
        assert_eq!(accepted.as_str(), "whisper-1");
        assert_eq!(
            serde_json::to_string(&accepted).expect("serializes"),
            r#""whisper-1""#
        );

        let refused = serde_json::from_str::<ModelId>(r#""whisper 1""#)
            .expect_err("a hand-edited document cannot smuggle past the constructor");
        assert!(
            refused.to_string().contains("only letters, digits"),
            "unexpected message: {refused}"
        );
    }

    /// Scenario: the answer ceiling accepts every value inside its bounds and
    /// refuses everything outside them, including the zero and the negative a
    /// hand-edited document can hold.
    ///
    /// The whole reason this is a newtype. `u32` is already on
    /// `ALLOWED_FIELD_TYPES`, so a bare integer would have satisfied the
    /// credential guard and then held `0` — a command stage that can never
    /// answer — or four billion, which is a bill rather than a setting.
    #[test]
    fn a_token_ceiling_holds_only_a_usable_range() {
        for accepted in [
            i64::from(MIN_TOKEN_CEILING),
            256,
            i64::from(DEFAULT_TOKEN_CEILING),
            i64::from(MAX_TOKEN_CEILING),
        ] {
            assert_eq!(
                TokenCeiling::parse(accepted).map(TokenCeiling::get),
                Ok(accepted as u32),
                "should accept {accepted}"
            );
        }
        for (refused, error) in [
            (0, TokenCeilingError::TooSmall),
            (-1, TokenCeilingError::TooSmall),
            (i64::MIN, TokenCeilingError::TooSmall),
            (
                i64::from(MIN_TOKEN_CEILING) - 1,
                TokenCeilingError::TooSmall,
            ),
            (
                i64::from(MAX_TOKEN_CEILING) + 1,
                TokenCeilingError::TooLarge,
            ),
            (4_000_000_000, TokenCeilingError::TooLarge),
            (i64::MAX, TokenCeilingError::TooLarge),
        ] {
            assert_eq!(
                TokenCeiling::parse(refused),
                Err(error),
                "should refuse {refused}"
            );
        }
        // The default is the replacement for the hardwired 256, and it is the
        // number `DEFAULT_TOKEN_CEILING` documents rather than whichever bound
        // happens to be nearest.
        assert_eq!(TokenCeiling::default().get(), DEFAULT_TOKEN_CEILING);
        assert_eq!(DEFAULT_TOKEN_CEILING, 4096);
    }

    /// Scenario: a ceiling written into a document is read through the
    /// constructor's own check, and a refused one reports the rule it broke
    /// rather than a Rust type name.
    ///
    /// Both formats, because the value arrives by both routes — TOML from the
    /// file a user hand-edits, JSON from the webview — and a check that ran on
    /// only one of them would be a check the other route walks past.
    #[test]
    fn a_token_ceiling_runs_its_check_on_both_wires() {
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        struct Holder {
            ceiling: TokenCeiling,
        }

        assert_eq!(
            toml_edit::de::from_str::<Holder>("ceiling = 8192")
                .unwrap()
                .ceiling,
            TokenCeiling::parse(8192).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Holder>(r#"{"ceiling":8192}"#)
                .unwrap()
                .ceiling,
            TokenCeiling::parse(8192).unwrap()
        );
        // Serialised as a bare number on both, which is what the TypeScript
        // DTO declares and what TOML's own integer syntax is.
        let holder = Holder {
            ceiling: TokenCeiling::parse(8192).unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&holder).unwrap(),
            r#"{"ceiling":8192}"#
        );

        for refused in ["ceiling = 0", "ceiling = -1", "ceiling = 32769"] {
            let error = toml_edit::de::from_str::<Holder>(refused)
                .expect_err("should refuse")
                .to_string();
            assert!(
                error.contains("an answer ceiling is at"),
                "{refused} reported `{error}`, which names no rule"
            );
        }
        assert!(
            serde_json::from_str::<Holder>(r#"{"ceiling":0}"#)
                .expect_err("should refuse")
                .to_string()
                .contains("an answer ceiling is at least 64 tokens")
        );
        // And never a byte of the offending value, for `SettingsDocumentProblem`'s
        // reason — the two error sentences are `&'static str`s, so there is
        // nothing for one to interpolate.
        assert!(
            !TokenCeilingError::TooLarge.detail().contains("32769"),
            "the message quotes the value it refused"
        );
    }
}
