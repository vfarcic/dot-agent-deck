//! PRD #1666 part 3, round 5-wake: which agent releases the doorbell may type
//! into, decided from what was measured on each one.
//!
//! The doorbell's admission is to type a line into an agent's pane only when
//! the agent's screen is one the deck has measured: its empty composer, how it
//! echoes the line, which of its dialogs can appear after it is ready, and
//! what each key of the line does inside them. Those measurements belong to
//! one release of one agent, and nothing reads this table until that admission
//! is wired in. [`activation`] turns a generation's release (latched from its
//! start-up header, [`crate::release_scanner`]) and the recorded measurements
//! ([`RELEASE_RECORDS`]) into a verdict: [`Activation::Go`],
//! [`Activation::Inherited`] (Codex only, a release newer than the newest one
//! measured, within four minor versions), or [`Activation::NoGo`] with the
//! clause the card's `cannot_type` reason and the deck log name.
//!
//! The rules apply in order, and the first that matches decides:
//!
//! 1. the release is not latched → `unknown_release` (or the latch's own
//!    clause, `release_conflict`, `scanner_geometry` or `scanner_budget`);
//! 2. a record exists for exactly this release → that record decides, and it
//!    never falls through to inheritance ([`exact_verdict`]: the required
//!    measurements, then no first-stage byte committing in any measured
//!    dialog, then `head_echo` when the line is staged, then the settle cap);
//! 3. the agent is not Codex, the release is a pre-release, or no record is
//!    older → `unmeasured_release`;
//! 4. the release's tripwire is set → `release_tripwire`;
//! 5. the newest older record (the **source**) is not itself `Go` →
//!    `inherit_source_incomplete`;
//! 6. the source is another major version, or more than [`INHERIT_MINOR_SPAN`]
//!    minor versions older → `unmeasured_release`;
//! 7. the inherited settle window, `max(2 × T, 10 s)` with `T` the slowest
//!    verified first-dialog time of any record of the agent, exceeds
//!    [`SETTLE_CAP`] → `settle_over_cap`, never clamped;
//! 8. otherwise `Inherited` from the source.
//!
//! Nothing here reads a screen or a clock: it is a pure function of its inputs,
//! so the doorbell can ask it at every admission and `inbox/doorbell/053`
//! can enumerate it.
//!
//! A record's `editor_probe` is not one of its inputs. It belongs to the
//! composer probe (`crate::composer_probe::permission`), which may check a
//! release's input box only when it is verified; no rule above reads it.

use std::time::Duration;

use semver::Version;

use crate::event::AgentType;

/// The longest first-ring settle window the deck accepts (plan §4.4). A
/// measured or inherited window above it is `settle_over_cap`, never clamped:
/// the hold clock pauses inside the window, and the cap keeps that pause from
/// hiding a hold for long.
pub const SETTLE_CAP: Duration = Duration::from_secs(30);

/// The shortest settle window an inherited release gets (plan §4.4, Q13).
pub const INHERITED_SETTLE_FLOOR: Duration = Duration::from_secs(10);

/// How many minor versions past its source an inherited Codex release may be
/// (Q13, K = 4). A patch release of the source is at distance 0.
pub const INHERIT_MINOR_SPAN: u64 = 4;

/// The state of one measurement in a release's record.
///
/// Anything but [`Measurement::Verified`] in a required field keeps the
/// release from being typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measurement<T> {
    /// Never measured for this release.
    Absent,
    /// Measured, but not to the standard the record requires (fewer trials,
    /// a missed trial, or a result nobody checked yet).
    Unverified,
    /// Measured, and the measurement failed its gate (for example 19 of 20
    /// trials). Recorded so the release still has its own row, which keeps an
    /// older record from standing in for it.
    Failed,
    /// The situation the measurement needs could not be produced for this
    /// release (Codex's update dialog that seeding did not raise). The release
    /// is `unmeasured_release`; there is no source-review exception.
    Unraisable,
    /// Measured and verified.
    Verified(T),
}

impl<T> Measurement<T> {
    pub fn verified(&self) -> Option<&T> {
        match self {
            Measurement::Verified(value) => Some(value),
            _ => None,
        }
    }
}

/// What one key does inside one dialog (plan rev 3 §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEffect {
    /// No visible effect.
    Inert,
    /// Changes the highlight or focus; commits nothing.
    Moves,
    /// Types into a text field of the dialog.
    Edits,
    /// Closes the dialog with no action.
    Dismisses,
    /// Selects or confirms an action.
    Commits,
}

/// The key effects measured in one post-ready dialog of one release: every
/// key the doorbell line can contain, plus the delete key and CR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogKeys<'a> {
    /// The dialog, by the name the measurements use (`update`, `trust`,
    /// `hooks`).
    pub dialog: &'a str,
    /// One entry per key byte measured.
    pub keys: &'a [(u8, KeyEffect)],
}

impl DialogKeys<'_> {
    fn effect(&self, key: u8) -> Option<KeyEffect> {
        self.keys.iter().find(|(k, _)| *k == key).map(|(_, e)| *e)
    }
}

/// When the first post-ready dialog appeared, over the launches measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstModal {
    /// No post-ready dialog appeared in any of `launches`.
    NoneObserved { launches: u32 },
    /// The slowest time from the settle anchor to the first byte of a
    /// post-ready dialog over `launches`.
    Slowest { launches: u32, max: Duration },
}

impl FirstModal {
    fn time(&self) -> Duration {
        match self {
            FirstModal::NoneObserved { .. } => Duration::ZERO,
            FirstModal::Slowest { max, .. } => *max,
        }
    }
}

/// Where an agent writes the file its external-editor action exports, and
/// how it hands that file to the editor (the argument rule): the shape an
/// invocation must have to be the composer's own (Q35 core §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportShape {
    /// Claude Code: a file under the validated `<TMPDIR>/claude-<uid>/`,
    /// passed as the editor's last argument.
    ClaudeTmpdir,
    /// Codex: `<CODEX_HOME>/editor/.tmpXXXXXX.md`, passed as the editor's last
    /// argument.
    CodexEditorDir,
}

