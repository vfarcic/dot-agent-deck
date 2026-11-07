//! PRD #20 M2 — the compiled-in agent registry + integration-strategy seam.
//!
//! Before this module, everything the deck knew about a specific agent lived in
//! scattered `match AgentType` arms: detection in [`crate::event::AgentType::from_command`],
//! the human label in the `Display` impl (`src/ui.rs`), the default authoring
//! command as a lone `const` (`src/ui.rs`), and the install/materialize dispatch
//! spread across `main.rs` and `agent_pty.rs`. Adding an agent meant touching
//! every one of those sites.
//!
//! This module centralises that per-agent data into **one cohesive
//! [`AgentSpec`] entry per agent** and names a small finite set of integration
//! [`IntegrationStrategy`] mechanisms. The agent identity stays a typed
//! [`crate::event::AgentType`] enum keyed into the registry — runtime/user
//! extensibility is an explicit non-goal (every new agent ships in a release
//! anyway), so a recompile-per-agent is acceptable and the win is
//! maintainability, not destructuring.
//!
//! This is a **behaviour-preserving** move for the shipped agents (Claude Code,
//! OpenCode, Pi): the scattered sites now READ from here instead of hardcoding,
//! and the existing test suite passes unchanged as the regression proof. The
//! badge colour field is populated now even though rendering it on cards is a
//! later milestone — the registry is meant to be the single source of truth per
//! the PRD success criteria.

use std::borrow::Cow;

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use crate::event::AgentType;

/// The finite set of mechanisms by which an agent's activity reaches the deck.
///
/// The two originally-shipped agents already used two different mechanisms
/// (native hooks vs. a plugin), and Pi added a third (a bundled extension),
/// which is precisely why this layer is inherently code rather than data:
/// adding an agent that reuses an existing strategy is a registry entry (+
/// release), while a genuinely new mechanism is a one-time strategy
/// implementation, then a registry entry thereafter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationStrategy {
    /// Native hook scripts installed into the agent's own config
    /// (Claude Code — `src/hooks_manage.rs`).
    NativeHooks,
    /// A JS plugin materialized into the agent's plugin directory
    /// (OpenCode — `src/opencode_manage.rs`).
    Plugin,
    /// A bundled extension materialized into the agent's HOME
    /// (Pi — `src/orchestrator_ext.rs`).
    Extension,
    /// A stdout wrapper (`dot-agent-deck wrap`) that spawns the agent, passes
    /// stdio through transparently, and tees its output through pattern
    /// detection into events. Shipped and dispatched-on: Codex uses it (PRD
    /// #20), and the new-agent spawn seam rewrites a Wrapper-strategy command
    /// into `dot-agent-deck wrap …` (`crate::wrap::wrap_launch_command`). For
    /// Codex the wrapper is also the PTY host + native-hook injector — its rich
    /// prompt/tool/turn events come from Codex's Claude-Code-compatible native
    /// hooks, with the coarse stdout classifier only as a fallback (see
    /// `docs/develop/agent-adapters.md`).
    Wrapper,
}

/// PRD #1541: the keys that edit or interrupt this agent's prompt — what voice
/// control presses to interrupt a turn, clear the prompt, or remove what it last
/// typed.
///
/// **Measured, per agent and per version, and only for the versions named.**
/// Each value below was driven against the real agent in a private tmux server
/// on 2026-10-03 (`prds/done/1541-voice-agent-prompt-control.md`, "Verified per-agent
/// key table"): Claude Code 2.1.289, Codex 0.160.0, OpenCode 1.18.34 and Pi
/// 0.87.1. An agent upgrade can move a key, so a reader citing one of these
/// should cite the version beside it, and a change here is a re-measurement, not
/// an edit.
///
/// **Devin carries none.** It was logged out on the box the table was measured
/// on, so none of its keys could be verified; its entry is `None` until someone
/// measures it logged in. The neutral [`NONE`] placeholder carries none either —
/// the deck does not know what is in that pane.
///
/// **`Ctrl+C` (`0x03`) is never one of these keys, deliberately.** On an empty
/// prompt it quits Codex and OpenCode outright, and a second one quits Pi, so a
/// misheard or repeated command could end the agent. The interrupt is `ESC`
/// instead, which ends the turn and keeps the agent, its session and any draft
/// typed mid-turn in every measured agent. A unit test pins that no sequence
/// here contains `0x03`.
///
/// **Served by the daemon, not compiled into a client** (PRD #1541 M1 decision
/// 7, CLAUDE.md rule 18): a deck answers for the agent versions on its own host, the way
/// [`crate::agent_pty::AgentRecord::cli_name`] answers which binary it forked
/// (issue #856). It reaches clients as
/// [`crate::agent_pty::AgentRecord::prompt_keys`], an additive optional field.
///
/// The strings are the exact bytes to write to the agent's PTY, unbracketed —
/// the same path the user's own keystrokes take. They are all ASCII control
/// characters today, and `str` rather than bytes so a JSON client can hand them
/// to its terminal-write call unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptKeys {
    /// Interrupt the agent's current turn: the steps to write in order, each
    /// followed by its pause. One `ESC` for Claude Code, Codex and Pi; `ESC`,
    /// ~300 ms, `ESC` for OpenCode, whose first `ESC` only arms "esc again to
    /// interrupt".
    ///
    /// **Only while the agent is working.** On an IDLE agent a repeated `ESC`
    /// opens Rewind in Claude Code (and clears a draft), the transcript browser
    /// in Codex and the Session Tree in Pi, so the sender's guard — send only
    /// while the status says working, and never twice in quick succession — is
    /// part of this key's contract, not an option.
    pub interrupt: Cow<'static, [KeyStep]>,
    /// Clear the whole prompt.
    pub clear: ClearKey,
    /// Delete the one character before the cursor.
    pub delete_char: DeleteCharKey,
}

/// One write in a [`PromptKeys`] sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyStep {
    /// The bytes to write.
    pub bytes: Cow<'static, str>,
    /// How long to wait after this write before the next step, in
    /// milliseconds. `0` on the last step.
    #[serde(default)]
    pub pause_after_ms: u32,
}

/// How [`PromptKeys::clear`] empties the prompt: one key, pressed as many times
/// as the prompt needs.
///
/// **Over-counting is harmless** in every measured agent — an extra press on an
/// empty prompt does nothing — so a sender that cannot see the prompt should
/// round up rather than risk leaving text behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearKey {
    /// The key to press. `Ctrl+U` (`0x15`, NAK) in every measured agent.
    pub bytes: Cow<'static, str>,
    /// What one press removes, so the sender can count the presses a prompt
    /// needs.
    pub presses: ClearPresses,
    /// The most presses to put in one write; `None` where no limit was
    /// observed. Claude Code ignores a single write of 64 or more NAKs
    /// ENTIRELY (10 to 63 worked), so its writes are capped well below that, at
    /// 32. Codex, OpenCode and Pi took 300 in one write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_presses_per_write: Option<u32>,
    /// How long to wait between two writes of one clear, in milliseconds;
    /// `None` where back-to-back writes were measured to work. Claude Code
    /// reads two 32-press writes sent back to back as ONE read of 64, which it
    /// ignores whole like a single write of 64, so its writes are 1 s apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_between_writes_ms: Option<u32>,
}

/// What one [`ClearKey`] press removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearPresses {
    /// One press per WRAPPED screen row (Claude Code): a prompt of `n`
    /// characters in a pane `cols` wide needs `ceil(n / cols)` presses, plus
    /// one per newline in a multi-line draft.
    PerWrappedRow,
    /// One press per logical line (Codex, OpenCode, Pi): a single-line prompt
    /// needs one press however long it is, and a multi-line draft two per line.
    PerLine,
    /// Forward-compat catch-all: a rule a NEWER daemon names that this build
    /// does not know. A client cannot count presses for it and must treat the
    /// key as unsupported. Never produced by this build.
    #[serde(other)]
    Unknown,
}

/// How [`PromptKeys::delete_char`] removes text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteCharKey {
    /// The key that deletes one character. `DEL` (`0x7f`) in every measured
    /// agent; bursts of 150 in one write were exact.
    pub bytes: Cow<'static, str>,
    /// The longest single unbracketed write, in characters, that this agent is
    /// measured to keep as typed text — so that pressing [`Self::bytes`] once
    /// per character removes it exactly. Above it the agent collapses the write
    /// into a paste placeholder that ONE delete removes whole: Claude Code
    /// above 800 characters (`[Pasted text #N]`), Codex above 1000 (`[Pasted
    /// Content …]`). `None` where no collapse was observed (OpenCode and Pi
    /// kept 2290 characters as text).
    ///
    /// This is the AGENT's measured figure, not voice's policy. Voice refuses
    /// to remove a write longer than 800 characters for every agent (PRD #1541
    /// M1), so a sender applies `min(800, this)` — the per-agent value exists so
    /// that policy is a client decision that can be revisited against the
    /// deck's own measurement rather than baked into the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_literal_write_chars: Option<u32>,
}

/// `ESC`: the interrupt key in every measured agent.
const ESC: &str = "\x1b";
/// `Ctrl+U` (NAK): deletes to the start of the line in every measured agent.
const CTRL_U: &str = "\x15";
/// `DEL`: deletes one character in every measured agent.
const DEL: &str = "\x7f";

/// The single `ESC` that interrupts Claude Code, Codex and Pi.
static SINGLE_ESC: [KeyStep; 1] = [KeyStep {
    bytes: Cow::Borrowed(ESC),
    pause_after_ms: 0,
}];

/// OpenCode's interrupt: the first `ESC` only arms "esc again to interrupt",
/// the second, ~300 ms later, interrupts.
static OPENCODE_DOUBLE_ESC: [KeyStep; 2] = [
    KeyStep {
        bytes: Cow::Borrowed(ESC),
        pause_after_ms: 300,
    },
    KeyStep {
        bytes: Cow::Borrowed(ESC),
        pause_after_ms: 0,
    },
];

/// PRD #20 finding #15: an integration-hook handler (install / uninstall) —
/// `Ok(())` on success, `Err(message)` on a reported failure.
pub type HookFn = fn() -> Result<(), String>;

/// PRD #20 finding #15: a materialize handler (the `Extension` strategy).
/// Receives an env so it can honor `HOME` / deck env vars. No production code
/// calls it through [`AgentSpec::materialize`]: Pi's extension is materialized
/// once at daemon startup instead (PRD #201, the #210 fix — see the note in
/// `AgentPtyRegistry::spawn_agent_with_dir`).
pub type MaterializeFn = fn(&[(String, String)]);

