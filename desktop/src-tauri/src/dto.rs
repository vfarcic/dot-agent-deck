use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dot_agent_deck::agent_pty::{
    AgentRecord, TabMembership, clamp_pty_dims, is_valid_cwd, is_valid_display_name,
    is_valid_orchestration_cwd, is_valid_pane_id_env,
};
use dot_agent_deck::authoring_seeds::AuthoringKind;
use dot_agent_deck::daemon_client::Endpoint;
use dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;
use dot_agent_deck::event::{
    AgentType, ProjectListing, ProjectOrchestration, ResolvedProject, SendResult, Writable,
};
use dot_agent_deck::state::SessionStatus;
use serde::{Deserialize, Serialize};
use tauri::ipc::JavaScriptChannelId;

pub(crate) const TERMINAL_INPUT_MAX_BYTES: usize = 64 * 1024;
pub(crate) const COMMAND_MAX_BYTES: usize = 64 * 1024;
const AGENT_ID_MAX_BYTES: usize = 256;
const ERROR_MESSAGE_MAX_CHARS: usize = 2048;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSnapshot {
    pub connection: DesktopConnection,
    pub agents: Vec<DesktopAgent>,
    // PRD #819 M6: no `project_cwd`. It was `desktop_project_cwd()` — a
    // compile-time `CARGO_MANIFEST_DIR` guess falling back to the desktop
    // process's own `current_dir()` — and it answered a question this client is
    // no longer allowed to ask itself. Against a remote daemon it named a path
    // on the WRONG filesystem and nothing said so. The project a launch runs in
    // now comes from `list-projects` / `resolve-project`; the directory the
    // header shows comes from the daemon-reported agent `cwd` it already
    // preferred.
    /// Issue #887: the daemon's registered-schedule revision, copied through
    /// from the `ListAgents` reply
    /// ([`dot_agent_deck::daemon_protocol::AttachResponse::schedule_revision`]).
    ///
    /// It is a **change notice and nothing else** — no schedule reaches this
    /// snapshot, and none should: the webview shows no schedule surface. What it
    /// buys is the one seed of the daemon's project list the webview could not
    /// observe. `projectsRevision` in `App.tsx` already carries the connection
    /// status, the agent count and the agent/orchestration cwds; appending this
    /// closes the last seed that could move without the picker being able to
    /// notice, which is why a schedule registered while the app is open did not
    /// make the **Projects** picker re-list.
    ///
    /// Absent from a daemon that does not report one, in which case the picker
    /// behaves exactly as it did before — the manual **Refresh** button was
    /// always the remedy and remains one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_revision: Option<u64>,
    pub protocol_version: u32,
    pub source: &'static str,
    /// Every deck the app is observing, by [`DesktopConnection::deck_id`], in
    /// observed order with the SELECTED deck first (PRD #742 M5).
    ///
    /// # Why membership is on the wire rather than inferred
    ///
    /// M4 asked the same question of the frontend and found there is no signal
    /// to infer it from: a deck LEAVING the observed set produces no event at
    /// all. `apply_selection` ends the departed deck's watcher, and under
    /// `All` -> `local` the resolved deck does not move, so it emits nothing
    /// either. A webview that only upserts what arrives would keep that deck's
    /// agents on screen, frozen and looking live. M4 approximated it by
    /// resetting membership at `connect()`; this is the exact version, and it
    /// costs one `Vec<String>` on a snapshot that already carries an agent list.
    ///
    /// # Two invariants, both relied on by the webview
    ///
    /// **Selected first**, so `fleet[0]` answers "which deck are the
    /// single-deck surfaces bound to" — a question nothing on the stream could
    /// answer before, because under `All` every observed deck emits the same
    /// shape and the frontend had to re-learn it at `connect()`.
    ///
    /// **Never empty.** A deck the app cannot reach is still an entry here and
    /// still emits its own `disconnected` snapshot, because "no agents" and "we
    /// cannot see the agents" are different statements and an absent entry
    /// cannot tell them apart.
    ///
    /// Every deck's snapshot carries the same list — it is a property of the
    /// applied document, not of the deck that happened to emit — so a webview
    /// may prune on any arrival.
    pub fleet: Vec<String>,
    /// The members of [`Self::fleet`] the app cannot connect to, with what they
    /// need to be rendered (PRD #742 M12).
    ///
    /// A configured deck with no socket path yet has no watcher, so it never
    /// emits a snapshot of its own — and the webview deliberately does not
    /// invent an entry for an id in `fleet` it has heard nothing about, because
    /// for a real deck that would be a connection state nobody measured. This
    /// is the crate saying what it knows without measuring: the row exists, it
    /// has no address, and here is what to call it. The webview builds the
    /// group from these three fields rather than guessing at any of them.
    ///
    /// Carried on every snapshot for the same reason `fleet` is — it is a
    /// property of the applied document and not of the deck that emitted — so
    /// any arrival re-states the whole list and a webview may rebuild from it.
    pub unconfigured: Vec<UnconfiguredDeckDto>,
    /// The members of [`Self::fleet`] the app CONNECTS to, each NAMED (PRD
    /// #742 M14).
    ///
    /// # Why an id is not enough
    ///
    /// A deck joins [`Self::fleet`] the moment the document is applied and
    /// emits its own snapshot only once its watcher has established — a tunnel,
    /// a handshake and a `ListAgents` later. Between the two the webview knows
    /// the deck exists and knows nothing else about it, so it renders a group
    /// that has not reported yet rather than letting the fleet's own total
    /// climb under the reader.
    ///
    /// It cannot name that group from `fleet` alone. [`deck_wire_id`] mints
    /// `deck-<16 hex>` from a hash of the endpoint's identity, which is exactly
    /// what makes it a safe key and exactly what makes it unreadable — and the
    /// webview has no id-to-row join to reach for, because a stored row's
    /// [`crate::settings::EndpointId`] is a different value entirely. Without
    /// this list `deckName` would fall through to "Local deck" for a remote
    /// deck it has never heard from.
    ///
    /// So this is the same statement [`Self::unconfigured`] makes for a row
    /// with no address, for a row that has one: here is the id, here is what to
    /// call it, and here is whether it is this machine's. It is what the crate
    /// KNOWS from the applied document, never a connection state nobody
    /// measured — the status of a deck in here is still whatever its own
    /// snapshot says when one arrives.
    ///
    /// Every connectable deck is listed, including the ones that have already
    /// reported: which of them the webview has heard from is the webview's own
    /// question, and an answer from here would be one the crate would have to
    /// keep in step with a stream it does not observe.
    pub observed: Vec<ObservedDeckDto>,
}

/// One deck the app connects to, named without having been heard from (PRD
/// #742 M14).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedDeckDto {
    /// [`deck_wire_id`] — the same key this deck's own snapshot carries in
    /// [`DesktopConnection::deck_id`], so the webview can tell an entry here
    /// apart from a deck that has already reported by looking in one map.
    pub deck_id: String,
    /// [`deck_path_text`] — the socket path for a local deck, `user@host[:port]`
    /// for a remote one. The same string this deck's own snapshot will carry in
    /// [`DesktopConnection::socket_path`], so the group is named the same
    /// before and after it reports and nothing moves when it does.
    pub label: String,
    /// `"local"` or `"remote"`, from [`selection_fields`] — the half of
    /// [`DesktopConnection::deck_kind`] that is a property of the deck rather
    /// than of the handshake. It decides whether the group is called "Local
    /// deck" or called by its address.
    pub deck_kind: &'static str,
}

/// One configured-but-unaddressed deck, as the fleet view renders it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnconfiguredDeckDto {
    /// [`crate::dto::unconfigured_deck_id`] — disjoint from every real
    /// `deck_id`, and what the webview keys this group on.
    pub deck_id: String,
    /// `user@host[:port]`, the same label a connected row would carry in
    /// [`DesktopConnection::socket_path`].
    pub label: String,
    /// Why there is nothing to show — the fleet view's idiom for
    /// [`crate::settings::SelectionFallback::NoRemoteSocket`], which says the
    /// same thing in the selector's.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopConnection {
    pub status: ConnectionStatus,
    /// What this deck is CALLED, for a human to read — `Endpoint::describe()`.
    ///
    /// **A label, and never a key**, which is the distinction PRD #742 M5 put on
    /// the wire: see [`Self::deck_id`] for the value anything keying on a deck
    /// must use instead.
    pub socket_path: String,
    /// What this deck IS, for anything keying on it — `EndpointIdentity::wire_id()`
    /// (PRD #742 M5).
    ///
    /// # The defect this field closes
    ///
    /// The webview derived its per-deck key from [`Self::socket_path`], which is
    /// `Endpoint::describe()` — and `describe()` renders a remote deck as
    /// `user@host[:port]`, omitting the remote socket path, the identity file
    /// and the jump host. So two `[[endpoints.remote]]` rows pointing at one
    /// host with different `socket` values — **two daemons on one machine**,
    /// which is exactly what that field exists for — described identically,
    /// folded into ONE group, and their agents shared a `daemonId`. The
    /// composite `(daemonId, agentId)` key does not save that: the key
    /// component is the collision. The failure is silent, which is the worst
    /// part of it — the two decks alternate inside one group, last writer wins
    /// per emit, and the screen looks like one healthy deck.
    ///
    /// `EndpointIdentity` was introduced on PR #1035 *specifically* for the
    /// N-deck case and then stopped at the Rust boundary. This is it crossing.
    ///
    /// Opaque on purpose (`deck-<16 hex>`): it is a key, and `socket_path`
    /// beside it is what the UI renders. Stable across restarts — not because
    /// anything keys stored state on it today (PRD #742 M8 checked: every
    /// `localStorage` key the webview writes is scoped by runtime mode and none
    /// is per deck), but because this is the identity a per-deck preference
    /// would be keyed on, and `EndpointIdentity::wire_id`'s *Stability* section
    /// is where that argument and its two caveats live.
    pub deck_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub client_protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_protocol_version: Option<u32>,
    pub client_build_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_build_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_agent_count: Option<usize>,
    /// True when the protocol version agreed on both sides and a **declared
    /// contract break** is what failed the handshake, so the webview may offer
    /// Connect anyway (issue #801).
    ///
    /// **Named for what used to set it**, which was a git-describe build-stamp
    /// difference the two builds' release digits did not excuse. Since #801 a
    /// stamp difference sets this — and refuses — never: the classification is
    /// over `daemon_protocol::CONTRACT_BREAKS`, which lives in the contract's own
    /// source rather than in a tag. The name is kept because it is the key the
    /// webview switches Connect anyway on, and renaming it across the bridge
    /// would buy nothing this doc comment does not.
    ///
    /// Always emitted, including as `false`, because the webview branches on it
    /// to decide whether an override exists: an absent field and an incompatible
    /// wire must not look the same. A protocol mismatch never sets it, which is
    /// what keeps the wire check unoverridable from the UI as well as from the
    /// bypass itself.
    pub build_stamp_mismatch_only: bool,
    /// `"local"` or `"remote"` — which kind of deck this connection is to
    /// (PRD #741 M7).
    ///
    /// Always emitted, for the same reason `build_stamp_mismatch_only` is: the
    /// webview branches on it to disable **Stop daemon** and **Replace daemon**,
    /// and an absent field must not read as "local".
    pub deck_kind: &'static str,
    /// Why the daemon-lifecycle controls are unavailable, when they are
    /// (PRD #741 M7).
    ///
    /// `Some` exactly when [`Self::deck_kind`] is `"remote"`. The sentence is
    /// `Endpoint::require_local("Stop daemon")`'s own — M2 made the operation
    /// unreachable by type and wrote the refusal at the same time, so this is
    /// rendering an error that already exists rather than inventing one. Stop
    /// and Replace act on a process on *this* machine; against a deck on
    /// another one they would either do nothing or, over a forwarded socket,
    /// SIGTERM the local `ssh` client and report that a daemon had stopped
    /// gracefully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_only_reason: Option<String>,
    /// Why the app is talking to the local deck when the stored selection named
    /// another one (PRD #741 M7).
    ///
    /// `SelectionFallback`'s own sentence. Reported rather than folded into
    /// "connected": "that deck is gone" and "that deck has no socket path yet"
    /// are different things to tell a user, and a silent substitution is how a
    /// user ends up acting on the wrong machine's agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_fallback: Option<String>,
    /// Why the project-aware surfaces — choosing a project, preparing and
    /// launching a workflow — are unavailable against this daemon (PRD #741 M8).
    ///
    /// **The field that replaces the build stamp as the thing a screen acts
    /// on.** It is derived from what the daemon ADVERTISED in its `Hello` reply,
    /// through `DaemonCapabilities::supports`, so it answers "can this deck do
    /// what I am about to ask" rather than "is this deck the same build as me" —
    /// the distinction issue #801 is about. A daemon that advertises no set at
    /// all is an older daemon and withholds every verb, which is why absence is
    /// a reason rather than a grant.
    ///
    /// `None` means available. Omitted from the wire when `None`, unlike
    /// [`Self::deck_kind`] and [`Self::build_stamp_mismatch_only`]: those two
    /// are branched on to decide whether a control EXISTS, where an absent field
    /// reading as `false` would be wrong. This one is a sentence to render, and
    /// "no sentence" is exactly what absence should mean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_actions_reason: Option<String>,
    /// Why the New agent flow cannot start anything on this deck (PRD #1223):
    /// the deck does not advertise `list-directories`, and browsing is the
    /// only way the flow chooses a directory. `None` means it can; omitted from
    /// the wire when `None`, for [`Self::project_actions_reason`]'s reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_agent_reason: Option<String>,
}

