//! PRD #1666 part 3: what the doorbell may type into, decided from one set of
//! facts per agent.
//!
//! The doorbell's admission is to type a line into an agent's pane only when
//! the agent's screen is one the deck knows: its empty composer, how it echoes
//! the line, which of its dialogs can appear after it is ready, and what each
//! key of the line does inside them. Those facts belong to an **agent**
//! ([`AGENT_FACTS`]: Claude Code and Codex), not to one of its releases: a
//! release nobody measured is assumed to behave like its agent's facts until a
//! measurement shows otherwise. [`activation`] turns a generation's agent and
//! its start-up header's latch ([`crate::release_scanner`]) into a verdict:
//! [`Activation::Go`], or [`Activation::NoGo`] with the clause the deck log
//! and the hold facts carry.
//!
//! The rules apply in order, and the first that matches decides:
//!
//! 1. the agent has no facts → `agent_not_supported`;
//! 2. no single header was latched → `unknown_release` (or the latch's own
//!    clause, `release_conflict`, `scanner_geometry` or `scanner_budget`). The
//!    latch is a per-generation check that the agent drew its start-up screen
//!    when it started; any `MAJOR.MINOR.PATCH` latches, and which one is not
//!    read here;
//! 3. the agent's facts decide ([`facts_verdict`]: the required facts, then no
//!    first-stage byte committing in any known dialog, then `head_echo` when
//!    the line is staged, then the settle cap).
//!
//! The releases that were measured by hand are kept in [`RECORDED_RELEASES`]
//! as conformance inputs: no admission reads them, and [`conformance`] says
//! whether one of them contradicts its agent's facts. A recorded release that
//! does is a test failure, fixed in the facts or the code; it never holds a
//! user's agent.
//!
//! Nothing here reads a screen or a clock: it is a pure function of its inputs,
//! so the doorbell can ask it at every admission and `inbox/doorbell/053`
//! can enumerate it.
//!
//! The facts' `editor_probe` is not one of the ring's inputs. It belongs to
//! the composer probe (`crate::composer_probe::permission`), which may check
//! an agent's input box only when it is verified; no rule above reads it.

use std::time::Duration;

use semver::Version;

use crate::event::AgentType;

/// The longest first-ring settle window the deck accepts (plan §4.4). A
/// settle window above it is `settle_over_cap`, never clamped:
/// the hold clock pauses inside the window, and the cap keeps that pause from
/// hiding a hold for long.
pub const SETTLE_CAP: Duration = Duration::from_secs(30);

/// The state of one fact.
///
/// Anything but [`Measurement::Verified`] in a required field keeps the
/// agent from being typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measurement<T> {
    /// Never measured.
    Absent,
    /// Measured, but not to the standard the facts require (fewer trials,
    /// a missed trial, or a result nobody checked yet).
    Unverified,
    /// Measured, and the measurement failed its gate (for example 19 of 20
    /// trials).
    Failed,
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

/// `m`'s state without its value.
fn presence<T>(m: &Measurement<T>) -> Measurement<()> {
    match m {
        Measurement::Verified(_) => Measurement::Verified(()),
        Measurement::Absent => Measurement::Absent,
        Measurement::Unverified => Measurement::Unverified,
        Measurement::Failed => Measurement::Failed,
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

/// The key effects known in one post-ready dialog: every key the doorbell
/// line can contain, plus the delete key and CR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogKeys<'a> {
    /// The dialog, by the name the measurements use (`update`, `trust`,
    /// `hooks`).
    pub dialog: &'a str,
    /// One entry per key byte.
    pub keys: &'a [(u8, KeyEffect)],
}

impl DialogKeys<'_> {
    pub fn effect(&self, key: u8) -> Option<KeyEffect> {
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
    pub fn time(&self) -> Duration {
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

/// What an agent's external-editor action does (Q35-11): the composer probe's
/// facts, kept in the agent's [`AgentFacts`].
///
/// It is an observer capability only. The ring's verdict ([`facts_verdict`],
/// [`activation`]) never reads it, so a missing one turns no ring off and a
/// verified one turns no ring on; it decides only whether the deck may check
/// the agent's input box (`crate::composer_probe::permission`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorProbe<'a> {
    /// The export's location and argument rule.
    pub export: ExportShape,
    /// The user's-editor selection the helper reproduces.
    pub selection: EditorSelection,
    /// The `?2004` writes per invocation.
    pub paste_toggles: PasteToggles,
    /// Every source the agent reads its key bindings from, in precedence
    /// order.
    pub keymap_sources: &'a [KeymapSource],
    /// P-KB1 (Claude Code, M49b): the agent loads its key bindings before
    /// its first ready frame, so a file unchanged since spawn is the binding
    /// it loaded. Codex has no such premise and leaves it absent.
    pub keymap_loaded_at_start: Measurement<()>,
    /// The candidate forms of the probe's key known inert in a composer
    /// holding text (M44e).
    pub inert_candidates: &'a [&'a [u8]],
    /// P-PASTE (M44e): with bracketed paste on, the agent treats a pasted
    /// `0x07` as text.
    pub pasted_bel_is_text: Measurement<()>,
}

/// Everything the deck relies on about one agent (plan §4.6), whatever its
/// release. The same shape holds what was measured on one recorded release
/// ([`RecordedRelease::measured`]).
///
/// The required fields decide whether the agent can be typed into at all;
/// the capability fields only turn a capability off; `editor_probe` belongs
/// to the composer probe and not to the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFacts<'a> {
    pub agent: AgentType,
    /// The screen profile whose recognisers the facts were verified with
    /// ([`crate::doorbell_screen`]).
    pub profile: &'a str,
    // Required.
    pub ready_composer: Measurement<()>,
    pub echo: Measurement<()>,
    pub submit_route: Measurement<()>,
    pub layout: Measurement<()>,
    pub key_effects: Measurement<&'a [DialogKeys<'a>]>,
    pub first_modal: Measurement<FirstModal>,
    pub input_mode: Measurement<()>,
    /// The agent's start-up header is read by [`crate::release_scanner`]'s
    /// recogniser, which the latch (rule 2) needs.
    pub release_recogniser: Measurement<()>,
    pub parser_cost: Measurement<()>,
    // Required when the line is staged (Q27): the token head alone, typed in
    // one write, echoed exactly in the composer within the echo wait, and the
    // rest typed after that echo, submitted and withdrawn as the transaction
    // does it. An agent whose line is not staged never types a head alone,
    // so its facts need none.
    pub head_echo: Measurement<()>,
    // Capability only.
    pub placeholder_empty: Measurement<()>,
    pub delete_key: Measurement<()>,
    pub transcript_echo: Measurement<()>,
    // Observer capability only: the verdict never reads it.
    pub editor_probe: Measurement<EditorProbe<'a>>,
}