/// One cohesive registry entry per agent — the single place per-agent data
/// lives (PRD #20 success criteria).
#[derive(Debug)]
pub struct AgentSpec {
    /// The typed identity this entry is keyed by.
    pub agent_type: AgentType,
    /// Human-facing label shown on cards / in the `Display` impl.
    pub label: &'static str,
    /// Binary basenames that resolve to this agent in
    /// [`crate::event::AgentType::from_command`]. Empty for the neutral
    /// [`NONE`] placeholder (it is never detected from a command).
    pub detect_basenames: &'static [&'static str],
    /// The canonical command that launches this agent, if it has one. `None`
    /// for the neutral placeholder.
    pub default_command: Option<&'static str>,
    /// Which integration mechanism carries this agent's events to the deck.
    /// `None` for the neutral placeholder (not a real agent).
    pub strategy: Option<IntegrationStrategy>,
    /// Per-agent badge colour. Populated now as the single source of truth even
    /// though rendering coloured badges on cards is a later PRD #20 milestone.
    /// A named ANSI colour only (no absolute `Color::Rgb`), matching the
    /// palette policy (`src/palette.rs`) so terminal themes can remap it.
    pub badge_color: Color,
    /// PRD #20 finding #15: this agent's OWN integration handlers, so strategy
    /// dispatch resolves from the SPEC rather than a hardcoded incumbent module
    /// keyed by the [`IntegrationStrategy`] enum. Before this, every `NativeHooks`
    /// entry ran Claude's installer, every `Plugin` ran OpenCode's, and every
    /// `Extension` materialized Pi's — so a FUTURE agent reusing one of those
    /// strategies would run another agent's implementation. With the handler on
    /// the spec, `main.rs` / `agent_pty.rs` call `spec(x).hook_install` etc. and
    /// a new agent slots in its own handlers here. `None` where the agent's
    /// strategy has no such action (a Wrapper agent has no hook installer or
    /// extension materialize; the neutral placeholder has none at all).
    pub hook_install: Option<HookFn>,
    pub hook_uninstall: Option<HookFn>,
    /// Materialize a bundled artifact into the agent's HOME (the `Extension`
    /// strategy). Capability metadata: the daemon materializes Pi's extension
    /// once at startup, and the spawn path calls no materialize (#210).
    pub materialize: Option<MaterializeFn>,
    /// PRD #20 R20-010: this agent's OWN startup auto-install action — the
    /// silent, best-effort install the TUI runs at launch for every shipped
    /// agent. Before this, startup dispatched by matching the reusable
    /// [`IntegrationStrategy`] enum to a hardcoded incumbent
    /// (`NativeHooks` → Claude's installer, `Plugin` → OpenCode's), so a FUTURE
    /// agent reusing one of those strategies would run another agent's
    /// installer. With the action on the spec, `main.rs` iterates [`ALL`] and
    /// calls `spec.startup_auto_install` directly, and a new agent slots in its
    /// own installer here. `None` where the agent has no startup install step —
    /// an `Extension` (Pi's is materialized once at daemon startup, outside
    /// this loop), a `Wrapper` (Codex
    /// synthesizes events from stdout), or the neutral placeholder.
    ///
    /// Issue #1637: it returns what the agent's hooks are pinned to afterwards,
    /// so `daemon serve` can name the binary an agent's events come from before
    /// any hook fires. The TUI ignores it.
    pub startup_auto_install: Option<fn() -> Vec<crate::hook_binary::HookPin>>,
    /// Issue #1637: what this agent's hook config pins NOW, read back without
    /// installing, writing or taking over anything, for the daemon's periodic
    /// [`crate::hook_binary::PinRefresh`]. `Some(pins)` when the config was
    /// read, an empty list meaning it names no deck binary; `None` when it
    /// could not be read (missing, unparseable, unreadable, or the agent is
    /// not installed), which leaves the pins the daemon already knows. `None`
    /// for an agent with no startup install step.
    pub configured_pins: Option<fn() -> Option<Vec<crate::hook_binary::HookPin>>>,
    /// PRD #1666 part 3, R-3: [`Self::startup_auto_install`]'s own install,
    /// run for `hooks refresh`, saying whether the agent is installed here and
    /// what the install could not read or write, which the startup install
    /// only logs. `None` exactly where `startup_auto_install` is.
    pub refresh_install: Option<fn() -> crate::hooks_refresh::InstallAttempt>,
    /// PRD #1541: the keys that interrupt this agent's turn and edit its
    /// prompt, as measured against the versions [`PromptKeys`] names. `None`
    /// where they are unmeasured (Devin) or there is no agent to press them at
    /// (the neutral placeholder). See [`PromptKeys`].
    pub prompt_keys: Option<PromptKeys>,
    /// PRD #1666 S7 (decision A): recognises this agent's empty composer in
    /// the pane's screen rows, for an agent whose wrapper reports it Idle while
    /// a start dialog may still be up. Until the agent's own hooks report, the
    /// doorbell writes no byte unless this matches
    /// ([`crate::agent_pty::AgentPtyRegistry::write_doorbell`]). `None` where
    /// the agent's status comes only from its own hooks, so it reads Unknown,
    /// and is never rung, until they report.
    pub doorbell_composer: Option<fn(&[String]) -> bool>,
    /// Recognises, in the composer [`Self::doorbell_composer`] guards, the
    /// line the doorbell last typed into this pane, left unsubmitted because
    /// its Enter was not taken. Such a screen is not empty, so without this
    /// the bare CR that flushes the line would wait for an empty composer
    /// forever. Called with the screen rows, the cursor (as
    /// [`crate::pane_screen_text::visible_rows_and_cursor`] reports them)
    /// and that exact line. `Some` exactly where `doorbell_composer` is.
    pub doorbell_stranded_line: Option<StrandedLineRecogniser>,
    /// PRD #1666 part 2 (`inbox/doorbell/008`): recognises, in the pane's
    /// screen rows, this agent's prompt being up, for an agent whose own
    /// `SessionStart` arrives before it is ([`crate::state::ReadinessLevel`]).
    /// Until the generation's own hooks report Idle, the doorbell writes no
    /// byte to it unless this matches. `None` where the agent's own
    /// `SessionStart` is readiness enough, or where [`Self::doorbell_composer`]
    /// already guards every write.
    pub doorbell_ready_screen: Option<fn(&[String]) -> bool>,
    /// PRD #1666 part 3, Q28: when this agent's own hooks first report, which
    /// decides what a generation that has not reported yet needs before the
    /// doorbell may ring it ([`FirstHook`]).
    pub first_hook: FirstHook,
    /// PRD #1631: the options this agent draws on screen but never reports in
    /// a payload — its permission and plan menus, and the free-text entries it
    /// appends to a multiple-choice list — with the keys that pick each where
    /// the deck answers by keys. Read by the producers that build a
    /// [`crate::question::PendingQuestion`] and by the daemon's keys channel
    /// ([`crate::question::answer_keys`]). `None` for an agent with no such
    /// menus (Pi, whose dialogs report their own options) and the placeholder.
    pub questions: Option<&'static QuestionTables>,
}

/// PRD #1666 part 3, Q28 ([`AgentSpec::first_hook`]): when an agent's own
/// hooks first report, as measured. A generation whose own hooks have not
/// reported is [`crate::state::ReadinessLevel::Unproven`] either way, and
/// its own hooks stay the only thing that makes it more: this decides only
/// whether the doorbell may ring it before they do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstHook {
    /// Its own hooks report before its prompt takes input (Claude Code's
    /// `SessionStart`), or nothing measured says otherwise: a generation
    /// they have not reported for is never rung (`inbox/doorbell/008`).
    BeforePrompt,
    /// Its first own hook comes with its first prompt, so a generation
    /// nobody has typed to reports nothing at all (Codex 0.164.0 and
    /// 0.160.0, measured: `SessionStart` 338 ms and 449 ms after the first
    /// prompt's Enter). Such a generation may be rung on its own measured
    /// proof instead, which the doorbell's transaction checks on every ring
    /// whatever this says: an activated release (`Go`) it
    /// latched from its own start-up header, a Clean frame of that release's
    /// profile showing the empty ready composer, its own input-mode evidence
    /// and its settle anchor. A predecessor's report in the same pane is
    /// none of these.
    WithFirstPrompt,
}

/// [`AgentSpec::doorbell_stranded_line`]: screen rows, cursor, the line the
/// doorbell left.
pub type StrandedLineRecogniser =
    fn(&[String], crate::pane_screen_text::ScreenCursor, &str) -> bool;

/// PRD #1631: one agent's per-version option tables (decision 3). The deck
/// does not read question options off the screen (the doorbell reads only the
/// composer rows), so every option an agent draws without reporting it comes
/// from here, keyed on the versions it was verified on; the lane-2
/// `question/live/*` tests are what pin them.
#[derive(Debug)]
pub struct QuestionTables {
    /// The oldest version these tables are known to describe. A version below
    /// it lists the table's options keyboard-only and refuses the keys channel.
    /// An unknown version (no producer reports one today) is treated as covered:
    /// the lane-2 tests are the guard.
    pub verified_from: (u64, u64, u64),
    /// The newest version the tables were verified on, for the docs and for
    /// whoever next measures them.
    pub verified_on: (u64, u64, u64),
    pub menus: &'static [MenuTable],
}

impl QuestionTables {
    /// The options of `kind`, empty when the agent has no such menu.
    pub fn menu(&self, kind: MenuKind) -> &'static [TableOption] {
        self.menus
            .iter()
            .find(|m| m.menu == kind)
            .map(|m| m.options)
            .unwrap_or(&[])
    }

    /// Whether `version` is one these tables describe; `None` (not reported)
    /// is.
    pub fn covers(&self, version: Option<(u64, u64, u64)>) -> bool {
        version.is_none_or(|v| v >= self.verified_from)
    }
}

/// Which on-screen menu a [`MenuTable`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// A tool-permission prompt.
    Permission,
    /// The options an agent appends after a multiple-choice question's own.
    ChoiceAppended,
    /// Claude Code's plan approval.
    Plan,
}

#[derive(Debug)]
pub struct MenuTable {
    pub menu: MenuKind,
    /// In on-screen order.
    pub options: &'static [TableOption],
}

/// When a table option is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableWhen {
    Always,
    /// Claude Code's "always" option beside an `addDirectories` suggestion.
    SuggestionAddDirectories,
    /// Claude Code's "always" option beside a `setMode` suggestion.
    SuggestionSetMode,
    /// Claude Code's "always" option beside any other suggestion.
    SuggestionOther,
    /// Claude Code's "Yes, and switch to auto mode", which its Bash prompt
    /// draws in the default and accept-edits permission modes.
    AutoModeOffer,
}

#[derive(Debug)]
pub struct TableOption {
    /// The on-screen label. `{dir}`, `{mode}`, `{detail}` and `{patterns}` are
    /// filled from the payload.
    pub label: &'static str,
    pub role: crate::question::OptionRole,
    /// What the keys channel types to pick this option, with no Enter after it.
    /// `None` where the agent is answered another way, or where the keys are
    /// unknown.
    pub keys: Option<&'static str>,
    /// The deck shows this option but cannot pick it.
    pub keyboard_only: bool,
    /// The agent may or may not draw this option, on something its payload
    /// does not say ([`crate::question::QuestionOption::maybe_hidden`]).
    pub maybe_hidden: bool,
    /// For an "always" option: what it covers, for the confirmation.
    pub scope: Option<&'static str>,
    pub when: TableWhen,
}