/// How an agent chooses the user's editor for its own external-editor
/// action, which the deck's helper reproduces when the invocation is the
/// user's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSelection {
    /// A truthy `VISUAL`, else a truthy `EDITOR`, else the first of `code`,
    /// `vi` and `nano` on `PATH`, split on literal spaces (M22, M23).
    ClaudeCode,
    /// A set, non-empty `VISUAL`, else `EDITOR`, a set but empty one being an
    /// error, split by shell-words rules (M34).
    Codex,
}

/// The bracketed-paste mode writes an agent makes around one editor
/// invocation: `ESC[?2004l` before it, `ESC[?2004h` after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteToggles {
    pub disables: u8,
    pub enables: u8,
}

/// A source an agent reads its key bindings from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapSource {
    /// Claude Code's `keybindings.json`.
    ClaudeKeybindings,
    /// `$CODEX_HOME/config.toml`.
    CodexConfig,
    /// The selected profile's `$CODEX_HOME/<name>.config.toml`.
    CodexProfile,
    /// A trusted project's `.codex/config.toml`.
    CodexProject,
    /// A `-c key=value` on Codex's command line.
    CodexOverride,
}

/// What one release's external-editor action was measured to do (Q35-11):
/// the composer probe's record, kept in the release's [`ReleaseRecord`].
///
/// It is an observer capability only. The ring's verdict ([`exact_verdict`],
/// [`activation`]) never reads it, so a missing one turns no ring off and a
/// verified one turns no ring on; it decides only whether the deck may check
/// the release's input box (`crate::composer_probe::permission`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorProbe<'a> {
    /// The export's location and argument rule.
    pub export: ExportShape,
    /// The user's-editor selection the helper reproduces.
    pub selection: EditorSelection,
    /// The `?2004` writes per invocation.
    pub paste_toggles: PasteToggles,
    /// Every source the release reads its key bindings from, in precedence
    /// order.
    pub keymap_sources: &'a [KeymapSource],
    /// P-KB1 (Claude Code, M49b): the release loads its key bindings before
    /// its first ready frame, so a file unchanged since spawn is the binding
    /// it loaded. Codex has no such premise and leaves it absent.
    pub keymap_loaded_at_start: Measurement<()>,
    /// The candidate forms of the probe's key measured inert in a composer
    /// holding text (M44e).
    pub inert_candidates: &'a [&'a [u8]],
    /// P-PASTE (M44e): with bracketed paste on, the release treats a pasted
    /// `0x07` as text.
    pub pasted_bel_is_text: Measurement<()>,
}

/// Everything measured for one release of one agent (plan §4.6).
///
/// The required fields decide whether the release can be typed into at all;
/// the capability fields only turn a capability off; `editor_probe` belongs
/// to the composer probe and not to the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRecord<'a> {
    pub agent: AgentType,
    pub release: Version,
    /// The screen profile whose recognisers this release's measurements
    /// verified ([`crate::doorbell_screen`]).
    pub profile: &'a str,
    // Required.
    pub ready_composer: Measurement<()>,
    pub echo: Measurement<()>,
    pub submit_route: Measurement<()>,
    pub layout: Measurement<()>,
    pub key_effects: Measurement<&'a [DialogKeys<'a>]>,
    pub first_modal: Measurement<FirstModal>,
    pub input_mode: Measurement<()>,
    pub release_recogniser: Measurement<()>,
    pub parser_cost: Measurement<()>,
    // Required when the line is staged (Q27): the token head alone, typed in
    // one write, echoed exactly in the composer within the echo wait, and the
    // rest typed after that echo, submitted and withdrawn as the transaction
    // does it. A release whose line is not staged never types a head alone,
    // so its record needs none.
    pub head_echo: Measurement<()>,
    // Capability only.
    pub placeholder_empty: Measurement<()>,
    pub delete_key: Measurement<()>,
    pub transcript_echo: Measurement<()>,
    // Observer capability only: the verdict never reads it.
    pub editor_probe: Measurement<EditorProbe<'a>>,
}

/// Why a generation is not typed into. [`NoGo::clause`] is the word the deck
/// log and the card's `cannot_type` reason carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoGo {
    /// No release was latched from the start-up header.
    UnknownRelease,
    /// Two different releases appeared in the start-up header window.
    ReleaseConflict,
    /// The pane was too large for the release scanner when it started.
    ScannerGeometry,
    /// The release scanner was dropped at its work budget before it saw the
    /// header (Q25).
    ScannerBudget,
    /// The release's record lacks a required measurement.
    MissingMeasurement(&'static str),
    /// The release's record has a required measurement that is unverified or
    /// failed.
    UnverifiedMeasurement(&'static str),
    /// No record covers this release, or the dialog a measurement needs could
    /// not be raised on it.
    UnmeasuredRelease,
    /// A byte of the line's first stage selects or confirms an action in one
    /// of the release's dialogs.
    FirstStageCommits { dialog: String, key: u8 },
    /// The release's first-ring settle window is longer than [`SETTLE_CAP`].
    SettleOverCap,
    /// A dialog was recognised on this inherited release since the daemon
    /// started.
    ReleaseTripwire,
    /// The newest older record, which an inherited release would take its
    /// profile from, is not complete.
    InheritSourceIncomplete,
}

impl NoGo {
    pub fn clause(&self) -> &'static str {
        match self {
            NoGo::UnknownRelease => "unknown_release",
            NoGo::ReleaseConflict => "release_conflict",
            NoGo::ScannerGeometry => "scanner_geometry",
            NoGo::ScannerBudget => "scanner_budget",
            NoGo::MissingMeasurement(_) => "missing_measurement",
            NoGo::UnverifiedMeasurement(_) => "unverified_measurement",
            NoGo::UnmeasuredRelease => "unmeasured_release",
            NoGo::FirstStageCommits { .. } => "first_stage_commits",
            NoGo::SettleOverCap => "settle_over_cap",
            NoGo::ReleaseTripwire => "release_tripwire",
            NoGo::InheritSourceIncomplete => "inherit_source_incomplete",
        }
    }
}

