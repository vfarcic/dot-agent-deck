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
//! [`Transcript`]. Its default is a **keyless container on loopback**, so the
//! feature works on the day it ships without anyone pasting a credential and
//! the audio never leaves the machine; `off` was the default and is gone, along
//! with the second implementation behind it.
//!
//! **M5 brought the real backends, and PRD #802's provider work took one of
//! them away.** It shipped two: [`remote`], one keyed HTTPS request with a
//! constrained enum, and an agent-CLI backend that spawned the
//! pre-authenticated `claude` already on the machine. The second is **gone** —
//! Commands is API-only. A stage that spends a credential has to let the user
//! say whose, and a backend that spawns one vendor's general-purpose coding
//! agent on a prompt built partly from untrusted input could not offer that at
//! any price worth paying. [`remote`] is what remains, and it speaks **two
//! protocols** — Anthropic Messages here and OpenAI-compatible
//! chat-completions in [`openai`] — so the provider choice a key obliges is a
//! real one. [`resolver_for`] is where `VoiceSettings.intent` picks one. Every
//! [`VoiceResult`] carries the latency it cost, because PRD
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

pub mod capture;
pub mod choice;
mod command_text;
pub mod dictation;
mod filter;
pub mod hold;
pub mod http;
pub mod human_voice;
pub mod numbers;
pub mod openai;
pub mod outcome;
pub mod prompt;
pub mod remote;
pub mod resolver;
pub mod schema;
pub mod table;
pub mod transcribe;
pub mod wake;

#[cfg(test)]
#[path = "choice_tests.rs"]
mod choice_tests;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Live state, as the DTO the app already projects from the daemon.
///
/// Re-exported rather than mirrored: PRD #802 has no daemon-side change in it
/// at all, so the resolver reads the snapshot the agent overview already
/// renders. A parallel shape here would be a second answer to "what agents are
/// there" with nothing keeping the two in step.
pub use crate::dto::DesktopAgent;

/// One deck a spoken [`ParamKind::DeckRef`] can name (PRD #1223).
///
/// Built from the fleet the desktop already observes
/// (`dto::observed_fleet_decks`), never from the webview, for
/// [`DesktopAgent`]'s reason: a list arriving from the page would be a second
/// answer to "which decks are there". Resolution needs the key the frontend
/// dispatches with, the name the screen shows, and whether the deck is this
/// machine's, which is what makes the literal *"local"* name it. The fourth
/// field is whether the New agent dialog can preselect it at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceDeck {
    /// `EndpointIdentity::wire_id()` — the same `deckId` the overview keys its
    /// groups on and `openNewAgent` preselects.
    pub id: String,
    /// What the overview calls it: "Local deck", a remote deck's name in the
    /// shared deck list (issue #1426), or `user@host[:port]` for a remote deck
    /// with no usable name.
    pub label: String,
    /// A remote deck's `user@host[:port]` when [`Self::label`] is its name,
    /// so a deck is still found by its host ("switch to the build box")
    /// after it is named something else. `None` for the local deck and for a
    /// deck labelled by its address already.
    pub address: Option<String>,
    /// Whether it is the local endpoint.
    pub local: bool,
    /// Why this deck cannot take a new agent, as the short reason class the
    /// webview declares for it ("it is not connected") — or `None` when it
    /// can. The New agent dialog does not list such a deck (PR #1451 round 3),
    /// so this short class is what voice names it with there.
    ///
    /// **Taken from the webview's [`VoiceDeckChoice`] declaration**, the one
    /// piece of a deck that is not read here: the dialog decides what to
    /// preselect from the webview's fleet (`preselectedDeck` in
    /// `desktop/src/lib/newAgent.ts`), and a report that is to agree with the
    /// dialog has to be judged against the same list the dialog judges. A deck
    /// with a reason is shown to the model only among the decks a new agent
    /// cannot start on (`decks_without_new_agent`, issue #1491), and one
    /// resolved anyway for the New agent dialog is reported as unable to take
    /// the agent rather than as preselected.
    pub unavailable: Option<String>,
    /// Whether the agents a spoken `agent_ref` resolves against are this
    /// deck's (issue #1495) — the deck `get_snapshot` read them from, which is
    /// the selected deck, and this machine's under All daemons. It is what lets
    /// "the agent on build box" name the daemon an agent is on, and what refuses
    /// it when the agents voice can reach are on another one. At most one deck
    /// carries it; none does when that deck is not in the observed fleet.
    pub holds_agents: bool,
}