const fn table_option(
    label: &'static str,
    role: crate::question::OptionRole,
    keys: Option<&'static str>,
) -> TableOption {
    TableOption {
        label,
        role,
        keys,
        keyboard_only: false,
        maybe_hidden: false,
        scope: None,
        when: TableWhen::Always,
    }
}

const fn keyboard_only(mut option: TableOption) -> TableOption {
    option.keyboard_only = true;
    option
}

const fn maybe_hidden(mut option: TableOption) -> TableOption {
    option.maybe_hidden = true;
    option
}

const fn scoped(mut option: TableOption, scope: &'static str) -> TableOption {
    option.scope = Some(scope);
    option
}

const fn when(mut option: TableOption, when: TableWhen) -> TableOption {
    option.when = when;
    option
}

use crate::question::OptionRole::{AllowAlways, AllowOnce, Choice, Deny, FreeText};

/// Claude Code, verified on 2.1.289 (PRD #1631 M1 and follow-up b). The
/// `PermissionRequest` hook it rests on is installed from 2.1.136
/// ([`crate::hooks_manage::PERMISSION_REQUEST_MIN_CLAUDE_VERSION`]). Permission
/// and choice answers go through the held hook, so they name no keys; plan
/// approval is answered by keys because a hook decision does not dismiss it.
pub static CLAUDE_CODE_QUESTIONS: QuestionTables = QuestionTables {
    verified_from: (2, 1, 136),
    verified_on: (2, 1, 289),
    menus: &[
        MenuTable {
            menu: MenuKind::Permission,
            options: &[
                table_option("Yes", AllowOnce, None),
                // The "always" options carry no `scope` here: what "always"
                // grants is the permission update the hook sends, so the
                // producer writes the confirmation's sentence from that update
                // itself (`crate::question::claude_update_scope`) and makes
                // the option keyboard-only when it cannot describe it.
                //
                // The labels are Claude Code's own on-screen text, kept short
                // on purpose: the `setMode` label drops the two parentheticals
                // Claude draws ("(auto-approve file edits and common file
                // commands)" and the "(shift+tab)" key hint) [observed text in
                // PRD #1631 follow-up b], which are hints rather than what the
                // option is. Claude draws "from this project" beside an
                // `addDirectories` suggestion whose destination is `session`
                // [observed]; the label keeps Claude's words and the
                // confirmation says what is actually sent.
                when(
                    table_option(
                        "Yes, and always allow access to {dir} from this project",
                        AllowAlways,
                        None,
                    ),
                    TableWhen::SuggestionAddDirectories,
                ),
                // `{mode}` is filled per mode; only "accept edits" was seen.
                when(
                    table_option(
                        "Yes, and switch to {mode} for this session",
                        AllowAlways,
                        None,
                    ),
                    TableWhen::SuggestionSetMode,
                ),
                // Label and index unverified: M1 saw only the two suggestion
                // kinds above.
                when(
                    table_option("Yes, and don't ask again", AllowAlways, None),
                    TableWhen::SuggestionOther,
                ),
                // Read from 2.1.296's own bundle (PR #1698): its Bash prompt
                // puts this after the "always" option and before No when the
                // permission mode is `default` or `acceptEdits`, auto mode is
                // available to the account, and neither a matched "ask" rule,
                // an organisation cap nor the command's display rules it out.
                // The payload carries only the mode, so the producer lists it
                // in those two modes for Bash, marked as one Claude may leave
                // out: where it does, No is drawn one number lower than the
                // deck's, so a client takes No by name, not number.
                // Keyboard-only: picking it switches Claude's mode on Claude's
                // side, which a hook decision cannot do.
                when(
                    maybe_hidden(keyboard_only(table_option(
                        "Yes, and switch to auto mode",
                        AllowAlways,
                        None,
                    ))),
                    TableWhen::AutoModeOffer,
                ),
                table_option("No", Deny, None),
            ],
        },
        MenuTable {
            menu: MenuKind::ChoiceAppended,
            options: &[
                table_option("Type something.", FreeText, None),
                keyboard_only(table_option("Chat about this", Choice, None)),
            ],
        },
        MenuTable {
            menu: MenuKind::Plan,
            options: &[
                // `1` is inferred from `2` [observed].
                scoped(
                    table_option("Yes, auto-accept edits", AllowAlways, Some("1")),
                    "file edits for the rest of this session (auto-accept)",
                ),
                table_option("Yes, manually approve edits", AllowOnce, Some("2")),
                keyboard_only(table_option("Tell Claude what to change", FreeText, None)),
            ],
        },
    ],
};

/// Codex, verified on 0.160.0 (PRD #1631 M1 and follow-up c). Answered by
/// keys: each acts without Enter.
pub static CODEX_QUESTIONS: QuestionTables = QuestionTables {
    verified_from: (0, 160, 0),
    verified_on: (0, 160, 0),
    menus: &[
        MenuTable {
            menu: MenuKind::Permission,
            options: &[
                table_option("Yes, proceed", AllowOnce, Some("1")),
                scoped(
                    table_option(
                        "Yes, and don't ask again for commands that start with the prefix shown",
                        AllowAlways,
                        Some("p"),
                    ),
                    "commands that start with the prefix Codex shows for `{detail}`",
                ),
                table_option("No, and tell Codex what to do differently", Deny, Some("3")),
            ],
        },
        MenuTable {
            menu: MenuKind::ChoiceAppended,
            // Its notes are typed after Tab, which is not measured.
            options: &[keyboard_only(table_option(
                "None of the above",
                FreeText,
                None,
            ))],
        },
    ],
};

/// OpenCode, verified on 1.18.34 (PRD #1631 M1 and follow-up d). Answered
/// through the plugin's reply API, so no keys.
pub static OPEN_CODE_QUESTIONS: QuestionTables = QuestionTables {
    verified_from: (1, 18, 34),
    verified_on: (1, 18, 34),
    menus: &[
        MenuTable {
            menu: MenuKind::Permission,
            options: &[
                table_option("Allow once", AllowOnce, None),
                scoped(
                    table_option("Allow always", AllowAlways, None),
                    "requests matching {patterns}",
                ),
                table_option("Reject", Deny, None),
            ],
        },
        MenuTable {
            menu: MenuKind::ChoiceAppended,
            options: &[table_option("Type your own answer", FreeText, None)],
        },
    ],
};

/// Devin 3000.11.3, from its documentation only: no logged-in Devin has run
/// it, so every entry here — the labels, their order and both keys — is
/// UNVERIFIED. Only Allow once and Deny are answerable.
pub static DEVIN_QUESTIONS: QuestionTables = QuestionTables {
    verified_from: (3000, 11, 3),
    verified_on: (3000, 11, 3),
    menus: &[MenuTable {
        menu: MenuKind::Permission,
        options: &[
            table_option("Allow once", AllowOnce, Some("\r")),
            keyboard_only(table_option("Allow for session", AllowAlways, None)),
            keyboard_only(table_option("Allow for project", AllowAlways, None)),
            keyboard_only(table_option("Allow for project (local)", AllowAlways, None)),
            keyboard_only(table_option("Allow globally", AllowAlways, None)),
            keyboard_only(table_option("Edit command", FreeText, None)),
            keyboard_only(table_option("Describe change to command", FreeText, None)),
            table_option("Deny", Deny, Some("\x1b")),
        ],
    }],
};

// PRD #20 finding #15: per-agent adapters that normalize each incumbent
// module's signature to the spec's handler shape. Keeping them here (as the
// values the statics point at) is what makes dispatch spec-resolved.
fn claude_install() -> Result<(), String> {
    crate::hooks_manage::install()
}
fn claude_uninstall() -> Result<(), String> {
    crate::hooks_manage::uninstall()
}
fn opencode_install() -> Result<(), String> {
    crate::opencode_manage::install().map_err(|e| e.to_string())
}
fn opencode_uninstall() -> Result<(), String> {
    crate::opencode_manage::uninstall().map_err(|e| e.to_string())
}
fn pi_materialize(env: &[(String, String)]) {
    crate::orchestrator_ext::auto_materialize(env);
}
fn devin_install() -> Result<(), String> {
    crate::devin_hooks_manage::install()
}
fn devin_uninstall() -> Result<(), String> {
    crate::devin_hooks_manage::uninstall()
}
/// PRD #20 §4.2.1: `dot-agent-deck hooks install --agent codex` — write the deck's
/// `hooks.json` into the active Codex home and record scoped, hash-pinned trust
/// for exactly those entries. The trust step is best-effort (it needs `codex` on
/// `PATH` to answer `hooks/list`): the definitions are what the command promises,
/// so a trust failure warns rather than failing the documented install.
fn codex_install() -> Result<(), String> {
    // PRD #381 Open Question 4, answered: this is not a spawn. It feeds
    // `install_to`, which PERSISTS the path into `~/.codex/hooks.json`, so it
    // resolves a durable path or refuses — the refusal reaching the shell as a
    // non-zero exit with nothing written.
    codex_install_resolved(crate::platform::paths::durable_binary_path())
}

