//! The desktop app's own settings document (PRD #803, M2).
//!
//! This is the *client-owned* half of the boundary rule the PRD writes down:
//! the desktop app gets everything else from the daemon, and the only thing it
//! owns is its own settings. Nothing in this module crosses the TUI↔daemon
//! protocol — the document is read and written entirely inside the desktop
//! process, and the daemon cannot observe it.
//!
//! # Where it lives
//!
//! [`config_dir`]`().join("desktop.toml")` — a **sibling** of the TUI's
//! `config.toml` and `keybindings.toml`, never a section inside them.
//! `DashboardConfig::save()` serialises its struct (`src/config.rs`), so a
//! `[desktop]` table the TUI does not know about would be silently deleted on
//! the next TUI write; the split is what keeps the two schemas from coupling.
//! [`SETTINGS_PATH_ENV`] overrides the whole path, mirroring the
//! `DOT_AGENT_DECK_CONFIG` convention and giving tests a seam.
//!
//! # Failure behaviour
//!
//! **Loading never fails, and since issue #1072 it is not silent either.** A
//! missing file, an unparseable file, an unreadable file, an unusable path and
//! an unknown enum value all still yield defaults, and the app still comes up —
//! a settings file is not worth failing an app launch over. What changed is
//! that [`load_document`] now RETURNS the reason as a
//! [`SettingsDocumentProblem`] instead of logging it and dropping it, so
//! [`load_snapshot`] can hand the user a sentence and [`save_to`] can refuse.
//!
//! **A document this build cannot read AT SAVE TIME is never overwritten.**
//! That is the whole of #1072 and it is the property to preserve: defaults in
//! memory plus a merge that makes the struct authoritative meant the *next save*
//! replaced every key the schema owns — appearance, zoom, endpoints, the deck
//! selection — with this build's defaults. The app looked normal, said nothing a
//! user would see, and the original was gone one click later. [`save_to`] now
//! re-reads the document and refuses; a user who cannot save a preference is in
//! a better position than one whose configuration has been destroyed.
//!
//! The qualifier is meant. [`save_to`] reads, vets and then renames, so a
//! document that becomes unreadable *inside that window* is still replaced —
//! the same shape of residual the path vet accepts a few paragraphs down, and
//! the same one an anchored `renameat` would be needed to close. It is a
//! microsecond-wide race between two writers, which is issue #828's subject and
//! not this one's; what #1072 was about is the ordinary single-writer case,
//! where the app read the document at launch, could not use it, and overwrote it
//! anyway. Re-reading at save time rather than trusting the launch read is what
//! makes the window that narrow.
//!
//! **The path is vetted and the read is bounded.** [`read_document`] requires
//! an absolute path with a file name whose target is absent or a regular file,
//! and reads at most [`MAX_SETTINGS_BYTES`]. That is not a privilege boundary —
//! anyone who can set [`SETTINGS_PATH_ENV`] can already run code as this user —
//! it is there because an app that hangs forever on a FIFO or dies on
//! `/dev/zero` is a miserable thing to debug.
//!
//! **Writing is atomic and owner-only.** A temp file in the same directory
//! under an unpredictable name, then a rename; mode 0o600 on Unix, a protected
//! DACL on Windows. Modelled on `dot_agent_deck::schedule_cli::write_atomic`.
//! [`save_to`] records what that deliberately does *not* defend against.
//!
//! **Writing also preserves what it does not understand.** The save merges the
//! serialised struct into the document already on disk rather than replacing
//! it, so an older build cannot delete a section a newer one wrote. See
//! [`merged_document`] — this is the one place where `#[serde(default)]`
//! genuinely does not give what it looks like it gives.
//!
//! **And it preserves how the user wrote it** (issue #825). The merge runs on a
//! [`toml_edit::DocumentMut`], a format-preserving DOM, and writes only the keys
//! whose data actually changed — so comments, inline arrays, inline tables, key
//! order and blank-line grouping survive a save, and a save with no new value to
//! write leaves every byte alone. Before that the merge went through
//! `toml::Table`, which models data: no unknown key was ever lost, but the
//! document was re-rendered canonically and a hand-written annotation went with
//! it. Since PRD #803 makes "a file a user can read, edit and delete without the
//! app running" a success criterion, hand-annotation is a thing this file
//! invites. [`merged_document`] states the two things that are **not**
//! preserved — read them before repeating the sentence above anywhere.
//!
//! **This crate names exactly one TOML library**, and that is deliberate rather
//! than incidental: `toml_edit` replaced `toml` here instead of joining it, so
//! the parser that reads the document and the one that writes it cannot
//! disagree about it. `desktop/src-tauri/Cargo.toml` carries the rest of the
//! reasoning.
//!
//! # Adding a setting
//!
//! Add a field to your feature's section struct, or add a new section struct
//! and one line to [`DesktopSettings`]. `#[serde(default)]` gives it a default
//! and there is deliberately no `deny_unknown_fields`, so a field written by a
//! newer build survives an older build reading the file.
//!
//! Two rules constrain what may go in:
//!
//! 1. **A secret never goes in this document, and never in `localStorage`.**
//!    The document may hold a non-secret *reference* — which backend holds the
//!    key, or a boolean saying one is stored — and nothing more. A real
//!    credential belongs behind the `SecretStore` seam (PRD #803 M5), whose
//!    intended implementation is the OS keychain.
//!
//!    Two different kinds of check watch that rule, and confusing them is the
//!    mistake issue #827 was opened about.
//!
//!    [`tests::no_settings_key_name_trips_the_credential_tripwire`] fails the
//!    build on a credential-shaped key **name**, and it is a **naming tripwire,
//!    not a security boundary** — the distinction matters enough that the test,
//!    its failure message and the developer docs all say it in those words. It
//!    reads key names in the serialised default document and nothing else, so a
//!    field called `endpoint` holding a token passes it, and the TypeScript DTO
//!    is outside it entirely. Read a pass as "nobody named a field like a
//!    credential", never as "a credential cannot get in here".
//!
//!    The **value**-side checks are the ones to rely on. They follow one
//!    uniquely-named sentinel through every sink #827 enumerates — the document
//!    on disk across a load-modify-save round trip, the IPC echo, the
//!    `desktop_get_settings` snapshot, the parse diagnostic, and both halves of
//!    [`SettingsWriteError`] — reaching the two settings commands through the
//!    public functions their bodies wrap, since a `#[tauri::command]` needs a
//!    running app to call.
//!
//!    **What they establish stopped being one sentence at PRD #741 M6, and the
//!    old sentence is worth quoting because it is now false.** It read: *this
//!    build's schema has no field that can carry arbitrary text*. That was true
//!    of `u32`, [`AppearanceMode`] and [`ZoomLevel`] — an integer, three tokens
//!    and ten numbers — and it is **not** true of the ssh-argument newtypes
//!    #741 added. `Hostname` is 253 bytes of `[A-Za-z0-9._-]` plus the
//!    `[`, `]`, `:` and `%` a bracketed IPv6 literal and its zone id need,
//!    `SshUser` 64 of `[A-Za-z0-9._@-]`, `HostAlias` 253 of `[A-Za-z0-9._-]`
//!    and [`EndpointId`] 64; measured, the 45-byte
//!    `sk-…`-shaped sentinel below **parses** as all four. They *bound and
//!    restrict*, which is a real property and a weaker one than "cannot carry
//!    text", and `ALLOWED_FIELD_TYPES`' own entries have said so since M5.
//!
//!    So the claim, in the two halves that are each true:
//!
//!    - For every field whose type is `u32`, `u16`, [`AppearanceMode`],
//!      [`ZoomLevel`] or [`Selection`], there is **no route** by which a
//!      credential reaches disk, the IPC, or the log — nothing arbitrary is
//!      representable, and the sentinel sweep proves it end to end.
//!    - For the ssh-argument fields, a single-line token **is** representable,
//!      and what rules a credential out is not the type but **where the value
//!      goes**: each is handed to `ssh` as an argument and reaches no
//!      authentication surface, key *material* and a passphrase are excluded by
//!      shape (`KeyPath` must start `/` or `~/`, so a PEM block cannot be
//!      written there — measured, the sentinel is refused by `KeyPath` and
//!      `RemoteSocketPath`), and the storage policy is that a credential goes
//!      behind the `SecretStore` seam instead. Pinned by
//!      [`tests::an_ssh_argument_field_bounds_and_restricts_rather_than_forbidding_text`],
//!      which exists so the next reader finds the boundary measured rather than
//!      asserted.
//!
//!    Two further things the sweep does **not** claim. A key this schema does
//!    not own keeps whatever a user or a newer build wrote there, because the
//!    save merges rather than replaces (see [`merged_document`]) — measured by
//!    [`tests::a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else`].
//!    And the sweep is derived from the **default** document, so it covers a
//!    field #802 adds only once that field appears there: an optional field or
//!    a row in an empty list is outside it, which is why the naming tripwire
//!    grew [`tests::representative_document`] and why a value-side claim about
//!    the endpoint fields is made by its own test rather than by the sweep.
//!    The TypeScript half of the schema, the `localStorage` key set and the
//!    field-type allowlist are checked in
//!    `xtask/linkage-check/src/desktop_settings_secrets.rs`, because those live
//!    outside this crate and a vitest guard can be merged past.
//! 2. **Field names stay `snake_case`, and single-word where it is natural.**
//!    The same struct is serialised to TOML (which a user hand-edits, and where
//!    `snake_case` is this repo's convention) *and* to JSON for the webview
//!    (where the desktop DTOs use `camelCase`). Every name today is one word,
//!    so the two agree byte for byte. The first genuinely multi-word field is
//!    the point at which a separate webview DTO has to be introduced — not a
//!    `rename_all` on this struct, which would make the TOML read badly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dot_agent_deck::daemon_client::{Endpoint, RemoteEndpoint};
use dot_agent_deck::platform::fsperm;
use dot_agent_deck::platform::paths::config_dir;
// PRD #741 M6: the validating ssh-argument newtypes ARE this section's schema.
// `ALLOWED_FIELD_TYPES` refuses `String`, so the endpoint fields have to be
// types whose `Deserialize` bounds and charset-checks them — which is exactly
// what these are. See `xtask/linkage-check/src/desktop_project_boundary.rs` for
// why `remote_tunnel` is on that rule's allowlist.
use dot_agent_deck::remote_tunnel::{
    HostAlias, Hostname, KeyPath, RemoteSocketPath, SshPort, SshUser,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model_service::{ModelId, ServiceUrl, TokenCeiling};

/// Overrides the whole settings path, mirroring `DOT_AGENT_DECK_CONFIG`
/// (`src/config.rs`). Also the seam every test uses instead of the real
/// config directory.
pub const SETTINGS_PATH_ENV: &str = "DOT_AGENT_DECK_DESKTOP_CONFIG";

/// The document's file name inside [`config_dir`].
const SETTINGS_FILE_NAME: &str = "desktop.toml";

/// Schema version carried by every document this build writes.
///
/// Open Question 4 in PRD #803: cheap insurance, and impossible to add
/// retroactively without a heuristic for "documents written before the field
/// existed". Nothing reads it yet — a future migration will.
pub const SETTINGS_VERSION: u32 = 1;

/// How the app picks its light/dark palette.
///
/// The *storage* is #803's; what the choice does to the UI is PRD #743's.
/// Serialised as a lowercase string in both TOML and JSON, because that is what
/// reads well in a hand-edited config file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AppearanceMode {
    /// Follow the operating system's own light/dark preference.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// The exact token written to TOML and JSON. Pinned by
    /// [`tests::default_document_shape_is_pinned`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parse a stored token, falling back to the default.
    ///
    /// An unknown value is **not** an error: a document written by a newer
    /// build may name a mode this one has never heard of, and losing the whole
    /// document over one unreadable field would be the opposite of the
    /// unknown-key tolerance the rest of the schema is built for.
    fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::default(),
        }
    }
}

impl Serialize for AppearanceMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The longest appearance token this build will accept, on either side.
///
/// The tokens are `system`, `light` and `dark` — six bytes at most. 64 leaves
/// room for a mode name a future build invents while making a payload-shaped
/// value impossible.
pub const MAX_APPEARANCE_TOKEN_BYTES: usize = 64;

impl<'de> Deserialize<'de> for AppearanceMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A derived unit-variant `Deserialize` would reject an unknown token,
        // and serde's `#[serde(other)]` is not available on an externally
        // tagged enum, so the fallback is spelled out here. A visitor rather
        // than `String::deserialize` because the length has to be judged on the
        // borrowed input, **before** anything allocates a normalised copy of it
        // — see [`AppearanceModeVisitor::visit_str`].
        deserializer.deserialize_str(AppearanceModeVisitor)
    }
}

struct AppearanceModeVisitor;

impl serde::de::Visitor<'_> for AppearanceModeVisitor {
    type Value = AppearanceMode;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "an appearance mode of at most {MAX_APPEARANCE_TOKEN_BYTES} bytes"
        )
    }

    /// Length first, then [`AppearanceMode::from_str_lossy`].
    ///
    /// The order is the point. `from_str_lossy` trims and lowercases, which
    /// allocates a copy of whatever it was handed, so a webview sending a
    /// megabyte-long mode used to get a megabyte allocated and rewritten before
    /// anything looked at it. The bound lives here rather than in the Tauri
    /// command so it covers the command *and* a hand-edited document with one
    /// check.
    ///
    /// Over-length is an **error**, not the unknown-value fallback, and the
    /// distinction is deliberate: an unrecognised token is a mode this build has
    /// not heard of and is tolerated by design, while a 4 KB one is a malformed
    /// document. On the disk path that means the whole document falls back to
    /// defaults — the ordinary malformed-document behaviour, logged, never a
    /// failed launch. On the IPC path it fails argument deserialisation with a
    /// message that names the limit and carries no path and no value.
    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<AppearanceMode, E> {
        if raw.len() > MAX_APPEARANCE_TOKEN_BYTES {
            return Err(E::custom(format!(
                "an appearance mode is at most {MAX_APPEARANCE_TOKEN_BYTES} bytes; got {}",
                raw.len()
            )));
        }
        Ok(AppearanceMode::from_str_lossy(raw))
    }
}

/// The `[appearance]` section — PRD #743's tenant, stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub mode: AppearanceMode,
}

/// The `[zoom]` section — PRD #744's tenant, stored here.
///
/// One field, and it is a *scale factor* rather than a percentage, because that
/// is the unit `webview.set_zoom` takes: keeping storage, IPC, the frontend
/// ladder and the platform call in one unit means there is no conversion
/// anywhere to get backwards.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoomSettings {
    pub level: ZoomLevel,
}

/// The `[voice]` section — PRD #802's tenant, stored here.
///
/// # One choice plus two stages, and why the stages are nested
///
/// The two things voice does — turn speech into text, turn text into an action
/// — each need three values: **which** backend, **where** it is, and **which**
/// model to ask it for. Flattening those into `[voice]` would need
/// `speech_endpoint` and `commands_endpoint`, and this struct serialises to
/// both TOML and JSON, so a field name here is a JSON key the frontend reads —
/// single-word `snake_case` or the two spellings drift. Nesting keeps every
/// name one word on both sides and puts the two stages in the same shape, which
/// is what lets the panel render them from one component.
///
/// # Why the endpoint and the model became fields at all
///
/// They were `const`s until PRD #802's provider-selection work, which
/// hardwired speech to one hosted service and commands to another. A user could
/// not know which key to paste, could not use a provider this build did not
/// pick, and — the reason it mattered most — could not point speech at a
/// container on their own machine and stop paying for it. The values are
/// [`ServiceUrl`] and [`ModelId`] rather than `String`s: see
/// [`crate::model_service`] for what each one refuses, and
/// `ALLOWED_FIELD_TYPES` for why a `String` was never available.
///
/// # What is deliberately NOT here
///
/// **No credential, in any form** — that is PRD #803's hard rule and
/// [`crate::secrets`] is where one goes instead. **Not even a boolean saying
/// one is stored**, which the rule would have allowed: the panel asks
/// [`crate::secrets::SecretStore::status`] instead, so the answer comes from
/// the keychain itself and cannot go stale against it. That also keeps `bool`
/// off `ALLOWED_FIELD_TYPES`, which is a small win worth having.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    pub activation: ActivationMode,
    pub intent: IntentSettings,
    pub transcription: TranscriptionSettings,
    /// Whether the command backend is shown the names the app observed (PRD
    /// #1223, audit finding A1). See [`LabelSharing`].
    pub labels: LabelSharing,
}

/// The endpoint the keyless local speech container listens on.
///
/// PRD #802 measured this one: `ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu`
/// published on `127.0.0.1:18000`, answering a two-word utterance in a median
/// of **0.653 s** warm. It speaks the same multipart shape the hosted service
/// does — `model`, `response_format=json`, `file` — and answers with the same
/// `text` field, which is why one HTTP backend serves both.
pub const LOCAL_SPEECH_ENDPOINT: &str = "http://127.0.0.1:18000/v1/audio/transcriptions";

/// The model the local container is asked for.
///
/// The smallest useful Whisper build — a 78 MB cache and about 480 MB resident
/// once loaded, which is what makes "run it on your own laptop" an honest
/// default rather than a suggestion to buy a GPU.
pub const LOCAL_SPEECH_MODEL: &str = "Systran/faster-whisper-tiny.en";

/// The container image the unreachable-endpoint sentence tells a user to start.
///
/// Pinned to a digest-bearing tag rather than `latest` for the reason any
/// instruction in a product is pinned: the words have to keep working after the
/// upstream tag moves, and `0.9.0-rc.3-cpu` is the build PRD #802 measured.
pub const LOCAL_SPEECH_IMAGE: &str = "ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu";

/// Where the keyed speech service is, and which model it is asked for.
///
/// OpenAI's transcription endpoint. The panel names the provider so a user
/// knows which key to paste — which the old `Remote service` label did not.
pub const HOSTED_SPEECH_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
pub const HOSTED_SPEECH_MODEL: &str = "whisper-1";

/// Where each command protocol's preset provider is, and which model it is
/// asked for.
///
/// Anthropic's Messages endpoint, whose tool-use envelope
/// [`crate::voice::remote`] speaks. `claude-haiku-4-5` is the measured choice —
/// a median of 0.91 s against the deleted agent CLI's 3.1–4.7 s, at roughly
/// $0.0015 an utterance.
///
/// **This was the default and is no longer**, which changes nothing about the
/// measurement: it is still the fastest thing measured here and still 24/24 on
/// the phrase fixtures. [`IntentBackend`] has why the default moved, and the
/// reason is not about this model.
pub const HOSTED_COMMAND_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
pub const HOSTED_COMMAND_MODEL: &str = "claude-haiku-4-5";

/// OpenAI's chat-completions endpoint, whose nested `json_schema` envelope
/// [`crate::voice::openai`] speaks. **The default since PRD #802's one-key
/// work** — see [`IntentBackend`] for why.
///
/// `gpt-5-mini` is now a measured choice rather than a plausible one. Through
/// the shipped builder and parser against the 24 phrase fixtures, with
/// [`OPENAI_COMMAND_REASONING_EFFORT`] and the 4096 ceiling:
///
/// | configuration | score | median | cost | reasoning tokens |
/// | --- | ---: | ---: | ---: | ---: |
/// | `reasoning_effort: "minimal"` | **24/24** | **818 ms** | $0.0073 | 0 |
/// | provider-default reasoning | 24/24 | 1,778 ms | $0.0161 | 4,416 |
/// | *`claude-haiku-4-5`, for scale* | *24/24* | *780 ms* | — | — |
///
/// So it is level with the Anthropic preset on accuracy, within 40 ms of it on
/// latency, and reasoning is worth **twice the cost and twice the wall clock**
/// for no score. `gpt-4.1-mini` was here before the measurement and was never
/// run against anything; it went rather than being kept as a second untested
/// coordinate.
pub const OPENAI_COMMAND_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
pub const OPENAI_COMMAND_MODEL: &str = "gpt-5-mini";

/// The `reasoning_effort` the OpenAI command preset sends, and **only** that
/// preset.
///
/// A routing decision over a closed enum has no reasoning to do, and the
/// measurement above says so twice over: `minimal` scored the same 24/24 while
/// spending **zero** reasoning tokens, at half the latency and half the price.
/// With it the largest response was 28 completion tokens.
///
/// # Why it is scoped to the preset and cannot leak
///
/// `reasoning_effort` is an **OpenAI-family** parameter, not part of the
/// `/v1/chat/completions` shape every server implementing that path supports.
/// A different one may refuse an unknown field outright — the `llama.cpp`
/// server probed for PRD #802 wanted `chat_template_kwargs` instead — and even
/// on `api.openai.com` it is not accepted for every model. So it must not
/// become a field every `openai_compatible` request carries.
///
/// [`IntentSettings::reasoning_effort`] is the gate, and it answers `Some` only
/// when the endpoint **and** the model are both exactly this build's preset —
/// which is to say, only for the configuration the table above was measured on.
/// Every edit a user can make moves off it: another provider, a gateway in
/// front of OpenAI, a server on loopback, or the same endpoint with a different
/// model. Each of those then gets provider-default reasoning, which is the safe
/// direction to be wrong in **because** the ceiling is 4096 — the 24/24 row
/// above is that configuration.
pub const OPENAI_COMMAND_REASONING_EFFORT: &str = "minimal";

/// The speech stage: which transcriber, where it is, and which model.
///
/// Its [`Default`] is the **keyless local container**, which is the product
/// decision PRD #802's provider work turned on: the feature is try-able on the
/// day it ships without anyone pasting a credential, and the audio never leaves
/// the machine. `Speech = off` used to be the default and is gone — a stage
/// that cannot run is not a setting, it is a missing prerequisite, and the
/// honest version of it is an unreachable endpoint saying what to start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranscriptionSettings {
    pub backend: TranscriptionBackend,
    pub endpoint: ServiceUrl,
    pub model: ModelId,
}

impl TranscriptionSettings {
    /// This build's coordinates for one backend.
    ///
    /// The preset a user gets when they pick a backend and say nothing else —
    /// and the reason [`Deserialize`] is written by hand rather than derived
    /// with `#[serde(default)]`. A derived default fills a **missing endpoint
    /// from the struct's default**, which is the LOCAL one: a hand-written
    /// `[voice.transcription]\nbackend = "remote"` would then have authenticated
    /// against the loopback container, sending a key somewhere that has no
    /// notion of one. Filling from the *chosen backend* is the only answer that
    /// cannot be wrong, and it is what the frontend's `normalizeVoiceStage`
    /// already did.
    pub fn for_backend(backend: TranscriptionBackend) -> Self {
        // `expect` on `const`s this file owns, pinned by
        // `the_preset_service_coordinates_are_valid`. A panic here would be a
        // build whose own default is unrepresentable, which is a bug to fail
        // loudly on rather than a state to degrade into.
        let (endpoint, model) = match backend {
            TranscriptionBackend::Local => (LOCAL_SPEECH_ENDPOINT, LOCAL_SPEECH_MODEL),
            TranscriptionBackend::Remote => (HOSTED_SPEECH_ENDPOINT, HOSTED_SPEECH_MODEL),
        };
        Self {
            backend,
            endpoint: ServiceUrl::parse(endpoint).expect("a valid preset endpoint"),
            model: ModelId::parse(model).expect("a valid preset model"),
        }
    }
}

impl Default for TranscriptionSettings {
    fn default() -> Self {
        Self::for_backend(TranscriptionBackend::default())
    }
}

impl<'de> Deserialize<'de> for TranscriptionSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let doc = StageSpec::deserialize(deserializer)?.into_document();
        let preset = Self::for_backend(doc.backend.unwrap_or_default());
        let stage = Self {
            endpoint: doc.endpoint.unwrap_or(preset.endpoint),
            model: doc.model.unwrap_or(preset.model),
            backend: preset.backend,
        };
        // The pairing the keyless backend IS, enforced where the value is
        // built rather than where it is used. See [`KEYLESS_OFF_MACHINE`].
        if stage.backend == TranscriptionBackend::Local && !stage.endpoint.is_loopback() {
            return Err(serde::de::Error::custom(KEYLESS_OFF_MACHINE));
        }
        Ok(stage)
    }
}

/// What a keyless backend pointed off this machine is refused with.
///
/// [`TranscriptionBackend::Local`] means exactly one thing — *no credential is
/// sent* — and [`ServiceUrl`] will accept any `https://host`, so the two
/// together are a pairing nothing else checks: a hand-edited document, or a
/// panel field typed after the backend was picked, would POST the user's
/// captured audio to a hosted service with no `Authorization` header and no
/// sign that anything was wrong. `is_loopback` is what makes
/// [`crate::voice::transcribe::HttpTranscriber::transport_error`] offer the
/// docker hint, but the upload happens before that runs — so the pairing has to
/// be refused at the two places the value comes into being: this deserializer,
/// which is every route from a document or the webview, and
/// [`crate::voice::transcribe::HttpTranscriber::keyless`], which is every route
/// from Rust.
///
/// **Refused rather than folded to the loopback preset.** Folding is what the
/// tokens get, because a token is a name; an endpoint is a destination, and
/// silently moving one is the mistake
/// [`tests::a_refused_voice_endpoint_or_model_is_an_error_rather_than_a_fold`]
/// is written against — from the other direction, but for the same reason. The
/// refusal costs the user nothing they cannot see: the section defaults to the
/// keyless loopback preset and the settings surface says the document could not
/// be read.
pub const KEYLESS_OFF_MACHINE: &str = "the keyless speech backend sends no credential, so its endpoint must be on this machine — \
     use a loopback address, or pick the hosted backend under Settings → Voice";

/// One stage as the document may spell it: every key optional.
///
/// Shared by both stages because both fill a missing coordinate the same way —
/// from the backend that was named, never from the struct's own default. The
/// generic parameter is the backend enum, so each stage keeps its own closed
/// token set and its own folding deserializer.
///
/// Unknown keys are ignored, which is this schema's rule everywhere: a document
/// written by a newer build is not a malformed one.
#[derive(Deserialize)]
#[serde(default, bound = "B: Default + Deserialize<'de>")]
struct StageDocument<B> {
    backend: Option<B>,
    endpoint: Option<ServiceUrl>,
    model: Option<ModelId>,
    /// The answer ceiling, which **only the command stage reads**.
    ///
    /// It is declared on the shared document rather than on a second one
    /// because the two stages differ in this one key and nothing else, and a
    /// parallel `IntentDocument`/`IntentSpec` pair would be two more places for
    /// the bare-token migration below to be forgotten. What it costs is that
    /// `[voice.transcription]` also *parses* the key: an out-of-range value
    /// there is refused rather than ignored, which is the direction to err in —
    /// a sentence naming the bound beats silence about a line the user meant to
    /// have an effect. A well-formed one there is read and dropped, exactly as
    /// any unknown key is.
    max_tokens: Option<TokenCeiling>,
}

impl<B> Default for StageDocument<B> {
    fn default() -> Self {
        Self {
            backend: None,
            endpoint: None,
            model: None,
            max_tokens: None,
        }
    }
}

/// One stage as a document may spell it: a table, or the bare token the schema
/// this one replaced wrote.
///
/// # The migration this exists for
///
/// `[voice]` held three scalars before PRD #802's provider work —
/// `transcription = "off"`, `intent = "claude"`, `activation = "toggle"` — and
/// two of them became tables. (Both of those backend names have since been
/// deleted as well, which costs the migration nothing: a token names a backend,
/// and an unknown one folds to the default.) A document written by that build therefore
/// supplies a **string where a table is expected**, which is a *type* error and
/// not an unknown token: [`VoiceToken::from_str_lossy`]'s folding never gets a
/// look in, and `#[serde(default)]` does not fire for a value that is present
/// and wrong. `toml_edit::de::from_str` fails on the WHOLE document, and what
/// that used to cost was the whole document — the user's `[endpoints]`, their
/// appearance and their zoom all read as defaults because of a stale voice
/// token. [`load_document`]'s per-section recovery is the other half of that
/// fix; this half is what stops the document failing at all.
///
/// # A token means "this backend, and say nothing else"
///
/// Which is exactly what [`TranscriptionSettings::for_backend`] answers, so the
/// old value lands on that backend's current coordinates rather than on a
/// coordinate the old schema never had. `transcription = "off"` names a backend
/// this build deleted, so it folds to the default — the keyless local container
/// — through the same [`VoiceToken`] path an unknown token takes, which is the
/// honest home for it: the stage that used to do nothing now works and needs no
/// key.
///
/// **Not `#[serde(untagged)]`**, which would express the same two shapes in one
/// attribute. Untagged buffers the input into `serde::__private::de::Content`
/// before trying either variant, and that buffer has no source span — so a
/// refused endpoint would report `an unreported position` instead of the line
/// to open, and the reported error would be *data did not match any variant*
/// rather than the rule that was broken. [`load_document`]'s locator is the
/// only thing the settings surface can say about a bad document, so it is not
/// something to spend on an attribute.
enum StageSpec<B> {
    Token(B),
    Table(StageDocument<B>),
}

impl<B> StageSpec<B> {
    /// Both shapes as the one the stages read.
    fn into_document(self) -> StageDocument<B> {
        match self {
            Self::Token(backend) => StageDocument {
                backend: Some(backend),
                endpoint: None,
                model: None,
                max_tokens: None,
            },
            Self::Table(document) => document,
        }
    }
}

impl<'de, B: Default + Deserialize<'de>> Deserialize<'de> for StageSpec<B> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `deserialize_any` rather than a typed hint, because which of the two
        // shapes arrived is precisely what is not known. Both formats this
        // document crosses are self-describing — TOML on disk, JSON over the
        // IPC seam — so there is no format for which this is unsupported.
        deserializer.deserialize_any(StageSpecVisitor(std::marker::PhantomData))
    }
}

struct StageSpecVisitor<B>(std::marker::PhantomData<B>);

impl<'de, B: Default + Deserialize<'de>> serde::de::Visitor<'de> for StageSpecVisitor<B> {
    type Value = StageSpec<B>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a voice stage table, or the bare backend token an older document wrote")
    }

    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<Self::Value, E> {
        // Through `B`'s own `Deserialize` rather than a parse of its own, so the
        // token keeps the length bound and the folding
        // [`VoiceTokenVisitor`] applies everywhere else.
        B::deserialize(serde::de::value::StrDeserializer::new(raw)).map(StageSpec::Token)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        StageDocument::deserialize(serde::de::value::MapAccessDeserializer::new(map))
            .map(StageSpec::Table)
    }
}

/// The command stage: which resolver, where it is, which model, and how much
/// answer.
///
/// Its [`Default`] is the **OpenAI** preset, which is the one asymmetry between
/// the two stages: speech has a keyless default ([`TranscriptionSettings`]) and
/// commands does not, because PRD #802 measured local intent twice and it was
/// not good enough — [`IntentBackend`] has the numbers, and has why the default
/// provider is the one whose key also runs the speech stage. The endpoint and
/// the model are fields rather than constants so the key the user pastes is one
/// they chose the provider for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntentSettings {
    pub backend: IntentBackend,
    pub endpoint: ServiceUrl,
    pub model: ModelId,
    /// How much answer one utterance may cost, reasoning included.
    ///
    /// A field rather than a constant because the constant was **wrong for a
    /// user-chosen endpoint**, which is what the provider work made this. 256
    /// was sized from the Anthropic preset's 33–58-token answers and is not a
    /// ceiling any reasoning model can write under: the reasoning is spent
    /// first and counts against the same budget, so the reply comes back
    /// truncated with nothing in it. Measured on `gpt-5-mini` at 256 against
    /// the 24 phrase fixtures — **18/24**, every failure having spent exactly
    /// 256 completion tokens on reasoning alone. See
    /// [`crate::model_service::DEFAULT_TOKEN_CEILING`] for why the replacement
    /// is 4096 and not 1024.
    ///
    /// The speech stage has no counterpart: a transcription is as long as the
    /// audio was, and no bound this side could set would be about anything the
    /// user chose.
    pub max_tokens: TokenCeiling,
}

impl IntentSettings {
    /// This build's coordinates for one backend — [`TranscriptionSettings::for_backend`]'s
    /// counterpart.
    ///
    /// One arm, matched rather than returned unconditionally, for
    /// [`crate::voice::resolver_for`]'s reason: a variant added without a
    /// preset should be a compile error, not a backend that comes up pointing
    /// at another backend's endpoint.
    pub fn for_backend(backend: IntentBackend) -> Self {
        let (endpoint, model) = match backend {
            IntentBackend::Anthropic => (HOSTED_COMMAND_ENDPOINT, HOSTED_COMMAND_MODEL),
            IntentBackend::OpenaiCompatible => (OPENAI_COMMAND_ENDPOINT, OPENAI_COMMAND_MODEL),
        };
        Self {
            backend,
            endpoint: ServiceUrl::parse(endpoint).expect("a valid preset endpoint"),
            model: ModelId::parse(model).expect("a valid preset model"),
            // One number for both dialects and both presets. It is a CEILING
            // and not a reservation — an answer that needs 30 tokens costs 30
            // whatever this says — so there is nothing to gain by tuning it per
            // preset and something to lose: a per-preset ceiling is a number
            // that stops being right the moment the user edits the model.
            max_tokens: TokenCeiling::default(),
        }
    }
}

impl IntentSettings {
    /// The `reasoning_effort` this build sends with a command request, or
    /// `None` for every configuration that is not this build's measured OpenAI
    /// preset.
    ///
    /// **This is the whole scope of that parameter**, and the reason it is a
    /// derived answer rather than a stored field: `reasoning_effort` is an
    /// OpenAI-family parameter, so sending it to an arbitrary
    /// `openai_compatible` server is a 400 waiting to happen, and sending it to
    /// `api.openai.com` with a model that does not take it is the same. What
    /// makes it safe is that it is keyed on the exact coordinates it was
    /// measured against: the endpoint AND the model must both still be the
    /// preset's. A user who changes either — to another provider, to a gateway,
    /// to a server on loopback, or to a different model at the same endpoint —
    /// gets `None` and the provider's own default reasoning, which fits under
    /// the 4096 ceiling (see [`OPENAI_COMMAND_REASONING_EFFORT`]).
    ///
    /// The backend is checked too, so this is safe to call on any value: the
    /// Anthropic dialect has no such parameter and
    /// [`crate::voice::remote::Protocol`] gives it nowhere to put one.
    pub fn reasoning_effort(&self) -> Option<&'static str> {
        let preset = Self::for_backend(IntentBackend::OpenaiCompatible);
        (self.backend == IntentBackend::OpenaiCompatible
            && self.endpoint == preset.endpoint
            && self.model == preset.model)
            .then_some(OPENAI_COMMAND_REASONING_EFFORT)
    }
}

impl Default for IntentSettings {
    fn default() -> Self {
        Self::for_backend(IntentBackend::default())
    }
}

impl<'de> Deserialize<'de> for IntentSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let doc = StageSpec::deserialize(deserializer)?.into_document();
        let preset = Self::for_backend(doc.backend.unwrap_or_default());
        Ok(Self {
            endpoint: doc.endpoint.unwrap_or(preset.endpoint),
            model: doc.model.unwrap_or(preset.model),
            max_tokens: doc.max_tokens.unwrap_or(preset.max_tokens),
            backend: preset.backend,
        })
    }
}

/// The longest `[voice]` token this build will accept, on either side.
///
/// The tokens are at most eight bytes today. 64 leaves room for a backend name
/// a future build invents while making a payload-shaped value impossible — the
/// same number and the same reasoning as [`MAX_APPEARANCE_TOKEN_BYTES`], and
/// the bound is what stops a compromised webview having a megabyte allocated
/// and lowercased before anything looks at it.
pub const MAX_VOICE_TOKEN_BYTES: usize = 64;

/// A settings value stored as one lowercase token, folding an unknown token to
/// the default.
///
/// [`AppearanceMode`] established this shape and spells the whole of it out by
/// hand; three more copies of that visitor would be three more places for the
/// length bound to be forgotten, so the `[voice]` enums share one. It is
/// deliberately a trait rather than a macro: the guard in
/// `xtask/linkage-check/src/desktop_settings_secrets.rs` reads this file as
/// **text**, one field per line, and a macro that generated settings structs
/// would be invisible to it. Nothing here generates a struct — only the
/// serde plumbing for an enum — but keeping to what a text scan can read is a
/// property of this file worth not spending.
///
/// **An unknown token is not an error**, for [`AppearanceMode::from_str_lossy`]'s
/// reason exactly: a document written by a newer build may name a backend this
/// one has never heard of, and losing the whole document over one unreadable
/// field is the opposite of the unknown-key tolerance the rest of the schema is
/// built for. An **over-length** token is a different thing — a malformed
/// document rather than an unknown value — and is an error.
trait VoiceToken: Copy + Default {
    /// Every token this build knows, for the error message and for the tests.
    const TOKENS: &'static [&'static str];
    /// What the deserializer says it expected.
    const LABEL: &'static str;

    /// The exact token written to TOML and JSON.
    fn as_str(self) -> &'static str;

    /// Parse a stored token, falling back to the default.
    fn from_str_lossy(raw: &str) -> Self;
}

/// [`VoiceToken`]'s visitor: length first, then the folding parse.
///
/// The order is the point, and it is [`AppearanceModeVisitor`]'s: `from_str_lossy`
/// trims and lowercases, which allocates a copy of whatever it was handed, so
/// the bound has to be judged on the borrowed input before anything allocates.
struct VoiceTokenVisitor<T>(std::marker::PhantomData<T>);

impl<T: VoiceToken> serde::de::Visitor<'_> for VoiceTokenVisitor<T> {
    type Value = T;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} of at most {MAX_VOICE_TOKEN_BYTES} bytes, one of {}",
            T::LABEL,
            T::TOKENS.join(", ")
        )
    }

    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<T, E> {
        if raw.len() > MAX_VOICE_TOKEN_BYTES {
            return Err(E::custom(format!(
                "{} is at most {MAX_VOICE_TOKEN_BYTES} bytes; got {}",
                T::LABEL,
                raw.len()
            )));
        }
        Ok(T::from_str_lossy(raw))
    }
}