impl VoiceDeck {
    /// Whether the New agent dialog would preselect this deck if asked to.
    pub fn eligible(&self) -> bool {
        self.unavailable.is_none()
    }
}

/// One deck of the New agent dialog's deck step, as the webview DECLARED it
/// with an utterance (PRD #1223): a deck id and, for a deck that cannot take a
/// spawn, its short reason class (`deckChoices` in
/// `desktop/src/lib/newAgent.ts`). Every deck in the webview's fleet is
/// declared, the ones the dialog does not list included (PR #1451 round 3).
///
/// # It comes from the webview, and only annotates [`VoiceDeck`]
///
/// The DECKS are still read Rust-side; this adds nothing to that list and a
/// deck id it names that the fleet does not have is ignored. What it carries is
/// whether each deck is eligible, and that is a question about the dialog: the
/// dialog preselects from the webview's fleet, whose connection states and
/// fallback sentences (`deckUnavailableReason`) are computed there. Declared on
/// every utterance rather than only while the dialog is open, because
/// `open_new_agent` — the one row with a `deck_ref` — runs while it is closed.
///
/// It can only narrow what voice will preselect: a deck it marks eligible
/// that the dialog then refuses is still refused by the dialog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceDeckChoice {
    /// The wire `deckId`.
    pub deck_id: String,
    /// Why it cannot take a new agent, as a short reason class ("it is not
    /// connected"); absent when it can.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The short reason class for a deck the fleet observes but the webview did
/// not declare: `DECK_SHORT_REASON.pending` in `desktop/src/lib/newAgent.ts`,
/// because a deck the webview's fleet has no entry for is one that has not
/// reported to it yet.
pub const DECK_NOT_REPORTED: &str = "it has not reported yet";

/// What a deck the Deck selector lists but the app is not connected to says
/// about itself (PRD #1195 M3). Under a single-deck selection that is every
/// deck but the one shown, and the New agent dialog does not list them: a new
/// agent starts on a deck the app is talking to, so the way to one is to
/// switch to it first — which is what the sentence says.
pub const DECK_NOT_CONNECTED: &str = "the app is not connected to it; switch to it first";

/// The short reason class for a deck the Deck selector lists with no address
/// yet — `DECK_SHORT_REASON.unconfigured` in `desktop/src/lib/newAgent.ts`.
pub const DECK_NO_ADDRESS: &str = "it has no address yet";

/// Issue #1491 — the key voice gives the Deck selector's **All daemons**
/// entry among its decks. Never a fleet key: those are `deck-<16 hex>` or
/// `unconfigured-<row id>`, so it cannot collide with a deck the app observes.
/// A switch to it is addressed to the selector's `all` token
/// (`crate::settings::ALL_SELECTION_TOKEN`) like any other switch.
pub const ALL_DECKS_ID: &str = "all-daemons";

/// What the Deck selector calls that entry (`deckChoices` in
/// `desktop/src/lib/endpoints.ts`), so a report names it the way the screen
/// does: "Showing All daemons."
pub const ALL_DECKS_LABEL: &str = "All daemons";

/// Why All daemons cannot take a new agent: it is a selection rather than one
/// deck. It keeps it out of what the New agent dialog preselects — a deck
/// with a reason is listed to the model as one a new agent cannot start on,
/// and is never preselected — while `switch_deck`, which ignores the reason,
/// switches to it.
pub const DECK_IS_EVERY_DAEMON: &str = "it is every daemon at once; name one daemon";