/// [`codex_install`] with the binary-path resolution injected (PRD #381 M6).
///
/// The refusal is checked BEFORE the Codex home is resolved, so a refusing
/// resolver makes this function touch the filesystem not at all — which is both
/// the property M6 asks for and what lets a test drive the branch without a
/// machine that has no durable deck.
fn codex_install_resolved(binary_path: Result<String, String>) -> Result<(), String> {
    let binary_path = binary_path?;
    let home = crate::codex_hooks_manage::active_codex_home()
        .ok_or_else(|| "no Codex home resolves (CODEX_HOME and HOME are both unset)".to_string())?;
    crate::codex_hooks_manage::install_to(&home, &binary_path).map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());
    // Trust exactly the command the install above wrote, for the binary path it
    // validated — never merely "something ending in the deck's verb" (#730).
    // The outcome is reported rather than discarded: a silent zero here is how a
    // Codex that stopped echoing our command byte-for-byte would look, and
    // `hooks install` used to exit 0 saying nothing about trust either way.
    //
    // **Each branch prints only what it knows** (issue #730, auditor S-C). This
    // one line is the whole of what a user sees about trust, so it may not name
    // a cause the code has not established: the old text said "Codex reported no
    // deck hook to trust" on *both* zeros, which is false on the one where Codex
    // reported one and `Exact` did not match it — the very case
    // `codex_hooks_manage`'s warn exists to surface. `TrustOutcome` carries that
    // distinction out precisely so this does not have to guess.
    //
    // The `NothingListed` arm says "no ELIGIBLE deck hook" for the same reason
    // (Greptile P2 on PR #1029). `deck_owned_entries` rejects a deck-signature
    // entry that is `isManaged` or whose `source_path` is not this home's own
    // `hooks.json`, and a listing containing only those lands here — so "Codex
    // reported no deck hook" would again assert a cause this branch cannot know.
    // Which of the three it was is not worth a fourth `TrustOutcome` variant;
    // not claiming the wrong one is.
    use crate::codex_hooks_manage::TrustOutcome;
    match crate::codex_hooks_manage::trust_deck_hooks_in(&home, &cwd, &binary_path) {
        Ok(TrustOutcome::NothingListed) => println!(
            "Trusted hooks: none (Codex reported no eligible deck hook to trust; events fall \
             back to stdout classification)"
        ),
        Ok(TrustOutcome::Unrecognised { listed }) => println!(
            "Trusted hooks: none (Codex reported {listed} deck-signature {}, but none carries \
             the command this install just wrote, so nothing could be trusted; events fall back \
             to stdout classification. Set DOT_AGENT_DECK_LOG to log the expected command.)",
            if listed == 1 { "entry" } else { "entries" }
        ),
        Ok(TrustOutcome::Trusted {
            count, turned_off, ..
        }) => {
            println!("Trusted hooks: {count}");
            // Issue #1027 item 2: the deck leaves a user's `/hooks` toggle alone,
            // so a hook they turned off stays off — and says so, on its own line
            // so the count above stays a bare number (`codex_hooks_005`).
            if !turned_off.is_empty() {
                let (noun, pronoun) = if turned_off.len() == 1 {
                    ("hook", "it")
                } else {
                    ("hooks", "them")
                };
                println!(
                    "Note: the deck's Codex {noun} for {} {} turned off in Codex's /hooks list, \
                     so Codex reports nothing through {pronoun}. The deck leaves {pronoun} off; \
                     turn {pronoun} back on in Codex's /hooks list to restore that detail.",
                    turned_off.join(", "),
                    if turned_off.len() == 1 { "is" } else { "are" },
                );
            }
        }
        // Say it on stderr as well as in the log. This is the one arm that
        // printed nothing at all, so the user got no trust line whatsoever and
        // exit 0 — and the log half needs `DOT_AGENT_DECK_LOG` to have been set
        // before the run. The definitions are written either way, which is why
        // this is not a failure.
        Err(e) => {
            tracing::warn!("codex hooks install: could not record scoped hook trust: {e}");
            eprintln!(
                "Warning: hook definitions were installed, but scoped hook trust could not be \
                 recorded ({e}); Codex events fall back to stdout classification"
            );
        }
    }
    Ok(())
}
/// PRD #20 §4.2.1: `dot-agent-deck hooks uninstall --agent codex`. Drops the deck's
/// trust records FIRST (while Codex can still enumerate the definitions), then
/// removes the deck's own rules from `hooks.json`, leaving user hooks alone.
fn codex_uninstall() -> Result<(), String> {
    let home = crate::codex_hooks_manage::active_codex_home()
        .ok_or_else(|| "no Codex home resolves (CODEX_HOME and HOME are both unset)".to_string())?;
    // The same two channels, and the same exit code, as the install arm above
    // (issue #1027, item 4). Fixing one of two identical arms and leaving the
    // other is not scoping: this arm reported through `tracing::warn!` and
    // nothing else, so on a machine without `DOT_AGENT_DECK_LOG` set the whole
    // of what the user saw was exit 0 and silence — while the command went on to
    // delete the definitions regardless. That leaves `[hooks.state]` rows whose
    // definitions are gone. Only a later install collects one, and only once
    // Codex lists the deck's hook again somewhere else (issue #1027 item 1, the
    // stale-record sweep in `trust_deck_hooks_in`); until then it stays.
    //
    // Not a failure, for the same reason the install arm is not: the primary
    // operation is the DEFINITIONS, and removing them still succeeds (or reports
    // its own error below). So this warns and the command continues, exiting 0
    // on an otherwise clean uninstall.
    //
    // Worded in the present tense on purpose — at this point the removal has not
    // happened yet and `uninstall_from` may still fail, so the message must not
    // claim it succeeded.
    if let Err(e) = crate::codex_hooks_manage::untrust_deck_hooks_in(&home) {
        tracing::warn!("codex hooks uninstall: could not drop scoped hook trust: {e}");
        eprintln!(
            "Warning: scoped hook trust could not be dropped ({e}); removal of the hook \
             definitions continues, so stale [hooks.state] records may be left behind in \
             Codex's config.toml"
        );
    }
    crate::codex_hooks_manage::uninstall_from(&home).map_err(|e| e.to_string())
}

/// Claude Code — native-hooks strategy (shipped).
pub static CLAUDE_CODE: AgentSpec = AgentSpec {
    agent_type: AgentType::ClaudeCode,
    label: "ClaudeCode",
    detect_basenames: &["claude"],
    default_command: Some("claude"),
    strategy: Some(IntegrationStrategy::NativeHooks),
    // Claude Code posts its native `SessionStart` early in boot, before any
    // prompt — the delegate gate's healthy fast path (3.80-4.39 s end to end
    // on the production samples in #243).
    badge_color: Color::LightMagenta,
    hook_install: Some(claude_install),
    hook_uninstall: Some(claude_uninstall),
    materialize: None,
    startup_auto_install: Some(crate::hooks_manage::auto_install),
    configured_pins: Some(crate::hooks_manage::configured_pins),
    refresh_install: Some(crate::hooks_manage::refresh_install),
    // PRD #1541, measured on Claude Code 2.1.289: one ESC interrupts; one
    // Ctrl+U per WRAPPED row, and a single write of 64+ is ignored, so 32 per
    // write; an unbracketed write over 800 characters collapses to a paste.
    prompt_keys: Some(PromptKeys {
        interrupt: Cow::Borrowed(&SINGLE_ESC),
        clear: ClearKey {
            bytes: Cow::Borrowed(CTRL_U),
            presses: ClearPresses::PerWrappedRow,
            max_presses_per_write: Some(32),
            pause_between_writes_ms: Some(1000),
        },
        delete_char: DeleteCharKey {
            bytes: Cow::Borrowed(DEL),
            max_literal_write_chars: Some(800),
        },
    }),
    doorbell_composer: None,
    doorbell_stranded_line: None,
    // PRD #1666 part 2: its `SessionStart` arrives early in boot (above),
    // before its prompt is drawn, so a generation that has not yet reported
    // Idle is rung only once that prompt is on screen.
    doorbell_ready_screen: Some(claude_prompt_on_screen),
    first_hook: FirstHook::BeforePrompt,
    questions: Some(&CLAUDE_CODE_QUESTIONS),
};

/// PRD #1666 part 2 (`inbox/doorbell/008`): do `rows`, one trimmed screen row
/// each, show Claude Code's prompt up — a row opening with its `❯` (or `>`)
/// marker that is not a numbered menu option, a rule of `─` directly above it,
/// and another below it within [`CLAUDE_PROMPT_MAX_ROWS`] rows, as Claude Code
/// 2.1.300 draws its input box. The rows are read as
/// [`crate::pane_screen_text::visible_rows_and_cursor`] gives them, which
/// shortens a rule to a few characters, so a rule is any row of `─` alone. Says only that the prompt is drawn, not that
/// it is empty: the doorbell's draft tracking guards a draft. A dialog whose
/// selected option opens with the same marker is a numbered option and does
/// not match.
pub fn claude_prompt_on_screen(rows: &[String]) -> bool {
    let rule = |row: &str| !row.is_empty() && row.chars().all(|c| c == '─');
    rows.iter().enumerate().skip(1).any(|(at, row)| {
        let Some(rest) = row.strip_prefix('❯').or_else(|| row.strip_prefix('>')) else {
            return false;
        };
        let option = rest
            .trim_start()
            .split_once('.')
            .is_some_and(|(number, _)| {
                !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
            });
        !option
            && rule(&rows[at - 1])
            && rows[at + 1..]
                .iter()
                .take(CLAUDE_PROMPT_MAX_ROWS)
                .any(|below| rule(below))
    })
}

/// How many rows below its marker [`claude_prompt_on_screen`] looks for the
/// rule that closes Claude Code's prompt: a draft that wraps pushes it down.
const CLAUDE_PROMPT_MAX_ROWS: usize = 8;

/// OpenCode — plugin strategy (shipped).
pub static OPEN_CODE: AgentSpec = AgentSpec {
    agent_type: AgentType::OpenCode,
    label: "OpenCode",
    detect_basenames: &["opencode"],
    default_command: Some("opencode"),
    strategy: Some(IntegrationStrategy::Plugin),
    // MEASURED (#146, `opencode 1.18.16`): nothing on the plugin bus before the
    // prompt. 35 s of idle cold boot produced zero `session.*` events, and
    // `session.created` then arrived 16 ms AFTER the submit — caused by the very
    // prompt the gate withholds. `server.connected` is NOT the missing event: it
    // is synthesized as the first frame of the SSE `/event` response, never
    // reaches the plugin hook, and would fire on the deck's own connect.
    badge_color: Color::LightGreen,
    hook_install: Some(opencode_install),
    hook_uninstall: Some(opencode_uninstall),
    materialize: None,
    startup_auto_install: Some(crate::opencode_manage::auto_install),
    configured_pins: Some(crate::opencode_manage::configured_pins),
    refresh_install: Some(crate::opencode_manage::refresh_install),
    // PRD #1541, measured on OpenCode 1.18.34: ESC, ~300 ms, ESC interrupts;
    // one Ctrl+U per line; no paste collapse of an unbracketed write.
    prompt_keys: Some(PromptKeys {
        interrupt: Cow::Borrowed(&OPENCODE_DOUBLE_ESC),
        clear: ClearKey {
            bytes: Cow::Borrowed(CTRL_U),
            presses: ClearPresses::PerLine,
            max_presses_per_write: None,
            pause_between_writes_ms: None,
        },
        delete_char: DeleteCharKey {
            bytes: Cow::Borrowed(DEL),
            max_literal_write_chars: None,
        },
    }),
    doorbell_composer: None,
    doorbell_stranded_line: None,
    doorbell_ready_screen: None,
    first_hook: FirstHook::BeforePrompt,
    questions: Some(&OPEN_CODE_QUESTIONS),
};

/// Pi — bundled-extension strategy (shipped, PRD #201).
pub static PI: AgentSpec = AgentSpec {
    agent_type: AgentType::Pi,
    label: "Pi",
    detect_basenames: &["pi"],
    default_command: Some("pi"),
    strategy: Some(IntegrationStrategy::Extension),
    badge_color: Color::LightCyan,
    hook_install: None,
    hook_uninstall: None,
    materialize: Some(pi_materialize),
    // Pi's extension is materialized at DAEMON startup
    // (`orchestrator_ext::auto_materialize`, called by `daemon serve`), not by
    // a startup install step, and refreshed per spawn only for a spawn detected
    // as Pi (`agent_pty::pi_spawn`, PRD #1666 G6; #210).
    startup_auto_install: None,
    configured_pins: None,
    refresh_install: None,
    // PRD #1541, measured on Pi 0.87.1: one ESC interrupts; one Ctrl+U per
    // line; no paste collapse of an unbracketed write.
    prompt_keys: Some(PromptKeys {
        interrupt: Cow::Borrowed(&SINGLE_ESC),
        clear: ClearKey {
            bytes: Cow::Borrowed(CTRL_U),
            presses: ClearPresses::PerLine,
            max_presses_per_write: None,
            pause_between_writes_ms: None,
        },
        delete_char: DeleteCharKey {
            bytes: Cow::Borrowed(DEL),
            max_literal_write_chars: None,
        },
    }),
    doorbell_composer: None,
    doorbell_stranded_line: None,
    doorbell_ready_screen: None,
    first_hook: FirstHook::BeforePrompt,
    questions: None,
};