/// The three endpoint-shaped fields of [`DesktopConnection`], **for one deck**.
///
/// One function so the two construction sites — the classified handshake and
/// [`disconnected_snapshot`] — cannot disagree about whether the deck is
/// remote, which is the disagreement that would leave **Stop daemon** enabled
/// on exactly the screen state where it is most tempting to press.
///
/// # Two of the three now come from the argument, and one still does not
///
/// PRD #742 M3. `deck_kind` and `local_only_reason` are properties of **a**
/// deck: whether *this* deck runs on this machine, and therefore whether Stop
/// and Replace can act on it. They read the process-global selection until M3,
/// which is why the PRD's Scope names "per-deck connection state" — with a fleet
/// on screen, reading them off the selection would disable the lifecycle
/// controls for the whole view whenever the *selected* deck happened to be
/// remote, and enable them for a remote group whenever it happened to be local.
///
/// `selection_fallback` stays the selection's, because it describes the
/// selection rather than a deck — "the deck you chose is gone, so you are on
/// local". Attaching it to every group would be a claim about each of them.
/// **The narrow fact that makes reading it off the global correct here**: the
/// only selection observing more than one deck is
/// [`crate::settings::Selection::All`], whose `resolve()` returns the local deck
/// with no fallback at all, so a fleet never has one to misattribute; and every
/// other selection observes exactly the deck it resolved to, so the fallback is
/// that deck's by construction.
pub(crate) fn selection_fields(
    endpoint: &Endpoint,
) -> (&'static str, Option<String>, Option<String>) {
    let kind = match endpoint {
        Endpoint::Local(_) => "local",
        Endpoint::Remote(_) => "remote",
    };
    let local_only = endpoint
        .require_local("Stop deck")
        .err()
        .map(|error| safe_display_text(error.to_string()));
    (kind, local_only, selected_deck().fallback)
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
    Incompatible,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAgent {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub agent_type: String,
    /// The BINARY this agent runs — `claude`, `opencode`, `pi`, `codex`,
    /// `devin` — **as the daemon reported it** ([`AgentRecord::cli_name`],
    /// issue #856). Copied through; never computed here.
    ///
    /// `agent_type` above is the wire IDENTITY and stays snake_case for the
    /// consumers that key on it; it is not a name anybody types. Rendering it
    /// showed Claude Code as `claude_code` and OpenCode as `open_code`, with
    /// `codex` right only by coincidence.
    ///
    /// **It used to be resolved here**, by looking `agent_type` up in this
    /// crate's own compiled-in `agent_registry` — the one entry PRD #819's M1
    /// ownership sweep classified as computed locally that no other issue
    /// covered. Which binary a running agent is, is a fact about the daemon's
    /// world: it forked the process. It could not diverge while
    /// `classify_handshake` demanded an exact `server_version` match plus
    /// matching build stamps, since both sides then compile one table by
    /// construction — but that is a property of the gate, and issue #801 exists
    /// to relax the gate.
    ///
    /// Absent — never a placeholder, and **never resolved from the local table**
    /// — whenever the daemon named no binary: an agent type whose spec has no
    /// `default_command` (`AgentType::None`, which carries `#[serde(other)]` and
    /// so absorbs a type from a NEWER daemon), a record reporting no type, or a
    /// daemon predating the field. Falling back here would reinstate exactly the
    /// divergence the field closes, so the webview renders nothing — the
    /// disposition `spawned_at_ms` and `last_activity_ms` already take.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_name: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tool: Option<DesktopActiveTool>,
    pub tool_count: u32,
    /// The daemon's `SessionSnapshot.last_user_prompt` (PRD #745 M8) — the most
    /// recent prompt the operator sent this agent, and the honest answer to
    /// "what was this one asked to do".
    ///
    /// Absent, never a placeholder: an agent that has emitted no prompt event,
    /// a record with no `live` snapshot, and an older daemon all yield `None`,
    /// and `skip_serializing_if` keeps the key off the wire so the webview sees
    /// absence rather than an empty string it would have to special-case. The
    /// value is already control-stripped and byte-bounded by
    /// `daemon_client::sanitize_record_tab_membership`; the webview bounds and
    /// sanitises its own DISPLAY copy again at the render seam, because that
    /// scrub covers category `Cc` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_user_prompt: Option<String>,
    /// Whether the daemon can deliver input to this agent right now, projected
    /// from `SessionSnapshot.live_target.writable` (PRD #745 M8). Replaces the
    /// webview's hardcoded `"unknown"`.
    ///
    /// Only the `writable` half is surfaced: the deck's model speaks
    /// read/write/none, and `TargetKind` (pty / tmux / sdk / process) is a
    /// daemon-side implementation detail no desktop surface consumes. Absent
    /// when the daemon declared no live target at all — which the TUI reads as
    /// the legacy live default, so the desktop must NOT read absence as
    /// "read-only".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_lease: Option<&'static str>,
    /// When the daemon last saw this agent do anything, as epoch milliseconds
    /// (`SessionSnapshot.last_activity_ms`, PRD #745 M9). Copied through
    /// unchanged: the daemon owns the instant, the webview owns the wording.
    ///
    /// Absent when the record carries no `live` snapshot — a daemon that
    /// restarted has no sessions, so it reports no activity times rather than
    /// resetting them all to "just now" — and absent from an older daemon that
    /// predates the field. Absence renders as nothing, never a placeholder.
    ///
    /// NOT clamped here. The daemon's value is producer-supplied and can land
    /// in the future, and the only seam that can decide what to do about that
    /// is the one that owns the OTHER clock: the webview compares it against
    /// its own `Date.now()`, absorbs ordinary skew, and refuses to relativise
    /// anything beyond it. Clamping here against the desktop process's clock
    /// would silently move a value the daemon reported, using a third clock
    /// that is not the one the comparison is made on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<i64>,
    /// When the daemon spawned this agent's process, as epoch milliseconds
    /// (`AgentRecord.spawned_at_ms`, PRD #745 M11). Copied through unchanged,
    /// same split as `last_activity_ms`: the daemon owns the instant, the
    /// webview owns the wording.
    ///
    /// It comes off the REGISTRY record rather than the live session, which is
    /// what makes it answer a question `last_activity_ms` cannot: a session
    /// exists only once a hook event has arrived, so an agent that has never
    /// emitted one still reports a spawn time. Absent when the daemon did not
    /// spawn the process it is describing (an id-only reply from an older
    /// daemon) or predates the field — and absent means the column renders
    /// nothing, never a placeholder.
    ///
    /// NOT clamped here, for the reason spelled out on `last_activity_ms`: the
    /// desktop process holds a third clock, and the seam that owns the one the
    /// comparison is actually made against is the webview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_at_ms: Option<i64>,
    pub tab: DesktopTab,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActiveTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopTab {
    Dashboard,
    Mode {
        name: String,
    },
    Orchestration {
        name: String,
        role_index: usize,
        role_name: String,
        is_start_role: bool,
        /// The orchestration TAB's own working directory
        /// (`TabMembership::orchestration_cwd`), shared by every role pane in
        /// the tab and distinct from each pane's own `cwd` — an orchestrator
        /// and its workers may sit in different per-pane directories while
        /// belonging to one orchestration (PRD #745 M8). The overview states it
        /// once in the group header, which is what turns the per-row column
        /// into a differences column. `None` when the daemon reported none.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        orchestration_id: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BootstrapOptions {
    pub start_if_missing: bool,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopAction {
    Refresh,
    Bootstrap {
        #[serde(default)]
        start_if_missing: bool,
    },
    /// Start one plain agent on the deck `deck_id` names (PRD #1223 M3).
    StartAgent {
        /// The target deck's wire id — `connection.deckId`, the value
        /// [`DeckScope::resolve`] accepts — and **required**, which is the
        /// point. This arm used to read the applied selection through
        /// `trusted_daemon()`, and under **All Decks** that resolves to the
        /// local deck (#1083): the overview, the one screen that shows every
        /// deck at once, would have started the agent on whichever deck
        /// happened to be selected. A start with no deck is now refused at
        /// decode rather than defaulted, so there is no selection-reading path
        /// left to fall back to.
        deck_id: String,
        command: Option<String>,
        cwd: Option<String>,
        display_name: Option<String>,
        rows: Option<u16>,
        cols: Option<u16>,
        /// PRD #1223 M7: start an AUTHORING agent — the TUI's `schedule`,
        /// `schedule: issues` or `dispatcher` option — whose seed the deck
        /// composes and delivers once the agent is ready. Absent is a plain
        /// agent, byte for byte what this action sent before.
        ///
        /// The crate's own closed enum rather than a string, so a kind this
        /// build does not know fails the decode instead of reaching a deck as a
        /// plain start with no seed — the failure the deck's capability gate
        /// exists to prevent, one hop earlier.
        #[serde(default)]
        authoring_kind: Option<AuthoringKind>,
    },
    /// Start one of a project's orchestrations on the deck `deck_id` names,
    /// the way the TUI's `Ctrl+n` does (PRD #1223 M6): no task prompt, the
    /// form's Name as the run's title, and every role run with the command its
    /// project config gives it — on the deck, which reads the config there.
    ///
    /// Not [`Self::StartWorkflow`], which is the Runs screen's launch and keeps
    /// its own form rules (a required task, desktop profile commands, no Pi
    /// coordinator). The two share the daemon verbs and the bridge's rollback
    /// and coordinator-delivery machinery, and none of those form rules.
    StartOrchestration {
        /// The target deck's wire id, required for [`Self::StartAgent`]'s
        /// reason.
        deck_id: String,
        /// The daemon-canonical project path the dialog's `ResolveProject`
        /// answered with, **verbatim**.
        path: String,
        /// The orchestration name as that reply offered it, verbatim.
        orchestration: String,
        /// The run's title — the form's Name. Absent when the Name is empty,
        /// which is the TUI's rule: the tab then takes the orchestration's name.
        #[serde(default)]
        display_title: Option<String>,
        /// The `configRevision` the dialog resolved against, echoed to
        /// `prepare-workflow` as the Runs launch echoes it.
        #[serde(default)]
        config_revision: Option<String>,
        rows: Option<u16>,
        cols: Option<u16>,
    },
    StartWorkflow {
        /// The orchestration name, as offered by the daemon's
        /// `resolve-project` reply for `cwd`.
        name: String,
        /// The daemon-**canonical** project path, exactly as
        /// `resolve-project` or `list-projects` spelled it. PRD #819 M6: the
        /// webview never derives this from its own environment and never
        /// re-spells it, because canonicalising a symlinked path changes its
        /// basename and an empty orchestration name is derived from that
        /// basename (PRD #220).
        cwd: String,
        task_prompt: String,
        roles: Vec<WorkflowRoleInput>,
        rows: Option<u16>,
        cols: Option<u16>,
        /// The `configRevision` the webview last resolved against, echoed
        /// through to `prepare-workflow`. `#[serde(default)]` and absent means
        /// "no expectation": a launch assembled without a resolve still works,
        /// it just does not get the staleness check.
        #[serde(default)]
        config_revision: Option<String>,
    },
    StopAgent {
        /// The deck the agent runs on — `connection.deckId`, resolved with
        /// [`DeckScope::resolve`] exactly as the start actions resolve theirs
        /// (PRD #1223 U4). This arm read the applied selection until then, so
        /// under **All Decks**, which resolves to the local deck (#1083), an
        /// agent started on another deck from the overview could not be
        /// stopped without switching the selection first. Required, for
        /// [`Self::StartAgent`]'s reason: a stop with no deck is refused at
        /// decode rather than defaulted.
        deck_id: String,
        agent_id: String,
    },
    /// PRD #1223 U4 — close a whole orchestration: stop every role the
    /// webview's fleet entry lists for it, on the deck `deck_id` names,
    /// concurrently. The TUI's Ctrl+W does the same over its tab's panes
    /// (`close_panes_concurrently`); there is no orchestration-wide daemon
    /// verb, and this adds none — it is `StopAgent` fanned out.
    StopOrchestration {
        deck_id: String,
        roles: Vec<StopOrchestrationRole>,
    },
    StopDaemon {
        #[serde(default)]
        force: bool,
    },
    RestartDaemon,
    /// Relax the build-stamp comparison for the rest of this app session and
    /// hand back a freshly classified snapshot (issue #801). Carries no
    /// payload: it is an assertion by the user, not a parameter, and it can
    /// only ever relax the stamp check — the protocol check runs first and is
    /// never bypassed.
    AllowBuildMismatch,
    RenameAgent {
        agent_id: String,
        #[serde(alias = "name")]
        display_name: String,
    },
    AttachTerminal {
        agent_id: String,
        on_output: JavaScriptChannelId,
    },
    DetachTerminal {
        session_id: String,
    },
    SubmitText {
        agent_id: String,
        text: String,
    },
}

/// One role a [`DesktopAction::StopOrchestration`] stops: the deck's agent id,
/// and the name the confirmation showed — which is what a role whose stop could
/// not be confirmed is reported as. The name is display text only; nothing is
/// resolved by it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopOrchestrationRole {
    pub agent_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRoleInput {
    pub role: String,
    pub command: String,
    #[serde(default)]
    pub start: bool,
}

/// What `desktop_run_action` rejects with (PRD #1223 audit F6).
///
/// **A bare string for every failure but one**, exactly as before this type
/// existed — `#[serde(untagged)]` serialises [`Self::Message`] as the string
/// itself, so every existing `catch` on the webview side reads the same value it
/// always read. The one exception is a launch whose rollback could not confirm
/// that every role it touched is stopped: that failure is an object carrying
/// the list as data, so the webview can put "these may still be running" in
/// front of the reader without parsing it back out of the prose — where it is
/// the LAST clause, after the primary error and every started role's name, and
/// so the first thing a display clamp cuts.
///
/// Serialize-only, so `untagged`'s deserialisation cost (see
/// `settings::StageSpec`) does not arise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum DesktopActionError {
    Message(String),
    LaunchCleanup(DesktopLaunchCleanupFailure),
}

/// A failed launch that could not confirm its cleanup — see
/// [`DesktopActionError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopLaunchCleanupFailure {
    /// The whole error, as a string rejection would have carried it.
    pub message: String,
    /// Every role the launch started, or may have started, whose stop it could
    /// not confirm — each through [`safe_message`]. Never empty: a launch
    /// whose cleanup was confirmed rejects with a plain
    /// [`DesktopActionError::Message`].
    pub unconfirmed_stops: Vec<String>,
}

impl DesktopActionError {
    /// The sentence either variant carries.
    #[cfg(test)]
    pub fn message(&self) -> &str {
        match self {
            Self::Message(message) => message,
            Self::LaunchCleanup(failure) => &failure.message,
        }
    }

    /// A launch failure, as the variant its cleanup calls for.
    pub fn launch(message: String, unconfirmed_stops: Vec<String>) -> Self {
        if unconfirmed_stops.is_empty() {
            return Self::Message(message);
        }
        Self::LaunchCleanup(DesktopLaunchCleanupFailure {
            message,
            unconfirmed_stops: unconfirmed_stops.iter().map(safe_message).collect(),
        })
    }
}

impl From<String> for DesktopActionError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for DesktopActionError {
    fn from(message: &str) -> Self {
        Self::Message(message.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActionResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_result: Option<SendResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalAttachResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub snapshot: DesktopSnapshot,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalAttachResult {
    pub session_id: String,
    pub agent_id: String,
    pub generation: u64,
    pub reused: bool,
    /// PRD #882 — the geometry the daemon has APPLIED for this agent, which is
    /// decided by its viewer policy (the last-focused client's viewer size, else
    /// the smallest viewport among every client attached to it, PRD #1105) and
    /// so is not necessarily the size this tile asked for.
    ///
    /// The frontend sizes its xterm grid from this rather than from
    /// `FitAddon.fit()`. Absent only when talking to a daemon that predates the
    /// policy, in which case the tile's own fit is the right answer — that
    /// daemon has no other client's constraint to tell us about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_cols: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalGeometryEvent {
    pub session_id: String,
    pub agent_id: String,
    pub generation: u64,
    /// PRD #882 — the geometry the daemon just applied, pushed because another
    /// client attached, detached or resized this agent. The tile reshapes its
    /// xterm grid to match; without it the tile would keep parsing the agent's
    /// bytes at its own geometry while the PTY sits at somebody else's.
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStateEvent {
    pub session_id: String,
    pub agent_id: String,
    pub generation: u64,
    pub state: TerminalState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Attached,
    End,
    Error,
}

// ---------------------------------------------------------------------------
// PRD #819 M6: the project surface, entirely daemon-sourced.
//
// These are the webview's shape for `crate::event::{ProjectListing,
// ResolvedProject}` — camelCase, and carrying every daemon-supplied identity
// **byte for byte** alongside a separate, scrubbed, display-only twin.
//
// # PRD #819 audit fix (P2, finding 1): why there are two fields, not one
//
// The first version of this seam ran each value through [`safe_message`] and
// kept the result as the ONLY copy. That breaks the principle the whole PRD
// exists to establish — the daemon owns canonical identity — because the
// desktop stores these values and later sends them back: `path` becomes
// `PrepareWorkflow.cwd` and every `StartAgent.cwd`, an orchestration `name`
// becomes `StartWorkflow.name`, a role `name` becomes the requested role and
// the pane's `display_name`, and `config_revision` is echoed back so a config
// edited under the picker is refused. Two concrete failures followed:
//
// * **Truncation, strictly inside the wire bounds.** `safe_message` cuts at
//   [`ERROR_MESSAGE_MAX_CHARS`] **characters** while the daemon accepts a
//   request path up to `agent_pty::CWD_MAX_LEN` (4096) **bytes**. A canonical
//   path of 2049..=4096 characters is valid on the wire in both directions and
//   was displayed AND submitted as a shorter, different path — so the launch
//   resolved somewhere other than where the user chose, or nowhere.
// * **Control stripping, which is worse than it first looks.** A canonical
//   path CAN carry an ASCII control byte — `canonicalize_project_dir` checks
//   UTF-8 and directory-ness, not control-freeness, and a filename may hold
//   any byte but `/` and NUL — while `is_valid_cwd` REJECTS those bytes, so
//   such a path is one the daemon will refuse on the way back. Scrubbing it
//   does not fix that; it converts a path the daemon would have refused
//   cleanly into a *different* path the daemon accepts, which may well be
//   another real project. Carried verbatim, the launch is refused with
//   `invalid-path`, which is the honest answer for a path this wire cannot
//   express.
// * `primary` was scrubbed and so was the `path` it is compared against for
//   the picker's ACTIVE marker, so the marker worked — but only because the
//   same mangling was applied to both. Fixing one field and not the other
//   would have broken it outright, which is why they move together.
//
// So the identity is carried verbatim and the escaping moved to a sibling
// field: `displayPath` / `displayName` are what the webview renders, `path` /
// `name` / `configRevision` are what goes back to the daemon. Rendering does
// not require mutating the value that is submitted, and `safe_message` keeps
// its job on the half that reaches a DOM text node — project and orchestration
// names are config content, so they are still untrusted text.
//
// Nothing here is persisted on either side. A selection is a property of the
// launch being assembled and dies with it — see the PRD's *Nothing remembers a
// project*.
// ---------------------------------------------------------------------------

/// The display-only twin of a daemon-supplied identity.
///
/// One named seam rather than a bare [`safe_message`] call per field, so the
/// two halves of every pair below are visibly a pair, and so `grep display_only`
/// finds every place a daemon identity is turned into renderable text.
fn display_only(identity: &str) -> String {
    safe_message(identity)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopProjectListing {
    pub projects: Vec<DesktopProject>,
    /// The project most recently active on this daemon, if any — a fact derived
    /// from live state, never a stored preference. Absent when the daemon has
    /// nothing live, which is the empty state rather than an error.
    ///
    /// **Verbatim**, because it is compared against [`DesktopProject::path`] to
    /// mark the active row: a scrubbed copy of one and a verbatim copy of the
    /// other would silently stop matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopProject {
    /// The daemon-canonical absolute path, **byte for byte**. This is the
    /// identity, and the exact string that goes back on `resolve-project`,
    /// `prepare-workflow` and every `StartAgent.cwd`. The webview must never
    /// re-spell it — and, since the audit fix, neither does this seam.
    pub path: String,
    /// [`Self::path`] made safe to render. Never sent anywhere.
    pub display_path: String,
    /// The daemon's projected basename, made safe to render. Display-only on
    /// both sides: nothing sends a project's *name* back, only its path.
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopResolvedProject {
    /// The canonical path the daemon resolved to, **byte for byte**. It may
    /// differ from the one that was sent — an alias or a symlink resolves
    /// elsewhere, and canonicalising changes the basename an empty
    /// orchestration name is derived from (PRD #220). This spelling is the one
    /// that propagates, so it is carried unmodified.
    pub path: String,
    /// [`Self::path`] made safe to render. Never sent anywhere.
    pub display_path: String,
    /// The canonical path's basename, made safe to render. Display-only.
    pub display_name: String,
    pub orchestrations: Vec<DesktopOrchestration>,
    /// Echoed back on the launch so a config edited between the picker and the
    /// spawn is refused rather than silently launched against — so **verbatim**:
    /// a re-spelled revision would fail the daemon's comparison for a reason
    /// nobody could see.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopOrchestration {
    /// **Verbatim** — this goes back as `PrepareWorkflow.orchestration`, and a
    /// name the daemon offered must be a name the daemon can find again.
    pub name: String,
    /// [`Self::name`] made safe to render.
    pub display_name: String,
    pub default: bool,
    pub roles: Vec<DesktopOrchestrationRole>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopOrchestrationRole {
    /// **Verbatim** — `order_workflow_roles` matches the requested roles against
    /// these by exact name, and the name becomes the pane's `display_name` and
    /// its `TabMembership.role_name`.
    pub name: String,
    /// [`Self::name`] made safe to render.
    pub display_name: String,
    pub start: bool,
}

fn map_orchestration(orchestration: ProjectOrchestration) -> DesktopOrchestration {
    DesktopOrchestration {
        display_name: display_only(&orchestration.name),
        name: orchestration.name,
        default: orchestration.default,
        roles: orchestration
            .roles
            .into_iter()
            .map(|role| DesktopOrchestrationRole {
                display_name: display_only(&role.name),
                name: role.name,
                start: role.start,
            })
            .collect(),
    }
}

pub(crate) fn map_project_listing(listing: ProjectListing) -> DesktopProjectListing {
    DesktopProjectListing {
        projects: listing
            .projects
            .into_iter()
            .map(|project| DesktopProject {
                display_path: display_only(&project.path),
                display_name: display_only(&project.name),
                path: project.path,
            })
            .collect(),
        primary: listing.primary,
    }
}

pub(crate) fn map_resolved_project(project: ResolvedProject) -> DesktopResolvedProject {
    // The display name is the canonical path's basename, derived HERE from the
    // path the daemon returned rather than carried separately: the two must name
    // the same directory, and a second field is a second thing to drift.
    //
    // It is split off the VERBATIM path and scrubbed afterwards, not off the
    // scrubbed one: truncating first can cut the basename off entirely, so the
    // order of those two steps is the difference between a shortened label and
    // the wrong label. The path itself is untouched by any of this — it is the
    // protocol identity and goes back to the daemon byte for byte.
    //
    // PRD #819 Greptile P2(e): PLATFORM-AWARE, and it used to split on `/`
    // alone. Project resolution is not refused on Windows — only
    // `PrepareWorkflow` is, with `unsupported-platform`, because only its
    // publish carries an owner-only guarantee it cannot deliver there. So a
    // Windows client lists and resolves, and a canonical Windows path
    // (`\\?\C:\Users\dev\project`) contains no `/` at all: the whole path
    // fell through to the `unwrap_or` and the picker labelled the project with
    // it. `Path::file_name` is the platform's own answer — under the Windows
    // target it separates on `\` as well as `/` and understands the `\\?\`
    // verbatim prefix, while on Unix it keeps a backslash as an ordinary
    // filename byte, which is exactly what a Unix filename containing one means.
    let basename = Path::new(project.path.as_str())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(project.path.as_str());
    DesktopResolvedProject {
        display_path: display_only(&project.path),
        display_name: display_only(basename),
        path: project.path,
        orchestrations: project
            .orchestrations
            .into_iter()
            .map(map_orchestration)
            .collect(),
        config_revision: project.config_revision,
    }
}

// ---------------------------------------------------------------------------
// PRD #1223 M4 — the New agent dialog's two deck-targeted queries.
//
// Both answer about ONE named deck, and both can be answered "this deck cannot"
// — a deck that predates the verb — which is a state the dialog degrades on
// rather than an error, so it is a variant here rather than an `Err`. The
// shapes follow the project DTOs above: every path is carried **verbatim**,
// because it is what goes back to the daemon, and has a scrubbed display twin
// beside it where it is rendered.
// ---------------------------------------------------------------------------

/// One directory of a deck's filesystem as that deck listed it, or the deck's
/// answer that it has no listing verb.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopDirectoryListing {
    Listing {
        /// The daemon's canonical spelling of the directory, **byte for
        /// byte** — for a typed path, what the flow carries from here on.
        path: String,
        /// `path` made safe to render. Never sent anywhere.
        display_path: String,
        /// The parent's canonical path as the daemon computed it, which is
        /// what "up" sends. The webview never derives one by trimming `path`.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        entries: Vec<DesktopDirectoryEntry>,
        /// The daemon's entry cap or time budget cut the listing short.
        truncated: bool,
    },
    /// The deck does not advertise `list-directories` — a deck older than PRD
    /// #1223. The dialog never asks such a deck (its connection carries
    /// `newAgentReason`, which disables it at the deck step), so this is the
    /// crate's own answer should one be asked anyway.
    Unsupported,
}

/// One subdirectory in a [`DesktopDirectoryListing`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopDirectoryEntry {
    /// The canonical path the daemon joined, **verbatim**: the string entering
    /// this directory sends back.
    pub path: String,
    /// The entry's name, made safe to render. Display-only.
    pub display_name: String,
    /// It holds a `.dot-agent-deck.toml` the daemon's project reader would open.
    pub is_project: bool,
}

impl DesktopDirectoryListing {
    /// The listing variant, built from the daemon reply's parts.
    ///
    /// Takes parts rather than the root crate's reply type because the desktop
    /// may not name that module: `xtask/linkage-check`'s rule 12 bounds which
    /// root modules the production desktop reaches across, and the module that
    /// owns the reply also owns a filesystem listing a client must never run.
    pub(crate) fn listing(
        path: String,
        parent: Option<String>,
        entries: impl IntoIterator<Item = (String, String, bool)>,
        truncated: bool,
    ) -> Self {
        Self::Listing {
            display_path: display_only(&path),
            path,
            parent,
            entries: entries
                .into_iter()
                .map(|(name, path, is_project)| DesktopDirectoryEntry {
                    path,
                    display_name: display_only(&name),
                    is_project,
                })
                .collect(),
            truncated,
        }
    }
}

/// The orchestrations the New agent form can offer for one directory on one
/// deck (PRD #1223 M6) — the deck's `ResolveProject` answer, or why there is
/// nothing to offer.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopNewAgentOrchestrations {
    /// The directory is a project on that deck. Its `path` is the deck's
    /// canonical spelling, which is what the launch sends.
    Project(DesktopResolvedProject),
    /// The deck refused it with `ResolveProject`'s generic `unresolved` code —
    /// an ordinary directory, which is a normal answer here and not an error.
    NotProject,
    /// The deck cannot launch an orchestration from this flow, and `reason`
    /// says why: it lacks the project verbs (the connection's
    /// `projectActionsReason`) or cannot start a role with its configured
    /// command. The form withholds its orchestration chips and shows this.
    Unsupported { reason: String },
}

/// What the New agent form needs to know about one deck, or the deck's answer
/// that it cannot say.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopNewAgentOptions {
    /// The deck answered `new-agent-options` about itself.
    Deck {
        /// The deck host's configured `default_command`, **verbatim** — it is
        /// the Command field's first prefill and goes back on the start.
        #[serde(skip_serializing_if = "Option::is_none")]
        default_command: Option<String>,
        /// The deck host's configured `default_dir` (PRD #1223), canonical and
        /// already vetted by the deck — absent when unset or unusable. It is
        /// never rendered from here: the form sends it back as the first
        /// directory listing's path, and what the user reads is that
        /// listing's own `displayPath`.
        #[serde(skip_serializing_if = "Option::is_none")]
        default_dir: Option<String>,
        /// The agent registry the DECK was built with, in its order.
        agents: Vec<DesktopAgentOption>,
        /// The deck's own experimental flag.
        experimental: bool,
        /// The authoring kinds the deck can compose a seed for.
        authoring_kinds: Vec<String>,
        /// The command this app last started a plain agent with on this deck.
        #[serde(skip_serializing_if = "Option::is_none")]
        last_command: Option<String>,
    },
    /// The deck does not advertise `new-agent-options` — a deck older than PRD
    /// #1223 — so nothing here comes from it.
    Unsupported {
        /// The agent registry compiled into THIS app, which is the only one
        /// left to offer and is labelled as such by the form.
        desktop_agents: Vec<DesktopAgentOption>,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_command: Option<String>,
    },
}

/// One entry of an agent registry, for the New agent form.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAgentOption {
    /// The registry's stable key (`claude`, `opencode`, …), verbatim.
    pub id: String,
    /// The registry's label, made safe to render.
    pub display_name: String,
    /// What choosing this agent writes into Command, **verbatim**.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_command: Option<String>,
}

impl DesktopAgentOption {
    pub(crate) fn new(id: String, display_name: &str, default_command: Option<String>) -> Self {
        Self {
            id,
            display_name: display_only(display_name),
            default_command,
        }
    }
}

/// The agent registry compiled into this app, projected the way a deck
/// projects its own for `new-agent-options`: each entry's first
/// `detect_basenames` value as the id, its label, its default command, in
/// registry order — and an entry with no basename left out rather than given an
/// invented id.
///
/// This is the older-deck fallback's list and nothing else. A deck that answers
/// the query supplies its own, because the agents a spawn can use are the ones
/// the DECK's build knows.
pub(crate) fn desktop_agent_registry() -> Vec<DesktopAgentOption> {
    dot_agent_deck::agent_registry::ALL
        .iter()
        .filter_map(|spec| {
            Some(DesktopAgentOption::new(
                (*spec.detect_basenames.first()?).to_string(),
                spec.label,
                spec.default_command.map(str::to_string),
            ))
        })
        .collect()
}

/// Which of the app's experimental surfaces this desktop process shows (issue
/// #1198), for the webview to gate its render and navigation seams on.
///
/// Each field is ONE wrapper in the root crate's `features` module (CLAUDE.md
/// #9), called in this process — so the flag is the desktop's own, read from
/// this process's environment (`DOT_AGENT_DECK_EXPERIMENTAL`, or the file
/// `DOT_AGENT_DECK_FEATURES_CONFIG` names) with no project walk; see
/// [`crate::init_features`]. It is deliberately NOT the per-deck
/// `experimental` a deck reports in [`DesktopNewAgentOptions`]: these surfaces
/// belong to the app, not to any one deck, and the app observes several.
///
/// All `false` is the shipped default. The overview, the agent overlay and
/// Settings have no field because they are never gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopFeatures {
    pub show_deck: bool,
    pub show_projects: bool,
    pub show_prompts: bool,
    pub show_workflows: bool,
    pub show_agent_profiles: bool,
}

impl DesktopFeatures {
    /// The surfaces the process-global flag shows. Read through the wrappers
    /// on each call; the flag behind them is resolved once at startup.
    pub(crate) fn current() -> Self {
        use dot_agent_deck::features;
        Self {
            show_deck: features::show_desktop_deck(),
            show_projects: features::show_desktop_projects(),
            show_prompts: features::show_desktop_prompts(),
            show_workflows: features::show_desktop_workflows(),
            show_agent_profiles: features::show_desktop_agent_profiles(),
        }
    }
}

/// The STRING-SHAPE check the desktop may make on a path the **user** typed,
/// before spending a daemon round trip on it.
///
/// It touches no filesystem, and it deliberately cannot: whether that path is a
/// project is the daemon's answer, on the daemon's host. This rejects only what
/// is malformed as a request — empty, relative, control-bearing, oversized —
/// through the same `is_valid_orchestration_cwd` predicate the daemon validates
/// the wire field with, so the two agree about what is even askable.
pub(crate) fn validate_pasted_project_path(path: &str) -> Result<(), String> {
    if !is_valid_orchestration_cwd(path) {
        return Err(
            "enter an absolute directory path, without control characters, that the deck can see"
                .into(),
        );
    }
    Ok(())
}

fn agent_type_name(agent_type: &AgentType) -> &'static str {
    match agent_type {
        AgentType::ClaudeCode => "claude_code",
        AgentType::OpenCode => "open_code",
        AgentType::Pi => "pi",
        AgentType::Codex => "codex",
        AgentType::Devin => "devin",
        AgentType::None => "none",
    }
}

fn session_status_name(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "thinking",
        SessionStatus::Working => "working",
        SessionStatus::Compacting => "compacting",
        SessionStatus::WaitingForInput => "waiting_for_input",
        SessionStatus::Idle => "idle",
        SessionStatus::Error => "error",
        SessionStatus::Unknown => "unknown",
    }
}

/// The orchestration arm deliberately binds EVERY field rather than ending in a
/// `..` rest pattern. The rest pattern is what silently swallowed
/// `orchestration_cwd` for as long as this function has existed (PRD #745 M8):
/// the daemon sent it, the desktop parsed it, and nothing here copied it out.
/// Binding them all makes the next field added to `TabMembership` a compile
/// error at this seam instead of another quietly dropped column.
fn map_tab(tab: Option<&TabMembership>) -> DesktopTab {
    match tab {
        None => DesktopTab::Dashboard,
        Some(TabMembership::Mode { name }) => DesktopTab::Mode { name: name.clone() },
        Some(TabMembership::Orchestration {
            name,
            role_index,
            role_name,
            is_start_role,
            orchestration_cwd,
            display_title,
            orchestration_id,
        }) => DesktopTab::Orchestration {
            name: name.clone(),
            role_index: *role_index,
            role_name: role_name.clone(),
            is_start_role: *is_start_role,
            cwd: orchestration_cwd.clone(),
            display_title: display_title.clone(),
            orchestration_id: orchestration_id.clone(),
        },
    }
}

/// The webview's read/write/none vocabulary for a daemon `LiveTarget`.
///
/// Only [`Writable`] is consulted — it is the half that answers "can the deck
/// type into this pane right now". `Writable::None` is also serde's
/// forward-compat catch-all, so a `writable` value a future daemon invents
/// lands on the SAFE, non-writable answer rather than being dressed up as a
/// live target.
fn write_lease_name(writable: &Writable) -> &'static str {
    match writable {
        Writable::Live => "write",
        Writable::HistoryOnly => "read",
        Writable::None => "none",
    }
}