/// What the New agent dialog's directory browser is showing, as the webview
/// DECLARED it for one utterance (PRD #1223) — the set a spoken
/// [`ParamKind::DirRef`] resolves against.
///
/// # It comes from the webview, unlike [`VoiceDeck`], and that is not a lapse
///
/// [`VoiceDeck`] and [`DesktopAgent`] are read Rust-side because that is where
/// they live, and a list from the page would be a second answer to a question
/// the snapshot already answers. The browser's listing has no Rust-side home at
/// all: it is `NewAgentDialog`'s component state — which deck the flow chose,
/// which directory it is looking at, and which of that directory's children the
/// filter leaves on screen — and the daemon lists one level per request and
/// remembers none of them. So it is exactly the kind of state the mounted
/// SCREEN is, and it travels the way the screen does: stated by the webview
/// immediately before the resolve (`DeckBridge.declareVoiceScreen`). Absent
/// means the dialog is closed, no deck is chosen, no listing has landed, or a
/// start is in flight — every case where there is nothing on screen to name —
/// and each `requires`-gated row is then `callable: false`.
///
/// Every path in it is one the deck returned (the dialog builds none), so a
/// dispatch sends back a path that came from the deck, by way of the page.
/// `lib.rs` bounds the declaration before it is used.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceDirectories {
    /// The flow's chosen deck — the wire `deckId` its listing came from.
    pub deck_id: String,
    /// The listing's own `path`: the directory on screen.
    pub path: String,
    /// Whether the listing has a parent, i.e. whether `..` is on screen.
    pub has_parent: bool,
    /// The children on screen, in the order the browser shows them — after
    /// the filter, because a spoken name means one the user can see. While
    /// the listing is split into pages (voice on, more children than fit),
    /// these are the CURRENT page's only.
    pub entries: Vec<VoiceDirectoryEntry>,
    /// The listing's pages, present only while it is split into them (PR
    /// #1451 round 3, change 4): which page is showing and the children on the
    /// others, so a name said for one of those is refused with the page it is
    /// on rather than as a name nothing matches.
    #[serde(default)]
    pub paging: Option<VoicePaging>,
}

/// A list split into pages while voice is on (PR #1451 round 3, change 4),
/// as the webview declared it: the page showing, and every item on another
/// page. Voice acts only on what is on screen, so an item here is never
/// resolved — it is named back with its page ([`VoiceOffPage`]).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoicePaging {
    /// The page showing, counted from 1.
    pub page: u32,
    /// The items on every other page, in list order.
    pub elsewhere: Vec<VoiceOffPage>,
}

/// One item of a paged list that is not on the page showing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceOffPage {
    /// The name the list renders for it, which is what a user says.
    pub name: String,
    /// The page it is on, counted from 1.
    pub page: u32,
}

/// One child directory on screen.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceDirectoryEntry {
    /// The `displayName` the browser renders, which is what a user says.
    pub name: String,
    /// The deck's own path for it — what `open_dir` dispatches with.
    pub path: String,
}

/// What the New agent dialog is showing BESIDES its directory browser, as the
/// webview declared it for one utterance (PRD #1223) — present exactly while the
/// dialog is mounted.
///
/// Declared by the webview for [`VoiceDirectories`]' reason, and separately
/// from it because the two are present at different times: the browser can be
/// empty (no deck chosen, no listing, a deck that cannot list) while the dialog
/// is open, and a spoken "start it" must be able to say THAT rather than be
/// refused as though no dialog were there.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceNewAgent {
    /// The form's live fields, present only while they can be changed: a deck
    /// and a directory chosen, no start in flight, and no start confirmation
    /// open. Absent, every fill row is `callable: false`.
    #[serde(default)]
    pub form: Option<VoiceNewAgentForm>,
}

/// The New agent form's closed sets, as they are ON SCREEN (PRD #1223).
///
/// **The chips and the picker entries are the ones the dialog actually
/// offers**, never a list this crate knows: the Mode row varies by the deck's
/// capabilities, the deck's experimental flag, and whether the chosen
/// directory is a project with orchestrations; the agent list is the deck's
/// own registry, or the desktop's labelled fallback for a deck that does not
/// report one. A spoken mode or agent type resolves against these and nothing
/// else, so a chip that is not offered is refused rather than guessed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceNewAgentForm {
    /// The deck the form is about — the wire `deckId` the flow captured.
    pub deck_id: String,
    /// The chosen directory, as the deck spelled it.
    pub path: String,
    /// The Mode chips offered, in the order the row shows them. A disabled
    /// namesake orchestration chip is NOT here: it cannot be chosen by a click
    /// either.
    pub modes: Vec<VoiceChoice>,
    /// The agents the deck offers, as the dialog declares them — its registry, or this app's fallback copy for a deck that reports none.
    pub agent_types: Vec<VoiceChoice>,
    /// The Mode chips the dialog KNOWS and withholds on this form — an
    /// authoring kind the deck cannot compose, or `schedule: issues` with the
    /// deck's experimental flag off. Never resolvable: they are here so that a
    /// user who names one is told it is not offered, instead of being handed
    /// the nearest chip that is (a model shown only the offered chips was
    /// measured substituting `schedule` for "schedule issues").
    #[serde(default)]
    pub withheld_modes: Vec<VoiceChoice>,
    /// The Mode row's pages, present only while it is split into them (PR
    /// #1451 round 3, change 4); `modes` is then the page showing. See
    /// [`VoicePaging`].
    #[serde(default)]
    pub mode_paging: Option<VoicePaging>,
}