/// One release that was measured by hand: a conformance input, read by no
/// admission. [`conformance`] checks it against its agent's facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRelease<'a> {
    pub release: Version,
    /// What was measured on this release, in the facts' shape. A field left
    /// `Absent` was not measured on it.
    pub measured: AgentFacts<'a>,
}

/// Why a generation is not typed into. [`NoGo::clause`] is the word the deck
/// log and the hold facts carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoGo {
    /// The generation's agent type is not known yet (a launcher whose agent
    /// has not identified itself).
    UnknownAgent,
    /// The deck has no facts for this agent (OpenCode, Pi, Devin): it never
    /// types into it.
    AgentNotSupported,
    /// The user turned the doorbell off for this agent
    /// ([`crate::doorbell_switch`]).
    TurnedOff,
    /// No release was latched from the start-up header.
    UnknownRelease,
    /// Two different releases appeared in the start-up header window.
    ReleaseConflict,
    /// The pane was too large for the release scanner when it started.
    ScannerGeometry,
    /// The release scanner was dropped at its work budget before it saw the
    /// header (Q25).
    ScannerBudget,
    /// The agent's facts lack a required one.
    MissingMeasurement(&'static str),
    /// The agent's facts have a required one that is unverified or failed.
    UnverifiedMeasurement(&'static str),
    /// A byte of the line's first stage selects or confirms an action in one
    /// of the agent's dialogs.
    FirstStageCommits { dialog: String, key: u8 },
    /// The agent's first-ring settle window is longer than [`SETTLE_CAP`].
    SettleOverCap,
}

impl NoGo {
    pub fn clause(&self) -> &'static str {
        match self {
            NoGo::UnknownAgent => "unknown_agent",
            NoGo::AgentNotSupported => "agent_not_supported",
            NoGo::TurnedOff => "turned_off",
            NoGo::UnknownRelease => "unknown_release",
            NoGo::ReleaseConflict => "release_conflict",
            NoGo::ScannerGeometry => "scanner_geometry",
            NoGo::ScannerBudget => "scanner_budget",
            NoGo::MissingMeasurement(_) => "missing_measurement",
            NoGo::UnverifiedMeasurement(_) => "unverified_measurement",
            NoGo::FirstStageCommits { .. } => "first_stage_commits",
            NoGo::SettleOverCap => "settle_over_cap",
        }
    }
}

/// The verdict for one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation<'a> {
    /// The agent's facts are complete and its header latched.
    Go {
        profile: &'a str,
        /// The line is typed in stages: a byte after the first stage selects
        /// or confirms an action in one of the agent's dialogs.
        staged: bool,
        settle: Duration,
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
/// The token's letters are checked against the facts' dialogs like the
/// parentheses, so facts in which one of them commits are
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

/// Rule 3 for one agent's facts: `Go` when every required fact is verified,
/// no first-stage byte commits in any of its dialogs, `head_echo` is verified
/// if a later-stage byte commits in one (the line is then staged), and its
/// settle window is within the cap; the first failing requirement otherwise.
pub fn facts_verdict<'a>(facts: &AgentFacts<'a>) -> Activation<'a> {
    fn need<T>(m: &Measurement<T>, name: &'static str) -> Result<(), NoGo> {
        match m {
            Measurement::Verified(_) => Ok(()),
            Measurement::Absent => Err(NoGo::MissingMeasurement(name)),
            Measurement::Unverified | Measurement::Failed => Err(NoGo::UnverifiedMeasurement(name)),
        }
    }
    let checks = [
        need(&facts.ready_composer, "ready_composer"),
        need(&facts.echo, "echo"),
        need(&facts.submit_route, "submit_route"),
        need(&facts.layout, "layout"),
        need(&facts.key_effects, "key_effects"),
        need(&facts.first_modal, "first_modal"),
        need(&facts.input_mode, "input_mode"),
        need(&facts.release_recogniser, "release_recogniser"),
        need(&facts.parser_cost, "parser_cost"),
    ];
    if let Some(Err(no_go)) = checks.into_iter().find(Result::is_err) {
        return Activation::NoGo(no_go);
    }
    let dialogs = facts.key_effects.verified().copied().unwrap_or_default();
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
    if staged && let Err(no_go) = need(&facts.head_echo, "head_echo") {
        return Activation::NoGo(no_go);
    }
    let first_modal = facts
        .first_modal
        .verified()
        .map(FirstModal::time)
        .unwrap_or_default();
    let settle = first_modal + first_modal / 2;
    if settle > SETTLE_CAP {
        return Activation::NoGo(NoGo::SettleOverCap);
    }
    Activation::Go {
        profile: facts.profile,
        staged,
        settle,
    }
}