pub(crate) fn map_agent(record: AgentRecord) -> DesktopAgent {
    let live = record.live.as_ref();
    // The wire identity, resolved with the same precedence the DAEMON uses to
    // resolve the binary name beside it (`AgentRecord::reported_agent_type`) —
    // which is what keeps the two from disagreeing about which agent this is.
    let reported_type = live
        .and_then(|snapshot| snapshot.agent_type.as_ref())
        .or(record.agent_type.as_ref());
    let agent_type = reported_type
        .map(agent_type_name)
        .unwrap_or("none")
        .to_string();
    let status = live
        .map(|snapshot| session_status_name(&snapshot.status))
        .unwrap_or("running")
        .to_string();
    let active_tool = live
        .and_then(|snapshot| snapshot.active_tool.as_ref())
        .map(|tool| DesktopActiveTool {
            name: tool.name.clone(),
            detail: tool.detail.clone(),
        });
    let tool_count = live.map(|snapshot| snapshot.tool_count).unwrap_or(0);
    let last_user_prompt = live.and_then(|snapshot| snapshot.last_user_prompt.clone());
    let write_lease = live
        .and_then(|snapshot| snapshot.live_target.as_ref())
        .map(|target| write_lease_name(&target.writable));
    let last_activity_ms = live.and_then(|snapshot| snapshot.last_activity_ms);
    // PRD #745 M11: off the RECORD, not the live snapshot — the daemon knows
    // when it spawned a process whether or not that process has ever emitted an
    // event.
    let spawned_at_ms = record.spawned_at_ms;
    // Issue #856: copied through from the daemon, which forked the process and
    // holds the answer. There is deliberately NO fallback to this crate's own
    // `agent_registry` — a fallback would reinstate the divergence the field
    // closes, and make the change cosmetic.
    let cli_name = record.cli_name;
    let tab = map_tab(record.tab_membership.as_ref());

    DesktopAgent {
        id: record.id,
        pane_id: record.pane_id_env,
        display_name: record.display_name,
        cwd: record.cwd,
        rows: record.rows,
        cols: record.cols,
        agent_type,
        cli_name,
        status,
        active_tool,
        tool_count,
        last_user_prompt,
        write_lease,
        last_activity_ms,
        spawned_at_ms,
        tab,
    }
}

pub(crate) fn safe_message(message: impl AsRef<str>) -> String {
    message
        .as_ref()
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(ERROR_MESSAGE_MAX_CHARS)
        .collect()
}

/// Display text for a value a **settings document** supplied, scrubbed of
/// control *and* bidi characters (PRD #741 M7).
///
/// [`safe_message`] is not enough here and the difference is the whole point:
/// its `char::is_control` test covers general category `Cc`, so the bidi
/// formatting codepoints (category `Cf`) pass through it intact. A single
/// `U+202E` in a settings-supplied host visually reverses the text after it and
/// can swallow the siblings printed beside it — and the connection footer's
/// whole job is telling the user which deck they are talking to, which makes
/// this the one place a mis-rendered endpoint has a direct security
/// consequence.
///
/// The policy is `untrusted_text::strip_control_and_bidi`'s, which the webview's
/// own `displayText.ts` mirrors character for character; routing through the
/// Rust function rather than restating it is what keeps the two ends from
/// drifting. `keep_newlines: false` because an endpoint label is one line.
///
/// Bounded afterwards by [`safe_message`]'s character cap, which is a second
/// line of defence rather than a duplicate: every endpoint field is already
/// byte-bounded by its own newtype, but this function is the seam and the seam
/// should not depend on that.
pub(crate) fn safe_display_text(text: impl AsRef<str>) -> String {
    safe_message(dot_agent_deck::untrusted_text::strip_control_and_bidi(
        text.as_ref(),
        false,
    ))
}