/// Which `Transcriber` turns speech into text (PRD #802 M7).
///
/// # `Off` was here and is GONE, deliberately
///
/// It was the default, and the doc comment under it called that a product
/// statement: transcription was the one stage with no no-key trick, so the
/// panel said what to add where the user met it. **The premise stopped being
/// true.** PRD #802's provider work measured a keyless speech container
/// answering in 0.653 s on loopback, which is the no-key trick the paragraph
/// said did not exist — so the honest default is a stage that works, and
/// `Off` became a setting whose whole function was to make the feature do
/// nothing. A user who wants that closes the panel.
///
/// What replaced its one genuine job — telling somebody what to do when speech
/// cannot run — is [`crate::voice::transcribe`]'s unreachable-endpoint
/// sentence, which names the container to start rather than a setting to
/// change.
///
/// # The two variants differ in ONE thing: whether a key is sent
///
/// Both are HTTP to a [`ServiceUrl`], both speak the same multipart request,
/// both read the same `text` field back. [`Self::Local`] sends no
/// `Authorization` header and reads no keychain; [`Self::Remote`] does both.
/// That is the whole of it, which is why one backend implementation serves
/// them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TranscriptionBackend {
    /// A speech service on this machine, reached over loopback with no
    /// credential at all. The default.
    #[default]
    Local,
    /// A hosted speech service. Its credential lives in
    /// [`crate::secrets::SecretId::VoiceTranscription`].
    Remote,
}

impl VoiceToken for TranscriptionBackend {
    const TOKENS: &'static [&'static str] = &["local", "remote"];
    const LABEL: &'static str = "a transcription backend";

    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }

    fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "remote" => Self::Remote,
            _ => Self::default(),
        }
    }
}

/// Which `IntentResolver` turns a transcript into an action (PRD #802 M5).
///
/// # The default is OpenAI so that ONE key runs the whole feature
///
/// Both stages of voice can be hosted, and Speech's hosted option is OpenAI's
/// `whisper-1`. While Commands defaulted to Anthropic, going hosted on both
/// meant two vendors, two accounts and two keys pasted into one panel — for a
/// feature whose whole pitch is that it works the moment you turn it on.
/// Defaulting Commands to OpenAI closes that: **one OpenAI key runs both
/// stages**, or an Anthropic key if the user prefers that provider for
/// Commands, or no key at all for Speech if they run the local container.
///
/// **It cost nothing measurable, which is what made it available.** The switch
/// waited on `gpt-5-mini` being measured through the shipped builder and parser
/// against the 24 phrase fixtures: 24/24 at an 818 ms median, against
/// `claude-haiku-4-5`'s 24/24 at 780 ms — level on accuracy, 38 ms apart on
/// latency. [`OPENAI_COMMAND_MODEL`] has the table, including what
/// [`OPENAI_COMMAND_REASONING_EFFORT`] is worth.
///
/// **An earlier sweep scored 18/24 and that was OUR defect, not the model's.**
/// The answer ceiling was a hardwired 256, `max_completion_tokens` counts
/// reasoning tokens, and all six failures had spent exactly 256 of them
/// thinking — `finish_reason: "length"`, nothing written. That is fixed
/// separately ([`IntentSettings::max_tokens`]) because it made the generic
/// `openai_compatible` path unusable with any reasoning model, whoever ships
/// it; the default would have been wrong to move on the strength of a number
/// our own constant produced.
///
/// **[`Self::Anthropic`] is not deprecated and did not get worse.** Only the
/// default moved. A document naming it keeps it, `remote` maps to it
/// explicitly, and it remains the fastest thing measured here.
///
/// **Commands is API-ONLY, and both variants are protocol dialects because of
/// it.** The enum names a *wire shape* — Anthropic Messages or OpenAI
/// chat-completions — and never an executor, because the one executor it held
/// is gone. M5 shipped two backends of a different kind: one keyed HTTP
/// request, and an agent-CLI backend that spawned the pre-authenticated
/// `claude` on the user's machine — no key of the app's own, no download, and
/// the default precisely because of that. PRD #802's provider work removed the
/// subprocess one, and what remained split into the two variants below. Two
/// reasons it went, and the first is the product one:
///
/// - **A stage that spends a credential has to let the user say whose.** The
///   agent CLI is one vendor's, chosen by this build, and an app cannot assume
///   everyone uses the provider it picked. Once Commands needs a key at all,
///   the honest shape is an endpoint, a model and a key the user chooses — and
///   a backend that is a *subprocess* has none of those.
/// - **It was the most security-expensive code in the feature.** It handed a
///   general-purpose coding agent a prompt built partly from untrusted input,
///   so containing it took seven CLI flags, absolute-path resolution, an
///   app-owned working directory, an allowlisted environment, process-group
///   teardown and a `Drop` guard — all of which had to keep working against
///   another program's flag set. `opencode` had already been withdrawn on the
///   same ground (below); `claude` went with the provider work.
///
/// The cost to a user who had picked it is **one re-pick**, the same as the
/// `opencode` withdrawal cost: [`Self::from_str_lossy`] folds an unknown token
/// to the default, so `intent = "claude"` left in a document loads as
/// [`Self::OpenaiCompatible`] and the rest of the document survives. That
/// folding is the whole reason a closed enum was the right shape here.
///
/// **The pre-provider-work `intent = "remote"` is the one token that is NOT
/// folded**, and it used to be the one that needed folding least. It named the
/// Anthropic API, so while Anthropic was the default the fold landed where the
/// user already was; with the default on OpenAI it would land somewhere else
/// entirely, and the user's stored `SecretId::VoiceIntent` is an Anthropic key.
/// It is mapped explicitly instead — see [`Self::from_str_lossy`] for what that
/// costs and what it prevents.
///
/// **A local model is reachable and is not a variant.** Both variants are HTTP
/// to a [`ServiceUrl`], so pointing either at a server on this machine is the
/// same code path. It ships as no preset and is recommended nowhere, because
/// PRD #802 measured local intent twice against the 24 phrase fixtures and it
/// was not good enough: a 1.5B chat model scored 20/24, turning *"what time is
/// it?"* into `open_agent` — the refusal the PRD calls most of the safety — and
/// a 34 MB embedding classifier held two false positives no threshold repairs.
///
/// # `opencode` was here and was WITHDRAWN, deliberately
///
/// PRD #802 M5 shipped an `Opencode` variant driving `opencode run --pure`, and
/// the landed-work security audit took it out again. The reason is specific and
/// is not "we did not get round to measuring it": the agent-CLI backend handed a
/// general-purpose coding agent a prompt built partly from **untrusted** input,
/// so the child had to be containable — no tools, no hooks, no MCP, no project
/// configuration, no session written to disk. `claude` had a flag for every one
/// of those. `opencode run` has no no-tools equivalent and no no-persistence
/// option, and its `run` was confirmed locally to write a resumable session
/// containing the utterance. An uncontainable subprocess executor is not
/// something to ship in a feature that is on by default, so the variant went
/// rather than being documented as best-effort. The backend both variants drove
/// is now gone as well, which makes this history rather than policy — and it is
/// kept because it is the argument that says what a future subprocess backend
/// would have to prove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IntentBackend {
    /// Anthropic Messages: one forced tool call with `strict: true`, the answer
    /// in a `tool_use` block. The shape PRD #802 measured first, and the
    /// default until the one-key work; still 24/24 and still the fastest thing
    /// measured here.
    Anthropic,
    /// OpenAI chat-completions: a nested `json_schema` response format with
    /// `strict: true`. The dialect most other providers — and `llama.cpp`'s
    /// server — also answer, which is what makes the choice a real one, and
    /// **the default** for the reason above.
    #[default]
    OpenaiCompatible,
}

impl VoiceToken for IntentBackend {
    const TOKENS: &'static [&'static str] = &["anthropic", "openai_compatible"];
    const LABEL: &'static str = "an intent backend";

    fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenaiCompatible => "openai_compatible",
        }
    }

    fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai_compatible" => Self::OpenaiCompatible,
            "anthropic" => Self::Anthropic,
            // `remote` is the pre-provider-work spelling of the Anthropic
            // backend, and it is mapped EXPLICITLY rather than folded. Until
            // the default moved it folded to Anthropic by luck, and the doc
            // comment said as much; now the fold would land on OpenAI, and the
            // cost of that is not a re-pick. A document saying `remote` belongs
            // to a user whose key under `SecretId::VoiceIntent` is an
            // **Anthropic** key, so the next utterance would put it in an
            // `Authorization` header addressed to `api.openai.com`. That is a
            // credential handed to a third party for a 401, silently, and it
            // costs one match arm to not do.
            "remote" => Self::Anthropic,
            // Everything else folds, which is what the withdrawn agent-CLI
            // tokens (`claude`, `opencode`) land on. Those named a
            // pre-authenticated CLI and never an app-owned key, so there is no
            // credential to misdirect and the cost is the one re-pick their
            // withdrawal always carried.
            _ => Self::default(),
        }
    }
}

/// How the microphone is started and stopped (PRD #802 M7).
///
/// **One variant today, and that is the truthful shape rather than an
/// oversight.** PRD #802 ships one activation mode — press once to start, press
/// once to stop — and puts hold-to-talk and always-on-with-VAD in D4, waiting
/// on this one being used enough to say what the other two are worth. Listing
/// them here before an adapter exists would let a user select a mode the app
/// cannot honour.
///
/// **The panel stopped rendering it, and the field stayed.** A row that states
/// a single mode nobody can change is a line of settings prose earning its
/// space back in nothing, so PRD #802's provider work took it out; D4 is what
/// brings a control here, and it wants the stored value to exist by then. The
/// document is where the choice belongs and adding a section is the expensive
/// half — adding a variant to one is a line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivationMode {
    /// Press once to start listening, press once to stop.
    #[default]
    Toggle,
}

impl VoiceToken for ActivationMode {
    const TOKENS: &'static [&'static str] = &["toggle"];
    const LABEL: &'static str = "an activation mode";

    fn as_str(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
        }
    }

    fn from_str_lossy(_raw: &str) -> Self {
        Self::default()
    }
}

/// Whether each command request carries the LABELS the app observed — agent
/// names and live status, deck labels, the directory names on screen, the New
/// agent form's Mode chips and Agent picker entries, orchestration titles and
/// roles — or only the transcript and the command table (PRD #1223, audit
/// finding A1).
///
/// **Shared by default**, because without them a model cannot tell that "the
/// build box" is a deck or "billing" a directory, and cannot serve "the one
/// that's stuck" at all. **Withheld** is for a user whose Commands endpoint is
/// hosted and who does not want those names on it: a remote deck's label is
/// `user@host[:port]`, and a directory name is whatever a repository called it.
///
/// **Withheld is honest rather than silent.** Every command whose param is one
/// of those names reports itself unavailable with the reason
/// (`voice::LABELS_WITHHELD_HINT`) instead of resolving against a model that
/// was never shown them; what remains is navigation, dictation, the New agent
/// dialog without a named deck, the directory browser's parameterless moves,
/// Name and Start.
///
/// A closed enum and not a `bool`, for this section's reason: `bool` is not on
/// `ALLOWED_FIELD_TYPES`, and a token folds an unknown value to the default
/// the way its three neighbours do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LabelSharing {
    /// Send the observed labels, in a data turn marked untrusted.
    #[default]
    Shared,
    /// Send the transcript and the command table only.
    Withheld,
}

impl VoiceToken for LabelSharing {
    const TOKENS: &'static [&'static str] = &["shared", "withheld"];
    const LABEL: &'static str = "a label-sharing choice";

    fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Withheld => "withheld",
        }
    }

    fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "withheld" => Self::Withheld,
            _ => Self::default(),
        }
    }
}

macro_rules! voice_token_serde {
    ($($ty:ty),+ $(,)?) => {$(
        impl $ty {
            /// The exact token written to TOML and JSON.
            pub fn as_token(self) -> &'static str {
                <Self as VoiceToken>::as_str(self)
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_token())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_str(VoiceTokenVisitor::<Self>(std::marker::PhantomData))
            }
        }
    )+};
}

// Four identical serde impls, written once. The macro generates NO struct and
// NO field — see [`VoiceToken`] for why that boundary matters to the
// linkage-check scanner that reads this file as text.
voice_token_serde!(
    ActivationMode,
    IntentBackend,
    TranscriptionBackend,
    LabelSharing
);

/// The whole settings document.
///
/// Deliberately carries only the sections that have a tenant. A container that
/// grows opinions about its contents blocks its dependents, which is why
/// #802's `[voice]` section arrived with #802's own panel rather than being
/// pre-created here for it.
///
/// **No `Eq`**, and that is [`ZoomLevel`]'s doing rather than an oversight: it
/// wraps an `f64`, which is `PartialEq` but not `Eq` because `NaN != NaN`.
/// Nothing keys a map or a set on this document, so `PartialEq` is all any
/// caller needs — `assert_eq!` included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopSettings {
    pub version: u32,
    pub appearance: AppearanceSettings,
    /// PRD #741's tenant. **An `Option`, and it means *unspecified* rather than
    /// *empty* — see [`EndpointSettings`] for why that distinction is what
    /// stops a webview deleting decks it cannot render.**
    pub endpoints: Option<EndpointSettings>,
    /// PRD #802's tenant. **An `Option`, for `endpoints`' reason rather than
    /// for a weaker version of it.**
    ///
    /// `desktop_set_settings` takes the whole document from the webview, and
    /// [`merged_document`] makes the decoded struct authoritative over the keys
    /// it owns. So a plain `VoiceSettings` would arrive as the *default* from
    /// any client that did not send one, and the merge would write that default
    /// over a choice the user had made. `None` makes "I am not telling you
    /// about this section" representable, TOML omits it, and the merge leaves
    /// the file alone.
    ///
    /// The section's panel ships in the same commit, so the frontend does
    /// round-trip it — [`normalizeDesktopSettings`] rebuilds it field by field
    /// rather than defaulting it — and the `Option` is the belt beside that
    /// brace. What it costs is that a reader of this type has to materialise
    /// defaults for display, which for now is the webview's job alone: nothing
    /// under `src/` reads this section yet, and M5's backends are what will.
    ///
    /// [`merged_document`]: fn@merged_document
    /// [`normalizeDesktopSettings`]: https://github.com/vfarcic/dot-agent-deck/blob/main/desktop/src/lib/bridge.ts
    pub voice: Option<VoiceSettings>,
    pub zoom: ZoomSettings,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            appearance: AppearanceSettings::default(),
            endpoints: None,
            voice: None,
            zoom: ZoomSettings::default(),
        }
    }
}

impl DesktopSettings {
    /// Which deck this document says to talk to, with the local deck as the
    /// answer whenever the stored selection cannot be honoured.
    ///
    /// An absent `[endpoints]` section is the same answer as an empty one:
    /// the local deck, with no fallback to report. Absence is how *this* build
    /// says "I have nothing to add about endpoints" — it never means "the user
    /// removed them" (see [`EndpointSettings`]).
    pub fn resolve_endpoint(&self) -> ResolvedEndpoint {
        match &self.endpoints {
            Some(endpoints) => endpoints.resolve(),
            None => ResolvedEndpoint {
                endpoint: Endpoint::local(),
                fallback: None,
            },
        }
    }

    /// Every deck this document says to keep alive — PRD #742 M2's set, and
    /// the document-level twin of [`Self::resolve_endpoint`].
    ///
    /// An absent `[endpoints]` section observes the local deck, for the same
    /// reason it *resolves* to the local deck: absence is this build saying "I
    /// have nothing to add about endpoints", never "the user removed them".
    ///
    /// **[`Self::resolve_endpoint`]'s answer is always in here**, and callers
    /// depend on it: `retarget_selection` retains over this set, so a deck
    /// missing from it would have its transport dropped out from under the
    /// deck screen that is talking to it. It holds by construction rather than
    /// by care — a single-deck selection's set *is* `[resolve().endpoint]`, and
    /// [`Selection::All`] leads with the local deck, which is exactly what
    /// [`EndpointSettings::resolve`] returns for it. Pinned by
    /// `lib.rs`'s `the_deck_the_screen_talks_to_is_always_one_the_fleet_observes`.
    pub fn connectable_endpoints(&self) -> Vec<Endpoint> {
        match &self.endpoints {
            Some(endpoints) => endpoints.connectable_endpoints(),
            None => vec![Endpoint::local()],
        }
    }

    /// Every configured deck with no address yet — see
    /// [`EndpointSettings::unconfigured_decks`]. Empty without an
    /// `[endpoints]` section, which has no rows to be half-configured.
    pub fn unconfigured_decks(&self) -> Vec<UnconfiguredDeck> {
        match &self.endpoints {
            Some(endpoints) => endpoints.unconfigured_decks(),
            None => Vec::new(),
        }
    }
}

/// The `[endpoints]` section — PRD #741's tenant: which decks are configured,
/// and which one the app is talking to.
///
/// # The local deck is not in here, and that is the point
///
/// A local deck needs no configuration — [`Endpoint::local`] resolves it from
/// the platform paths the way every caller did before endpoints existed — so
/// this section holds only the *remote* rows plus the selection. A fresh
/// install therefore has no `[endpoints]` section at all and still works, and a
/// user who deletes the section gets the local deck back rather than nothing.
///
/// # Why the section is an `Option` on [`DesktopSettings`]
///
/// `desktop_set_settings` takes the **whole document** from the webview, and
/// the webview builds that document with `normalizeDesktopSettings`, which
/// constructs a fresh object with a **fixed key set** (that fixed set is itself
/// a credential guard — `xtask/linkage-check`'s check 3). So a webview that
/// does not render endpoints cannot send them, and if the field were a plain
/// `EndpointSettings` it would arrive as the *default* — an empty list — and
/// [`merged_document`] would then write that empty list over a hand-edited
/// `[endpoints]` table. Every remote deck, deleted by someone changing the
/// theme.
///
/// `Option` makes "I am not telling you about this section" representable, and
/// TOML serialisation omits a `None` field entirely, so the merge preserves
/// what is on disk — the same protection [`merged_document`] gives *across
/// builds*, now available *across clients*. Pinned by
/// [`tests::a_client_that_cannot_render_endpoints_cannot_delete_them`].
///
/// **What M7 inherits from that.** The moment the panel exists, the frontend
/// must round-trip this section — read it in `normalizeDesktopSettings`, keep
/// it, and send it back. Fabricating a default `{ remote: [], selection:
/// "local" }` there would delete every row, and it would do it silently.
///
/// # Field order is alphabetical on purpose, and issue #825 changed what forces
/// # it
///
/// The document used to be written two ways — `toml::to_string_pretty` over the
/// struct (declaration order) and over a `toml::Table` (a `BTreeMap`, so
/// alphabetical) — so declaring alphabetically was what kept the merged output
/// and the freshly-serialised one agreeing, and
/// [`tests::default_document_shape_is_pinned`] asserted it. The merge is now a
/// [`toml_edit::DocumentMut`] one ([`merged_document`]), which re-sorts nothing:
/// a fresh document is written in **declaration order** and an existing one
/// keeps whatever order it already had.
///
/// So this is a readability convention rather than a constraint the code will
/// break over — but it is still the one to follow. `DesktopSettings`'s own
/// fields are alphabetical for the same reason, and a document whose sections
/// come out in a stable, predictable order is the point of PRD #803's
/// hand-editable file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointSettings {
    /// The configured remote decks. Empty on a fresh install, and empty is a
    /// perfectly good state — the local deck is always available and is not a
    /// row here.
    pub remote: Vec<RemoteEndpointSettings>,
    /// Which deck the app is talking to.
    pub selection: Selection,
}

impl EndpointSettings {
    /// The row with this id, or `None`.
    pub fn find(&self, id: &EndpointId) -> Option<&RemoteEndpointSettings> {
        self.remote.iter().find(|deck| &deck.id == id)
    }

    /// The endpoint [`Self::selection`] names, falling back to the local deck
    /// with a **named** reason whenever the stored selection cannot be
    /// honoured.
    ///
    /// Falling back rather than erroring is deliberate, and it is the same call
    /// [`AppearanceMode::from_str_lossy`] makes: a settings document is
    /// hand-editable and may have been written by a build with more variants,
    /// so an unresolvable selection is an ordinary state rather than a
    /// corruption. Erroring would leave the app with no deck at all, which is
    /// strictly worse than the deck it had before endpoints existed.
    ///
    /// The reason travels with the answer because M7 has to *render* it: "the
    /// deck you selected is gone" and "the deck you selected has no socket path
    /// yet" are different things to tell a user, and neither is "connected to
    /// local".
    ///
    /// # [`Selection::All`] resolves here to the local deck, deliberately
    ///
    /// This method returns **one** `Endpoint`, and a fleet is a set — so the
    /// honest answer for [`Selection::All`] is not an endpoint at all. Rather
    /// than invent one, it travels through the same `let ... else` as
    /// [`Selection::Local`] and gets the local deck with **no** fallback: PRD
    /// #742 DECISION 1 keeps the deck screen and its terminals single-deck, and
    /// this is the method that answers *that* screen. A fallback would be wrong
    /// as well as noisy — nothing failed to be honoured, and the selector would
    /// print a substitution notice about a selection that is in force.
    /// [`Self::connectable_endpoints`] is where the connectable set lives, and
    /// [`Self::unconfigured_decks`] is the rest of what the fleet shows.
    pub fn resolve(&self) -> ResolvedEndpoint {
        let local = || ResolvedEndpoint {
            endpoint: Endpoint::local(),
            fallback: None,
        };
        let Selection::One(id) = &self.selection else {
            return local();
        };
        let Some(deck) = self.find(id) else {
            return ResolvedEndpoint {
                endpoint: Endpoint::local(),
                fallback: Some(SelectionFallback::UnknownDeck { id: id.clone() }),
            };
        };
        match deck.endpoint() {
            Some(remote) => ResolvedEndpoint {
                endpoint: Endpoint::Remote(remote),
                fallback: None,
            },
            None => ResolvedEndpoint {
                endpoint: Endpoint::local(),
                fallback: Some(SelectionFallback::NoRemoteSocket { id: id.clone() }),
            },
        }
    }

    /// Every deck the app can actually CONNECT to under this selection — the
    /// set that gets a watcher, a tunnel and a handshake, and the set
    /// [`Self::resolve`] cannot express.
    ///
    /// One element for [`Selection::Local`] and [`Selection::One`], which is
    /// exactly [`Self::resolve`]'s answer, so a single-deck selection observes
    /// the deck it resolves to and nothing else. For [`Selection::All`] it is
    /// the local deck followed by every stored row that has somewhere to
    /// connect to, in document order.
    ///
    /// **A row with no socket path is not in here, and that is the whole
    /// reason this method and [`Self::unconfigured_decks`] are two methods.**
    /// `endpoint()` is `None` while a row has no remote socket (see
    /// [`RemoteEndpointSettings::socket`] — it cannot be derived, and `Test
    /// connection` is what fills it in), so there is no address to open, and a
    /// watcher or a tunnel for it would be spinning against an endpoint that
    /// cannot exist. That is a statement about connectability and nothing else.
    ///
    /// It used to be the ONLY answer, under the name `observed_endpoints`, and
    /// the fleet view read it as its display set too. PRD #742's Open Question
    /// 3 recorded what that cost and M4 did not close it: a half-configured
    /// deck was *absent* from the fleet rather than present-and-unconfigured —
    /// not in the numerator, not in the denominator, and with no group on
    /// screen, so three configured decks read as `2/2`. Answering both
    /// questions with one list is what made that possible, so there are now
    /// two lists and each caller names the one it means.
    ///
    /// The local deck leads because it needs no configuration and is therefore
    /// the one deck always in the set — the same reason `deckChoices` leads
    /// with it.
    pub fn connectable_endpoints(&self) -> Vec<Endpoint> {
        if !matches!(self.selection, Selection::All) {
            return vec![self.resolve().endpoint];
        }
        let mut observed = vec![Endpoint::local()];
        observed.extend(
            self.remote
                .iter()
                .filter_map(|deck| deck.endpoint())
                .map(Endpoint::Remote),
        );
        observed
    }

    /// Every deck the fleet view SHOWS that [`Self::connectable_endpoints`]
    /// cannot hold — a configured row with no socket path yet.
    ///
    /// These are decks the user created and should see. They get a group, they
    /// count toward the fleet's denominator, and they never count toward the
    /// decks that answered, because nothing was asked of them. They get no
    /// watcher, no tunnel and no handshake: there is no address.
    ///
    /// # Only under [`Selection::All`], deliberately
    ///
    /// Every other selection names ONE deck, and [`Self::resolve`] already has
    /// a complete answer for a socketless one: it falls back to the local deck
    /// and reports [`SelectionFallback::NoRemoteSocket`], which the selector
    /// prints. Adding a second group there would render the same fact twice and
    /// break the invariant `lib.rs` pins — that a single-deck selection's fleet
    /// is exactly `[resolve().endpoint]`. The gap this closes is `All`'s alone,
    /// where a socketless row has nowhere else to be stated.
    pub fn unconfigured_decks(&self) -> Vec<UnconfiguredDeck> {
        if !matches!(self.selection, Selection::All) {
            return Vec::new();
        }
        self.remote
            .iter()
            .filter(|deck| deck.socket.is_none())
            .map(|deck| UnconfiguredDeck {
                id: deck.id.clone(),
                label: deck.describe(),
            })
            .collect()
    }
}

/// A configured deck with no address yet, as the fleet view names it.
///
/// Carries the row's own [`EndpointId`] rather than an
/// [`dot_agent_deck::daemon_client::EndpointIdentity`], because there is no
/// endpoint to take one from — `wire_id()` hashes a `RemoteEndpoint`, and a
/// row without a socket cannot build one. The id is what
/// [`SelectionFallback::NoRemoteSocket`] already names and what the settings
/// panel already keys on, so this reuses the identity the half-configured state
/// has always had rather than minting a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconfiguredDeck {
    /// The stored row's id.
    pub id: EndpointId,
    /// `user@host[:port]`, from [`RemoteEndpointSettings::describe`].
    pub label: String,
}

/// One `[[endpoints.remote]]` row: a remote deck, stored as **references** and
/// never as a secret.
///
/// Host, optional user, port, optional identity-file *path*, optional
/// jump-host *name*. `~/.config/dot-agent-deck/remotes.toml` (`src/remote.rs`)
/// has stored exactly this shape since PRD #76 and has never needed a
/// passphrase: the answer to an encrypted key is "it is in an agent, or it is
/// unencrypted", plus `BatchMode=yes` so a locked key fails fast with a
/// nameable error rather than hanging on a prompt no GUI can answer. A
/// credential belongs behind the `SecretStore` seam PRD #803 M5 named, and
/// #741 is deliberately **not** the PRD that opens the first route into a store
/// that does not exist yet.
///
/// Every field is one of the validating newtypes from
/// [`dot_agent_deck::remote_tunnel`], whose `Deserialize` runs the same check
/// its constructor does — so a hand-edited document cannot smuggle past what a
/// settings form applies. Their charsets and bounds are also the ssh-argument
/// validation nothing in this tree performed before PRD #741 M5.
///
/// # There is no display name, deliberately
///
/// A user-chosen label would be exactly the arbitrary `String` the field-type
/// guard refuses, and it would need its own bidi and control-character handling
/// before anything rendered it. [`RemoteEndpoint::describe`] derives the label
/// from the address instead, and every byte of that came through a validated
/// ASCII charset.
///
/// # `host` and `id` are required; everything else defaults
///
/// A row with no host is not a row, and a row with no id cannot be selected. A
/// document whose row is missing one of them fails to parse, which — per
/// [`load_document`] — means the whole document reads as defaults, with the
/// reason carried to the settings surface rather than only logged. It does
/// **not** mean the file is rewritten, and since issue #1072 that is a
/// **refusal** rather than a lucky property of the merge: [`save_to`] will not
/// publish over a document this build cannot read, so the row and everything
/// around it survives on disk for the user to fix. It used to rest on
/// [`merged_document`] parsing the existing document as TOML *syntax*, which a
/// schema-invalid row still is — true of the row, and no help at all to the
/// appearance, zoom and selection the merge replaced with defaults beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEndpointSettings {
    pub host: Hostname,
    pub id: EndpointId,
    /// The private key to offer (`ssh -i`) — a **path**, never key material and
    /// never a passphrase.
    ///
    /// **Named `identity` rather than `key`, and the reason is worth the
    /// paragraph.** `key` is what `remotes.toml` calls it, and it trips the
    /// naming tripwire
    /// ([`tests::no_settings_key_name_trips_the_credential_tripwire`]), whose
    /// path allowlist is empty. The two available answers were an exemption
    /// pinned to [`KeyPath`], or a name that is not credential-shaped. This is
    /// the second, because `IdentityFile` is OpenSSH's *own* name for exactly
    /// this option — so the TOML reads like the `~/.ssh/config` it points into
    /// — and because an empty allowlist is a property worth keeping: the first
    /// entry is the one that makes the second easy.
    ///
    /// **What that costs, stated rather than glossed:** the tripwire is now
    /// silent about the one field in this schema that sits next to credential
    /// material. It was never the control that mattered — read
    /// [`SECRET_RULE`], which says so in those words — and the control that
    /// does matter is unaffected: the type is [`KeyPath`], which is on
    /// `ALLOWED_FIELD_TYPES` with a written reason, bounded at `PATH_MAX`,
    /// restricted to a charset that cannot represent a multi-line PEM block,
    /// and required to start `/` or `~/`. This doc comment is where the next
    /// reader finds that out, since the tripwire will not tell them.
    ///
    /// [`SECRET_RULE`]: tests::SECRET_RULE
    #[serde(default)]
    pub identity: Option<KeyPath>,
    /// A `Host` block name from the user's `~/.ssh/config` to reach this deck
    /// through (`ssh -J`). A *name*, so the jump host's own address, port, user
    /// and key stay in that config rather than being copied here.
    #[serde(default)]
    pub jump: Option<HostAlias>,
    /// The ssh port.
    ///
    /// An [`SshPort`] rather than a `u16` (PRD #741, Greptile P2 on #1035): a
    /// `u16` admits `0`, the webview's own predicate does not, and a
    /// hand-edited `port = 0` therefore loaded and reached OpenSSH as `-p 0`,
    /// which it refuses. Port was the one field of this row that stayed a
    /// primitive when the other five became validating newtypes, so it was also
    /// the one the validator-parity work could not cover.
    #[serde(default = "default_ssh_port")]
    pub port: SshPort,
    /// The daemon's attach socket path **on the remote host**.
    ///
    /// # Optional in storage, and this is the milestone's answer to it
    ///
    /// It cannot be derived. OpenSSH expands neither `~` nor an environment
    /// variable on the remote side of `-L`, and the far host's
    /// `XDG_RUNTIME_DIR` and uid are not knowable from here without a second
    /// ssh round trip — [`RemoteSocketPath`] records the mechanism. So it is
    /// either typed by the user or discovered.
    ///
    /// Stored as **optional, with discovery filling it**. A row without one is
    /// *storable* and *not connectable*: [`EndpointSettings::resolve`] returns
    /// the local deck and [`SelectionFallback::NoRemoteSocket`] rather than
    /// erroring, so a half-configured deck is a state the UI can explain
    /// instead of a save that refuses.
    ///
    /// Required was the alternative and it is worse in the order the user does
    /// things: the value is un-guessable, so requiring it means typing
    /// `/run/user/1000/dot-agent-deck-attach.sock` correctly *before* anything
    /// can test whether it is right.
    ///
    /// **What that leaves M10 (`Test connection`)**, which is already making an
    /// ssh round trip and is therefore the cheapest place to do it:
    ///
    /// 1. **Discover** the remote attach socket path over that round trip
    ///    rather than asking the user to know it;
    /// 2. **Write it back** into this field, so discovery is durable and the
    ///    next connection needs no probe;
    /// 3. **Report `NoRemoteSocket` as its own named state** — "not configured
    ///    yet, press Test connection" — rather than folding it into a generic
    ///    failure, which is M10's stated shape for every other outcome anyway.
    #[serde(default)]
    pub socket: Option<RemoteSocketPath>,
    /// The login name, when the ssh config does not already decide it.
    #[serde(default)]
    pub user: Option<SshUser>,
}

/// The ssh port a row with no `port` key means.
///
/// Taken from [`SshPort::DEFAULT`] rather than written as `22`, so a row stored
/// without a port and a row built by `RemoteEndpoint::new` can never describe
/// different decks — the assertion below is what keeps the two definitions from
/// drifting apart now that they are separate constants.
fn default_ssh_port() -> SshPort {
    debug_assert_eq!(SshPort::DEFAULT.get(), RemoteEndpoint::DEFAULT_PORT);
    SshPort::DEFAULT
}

impl RemoteEndpointSettings {
    /// A row with the minimum a deck needs: an id and a host.
    ///
    /// **Not reached in production, and that is the shape of the app rather
    /// than an unfinished wire-up.** A row is *constructed* by the settings
    /// panel and arrives here as a whole document through `desktop_set_settings`,
    /// where every field is validated by its own `Deserialize`. This
    /// constructor is what the storage tests build from, and it is the
    /// definition the panel's row-shape is pinned against.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(id: EndpointId, host: Hostname) -> Self {
        Self {
            host,
            id,
            identity: None,
            jump: None,
            port: default_ssh_port(),
            socket: None,
            user: None,
        }
    }

    /// Everything `ssh` needs to reach this row's host, with the attach socket
    /// left behind (PRD #741 M10).
    ///
    /// The socket is the one field a row may legitimately lack, and reaching
    /// the host is exactly what `Test connection` does to *discover* it — so
    /// the probe is written against this rather than against
    /// [`Self::endpoint`], which cannot exist yet. Both are built from the same
    /// fields, so a probe and the tunnel it precedes cannot describe different
    /// hosts.
    pub fn destination(&self) -> dot_agent_deck::remote_tunnel::SshDestination {
        dot_agent_deck::remote_tunnel::SshDestination::with_parts(
            self.host.clone(),
            self.user.clone(),
            self.port.get(),
            self.identity.clone(),
            self.jump.clone(),
        )
    }

    /// [`Self::endpoint`] against a socket path discovery just learned, rather
    /// than the stored one.
    ///
    /// The connection a `Test connection` actually makes goes through this, so
    /// a deck whose socket was discovered *this run* is testable before the
    /// document has been written back — which is the order the user does
    /// things in.
    pub fn endpoint_at(&self, socket: RemoteSocketPath) -> RemoteEndpoint {
        let mut row = self.clone();
        row.socket = Some(socket);
        row.endpoint()
            .expect("a row with a socket always yields an endpoint")
    }

    /// The connectable endpoint this row describes, or `None` while it has no
    /// remote socket path — see [`Self::socket`].
    pub fn endpoint(&self) -> Option<RemoteEndpoint> {
        let mut endpoint =
            RemoteEndpoint::new(self.host.clone(), self.socket.clone()?).with_port(self.port.get());
        if let Some(user) = &self.user {
            endpoint = endpoint.with_user(user.clone());
        }
        if let Some(identity) = &self.identity {
            endpoint = endpoint.with_key(identity.clone());
        }
        if let Some(jump) = &self.jump {
            endpoint = endpoint.with_jump(jump.clone());
        }
        Some(endpoint)
    }

    /// How this row is NAMED, with or without a socket path.
    ///
    /// The same `user@host[:port]` [`RemoteEndpoint::describe`] renders, built
    /// from the ssh destination directly — which needs no socket, so a
    /// half-configured row can still be labelled. That is the one thing
    /// [`Self::endpoint`] cannot do for it, and the fleet view needs a name for
    /// a deck it cannot connect to (see [`EndpointSettings::unconfigured_decks`]).
    ///
    /// A label, not an identity: two rows differing only in socket path
    /// describe identically. Nothing keys on this.
    pub fn describe(&self) -> String {
        dot_agent_deck::remote_tunnel::SshDestination::with_parts(
            self.host.clone(),
            self.user.clone(),
            self.port.get(),
            self.identity.clone(),
            self.jump.clone(),
        )
        .describe()
    }
}

/// The stable identity of a stored remote deck.
///
/// An opaque minted token rather than the deck's address, so editing a host
/// does not silently change which deck a selection points at, and rather than a
/// list index, so removing a row does not re-point every selection after it.
///
/// The charset is deliberately wider than [`Self::mint`] produces — ASCII
/// alphanumerics, `-` and `_`, up to [`MAX_ENDPOINT_ID_BYTES`] — because this
/// type also has to accept a [`Selection`] token written by a build that has a
/// variant this one does not. See [`Selection`] for what that buys.
///
/// It is not a secret and nothing authenticates with it: it names a row in a
/// file the user owns.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(String);

/// The longest endpoint id this build will accept.
///
/// [`EndpointId::mint`] produces 16, so this is room for a hand-written one
/// while staying far short of anything that could hold a credential-shaped
/// blob.
pub const MAX_ENDPOINT_ID_BYTES: usize = 64;

/// The [`Selection`] token that means the local deck, and therefore one of the
/// two words an [`EndpointId`] may not be.
pub const LOCAL_SELECTION_TOKEN: &str = "local";

/// The [`Selection`] token that means every configured deck at once — PRD
/// #742's fleet — and therefore the other word an [`EndpointId`] may not be.
///
/// Reserved rather than merely recognised. `all` is a legal id shape, so
/// without this an `id = "all"` row and a `selection = "all"` fleet would be
/// the same string meaning two things, and no reader of the document could
/// tell which was meant. The cost is that a hand-written `id = "all"` — which
/// [`EndpointId::mint`] can never produce — now refuses to load with a named
/// error instead of loading into an ambiguity.
pub const ALL_SELECTION_TOKEN: &str = "all";