/// The agent the voice panel is in the dictation mode for (PRD #1260), declared
/// with each utterance while the mode is on and absent otherwise.
///
/// The Rust side keeps no memory between utterances, so the mode travels in the
/// declaration exactly as the New agent dialog's state does. Its presence is
/// the whole signal: an utterance declared with one is classified locally
/// against the reserved phrases and otherwise typed whole, and nothing reaches
/// the Commands backend. The composite `{deck_id, agent_id}` rather than a bare
/// agent id, because an agent id is only unique within its deck.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceDictationTarget {
    pub deck_id: String,
    pub agent_id: String,
}

/// One entry of a closed set on screen: the id the dialog selects by, and the
/// label it renders — which is what a user says.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceChoice {
    pub id: String,
    pub label: String,
}
pub use dictation::{
    DICTATION_OFF_PHRASES, DICTATION_ON_PHRASES, DICTATION_OPENERS, SUBMIT_PHRASES,
    VOICE_OFF_PHRASES,
};

pub use capture::{
    AudioFormat, AudioSource, AudioStream, Capture, CaptureError, CaptureSession, CaptureState,
    CaptureStatus, CaptureTicket, CpalSource, MAX_UTTERANCE, MIN_SPEECH, Pcm16, PcmSink,
    SILENCE_HOLD, SILENCE_RMS, SPEECH_MARGIN, SPEECH_WINDOW, SpeechMeasure, StubSource,
    TARGET_SAMPLE_RATE, Vad,
};
pub use choice::{ChoiceAnswer, ChoiceLive, MAX_CHOICES};
pub use hold::VoiceHold;
pub use outcome::{
    ChoiceMatch, DeckRefMatch, DirRefMatch, FILTER_DASHBOARD_ROW, ResolvedParam, SWITCH_DECK_ROW,
    VoiceDeckIdentity, VoiceDeckSelection, VoiceOutcome, VoiceResult, address_deck_switch,
    handle_utterance, handle_utterance_with, handle_utterance_with_dictation,
    refuse_switch_beyond_selector, resolve_agent_type_ref, resolve_deck_ref, resolve_dir_ref,
    resolve_mode_ref,
};
pub use remote::{Protocol, REMOTE_TIMEOUT, RemoteResolver};
pub use resolver::{
    IntentAnswer, IntentError, IntentRequest, IntentResolver, StubResolver, resolver_for,
};
pub use schema::{
    AnnotatedCommand, AnnotatedParam, LABELS_WITHHELD_HINT, TOOL_INSTRUCTIONS, TOOL_NAME, annotate,
    annotate_for, annotate_with, needs_labels, tool_schema,
};
pub use table::{
    CommandRow, CommandTable, NO_MATCH_ACTION, ParamKind, ParamSpec, Requirement, Screen, table,
};
pub use transcribe::{
    HttpTranscriber, StubTranscriber, TRANSCRIBE_TIMEOUT, Transcriber, TranscriptionError,
    TranscriptionOutcome, VoiceTranscription, handle_audio, transcriber_for, unreachable_detail,
};
pub use wake::{
    SleepInhibit, SleepInhibitor, StubInhibitor, WAKE_WHO, WAKE_WHY, WakeCounts, WakeLock,
    platform_inhibitor,
};