/// The verdict for one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation<'a> {
    /// The release has its own complete record.
    Go {
        profile: &'a str,
        /// The line is typed in stages: a byte after the first stage selects
        /// or confirms an action in one of the release's dialogs.
        staged: bool,
        settle: Duration,
    },
    /// A Codex release newer than the newest one measured, typed into under
    /// that record's profile (Q13).
    Inherited {
        from: Version,
        profile: &'a str,
        staged: bool,
        settle: Duration,
        /// The record whose first-dialog time set the settle window, if any
        /// record carries one.
        settle_from: Option<Version>,
    },
    NoGo(NoGo),
}

/// A generation's release, as its start-up header latched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatchedRelease {
    Known(Version),
    /// No header was seen before the window closed.
    Unseen,
    /// Two different headers were seen.
    Conflict,
    /// The scanner was not built or was dropped for its geometry before a
    /// header was seen.
    ScannerGeometry,
    /// The scanner was dropped at its work budget before a header was seen
    /// (Q25).
    ScannerBudget,
}

/// Every byte the line's first stage can contain (Q27): the token head
/// `(xxxxxx)`, its parentheses and [`crate::doorbell_txn::TOKEN_ALPHABET`].
/// The token's letters are checked against each record's dialogs like the
/// parentheses, so a record in which one of them commits is
/// `first_stage_commits` rather than typed into on a promise.
pub fn first_stage_bytes() -> impl Iterator<Item = u8> {
    b"()"
        .iter()
        .chain(crate::doorbell_txn::TOKEN_ALPHABET.iter())
        .copied()
}

/// The bytes after the first stage: [`crate::doorbell::LINE_PREFIX`], a
/// message id's alphabet (`m-`, base-32 letters and digits, `-`, hex digits)
/// and [`crate::doorbell::LINE_SUFFIX`].
fn later_stage_bytes() -> impl Iterator<Item = u8> {
    crate::doorbell::LINE_PREFIX
        .bytes()
        .chain(b'a'..=b'z')
        .chain(b'0'..=b'9')
        .chain(std::iter::once(b'-'))
        .chain(crate::doorbell::LINE_SUFFIX.bytes())
}

/// Rule 2 for one record: `Go` when every required measurement is verified,
/// no first-stage byte commits in any of its dialogs, `head_echo` is verified
/// if a later-stage byte commits in one (the line is then staged), and its own
/// settle window is within the cap; the first failing requirement otherwise.
pub fn exact_verdict<'a>(record: &ReleaseRecord<'a>) -> Activation<'a> {
    fn need<T>(m: &Measurement<T>, name: &'static str) -> Result<(), NoGo> {
        match m {
            Measurement::Verified(_) => Ok(()),
            Measurement::Absent => Err(NoGo::MissingMeasurement(name)),
            Measurement::Unverified | Measurement::Failed => Err(NoGo::UnverifiedMeasurement(name)),
            Measurement::Unraisable => Err(NoGo::UnmeasuredRelease),
        }
    }
    let checks = [
        need(&record.ready_composer, "ready_composer"),
        need(&record.echo, "echo"),
        need(&record.submit_route, "submit_route"),
        need(&record.layout, "layout"),
        need(&record.key_effects, "key_effects"),
        need(&record.first_modal, "first_modal"),
        need(&record.input_mode, "input_mode"),
        need(&record.release_recogniser, "release_recogniser"),
        need(&record.parser_cost, "parser_cost"),
    ];
    if let Some(Err(no_go)) = checks.into_iter().find(Result::is_err) {
        return Activation::NoGo(no_go);
    }
    let dialogs = record.key_effects.verified().copied().unwrap_or_default();
    for dialog in dialogs {
        for key in first_stage_bytes() {
            if dialog.effect(key) == Some(KeyEffect::Commits) {
                return Activation::NoGo(NoGo::FirstStageCommits {
                    dialog: dialog.dialog.to_string(),
                    key,
                });
            }
        }
    }
    let staged = dialogs
        .iter()
        .any(|d| later_stage_bytes().any(|key| d.effect(key) == Some(KeyEffect::Commits)));
    if staged && let Err(no_go) = need(&record.head_echo, "head_echo") {
        return Activation::NoGo(no_go);
    }
    let first_modal = record
        .first_modal
        .verified()
        .map(FirstModal::time)
        .unwrap_or_default();
    let settle = first_modal + first_modal / 2;
    if settle > SETTLE_CAP {
        return Activation::NoGo(NoGo::SettleOverCap);
    }
    Activation::Go {
        profile: record.profile,
        staged,
        settle,
    }
}