/// Codex — stdout-wrapper strategy (PRD #20 M7). The first agent to use the
/// [`IntegrationStrategy::Wrapper`] mechanism: `dot-agent-deck wrap -- codex …`
/// tees Codex's stdout through pattern detection into `AgentEvent`s. Its badge
/// colour is a distinct named ANSI colour (LightYellow) — not reused by Claude
/// (LightMagenta), OpenCode (LightGreen), or Pi (LightCyan), and never the
/// neutral [`NONE`] DarkGray.
pub static CODEX: AgentSpec = AgentSpec {
    agent_type: AgentType::Codex,
    label: "Codex",
    detect_basenames: &["codex"],
    default_command: Some("codex"),
    strategy: Some(IntegrationStrategy::Wrapper),
    // Codex's NATIVE `SessionStart` fires when the first turn starts, i.e. after
    // a prompt is submitted (measured on 0.145.0 and still true on 0.149.0), so
    // it is useless as a gate. The wrapper that hosts its PTY announces the
    // child's interface instead (#243).
    badge_color: Color::LightYellow,
    // Codex is a HYBRID (PRD #20 W1): the wrapper is its PTY host, but its rich
    // events come from Codex's Claude-Code-compatible NATIVE hooks — so unlike a
    // pure stdout wrapper it does have hook install/uninstall handlers (the
    // documented `dot-agent-deck hooks install --agent codex`), and they also
    // record/drop the scoped, hash-pinned trust those hooks need to run.
    hook_install: Some(codex_install),
    hook_uninstall: Some(codex_uninstall),
    materialize: None,
    // PRD #20 §4.2.1: install + trust ONCE at startup, command-agnostically, so
    // Codex hooks fire however Codex is launched — including a launcher whose
    // basename isn't `codex` (`devbox run codex-big`), which the spawn-command
    // seam can't detect and which therefore got NO integration before.
    startup_auto_install: Some(crate::codex_hooks_manage::auto_install_and_trust_at_startup),
    configured_pins: Some(crate::codex_hooks_manage::configured_pins),
    refresh_install: Some(crate::codex_hooks_manage::refresh_install),
    // PRD #1541, measured on Codex 0.160.0: one ESC interrupts; one Ctrl+U per
    // line; an unbracketed write over 1000 characters collapses to a paste.
    prompt_keys: Some(PromptKeys {
        interrupt: Cow::Borrowed(&SINGLE_ESC),
        clear: ClearKey {
            bytes: Cow::Borrowed(CTRL_U),
            presses: ClearPresses::PerLine,
            max_presses_per_write: None,
            pause_between_writes_ms: None,
        },
        delete_char: DeleteCharKey {
            bytes: Cow::Borrowed(DEL),
            max_literal_write_chars: Some(1000),
        },
    }),
    // PRD #1666 S7: Codex's wrapper reports raw input while its start
    // dialogs are up, so the doorbell needs the composer itself.
    doorbell_composer: Some(codex_composer_on_screen),
    doorbell_stranded_line: Some(codex_composer_holds_doorbell_line),
    doorbell_ready_screen: None,
    // PRD #1666 part 3, Q28: its SessionStart comes with its first prompt.
    first_hook: FirstHook::WithFirstPrompt,
    questions: Some(&CODEX_QUESTIONS),
};

/// PRD #1666 S7 (decision A): whether `rows` — a pane's screen, one trimmed
/// row each — show Codex's composer **empty**, as measured on Codex 0.160.0:
///
/// ```text
/// › Ask Codex to do anything
///
///   GPT-5.6-Luna low · /path/to/project
///   ? for shortcuts                                  ⚠ 1 warning · f2 to view
/// ```
///
/// The footer must be the last row with text and hold `? for shortcuts`,
/// either at its start or after a `· ` separator (measured on a real 0.160.0
/// after a turn: `← for agents · ? for shortcuts`). A `›` prompt row that is
/// not a numbered choice must sit at most
/// [`CODEX_COMPOSER_MAX_ROWS_ABOVE_FOOTER`] rows above it, holding nothing or
/// one of Codex's placeholders ([`CODEX_COMPOSER_PLACEHOLDERS`]), with a
/// blank row right below it. Codex's start dialogs (update, folder trust,
/// hooks review) put `›` on a numbered choice and end in a key-hint footer
/// instead, so none of them matches; nor does any screen this does not know.
///
/// **Empty, not merely present** (decision 2): Codex can keep a human's text
/// in the composer after an Enter it took as a newline (a paste burst), so
/// the input tracker can believe a draft was submitted while it is still on
/// screen. Any text on the prompt row other than a placeholder, or a second
/// composer row below it, is a draft the doorbell must not join.
///
/// **This is the one place that knows what Codex's composer looks like.** A
/// Codex release that redraws it makes this return `false`, and the effect is
/// the safe one: the doorbell holds, and queued messages wait for the `Stop`
/// hand-over or for a recognised empty composer, never typed into a screen
/// the deck does not recognise. Re-measure and update this function, and the
/// fixture `tests/fixtures/codex-inbox/composer.txt`, together.
pub fn codex_composer_on_screen(rows: &[String]) -> bool {
    let Some(footer) = rows.iter().rposition(|row| !row.is_empty()) else {
        return false;
    };
    let footer_row = &rows[footer];
    if !(footer_row.starts_with("? for shortcuts") || footer_row.contains("· ? for shortcuts")) {
        return false;
    }
    let first = footer.saturating_sub(CODEX_COMPOSER_MAX_ROWS_ABOVE_FOOTER);
    let Some(prompt) = (first..footer).rev().find(|&n| {
        rows[n].strip_prefix('›').is_some_and(|rest| {
            let rest = rest.trim_start();
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            !(digits > 0 && rest[digits..].starts_with('.'))
        })
    }) else {
        return false;
    };
    let content = rows[prompt].strip_prefix('›').unwrap_or_default().trim();
    let empty = content.is_empty() || CODEX_COMPOSER_PLACEHOLDERS.contains(&content);
    empty && rows.get(prompt + 1).is_some_and(String::is_empty)
}

/// PRD #1666: do `rows` show Codex's composer holding `line` — the doorbell
/// line ([`crate::doorbell::doorbell_line`]) the deck last typed into this
/// pane — unsubmitted, in one of the two states measured on Codex 0.160.0.
/// The cursor is marked `▏`.
///
/// Its Enter was dropped, and the cursor sits right after the text (live
/// `codex/worker/001`: on the prompt row at column 95):
///
/// ```text
/// ›  dot-agent-deck: message m-k3xzq2ab-0f3c9a71d2e4b850 waiting, run: dot-agent-deck inbox fetch▏
///
///   GPT-5.6-Luna low · /path/to/project
///                                                    ⚠ 1 warning · f2 to view
/// ```
///
/// Or, under CPU pressure, its Enter became a newline in the composer, and
/// the cursor sits on the empty row directly below the text, at the
/// composer indent (live `inbox/load/004`: row 36, cell column 2, below the
/// line on row 35):
///
/// ```text
/// ›  dot-agent-deck: message m-k3xzq2ab-0f3c9a71d2e4b850 waiting, run: dot-agent-deck inbox fetch
///   ▏
///
///   GPT-5.6-Luna low · /path/to/project
///                                                    ⚠ 1 warning · f2 to view
/// ```
///
/// Codex hides `? for shortcuts` while its composer holds text, so the
/// composer is found from the bottom instead: the lowest `›` row that is not a
/// numbered choice, at most [`CODEX_STRANDED_LINE_MAX_ROWS_ABOVE_BOTTOM`] rows
/// above the last row with text. Its text, with any wrapped rows below it
/// joined up to the first blank row, must be `line` exactly, message id
/// included; a row with text must follow below the cursor (Codex's model
/// line); and the cursor must sit at the end of the composer's last text
/// row, or on the blank row directly below it at
/// [`CODEX_COMPOSER_INDENT`]. A human draft, an edited line, a
/// doorbell-shaped line naming another message and a dialog do not match,
/// and neither does `line` itself with the cursor anywhere else. `line` comes
/// from the doorbell's record of what it typed, so with no record the
/// doorbell does not ask.
///
/// **This narrows the match; it does not authenticate the screen.** Text
/// on a screen carries no record of who wrote it: a process that controls
/// what this pane renders can draw this exact frame, line and cursor, and a
/// human can type the same line by hand. Either earns the one bare CR the
/// flush sends, which goes to whatever then owns the pane's input. The
/// writer's other guards — the pane's agent identity, no unsent draft, and
/// the doorbell's admission re-checked before the byte — still apply.
pub fn codex_composer_holds_doorbell_line(
    rows: &[String],
    cursor: crate::pane_screen_text::ScreenCursor,
    line: &str,
) -> bool {
    let Some(bottom) = rows.iter().rposition(|row| !row.is_empty()) else {
        return false;
    };
    let first = bottom.saturating_sub(CODEX_STRANDED_LINE_MAX_ROWS_ABOVE_BOTTOM);
    let Some(prompt) = (first..=bottom).rev().find(|&n| {
        rows[n].strip_prefix('›').is_some_and(|rest| {
            let rest = rest.trim_start();
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            !(digits > 0 && rest[digits..].starts_with('.'))
        })
    }) else {
        return false;
    };
    let mut text = rows[prompt]
        .strip_prefix('›')
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut last = prompt;
    for row in rows[prompt + 1..].iter().take_while(|row| !row.is_empty()) {
        text.push(' ');
        text.push_str(row.trim());
        last += 1;
    }
    if text != line.trim() {
        return false;
    }
    // The Enter was dropped: the cursor right after the text.
    let after_text =
        last < bottom && cursor.row == last && cursor.through == rows[last].chars().count();
    // The Enter became a newline: the cursor on the composer's new, empty
    // row, which `take_while` above stopped at, so it is blank.
    let on_new_row =
        last + 1 < bottom && cursor.row == last + 1 && cursor.cell == CODEX_COMPOSER_INDENT;
    after_text || on_new_row
}

/// The cell column where Codex 0.160.0's composer text starts, after `› `,
/// and where it puts the cursor on an empty composer row (measured: cell
/// column 2 in live `inbox/load/004`).
pub const CODEX_COMPOSER_INDENT: usize = 2;