/// What the user said, as text — from the microphone through the `Transcriber`
/// seam (M7), which since the M6 rewrite is the only thing that produces one.
///
/// **The rule this feature adopts: THIS APP writes no transcript, utterance or
/// audio buffer to a log or to disk — and where it hands one to a child
/// process, it instructs that child not to either.** That covers `eprintln!`,
/// which is how this crate logs today (`lib.rs` and `settings.rs` between them
/// are its only log calls, and neither `tracing` nor `log` is a dependency of
/// it), and it covers whatever replaces it. A transcript lives in memory for
/// the session's UI, and the one place it is meant to appear is the sentence
/// the app renders back to the user.
///
/// **The second clause is kept although nothing here hands a prompt to a child
/// process any more, and that is deliberate.** PRD #802's landed-work security
/// audit found the agent-CLI intent backend handing the whole prompt — the
/// utterance and the agent labels with it — to `claude` in its ordinary session
/// mode, which writes a resumable session to disk; redacted `Debug` impls do
/// nothing about a child application's own storage. That backend was contained
/// with `--no-session-persistence` and then **removed outright** by the
/// provider work, so today every intent backend is one HTTPS request to a model
/// with no tools and no session, and the first clause carries the whole claim.
/// The second stays as the rule a future backend inherits rather than has to
/// rediscover: the last one cost an audit round to notice.
///
/// It is a **rule**, not a property this module can assert. PRD #802's Open
/// Question 5 asks whether any part of an utterance is persisted; this is the
/// answer, and `docs/develop/desktop-gui.md` carries it in the same two halves.
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

    /// Whether the text holds a word at all — a letter or a digit — rather
    /// than being empty or only punctuation. Whisper-family models answer
    /// non-speech with runs of `...`, which is not something anybody said.
    pub fn has_words(&self) -> bool {
        self.0.chars().any(char::is_alphanumeric)
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

/// Snapshot-agent builders for tests, in the crate's own `src/` so there is ONE
/// construction site for [`DesktopAgent`]'s sixteen fields rather than one per
/// test module.
///
/// # Why this is `pub` when nothing in production calls it
///
/// `mod dto` is private, so `DesktopTab` and `DesktopActiveTool` are not
/// nameable from outside this crate — which means an **integration** test
/// (`tests/voice_phrase_fixtures.rs`, PRD #802 M9) cannot write a `DesktopAgent`
/// literal at all, however `pub` the struct itself is. The credentialed phrase
/// fixtures have to plant live state to prove a reference like *"the one that's
/// stuck"* resolves, and an integration test compiles against the lib with
/// `cfg(test)` **off**, so the `#[cfg(test)]` module this replaced was invisible
/// to it.
///
/// # How it is kept from becoming a production API
///
/// Three things, and the first is the one that matters:
///
/// - **nothing under `src/` may call it**, which is a rule rather than a
///   mechanism — but a cheap one to check (`grep -rn test_support src/`) and one
///   whose violation is obvious in review, since every caller here would be
///   building a fake agent in a crate whose whole job is projecting real ones;
/// - `#[doc(hidden)]`, so it is not offered to anybody reading the crate's docs;
/// - the name says it, at the call site as well as here.
///
/// **Deliberately NOT behind a cargo feature**, which is the tempting way to
/// keep it out of a normal build. This repository has closed that hole class
/// three times (#407, #436, #502) and `Cargo.toml`'s `cpal` entry states the
/// reason at length: code behind a feature nobody's gate enables is type-checked
/// by nothing and lints clean by compiling nothing. A `pub` module compiles and
/// lints on every gate, and a builder of five statements costs the binary
/// nothing worth measuring.
#[doc(hidden)]
pub mod test_support {
    use std::sync::Arc;

    use super::DesktopAgent;
    use crate::dto::{DesktopActiveTool, DesktopTab};
    use crate::secrets::{Secret, SecretError, SecretId, SecretStatus, SecretStore};
    use crate::settings::IntentSettings;

    /// The command backend a fresh install gets, with a credential the caller
    /// already holds.
    ///
    /// **It takes no coordinates and offers no switch**, deliberately: the
    /// phrase fixtures are authoritative for the shipping default, and a green
    /// run against another protocol, endpoint or model would prove less than it
    /// appears to. Everything but the credential comes from
    /// [`IntentSettings::default`] through [`super::resolver_for`] — the same
    /// call `crate::lib`'s voice command makes — so what the fixtures exercise
    /// is the construction path a user gets rather than a reconstruction of it.
    /// That is how the protocol stopped being hardwired here: this function
    /// named `Protocol::Anthropic` in its own body, and would have gone on
    /// naming it after the default moved.
    ///
    /// # Why an integration test cannot build one itself
    ///
    /// The same reason [`agent`] exists, one layer along: `IntentSettings`, the
    /// newtypes inside it and `SecretStore` all live in **private** modules
    /// (`settings`, `model_service`, `secrets`). `pub mod voice` is the crate's
    /// only public module, so `tests/voice_phrase_fixtures.rs` can name none of
    /// them however `pub` the items themselves are — and the in-memory store
    /// the unit tests drive is `#[cfg(test)]`, which an integration test
    /// compiles with off.
    ///
    /// # The key is passed in, never read from the environment here
    ///
    /// The caller names the variable it wants and reports honestly when it is
    /// absent. This function taking `key` rather than reading one keeps the
    /// credential's provenance at the call site, where the test's own
    /// preflight already is.
    ///
    /// The returned resolver reads that key through the same
    /// [`crate::secrets::load_off_runtime`] path production takes — the store is
    /// the double, and nothing else about the call differs.
    pub fn api_resolver(key: &str) -> Box<dyn super::IntentResolver> {
        super::resolver_for(
            &IntentSettings::default(),
            Arc::new(OneSecret(Secret::new(key))),
        )
    }

    /// This build's shipping coordinates for the command backend, as
    /// `(endpoint, model)`.
    ///
    /// Read off [`IntentSettings::default`] rather than off a named constant,
    /// so it reports whichever preset is the default rather than the one that
    /// was the default when this was written. The fixtures print it; nothing
    /// asserts on it.
    pub fn api_preset() -> (String, String) {
        let preset = IntentSettings::default();
        (
            preset.endpoint.as_str().to_string(),
            preset.model.as_str().to_string(),
        )
    }

    /// A [`SecretStore`] holding exactly one credential, in memory, for
    /// [`api_resolver`].
    ///
    /// Writes are accepted and discarded rather than refused: nothing in the
    /// resolve path writes, and a store that returned an error for a call it
    /// never receives would be inventing a failure mode to look thorough.
    struct OneSecret(Secret);

    impl SecretStore for OneSecret {
        fn store(&self, _id: SecretId, _secret: &Secret) -> Result<(), SecretError> {
            Ok(())
        }

        fn load(&self, _id: SecretId) -> Result<Option<Secret>, SecretError> {
            Ok(Some(self.0.clone()))
        }

        fn delete(&self, _id: SecretId) -> Result<(), SecretError> {
            Ok(())
        }

        fn status(&self, _id: SecretId) -> SecretStatus {
            SecretStatus {
                stored: true,
                problem: None,
            }
        }
    }

    /// A snapshot agent. Only the fields a spoken reference can reach are
    /// interesting; the rest are what the daemon would have reported.
    pub fn agent(id: &str, display_name: Option<&str>, agent_type: &str) -> DesktopAgent {
        DesktopAgent {
            id: id.to_string(),
            pane_id: None,
            display_name: display_name.map(str::to_string),
            cwd: None,
            rows: 24,
            cols: 80,
            agent_type: agent_type.to_string(),
            cli_name: None,
            prompt_keys: None,
            status: "running".to_string(),
            active_tool: None,
            tool_count: 0,
            last_user_prompt: None,
            write_lease: None,
            last_activity_ms: None,
            spawned_at_ms: None,
            blocked: None,
            authoring_kind: None,
            tab: DesktopTab::Dashboard,
        }
    }

    /// An agent the deck names by its orchestration ROLE, which is how a user
    /// refers to one out loud.
    pub fn role_agent(id: &str, role: &str) -> DesktopAgent {
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

    /// The same agent, in a named live state — what the M9 fixtures plant so a
    /// spoken reference like *"the one that's stuck"* has something to resolve
    /// against.
    ///
    /// `status` is a string rather than an enum because that is what the DTO
    /// carries: the daemon owns the vocabulary (`working`, `waiting_for_input`,
    /// `error`, …) and this crate copies it through. A test that plants a word
    /// no daemon emits is testing nothing, so pass one of the daemon's.
    pub fn role_agent_in_state(id: &str, role: &str, status: &str) -> DesktopAgent {
        let mut agent = role_agent(id, role);
        agent.status = status.to_string();
        agent
    }

    /// The same role agent as a member of the orchestration `id` — so several
    /// share one overview card, as a real run's roles do (PRD #1223).
    pub fn in_orchestration(mut agent: DesktopAgent, id: &str) -> DesktopAgent {
        if let DesktopTab::Orchestration {
            orchestration_id, ..
        } = &mut agent.tab
        {
            *orchestration_id = Some(id.to_string());
        }
        agent
    }

    /// The same role agent as a member of the orchestration `id`, running the
    /// config `name` under the run title `title` — the auto-generated
    /// `<basename>-orchestrator-N` a real run is headed with, which is what a
    /// user has to refer to on the overview (PRD #1223).
    pub fn in_titled_orchestration(
        mut agent: DesktopAgent,
        id: &str,
        name: &str,
        title: &str,
    ) -> DesktopAgent {
        if let DesktopTab::Orchestration {
            orchestration_id,
            name: config,
            display_title,
            ..
        } = &mut agent.tab
        {
            *orchestration_id = Some(id.to_string());
            *config = name.to_string();
            *display_title = Some(title.to_string());
        }
        agent
    }

    /// An agent with a tool running, for the other half of the live state the
    /// prompt carries.
    pub fn with_tool(mut agent: DesktopAgent, name: &str, detail: Option<&str>) -> DesktopAgent {
        agent.active_tool = Some(DesktopActiveTool {
            name: name.to_string(),
            detail: detail.map(str::to_string),
        });
        agent
    }

    /// Issue #1495 — agents whose labels say nothing about what they are
    /// doing, the way a dispatcher's own name does not say "dispatcher". Each
    /// is told apart only by a fact the deck holds beside the label: its mode,
    /// its agent type, its directory, its orchestration, its last prompt (read
    /// on this machine) or
    /// when it started.
    ///
    /// - **Mercury** runs in the `dispatcher` mode, in `dot-agent-deck`, and
    ///   started first.
    /// - **Juno** is the one Codex agent, in `billing`, and was last asked to
    ///   fix the scroll.
    /// - **Vega** is a second Claude Code agent beside Mercury, in
    ///   `docs-site`, and started last — the newest.
    /// - two OpenCode **reviewers**, one in the `prd-1487` run and one in the
    ///   `docs-1502` run, so "the reviewer" alone is a tie and the run's name
    ///   breaks it.
    pub fn facets_fleet() -> Vec<DesktopAgent> {
        const STARTED: i64 = 1_790_000_000_000;
        let named = |id: &str, name: &str, agent_type: &str, cli: &str, cwd: &str| {
            let mut agent = agent(id, Some(name), agent_type);
            agent.cli_name = Some(cli.to_string());
            agent.cwd = Some(cwd.to_string());
            agent.status = "working".to_string();
            agent
        };
        let mut mercury = named(
            "agent-mercury",
            "Mercury",
            "claude_code",
            "claude",
            "/home/dev/code/dot-agent-deck",
        );
        mercury.tab = DesktopTab::Mode {
            name: "dispatcher".to_string(),
        };
        mercury.status = "idle".to_string();
        mercury.spawned_at_ms = Some(STARTED);
        let mut juno = named(
            "agent-juno",
            "Juno",
            "codex",
            "codex",
            "/home/dev/code/billing",
        );
        juno.last_user_prompt =
            Some("Fix the scroll jump when the terminal pane resizes".to_string());
        juno.spawned_at_ms = Some(STARTED + 60_000);
        let mut vega = named(
            "agent-vega",
            "Vega",
            "claude_code",
            "claude",
            "/home/dev/code/docs-site",
        );
        vega.last_user_prompt = Some("Rewrite the install guide for Windows".to_string());
        vega.spawned_at_ms = Some(STARTED + 600_000);
        let reviewer = |id: &str, run: &str, config: &str, title: &str, cwd: &str, at: i64| {
            let mut agent = in_titled_orchestration(role_agent(id, "reviewer"), run, config, title);
            agent.agent_type = "open_code".to_string();
            agent.cli_name = Some("opencode".to_string());
            agent.cwd = Some(cwd.to_string());
            agent.spawned_at_ms = Some(at);
            if let DesktopTab::Orchestration { cwd: run_cwd, .. } = &mut agent.tab {
                *run_cwd = Some(cwd.to_string());
            }
            agent
        };
        vec![
            mercury,
            juno,
            vega,
            reviewer(
                "agent-review-1487",
                "orch-1487",
                "prd-review",
                "prd-1487",
                "/home/dev/code/dot-agent-deck-prd-1487",
                STARTED + 120_000,
            ),
            reviewer(
                "agent-review-docs",
                "orch-docs",
                "docs-review",
                "docs-1502",
                "/home/dev/code/handbook",
                STARTED + 180_000,
            ),
        ]
    }
}

/// The in-crate spelling of [`test_support`], so the test modules under `voice`
/// keep the import they had.
#[cfg(test)]
pub(crate) use test_support as fixtures;

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