/// The verdict for a generation of `agent` whose header latched `release`,
/// given the recorded measurements and the tripwires set since the daemon
/// started. See the module documentation for the rule order.
pub fn activation<'a>(
    records: &[ReleaseRecord<'a>],
    agent: &AgentType,
    release: &LatchedRelease,
    tripwires: &[(AgentType, Version)],
) -> Activation<'a> {
    // Rule 1.
    let release = match release {
        LatchedRelease::Known(release) => release,
        LatchedRelease::Unseen => return Activation::NoGo(NoGo::UnknownRelease),
        LatchedRelease::Conflict => return Activation::NoGo(NoGo::ReleaseConflict),
        LatchedRelease::ScannerGeometry => return Activation::NoGo(NoGo::ScannerGeometry),
        LatchedRelease::ScannerBudget => return Activation::NoGo(NoGo::ScannerBudget),
    };
    let own: Vec<&ReleaseRecord<'a>> = records.iter().filter(|r| r.agent == *agent).collect();
    // Rule 2: an exact record decides, complete or not.
    if let Some(record) = own.iter().find(|r| r.release == *release) {
        return exact_verdict(record);
    }
    // Rule 3.
    let source = own
        .iter()
        .filter(|r| r.release < *release)
        .max_by(|a, b| a.release.cmp(&b.release));
    let Some(source) = source else {
        return Activation::NoGo(NoGo::UnmeasuredRelease);
    };
    if *agent != AgentType::Codex || !release.pre.is_empty() {
        return Activation::NoGo(NoGo::UnmeasuredRelease);
    }
    // Rule 4.
    if tripwires.iter().any(|(a, r)| a == agent && r == release) {
        return Activation::NoGo(NoGo::ReleaseTripwire);
    }
    // Rule 5: the newest older record, and never one past it.
    let Activation::Go {
        profile, staged, ..
    } = exact_verdict(source)
    else {
        return Activation::NoGo(NoGo::InheritSourceIncomplete);
    };
    // Rule 6.
    if source.release.major != release.major
        || release.minor.saturating_sub(source.release.minor) > INHERIT_MINOR_SPAN
    {
        return Activation::NoGo(NoGo::UnmeasuredRelease);
    }
    // Rule 7: the timing set is every record of the agent with a verified
    // first-dialog time, whatever its release and whether or not the record is
    // otherwise complete.
    let slowest = own
        .iter()
        .filter_map(|r| r.first_modal.verified().map(|m| (m.time(), &r.release)))
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let (t, settle_from) = match slowest {
        Some((t, from)) if t > Duration::ZERO => (t, Some(from.clone())),
        _ => (Duration::ZERO, None),
    };
    let settle = (t * 2).max(INHERITED_SETTLE_FLOOR);
    if settle > SETTLE_CAP {
        return Activation::NoGo(NoGo::SettleOverCap);
    }
    // Rule 8.
    Activation::Inherited {
        from: source.release.clone(),
        profile,
        staged,
        settle,
        settle_from,
    }
}

/// The releases of `agent` the deck has records for, oldest first: what the
/// `doorbell attempt` log line names for an `unmeasured_release`, so a user can
/// see which versions get the typed doorbell.
pub fn measured_releases(records: &[ReleaseRecord<'_>], agent: &AgentType) -> Vec<Version> {
    let mut releases: Vec<Version> = records
        .iter()
        .filter(|r| r.agent == *agent)
        .map(|r| r.release.clone())
        .collect();
    releases.sort();
    releases
}

/// The variable the e2e build reads for [`unrecorded_claude_releases`].
pub const UNRECORD_CLAUDE_RELEASES_ENV: &str = "DOT_AGENT_DECK_E2E_UNRECORD_CLAUDE_RELEASES";

/// Parse a comma-separated list of bare Claude Code releases (`2.1.310`,
/// `2.1.311`): no pre-release, no build metadata, no empty entry. Any entry
/// that is not one refuses the whole list.
pub fn parse_unrecorded_claude_releases(list: &str) -> Result<Vec<Version>, String> {
    if list.trim().is_empty() {
        return Ok(Vec::new());
    }
    list.split(',')
        .map(|entry| {
            let entry = entry.trim();
            match Version::parse(entry) {
                Ok(v) if v.pre.is_empty() && v.build.is_empty() => Ok(v),
                _ => Err(format!("not a bare release: {entry:?}")),
            }
        })
        .collect()
}

/// The Claude Code releases an e2e run asks the deck to treat as unrecorded
/// (`inbox/claude/050` names the installed one, so whatever it is, the deck
/// has no record for it). A malformed list excludes nothing and is logged as
/// `record exclusion refused`.
#[cfg(feature = "e2e")]
pub fn unrecorded_claude_releases() -> Vec<Version> {
    let Ok(list) = std::env::var(UNRECORD_CLAUDE_RELEASES_ENV) else {
        return Vec::new();
    };
    match parse_unrecorded_claude_releases(&list) {
        Ok(releases) => releases,
        Err(why) => {
            tracing::warn!(target: "doorbell", %why, "record exclusion refused");
            Vec::new()
        }
    }
}

/// The production build reads no variable and excludes nothing.
#[cfg(not(feature = "e2e"))]
pub fn unrecorded_claude_releases() -> Vec<Version> {
    Vec::new()
}

/// `records` without the **Claude Code** records for `unrecorded`. A record of
/// any other agent is kept whatever its version string, so the exclusion can
/// only turn a Claude Code release's own verdict into rule 3's
/// `unmeasured_release`: Claude Code never inherits, and no Claude Code record
/// feeds another release's verdict.
pub fn without_unrecorded_claude<'a>(
    records: &[ReleaseRecord<'a>],
    unrecorded: &[Version],
) -> Vec<ReleaseRecord<'a>> {
    records
        .iter()
        .filter(|r| !(r.agent == AgentType::ClaudeCode && unrecorded.contains(&r.release)))
        .cloned()
        .collect()
}

/// The variable the e2e build reads for [`assumed_codex_head_echo`].
pub const ASSUME_CODEX_HEAD_ECHO_ENV: &str = "DOT_AGENT_DECK_E2E_ASSUME_CODEX_HEAD_ECHO";

/// PRD #1666 part 3, Q28: the Codex releases whose `head_echo` an e2e run
/// asks the deck to treat as verified, so a lane-1 test can drive an
/// activated Codex before the records say so (the Q27 Go change). A
/// malformed list assumes nothing and is logged as `head echo assumption
/// refused`.
#[cfg(feature = "e2e")]
pub fn assumed_codex_head_echo() -> Vec<Version> {
    let Ok(list) = std::env::var(ASSUME_CODEX_HEAD_ECHO_ENV) else {
        return Vec::new();
    };
    match parse_unrecorded_claude_releases(&list) {
        Ok(releases) => releases,
        Err(why) => {
            tracing::warn!(target: "doorbell", %why, "head echo assumption refused");
            Vec::new()
        }
    }
}

/// The production build reads no variable and assumes nothing.
#[cfg(not(feature = "e2e"))]
pub fn assumed_codex_head_echo() -> Vec<Version> {
    Vec::new()
}

/// `records` with an absent `head_echo` read as verified on the **Codex**
/// records for `assumed`. Every other field, and every other agent's record,
/// is kept as it is, so the assumption can only turn a Codex release whose
/// one missing measurement is `head_echo` into its measured verdict.
pub fn with_assumed_codex_head_echo<'a>(
    records: Vec<ReleaseRecord<'a>>,
    assumed: &[Version],
) -> Vec<ReleaseRecord<'a>> {
    records
        .into_iter()
        .map(|mut r| {
            if r.agent == AgentType::Codex
                && assumed.contains(&r.release)
                && r.head_echo == Measurement::Absent
            {
                r.head_echo = Measurement::Verified(());
            }
            r
        })
        .collect()
}