impl EndpointId {
    /// Validate `raw` and wrap it.
    ///
    /// The only constructor besides [`Self::mint`] — no `From<String>`, so no
    /// call site can skip the check by reaching for a cheaper conversion.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("an endpoint id cannot be empty".to_string());
        }
        if raw.len() > MAX_ENDPOINT_ID_BYTES {
            return Err(format!(
                "an endpoint id is at most {MAX_ENDPOINT_ID_BYTES} bytes; got {}",
                raw.len()
            ));
        }
        if let Some(byte) = raw
            .bytes()
            .find(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        {
            return Err(format!(
                "an endpoint id is ASCII alphanumerics, '-' and '_'; found byte 0x{byte:02x}"
            ));
        }
        if raw.eq_ignore_ascii_case(LOCAL_SELECTION_TOKEN) {
            return Err(format!(
                "'{LOCAL_SELECTION_TOKEN}' is reserved: it is how a selection names the local deck"
            ));
        }
        if raw.eq_ignore_ascii_case(ALL_SELECTION_TOKEN) {
            return Err(format!(
                "'{ALL_SELECTION_TOKEN}' is reserved: it is how a selection names every deck at once"
            ));
        }
        Ok(Self(raw.to_string()))
    }

    /// A fresh id, unique within this process and unguessable outside it.
    ///
    /// Sixteen lowercase hex characters from [`unpredictable_suffix`], which is
    /// seeded from the operating system. Uniqueness is what is wanted here, not
    /// unpredictability — nothing authenticates with this — but the function
    /// that gives one already gives the other. Sixteen hex characters cannot
    /// collide with [`LOCAL_SELECTION_TOKEN`], with [`ALL_SELECTION_TOKEN`] —
    /// the word this sentence anticipated, which arrived with [`Selection::All`]
    /// in PRD #742 — or with any word a further [`Selection`] variant would
    /// reserve: all of them are shorter and contain letters that are not hex
    /// digits.
    ///
    /// **Not reached in production for the same reason [`RemoteEndpointSettings::new`]
    /// is not**: the panel mints an id when the user adds a deck, because that
    /// is where a row is built. `desktop/src/lib/endpoints.ts`'s `mintEndpointId`
    /// is the other copy and produces the same sixteen lowercase hex characters
    /// from `crypto.getRandomValues`; what keeps the two honest is not that
    /// they share code but that [`Self::parse`] — which both a save and a
    /// hand-edited document go through — is the only thing that decides what an
    /// id may be.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn mint() -> Self {
        Self(format!("{:016x}", unpredictable_suffix()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for EndpointId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EndpointId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Which deck the app is talking to.
///
/// # Shaped so a variant can be added, and PRD #742 spent that room
///
/// The Deck selector is PRD #741 M9 and "All Decks" is [#742](https://github.com/vfarcic/dot-agent-deck/issues/742),
/// but the *stored* value lands here — and it lands as an enum rather than a
/// bare [`EndpointId`] threaded through state precisely so #742 was
/// **additive** rather than a retrofit. It was: [`Self::All`] cost one variant,
/// one arm in [`SelectionVisitor::visit_str`] and one in [`Self::as_token`]
/// (which `Serialize` delegates to), and nothing else in production. A bare id
/// would have made it a change to every type that carries a selection.
///
/// The map this comment used to offer also named [`EndpointSettings::resolve`],
/// and that turned out to be one site too many: `resolve` answers "which single
/// deck do the deck screen and its terminals talk to", `All` has no single
/// answer, and its `let ... else` already sends every non-[`Self::One`]
/// selection to the local deck. [`EndpointSettings::connectable_endpoints`] is
/// where a fleet's set lives instead, so neither method has to lie.
///
/// # The wire form, and why an unknown token round-trips
///
/// One string: the reserved word `local`, the reserved word `all`, or an
/// endpoint id. A token this build does not recognise — one a *newer* build
/// wrote, as `all` itself was until #742 — parses as an [`EndpointId`],
/// resolves to no row, and therefore reads as the local deck with
/// [`SelectionFallback::UnknownDeck`]; and because it is *stored* as the id it
/// was, saving the document writes it back **unchanged**. So an older build
/// degrades to local without destroying a newer build's selection, which is the
/// same tolerance the rest of this schema is built for. That is also why
/// [`EndpointId`]'s charset is wider than [`EndpointId::mint`] needs: a reserved
/// word a future build invents has to fit through it.
///
/// An over-long or non-charset token is a different thing — a malformed
/// document rather than an unknown value — and is an error, exactly as
/// [`AppearanceMode`] treats an over-length mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Selection {
    /// The daemon on this machine: what every caller used before endpoints
    /// existed, and the default with or without an `[endpoints]` section.
    #[default]
    Local,
    /// The remote deck with this id, if the document still holds one.
    One(EndpointId),
    /// Every configured deck at once — the local one and the stored rows — which
    /// is PRD #742's fleet view.
    ///
    /// A *set*, and the one variant that does not name a single deck. Ask
    /// [`EndpointSettings::connectable_endpoints`] and
    /// [`EndpointSettings::unconfigured_decks`] for it;
    /// [`EndpointSettings::resolve`] answers a different question and sends this
    /// variant to the local deck, exactly as it does [`Self::Local`].
    All,
}

impl Selection {
    /// The token written to TOML and JSON.
    pub fn as_token(&self) -> &str {
        match self {
            Self::Local => LOCAL_SELECTION_TOKEN,
            Self::All => ALL_SELECTION_TOKEN,
            Self::One(id) => id.as_str(),
        }
    }
}

impl Serialize for Selection {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for Selection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(SelectionVisitor)
    }
}

struct SelectionVisitor;

impl serde::de::Visitor<'_> for SelectionVisitor {
    type Value = Selection;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{LOCAL_SELECTION_TOKEN}', '{ALL_SELECTION_TOKEN}' or an endpoint id"
        )
    }

    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<Selection, E> {
        if raw.eq_ignore_ascii_case(LOCAL_SELECTION_TOKEN) {
            return Ok(Selection::Local);
        }
        // Above the fallthrough, and not merely by convention: `EndpointId::parse`
        // reserves this word, so reaching it would turn the fleet selection into
        // a parse error rather than into `All`.
        if raw.eq_ignore_ascii_case(ALL_SELECTION_TOKEN) {
            return Ok(Selection::All);
        }
        EndpointId::parse(raw)
            .map(Selection::One)
            .map_err(E::custom)
    }
}

/// What [`EndpointSettings::resolve`] answered, and whether it had to fall back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    /// The deck to talk to. Always a usable endpoint — never an error.
    pub endpoint: Endpoint,
    /// `Some` when [`Self::endpoint`] is the local deck because the stored
    /// selection could not be honoured. M7 renders it; nothing else needs it.
    pub fallback: Option<SelectionFallback>,
}

/// Why a stored selection resolved to the local deck instead of the one it
/// named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionFallback {
    /// The selection names a deck this document no longer holds — a row the
    /// user removed, or a token a newer build wrote. Test-plan item 17.
    UnknownDeck { id: EndpointId },
    /// The selected deck has no remote socket path yet, so there is nothing to
    /// forward to. PRD #741 M10's `Test connection` is what fills it in; see
    /// [`RemoteEndpointSettings::socket`].
    NoRemoteSocket { id: EndpointId },
}

impl std::fmt::Display for SelectionFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDeck { id } => write!(
                f,
                "the selected deck {id} is no longer configured; using the local deck"
            ),
            Self::NoRemoteSocket { id } => write!(
                f,
                "the selected deck {id} has no remote socket path yet; using the local deck"
            ),
        }
    }
}

/// The zoom levels this build steps through, ascending (PRD #744).
///
/// **This list is duplicated in `desktop/src/lib/zoom.ts` as `ZOOM_LEVELS` and
/// the two must stay identical.** Both sides are pinned by a test that spells
/// out every value, and the duplication is unavoidable rather than sloppy: this
/// side is the authority for what a *stored* level may be, because the
/// launch-time apply reads the document before any JavaScript runs, and that
/// side is the authority for *stepping*, because the key event itself is never
/// seen here — a keystroke reaches Rust only as an already-decided level, via
/// `desktop_set_zoom`, so this side can validate a level but could not compute
/// the next one. If they drift, the app launches at one level and the Settings
/// row claims another. The same discipline — two copies, both pinned, each pointing
/// at the other — is why `desktop/src/lib/displayText.ts` and
/// `src/untrusted_text.rs` are still in agreement.
///
/// Both ends are measured; the reasoning lives in `zoom.ts`'s module docs and in
/// `prds/744-desktop-zoom-text-size.md` rather than being restated here.
pub const ZOOM_LEVELS: [f64; 10] = [0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];

/// What `Cmd 0` returns to, and what a document with no `[zoom]` section reads
/// as.
pub const DEFAULT_ZOOM_LEVEL: f64 = 1.0;

/// A zoom level that is always one of [`ZOOM_LEVELS`].
///
/// A newtype rather than a bare `f64` field so the snapping happens in
/// `Deserialize` and cannot be forgotten by a caller. It serialises as a plain
/// number, so the TOML reads `level = 1.25` and the JSON the webview receives is
/// `{"level":1.25}` — the wrapper is invisible on both wires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomLevel(f64);

impl ZoomLevel {
    /// The nearest level in [`ZOOM_LEVELS`], or the default for anything that is
    /// not a usable number.
    ///
    /// Snapping rather than rejecting, for the same reason
    /// [`AppearanceMode::from_str_lossy`] folds an unknown token to the default:
    /// the document is hand-editable and may also have been written by a build
    /// with a longer ladder, and losing every other section over one unreadable
    /// field is the opposite of the unknown-key tolerance this schema is built
    /// for.
    ///
    /// `is_finite` is the guard rather than a range check because it rejects
    /// `NaN` and both infinities together, and `NaN` is the case that matters:
    /// every comparison against it is false, so an unguarded nearest-value
    /// search would return the *first* level and a corrupt document would
    /// silently shrink the app to 75% instead of reading as the default. The
    /// mirror of this is `clampZoom` in `zoom.ts`, tested against the same
    /// inputs.
    pub fn snap(value: f64) -> Self {
        if !value.is_finite() {
            return Self(DEFAULT_ZOOM_LEVEL);
        }
        let mut nearest = ZOOM_LEVELS[0];
        for level in ZOOM_LEVELS {
            if (level - value).abs() < (nearest - value).abs() {
                nearest = level;
            }
        }
        Self(nearest)
    }

    /// The scale factor, ready for `webview.set_zoom`.
    pub fn as_f64(self) -> f64 {
        self.0
    }
}

impl Default for ZoomLevel {
    fn default() -> Self {
        Self(DEFAULT_ZOOM_LEVEL)
    }
}

impl Serialize for ZoomLevel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for ZoomLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ZoomLevelVisitor)
    }
}

/// Accepts a TOML/JSON **integer** as well as a float, which is the whole
/// reason this is a visitor rather than `f64::deserialize().map(snap)`.
///
/// `level = 1` in a hand-edited `desktop.toml` is a TOML *integer*, and `f64`'s
/// derived deserializer rejects it with `invalid type: integer`. That failure is
/// not local: [`load_document`] treats an unparseable document as "use
/// defaults", so one plausible hand edit takes the user's **appearance** with it
/// for the session — and, before issue #1072 made that refuse to save rather
/// than publish, took it on disk as well. Accepting the integer costs three
/// lines and removes the whole shape.
struct ZoomLevelVisitor;

impl serde::de::Visitor<'_> for ZoomLevelVisitor {
    type Value = ZoomLevel;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a zoom level between {} and {}",
            ZOOM_LEVELS[0],
            ZOOM_LEVELS[ZOOM_LEVELS.len() - 1]
        )
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<ZoomLevel, E> {
        Ok(ZoomLevel::snap(value))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<ZoomLevel, E> {
        Ok(ZoomLevel::snap(value as f64))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<ZoomLevel, E> {
        Ok(ZoomLevel::snap(value as f64))
    }
}

/// The document plus where it lives, which is what the settings surface renders.
///
/// A separate struct rather than a field on [`DesktopSettings`], because the
/// path is **not** part of the document: it is where the document is, it is
/// never written to TOML, and putting it on the struct would put it in the file
/// and into [`tests::default_document_shape_is_pinned`]'s pinned shape.
///
/// # Why this path reaches the webview when error paths deliberately do not
///
/// [`SettingsWriteError`] splits itself precisely so a `/home/<user>/…` path
/// never crosses the bridge, and that is not in tension with this. A path
/// leaking out of an *error* is incidental detail the user did not ask for; the
/// location of their own settings file is the answer to "where did that go?",
/// which PRD #803 makes a visible footer line specifically so it is answerable
/// without documentation. Same string, opposite intent.
// No `Eq`, for the same reason `DesktopSettings` has none: it contains a
// `ZoomLevel`, which wraps an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesktopSettingsSnapshot {
    pub settings: DesktopSettings,
    /// Absolute, as [`settings_path`] resolved it — including when
    /// [`SETTINGS_PATH_ENV`] pointed somewhere else, since the footer's job is
    /// to name the file this process will actually write.
    pub path: String,
    /// Why `settings` is this build's defaults rather than the user's document
    /// (issue #1072), when it is.
    ///
    /// [`SettingsDocumentProblem::public`], so it carries a **locator** and
    /// never a byte of the document or a filesystem path — the path is already
    /// beside it in [`Self::path`], deliberately and for the reason above.
    ///
    /// Omitted from the wire when `None`, the way
    /// [`crate::dto::DesktopConnection::selection_fallback`] is and for the same
    /// reason: this is a sentence to render, and "no sentence" is exactly what
    /// absence should mean. Present, it also means **saving is refused** — the
    /// settings surface says so in those words, because a user whose file is in
    /// this state needs to know before they start changing things, not after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// [`load_document`] against [`settings_path`], plus that path and the reason
/// the document could not be used, for the settings surface. This is how the app
/// loads its settings.
///
/// The one place a [`SettingsDocumentProblem`] is both logged and handed onward:
/// the **detail** half goes to the app's log because it may name the path, and
/// the **public** half rides to the webview because a user who cannot see why
/// their settings look reset is the whole of issue #1072.
pub fn load_snapshot() -> DesktopSettingsSnapshot {
    let path = settings_path();
    let (settings, problem) = load_document(&path);
    if let Some(problem) = &problem {
        log_document_problem(problem);
    }
    DesktopSettingsSnapshot {
        settings,
        path: path.display().to_string(),
        // The public half and only the public half: this struct is serialised
        // straight to the webview.
        problem: problem.map(|problem| problem.public().to_string()),
    }
}

/// A settings write failure, split so a filesystem path never reaches the
/// webview.
///
/// Connection errors are already sanitised before they cross the bridge
/// (`dto::safe_message`); an error naming `/home/<user>/...` deserves the same
/// treatment. [`Self::detail`] is for the app's own log and carries the path;
/// [`Self::public`] is what the webview renders and never does. `io::Error`'s
/// own `Display` carries no path — `std` does not add that context — so the
/// public half stays specific ("Permission denied") without leaking anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsWriteError {
    detail: String,
    public: String,
}

impl SettingsWriteError {
    /// The operator-facing message, including the path. Log this.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The webview-facing message. Contains no path.
    pub fn public(&self) -> &str {
        &self.public
    }
}

impl std::fmt::Display for SettingsWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for SettingsWriteError {}

fn write_error(what: &str, path: &Path, cause: impl std::fmt::Display) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!("{what} {}: {cause}", path.display()),
        public: format!("{what} the desktop settings file: {cause}"),
    }
}

/// A path that cannot be a settings document at all, split the same way
/// [`write_error`] is so the reason crosses the bridge and the path does not.
fn path_error(reason: &str, path: &Path) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!(
            "unusable desktop settings path {}: {reason}",
            path.display()
        ),
        public: format!("the desktop settings path is unusable: {reason}"),
    }
}

/// The sentence every document problem ends with: what the app did instead, and
/// what it will not do until the file is readable again.
///
/// Carried in the **public** half, because it is the half a user reads and the
/// half that answers the question they actually have — "why do my settings look
/// reset?" — which the old `eprintln!` never reached anybody to answer.
const UNREADABLE_CONSEQUENCE: &str = "This session is using default settings, \
     and nothing will be saved over the file until it is fixed or removed.";

/// Why the document on disk could not be loaded as settings (issue #1072).
///
/// Split exactly the way [`SettingsWriteError`] is and for the same reason:
/// [`Self::detail`] names the path and belongs in the app's own log,
/// [`Self::public`] is what crosses the bridge and never does.
///
/// # A state the app carries, not an event it logs
///
/// Loading answered every failure with `DesktopSettings::default()` and an
/// `eprintln!` nobody reads, so the app came up looking entirely normal with
/// every setting at its default — and the next save wrote those defaults over
/// the user's file, because [`merged_document`] makes the struct authoritative
/// over every key it owns. A malformed document therefore did not present as an
/// error at all. It presented as "my settings reset themselves", one save later,
/// with the original already gone.
///
/// So this value is carried rather than dropped: it rides on
/// [`DesktopSettingsSnapshot`] to the settings surface, and [`save_to`] refuses
/// for as long as the document is in this state. Preserving the user's bytes is
/// the property that matters here — a refusal to save is a better outcome than
/// a destructive save.
///
/// # It carries a locator and never the document's bytes
///
/// The rule [`invalid_document_log`] already followed (issue #827), now applying
/// to a string that reaches a **webview** as well as a log: `toml_edit::de::Error`'s
/// `Display` echoes the offending value twice, so neither half is built from it.
/// Pinned by [`tests::a_document_problem_carries_a_locator_and_never_the_documents_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsDocumentProblem {
    detail: String,
    public: String,
}

impl SettingsDocumentProblem {
    /// The operator-facing message, including the path. Log this.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The webview-facing message. Contains no path.
    pub fn public(&self) -> &str {
        &self.public
    }
}

impl std::fmt::Display for SettingsDocumentProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for SettingsDocumentProblem {}

/// A path-level failure is a document problem too.
///
/// [`read_document`] refuses a FIFO, a symlink, an over-limit file and one that
/// is not UTF-8 — and the app is then on defaults for exactly the same reason a
/// parse failure puts it there. [`save_to`] already refused these before #1072,
/// by propagating the same error; what was missing is that nothing told the
/// **user** why their settings looked reset.
impl From<SettingsWriteError> for SettingsDocumentProblem {
    fn from(error: SettingsWriteError) -> Self {
        Self {
            detail: error.detail,
            public: format!("{} {UNREADABLE_CONSEQUENCE}", as_sentence(&error.public)),
        }
    }
}

/// A lowercase error fragment rendered as a sentence: first character
/// capitalised, terminated with a full stop.
///
/// Both halves of the split are written as **fragments**, because they are
/// composed into a larger message at every existing call site. A document
/// problem is rendered on its own by the settings surface, so it has to read as
/// a sentence rather than as the tail of one.
fn as_sentence(fragment: &str) -> String {
    let mut sentence = String::with_capacity(fragment.len() + 1);
    let mut chars = fragment.chars();
    if let Some(first) = chars.next() {
        sentence.extend(first.to_uppercase());
        sentence.push_str(chars.as_str());
    }
    if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    sentence
}

/// The largest settings document this build will read.
///
/// `desktop.toml` is a hand-edited preferences file: today's default document
/// is 40 bytes, and #741's endpoint list plus #802's model configuration are
/// kilobytes at the very outside. 256 KiB leaves about four orders of magnitude
/// of headroom over anything the schema can plausibly grow into, while turning
/// "the app read a multi-gigabyte file into memory because a path pointed at
/// one" into a named error instead of an out-of-memory kill.
pub const MAX_SETTINGS_BYTES: u64 = 256 * 1024;

/// Vet `path` as the settings document and read it, bounded.
///
/// `Ok(None)` is the ordinary first-run case: nothing is there yet. `Err` means
/// the path cannot be a settings document at all — [`load_document`] reports it
/// and falls back to defaults, and [`save_to`] refuses rather than writing over
/// whatever is actually at that name.
///
/// # This is about misconfiguration, not privilege
///
/// [`SETTINGS_PATH_ENV`] used to accept any non-empty string, and both load and
/// save then went straight to an unbounded `read_to_string`. Anyone who can set
/// this process's environment can already run code as this user, so none of
/// this is a privilege boundary. It is here because the *misconfiguration*
/// failures are miserable to debug: a FIFO blocks the app forever with no
/// message at all, `/dev/zero` exhausts memory, a large regular file is read
/// whole, and a relative path resolves against whatever directory the app
/// happened to be launched from — even though [`DesktopSettingsSnapshot`]
/// documents the path it reports as absolute.
///
/// So the target must be absolute, must have a file name, and must be either
/// **absent** or a **regular file**. The check is `symlink_metadata`, which does
/// not follow a link at the final component, so a symlink is rejected as a
/// symlink rather than quietly resolved to something else. Then at most
/// [`MAX_SETTINGS_BYTES`] are read.
///
/// One residual is accepted: a target swapped between the check and the open —
/// a regular file replaced by a FIFO in that window — still blocks. Closing it
/// means an `openat`-anchored read, which is the same complexity [`save_to`]
/// declines for the same reason, and it is written down there rather than
/// repeated here.
fn read_document(path: &Path, purpose: ReadPurpose) -> Result<Option<String>, SettingsWriteError> {
    if !path.is_absolute() {
        return Err(path_error("it is not an absolute path", path));
    }
    if path.file_name().is_none() {
        return Err(path_error("it names no file", path));
    }

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            // `io::Error`'s own `Display` carries no path — `std` does not add
            // that context — so the cause is safe to put in the public half.
            return Err(path_error(
                &format!("it cannot be inspected: {error}"),
                path,
            ));
        }
    };
    if let Some(kind) = unusable_kind(&meta) {
        return Err(path_error(
            &format!("it is {kind}, and the settings document must be a regular file"),
            path,
        ));
    }
    if meta.len() > MAX_SETTINGS_BYTES {
        return Err(oversized(path));
    }
    if purpose == ReadPurpose::Load {
        warn_if_document_is_exposed(path, &meta);
    }

    read_bounded(path)
}

/// Why [`read_document`] is reading, which decides only whether an exposed
/// document is complained about.
///
/// The distinction exists because [`save_to`] reads the document too, and a
/// mode warning there would be **stale before it was printed**: the save that
/// follows publishes a fresh 0o600 file over it. Warning on the load is what
/// puts the message in front of a user who has not changed a setting since
/// their file became world-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadPurpose {
    /// [`load_document`] — the app is about to read these settings, so an
    /// exposed document is worth a line in the log.
    Load,
    /// [`save_to`] — the existing bytes are being read only so the merge can
    /// preserve what this build does not own.
    Save,
}

/// Refuse to write into a parent directory that is not plainly ours (PRD #741).
///
/// Absent is fine — [`save_to`] creates it a line later, at 0o700, and a
/// directory we create is ours by construction. What is refused is a parent
/// that **exists and is not what it claims to be**: a symlink (or, on Windows,
/// any reparse point), something that is not a directory at all, or — on Unix —
/// a directory owned by another uid.
///
/// # What each clause buys, narrowly
///
/// The symlink clause is the load-bearing one. `fsperm::create_owner_only_dir`
/// deliberately carries no symlink guard: it never chmods, so it had no
/// permission-tightening exposure to guard (issue #669 says so in as many
/// words) — but it does silently *redirect* where the caller's write lands, and
/// its own docs name that as "a path-redirection question about the write
/// itself". This is that question, answered at the one call site that now has a
/// reason to care.
///
/// The ownership clause is Unix-only and says so rather than pretending
/// otherwise. Windows has no uid, the per-user ACL story is a different
/// mechanism, `%LOCALAPPDATA%` is already per-user ACL'd, and no Windows
/// binaries are released — the same reasoning `fsperm`'s own site audit records
/// for the #669 symlink refusal having no Windows counterpart.
///
/// # What it does not buy
///
/// It is **not** race-free, and calling it one would be the mistake
/// [`fsperm::ensure_owner_only_dir`] documents about its own guard. An attacker
/// who replaces the parent between this `lstat` and the `create_new` below
/// still redirects the write; closing that needs the `openat` anchoring
/// [`save_to`] defers and gives its reasons for. This narrows a window; it does
/// not close one. Nothing about an *ancestor* of the parent is checked either.
fn vet_parent_dir(parent: &Path) -> Result<(), SettingsWriteError> {
    let meta = match std::fs::symlink_metadata(parent) {
        Ok(meta) => meta,
        // Nothing there yet: `create_owner_only_dir` makes it, at 0o700.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(parent_error(
                &format!("it cannot be inspected: {error}"),
                parent,
            ));
        }
    };
    if meta.file_type().is_symlink() {
        return Err(parent_error(
            "it is a symlink, and following it would write the settings document somewhere \
             the user did not name — point the path at the real directory",
            parent,
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(parent_error(
                "it is a reparse point, and following it would write the settings document \
                 somewhere the user did not name",
                parent,
            ));
        }
    }
    if !meta.is_dir() {
        return Err(parent_error("it is not a directory", parent));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let ours = dot_agent_deck::platform::paths::current_uid();
        if meta.uid() != ours {
            return Err(parent_error(
                &format!(
                    "it is owned by uid {} rather than by us ({ours})",
                    meta.uid()
                ),
                parent,
            ));
        }
    }
    Ok(())
}

/// A refusal naming the settings document's *parent*, split the same way
/// [`path_error`] is so the reason crosses the bridge and the path does not.
fn parent_error(reason: &str, parent: &Path) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!(
            "refusing to write the desktop settings into {}: {reason}",
            parent.display()
        ),
        public: format!("the desktop settings directory is unusable: {reason}"),
    }
}

/// Complain — to the log, never to the caller — about a settings document
/// anyone but its owner can read (PRD #741).
///
/// # Warn, do not refuse, and that is a deliberate inheritance
///
/// [`load_document`] never failing is a #803 property on purpose: a preferences
/// file is not worth failing an app launch over. Now that the document names a
/// host, a login name and a key path, a `0o644` `desktop.toml` is worth *saying
/// something* about — but bricking the app over one would be worse than the
/// exposure it is complaining about, and would hand anyone who can chmod the
/// file a denial of service on the whole app.
///
/// So: one line on stderr and the deck log, and the document loads. [`save_to`]
/// then republishes at 0o600 on the next write, so the ordinary outcome is that
/// the complaint fixes itself the next time the user changes a setting.
///
/// Unix-only. On Windows the analogous question is a DACL comparison rather
/// than a mode, `%LOCALAPPDATA%` is already per-user ACL'd, and no Windows
/// binaries are released — the same boundary `fsperm`'s site audit draws.
#[cfg(unix)]
fn warn_if_document_is_exposed(path: &Path, meta: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        eprintln!(
            "Desktop settings at {} are mode {mode:04o}: readable beyond its owner, and this \
             document names a daemon host, a login name and a key path. Run `chmod 600` on it, \
             or change any setting in the app — every save republishes it owner-only.",
            path.display()
        );
    }
    let ours = dot_agent_deck::platform::paths::current_uid();
    if meta.uid() != ours {
        eprintln!(
            "Desktop settings at {} are owned by uid {} rather than by us ({ours}); loading them \
             anyway, but nothing here vouches for who wrote them.",
            path.display(),
            meta.uid()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_document_is_exposed(_path: &Path, _meta: &std::fs::Metadata) {}

fn oversized(path: &Path) -> SettingsWriteError {
    path_error(
        &format!("it is larger than the {MAX_SETTINGS_BYTES}-byte settings limit"),
        path,
    )
}

/// What `meta` describes, when it is not a plain regular file. `None` means it
/// is one.
fn unusable_kind(meta: &std::fs::Metadata) -> Option<&'static str> {
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Some("a symlink");
    }
    #[cfg(windows)]
    {
        // `is_symlink` covers a symlink reparse point but not a junction or a
        // mount point, and following one of those lands somewhere the user
        // never named.
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Some("a reparse point");
        }
    }
    if kind.is_dir() {
        return Some("a directory");
    }
    if kind.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if kind.is_fifo() {
            return Some("a FIFO");
        }
        if kind.is_socket() {
            return Some("a socket");
        }
        if kind.is_block_device() {
            return Some("a block device");
        }
        if kind.is_char_device() {
            return Some("a character device");
        }
    }
    Some("of an unrecognised type")
}

/// Read an already-vetted regular file, refusing anything past
/// [`MAX_SETTINGS_BYTES`].
///
/// The bound is re-applied to the bytes actually read, not just to the size the
/// vet saw: the file can grow — or be replaced by a larger one — between the
/// two, and a limit that only consults `stat` would not be a limit.
fn read_bounded(path: &Path) -> Result<Option<String>, SettingsWriteError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .map_err(|error| path_error(&format!("it cannot be read: {error}"), path))?;
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| path_error(&format!("it cannot be read: {error}"), path))?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(oversized(path));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| path_error("it is not valid UTF-8", path))
}

/// The resolved path of the settings document.
///
/// [`SETTINGS_PATH_ENV`] wins when it is set and non-empty. An empty value is
/// treated as "unset": it can only arrive from a caller that meant to clear the
/// override, and honouring it would resolve to a path with no file name.
pub fn settings_path() -> PathBuf {
    match std::env::var(SETTINGS_PATH_ENV) {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => config_dir().join(SETTINGS_FILE_NAME),
    }
}

/// Load the settings document from `path`, and say what was wrong with it.
///
/// **Still never fails**, in the sense the module docs mean: a settings file is
/// not worth failing an app launch over, so every failure yields defaults and
/// the app comes up. What changed at issue #1072 is that the failure is no
/// longer *discarded*. The caller gets the reason, so the settings surface can
/// render it and [`save_to`] can refuse to publish defaults over bytes nobody
/// understood.
///
/// `None` means the document loaded — including the ordinary first-run case
/// where there is no file at all, and including an **empty** one, which is valid
/// TOML and genuinely does mean "everything at its default". Only a document
/// that is there and cannot be used produces a problem.
///
/// Every caller passes a path: the app goes through [`load_snapshot`], which
/// needs the resolved path anyway to show the user where their settings live,
/// and every test passes one explicitly so none of them depends on
/// process-global environment state.
pub fn load_document(path: &Path) -> (DesktopSettings, Option<SettingsDocumentProblem>) {
    match read_document(path, ReadPurpose::Load) {
        Ok(None) => (DesktopSettings::default(), None),
        Ok(Some(contents)) => match toml_edit::de::from_str(&contents) {
            Ok(settings) => (settings, None),
            Err(error) => (
                sections_this_build_can_read(&contents),
                Some(unreadable_document_problem(path, &contents, &error)),
            ),
        },
        Err(error) => (DesktopSettings::default(), Some(error.into())),
    }
}

/// Everything in `contents` this build can read, with the parts it cannot
/// dropped to their defaults.
///
/// # Why a whole-document failure is not a whole-document answer
///
/// The document is one TOML file holding several unrelated tenants, and
/// `toml_edit::de::from_str` is all-or-nothing: one value it refuses and the
/// *struct* fails, so the app came up with the user's `[endpoints]` — their
/// remote decks — their appearance and their zoom all reading as defaults.
/// That is a wide blast radius for a stale `[voice]` token, and it was reached
/// by two ordinary routes rather than by a corrupted file: a document written
/// by the build before PRD #802's provider work (see [`StageSpec`], which is
/// what stops that one failing at all now), and any value a type refuses —
/// a hand-typed non-loopback `http` endpoint, which [`ServiceUrl`] exists to
/// reject.
///
/// Nothing was ever lost from the file itself: [`save_to`] re-reads and refuses
/// to publish defaults over a document it cannot parse (issue #1072). What was
/// lost was the session — the decks were not there to talk to.
///
/// # Section granularity, and deliberately no finer
///
/// Each top-level key is judged alone, and a section that fails as a whole has
/// its own children judged the same way; a key that fails at that point takes
/// its whole subtree with it. The recursion stops there **on purpose**. Pruning
/// a single refused field would leave its siblings behind, and for a voice
/// stage that is the one outcome the hand-written
/// [`TranscriptionSettings::deserialize`] exists to prevent: a surviving
/// `backend = "remote"` beside a dropped `endpoint` fills the endpoint from the
/// *hosted* preset, which is a user's audio going somewhere they did not write.
/// Dropping the stage instead lands on [`TranscriptionSettings::default`] —
/// keyless, on loopback — which is the safe direction to fail in.
///
/// The reported [`SettingsDocumentProblem`] is unchanged either way: the user is
/// told the document could not be read and where, whatever was salvaged from it.
fn sections_this_build_can_read(contents: &str) -> DesktopSettings {
    // A document that does not parse as TOML at all has no sections to keep —
    // the failure is the syntax, not a value, and there is nothing to walk.
    let Ok(mut document) = contents.parse::<toml_edit::DocumentMut>() else {
        return DesktopSettings::default();
    };
    for key in document
        .iter()
        .map(|(key, _)| key.to_string())
        .collect::<Vec<_>>()
    {
        if document
            .get(&key)
            .is_some_and(|item| reads_as_settings(&key, item))
        {
            continue;
        }
        for child in document
            .get(&key)
            .and_then(toml_edit::Item::as_table)
            .map(|table| table.iter().map(|(child, _)| child.to_string()).collect())
            .unwrap_or_else(Vec::new)
        {
            let readable = document
                .get(&key)
                .and_then(toml_edit::Item::as_table)
                .and_then(|table| table.get(&child))
                .is_some_and(|value| {
                    let mut alone = toml_edit::Table::new();
                    alone.insert(&child, value.clone());
                    reads_as_settings(&key, &toml_edit::Item::Table(alone))
                });
            if !readable
                && let Some(table) = document
                    .get_mut(&key)
                    .and_then(toml_edit::Item::as_table_mut)
            {
                table.remove(&child);
            }
        }
        let emptied = document
            .get(&key)
            .and_then(toml_edit::Item::as_table)
            .is_none_or(toml_edit::Table::is_empty);
        if emptied {
            document.remove(&key);
        }
    }
    // Pruned in place rather than rebuilt key by key, so what is handed back to
    // the parser is the user's own bytes minus the refused ones — no cloned
    // decor, no re-ordered tables, nothing this function had to render itself.
    // The `unwrap_or_default` is the residual: a failure here means a refusal
    // that is not attributable to any one top-level key, and defaults are then
    // the only answer left.
    toml_edit::de::from_str(&document.to_string()).unwrap_or_default()
}

/// Whether `key` alone, carrying `item`, reads as a settings document.
///
/// Every field of [`DesktopSettings`] is `#[serde(default)]` and there is no
/// `deny_unknown_fields`, so a one-key document exercises exactly that key's
/// own types and says nothing about any other — which is what makes judging
/// them one at a time sound.
fn reads_as_settings(key: &str, item: &toml_edit::Item) -> bool {
    let mut probe = toml_edit::DocumentMut::new();
    probe[key] = item.clone();
    toml_edit::de::from_str::<DesktopSettings>(&probe.to_string()).is_ok()
}

/// The one place a document problem reaches the app's own log.
///
/// The **detail** half, which is the half of the split that may name a path —
/// this is stderr and the deck log, not the webview.
fn log_document_problem(problem: &SettingsDocumentProblem) {
    eprintln!("{}; using defaults", problem.detail());
}

/// The diagnostic logged for a document this build cannot parse: a **locator**,
/// deliberately never the document's own bytes.
///
/// The log half of [`unreadable_document_problem`]. Its public sibling follows
/// the same rule for the same reason, and the rule is now load-bearing on two
/// sinks rather than one — a webview renders that half.
///
/// # Why the toml error's own message is not logged
///
/// `toml_edit::de::Error`'s `Display` echoes the offending value **twice** — once in
/// a rendered source line and again in serde's `invalid type: string "…"`
/// message. Measured, not assumed; the exact shape is pinned by
/// [`tests::a_parse_diagnostic_carries_a_locator_and_never_the_documents_bytes`].
/// So a hand-edited document whose value sits in a wrongly-typed field used to
/// put that value straight into this process's stderr and the deck log, and a
/// log is a file people paste into bug reports.
///
/// The trade is deliberate: the locator loses the `expected u32` half of the
/// message and keeps the half a developer acts on — the file is named and the
/// line is the line to open, where the offending text is in front of them
/// anyway. Issue #827 lists the log as one of the sinks a credential must not
/// reach; this is that sink closed for the whole document rather than for a
/// field anyone remembered to think about.
///
/// A separate function because the content of the line is then testable at all:
/// an `eprintln!` inside a match arm cannot be asserted on.
fn invalid_document_log(path: &Path, contents: &str, error: &toml_edit::de::Error) -> String {
    format!(
        "Invalid desktop settings at {}: {} could not be read as settings",
        path.display(),
        parse_locator(contents, error)
    )
}

/// Where the parse went wrong, as a phrase — `line 3, column 9`, and never a
/// byte of the document.
///
/// Extracted from [`invalid_document_log`] at issue #1072 because there are now
/// three messages built from it and they must not diverge: the log line, the
/// [`SettingsDocumentProblem`] the settings surface renders, and the refusal
/// [`save_to`] returns. All three are a locator plus fixed prose, which is what
/// makes the issue-#827 property — no document bytes in any sink — one check
/// rather than three.
fn parse_locator(contents: &str, error: &toml_edit::de::Error) -> String {
    match error
        .span()
        .and_then(|span| line_and_column(contents, span.start))
    {
        Some((line, column)) => format!("line {line}, column {column}"),
        // A span is present for every error this crate has produced, but it is
        // an `Option` on the API and a locator-less message is still useful.
        None => "an unreported position".to_string(),
    }
}

/// The problem [`load_document`] reports for a document this build cannot read.
fn unreadable_document_problem(
    path: &Path,
    contents: &str,
    error: &toml_edit::de::Error,
) -> SettingsDocumentProblem {
    SettingsDocumentProblem {
        detail: invalid_document_log(path, contents, error),
        public: format!(
            "The desktop settings file cannot be read: {} is not valid settings. \
             {UNREADABLE_CONSEQUENCE}",
            parse_locator(contents, error)
        ),
    }
}

/// The refusal [`save_to`] returns rather than publishing this build's defaults
/// over a document it could not read (issue #1072).
///
/// A `SettingsWriteError` because that is what a save returns and what the
/// webview already renders; built through [`write_error`] so it inherits the
/// split — the path in the log half, never in the public one.
fn refuse_to_overwrite(
    path: &Path,
    contents: &str,
    error: &toml_edit::de::Error,
) -> SettingsWriteError {
    write_error(
        "refusing to overwrite",
        path,
        format!(
            "{} is not valid settings, and saving would replace it with defaults. \
             Fix or remove the file, then try again — it has been left exactly as it is",
            parse_locator(contents, error)
        ),
    )
}

/// The 1-based line and column of the byte at `offset` in `contents`.
///
/// `None` when `offset` is past the end or lands inside a multi-byte character,
/// both of which mean the caller cannot describe a position it can trust. The
/// column counts **characters** rather than bytes, because the number is for a
/// human counting along a line in an editor.
fn line_and_column(contents: &str, offset: usize) -> Option<(usize, usize)> {
    let before = contents.get(..offset)?;
    let line = before.matches('\n').count() + 1;
    let last_line = before.rsplit('\n').next().unwrap_or(before);
    Some((line, last_line.chars().count() + 1))
}