/// How far above the last row with text [`codex_composer_holds_doorbell_line`]'s
/// prompt row may be: the composer's wrapped rows, a blank row, the model
/// line and the footer.
const CODEX_STRANDED_LINE_MAX_ROWS_ABOVE_BOTTOM: usize = 6;

/// The texts Codex 0.160.0 shows in an empty composer: the first at start
/// and after a turn (measured), the second from the binary's strings
/// (unmeasured on screen). Anything else on the prompt row is a draft.
const CODEX_COMPOSER_PLACEHOLDERS: [&str; 2] =
    ["Ask Codex to do anything", "Ask a follow-up question"];

/// How far above [`codex_composer_on_screen`]'s footer the prompt row may be:
/// the prompt, a blank row, the model line, plus room for a wrapped prompt.
const CODEX_COMPOSER_MAX_ROWS_ABOVE_FOOTER: usize = 6;

/// Devin CLI — native-hooks strategy. The second agent to reuse
/// [`IntegrationStrategy::NativeHooks`], and the reason the strategy dispatch had
/// to move onto the spec (PRD #20 finding #15): its handlers below are its OWN
/// ([`crate::devin_hooks_manage`]), not Claude's.
///
/// Devin ships a Claude-Code-compatible hooks engine, so — like Codex — its
/// command hooks post the same stdin JSON shape Claude does and ride the existing
/// hook socket. Unlike Codex it needs neither a wrapper (its own TUI runs
/// directly on the deck's PTY) nor a hook-trust ceremony, which makes it the
/// cheapest possible registry addition: pure `NativeHooks` + one `hook.rs` arm.
pub static DEVIN: AgentSpec = AgentSpec {
    agent_type: AgentType::Devin,
    label: "Devin",
    detect_basenames: &["devin"],
    default_command: Some("devin"),
    strategy: Some(IntegrationStrategy::NativeHooks),
    // Devin documents a `SessionStart` hook and runs unwrapped, so the gate simply
    // waits for the genuine one.
    //
    // DOCUMENTED, NOT MEASURED — the one value here that is not (issue #243
    // review finding 3). Claude, OpenCode, Codex and Pi each rest on a boot-window
    // observation recorded next to them; this rests on Devin's own documentation
    // of the hook, with no measurement of WHEN in its boot the event actually
    // lands relative to the first prompt. It is the conservative classification,
    // so being wrong costs a delegate the 30 s fallback rather than a lost prompt
    // — which is why it ships unmeasured rather than as `Unknown`, and why a
    // future reader should not cite it as evidence the way the other four can be
    // cited. Measure it before treating this as established.
    // A named ANSI colour not used by Claude (LightMagenta), OpenCode
    // (LightGreen), Pi (LightCyan) or Codex (LightYellow), and never the neutral
    // DarkGray reserved for the "No agent" placeholder.
    badge_color: Color::LightBlue,
    hook_install: Some(devin_install),
    hook_uninstall: Some(devin_uninstall),
    materialize: None,
    startup_auto_install: Some(crate::devin_hooks_manage::auto_install),
    configured_pins: Some(crate::devin_hooks_manage::configured_pins),
    refresh_install: Some(crate::devin_hooks_manage::refresh_install),
    // PRD #1541: unsupported, not measured — Devin was logged out on the box
    // the key table was measured on, so none of its keys could be verified.
    prompt_keys: None,
    doorbell_composer: None,
    doorbell_stranded_line: None,
    doorbell_ready_screen: None,
    first_hook: FirstHook::BeforePrompt,
    questions: Some(&DEVIN_QUESTIONS),
};

/// Neutral entry for the "no recognized agent" placeholder. Not a real agent:
/// it has no detection basenames, no default command, and no integration
/// strategy. It exists so registry lookups ([`spec`]) are total — the `Display`
/// label path still resolves through here — and so unknown/`None` gets a
/// deliberate neutral badge colour rather than an accidental one.
pub static NONE: AgentSpec = AgentSpec {
    agent_type: AgentType::None,
    label: "No agent",
    detect_basenames: &[],
    default_command: None,
    strategy: None,
    // Not an agent: nothing is known about what is in the pane, so the gate keeps
    // its conservative wait.
    badge_color: Color::DarkGray,
    hook_install: None,
    hook_uninstall: None,
    materialize: None,
    startup_auto_install: None,
    configured_pins: None,
    refresh_install: None,
    // Not an agent: there is nothing to press keys at.
    prompt_keys: None,
    doorbell_composer: None,
    doorbell_stranded_line: None,
    doorbell_ready_screen: None,
    first_hook: FirstHook::BeforePrompt,
    questions: None,
};

/// All SHIPPED, detectable agents, in a stable order. Excludes the neutral
/// [`NONE`] placeholder — it is not a detectable agent and has no strategy to
/// dispatch. Detection and startup auto-install iterate this slice.
pub static ALL: &[&AgentSpec] = &[&CLAUDE_CODE, &OPEN_CODE, &PI, &CODEX, &DEVIN];

/// The registry entry for a given agent type. Total: every [`AgentType`]
/// variant — including the neutral [`AgentType::None`] — maps to an entry, so
/// callers never have to special-case the placeholder.
pub fn spec(agent_type: &AgentType) -> &'static AgentSpec {
    match agent_type {
        AgentType::ClaudeCode => &CLAUDE_CODE,
        AgentType::OpenCode => &OPEN_CODE,
        AgentType::Pi => &PI,
        AgentType::Codex => &CODEX,
        AgentType::Devin => &DEVIN,
        AgentType::None => &NONE,
    }
}

/// Resolve a binary basename to its agent type, or `None` if no shipped agent
/// claims it. Backs [`crate::event::AgentType::from_command`]: an unrecognized
/// basename yields `None` (the daemon then stores "type not known" rather than
/// misclassifying), exactly as the hand-written `match` did before this move.
pub fn detect_from_basename(basename: &str) -> Option<AgentType> {
    ALL.iter()
        .find(|spec| spec.detect_basenames.contains(&basename))
        .map(|spec| spec.agent_type.clone())
}

/// Resolve an EXPLICITLY DECLARED agent name to its type — the one rule every
/// declaration surface shares.
///
/// Two surfaces declare an agent by name rather than letting it be inferred
/// from the launched binary: `dot-agent-deck wrap --agent <name>` on the
/// command line (`crate::wrap`), and the `agent = "…"` key a role or mode
/// carries in `.dot-agent-deck.toml` (issue #308). Both exist for exactly the
/// same reason — a command like `devbox run codex-big` names a *launcher*, and
/// no amount of parsing can see the agent behind it — so both must answer the
/// same name the same way. One function, so `agent = "codex"` and
/// `wrap --agent codex` cannot drift apart.
///
/// Unlike [`detect_from_basename`], this never answers "unknown": an
/// unrecognized name resolves to the neutral [`AgentType::None`] rather than to
/// `None`. That distinction is the whole point of a declaration — the caller
/// said what this pane is, so falling back to guessing from the command would
/// silently overrule them. A typo therefore yields a pane with no agent (and,
/// for a config declaration, a `dot-agent-deck validate` warning naming the
/// unknown name) instead of a plausible-looking wrong one.
///
/// Matching is by detection basename and is deliberately EXACT — no trimming,
/// no case folding — because that is what `--agent` has always done and this
/// function is the shared implementation of it, not a new lenient sibling.
/// Callers that own a text field rather than an argv slot (the config surface)
/// trim before calling.
pub fn resolve_declared_agent(name: &str) -> AgentType {
    detect_from_basename(name).unwrap_or(AgentType::None)
}

/// Every agent name [`resolve_declared_agent`] accepts, in registry order, for
/// error messages that have to tell a user what they could have written. The
/// neutral [`NONE`] placeholder is excluded — it is not something anyone
/// declares.
pub fn declarable_agent_names() -> Vec<&'static str> {
    ALL.iter()
        .flat_map(|spec| spec.detect_basenames.iter().copied())
        .collect()
}