/// The deck this app is talking to, and why (PRD #741 M7).
///
/// `fallback` is `Some` when the stored selection could not be honoured and the
/// local deck was substituted — "that deck is gone" and "that deck has no
/// socket path yet" are different things to tell a user, and neither is
/// "connected to local".
#[derive(Debug, Clone)]
pub(crate) struct SelectedDeck {
    pub(crate) endpoint: Endpoint,
    pub(crate) fallback: Option<String>,
}

impl Default for SelectedDeck {
    fn default() -> Self {
        Self {
            endpoint: Endpoint::local(),
            fallback: None,
        }
    }
}

/// The applied selection — the deck in force AND the set the fleet observes, as
/// **one value under one lock** (PRD #742 M8).
///
/// # A process-global, deliberately
///
/// For the same reason `SESSION_BUILD_MISMATCH_ALLOWED` is one: every path that
/// names the deck — [`disconnected_snapshot`], the watcher's refresh, the
/// bootstrap's lazy-spawn guard — needs the answer, and most of them are not
/// handed `DesktopState`. Threading it would mean changing every one of those
/// signatures to carry a value that has exactly one writer.
///
/// # Why the two halves are one struct and not two locks
///
/// They were two `RwLock`s written in sequence by [`apply_settings_selection`],
/// and the comment on the second said the pair "can never describe different
/// saves" because there is one writer. That is true of **saves** and false of
/// **reads**: a reader scheduled between the two `write()` calls saw one new
/// value and one old one. [`observed_fleet`] is the one function that reads both
/// halves in a single call, so it is where a torn pair could be observed
/// outright; [`deck_is_observed`] and `ensure_snapshot_watchers` read the
/// observed set alone and could disagree with a `selected_deck()` taken beside
/// them by their own caller.
///
/// The direction that costs something is `endpoint_test::release_if_not_observed`
/// reading a pre-save observed set while a just-added deck's watcher already
/// holds a lease: the probe drops the map's handle, the watcher's own lease keeps
/// the child alive, and the next `establish()` opens a **second** `ssh` child
/// beside it — the defect PRD #742 M6's rename fixed, reached through the write
/// gap rather than through the wrong predicate. Neither the reviewer nor the
/// auditor who found this could convince themselves it was reachable; the gap is
/// two adjacent lock acquisitions with no `await` between them. It is closed by
/// construction anyway, because "two adjacent statements never interleave" is
/// the kind of invariant that stops being true two refactors later and fails
/// silently when it does.
///
/// # Why the halves are not derived from one another
///
/// Under [`crate::settings::Selection::All`] the resolved deck is the local one
/// and the observed set is the whole fleet, so a caller asking "is this deck one
/// of ours" cannot get the answer from the selection. Under every other
/// selection the set is exactly the one element `selected` holds, which is why
/// this could not be noticed before `All` existed.
#[derive(Debug, Clone)]
struct AppliedSelection {
    selected: SelectedDeck,
    /// The decks the app CONNECTS to — watchers, tunnels, handshakes.
    observed: Vec<Endpoint>,
    /// Bumped whenever a deck LEAVES [`Self::observed`] — the epoch a
    /// [`DeckScope`] captures and revalidates against.
    ///
    /// Held in here rather than in a [`crate::generation::Generation`] of its
    /// own precisely because of what this struct is for: a scope that read the
    /// set from one place and the epoch from another could straddle a write and
    /// hold a pair that never coexisted. One read answers both.
    ///
    /// **Only departures move it**, because only a departure can make an
    /// in-flight operation unwanted. A save that *adds* a deck leaves every
    /// existing scope valid, which is what keeps the common settings save from
    /// failing an unrelated attach.
    observed_generation: u64,
    /// The decks the app SHOWS but cannot connect to (PRD #742 M12). Held
    /// beside `observed` rather than folded into it because every reader of
    /// `observed` wants the connectable set and would spin a watcher against an
    /// endpoint that cannot exist; [`observed_fleet`] is the one reader that
    /// wants both, and it is the display set.
    unconfigured: Vec<crate::settings::UnconfiguredDeck>,
}

impl Default for AppliedSelection {
    /// Unset reads as the local deck alone — what every caller did before
    /// endpoints existed, what a test that never loads a document sees, and what
    /// `DesktopSettings::connectable_endpoints()` answers for a document with no
    /// `[endpoints]` section.
    fn default() -> Self {
        Self {
            selected: SelectedDeck::default(),
            observed: vec![Endpoint::local()],
            unconfigured: Vec::new(),
            observed_generation: 0,
        }
    }
}

/// The applied selection. Written by [`apply_settings_selection`] and nobody
/// else; read through [`applied_selection`] and nobody else.
///
/// The writer is called from the app's `setup` hook (where the settings document
/// is already being read for the zoom level) and from `desktop_set_settings`
/// after a successful save.
///
/// **PRD #742 M3 narrowed what the `selected` half answers.** It is still the one
/// deck the *deck screen* and its terminals talk to (DECISION 1 keeps that
/// single-deck), but it is no longer the deck a snapshot is stamped with — see
/// [`deck_path_text`] and [`selection_fields`], which take the endpoint they are
/// describing.
static APPLIED_SELECTION: std::sync::RwLock<Option<AppliedSelection>> =
    std::sync::RwLock::new(None);

/// Serializes the TESTS that write [`APPLIED_SELECTION`], wherever they live.
///
/// [`apply_settings_selection`] is a process-global write, so a test that makes
/// one and asserts on what it reads back needs every other test's write to be
/// outside its own window. Under nextest each test owns its process and this is
/// always free; under a plain `cargo test` the crate's tests are threads in one
/// process and this is the only thing keeping those writes out of each other's
/// windows.
///
/// It lives here rather than in [`tests`] because the writers do not: issue
/// #1078 found eight tests across `endpoint_test::tests` and `lib::tests`
/// writing the global without it, six of them by way of
/// `lib::retarget_selection`.
///
/// **It is one of three process-globals `cargo test --lib` raced on, not the
/// only one**, so taking it does not on its own make that command green —
/// measured, with the tests that move the other two excluded, at 6 runs red
/// before this lock and 6 green after. The other two are the
/// `DOT_AGENT_DECK_ATTACH_SOCKET` override that `endpoint_test::tests` sets
/// process-wide (every `Endpoint::local()` in the crate reads it, and only
/// `endpoint_test` holds `ATTACH_ENV_LOCK` while it moves) and the umask
/// `bind_attach_listener` flips inside `daemon_bridge::tests`' `RealDeck`
/// (documented and accepted there). Both need their own change and neither is
/// what this guards.
///
/// **Async-aware on purpose**, matching `endpoint_test::tests`'
/// `ATTACH_ENV_LOCK`: most of the writers are `#[tokio::test]`s that hold the
/// selection across an `.await`, and a `std::sync::MutexGuard` doing that is
/// `clippy::await_holding_lock` — an error under the workspace's `-D warnings`.
/// It also has no poisoning, so a test that panics mid-selection releases the
/// lock rather than turning its own failure into one in every sibling.
///
/// So take it with `.lock().await` from an async test and `blocking_lock()`
/// from a synchronous one — the latter panics in an async context.
#[cfg(test)]
pub(crate) static SELECTION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The applied selection, as **one** read.
///
/// Every caller goes through here rather than reaching for a half, which is the
/// whole of what [`AppliedSelection`] is for: two calls are two reads and can
/// straddle a write, and a caller that needs both halves to agree — every caller
/// that needs either — would be back where M8 found it.
fn applied_selection() -> AppliedSelection {
    APPLIED_SELECTION
        .read()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default()
}

/// Apply a settings document's selection. Returns the deck now in force.
///
/// The fallback is rendered here rather than stored as a type because the only
/// consumer is a sentence on screen, and `SelectionFallback: Display` already
/// writes it.
///
/// PRD #742 M3: this also applies the document's **observed set**. The two are
/// applied together on purpose — a probe that asked one and a watcher that asked
/// the other could otherwise disagree about whether a deck is in the fleet, and
/// disagreeing is precisely how a live transport gets released out from under a
/// watcher holding a lease on it (see `endpoint_test::release_if_not_observed`).
/// **PRD #742 M8 made "together" mean one write of one value** rather than two
/// writes a reader could land between; see [`AppliedSelection`].
pub(crate) fn apply_settings_selection(
    settings: &crate::settings::DesktopSettings,
) -> SelectedDeck {
    let resolved = settings.resolve_endpoint();
    let deck = SelectedDeck {
        endpoint: resolved.endpoint,
        fallback: resolved
            .fallback
            .map(|fallback| safe_display_text(fallback.to_string())),
    };
    let observed = settings.connectable_endpoints();
    if let Ok(mut slot) = APPLIED_SELECTION.write() {
        // The previous value is read **under the write lock**, so the compare
        // and the bump are one critical section. Reading it through
        // `applied_selection()` first would let two concurrent saves both see
        // epoch G and both write G+1 while describing different fleets, and a
        // scope captured at that G+1 would then revalidate against the other
        // save's — the one case where a monotonic counter can repeat a value
        // that means two things.
        let previous = slot.clone().unwrap_or_default();
        let departed = previous.observed.iter().any(|before| {
            let key = before.identity();
            !observed.iter().any(|kept| kept.identity() == key)
        });
        *slot = Some(AppliedSelection {
            selected: deck.clone(),
            observed,
            unconfigured: settings.unconfigured_decks(),
            observed_generation: previous.observed_generation + u64::from(departed),
        });
    }
    deck
}

/// The deck **one operation** acts on, captured once before its first await.
///
/// # The defect class this exists to close
///
/// The applied selection is mutable process state, so reading it is a question
/// about *now* — and an operation that reads it twice with an await in between
/// asks that question twice and can get two answers. Three instances shipped,
/// all with the same shape and none of them noticed by three security audits
/// aimed at the subject matter rather than at the shape:
///
/// - `DesktopAction::StopAgent` read [`selected_endpoint`] before
///   `DaemonLinks::trusted` and **again** after `stop_agent(…).await` returned,
///   using the second read to pick which deck's session to detach. Move the
///   selection while the request is in flight and the daemon stops deck A's
///   `planner` while the cleanup detaches deck **B's** same-id session. Agent
///   ids are per-daemon monotonic integers, so the collision is the ordinary
///   case rather than a contrived one.
/// - `crate::terminal::attach` validated its `deck_id` against
///   [`observed_decks`], then waited for a process-wide gate and performed the
///   handshake and stream attach with no further check — so an attach queued
///   behind a slower one could publish a session for a deck the user removed
///   while it waited.
/// - `crate::daemon_bridge::bootstrap` read the selection three times across
///   two awaits: once for the snapshot that decides whether to lazy-spawn, once
///   for the address to spawn at, and once for the snapshot it answers with.
///
/// # The mechanism
///
/// An operation captures a scope **once**, before its first await, and every
/// later step — including cleanup after an await — reads [`Self::endpoint`] or
/// [`Self::identity`] and never the applied selection again. Where the
/// operation also *publishes* something durable, it calls [`Self::revalidate`]
/// at the publication point.
///
/// This is deliberately the same answer [`crate::generation`] gives for
/// `DaemonLinks` and `EndpointTunnels`, one layer up: that module guards a
/// *cache entry* against a teardown, and this guards an *operation's notion of
/// which deck it is* against the selection moving underneath it.
///
/// # What it does not do
///
/// It cannot stop a function from capturing two scopes, and nothing here is
/// compiler-enforced. What it buys is that the captured value and the live
/// value are different expressions — `scope.endpoint()` versus
/// `selected_endpoint()` — so mixing them is visible at the call site instead
/// of reading like one idea. `crate::selection_capture` pins the count of raw
/// reads so a new one has to be added deliberately.
#[derive(Debug, Clone)]
pub(crate) struct DeckScope {
    endpoint: Endpoint,
    /// [`AppliedSelection::observed_generation`] as it stood at capture.
    observed_generation: u64,
}

impl DeckScope {
    /// The selected deck, captured for this operation.
    ///
    /// The selected deck is **not** necessarily in the observed set — see
    /// [`observed_fleet`], which prepends it for exactly that reason — so this
    /// makes no membership claim. [`Self::revalidate`] is still meaningful for
    /// it, because what that asks is whether the fleet moved, not whether this
    /// deck was ever in it.
    pub(crate) fn selected() -> Self {
        let applied = applied_selection();
        Self {
            endpoint: applied.selected.endpoint,
            observed_generation: applied.observed_generation,
        }
    }

    /// The deck one wire id names, or the selected deck when the caller named
    /// none.
    ///
    /// # Why this is not `trusted_daemon`
    ///
    /// `crate::daemon_bridge::trusted_daemon` resolves [`selected_endpoint`] —
    /// the applied selection, read at the instant the call runs. Every terminal
    /// verb went through it, so an attach declared for an agent on build-box
    /// reached whatever deck happened to be selected when the command was
    /// dispatched, and agent ids are per-daemon monotonic integers: it found a
    /// `planner` there and streamed it. That is issue
    /// [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116)'s whole
    /// shape at the one layer where it decides which machine the bytes come
    /// from.
    ///
    /// # The resolution is against the OBSERVED set, and that is a security
    /// boundary rather than a lookup detail
    ///
    /// A deck id from the webview is untrusted input. Matching it against the
    /// observed set means the only endpoints reachable are the ones the applied
    /// settings document already tells this app to connect to, so a malformed
    /// or stale id yields a refusal rather than a connection: there is no path
    /// here by which a value from the webview becomes an address.
    ///
    /// `None` keeps the previous behaviour for a caller that names no deck.
    /// No production webview path sends one — the webview's own attach always
    /// names the deck it is attaching to — but callers in this tree do:
    /// `DesktopAction::AttachTerminal` is the legacy declarative attach path
    /// and passes `None`, for which it legitimately means "the selected
    /// deck". That action is not reachable from the frontend today, which
    /// defines the variant in its action union and dispatches it from
    /// nowhere. `Option` is kept because the parameter is optional on the IPC
    /// boundary, so absence must mean something defined rather than an error
    /// the user cannot act on.
    pub(crate) fn resolve(deck_id: Option<&str>) -> Result<Self, String> {
        // ONE read, so the endpoint and the epoch describe the same fleet.
        let applied = applied_selection();
        let Some(deck_id) = deck_id else {
            return Ok(Self {
                endpoint: applied.selected.endpoint,
                observed_generation: applied.observed_generation,
            });
        };
        let endpoint = applied
            .observed
            .into_iter()
            .find(|endpoint| deck_wire_id(endpoint) == deck_id)
            .ok_or_else(|| {
                format!(
                    "that deck is not one this app is observing: {}",
                    safe_message(deck_id)
                )
            })?;
        Ok(Self {
            endpoint,
            observed_generation: applied.observed_generation,
        })
    }

    /// The deck this operation acts on. Every step after the capture reads this
    /// rather than the applied selection.
    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// [`Self::endpoint`]'s key — what both `DaemonLinks` and `EndpointTunnels`
    /// are indexed by, and what a session records.
    pub(crate) fn identity(&self) -> dot_agent_deck::daemon_client::EndpointIdentity {
        self.endpoint.identity()
    }

    /// Is this scope still the fleet's, or did a deck leave while the operation
    /// was in flight?
    ///
    /// Called at the point an operation publishes something that outlives it —
    /// a terminal session, a lease, a watcher. An operation with nothing to
    /// publish (a one-shot daemon request whose only later step is cleanup on
    /// its *own* captured deck) does not need it: capturing once is already
    /// enough for that shape.
    ///
    /// # ONE check, and it is the epoch rather than membership
    ///
    /// Membership answers "is that deck there **now**", which a deck that left
    /// and came back passes — and a returning deck's link and transport are
    /// from a later epoch than the one this operation established against, so
    /// publishing into it would install a session over a tunnel the teardown
    /// already released. The epoch distinguishes the two and membership cannot,
    /// which is why membership below only picks the wording.
    ///
    /// # It is conservative, and here is the cost
    ///
    /// The epoch does not say *which* deck departed, so removing deck E refuses
    /// an in-flight attach on unrelated deck D. That costs one refused attach
    /// on a settings save that happened to land inside one, which the webview
    /// recovers from by attaching again; the precise answer would need per-deck
    /// departure history, and a removal is a deliberate, rare user act. This is
    /// the same trade [`crate::generation`]'s module docs take for the same
    /// reason.
    pub(crate) fn revalidate(&self) -> Result<(), String> {
        let applied = applied_selection();
        if applied.observed_generation == self.observed_generation {
            return Ok(());
        }
        let key = self.endpoint.identity();
        let still_observed = applied
            .observed
            .iter()
            .any(|observed| observed.identity() == key);
        let deck = safe_display_text(self.endpoint.describe());
        Err(if still_observed {
            format!(
                "the fleet changed while this operation was in flight, so nothing was published for {deck}"
            )
        } else {
            format!("that deck left the fleet while this operation was in flight: {deck}")
        })
    }

    /// A scope over `endpoint` captured **now**, for a test pinning something
    /// other than this boundary.
    ///
    /// It revalidates for as long as no deck leaves the observed set after it
    /// is taken, which is what a test about (say) registry eviction wants: the
    /// boundary stays armed rather than being stubbed out, and it simply has
    /// nothing to object to. A test about the boundary itself captures one of
    /// these and *then* moves the fleet.
    #[cfg(test)]
    pub(crate) fn capturing(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            observed_generation: applied_selection().observed_generation,
        }
    }
}