/// `agent`'s facts in `facts`, when the deck has any.
pub fn facts_for<'f, 'a>(
    facts: &'f [AgentFacts<'a>],
    agent: &AgentType,
) -> Option<&'f AgentFacts<'a>> {
    facts.iter().find(|f| f.agent == *agent)
}

/// The verdict for a generation of `agent` whose header latched `release`,
/// given the agents' facts. See the module documentation for the rule order.
pub fn activation<'a>(
    facts: &[AgentFacts<'a>],
    agent: &AgentType,
    release: &LatchedRelease,
) -> Activation<'a> {
    // Rule 1.
    let Some(facts) = facts_for(facts, agent) else {
        return Activation::NoGo(NoGo::AgentNotSupported);
    };
    // Rule 2: any single release latched; which one is not read.
    match release {
        LatchedRelease::Known(_) => {}
        LatchedRelease::Unseen => return Activation::NoGo(NoGo::UnknownRelease),
        LatchedRelease::Conflict => return Activation::NoGo(NoGo::ReleaseConflict),
        LatchedRelease::ScannerGeometry => return Activation::NoGo(NoGo::ScannerGeometry),
        LatchedRelease::ScannerBudget => return Activation::NoGo(NoGo::ScannerBudget),
    }
    // Rule 3.
    facts_verdict(facts)
}