/// PRD #20 M9: resolve a `type:<alias>` dashboard-filter token to an agent
/// type, matching case-insensitively against either the agent's human [`label`]
/// (e.g. `type:codex`, `type:ClaudeCode`) or any of its detection basenames
/// (e.g. `type:claude`). Returns `None` for an unrecognized or empty alias so
/// the `/` filter (`src/ui.rs`) can treat `type:bogus` as "matches nothing".
///
/// Driven by [`ALL`] so every shipped agent is filterable and a future agent
/// needs no new filter code — the neutral [`NONE`] placeholder is excluded (it
/// is not a real, filterable agent).
///
/// [`label`]: AgentSpec::label
pub fn resolve_type_alias(alias: &str) -> Option<AgentType> {
    let alias = alias.trim();
    if alias.is_empty() {
        return None;
    }
    ALL.iter()
        .find(|spec| {
            spec.label.eq_ignore_ascii_case(alias)
                || spec
                    .detect_basenames
                    .iter()
                    .any(|basename| basename.eq_ignore_ascii_case(alias))
        })
        .map(|spec| spec.agent_type.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows the doorbell's gate reads for a fixture screen: one trimmed
    /// row per line, as `crate::pane_screen_text` produces them.
    fn rows(screen: &str) -> Vec<String> {
        screen.lines().map(|line| line.trim().to_string()).collect()
    }

    /// PRD #1666 part 2: the Claude Code prompt recogniser accepts the input
    /// box Claude Code 2.1.300 draws, empty or holding a draft that wraps, and
    /// refuses a boot banner with no box, a numbered dialog option under a
    /// rule, and a marker with no rule closing it — both on rows built here
    /// and on the rows the doorbell's writer reads off a rendered screen,
    /// which shorten each rule.
    #[test]
    fn claude_prompt_is_recognised_and_dialogs_are_not() {
        let rendered = |screen: &str| {
            crate::pane_screen_text::visible_rows_and_cursor(
                screen.replace('\n', "\r\n").as_bytes(),
                24,
                80,
            )
            .expect("parsed")
            .0
        };
        let rule = "─".repeat(78);
        let frame =
            format!("▐▛███▜▌   Claude Code v2.1.300\n\n{rule}\n❯\n{rule}\n  ? for shortcuts");
        assert!(claude_prompt_on_screen(&rendered(&frame)));
        assert!(!claude_prompt_on_screen(&rendered(&format!(
            "{rule}\n❯ 1. Yes, I trust this folder\n  2. No, exit\n{rule}"
        ))));
        let prompt =
            format!("▐▛███▜▌   Claude Code v2.1.300\n\n{rule}\n❯\n{rule}\n  ? for shortcuts");
        assert!(claude_prompt_on_screen(&rows(&prompt)));
        let wrapped = format!("{rule}\n❯ a draft that\n  wraps onto a second row\n{rule}\n");
        assert!(claude_prompt_on_screen(&rows(&wrapped)));
        assert!(!claude_prompt_on_screen(&rows(
            "▐▛███▜▌   Claude Code v2.1.300\nDOORBELL-STANDIN-READY"
        )));
        let dialog = format!("{rule}\n❯ 1. Yes, I trust this folder\n  2. No, exit\n{rule}\n");
        assert!(!claude_prompt_on_screen(&rows(&dialog)));
        let unclosed = format!("{rule}\n❯\n\n  ? for shortcuts");
        assert!(!claude_prompt_on_screen(&rows(&unclosed)));
    }

    /// PRD #1666 S7: the Codex composer recogniser accepts the measured 0.160.0
    /// composer, with or without the warning indicator on its footer row, and
    /// refuses every measured start dialog, an unknown dialog, a composer with
    /// output below it, and a blank screen.
    #[test]
    fn codex_composer_is_recognised_and_dialogs_are_not() {
        let composer = include_str!("../tests/fixtures/codex-inbox/composer.txt");
        assert!(codex_composer_on_screen(&rows(composer)));
        let warned = composer.replace(
            "? for shortcuts",
            "? for shortcuts                       ⚠ 1 warning · f2 to view",
        );
        assert!(codex_composer_on_screen(&rows(&warned)));
        // Measured on a real 0.160.0 after a turn.
        let agents = composer.replace("? for shortcuts", "← for agents · ? for shortcuts");
        assert!(codex_composer_on_screen(&rows(&agents)));
        let empty_line_draft =
            composer.replace("› Ask Codex to do anything\n", "›\n  typed below\n");
        assert!(!codex_composer_on_screen(&rows(&empty_line_draft)));
        for dialog in [
            include_str!("../tests/fixtures/codex-inbox/update.txt"),
            include_str!("../tests/fixtures/codex-inbox/folder.txt"),
            include_str!("../tests/fixtures/codex-inbox/hooks.txt"),
            include_str!("../tests/fixtures/codex-inbox/unknown.txt"),
        ] {
            assert!(!codex_composer_on_screen(&rows(dialog)), "{dialog}");
        }
        assert!(!codex_composer_on_screen(&rows(&format!(
            "{composer}\n  something printed below"
        ))));
        let numbered = composer.replace("› Ask Codex to do anything", "› 1. Ask Codex");
        assert!(!codex_composer_on_screen(&rows(&numbered)));
        assert!(!codex_composer_on_screen(&rows("\n\n")));
        assert!(!codex_composer_on_screen(&[]));
    }

    // Pure-data registry lookups — plain `#[test]` unit tests (no `#[spec]` /
    // CATALOG reproducer needed; these assert compiled-in data, not runtime
    // TUI behaviour).

    /// Every shipped agent's registry label equals what the `Display` impl
    /// rendered before centralisation, and the neutral placeholder is still
    /// "No agent". These strings are user-visible in card titles, so the move
    /// must not change them.
    #[test]
    fn labels_match_prior_display_strings() {
        assert_eq!(spec(&AgentType::ClaudeCode).label, "ClaudeCode");
        assert_eq!(spec(&AgentType::OpenCode).label, "OpenCode");
        assert_eq!(spec(&AgentType::Pi).label, "Pi");
        assert_eq!(spec(&AgentType::None).label, "No agent");

        // And the `Display` impl (src/ui.rs) now reads through the registry, so
        // formatting an AgentType yields the same label.
        assert_eq!(format!("{}", AgentType::ClaudeCode), "ClaudeCode");
        assert_eq!(format!("{}", AgentType::OpenCode), "OpenCode");
        assert_eq!(format!("{}", AgentType::Pi), "Pi");
        assert_eq!(format!("{}", AgentType::None), "No agent");
    }

    /// Detection through the registry reproduces the prior `from_command`
    /// mapping exactly: the three shipped binaries resolve to their types and
    /// everything else is unrecognized.
    #[test]
    fn detect_from_basename_matches_prior_mapping() {
        assert_eq!(detect_from_basename("claude"), Some(AgentType::ClaudeCode));
        assert_eq!(detect_from_basename("opencode"), Some(AgentType::OpenCode));
        assert_eq!(detect_from_basename("pi"), Some(AgentType::Pi));
        assert_eq!(detect_from_basename("sh"), None);
        assert_eq!(detect_from_basename("vim"), None);
        assert_eq!(detect_from_basename(""), None);
    }

    /// The public `from_command` entry point (event.rs) still infers the type
    /// from a full spawn command via the registry — same binary/path/arg
    /// handling as before.
    #[test]
    fn from_command_routes_through_registry() {
        assert_eq!(
            AgentType::from_command(Some("claude --dangerously-skip-permissions")),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            AgentType::from_command(Some("/usr/local/bin/pi run")),
            Some(AgentType::Pi)
        );
        assert_eq!(AgentType::from_command(Some("bash")), None);
    }

    /// PRD #20 R20-010: startup auto-install is resolved PER SPEC, not by
    /// mapping the reusable `IntegrationStrategy` enum to a hardcoded incumbent.
    /// The two agents with a startup install step (Claude native hooks, OpenCode
    /// plugin) carry an action; the `Extension` (Pi), the `Wrapper`
    /// (Codex), and the neutral placeholder carry `None` — so a future agent
    /// reusing `NativeHooks`/`Plugin` runs ITS OWN installer, never another
    /// agent's.
    /// Issue #1637: every agent whose startup install reports a hook pin has a
    /// read-back of the same config, so the daemon's periodic refresh sees a
    /// pin change for every agent whose startup pin it knows, and none for an
    /// agent it has none for.
    #[test]
    fn every_startup_install_has_a_configured_pins_read_back() {
        for spec in ALL {
            assert_eq!(
                spec.startup_auto_install.is_some(),
                spec.configured_pins.is_some(),
                "{:?}",
                spec.agent_type
            );
            // PRD #1666 part 3, R-3: and `hooks refresh` runs it.
            assert_eq!(
                spec.startup_auto_install.is_some(),
                spec.refresh_install.is_some(),
                "{:?}",
                spec.agent_type
            );
        }
    }

    #[test]
    fn startup_auto_install_is_resolved_per_spec() {
        assert!(
            spec(&AgentType::ClaudeCode).startup_auto_install.is_some(),
            "Claude installs its native hooks at startup"
        );
        assert!(
            spec(&AgentType::OpenCode).startup_auto_install.is_some(),
            "OpenCode installs its plugin at startup"
        );
        assert!(
            spec(&AgentType::Pi).startup_auto_install.is_none(),
            "Pi materializes its extension at spawn time, not startup"
        );
        assert!(
            spec(&AgentType::Codex).startup_auto_install.is_some(),
            "Codex installs its native hooks + scoped trust at startup, \
             command-agnostically (PRD #20 §4.2.1)"
        );
        assert!(
            spec(&AgentType::None).startup_auto_install.is_none(),
            "the neutral placeholder has no startup install step"
        );
    }

    /// Default commands match the prior per-agent launch commands; the neutral
    /// placeholder has none.
    #[test]
    fn default_commands_match_prior_behaviour() {
        assert_eq!(spec(&AgentType::ClaudeCode).default_command, Some("claude"));
        assert_eq!(spec(&AgentType::OpenCode).default_command, Some("opencode"));
        assert_eq!(spec(&AgentType::Pi).default_command, Some("pi"));
        assert_eq!(spec(&AgentType::None).default_command, None);
    }

    /// Each shipped agent names the integration mechanism it actually used
    /// before this move: Claude → native hooks, OpenCode → plugin, Pi →
    /// bundled extension. The neutral placeholder has no strategy.
    #[test]
    fn shipped_agents_map_to_expected_strategy() {
        assert_eq!(
            spec(&AgentType::ClaudeCode).strategy,
            Some(IntegrationStrategy::NativeHooks)
        );
        assert_eq!(
            spec(&AgentType::OpenCode).strategy,
            Some(IntegrationStrategy::Plugin)
        );
        assert_eq!(
            spec(&AgentType::Pi).strategy,
            Some(IntegrationStrategy::Extension)
        );
        assert_eq!(spec(&AgentType::None).strategy, None);
    }

    /// PRD #20 M7: Codex is the wrapper-strategy agent. It is the ONLY shipped
    /// agent that uses [`IntegrationStrategy::Wrapper`]; the others keep their
    /// own mechanisms (native hooks / plugin / bundled extension). This guards
    /// against a stray registry edit wiring another agent to the wrapper.
    #[test]
    fn only_codex_uses_wrapper_strategy() {
        assert_eq!(
            spec(&AgentType::Codex).strategy,
            Some(IntegrationStrategy::Wrapper)
        );
        for spec in ALL {
            if spec.agent_type == AgentType::Codex {
                continue;
            }
            assert_ne!(
                spec.strategy,
                Some(IntegrationStrategy::Wrapper),
                "only Codex should use the Wrapper strategy"
            );
        }
    }

    /// `ALL` holds exactly the shipped, detectable agents (the neutral
    /// placeholder is excluded), and each entry round-trips through detection.
    #[test]
    fn all_holds_shipped_agents_and_round_trips() {
        let types: Vec<&AgentType> = ALL.iter().map(|spec| &spec.agent_type).collect();
        assert_eq!(
            types,
            vec![
                &AgentType::ClaudeCode,
                &AgentType::OpenCode,
                &AgentType::Pi,
                &AgentType::Codex,
                &AgentType::Devin
            ]
        );
        assert!(!ALL.iter().any(|spec| spec.agent_type == AgentType::None));

        // Every shipped agent is detectable from at least one basename, and
        // that basename resolves back to the same type.
        for spec in ALL {
            let basename = spec
                .detect_basenames
                .first()
                .expect("a shipped agent must have a detection basename");
            assert_eq!(
                detect_from_basename(basename),
                Some(spec.agent_type.clone())
            );
        }
    }

    /// PRD #20 finding #15: strategy dispatch resolves from the SPEC's own
    /// handler, not a hardcoded incumbent keyed by the strategy enum. Each
    /// shipped agent carries exactly the handlers its strategy needs, a Wrapper
    /// agent and the neutral placeholder carry none, and two agents that share a
    /// hook-install shape resolve to DIFFERENT handlers — so a future agent
    /// reusing an existing strategy runs its own implementation, never another
    /// agent's module.
    #[test]
    fn strategy_handlers_resolve_from_spec_not_incumbent() {
        assert!(spec(&AgentType::ClaudeCode).hook_install.is_some());
        assert!(spec(&AgentType::ClaudeCode).hook_uninstall.is_some());
        assert!(spec(&AgentType::ClaudeCode).materialize.is_none());

        assert!(spec(&AgentType::OpenCode).hook_install.is_some());
        assert!(spec(&AgentType::OpenCode).hook_uninstall.is_some());
        assert!(spec(&AgentType::OpenCode).materialize.is_none());

        assert!(spec(&AgentType::Pi).materialize.is_some());
        assert!(spec(&AgentType::Pi).hook_install.is_none());

        // Codex is a HYBRID (PRD #20 W1/§4.2.1): the Wrapper is its PTY host, but
        // its events come from NATIVE hooks, so it does carry hook handlers (the
        // documented `hooks install --agent codex`) — while still materializing no
        // extension. The neutral placeholder carries neither.
        assert!(spec(&AgentType::Codex).hook_install.is_some());
        assert!(spec(&AgentType::Codex).hook_uninstall.is_some());
        assert!(spec(&AgentType::Codex).materialize.is_none());
        assert!(spec(&AgentType::None).hook_install.is_none());
        assert!(spec(&AgentType::None).materialize.is_none());

        // Claude, OpenCode, and Codex install through DIFFERENT handlers — proof
        // the handler is sourced per-spec, not from one per-strategy incumbent.
        let claude = spec(&AgentType::ClaudeCode)
            .hook_install
            .expect("Claude has an installer");
        let opencode = spec(&AgentType::OpenCode)
            .hook_install
            .expect("OpenCode has an installer");
        let codex = spec(&AgentType::Codex)
            .hook_install
            .expect("Codex has an installer");
        assert!(
            !std::ptr::fn_addr_eq(claude, opencode)
                && !std::ptr::fn_addr_eq(claude, codex)
                && !std::ptr::fn_addr_eq(opencode, codex),
            "each agent must resolve to its OWN installer, not a shared incumbent"
        );
    }

    /// The badge colour field is populated for every entry (single source of
    /// truth for the later badge-rendering milestone), and the neutral
    /// placeholder gets a deliberately neutral colour distinct from the real
    /// agents'.
    #[test]
    fn badge_colours_present_and_neutral_for_none() {
        assert_eq!(spec(&AgentType::None).badge_color, Color::DarkGray);
        for spec in ALL {
            assert_ne!(
                spec.badge_color,
                Color::DarkGray,
                "a shipped agent's badge should not reuse the neutral placeholder colour"
            );
        }
    }

    fn keys(agent_type: AgentType) -> &'static PromptKeys {
        spec(&agent_type)
            .prompt_keys
            .as_ref()
            .unwrap_or_else(|| panic!("{agent_type:?} should carry prompt keys"))
    }

    fn step(bytes: &'static str, pause_after_ms: u32) -> KeyStep {
        KeyStep {
            bytes: Cow::Borrowed(bytes),
            pause_after_ms,
        }
    }

    /// PRD #1541: each measured agent's keys, pinned by value against the
    /// verified table in the PRD. A change here is a re-measurement — update
    /// the versions in [`PromptKeys`]'s doc comment with it.
    #[test]
    fn prompt_keys_match_the_verified_table() {
        let claude = keys(AgentType::ClaudeCode);
        assert_eq!(claude.interrupt.as_ref(), &[step("\x1b", 0)]);
        assert_eq!(claude.clear.bytes, "\x15");
        assert_eq!(claude.clear.presses, ClearPresses::PerWrappedRow);
        assert_eq!(claude.clear.max_presses_per_write, Some(32));
        assert_eq!(claude.clear.pause_between_writes_ms, Some(1000));
        assert_eq!(claude.delete_char.bytes, "\x7f");
        assert_eq!(claude.delete_char.max_literal_write_chars, Some(800));

        let codex = keys(AgentType::Codex);
        assert_eq!(codex.interrupt.as_ref(), &[step("\x1b", 0)]);
        assert_eq!(codex.clear.bytes, "\x15");
        assert_eq!(codex.clear.presses, ClearPresses::PerLine);
        assert_eq!(codex.clear.max_presses_per_write, None);
        assert_eq!(codex.clear.pause_between_writes_ms, None);
        assert_eq!(codex.delete_char.bytes, "\x7f");
        assert_eq!(codex.delete_char.max_literal_write_chars, Some(1000));

        let opencode = keys(AgentType::OpenCode);
        assert_eq!(
            opencode.interrupt.as_ref(),
            &[step("\x1b", 300), step("\x1b", 0)],
            "OpenCode's first ESC only arms the interrupt"
        );
        assert_eq!(opencode.clear.bytes, "\x15");
        assert_eq!(opencode.clear.presses, ClearPresses::PerLine);
        assert_eq!(opencode.clear.max_presses_per_write, None);
        assert_eq!(opencode.clear.pause_between_writes_ms, None);
        assert_eq!(opencode.delete_char.bytes, "\x7f");
        assert_eq!(opencode.delete_char.max_literal_write_chars, None);

        let pi = keys(AgentType::Pi);
        assert_eq!(pi.interrupt.as_ref(), &[step("\x1b", 0)]);
        assert_eq!(pi.clear.bytes, "\x15");
        assert_eq!(pi.clear.presses, ClearPresses::PerLine);
        assert_eq!(pi.clear.max_presses_per_write, None);
        assert_eq!(pi.clear.pause_between_writes_ms, None);
        assert_eq!(pi.delete_char.bytes, "\x7f");
        assert_eq!(pi.delete_char.max_literal_write_chars, None);
    }

    /// PRD #1541: Devin is unsupported until measured logged in, and the
    /// neutral placeholder has no agent to press keys at — so neither carries
    /// keys, and a client refuses rather than guessing.
    #[test]
    fn devin_and_the_placeholder_have_no_prompt_keys() {
        assert!(spec(&AgentType::Devin).prompt_keys.is_none());
        assert!(spec(&AgentType::None).prompt_keys.is_none());
    }

    /// PRD #1541: no key sequence contains `Ctrl+C` (`0x03`), which quits Codex
    /// and OpenCode on an empty prompt. Checked over every entry, so a future
    /// agent's keys are held to it too.
    #[test]
    fn no_prompt_key_contains_ctrl_c() {
        let entries = ALL.iter().copied().chain(std::iter::once(&NONE));
        for agent in entries {
            let Some(keys) = agent.prompt_keys.as_ref() else {
                continue;
            };
            let sequences = keys
                .interrupt
                .iter()
                .map(|step| step.bytes.as_ref())
                .chain([keys.clear.bytes.as_ref(), keys.delete_char.bytes.as_ref()]);
            for bytes in sequences {
                assert!(
                    !bytes.is_empty(),
                    "{}: an empty key presses nothing",
                    agent.label
                );
                assert!(
                    !bytes.as_bytes().contains(&0x03),
                    "{}: a prompt key must never contain Ctrl+C",
                    agent.label
                );
            }
            assert!(
                !keys.interrupt.is_empty(),
                "{}: an interrupt needs at least one step",
                agent.label
            );
            assert_eq!(
                keys.interrupt.last().map(|step| step.pause_after_ms),
                Some(0),
                "{}: nothing follows the last interrupt step, so it waits for nothing",
                agent.label
            );
        }
    }

    /// PRD #1541 A-B1: every entry's keys are within the desktop's exact
    /// allowlist (`map_prompt_keys` in `desktop/src-tauri/src/dto.rs`), which
    /// drops a whole set that strays from it: one or two interrupt steps of a
    /// single `ESC` each, pausing 2 s at most in total; `Ctrl+U` to clear;
    /// `DEL` or `0x08` to delete. A new key here needs that allowlist widened
    /// in the same change, or the desktop refuses every voice prompt command
    /// for the agent.
    #[test]
    fn prompt_keys_stay_within_the_desktop_allowlist() {
        let entries = ALL.iter().copied().chain(std::iter::once(&NONE));
        for agent in entries {
            let Some(keys) = agent.prompt_keys.as_ref() else {
                continue;
            };
            assert!(
                (1..=2).contains(&keys.interrupt.len()),
                "{}: an interrupt is one or two steps",
                agent.label
            );
            for step in keys.interrupt.iter() {
                assert_eq!(
                    step.bytes, ESC,
                    "{}: each interrupt step is one ESC",
                    agent.label
                );
            }
            let pause: u64 = keys
                .interrupt
                .iter()
                .map(|step| u64::from(step.pause_after_ms))
                .sum();
            assert!(pause <= 2_000, "{}: interrupt pauses over 2 s", agent.label);
            assert_eq!(keys.clear.bytes, CTRL_U, "{}: the clear key", agent.label);
            assert!(
                [DEL, "\x08"].contains(&keys.delete_char.bytes.as_ref()),
                "{}: the delete key",
                agent.label
            );
            assert_ne!(keys.clear.presses, ClearPresses::Unknown);
        }
    }

    /// PRD #1541: the wire spelling of the keys — snake_case, the bytes as a
    /// JSON string, absent limits as absent keys — and a round trip back.
    #[test]
    fn prompt_keys_round_trip_through_json() {
        let claude = keys(AgentType::ClaudeCode);
        let value = serde_json::to_value(claude).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "interrupt": [{"bytes": "\u{1b}", "pause_after_ms": 0}],
                "clear": {"bytes": "\u{15}", "presses": "per_wrapped_row", "max_presses_per_write": 32, "pause_between_writes_ms": 1000},
                "delete_char": {"bytes": "\u{7f}", "max_literal_write_chars": 800},
            })
        );
        let back: PromptKeys = serde_json::from_value(value).expect("deserializes");
        assert_eq!(&back, claude);

        let pi = serde_json::to_value(keys(AgentType::Pi)).expect("serializes");
        assert!(pi["clear"].get("max_presses_per_write").is_none());
        assert!(pi["clear"].get("pause_between_writes_ms").is_none());
        assert!(pi["delete_char"].get("max_literal_write_chars").is_none());
    }

    /// PRD #1541: a clear rule a NEWER daemon names decodes as `Unknown`
    /// rather than failing the whole record — an older client must keep
    /// listing agents, and treats the key as unsupported.
    #[test]
    fn an_unknown_clear_rule_decodes_as_unknown() {
        let newer = serde_json::json!({
            "interrupt": [{"bytes": "\u{1b}"}],
            "clear": {"bytes": "\u{15}", "presses": "per_paragraph", "future_field": 1},
            "delete_char": {"bytes": "\u{7f}"},
        });
        let keys: PromptKeys = serde_json::from_value(newer).expect("a newer shape decodes");
        assert_eq!(keys.clear.presses, ClearPresses::Unknown);
        assert_eq!(keys.interrupt[0].pause_after_ms, 0);
    }

    /// PRD #381 Open Question 4 / M6: the `hooks install --agent codex` adapter
    /// persists the path it resolves, so a refusal must surface as `Err` — and
    /// must do so BEFORE anything on disk is consulted, which is why the check
    /// is the first statement of `codex_install_resolved`. This test touches no
    /// filesystem and no environment variable precisely because the refusal
    /// returns first; if that order ever regresses, this test starts reading the
    /// developer's real `~/.codex`.
    #[test]
    fn codex_install_surfaces_a_refused_binary_path_as_an_error() {
        let err = codex_install_resolved(Err("no durable dot-agent-deck".to_string()))
            .expect_err("a refused resolution must not report success");
        assert_eq!(
            err, "no durable dot-agent-deck",
            "the resolver's own message must reach the CLI verbatim, not be replaced by a \
             generic one"
        );
    }
}