/// The variable the e2e build reads for [`assumed_editor_probes`].
pub const ASSUME_EDITOR_PROBE_ENV: &str = "DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE";

/// Parse a comma-separated list of exact `agent@release` entries (`claude`
/// or `codex`, then a bare release) against `records`: an entry is known only
/// when `records` has that agent's record for exactly that release. Any entry
/// that is not known refuses the whole list, as does an entry named twice.
/// The result names records that exist; it never carries a release a
/// generation has not latched itself.
pub fn parse_assumed_editor_probes(
    list: &str,
    records: &[ReleaseRecord<'_>],
) -> Result<Vec<(AgentType, Version)>, String> {
    if list.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(AgentType, Version)> = Vec::new();
    for entry in list.split(',') {
        let entry = entry.trim();
        let unknown = || format!("not a recorded agent@release: {entry:?}");
        let (agent, release) = entry.split_once('@').ok_or_else(unknown)?;
        let agent = match agent {
            "claude" => AgentType::ClaudeCode,
            "codex" => AgentType::Codex,
            _ => return Err(unknown()),
        };
        // A pre-release or build is refused here too: a record's release is
        // bare, and two versions are equal only with equal metadata.
        let release = Version::parse(release).map_err(|_| unknown())?;
        if !records
            .iter()
            .any(|r| r.agent == agent && r.release == release)
        {
            return Err(unknown());
        }
        if entries.contains(&(agent.clone(), release.clone())) {
            return Err(format!("named twice: {entry:?}"));
        }
        entries.push((agent, release));
    }
    Ok(entries)
}

/// PRD #1666 part 3, Q35-11: the recorded releases an e2e run asks the deck
/// to treat as having a verified `editor_probe`, each with the record the
/// e2e stand-ins follow ([`crate::doorbell_screen::standin_editor_probe`]).
/// It substitutes a record, never a release: a generation still needs its own
/// header to latch one of them. A list with an unknown entry assumes nothing
/// and is logged as `editor probe assumption refused`. The daemon reads it
/// once, at start (`crate::composer_probe::install::capture_assumption`).
#[cfg(feature = "e2e")]
pub fn assumed_editor_probes() -> Vec<(AgentType, Version, EditorProbe<'static>)> {
    read_assumed_editor_probes().unwrap_or_else(|why| {
        tracing::warn!(target: "doorbell", %why, "editor probe assumption refused");
        Vec::new()
    })
}

/// [`assumed_editor_probes`], with a refused list as its reason: the
/// variable unset or empty is no assumption.
#[cfg(feature = "e2e")]
pub fn read_assumed_editor_probes()
-> Result<Vec<(AgentType, Version, EditorProbe<'static>)>, String> {
    let Ok(list) = std::env::var(ASSUME_EDITOR_PROBE_ENV) else {
        return Ok(Vec::new());
    };
    Ok(parse_assumed_editor_probes(&list, &RELEASE_RECORDS)?
        .into_iter()
        .filter_map(|(agent, release)| {
            let probe = crate::doorbell_screen::standin_editor_probe(&agent)?;
            Some((agent, release, probe))
        })
        .collect())
}

/// The production build reads no variable and assumes nothing.
#[cfg(not(feature = "e2e"))]
pub fn assumed_editor_probes() -> Vec<(AgentType, Version, EditorProbe<'static>)> {
    Vec::new()
}

/// `records` with `editor_probe` set to the assumed record on each record
/// `assumed` names by agent and exact release. Every other field, and every
/// record not named, is kept as it is, and no record is added, so a release
/// without a record of its own stays without one.
pub fn with_assumed_editor_probes<'a>(
    records: Vec<ReleaseRecord<'a>>,
    assumed: &[(AgentType, Version, EditorProbe<'a>)],
) -> Vec<ReleaseRecord<'a>> {
    records
        .into_iter()
        .map(|mut r| {
            if let Some((_, _, probe)) = assumed
                .iter()
                .find(|(agent, release, _)| *agent == r.agent && *release == r.release)
            {
                r.editor_probe = Measurement::Verified(*probe);
            }
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The recorded measurements (M1, M2; 2026-10-24). Each record states its
// sample counts and figures; the raw captures 036 replays are under
// `tests/fixtures/{codex,claude}-inbox/r5wake/`.
// ---------------------------------------------------------------------------

// Every key the doorbell line can contain, plus DEL and CR, in each Codex
// post-ready dialog: one fresh launch per key, the dialog raised, the key
// sent, the screen and Codex's state files compared 1.5 s later. Selecting
// any choice, or leaving for another screen, is `Commits`.
const CODEX_0164_UPDATE_KEYS: &[(u8, KeyEffect)] = &[
    (b'\r', KeyEffect::Commits),
    (b' ', KeyEffect::Inert),
    (b'(', KeyEffect::Inert),
    (b')', KeyEffect::Inert),
    (b',', KeyEffect::Inert),
    (b'-', KeyEffect::Inert),
    (b'0', KeyEffect::Inert),
    (b'1', KeyEffect::Commits),
    (b'2', KeyEffect::Commits),
    (b'3', KeyEffect::Commits),
    (b'4', KeyEffect::Inert),
    (b'5', KeyEffect::Inert),
    (b'6', KeyEffect::Inert),
    (b'7', KeyEffect::Inert),
    (b'8', KeyEffect::Inert),
    (b'9', KeyEffect::Inert),
    (b':', KeyEffect::Inert),
    (b'a', KeyEffect::Inert),
    (b'b', KeyEffect::Inert),
    (b'c', KeyEffect::Inert),
    (b'd', KeyEffect::Inert),
    (b'e', KeyEffect::Inert),
    (b'f', KeyEffect::Inert),
    (b'g', KeyEffect::Inert),
    (b'h', KeyEffect::Inert),
    (b'i', KeyEffect::Inert),
    (b'j', KeyEffect::Moves),
    (b'k', KeyEffect::Moves),
    (b'l', KeyEffect::Inert),
    (b'm', KeyEffect::Inert),
    (b'n', KeyEffect::Inert),
    (b'o', KeyEffect::Inert),
    (b'p', KeyEffect::Inert),
    (b'q', KeyEffect::Inert),
    (b'r', KeyEffect::Inert),
    (b's', KeyEffect::Inert),
    (b't', KeyEffect::Inert),
    (b'u', KeyEffect::Inert),
    (b'v', KeyEffect::Inert),
    (b'w', KeyEffect::Inert),
    (b'x', KeyEffect::Inert),
    (b'y', KeyEffect::Inert),
    (b'z', KeyEffect::Inert),
    (0x7f, KeyEffect::Inert),
];
const CODEX_0164_TRUST_KEYS: &[(u8, KeyEffect)] = &[
    (b'\r', KeyEffect::Commits),
    (b' ', KeyEffect::Inert),
    (b'(', KeyEffect::Inert),
    (b')', KeyEffect::Inert),
    (b',', KeyEffect::Inert),
    (b'-', KeyEffect::Inert),
    (b'0', KeyEffect::Inert),
    (b'1', KeyEffect::Inert),
    (b'2', KeyEffect::Dismisses),
    (b'3', KeyEffect::Inert),
    (b'4', KeyEffect::Inert),
    (b'5', KeyEffect::Inert),
    (b'6', KeyEffect::Inert),
    (b'7', KeyEffect::Inert),
    (b'8', KeyEffect::Inert),
    (b'9', KeyEffect::Inert),
    (b':', KeyEffect::Inert),
    (b'a', KeyEffect::Inert),
    (b'b', KeyEffect::Inert),
    (b'c', KeyEffect::Inert),
    (b'd', KeyEffect::Inert),
    (b'e', KeyEffect::Inert),
    (b'f', KeyEffect::Inert),
    (b'g', KeyEffect::Inert),
    (b'h', KeyEffect::Inert),
    (b'i', KeyEffect::Inert),
    (b'j', KeyEffect::Moves),
    (b'k', KeyEffect::Inert),
    (b'l', KeyEffect::Inert),
    (b'm', KeyEffect::Inert),
    (b'n', KeyEffect::Commits),
    (b'o', KeyEffect::Inert),
    (b'p', KeyEffect::Inert),
    (b'q', KeyEffect::Commits),
    (b'r', KeyEffect::Inert),
    (b's', KeyEffect::Inert),
    (b't', KeyEffect::Inert),
    (b'u', KeyEffect::Inert),
    (b'v', KeyEffect::Inert),
    (b'w', KeyEffect::Inert),
    (b'x', KeyEffect::Inert),
    (b'y', KeyEffect::Inert),
    (b'z', KeyEffect::Inert),
    (0x7f, KeyEffect::Inert),
];
const CODEX_0164_HOOKS_KEYS: &[(u8, KeyEffect)] = &[
    (b'\r', KeyEffect::Commits),
    (b' ', KeyEffect::Inert),
    (b'(', KeyEffect::Inert),
    (b')', KeyEffect::Inert),
    (b',', KeyEffect::Inert),
    (b'-', KeyEffect::Inert),
    (b'0', KeyEffect::Inert),
    (b'1', KeyEffect::Commits),
    (b'2', KeyEffect::Moves),
    (b'3', KeyEffect::Commits),
    (b'4', KeyEffect::Inert),
    (b'5', KeyEffect::Inert),
    (b'6', KeyEffect::Inert),
    (b'7', KeyEffect::Inert),
    (b'8', KeyEffect::Inert),
    (b'9', KeyEffect::Inert),
    (b':', KeyEffect::Inert),
    (b'a', KeyEffect::Inert),
    (b'b', KeyEffect::Inert),
    (b'c', KeyEffect::Inert),
    (b'd', KeyEffect::Inert),
    (b'e', KeyEffect::Inert),
    (b'f', KeyEffect::Inert),
    (b'g', KeyEffect::Inert),
    (b'h', KeyEffect::Inert),
    (b'i', KeyEffect::Inert),
    (b'j', KeyEffect::Moves),
    (b'k', KeyEffect::Moves),
    (b'l', KeyEffect::Inert),
    (b'm', KeyEffect::Inert),
    (b'n', KeyEffect::Inert),
    (b'o', KeyEffect::Inert),
    (b'p', KeyEffect::Inert),
    (b'q', KeyEffect::Inert),
    (b'r', KeyEffect::Inert),
    (b's', KeyEffect::Inert),
    (b't', KeyEffect::Inert),
    (b'u', KeyEffect::Inert),
    (b'v', KeyEffect::Inert),
    (b'w', KeyEffect::Inert),
    (b'x', KeyEffect::Inert),
    (b'y', KeyEffect::Inert),
    (b'z', KeyEffect::Inert),
    (0x7f, KeyEffect::Inert),
];
const CODEX_0160_UPDATE_KEYS: &[(u8, KeyEffect)] = &[
    (b'\r', KeyEffect::Commits),
    (b' ', KeyEffect::Inert),
    (b'(', KeyEffect::Inert),
    (b')', KeyEffect::Inert),
    (b',', KeyEffect::Inert),
    (b'-', KeyEffect::Inert),
    (b'0', KeyEffect::Inert),
    (b'1', KeyEffect::Commits),
    (b'2', KeyEffect::Commits),
    (b'3', KeyEffect::Commits),
    (b'4', KeyEffect::Inert),
    (b'5', KeyEffect::Inert),
    (b'6', KeyEffect::Inert),
    (b'7', KeyEffect::Inert),
    (b'8', KeyEffect::Inert),
    (b'9', KeyEffect::Inert),
    (b':', KeyEffect::Inert),
    (b'a', KeyEffect::Inert),
    (b'b', KeyEffect::Inert),
    (b'c', KeyEffect::Inert),
    (b'd', KeyEffect::Inert),
    (b'e', KeyEffect::Inert),
    (b'f', KeyEffect::Inert),
    (b'g', KeyEffect::Inert),
    (b'h', KeyEffect::Inert),
    (b'i', KeyEffect::Inert),
    (b'j', KeyEffect::Moves),
    (b'k', KeyEffect::Moves),
    (b'l', KeyEffect::Inert),
    (b'm', KeyEffect::Inert),
    (b'n', KeyEffect::Inert),
    (b'o', KeyEffect::Inert),
    (b'p', KeyEffect::Inert),
    (b'q', KeyEffect::Inert),
    (b'r', KeyEffect::Inert),
    (b's', KeyEffect::Inert),
    (b't', KeyEffect::Inert),
    (b'u', KeyEffect::Inert),
    (b'v', KeyEffect::Inert),
    (b'w', KeyEffect::Inert),
    (b'x', KeyEffect::Inert),
    (b'y', KeyEffect::Inert),
    (b'z', KeyEffect::Inert),
    (0x7f, KeyEffect::Inert),
];
const CODEX_0160_TRUST_KEYS: &[(u8, KeyEffect)] = &[
    (b'\r', KeyEffect::Commits),
    (b' ', KeyEffect::Inert),
    (b'(', KeyEffect::Inert),
    (b')', KeyEffect::Inert),
    (b',', KeyEffect::Inert),
    (b'-', KeyEffect::Inert),
    (b'0', KeyEffect::Inert),
    (b'1', KeyEffect::Inert),
    (b'2', KeyEffect::Commits),
    (b'3', KeyEffect::Inert),
    (b'4', KeyEffect::Inert),
    (b'5', KeyEffect::Inert),
    (b'6', KeyEffect::Inert),
    (b'7', KeyEffect::Inert),
    (b'8', KeyEffect::Inert),
    (b'9', KeyEffect::Inert),
    (b':', KeyEffect::Inert),
    (b'a', KeyEffect::Inert),
    (b'b', KeyEffect::Inert),
    (b'c', KeyEffect::Inert),
    (b'd', KeyEffect::Inert),
    (b'e', KeyEffect::Inert),
    (b'f', KeyEffect::Inert),
    (b'g', KeyEffect::Inert),
    (b'h', KeyEffect::Inert),
    (b'i', KeyEffect::Inert),
    (b'j', KeyEffect::Moves),
    (b'k', KeyEffect::Inert),
    (b'l', KeyEffect::Inert),
    (b'm', KeyEffect::Inert),
    (b'n', KeyEffect::Commits),
    (b'o', KeyEffect::Inert),
    (b'p', KeyEffect::Inert),
    (b'q', KeyEffect::Commits),
    (b'r', KeyEffect::Inert),
    (b's', KeyEffect::Inert),
    (b't', KeyEffect::Inert),
    (b'u', KeyEffect::Inert),
    (b'v', KeyEffect::Inert),
    (b'w', KeyEffect::Inert),
    (b'x', KeyEffect::Inert),
    (b'y', KeyEffect::Inert),
    (b'z', KeyEffect::Inert),
    (0x7f, KeyEffect::Inert),
];
const CODEX_0160_HOOKS_KEYS: &[(u8, KeyEffect)] = &[
    (b'\r', KeyEffect::Commits),
    (b' ', KeyEffect::Inert),
    (b'(', KeyEffect::Inert),
    (b')', KeyEffect::Inert),
    (b',', KeyEffect::Inert),
    (b'-', KeyEffect::Inert),
    (b'0', KeyEffect::Inert),
    (b'1', KeyEffect::Commits),
    (b'2', KeyEffect::Moves),
    (b'3', KeyEffect::Commits),
    (b'4', KeyEffect::Inert),
    (b'5', KeyEffect::Inert),
    (b'6', KeyEffect::Inert),
    (b'7', KeyEffect::Inert),
    (b'8', KeyEffect::Inert),
    (b'9', KeyEffect::Inert),
    (b':', KeyEffect::Inert),
    (b'a', KeyEffect::Inert),
    (b'b', KeyEffect::Inert),
    (b'c', KeyEffect::Inert),
    (b'd', KeyEffect::Inert),
    (b'e', KeyEffect::Inert),
    (b'f', KeyEffect::Inert),
    (b'g', KeyEffect::Inert),
    (b'h', KeyEffect::Inert),
    (b'i', KeyEffect::Inert),
    (b'j', KeyEffect::Moves),
    (b'k', KeyEffect::Moves),
    (b'l', KeyEffect::Inert),
    (b'm', KeyEffect::Inert),
    (b'n', KeyEffect::Inert),
    (b'o', KeyEffect::Inert),
    (b'p', KeyEffect::Inert),
    (b'q', KeyEffect::Inert),
    (b'r', KeyEffect::Inert),
    (b's', KeyEffect::Inert),
    (b't', KeyEffect::Inert),
    (b'u', KeyEffect::Inert),
    (b'v', KeyEffect::Inert),
    (b'w', KeyEffect::Inert),
    (b'x', KeyEffect::Inert),
    (b'y', KeyEffect::Inert),
    (b'z', KeyEffect::Inert),
    (0x7f, KeyEffect::Inert),
];

const CODEX_0164_DIALOGS: &[DialogKeys<'static>] = &[
    DialogKeys {
        dialog: "update",
        keys: CODEX_0164_UPDATE_KEYS,
    },
    DialogKeys {
        dialog: "trust",
        keys: CODEX_0164_TRUST_KEYS,
    },
    DialogKeys {
        dialog: "hooks",
        keys: CODEX_0164_HOOKS_KEYS,
    },
];

const CODEX_0160_DIALOGS: &[DialogKeys<'static>] = &[
    DialogKeys {
        dialog: "update",
        keys: CODEX_0160_UPDATE_KEYS,
    },
    DialogKeys {
        dialog: "trust",
        keys: CODEX_0160_TRUST_KEYS,
    },
    DialogKeys {
        dialog: "hooks",
        keys: CODEX_0160_HOOKS_KEYS,
    },
];

/// The records the doorbell's admission reads. Each field says what was
/// measured; a field left unverified keeps its release from being typed into
/// until the measurement that verifies it is recorded. `parser_cost` is the
/// release scanner's and the evidence parser's cost on the real push path,
/// with their work budgets (Q25), replayed from every release's recorded
/// launches at 50 × 200 and 80 × 300 (`src/agent_pty_scanner_budget.rs`):
/// at most 501 µs added to a push's lock hold and 5.0 ms of scanner CPU per
/// generation, no work limit reached (`docs/develop/doorbell-admission.md`).
/// Of the three, only Claude Code 2.1.310 is `Go`: both Codex releases stage
/// the line (a digit of a message id answers their update dialog, `n` their
/// folder-trust dialog), and neither records `head_echo` yet, so both are
/// `missing_measurement`. No release has its `editor_probe` measured yet, so
/// the deck checks no input box on any of them.
pub static RELEASE_RECORDS: [ReleaseRecord<'static>; 3] = [
    // Codex 0.164.0, the release current at measurement. Echo: the exact line
    // in the composer 30 of 30 (20 at 50 × 200, 10 at 24 × 80; ≤ 62 ms after
    // its one write). Submit: the transcript's user message 30 of 30 ≤ 418 ms
    // after the CR; the hook saw the exact prompt 30 of 30, but a session's
    // first prompt took 3.1 s and 5.5 s to reach it. Dialogs: update, folder
    // trust and hooks review, each raised 12 of 12 launches, the first at most
    // 4.02 s after the settle anchor. Input mode: raw from about 290 ms after
    // start, through every dialog. Layout: after each of 10 resizes and 10
    // one-column nudges, before and after a first prompt, Codex redrew in
    // full and the frame 5 s later was drawn for the width in force (a nudge
    // can leave a frame for the narrower width for 2 to 4 s first). Release
    // header: row 1, column 5, on 36 of 36 direct launches and on the deck's
    // own `devbox run codex-big` role, fresh (complete at stream byte 11 149,
    // devbox installing first) and respawned (byte 1 235).
    ReleaseRecord {
        agent: AgentType::Codex,
        release: Version::new(0, 164, 0),
        profile: "codex-2026-10",
        ready_composer: Measurement::Verified(()),
        echo: Measurement::Verified(()),
        submit_route: Measurement::Verified(()),
        layout: Measurement::Verified(()),
        key_effects: Measurement::Verified(CODEX_0164_DIALOGS),
        first_modal: Measurement::Verified(FirstModal::Slowest {
            launches: 36,
            max: Duration::from_millis(4020),
        }),
        input_mode: Measurement::Verified(()),
        release_recogniser: Measurement::Verified(()),
        parser_cost: Measurement::Verified(()),
        head_echo: Measurement::Absent,
        placeholder_empty: Measurement::Verified(()),
        delete_key: Measurement::Verified(()),
        transcript_echo: Measurement::Verified(()),
        editor_probe: Measurement::Absent,
    },
    // Codex 0.160.0, the minimum `scripts/box/require-codex-version.sh`
    // accepts and the incident's release, installed into a scratch npm prefix.
    // Echo 30 of 30 (≤ 68 ms); the transcript's user message 30 of 30
    // (≤ 18 ms after the CR); hook 30 of 30 exact, the first prompts 1.6 s.
    // Dialogs as 0.164.0, the first at most 4.46 s after the anchor over 36
    // launches. Layout as 0.164.0. Release header on 36 of 36 direct launches
    // and through the deck's `devbox run codex-big` role, fresh (byte 10 376)
    // and respawned (byte 340).
    ReleaseRecord {
        agent: AgentType::Codex,
        release: Version::new(0, 160, 0),
        profile: "codex-2026-10",
        ready_composer: Measurement::Verified(()),
        echo: Measurement::Verified(()),
        submit_route: Measurement::Verified(()),
        layout: Measurement::Verified(()),
        key_effects: Measurement::Verified(CODEX_0160_DIALOGS),
        first_modal: Measurement::Verified(FirstModal::Slowest {
            launches: 36,
            max: Duration::from_millis(4461),
        }),
        input_mode: Measurement::Verified(()),
        release_recogniser: Measurement::Verified(()),
        parser_cost: Measurement::Verified(()),
        head_echo: Measurement::Absent,
        placeholder_empty: Measurement::Verified(()),
        delete_key: Measurement::Verified(()),
        transcript_echo: Measurement::Verified(()),
        editor_probe: Measurement::Absent,
    },
    // Claude Code 2.1.310. Echo 30 of 30 (≤ 27 ms); the hook saw the exact
    // prompt 30 of 30, ≤ 102 ms after the CR, and the transcript's user
    // message showed ≤ 60 ms after it. No dialog appeared after readiness in
    // 10 launches idling 20 s each. A full redraw within 9 ms of each of 10
    // resizes and 10 nudges. Deleting the line leaves a blank composer, not
    // the suggestion placeholder (6 of 6), so a withdrawal cannot be
    // confirmed. Header: `Claude Code` at row 1, column 11, in each of the 13
    // launches measured (10 start-ups and 3 sessions).
    ReleaseRecord {
        agent: AgentType::ClaudeCode,
        release: Version::new(2, 1, 310),
        profile: "claude-code-2026-10",
        ready_composer: Measurement::Verified(()),
        echo: Measurement::Verified(()),
        submit_route: Measurement::Verified(()),
        layout: Measurement::Verified(()),
        key_effects: Measurement::Verified(&[]),
        first_modal: Measurement::Verified(FirstModal::NoneObserved { launches: 10 }),
        input_mode: Measurement::Verified(()),
        release_recogniser: Measurement::Verified(()),
        parser_cost: Measurement::Verified(()),
        // Nothing commits in any dialog, so the line is never staged.
        head_echo: Measurement::Absent,
        placeholder_empty: Measurement::Failed,
        delete_key: Measurement::Verified(()),
        transcript_echo: Measurement::Verified(()),
        editor_probe: Measurement::Absent,
    },
];