/// Every deck the app CONNECTS to under the applied document (PRD #742 M3).
///
/// One element for every selection but [`crate::settings::Selection::All`], and
/// for that one the local deck followed by every configured row that has
/// somewhere to connect to — [`crate::settings::EndpointSettings::connectable_endpoints`]
/// is where that judgement is made and this only stores its answer.
///
/// **Not the fleet the screen shows.** A configured row with no socket path is
/// deliberately absent here and present in [`observed_fleet`]; see
/// [`unconfigured_decks`] for the other half and why the two are separate.
///
/// **Also a read of MUTABLE state.** A caller resolving one deck out of this
/// set for an operation wants [`DeckScope::resolve`], which takes the set and
/// the epoch in one read so the pair cannot straddle a save.
pub(crate) fn observed_decks() -> Vec<Endpoint> {
    applied_selection().observed
}

/// Every configured deck the app cannot connect to under the applied document
/// (PRD #742 M12) — a row whose socket path is not filled in yet.
pub(crate) fn unconfigured_decks() -> Vec<crate::settings::UnconfiguredDeck> {
    applied_selection().unconfigured
}

/// Is `endpoint` one of the decks the fleet observes?
///
/// Compared by [`EndpointIdentity`] — the key both `DaemonLinks` and
/// `EndpointTunnels` are indexed by — and never by `describe()`, which omits the
/// remote socket path, the identity file and the jump host. PRD #741's Greptile
/// P1 is the reason: two decks differing only in one of those three read as one
/// deck under a display string, and here that would answer "yes, we observe it"
/// for a deck nothing is watching.
pub(crate) fn deck_is_observed(endpoint: &Endpoint) -> bool {
    let key = endpoint.identity();
    applied_selection()
        .observed
        .iter()
        .any(|observed| observed.identity() == key)
}

/// The selected deck, with whatever fallback reason came with it.
pub(crate) fn selected_deck() -> SelectedDeck {
    applied_selection().selected
}

/// The deck this app is talking to (PRD #741 M2, selected since M7).
///
/// A function rather than a constant so the selection has exactly one source,
/// and so the call sites that must refuse a remote deck — the Stop and Replace
/// actions — are written against an [`Endpoint`] rather than against a path.
///
/// **This is a read of MUTABLE state, so it answers "now" and not "the deck my
/// operation is about".** An operation that keeps using the deck after an await
/// — cleanup included — captures a [`DeckScope`] instead; see that type for the
/// three shipped defects that are all this function called twice.
pub(crate) fn selected_endpoint() -> Endpoint {
    selected_deck().endpoint
}

/// How **this** deck is named in the connection banner. For a local deck that is
/// its socket path, which is exactly what this reported before the endpoint type
/// existed; for a remote one it is `user@host` (with the port when it is not 22),
/// derived by `RemoteEndpoint::describe` from validated fields.
///
/// **It takes the deck it is naming (PRD #742 M3).** It read the process-global
/// selection until then, which was invisible while there was one deck and is the
/// whole of items 8 and 9: `snapshot_with` already took the endpoint as a
/// parameter and this dropped it at the one place the identity is stamped, so
/// every deck in a fleet came back under the selected deck's name and the
/// frontend's `(daemonId, agentId)` composite key — `daemonId` is derived from
/// this string at `desktop/src/lib/bridge.ts` — collapsed two fleets into one.
///
/// Scrubbed through [`safe_display_text`] rather than [`safe_message`]. Every
/// byte of a stored endpoint came through an ASCII charset that excludes the
/// bidi codepoints, so today this cannot change the output — the seam is here
/// because the *next* thing rendered on this line may not have that property,
/// and a footer that strips only category `Cc` is a footer a reordering
/// character walks through.
pub(crate) fn deck_path_text(endpoint: &Endpoint) -> String {
    safe_display_text(endpoint.describe())
}

/// How **this** deck is KEYED — the other half of the pair [`deck_path_text`]
/// writes, and the one PRD #742 M5 added (`DesktopConnection::deck_id`).
///
/// `EndpointIdentity::wire_id()` and nothing else. The identity is where the
/// judgement lives — a new `RemoteEndpoint` field joins it by derive — so this
/// is one call rather than a second opinion about which fields name a deck,
/// which is the shape the defect took the first time: `describe()` was a
/// perfectly good display string that nobody had asked to be an identity.
///
/// Not run through [`safe_display_text`], deliberately and unlike its sibling.
/// The value is 21 ASCII bytes this build mints from a hash; a scrub would be
/// theatre over a string with no user-supplied byte in it, and the reader of
/// THIS line should be looking at `wire_id` rather than at a sanitiser to see
/// why the value is safe.
pub(crate) fn deck_wire_id(endpoint: &Endpoint) -> String {
    endpoint.identity().wire_id()
}

/// How an UNCONFIGURED deck is keyed on the wire (PRD #742 M12).
///
/// The row's own [`crate::settings::EndpointId`] under a prefix that
/// [`deck_wire_id`] can never produce: that mints `deck-<16 hex>`, so
/// `unconfigured-…` is disjoint from it whatever a user names a row — which
/// matters because the webview keys React elements, per-deck maps and the
/// `agentKey` composite on this value alongside real deck ids.
///
/// Not run through [`safe_display_text`], for the same reason [`deck_wire_id`]
/// is not: `EndpointId` accepts ASCII alphanumerics, `-` and `_` and nothing
/// else (its `Deserialize` runs the constructor's check), so there is no
/// user-supplied byte here that a scrub could act on.
pub(crate) fn unconfigured_deck_id(id: &crate::settings::EndpointId) -> String {
    format!("unconfigured-{}", id.as_str())
}

/// The unconfigured half of the fleet as [`DesktopSnapshot::unconfigured`]
/// carries it (PRD #742 M12).
///
/// One read of the applied selection, like [`observed_fleet`], and every
/// `DesktopSnapshot` construction calls both — the two lists have to describe
/// the same document or the webview gets an id in `fleet` with nothing to build
/// its group from.
pub(crate) fn unconfigured_fleet() -> Vec<UnconfiguredDeckDto> {
    unconfigured_decks()
        .into_iter()
        .map(|deck| UnconfiguredDeckDto {
            deck_id: unconfigured_deck_id(&deck.id),
            label: safe_display_text(deck.label),
            reason: UNCONFIGURED_DECK_REASON.to_string(),
        })
        .collect()
}

/// The connectable half of the fleet as [`DesktopSnapshot::observed`] carries
/// it — every observed deck NAMED, whether or not it has reported (PRD #742
/// M14).
///
/// One read of the applied selection, like [`observed_fleet`] and
/// [`unconfigured_fleet`], and in the same order as the `observed` half of
/// [`observed_fleet`] before its selected-first rotation. That rotation is
/// deliberately not applied here: the webview renders a deck out of this list
/// only while it has not reported, and such a deck is never the one leading the
/// fleet — the leader is the deck the bootstrap answered.
///
/// **The webview derives "has not reported yet" from THIS list and not from
/// [`DesktopSnapshot::fleet`]**, which is what keeps a torn read between the
/// two cheap: the three lists are three reads, so a save landing between them
/// can pair one document's `fleet` with another's `observed`, and an id in
/// `fleet` with nothing here to name it would otherwise be an unnameable group.
/// Deriving from here cannot produce one — every entry carries its own name —
/// and the next arrival restates all three.
pub(crate) fn observed_fleet_decks() -> Vec<ObservedDeckDto> {
    observed_decks()
        .iter()
        .map(|endpoint| ObservedDeckDto {
            deck_id: deck_wire_id(endpoint),
            label: deck_path_text(endpoint),
            deck_kind: selection_fields(endpoint).0,
        })
        .collect()
}

/// What a deck with no address says instead of a state.
///
/// The fleet view's idiom for the same fact
/// [`crate::settings::SelectionFallback::NoRemoteSocket`] states in the
/// selector's — deliberately the settings panel's own vocabulary rather than a
/// third one, because a user who sees this is being sent to that panel.
const UNCONFIGURED_DECK_REASON: &str = "Not configured yet — press Test connection in Settings.";

/// The observed fleet as [`DesktopSnapshot::fleet`] carries it — every observed
/// deck's [`deck_wire_id`], selected deck first, never empty (PRD #742 M5).
///
/// # The two invariants are enforced here, not assumed
///
/// [`crate::settings::EndpointSettings::connectable_endpoints`] already yields
/// them — it is `[resolve().endpoint]` for every selection but `All`, and `All`
/// leads with the local deck, which is what `All` resolves to. This
/// re-establishes them anyway, because they are what the webview's `fleet[0]`
/// reads as "the selected deck" and a silently wrong answer there binds every
/// single-deck surface to somebody else's machine. Restating a property the
/// producer already has is cheap; discovering it moved is not.
///
/// # This is the DISPLAY set, and it is wider than the connectable one
///
/// PRD #742 M12: a configured row with no socket path is in here, under
/// [`unconfigured_deck_id`], and is deliberately not in [`observed_decks`].
/// It is a deck the user created and should see — so it gets a group and a
/// place in the fleet's denominator — and there is no address to give a
/// watcher, a tunnel or a handshake. Conflating the two sets is what made a
/// socketless deck vanish from the overview entirely: absent from the
/// numerator, absent from the denominator, and with no group on screen.
///
/// A selected deck that is somehow absent from the observed set is PREPENDED
/// rather than dropped: the app is talking to it either way, so a fleet that
/// omitted it would render the deck screen's own agents under no group at all.
pub(crate) fn observed_fleet() -> Vec<String> {
    // ONE read for both halves (PRD #742 M8). It used to be `selected_endpoint()`
    // followed by `observed_decks()`, which is two, and a save landing between
    // them produced a fleet whose head named a deck the list did not contain —
    // repaired below by the prepend, but repaired rather than prevented.
    let applied = applied_selection();
    let selected = deck_wire_id(&applied.selected.endpoint);
    let mut fleet: Vec<String> = applied.observed.iter().map(deck_wire_id).collect();
    // PRD #742 M12: the configured decks with no address, after the ones that
    // have one. They are members of the fleet and never of the connectable set,
    // so this is the one place the two lists are added together — and it is the
    // display side, which is the only side that wants them.
    fleet.extend(
        applied
            .unconfigured
            .iter()
            .map(|deck| unconfigured_deck_id(&deck.id)),
    );
    match fleet.iter().position(|id| *id == selected) {
        Some(0) => {}
        Some(at) => {
            let id = fleet.remove(at);
            fleet.insert(0, id);
        }
        None => fleet.insert(0, selected),
    }
    fleet
}

/// The disconnected snapshot **for one deck** (PRD #742 M3 takes the endpoint).
///
/// Every failure path in `snapshot_with` reaches here, and each of them already
/// had the endpoint in hand — so before M3 a fleet's unreachable deck reported
/// its failure under the *selected* deck's name, which on the overview would
/// have painted a healthy deck as disconnected.
pub(crate) fn disconnected_snapshot(
    endpoint: &Endpoint,
    error: impl AsRef<str>,
) -> DesktopSnapshot {
    let (deck_kind, local_only_reason, selection_fallback) = selection_fields(endpoint);
    DesktopSnapshot {
        connection: DesktopConnection {
            status: ConnectionStatus::Disconnected,
            socket_path: deck_path_text(endpoint),
            deck_id: deck_wire_id(endpoint),
            error: Some(safe_message(error)),
            client_protocol_version: PROTOCOL_VERSION,
            server_protocol_version: None,
            client_build_version: dot_agent_deck::build_id::local_build_id(),
            daemon_build_version: None,
            daemon_version: None,
            running_agent_count: None,
            build_stamp_mismatch_only: false,
            deck_kind,
            local_only_reason,
            selection_fallback,
            // Nothing was advertised because nothing answered. A disconnected
            // screen is already saying the only thing there is to say.
            project_actions_reason: None,
            new_agent_reason: None,
        },
        agents: Vec::new(),
        // Issue #887: nothing answered, so this daemon reported no revision.
        schedule_revision: None,
        protocol_version: PROTOCOL_VERSION,
        source: "daemon",
        fleet: observed_fleet(),
        unconfigured: unconfigured_fleet(),
        observed: observed_fleet_decks(),
    }
}

pub(crate) fn validate_agent_id(agent_id: &str) -> Result<(), String> {
    if agent_id.is_empty()
        || agent_id.len() > AGENT_ID_MAX_BYTES
        || agent_id.chars().any(char::is_control)
    {
        return Err(format!(
            "agentId must be 1..={AGENT_ID_MAX_BYTES} bytes without control characters"
        ));
    }
    Ok(())
}

pub(crate) fn validate_dimensions(rows: u16, cols: u16) -> Result<(u16, u16), String> {
    if rows == 0 || cols == 0 {
        return Err(format!(
            "rows and cols must be greater than zero (got {rows}x{cols})"
        ));
    }
    // Issue #747: through the shared helper rather than a second copy of the
    // `.min()` pair, so the desktop bridge cannot drift from the TUI and the
    // daemon about what geometry a resize request actually produces.
    Ok(clamp_pty_dims(rows, cols))
}