/// Whether `recorded` is consistent with its agent's facts in `facts`:
/// `Ok` when nothing measured on that release contradicts a fact the deck
/// relies on, the first contradiction otherwise. A field the release left
/// `Absent` (or `Unverified`) was not measured there and contradicts nothing.
///
/// - the agent has facts, with the same screen profile;
/// - every required and capability fact the facts verify was not `Failed` on
///   the release;
/// - every dialog and key measured on the release is in the facts, with the
///   measured effect or `Commits` (the facts may only be more cautious);
/// - the release's slowest first dialog is no later than the facts';
/// - a verified `editor_probe` on the release equals the facts'.
pub fn conformance(recorded: &RecordedRelease<'_>, facts: &[AgentFacts<'_>]) -> Result<(), String> {
    let measured = &recorded.measured;
    let who = format!("{:?} {}", measured.agent, recorded.release);
    let Some(facts) = facts_for(facts, &measured.agent) else {
        return Err(format!("{who}: the agent has no facts"));
    };
    if measured.profile != facts.profile {
        return Err(format!(
            "{who}: measured with profile {}, the facts name {}",
            measured.profile, facts.profile
        ));
    }
    let flags: [(&str, &Measurement<()>, &Measurement<()>); 12] = [
        (
            "ready_composer",
            &facts.ready_composer,
            &measured.ready_composer,
        ),
        ("echo", &facts.echo, &measured.echo),
        ("submit_route", &facts.submit_route, &measured.submit_route),
        ("layout", &facts.layout, &measured.layout),
        ("input_mode", &facts.input_mode, &measured.input_mode),
        (
            "release_recogniser",
            &facts.release_recogniser,
            &measured.release_recogniser,
        ),
        ("parser_cost", &facts.parser_cost, &measured.parser_cost),
        ("head_echo", &facts.head_echo, &measured.head_echo),
        (
            "placeholder_empty",
            &facts.placeholder_empty,
            &measured.placeholder_empty,
        ),
        ("delete_key", &facts.delete_key, &measured.delete_key),
        (
            "transcript_echo",
            &facts.transcript_echo,
            &measured.transcript_echo,
        ),
        (
            "editor_probe",
            &presence(&facts.editor_probe),
            &presence(&measured.editor_probe),
        ),
    ];
    for (name, fact, on_release) in flags {
        if fact.verified().is_some() && *on_release == Measurement::Failed {
            return Err(format!("{who}: {name} failed on this release"));
        }
    }
    if let (Measurement::Verified(fact), Measurement::Verified(on_release)) =
        (&facts.editor_probe, &measured.editor_probe)
        && fact != on_release
    {
        return Err(format!(
            "{who}: editor_probe measured {on_release:?}, the facts say {fact:?}"
        ));
    }
    match (&facts.key_effects, &measured.key_effects) {
        (_, Measurement::Failed) if facts.key_effects.verified().is_some() => {
            return Err(format!("{who}: key_effects failed on this release"));
        }
        (Measurement::Verified(known), Measurement::Verified(on_release)) => {
            for dialog in on_release.iter() {
                let Some(fact) = known.iter().find(|d| d.dialog == dialog.dialog) else {
                    return Err(format!(
                        "{who}: dialog {} is not in the facts",
                        dialog.dialog
                    ));
                };
                for &(key, effect) in dialog.keys {
                    match fact.effect(key) {
                        Some(f) if f == effect || f == KeyEffect::Commits => {}
                        Some(f) => {
                            return Err(format!(
                                "{who}: key {key:#04x} in {} is {effect:?} on this release, \
                                 the facts say {f:?}",
                                dialog.dialog
                            ));
                        }
                        None => {
                            return Err(format!(
                                "{who}: key {key:#04x} in {} is not in the facts",
                                dialog.dialog
                            ));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    match (&facts.first_modal, &measured.first_modal) {
        (_, Measurement::Failed) if facts.first_modal.verified().is_some() => {
            return Err(format!("{who}: first_modal failed on this release"));
        }
        (Measurement::Verified(fact), Measurement::Verified(on_release))
            if on_release.time() > fact.time() =>
        {
            return Err(format!(
                "{who}: a first dialog {:?} after the anchor, later than the facts' {:?}",
                on_release.time(),
                fact.time()
            ));
        }
        _ => {}
    }
    Ok(())
}

/// The variable the e2e build reads for [`assumed_editor_probes`].
pub const ASSUME_EDITOR_PROBE_ENV: &str = "DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE";

/// Parse a comma-separated list of exact `agent@release` entries (`claude`
/// or `codex`, then a bare `MAJOR.MINOR.PATCH`). An entry that is not one
/// refuses the whole list, as does an entry named twice. The result names
/// the releases a stand-in's header draws; it never carries a release a
/// generation has not latched itself.
pub fn parse_assumed_editor_probes(list: &str) -> Result<Vec<(AgentType, Version)>, String> {
    if list.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(AgentType, Version)> = Vec::new();
    for entry in list.split(',') {
        let entry = entry.trim();
        let unknown = || format!("not an agent@release: {entry:?}");
        let (agent, release) = entry.split_once('@').ok_or_else(unknown)?;
        let agent = match agent {
            "claude" => AgentType::ClaudeCode,
            "codex" => AgentType::Codex,
            _ => return Err(unknown()),
        };
        // A pre-release or build is refused: a header the scanner latches
        // is a bare release, so such an entry could never match one.
        let release = Version::parse(release)
            .ok()
            .filter(|v| v.pre.is_empty() && v.build.is_empty())
            .ok_or_else(unknown)?;
        if entries.contains(&(agent.clone(), release.clone())) {
            return Err(format!("named twice: {entry:?}"));
        }
        entries.push((agent, release));
    }
    Ok(entries)
}

/// PRD #1666 part 3, Q35-11: the releases an e2e run asks the deck to treat
/// as having the e2e stand-ins' `editor_probe`
/// ([`crate::doorbell_screen::standin_editor_probe`]) in place of their
/// agent's. It substitutes an editor probe for a release, never a release: a
/// generation still needs its own header to latch one of them. A list with
/// an unknown entry assumes nothing and is logged as
/// `editor probe assumption refused`. The daemon reads it once, at start
/// (`crate::composer_probe::install::capture_assumption`).
#[cfg(feature = "e2e")]
pub fn assumed_editor_probes() -> Vec<(AgentType, Version, EditorProbe<'static>)> {
    read_assumed_editor_probes().unwrap_or_else(|why| {
        tracing::warn!(target: "doorbell", %why, "editor probe assumption refused");
        Vec::new()
    })
}

/// [`assumed_editor_probes`], with a refused list as its reason: the
/// variable unset or empty is no assumption; set but not UTF-8 is refused.
#[cfg(feature = "e2e")]
pub fn read_assumed_editor_probes()
-> Result<Vec<(AgentType, Version, EditorProbe<'static>)>, String> {
    let list = match std::env::var(ASSUME_EDITOR_PROBE_ENV) {
        Ok(list) => list,
        Err(std::env::VarError::NotPresent) => return Ok(Vec::new()),
        Err(std::env::VarError::NotUnicode(_)) => return Err("not UTF-8".to_string()),
    };
    Ok(parse_assumed_editor_probes(&list)?
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

/// The `editor_probe` a generation of `agent` that latched `release` is
/// checked with: the assumed one for exactly that agent and release, else the
/// agent's own in `facts` (`Absent` for an agent with none).
pub fn editor_probe_for<'a>(
    facts: &[AgentFacts<'a>],
    assumed: &[(AgentType, Version, EditorProbe<'a>)],
    agent: &AgentType,
    release: &Version,
) -> Measurement<EditorProbe<'a>> {
    if let Some((_, _, probe)) = assumed.iter().find(|(a, r, _)| a == agent && r == release) {
        return Measurement::Verified(*probe);
    }
    facts_for(facts, agent).map_or(Measurement::Absent, |f| f.editor_probe)
}

// ---------------------------------------------------------------------------
// The agents' facts.
// ---------------------------------------------------------------------------

/// Codex's dialogs as the facts know them: the recorded releases' tables,
/// with a key whose effect differs between them taken as `Commits` (folder
/// trust's `2` dismisses on 0.164.0 and commits on 0.160.0, so the facts use
/// 0.160.0's folder-trust table; the update and hooks tables are the same on
/// both).
const CODEX_DIALOGS: &[DialogKeys<'static>] = &[
    DialogKeys {
        dialog: "update",
        keys: CODEX_0164_UPDATE_KEYS,
    },
    DialogKeys {
        dialog: "trust",
        keys: CODEX_0160_TRUST_KEYS,
    },
    DialogKeys {
        dialog: "hooks",
        keys: CODEX_0164_HOOKS_KEYS,
    },
];

/// Claude Code's external-editor action, as measured on 2.1.317 and again on
/// 2.1.318 with every value the same ([`CLAUDE_2_1_317_EDITOR_PROBE`],
/// [`CLAUDE_2_1_318_EDITOR_PROBE`]).
pub const CLAUDE_EDITOR_PROBE: EditorProbe<'static> = CLAUDE_2_1_317_EDITOR_PROBE;

/// Claude Code's facts, from its recorded releases (2.1.310, 2.1.312, 2.1.317,
/// 2.1.318, which measured the same on every value they share): no dialog
/// after readiness, so the line is never staged and the first ring waits for
/// no settle window; deleting the line leaves a blank composer, not the
/// placeholder, so a withdrawal cannot be confirmed; the editor probe as
/// measured on 2.1.317 and 2.1.318.
pub const CLAUDE_FACTS: AgentFacts<'static> = AgentFacts {
    agent: AgentType::ClaudeCode,
    profile: "claude-code-2026-10",
    ready_composer: Measurement::Verified(()),
    echo: Measurement::Verified(()),
    submit_route: Measurement::Verified(()),
    layout: Measurement::Verified(()),
    key_effects: Measurement::Verified(&[]),
    first_modal: Measurement::Verified(FirstModal::NoneObserved { launches: 40 }),
    input_mode: Measurement::Verified(()),
    release_recogniser: Measurement::Verified(()),
    parser_cost: Measurement::Verified(()),
    // Nothing commits in any dialog, so the line is never staged.
    head_echo: Measurement::Absent,
    placeholder_empty: Measurement::Failed,
    delete_key: Measurement::Verified(()),
    transcript_echo: Measurement::Verified(()),
    editor_probe: Measurement::Verified(CLAUDE_EDITOR_PROBE),
};

/// Codex's facts, from its recorded releases (0.160.0 and 0.164.0): the line
/// is staged (a digit of a message id answers the update dialog, `n` the
/// folder-trust dialog), its head echo as measured on 0.164.0; the first
/// ring's settle window from the slowest first dialog over both releases'
/// 72 launches (4.461 s, on 0.160.0), so 6.69 s. No input-box check yet: the
/// editor probe stays absent until Codex's activation.
pub const CODEX_FACTS: AgentFacts<'static> = AgentFacts {
    agent: AgentType::Codex,
    profile: "codex-2026-10",
    ready_composer: Measurement::Verified(()),
    echo: Measurement::Verified(()),
    submit_route: Measurement::Verified(()),
    layout: Measurement::Verified(()),
    key_effects: Measurement::Verified(CODEX_DIALOGS),
    first_modal: Measurement::Verified(FirstModal::Slowest {
        launches: 72,
        max: Duration::from_millis(4461),
    }),
    input_mode: Measurement::Verified(()),
    release_recogniser: Measurement::Verified(()),
    parser_cost: Measurement::Verified(()),
    head_echo: Measurement::Verified(()),
    placeholder_empty: Measurement::Verified(()),
    delete_key: Measurement::Verified(()),
    transcript_echo: Measurement::Verified(()),
    editor_probe: Measurement::Absent,
};

/// The facts the doorbell's admission reads, one set per agent it types into.
pub static AGENT_FACTS: [AgentFacts<'static>; 2] = [CLAUDE_FACTS, CODEX_FACTS];

// ---------------------------------------------------------------------------
// The recorded measurements (M1, M2; 2026-10-24 onwards): conformance inputs.
// Each record states its sample counts and figures; the raw captures 036
// replays are under `tests/fixtures/{codex,claude}-inbox/r5wake/`.
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

/// The releases measured by hand, each with what was measured on it: the
/// inputs of the conformance tests (`inbox/doorbell/053`, [`conformance`]),
/// read by no admission. A release missing from here is typed into on its
/// agent's facts like any other. `parser_cost` is the release scanner's and
/// the evidence parser's cost on the real push path, with their work budgets
/// (Q25), replayed from every release's recorded launches at 50 × 200 and
/// 80 × 300 (`src/agent_pty_scanner_budget.rs`). The last replay, on
/// 2026-11-03 with Claude Code 2.1.317's launches added, ran on a contended
/// machine (load average 10.65 on 16 CPUs at its start, from other work):
/// over every recorded trace, p99 213.9 µs and at most 2980 µs added to a
/// push's lock hold and at most 14.63 ms of scanner CPU per generation;
/// 2.1.317's own traces at most 1315.8 µs and 10.95 ms, with 2.1.312's rows
/// in the same run of the same size. No work limit was reached; the
/// aggregate p99, every push's maximum and every generation's CPU are within
/// plan §4.6's limits, while on the smallest traces (7 to 34 pushes, at
/// 80 × 300) the per-trace p99 reached 1.3 ms under this load
/// (`tests/fixtures/claude-inbox/release-2.1.317/parser-cost-replay.txt`;
/// the earlier, quieter replay's figures are in
/// `docs/develop/doorbell-admission.md`). Codex 0.160.0's `head_echo` was
/// never measured, which contradicts no fact; its slower first dialog is
/// what Codex's settle fact covers.
pub static RECORDED_RELEASES: [RecordedRelease<'static>; 6] = [
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
    RecordedRelease {
        release: Version::new(0, 164, 0),
        measured: AgentFacts {
            agent: AgentType::Codex,
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
            // Codex Go (Q27, measured 2026-11-04): Codex 0.164.0 only, the native
            // executable with SHA-256 b93f6811d3f653f0fb83be4050d857fb9abbb111df
            // a2ce160563327daff7f306 run through its npm launcher under
            // `dot-agent-deck wrap --agent codex`, with TERM=xterm-256color and
            // COLORTERM=truecolor (every capture ran under both; nothing was
            // recorded without COLORTERM), at 50 x 200 and 24 x 80, on an
            // untouched idle composer, with the deck's hooks trusted (30),
            // switched off (10) or untrusted through a `sh -c` launcher (10).
            // The token head typed alone in one write was echoed exactly 50 of
            // 50 times, at most 73.6 ms after its write against the 1500 ms echo
            // wait; no head byte opened a recognised dialog or submitted
            // anything; the rest, typed after that echo, was held exactly before
            // every Enter, and the Enter at the transaction's timing (153-178 ms
            // after the rest's write returned) submitted the exact line 50 of
            // 50, read off the transcript within the confirm wait and once in
            // Codex's own rollout. A whole head was withdrawn with 8 deletes 6
            // of 6, and the strict recogniser refused partial heads, a second
            // stage ending in a space, other lines and a line kept by an Enter
            // in the same burst. Load: QUIET only (1-minute load 1.6-5.3 on 16
            // CPUs, CPU pressure at most 0.91%). It does NOT cover: another
            // release or build; another size, or a fresh 24 x 80 Codex before
            // its first turn (no Clean frame); load; what model turns do; submit
            // latency with daemon-bound hooks; Codex without COLORTERM; Codex
            // menus, themes or palettes; a touched box or `editor_probe`; the
            // deck's own driver, daemon or writer, none of which was in the
            // loop; an Enter sent with the line or before its echo (kept in the
            // draft 3 of 3); an Enter margin below about 82 ms; whole-line or
            // wrapped withdrawal, which rests on `delete_key` and
            // `placeholder_empty` above, not on this evidence. Evidence:
            // `tests/fixtures/codex-inbox/release-0.164.0/`.
            head_echo: Measurement::Verified(()),
            placeholder_empty: Measurement::Verified(()),
            delete_key: Measurement::Verified(()),
            transcript_echo: Measurement::Verified(()),
            editor_probe: Measurement::Absent,
        },
    },
    // Codex 0.160.0, the minimum `scripts/box/require-codex-version.sh`
    // accepts and the incident's release, installed into a scratch npm prefix.
    // Echo 30 of 30 (≤ 68 ms); the transcript's user message 30 of 30
    // (≤ 18 ms after the CR); hook 30 of 30 exact, the first prompts 1.6 s.
    // Dialogs as 0.164.0, the first at most 4.46 s after the anchor over 36
    // launches. Layout as 0.164.0. Release header on 36 of 36 direct launches
    // and through the deck's `devbox run codex-big` role, fresh (byte 10 376)
    // and respawned (byte 340).
    RecordedRelease {
        release: Version::new(0, 160, 0),
        measured: AgentFacts {
            agent: AgentType::Codex,
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
    },
    // Claude Code 2.1.310. Echo 30 of 30 (≤ 27 ms); the hook saw the exact
    // prompt 30 of 30, ≤ 102 ms after the CR, and the transcript's user
    // message showed ≤ 60 ms after it. No dialog appeared after readiness in
    // 10 launches idling 20 s each. A full redraw within 9 ms of each of 10
    // resizes and 10 nudges. Deleting the line leaves a blank composer, not
    // the suggestion placeholder (6 of 6), so a withdrawal cannot be
    // confirmed. Header: `Claude Code` at row 1, column 11, in each of the 13
    // launches measured (10 start-ups and 3 sessions).
    RecordedRelease {
        release: Version::new(2, 1, 310),
        measured: AgentFacts {
            agent: AgentType::ClaudeCode,
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
    },
    // Claude Code 2.1.312, pinned SHA-256 4f4178ce41d83e23341ebfbaecdbcf8ea6
    // bf7baea0690025abe49eeec3a5d01d. M1/Q32 through the private lane-2 TUI
    // harness: 30/30 exact echoes (<= 55 ms), exact blocked prompt hooks
    // (<= 229 ms), and transcript user echoes (<= 137 ms), across 24x80 and
    // 50x200. Ten fresh launches idle 20 s each with no post-ready dialog;
    // the header stays at row 1, column 11. Ten resizes and ten nudges draw
    // the measured marker and width witness (<= 44 ms / 38 ms); six deletes
    // leave no placeholder. All nineteen Q32 sequences match 2.1.310.
    // M1 uses xterm-256color without COLORTERM, as the original captures do.
    // Real push replay at 50x200 and 80x300: all traces' added hold p99
    // 64.8 us, max 235.5 us; this release's max scanner CPU 3.82 ms/gen.
    RecordedRelease {
        release: Version::new(2, 1, 312),
        measured: AgentFacts {
            agent: AgentType::ClaudeCode,
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
            head_echo: Measurement::Absent,
            placeholder_empty: Measurement::Failed,
            delete_key: Measurement::Verified(()),
            transcript_echo: Measurement::Verified(()),
            editor_probe: Measurement::Absent,
        },
    },
    // Claude Code 2.1.317, the release installed on 2026-11-03, pinned
    // SHA-256 eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9a
    // e57eb0 (the official @anthropic-ai/claude-code-linux-x64 package).
    // M1/Q32 through the private lane-2 TUI harness, every submission blocked
    // before a model turn: 30/30 exact echoes (<= 64.0 ms), exact blocked
    // prompt hooks (<= 242.3 ms after the CR) and transcript user echoes
    // (<= 97.1 ms), across 24x80 and 50x200. Ten fresh launches idle 20 s
    // each with no post-ready dialog, raw input from the ready composer; the
    // recorded header latches 2.1.317 byte by byte, at the recorded split and
    // in one push. Ten resizes and ten nudges draw the measured marker
    // and width witness (first redraw <= 45.5 ms); six deletes leave no
    // placeholder. All nineteen Q32 sequences match 2.1.312, and the nine
    // menu size/palette sets read as 2.1.310's and 2.1.312's do (no wrong
    // acceptance or refusal in the fast replay). Real push replay: see the
    // doc comment above (this release's traces at most 1315.8 us added hold
    // and 10.95 ms scanner CPU per generation, under load). Restoring a
    // blocked prompt took up to 742 ms after the hook; that is the
    // measurement's private hook, and nothing here reads it. Evidence:
    // `tests/fixtures/claude-inbox/r5wake/claude-2.1.317-*` and
    // `tests/fixtures/claude-inbox/release-2.1.317/`.
    RecordedRelease {
        release: Version::new(2, 1, 317),
        measured: AgentFacts {
            agent: AgentType::ClaudeCode,
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
            // The Claude activation (Q35-28): the measured record, so a
            // directly spawned 2.1.317 is armable once its root is bound
            // (`inbox/doorbell/053`, `inbox/probe/012`).
            editor_probe: Measurement::Verified(CLAUDE_2_1_317_EDITOR_PROBE),
        },
    },
    // Claude Code 2.1.318, the release installed on 2026-11-04, pinned
    // SHA-256 708c77b204db30086ff6f1ed34577bda03e1be74ced80557cbf7b54249
    // ebf0bd (the official @anthropic-ai/claude-code-linux-x64 package,
    // byte-identical to the installed copy). It measured the same as 2.1.317
    // on every value this record takes, through the same per-release test
    // bodies, every submission blocked before a model turn: 30/30 exact
    // echoes (<= 44.2 ms), exact blocked prompt hooks (<= 240.7 ms after the
    // CR) and transcript user echoes (<= 114.4 ms), across 24x80 and 50x200.
    // Ten fresh launches idle 20 s each with no post-ready dialog, raw input
    // from the ready composer; the recorded header latches 2.1.318 byte by
    // byte, at the recorded split and in one push. Ten resizes and ten
    // nudges draw the measured marker and width witness (first redraw
    // <= 58.4 ms); six deletes leave no placeholder. All nineteen Q32
    // sequences match 2.1.312, and the nine menu size/palette sets read as
    // 2.1.317's do (63/63 screens in the fast replay). Real push replay, run
    // in an RWX sandbox (16 cores, load average 6.52 at its start, from its
    // own build), so not comparable with 2.1.317's run: this release's traces
    // at most 87.5 us added hold and 3.39 ms scanner CPU per generation; over
    // the run's 144 recorded-trace rows the aggregate p99 66.1 us and at most
    // 292.1 us, and every row's own p99 and maximum and every generation's
    // CPU within plan §4.6's limits, with no work limit reached
    // (`tests/fixtures/claude-inbox/release-2.1.318/parser-cost-replay.txt`).
    // Under a recalled prompt's `History i/n` rule the profile read no
    // composer in any of the 24 such frames per capture. Evidence:
    // `tests/fixtures/claude-inbox/release-2.1.318/README.md` and
    // `tests/fixtures/claude-inbox/r5wake/claude-menu-candidates.md`,
    // "Claude Code 2.1.318".
    RecordedRelease {
        release: Version::new(2, 1, 318),
        measured: AgentFacts {
            agent: AgentType::ClaudeCode,
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
            // Verified by the Claude activation (Q35-28, act29), as 2.1.317's.
            editor_probe: Measurement::Verified(CLAUDE_2_1_318_EDITOR_PROBE),
        },
    },
];

/// Claude Code 2.1.317's external-editor action as measured on 2026-11-03
/// (`tests/fixtures/claude-inbox/release-2.1.317/`, `inbox/probe/088`,
/// `089`): its record's `editor_probe` since the Claude activation (Q35-28).
/// Each field is verified only as far as it was measured.
///
/// - `export`: 5/5 Ctrl+G presses under `VISUAL` handed the editor the draft,
///   with `TMPDIR` unset, under `/tmp/claude-<uid>/` (as
///   `claude-prompt-<uuid>.md`) as its last argument, and reimported the
///   rewritten file; a set `TMPDIR` was not measured.
/// - `selection`: a set `VISUAL`, split on spaces, before `EDITOR`; an empty
///   `VISUAL` falls to `EDITOR`; with neither, `code` on `PATH` before `vi`.
/// - `paste_toggles`: one `?2004l` before the editor and one `?2004h` after
///   it, 5/5, measured on the `VISUAL` path, which is how the deck's helper
///   is reached; the `EDITOR` and `PATH` `vi` cells wrote the same, and the
///   `PATH` `code -w -g` fallback wrote no `?2004` at all.
/// - `keymap_sources`: the user's `keybindings.json` only; the same file in
///   the project's `.claude/` had no effect, with user settings only and
///   with every settings source.
/// - `keymap_loaded_at_start` (P-KB1, M49b): a Ctrl+G-unbinding file present
///   at spawn was in force at the first composer frame 5/5, stayed in force
///   unchanged for ten minutes 10/10, and a file created or removed during a
///   session took effect by 3.75 s and 2.53 s (sampled, not a race bound).
/// - `inert_candidates`: none, the M44e safe default. BEL, implicit CSI-u
///   103/71 and modifyOtherKeys 103/71 open the editor; no form is promoted
///   to inert.
/// - `pasted_bel_is_text`: `Absent`, the safe default. Measured, the pasted
///   BEL is kept as text: a bracketed `PASTE` BEL `TAIL` opened no editor
///   and the next export held `BASEPASTE\x07TAIL`, 2/2 whole and split, in
///   two runs (`inbox/probe/089` now fails on any other reading). The first
///   report's "dropped" read the screen, which draws no BEL.
pub const CLAUDE_2_1_317_EDITOR_PROBE: EditorProbe<'static> = EditorProbe {
    export: ExportShape::ClaudeTmpdir,
    selection: EditorSelection::ClaudeCode,
    paste_toggles: PasteToggles {
        disables: 1,
        enables: 1,
    },
    keymap_sources: &[KeymapSource::ClaudeKeybindings],
    keymap_loaded_at_start: Measurement::Verified(()),
    inert_candidates: &[],
    pasted_bel_is_text: Measurement::Absent,
};

/// Claude Code 2.1.318's external-editor action as measured on 2026-11-04
/// (`tests/fixtures/claude-inbox/release-2.1.318/`, `inbox/probe/088`,
/// `089`, through the same test bodies as 2.1.317's): the `editor_probe` its
/// record gets at activation, a one-line change. Every value equals
/// [`CLAUDE_2_1_317_EDITOR_PROBE`]'s, and each field is verified only as far
/// as it was measured.
///
/// - `export`: 5/5 Ctrl+G presses under `VISUAL` handed the editor the draft,
///   with `TMPDIR` unset, under `/tmp/claude-<uid>/` (as
///   `claude-prompt-<uuid>.md`) as its last argument, and reimported the
///   rewritten file; a set `TMPDIR` was not measured.
/// - `selection`: a set `VISUAL` before `EDITOR`; an empty `VISUAL` falls to
///   `EDITOR`; with neither, `code` on `PATH` (as `code -w -g`) before `vi`.
/// - `paste_toggles`: one `?2004l` before the editor and one `?2004h` after
///   it, 5/5, measured on the `VISUAL` path; the `EDITOR` and `PATH` `vi`
///   cells wrote the same, and the `PATH` `code -w -g` fallback wrote none.
/// - `keymap_sources`: the user's `keybindings.json` only; the same file in
///   the project's `.claude/` had no effect, with user settings only and
///   with every settings source.
/// - `keymap_loaded_at_start` (P-KB1, M49b): a Ctrl+G-unbinding file present
///   at spawn was in force at the first composer frame 5/5, stayed in force
///   unchanged for ten minutes 10/10, and a file created or removed during a
///   session took effect by 3.65 s and 3.87 s (sampled, not a race bound).
/// - `inert_candidates`: none, the M44e safe default; all 73 state/form cells
///   read as 2.1.317's.
/// - `pasted_bel_is_text`: `Absent`, the safe default. Measured, the pasted
///   BEL is kept as text: a bracketed `PASTE` BEL `TAIL` opened no editor
///   and the next export held `BASEPASTE\x07TAIL`, 2/2 whole and split.
pub const CLAUDE_2_1_318_EDITOR_PROBE: EditorProbe<'static> = EditorProbe {
    export: ExportShape::ClaudeTmpdir,
    selection: EditorSelection::ClaudeCode,
    paste_toggles: PasteToggles {
        disables: 1,
        enables: 1,
    },
    keymap_sources: &[KeymapSource::ClaudeKeybindings],
    keymap_loaded_at_start: Measurement::Verified(()),
    inert_candidates: &[],
    pasted_bel_is_text: Measurement::Absent,
};