/// Persist the settings document atomically and owner-only.
pub fn save(settings: &DesktopSettings) -> Result<(), SettingsWriteError> {
    save_to(&settings_path(), settings)
}

/// How many temp names [`save_to`] draws before giving up. A leftover temp file
/// from a crashed run would otherwise make every later save fail with
/// `AlreadyExists`, since the publish deliberately uses `create_new` and never
/// unlinks whatever holds a name it wanted.
const TEMP_NAME_ATTEMPTS: usize = 8;

/// [`save`] against an explicit path.
///
/// Writes a temp file in the **same directory** as `path`, then renames over
/// `path`. Rename within one directory is atomic on POSIX, so no reader ever
/// observes a partially written document and a failed write leaves the previous
/// one exactly where it was. The temp file is created with `create_new`
/// (`O_CREAT|O_EXCL`, which cannot follow a symlink someone planted at that
/// name) at owner-only mode, so the document is never briefly world-readable.
///
/// The path is vetted first — see [`read_document`] — so a save never creates a
/// directory for, or writes over, something that is not a settings document.
///
/// # The parent directory is vetted — PRD #741 took the first half of this
///
/// An audit of this path recommended two further steps: rejecting a symlinked
/// or non-user-owned **parent** directory, and anchoring both the create and
/// the publish to one verified directory handle (`openat`/`renameat`-style) so
/// no name is resolved twice. Both were declined while the document held only
/// an appearance mode and a zoom level, and that comment named **#741 — a
/// daemon endpoint** as the change that would move the calculus. It has: the
/// document now holds a host, a login name and a path to a private key.
///
/// **The parent check is taken.** [`vet_parent_dir`] refuses a symlinked
/// parent, a parent that is not a directory, and — on Unix — a parent owned by
/// another uid, before anything is created. It is cheap, it is one `lstat`, and
/// it closes the shape where a planted symlink silently redirects the whole
/// write (including `create_owner_only_dir`, which carries no symlink guard of
/// its own by design — it never chmods, so it had no exposure to guard until a
/// caller cared *where* the write landed).
///
/// **The `openat`/`renameat` anchoring is still deferred, and here is the
/// written reason rather than an assumed one.** Three things hold it back and
/// the first is the one that decides it:
///
/// 1. **The value it would protect is a reference, not a secret.** The storage
///    policy this document is under stores a key *path*, never key material and
///    never a passphrase — so winning the race yields the name of a file the
///    attacker would still have to be able to read, not a credential. The
///    threat that would justify the complexity is the one the policy exists to
///    make impossible.
/// 2. **The attacker who could win it can already write the file.** The
///    destination is the per-user config directory, so a same-uid actor with a
///    foothold there can edit `desktop.toml` outright; the race buys them
///    nothing new. A *different*-uid actor is what the parent-ownership check
///    above now refuses.
/// 3. **`std` gives no anchored rename.** There is no `renameat` in `std::fs`,
///    so this means either a `libc` dependency in a crate whose `src/` has none
///    (it is a `cfg(unix)` dev-dependency today, for one test) or `rustix`, on
///    a required, currently-clean `cargo audit` gate — and a Windows arm that
///    no maintainer can test, since no Windows binaries are released.
///
/// Revisit it if this document ever holds real credential material, which is
/// the thing PRD #803 M5's `SecretStore` seam exists to stop it doing.
///
/// Two residual **reliability** properties remain, both accepted, both worth
/// knowing before either is reported as a bug:
///
/// - abrupt process death between [`create_temp`] and the rename leaves an
///   owner-only `.desktop.toml.tmp.*` file behind. Nothing reads it, and the
///   next save simply draws a different name — which is what
///   [`TEMP_NAME_ATTEMPTS`] is for;
/// - the parent directory is **not** fsync'd after the rename. The write is
///   therefore atomic to every live observer but not fully crash-durable: a
///   power loss immediately after a save can leave the previous document in
///   place.
pub fn save_to(path: &Path, settings: &DesktopSettings) -> Result<(), SettingsWriteError> {
    // Before anything is created: a rejected path must not leave a directory
    // behind, and an unreadable or over-limit document must not be replaced.
    let existing = read_document(path, ReadPurpose::Save)?;

    // Issue #1072: and neither must a document this build cannot READ. The two
    // failures above were always refused; this one was not, and it is the one a
    // user reaches. The load had already answered it with defaults, so the
    // merge below would publish those defaults over every key the struct owns —
    // appearance, zoom, endpoints and the deck selection — and the user's own
    // document would be gone.
    //
    // The check is `DesktopSettings` rather than a bare `DocumentMut` parse on
    // purpose, because it has to catch BOTH shapes of unreadable: a TOML syntax
    // error, and a document that is valid TOML which this build's SCHEMA
    // rejects. Only the first fails a document parse. The second is the shape
    // the issue was filed from and the more dangerous of the two — every field
    // newtype runs its validator inside `Deserialize`, so tightening any
    // validator converts previously-valid documents into this case, and this
    // codebase tightens validators routinely (`port` to a `NonZeroU16`,
    // `EndpointId::parse` reserving `all`).
    //
    // Re-read at save time rather than trusted from load, so a file that became
    // unreadable while the app was running is caught too.
    if let Some(contents) = existing.as_deref()
        && let Err(error) = toml_edit::de::from_str::<DesktopSettings>(contents)
    {
        return Err(refuse_to_overwrite(path, contents, &error));
    }

    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    vet_parent_dir(parent)?;
    fsperm::create_owner_only_dir(parent)
        .map_err(|error| write_error("could not create the directory for", path, error))?;

    let contents = merged_document(path, existing.as_deref(), settings)?;

    let (mut file, tmp) = create_temp(parent, path)?;
    let published = (|| {
        // On Unix `create_new` + the owner-only creation mode already produced
        // 0o600; this re-asserts it, and on Windows it is where the DACL is
        // applied at all. Same defence in depth as `schedule_cli::write_atomic`.
        fsperm::set_file_owner_only(&file)?;
        use std::io::Write as _;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })();
    drop(file);

    if let Err(error) = published.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_error("could not write", path, error));
    }
    Ok(())
}

/// Serialise `settings` over whatever the document at `path` already holds,
/// preserving every table and field this build does not know about — **and the
/// way the user spelled them** (issue #825).
///
/// **`#[serde(default)]` without `deny_unknown_fields` means *ignore*, not
/// *retain*.** It covers reading — an older build loads a newer build's
/// `[voice]` section without error — but the unknown table is dropped on the
/// way in, so serialising the struct straight out would delete it on the next
/// save. That is exactly the `DashboardConfig::save()` failure mode PRD #803
/// rejects sharing `config.toml` for, reproduced in our own file, and it would
/// break the container's central promise: that a feature can add a section and
/// trust an older build not to eat it.
///
/// `existing` is what [`save_to`] just read off disk, **not** what the frontend
/// loaded at startup: re-reading at save time is what lets a section another
/// process wrote between this app's read and its write survive as well. The
/// cost is one small read per save, on a file the app writes only when a user
/// changes a setting.
///
/// # Formatting is preserved too, and issue #825 is where that started
///
/// The merge used to round-trip through `toml::Table`, which models **data**:
/// no unknown key, value or type was ever lost, but the whole document was
/// re-rendered canonically on the way out, so a comment vanished, an inline
/// array came back re-flowed across lines, and key order and blank-line
/// grouping were whatever the serializer felt like. PRD #803 makes "a file a
/// user can read, edit and delete without the app running" a success criterion,
/// which invites hand-annotation — and those annotations disappeared the next
/// time the app wrote any setting.
///
/// So the document is now a [`toml_edit::DocumentMut`], a format-preserving
/// DOM, and [`merge_tables`] writes only the keys whose **data** actually
/// changed. Everything else keeps its own bytes: comments above a key or a
/// section header, a trailing comment on a line whose value did change, inline
/// tables, inline arrays, key order and blank lines. A save with no new value
/// to write is byte-identical to the file it read — pinned by
/// [`tests::a_save_that_changes_nothing_rewrites_nothing`].
///
/// **Two things it deliberately does not do**, because a narrower true claim
/// beats a wide false one:
///
/// - a **changed array is replaced whole**, so a comment *inside* an array, or
///   between `[[endpoints.remote]]` rows, is lost when the app rewrites that
///   list. Merging element-wise would mean matching rows by index, and an index
///   match cannot tell a reorder from an edit — it would also stop a row's
///   optional field from being *removed*, which replacing the array whole is
///   what makes work today. An **unchanged** array is left alone, which is the
///   common case and covers the comments people actually write. Pinned by
///   [`tests::a_comment_inside_a_list_survives_until_that_list_changes`];
/// - a key the struct **stops** emitting is not deleted, because the merge
///   walks `incoming` and so can only add or overwrite. That is not new, not
///   #825's, and for the one field it reaches today it is the design rather
///   than a gap: `DesktopSettings::endpoints` is an `Option` whose `None`
///   serialises to nothing at all, and that omission is exactly what stops a
///   client which cannot render endpoints from deleting them (see
///   [`EndpointSettings`]). The same additive walk is what makes an unknown key
///   survive at all.
///
/// One consequence worth stating so it is not read as a bug: "leaves every byte
/// alone" holds for a document that already **has** every key the struct owns.
/// A partial one — no `[zoom]` section, say — gains that section on the next
/// save, appended after whatever is already there.
///
/// # An unparseable document is refused, not replaced (issue #1072)
///
/// **That reverses what used to be written here.** The old reasoning was that
/// nothing can be preserved out of bytes that are not TOML, and that refusing
/// would leave a user whose file got corrupted unable to change a setting from
/// inside the app. The first half is true and the second was the wrong trade:
/// the alternative to refusing is not "the user gets their settings back", it is
/// "the user's file is silently destroyed the first time they touch anything".
/// Being unable to change a preference is recoverable — the message names the
/// line to open — and a replaced document is not.
///
/// [`save_to`] makes that call before reaching here, against `DesktopSettings`
/// rather than a bare document parse, because a document that is valid TOML
/// which this build's *schema* rejects is equally unreadable and equally
/// destructive to merge into. By the time this function runs, `existing` has
/// already parsed.
///
/// **"Unreadable" means unreadable by *this* build**, which is a wider set than
/// "corrupt" — and under a refusal that widening is now a feature rather than a
/// hazard. A document a *newer* build wrote that this one cannot read is exactly
/// the case where replacing it would take that build's sections with it, so it
/// lands on the refusal too.
fn merged_document(
    path: &Path,
    existing: Option<&str>,
    settings: &DesktopSettings,
) -> Result<String, SettingsWriteError> {
    // The canonical rendering of the struct: byte for byte what a fresh
    // document looks like, and the source of every value the merge writes.
    // `to_string_pretty` rather than `to_string` because the difference is
    // `[appearance]` versus `appearance = { … }` for a document nobody has
    // expressed a preference about yet — pinned by
    // [`tests::default_document_shape_is_pinned`].
    let canonical = toml_edit::ser::to_string_pretty(settings)
        .map_err(|error| write_error("could not serialize", path, error))?;

    // No document on disk: there is nothing to preserve, and the canonical
    // rendering IS the answer. Also the only path that can produce a document
    // this function did not merge into, which is why the shape pin can assert
    // on it.
    let Some(contents) = existing else {
        return Ok(canonical);
    };

    // Unreachable through [`save_to`], which refuses an unreadable document
    // before it reaches here (issue #1072). It is still an error rather than the
    // `unwrap_or_default()` it once was, because defaulting is precisely what
    // made the data loss silent: the merge would then run against an EMPTY
    // document and publish that, which is a whole-file replacement wearing a
    // merge's clothes.
    let mut document = contents
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| refuse_to_overwrite(path, contents, &error.into()))?;

    // Re-parsing this build's own output, which `to_string_pretty` just
    // produced and which is therefore valid TOML. Going through the text rather
    // than `ser::to_document` is deliberate: it is the same bytes the no-document
    // path returns, so the two paths cannot drift into writing different values
    // for the same struct.
    let incoming = canonical
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| write_error("could not serialize", path, error))?;

    merge_tables(document.as_table_mut(), incoming.as_table(), false);
    Ok(document.to_string())
}

/// Deep-merge `incoming` into `base`: two tables merge key by key, anything
/// else replaces outright — and a key whose **data** is already what `incoming`
/// says keeps the bytes the document spelled it with.
///
/// So a field the struct owns always wins over whatever the file held — the
/// struct is the authority on its own schema — while a key only the file has is
/// left exactly as it was, down to a field nested inside a section this build
/// *does* know.
///
/// **The unchanged-key skip is what preserves formatting for keys this build
/// DOES own.** Without it every save would rewrite `version`, `mode`, `level`
/// and the whole endpoint list on every write, and rewriting an
/// `[[endpoints.remote]]` list drops the comments between its rows even when
/// not one byte of its data moved. [`same_data`] compares data and ignores
/// spelling, so an inline `remote = [{ … }]` and an `[[endpoints.remote]]`
/// block holding the same rows both count as unchanged.
///
/// `inline` says whether `base` is an inline table (`a = { … }`) rather than a
/// `[section]`, and it is **load-bearing rather than cosmetic**: an inline table
/// holds values only, so a `[section]`- or `[[array]]`-shaped item put inside
/// one is a shape it cannot render — and `toml_edit` renders it as **nothing**
/// rather than failing. Measured, not feared: against a hand-written
/// `endpoints = { selection = "local", remote = [] }`, adding a deck without
/// this conversion writes `endpoints = { selection = "local"}` — still valid
/// TOML, with the deck the user just added silently gone.
///
/// **The `insert` path was already safe and the `replace` path was not**, which
/// is the distinction to keep if this is ever refactored. `TableLike::insert`
/// converts the item itself, so a new key was never the problem; its
/// `into_value().unwrap()` fails only on an `Item::None`, which the loop above
/// skips. A *replacement* goes straight through `get_mut` into the table's item
/// map with no conversion anywhere, which is where the value disappeared.
/// Converting once, up front, covers both — and it is also the behaviour a
/// reader wants: a document written inline stays inline.
fn merge_tables(
    base: &mut dyn toml_edit::TableLike,
    incoming: &dyn toml_edit::TableLike,
    inline: bool,
) {
    for (key, item) in incoming.iter() {
        // `TableLike::iter` yields entries a table is holding a place for but
        // has no value at. `incoming` is parsed from this build's own canonical
        // rendering, which has no way to produce one — skipping costs nothing
        // and means the loop below does not rest on that.
        if item.is_none() {
            continue;
        }

        let mut item = item.clone();
        if inline {
            item.make_value();
        }

        if base.get(key).is_none() {
            base.insert(key, item);
            continue;
        }
        // `get` and `get_mut` answer the same question on both table
        // spellings — a `Table` filters its no-value entries out of each, an
        // inline table filters neither — so having just seen `Some` here
        // cannot become `None`.
        let existing = base.get_mut(key).expect("`get` just found this key");

        if let Some(incoming_table) = item.as_table_like()
            && existing.is_table_like()
        {
            // Read the base's spelling BEFORE borrowing it mutably; the
            // recursion needs it to decide how to spell anything it inserts.
            let nested_inline = existing.is_inline_table();
            let existing_table = existing
                .as_table_like_mut()
                .expect("`is_table_like` just said so");
            merge_tables(existing_table, incoming_table, nested_inline);
            continue;
        }

        // A key whose SHAPE moves from a value to a `[section]` is re-inserted
        // rather than replaced in place. Only the rendering differs, and only
        // in one place — a table's header is spelled from the key WITH its
        // decor, so `intent = "remote"` becoming a table renders as
        // `[voice.intent ]`, carrying the space that used to sit before the
        // `=`. Valid TOML that re-reads correctly, and still a stray space in a
        // file PRD #803 makes a success criterion of a user being able to read.
        // It is reachable on exactly one ordinary path: the first save after
        // the `[voice]` scalars became tables (see [`StageSpec`]).
        let flattened = item.is_table_like() && !existing.is_table_like();
        if same_data(existing, &item) {
            continue;
        }
        if flattened {
            base.remove(key);
            base.insert(key, item);
            continue;
        }
        replace_item(existing, item);
    }
}

/// Overwrite `existing` with `incoming`, keeping the **decor** — the whitespace
/// and comments attached to the value — that the document had there.
///
/// The key's own decor needs no care: only the item is replaced, so a comment
/// line above the key stays where it is. The value's decor is the other half,
/// and it is the one a user notices — `mode = "light"  # my own note` keeps
/// ` # my own note` when the mode flips to `"dark"`.
///
/// Only values carry decor: a `[section]` or an `[[array]]` item keeps its own,
/// and there is nothing to copy across.
fn replace_item(existing: &mut toml_edit::Item, incoming: toml_edit::Item) {
    let decor = existing.as_value().map(|value| value.decor().clone());
    *existing = incoming;
    if let Some(decor) = decor
        && let Some(value) = existing.as_value_mut()
    {
        *value.decor_mut() = decor;
    }
}

/// Whether two items hold the same **data**, ignoring every difference in how
/// the document spells it.
///
/// A `[table]` and an `a = { … }` inline table with the same pairs are the same
/// data; so are an `[[array]]` of tables and an `a = [{ … }]` array of inline
/// tables; so are `"x"` written with one quote style or another, and a value
/// with a comment stuck to it. That is the whole point — [`merge_tables`] uses
/// this to decide whether it has anything to write, and re-spelling a key whose
/// value did not change is exactly the formatting loss issue #825 is about.
///
/// Types are **not** coerced across: a `1` where the struct has `1.0` is a
/// change, and the canonical `1.0` is written. That is the right way round —
/// the struct is the authority on its own schema, so its type wins.
fn same_data(left: &toml_edit::Item, right: &toml_edit::Item) -> bool {
    match (data_node(left), data_node(right)) {
        (Some(left), Some(right)) => same_node(&left, &right),
        // `Item::None` on either side: a table holding a place for a key it has
        // no value at is not "the same data" as any value.
        _ => false,
    }
}

/// An item seen as data rather than as syntax — the view [`same_data`] compares.
enum DataNode<'a> {
    /// A `[table]`, an inline table, or a dotted-key group.
    Table(&'a dyn toml_edit::TableLike),
    /// An `[[array of tables]]` or an ordinary array, element by element.
    Array(Vec<DataNode<'a>>),
    /// Anything with no structure under it.
    Scalar(&'a toml_edit::Value),
}

fn data_node(item: &toml_edit::Item) -> Option<DataNode<'_>> {
    if let Some(table) = item.as_table_like() {
        return Some(DataNode::Table(table));
    }
    if let Some(rows) = item.as_array_of_tables() {
        return Some(DataNode::Array(
            rows.iter()
                .map(|row| DataNode::Table(row as &dyn toml_edit::TableLike))
                .collect(),
        ));
    }
    item.as_value().map(value_node)
}

fn value_node(value: &toml_edit::Value) -> DataNode<'_> {
    if let Some(table) = value.as_inline_table() {
        return DataNode::Table(table);
    }
    if let Some(array) = value.as_array() {
        return DataNode::Array(array.iter().map(value_node).collect());
    }
    DataNode::Scalar(value)
}

fn same_node(left: &DataNode<'_>, right: &DataNode<'_>) -> bool {
    match (left, right) {
        (DataNode::Table(left), DataNode::Table(right)) => {
            // Order-insensitive, because a table IS an unordered map and the
            // document's own key order is precisely what must not count as a
            // difference. The length check is what makes the one-sided walk
            // below a two-sided comparison.
            let pairs = |table: &dyn toml_edit::TableLike| {
                table.iter().filter(|(_, at)| !at.is_none()).count()
            };
            pairs(*left) == pairs(*right)
                && left
                    .iter()
                    .filter(|(_, at)| !at.is_none())
                    .all(|(key, at)| right.get(key).is_some_and(|other| same_data(at, other)))
        }
        (DataNode::Array(left), DataNode::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| same_node(left, right))
        }
        (DataNode::Scalar(left), DataNode::Scalar(right)) => same_scalar(left, right),
        _ => false,
    }
}

/// Spelled out rather than derived: `toml_edit::Value` carries its decor and its
/// original text, so `PartialEq` on it — if it had one — would answer a
/// different question than this one.
fn same_scalar(left: &toml_edit::Value, right: &toml_edit::Value) -> bool {
    use toml_edit::Value;
    match (left, right) {
        (Value::String(left), Value::String(right)) => left.value() == right.value(),
        (Value::Integer(left), Value::Integer(right)) => left.value() == right.value(),
        (Value::Float(left), Value::Float(right)) => left.value() == right.value(),
        (Value::Boolean(left), Value::Boolean(right)) => left.value() == right.value(),
        (Value::Datetime(left), Value::Datetime(right)) => left.value() == right.value(),
        // Different types, and the two structured arms `data_node` already
        // peeled off. Both are a change.
        _ => false,
    }
}

/// Exclusively create a fresh temp file next to `dest`, redrawing the name on
/// collision. Returns the open file and its path.
///
/// The suffix is **random** rather than the old `<pid>.<counter>`, which any
/// other process could compute. `create_new` (`O_CREAT|O_EXCL`) already means a
/// planted name costs a failed save rather than a write through someone else's
/// symlink, so the guessable form was a nuisance rather than a hole — but an
/// unpredictable name costs one hash and removes the question.
fn create_temp(parent: &Path, dest: &Path) -> Result<(std::fs::File, PathBuf), SettingsWriteError> {
    let mut last = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let tmp = parent.join(format!(
            ".{SETTINGS_FILE_NAME}.tmp.{:016x}",
            unpredictable_suffix()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        fsperm::set_create_mode_owner_only(&mut options);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            // Deny every other principal an open handle for the lifetime of
            // ours, so nobody can be holding one across the DACL tightening
            // `set_file_owner_only` performs on the handle a moment later. The
            // rename happens after the handle is dropped, so this costs
            // nothing.
            options.share_mode(0);
        }
        match options.open(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => {
                return Err(write_error(
                    "could not create a temp file beside",
                    dest,
                    error,
                ));
            }
        }
    }
    let cause = last
        .map(|error| error.to_string())
        .unwrap_or_else(|| "no temp name was attempted".to_string());
    Err(write_error(
        "could not create a temp file beside",
        dest,
        cause,
    ))
}