pub(crate) fn validate_command(command: Option<&str>) -> Result<(), String> {
    if let Some(command) = command {
        if command.trim().is_empty() {
            return Err("command must not be empty or whitespace-only".into());
        }
        if command.len() > COMMAND_MAX_BYTES || command.contains('\0') {
            return Err(format!(
                "command must be at most {COMMAND_MAX_BYTES} bytes and contain no NUL"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_start_fields(
    command: Option<&str>,
    cwd: Option<&str>,
    display_name: Option<&str>,
    rows: u16,
    cols: u16,
) -> Result<(u16, u16), String> {
    validate_command(command)?;
    if let Some(cwd) = cwd
        && !is_valid_cwd(cwd)
    {
        return Err("cwd is invalid, oversized, empty, or contains control characters".into());
    }
    if let Some(display_name) = display_name
        && !is_valid_display_name(display_name)
    {
        return Err(
            "displayName is invalid, oversized, empty, or contains control characters".into(),
        );
    }
    validate_dimensions(rows, cols)
}

pub(crate) fn validate_terminal_input(data: &[u8]) -> Result<(), String> {
    if data.len() > TERMINAL_INPUT_MAX_BYTES {
        return Err(format!(
            "terminal input chunk exceeds the {TERMINAL_INPUT_MAX_BYTES}-byte limit"
        ));
    }
    Ok(())
}

pub(crate) fn mint_desktop_pane_id() -> String {
    static NONCE: OnceLock<u64> = OnceLock::new();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nonce = *NONCE.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        if let Ok(duration) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            duration.as_nanos().hash(&mut hasher);
        }
        hasher.finish()
    });
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pane_id = format!("desktop-{nonce:016x}-{sequence}");
    debug_assert!(is_valid_pane_id_env(&pane_id));
    pane_id
}

pub(crate) fn validate_workflow_shape(
    name: &str,
    cwd: &str,
    roles: &[WorkflowRoleInput],
    rows: u16,
    cols: u16,
) -> Result<(u16, u16), String> {
    const MAX_WORKFLOW_ROLES: usize = 16;
    if !is_valid_display_name(name) {
        return Err(
            "workflow name is invalid, oversized, empty, or contains control characters".into(),
        );
    }
    if !is_valid_orchestration_cwd(cwd) {
        return Err("workflow cwd must be a valid absolute path without control characters".into());
    }
    if roles.is_empty() || roles.len() > MAX_WORKFLOW_ROLES {
        return Err(format!(
            "workflow roles must contain 1..={MAX_WORKFLOW_ROLES} entries"
        ));
    }
    let mut names = HashSet::with_capacity(roles.len());
    let mut start_count = 0usize;
    for role in roles {
        if !is_valid_display_name(&role.role) {
            return Err(format!(
                "invalid workflow role name: {}",
                safe_message(&role.role)
            ));
        }
        if !names.insert(role.role.as_str()) {
            return Err(format!(
                "duplicate workflow role: {}",
                safe_message(&role.role)
            ));
        }
        validate_command(Some(&role.command))?;
        start_count += usize::from(role.start);
    }
    if start_count != 1 {
        return Err(format!(
            "workflow must define exactly one start role (found {start_count})"
        ));
    }
    validate_dimensions(rows, cols)
}

pub(crate) fn ensure_desktop_workflow_platform_supported(target_os: &str) -> Result<(), String> {
    if target_os == "windows" {
        return Err(
            "desktop workflow launch is unavailable on Windows in this preview because profile commands are POSIX-shell quoted; use the TUI or launch commands manually until native Windows command construction is implemented"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // PRD #741 M7 — the selection, and the two things it decides on screen
    // -----------------------------------------------------------------------

    /// A document whose `[endpoints]` selection names a connectable remote row.
    fn selecting_a_remote_deck() -> crate::settings::DesktopSettings {
        use crate::settings::{
            DesktopSettings, EndpointId, EndpointSettings, RemoteEndpointSettings, Selection,
        };
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let id = EndpointId::parse("deck00000000000b").expect("a valid id");
        let mut row = RemoteEndpointSettings::new(
            id.clone(),
            Hostname::parse("build-box").expect("a valid host"),
        );
        row.user = Some(dot_agent_deck::remote_tunnel::SshUser::parse("deploy").expect("user"));
        row.socket = Some(
            RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock").expect("path"),
        );
        DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![row],
                selection: Selection::One(id),
            }),
            ..DesktopSettings::default()
        }
    }

    /// Apply `settings`, run `body`, and restore the default selection.
    fn with_selection<T>(
        settings: &crate::settings::DesktopSettings,
        body: impl FnOnce() -> T,
    ) -> T {
        let guard = SELECTION_LOCK.blocking_lock();
        apply_settings_selection(settings);
        let result = body();
        apply_settings_selection(&crate::settings::DesktopSettings::default());
        drop(guard);
        result
    }

    /// The fleet on the wire lists the unaddressed deck; the set anything
    /// connects to does not (PRD #742 M12).
    ///
    /// The split at the seam the webview reads. Both halves come from ONE
    /// applied document, so this also pins that `apply_settings_selection`
    /// stores them together — a reader that got the two from different saves
    /// could show an unconfigured group for a deck that had just gained an
    /// address, beside the real group its new watcher emits.
    #[test]
    fn the_wire_fleet_carries_the_unaddressed_deck_and_the_observed_set_does_not() {
        use crate::settings::{EndpointId, EndpointSettings, RemoteEndpointSettings, Selection};
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let connectable = |index: usize, host: &str| {
            let id = EndpointId::parse(&format!("deck00000000000{index}")).expect("a valid id");
            let mut row =
                RemoteEndpointSettings::new(id, Hostname::parse(host).expect("a valid host"));
            row.socket = Some(RemoteSocketPath::parse("/run/deck.sock").expect("a path"));
            row
        };
        let halfway = EndpointId::parse("halfway").expect("a valid id");
        let settings = crate::settings::DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![
                    connectable(0, "build-box.example.com"),
                    connectable(1, "laptop.example.com"),
                    RemoteEndpointSettings::new(
                        halfway.clone(),
                        Hostname::parse("relay.example.com").expect("a valid host"),
                    ),
                ],
                selection: Selection::All,
            }),
            ..crate::settings::DesktopSettings::default()
        };

        with_selection(&settings, || {
            let wire = observed_fleet();
            assert_eq!(
                wire.len(),
                4,
                "the local deck, the two with an address, and the one without — the \
                 denominator the overview header states: {wire:?}"
            );
            assert!(
                wire.contains(&unconfigured_deck_id(&halfway)),
                "the unaddressed deck is a member of the fleet: {wire:?}"
            );
            assert_eq!(
                wire[0],
                deck_wire_id(&selected_endpoint()),
                "and the selected deck still leads, which every single-deck surface reads"
            );
            assert_eq!(
                observed_decks().len(),
                3,
                "while the set that gets a watcher, a tunnel and a handshake is unchanged"
            );

            let stated = unconfigured_fleet();
            assert_eq!(stated.len(), 1, "one deck to render as unconfigured");
            assert_eq!(stated[0].deck_id, unconfigured_deck_id(&halfway));
            assert_eq!(stated[0].label, "relay.example.com");
            assert!(
                !stated[0].reason.is_empty(),
                "the group needs a sentence saying why it is empty"
            );
            assert!(
                stated.iter().all(|deck| !deck.deck_id.starts_with("deck-")),
                "an unconfigured id must never look like a real one: {stated:?}"
            );
        });
    }

    /// Every connectable deck is NAMED on the wire, so a deck that has not
    /// reported yet can still be rendered as itself (PRD #742 M14).
    ///
    /// The webview draws a group for every observed deck the moment the first
    /// snapshot lands — that is what keeps the header's denominator still while
    /// a remote deck's tunnel is being established, instead of `1/1` becoming
    /// `2/2` under the reader. It cannot name that group from
    /// [`DesktopSnapshot::fleet`], whose entries are [`deck_wire_id`] hashes,
    /// and a nameless group would fall through to "Local deck" for a remote
    /// deck. So the label and the kind ride along, exactly as
    /// [`unconfigured_fleet`] carries them for a row with no address.
    ///
    /// The pairing is what matters and is what is asserted: each entry's
    /// `deck_id` is the key that deck's own snapshot will arrive under, and its
    /// `label` is the string that snapshot will carry — so nothing about the
    /// group moves when the deck finally reports.
    #[test]
    fn every_observed_deck_is_named_on_the_wire_before_it_reports() {
        use crate::settings::{EndpointId, EndpointSettings, RemoteEndpointSettings, Selection};
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let id = EndpointId::parse("buildbox0000000").expect("a valid id");
        let mut row = RemoteEndpointSettings::new(
            id,
            Hostname::parse("build-box.example.com").expect("a valid host"),
        );
        row.socket = Some(RemoteSocketPath::parse("/run/deck.sock").expect("a path"));
        let settings = crate::settings::DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![row],
                selection: Selection::All,
            }),
            ..crate::settings::DesktopSettings::default()
        };

        with_selection(&settings, || {
            let named = observed_fleet_decks();
            let observed = observed_decks();
            assert_eq!(
                named.len(),
                observed.len(),
                "one named entry per connectable deck: {named:?}"
            );
            for (entry, endpoint) in named.iter().zip(observed.iter()) {
                assert_eq!(
                    entry.deck_id,
                    deck_wire_id(endpoint),
                    "the key a deck's own snapshot will arrive under"
                );
                assert_eq!(
                    entry.label,
                    deck_path_text(endpoint),
                    "the label that snapshot will carry, so the name does not move"
                );
                assert_eq!(entry.deck_kind, selection_fields(endpoint).0);
            }

            let remote = named
                .iter()
                .find(|entry| entry.deck_kind == "remote")
                .expect("the configured row is connectable and remote");
            assert_eq!(
                remote.label, "build-box.example.com",
                "named by its address, which is what the group prints"
            );
            assert!(
                named.iter().any(|entry| entry.deck_kind == "local"),
                "and the local deck is in here too, so the webview needs no second source"
            );

            // The three lists describe one document: every named entry is a
            // member of the fleet, and none of them is an unconfigured id.
            let wire = observed_fleet();
            assert!(
                named.iter().all(|entry| wire.contains(&entry.deck_id)),
                "a named deck that is not in the fleet could never be rendered: {named:?}"
            );
            assert!(
                unconfigured_fleet().is_empty(),
                "every configured row here has an address"
            );
        });
    }

    /// Selecting a remote deck disables the two daemon-lifecycle controls, and
    /// the explanation is `require_local`'s own sentence rather than a new one.
    ///
    /// This is the property M2 built by type and M7 renders: **Stop daemon**
    /// and **Replace daemon** act on a process on *this* machine, and over a
    /// forwarded socket `run_daemon_stop`'s `SO_PEERCRED` lookup would name the
    /// local `ssh` client — so pressing Stop would tear the tunnel down and
    /// report that a daemon had stopped gracefully.
    #[test]
    fn a_remote_selection_disables_the_daemon_lifecycle_controls() {
        let (kind, local_only, fallback) = with_selection(&selecting_a_remote_deck(), || {
            selection_fields(&selected_endpoint())
        });

        assert_eq!(kind, "remote");
        assert_eq!(
            fallback, None,
            "the selection resolved, so nothing fell back"
        );
        let reason = local_only.expect("a remote deck must say why Stop and Replace are off");
        assert!(
            reason.contains("Stop deck") && reason.contains("deploy@build-box"),
            "the explanation must name the operation and the deck: {reason}"
        );
        assert!(
            reason.contains("not the machine that deck runs on"),
            "the explanation is `Endpoint::require_local`'s, not a second one written here: \
             {reason}"
        );
    }

    /// The default — and every document with no `[endpoints]` section — is the
    /// local deck, with the controls enabled and nothing to explain.
    #[test]
    fn the_local_deck_is_the_default_and_keeps_its_controls() {
        let (kind, local_only, fallback) =
            with_selection(&crate::settings::DesktopSettings::default(), || {
                selection_fields(&selected_endpoint())
            });

        assert_eq!(kind, "local");
        assert_eq!(local_only, None);
        assert_eq!(fallback, None);
    }

    /// A selection naming a row the document no longer holds falls back to the
    /// local deck **and says so**. "That deck is gone" is not "connected to
    /// local", and a silent substitution is how a user ends up acting on the
    /// wrong machine's agents.
    #[test]
    fn a_selection_that_cannot_be_honoured_reports_why() {
        use crate::settings::{DesktopSettings, EndpointId, EndpointSettings, Selection};

        let missing = DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: Vec::new(),
                selection: Selection::One(
                    EndpointId::parse("deck00000000000c").expect("a valid id"),
                ),
            }),
            ..DesktopSettings::default()
        };
        let (kind, local_only, fallback) =
            with_selection(&missing, || selection_fields(&selected_endpoint()));

        assert_eq!(kind, "local", "the app still has a deck to talk to");
        assert_eq!(local_only, None, "and its controls still work");
        let reason = fallback.expect("an unhonoured selection must be reported");
        assert!(
            reason.contains("no longer configured"),
            "the sentence must distinguish a missing row from a row with no socket: {reason}"
        );
    }

    /// The other fallback: a row that exists and has no socket path yet — the
    /// state M6 made a first-class answer so a half-configured deck is
    /// something the UI can explain rather than a save that refuses.
    #[test]
    fn a_row_with_no_socket_path_falls_back_with_its_own_reason() {
        use crate::settings::{
            DesktopSettings, EndpointId, EndpointSettings, RemoteEndpointSettings, Selection,
        };
        use dot_agent_deck::remote_tunnel::Hostname;

        let id = EndpointId::parse("deck00000000000d").expect("a valid id");
        let row = RemoteEndpointSettings::new(
            id.clone(),
            Hostname::parse("build-box").expect("a valid host"),
        );
        let half_configured = DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![row],
                selection: Selection::One(id),
            }),
            ..DesktopSettings::default()
        };
        let (kind, _, fallback) =
            with_selection(&half_configured, || selection_fields(&selected_endpoint()));

        assert_eq!(kind, "local");
        let reason = fallback.expect("a socket-less row must be reported");
        assert!(
            reason.contains("no remote socket path yet"),
            "this is a different thing to tell a user than a missing row: {reason}"
        );
    }

    // -----------------------------------------------------------------------
    // PRD #742 M5 — the deck id, and exact fleet membership
    // -----------------------------------------------------------------------

    /// A document observing the whole fleet: two remote rows that
    /// `Endpoint::describe()` renders IDENTICALLY, differing only in the remote
    /// socket path.
    ///
    /// That is not a contrived pair — it is two daemons on one host, which is
    /// exactly what the `socket` field exists to distinguish. Under the old
    /// identity the two rows collapsed onto one key, which is the defect M5
    /// closes.
    fn two_daemons_on_one_host() -> crate::settings::DesktopSettings {
        use crate::settings::{
            DesktopSettings, EndpointId, EndpointSettings, RemoteEndpointSettings, Selection,
        };
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let row = |id: &str, socket: &str| {
            let mut row = RemoteEndpointSettings::new(
                EndpointId::parse(id).expect("a valid id"),
                Hostname::parse("build-box").expect("a valid host"),
            );
            row.socket = Some(RemoteSocketPath::parse(socket).expect("a valid remote socket"));
            row
        };
        DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![
                    row("deck000000000010", "/run/deck-one.sock"),
                    row("deck000000000011", "/run/deck-two.sock"),
                ],
                selection: Selection::All,
            }),
            ..DesktopSettings::default()
        }
    }

    /// The snapshot's fleet is every observed deck, in observed order, selected
    /// first — and two decks the label cannot tell apart are two entries.
    ///
    /// Scenario: apply a document selecting `All` with two remote rows on one
    /// host differing only in their remote socket path, then take a snapshot.
    /// `fleet` must hold three distinct ids, led by the local deck (which is
    /// what `All` resolves to), and `connection.deckId` must be the local
    /// deck's — while the two remote rows, whose `socketPath` label is one
    /// string, hold two different ids.
    #[test]
    fn the_snapshot_fleet_is_the_observed_set_selected_first() {
        let settings = two_daemons_on_one_host();
        let (fleet, snapshot, labels, ids) = with_selection(&settings, || {
            let observed = observed_decks();
            (
                observed_fleet(),
                disconnected_snapshot(&selected_endpoint(), "nothing is listening"),
                observed.iter().map(deck_path_text).collect::<Vec<_>>(),
                observed.iter().map(deck_wire_id).collect::<Vec<_>>(),
            )
        });

        assert_eq!(
            fleet, ids,
            "the fleet is the observed set, in observed order"
        );
        assert_eq!(
            fleet.len(),
            3,
            "the local deck plus both configured rows: {fleet:?}"
        );
        assert_eq!(
            fleet[0], snapshot.connection.deck_id,
            "`fleet[0]` is the selected deck, which is what the webview binds \
             its single-deck surfaces to"
        );
        assert_eq!(
            snapshot.fleet, fleet,
            "every snapshot carries the same list, so a webview may prune on \
             whichever one lands first"
        );
        assert_eq!(
            fleet.iter().collect::<HashSet<_>>().len(),
            3,
            "three observed decks, three distinct keys: {fleet:?}"
        );
        assert_eq!(
            labels[1], labels[2],
            "this case only means something while the LABEL cannot tell the two \
             daemons apart: {labels:?}"
        );
        assert_ne!(
            fleet[1], fleet[2],
            "two daemons on one host are two decks, however they describe"
        );
    }

    /// Every selection but `All` observes exactly the deck it resolved to, so
    /// the fleet is that one deck and the fleet view is a single group.
    ///
    /// The invariant that matters here is "never empty": an unreachable deck is
    /// still an entry carrying a disconnected connection, because an absent
    /// entry cannot tell "no agents" from "we cannot see the agents".
    #[test]
    fn a_single_deck_selection_is_a_one_entry_fleet() {
        let snapshot = with_selection(&selecting_a_remote_deck(), || {
            disconnected_snapshot(&selected_endpoint(), "the tunnel refused")
        });

        assert_eq!(snapshot.fleet, vec![snapshot.connection.deck_id.clone()]);
        assert_eq!(snapshot.connection.status, ConnectionStatus::Disconnected);
    }

    /// The deck id is a KEY and the socket path is a LABEL, and the wire says so
    /// with two fields rather than with one string doing both jobs.
    ///
    /// The second half is the part worth pinning: the id carries no endpoint
    /// text at all. A `deck_id` that happened to embed the host would be a
    /// `deck_id` somebody eventually reassembles by hand, which is how
    /// `describe()` became an identity in the first place.
    #[test]
    fn the_deck_id_is_opaque_and_the_socket_path_stays_the_label() {
        let (kind, snapshot) = with_selection(&selecting_a_remote_deck(), || {
            let endpoint = selected_endpoint();
            (
                selection_fields(&endpoint).0,
                disconnected_snapshot(&endpoint, "the tunnel refused"),
            )
        });

        assert_eq!(kind, "remote");
        assert_eq!(
            snapshot.connection.socket_path, "deploy@build-box",
            "the label is still `Endpoint::describe()`, unchanged"
        );
        let id = snapshot.connection.deck_id;
        assert!(
            id.starts_with("deck-") && !id.contains("build-box") && !id.contains('@'),
            "the deck id is opaque and carries no endpoint text: {id}"
        );
    }

    /// A bidi override in a settings-supplied host cannot reach the line whose
    /// whole job is telling the user which deck they are talking to.
    ///
    /// The stored newtypes exclude these bytes, so today this is the seam
    /// holding rather than the only thing between the user and a reordered
    /// footer — which is the point: `safe_message` alone strips category `Cc`
    /// and lets category `Cf` through.
    #[test]
    fn endpoint_display_text_strips_bidi_as_well_as_control_characters() {
        assert_eq!(safe_display_text("build\u{202e}xob"), "buildxob");
        assert_eq!(safe_display_text("deploy@build-box"), "deploy@build-box");
        assert!(
            safe_message("build\u{202e}xob").contains('\u{202e}'),
            "the control-only scrub is what this function exists to replace on this path"
        );
    }

    use dot_agent_deck::agent_pty::PTY_RESIZE_DIM_MAX;
    use dot_agent_deck::event::{LiveTarget, TargetKind};
    use dot_agent_deck::state::{ActiveTool, SessionSnapshot};

    fn fixture_record() -> AgentRecord {
        AgentRecord {
            id: "agent-7".into(),
            pane_id_env: Some("pane-7".into()),
            display_name: Some("builder".into()),
            cwd: Some("/tmp/project".into()),
            tab_membership: Some(TabMembership::Mode {
                name: "build".into(),
            }),
            agent_type: Some(AgentType::Codex),
            rows: 32,
            cols: 120,
            live: Some(SessionSnapshot {
                status: SessionStatus::Working,
                agent_type: Some(AgentType::Codex),
                active_tool: Some(ActiveTool {
                    name: "shell".into(),
                    detail: Some("cargo test".into()),
                }),
                tool_count: 4,
                first_prompts: Vec::new(),
                last_user_prompt: None,
                live_target: None,
                last_activity_ms: None,
            }),
            spawned_at_ms: None,
            // Issue #856: as the DAEMON reported it. The fixture agent is
            // Codex, and `codex` is what a codex daemon resolves.
            cli_name: Some("codex".into()),
            crashed: None,
        }
    }

    #[test]
    fn agent_mapping_is_frontend_stable() {
        let value = serde_json::to_value(map_agent(fixture_record())).unwrap();
        assert_eq!(value["id"], "agent-7");
        assert_eq!(value["paneId"], "pane-7");
        assert_eq!(value["agentType"], "codex");
        assert_eq!(value["cliName"], "codex");
        assert_eq!(value["status"], "working");
        assert_eq!(value["activeTool"]["name"], "shell");
        assert_eq!(value["tab"]["kind"], "mode");
    }

    /// Issue #856: the CLI column is the DAEMON's answer, copied through
    /// verbatim — and a daemon this build disagrees with about the registry
    /// still gets its own answer rendered.
    ///
    /// The fixture record reports Codex while naming `pinocchio`, a binary no
    /// `AgentSpec` in this build holds. Any local lookup — the
    /// `agent_registry::spec(agent_type).default_command` this used to do —
    /// answers `codex` for that record, so the assertion fails the moment a
    /// fallback is reintroduced. That is the whole point of the issue: the
    /// desktop and the daemon compile one table only because
    /// `classify_handshake` demands matching builds, and #801 exists to relax
    /// exactly that.
    #[test]
    fn the_cli_binary_is_copied_from_the_daemon_and_never_derived_locally() {
        let mut record = fixture_record();
        record.agent_type = Some(AgentType::Codex);
        record.cli_name = Some("pinocchio".into());
        assert_eq!(map_agent(record).cli_name.as_deref(), Some("pinocchio"));
    }

    /// Issue #856: a record the daemon named no binary for renders nothing, and
    /// the key stays OFF the wire so the webview reads absence rather than an
    /// empty string it would have to special-case.
    ///
    /// **Never a fallback to the local table**, which is what makes this the
    /// load-bearing half of the pair above. The record here reports a perfectly
    /// well-known `AgentType::Codex` — a local lookup has an answer for it and
    /// would print `codex`. Absence on the wire means the daemon named no
    /// binary, and inventing one from a table the daemon may not share
    /// reinstates the divergence the field closes.
    #[test]
    fn an_unnameable_cli_is_absent_from_the_serialized_shape() {
        let mut record = fixture_record();
        record.agent_type = Some(AgentType::Codex);
        record.cli_name = None;
        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["agentType"], "codex");
        assert!(value.get("cliName").is_none());
    }

    #[test]
    fn record_without_hook_state_is_still_running() {
        let mut record = fixture_record();
        record.live = None;
        let mapped = map_agent(record);
        assert_eq!(mapped.status, "running");
        assert_eq!(mapped.tool_count, 0);
        assert!(mapped.active_tool.is_none());
        // PRD #745 M8: no live snapshot means no prompt and no lease to report.
        // Absent, not blank — the webview must be able to tell "nothing to say"
        // from "the daemon said the empty string".
        assert!(mapped.last_user_prompt.is_none());
        assert!(mapped.write_lease.is_none());
        // PRD #745 M9: nor an activity time. This is the case a RESTARTED
        // daemon produces for every agent — it persists no `AppState`, so it
        // has no sessions to snapshot — and it is exactly why this field could
        // be shipped where session duration could not: the honest answer to
        // "when did this last do something" after a restart is "I do not
        // know", and absence says that.
        assert!(mapped.last_activity_ms.is_none());
        // PRD #745 M11: the spawn time is INDEPENDENT of `live`, so removing
        // the snapshot must not be what makes it absent — this fixture is
        // absent because the record itself reports no spawn (see
        // `agent_mapping_surfaces_the_spawn_instant_unchanged` for the present
        // case, which keeps `live` untouched).
        assert!(mapped.spawned_at_ms.is_none());
    }

    /// PRD #745 M8: the two `SessionSnapshot` fields the desktop's own DTO used
    /// to drop even though the daemon sends them and the desktop parses them.
    #[test]
    fn agent_mapping_surfaces_the_last_prompt_and_the_write_lease() {
        let mut record = fixture_record();
        let live = record.live.as_mut().unwrap();
        live.last_user_prompt = Some("ship the overview".into());
        live.live_target = Some(LiveTarget {
            kind: TargetKind::Pty,
            writable: Writable::Live,
        });

        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["lastUserPrompt"], "ship the overview");
        assert_eq!(value["writeLease"], "write");
    }

    /// PRD #745 M9: the daemon's `last_activity_ms` reaches the webview as the
    /// same integer, under `lastActivityMs`, with no reformatting and no clamp.
    ///
    /// Pinned on the SERIALIZED value because a `serde_json` number is where a
    /// silent unit change would show up — seconds instead of milliseconds
    /// divides it by a thousand and every relative time on the overview becomes
    /// fifty-seven years, which is why the field carries its unit in its name.
    #[test]
    fn agent_mapping_surfaces_the_last_activity_instant_unchanged() {
        let mut record = fixture_record();
        record.live.as_mut().unwrap().last_activity_ms = Some(1_756_684_800_123);

        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["lastActivityMs"], 1_756_684_800_123i64);
    }

    /// The value is NOT clamped or validated here, deliberately: the daemon's
    /// `last_activity` is producer-supplied and can land in the future, and the
    /// only seam that can judge that is the one holding the other clock. A
    /// far-future instant therefore crosses this boundary intact, and the
    /// webview is what refuses to relativise it.
    #[test]
    fn a_future_last_activity_crosses_the_dto_boundary_intact() {
        let far_future = 4_102_444_800_000i64; // 2100-01-01T00:00:00Z
        let mut record = fixture_record();
        record.live.as_mut().unwrap().last_activity_ms = Some(far_future);

        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["lastActivityMs"], far_future);
    }

    /// PRD #745 M11: the daemon's `spawned_at_ms` reaches the webview as the
    /// same integer, under `spawnedAtMs`, with no reformatting and no clamp —
    /// and it comes off the RECORD, so it survives a record carrying no live
    /// session at all.
    ///
    /// That last half is the whole reason spawn time beats
    /// `SessionState.started_at`: a session exists only once a hook event has
    /// arrived, so an agent that has never emitted one has no start instant —
    /// and it is exactly the agent whose uptime a reader most wants. Pinned on
    /// the SERIALIZED value for the same reason M9's is: a seconds/milliseconds
    /// slip is a ×1000 error, which is why the unit is in the name.
    #[test]
    fn agent_mapping_surfaces_the_spawn_instant_unchanged() {
        let mut record = fixture_record();
        record.spawned_at_ms = Some(1_756_684_800_123);

        let value = serde_json::to_value(map_agent(record.clone())).unwrap();
        assert_eq!(value["spawnedAtMs"], 1_756_684_800_123i64);

        // No live snapshot, same answer: the daemon knows when it forked a
        // process whether or not that process has ever reported anything.
        record.live = None;
        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["spawnedAtMs"], 1_756_684_800_123i64);
        assert!(value.get("lastActivityMs").is_none());
    }

    /// The absent case for both, pinned in the SERIALIZED shape: the keys are
    /// missing from the JSON the webview receives rather than present and null,
    /// so `agent.lastUserPrompt` is `undefined` there and absence survives the
    /// boundary.
    #[test]
    fn absent_prompt_and_lease_are_omitted_from_the_frontend_shape() {
        let value = serde_json::to_value(map_agent(fixture_record())).unwrap();
        assert!(value.get("lastUserPrompt").is_none());
        assert!(value.get("writeLease").is_none());
        // PRD #745 M9, same rule: no key at all, so `agent.lastActivityMs` is
        // `undefined` in the webview and the column renders nothing.
        assert!(value.get("lastActivityMs").is_none());
        // PRD #745 M11, same rule again: a daemon that did not spawn the agent
        // — or one predating the field — sends no key, and the uptime column
        // renders nothing rather than a fabricated age.
        assert!(value.get("spawnedAtMs").is_none());
    }

    /// Only `Writable` decides the lease, and its `#[serde(other)]` catch-all
    /// means an unknown future value arrives as `None` — so the non-writable
    /// answer is what a daemon this build does not understand produces.
    #[test]
    fn write_lease_projects_every_writable_value() {
        let lease = |writable| {
            let mut record = fixture_record();
            record.live.as_mut().unwrap().live_target = Some(LiveTarget {
                kind: TargetKind::Tmux,
                writable,
            });
            map_agent(record).write_lease
        };
        assert_eq!(lease(Writable::Live), Some("write"));
        assert_eq!(lease(Writable::HistoryOnly), Some("read"));
        assert_eq!(lease(Writable::None), Some("none"));
    }

    /// PRD #745 M8: the orchestration tab's own cwd, which the `..` rest
    /// pattern in `map_tab` used to swallow.
    #[test]
    fn orchestration_tab_carries_the_orchestration_cwd() {
        let tab = |orchestration_cwd| {
            serde_json::to_value(map_tab(Some(&TabMembership::Orchestration {
                name: "dot-agent-deck".into(),
                role_index: 1,
                role_name: "coder".into(),
                is_start_role: false,
                orchestration_cwd,
                display_title: Some("PRD #745".into()),
                orchestration_id: Some("orc-745".into()),
            })))
            .unwrap()
        };

        let reported = tab(Some("/home/dev/code/dot-agent-deck".into()));
        assert_eq!(reported["kind"], "orchestration");
        assert_eq!(reported["cwd"], "/home/dev/code/dot-agent-deck");
        assert_eq!(reported["roleName"], "coder");
        assert_eq!(reported["orchestrationId"], "orc-745");

        // Absent stays absent: an orchestration whose cwd the daemon did not
        // report has no key at all, so the group header states nothing rather
        // than a placeholder.
        assert!(tab(None).get("cwd").is_none());
    }

    #[test]
    fn asymmetric_dimensions_keep_rows_then_cols_and_clamp() {
        assert_eq!(validate_dimensions(50, 200).unwrap(), (50, 200));
        assert!(validate_dimensions(0, 200).is_err());
        assert_eq!(
            validate_dimensions(u16::MAX, u16::MAX).unwrap(),
            (PTY_RESIZE_DIM_MAX, PTY_RESIZE_DIM_MAX)
        );
    }

    #[test]
    fn terminal_input_is_bounded() {
        assert!(validate_terminal_input(&vec![0; TERMINAL_INPUT_MAX_BYTES]).is_ok());
        assert!(validate_terminal_input(&vec![0; TERMINAL_INPUT_MAX_BYTES + 1]).is_err());
    }

    #[test]
    fn command_validation_rejects_blank_nul_and_oversize_values() {
        assert!(validate_command(None).is_ok());
        assert!(validate_command(Some("codex --model gpt-5.6-sol")).is_ok());
        assert!(validate_command(Some("  ")).is_err());
        assert!(validate_command(Some("codex\0oops")).is_err());
        let oversized = "x".repeat(COMMAND_MAX_BYTES + 1);
        assert!(validate_command(Some(&oversized)).is_err());
    }

    #[test]
    fn profile_start_action_uses_camel_case_fields() {
        let action: DesktopAction = serde_json::from_value(serde_json::json!({
            "type": "start_agent",
            "deckId": "deck-000000000000dec1",
            "command": "codex --model gpt-5.6-sol",
            "cwd": "/tmp/project",
            "displayName": "builder",
            "rows": 30,
            "cols": 110
        }))
        .unwrap();
        assert!(matches!(
            action,
            DesktopAction::StartAgent {
                deck_id,
                display_name: Some(name),
                rows: Some(30),
                cols: Some(110),
                ..
            } if name == "builder" && deck_id == "deck-000000000000dec1"
        ));
    }

    /// Scenario: the webview sends a `start_agent` that names no deck. It must
    /// fail to decode — PRD #1223 M3 made `deckId` required so that a start can
    /// never fall back to the applied selection, which under All Decks is the
    /// local deck whatever the user was looking at (#1083).
    #[test]
    fn a_start_agent_action_without_a_deck_is_refused_at_decode() {
        let refused = serde_json::from_value::<DesktopAction>(serde_json::json!({
            "type": "start_agent",
            "command": "codex"
        }));
        let error = match refused {
            Ok(_) => panic!("a start with no deck must not decode"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("deckId"),
            "the refusal names the field: {error}"
        );
    }

    /// Scenario: the webview sends `start_agent` with an `authoringKind` (PRD
    /// #1223 M7). Each of the three kinds decodes to the crate's own enum, an
    /// absent one decodes as a plain start, and a kind this build does not know
    /// fails the decode rather than reaching a deck as a plain start with no
    /// seed.
    #[test]
    fn a_start_agent_action_carries_a_closed_authoring_kind() {
        let decode = |kind: Option<&str>| {
            let mut action = serde_json::json!({
                "type": "start_agent",
                "deckId": "deck-000000000000dec1",
                "command": "claude",
                "cwd": "/srv/repo",
            });
            if let Some(kind) = kind {
                action["authoringKind"] = kind.into();
            }
            serde_json::from_value::<DesktopAction>(action)
        };
        for kind in AuthoringKind::ALL {
            assert!(
                matches!(
                    decode(Some(kind.as_str())),
                    Ok(DesktopAction::StartAgent { authoring_kind: Some(decoded), .. }) if decoded == kind
                ),
                "{} decodes",
                kind.as_str()
            );
        }
        assert!(matches!(
            decode(None),
            Ok(DesktopAction::StartAgent {
                authoring_kind: None,
                ..
            })
        ));
        assert!(
            decode(Some("orchestration")).is_err(),
            "an unknown kind is refused at decode"
        );
    }

    /// Scenario: the webview sends `start_orchestration` (PRD #1223 M6). The
    /// deck, path and orchestration are required — a launch with no deck is
    /// refused at decode rather than defaulted to the selection — and the run
    /// title and config revision are optional and carried verbatim.
    #[test]
    fn a_start_orchestration_action_names_its_deck_and_carries_its_title() {
        let decoded = serde_json::from_value::<DesktopAction>(serde_json::json!({
            "type": "start_orchestration",
            "deckId": "deck-000000000000dec1",
            "path": "/srv/repo",
            "orchestration": "loop",
            "displayTitle": "repo-orchestrator-2",
            "configRevision": "fnv1a128-00",
        }));
        assert!(matches!(
            decoded,
            Ok(DesktopAction::StartOrchestration {
                ref deck_id,
                ref path,
                ref orchestration,
                display_title: Some(ref title),
                config_revision: Some(ref revision),
                rows: None,
                cols: None,
            }) if deck_id == "deck-000000000000dec1"
                && path == "/srv/repo"
                && orchestration == "loop"
                && title == "repo-orchestrator-2"
                && revision == "fnv1a128-00"
        ));
        assert!(matches!(
            serde_json::from_value::<DesktopAction>(serde_json::json!({
                "type": "start_orchestration",
                "deckId": "deck-000000000000dec1",
                "path": "/srv/repo",
                "orchestration": "loop",
            })),
            Ok(DesktopAction::StartOrchestration {
                display_title: None,
                config_revision: None,
                ..
            })
        ));
        assert!(
            serde_json::from_value::<DesktopAction>(serde_json::json!({
                "type": "start_orchestration",
                "path": "/srv/repo",
                "orchestration": "loop",
            }))
            .is_err(),
            "a launch with no deck is refused at decode"
        );
    }

    #[test]
    fn restart_daemon_action_has_no_force_field() {
        let action: DesktopAction = serde_json::from_value(serde_json::json!({
            "type": "restart_daemon"
        }))
        .unwrap();
        assert!(matches!(action, DesktopAction::RestartDaemon));
    }

    #[test]
    fn allow_build_mismatch_action_carries_no_payload() {
        let action: DesktopAction = serde_json::from_value(serde_json::json!({
            "type": "allow_build_mismatch"
        }))
        .unwrap();
        assert!(matches!(action, DesktopAction::AllowBuildMismatch));
    }

    #[test]
    fn disconnected_snapshot_is_fixture_safe_and_sanitized() {
        let snapshot = disconnected_snapshot(&Endpoint::local(), "offline\u{1b}[31m");
        assert_eq!(snapshot.connection.status, ConnectionStatus::Disconnected);
        assert_eq!(snapshot.connection.error.as_deref(), Some("offline[31m"));
        assert!(snapshot.agents.is_empty());
        let value = serde_json::to_value(snapshot).unwrap();
        assert_eq!(value["connection"]["status"], "disconnected");
        assert_eq!(value["source"], "daemon");
        // No daemon answered, so there is no stamp-only mismatch to override —
        // and the field is PRESENT rather than absent, because the webview
        // branches on it (issue #801).
        assert_eq!(value["connection"]["buildStampMismatchOnly"], false);
    }

    #[test]
    fn desktop_pane_ids_are_unique_and_pass_daemon_validation() {
        let first = mint_desktop_pane_id();
        let second = mint_desktop_pane_id();
        assert_ne!(first, second);
        assert!(is_valid_pane_id_env(&first));
        assert!(is_valid_pane_id_env(&second));
    }

    #[test]
    fn workflow_shape_requires_unique_roles_and_one_start() {
        let roles = vec![
            WorkflowRoleInput {
                role: "planner".into(),
                command: "codex --model gpt-5.6-sol".into(),
                start: true,
            },
            WorkflowRoleInput {
                role: "builder".into(),
                command: "codex --model gpt-5.6-sol".into(),
                start: false,
            },
        ];
        assert_eq!(
            validate_workflow_shape("loop", "/tmp/project", &roles, 50, 200).unwrap(),
            (50, 200)
        );
        let mut duplicate = roles.clone();
        duplicate[1].role = "planner".into();
        assert!(validate_workflow_shape("loop", "/tmp/project", &duplicate, 50, 200).is_err());
        let mut no_start = roles;
        no_start[0].start = false;
        assert!(validate_workflow_shape("loop", "/tmp/project", &no_start, 50, 200).is_err());
    }

    #[test]
    fn desktop_workflow_platform_guard_blocks_windows_only() {
        assert!(ensure_desktop_workflow_platform_supported("macos").is_ok());
        assert!(ensure_desktop_workflow_platform_supported("linux").is_ok());
        let error = ensure_desktop_workflow_platform_supported("windows").unwrap_err();
        assert!(error.contains("unavailable on Windows"));
        assert!(error.contains("POSIX-shell quoted"));
    }

    // -----------------------------------------------------------------------
    // PRD #819 audit fix (P2, finding 1): daemon identities cross this seam
    // unmodified, and the escaping lives in a sibling field.
    // -----------------------------------------------------------------------

    /// A path this seam is not allowed to rewrite, and a name it is.
    ///
    /// `\u{1}` is an ASCII control character `safe_message` strips. A canonical
    /// path can carry one — `canonicalize_project_dir` checks UTF-8 and
    /// directory-ness, not control-freeness — and `is_valid_cwd` refuses one,
    /// so this is the shape where scrubbing turned a path the daemon would
    /// refuse into a *different* path it accepts.
    const CONTROL: &str = "\u{1}";

    fn control_bearing_path() -> String {
        format!("/srv/pro{CONTROL}jects/api")
    }

    /// Scenario: the daemon lists a project whose canonical path carries a
    /// strippable control character. The DTO must carry that path byte for
    /// byte — it is the string that goes back on `resolve-project`,
    /// `prepare-workflow` and every `StartAgent.cwd` — while the display twin
    /// is scrubbed. `primary` is checked in the same test because the webview
    /// compares it against `path` to mark the active row: scrubbing one and not
    /// the other silently breaks that marker.
    #[test]
    fn a_control_bearing_canonical_path_reaches_the_daemon_unmodified() {
        let path = control_bearing_path();
        let mapped = map_project_listing(ProjectListing {
            projects: vec![dot_agent_deck::event::KnownProject {
                path: path.clone(),
                name: format!("a{CONTROL}pi"),
            }],
            primary: Some(path.clone()),
        });

        assert_eq!(mapped.projects[0].path, path, "the identity was rewritten");
        assert_eq!(
            mapped.primary.as_deref(),
            Some(path.as_str()),
            "primary must be the same spelling as the path it marks"
        );
        assert_eq!(mapped.projects[0].display_path, "/srv/projects/api");
        assert_eq!(mapped.projects[0].display_name, "api");
        assert!(
            !mapped.projects[0].display_path.contains(CONTROL)
                && !mapped.projects[0].display_name.contains(CONTROL),
            "the DISPLAY half must still be escaped"
        );
    }

    /// Scenario: the daemon resolves a project whose canonical path carries a
    /// control character and whose config revision is echoed back on the
    /// launch. Both are identities; the basename shown beside them is not.
    #[test]
    fn a_resolved_project_carries_its_path_and_revision_verbatim() {
        let path = control_bearing_path();
        let revision = format!("fnv{CONTROL}1a-deadbeef");
        let mapped = map_resolved_project(ResolvedProject {
            path: path.clone(),
            orchestrations: Vec::new(),
            config_revision: Some(revision.clone()),
        });

        assert_eq!(mapped.path, path);
        assert_eq!(
            mapped.config_revision.as_deref(),
            Some(revision.as_str()),
            "a re-spelled revision fails the daemon's comparison for an invisible reason"
        );
        assert_eq!(mapped.display_path, "/srv/projects/api");
        assert_eq!(
            mapped.display_name, "api",
            "the basename is split off the VERBATIM path and scrubbed afterwards"
        );
    }

    /// Scenario: the daemon resolves a project on **Windows**, where a canonical
    /// path is `\\?\C:\Users\dev\project` and contains no `/` at all. The picker
    /// must label it `project`, not with the whole path.
    ///
    /// PRD #819 Greptile P2(e). The basename used to be `path.rsplit('/')`, and
    /// project resolution is NOT refused on Windows — only `PrepareWorkflow` is,
    /// with `unsupported-platform`, because only its publish carries an
    /// owner-only guarantee it cannot deliver there. So a Windows client lists
    /// and resolves, every segment fell out of the `/` split, and the whole path
    /// was shown where a directory name belongs. `Path::file_name` is the
    /// platform's own answer, verbatim prefix included.
    ///
    /// `#[cfg(windows)]` because the claim is about the Windows target's path
    /// parser: asserting it from a Unix run would report a wider result than was
    /// measured. Its sibling below pins the Unix half of the same change.
    #[cfg(windows)]
    #[test]
    fn a_windows_canonical_path_is_labelled_with_its_directory_name() {
        for (path, expected) in [
            (r"\\?\C:\Users\dev\project", "project"),
            (r"C:\Users\dev\project", "project"),
            (r"C:\Users\dev\project\", "project"),
            // A mixed separator is still a separator to the Windows parser.
            (r"C:\Users\dev/project", "project"),
        ] {
            let mapped = map_resolved_project(ResolvedProject {
                path: path.to_string(),
                orchestrations: Vec::new(),
                config_revision: None,
            });
            assert_eq!(
                mapped.display_name, expected,
                "{path:?} names the directory {expected:?}"
            );
            assert_eq!(mapped.path, path, "the identity must be carried verbatim");
        }
    }

    /// Scenario: the daemon resolves a Unix project whose directory name
    /// contains a backslash — a perfectly ordinary Unix filename, since only `/`
    /// and NUL are excluded. The label must be that name, backslash included.
    ///
    /// This is the guard on P2(e)'s fix rather than the regression test for it:
    /// the tempting way to "support Windows" is to split on `/` and `\` on every
    /// platform, which would cut this name in half and label the project
    /// `ird`. `Path::file_name` is per-target, so it treats the backslash as a
    /// separator only where the platform does.
    #[cfg(unix)]
    #[test]
    fn a_unix_directory_name_containing_a_backslash_survives_whole() {
        let mapped = map_resolved_project(ResolvedProject {
            path: r"/srv/projects/we\ird".to_string(),
            orchestrations: Vec::new(),
            config_revision: None,
        });
        assert_eq!(mapped.display_name, r"we\ird");
        assert_eq!(mapped.path, r"/srv/projects/we\ird");
    }

    /// Scenario: the shapes the basename derivation has to survive on any
    /// platform — a trailing separator, and a root that names no directory at
    /// all. The root falls back to the whole path, which is what it did before
    /// and is the only honest label available for it.
    #[test]
    fn a_trailing_separator_and_a_rootless_path_still_produce_a_label() {
        for (path, expected) in [("/srv/projects/api/", "api"), ("/", "/")] {
            let mapped = map_resolved_project(ResolvedProject {
                path: path.to_string(),
                orchestrations: Vec::new(),
                config_revision: None,
            });
            assert_eq!(mapped.display_name, expected, "{path:?}");
            assert_eq!(mapped.path, path, "the identity must be carried verbatim");
        }
    }

    /// Scenario: a canonical path longer than `safe_message`'s 2048-character
    /// truncation but inside the wire bound the daemon documents
    /// (`agent_pty::CWD_MAX_LEN`, 4096 bytes). It used to be submitted
    /// truncated, i.e. as a different directory; it must now survive whole.
    #[test]
    fn an_oversized_canonical_path_is_carried_whole_rather_than_truncated() {
        let path = format!("/{}", "p".repeat(3000));
        assert!(
            is_valid_cwd(&path),
            "the fixture must be inside the daemon's own wire bound, or it proves nothing"
        );
        let mapped = map_resolved_project(ResolvedProject {
            path: path.clone(),
            orchestrations: Vec::new(),
            config_revision: None,
        });

        assert_eq!(mapped.path.len(), path.len());
        assert_eq!(mapped.path, path);
        assert_eq!(
            mapped.display_path.chars().count(),
            ERROR_MESSAGE_MAX_CHARS,
            "the display twin is still bounded"
        );
    }

    /// Scenario: an orchestration and one of its roles carry control
    /// characters. Both names are protocol identities — the orchestration's
    /// goes back as `PrepareWorkflow.orchestration`, the role's is matched by
    /// `order_workflow_roles` and becomes the pane's `display_name` — so both
    /// cross verbatim, with escaped twins for the picker.
    #[test]
    fn orchestration_and_role_names_reach_the_daemon_unmodified() {
        let orchestration_name = format!("lo{CONTROL}op");
        let role_name = format!("plan{CONTROL}ner");
        let mapped = map_resolved_project(ResolvedProject {
            path: "/srv/projects/api".into(),
            orchestrations: vec![ProjectOrchestration {
                name: orchestration_name.clone(),
                default: true,
                roles: vec![dot_agent_deck::event::ProjectRole {
                    name: role_name.clone(),
                    start: true,
                }],
            }],
            config_revision: None,
        });

        let orchestration = &mapped.orchestrations[0];
        assert_eq!(orchestration.name, orchestration_name);
        assert_eq!(orchestration.display_name, "loop");
        assert_eq!(orchestration.roles[0].name, role_name);
        assert_eq!(orchestration.roles[0].display_name, "planner");
        assert!(orchestration.roles[0].start);
    }

    /// Scenario (PRD #1223 audit F6): `desktop_run_action`'s rejection is the
    /// bare string every webview `catch` already reads — including a launch
    /// whose cleanup was confirmed — and only a launch that could not confirm
    /// every stop rejects with an object carrying the roles as data.
    #[test]
    fn a_launch_rejection_carries_unconfirmed_stops_as_data_and_every_other_is_a_string() {
        assert_eq!(
            serde_json::to_value(DesktopActionError::from("deck refused")).unwrap(),
            serde_json::json!("deck refused")
        );
        assert_eq!(
            serde_json::to_value(DesktopActionError::launch(
                "failed; stopped 2 already-started role(s)".into(),
                Vec::new()
            ))
            .unwrap(),
            serde_json::json!("failed; stopped 2 already-started role(s)"),
            "a confirmed rollback needs no warning, so it keeps the string shape"
        );
        assert_eq!(
            serde_json::to_value(DesktopActionError::launch(
                "failed; cleanup could not confirm stop".into(),
                vec!["reviewer".into(), "plan\u{1b}ner".into()]
            ))
            .unwrap(),
            serde_json::json!({
                "message": "failed; cleanup could not confirm stop",
                "unconfirmedStops": ["reviewer", "planner"],
            }),
            "each role name goes through safe_message"
        );
    }

    /// Scenario: the longest name the daemon will project is one this crate's
    /// own launch validation accepts. This is the consumer half of the limit
    /// reconciliation — the daemon's `MAX_PROJECTED_LAUNCH_NAME_BYTES` is now
    /// `agent_pty::DISPLAY_NAME_MAX_LEN`, so the offered set is the launchable
    /// set and `validate_workflow_shape` cannot refuse a name the picker
    /// offered on length alone.
    #[test]
    fn the_longest_projected_name_passes_desktop_launch_validation() {
        use dot_agent_deck::project_resolve::MAX_PROJECTED_LAUNCH_NAME_BYTES;

        let at_ceiling = "n".repeat(MAX_PROJECTED_LAUNCH_NAME_BYTES);
        let roles = vec![WorkflowRoleInput {
            role: at_ceiling.clone(),
            command: "claude".into(),
            start: true,
        }];
        assert!(
            validate_workflow_shape(&at_ceiling, "/tmp/project", &roles, 32, 120).is_ok(),
            "a workflow and role name at the daemon's projection ceiling must be launchable"
        );
    }

    /// A document naming `hosts` and observing all of them.
    fn observing_all(hosts: &[&str]) -> crate::settings::DesktopSettings {
        use crate::settings::{
            DesktopSettings, EndpointId, EndpointSettings, RemoteEndpointSettings, Selection,
        };
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let remote = hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let id = EndpointId::parse(&format!("deck00000000000{index}")).expect("a valid id");
                let mut row =
                    RemoteEndpointSettings::new(id, Hostname::parse(host).expect("a valid host"));
                row.socket = Some(RemoteSocketPath::parse("/run/deck.sock").expect("a path"));
                row
            })
            .collect();
        DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote,
                selection: Selection::All,
            }),
            ..DesktopSettings::default()
        }
    }

    /// **PRD #742 M8's F2.** Scenario: the applied selection is rewritten over
    /// and over between a one-deck document and a three-deck fleet, while three
    /// readers ask [`observed_fleet`] for the answer. Every answer must be one
    /// document's fleet or the other's — never a head from one save with a list
    /// from the next.
    ///
    /// The selection used to be two `RwLock`s written in sequence, and the
    /// comment on the second said they "can never describe different saves"
    /// because there is one writer. That is true of saves and false of reads:
    /// a reader scheduled between the two `write()` calls saw one new value and
    /// one old one. The direction that costs something is
    /// `endpoint_test::release_if_not_observed` reading a pre-save observed set
    /// while a just-added deck's watcher already holds a lease — the probe drops
    /// the map's handle, the watcher's lease keeps the child alive, and the next
    /// `establish()` opens a **second** `ssh` child.
    ///
    /// **What this proves:** that the two halves are one value under one lock,
    /// and that [`observed_fleet`] takes one read of it — a torn pair here would
    /// be `build-box` at the head of a three-deck list, or the local deck at the
    /// head of a one-deck list, and both are rejected below.
    ///
    /// **And it reproduces, which neither of the two people who found it
    /// believed it would.** Both rated the window unreachable-in-practice — two
    /// adjacent lock acquisitions with no `await` between them — and it was
    /// reported only because F1's remedy keys on the value. Run against a
    /// faithful reconstruction of the pre-M8 shape (two statics, two writes,
    /// `observed_fleet` taking two reads) this test failed **6 times out of 6**,
    /// each time on exactly the torn pair the finding describes: the one-deck
    /// document's selected deck at the head of the three-deck document's
    /// observed list, a four-element fleet naming a save that never existed. A
    /// user-driven save is far rarer than this writer loop, so the measurement
    /// bounds nothing about production frequency — what it settles is that the
    /// window is real rather than theoretical, and that this test would have
    /// caught it.
    ///
    /// **What it does NOT prove:** anything about
    /// `release_if_not_observed`, the reader whose torn view actually costs an
    /// `ssh` child. That one needs a live watcher holding a lease, which is an
    /// integration-level setup; this asserts the property they share.
    #[test]
    fn no_reader_assembles_a_fleet_from_two_different_saves() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = SELECTION_LOCK.blocking_lock();

        let one = selecting_a_remote_deck();
        let all = observing_all(&["build-box.example.com", "laptop.example.com"]);
        // The two legal answers, each read while nothing else is writing.
        apply_settings_selection(&one);
        let fleet_of_one = observed_fleet();
        apply_settings_selection(&all);
        let fleet_of_all = observed_fleet();
        assert_ne!(
            fleet_of_one, fleet_of_all,
            "the two documents must disagree, or this test asserts nothing"
        );
        assert_eq!(fleet_of_one.len(), 1);
        assert_eq!(fleet_of_all.len(), 3);

        static WRITING: AtomicBool = AtomicBool::new(true);
        WRITING.store(true, Ordering::SeqCst);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for turn in 0..20_000 {
                    apply_settings_selection(if turn % 2 == 0 { &one } else { &all });
                }
                WRITING.store(false, Ordering::SeqCst);
            });
            for _ in 0..3 {
                scope.spawn(|| {
                    while WRITING.load(Ordering::SeqCst) {
                        let fleet = observed_fleet();
                        assert!(
                            fleet == fleet_of_one || fleet == fleet_of_all,
                            "a fleet must describe ONE applied document: {fleet:?} is neither \
                             {fleet_of_one:?} nor {fleet_of_all:?}"
                        );
                    }
                });
            }
        });

        apply_settings_selection(&crate::settings::DesktopSettings::default());
    }

    // -----------------------------------------------------------------------
    // Issue #1198 — the app-level experimental surfaces
    // -----------------------------------------------------------------------

    /// Serialises the tests that write the process-global `Features`. Under
    /// nextest every test is its own process, so this only matters to a plain
    /// `cargo test`, where they share one — the same shape as the root crate's
    /// `tests/features.rs`. Nothing else in this crate's tests reads the flag.
    static FEATURES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Puts back whatever `Features` a test found, even when it panics.
    struct RestoreFeatures(dot_agent_deck::features::Features);

    impl Drop for RestoreFeatures {
        fn drop(&mut self) {
            dot_agent_deck::features::set_for_test(self.0);
        }
    }

    /// Every gated surface follows the one flag through its own wrapper: all
    /// hidden while it is off — the shipped default — and all shown once it is
    /// on, with no surface left behind in either direction.
    #[test]
    fn desktop_features_follow_the_experimental_flag() {
        use dot_agent_deck::features::{self, Features};
        let _lock = FEATURES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = RestoreFeatures(features::current());

        features::set_for_test(Features::test_with(false));
        assert_eq!(
            DesktopFeatures::current(),
            DesktopFeatures {
                show_deck: false,
                show_projects: false,
                show_prompts: false,
                show_workflows: false,
                show_agent_profiles: false,
            }
        );

        features::set_for_test(Features::test_with(true));
        assert_eq!(
            DesktopFeatures::current(),
            DesktopFeatures {
                show_deck: true,
                show_projects: true,
                show_prompts: true,
                show_workflows: true,
                show_agent_profiles: true,
            }
        );
    }

    /// The wire shape the webview's `DesktopFeaturesDto` reads: camelCase keys,
    /// one per gated surface, and nothing else.
    #[test]
    fn desktop_features_serialise_in_camel_case() {
        let value = serde_json::to_value(DesktopFeatures {
            show_deck: true,
            show_projects: false,
            show_prompts: true,
            show_workflows: false,
            show_agent_profiles: true,
        })
        .expect("serialises");
        assert_eq!(
            value,
            serde_json::json!({
                "showDeck": true,
                "showProjects": false,
                "showPrompts": true,
                "showWorkflows": false,
                "showAgentProfiles": true,
            })
        );
    }
}