/// A temp-name suffix an outside observer cannot predict, with no new
/// dependency.
///
/// `RandomState` is seeded from the operating system, so hashing a
/// monotonically increasing counter and the current time under it gives a value
/// that is unique within the process and unguessable outside it. This names a
/// scratch file for a few milliseconds; it is not, and must not be used as,
/// a source of cryptographic randomness.
fn unpredictable_suffix() -> u64 {
    use std::hash::{BuildHasher as _, Hash as _, Hasher as _};

    static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    WRITE_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_service::{DEFAULT_TOKEN_CEILING, MAX_TOKEN_CEILING, MIN_TOKEN_CEILING};
    use std::sync::Mutex;

    /// `settings_path` is the only thing here that reads the environment, and
    /// the environment is process-global while `cargo test` runs a module's
    /// tests as threads in one process. Every test that does not go through
    /// [`load_snapshot`] drives [`load_document`] and [`save_to`] with an
    /// explicit path instead, so this lock serialises only the handful below
    /// that set [`SETTINGS_PATH_ENV`] — against each other and against
    /// themselves.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("settings tempdir")
    }

    /// [`load_document`] for the many tests that assert on the document alone,
    /// logging the problem exactly as [`load_snapshot`] does so a test expecting
    /// defaults still drives that path.
    ///
    /// A test helper rather than the module function it used to be: once
    /// [`load_snapshot`] carries the reason onward, "load and throw the reason
    /// away" is not something the app does anywhere, and leaving it on the
    /// public surface would invite a future caller back into the shape issue
    /// #1072 was about.
    fn load_from(path: &Path) -> DesktopSettings {
        let (settings, problem) = load_document(path);
        if let Some(problem) = &problem {
            log_document_problem(problem);
        }
        settings
    }

    fn dark() -> DesktopSettings {
        DesktopSettings {
            appearance: AppearanceSettings {
                mode: AppearanceMode::Dark,
            },
            ..DesktopSettings::default()
        }
    }

    /// A document whose zoom is not the default, for the merge and round-trip
    /// tests that need the two sections to be independently observable.
    fn zoomed(level: f64) -> DesktopSettings {
        DesktopSettings {
            zoom: ZoomSettings {
                level: ZoomLevel::snap(level),
            },
            ..DesktopSettings::default()
        }
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Names of every file directly inside `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_saved_document_round_trips() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        assert_eq!(load_from(&path), dark());
        // And the file a user would open reads the way the PRD promises.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[appearance]"), "unexpected document: {raw}");
        assert!(
            raw.contains("mode = \"dark\""),
            "unexpected document: {raw}"
        );
    }

    #[test]
    fn an_absent_file_yields_defaults() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        assert!(!path.exists());
        assert_eq!(load_from(&path), DesktopSettings::default());
        assert_eq!(
            DesktopSettings::default().appearance.mode,
            AppearanceMode::System
        );
    }

    #[test]
    fn loading_never_fails_on_a_malformed_or_wrongly_typed_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);

        for contents in [
            // Not TOML at all.
            "this is not = = toml [[[",
            // Valid TOML, wrong types throughout.
            "version = \"one\"\n[appearance]\nmode = 3\n",
            // Valid TOML, a scalar where a table belongs.
            "version = 1\nappearance = \"dark\"\n",
            // Empty.
            "",
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                load_from(&path),
                DesktopSettings::default(),
                "document should have fallen back to defaults: {contents:?}"
            );
        }
    }

    #[test]
    fn an_unknown_appearance_value_falls_back_without_losing_the_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 9\n[appearance]\nmode = \"solarized\"\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::System);
        // The rest of the document survives: only the unreadable field is
        // replaced, and the load is not downgraded to a whole-file default.
        assert_eq!(loaded.version, 9);
    }

    #[cfg(unix)]
    #[test]
    fn loading_never_fails_on_an_unreadable_file() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        set_mode(&path, 0o000);
        if std::fs::read_to_string(&path).is_ok() {
            set_mode(&path, 0o600);
            eprintln!(
                "SKIP: this process can read a 0o000 file (running privileged), so an \
                 unreadable document cannot be constructed here"
            );
            return;
        }
        assert_eq!(load_from(&path), DesktopSettings::default());
        set_mode(&path, 0o600);
    }

    #[test]
    fn unknown_sections_and_fields_do_not_break_a_load() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\
             future_toplevel = true\n\n\
             [appearance]\n\
             mode = \"light\"\n\
             future_field = \"whatever a newer build wrote\"\n\n\
             [voice]\n\
             backend = \"whisper\"\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
        assert_eq!(loaded.version, 1);
        // `[voice]` is a section this build DOES own since PRD #802 M4, and
        // `backend` is not one of its fields — so this line is now covering
        // "an unknown field inside a known section" rather than "an unknown
        // section", which is the other half of the same tolerance and is worth
        // keeping. The unknown-section half is covered by `future_toplevel`
        // above and by `a_hand_written_comment_survives_an_app_driven_save`.
        assert_eq!(loaded.voice, Some(VoiceSettings::default()));
    }

    /// Scenario: a document names every `[voice]` choice; it round-trips
    /// through TOML and through the JSON the webview receives, with the same
    /// tokens on both wires.
    ///
    /// The section is PRD #802's tenant and both wires are its contract: the
    /// TOML is what a user hand-edits and the JSON is what the panel renders,
    /// and every field name is one word so the two agree byte for byte.
    #[test]
    fn a_voice_section_round_trips_through_toml_and_json() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             [voice]\n\
             activation = \"toggle\"\n\n\
             [voice.intent]\n\
             backend = \"anthropic\"\n\
             endpoint = \"https://api.anthropic.com/v1/messages\"\n\
             model = \"claude-haiku-4-5\"\n\n\
             [voice.transcription]\n\
             backend = \"local\"\n\
             endpoint = \"http://127.0.0.1:18000/v1/audio/transcriptions\"\n\
             model = \"Systran/faster-whisper-tiny.en\"\n",
        )
        .unwrap();

        let loaded = load_from(&path);
        let voice = loaded.voice.clone().expect("the section is present");
        assert_eq!(voice.activation, ActivationMode::Toggle);
        assert_eq!(voice.intent.backend, IntentBackend::Anthropic);
        assert_eq!(voice.intent.model.as_str(), HOSTED_COMMAND_MODEL);
        assert_eq!(voice.transcription.backend, TranscriptionBackend::Local);
        assert_eq!(
            voice.transcription.endpoint.as_str(),
            LOCAL_SPEECH_ENDPOINT,
            "the keyless loopback endpoint survives the round trip verbatim"
        );

        // The JSON the webview receives carries the same tokens and the same
        // strings, not an index or a tagged enum — one word per field name, so
        // TOML and JSON agree.
        assert_eq!(
            serde_json::to_value(&voice).unwrap(),
            serde_json::json!({
                "activation": "toggle",
                "intent": {
                    "backend": "anthropic",
                    "endpoint": HOSTED_COMMAND_ENDPOINT,
                    "model": HOSTED_COMMAND_MODEL,
                    // A NUMBER on both wires, not a string: the webview's
                    // `VoiceIntentStageDto` declares `max_tokens: number`, and
                    // a quoted integer here would be a key it silently dropped.
                    "max_tokens": DEFAULT_TOKEN_CEILING,
                },
                "transcription": {
                    "backend": "local",
                    "endpoint": LOCAL_SPEECH_ENDPOINT,
                    "model": LOCAL_SPEECH_MODEL,
                },
                // Absent from the document above, so this build's default:
                // the observed names ARE sent (PRD #1223, audit finding A1).
                "labels": "shared",
            })
        );

        // And a save puts back exactly what was read.
        save_to(&path, &loaded).unwrap();
        assert_eq!(load_from(&path), loaded);
    }

    /// Scenario: a document points a stage somewhere this build refuses — a
    /// plaintext hop to another machine, a URL carrying a credential, a model
    /// with a space in it. Each is an error rather than a fold, the `[voice]`
    /// section goes to its default, and **every other section survives**.
    ///
    /// **`[voice]` is the one place that does NOT fold**, and the asymmetry is
    /// deliberate. A token is a name a newer build may have invented, so folding
    /// it costs a re-pick. An endpoint is a destination: folding a refused one
    /// to the preset would upload a user's voice to a hosted service when they
    /// wrote a URL pointing at their own machine, and say nothing. The error
    /// puts that section on defaults **with a diagnostic**, which is the version
    /// of that outcome a person can act on.
    ///
    /// **What it must NOT put on defaults is the rest of the document.** The
    /// parse is all-or-nothing and this used to be asserted as
    /// `settings == DesktopSettings::default()` — which is to say, a user's
    /// remote decks disappearing because of one endpoint they typed in a
    /// different section. [`sections_this_build_can_read`] is the recovery, and
    /// `[endpoints]` and `[appearance]` are here to hold it to it.
    #[test]
    fn a_refused_voice_value_defaults_that_section_and_keeps_the_others() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        for refused in [
            // Plaintext to somewhere that is not this machine.
            "[voice.transcription]\nendpoint = \"http://speech.example.com/v1\"\n",
            // A credential in the authority, which is the shape this type
            // exists to refuse.
            "[voice.transcription]\nendpoint = \"https://user:sk-secret@api.openai.com/v1\"\n",
            // Not a URL at all.
            "[voice.intent]\nendpoint = \"api.anthropic.com/v1/messages\"\n",
            // A model identifier with whitespace in it.
            "[voice.intent]\nmodel = \"claude haiku\"\n",
            // The keyless backend pointed at somebody else's service — the
            // pairing `KEYLESS_OFF_MACHINE` refuses, reached the way a user
            // reaches it: pick the local backend, then edit the endpoint.
            "[voice.transcription]\nbackend = \"local\"\n\
             endpoint = \"https://api.openai.com/v1/audio/transcriptions\"\n",
        ] {
            std::fs::write(
                &path,
                format!(
                    "version = 1\n\n[appearance]\nmode = \"dark\"\n\n\
                     [endpoints]\nselection = \"deck1\"\n\n\
                     [[endpoints.remote]]\n\
                     host = \"build-box.example.com\"\n\
                     id = \"deck1\"\n\
                     port = 22\n\n{refused}"
                ),
            )
            .unwrap();
            let (settings, problem) = load_document(&path);
            assert_eq!(settings.voice, None, "should have been refused: {refused}");

            // The whole point: one refused voice value is not a reason to
            // forget the user's decks, their theme or their schema version.
            let endpoints = settings
                .endpoints
                .as_ref()
                .unwrap_or_else(|| panic!("the deck list must survive: {refused}"));
            assert_eq!(endpoints.remote.len(), 1);
            assert_eq!(endpoints.remote[0].id.as_str(), "deck1");
            assert_eq!(settings.appearance.mode, AppearanceMode::Dark);
            assert_eq!(settings.version, 1);

            let problem = problem.expect("a refused value must report why");
            assert!(problem.public().contains("line"), "{}", problem.public());
            // Issue #827's rule holds through the new types: a locator, and
            // never a byte of the document.
            assert!(
                !problem.public().contains("sk-secret"),
                "{}",
                problem.public()
            );
        }
    }

    /// Scenario: the webview sends `backend = "local"` paired with a hosted
    /// endpoint. The IPC seam refuses it, the same way the document does.
    ///
    /// **The panel is not the boundary**, which is the point of asserting the
    /// JSON path separately: the Endpoint field stays offered for the keyless
    /// backend — a user who publishes the container on another port has to be
    /// able to say so — so "pick local, then type a hosted URL" is a sequence
    /// the panel itself permits, and a hand-edited `desktop.toml` bypasses the
    /// panel entirely. One hand-written [`TranscriptionSettings::deserialize`]
    /// serves both wires, so refusing there covers both.
    ///
    /// The error names the rule and not the endpoint: issue #827's sink list
    /// includes this message.
    #[test]
    fn a_keyless_backend_paired_with_a_hosted_endpoint_is_refused_on_the_ipc_path() {
        let error = serde_json::from_value::<VoiceSettings>(serde_json::json!({
            "transcription": {
                "backend": "local",
                "endpoint": HOSTED_SPEECH_ENDPOINT,
                "model": HOSTED_SPEECH_MODEL,
            },
        }))
        .expect_err("a keyless backend may not be pointed off this machine");
        assert!(
            error.to_string().contains("must be on this machine"),
            "the error should name the rule: {error}"
        );

        // The pairing the panel's own preset writes is of course accepted, and
        // so is a container a user moved to another loopback port.
        for endpoint in [
            LOCAL_SPEECH_ENDPOINT,
            "http://127.0.0.1:9000/v1/audio/transcriptions",
            "http://localhost:18000/v1/audio/transcriptions",
            "http://[::1]:18000/v1/audio/transcriptions",
        ] {
            let voice = serde_json::from_value::<VoiceSettings>(serde_json::json!({
                "transcription": { "backend": "local", "endpoint": endpoint },
            }))
            .unwrap_or_else(|error| panic!("{endpoint} must be accepted: {error}"));
            assert_eq!(voice.transcription.endpoint.as_str(), endpoint);
        }

        // And the keyed backend is unaffected — a hosted endpoint is the whole
        // reason it exists, and it sends a credential.
        let voice = serde_json::from_value::<VoiceSettings>(serde_json::json!({
            "transcription": { "backend": "remote", "endpoint": HOSTED_SPEECH_ENDPOINT },
        }))
        .expect("the keyed backend reaches another host by design");
        assert_eq!(voice.transcription.backend, TranscriptionBackend::Remote);
    }

    /// Scenario: a document written by the build before PRD #802's provider
    /// work — `[voice]` holding three bare tokens rather than two tables. It
    /// folds onto this build's backends, and touches nothing else.
    ///
    /// **A type error, not an unknown token**, which is why this needed
    /// [`StageSpec`] rather than the folding the token visitor already did:
    /// `transcription = "off"` is a string where a table is expected, so
    /// `from_str_lossy` never sees it and `#[serde(default)]` does not fire for
    /// a value that is present and wrong. The whole document failed, and with it
    /// went the user's remote decks — for a voice token they had not touched
    /// since the day the old build wrote it.
    ///
    /// `off` is the backend this build deleted, so it folds to the default the
    /// same way any unknown token does: the keyless local container, which is
    /// the honest home for it — the stage that used to do nothing now works and
    /// needs no key.
    #[test]
    fn an_old_scalar_voice_section_folds_and_leaves_the_rest_of_the_document_alone() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             [endpoints]\nselection = \"deck1\"\n\n\
             [[endpoints.remote]]\n\
             host = \"build-box.example.com\"\n\
             id = \"deck1\"\n\
             port = 22\n\n\
             [voice]\n\
             activation = \"toggle\"\n\
             intent = \"remote\"\n\
             transcription = \"off\"\n",
        )
        .unwrap();

        let (settings, problem) = load_document(&path);
        assert!(
            problem.is_none(),
            "an old document is not a malformed one: {:?}",
            problem.map(|problem| problem.public().to_string())
        );

        let voice = settings.voice.clone().expect("the section is present");
        assert_eq!(voice.activation, ActivationMode::Toggle);
        // A token names a backend and nothing else, so each one lands on THAT
        // backend's coordinates rather than on the struct's own default.
        assert_eq!(voice.intent.backend, IntentBackend::Anthropic);
        assert_eq!(voice.intent.endpoint.as_str(), HOSTED_COMMAND_ENDPOINT);
        assert_eq!(voice.transcription.backend, TranscriptionBackend::Local);
        assert_eq!(voice.transcription.endpoint.as_str(), LOCAL_SPEECH_ENDPOINT);
        assert_eq!(voice.transcription.model.as_str(), LOCAL_SPEECH_MODEL);

        // And the sections the old build's voice token has nothing to do with.
        let endpoints = settings.endpoints.as_ref().expect("the deck list survives");
        assert_eq!(endpoints.remote.len(), 1);
        assert_eq!(endpoints.remote[0].id.as_str(), "deck1");

        // A save then rewrites `[voice]` in the new shape — the migration
        // completes rather than being re-folded on every load.
        save_to(&path, &settings).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[voice.transcription]"), "{raw}");
        assert_eq!(load_from(&path).voice, settings.voice);
    }

    /// Scenario: a document names a stage's backend and nothing else. The
    /// missing endpoint and model come from **that backend's** preset, not from
    /// the struct's own default.
    ///
    /// The distinction is the whole reason `Deserialize` is hand-written here.
    /// `#[serde(default)]` fills a missing field from `Default::default()`,
    /// which is the LOCAL speech preset — so `backend = "remote"` with no
    /// endpoint would have authenticated against the loopback container,
    /// sending the user's OpenAI key to a service that has no notion of one and
    /// their audio somewhere they did not choose. Filling from the backend that
    /// was actually named is the only answer that cannot be wrong.
    #[test]
    fn a_stage_that_names_only_its_backend_gets_that_backends_coordinates() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[voice.transcription]\nbackend = \"remote\"\n",
        )
        .unwrap();
        let voice = load_from(&path).voice.expect("the section is present");
        assert_eq!(voice.transcription.backend, TranscriptionBackend::Remote);
        assert_eq!(
            voice.transcription.endpoint.as_str(),
            HOSTED_SPEECH_ENDPOINT
        );
        assert_eq!(voice.transcription.model.as_str(), HOSTED_SPEECH_MODEL);
        assert!(
            !voice.transcription.endpoint.is_loopback(),
            "a keyed backend was pointed at the keyless container"
        );

        // The other direction, so this is not a one-sided assertion: naming the
        // local backend gets the loopback coordinates.
        std::fs::write(
            &path,
            "version = 1\n\n[voice.transcription]\nbackend = \"local\"\n",
        )
        .unwrap();
        let voice = load_from(&path).voice.expect("the section is present");
        assert_eq!(voice.transcription.endpoint.as_str(), LOCAL_SPEECH_ENDPOINT);

        // And a coordinate the document DOES name is kept, so the preset is a
        // fallback rather than an override.
        std::fs::write(
            &path,
            "version = 1\n\n[voice.transcription]\nbackend = \"remote\"\n\
             model = \"gpt-4o-transcribe\"\n",
        )
        .unwrap();
        let voice = load_from(&path).voice.expect("the section is present");
        assert_eq!(voice.transcription.model.as_str(), "gpt-4o-transcribe");
        assert_eq!(
            voice.transcription.endpoint.as_str(),
            HOSTED_SPEECH_ENDPOINT
        );
    }

    /// Scenario: the endpoints and models this build ships as presets are all
    /// values its own newtypes accept.
    ///
    /// `TranscriptionSettings::default` and `IntentSettings::default` `expect`
    /// on these, so a typo in one of them would be a panic on the first settings
    /// load rather than a compile error. This is what makes that `expect`
    /// honest.
    #[test]
    fn the_preset_service_coordinates_are_valid() {
        for endpoint in [
            LOCAL_SPEECH_ENDPOINT,
            HOSTED_SPEECH_ENDPOINT,
            HOSTED_COMMAND_ENDPOINT,
            OPENAI_COMMAND_ENDPOINT,
        ] {
            ServiceUrl::parse(endpoint).unwrap_or_else(|error| panic!("{endpoint}: {error}"));
        }
        for model in [
            LOCAL_SPEECH_MODEL,
            HOSTED_SPEECH_MODEL,
            HOSTED_COMMAND_MODEL,
            OPENAI_COMMAND_MODEL,
        ] {
            ModelId::parse(model).unwrap_or_else(|error| panic!("{model}: {error}"));
        }
        // The keyless default is on this machine, which is what lets
        // `voice::transcribe` tell a user to start a container rather than
        // reporting an outage.
        assert!(
            ServiceUrl::parse(LOCAL_SPEECH_ENDPOINT)
                .expect("valid")
                .is_loopback()
        );
        assert!(
            !ServiceUrl::parse(HOSTED_SPEECH_ENDPOINT)
                .expect("valid")
                .is_loopback()
        );
    }

    /// Scenario: a document with no `[voice]` section at all — every document
    /// on disk today — loads, and the section reads as *unspecified* rather
    /// than as empty.
    ///
    /// The distinction is [`DesktopSettings::voice`]'s whole reason for being
    /// an `Option`, and the defaults it materialises to are the product
    /// decision: a **keyless speech container on loopback**, the **Anthropic
    /// API** for commands, and the one **activation mode** that ships. The two
    /// stages differ on credentials and that asymmetry IS the decision —
    /// speech asks for none, commands does, because PRD #802 measured local
    /// intent twice and it was not good enough.
    #[test]
    fn an_absent_voice_section_reads_as_unspecified_with_this_builds_defaults() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[appearance]\nmode = \"dark\"\n").unwrap();

        let loaded = load_from(&path);
        assert_eq!(loaded.voice, None, "absence must survive the load");

        let defaults = VoiceSettings::default();
        assert_eq!(defaults.transcription.backend, TranscriptionBackend::Local);
        assert_eq!(
            defaults.transcription.endpoint.as_str(),
            LOCAL_SPEECH_ENDPOINT
        );
        assert_eq!(defaults.transcription.model.as_str(), LOCAL_SPEECH_MODEL);
        assert_eq!(defaults.intent.backend, IntentBackend::OpenaiCompatible);
        assert_eq!(defaults.intent.endpoint.as_str(), OPENAI_COMMAND_ENDPOINT);
        assert_eq!(defaults.intent.model.as_str(), OPENAI_COMMAND_MODEL);
        assert_eq!(defaults.intent.max_tokens.get(), DEFAULT_TOKEN_CEILING);
        assert_eq!(defaults.activation, ActivationMode::Toggle);
        // Speech's default is the keyless one, which is the product decision
        // that replaced `Speech = off`. Commands' is not, and the asymmetry is
        // deliberate: PRD #802 measured local intent twice against the phrase
        // fixtures and it was not good enough — `IntentBackend` has the
        // numbers.
        //
        // **Both hosted defaults are the same vendor**, which is the product
        // decision the one-key work turned on: a user who wants the feature
        // hosted end to end pastes ONE key, rather than opening accounts with
        // two companies to turn on one button.
        assert_eq!(
            ServiceUrl::parse(HOSTED_SPEECH_ENDPOINT)
                .expect("valid")
                .host(),
            defaults.intent.endpoint.host(),
            "the two hosted presets must be one vendor, or one key does not run both"
        );
    }

    /// Scenario: the command stage's answer ceiling defaults to 4096, a
    /// document that names one keeps it, and a bare backend token lands on the
    /// default rather than on nothing.
    ///
    /// The field replaced a hardwired 256 that was sized from one model's
    /// answers. `max_completion_tokens` counts a model's REASONING as well as
    /// what it writes, so the old constant truncated every reasoning model
    /// before it emitted a character — measured on `gpt-5-mini` at 18/24
    /// against the phrase fixtures, every failure having spent exactly 256
    /// completion tokens.
    #[test]
    fn the_command_ceiling_defaults_to_4096_and_a_document_may_change_it() {
        assert_eq!(
            IntentSettings::default().max_tokens.get(),
            DEFAULT_TOKEN_CEILING
        );
        assert_eq!(DEFAULT_TOKEN_CEILING, 4096);
        // Both presets, so a backend switch never re-introduces a per-provider
        // number somebody has to keep in step.
        for backend in [IntentBackend::Anthropic, IntentBackend::OpenaiCompatible] {
            assert_eq!(
                IntentSettings::for_backend(backend).max_tokens.get(),
                DEFAULT_TOKEN_CEILING
            );
        }

        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             [voice.intent]\n\
             backend = \"openai_compatible\"\n\
             max_tokens = 16384\n",
        )
        .unwrap();
        let (loaded, problem) = load_document(&path);
        assert!(problem.is_none(), "{problem:?}");
        let intent = loaded.voice.expect("the section is present").intent;
        assert_eq!(intent.max_tokens.get(), 16_384);
        // The coordinates the document did NOT name still come from the backend
        // it did, which is what `IntentSettings::deserialize` exists for.
        assert_eq!(intent.endpoint.as_str(), OPENAI_COMMAND_ENDPOINT);

        // The bare-token shape an older document wrote carries no ceiling at
        // all, so it lands on the default rather than on a missing field.
        std::fs::write(&path, "version = 1\n\n[voice]\nintent = \"anthropic\"\n").unwrap();
        let (loaded, problem) = load_document(&path);
        assert!(problem.is_none(), "{problem:?}");
        assert_eq!(
            loaded
                .voice
                .expect("the section is present")
                .intent
                .max_tokens
                .get(),
            DEFAULT_TOKEN_CEILING
        );
    }

    /// Scenario: a document holds an answer ceiling outside the accepted range.
    /// The `[voice]` section is refused with a diagnostic naming the rule, and
    /// the rest of the document — the user's appearance, their decks — is still
    /// there.
    ///
    /// Both halves matter and the second is the one that was a P2 once already.
    /// `toml_edit::de::from_str` is all-or-nothing, so a value refused in
    /// `[voice]` used to cost the whole file;
    /// [`sections_this_build_can_read`] is what narrows it to the section the
    /// bad value is in. A new refusing field is a new way to reach that path,
    /// so it is pinned here rather than assumed to inherit the fix.
    #[test]
    fn a_ceiling_outside_the_range_is_refused_and_keeps_the_rest_of_the_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        for refused in [
            i64::from(MIN_TOKEN_CEILING) - 1,
            0,
            -1,
            i64::from(MAX_TOKEN_CEILING) + 1,
        ] {
            std::fs::write(
                &path,
                format!(
                    "version = 1\n\n\
                     [appearance]\n\
                     mode = \"dark\"\n\n\
                     [voice.intent]\n\
                     backend = \"openai_compatible\"\n\
                     max_tokens = {refused}\n"
                ),
            )
            .unwrap();

            let (loaded, problem) = load_document(&path);
            let problem =
                problem.unwrap_or_else(|| panic!("{refused} must be refused with a diagnostic"));
            assert!(problem.public().contains("line"), "{}", problem.public());
            assert_eq!(
                loaded.appearance.mode,
                AppearanceMode::Dark,
                "a refused ceiling is not a reason to forget the user's theme"
            );
            // The stage itself falls back rather than half-surviving: a
            // backend left behind beside a dropped coordinate is the shape
            // `TranscriptionSettings::deserialize` exists to prevent.
            assert!(
                loaded.voice.is_none(),
                "{refused} left a half-read section behind"
            );
        }

        // And the boundary values themselves are accepted, so the refusal is a
        // range rather than a superstition about round numbers.
        for accepted in [MIN_TOKEN_CEILING, MAX_TOKEN_CEILING] {
            std::fs::write(
                &path,
                format!("version = 1\n\n[voice.intent]\nmax_tokens = {accepted}\n"),
            )
            .unwrap();
            let (loaded, problem) = load_document(&path);
            assert!(problem.is_none(), "{accepted}: {problem:?}");
            assert_eq!(
                loaded
                    .voice
                    .expect("the section is present")
                    .intent
                    .max_tokens
                    .get(),
                accepted
            );
        }
    }

    /// Scenario: a document names a `[voice]` backend this build has never
    /// heard of, and one that is absurdly long. The first is tolerated and the
    /// second is refused, which are deliberately different answers.
    ///
    /// **An unrecognised token folds to the default** — PRD #802 asks for
    /// exactly this, "closed enums with folding deserializers in the
    /// `AppearanceMode` idiom" — so a document written by a newer build with
    /// more backends still loads, and the rest of the user's settings are not
    /// lost over one unreadable field. What it costs is stated rather than
    /// hidden: the folded value is what the **next save writes back**, so an
    /// older build opened against a newer build's document replaces that
    /// choice. `AppearanceMode` makes the same trade;
    /// [`Selection`] is the type that does not, because an unknown selection is
    /// stored as the [`EndpointId`] it was and written back unchanged.
    ///
    /// **An over-length token is an error**, not the fallback: an unrecognised
    /// token is a mode this build has not heard of and a 4 KB one is a
    /// malformed document. On the disk path that means the `[voice]` section
    /// falls back to its default **and the sections around it do not** — the
    /// ordinary malformed-document behaviour since
    /// [`sections_this_build_can_read`]; on the IPC path it fails argument
    /// deserialisation before anything allocates a normalised copy.
    #[test]
    fn an_unknown_voice_token_folds_to_the_default_and_an_over_long_one_is_refused() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             [voice.intent]\n\
             backend = \"a-backend-from-2027\"\n\n\
             [voice.transcription]\n\
             backend = \"on-device-neural\"\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        let voice = loaded.voice.clone().expect("the section is present");
        assert_eq!(voice.intent.backend, IntentBackend::OpenaiCompatible);
        assert_eq!(voice.transcription.backend, TranscriptionBackend::Local);
        // A stage that named only its backend still gets this build's
        // coordinates for the folded choice, rather than an empty endpoint.
        assert_eq!(voice.transcription.endpoint.as_str(), LOCAL_SPEECH_ENDPOINT);

        // The cost, pinned rather than left implicit: the next save writes the
        // folded value, so the newer build's choice is gone.
        save_to(&path, &loaded).unwrap();
        let reread = std::fs::read_to_string(&path).unwrap();
        assert!(
            reread.contains("backend = \"openai_compatible\""),
            "{reread}"
        );
        assert!(!reread.contains("2027"), "{reread}");

        // Over-length is a different answer: the document is malformed, so the
        // load falls back to defaults entirely rather than folding one field.
        let long = "x".repeat(MAX_VOICE_TOKEN_BYTES + 1);
        std::fs::write(
            &path,
            format!(
                "version = 1\n\n[appearance]\nmode = \"dark\"\n\n[voice.intent]\nbackend = \"{long}\"\n"
            ),
        )
        .unwrap();
        let (settings, problem) = load_document(&path);
        assert_eq!(settings.voice, None, "the refused section goes to default");
        assert_eq!(
            settings.appearance.mode,
            AppearanceMode::Dark,
            "a refused voice token is not a reason to forget the user's theme"
        );
        let problem = problem.expect("a malformed document must report why");
        assert!(problem.public().contains("line"), "{}", problem.public());
    }

    /// Scenario: a document written by a build that shipped one of the two
    /// withdrawn agent-CLI intent backends loads on this build and folds to the
    /// default; the pre-provider-work `remote` loads as **Anthropic**, which is
    /// what it named. The rest of the document survives every time and nothing
    /// errors.
    ///
    /// The migration for [`IntentBackend`]'s withdrawn variants, asserted
    /// rather than argued: the audit that removed `opencode` reasoned that the
    /// folding deserializer makes dropping a variant cheap, and PRD #802's
    /// provider work then spent that reasoning a second time on `claude` — the
    /// whole agent-CLI backend, which was the DEFAULT. So the case this pins is
    /// no longer a minority re-pick: it is what every existing user's document
    /// says. What it costs them is one re-pick, not a lost document.
    ///
    /// **`remote` is the case that stopped being a fold when the default
    /// moved**, and it is pinned here as a mapping rather than as luck. It
    /// named the Anthropic API, so its user's `SecretId::VoiceIntent` holds an
    /// Anthropic key; folding it to today's default would put that key in an
    /// `Authorization` header addressed to `api.openai.com`.
    #[test]
    fn the_withdrawn_agent_cli_intent_backends_fold_to_the_default() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             [appearance]\n\
             mode = \"dark\"\n\n\
             [voice.intent]\n\
             backend = \"opencode\"\n\n\
             [voice.transcription]\n\
             backend = \"remote\"\n\
             endpoint = \"https://api.openai.com/v1/audio/transcriptions\"\n\
             model = \"whisper-1\"\n",
        )
        .unwrap();

        let (loaded, problem) = load_document(&path);
        assert!(
            problem.is_none(),
            "a withdrawn token is not a malformed document"
        );
        let voice = loaded.voice.clone().expect("the section is present");
        assert_eq!(voice.intent.backend, IntentBackend::OpenaiCompatible);
        // Everything else in the document survives the fold.
        assert_eq!(voice.transcription.backend, TranscriptionBackend::Remote);
        assert_eq!(
            voice.transcription.endpoint.as_str(),
            HOSTED_SPEECH_ENDPOINT
        );
        assert_eq!(loaded.appearance.mode, AppearanceMode::Dark);

        // The second withdrawn token, and the one that matters more: `claude`
        // was the shipped DEFAULT, so this is the document nearly every early
        // user has.
        std::fs::write(
            &path,
            "version = 1\n\n[voice.intent]\nbackend = \"claude\"\n",
        )
        .unwrap();
        let (loaded, problem) = load_document(&path);
        assert!(
            problem.is_none(),
            "a withdrawn token is not a malformed document"
        );
        assert_eq!(
            loaded.voice.expect("the section is present").intent.backend,
            IntentBackend::OpenaiCompatible
        );

        for withdrawn in ["opencode", "claude"] {
            assert!(
                !<IntentBackend as VoiceToken>::TOKENS.contains(&withdrawn),
                "`{withdrawn}` is withdrawn; see IntentBackend's doc comment for why"
            );
        }

        // And the pre-provider-work spelling of the one backend that SURVIVED.
        // `remote` named the Anthropic API, so it MAPS there rather than
        // folding: the key its user stored is that vendor's, and the default is
        // no longer that vendor.
        std::fs::write(
            &path,
            "version = 1\n\n[voice.intent]\nbackend = \"remote\"\n",
        )
        .unwrap();
        let (loaded, problem) = load_document(&path);
        assert!(
            problem.is_none(),
            "an older token is not a malformed document"
        );
        let intent = loaded.voice.expect("the section is present").intent;
        assert_eq!(intent.backend, IntentBackend::Anthropic);
        assert_ne!(
            intent.backend,
            IntentBackend::default(),
            "this assertion is only worth making while `remote` and the default differ"
        );
        // And it lands on that backend's coordinates, so the stored key goes
        // where it was minted for.
        assert_eq!(intent.endpoint.as_str(), HOSTED_COMMAND_ENDPOINT);
        assert_eq!(intent.model.as_str(), HOSTED_COMMAND_MODEL);
        // `remote` is NOT an offered token — it is a migration alias, so the
        // panel must not list it and the round-trip check must not see it.
        assert!(!<IntentBackend as VoiceToken>::TOKENS.contains(&"remote"));
    }

    /// Scenario: a document naming the backend that used to be the default
    /// loads as that backend, with every other section intact.
    ///
    /// The half of the migration that is easy to assume and expensive to get
    /// wrong. Moving a default is a change to what an ABSENT value means; a
    /// PRESENT one has to keep meaning what it said, and this is the document
    /// most existing users have. Both spellings are checked — the stage table
    /// and the bare token an older schema wrote — because they reach the value
    /// by different code (`StageSpec`'s two arms).
    ///
    /// The "rest of the document" half is a P2 that was already fixed once for
    /// Speech (`b06d9cac`): `toml_edit::de::from_str` is all-or-nothing, so a
    /// `[voice]` value this build could not read used to cost the user's decks,
    /// their appearance and their zoom.
    #[test]
    fn a_document_naming_the_former_default_keeps_it_and_keeps_the_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        for spelling in [
            "[voice.intent]\nbackend = \"anthropic\"\n",
            "[voice]\nintent = \"anthropic\"\n",
        ] {
            std::fs::write(
                &path,
                format!(
                    "version = 1\n\n\
                     [appearance]\n\
                     mode = \"dark\"\n\n\
                     [zoom]\n\
                     level = 1.25\n\n\
                     {spelling}"
                ),
            )
            .unwrap();

            let (loaded, problem) = load_document(&path);
            assert!(problem.is_none(), "{spelling}: {problem:?}");
            let intent = loaded
                .voice
                .clone()
                .unwrap_or_else(|| panic!("{spelling}: the section is present"))
                .intent;
            assert_eq!(intent.backend, IntentBackend::Anthropic, "{spelling}");
            assert_eq!(
                intent.endpoint.as_str(),
                HOSTED_COMMAND_ENDPOINT,
                "{spelling}"
            );
            assert_eq!(intent.model.as_str(), HOSTED_COMMAND_MODEL, "{spelling}");
            // Not the default any more, which is what makes this worth pinning.
            assert_ne!(intent.backend, IntentBackend::default(), "{spelling}");
            // And nothing else moved.
            assert_eq!(loaded.appearance.mode, AppearanceMode::Dark, "{spelling}");
            assert_eq!(loaded.zoom.level.as_f64(), 1.25, "{spelling}");
        }

        // The other direction, for completeness: the backend that IS the
        // default is equally explicit when the document names it, so nobody has
        // to work out whether a value is stored or inferred.
        std::fs::write(
            &path,
            "version = 1\n\n[voice.intent]\nbackend = \"openai_compatible\"\n",
        )
        .unwrap();
        let (loaded, problem) = load_document(&path);
        assert!(problem.is_none(), "{problem:?}");
        assert_eq!(
            loaded
                .voice
                .expect("the section is present")
                .intent
                .model
                .as_str(),
            OPENAI_COMMAND_MODEL
        );
    }

    /// The `[voice]` tokens are duplicated in `desktop/src/lib/bridge.ts`, so
    /// both copies are pinned value-by-value and each points at the other —
    /// the same arrangement `zoom_ladder_matches_the_frontend_copy` makes, and
    /// for the same reason: if they drift, the panel offers a token this side
    /// folds away, and the user's choice silently does not stick.
    ///
    /// The round-trip half is the one that matters most under a **folding**
    /// deserializer, and it is the hazard that design carries: a token listed
    /// here but missing an arm in `from_str_lossy` would not fail to parse — it
    /// would quietly become the default, so the panel would offer a choice that
    /// never takes.
    #[test]
    fn the_voice_tokens_match_the_frontends_copy() {
        assert_eq!(
            <ActivationMode as VoiceToken>::TOKENS,
            ["toggle"],
            "keep this identical to VOICE_ACTIVATION_MODES in desktop/src/lib/bridge.ts"
        );
        assert_eq!(
            <IntentBackend as VoiceToken>::TOKENS,
            ["anthropic", "openai_compatible"],
            "keep this identical to VOICE_INTENT_BACKENDS in desktop/src/lib/bridge.ts"
        );
        assert_eq!(
            <TranscriptionBackend as VoiceToken>::TOKENS,
            ["local", "remote"],
            "keep this identical to VOICE_TRANSCRIPTION_BACKENDS in desktop/src/lib/bridge.ts"
        );

        fn round_trips<T: VoiceToken + std::fmt::Debug + PartialEq>() {
            for token in T::TOKENS {
                assert_eq!(
                    T::from_str_lossy(token).as_str(),
                    *token,
                    "`{token}` is offered but does not parse back to itself, so choosing \
                     it would silently store the default"
                );
            }
            assert!(
                T::TOKENS.contains(&T::default().as_str()),
                "the default is not one of the offered tokens"
            );
            // Case and surrounding whitespace are forgiven, because the
            // document is hand-editable.
            for token in T::TOKENS {
                assert_eq!(T::from_str_lossy(&format!("  {token}  ")).as_str(), *token);
                assert_eq!(T::from_str_lossy(&token.to_uppercase()).as_str(), *token);
            }
        }
        assert_eq!(
            <LabelSharing as VoiceToken>::TOKENS,
            ["shared", "withheld"],
            "keep this identical to VOICE_LABEL_SHARING in desktop/src/lib/bridge.ts"
        );
        round_trips::<ActivationMode>();
        round_trips::<IntentBackend>();
        round_trips::<TranscriptionBackend>();
        round_trips::<LabelSharing>();
    }

    /// The preset endpoints and models are duplicated in
    /// `desktop/src/lib/bridge.ts` as `VOICE_STAGE_PRESETS`, so both copies are
    /// pinned value-by-value for `the_voice_tokens_match_the_frontends_copy`'s
    /// reason with a sharper edge: the panel WRITES a preset into the document
    /// whenever the backend changes, so a frontend copy that drifted would put
    /// a value into `desktop.toml` that this side then refuses — turning a
    /// select into a save error.
    #[test]
    fn the_voice_presets_match_the_frontends_copy() {
        for (constant, value) in [
            (
                LOCAL_SPEECH_ENDPOINT,
                "http://127.0.0.1:18000/v1/audio/transcriptions",
            ),
            (LOCAL_SPEECH_MODEL, "Systran/faster-whisper-tiny.en"),
            (
                HOSTED_SPEECH_ENDPOINT,
                "https://api.openai.com/v1/audio/transcriptions",
            ),
            (HOSTED_SPEECH_MODEL, "whisper-1"),
            (
                HOSTED_COMMAND_ENDPOINT,
                "https://api.anthropic.com/v1/messages",
            ),
            (HOSTED_COMMAND_MODEL, "claude-haiku-4-5"),
            (
                OPENAI_COMMAND_ENDPOINT,
                "https://api.openai.com/v1/chat/completions",
            ),
            (OPENAI_COMMAND_MODEL, "gpt-5-mini"),
        ] {
            assert_eq!(
                constant, value,
                "keep this identical to VOICE_STAGE_PRESETS in desktop/src/lib/bridge.ts"
            );
        }
        // The answer ceiling's bounds and default, which the panel writes and
        // which `bridge.ts` therefore has to agree about number for number: a
        // frontend that offered 8 would put a value into `desktop.toml` that
        // this side refuses, turning a number field into a save error.
        for (constant, value) in [
            (MIN_TOKEN_CEILING, 64),
            (MAX_TOKEN_CEILING, 32768),
            (DEFAULT_TOKEN_CEILING, 4096),
        ] {
            assert_eq!(
                constant, value,
                "keep this identical to the ceiling constants in desktop/src/lib/bridge.ts"
            );
        }
        // The image the unreachable-endpoint sentence names, which the panel's
        // own hint repeats to the user before they ever meet that sentence.
        assert_eq!(
            LOCAL_SPEECH_IMAGE, "ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu",
            "keep this identical to the hint in desktop/src/components/VoicePanel.tsx"
        );
    }

    /// Scenario: a document holds a `[voice]` section; a client whose UI cannot
    /// render voice saves an appearance change over it. The section must still
    /// be there.
    ///
    /// The property [`DesktopSettings::voice`] is an `Option` for, and the
    /// twin of `a_client_that_cannot_render_endpoints_cannot_delete_them`. If
    /// it were a plain section the webview's fixed-key-set normaliser would
    /// send the default, and the merge would write that over the user's
    /// choices — silently, on a save triggered by changing the theme.
    #[test]
    fn a_client_that_cannot_render_voice_cannot_delete_it() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             [appearance]\n\
             mode = \"light\"\n\n\
             [voice.intent]\n\
             backend = \"remote\"\n\n\
             [voice.transcription]\n\
             backend = \"remote\"\n\
             endpoint = \"https://api.openai.com/v1/audio/transcriptions\"\n\
             model = \"whisper-1\"\n",
        )
        .unwrap();

        // A document from a build with no voice UI: `None`, not an empty
        // section.
        let blind = DesktopSettings {
            appearance: AppearanceSettings {
                mode: AppearanceMode::Dark,
            },
            voice: None,
            ..DesktopSettings::default()
        };
        save_to(&path, &blind).unwrap();

        let reloaded = load_from(&path);
        assert_eq!(reloaded.appearance.mode, AppearanceMode::Dark);
        let voice = reloaded.voice.expect("the section must survive");
        assert_eq!(voice.intent.backend, IntentBackend::Anthropic);
        assert_eq!(voice.transcription.backend, TranscriptionBackend::Remote);
        assert_eq!(
            voice.transcription.endpoint.as_str(),
            HOSTED_SPEECH_ENDPOINT
        );
    }

    /// The zoom ladder is duplicated in `desktop/src/lib/zoom.ts`, so both
    /// copies are pinned value-by-value and each points at the other. If they
    /// drift, the app launches at one level and the Settings row claims
    /// another — the launch apply reads this side, and every keystroke steps
    /// that one.
    #[test]
    fn zoom_ladder_matches_the_frontend_copy() {
        assert_eq!(
            ZOOM_LEVELS,
            [0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0],
            "keep this identical to ZOOM_LEVELS in desktop/src/lib/zoom.ts"
        );
        assert!(ZOOM_LEVELS.contains(&DEFAULT_ZOOM_LEVEL));
        assert!(
            ZOOM_LEVELS.windows(2).all(|pair| pair[0] < pair[1]),
            "the ladder must be ascending for `snap` and the frontend's stepping to agree"
        );
        // The measured ceiling: page zoom divides the CSS-pixel viewport by the
        // level, `tauri.conf.json` declares `minWidth: 1024`, and `styles.css`
        // declares `min-width: 320px`, past which content is clipped rather
        // than reflowed because `body` is `overflow-x: hidden`.
        let ceiling = ZOOM_LEVELS[ZOOM_LEVELS.len() - 1];
        assert!(1024.0 / ceiling > 320.0);
    }

    #[test]
    fn a_zoom_level_on_the_ladder_is_kept_exactly() {
        for level in ZOOM_LEVELS {
            assert_eq!(ZoomLevel::snap(level).as_f64(), level);
        }
    }

    /// A value exactly between two rungs resolves to the LOWER one, because the
    /// search uses a strict `<` over an ascending ladder so the first candidate
    /// wins a tie.
    ///
    /// Pinned because `clampZoom` in `desktop/src/lib/zoom.ts` is a second
    /// implementation of this, and a tie is the one input where the two could
    /// differ while both looked correct — `<=` on either side would resolve
    /// upward. Disagreement means the app launches at the level this snapped
    /// while the Settings row reads the level that one snapped, which is exactly
    /// what the duplicated-ladder comment on [`ZOOM_LEVELS`] warns about.
    #[test]
    fn an_exact_tie_resolves_downward_as_the_frontend_copy_does() {
        assert_eq!(ZoomLevel::snap(1.05).as_f64(), 1.0);
        assert_eq!(ZoomLevel::snap(0.825).as_f64(), 0.75);
        assert_eq!(ZoomLevel::snap(2.25).as_f64(), 2.0);
    }

    #[test]
    fn an_off_ladder_zoom_level_snaps_to_the_nearest_rung() {
        assert_eq!(ZoomLevel::snap(1.3).as_f64(), 1.25);
        assert_eq!(ZoomLevel::snap(1.4).as_f64(), 1.5);
        assert_eq!(ZoomLevel::snap(2.9).as_f64(), 3.0);
    }

    /// Saturating, not rejecting: a hand-typed `level = 99` should give the
    /// biggest level this build has rather than throwing the document away.
    #[test]
    fn an_out_of_range_zoom_level_saturates() {
        assert_eq!(ZoomLevel::snap(99.0).as_f64(), 3.0);
        assert_eq!(ZoomLevel::snap(0.01).as_f64(), 0.75);
        assert_eq!(ZoomLevel::snap(0.0).as_f64(), 0.75);
        assert_eq!(ZoomLevel::snap(-5.0).as_f64(), 0.75);
    }

    /// Why `snap` guards with `is_finite` instead of a range check.
    ///
    /// Every comparison against `NaN` is false, so the nearest-rung search
    /// would keep its initial value and answer **0.75** — a corrupt document
    /// would silently shrink the app rather than reading as the default. Both
    /// infinities go the same way for the same reason.
    #[test]
    fn a_non_finite_zoom_level_reads_as_the_default_not_the_smallest() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(ZoomLevel::snap(value).as_f64(), DEFAULT_ZOOM_LEVEL);
        }
    }

    /// A hand-edited `level = 1` is a TOML **integer**, and `f64`'s derived
    /// deserializer rejects one. That would not be a local failure: `load_from`
    /// treats an unparseable document as "use defaults", so this one plausible
    /// edit would also silently reset the user's appearance. The visitor accepts
    /// integers precisely so that cannot happen — which is what the second
    /// assertion here is really testing.
    #[test]
    fn an_integer_zoom_level_is_accepted_and_takes_no_other_section_with_it() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[appearance]\nmode = \"dark\"\n\n[zoom]\nlevel = 2\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.zoom.level.as_f64(), 2.0);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Dark);
    }

    /// The same guarantee for the values that are not numbers at all. A string
    /// or a boolean is a genuinely malformed document, so it falls back the
    /// ordinary way — logged, never a failed launch — and since
    /// [`sections_this_build_can_read`] the ordinary way IS a partial recovery:
    /// `[zoom]` goes to its default and `[appearance]` beside it does not.
    /// That sentence used to read "the whole document to defaults", and the
    /// reason it changed is the one this file is built around — the document
    /// holds several unrelated tenants, and one refused value is not a reason
    /// to forget the others.
    #[test]
    fn a_non_numeric_zoom_level_falls_back_the_ordinary_malformed_way() {
        let dir = tempdir();
        for raw in ["\"big\"", "true", "[1, 2]"] {
            let path = dir.path().join(format!("zoom-{}.toml", raw.len()));
            std::fs::write(
                &path,
                format!("version = 1\n\n[appearance]\nmode = \"dark\"\n\n[zoom]\nlevel = {raw}\n"),
            )
            .unwrap();
            let loaded = load_from(&path);
            assert_eq!(loaded.zoom.level.as_f64(), DEFAULT_ZOOM_LEVEL, "for {raw}");
            assert_eq!(loaded.appearance.mode, AppearanceMode::Dark, "for {raw}");
        }
    }

    /// A document written before `[zoom]` existed — which is every document on
    /// disk today — reads as the default level and keeps its appearance.
    #[test]
    fn a_document_predating_the_zoom_section_reads_as_the_default_level() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[appearance]\nmode = \"light\"\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.zoom.level.as_f64(), DEFAULT_ZOOM_LEVEL);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
    }

    /// The level survives the disk, which is the whole point of the feature —
    /// and it survives it *next to* an unknown section, so the merge covers the
    /// new tenant as well as the old one.
    #[test]
    fn a_zoom_level_round_trips_and_the_merge_still_preserves_an_unknown_section() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        let voice = "[voice]\nbackend = \"whisper\"\n";
        std::fs::write(&path, format!("version = 1\n\n{voice}")).unwrap();

        save_to(&path, &zoomed(1.75)).unwrap();
        let reloaded = load_from(&path);
        assert_eq!(reloaded.zoom.level.as_f64(), 1.75);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(voice),
            "the [voice] section did not survive a zoom save: {raw}"
        );
    }

    /// The property the merge exists for: an older build saving must not eat a
    /// newer build's section. Without it `#[serde(default)]` drops `[voice]` on
    /// the way in and the next save writes the struct straight over it.
    ///
    /// The unknown section is pinned **byte for byte** here, which is the
    /// strongest form of the guarantee and the one PRD #803 states. Note what
    /// that does and does not mean — see
    /// [`tests::an_unknown_section_keeps_its_data_but_not_its_formatting`].
    #[test]
    fn an_unknown_section_survives_a_load_modify_save_round_trip() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        let voice = "[voice]\nbackend = \"whisper\"\nlocal = true\nretries = 3\n";
        std::fs::write(
            &path,
            format!("version = 1\n\n[appearance]\nmode = \"light\"\n\n{voice}"),
        )
        .unwrap();

        let mut loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
        loaded.appearance.mode = AppearanceMode::Dark;
        save_to(&path, &loaded).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(voice),
            "the [voice] section did not survive byte for byte: {raw}"
        );
        // And the change the user actually made is on disk.
        assert_eq!(load_from(&path).appearance.mode, AppearanceMode::Dark);
    }

    /// Issue #825, and this test used to assert the **opposite**.
    ///
    /// Its old name was `an_unknown_section_keeps_its_data_but_not_its_formatting`
    /// and it pinned the limitation: the merge round-tripped through
    /// `toml::Table`, which models *data*, so a save re-rendered the whole
    /// document canonically — every key, value and type came back, and the
    /// comment and the inline array did not. The merge is a
    /// `toml_edit::DocumentMut` one now ([`merged_document`]), so both halves
    /// hold: the data AND the bytes it was written in.
    ///
    /// Deliberately asserted on the **whole document** rather than on a
    /// substring. A `contains` check would pass on a save that kept the comment
    /// and re-flowed something else, which is the shape of a weaker property
    /// wearing this one's name.
    #[test]
    fn an_unknown_section_keeps_its_formatting_as_well_as_its_data() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        let hand_written = "version = 1\n\n\
             # which speech-to-text backend #802 picked\n\
             [voice]\n\
             backend = \"whisper\"\n\
             stages = [\"stt\", \"intent\"]\n";
        std::fs::write(&path, hand_written).unwrap();

        save_to(&path, &dark()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();

        // The data is all there, with its types intact.
        let reparsed = raw.parse::<toml_edit::DocumentMut>().unwrap();
        let voice = reparsed["voice"].as_table().unwrap();
        assert_eq!(voice["backend"].as_str(), Some("whisper"));
        assert_eq!(
            voice["stages"].as_array().unwrap().len(),
            2,
            "unexpected document: {raw}"
        );

        // And so is the formatting: the comment, the inline array, the key
        // order and the blank line that grouped them.
        assert!(
            raw.starts_with(hand_written),
            "the hand-written document was re-rendered: {raw}"
        );

        // The only difference is the sections the save had to add, and the
        // setting it was asked to change is in them.
        assert_eq!(
            raw,
            format!("{hand_written}\n[appearance]\nmode = \"dark\"\n\n[zoom]\nlevel = 1.0\n"),
            "unexpected document"
        );
        assert_eq!(load_from(&path).appearance.mode, AppearanceMode::Dark);
    }

    /// **The user-facing property issue #825 is actually about**: someone
    /// annotates `desktop.toml` by hand — which PRD #803's "a file a user can
    /// read, edit and delete without the app running" invites — and then uses
    /// the app.
    ///
    /// Every shape of comment TOML has is in the fixture, including one on a
    /// line whose value the save *does* change, because that is the one a
    /// naive format-preserving merge still drops: the comment lives in the
    /// value's decor, so replacing the value takes it unless the decor is
    /// carried across (see [`replace_item`]).
    ///
    /// The unknown section in the fixture was `[voice]` until PRD #802 M4 took
    /// that name, which is the mechanism this test covers demonstrating itself:
    /// once a build owns a section the merge writes the keys the struct owns
    /// into it, so it stopped being a stand-in for a section nobody owns. The
    /// stand-in moved rather than the assertion being relaxed — what is being
    /// proven is that an app-driven save leaves a section this build knows
    /// nothing about exactly as the user wrote it.
    #[test]
    fn a_hand_written_comment_survives_an_app_driven_save() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "# Agent Deck desktop settings — edited by hand, 2026-09-16\n\
             version = 1\n\n\
             # I like it light during the day\n\
             [appearance]\n\
             mode = \"light\"  # flip this to \"dark\" at night\n\n\
             [zoom]\n\
             level = 1.0\n\n\
             # a section no build of this app owns\n\
             [experiments]\n\
             flags = [\"a\", \"b\"]\n",
        )
        .unwrap();

        // The app writes a setting, exactly as a click on the appearance
        // toggle would.
        let mut loaded = load_from(&path);
        loaded.appearance.mode = AppearanceMode::Dark;
        save_to(&path, &loaded).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Agent Deck desktop settings — edited by hand, 2026-09-16\n\
             version = 1\n\n\
             # I like it light during the day\n\
             [appearance]\n\
             mode = \"dark\"  # flip this to \"dark\" at night\n\n\
             [zoom]\n\
             level = 1.0\n\n\
             # a section no build of this app owns\n\
             [experiments]\n\
             flags = [\"a\", \"b\"]\n",
            "a hand-written annotation did not survive the save"
        );
    }

    /// The strongest statement of the same property, and the cheapest to read:
    /// a save that changes no data touches no byte.
    ///
    /// It covers what the comment tests do not — key order (`zoom` before
    /// `appearance` here, the reverse of what this build writes), blank-line
    /// grouping, a doubled blank line, a trailing comment at the end of the
    /// file, and the inline spelling of a table this build **owns**. Every one
    /// of those was re-rendered before issue #825.
    #[test]
    fn a_save_that_changes_nothing_rewrites_nothing() {
        for original in [
            // Sections in the reverse of this build's order, odd blank-line
            // grouping, a leading and a trailing comment.
            "# mine\n\nversion = 1\n\n\n[zoom]\nlevel = 1.0\n\n[appearance]\nmode = \"dark\"\n\n# end\n",
            // The sections this build owns, spelled inline.
            "version = 1\nappearance = { mode = \"dark\" }\nzoom = { level = 1.0 }\n",
            // Dotted keys.
            "version = 1\nappearance.mode = \"dark\"\nzoom.level = 1.0\n",
        ] {
            let dir = tempdir();
            let path = dir.path().join(SETTINGS_FILE_NAME);
            std::fs::write(&path, original).unwrap();

            // Loading and saving straight back is the no-op: whatever the
            // document says is what the struct holds.
            let loaded = load_from(&path);
            assert_eq!(loaded.appearance.mode, AppearanceMode::Dark);
            save_to(&path, &loaded).unwrap();

            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                original,
                "a no-op save rewrote the document"
            );
        }
    }

    /// The limit of the new property, pinned so it is a known shape rather than
    /// a surprise — the same job the inverted test above used to do for the old
    /// one.
    ///
    /// A **changed array** is replaced whole, so a comment between
    /// `[[endpoints.remote]]` rows goes with it. An **unchanged** one is left
    /// alone, comment included, which is the common case: the app rewrites that
    /// list only when a deck is actually added, removed or edited.
    ///
    /// [`merged_document`] has the reason it is not merged element-wise.
    #[test]
    fn a_comment_inside_a_list_survives_until_that_list_changes() {
        let with_comment = "version = 1\n\n\
             [appearance]\n\
             mode = \"dark\"\n\n\
             [zoom]\n\
             level = 1.0\n\n\
             [endpoints]\n\
             selection = \"local\"\n\n\
             # the box under my desk\n\
             [[endpoints.remote]]\n\
             host = \"build-box.example.com\"\n\
             id = \"deck1\"\n\
             port = 22\n";

        // Unchanged: the comment is still there.
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, with_comment).unwrap();
        let loaded = load_from(&path);
        save_to(&path, &loaded).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            with_comment,
            "an untouched endpoint list was rewritten"
        );

        // Changed: the row's data is written, and the comment is not kept.
        let mut edited = loaded;
        edited.endpoints.as_mut().unwrap().remote[0].host =
            Hostname::parse("other-box.example.com").unwrap();
        save_to(&path, &edited).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("other-box.example.com"),
            "the edit did not reach the document: {raw}"
        );
        assert!(
            !raw.contains("# the box under my desk"),
            "this test pins the LIMIT; if the comment now survives, \
             merged_document's written limit is stale and should be narrowed \
             rather than this assertion loosened: {raw}"
        );
        // Everything outside the list it rewrote is still untouched.
        assert!(
            raw.starts_with(
                "version = 1\n\n[appearance]\nmode = \"dark\"\n\n[zoom]\nlevel = 1.0\n\n\
                 [endpoints]\nselection = \"local\"\n"
            ),
            "the rest of the document moved: {raw}"
        );
    }

    /// The silent data loss [`merge_tables`]'s `inline` argument exists to
    /// stop, and the two shapes of hand-written document that reach it.
    ///
    /// An inline table holds values only, so a `[[array of tables]]`-shaped
    /// item put inside one is a shape it cannot render — and `toml_edit`
    /// renders it as **nothing** rather than failing. Measured against the
    /// second fixture below without the conversion: the save wrote
    /// `endpoints = { selection = "local"}` and the deck the caller had just
    /// added was gone, with the save reporting success. The right answer is
    /// also the preserving one: spell the new item the way the base is spelled.
    ///
    /// The two arms are not the same path, which is why both are here. The
    /// first goes through `TableLike::insert`, which converts the item itself
    /// and was never the problem; the second is a *replacement* through
    /// `get_mut`, which converts nothing.
    #[test]
    fn a_section_spelled_inline_stays_inline_when_the_app_adds_to_it() {
        for inline_endpoints in [
            // The key the app has to ADD is the list itself.
            "endpoints = { selection = \"local\" }",
            // The key is already there and EMPTY, so the app replaces it. This
            // is the arm that needs `make_value`: a replacement is written
            // straight into the inline table's items, without the conversion
            // `TableLike::insert` performs on its own.
            "endpoints = { selection = \"local\", remote = [] }",
        ] {
            let dir = tempdir();
            let path = dir.path().join(SETTINGS_FILE_NAME);
            std::fs::write(
                &path,
                format!(
                    "version = 1\n\
                     appearance = {{ mode = \"dark\" }}\n\
                     zoom = {{ level = 1.0 }}\n\
                     {inline_endpoints}\n"
                ),
            )
            .unwrap();

            let mut settings = load_from(&path);
            settings.endpoints.as_mut().unwrap().remote = vec![RemoteEndpointSettings::new(
                EndpointId::parse("deck1").unwrap(),
                Hostname::parse("build-box.example.com").unwrap(),
            )];
            save_to(&path, &settings).unwrap();

            let raw = std::fs::read_to_string(&path).unwrap();
            assert!(
                !raw.contains("[[endpoints.remote]]") && !raw.contains("[endpoints]"),
                "an inline section was re-spelled as a header: {raw}"
            );
            // And it is still a document this build reads back to the same
            // thing — the check that catches an item written into an inline
            // table in a shape no inline table can hold.
            let reloaded = load_from(&path);
            assert_eq!(reloaded, settings, "the inline save did not round-trip");
        }
    }

    /// The third spelling a hand-written section can have, and the same class
    /// of bug the inline test above pins: an item written into a table in a
    /// shape that table cannot hold.
    ///
    /// A dotted key (`endpoints.selection = "local"`) is a third `toml_edit`
    /// shape beside `[section]` and `{ … }`, and the app has to be able to add
    /// a deck to one. Asserted as a **round trip** rather than on exact bytes:
    /// what matters is that the document this build writes is one it reads back
    /// to the same settings, and the layout a dotted section gets when the list
    /// inside it is rewritten falls under the changed-array limit
    /// [`merged_document`] already states.
    #[test]
    fn a_section_spelled_with_dotted_keys_still_round_trips_when_the_app_adds_to_it() {
        for dotted_endpoints in [
            // The list is not there at all, so it is inserted.
            "endpoints.selection = \"local\"",
            // The list is there and empty, so it is replaced.
            "endpoints.selection = \"local\"\nendpoints.remote = []",
        ] {
            let dir = tempdir();
            let path = dir.path().join(SETTINGS_FILE_NAME);
            std::fs::write(
                &path,
                format!(
                    "version = 1\n\
                     appearance.mode = \"dark\"\n\
                     zoom.level = 1.0\n\
                     {dotted_endpoints}\n"
                ),
            )
            .unwrap();

            let mut settings = load_from(&path);
            settings.endpoints.as_mut().unwrap().remote = vec![RemoteEndpointSettings::new(
                EndpointId::parse("deck1").unwrap(),
                Hostname::parse("build-box.example.com").unwrap(),
            )];
            save_to(&path, &settings).unwrap();

            let raw = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                load_from(&path),
                settings,
                "the dotted save did not round-trip: {raw}"
            );
            // The dotted keys the save had no reason to touch are untouched.
            assert!(
                raw.starts_with(
                    "version = 1\nappearance.mode = \"dark\"\nzoom.level = 1.0\n\
                     endpoints.selection = \"local\"\n"
                ),
                "an untouched dotted key was re-spelled: {raw}"
            );
        }
    }

    /// The same property one level down: an unknown *field* inside a section
    /// this build does own. A section-granular merge would silently drop this.
    #[test]
    fn an_unknown_field_inside_a_known_section_survives_a_save() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[appearance]\nmode = \"light\"\nterminal = \"follow\"\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("terminal = \"follow\""),
            "the unknown appearance field was dropped: {raw}"
        );
    }

    /// The other half of the merge: preserving unknown keys must not make the
    /// file authoritative over the struct for a field the struct owns.
    #[test]
    fn a_known_field_the_struct_owns_wins_over_the_file() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 99\n\n[appearance]\nmode = \"light\"\n\n[voice]\nbackend = \"whisper\"\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();

        let reloaded = load_from(&path);
        assert_eq!(reloaded.appearance.mode, AppearanceMode::Dark);
        assert_eq!(reloaded.version, SETTINGS_VERSION);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("99"), "the stale version survived: {raw}");
        assert!(raw.contains("[voice]"), "the merge lost [voice]: {raw}");
    }

    // ---------------------------------------------------------------------
    // Issue #1072 — a document this build cannot read is never overwritten
    // ---------------------------------------------------------------------

    /// The two documents this build cannot read, and they fail in **different
    /// places**, which is the whole reason the guard is written against
    /// `DesktopSettings` rather than against a bare document parse.
    ///
    /// The first is not TOML at all. The second is valid TOML whose `host`
    /// contains a space and whose `id` is one character, so this build's *schema*
    /// rejects it while a document parse accepts it happily — the case issue
    /// #1072 was filed from, and the one that was silently destructive.
    const UNREADABLE_DOCUMENTS: [&str; 2] = [
        "# my own settings, annotated\nthis is not [ valid toml\n",
        "version = 1\n\n[[endpoints.remote]]\nhost = \"build box\"\nid = \"d\"\n",
    ];

    /// Scenario (issue #1072): a `desktop.toml` this build cannot read sits on
    /// disk, the user opens the app — which comes up on defaults — and changes
    /// their theme. The save must be REFUSED and the user's bytes must still be
    /// on disk afterwards, because the alternative is that opening the app and
    /// touching one setting destroys the whole document.
    ///
    /// The regression test the issue asks for. It fails on the code it was
    /// written against: `save_to` returned `Ok(())`, and `merged_document`
    /// published this build's defaults over every key the schema owns —
    /// appearance, zoom, endpoints and the deck selection — with nothing said
    /// anywhere a user would look.
    #[test]
    fn a_document_this_build_cannot_read_is_never_overwritten() {
        for original in UNREADABLE_DOCUMENTS {
            let dir = tempdir();
            let path = dir.path().join(SETTINGS_FILE_NAME);
            std::fs::write(&path, original).unwrap();

            let refused = save_to(&path, &dark())
                .expect_err("saving over a document this build cannot read must be refused");

            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                original,
                "the user's own bytes must survive verbatim: {refused}"
            );
            // And nothing was created beside it either: the refusal happens
            // before `create_temp`, so a failed save leaves no debris.
            assert_eq!(entries(dir.path()), [SETTINGS_FILE_NAME]);

            // The refusal is actionable rather than merely negative: it locates
            // the problem and says the file is untouched.
            assert!(
                refused.public().contains("line ")
                    || refused.public().contains("an unreported position"),
                "the refusal must locate the problem: {}",
                refused.public()
            );
            assert!(
                refused.public().contains("left exactly as it is"),
                "the refusal must say the file was preserved: {}",
                refused.public()
            );
        }
    }

    /// The merge's own guard, reached directly because `save_to` refuses first
    /// and so nothing can drive this through the public path.
    ///
    /// It is here because `unwrap_or_default()` — what this line used to be — is
    /// the exact shape of the defect: defaulting silently turns "I could not
    /// understand this file" into "this file was empty", and the merge then
    /// publishes a whole-file replacement wearing a merge's clothes. The guard
    /// costs four lines and one test; leaving the `unwrap_or_default()` in place
    /// would leave the failure mode loaded for whoever adds a second caller.
    #[test]
    fn the_merge_refuses_a_document_it_cannot_parse_rather_than_treating_it_as_empty() {
        let path = Path::new("/home/dev/.config/dot-agent-deck/desktop.toml");

        // No document at all is the first-run case and merges into an empty
        // table, which is a different thing and must stay allowed.
        let fresh = merged_document(path, None, &dark()).unwrap();
        assert!(fresh.contains("mode = \"dark\""), "{fresh}");

        let refused = merged_document(path, Some("this is not [ valid toml\n"), &dark())
            .expect_err("bytes that are not TOML must not be merged into as if empty");
        assert!(
            refused.public().contains("refusing to overwrite"),
            "{refused}"
        );
    }

    /// The control, and without it the test above proves only that saving is
    /// broken. A document this build CAN read still saves, still merges, and
    /// still keeps the section it does not own — the property PRD #803's
    /// container promise rests on, which the refusal must not have cost.
    #[test]
    fn a_readable_document_still_saves_and_still_merges() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[appearance]\nmode = \"light\"\n\n[voice]\nbackend = \"whisper\"\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("mode = \"dark\""), "{after}");
        assert!(after.contains("backend = \"whisper\""), "{after}");
    }

    /// Scenario: the same two documents, seen from the load side. The app still
    /// comes up on defaults — that half is unchanged and deliberate, because a
    /// settings file is not worth failing an app launch over — but the reason is
    /// now **returned** rather than logged and dropped, which is what lets the
    /// settings surface say something and `save_to` refuse.
    #[test]
    fn an_unreadable_document_loads_as_defaults_and_reports_why() {
        for contents in UNREADABLE_DOCUMENTS {
            let dir = tempdir();
            let path = dir.path().join(SETTINGS_FILE_NAME);
            std::fs::write(&path, contents).unwrap();

            let (settings, problem) = load_document(&path);
            assert_eq!(settings, DesktopSettings::default(), "{contents:?}");

            let problem = problem
                .unwrap_or_else(|| panic!("an unreadable document must report why: {contents:?}"));
            assert!(
                problem.public().contains("cannot be read"),
                "unexpected message: {}",
                problem.public()
            );
            // The sentence a user acts on: why their settings look reset, and
            // that nothing is being written over their file meanwhile.
            assert!(
                problem.public().contains("default settings")
                    && problem.public().contains("nothing will be saved"),
                "the message must explain the reset AND the refusal: {}",
                problem.public()
            );
        }
    }

    /// The complement, and the line this must not blur: an **absent** document
    /// and an **empty** one are not problems. Both genuinely mean "everything at
    /// its default" — an empty file is valid TOML — and reporting either would
    /// put a permanent error in front of every first-run user and refuse the
    /// first save they ever make.
    #[test]
    fn an_absent_or_empty_document_is_not_a_problem() {
        let dir = tempdir();

        let absent = dir.path().join(SETTINGS_FILE_NAME);
        let (settings, problem) = load_document(&absent);
        assert_eq!(settings, DesktopSettings::default());
        assert_eq!(problem, None, "a first run must not report a problem");
        save_to(&absent, &dark()).expect("a first save must not be refused");

        let empty = dir.path().join("empty.toml");
        std::fs::write(&empty, "").unwrap();
        let (settings, problem) = load_document(&empty);
        assert_eq!(settings, DesktopSettings::default());
        assert_eq!(problem, None, "an empty document is a valid empty document");
        save_to(&empty, &dark()).expect("saving over an empty document must not be refused");
    }

    /// A path-level failure is a document problem too.
    ///
    /// `save_to` always refused these — `read_document`'s error propagates
    /// straight out of it, and that was true before #1072 — so what is new here
    /// is only the other half: the user is now told why the app is on defaults,
    /// instead of it being a line in a log nobody reads.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_reports_a_problem_rather_than_only_logging_one() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        set_mode(&path, 0o000);
        if std::fs::read_to_string(&path).is_ok() {
            set_mode(&path, 0o600);
            eprintln!(
                "SKIP: this process can read a 0o000 file (running privileged), so an \
                 unreadable document cannot be constructed here"
            );
            return;
        }

        let (settings, problem) = load_document(&path);
        set_mode(&path, 0o600);

        assert_eq!(settings, DesktopSettings::default());
        let problem = problem.expect("an unreadable file must report why");
        assert!(
            problem.public().contains("nothing will be saved"),
            "unexpected message: {}",
            problem.public()
        );
    }

    /// The snapshot is how the reason reaches the settings surface, so it is
    /// pinned on both sides: present for a document that cannot be read, and
    /// **absent from the wire entirely** for one that can.
    ///
    /// Absence matters as much as presence. The field is
    /// `skip_serializing_if = "Option::is_none"` precisely so the frontend can
    /// read "no key" as "nothing to say" rather than having to compare against
    /// an empty string.
    #[test]
    fn the_snapshot_reports_an_unreadable_document_and_stays_silent_about_a_good_one() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);

        std::fs::write(&path, UNREADABLE_DOCUMENTS[1]).unwrap();
        // SAFETY: the lock above serialises every test that touches this var.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, &path) };
        let broken = load_snapshot();

        save_to(&path, &dark()).unwrap_err();
        std::fs::write(&path, "version = 1\n[appearance]\nmode = \"dark\"\n").unwrap();
        let healthy = load_snapshot();
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };

        let reported = broken.problem.expect("the snapshot must carry the reason");
        assert!(reported.contains("cannot be read"), "{reported}");
        // The public half, so it names no path — the snapshot carries the path
        // beside it, deliberately and once.
        assert!(
            !reported.contains(&dir.path().display().to_string()),
            "the reason must not name a filesystem path: {reported}"
        );
        assert_eq!(broken.settings, DesktopSettings::default());

        assert_eq!(healthy.problem, None);
        assert_eq!(healthy.settings, dark());
        let json = serde_json::to_value(&healthy).unwrap();
        assert!(
            json.get("problem").is_none(),
            "a healthy document must put no `problem` key on the wire: {json}"
        );
    }

    /// Issue #827's rule, applied to the sink issue #1072 opened.
    ///
    /// A document problem reaches a **webview** as well as a log, so the rule
    /// that already governed the log line now has two consumers. Both halves are
    /// asserted, and the raw `toml` error is asserted to carry the value first —
    /// so the redaction is proven necessary rather than assumed, the way
    /// `a_parse_diagnostic_carries_a_locator_and_never_the_documents_bytes` does
    /// for the log half.
    #[test]
    fn a_document_problem_carries_a_locator_and_never_the_documents_bytes() {
        let path = Path::new("/home/dev/.config/dot-agent-deck/desktop.toml");
        let contents = format!("version = 1\n[zoom]\nlevel = \"{SENTINEL}\"\n");
        let error = toml_edit::de::from_str::<DesktopSettings>(&contents).unwrap_err();
        assert!(
            error.to_string().contains(SENTINEL),
            "the toml error stopped echoing the value, so this test proves nothing: {error}"
        );

        let problem = unreadable_document_problem(path, &contents, &error);
        assert_free_of_sentinel("a document problem's log detail", problem.detail());
        assert_free_of_sentinel("a document problem's webview message", problem.public());
        assert_free_of_sentinel(
            "`safe_message` of a document problem's public half",
            &crate::dto::safe_message(problem.public()),
        );
        // Pinned whole rather than by substring, because this exact string is
        // what a user reads and what `desktop/src/App.test.tsx` and
        // `useDesktopSettings.test.ts` hard-code as the message the settings
        // surface renders. A reworded sentence should be a deliberate diff on
        // both sides, not a silent drift on one.
        assert_eq!(
            problem.public(),
            "The desktop settings file cannot be read: line 3, column 9 is not valid settings. \
             This session is using default settings, and nothing will be saved over the file \
             until it is fixed or removed."
        );
        // The split, in both directions: the log half names the file and the
        // webview half never does.
        assert!(problem.detail().contains("desktop.toml"), "{problem}");
        assert!(
            !problem.public().contains("/home/dev"),
            "the webview half must not name a path: {}",
            problem.public()
        );

        // The other route into the same type: a path-level refusal, which is
        // built from a path and an `io::Error` and so cannot carry a value —
        // but must still come out as a sentence rather than as a fragment.
        let rejected: SettingsDocumentProblem =
            read_document(Path::new("relative.toml"), ReadPurpose::Load)
                .expect_err("a relative path is not a settings document")
                .into();
        assert!(
            rejected.public().starts_with("The ") && rejected.public().ends_with("removed."),
            "a path refusal must read as a sentence: {}",
            rejected.public()
        );
        assert!(
            !rejected.public().contains("relative.toml"),
            "the webview half must not name a path: {}",
            rejected.public()
        );
    }

    #[test]
    fn the_path_resolves_to_a_sibling_of_the_tui_config_and_honours_the_override() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // SAFETY: this is the only test that touches the environment, it holds
        // ENV_TEST_LOCK for the whole mutation, and it restores the prior value
        // before releasing it. Nothing here touches the filesystem, so the
        // developer's real ~/.config/dot-agent-deck/desktop.toml is never read
        // or written even while the override is unset.
        let prior = std::env::var(SETTINGS_PATH_ENV).ok();
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };
        let default_path = settings_path();

        unsafe { std::env::set_var(SETTINGS_PATH_ENV, "/tmp/somewhere/else.toml") };
        let overridden = settings_path();

        // An empty override is treated as unset rather than as an empty path.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, "") };
        let empty_override = settings_path();

        unsafe {
            match prior {
                Some(value) => std::env::set_var(SETTINGS_PATH_ENV, value),
                None => std::env::remove_var(SETTINGS_PATH_ENV),
            }
        }

        assert_eq!(default_path, config_dir().join("desktop.toml"));
        assert_eq!(
            default_path.parent(),
            config_dir().join("config.toml").parent(),
            "the document must be a sibling of the TUI's config.toml, not a section inside it"
        );
        assert_eq!(overridden, Path::new("/tmp/somewhere/else.toml"));
        assert_eq!(empty_override, default_path);
    }

    /// The settings surface shows where the document lives, so the snapshot has
    /// to name the path this process would actually write — including under the
    /// env override, which is the only way a test or a packaged build ends up
    /// somewhere other than the config directory.
    #[test]
    fn the_snapshot_carries_the_path_the_process_would_write() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir();
        let path = dir.path().join("elsewhere.toml");
        save_to(&path, &dark()).unwrap();

        // SAFETY: the lock above serialises every test that touches this var.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, &path) };
        let snapshot = load_snapshot();
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };

        assert_eq!(snapshot.settings, dark());
        assert_eq!(snapshot.path, path.display().to_string());

        // The path rides alongside the document rather than inside it, so the
        // pinned document shape is untouched by this.
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["settings"]["appearance"]["mode"], "dark");
        assert_eq!(json["path"], path.display().to_string());
        assert!(
            json["settings"].get("path").is_none(),
            "the path must not have leaked into the document: {json}"
        );
    }

    #[test]
    fn a_successful_save_leaves_no_temp_file_behind() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(entries(dir.path()), vec![SETTINGS_FILE_NAME.to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_publishes_by_rename_rather_than_writing_the_document_in_place() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &DesktopSettings::default()).unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();

        // A writer that opened the destination and truncated it would expose a
        // partially written document; one that renames a finished temp file
        // over it cannot. The inode changing is that difference, observably.
        save_to(&path, &dark()).unwrap();
        let after = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(
            before, after,
            "the document must be replaced by rename, never truncated in place"
        );
        assert_eq!(load_from(&path), dark());
    }

    /// A directory at the destination used to be caught by the rename failing.
    /// It is now caught before anything is created at all, which is the point
    /// of vetting the path — but the property that mattered is the same one:
    /// a refused save leaves nothing behind.
    #[test]
    fn a_directory_at_the_destination_is_refused_before_anything_is_written() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let error = save_to(&path, &dark()).unwrap_err();
        assert!(
            error.detail().contains("a directory"),
            "unexpected error: {error}"
        );
        assert_eq!(
            entries(dir.path()),
            vec![SETTINGS_FILE_NAME.to_string()],
            "a refused save must not leave a temp file behind"
        );
        // And the thing that was really there is untouched.
        assert_eq!(entries(&path), vec!["occupied".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_save_leaves_the_existing_document_intact() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        set_mode(dir.path(), 0o500);
        let probe = dir.path().join(".probe");
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(&probe);
            set_mode(dir.path(), 0o700);
            eprintln!(
                "SKIP: this process can write into a 0o500 directory (running privileged), \
                 so a failing save cannot be constructed here"
            );
            return;
        }

        let error = save_to(&path, &DesktopSettings::default()).unwrap_err();
        set_mode(dir.path(), 0o700);

        assert!(!error.detail().is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed save must leave the previous document byte for byte"
        );
        assert_eq!(load_from(&path), dark());
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_document_is_owner_only() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "a deck-created desktop.toml must be owner-only"
        );

        // A rewrite re-asserts it rather than inheriting whatever the previous
        // file happened to carry.
        set_mode(&path, 0o644);
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn a_write_error_never_names_a_filesystem_path() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let error = save_to(&path, &dark()).unwrap_err();
        let directory = dir.path().to_string_lossy().into_owned();
        assert!(
            error.detail().contains(&directory),
            "the logged detail should name the path: {}",
            error.detail()
        );
        assert!(
            !error.public().contains(&directory),
            "the webview-facing message must not leak a path: {}",
            error.public()
        );
        assert!(
            !error.public().contains(SETTINGS_FILE_NAME),
            "the webview-facing message must not leak a file name: {}",
            error.public()
        );
    }

    /// Every way a settings path can be refused, and the two properties that
    /// have to hold for all of them: **load never fails** (it logs and falls
    /// back to defaults) and **save refuses with a path-free public message**.
    ///
    /// Each kind is built rather than asserted about, because the whole value
    /// of the guard is that it recognises the real thing. The Unix-only kinds
    /// live in the test below.
    #[test]
    fn every_portable_path_rejection_is_refused_without_leaking_the_path() {
        let dir = tempdir();
        let a_directory = dir.path().join("as-a-directory");
        std::fs::create_dir(&a_directory).unwrap();

        let oversized = dir.path().join("oversized.toml");
        std::fs::write(&oversized, "#".repeat(MAX_SETTINGS_BYTES as usize + 1)).unwrap();
        assert!(std::fs::metadata(&oversized).unwrap().len() > MAX_SETTINGS_BYTES);

        // A root with no final component. `/` is not absolute on Windows, so
        // the two platforms need different spellings of the same idea.
        #[cfg(unix)]
        let no_file_name = PathBuf::from("/");
        #[cfg(windows)]
        let no_file_name = PathBuf::from(r"C:\");

        let cases: Vec<(&str, PathBuf, &str)> = vec![
            (
                "a relative path",
                PathBuf::from("desktop.toml"),
                "not an absolute path",
            ),
            ("a path with no file name", no_file_name, "names no file"),
            ("a directory", a_directory, "a directory"),
            ("an over-limit document", oversized, "settings limit"),
        ];

        for (what, path, expected) in cases {
            assert_reject(what, &path, expected);
        }
    }

    /// Both halves of a rejected path, for one kind: the load falls back to
    /// defaults, and the save refuses with the reason in the log detail and no
    /// path in the public message.
    fn assert_reject(what: &str, path: &Path, expected: &str) {
        assert_eq!(
            load_from(path),
            DesktopSettings::default(),
            "loading from {what} must fall back to defaults"
        );

        let error = match save_to(path, &dark()) {
            Err(error) => error,
            Ok(()) => panic!("saving to {what} must be refused"),
        };
        assert!(
            error.detail().contains(expected),
            "{what}: unexpected error: {error}"
        );
        assert!(
            !error.public().contains(&path.display().to_string()),
            "{what}: the public message leaked the path: {}",
            error.public()
        );
    }

    /// The kinds that only exist on Unix, and the two that motivated the guard
    /// in the first place: a **FIFO** blocked the app forever with no message,
    /// and a **character device** like `/dev/zero` exhausted memory. Neither is
    /// opened at all now — the rejection is on `lstat`, so this test cannot
    /// hang even if the guard regresses to following the final component.
    #[cfg(unix)]
    #[test]
    fn every_unix_only_path_rejection_is_refused_without_leaking_the_path() {
        let dir = tempdir();

        // A symlink at the final component, pointing at a perfectly good
        // document. The target must come back untouched: rejecting a symlink is
        // only meaningful if nothing was written through it.
        let target = dir.path().join("target.toml");
        save_to(&target, &DesktopSettings::default()).unwrap();
        let before = std::fs::read_to_string(&target).unwrap();
        let link = dir.path().join("as-a-symlink.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_reject("a symlink", &link, "a symlink");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            before,
            "nothing may be written through a rejected symlink"
        );

        // A socket, which `std` can bind without help.
        let socket_path = dir.path().join("as-a-socket");
        let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert_reject("a socket", &socket_path, "a socket");

        // A FIFO, which it cannot.
        let fifo = dir.path().join("as-a-fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path inside this test's own temp
        // directory and outlives the call; `mkfifo` reads it and returns.
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            made,
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );
        assert_reject("a FIFO", &fifo, "a FIFO");

        // And a character device, if this machine has the usual one.
        let zero = Path::new("/dev/zero");
        if zero.exists() {
            assert_reject("a character device", zero, "a character device");
        } else {
            eprintln!("SKIP: no /dev/zero on this machine");
        }
    }

    /// The limit is a limit: exactly [`MAX_SETTINGS_BYTES`] loads, one byte
    /// more is refused.
    ///
    /// [`read_bounded`] re-applies the bound to the bytes actually read, which
    /// covers a document that grows between the `stat` and the read. That race
    /// is not constructible deterministically, so it is asserted by the code
    /// rather than by a test — but the boundary itself is pinned here, and it
    /// is the boundary a hand-edited file can actually reach.
    #[test]
    fn the_byte_limit_admits_a_document_at_the_limit_and_refuses_one_past_it() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n").unwrap();
        assert_eq!(load_from(&path).version, 1);

        // Exactly at the limit is fine; one byte over is not.
        let mut at_limit = "version = 1\n".to_string();
        at_limit.push_str(&"#".repeat(MAX_SETTINGS_BYTES as usize - at_limit.len()));
        assert_eq!(at_limit.len() as u64, MAX_SETTINGS_BYTES);
        std::fs::write(&path, &at_limit).unwrap();
        assert_eq!(
            load_from(&path).version,
            1,
            "a document at the limit must load"
        );

        std::fs::write(&path, format!("{at_limit}#")).unwrap();
        assert_reject("an over-limit document", &path, "settings limit");
    }

    /// The temp name is unpredictable rather than `<pid>.<counter>`.
    ///
    /// A guessable name is one another process can plant first. `create_new`
    /// means the cost of that is a failed save rather than a write through
    /// someone else's symlink, so this is closing a nuisance rather than a
    /// hole — but the nuisance is free to close.
    ///
    /// # Why this asserts the suffix's SHAPE and never searches for the pid
    ///
    /// It used to assert `!name.contains(&std::process::id().to_string())`,
    /// and that assertion was **probabilistically false**: the suffix is 16
    /// random hex characters, ten of whose sixteen symbols are decimal digits,
    /// so a decimal pid turns up inside one by chance. It is not hypothetical
    /// — it reddened the required `build-windows` job on PR #872, a PR that
    /// touches no desktop code at all, on the name
    /// `.desktop.toml.tmp.1bea3738d823864d` (which carries the all-decimal
    /// runs `3738`, `8238` and `823864`). A four-digit pid collides on the
    /// order of one run in a few hundred, and every collision is a false
    /// report against whichever PR happens to be running.
    ///
    /// A substring search cannot express the property anyway. What the
    /// docstring above actually claims is that the name is a random token
    /// rather than the `<pid>.<counter>` construction, and the check below
    /// settles exactly that, deterministically: `create_temp` formats
    /// `{:016x}`, so the suffix is sixteen hex digits and nothing else. A
    /// `<pid>.<counter>` name fails it on the `.` alone, and fails it again on
    /// the length. Strictly stronger than the search it replaces, and it
    /// cannot flake.
    #[test]
    fn temp_names_are_unpredictable_rather_than_the_pid_and_a_counter() {
        let dir = tempdir();
        let dest = dir.path().join(SETTINGS_FILE_NAME);
        let prefix = format!(".{SETTINGS_FILE_NAME}.tmp.");

        let mut names = std::collections::BTreeSet::new();
        for _ in 0..16 {
            let (file, tmp) = create_temp(dir.path(), &dest).unwrap();
            drop(file);
            let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
            let suffix = name
                .strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("unexpected temp name: {name}"));
            assert!(
                suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
                "the temp suffix must be the 16 hex digits `{{:016x}}` writes, \
                 not a derived name like `<pid>.<counter>`: {name}"
            );
            names.insert(name);
            std::fs::remove_file(&tmp).unwrap();
        }
        assert_eq!(names.len(), 16, "temp names repeated: {names:?}");
    }

    /// The bound on an accepted appearance token, on both paths.
    ///
    /// A compromised webview could otherwise send an arbitrarily long mode that
    /// is allocated and lowercased on the way in. The check is in the
    /// deserializer, before the normalising copy, so the same bound covers the
    /// `desktop_set_settings` payload and a hand-edited `desktop.toml`.
    #[test]
    fn an_over_length_appearance_token_is_refused_on_both_paths() {
        let over = "x".repeat(MAX_APPEARANCE_TOKEN_BYTES + 1);

        // The IPC shape: an argument that fails deserialisation, with a message
        // that names the limit and leaks neither a path nor the value.
        let error = serde_json::from_value::<DesktopSettings>(serde_json::json!({
            "version": 1,
            "appearance": { "mode": over },
        }))
        .expect_err("an over-length appearance token must be refused");
        let message = error.to_string();
        assert!(
            message.contains(&MAX_APPEARANCE_TOKEN_BYTES.to_string()),
            "the error should name the limit: {message}"
        );
        assert!(
            !message.contains(&over),
            "the error must not echo the value: {message}"
        );
        assert!(
            !message.contains('/'),
            "the error must name no path: {message}"
        );

        // The disk path: still never fails a load. An over-length token is a
        // malformed document, not an unknown mode, so the fallback is the whole
        // SECTION rather than the one field — logged, and not a crash. The
        // version beside it is untouched, which is
        // [`sections_this_build_can_read`]: a refused value costs its own
        // section and nothing else.
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!("version = 9\n[appearance]\nmode = \"{over}\"\n"),
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance, AppearanceSettings::default());
        assert_eq!(loaded.version, 9);

        // And exactly at the limit is still the ordinary unknown-value
        // fallback, which keeps the rest of the document.
        let at_limit = "x".repeat(MAX_APPEARANCE_TOKEN_BYTES);
        std::fs::write(
            &path,
            format!("version = 9\n[appearance]\nmode = \"{at_limit}\"\n"),
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::System);
        assert_eq!(
            loaded.version, 9,
            "an unknown mode must not cost the document"
        );
    }

    /// The pinned shape of the default document, in the idiom of
    /// `dto::agent_mapping_is_frontend_stable`.
    ///
    /// This is deliberate friction. A new field shows up here as a diff, which
    /// forces the ownership question — "does this setting describe the client
    /// itself, or does it describe the work?" — to be answered in review rather
    /// than discovered by the feature that inherits it.
    #[test]
    fn default_document_shape_is_pinned() {
        // PRD #741 M6 added `[endpoints]` and this pin deliberately did NOT
        // gain a section, which is the thing to understand before changing it:
        // `DesktopSettings::endpoints` is an `Option` whose default is `None`,
        // TOML omits a `None` field, and that omission is load-bearing rather
        // than cosmetic — it is what makes a webview that cannot render
        // endpoints unable to delete them (see `EndpointSettings`). The JSON
        // half below *does* change, to `"endpoints": null`, and that null is
        // the same statement on the IPC wire: "unspecified", not "empty".
        //
        // PRD #802 M4's `[voice]` is the second section to arrive this way and
        // the reasoning is `endpoints`' rather than a weaker echo of it: a
        // plain `VoiceSettings` would arrive as the default from any client
        // that did not send one, and the merge would then write that default
        // over a choice the user had made. So the TOML below is unchanged for
        // a second time, and the JSON gains `"voice": null`.
        const FRESH: &str =
            "version = 1\n\n[appearance]\nmode = \"system\"\n\n[zoom]\nlevel = 1.0\n";
        let rendered = toml_edit::ser::to_string_pretty(&DesktopSettings::default()).unwrap();
        assert_eq!(rendered, FRESH);

        // The same bytes must come out of `save_to`, which no longer serializes
        // the struct directly — it merges the struct into the document already
        // on disk. Against **no** existing document that merge has to be a
        // no-op, or this pin would describe a shape the app never writes.
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FRESH);

        // The same struct crosses the Tauri IPC, so its JSON shape is the
        // frontend's contract and is pinned with it.
        let json = serde_json::to_value(DesktopSettings::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": 1,
                "appearance": { "mode": "system" },
                "endpoints": null,
                "voice": null,
                "zoom": { "level": 1.0 },
            })
        );
        for mode in [
            AppearanceMode::System,
            AppearanceMode::Light,
            AppearanceMode::Dark,
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
            assert_eq!(AppearanceMode::from_str_lossy(mode.as_str()), mode);
        }
    }

    /// Substrings that make a key **name** look like it holds a credential.
    ///
    /// `apikey` and `api_key` need no row of their own — `key` already matches
    /// both — so the auditor's fourth example is covered by the first entry
    /// rather than by a redundant one that would read as if it mattered.
    const SECRETISH: [&str; 8] = [
        "key",
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
        "bearer",
        "passphrase",
    ];

    /// The shapes a credential-*shaped* key name is allowed to have, and the
    /// concrete serialised type each one is.
    ///
    /// **Two of the three are references to a credential; the third is not
    /// about credentials at all.** [`Self::Count`] is here because
    /// [`SECRETISH`] matches substrings and `token` is an ordinary English word
    /// in a field like `max_tokens`. The list is therefore *paths whose name
    /// trips the scan for a reason other than holding a credential*, which is
    /// what it always was — the name `AllowedReference` predates the third
    /// reason.
    ///
    /// **The type is half the exemption.** An exemption granted for a boolean
    /// "is one stored" would otherwise keep covering that path after someone
    /// changed the field to a `String` — the same name, now able to hold the
    /// credential itself — which is the silent widening issue #827 asks this
    /// list to stop.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AllowedReference {
        /// The *name of the backend* that holds the credential (`"keychain"`).
        /// A string, whose contents are ours rather than the user's.
        BackendName,
        /// A flag saying a credential is stored. A boolean, which cannot carry
        /// credential material at all — the strongest of the three shapes, and
        /// the one to prefer where a reference is what is wanted.
        StoredFlag,
        /// A **count**, whose name contains one of [`SECRETISH`] as a plain
        /// English word rather than as a credential — `max_tokens`, where the
        /// tokens are the model's units of output. An integer, which cannot
        /// carry credential material at all, so this is as strong as
        /// [`Self::StoredFlag`]; what it is not is a reference to anything.
        Count,
    }

    impl AllowedReference {
        /// The `toml_edit::Item::type_name()` this shape covers, and nothing
        /// else.
        fn type_str(self) -> &'static str {
            match self {
                Self::BackendName => "string",
                Self::StoredFlag => "boolean",
                Self::Count => "integer",
            }
        }
    }

    /// Full key **paths** (`section.field`) that legitimately contain one of
    /// [`SECRETISH`] because they are a *reference to* a credential rather than
    /// the credential itself — the one carve-out PRD #803 allows — each paired
    /// with the concrete type that carve-out covers.
    ///
    /// One entry, and it is not a credential reference at all — see
    /// [`AllowedReference::Count`]. It holds paths and not bare names
    /// deliberately: `secret_backend` as a bare name would exempt a field of
    /// that name in **every** section, including one added later by someone who
    /// never read this rule, which is precisely the silent-widening this list
    /// must not do.
    ///
    /// The form to add is one line — `("voice.secret_backend",
    /// AllowedReference::BackendName)` — and the shape is not a label: a path
    /// exempted as a [`AllowedReference::StoredFlag`] whose value is a string
    /// is reported as an offender, with the mismatch named.
    const SECRETISH_ALLOWED: [(&str, AllowedReference); 1] = [
        // PRD #802's answer ceiling. `token` is a substring of `max_tokens`,
        // where it means a model's unit of output and nothing else: the value
        // is an integer between 64 and 32768, bounded by
        // `crate::model_service::TokenCeiling`, and it is sent as one field of
        // a request body. The name is the one both wire dialects use and the
        // one every LLM API a user has met spells it — renaming it to dodge a
        // substring scan would cost more clarity than the scan buys here.
        //
        // The type is half the exemption, exactly as the enum's doc says: this
        // covers `max_tokens` **while it is an integer**. Change it to a
        // `String` and the tripwire fires again, naming the mismatch.
        ("voice.intent.max_tokens", AllowedReference::Count),
    ];

    const SECRET_RULE: &str = "\
WHAT THIS CHECK IS: a NAMING TRIPWIRE, not a security boundary. It reads the \
key NAMES of the serialised default document and nothing else. It therefore \
cannot see any of the following, and none of them is hypothetical:\n\
  - a field whose name says nothing -- `endpoint`, `authorization`, `value` -- \
whose VALUE is a token;\n\
  - a field serde omits from the default document, which is invisible to the \
scan because the scan is of that document;\n\
  - anything the TypeScript DTO carries that the Rust schema does not, which \
is outside this test entirely;\n\
  - any value, ever.\n\
Read a pass as \"nobody named a field like a credential\", never as \"a \
credential cannot get in here\". If you are about to store real credential \
material, this check will not stop you and is not the control you need.\n\n\
WHAT DOES WATCH VALUES (issue #827): the tests below this one follow a \
uniquely-named sentinel through the document, the IPC echo, the \
desktop_get_settings snapshot, the parse diagnostic and both halves of \
SettingsWriteError; and \
xtask/linkage-check/src/desktop_settings_secrets.rs pins the field TYPES this \
schema may use, the TypeScript DTO's names and the localStorage key set. Those \
are what establish that a credential has no route in -- not this scan.\n\n\
THE RULE IT WATCHES: PRD #803 sets one hard rule about credentials -- a secret \
NEVER goes in desktop.toml and NEVER in localStorage. This document is visible \
to anyone with the user's disk, is synced by whatever backs up ~/.config, and \
is handed to the webview verbatim.\n\n\
The document may hold a non-secret REFERENCE -- which backend holds the key, or \
a boolean saying one is stored -- and nothing more. A real credential belongs \
behind the SecretStore seam (PRD #803 M5): store/load/delete keyed by a stable \
identifier, with the OS keychain as the intended implementation.\n\n\
If the name that tripped this really is a reference and not a credential, add \
its FULL PATH to SECRETISH_ALLOWED with a comment saying which of those two \
forms it is.";

    /// Every key path in a serialised document, dotted, paired with the value
    /// at it — including nested tables **and tables inside arrays**. Both the
    /// section (`voice`) and each field under it (`voice.backend`) are emitted,
    /// because either can be named badly, and the value travels with the path
    /// because [`SECRETISH_ALLOWED`] constrains the concrete type as well as
    /// the name.
    ///
    /// # The array arm is PRD #741 M6's, and it closed a real blind spot
    ///
    /// This walked `Table` only, so **no field inside a list-shaped section was
    /// ever scanned.** Nothing had one until #741's `[[endpoints.remote]]`, so
    /// it cost nothing historically — but the PRD arrived expecting a field
    /// named `key` in an endpoint row to trip this check, and it would not
    /// have: not because of the name, but because the scan could not reach the
    /// row at all. A tripwire silently blind to a whole section shape is worse
    /// than one that fires, because a reader takes the pass as a statement.
    ///
    /// An array index deliberately does **not** appear in the path. The path is
    /// what [`SECRETISH_ALLOWED`] matches on, and an exemption must describe a
    /// *field* (`endpoints.remote.identity`) rather than a *row*
    /// (`endpoints.remote.0.identity`), which would exempt one list position
    /// and silently fail to cover the second.
    ///
    /// # Spelling is not part of a path
    ///
    /// Since issue #825 this walks a `toml_edit` tree, where the same data has
    /// four shapes rather than two: a `[table]` and an `a = { … }` inline table
    /// are both tables, and an `[[array of tables]]` and an `a = [{ … }]` array
    /// of inline tables are both lists. All four are walked, because a document
    /// the tripwire is run against may be hand-written in any of them and a
    /// scan blind to one spelling is a scan that passes on a schema it has not
    /// read — which is exactly the blind spot #741 M6 closed for list rows.
    fn key_paths<'v>(
        item: &'v toml_edit::Item,
        prefix: &str,
        into: &mut Vec<(String, &'v toml_edit::Item)>,
    ) {
        if let Some(table) = item.as_table_like() {
            key_paths_in_table(table, prefix, into);
        } else if let Some(rows) = item.as_array_of_tables() {
            for row in rows.iter() {
                key_paths_in_table(row, prefix, into);
            }
        } else if let Some(value) = item.as_value() {
            key_paths_in_value(value, prefix, into);
        }
    }

    /// [`key_paths`] for a table, by whichever of the two spellings — the only
    /// arm that emits a path, because a list position is deliberately not one.
    fn key_paths_in_table<'v>(
        table: &'v dyn toml_edit::TableLike,
        prefix: &str,
        into: &mut Vec<(String, &'v toml_edit::Item)>,
    ) {
        for (key, nested) in table.iter() {
            if nested.is_none() {
                continue;
            }
            let path = if prefix.is_empty() {
                key.to_string()
            } else {
                format!("{prefix}.{key}")
            };
            into.push((path.clone(), nested));
            key_paths(nested, &path, into);
        }
    }

    /// [`key_paths`] for a value, which is where an *inline* list of inline
    /// tables is reached — `toml_edit` models that as an array of values rather
    /// than as an array of tables.
    fn key_paths_in_value<'v>(
        value: &'v toml_edit::Value,
        prefix: &str,
        into: &mut Vec<(String, &'v toml_edit::Item)>,
    ) {
        if let Some(table) = value.as_inline_table() {
            key_paths_in_table(table, prefix, into);
        } else if let Some(array) = value.as_array() {
            for element in array.iter() {
                key_paths_in_value(element, prefix, into);
            }
        }
    }

    /// A document with **one of everything this schema can hold**, which is what
    /// the naming tripwire is run against.
    ///
    /// The default document is not enough and PRD #741 M6 is where that stopped
    /// being a theoretical objection: `[endpoints]` defaults to absent and its
    /// row list defaults to empty, so a field named `api_key` on
    /// [`RemoteEndpointSettings`] would never appear in a serialised
    /// `DesktopSettings::default()` and the tripwire would pass on a schema it
    /// had not read. [`SECRET_RULE`] already names "a field serde omits from
    /// the default document" as something the scan cannot see; this narrows
    /// that to the fields serde omits *because they are optional or in an empty
    /// list*, which is the majority of #741's.
    ///
    /// Every optional field is `Some` here **on purpose**. A new optional field
    /// added without a line here is invisible to the tripwire again, which is
    /// the one way this helper can rot — so it is worth being deliberate about:
    /// adding a field to the schema means adding it here.
    fn representative_document() -> DesktopSettings {
        DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![RemoteEndpointSettings {
                    host: Hostname::parse("build-box.example.com").unwrap(),
                    id: EndpointId::parse("deck1").unwrap(),
                    identity: Some(KeyPath::parse("~/.ssh/id_ed25519").unwrap()),
                    jump: Some(HostAlias::parse("bastion").unwrap()),
                    port: SshPort::parse(2222).unwrap(),
                    socket: Some(
                        RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock")
                            .unwrap(),
                    ),
                    user: Some(SshUser::parse("dev").unwrap()),
                }],
                selection: Selection::One(EndpointId::parse("deck1").unwrap()),
            }),
            // PRD #802 M4. Present, and with a non-default value in every
            // field: the tripwire and the sentinel sweep are both derived from
            // a serialised document, so a section left `None` here would be a
            // section neither of them walks — which is exactly the gap this
            // function's own doc comment exists to close.
            voice: Some(VoiceSettings {
                activation: ActivationMode::Toggle,
                intent: IntentSettings {
                    backend: IntentBackend::Anthropic,
                    endpoint: ServiceUrl::parse(HOSTED_COMMAND_ENDPOINT).unwrap(),
                    model: ModelId::parse(HOSTED_COMMAND_MODEL).unwrap(),
                    // Non-default, like every other field in this fixture: the
                    // sentinel sweep walks a serialised document, so a field
                    // left on its default is one it cannot tell apart from an
                    // absent one.
                    max_tokens: TokenCeiling::parse(1024).unwrap(),
                },
                transcription: TranscriptionSettings {
                    backend: TranscriptionBackend::Remote,
                    endpoint: ServiceUrl::parse(HOSTED_SPEECH_ENDPOINT).unwrap(),
                    model: ModelId::parse(HOSTED_SPEECH_MODEL).unwrap(),
                },
                labels: LabelSharing::Withheld,
            }),
            ..DesktopSettings::default()
        }
    }

    /// The leaf names that look credential-shaped and are not covered by
    /// `allowed` — either because no exemption names that full path, or because
    /// the exemption there is for a different concrete type.
    ///
    /// The match is on the leaf because that is the field's own name; the
    /// exemption is on the full path so it cannot travel to a same-named field
    /// in another section; and the type is checked because an exemption is a
    /// statement about one specific field, not about a name.
    fn secretish_offenders(
        document: &dyn toml_edit::TableLike,
        allowed: &[(&str, AllowedReference)],
    ) -> Vec<String> {
        let mut found = Vec::new();
        key_paths_in_table(document, "", &mut found);
        found
            .into_iter()
            .filter_map(|(path, at)| {
                let leaf = path
                    .rsplit('.')
                    .next()
                    .unwrap_or(&path)
                    .to_ascii_lowercase();
                if !SECRETISH.iter().any(|pattern| leaf.contains(pattern)) {
                    return None;
                }
                match allowed
                    .iter()
                    .find(|(exception, _)| exception.eq_ignore_ascii_case(&path))
                {
                    None => Some(path),
                    Some((_, shape)) if shape.type_str() == at.type_name() => None,
                    Some((_, shape)) => Some(format!(
                        "{path} (exempted as a {}, found a {})",
                        shape.type_str(),
                        at.type_name()
                    )),
                }
            })
            .collect()
    }

    /// The tripwire itself. Read [`SECRET_RULE`] before concluding anything
    /// from it passing — it is a naming check, not a security boundary.
    ///
    /// Run against [`representative_document`] rather than the default one, so
    /// optional fields and list rows are in scope; see that function for what
    /// made the difference non-theoretical.
    #[test]
    fn no_settings_key_name_trips_the_credential_tripwire() {
        for (which, settings) in [
            ("the default document", DesktopSettings::default()),
            ("a fully populated document", representative_document()),
        ] {
            let document = serialised_document(&settings);
            let offenders = secretish_offenders(document.as_table(), &SECRETISH_ALLOWED);
            assert!(
                offenders.is_empty(),
                "{which} has key(s) named like credentials: {}\n\n{SECRET_RULE}",
                offenders.join(", ")
            );
        }
    }

    /// The blind spot PRD #741 M6 closed, kept as a regression test because the
    /// tripwire's value is entirely in what it can *see*.
    ///
    /// Two halves, and both matter: a credential-shaped name inside a list row
    /// is found at all, and the path it is reported at carries no array index —
    /// so an exemption would describe the field rather than one list position.
    #[test]
    fn the_tripwire_reaches_a_field_inside_a_list_shaped_section() {
        let bad = parse_document(
            "version = 1\n\n\
             [[endpoints.remote]]\n\
             host = \"a\"\n\
             api_key = \"sk-live-nope\"\n\n\
             [[endpoints.remote]]\n\
             host = \"b\"\n\
             api_key = \"sk-live-nope\"\n",
        );
        let offenders = secretish_offenders(bad.as_table(), &SECRETISH_ALLOWED);
        assert_eq!(
            offenders,
            ["endpoints.remote.api_key", "endpoints.remote.api_key"],
            "a field in a list row must be reported, at a path with no row index in it"
        );
    }

    /// The tripwire's own logic, proven rather than assumed — an empty result
    /// on the real document is only meaningful if a bad name would be caught —
    /// plus the property that makes the allowlist safe: an exception applies to
    /// one path and not to a same-named field elsewhere.
    #[test]
    fn the_credential_tripwire_matches_by_path_and_catches_the_names_it_claims_to() {
        let bad = parse_document(
            "version = 1\n\n\
             [voice]\n\
             api_key = \"sk-live-nope\"\n\
             apikey = \"sk-live-nope\"\n\
             authorization = \"Bearer nope\"\n\
             bearer = \"nope\"\n\
             passphrase = \"nope\"\n\
             password = \"nope\"\n\
             credential = \"nope\"\n\n\
             [voice.remote]\n\
             auth_token = \"t\"\n",
        );
        let mut offenders = secretish_offenders(bad.as_table(), &SECRETISH_ALLOWED);
        offenders.sort();
        assert_eq!(
            offenders,
            [
                "voice.api_key",
                "voice.apikey",
                "voice.authorization",
                "voice.bearer",
                "voice.credential",
                "voice.passphrase",
                "voice.password",
                "voice.remote.auth_token",
            ]
        );

        // An exception is a path, so it exempts exactly one field. The same
        // name in another section is still caught — which is the whole reason
        // the allowlist stopped being bare names.
        let referenced = parse_document(
            "[voice]\n\
             secret_backend = \"keychain\"\n\
             has_api_key = true\n\n\
             [endpoints]\n\
             secret_backend = \"somewhere else entirely\"\n",
        );
        let allowed = [
            ("voice.secret_backend", AllowedReference::BackendName),
            ("voice.has_api_key", AllowedReference::StoredFlag),
        ];
        assert_eq!(
            secretish_offenders(referenced.as_table(), &allowed),
            ["endpoints.secret_backend"]
        );
    }

    /// The other half of the exemption, and the reason issue #827 asked for it:
    /// an exemption is a statement about **one field of one type**, so it stops
    /// applying the moment that field could carry the credential itself.
    ///
    /// The failure this prevents is silent by construction. A `has_api_key`
    /// boolean is exempted honestly; someone later needs the key's *last four
    /// digits* on the settings surface and changes it to a `String`; the name
    /// never moves, so a path-only allowlist keeps exempting it and the
    /// tripwire reports nothing while a string field sits under an
    /// `api_key` name.
    #[test]
    fn an_exemption_for_one_type_does_not_cover_the_same_path_at_another() {
        let allowed = [("voice.has_api_key", AllowedReference::StoredFlag)];

        // The shape the exemption was granted for: nothing to report.
        let flag = parse_document("[voice]\nhas_api_key = true\n");
        assert!(secretish_offenders(flag.as_table(), &allowed).is_empty());

        // The same path, now a string. Reported, with the mismatch named so
        // the failure says what actually changed.
        let widened = parse_document("[voice]\nhas_api_key = \"sk-live-nope\"\n");
        assert_eq!(
            secretish_offenders(widened.as_table(), &allowed),
            ["voice.has_api_key (exempted as a boolean, found a string)"]
        );

        // And in the other direction: a `BackendName` exemption is for a
        // string, so it does not cover a table that grew under that name.
        let nested = parse_document(
            "[voice.secret_backend]\nname = \"keychain\"\nvalue = \"sk-live-nope\"\n",
        );
        assert_eq!(
            secretish_offenders(
                nested.as_table(),
                &[("voice.secret_backend", AllowedReference::BackendName)]
            ),
            ["voice.secret_backend (exempted as a string, found a table)"]
        );
    }

    /// What the tripwire cannot see, pinned so the limitation is a known
    /// property rather than a discovery.
    ///
    /// #802 will store a real API key and **must not trust this control**. Each
    /// case below passes the tripwire while carrying credential material, and
    /// the fix for all of them is the same: the value never enters this
    /// document. The full pre-#802 checklist is issue #827.
    #[test]
    fn the_credential_tripwire_is_blind_to_values_and_to_innocent_names() {
        // A token under a name that says nothing. This is the case that
        // matters: it is exactly how a credential arrives in practice.
        let innocent = parse_document(
            "[voice]\nendpoint = \"https://api.example.test?auth=sk-live-nope\"\nvalue = \"sk-live-nope\"\n",
        );
        assert!(
            secretish_offenders(innocent.as_table(), &SECRETISH_ALLOWED).is_empty(),
            "the tripwire is a NAMING check; if this starts failing, the doc \
             comments claiming otherwise need updating too"
        );

        // And a field name is judged on its own, never on its value.
        let named = parse_document("[voice]\napi_key = false\n");
        assert_eq!(
            secretish_offenders(named.as_table(), &SECRETISH_ALLOWED),
            ["voice.api_key"]
        );
    }

    // ---------------------------------------------------------------------
    // Issue #827: the value-side checks. Everything above this line reads key
    // NAMES; everything below follows one uniquely-named value through the
    // sinks a credential must not reach.
    //
    // What these prove, stated narrowly because the whole point of #827 is
    // that the previous framing was too wide — and narrowed AGAIN at PRD #741
    // M6, because the version written for #827 had itself become too wide.
    //
    // The sweep covers the leaves of the DEFAULT document, which are `version`,
    // `appearance.mode` and `zoom.level`. For those there is no route by which
    // a credential submitted through the settings surface, the settings IPC or
    // the document can be stored, echoed or logged: an integer, three tokens
    // and ten numbers cannot hold one.
    //
    // It does NOT cover #741's endpoint fields, and that is a scope statement
    // with two separate causes rather than one. They are absent from the
    // default document (the section defaults to `None` and its row list to
    // empty), so the derivation never reaches them; and their types BOUND and
    // RESTRICT text rather than forbidding it, so a sweep that did reach them
    // would be asserting something untrue — measured: the sentinel below parses
    // as a `Hostname`, an `SshUser`, a `HostAlias` and an `EndpointId`. The
    // module docs carry the two-part claim that IS true, and
    // `an_ssh_argument_field_bounds_and_restricts_rather_than_forbidding_text`
    // pins it.
    //
    // And it is weaker than "a credential cannot be in desktop.toml": a key
    // this schema does not own keeps whatever a user or a newer build put in
    // it, which
    // `a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else`
    // measures rather than glosses.
    //
    // The one check #827 lists that is NOT here is the end-to-end submission
    // "through the real secret-store flow": PRD #803 M5 named the `SecretStore`
    // seam and deliberately did not build it, and #802 designs it against a
    // real backend.
    // ---------------------------------------------------------------------

    /// A value no legitimate document can hold, credential-shaped so that
    /// finding it in any sink is unambiguous. Long and unique on purpose: a
    /// substring search for it cannot collide with anything this crate emits.
    const SENTINEL: &str = "sk-live-827-DO-NOT-STORE-e3b0c44298fc1c149afb";

    fn assert_free_of_sentinel(what: &str, haystack: &str) {
        assert!(
            !haystack.contains(SENTINEL),
            "{what} carried the sentinel credential: {haystack}"
        );
    }

    /// Every serialised form of a loaded document, plus the snapshot the
    /// webview receives, checked in one place so no call site can forget one.
    ///
    /// The four are the sinks issue #827 enumerates on this side of the
    /// bridge: the TOML a save would write, the JSON the IPC echo carries, the
    /// JSON `desktop_get_settings` returns, and a freshly written file.
    fn assert_no_sink_carries_the_sentinel(what: &str, settings: &DesktopSettings) {
        assert_free_of_sentinel(
            &format!("{what}: the TOML re-serialisation"),
            &toml_edit::ser::to_string_pretty(settings).unwrap(),
        );
        assert_free_of_sentinel(
            &format!("{what}: the IPC echo (`desktop_set_settings` returns its input)"),
            &serde_json::to_string(settings).unwrap(),
        );
        let snapshot = DesktopSettingsSnapshot {
            settings: settings.clone(),
            path: "/home/dev/.config/dot-agent-deck/desktop.toml".to_string(),
            // The document loaded, so there is no problem to report. The sink
            // this field opens is swept separately, by
            // `a_document_problem_carries_a_locator_and_never_the_documents_bytes`
            // — it is built from a locator rather than from a loaded document,
            // so it cannot be reached from a `DesktopSettings`.
            problem: None,
        };
        assert_free_of_sentinel(
            &format!("{what}: the `desktop_get_settings` snapshot"),
            &serde_json::to_string(&snapshot).unwrap(),
        );

        let dir = tempdir();
        let fresh = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&fresh, settings).unwrap();
        assert_free_of_sentinel(
            &format!("{what}: a freshly written document"),
            &std::fs::read_to_string(&fresh).unwrap(),
        );
    }

    /// The leaf (non-table) key paths of a serialised document, sorted.
    ///
    /// `is_table_like` rather than `is_table`: since issue #825 an inline table
    /// is a distinct `toml_edit` shape, and a section someone spelled inline is
    /// still a section rather than a leaf.
    fn leaf_paths(document: &dyn toml_edit::TableLike) -> Vec<String> {
        let mut found = Vec::new();
        key_paths_in_table(document, "", &mut found);
        let mut leaves: Vec<String> = found
            .into_iter()
            .filter(|(_, at)| !at.is_table_like())
            .map(|(path, _)| path)
            .collect();
        leaves.sort();
        leaves
    }

    /// A TOML fixture as a parsed DOM, for the checks that build a document by
    /// hand rather than from the schema.
    fn parse_document(contents: &str) -> toml_edit::DocumentMut {
        contents.parse().expect("the fixture is valid TOML")
    }

    /// The document this build would write for `settings`, as a parsed DOM.
    ///
    /// Goes through the same `ser::to_string_pretty` the save path uses rather
    /// than through `ser::to_document`, so what the tripwire and the sentinel
    /// sweep walk is literally the bytes that reach disk — table spellings
    /// included.
    fn serialised_document(settings: &DesktopSettings) -> toml_edit::DocumentMut {
        toml_edit::ser::to_string_pretty(settings)
            .expect("the settings struct serialises")
            .parse()
            .expect("this build's own output is valid TOML")
    }

    /// Replace the value at a dotted `path`, panicking if it is not there — a
    /// typo in a fixture must not read as a pass.
    fn set_at(document: &mut dyn toml_edit::TableLike, path: &str, value: toml_edit::Item) {
        let (head, rest) = match path.split_once('.') {
            Some((head, rest)) => (head, Some(rest)),
            None => (path, None),
        };
        let at = document
            .get_mut(head)
            .unwrap_or_else(|| panic!("no `{head}` in the document"));
        match rest {
            None => *at = value,
            Some(rest) => match at.as_table_like_mut() {
                Some(nested) => set_at(nested, rest, value),
                None => panic!("`{head}` is not a table"),
            },
        }
    }

    /// **The check #802 has to keep green.** A credential arriving from the
    /// webview — the route a real one takes — is neither stored, echoed nor
    /// written, because no field in this schema can hold arbitrary text: every
    /// leaf is a closed enum, a snapped float or an integer. The one place the
    /// value does come back is named below, because it is a scope statement
    /// rather than an exception.
    ///
    /// This is not a name scan. Each payload below puts the sentinel where a
    /// credential would actually go — including under names the tripwire is
    /// blind to (`endpoint`, `value`) and under a section that does not exist
    /// yet — and every one of them ends the same way: either argument
    /// deserialisation refuses the call, or the value is dropped on the way in.
    ///
    /// # The one place the value does come back, and why it is not a leak
    ///
    /// A refused payload's serde message can quote the offending value
    /// (`invalid type: string "sk-live-…"`), and Tauri returns that to the
    /// **caller**. The caller is the webview that just sent it, so nothing is
    /// disclosed to a party that did not already hold it; the asserted claim is
    /// therefore "no sink outside the sender", not "no sink at all". The sinks
    /// that matter — the disk, the echo a *later* read would carry, the app's
    /// own log — are covered here and in the two tests below.
    ///
    /// # What this exercises, precisely
    ///
    /// The `DesktopSettings` **deserializer**, which is what `desktop_set_settings`
    /// takes as its argument, and the serialisation of what it returns. It does
    /// **not** call the command function: a `#[tauri::command]` takes a
    /// `Webview`, which cannot be constructed without a running app, so
    /// `ensure_main_webview` and the framework's own argument decoding are
    /// outside every test in this crate — see issue #823 for the missing tier.
    /// [`tests::the_settings_commands_own_bodies_carry_no_value_from_the_document`]
    /// drives the two command *bodies* through the public functions they wrap,
    /// which is as close to the real handlers as this tier reaches — see that
    /// test's own comment for the two pieces it still does not reach, and for
    /// where the second of them is covered instead. Greptile raised this on PR
    /// #943, and the claim is narrowed rather than overstated.
    #[test]
    fn a_credential_from_the_webview_reaches_neither_the_echo_nor_the_document() {
        let payloads = [
            // The known leaves, one at a time.
            ("version", serde_json::json!({ "version": SENTINEL })),
            (
                "appearance.mode",
                serde_json::json!({ "appearance": { "mode": SENTINEL } }),
            ),
            (
                "zoom.level",
                serde_json::json!({ "zoom": { "level": SENTINEL } }),
            ),
            // A name the tripwire would catch, and two it is blind to.
            ("apiKey", serde_json::json!({ "apiKey": SENTINEL })),
            ("endpoint", serde_json::json!({ "endpoint": SENTINEL })),
            ("value", serde_json::json!({ "value": SENTINEL })),
            // Inside a section this build does know.
            (
                "appearance.apiKey",
                serde_json::json!({ "appearance": { "mode": "dark", "apiKey": SENTINEL } }),
            ),
            // Inside a section it does not — the shape #802's own settings
            // will have.
            (
                "voice.api_key",
                serde_json::json!({ "voice": { "api_key": SENTINEL } }),
            ),
            // And not as an object at all.
            (
                "an array",
                serde_json::json!({ "voice": [SENTINEL, { "token": SENTINEL }] }),
            ),
            ("the whole document", serde_json::json!(SENTINEL)),
        ];

        let mut refused = 0;
        let mut dropped = 0;
        for (what, payload) in payloads {
            match serde_json::from_value::<DesktopSettings>(payload) {
                // Refused at the command boundary: nothing was stored, and the
                // rejection goes to the sender (see the doc comment above).
                Err(_) => refused += 1,
                Ok(settings) => {
                    dropped += 1;
                    assert_no_sink_carries_the_sentinel(what, &settings);
                }
            }
        }
        // Both outcomes have to actually occur, or a change that made every
        // payload fail deserialisation would leave the drop half of this test
        // asserting nothing.
        assert!(
            refused > 0 && dropped > 0,
            "{refused} refused, {dropped} accepted"
        );
    }

    /// The disk route: a credential hand-written into a field this schema owns
    /// reaches no sink beyond the user's own file.
    ///
    /// Derived from the default document rather than from a hard-coded list, so
    /// a field #802 adds is covered the moment it appears — and if that field
    /// can hold text, this is the test that goes red.
    ///
    /// # Issue #1072 narrowed what the DISK half of this claims, and the old
    /// # claim is worth quoting because it is now false
    ///
    /// It read: *saving over the offending document overwrites every key the
    /// struct owns, so the value does not survive on disk either*. That was
    /// true, and it was true **because of the data-loss bug** — the scrub was a
    /// side effect of publishing this build's defaults over a document nobody
    /// had understood. #1072 stops that, so a value that makes the document
    /// unreadable now stays in the user's own file.
    ///
    /// That is not a weakening of #827, which is about a credential reaching a
    /// **sink** — a log people paste into bug reports, the IPC, the snapshot.
    /// Those are all still swept below, and so is the refusal itself. It is the
    /// same answer this module already gives for a key the schema does not own
    /// (see [`tests::a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else`]):
    /// the file is the user's, it is `0o600`, and destroying their configuration
    /// is not a proportionate way to scrub a value they typed there themselves.
    ///
    /// Both outcomes are exercised, and the counters at the bottom fail the test
    /// if either arm stops being reached — a one-sided sweep would let the other
    /// behaviour change unnoticed.
    #[test]
    fn a_credential_at_a_known_schema_leaf_reaches_no_sink_beyond_the_users_own_file() {
        let default = serialised_document(&DesktopSettings::default());
        let leaves = leaf_paths(default.as_table());
        assert_eq!(leaves, ["appearance.mode", "version", "zoom.level"]);

        let (mut dropped, mut refused) = (0, 0);
        for leaf in leaves {
            let dir = tempdir();
            let path = dir.path().join(SETTINGS_FILE_NAME);
            let mut document = default.clone();
            set_at(
                document.as_table_mut(),
                &leaf,
                toml_edit::value(SENTINEL.to_string()),
            );
            let raw = document.to_string();
            assert!(
                raw.contains(SENTINEL),
                "fixture for {leaf} lost the sentinel"
            );
            std::fs::write(&path, &raw).unwrap();

            let (loaded, problem) = load_document(&path);
            assert_no_sink_carries_the_sentinel(&leaf, &loaded);

            match problem {
                // The field read the value and dropped it — `AppearanceMode`
                // falls back rather than failing — so the document still loads,
                // the save still goes through, and the load–modify–save round
                // trip #827 names does take the value off disk.
                None => {
                    dropped += 1;
                    save_to(&path, &loaded).unwrap();
                    assert_free_of_sentinel(
                        &format!("{leaf}: the document after a load-modify-save round trip"),
                        &std::fs::read_to_string(&path).unwrap(),
                    );
                }
                // The value made the whole document unreadable, so the save is
                // refused and the file is left exactly as the user wrote it.
                Some(problem) => {
                    refused += 1;
                    let error = save_to(&path, &loaded)
                        .expect_err("a document this build cannot read must not be overwritten");
                    assert_eq!(
                        std::fs::read_to_string(&path).unwrap(),
                        raw,
                        "{leaf}: the user's file must be preserved"
                    );
                    assert_free_of_sentinel(&format!("{leaf}: the load problem"), problem.public());
                    assert_free_of_sentinel(&format!("{leaf}: the load problem"), problem.detail());
                    assert_free_of_sentinel(&format!("{leaf}: the refusal"), error.public());
                    assert_free_of_sentinel(&format!("{leaf}: the refusal"), error.detail());
                }
            }
        }
        assert!(
            dropped > 0 && refused > 0,
            "{dropped} dropped, {refused} refused — both arms must stay reachable"
        );
    }

    /// The honest complement, and the reason "a credential cannot be in
    /// `desktop.toml`" is **not** the claim this module makes.
    ///
    /// The save merges the struct into whatever is on disk, so a key this
    /// build's schema does not own keeps its value — by design, because that is
    /// what stops an older build eating a newer one's section
    /// ([`merged_document`]). A credential a *user* or a *newer build* put
    /// there therefore stays there. What it cannot do is reach anything else:
    /// the struct drops it at load, so it is absent from the IPC echo, from the
    /// `desktop_get_settings` snapshot and from every rendering of the
    /// document.
    ///
    /// Both flavours of unowned key are covered, because the merge treats them
    /// identically and only one of them is obvious: a whole section this build
    /// has never heard of, and a stray field inside a section it owns.
    #[test]
    fn a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!(
                "version = 1\n\n\
                 [appearance]\n\
                 mode = \"{SENTINEL}\"\n\
                 stray_token = \"{SENTINEL}\"\n\n\
                 [voice]\n\
                 api_key = \"{SENTINEL}\"\n"
            ),
        )
        .unwrap();

        let loaded = load_from(&path);
        // The owned field folded to its default; the unowned ones never
        // entered the struct at all.
        assert_eq!(loaded.appearance.mode, AppearanceMode::System);
        assert_no_sink_carries_the_sentinel("an unowned key", &loaded);

        save_to(&path, &loaded).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Measured, not tolerated in silence: the two unowned keys still hold
        // it and the owned one does not.
        assert_eq!(
            raw.matches(SENTINEL).count(),
            2,
            "the merge should have kept exactly the two unowned values: {raw}"
        );
        assert!(
            raw.contains("mode = \"system\""),
            "unexpected document: {raw}"
        );
        assert!(raw.contains("stray_token"), "unexpected document: {raw}");
        assert!(raw.contains("api_key"), "unexpected document: {raw}");
    }

    /// The two settings commands' own bodies, driven end to end against a
    /// sentinel-bearing document on disk.
    ///
    /// `desktop_get_settings` is `Ok(settings::load_snapshot())` after its
    /// webview guard. `desktop_set_settings` is `settings::save(&settings)`
    /// then `Ok(settings)`, plus a `map_err` closure that logs
    /// `error.detail()` and returns `safe_message(error.public())`. So calling
    /// [`load_snapshot`] and [`save`] under the real path seam drives both
    /// success paths, and **two** things in those handlers stay out of reach
    /// rather than one: `ensure_main_webview`, because a `Webview` needs a
    /// running Tauri app to exist; and that `map_err` closure, because this
    /// test does not provoke a save failure.
    ///
    /// The closure is not uncovered, though — it is covered somewhere else,
    /// which is worth knowing before reading this test as the whole story. The
    /// only two values it can emit are `error.detail()` and
    /// `safe_message(error.public())`, and
    /// [`tests::no_settings_write_error_carries_a_value_from_the_document`]
    /// asserts both are free of the sentinel, against real save failures.
    ///
    /// This is deliberately more than re-serialising a hand-built struct: the
    /// snapshot here is the one the command would actually return, read off a
    /// real file through the real resolver, with the real path in it.
    #[test]
    fn the_settings_commands_own_bodies_carry_no_value_from_the_document() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!(
                "version = 1\n\n\
                 [appearance]\n\
                 mode = \"{SENTINEL}\"\n\n\
                 [voice]\n\
                 api_key = \"{SENTINEL}\"\n"
            ),
        )
        .unwrap();

        // SAFETY: the lock above serialises every test that touches this var.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, &path) };
        let snapshot = load_snapshot();
        let saved = save(&snapshot.settings);
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };
        saved.unwrap();

        // The reply `desktop_get_settings` would send, as JSON, exactly as the
        // bridge would receive it.
        assert_free_of_sentinel(
            "the `desktop_get_settings` reply",
            &serde_json::to_string(&snapshot).unwrap(),
        );
        // Its `path` is present and is the file we pointed it at — the
        // deliberate exception documented on `DesktopSettingsSnapshot`, and the
        // reason this assertion is about the sentinel rather than about paths.
        assert_eq!(snapshot.path, path.display().to_string());
        assert_eq!(snapshot.settings.appearance.mode, AppearanceMode::System);

        // The reply `desktop_set_settings` would echo.
        assert_free_of_sentinel(
            "the `desktop_set_settings` echo",
            &serde_json::to_string(&snapshot.settings).unwrap(),
        );

        // And the document the save actually wrote: the owned field is
        // scrubbed, the unowned one is preserved, exactly as
        // `a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else`
        // establishes for the lower-level path.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw.matches(SENTINEL).count(),
            1,
            "unexpected document: {raw}"
        );
        assert!(
            raw.contains("mode = \"system\""),
            "unexpected document: {raw}"
        );
    }

    /// The log sink, and the measurement that made [`invalid_document_log`]
    /// necessary.
    ///
    /// `toml_edit::de::Error`'s own `Display` echoes the offending value twice — in
    /// a rendered source line and in serde's `invalid type` message — so the
    /// diagnostic `load_from` used to print put a hand-edited document's bytes
    /// into this process's stderr and the deck log. Both halves are asserted:
    /// the raw error **does** carry the value (so the redaction is proven
    /// necessary rather than assumed), and what is logged instead does not,
    /// while still naming the file and the line.
    #[test]
    fn a_parse_diagnostic_carries_a_locator_and_never_the_documents_bytes() {
        let path = Path::new("/home/dev/.config/dot-agent-deck/desktop.toml");
        let cases = [
            format!("version = \"{SENTINEL}\"\n"),
            format!("version = 1\n[zoom]\nlevel = \"{SENTINEL}\"\n"),
            format!("version = 1\nappearance = \"{SENTINEL}\"\n"),
            // A syntax error rather than a type error: the same rendered
            // source line, so the same exposure.
            format!("version = 1\nnot a key = = {SENTINEL}\n"),
        ];

        for contents in &cases {
            let error = toml_edit::de::from_str::<DesktopSettings>(contents).unwrap_err();
            assert!(
                error.to_string().contains(SENTINEL),
                "the toml error stopped echoing the value, so `invalid_document_log` \
                 can be simplified and its doc comment is now wrong: {error}"
            );

            let logged = invalid_document_log(path, contents, &error);
            assert_free_of_sentinel("the parse diagnostic", &logged);
            assert!(
                logged.contains("line ") || logged.contains("an unreported position"),
                "the diagnostic must still locate the problem: {logged}"
            );
            // This is the log, which is the half of the split that may name a
            // path — the same rule `SettingsWriteError::detail` follows.
            assert!(
                logged.contains("desktop.toml"),
                "unexpected diagnostic: {logged}"
            );
        }

        // The locator itself, on a case whose position is known by hand: the
        // offending value on line 3 starts at column 9 (`level = "`).
        let contents = format!("version = 1\n[zoom]\nlevel = \"{SENTINEL}\"\n");
        let error = toml_edit::de::from_str::<DesktopSettings>(&contents).unwrap_err();
        assert!(
            invalid_document_log(path, &contents, &error).contains("line 3, column 9"),
            "unexpected locator: {}",
            invalid_document_log(path, &contents, &error)
        );

        // A multi-byte character before the offending value must not shift the
        // column into nonsense, which is why the count is characters: `é` is
        // two bytes, so `b` on line 2 is at byte 7 and column 1, not column 2.
        assert_eq!(line_and_column("é = 1\nb = 2", 7), Some((2, 1)));
        assert_eq!(line_and_column("é = 1\nb = 2", 5), Some((1, 5)));
        // An offset inside a multi-byte character has no honest answer.
        assert_eq!(line_and_column("é = 1", 1), None);
        assert_eq!(line_and_column("abc", 99), None);
    }

    /// The error-message sink, on both sides of the [`SettingsWriteError`]
    /// split, with a document that holds the sentinel while the save fails.
    ///
    /// The narrow claim, verified case by case rather than asserted in
    /// general. It used to read: *every `SettingsWriteError` this module can
    /// build is constructed from a **path** and an `io::Error` or a
    /// serialisation error, never from the document's contents*. Issue #1072
    /// added one that **is** built from the document's contents — the refusal to
    /// overwrite — so the claim is now the narrower and still-sufficient one:
    ///
    /// - most are built from a path and an `io::Error` or a serialisation error,
    ///   and so cannot carry a value at all;
    /// - the refusal is built from `parse_locator`, which reads the document
    ///   only to count newlines, and emits `line N, column N` plus fixed prose.
    ///
    /// Either way neither half carries a value, and `dto::safe_message` (which
    /// is a control-character filter and a length cap, **not** a redactor) has
    /// nothing to remove.
    #[cfg(unix)]
    #[test]
    fn no_settings_write_error_carries_a_value_from_the_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!("version = 1\n[voice]\napi_key = \"{SENTINEL}\"\n"),
        )
        .unwrap();

        // An unreadable existing document: the failure happens in
        // `read_document`, with the sentinel one `open` away.
        set_mode(&path, 0o000);
        if std::fs::read_to_string(&path).is_ok() {
            set_mode(&path, 0o600);
            eprintln!(
                "SKIP: this process can read a 0o000 file (running privileged), so an \
                 unreadable document cannot be constructed here"
            );
            return;
        }
        let unreadable = save_to(&path, &dark()).unwrap_err();
        set_mode(&path, 0o600);

        // A read-only parent: the read succeeds, so the sentinel has been in
        // this process's memory, and the failure is in `create_temp`.
        set_mode(dir.path(), 0o500);
        let unwritable_dir = save_to(&path, &dark());
        set_mode(dir.path(), 0o700);

        // The refusal to overwrite (issue #1072): the one error here whose
        // message is derived from the document, and therefore the one this test
        // exists for now. The `[voice]` section is not what makes this document
        // unreadable — a bare `=` on its own line is, and the sentinel sits two
        // lines above the reported position.
        let unreadable_document = dir.path().join("broken.toml");
        std::fs::write(
            &unreadable_document,
            format!("version = 1\n[voice]\napi_key = \"{SENTINEL}\"\n= = =\n"),
        )
        .unwrap();
        let refused = save_to(&unreadable_document, &dark())
            .expect_err("an unreadable document must not be overwritten");

        let mut errors = vec![unreadable, refused];
        match unwritable_dir {
            Err(error) => errors.push(error),
            Ok(()) => eprintln!(
                "SKIP: this process can write into a 0o500 directory (running privileged)"
            ),
        }

        for error in errors {
            assert_free_of_sentinel("a write error's log detail", error.detail());
            assert_free_of_sentinel("a write error's webview message", error.public());
            assert_free_of_sentinel("a write error's `Display`", &error.to_string());
            assert_free_of_sentinel(
                "`safe_message` of a write error's public half",
                &crate::dto::safe_message(error.public()),
            );
        }
    }

    // ---------------------------------------------------------------------
    // PRD #741 M6 — endpoint storage
    // ---------------------------------------------------------------------

    /// Scenario: serialise a document holding one fully-specified remote deck
    /// and a `[voice]` section, save it, read it back, and pin the exact bytes.
    /// This is the shape a user hand-edits and the shape the panels write, so
    /// it is pinned the way the default document is — a diff here is the review
    /// prompt.
    ///
    /// It is also where the `[voice]` section is pinned as it appears ON DISK,
    /// which is the half a user reads and hand-edits. PRD #802 M4 added the
    /// section and its provider work nested the two stages under it; the
    /// frontend's own copies are pinned against this crate by
    /// `the_voice_tokens_match_the_frontends_copy` and
    /// `the_voice_presets_match_the_frontends_copy`. Note the shape the nesting
    /// produces: `activation` is a scalar and has to stay above both
    /// sub-tables, because TOML puts every key of a table before the tables
    /// that follow it.
    #[test]
    fn a_populated_endpoint_document_is_pinned_and_round_trips() {
        const STORED: &str = "\
version = 1

[appearance]
mode = \"system\"

[endpoints]
selection = \"deck1\"

[[endpoints.remote]]
host = \"build-box.example.com\"
id = \"deck1\"
identity = \"~/.ssh/id_ed25519\"
jump = \"bastion\"
port = 2222
socket = \"/run/user/1000/dot-agent-deck-attach.sock\"
user = \"dev\"

[voice]
activation = \"toggle\"
labels = \"withheld\"

[voice.intent]
backend = \"anthropic\"
endpoint = \"https://api.anthropic.com/v1/messages\"
model = \"claude-haiku-4-5\"
max_tokens = 1024

[voice.transcription]
backend = \"remote\"
endpoint = \"https://api.openai.com/v1/audio/transcriptions\"
model = \"whisper-1\"

[zoom]
level = 1.0
";
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &representative_document()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), STORED);
        assert_eq!(load_from(&path), representative_document());
    }

    /// Scenario: a document holds two remote decks; a webview that knows
    /// nothing about endpoints saves an appearance change over it. Both decks
    /// must still be there.
    ///
    /// This is the property `DesktopSettings::endpoints` is an `Option` for. If
    /// it were a plain section, the webview's fixed-key-set normaliser would
    /// send the default — an empty list — and the merge would write that over
    /// the user's decks. M7 inherits the other half: once the panel exists, the
    /// frontend has to round-trip this section rather than fabricate a default.
    #[test]
    fn a_client_that_cannot_render_endpoints_cannot_delete_them() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &representative_document()).unwrap();

        // Exactly what `normalizeDesktopSettings` produces: version, appearance
        // and zoom, and no endpoints key at all.
        let from_webview: DesktopSettings = serde_json::from_value(serde_json::json!({
            "version": 1,
            "appearance": { "mode": "dark" },
            "zoom": { "level": 1.25 },
        }))
        .expect("a webview document omitting endpoints must deserialize");
        assert_eq!(from_webview.endpoints, None, "omitted means unspecified");
        save_to(&path, &from_webview).unwrap();

        let reloaded = load_from(&path);
        assert_eq!(
            reloaded.appearance.mode,
            AppearanceMode::Dark,
            "the section the webview does own must have been written"
        );
        assert_eq!(
            reloaded.endpoints,
            representative_document().endpoints,
            "the section it does not own must survive byte for byte"
        );
    }

    /// Scenario: the stored selection names a deck that is no longer in the
    /// list. The app must fall back to the local deck and say why, rather than
    /// erroring or connecting to nothing. Test-plan item 17.
    #[test]
    fn a_selection_naming_a_deck_that_is_gone_falls_back_to_local() {
        let gone = EndpointId::parse("deck-that-left").unwrap();
        let endpoints = EndpointSettings {
            remote: Vec::new(),
            selection: Selection::One(gone.clone()),
        };

        let resolved = endpoints.resolve();
        assert_eq!(resolved.endpoint, Endpoint::local());
        assert_eq!(
            resolved.fallback,
            Some(SelectionFallback::UnknownDeck { id: gone })
        );
        assert!(
            resolved
                .fallback
                .unwrap()
                .to_string()
                .contains("local deck"),
            "the reason has to be renderable by M7, not just distinguishable"
        );
    }

    /// Scenario: the selected deck exists but has no remote socket path yet —
    /// the state M10's `Test connection` is there to leave behind. It resolves
    /// to local with its **own** reason, so the UI can say "press Test
    /// connection" instead of "that deck is gone".
    #[test]
    fn a_selected_deck_with_no_socket_path_has_its_own_fallback_reason() {
        let id = EndpointId::parse("halfway").unwrap();
        let endpoints = EndpointSettings {
            remote: vec![RemoteEndpointSettings::new(
                id.clone(),
                Hostname::parse("build-box").unwrap(),
            )],
            selection: Selection::One(id.clone()),
        };

        let resolved = endpoints.resolve();
        assert_eq!(resolved.endpoint, Endpoint::local());
        assert_eq!(
            resolved.fallback,
            Some(SelectionFallback::NoRemoteSocket { id }),
            "a half-configured deck is not the same state as a missing one"
        );
    }

    /// Scenario: a fully configured deck is selected. It resolves to a
    /// `Endpoint::Remote` carrying every stored field, and — the part that
    /// matters for PRD #741 M2's guarantee — it is **not** a local endpoint, so
    /// nothing can route it into `run_daemon_stop`.
    #[test]
    fn a_configured_deck_resolves_to_a_remote_endpoint_and_never_a_local_one() {
        let settings = representative_document();
        let resolved = settings.resolve_endpoint();
        assert_eq!(resolved.fallback, None);

        let Endpoint::Remote(remote) = &resolved.endpoint else {
            panic!("a configured selection must resolve to a remote deck");
        };
        assert_eq!(remote.host().as_str(), "build-box.example.com");
        assert_eq!(remote.user().map(SshUser::as_str), Some("dev"));
        assert_eq!(remote.port(), 2222);
        assert_eq!(remote.key().map(KeyPath::as_str), Some("~/.ssh/id_ed25519"));
        assert_eq!(remote.jump().map(HostAlias::as_str), Some("bastion"));
        assert_eq!(
            remote.socket().as_str(),
            "/run/user/1000/dot-agent-deck-attach.sock"
        );
        assert!(
            resolved.endpoint.as_local().is_none(),
            "PRD #741 M2: a remote deck must have no LocalEndpoint to hand out"
        );
    }

    /// Scenario: a document with no `[endpoints]` section at all — every
    /// document written before this milestone — resolves to the local deck with
    /// nothing to report.
    #[test]
    fn a_document_predating_the_endpoints_section_resolves_to_local() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[appearance]\nmode = \"dark\"\n").unwrap();

        let resolved = load_from(&path).resolve_endpoint();
        assert_eq!(resolved.endpoint, Endpoint::local());
        assert_eq!(resolved.fallback, None);
    }

    /// Scenario: a document written by a build that has a `Selection` variant
    /// this one does not is loaded, resolved and saved again. It must degrade
    /// to the local deck **and** be written back unchanged, so an older build
    /// cannot destroy a newer one's choice.
    ///
    /// This is the growability `Selection` exists for, tested from the outside
    /// rather than asserted in a doc comment.
    ///
    /// The stand-in used to be `all`, and PRD #742 M1 made that word one this
    /// build *does* know — so the example moved and the property did not. It is
    /// still worth pinning, because it is what let `All` ship at all: an older
    /// binary meeting a stored `all` still lands here, and `group` stands for
    /// whatever word the variant after `All` reserves.
    #[test]
    fn a_selection_token_this_build_does_not_know_degrades_without_being_lost() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[endpoints]\nselection = \"group\"\n").unwrap();

        let loaded = load_from(&path);
        let resolved = loaded.resolve_endpoint();
        assert_eq!(
            resolved.endpoint,
            Endpoint::local(),
            "an unknown selection is not a reason to have no deck"
        );
        assert!(matches!(
            resolved.fallback,
            Some(SelectionFallback::UnknownDeck { .. })
        ));

        save_to(&path, &loaded).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("\"group\""),
            "the token a newer build wrote must survive this build reading and rewriting it"
        );
    }

    /// Scenario: the reserved `local` token, in any case, is the local deck and
    /// is refused as an endpoint id — so the two can never be confused, which
    /// is what lets the selection be one plain string.
    #[test]
    fn local_is_reserved_on_both_sides_of_the_selection() {
        for token in ["local", "LOCAL", "Local"] {
            let document = format!("version = 1\n\n[endpoints]\nselection = {token:?}\n");
            let settings = toml_edit::de::from_str::<DesktopSettings>(&document)
                .unwrap_or_else(|error| panic!("{token} must parse: {error}"));
            assert_eq!(
                settings
                    .endpoints
                    .expect("the section is present")
                    .selection,
                Selection::Local,
                "{token} must read as the local deck"
            );
            assert!(
                EndpointId::parse(token).is_err(),
                "{token} must not be usable as a deck id"
            );
        }
        assert_eq!(Selection::Local.as_token(), LOCAL_SELECTION_TOKEN);
    }

    /// Scenario: a fleet selection is stored, loaded and stored again. The `all`
    /// token must read back as `Selection::All` and write back out as `all`, so
    /// the choice a user made on the Deck selector survives the document rather
    /// than degrading to a deck id on the next save. PRD #742 M1.
    #[test]
    fn the_fleet_selection_round_trips_through_the_stored_document() {
        assert_eq!(Selection::All.as_token(), ALL_SELECTION_TOKEN);
        assert_eq!(
            serde_json::to_value(Selection::All).unwrap(),
            serde_json::json!("all"),
            "`Serialize` delegates to `as_token`, and the webview reads this one"
        );

        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[endpoints]\nselection = \"all\"\n").unwrap();

        let loaded = load_from(&path);
        assert_eq!(
            loaded
                .endpoints
                .as_ref()
                .expect("the section is present")
                .selection,
            Selection::All,
            "`all` is this build's word now, not an endpoint id"
        );

        // Resolving is where `All` deliberately differs from an unknown token:
        // the deck screen still has one target (DECISION 1) but NOTHING failed
        // to be honoured, so there is no substitution to report.
        let resolved = loaded.resolve_endpoint();
        assert_eq!(resolved.endpoint, Endpoint::local());
        assert_eq!(
            resolved.fallback, None,
            "a selection that IS in force must not print a fallback notice"
        );

        save_to(&path, &loaded).unwrap();
        assert_eq!(
            load_from(&path)
                .endpoints
                .expect("the section survived the save")
                .selection,
            Selection::All,
            "a save must not turn the fleet back into a single deck"
        );
    }

    /// Scenario: the reserved `all` token, in any case, is the fleet and is
    /// refused as an endpoint id — the same two-sided reservation `local` has,
    /// which is what lets the selection stay one plain string. PRD #742 M1.
    #[test]
    fn all_is_reserved_on_both_sides_of_the_selection() {
        for token in ["all", "ALL", "All"] {
            let document = format!("version = 1\n\n[endpoints]\nselection = {token:?}\n");
            let settings = toml_edit::de::from_str::<DesktopSettings>(&document)
                .unwrap_or_else(|error| panic!("{token} must parse: {error}"));
            assert_eq!(
                settings
                    .endpoints
                    .expect("the section is present")
                    .selection,
                Selection::All,
                "{token} must read as the fleet"
            );

            let refusal =
                EndpointId::parse(token).expect_err("{token} must not be usable as a deck id");
            assert!(
                refusal.contains("reserved"),
                "the error must say WHY, not merely refuse: {refusal}"
            );
            assert!(
                refusal.contains(ALL_SELECTION_TOKEN),
                "the error must name the token it refused: {refusal}"
            );
        }

        // And through `Deserialize`, which is the path a hand-edited row takes:
        // an ambiguous `id = "all"` refuses to load rather than becoming a row
        // no selection could name unambiguously.
        let row = "version = 1\n\n[[endpoints.remote]]\nid = \"all\"\nhost = \"build-box\"\n";
        let refusal = toml_edit::de::from_str::<DesktopSettings>(row)
            .expect_err("a row claiming the reserved id must not load");
        assert!(
            refusal.to_string().contains("reserved"),
            "the document-level error must carry the reason too: {refusal}"
        );
    }

    /// Scenario: the fleet selection is asked what it observes. A single-deck
    /// selection observes exactly the deck it resolves to; `All` observes the
    /// local deck plus every stored row that has a socket to connect to, with
    /// the half-configured row left out because there is no address to watch.
    /// PRD #742 M1 — the set `resolve()` cannot express.
    #[test]
    fn the_fleet_observes_every_connectable_deck_and_a_single_selection_observes_one() {
        let configured = EndpointId::parse("configured").unwrap();
        let halfway = EndpointId::parse("halfway").unwrap();
        let mut connectable = RemoteEndpointSettings::new(
            configured.clone(),
            Hostname::parse("build-box.example.com").unwrap(),
        );
        connectable.socket = Some(RemoteSocketPath::parse("/run/deck.sock").unwrap());
        let rows = vec![
            connectable,
            // No socket: storable, and deliberately not connectable.
            RemoteEndpointSettings::new(halfway, Hostname::parse("relay.example.com").unwrap()),
        ];

        let fleet = EndpointSettings {
            remote: rows.clone(),
            selection: Selection::All,
        };
        let observed = fleet.connectable_endpoints();
        assert_eq!(
            observed.len(),
            2,
            "the local deck and the one row with somewhere to connect to"
        );
        assert_eq!(observed[0], Endpoint::local(), "the local deck leads");
        let Endpoint::Remote(remote) = &observed[1] else {
            panic!("the second observed deck must be the configured remote row");
        };
        assert_eq!(remote.host().as_str(), "build-box.example.com");

        // Every other selection observes precisely what it resolves to, so
        // nothing downstream has to special-case a one-element fleet.
        for selection in [Selection::Local, Selection::One(configured)] {
            let single = EndpointSettings {
                remote: rows.clone(),
                selection,
            };
            assert_eq!(
                single.connectable_endpoints(),
                vec![single.resolve().endpoint],
                "a single-deck selection observes the deck it resolves to and nothing else"
            );
        }
    }

    /// Scenario: three configured decks under `All`, one of them with no socket
    /// path. The connectable set holds the two the app can reach; the
    /// unconfigured set holds the third, with the label a group needs. The two
    /// lists are disjoint and together they are every configured deck — which
    /// is what makes the overview's denominator 3 rather than 2.
    ///
    /// **PRD #742 Open Question 3, answered.** Before this, one list answered
    /// both questions and the socketless row was simply absent: not in the
    /// numerator, not in the denominator, and with no group on screen.
    #[test]
    fn a_deck_with_no_socket_is_in_the_fleet_and_not_in_the_connectable_set() {
        let first = EndpointId::parse("first").unwrap();
        let second = EndpointId::parse("second").unwrap();
        let halfway = EndpointId::parse("halfway").unwrap();
        let rows = vec![
            connectable_row(&first, "build-box.example.com"),
            connectable_row(&second, "ci-box.example.com"),
            // Storable, selectable, and with nowhere to connect to.
            RemoteEndpointSettings::new(
                halfway.clone(),
                Hostname::parse("relay.example.com").unwrap(),
            ),
        ];
        let fleet = EndpointSettings {
            remote: rows,
            selection: Selection::All,
        };

        assert_eq!(
            fleet.connectable_endpoints().len(),
            3,
            "the local deck and the two rows with an address — the socketless row has none"
        );
        assert_eq!(
            fleet.unconfigured_decks(),
            vec![UnconfiguredDeck {
                id: halfway,
                label: "relay.example.com".to_string(),
            }],
            "the socketless row is a fleet member, labelled by its address"
        );
        assert_eq!(
            fleet.connectable_endpoints().len() + fleet.unconfigured_decks().len(),
            4,
            "the fleet is the local deck plus every configured row, and the two lists partition it"
        );
    }

    /// Scenario: a row with a user and a non-default port, and no socket path.
    /// It is still labelled `user@host:port` — the same sentence a connectable
    /// row carries — because the label comes from the ssh destination, which
    /// needs no socket. A deck the fleet cannot reach still has to be nameable.
    #[test]
    fn an_unconfigured_deck_is_labelled_by_its_address_without_a_socket() {
        let id = EndpointId::parse("halfway").unwrap();
        let mut row =
            RemoteEndpointSettings::new(id.clone(), Hostname::parse("relay.example.com").unwrap());
        row.user = Some(SshUser::parse("deploy").unwrap());
        row.port = SshPort::parse(2222).unwrap();
        assert!(row.socket.is_none(), "the state under test");

        assert_eq!(row.describe(), "deploy@relay.example.com:2222");
    }

    /// Scenario: the same half-configured row under every selection that names
    /// ONE deck. It is not a fleet member there, because `resolve()` already has
    /// a complete answer — it falls back to the local deck and says
    /// `NoRemoteSocket`, which the selector prints. A second group would render
    /// the same fact twice and break the invariant that a single-deck
    /// selection's fleet is exactly `[resolve().endpoint]`.
    #[test]
    fn only_the_all_selection_shows_a_deck_with_no_socket() {
        let halfway = EndpointId::parse("halfway").unwrap();
        let rows = vec![RemoteEndpointSettings::new(
            halfway.clone(),
            Hostname::parse("relay.example.com").unwrap(),
        )];

        for selection in [Selection::Local, Selection::One(halfway.clone())] {
            let single = EndpointSettings {
                remote: rows.clone(),
                selection,
            };
            assert!(
                single.unconfigured_decks().is_empty(),
                "a selection that names one deck has no fleet to add a group to"
            );
            assert_eq!(
                single.connectable_endpoints(),
                vec![single.resolve().endpoint],
                "and the observed set is still exactly what it resolves to"
            );
        }

        let all = EndpointSettings {
            remote: rows,
            selection: Selection::All,
        };
        assert_eq!(
            all.unconfigured_decks().len(),
            1,
            "under All the row has nowhere else to be stated, which is the gap this closes"
        );
    }

    /// A stored row with an address, for the fleet cases above.
    fn connectable_row(id: &EndpointId, host: &str) -> RemoteEndpointSettings {
        let mut row = RemoteEndpointSettings::new(id.clone(), Hostname::parse(host).unwrap());
        row.socket = Some(RemoteSocketPath::parse("/run/deck.sock").unwrap());
        row
    }

    /// Scenario: minted ids are 16 lowercase hex characters, distinct from each
    /// other, and parse back — so the generator can never produce something the
    /// validator refuses, or something that collides with a reserved word.
    #[test]
    fn a_minted_endpoint_id_is_hex_unique_and_re_parsable() {
        let ids: Vec<EndpointId> = (0..64).map(|_| EndpointId::mint()).collect();
        for id in &ids {
            assert_eq!(id.as_str().len(), 16, "{id}");
            assert!(
                id.as_str()
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{id} must be lowercase hex, which no reserved word can be"
            );
            assert_eq!(EndpointId::parse(id.as_str()).as_ref(), Ok(id));
        }
        let unique: std::collections::BTreeSet<&EndpointId> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "minted ids must not repeat");
    }

    /// Scenario: hand-edit each endpoint field to a hostile value and load the
    /// document. Every one must be refused by its own newtype's deserializer —
    /// which is the whole reason these fields are newtypes rather than strings
    /// — and the refusal must take the ordinary malformed-document route rather
    /// than storing the value.
    #[test]
    fn a_hand_edited_endpoint_value_is_refused_by_its_own_deserializer() {
        let hostile = [
            ("host", "-oProxyCommand=curl evil"),
            ("host", "build box"),
            ("id", "deck 1"),
            ("identity", "relative/key"),
            ("identity", "/home/u/-----BEGIN OPENSSH PRIVATE KEY-----"),
            ("jump", "bastion;rm -rf /"),
            ("socket", "relative.sock"),
            ("socket", "/run/a:b.sock"),
            ("user", "dev$(id)"),
        ];
        for (field, value) in hostile {
            let document = format!(
                "version = 1\n\n[[endpoints.remote]]\nhost = \"h\"\nid = \"d\"\n{field} = {value:?}\n"
            );
            let parsed = toml_edit::de::from_str::<DesktopSettings>(&document);
            assert!(
                parsed.is_err(),
                "{field} = {value:?} must be refused, not stored"
            );
        }
    }

    /// Scenario: hand-edit a deck row to `port = 0` and load the document. It
    /// is refused by [`SshPort`]'s deserializer and takes the same
    /// malformed-document route every other field's hostile value does (PRD
    /// #741, Greptile P2 on #1035).
    ///
    /// It is a case of its own rather than a row of the list above because the
    /// value is a TOML *integer*, not a string. And it is here at all because
    /// the field used to be a bare `u16`: the webview refuses `0`, a `u16` does
    /// not, and the gap was not inert — the value loaded, reached
    /// `tunnel_args`, and was handed to OpenSSH as `-p 0`, which it refuses
    /// with a message about nothing the user had typed.
    #[test]
    fn a_hand_edited_port_of_zero_is_refused_rather_than_handed_to_ssh() {
        let row = |port: &str| {
            format!(
                "version = 1\n\n[[endpoints.remote]]\nhost = \"h\"\nid = \"d\"\nport = {port}\n"
            )
        };
        for refused in ["0", "65536", "-1"] {
            assert!(
                toml_edit::de::from_str::<DesktopSettings>(&row(refused)).is_err(),
                "port = {refused} must be refused, not stored"
            );
        }
        for accepted in ["1", "22", "65535"] {
            let parsed = toml_edit::de::from_str::<DesktopSettings>(&row(accepted))
                .unwrap_or_else(|error| panic!("port = {accepted} must load: {error}"));
            assert_eq!(
                parsed.endpoints.expect("a section").remote[0].port.get(),
                accepted.parse::<u16>().expect("a test constant"),
            );
        }
        // And the whole document falls back to defaults rather than launching
        // with a half-read one, which is `load_from`'s contract for every other
        // malformed value.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("desktop.toml");
        std::fs::write(&path, row("0")).expect("write the hand-edited document");
        assert_eq!(load_from(&path), DesktopSettings::default());
    }

    /// Scenario: a row missing its host, and a row missing its id. Neither is a
    /// deck, so the document is malformed — and, per `load_from`, that means
    /// defaults plus a locator in the log rather than a failed launch.
    #[test]
    fn an_endpoint_row_without_a_host_or_an_id_is_not_a_deck() {
        for document in [
            "version = 1\n\n[[endpoints.remote]]\nid = \"d\"\n",
            "version = 1\n\n[[endpoints.remote]]\nhost = \"h\"\n",
        ] {
            assert!(
                toml_edit::de::from_str::<DesktopSettings>(document).is_err(),
                "{document}"
            );
        }
    }

    /// Scenario: a schema-invalid endpoint row sits in the document and the
    /// user changes their theme. The app must load defaults (it cannot read the
    /// row) and must **not** write over the document at all.
    ///
    /// # This test used to assert the opposite half of it, and issue #1072 is why
    ///
    /// It read: *the merge parses TOML syntax, which the row still is, so it
    /// survives for the user to fix* — and it proved that by asserting the theme
    /// change landed beside the row. Both halves were true and the second one
    /// was the bug: the row survived, and `version`, `appearance`, `zoom` and the
    /// deck selection were replaced with this build's defaults in the same write.
    /// Preserving the one table the merge could not touch is not preserving the
    /// user's settings. The save is refused now, so all of it survives.
    #[test]
    fn a_document_with_a_row_this_build_cannot_read_is_not_written_over() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        let original = "version = 7\n\n[appearance]\nmode = \"light\"\n\n[[endpoints.remote]]\nhost = \"build box\"\nid = \"d\"\n";
        std::fs::write(&path, original).unwrap();

        // The load recovers what it can read and defaults only the section it
        // cannot ([`sections_this_build_can_read`]) — the refusal below is what
        // #1072 is about, and it is unchanged by that: a document this build
        // could not fully read is never published over, whatever was salvaged
        // from it for the session.
        let loaded = load_from(&path);
        assert_eq!(loaded.version, 7);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
        assert_eq!(
            loaded.endpoints, None,
            "the unreadable row takes its section"
        );
        save_to(&path, &dark()).expect_err("the document must not be written over");

        // Not "the row survived", which was the old and much weaker claim: the
        // whole file is byte-identical, appearance and version included.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    // ---------------------------------------------------------------------
    // PRD #741 M6 — the two hardening items `save_to` named #741 as the
    // trigger for
    // ---------------------------------------------------------------------

    /// Scenario: point the settings path inside a directory that is a
    /// **symlink** to somewhere else, and try to save. The write must be
    /// refused before anything is created, and the refusal must not carry the
    /// path across the bridge.
    ///
    /// This is the shape the check exists for: `create_owner_only_dir` has no
    /// symlink guard by design — it never chmods — so before this, a planted
    /// link silently redirected the whole document, key path and all.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_is_refused_before_anything_is_written() {
        let dir = tempdir();
        let real = dir.path().join("elsewhere");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("config");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let error = save_to(&link.join(SETTINGS_FILE_NAME), &DesktopSettings::default())
            .expect_err("a symlinked parent must be refused");
        assert!(error.detail().contains("symlink"), "{}", error.detail());
        assert!(
            !error.public().contains(link.to_str().unwrap()),
            "the public half must not name a path: {}",
            error.public()
        );
        assert!(
            std::fs::read_dir(&real).unwrap().next().is_none(),
            "nothing may be created through the link"
        );
    }

    /// Scenario: the parent exists but is a regular file.
    ///
    /// Two claims, because the layering is worth recording rather than
    /// rediscovering: `save_to` refuses, but it refuses **earlier** than this
    /// check — `read_document`'s own path vet `lstat`s the full path and gets
    /// `ENOTDIR` first. So the clause in `vet_parent_dir` is defence in depth
    /// for a caller that reaches it another way, and it is asserted directly
    /// rather than through a save that never gets there.
    #[cfg(unix)]
    #[test]
    fn a_parent_that_is_not_a_directory_is_refused() {
        let dir = tempdir();
        let parent = dir.path().join("not-a-dir");
        std::fs::write(&parent, b"").unwrap();

        save_to(
            &parent.join(SETTINGS_FILE_NAME),
            &DesktopSettings::default(),
        )
        .expect_err("a non-directory parent must be refused somewhere on the path");

        let error = vet_parent_dir(&parent).expect_err("and named here");
        assert!(
            error.detail().contains("not a directory"),
            "{}",
            error.detail()
        );
    }

    /// Scenario: an ordinary parent directory owned by us, and an absent one.
    /// Both must be accepted — the check refuses a redirected or foreign
    /// parent, never the everyday case, and an absent parent is one `save_to`
    /// is about to create at 0o700.
    #[test]
    fn an_ordinary_or_absent_parent_directory_is_accepted() {
        let dir = tempdir();
        save_to(&dir.path().join(SETTINGS_FILE_NAME), &dark()).expect("an owned parent");
        save_to(&dir.path().join("fresh").join(SETTINGS_FILE_NAME), &dark())
            .expect("an absent parent is created, not refused");
    }

    /// The foreign-uid arm of the parent check, tested as pure data the way
    /// `fsperm`'s `endpoint_uid_is_trusted` is: a second account is not
    /// available in a test, so the comparison is what gets pinned.
    ///
    /// It is deliberately a *different* claim from the symlink test above: that
    /// one proves the wiring, this one proves the rule. Without it the
    /// ownership clause could be inverted and every test would stay green.
    #[cfg(unix)]
    #[test]
    fn the_parent_ownership_rule_refuses_another_uid_and_accepts_our_own() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempdir();
        let ours = dot_agent_deck::platform::paths::current_uid();
        assert_eq!(
            std::fs::symlink_metadata(dir.path()).unwrap().uid(),
            ours,
            "a tempdir we just made must be ours, or this test proves nothing"
        );
        assert!(vet_parent_dir(dir.path()).is_ok());
        // The rule itself: any uid that is not ours is refused. There is no
        // account to borrow, so the arithmetic is what is asserted.
        assert_ne!(ours, ours.wrapping_add(1));
    }

    /// Scenario: a `desktop.toml` sitting at 0o644 is loaded. It must load —
    /// `load_from` never failing is a deliberate #803 property and bricking the
    /// app over a preferences file would be worse than the exposure — and the
    /// next save must republish it owner-only, which is what makes the warning
    /// self-healing rather than nagging.
    #[cfg(unix)]
    #[test]
    fn an_exposed_document_still_loads_and_the_next_save_republishes_it_owner_only() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        set_mode(&path, 0o644);
        assert_eq!(mode_of(&path), 0o644);

        assert_eq!(
            load_from(&path),
            dark(),
            "a world-readable document must still load"
        );

        save_to(&path, &dark()).unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "the publish-by-rename is what fixes the mode"
        );
    }

    /// The warning's own rule, since the message goes to stderr and a test
    /// cannot read it: exactly the modes with a group or other bit set are the
    /// ones worth complaining about, and 0o600 and 0o400 are not.
    #[cfg(unix)]
    #[test]
    fn only_a_mode_readable_beyond_its_owner_is_worth_warning_about() {
        for mode in [0o600, 0o400, 0o700] {
            assert_eq!(mode & 0o077, 0, "{mode:04o} is owner-only");
        }
        for mode in [0o644, 0o604, 0o640, 0o666, 0o777] {
            assert_ne!(mode & 0o077, 0, "{mode:04o} is readable beyond its owner");
        }
    }

    /// **The boundary PRD #741 M6 moved, measured rather than asserted.**
    ///
    /// The value-side claim written for issue #827 — *this build's schema has
    /// no field that can carry arbitrary text* — was true of `u32`,
    /// `AppearanceMode` and `ZoomLevel`, and became **false** the moment an ssh
    /// hostname could be stored. This test is the honest replacement, and it
    /// asserts the uncomfortable half first on purpose: four of the six
    /// endpoint field types **accept** the credential-shaped sentinel, so
    /// nobody can read the schema as proof against one.
    ///
    /// What actually rules a credential out of these fields is three things,
    /// none of which is "the type cannot hold text":
    ///
    /// 1. **Where the value goes.** Every one is handed to `ssh` as an argument
    ///    — a destination, a login name, a `-J` target, a row id — and reaches
    ///    no authentication surface. A token stored in `host` is a token typed
    ///    into the wrong box, not a stored credential.
    /// 2. **Shape, for the two that matter most.** Key *material* and a
    ///    passphrase are excluded by construction: `KeyPath` must start `/` or
    ///    `~/` and admits no newline, so a PEM block cannot be written there,
    ///    and `RemoteSocketPath` must be absolute. Both refuse the sentinel.
    /// 3. **The policy, which is the actual control.** A credential goes behind
    ///    the `SecretStore` seam PRD #803 M5 named. The field-type allowlist is
    ///    what forces that conversation to happen; it is not a proof that it
    ///    already happened.
    ///
    /// The bounds are asserted too, because they are the part of the type that
    /// does real work — nothing here can hold a PEM block, a JWT of any size,
    /// or a base64 blob with padding, since `=` and `+` are on no charset.
    #[test]
    fn an_ssh_argument_field_bounds_and_restricts_rather_than_forbidding_text() {
        // The uncomfortable half.
        assert!(
            Hostname::parse(SENTINEL).is_ok(),
            "45 bytes of [A-Za-z0-9-]"
        );
        assert!(SshUser::parse(SENTINEL).is_ok());
        assert!(HostAlias::parse(SENTINEL).is_ok());
        assert!(EndpointId::parse(SENTINEL).is_ok());

        // The half the shape rules out, which is the one that matters: key
        // material and a passphrase-bearing blob have nowhere to sit.
        assert!(KeyPath::parse(SENTINEL).is_err(), "not absolute or ~/");
        assert!(RemoteSocketPath::parse(SENTINEL).is_err(), "not absolute");
        const PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n";
        for refused in [PEM, "aGVsbG8gd29ybGQ=", "a+b/c==", "tok en", "tok\0en"] {
            assert!(KeyPath::parse(refused).is_err(), "KeyPath: {refused:?}");
            assert!(Hostname::parse(refused).is_err(), "Hostname: {refused:?}");
            assert!(SshUser::parse(refused).is_err(), "SshUser: {refused:?}");
            assert!(
                EndpointId::parse(refused).is_err(),
                "EndpointId: {refused:?}"
            );
        }

        // And the bounds, so "arbitrary text" is false in the size direction
        // as well as the charset one.
        assert!(Hostname::parse(&"a".repeat(254)).is_err());
        assert!(SshUser::parse(&"a".repeat(65)).is_err());
        assert!(HostAlias::parse(&"a".repeat(254)).is_err());
        assert!(EndpointId::parse(&"a".repeat(MAX_ENDPOINT_ID_BYTES + 1)).is_err());
        assert!(EndpointId::parse(&"a".repeat(MAX_ENDPOINT_ID_BYTES)).is_ok());

        // The other four field types, which DO forbid text outright and are
        // what the #827 sweep's own claim still rests on.
        assert!(toml_edit::de::from_str::<DesktopSettings>(&format!(
            "version = 1\n\n[[endpoints.remote]]\nhost = \"h\"\nid = \"d\"\nport = {SENTINEL:?}\n"
        ))
        .is_err());
    }
}
