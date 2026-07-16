//! Reusable PTY-spawn primitive shared by the TUI and the daemon.
//!
//! Both the TUI process (`embedded_pane`) and the daemon (`daemon`) need to
//! spawn agent processes attached to a PTY and own the child + master handles
//! for the lifetime of the agent. This module extracts that core so it isn't
//! trapped inside the TUI path. The daemon piece is the foundation for Phase 1
//! (M1.2 streaming attach protocol) — see PRD #76 lines 140–146.

use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast};

use crate::event::{AgentType, OrchestrationSurface};
use crate::pane_input::{PaneInputError, SUBMIT_DELAY, encode_pane_payload, escape_bytes_for_log};

/// Trigger flag the deck client honors to mean "the daemon is already
/// running; attach over its stream socket instead of spawning one." The
/// read site (in `main.rs`) and the scrub site (in [`spawn`] below) share
/// this constant so two string literals can't drift apart.
pub const DOT_AGENT_DECK_VIA_DAEMON: &str = "DOT_AGENT_DECK_VIA_DAEMON";

/// PRD #93 M1.2 idle-shutdown override: when set to a non-negative integer,
/// the daemon exits N seconds after the last attached client disconnects
/// *and* no managed agents remain. `0` disables the timer (matching the
/// pre-PRD-93 "stay up forever" behavior). Defaults to
/// [`crate::daemon::DEFAULT_IDLE_SHUTDOWN_SECS`] when unset or unparseable.
pub const DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS: &str = "DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS";

/// Per-pane id the TUI injects into agent children so hooks running inside
/// the agent (or anything that shells out via `dot-agent-deck`) can route
/// events back to the originating pane. Defined here for the same
/// drift-safety reason as [`DOT_AGENT_DECK_VIA_DAEMON`], and so the daemon
/// scrub site below can reference it by name.
pub const DOT_AGENT_DECK_PANE_ID: &str = "DOT_AGENT_DECK_PANE_ID";

/// PRD #92 F9 followup-7: per-spawn daemon-side agent id the daemon
/// injects into every spawned agent's environment. The agent's hook
/// script reads this and attaches it to each emitted `AgentEvent` as
/// `agent_id`, letting the post-respawn dispatch task scope its
/// `SessionStart` wait to the NEW agent — a late `SessionStart` from
/// the OLD agent firing within the subscribe→kill window carries the
/// OLD id and is rejected.
///
/// Same drift-safety pattern as [`DOT_AGENT_DECK_PANE_ID`]: define
/// the constant once and let the spawn-side injector, the env-scrub
/// site in [`spawn`], and the hook-script reader in
/// [`crate::hook`] all reference the same symbol so two string
/// literals can't drift apart.
pub const DOT_AGENT_DECK_AGENT_ID: &str = "DOT_AGENT_DECK_AGENT_ID";

/// Test-only safety watchdog: when set truthy (`1`/`true`/`yes`/`on`), a
/// `daemon serve` captures its parent pid at startup and gracefully exits once
/// it is orphaned (parent becomes `init`/pid 1, or otherwise changes). OFF by
/// default — production daemons are intentionally detached/lazy-spawned and
/// would be orphaned from birth, so the watchdog only runs when a test sets
/// this. Stops idle-disabled test daemons from leaking to PID 1 when the test
/// process dies without running `Drop` (SIGKILL / panic-abort / nextest
/// timeout / Ctrl-C).
pub const DOT_AGENT_DECK_EXIT_WHEN_ORPHANED: &str = "DOT_AGENT_DECK_EXIT_WHEN_ORPHANED";

/// Test-only backstop: when set to a positive integer, a `daemon serve`
/// gracefully self-exits after that many seconds no matter what. Unset = no cap
/// (production unaffected). Belt-and-suspenders for anything that slips past the
/// orphan watchdog (e.g. a detached test daemon whose parent is already PID 1).
pub const DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS: &str = "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS";

/// PRD #201 native prompt delivery: how long the daemon waits for a Pi pane's
/// extension to pull a stashed seed via `get-seed` (→ `pi.sendUserMessage`)
/// before falling back to typing the seed into the PTY (the safety net that
/// keeps the pane working if the extension failed to load or pull). Overridable
/// via `DOT_AGENT_DECK_SEED_FALLBACK_SECS` (integer seconds); a real-pi e2e sets
/// it high so it can prove the NATIVE pull path ran rather than the fallback.
/// Default matches the legacy pi injection latency (`SESSION_START_WAIT_TIMEOUT`
/// always timed out at 10s for pi), plus a small margin for Node/Bun boot.
pub const DOT_AGENT_DECK_SEED_FALLBACK_SECS: &str = "DOT_AGENT_DECK_SEED_FALLBACK_SECS";

/// Resolve the native-seed PTY-injection fallback grace (see
/// [`DOT_AGENT_DECK_SEED_FALLBACK_SECS`]). Falls back to 15s when unset or
/// unparseable.
pub fn seed_fallback_grace() -> std::time::Duration {
    std::env::var(DOT_AGENT_DECK_SEED_FALLBACK_SECS)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(15))
}

/// PRD #201: arm the PTY-injection SAFETY NET for a pane the daemon just
/// stashed a native seed for. Spawns a background task that waits `grace` (see
/// [`seed_fallback_grace`]) then — only if the seed was NOT already consumed by
/// the native `get-seed` pull — types it into the PTY. The `take`/`set` on the
/// registry is atomic, so the extension's pull and this fallback can never both
/// deliver. A no-op delivery (native already won) is the common, expected case.
pub fn arm_seed_fallback(
    registry: Arc<AgentPtyRegistry>,
    pane_id: String,
    grace: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        match registry.take_pending_seed_fallback(&pane_id) {
            Some(seed) => {
                // Native pull did not happen within the grace window — deliver
                // via the legacy PTY injection so the pane still works.
                if let Err(e) = registry.write_to_pane_and_submit(&pane_id, &seed).await {
                    tracing::warn!(
                        pane_id = %pane_id,
                        error = %e,
                        "seed fallback: PTY injection failed"
                    );
                } else {
                    tracing::debug!(
                        pane_id = %pane_id,
                        "seed fallback: delivered seed via PTY injection (native pull did not occur)"
                    );
                }
            }
            None => {
                tracing::debug!(
                    pane_id = %pane_id,
                    "seed fallback: seed already delivered natively; no injection"
                );
            }
        }
    });
}

/// Hard upper bound on PTY rows/cols accepted by the daemon. Larger values
/// are clamped down before reaching `MasterPty::resize`. The cap defends
/// against a same-uid attach-socket peer perturbing an existing agent's
/// geometry to extreme values: applications inside the PTY may trust
/// `TIOCGWINSZ` and allocate or redraw based on the reported dimensions, so
/// `65535x65535` is a cheap local DoS vector. 4096 is far above any real
/// terminal size while still keeping downstream allocations bounded.
pub const PTY_RESIZE_DIM_MAX: u16 = 4096;

/// Maximum byte length the daemon will *retain* for a caller-supplied
/// `DOT_AGENT_DECK_PANE_ID` value (and the TUI will *reuse* on rehydration).
/// The agent's child process still receives whatever the caller sent — we
/// only scrub the daemon's stored copy that gets echoed in `agent_records`.
/// 64 bytes is well above the numeric ids the TUI itself emits while
/// keeping the cumulative `list_agents` response small enough that a buggy
/// peer can't push it past `MAX_FRAME_LEN` and lock the reconnecting TUI
/// out of hydration entirely. See [`is_valid_pane_id_env`].
pub const PANE_ID_ENV_MAX_LEN: usize = 64;

/// Returns `true` if `value` is a well-formed pane-id env value worth
/// retaining: non-empty, ≤ [`PANE_ID_ENV_MAX_LEN`] bytes, and made entirely
/// of `[a-zA-Z0-9_-]`. Rejects oversize, empty, ANSI/control-char, and
/// otherwise weird payloads from a buggy or hostile same-user peer that
/// reaches the attach socket. Used at two layers (daemon-side capture in
/// [`AgentPtyRegistry::spawn_agent`] and client-side hydration in
/// `embedded_pane::hydrate_from_daemon`) so a stale daemon predating the
/// daemon-side check still has the client-side filter as backstop.
pub fn is_valid_pane_id_env(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= PANE_ID_ENV_MAX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// PRD #42 M1: the shell-wrap policy (which commands need wrapping, and the
// `$SHELL`/`/bin/sh -c` vs `%COMSPEC%`/`cmd /C` shell selection) moved to
// `crate::platform::shell`. Re-exported here so existing
// `agent_pty::command_needs_shell_wrap` callers (e.g. `spawn.rs`) keep
// resolving without churn.
pub use crate::platform::shell::command_needs_shell_wrap;

/// Maximum byte length the daemon will accept for a per-agent display name
/// (M2.11). Anything longer is rejected and the agent's display_name is
/// recorded as `None`. 128 bytes is roughly four times the visible width
/// of a typical tab label; well past that and we're paying for storage we
/// can never render anyway.
pub const DISPLAY_NAME_MAX_LEN: usize = 128;

/// Maximum byte length the daemon will accept for a per-agent cwd (M2.11),
/// matching the conventional PATH_MAX on Linux/macOS. The daemon stores the
/// value verbatim — paths legitimately contain a wide range of bytes — but
/// caps the length so a buggy or hostile same-user peer can't push
/// `list_agents` past [`crate::daemon_protocol::MAX_FRAME_LEN`] with one
/// pathological cwd.
pub const CWD_MAX_LEN: usize = 4096;

/// Returns `true` if `value` is a well-formed display name: non-empty,
/// ≤ [`DISPLAY_NAME_MAX_LEN`] bytes, and free of ASCII control characters
/// (bytes < 0x20 plus 0x7F DEL). Unicode beyond 0x7F is allowed so the
/// user can type UTF-8 names. Rejects values containing ANSI escapes,
/// NUL, newlines, carriage returns, etc. — anything that could perturb
/// the TUI render path when echoed back via `list_agents`.
pub fn is_valid_display_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= DISPLAY_NAME_MAX_LEN
        && value.bytes().all(|b| b >= 0x20 && b != 0x7f)
}

/// Canonical resolver for the human-readable display name shown on a pane
/// and stored on the daemon-side `AgentRecord.display_name`. This is the
/// single source of truth shared by the UI's new-pane handler and the
/// controller's local/stream pane creation paths so all four sites apply
/// the same trim + validation + fallback rules (PRD #76 M2.11 fixup 4).
///
/// Resolution order:
/// 1. `str::trim()` the form-supplied `form_name`. If non-empty and
///    [`is_valid_display_name`] accepts the trimmed value, return it.
/// 2. Otherwise `str::trim()` the `command`. If non-empty and
///    [`is_valid_display_name`] accepts the trimmed value, return it.
/// 3. Otherwise return `"shell"` — the ultimate fallback, assumed valid.
///
/// A whitespace-only form_name falls through to command. A command with
/// ASCII control bytes (e.g. `"echo \x1b[31m"` with a real ESC) fails
/// validation and falls through to `"shell"`, matching the daemon-side
/// drop behavior so the in-session UI maps can't diverge from the daemon
/// record (M2.11 fixup-3 AUDITOR LOW).
pub fn resolve_display_name(form_name: Option<&str>, command: Option<&str>) -> String {
    if let Some(name) = form_name {
        let trimmed = name.trim();
        if !trimmed.is_empty() && is_valid_display_name(trimmed) {
            return trimmed.to_string();
        }
    }
    if let Some(cmd) = command {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() && is_valid_display_name(trimmed) {
            return trimmed.to_string();
        }
    }
    "shell".to_string()
}

/// Returns `true` if `value` is acceptable to retain as a cwd: non-empty,
/// ≤ [`CWD_MAX_LEN`] bytes, and free of ASCII control characters (bytes
/// < 0x20 plus 0x7F DEL). Mirrors the [`is_valid_display_name`] filter so
/// the dashboard, which renders `cwd`'s basename through `Span::raw`,
/// can't be tricked into emitting terminal control sequences via a
/// hostile `SetAgentLabel` like `/tmp/\x1b[31mpwn`. Unicode beyond 0x7F
/// stays valid (paths are UTF-8 and legitimately contain accented bytes).
pub fn is_valid_cwd(value: &str) -> bool {
    !value.is_empty() && value.len() <= CWD_MAX_LEN && value.bytes().all(|b| b >= 0x20 && b != 0x7f)
}

/// Which tab a daemon-tracked agent pane belonged to at spawn time
/// (PRD #76 M2.12). Echoed back via `list_agents` so the TUI can rebuild
/// the user's mode/orchestration tab structure on reconnect instead of
/// stranding every hydrated pane on the dashboard.
///
/// Validation: the embedded `name` follows the same `is_valid_display_name`
/// grammar as `display_name` — non-empty, ≤ 128 bytes, no control bytes.
/// Anything failing that is dropped to `None` on capture so a buggy or
/// hostile same-user peer reaching the attach socket can't smuggle ANSI
/// escapes back via `list_agents` (the auditor-flagged echo path).
///
/// Wire shape (serde):
/// ```json
/// { "kind": "mode", "name": "k8s-ops" }
/// { "kind": "orchestration", "name": "tdd-cycle", "role_index": 2 }
/// ```
///
/// `kind` tag is `snake_case` to match the other JSON enums in this crate.
/// `Option<TabMembership>` on `AgentRecord` / `StartAgent` is serialized with
/// `skip_serializing_if = "Option::is_none"` so older clients/daemons keep
/// working: a daemon predating this field sends nothing, and a TUI predating
/// this field ignores any extra key. `None` is the dashboard pane.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabMembership {
    /// Agent pane of a Mode tab. Side panes (the cards on the left) are
    /// NOT daemon-tracked — they respawn fresh from `ModeConfig.panes` on
    /// reconnect, see PRD #76 M2.12 design decision 2.
    Mode { name: String },
    /// One role slot of an orchestration tab. `role_index` is the position
    /// of this role in `OrchestrationConfig.roles`; on reconnect a dead
    /// slot (between role-index 0 and `roles.len()` with no surviving
    /// agent) is marked failed rather than respawned.
    ///
    /// PRD #93 round-5: the daemon now owns the orchestration dispatch flow
    /// (delegate / work-done) and writes the per-role prompt directly into
    /// the target pane's PTY. To do that without needing to load the
    /// orchestration config on the daemon side, `role_name` and
    /// `is_start_role` are carried inline alongside the index: `role_name`
    /// populates [`crate::state::AppState::pane_role_map`] and
    /// `is_start_role` populates
    /// [`crate::state::AppState::orchestrator_pane_ids`].
    Orchestration {
        name: String,
        role_index: usize,
        #[serde(default)]
        role_name: String,
        #[serde(default)]
        is_start_role: bool,
        /// Round-11 auditor #C: the absolute cwd of the orchestration
        /// tab, shared across every role pane in the same orchestration.
        /// Used as a disambiguator in `pane_orchestration_map` so two
        /// unnamed orchestrations whose cwd-basenames collide (e.g.
        /// `~/a/foo` and `~/b/foo`) get distinct identities. Distinct
        /// from each pane's own per-pane cwd: orchestrator and workers
        /// may have different per-pane cwds (PRD #93 round-9 #2) but
        /// they share one orchestration_cwd because they belong to the
        /// same tab. `Option<String>` with `#[serde(default)]` so an
        /// older client/daemon that omits the field still parses.
        /// `None` means "no disambiguator" — the lookup falls back to
        /// name-only, matching the pre-round-11 behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orchestration_cwd: Option<String>,
        /// PRD #107 follow-up: the user-typed orchestration name from the
        /// new-pane form. Carried through the daemon round-trip so a
        /// detach/reattach restores the displayed tab TITLE instead of
        /// recomputing it from `resolve_orchestration_name` (config name or
        /// cwd basename). The orchestration IDENTITY stays in `name` — this
        /// is title-only and never feeds delegate/role lookups. `None` (the
        /// common case for daemon-initiated/scheduled orchestrations and
        /// older clients) means the title falls back to the canonical name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_title: Option<String>,
    },
}

impl TabMembership {
    /// Borrow the tab name (mode or orchestration) so callers don't have
    /// to match on the variant for the common "extract name for validation
    /// or lookup" case.
    pub fn name(&self) -> &str {
        match self {
            TabMembership::Mode { name } => name,
            TabMembership::Orchestration { name, .. } => name,
        }
    }
}

/// PRD #111 auditor BLOCKER: hard ceiling on `TabMembership::Orchestration::role_index`
/// enforced at the wire boundary. The TUI hydration path
/// (`OrchestrationConfig::synthesize_from_bucket_metadata`) sizes a
/// `Vec<OrchestrationRoleConfig>` to `max(role_index) + 1`, so a hostile
/// or buggy daemon sending `role_index: u64::MAX` would push the TUI
/// into an OOM allocation. 256 is well above any realistic orchestration
/// role count (the largest configs we ship are single digits) and small
/// enough that the worst-case vector stays trivial.
pub const ORCHESTRATION_ROLE_INDEX_MAX: usize = 256;

/// Validate a [`TabMembership`] in the same way display_name is validated.
/// Returns the input on accept, `None` on reject. Mirrors the spawn-time
/// drop semantics for display_name/cwd: invalid → stored as `None`, so
/// `list_agents` can't echo control bytes from a hostile peer.
///
/// Exposed publicly so the client-side wire boundary
/// ([`crate::daemon_client::DaemonClient::list_agents`]) can apply the
/// same sanitization to incoming `AgentRecord.tab_membership` — defense
/// in depth against a malformed or older daemon (M2.12 fixup auditor
/// #1).
///
/// Round-12 auditor #2: the new `orchestration_cwd` field also goes
/// through validation. A same-user attach client (or a buggy TUI) can
/// otherwise smuggle oversized strings, NUL bytes, or escape sequences
/// in there, and the daemon echoes them back via `agent_records`
/// where downstream parsing/display can misbehave.
///
/// PRD #111 auditor BLOCKER + suggestion: also validate
/// `role_name` (echoed into tab labels — ANSI escapes here perturb the
/// TUI render path the same way they do for display_name) and cap
/// `role_index` at [`ORCHESTRATION_ROLE_INDEX_MAX`] (a hostile daemon
/// sending a huge index would otherwise OOM the TUI when
/// `synthesize_from_bucket_metadata` allocates a placeholder vec of
/// `max_index + 1` length). Both are wire-boundary checks so every
/// downstream consumer is protected without per-call-site validation.
pub fn validate_tab_membership(mut tm: TabMembership) -> Option<TabMembership> {
    if !is_valid_display_name(tm.name()) {
        return None;
    }
    if let TabMembership::Orchestration {
        role_index,
        role_name,
        orchestration_cwd,
        display_title,
        ..
    } = &mut tm
    {
        if *role_index > ORCHESTRATION_ROLE_INDEX_MAX {
            return None;
        }
        // role_name is `#[serde(default)]`, so an empty value from an
        // older daemon is legitimate (the synthesis path falls back to
        // a `role-{i}` placeholder). Only reject non-empty values that
        // would smuggle control bytes into the tab label.
        if !role_name.is_empty() && !is_valid_display_name(role_name) {
            return None;
        }
        if let Some(c) = orchestration_cwd.as_deref()
            && !is_valid_orchestration_cwd(c)
        {
            return None;
        }
        // display_title flows to the tab label exactly like name/role_name,
        // so it needs the same control-byte guard. But it's purely
        // cosmetic with a defined `None` fallback (the title reverts to the
        // canonical resolved name), so an invalid value is nulled out
        // rather than rejecting the whole membership — dropping the
        // orchestration tab over a bad cosmetic string would be a worse
        // outcome than losing the custom title (Greptile PR #160 P1).
        if display_title
            .as_deref()
            .is_some_and(|t| !is_valid_display_name(t))
        {
            *display_title = None;
        }
    }
    Some(tm)
}

/// Returns `true` if `value` is acceptable as an orchestration's
/// identity cwd: non-empty, ≤ [`CWD_MAX_LEN`] bytes, free of ASCII
/// control characters, AND an absolute path (starts with `/`).
///
/// Round-12 auditor #2: the orchestration_cwd field is treated as
/// the project root, so being absolute is part of the contract — a
/// relative or empty value would either fail the daemon's later
/// filesystem operations or quietly collide with sibling
/// orchestrations whose own resolved cwd happens to match. Reject up
/// front instead.
pub fn is_valid_orchestration_cwd(value: &str) -> bool {
    is_valid_cwd(value) && value.starts_with('/')
}

/// PRD #120 (H1/M1/L2): wire-boundary validation for the live
/// [`OrchestrationSurface`] broadcast, mirroring [`validate_tab_membership`]
/// for the reconnect path. The receive path
/// (`EventSubscription::next_event` → `AppState::queue_orchestration_surface`
/// → `resolve_orch_config_for_hydration` →
/// `OrchestrationConfig::synthesize_from_bucket_metadata`) would otherwise feed
/// an UNVALIDATED, daemon-supplied surface straight into synthesis, which sizes
/// a role vec to `max(role_index) + 1`. A hostile/buggy `role_index` (e.g.
/// `1e9`) OOMs the TUI and `usize::MAX` panics in debug — the exact OOM
/// [`ORCHESTRATION_ROLE_INDEX_MAX`] was added to defend on the reconnect path,
/// which the new path bypassed entirely.
///
/// Returns the sanitized surface on accept, `None` when it is structurally
/// untrustworthy (the caller drops it without ending the event stream). Checks,
/// applied BEFORE any allocation/use:
/// - **H1:** every role whose `role_index > ORCHESTRATION_ROLE_INDEX_MAX` (the
///   OOM cap) is dropped, so synthesis can never size a giant placeholder vec.
/// - **M1:** `name`, `role_name`, and `display_title` feed the tab label /
///   role cards exactly like the reconnect path's validated fields, so they get
///   the same control-byte/ANSI guard. `name` is the tab IDENTITY + bucket key
///   with no safe fallback → an invalid value rejects the whole surface. A role
///   whose non-empty `role_name` carries control bytes is dropped (its slot
///   falls back to a `role-{i}` placeholder). `display_title` is purely
///   cosmetic with a defined `None` fallback (→ `name`), so an invalid value is
///   nulled out rather than rejecting the surface (matches
///   [`validate_tab_membership`]).
/// - **L2:** `cwd` drives `load_project_config` and is the bucket key, so it
///   must be a valid ABSOLUTE orchestration cwd → reject otherwise.
///
/// A surface left with no roles after the per-role drops is rejected: an
/// orchestration always has ≥1 role, and a zero-role surface can only build a
/// dead/empty tab.
pub fn validate_orchestration_surface(
    mut surface: OrchestrationSurface,
) -> Option<OrchestrationSurface> {
    // `name` is the tab identity + hydration bucket key — there is no safe
    // fallback for a corrupt identity, so reject the whole surface.
    if !is_valid_display_name(&surface.name) {
        return None;
    }
    // `cwd` drives `load_project_config` and keys the bucket; require a valid
    // absolute path free of control bytes.
    if !is_valid_orchestration_cwd(&surface.cwd) {
        return None;
    }
    // Cosmetic title with a defined `None` fallback: null it out on a bad value
    // rather than dropping the orchestration tab over a bad label string.
    if surface
        .display_title
        .as_deref()
        .is_some_and(|t| !is_valid_display_name(t))
    {
        surface.display_title = None;
    }
    // Drop any role that would OOM the synthesis allocation (role_index over the
    // cap) or smuggle control bytes into the tab via a non-empty role_name. An
    // empty role_name is legitimate — synthesis falls back to a `role-{i}`
    // placeholder.
    surface.roles.retain(|r| {
        r.role_index <= ORCHESTRATION_ROLE_INDEX_MAX
            && (r.role_name.is_empty() || is_valid_display_name(&r.role_name))
    });
    if surface.roles.is_empty() {
        return None;
    }
    Some(surface)
}

#[derive(Debug, Error)]
pub enum AgentPtyError {
    #[error("Failed to open PTY: {0}")]
    Open(String),
    #[error("Failed to spawn command: {0}")]
    Spawn(String),
    #[error("Failed to acquire PTY writer: {0}")]
    Writer(String),
    #[error("Failed to clone PTY reader: {0}")]
    Reader(String),
    #[error("Failed to resize PTY: {0}")]
    Resize(String),
    #[error("Agent {0} not found")]
    NotFound(String),
    /// Caller-supplied spawn metadata failed validation. Surfaced to the
    /// attach client via `AttachResponse::err` so a malformed spawn fails
    /// loudly instead of silently dropping the bad field (PRD #76 M2.12
    /// review fixup — reject invalid `tab_membership.name` rather than
    /// reclassify the pane as dashboard).
    #[error("Invalid spawn options: {0}")]
    Validation(String),
    /// The text handed to one of the `write_to_pane_*` entrypoints could not
    /// be encoded into a safe pane payload (PRD #93 round-8). Today this
    /// fires when a multi-line input contains an embedded bracketed-paste
    /// marker (`ESC[200~` / `ESC[201~`) that would terminate the outer
    /// wrapper and leak the tail as raw keystrokes inside the agent TUI.
    #[error("Invalid pane payload: {0}")]
    InvalidPayload(#[from] PaneInputError),
    /// A spawn carried a `DOT_AGENT_DECK_PANE_ID` env value that already
    /// names another live agent in this registry. The `write_to_pane_*`
    /// entrypoints key off `pane_id_env`, so accepting a second agent with the same id
    /// would silently route delegate/work-done writes to whichever entry
    /// `HashMap::values().find(...)` returns first — i.e., the wrong PTY.
    /// Reject the spawn loudly instead.
    #[error("Duplicate pane id: {0}")]
    DuplicatePaneId(String),
}

/// How to spawn an agent.
pub struct SpawnOptions<'a> {
    /// Command to run. `None` falls back to `$SHELL`. Strings containing spaces
    /// are routed through `$SHELL -c <cmd>` to mirror the TUI's existing
    /// behavior.
    pub command: Option<&'a str>,
    /// Working directory for the spawned process.
    pub cwd: Option<&'a str>,
    /// Optional human-readable label for the agent (M2.11). Captured into
    /// `RunningAgent::display_name` and echoed back to clients via
    /// `list_agents` so renamed panes survive a reconnect. The PTY child
    /// itself does not see this value; it lives only in the registry.
    pub display_name: Option<&'a str>,
    /// Initial PTY size.
    pub rows: u16,
    pub cols: u16,
    /// Extra environment variables to inject (e.g. `DOT_AGENT_DECK_PANE_ID`).
    pub env: Vec<(String, String)>,
    /// Which tab this agent pane belongs to (PRD #76 M2.12). `None` means
    /// "dashboard pane". Captured into `RunningAgent::tab_membership` and
    /// echoed back via `list_agents` so the TUI can rebuild mode and
    /// orchestration tabs on reconnect. Invalid values (name fails
    /// `is_valid_display_name`) cause the spawn to fail with
    /// [`AgentPtyError::Validation`] — silent drop would hide bad spawn
    /// metadata behind a "looks dashboard" pane on reconnect (M2.12 fixup
    /// reviewer #2).
    pub tab_membership: Option<TabMembership>,
    /// Which AI agent the spawn command runs (PRD #76 M2.13). Captured
    /// into `RunningAgent::agent_type` and echoed back via `list_agents`
    /// so a remote reconnect can build placeholder sessions with the
    /// correct type instead of "No agent". `None` means "unknown / not an
    /// agent" — same wire shape as older daemons that predate this field
    /// (`skip_serializing_if` on the `AgentRecord` mirror keeps it
    /// backwards-compatible). The TUI computes the value at the spawn site
    /// via [`AgentType::from_command`].
    pub agent_type: Option<AgentType>,
}

impl Default for SpawnOptions<'_> {
    fn default() -> Self {
        Self {
            command: None,
            cwd: None,
            display_name: None,
            rows: 24,
            cols: 80,
            env: Vec::new(),
            tab_membership: None,
            agent_type: None,
        }
    }
}

/// A spawned agent and the handles needed to keep it alive, write to it, read
/// from it, and resize it. Callers are responsible for explicit cleanup when
/// shutting an agent down — there's no `Drop` impl, since some callers
/// (e.g. `embedded_pane`) destructure these fields and store them
/// individually. The registry uses [`force_kill_and_wait`] (SIGKILL) when it
/// owns whole `AgentPty` values, and [`PtyGuard`] to keep the spawn path
/// leak-free between `spawn()` and registry insertion.
pub struct AgentPty {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub reader: Box<dyn std::io::Read + Send>,
}

/// PRD #92 F8: hardcoded grace window between SIGTERM and the SIGKILL
/// fallback used by the single-pane Ctrl+W path
/// ([`crate::platform::proc::terminate_child_with_grace_and_wait`]) and as the
/// poll budget in the daemon-wide [`AgentPtyRegistry::shutdown_all_graceful`].
/// 3 s matches the F1 graceful-shutdown grace, which is the natural
/// sibling. Hardcoded as a constant for now (one symbol to find) rather
/// than lifted to `DashboardConfig` until a real user need surfaces.
pub(crate) const AGENT_TERMINATE_GRACE: Duration = Duration::from_secs(3);

// PRD #42 M1: the process-group teardown helpers (`pid_to_pgid`,
// `signal_child_pgroup_or_fallback`, `force_kill_child_and_wait`,
// `terminate_child_with_grace_and_wait`) moved to `crate::platform::proc`,
// where the Unix `killpg`/SIGTERM→SIGKILL logic lives behind the platform seam
// and a Windows Job-Object backend lands in PRD #163. Call sites below use
// `crate::platform::proc::*`.

fn force_kill_and_wait(pty: &mut AgentPty) {
    crate::platform::proc::force_kill_child_and_wait(&mut pty.child);
}

/// RAII guard that owns a freshly-spawned child between the `spawn_command`
/// call and the point at which ownership is handed off to an [`AgentPty`].
/// If the guard is dropped while still holding the child (e.g. because a
/// later step in [`spawn`] like `take_writer` or `try_clone_reader` returned
/// an error, or a panic unwound through the spawn path), the child is
/// force-killed and reaped so no orphan process is left behind.
struct ChildGuard {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl ChildGuard {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self { child: Some(child) }
    }

    fn take(mut self) -> Box<dyn portable_pty::Child + Send + Sync> {
        self.child.take().expect("ChildGuard already taken")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            crate::platform::proc::force_kill_child_and_wait(&mut child);
        }
    }
}

/// RAII guard that owns a fully-built `AgentPty` until ownership is handed
/// off via [`PtyGuard::take`]. Used by the registry to cover the gap between
/// [`spawn`] returning an `AgentPty` and the registry's internal `insert`,
/// where a panic (e.g. from lock poisoning) would otherwise drop the
/// `AgentPty` on the floor without killing the child (`AgentPty` has no
/// `Drop` of its own — see the type docs).
struct PtyGuard {
    pty: Option<AgentPty>,
}

impl PtyGuard {
    fn new(pty: AgentPty) -> Self {
        Self { pty: Some(pty) }
    }

    fn take(mut self) -> AgentPty {
        self.pty.take().expect("PtyGuard already taken")
    }
}

impl Drop for PtyGuard {
    fn drop(&mut self) {
        if let Some(mut pty) = self.pty.take() {
            force_kill_and_wait(&mut pty);
        }
    }
}

/// Spawn a new PTY-attached child process.
pub fn spawn(opts: SpawnOptions<'_>) -> Result<AgentPty, AgentPtyError> {
    // Mirror the `resize` bounds at spawn time: reject 0 rows/cols and clamp
    // oversized values down to [`PTY_RESIZE_DIM_MAX`]. Without this, a same-uid
    // attach-socket peer issuing `StartAgent { rows: 0, cols: 0 }` (or
    // `u16::MAX × u16::MAX`) skips the post-spawn `resize` gate entirely and
    // hands `openpty` either a deadlock-prone 0×0 PTY or a giant geometry that
    // apps inside the PTY trust via TIOCGWINSZ.
    if opts.rows == 0 || opts.cols == 0 {
        return Err(AgentPtyError::Validation(format!(
            "rows and cols must be > 0 (got {}x{})",
            opts.rows, opts.cols
        )));
    }
    let rows = opts.rows.min(PTY_RESIZE_DIM_MAX);
    let cols = opts.cols.min(PTY_RESIZE_DIM_MAX);

    let pty_system = NativePtySystem::default();

    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| AgentPtyError::Open(e.to_string()))?;

    // Shell used for the `-c` wrap of a multi-word command and for the
    // no-command fallback. A caller may pin it by injecting `SHELL` into
    // `opts.env` (PRD #127 M2.1: the scheduler's spawn primitive runs an
    // explicit multi-word `command` under a deterministic `/bin/sh -c` while
    // reserving the daemon's own `$SHELL` for the omitted-command fallback).
    // Falls back to the process `$SHELL`, then `/bin/sh`. The dialog never sets
    // `SHELL` in `opts.env`, so its behavior is unchanged.
    //
    // PRD #127 C2: this injected `SHELL` is a *wrapper-choice override only* —
    // it is consumed here and deliberately NOT exported into the spawned
    // child's environment (see the env-application loop below), so the agent's
    // own sub-shell matches an interactive session.
    let shell_override: Option<String> = opts
        .env
        .iter()
        .find(|(k, _)| k == "SHELL")
        .map(|(_, v)| v.clone());
    let default_shell = crate::platform::shell::default_shell(shell_override.as_deref());

    // PRD #20 blocker-3: apply the Wrapper integration strategy at the COMMON
    // spawn boundary. Every launch path that reaches a real child — fresh/plain
    // new-pane, plain/mode RESTORE, orchestration role, scheduler single/role,
    // issue-dispatch single/role, and respawn — funnels through here, so a
    // Wrapper-strategy agent (Codex) is wrapped into
    // `dot-agent-deck wrap --agent <name> -- <command>` exactly once regardless
    // of which path created it. Prefer the caller's resolved identity (finding
    // #19), falling back to parsing the command. `wrap_launch_command` is
    // idempotent (never double-wraps an already-`wrap` command) and a no-op for
    // non-Wrapper agents, so native agents and pre-wrapped commands are
    // untouched. The BARE command remains the persisted/user-facing metadata
    // upstream (Command field, last_command, SavedPane.command) — only the
    // actual exec here is transformed. Mode panes type their command into a
    // shell rather than passing it here; those seams wrap at the type site.
    let resolved_agent = opts
        .agent_type
        .clone()
        .or_else(|| AgentType::from_command(opts.command))
        .unwrap_or(AgentType::None);
    let launch_command: Option<String> = opts
        .command
        .map(|c| crate::wrap::wrap_launch_command(c, &resolved_agent));

    let mut cmd = match launch_command.as_deref() {
        Some(c) if command_needs_shell_wrap(c) => {
            let mut cb = CommandBuilder::new(&default_shell);
            cb.arg(crate::platform::shell::shell_command_flag());
            cb.arg(c);
            cb
        }
        Some(c) => CommandBuilder::new(c),
        None => CommandBuilder::new(&default_shell),
    };

    if let Some(dir) = opts.cwd {
        cmd.cwd(dir);
    }

    // Scrub deck-internal env vars from the inherited base *before* applying
    // `opts.env`, so an explicit caller-supplied value (e.g. embedded_pane
    // injecting the pane's own `DOT_AGENT_DECK_PANE_ID`) wins over a stale
    // inherited one. Inheritance is the default for `CommandBuilder`, so
    // without these explicit unsets the daemon's own environment leaks into
    // every agent it spawns:
    //   - `DOT_AGENT_DECK_VIA_DAEMON`: a developer who launched the daemon
    //     with this set would have every agent shell-out to `dot-agent-deck`
    //     itself try to act as a stream client.
    //   - `DOT_AGENT_DECK_PANE_ID`: the daemon may have been launched as a
    //     child of an existing deck pane, in which case its inherited
    //     pane-id would tag every spawned agent with the wrong pane.
    cmd.env_remove(DOT_AGENT_DECK_VIA_DAEMON);
    cmd.env_remove(DOT_AGENT_DECK_PANE_ID);
    // PRD #92 F9 followup-7: same scrub-then-overlay rule for the
    // daemon-injected agent_id. If the daemon itself was launched
    // from inside another deck pane that already had this set, an
    // unfiltered inherit would tag every spawned agent with the
    // parent deck's id and the hook script would misroute events.
    cmd.env_remove(DOT_AGENT_DECK_AGENT_ID);
    // PRD #93 tuning env var: same scrub rationale — a deck launched
    // with this set would otherwise leak it into every child it spawns,
    // where it's meaningless to the child's environment.
    cmd.env_remove(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS);

    for (k, v) in &opts.env {
        // PRD #127 C2: `SHELL` in `opts.env` is a wrapper-choice override only
        // (consumed as `shell_override` above) — do NOT export it into the
        // child, or the spawned agent's sub-shell would silently differ from an
        // interactive session.
        if k == "SHELL" {
            continue;
        }
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| AgentPtyError::Spawn(e.to_string()))?;

    // Wrap the freshly-spawned child in an RAII guard *before* any fallible
    // step below: a failure in `take_writer` / `try_clone_reader` (or a
    // panic between them) would otherwise orphan the child. The guard is
    // taken on the success path and its child moved into the AgentPty.
    let child_guard = ChildGuard::new(child);

    // Drop the slave — we interact through the master side only.
    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| AgentPtyError::Writer(e.to_string()))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| AgentPtyError::Reader(e.to_string()))?;

    Ok(AgentPty {
        child: child_guard.take(),
        master: pair.master,
        writer,
        reader,
    })
}

/// Cap on the per-agent scrollback buffer (bytes). Keeps reattach affordable
/// without unbounded memory growth — when a fresh client subscribes, the
/// daemon emits this many recent bytes as the initial render before live
/// output resumes. 1 MiB comfortably covers a typical TUI screen plus a few
/// scrollback pages; the policy is "ring buffer, evict oldest on overflow".
const SCROLLBACK_CAP_BYTES: usize = 1024 * 1024;

/// Capacity of the per-agent broadcast channel for live PTY output. Lossy
/// by design (tokio broadcast semantics) — a slow subscriber that lags past
/// this many messages observes `RecvError::Lagged` and is disconnected by
/// the protocol layer (the client can reattach and replay the snapshot).
const BROADCAST_CAPACITY: usize = 4096;

/// PRD #20 R20-004 (finding #3): cap on the atomic-send idempotency ledger
/// ([`AgentPtyRegistry::delivery_ledger`]). Far above any plausible number of
/// distinct in-flight deliveries. On overflow the ledger evicts the OLDEST
/// entries one at a time (LRU) rather than clearing wholesale — the old
/// wholesale clear could wipe a delivery id that was STILL retrying, re-enabling
/// a duplicate submit; LRU eviction only ever drops the least-recently-touched
/// ids, which are the ones a real (seconds-long) retry window has long since
/// abandoned.
const MAX_DELIVERY_RESULTS: usize = 8192;

/// Per-agent broadcast bus. Producers (the reader thread) atomically append
/// to scrollback and publish to subscribers under the same lock so a fresh
/// subscriber's `(snapshot, receiver)` is always consistent: the snapshot
/// covers everything written before the subscriber attached, and the
/// receiver delivers everything written after — no duplicates, no gaps.
pub struct AgentBus {
    tx: broadcast::Sender<Arc<Vec<u8>>>,
    state: Mutex<AgentBusState>,
}

struct AgentBusState {
    scrollback: VecDeque<u8>,
}

impl Default for AgentBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBus {
    pub fn new() -> Self {
        let (tx, _rx0) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            tx,
            state: Mutex::new(AgentBusState {
                scrollback: VecDeque::new(),
            }),
        }
    }

    /// Append bytes to scrollback and publish to subscribers. Held under the
    /// same lock that subscribers use to take their initial snapshot, so a
    /// concurrent `subscribe` can never split a write between snapshot and
    /// live receiver.
    fn push(&self, data: Vec<u8>) {
        let arc = Arc::new(data);
        let mut state = self.state.lock().unwrap();
        for &b in arc.iter() {
            state.scrollback.push_back(b);
        }
        while state.scrollback.len() > SCROLLBACK_CAP_BYTES {
            state.scrollback.pop_front();
        }
        // Lossy on purpose: we don't block the reader thread on slow
        // subscribers. `send` returns Err only when there are zero
        // receivers, which is fine — scrollback still has the bytes.
        let _ = self.tx.send(arc);
    }

    /// Atomically take the current scrollback snapshot and a receiver
    /// positioned just past it. See type-level docs for the consistency
    /// guarantee.
    pub fn subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Arc<Vec<u8>>>) {
        let state = self.state.lock().unwrap();
        let snapshot: Vec<u8> = state.scrollback.iter().copied().collect();
        let rx = self.tx.subscribe();
        drop(state);
        (snapshot, rx)
    }

    /// Take just the scrollback snapshot, no subscription.
    pub fn snapshot(&self) -> Vec<u8> {
        self.state
            .lock()
            .unwrap()
            .scrollback
            .iter()
            .copied()
            .collect()
    }

    /// Drop the scrollback ring on the floor, leaving live subscribers
    /// untouched (PRD #104 M3). Called from
    /// [`AgentPtyRegistry::resize`] after the master ioctl succeeds so
    /// the next attach-replay snapshot only covers bytes written at
    /// the new (rows, cols) — without this, a single snapshot could
    /// span multiple dimension epochs and the early bytes would be
    /// parsed at the wrong width.
    ///
    /// Takes the same `state` mutex `push`/`subscribe`/`snapshot` use,
    /// so a concurrent `subscribe` either sees the full pre-resize
    /// snapshot (and the live receiver picks up post-resize bytes) or
    /// sees an empty snapshot and the receiver picks up everything —
    /// no torn read.
    fn clear_scrollback(&self) {
        let mut state = self.state.lock().unwrap();
        state.scrollback.clear();
    }

    /// Current number of live broadcast subscribers. Lets diagnostics and
    /// tests observe when an attach handler has dropped its receiver — e.g.
    /// after a wedged client triggered the bounded-write timeout — without
    /// having to read from that client's socket.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// Reader-thread loop: pull bytes from the PTY master and publish them to
/// the bus. Exits cleanly when the PTY returns EOF (the child was killed or
/// otherwise terminated). The thread is detached — `RunningAgent` does not
/// hold a `JoinHandle` for it because shutdown is driven entirely by closing
/// the PTY (see `AgentPtyRegistry::close_agent`).
///
/// On loop exit (EOF or read error — both mean the child is gone) the
/// per-agent `exited` flag is set and `change_notify` is signaled. The
/// daemon's idle monitor reads `exited` via [`AgentPtyRegistry::live_count`]
/// so an agent that died but whose registry entry hasn't been closed yet
/// stops pinning the daemon up past its idle window (PRD #93 round-2
/// reviewer REV-3 — `len()` on its own counted exited entries and broke
/// idle shutdown).
fn pump_reader(
    mut reader: Box<dyn std::io::Read + Send>,
    bus: Arc<AgentBus>,
    exited: Arc<AtomicBool>,
    change_notify: Arc<Notify>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bus.push(buf[..n].to_vec()),
            Err(_) => break,
        }
    }
    exited.store(true, Ordering::SeqCst);
    change_notify.notify_one();
}

/// Snapshot of the writer + bus needed to attach a streaming client.
/// Returned by [`AgentPtyRegistry::subscribe`].
///
/// PRD #20 R20-008: the handle now CAPTURES the target's immutable identity
/// (`agent_id`, `pane_id_env`) and its liveness token (`exited`) ATOMICALLY with
/// the writer, under the single registry lock. Before this, `handle_attach_stream`
/// looked the pane up separately AFTER the lock was released; if the entry was
/// removed in between, the handler kept the cached writer but resolved the pane
/// to the `<agent-gone>` sentinel — and `pane_writable("<agent-gone>")` defaults
/// to `Live`, so a teardown-time frame could still be written to the dead
/// writer. Carrying the identity on the handle removes that racy second lookup
/// and lets the input path reject writes to an exited target.
pub struct AttachHandle {
    pub snapshot: Vec<u8>,
    pub rx: broadcast::Receiver<Arc<Vec<u8>>>,
    pub writer: Arc<AsyncMutex<Box<dyn std::io::Write + Send>>>,
    /// The registry id of the agent this handle attached to, captured under the
    /// same lock as `writer`.
    pub agent_id: String,
    /// The agent's spawn-time `DOT_AGENT_DECK_PANE_ID`, captured atomically with
    /// `writer`. `None` for a daemon-side agent that carried no pane id. The
    /// attach handler uses this instead of a post-lock lookup that could return
    /// the `<agent-gone>` sentinel.
    pub pane_id_env: Option<String>,
    /// Liveness token shared with the agent's reader thread: set `true` once the
    /// PTY returns EOF (the child died / was killed). The input path re-checks
    /// this before every write so bytes never reach a dead writer.
    pub exited: Arc<AtomicBool>,
}

/// PRD #20 R20-003/R20-006: the outcome of an identity-guarded atomic
/// write-and-submit ([`AgentPtyRegistry::write_and_submit_guarded`]). The
/// daemon-protocol handler maps these onto the wire [`crate::event::SendResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedSend {
    /// Bytes were written and submitted to the exact authorized live target.
    Applied,
    /// The live target that currently owns the pane is NOT the one the caller
    /// expected (a respawn/rebind between enqueue and delivery). No bytes written.
    WrongSession,
    /// The target changed liveness/session (or the writer's target rebound)
    /// WHILE the caller waited for the writer lock. No bytes written.
    Stale,
    /// No live registry entry owns the pane. No bytes written.
    NoLiveTarget,
    /// PRD #20 R20-004 (finding #3): the write to the authorized target STARTED
    /// but the full payload+submit sequence did not complete (a partial write
    /// then a writer error). Some bytes may already have reached the PTY, so the
    /// delivery is AMBIGUOUS — it must be recorded (not blindly retried into a
    /// duplicate). Maps to [`crate::event::SendResult::Ambiguous`].
    Ambiguous,
}

/// PRD #20 R20-003/R20-006: the live target that currently owns a pane, resolved
/// atomically for the identity-guarded send path. Bundles the shared writer with
/// the identity/liveness needed to re-validate after the writer lock is acquired.
struct PaneWriterTarget {
    writer: Arc<AsyncMutex<Box<dyn std::io::Write + Send>>>,
    agent_id: String,
    exited: Arc<AtomicBool>,
}

/// PRD #20 R20-004 (finding #3): one ledger record per seen `delivery_id`.
struct DeliveryRecord {
    /// Fingerprint of (target agent identity + pane + text). A reuse of the id
    /// with a DIFFERENT fingerprint is a conflict, never a replay.
    fingerprint: u64,
    /// Single-flight lock: the FIRST attempt for this id holds it while it
    /// computes the outcome; a concurrent duplicate awaits it, then re-reads
    /// `result` — so two in-flight duplicates never both submit.
    lock: Arc<AsyncMutex<()>>,
    /// The cached outcome once a DELIVERED (`applied`/`queued`) or `ambiguous`
    /// attempt completes. Stays `None` while the first attempt is in flight AND
    /// after a NON-delivered outcome (history-only / stale / wrong-session /
    /// no-live-target), so a later retry re-attempts — a role that becomes live
    /// still gets its prompt, while a real delivery is never repeated.
    result: Option<crate::event::SendResult>,
}

/// PRD #20 R20-004 (finding #3): atomic, fingerprint-bound idempotency ledger.
/// See [`AgentPtyRegistry::delivery_ledger`].
#[derive(Default)]
struct DeliveryLedger {
    records: HashMap<String, DeliveryRecord>,
    /// LRU order — front = least-recently-used, back = most-recent. A touched or
    /// inserted id moves to the back; eviction drops from the front.
    order: VecDeque<String>,
}

impl DeliveryLedger {
    /// Move `id` to the most-recently-used position.
    fn touch(&mut self, id: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == id) {
            self.order.remove(pos);
        }
        self.order.push_back(id.to_string());
    }

    /// Drop the record entirely (a non-delivered outcome stays retryable).
    fn forget(&mut self, id: &str) {
        self.records.remove(id);
        if let Some(pos) = self.order.iter().position(|k| k == id) {
            self.order.remove(pos);
        }
    }

    /// Evict least-recently-used records until at most [`MAX_DELIVERY_RESULTS`]
    /// remain. Unlike the old wholesale clear, this never drops a
    /// recently-touched (still-retrying) id.
    fn evict_to_cap(&mut self) {
        while self.records.len() > MAX_DELIVERY_RESULTS {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.records.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// PRD #20 R20-004 (finding #3): the outcome of admitting a `delivery_id` into
/// the ledger before a guarded send runs.
pub enum DeliveryAdmission {
    /// This id already completed with a MATCHING fingerprint — replay verbatim,
    /// do NOT write again.
    Replay(crate::event::SendResult),
    /// This id was reused with a DIFFERENT fingerprint (payload/target changed) —
    /// refuse; never replay a false success onto conflicting content.
    Conflict,
    /// First attempt (or a retry of a still-retryable non-delivered outcome):
    /// the caller should compute and then record via
    /// [`AgentPtyRegistry::record_delivery_outcome`]. The permit holds the
    /// single-flight guard so concurrent duplicates wait behind it.
    Proceed(DeliveryPermit),
}

/// PRD #20 R20-004 (finding #3): RAII-ish permit returned by
/// [`AgentPtyRegistry::admit_delivery`]. Holds the single-flight guard for the
/// admitted `delivery_id` until dropped; carry it to
/// [`AgentPtyRegistry::record_delivery_outcome`] to publish the result.
pub struct DeliveryPermit {
    delivery_id: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

/// PRD #20 R20-004 (finding #3): the outcome of physically writing a payload +
/// submit CR to a PTY writer, classifying WHERE a write error struck.
#[derive(Debug, PartialEq, Eq)]
enum PayloadDelivery {
    /// Payload and submit CR both fully written.
    Applied,
    /// Some bytes were written but the sequence did not complete — a partial
    /// write. The bytes may already have reached the target; the caller must NOT
    /// blind-retry (that could duplicate the partial input).
    Ambiguous,
    /// The very first byte could not be written — nothing reached the target, so
    /// a retry is safe. Carries the error text for surfacing.
    CleanFailure(String),
}

/// How far a single `write_all`-style loop got before an error.
enum WriteProgress {
    Complete,
    /// >0 bytes written, then an error / write-zero.
    Partial,
    /// 0 bytes written — the first write failed (nothing reached the target).
    NothingWritten(String),
}

/// Write all of `buf`, tracking whether any bytes reached the writer so a
/// partial write can be told apart from a clean first-write failure. Retries on
/// `Interrupted` like `write_all`.
fn write_all_tracked(w: &mut (dyn std::io::Write + Send), buf: &[u8]) -> WriteProgress {
    let mut written = 0usize;
    while written < buf.len() {
        match w.write(&buf[written..]) {
            Ok(0) => {
                return if written == 0 {
                    WriteProgress::NothingWritten("writer accepted zero bytes".to_string())
                } else {
                    WriteProgress::Partial
                };
            }
            Ok(n) => written += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return if written == 0 {
                    WriteProgress::NothingWritten(e.to_string())
                } else {
                    WriteProgress::Partial
                };
            }
        }
    }
    WriteProgress::Complete
}

/// PRD #20 R20-004 (finding #3): write `payload`, wait `SUBMIT_DELAY`, then write
/// a submit CR — reporting a partial write as [`PayloadDelivery::Ambiguous`]
/// rather than a clean failure. Extracted from
/// [`AgentPtyRegistry::write_and_submit_guarded`] so the ambiguity classification
/// is unit-testable against a fault-injecting writer.
async fn deliver_payload_and_submit(
    w: &mut (dyn std::io::Write + Send),
    payload: &[u8],
) -> PayloadDelivery {
    match write_all_tracked(w, payload) {
        WriteProgress::Complete => {}
        // Payload partially written — bytes may have reached the PTY.
        WriteProgress::Partial => return PayloadDelivery::Ambiguous,
        // Nothing written — safe to retry.
        WriteProgress::NothingWritten(e) => return PayloadDelivery::CleanFailure(e),
    }
    let _ = w.flush();
    tokio::time::sleep(SUBMIT_DELAY).await;
    // The payload already landed; ANY failure writing the submit CR now leaves
    // the target holding un-submitted payload bytes — ambiguous, not clean.
    match write_all_tracked(w, b"\r") {
        WriteProgress::Complete => {}
        WriteProgress::Partial | WriteProgress::NothingWritten(_) => {
            return PayloadDelivery::Ambiguous;
        }
    }
    let _ = w.flush();
    PayloadDelivery::Applied
}

/// One agent owned by the registry: child + master + shared writer + bus.
/// Field names are stable — tests and tooling that peek into the registry
/// (e.g. for `process_id()`) rely on `child` existing here.
pub struct RunningAgent {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub writer: Arc<AsyncMutex<Box<dyn std::io::Write + Send>>>,
    pub bus: Arc<AgentBus>,
    /// Value of [`DOT_AGENT_DECK_PANE_ID`] captured from the spawn-time env,
    /// if the caller supplied one. Echoed back to clients via the M2.x
    /// rehydration path so the TUI can re-bind a freshly-attached pane to
    /// the *same* local pane id the agent's child env was tagged with —
    /// otherwise hook events emitted by the agent (which carry the original
    /// pane id) would be rejected by `AppState::apply_event` after a
    /// reconnect, silently dropping delegate / work-done signals.
    pub pane_id_env: Option<String>,
    /// Human-readable label assigned by the user (M2.11). Captured from
    /// [`SpawnOptions::display_name`] at spawn time and updated via
    /// [`AgentPtyRegistry::set_agent_label`] whenever the TUI renames the
    /// pane. Replayed via `list_agents` on reconnect so renamed panes keep
    /// their names across ssh drops. Values are filtered through
    /// [`is_valid_display_name`]; failing strings are stored as `None`.
    pub display_name: Option<String>,
    /// Working directory the agent was launched in (M2.11). Mirrors
    /// [`SpawnOptions::cwd`] when supplied and validated by [`is_valid_cwd`];
    /// updateable via [`AgentPtyRegistry::set_agent_label`] so a TUI that
    /// learns the cwd after spawn (e.g. via a hook event) can persist it
    /// alongside the display name. Echoed back to clients via `list_agents`
    /// so the dashboard cwd column survives a reconnect.
    pub cwd: Option<String>,
    /// Which tab this pane belonged to at spawn time (PRD #76 M2.12).
    /// Captured from [`SpawnOptions::tab_membership`] after validation;
    /// invalid values are stored as `None` (same drop pattern as
    /// `display_name`). The TUI uses this on reconnect to rebuild
    /// mode/orchestration tabs instead of stranding every hydrated pane
    /// on the dashboard. `None` means dashboard pane (or an older daemon
    /// predating this field — wire-format `skip_serializing_if` keeps the
    /// hydration path backwards compatible).
    pub tab_membership: Option<TabMembership>,
    /// Which AI agent this pane was spawned to run (PRD #76 M2.13).
    /// Captured from [`SpawnOptions::agent_type`] at spawn time and echoed
    /// back via `list_agents` so a TUI reconnect can populate the
    /// hydrated session's `agent_type` instead of defaulting to
    /// `AgentType::None` (which the dashboard renders as "No agent"). The
    /// TUI computes the field via [`AgentType::from_command`]; unknown
    /// commands and non-agent panes stay `None`. Same forward-compat
    /// rationale as `display_name` / `tab_membership` — older clients
    /// that omit the field round-trip as `None`.
    pub agent_type: Option<AgentType>,
    /// The full env vec passed to [`AgentPtyRegistry::spawn_agent`] at
    /// the original spawn, captured so
    /// [`AgentPtyRegistry::respawn_agent_for_pane`] can re-apply it on
    /// the fresh child. Includes `DOT_AGENT_DECK_PANE_ID` and any extra
    /// vars the caller (a role config, the orchestration setup) injected;
    /// without this capture the respawn ran with a leaner env than the
    /// original and silently dropped role-supplied vars.
    pub spawn_env: Vec<(String, String)>,
    /// Last-known PTY size (rows, cols), captured at spawn and
    /// refreshed by [`AgentPtyRegistry::resize`]. Replayed on respawn
    /// so the fresh PTY comes up at the same geometry instead of the
    /// default 24×80 — without this, the new agent's first output
    /// briefly wraps or truncates until the TUI's next resize call
    /// lands.
    pub pty_rows: u16,
    pub pty_cols: u16,
    /// PRD #93 round-2 reviewer REV-3: set to `true` by the reader thread
    /// once the PTY returns EOF (the child died or was killed). The daemon's
    /// idle monitor consults this via [`AgentPtyRegistry::live_count`] so an
    /// agent whose registry entry hasn't been closed yet stops blocking
    /// idle shutdown — otherwise `len()` would include exited entries and
    /// the daemon would stay up forever. The flag is *not* drained from the
    /// registry: tests and tooling that explicitly call `close_agent` /
    /// `shutdown_all` still find the entry; only the idle gate filters it
    /// out. `Arc` because the reader thread holds an independent clone.
    pub exited: Arc<AtomicBool>,
    /// PRD #201 native prompt delivery: a seed/prompt the daemon prepared for
    /// this pane, awaiting a NATIVE pull by the agent's extension via
    /// `dot-agent-deck get-seed` (which then calls `pi.sendUserMessage`).
    /// `None` = nothing pending. Taken (cleared) on first read — whichever
    /// path takes it first (the extension's `get-seed` pull, or the daemon's
    /// PTY-injection safety net) is the SOLE delivery, so a seed is never
    /// delivered twice. Runtime-only: it never crosses the wire (unlike the
    /// `AgentRecord` fields), because `get-seed` reads it directly on the
    /// daemon over the hook socket.
    pub pending_seed: Option<String>,
    /// PRD #201: set `true` when [`RunningAgent::pending_seed`] was consumed by
    /// the native `get-seed` pull, as opposed to the PTY-injection fallback.
    /// Lets a test prove the NATIVE delivery path ran rather than the safety
    /// net (the whole point of dissolving the keystroke-injection workaround).
    pub seed_delivered_native: bool,
}

/// Snapshot of one daemon-side agent that the M2.x rehydration path needs.
/// Carries the registry id plus the spawn-time `DOT_AGENT_DECK_PANE_ID`
/// captured in [`RunningAgent::pane_id_env`], so the TUI can rebuild its
/// pane→agent mapping using the *same* pane id the agent's child process
/// already carries in its environment. Also doubles as the wire-format
/// element for `AttachResponse::agent_records` — serde derives live here
/// so the in-memory and over-the-wire shapes can't drift apart.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id_env: Option<String>,
    /// Display name as last set on the daemon (M2.11). `None` means either
    /// the agent was spawned without a label or the value failed
    /// [`is_valid_display_name`] validation. `skip_serializing_if` keeps
    /// the wire shape backwards-compatible with older clients that don't
    /// know about this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Working directory the agent was launched in, if recorded (M2.11).
    /// `None` when neither the original spawn nor a later `SetAgentLabel`
    /// supplied a value, or when the supplied value failed [`is_valid_cwd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Which tab this pane belonged to at spawn time (PRD #76 M2.12).
    /// `None` means either the agent was a dashboard pane, the spawn
    /// supplied an invalid value (dropped at capture), or the daemon ran
    /// an older binary that didn't persist this field. The TUI uses this
    /// to rebuild mode/orchestration tabs on reconnect.
    /// `skip_serializing_if` keeps the wire shape backwards-compatible
    /// with daemons predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_membership: Option<TabMembership>,
    /// Which AI agent this pane was spawned to run (PRD #76 M2.13).
    /// `None` means either the spawn didn't supply a recognized agent
    /// command, the pane is non-agent, or the daemon ran an older binary
    /// that didn't persist this field. The TUI uses this on reconnect to
    /// populate the placeholder session's `agent_type` (otherwise the
    /// dashboard renders "No agent" until a `SessionStart` hook fires).
    /// `skip_serializing_if` keeps the wire shape backwards-compatible
    /// with daemons predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<AgentType>,
    /// Current PTY rows as last opened or resized on the daemon (PRD
    /// #104). Threaded into the client's vt100 parser at hydration so
    /// snapshot bytes are parsed at the dims they were written at —
    /// without this, a wide-PTY agent's scrollback was clamped to
    /// 80 columns on reattach and the historical rows were corrupted.
    ///
    /// `#[serde(default)]` keeps the wire shape backwards-compatible
    /// for *decode*: an older daemon that omits the field round-trips
    /// as `0`, and the hydration path falls back to the 24×80
    /// placeholder when it sees `0`.
    ///
    /// `skip_serializing_if = "is_zero_u16"` (PRD #104 RN1, reviewer):
    /// on the *encode* side, a daemon that has no real dims yet (e.g.
    /// a future code path that constructs an `AgentRecord` before the
    /// PTY is open) emits the legacy shape — no `rows`/`cols` keys —
    /// instead of the new-shape literal `0`. Pre-PRD clients see the
    /// same JSON they always have; post-PRD clients decode the absent
    /// field via `#[serde(default)]`. Symmetric with the
    /// `pane_id_env` / `display_name` / `cwd` / `tab_membership` /
    /// `agent_type` fields that already use this pattern.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub rows: u16,
    /// Current PTY cols. See `rows` for the full rationale.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub cols: u16,
    /// PRD #162: the daemon's live, event-derived session state for this
    /// agent, joined in by the `ListAgents` handler from `AppState.sessions`
    /// (on `agent_id` + `pane_id`, newest-`last_activity`-wins). `None` when
    /// no live session matches — an older daemon, the test/dummy-state attach
    /// path, or an agent that never emitted an event — and the TUI falls back
    /// to today's bare-placeholder behavior. Additive optional, so the wire
    /// shape stays backwards-compatible with daemons predating this field and
    /// no `PROTOCOL_VERSION` bump is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<crate::state::SessionSnapshot>,
}

/// Skip-predicate for `AgentRecord::rows` / `AgentRecord::cols`
/// serialization. Pulled out as a named helper so the two `#[serde]`
/// attributes share one symbol — closure literals aren't allowed in
/// `skip_serializing_if`.
fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

/// In-process registry of agent PTYs owned by the daemon. M1.1 only exposed
/// the in-process API; M1.2 wires it to the streaming attach protocol via
/// [`AgentBus`] and [`AttachHandle`].
pub struct AgentPtyRegistry {
    inner: Mutex<RegistryInner>,
    /// Per-pane dispatch mutex held by `AppState::handle_delegate`
    /// across the entire respawn+write window for a `clear = true`
    /// delegate. Two concurrent connections submitting `Delegate`
    /// signals to the same worker pane would otherwise race the
    /// `registry.remove` + `spawn_agent` gap inside
    /// [`AgentPtyRegistry::respawn_agent_for_pane`]: the second call
    /// would observe `NotFound` and its prompt would be silently
    /// dropped. The mutex map is keyed by `pane_id_env` so writes to
    /// different panes still proceed in parallel; the existing
    /// per-agent `writer` mutex serializes byte-level writes to one
    /// PTY, but the respawn's remove+spawn window needs a higher-level
    /// lock because the agent identity itself rolls over.
    ///
    /// Entries are NEVER pruned. The map grows monotonically by every
    /// `pane_id_env` ever seen, ~64 B/entry, bounded by pane creation
    /// rate — negligible in practice. Pruning was tried in F9
    /// followup-2 and reverted in F9 followup-3 because it re-opened
    /// the F9 followup-1 race: after a close+respawn for the same
    /// `pane_id_env`, an in-flight dispatcher holds an `Arc<AsyncMutex>`
    /// that's no longer in the map, so a fresh dispatcher gets a
    /// *different* `AsyncMutex` instance for the same `pane_id_env`.
    /// The two dispatchers then don't serialize against each other,
    /// re-introducing the registry remove+spawn race the lock exists
    /// to prevent.
    dispatch_mutexes: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Total number of explicit `KIND_DETACH` frames the daemon has observed
    /// across all attach-stream connections. Plain socket close (implicit
    /// detach) does *not* increment this — only the M2.5 explicit-detach
    /// keybinding path does. Surfaced for tests asserting "the client meant
    /// to detach, not just disconnect," and lightweight observability if a
    /// future status command wants it.
    detach_count: AtomicU64,
    /// PRD #93 round-2 (reviewer REV-1 / REV-3): signaled whenever the set
    /// of *live* agents changes — i.e. when a spawn lands, when a close
    /// runs, or when the reader thread for an agent observes EOF. The
    /// daemon's edge-triggered idle monitor waits on this so a brief
    /// detach+reconnect or an agent dying mid-window wakes the monitor
    /// immediately instead of waiting for the next poll. Cloned by the
    /// per-agent pump_reader so the EOF path can notify without holding a
    /// registry lock.
    change_notify: Arc<Notify>,
    /// PRD #92 F1: latch set the first time the daemon enters its
    /// `KIND_SHUTDOWN` teardown so a second `KIND_SHUTDOWN` (or a SIGTERM
    /// landing during shutdown) doesn't re-iterate the agent map or fight
    /// the original shutdown for ownership of each `Child`. Read by
    /// [`shutdown_all_graceful`]; a second call returns immediately.
    shutting_down: AtomicBool,
    /// PRD #127 M2.2 (deliver-on-idle): the last time a *user* keystroke
    /// (STREAM_IN frame) was forwarded to a pane, keyed by `pane_id_env`. The
    /// scheduler's reuse path consults this to decide whether to deliver a
    /// reuse prompt immediately or queue it until the user goes idle. Only
    /// STREAM_IN updates it — daemon-initiated writes
    /// ([`write_to_pane_and_submit`](Self::write_to_pane_and_submit)) do not,
    /// so a scheduled delivery never resets its own debounce clock. In-memory,
    /// monotonically growing by `pane_id_env` seen (negligible).
    user_input_at: Mutex<HashMap<String, Instant>>,
    /// PRD #20 R20-004 (finding #3): atomic, fingerprint-bound idempotency ledger
    /// for guarded write-and-submit. Keyed by the caller's stable `delivery_id`;
    /// each record binds the id to a fingerprint of the target agent identity,
    /// pane, and text, and carries a single-flight async lock. Concurrent
    /// duplicates of one id serialize on that lock and REPLAY the leader's result
    /// instead of both submitting; a retry after a lost response replays the
    /// cached delivered (or ambiguous) result; reusing an id with a DIFFERENT
    /// fingerprint is a CONFLICT (never a false replay). Bounded by LRU eviction —
    /// see [`MAX_DELIVERY_RESULTS`].
    delivery_ledger: Mutex<DeliveryLedger>,
}

struct RegistryInner {
    next_id: u64,
    agents: HashMap<String, RunningAgent>,
}

/// Internal selector for the two public byte-write entrypoints.
/// `Submit` is the prompt path (payload + `SUBMIT_DELAY` + `\r`);
/// `Notice` is the visibility path (payload + `\n`, no submit). Kept
/// private because the public API exposes the two named methods
/// directly — see [`AgentPtyRegistry::write_to_pane_and_submit`] and
/// [`AgentPtyRegistry::write_to_pane_notice`].
#[derive(Debug)]
enum SubmitMode {
    Submit,
    Notice,
}

impl Default for AgentPtyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentPtyRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                next_id: 1,
                agents: HashMap::new(),
            }),
            dispatch_mutexes: Mutex::new(HashMap::new()),
            detach_count: AtomicU64::new(0),
            change_notify: Arc::new(Notify::new()),
            shutting_down: AtomicBool::new(false),
            user_input_at: Mutex::new(HashMap::new()),
            delivery_ledger: Mutex::new(DeliveryLedger::default()),
        }
    }

    /// PRD #127 M2.2: record that a user keystroke just reached the pane with
    /// `pane_id_env` (the deliver-on-idle debounce clock). Called from the
    /// attach-stream STREAM_IN path. Sentinel / empty pane ids are ignored.
    pub fn note_user_input(&self, pane_id_env: &str) {
        if pane_id_env.is_empty() || pane_id_env.starts_with('<') {
            return;
        }
        self.user_input_at
            .lock()
            .unwrap()
            .insert(pane_id_env.to_string(), Instant::now());
    }

    /// PRD #127 M2.2: the last time a user keystroke reached `pane_id_env`, or
    /// `None` if none has. The reuse path compares this against the debounce
    /// window to choose deliver-now vs queue.
    pub fn last_user_input_at(&self, pane_id_env: &str) -> Option<Instant> {
        self.user_input_at.lock().unwrap().get(pane_id_env).copied()
    }

    /// PRD #127 M2.2: whether `agent_id` is still a live (non-exited) agent in
    /// the registry. The scheduler's reuse registry uses this to decide whether
    /// a recorded tab is still reusable or stale (closed/exited → spawn fresh).
    pub fn agent_is_live(&self, agent_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .agents
            .get(agent_id)
            .map(|a| !a.exited.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// PRD #127 C3: whether the pane with `pane_id_env` is backed by a live
    /// (non-exited) agent. The reuse path gates reuse on the liveness of the
    /// SPECIFIC delivery pane (the orchestrator role pane / single-agent pane)
    /// rather than "any agent for the task", so it never re-delivers into a
    /// dead pane while a sibling role pane happens to still be alive.
    pub fn pane_is_live(&self, pane_id_env: &str) -> bool {
        self.inner.lock().unwrap().agents.values().any(|a| {
            a.pane_id_env.as_deref() == Some(pane_id_env) && !a.exited.load(Ordering::SeqCst)
        })
    }

    /// Borrow (or lazily create) the per-pane dispatch mutex for a
    /// given `pane_id_env`. Callers hold this lock across the entire
    /// respawn+write window of a `clear = true` delegate so two
    /// concurrent same-pane delegates can't race the `registry.remove`
    /// + `spawn_agent` gap inside [`AgentPtyRegistry::respawn_agent_for_pane`].
    ///
    /// PRD #92 F9 followup-3: entries are never pruned. The map grows
    /// by `pane_id_env` ever seen, which is small in practice; pruning
    /// would re-open the followup-1 race where two dispatchers for the
    /// same `pane_id_env` across a close+respawn end up holding
    /// different `AsyncMutex` instances and stop serializing against
    /// each other.
    pub fn pane_dispatch_lock(&self, pane_id_env: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self.dispatch_mutexes.lock().unwrap();
        map.entry(pane_id_env.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// PRD #93 round-2 reviewer REV-1: borrow the change-notify the daemon's
    /// idle monitor waits on. Cloned by callers so they can `.notified()`
    /// without owning the registry. Public so `daemon::run_daemon_with` can
    /// hand the same Arc to the idle monitor it spawns alongside the
    /// hook-ingestion loop.
    pub fn change_notify(&self) -> Arc<Notify> {
        self.change_notify.clone()
    }

    /// Bump the global detach counter. Called by the attach protocol handler
    /// when an explicit `KIND_DETACH` frame is received. Keeps the
    /// distinction between voluntary detach and abrupt disconnect (which is
    /// observed as socket EOF and intentionally not counted here).
    pub fn record_detach(&self) {
        self.detach_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Total number of explicit detach frames seen since this registry was
    /// created. See [`AgentPtyRegistry::record_detach`] for what does and
    /// doesn't increment this.
    pub fn detach_count(&self) -> u64 {
        self.detach_count.load(Ordering::Relaxed)
    }

    /// Spawn a new agent and return its registry id.
    pub fn spawn_agent(&self, mut opts: SpawnOptions<'_>) -> Result<String, AgentPtyError> {
        // CodeRabbit MAJOR (PRD #92 PR #105): Guard A — reject the spawn
        // immediately if the registry has already entered its shutdown
        // path. `daemon_protocol::handle_attach` already rejects an
        // in-flight `StartAgent` once the latch flips, but `spawn_agent`
        // is also reachable from other callers (e.g. respawn, tests),
        // and the early return keeps every entry point uniform without
        // having to plumb the check through each one. Guard B below
        // closes the TOCTOU window between this check and the
        // `inner.agents.insert` that publishes the new agent.
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(AgentPtyError::Spawn("registry is shutting down".into()));
        }

        // Capture the caller-supplied `DOT_AGENT_DECK_PANE_ID` *before*
        // moving `opts` into `spawn`, so the registry retains a copy for
        // M2.x rehydration. The agent's child process gets tagged with
        // this same value via the env scrub-then-apply path in `spawn`,
        // and the TUI uses the captured value on reconnect to rebind its
        // local pane id to whatever the running child already carries —
        // see `RunningAgent::pane_id_env`.
        //
        // Defense in depth (PRD #76 M2.x audit follow-up): scrub the
        // *stored* copy via [`is_valid_pane_id_env`] before retaining it.
        // A hostile or buggy same-user peer reaching the attach socket
        // could otherwise have us echo back oversize / control-char /
        // ANSI-laden values via `agent_records`, growing the cumulative
        // `list_agents` response past `MAX_FRAME_LEN` and breaking
        // hydration for *every* agent. The child process still sees the
        // caller's verbatim value — only the registry's mirror is scrubbed.
        let pane_id_env = opts
            .env
            .iter()
            .find(|(k, _)| k == DOT_AGENT_DECK_PANE_ID)
            .map(|(_, v)| v.clone())
            .and_then(|v| {
                if is_valid_pane_id_env(&v) {
                    Some(v)
                } else {
                    tracing::debug!(
                        len = v.len(),
                        "spawn_agent: dropping caller-supplied DOT_AGENT_DECK_PANE_ID — fails validation, child still sees it but registry won't echo it"
                    );
                    None
                }
            });

        // M2.11: capture display_name and cwd into the registry so renamed
        // panes survive a reconnect. Both go through the same validation
        // helpers used by [`set_agent_label`] so the wire-format invariants
        // (no control chars in display_name, bounded length) hold the same
        // way whether the value arrived via the initial StartAgent or via a
        // later SetAgentLabel.
        let display_name = opts.display_name.and_then(|v| {
            if is_valid_display_name(v) {
                Some(v.to_string())
            } else {
                tracing::debug!(
                    len = v.len(),
                    "spawn_agent: dropping caller-supplied display_name — fails validation"
                );
                None
            }
        });
        let cwd_stored = opts.cwd.and_then(|v| {
            if is_valid_cwd(v) {
                Some(v.to_string())
            } else {
                tracing::debug!(
                    len = v.len(),
                    "spawn_agent: dropping caller-supplied cwd from registry — fails validation (child still sees it)"
                );
                None
            }
        });

        // M2.12: capture tab_membership through the same validation lens
        // (the embedded `name` must satisfy `is_valid_display_name`) so the
        // echo via `list_agents` can't carry control bytes from a hostile
        // same-user peer. M2.12 fixup reviewer #2: an invalid name now
        // *rejects* the spawn (returns `AgentPtyError::Validation`). The
        // earlier behavior — silently dropping to `None` — let a malformed
        // client get a successful `StartAgent` response and quietly
        // reclassified the pane as dashboard on reconnect, hiding the bad
        // spawn metadata. Take the value out of `opts` before `spawn` moves
        // the struct so we don't fight the borrow checker.
        let tab_membership = match opts.tab_membership.take() {
            Some(tm) => {
                // Capture diagnostic info BEFORE moving `tm` into the
                // validator: name length and the optional
                // orchestration_cwd length are surfaced in the
                // rejection error so a buggy client sees which axis
                // failed without exposing the (possibly hostile)
                // bytes themselves.
                let name_len = tm.name().len();
                let orch_cwd_len = match &tm {
                    TabMembership::Orchestration {
                        orchestration_cwd: Some(c),
                        ..
                    } => Some(c.len()),
                    _ => None,
                };
                match validate_tab_membership(tm) {
                    Some(v) => Some(v),
                    None => {
                        return Err(AgentPtyError::Validation(format!(
                            "tab_membership fails validation (name_len={name_len}, \
                             orchestration_cwd_len={orch_cwd_len:?})"
                        )));
                    }
                }
            }
            None => None,
        };

        // M2.13: capture agent_type as-is; the enum is closed (ClaudeCode /
        // OpenCode / None) so there's no equivalent of the display_name /
        // tab_membership validation gate — serde already rejected anything
        // outside the variant set at deserialization.
        //
        // R20-009: PRESERVE the explicit caller identity in `opts` so the common
        // `spawn` seam's Wrapper transform wraps it. Previously this `take()`
        // DROPPED the identity before `spawn`, so an explicitly-Codex spawn whose
        // launcher basename is not `codex` (an alias / launcher / custom path) was
        // recorded as Codex but launched UNWRAPPED. Cloning (not taking) leaves
        // `opts.agent_type` intact for `spawn`'s wrapper decision while registry
        // metadata still records the caller-supplied identity (the
        // learn-from-event upgrade still fills it in for a bare shell spawn).
        let agent_type = opts.agent_type.clone();

        // PRD #92 F9 followup-7: pre-allocate the registry id *before*
        // `spawn` so we can inject `DOT_AGENT_DECK_AGENT_ID = <id>` into
        // the spawned child's environment. The agent's hook script reads
        // this env var and attaches the id to each emitted `AgentEvent`
        // as `agent_id`, which lets the post-respawn dispatch task scope
        // its `SessionStart` wait to the NEW agent — closing the
        // stale-OLD-agent race that the followup-6 broadcast filter
        // (pane_id only) couldn't distinguish.
        //
        // Two lock acquisitions (here + the post-spawn insert) are cheap
        // and uncontended for the common single-spawn path; a failed
        // spawn or duplicate-pane-id rejection just wastes the
        // pre-allocated id (`next_id` is monotonic and not required to
        // be contiguous).
        //
        // Caller-supplied `DOT_AGENT_DECK_AGENT_ID` values are stripped
        // before our injection wins: `respawn_agent_for_pane` replays
        // the OLD agent's `spawn_env` (which carries its id), and an
        // untrimmed replay would tag the NEW agent's hooks with the
        // OLD id — defeating the whole point of the filter.
        let preallocated_id = {
            let mut inner = self.inner.lock().unwrap();
            let id = inner.next_id.to_string();
            inner.next_id += 1;
            id
        };
        opts.env.retain(|(k, _)| k != DOT_AGENT_DECK_AGENT_ID);
        opts.env
            .push((DOT_AGENT_DECK_AGENT_ID.to_string(), preallocated_id.clone()));

        // Capture the full env vec and the requested PTY size BEFORE
        // `spawn(opts)` consumes the options. Stored on `RunningAgent`
        // so [`respawn_agent_for_pane`] can re-apply them to the fresh
        // child instead of resetting to a leaner env and the 24×80
        // default geometry.
        //
        // PRD #104 R3 (reviewer): apply the same `[1, PTY_RESIZE_DIM_MAX]`
        // clamp that [`spawn`] (at the top of this file) and
        // [`AgentPtyRegistry::resize`] use. Pre-PRD this was a private
        // shortcut for the respawn path — a caller-supplied `0` would
        // already have been rejected by `spawn`, and an oversized value
        // was clamped inside `spawn`'s `pty_system.openpty` call but
        // the capture here kept the raw value. With PRD #104 the
        // captured value is now wire-visible via `AgentRecord.rows/cols`
        // and would surface to the client's vt100 parser
        // (`parser_init_dims` clamps defensively, but pinning at the
        // capture site keeps the on-wire value consistent with the
        // kernel's actual TIOCGWINSZ).
        let captured_env = opts.env.clone();
        let captured_rows = opts.rows.clamp(1, PTY_RESIZE_DIM_MAX);
        let captured_cols = opts.cols.clamp(1, PTY_RESIZE_DIM_MAX);

        // PRD #201 (#210 fix): the bundled Pi orchestrator extension is
        // materialized ONCE at daemon startup (`orchestrator_ext::auto_materialize`,
        // called from the `daemon serve` entry), NOT here per spawn. Doing it per
        // spawn meant an unrelated agent start (claude, a shell, a test) rewrote
        // `~/.pi` whenever pi was on PATH; the daemon-startup seam is
        // command-agnostic and touches Pi's dir only once. So there is
        // deliberately no materialize call in the spawn path.
        //
        // PRD #20 note (finding #15 reconciliation): PRD #20 had generalized this
        // into a per-spawn `spec(agent).materialize` dispatch. That per-spawn seam
        // is intentionally dropped here to preserve #210's fix — the registry
        // `AgentSpec.materialize` field remains as capability metadata, but no
        // agent needs a spawn-time materialize (Codex uses the `wrap` seam;
        // `materialize` is None). Adding one back would reintroduce #210's bug.

        // Defense in depth: `spawn` already protects the child internally
        // via its own `ChildGuard`, so any failure or panic *inside* spawn
        // cannot orphan the child. This outer `PtyGuard` covers the
        // remaining gap — between `spawn` returning the `AgentPty` and the
        // `agents.insert` below — where lock poisoning on `inner.lock()`
        // would otherwise drop the `AgentPty` without killing the child
        // (`AgentPty` has no `Drop`).
        let guard = PtyGuard::new(spawn(opts)?);
        let mut inner = self.inner.lock().unwrap();

        // CodeRabbit MAJOR (PRD #92 PR #105): Guard B — re-check the
        // shutdown latch *inside* the inner lock, so the check + insert
        // are atomic against `shutdown_all_graceful`'s `inner.lock()` +
        // drain. Without this, the race is:
        //   T0 daemon_protocol checks is_shutting_down() — false
        //   T1 shutdown flips the latch and drains `inner.agents`
        //   T2 spawn_agent reaches the insert below and adds an agent
        //      the drain already iterated past — orphaned child.
        // Guard A at the top of `spawn_agent` covers the common case;
        // this re-check closes the narrow window between Guard A and
        // the insert. On Err the `guard` Drop kills the child we just
        // spawned, so the rejection doesn't leak a PTY.
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(AgentPtyError::Spawn("registry is shutting down".into()));
        }

        // CodeRabbit MAJOR (PRD #93 round-9): reject the spawn if
        // another live agent already claims this `pane_id_env`.
        // `write_to_pane_and_submit` routes by `pane_id_env`, so two agents sharing
        // one id silently misroute every delegate/work-done write to
        // whichever entry `values().find(...)` happened to visit first.
        // The check sits INSIDE the post-spawn lock acquisition so the
        // check + insert is atomic — a concurrent spawn with the same
        // pane id can't squeeze between a pre-spawn check and the
        // insert. On Err the `guard` Drop kills the child we just
        // spawned, so the rejection doesn't leak a PTY.
        //
        // Round-10 LOW (auditor): skip exited agents — `live_count`'s
        // contract is "an exited entry is reaped only when something
        // else (an explicit close or close_all) actually removes it,"
        // so a dead-but-not-yet-reaped entry would otherwise block
        // reuse of its pane_id_env forever. The same `exited.load`
        // filter is applied across every operational lookup —
        // `write_to_pane_and_submit`, `agent_records`, and this dup check —
        // so the live/dead boundary stays consistent
        // (round-11 reviewer #A). Cleanup paths (`close_agent`,
        // `shutdown_all`) deliberately still touch exited entries.
        if let Some(ref candidate) = pane_id_env
            && inner.agents.values().any(|a| {
                a.pane_id_env.as_deref() == Some(candidate.as_str())
                    && !a.exited.load(Ordering::SeqCst)
            })
        {
            return Err(AgentPtyError::DuplicatePaneId(candidate.clone()));
        }

        let pty = guard.take();
        let AgentPty {
            child,
            master,
            writer,
            reader,
        } = pty;

        let bus = Arc::new(AgentBus::new());
        let bus_for_thread = bus.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_thread = exited.clone();
        let notify_for_thread = self.change_notify.clone();
        // Detached thread: exits when the PTY returns EOF (child killed).
        // On exit, pump_reader sets `exited` and signals `change_notify` so
        // the idle monitor learns about the death immediately instead of
        // waiting for the next poll cycle.
        std::thread::spawn(move || {
            pump_reader(reader, bus_for_thread, exited_for_thread, notify_for_thread)
        });

        let agent = RunningAgent {
            child,
            master,
            writer: Arc::new(AsyncMutex::new(writer)),
            bus,
            pane_id_env,
            display_name,
            cwd: cwd_stored,
            tab_membership,
            agent_type,
            spawn_env: captured_env,
            pty_rows: captured_rows,
            pty_cols: captured_cols,
            exited,
            // PRD #201: a fresh agent starts with no pending seed; the seed
            // path (StartAgent `seed` at spawn / a delegate respawn) sets it
            // right after this spawn returns, before the agent's extension
            // pulls it on `session_start`.
            pending_seed: None,
            seed_delivered_native: false,
        };

        // Use the id we pre-allocated above (before spawn) and injected
        // as `DOT_AGENT_DECK_AGENT_ID` into the child's env. Keeping
        // the inserted-into-registry id identical to the env-injected
        // id is the invariant the agent-id-scoped SessionStart filter
        // depends on.
        let id = preallocated_id;
        inner.agents.insert(id.clone(), agent);
        // Signal *after* releasing the lock would be cleaner, but we still
        // hold `inner` here. Notify is cheap and a spurious wake-up is
        // harmless — the monitor will re-check counters anyway.
        self.change_notify.notify_one();
        Ok(id)
    }

    /// Write `text` as a submitted prompt to the PTY of the agent whose
    /// `pane_id_env` matches `pane_id`.
    ///
    /// PRD #93 round-5: orchestration dispatch (delegate / work-done) now
    /// lives on the daemon side, and routing happens via this method. The
    /// caller (typically `AppState::handle_delegate` /
    /// `AppState::handle_work_done` inside the daemon's hook loop) holds the
    /// TUI's pane id, not the registry's agent id; we look up by
    /// `pane_id_env` so the daemon can target panes without keeping a
    /// separate pane→agent index. Bytes that land in the PTY surface as
    /// normal terminal output in the pane's scrollback — that's the new
    /// "journal" surface for orchestration feedback (no separate
    /// broadcast / file cursor / buffer).
    ///
    /// PRD #93 round-6: the daemon must mirror the TUI's submit contract
    /// (see [`crate::pane_input`] and `EmbeddedPaneController::write_to_pane`
    /// in `src/embedded_pane.rs`). Just dropping the prompt bytes into the
    /// PTY leaves them sitting in the agent TUI's input box — the worker
    /// never starts processing until the user manually presses Enter.
    /// So: encode the payload (raw for single-line, bracketed paste for
    /// multi-line), flush, wait [`SUBMIT_DELAY`] so the CR isn't fused with
    /// the preceding text into "newline-in-input", then write the CR.
    ///
    /// PRD #93 round-8: per-pane serialization is now enforced by holding
    /// the agent's writer mutex across the *entire* payload + sleep + CR
    /// sequence. Earlier rounds released the lock around the sleep so
    /// other panes could be written to in parallel — which already worked
    /// because each agent owns its own writer mutex — but released it for
    /// the *same* pane too, letting two concurrent calls interleave as
    /// `payload_A + payload_B + CR + CR` (auditor finding). `tokio::sync::Mutex`
    /// can be held across `.await` safely, and writes to other panes use
    /// other writer mutexes, so holding for the ~150ms `SUBMIT_DELAY`
    /// affects only the offending pane and the deck dispatches at most
    /// one delegate or work-done per pane at a time in practice.
    pub async fn write_to_pane_and_submit(
        &self,
        pane_id: &str,
        text: &str,
    ) -> Result<(), AgentPtyError> {
        self.write_to_pane_internal(pane_id, text, SubmitMode::Submit)
            .await
    }

    /// PRD #20 R20-004 (finding #3): a stable fingerprint of a delivery's
    /// identity — the (expected) target agent id, the pane, and the exact text.
    /// A `delivery_id` is bound to its fingerprint at first admission; a later
    /// request that reuses the id with a DIFFERENT fingerprint is refused as a
    /// conflict rather than replaying the first (unrelated) result. Process-local
    /// (the ledger never crosses the wire), so `DefaultHasher` is sufficient.
    pub fn delivery_fingerprint(expected_agent_id: Option<&str>, pane_id: &str, text: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        expected_agent_id.hash(&mut h);
        pane_id.hash(&mut h);
        text.hash(&mut h);
        h.finish()
    }

    /// PRD #20 R20-004 (finding #3): admit a `delivery_id` + `fingerprint` into
    /// the idempotency ledger before a guarded send. Atomic and single-flight:
    ///
    /// * an id already completed with a MATCHING fingerprint → [`DeliveryAdmission::Replay`];
    /// * an id reused with a DIFFERENT fingerprint → [`DeliveryAdmission::Conflict`];
    /// * otherwise → [`DeliveryAdmission::Proceed`] holding the single-flight
    ///   guard, so a concurrent duplicate blocks and replays this attempt's
    ///   result instead of double-submitting.
    pub async fn admit_delivery(&self, delivery_id: &str, fingerprint: u64) -> DeliveryAdmission {
        // Phase 1 (sync): immediate replay/conflict check + get-or-create the
        // per-id single-flight lock.
        let lock = {
            let mut ledger = self.delivery_ledger.lock().unwrap();
            if let Some(rec) = ledger.records.get(delivery_id) {
                if rec.fingerprint != fingerprint {
                    return DeliveryAdmission::Conflict;
                }
                if let Some(result) = rec.result {
                    ledger.touch(delivery_id);
                    return DeliveryAdmission::Replay(result);
                }
                rec.lock.clone()
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                ledger.records.insert(
                    delivery_id.to_string(),
                    DeliveryRecord {
                        fingerprint,
                        lock: lock.clone(),
                        result: None,
                    },
                );
                ledger.touch(delivery_id);
                lock
            }
        };
        // Phase 2 (async): serialize concurrent duplicates of this id.
        let guard = lock.lock_owned().await;
        // Phase 3 (sync): double-check — another attempt may have completed (or a
        // conflicting reuse landed) while we waited for the single-flight lock.
        {
            let mut ledger = self.delivery_ledger.lock().unwrap();
            if let Some(rec) = ledger.records.get(delivery_id) {
                if rec.fingerprint != fingerprint {
                    return DeliveryAdmission::Conflict;
                }
                if let Some(result) = rec.result {
                    ledger.touch(delivery_id);
                    return DeliveryAdmission::Replay(result);
                }
            }
        }
        DeliveryAdmission::Proceed(DeliveryPermit {
            delivery_id: delivery_id.to_string(),
            _guard: guard,
        })
    }

    /// PRD #20 R20-004 (finding #3): publish the honest `outcome` produced for a
    /// [`DeliveryPermit`]. A DELIVERED (`applied`/`queued`) or `ambiguous`
    /// outcome is CACHED so a retry (or a concurrent duplicate still awaiting the
    /// single-flight lock) replays it instead of writing again. Every other
    /// (non-delivered) outcome is FORGOTTEN so a later retry re-attempts — a
    /// history-only role that becomes live must still receive its prompt.
    pub fn record_delivery_outcome(
        &self,
        permit: &DeliveryPermit,
        outcome: crate::event::SendResult,
    ) {
        use crate::event::SendResult;
        let cache = matches!(
            outcome,
            SendResult::Applied | SendResult::Queued | SendResult::Ambiguous
        );
        let mut ledger = self.delivery_ledger.lock().unwrap();
        if cache {
            if let Some(rec) = ledger.records.get_mut(&permit.delivery_id) {
                rec.result = Some(outcome);
            }
            ledger.touch(&permit.delivery_id);
            ledger.evict_to_cap();
        } else {
            ledger.forget(&permit.delivery_id);
        }
    }

    /// PRD #20 R20-004 (finding #3): forget an in-flight delivery whose attempt
    /// failed CLEANLY (a transport error before any byte reached the target), so
    /// a retry re-attempts rather than being pinned to a stale in-flight record.
    /// The single-flight guard releases when the caller drops the permit.
    pub fn forget_delivery(&self, permit: &DeliveryPermit) {
        self.delivery_ledger
            .lock()
            .unwrap()
            .forget(&permit.delivery_id);
    }

    /// PRD #20 R20-003/R20-006: the live target that currently owns `pane_id`,
    /// resolved under the registry lock. Returns the shared writer, the target's
    /// registry id, and its `exited` liveness token so the caller can bind
    /// authorization to the EXACT identity and re-check it after acquiring the
    /// writer. Skips exited entries (mirrors [`Self::write_to_pane_internal`]).
    /// PRD #20 R20-006 (finding #7): the registry id of the live (non-exited)
    /// agent that CURRENTLY owns `pane_id`, or `None` if no live entry does. The
    /// attach input path calls this AFTER acquiring the target writer to
    /// re-authorize a stream write against the current owner: a close/respawn
    /// that landed while the frame waited for the writer flips the owner (a
    /// different id) or removes it (`None`), so no bytes reach a stale/removed
    /// target. Mirrors the exited-entry skip of [`Self::writer_target_for_pane`].
    pub fn pane_current_agent_id(&self, pane_id: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .agents
            .iter()
            .find(|(_, a)| {
                a.pane_id_env.as_deref() == Some(pane_id) && !a.exited.load(Ordering::SeqCst)
            })
            .map(|(id, _)| id.clone())
    }

    /// PRD #20 R20-003 (finding #4): whether a deck client is CURRENTLY attached
    /// to (driving) `pane_id` — i.e. its agent's PTY stream has ≥1 live
    /// subscriber. The write-and-submit session guard uses this to scope its
    /// strictest check: a stale prompt that would surface in a LIVE INTERACTIVE
    /// conversation (an attached pane the user is watching — finding #4's actual
    /// threat) is refused even when the pane reports NO current hook session,
    /// whereas a headless, unattached delivery with a confirmed agent identity
    /// proceeds. In the real deck the TUI is always attached to a pane it drives,
    /// so the strict guard applies to every real automatic-prompt delivery.
    pub fn pane_has_live_attach(&self, pane_id: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.agents.values().any(|a| {
            a.pane_id_env.as_deref() == Some(pane_id)
                && !a.exited.load(Ordering::SeqCst)
                && a.bus.receiver_count() > 0
        })
    }

    fn writer_target_for_pane(&self, pane_id: &str) -> Option<PaneWriterTarget> {
        let inner = self.inner.lock().unwrap();
        inner
            .agents
            .iter()
            .find(|(_, a)| {
                a.pane_id_env.as_deref() == Some(pane_id) && !a.exited.load(Ordering::SeqCst)
            })
            .map(|(id, a)| PaneWriterTarget {
                writer: a.writer.clone(),
                agent_id: id.clone(),
                exited: a.exited.clone(),
            })
    }

    /// PRD #20 R20-003/R20-006: atomic write-and-submit that binds delivery to an
    /// EXACT target identity and RE-VALIDATES it after acquiring that target's
    /// writer, immediately before writing — closing the liveness/rebind TOCTOU
    /// that the plain [`Self::write_to_pane_and_submit`] leaves open (it checks
    /// liveness, releases the state lock, then awaits a separate writer lookup).
    ///
    /// Flow:
    /// 1. Resolve the live target for `pane_id`; `None` → [`GuardedSend::NoLiveTarget`].
    /// 2. If `expected_agent_id` names a different agent than currently owns the
    ///    pane → [`GuardedSend::WrongSession`] (no write).
    /// 3. Acquire that target's writer (may block behind an in-flight write).
    /// 4. RE-VALIDATE under the held writer: the pane must still resolve to the
    ///    SAME live, non-exited agent, and `revalidate()` (the caller's
    ///    liveness/session recheck against `AppState`) must still hold — else
    ///    [`GuardedSend::Stale`]/[`GuardedSend::WrongSession`] with NO bytes written.
    /// 5. Write payload → `SUBMIT_DELAY` → CR, all under the held writer.
    pub async fn write_and_submit_guarded<Fut>(
        &self,
        pane_id: &str,
        text: &str,
        expected_agent_id: Option<&str>,
        revalidate: impl FnOnce() -> Fut,
    ) -> Result<GuardedSend, AgentPtyError>
    where
        Fut: std::future::Future<Output = bool>,
    {
        let Some(target) = self.writer_target_for_pane(pane_id) else {
            return Ok(GuardedSend::NoLiveTarget);
        };
        // Pre-lock identity gate: refuse a prompt queued for a different agent
        // than the one that now owns the pane (respawn/rebind before delivery).
        if let Some(expected) = expected_agent_id
            && expected != target.agent_id
        {
            return Ok(GuardedSend::WrongSession);
        }
        // Encode before locking so a bad payload doesn't pin the writer.
        let payload = encode_pane_payload(text)?;
        // Acquire the EXACT target writer, THEN re-validate — this is the
        // barrier the TOCTOU test holds open by locking the writer externally.
        let mut w = target.writer.lock().await;
        // Re-resolve identity: the pane may have rebound to a new agent, or the
        // target may have exited, while we waited for the writer.
        match self.writer_target_for_pane(pane_id) {
            Some(current) if current.agent_id == target.agent_id => {}
            Some(_) => return Ok(GuardedSend::WrongSession),
            None => return Ok(GuardedSend::Stale),
        }
        if target.exited.load(Ordering::SeqCst) {
            return Ok(GuardedSend::Stale);
        }
        // Liveness/session recheck against the authoritative session state.
        if !revalidate().await {
            return Ok(GuardedSend::Stale);
        }
        // Authorized — write + submit, holding the writer across the whole
        // sequence (mirrors `write_to_pane_internal`'s atomic submit contract).
        // PRD #20 R20-004 (finding #3): classify WHERE a writer error struck. A
        // failure before any byte reached the PTY is a clean, retryable transport
        // error; a partial write (payload started, or the submit CR failed after
        // the payload landed) is AMBIGUOUS and must not be blind-retried.
        match deliver_payload_and_submit(&mut **w, &payload).await {
            PayloadDelivery::Applied => Ok(GuardedSend::Applied),
            PayloadDelivery::Ambiguous => Ok(GuardedSend::Ambiguous),
            PayloadDelivery::CleanFailure(e) => Err(AgentPtyError::Writer(e)),
        }
    }

    /// Writes bytes to the pane's PTY without triggering submission semantics
    /// (no SUBMIT_DELAY, no CR). Used for visible status notices (e.g., respawn
    /// failures) that must appear in the orchestrator pane's scrollback but
    /// should not be processed by the agent's LLM as a user prompt.
    ///
    /// The notice is terminated with a single `\n` (LF, NOT CR) — agents
    /// like claude / codex submit on CR, so LF leaves the bytes as a
    /// visible-but-unsubmitted line in the pane's scrollback.
    ///
    /// KNOWN LIMITATIONS — agent-side behavior the daemon cannot control:
    /// - If an agent's TUI interprets LF (\n) as Enter, the notice will be
    ///   submitted as a prompt anyway. Observed safe: TODO(M7.1) — populate
    ///   after manual test against each supported agent. Observed unsafe:
    ///   (none confirmed).
    /// - Subsequent [`AgentPtyRegistry::write_to_pane_and_submit`] calls on
    ///   the same pane will submit "{notice text}\n{user prompt}" together —
    ///   the notice bytes accumulate in the agent's stdin line buffer.
    ///
    /// Both limitations point to F11 (bus-push status delivery) as the proper
    /// long-term fix — see `audit/pre-daemon-parity-audit.md`.
    pub async fn write_to_pane_notice(
        &self,
        pane_id: &str,
        text: &str,
    ) -> Result<(), AgentPtyError> {
        self.write_to_pane_internal(pane_id, text, SubmitMode::Notice)
            .await
    }

    async fn write_to_pane_internal(
        &self,
        pane_id: &str,
        text: &str,
        mode: SubmitMode,
    ) -> Result<(), AgentPtyError> {
        // Resolve writer under the sync lock, then drop the lock before
        // awaiting the async writer mutex — otherwise we'd hold the
        // registry mutex across an `await`, blocking every other registry
        // op (spawn, subscribe, list) until the PTY accepted the bytes.
        //
        // Round-11 reviewer #A: skip exited agents in the find. Round
        // 10 added the exited filter on the spawn-side dup check so a
        // new agent can reuse an exited agent's pane_id_env; without
        // the symmetric filter HERE, this find could still match the
        // dead entry and route bytes into a closed PTY whose pump
        // thread already saw EOF. Mirrors `live_count`'s contract:
        // operational lookups treat exited entries as gone, cleanup
        // paths (`close_agent`, `shutdown_all`) still touch them.
        // Capture both the writer and the agent_id (HashMap key) so the
        // trace events below can emit `pane_id` AND `agent_id` — the
        // M1.4 cross-path byte trace diffs against the STREAM_IN trace
        // in `daemon_protocol::handle_attach_stream`, which keys off
        // agent_id; both sides need the common key to correlate writes.
        let (writer, agent_id) = {
            let inner = self.inner.lock().unwrap();
            inner
                .agents
                .iter()
                .find(|(_, a)| {
                    a.pane_id_env.as_deref() == Some(pane_id) && !a.exited.load(Ordering::SeqCst)
                })
                .map(|(id, a)| (a.writer.clone(), id.clone()))
                .ok_or_else(|| AgentPtyError::NotFound(pane_id.to_string()))?
        };
        use std::io::Write as _;
        let payload = encode_pane_payload(text)?;
        let mut w = writer.lock().await;
        // PRD #128 (cherry-picked from PR #122): byte-level trace of every
        // daemon-initiated PTY write. Gated by `RUST_LOG=trace`. Logs the
        // payload and trailing terminator separately so an operator can
        // see whether bracketed-paste framing (`\x1b[200~...\x1b[201~`) is
        // present and whether the terminator is `\r` (13) or `\n` (10).
        // Emitted INSIDE the writer mutex critical section so trace order
        // matches actual write order under concurrent writers. Both
        // `pane_id` and `agent_id` are emitted so the M1.4 diff against
        // the STREAM_IN trace can join on either key.
        tracing::trace!(
            target: "pane_write",
            source = "daemon",
            mode = ?mode,
            pane_id = %pane_id,
            agent_id = %agent_id,
            payload_len = payload.len(),
            payload = %escape_bytes_for_log(&payload),
            "daemon write_to_pane: payload bytes"
        );
        w.write_all(&payload)
            .map_err(|e| AgentPtyError::Writer(e.to_string()))?;
        let _ = w.flush();
        match mode {
            SubmitMode::Submit => {
                tokio::time::sleep(SUBMIT_DELAY).await;
                tracing::trace!(
                    target: "pane_write",
                    source = "daemon",
                    pane_id = %pane_id,
                    agent_id = %agent_id,
                    terminator = %escape_bytes_for_log(b"\r"),
                    "daemon write_to_pane: submit terminator"
                );
                w.write_all(b"\r")
                    .map_err(|e| AgentPtyError::Writer(e.to_string()))?;
                let _ = w.flush();
            }
            SubmitMode::Notice => {
                // PRD #92 F9 followup-2: terminate the notice on a `\n`
                // so it forms a visible line in the orchestrator pane's
                // scrollback without an agent TUI treating it as a
                // submitted prompt (claude / codex submit on CR).
                // `encode_pane_payload` strips trailing whitespace so a
                // caller-provided `\n` would have been swallowed; the
                // single byte is written here unambiguously.
                tracing::trace!(
                    target: "pane_write",
                    source = "daemon",
                    pane_id = %pane_id,
                    agent_id = %agent_id,
                    terminator = %escape_bytes_for_log(b"\n"),
                    "daemon write_to_pane: notice terminator"
                );
                w.write_all(b"\n")
                    .map_err(|e| AgentPtyError::Writer(e.to_string()))?;
                let _ = w.flush();
            }
        }
        Ok(())
    }

    /// Stop an agent: SIGKILL the child, reap it, drop its handles. Any
    /// streaming subscribers will observe their broadcast receiver close
    /// shortly after (once the reader thread sees EOF and drops its bus
    /// reference).
    ///
    /// PRD #92 F8: the kill path now uses
    /// [`terminate_child_with_grace_and_wait`] — SIGTERM with a
    /// 3-second grace before SIGKILL — so a well-behaved agent can
    /// trap SIGTERM and clean up its own descendants (e.g. the
    /// `setsid`'d sub-shells Claude Code creates internally).
    /// Misbehaving agents are still reaped after the grace window.
    ///
    /// PRD #92 F9 followup-3: this path used to prune the
    /// `dispatch_mutexes` entry for `agent.pane_id_env`. Pruning was
    /// reverted because it re-opened the followup-1 race: an
    /// in-flight dispatcher holds an `Arc<AsyncMutex>` already cloned
    /// out of the map; after the close+respawn a fresh dispatcher
    /// would `or_insert_with(...)` a *different* `AsyncMutex` for the
    /// same `pane_id_env`, and the two dispatchers stop serializing.
    /// The map's monotonic growth is bounded by pane creation rate
    /// (~64 B/entry) — accepted as negligible.
    pub fn close_agent(&self, id: &str) -> Result<(), AgentPtyError> {
        let mut agent = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .agents
                .remove(id)
                .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?
        };
        crate::platform::proc::terminate_child_with_grace_and_wait(
            &mut agent.child,
            AGENT_TERMINATE_GRACE,
        );
        // Notify the idle monitor so it observes the registry shrink
        // immediately. The pump_reader thread will *also* signal once it
        // sees EOF from the kill, but doing it here makes the
        // explicit-close path edge-trigger the monitor without depending
        // on the kernel's PTY drain timing.
        self.change_notify.notify_one();
        Ok(())
    }

    /// Respawn the agent attached to a given `pane_id_env`: gracefully
    /// terminate the current child, then spawn a fresh one running
    /// `command` and rebind it to the same `pane_id_env`. Returns the
    /// new registry id.
    ///
    /// PRD #92 F9: the per-role `clear` flag pre-baseline meant "kill
    /// the worker agent and spawn a fresh one before the next task
    /// lands so the new task starts with empty context." Pre-PRD-#76
    /// this was implemented TUI-side via close-then-create on the
    /// pane controller (see `git show 2fc39c3:src/ui.rs::dispatch_delegate_events`).
    /// Post-PRD-#93 the daemon owns the PTYs, so the equivalent has
    /// to happen daemon-side — this method.
    ///
    /// Identity-preserving fields on [`RunningAgent`] (`pane_id_env`,
    /// `display_name`, `cwd`, `tab_membership`, `agent_type`) are
    /// captured from the existing entry and re-applied to the new
    /// spawn. The TUI's pane card therefore stays put across the
    /// respawn: the daemon's `agent_records()` snapshot still lists
    /// the same `pane_id_env` and `tab_membership`, so a TUI that
    /// reattaches mid-respawn rebinds to the new agent cleanly.
    /// Registry ids (`id`) are sequential and DO change — callers
    /// that key off the old id (e.g. a subscriber holding an
    /// `AttachHandle`) will see their broadcast receiver close once
    /// the old child's reader thread reaches EOF; the standard
    /// reattach path (`subscribe` by `pane_id_env` lookup) brings
    /// them onto the new agent's bus.
    ///
    /// The blocking termination work (up to
    /// [`AGENT_TERMINATE_GRACE`] of `try_wait` polling, mirroring
    /// `close_agent`'s contract) runs on a `spawn_blocking` pool task
    /// so the daemon's async runtime threads stay responsive. Mirrors
    /// the pattern `daemon_protocol.rs::handle_close_agent` uses for
    /// the Ctrl+W close path (PRD #92 F8 followup auditor #1).
    ///
    /// The new agent comes up at a default 24×80 PTY size; the TUI's
    /// next `resize` call (sent on attach / render) corrects it to
    /// the client's actual geometry. Deferring the post-respawn prompt
    /// write until the freshly-spawned agent signals readiness is the
    /// caller's responsibility — the daemon doesn't peek into the new
    /// agent's stdout, so it can't observe "ready" directly here. The
    /// dispatch path subscribes to the daemon-wide hook broadcast
    /// before this call and waits for the new agent's `SessionStart`
    /// event (10 s timeout fallback) — see
    /// [`crate::state::SESSION_START_WAIT_TIMEOUT`].
    pub async fn respawn_agent_for_pane(
        &self,
        pane_id_env: &str,
        command: &str,
    ) -> Result<String, AgentPtyError> {
        // Step 1: atomically lift the existing entry out of the
        // registry. Holding the sync lock across the find+remove keeps
        // a concurrent `write_to_pane_and_submit` from racing in and
        // writing to a PTY whose child we're about to terminate (the
        // writer mutex is per-agent so concurrent writes against the
        // same `pane_id_env` are still serialized, but a write that arrived
        // BEFORE we removed the entry could already be flushing).
        //
        // The `exited` filter is deliberately omitted: a dead-but-not-
        // yet-reaped agent should also be replaced — its registry
        // entry is the place the new agent's identity (display_name,
        // tab_membership, etc.) lives, and `clear = true` on a
        // crashed agent should still produce a fresh worker.
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            let agent_id = inner
                .agents
                .iter()
                .find(|(_, a)| a.pane_id_env.as_deref() == Some(pane_id_env))
                .map(|(id, _)| id.clone())
                .ok_or_else(|| AgentPtyError::NotFound(pane_id_env.to_string()))?;
            inner
                .agents
                .remove(&agent_id)
                .expect("agent_id was just located inside the same lock hold")
        };

        let RunningAgent {
            child,
            master,
            writer,
            bus: _,
            // The `pane_id_env` lives inside `spawn_env` already (the
            // initial `spawn_agent` call placed it there), so we don't
            // re-inject it explicitly on respawn — see step 3 below.
            pane_id_env: _captured_pane_id_env,
            display_name,
            cwd,
            tab_membership,
            agent_type,
            spawn_env,
            pty_rows,
            pty_cols,
            exited: _,
            // PRD #201: a respawn (`clear = true` delegate) drops any seed the
            // old child left unconsumed; the caller re-arms the fresh child's
            // seed via `set_pending_seed` right after this returns.
            pending_seed: _,
            seed_delivered_native: _,
        } = removed;

        // Drop this reference to the writer Arc; the slave half closes
        // when the last reference is dropped (typically immediately,
        // unless a concurrent write is in flight against the old
        // `pane_id_env`). The writer is an `Arc<AsyncMutex<...>>` and
        // `write_to_pane_internal` clones the Arc before awaiting the
        // inner lock, so a write that started before the respawn's atomic
        // remove still holds a clone and delays the slave-close until
        // it finishes its CR write and drops the clone. The terminate
        // helper still escalates to SIGKILL if the child hangs on
        // slave EOF, so a buggy agent is still reaped within the
        // grace window.
        drop(writer);
        drop(master);

        // Step 2: terminate the previous child on the blocking pool.
        // `terminate_child_with_grace_and_wait` polls `try_wait`
        // synchronously for up to `AGENT_TERMINATE_GRACE` (3 s); running
        // that on a tokio worker thread would block other futures on
        // the same worker. Same shape `daemon_protocol.rs` uses for
        // `close_agent`.
        let mut child = child;
        let join = tokio::task::spawn_blocking(move || {
            crate::platform::proc::terminate_child_with_grace_and_wait(
                &mut child,
                AGENT_TERMINATE_GRACE,
            );
        })
        .await;
        if let Err(join_err) = join {
            // The spawn_blocking task ran the SIGTERM → poll → SIGKILL
            // sequence in `terminate_child_with_grace_and_wait`. A
            // `JoinError` here means the closure panicked or was
            // cancelled before returning; the SIGKILL backstop inside
            // the helper only fires if the closure reached that line.
            // The helper is panic-free in practice (no panic-prone
            // calls in its body), so this branch is a defensive log —
            // the child may or may not have been reaped depending on
            // where the panic landed.
            tracing::warn!(
                pane_id = %pane_id_env,
                error = %join_err,
                "respawn: spawn_blocking for terminate panicked or was cancelled; \
                 proceeding with fresh spawn anyway"
            );
        }

        // Step 3: spawn a fresh agent with the captured identity.
        // Replay the full env from the original spawn (including
        // `DOT_AGENT_DECK_PANE_ID` and any role-supplied extras) and
        // the last-known PTY size so the fresh child comes up with
        // the same environment + geometry as its predecessor. Earlier
        // versions reconstructed a minimal env containing only
        // `DOT_AGENT_DECK_PANE_ID` and pinned the size to the 24×80
        // default, silently dropping role-supplied env vars and
        // briefly mis-wrapping the new agent's first output until the
        // TUI's next resize landed.
        let opts = SpawnOptions {
            command: Some(command),
            cwd: cwd.as_deref(),
            display_name: display_name.as_deref(),
            rows: pty_rows,
            cols: pty_cols,
            env: spawn_env,
            tab_membership,
            agent_type,
        };
        self.spawn_agent(opts)
    }

    /// Subscribe to an agent's live output and take its scrollback snapshot
    /// in one atomic step. Used by the attach protocol handler.
    pub fn subscribe(&self, id: &str) -> Result<AttachHandle, AgentPtyError> {
        let inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        let (snapshot, rx) = agent.bus.subscribe();
        // PRD #20 R20-008: capture the writer AND the target's identity/liveness
        // under this single lock, so the attach handler never needs the racy
        // post-lock `pane_id_env_for_agent` lookup that could resolve to
        // `<agent-gone>` (which `pane_writable` treats as `Live`).
        Ok(AttachHandle {
            snapshot,
            rx,
            writer: agent.writer.clone(),
            agent_id: id.to_string(),
            pane_id_env: agent.pane_id_env.clone(),
            exited: agent.exited.clone(),
        })
    }

    /// Resize an agent's PTY. Mirrors the local-mode `MasterPty::resize`
    /// shape (`PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }`).
    /// Zero rows or cols are rejected up front so a buggy caller can't
    /// quietly produce a 0×0 PTY (which would deadlock any agent that
    /// reads `TIOCGWINSZ`). Non-zero values are silently clamped down to
    /// [`PTY_RESIZE_DIM_MAX`] — see the constant docs for the rationale.
    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<(), AgentPtyError> {
        if rows == 0 || cols == 0 {
            return Err(AgentPtyError::Resize(format!(
                "rows and cols must be > 0 (got {rows}x{cols})"
            )));
        }
        let rows = rows.min(PTY_RESIZE_DIM_MAX);
        let cols = cols.min(PTY_RESIZE_DIM_MAX);
        let mut inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get_mut(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        // PRD #104 A1 followup: skip the entire ioctl + bookkeeping
        // when neither dimension changes. The local TUI resize sweep
        // calls `resize_pane_pty` on every frame the viewport is
        // unchanged (cheap idempotent path), so without this guard
        // every no-op tick would:
        //   (a) issue TIOCSWINSZ to the kernel, which on Linux/macOS
        //       delivers SIGWINCH to the child even when the dimensions
        //       are identical — causing the inner TUI to redraw on every
        //       frame tick;
        //   (b) clear the scrollback ring unnecessarily, so a
        //       hydration-replay snapshot taken mid-stream would observe
        //       an empty buffer instead of the live agent's scrollback.
        // Guard is *before* the ioctl to avoid both side-effects.
        if agent.pty_rows == rows && agent.pty_cols == cols {
            return Ok(());
        }
        agent
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AgentPtyError::Resize(e.to_string()))?;
        // Refresh the captured size so a subsequent respawn replays
        // the latest geometry, not the spawn-time default. PRD #104
        // also surfaces this on `AgentRecord` via `agent_records()`
        // so the client's vt100 parser is initialised at the dims the
        // snapshot bytes were written at.
        agent.pty_rows = rows;
        agent.pty_cols = cols;
        // PRD #104 M3: drop the scrollback ring on resize. After this
        // point a snapshot returned to a fresh subscriber represents
        // a single (rows, cols) epoch — the agent's current one. The
        // inner TUI's SIGWINCH-driven full-screen redraw repopulates
        // scrollback at the new dims within the first frame, so this
        // is not a content-loss for the interactive case. Pre-PRD,
        // a snapshot could carry bytes from before *and* after a
        // resize, and the parser at attach time had no way to know
        // which was which.
        //
        // PRD #104 R2 (reviewer): the clear takes the same
        // `AgentBus::state` mutex that `AgentBus::push` (and
        // `subscribe`/`snapshot`) take, so push and clear serialize
        // through one lock — no data race, no torn read.
        // Residual best-effort gap: a `pump_reader` thread that has
        // already returned from `reader.read(...)` with pre-resize
        // bytes in its userspace buffer but has not yet acquired the
        // bus lock will push those bytes AFTER this clear. The kernel
        // can also have pre-SIGWINCH-emit bytes buffered on the
        // master FD that `pump_reader` will read after the ioctl
        // returns. Neither can be closed without holding the bus lock
        // across a blocking `read()` (or coordinating with the inner
        // agent's SIGWINCH ack — neither tractable). The interactive
        // recovery path makes this acceptable: the inner TUI's
        // SIGWINCH-driven full-screen redraw emits a clear + reposition
        // + content burst at the new dims that overwrites the parser's
        // live screen within a frame, so any leaked pre-resize bytes
        // age out of the parser's live area into the (still-correct
        // at the wider dim case) parser-side scrollback. See the
        // Risks table in `prds/104-snapshot-replay-preserves-pty-dims.md`.
        agent.bus.clear_scrollback();
        Ok(())
    }

    /// Last-known PTY size for the agent attached to `pane_id_env`,
    /// captured at spawn and refreshed by [`resize`]. Returns `None`
    /// if no live agent matches the pane id. Used by tests; production
    /// callers don't need this.
    pub fn pty_size_for_pane(&self, pane_id_env: &str) -> Option<(u16, u16)> {
        let inner = self.inner.lock().unwrap();
        inner
            .agents
            .values()
            .find(|a| {
                a.pane_id_env.as_deref() == Some(pane_id_env) && !a.exited.load(Ordering::SeqCst)
            })
            .map(|a| (a.pty_rows, a.pty_cols))
    }

    /// Take just the current scrollback snapshot for an agent.
    pub fn snapshot(&self, id: &str) -> Result<Vec<u8>, AgentPtyError> {
        let inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        Ok(agent.bus.snapshot())
    }

    /// Current number of live broadcast subscribers for an agent. Returns
    /// `None` if the agent is not in the registry.
    pub fn receiver_count(&self, id: &str) -> Option<usize> {
        let inner = self.inner.lock().unwrap();
        inner.agents.get(id).map(|a| a.bus.receiver_count())
    }

    /// OS-level PID of the agent's child process, if exposed by the
    /// underlying PTY layer. Used by tests to verify actual process
    /// liveness (`kill(pid, 0)`) rather than just registry membership —
    /// catches regressions where the child is killed but the registry
    /// entry survives, or vice versa.
    pub fn child_pid(&self, id: &str) -> Option<u32> {
        let inner = self.inner.lock().unwrap();
        inner.agents.get(id).and_then(|a| a.child.process_id())
    }

    /// All currently-owned agent ids, sorted ascending.
    pub fn agent_ids(&self) -> Vec<String> {
        self.agent_records().into_iter().map(|r| r.id).collect()
    }

    /// All currently-owned *live* agents as `(id, pane_id_env)`
    /// records, sorted ascending by id. M2.x rehydration relies on the
    /// captured `pane_id_env` to rebind the TUI's local pane id to
    /// whatever value the agent's child process already carries in its
    /// environment — without this, hook events emitted by the agent
    /// would be silently dropped after a reconnect (see
    /// `RunningAgent::pane_id_env`).
    ///
    /// Round-11 reviewer #A: exited-but-not-reaped entries are
    /// filtered out. Hydration uses this to rebuild the TUI's pane
    /// set on reattach; surfacing a dead entry alongside a live
    /// reuse of the same pane_id_env would materialize a ghost
    /// pane on the dashboard or, worse, race the live entry for
    /// which one wins the local pane_id slot in `wire_stream_pane`.
    pub fn agent_records(&self) -> Vec<AgentRecord> {
        let inner = self.inner.lock().unwrap();
        let mut records: Vec<AgentRecord> = inner
            .agents
            .iter()
            .filter(|(_, agent)| !agent.exited.load(Ordering::SeqCst))
            .map(|(id, agent)| AgentRecord {
                id: id.clone(),
                pane_id_env: agent.pane_id_env.clone(),
                display_name: agent.display_name.clone(),
                cwd: agent.cwd.clone(),
                tab_membership: agent.tab_membership.clone(),
                agent_type: agent.agent_type.clone(),
                rows: agent.pty_rows,
                cols: agent.pty_cols,
                // PRD #162: the registry has no live session state; the
                // `ListAgents` handler joins `AppState.sessions` in and
                // overrides this when a matching live session exists.
                live: None,
            })
            .collect();
        records.sort_by_key(|r| r.id.parse::<u64>().unwrap_or(0));
        records
    }

    /// Update the per-agent display name and cwd captured in the registry
    /// (M2.11). Each value is validated independently — invalid display
    /// names are rejected and stored as `None`, invalid cwds likewise.
    /// Passing `None` clears the corresponding field. Returns
    /// [`AgentPtyError::NotFound`] if the agent id is unknown.
    pub fn set_agent_label(
        &self,
        id: &str,
        display_name: Option<String>,
        cwd: Option<String>,
    ) -> Result<(), AgentPtyError> {
        let display_name = display_name.and_then(|v| {
            if is_valid_display_name(&v) {
                Some(v)
            } else {
                tracing::debug!(
                    len = v.len(),
                    "set_agent_label: dropping display_name — fails validation"
                );
                None
            }
        });
        let cwd = cwd.and_then(|v| {
            if is_valid_cwd(&v) {
                Some(v)
            } else {
                tracing::debug!(
                    len = v.len(),
                    "set_agent_label: dropping cwd — fails validation"
                );
                None
            }
        });
        let mut inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get_mut(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        agent.display_name = display_name;
        agent.cwd = cwd;
        Ok(())
    }

    /// Persist the agent type the daemon *learned from a hook event* into the
    /// registry, keyed by `pane_id_env` (hook events carry the originating
    /// pane via `DOT_AGENT_DECK_PANE_ID`, which is exactly what
    /// [`RunningAgent::pane_id_env`] holds).
    ///
    /// The spawn-time [`AgentType::from_command`] guess (stored at
    /// [`AgentPtyRegistry::spawn_agent`]) is `None` for the common
    /// interactive flow — the daemon spawns a shell and the user launches
    /// `claude` / `opencode` *inside* it, so the command the daemon saw was
    /// the shell. Without this write-back the registry — and therefore
    /// [`AgentPtyRegistry::agent_records`] / the `list_agents` reply — keeps
    /// reporting `AgentType::None` ("No agent") on a fresh `dot-agent-deck
    /// connect`, until the agent happens to emit its next hook. The daemon's
    /// hook-ingestion loop calls this so the real type, once observed, lands
    /// in the source of truth and survives a TUI reconnect.
    ///
    /// Upgrade-only: ignores `AgentType::None` and never overwrites an
    /// already-known type, mirroring the strict `None` → `Some` upgrade in
    /// [`crate::state::AppState::apply_event`]. A no-op when no live agent
    /// matches `pane_id_env` (unmanaged / external pane id, or empty id).
    pub fn set_agent_type(&self, pane_id_env: &str, agent_type: &AgentType) {
        if *agent_type == AgentType::None || pane_id_env.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(agent) = inner
            .agents
            .values_mut()
            .find(|a| a.pane_id_env.as_deref() == Some(pane_id_env))
            && agent.agent_type.is_none()
        {
            agent.agent_type = Some(agent_type.clone());
        }
    }

    /// PRD #201 native prompt delivery: stash a seed/prompt for the pane whose
    /// `DOT_AGENT_DECK_PANE_ID` matches `pane_id_env`, to be pulled NATIVELY by
    /// the agent's extension via `dot-agent-deck get-seed` (→
    /// `pi.sendUserMessage`). Overwrites any previous unconsumed seed (the
    /// freshest seed wins) and resets the native-delivered flag. No-op when the
    /// pane is unknown or the seed is blank. Keyed by `pane_id_env` (linear
    /// scan) like [`AgentPtyRegistry::set_agent_type`].
    pub fn set_pending_seed(&self, pane_id_env: &str, seed: &str) {
        if pane_id_env.is_empty() || seed.trim().is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(agent) = inner
            .agents
            .values_mut()
            .find(|a| a.pane_id_env.as_deref() == Some(pane_id_env))
        {
            agent.pending_seed = Some(seed.to_string());
            agent.seed_delivered_native = false;
        }
    }

    /// PRD #201: take (clear) the pending seed for `pane_id_env` on behalf of
    /// the NATIVE `get-seed` pull. Marks the seed as delivered natively so a
    /// test can prove the native path ran. Returns `None` when the pane is
    /// unknown or has no pending seed (already delivered, or never set). The
    /// take is atomic under the registry lock, so a race with the fallback
    /// path can only let one of them win.
    pub fn take_pending_seed_native(&self, pane_id_env: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .values_mut()
            .find(|a| a.pane_id_env.as_deref() == Some(pane_id_env))?;
        let seed = agent.pending_seed.take()?;
        agent.seed_delivered_native = true;
        Some(seed)
    }

    /// PRD #201: take (clear) the pending seed for `pane_id_env` on behalf of
    /// the daemon's PTY-injection SAFETY NET. Returns `Some` only if the seed
    /// was NOT already consumed by the native pull — so the fallback injects
    /// exactly when (and only when) native delivery did not happen.
    pub fn take_pending_seed_fallback(&self, pane_id_env: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .values_mut()
            .find(|a| a.pane_id_env.as_deref() == Some(pane_id_env))?;
        agent.pending_seed.take()
    }

    /// PRD #201: whether this pane's seed was delivered via the NATIVE
    /// `get-seed` pull (vs. the PTY-injection fallback, or not yet delivered).
    /// Test observable that distinguishes native delivery from the safety net.
    pub fn seed_delivered_native(&self, pane_id_env: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .agents
            .values()
            .find(|a| a.pane_id_env.as_deref() == Some(pane_id_env))
            .map(|a| a.seed_delivered_native)
            .unwrap_or(false)
    }

    /// Number of agents currently owned by the registry.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().agents.is_empty()
    }

    /// PRD #93 round-2 reviewer REV-3: count of *live* (non-exited) agents.
    /// The daemon's idle monitor uses this instead of [`len`] so an agent
    /// whose child died but whose registry entry is still around (no
    /// `close_agent` yet) doesn't pin the daemon up past its idle window.
    /// An exited entry is reaped only when something else (an explicit
    /// `close_agent`, a `shutdown_all`, or the daemon's drop) removes it
    /// — `live_count` is the gate, not the cleanup.
    pub fn live_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .agents
            .values()
            .filter(|a| !a.exited.load(Ordering::SeqCst))
            .count()
    }

    /// PRD #92 F1 followup: true once the registry has entered its
    /// shutdown path (`shutdown_all_graceful` flipped the latch).
    /// Consulted by `AttachRequest::StartAgent` in `daemon_protocol.rs`
    /// to refuse new agent spawns while the daemon is tearing down,
    /// closing the race window between an in-flight `StartAgent` and a
    /// `KIND_SHUTDOWN` arrival.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// SIGKILL every agent and drain the registry. Idempotent.
    pub fn shutdown_all(&self) {
        let agents: Vec<RunningAgent> = {
            let mut inner = self.inner.lock().unwrap();
            inner.agents.drain().map(|(_, a)| a).collect()
        };
        for mut agent in agents {
            crate::platform::proc::force_kill_child_and_wait(&mut agent.child);
        }
        // Wake the idle monitor if it's parked on `change_notify` — the
        // registry just emptied, so the next gate check should see
        // live_count == 0.
        self.change_notify.notify_one();
    }

    /// PRD #92 F1: graceful shutdown of every agent in the registry. Sends
    /// SIGTERM to each child, waits up to `grace` for them to exit (polling
    /// `try_wait` so an early exiter isn't penalised by the wall-clock
    /// deadline), then SIGKILLs anything that's still alive. Idempotent —
    /// a second call (e.g. from a second `KIND_SHUTDOWN` arriving during
    /// teardown, or from a SIGTERM-triggered drop path racing the protocol
    /// handler) returns immediately so we don't fight ourselves for
    /// ownership of each `Child`.
    ///
    /// The Drop impl still calls [`shutdown_all`] for the SIGKILL-without-grace
    /// path — that path is reached on idle shutdown and test cleanup where
    /// the grace period is unnecessary. F1's graceful path is invoked
    /// explicitly via the `KIND_SHUTDOWN` handler.
    pub fn shutdown_all_graceful(&self, grace: Duration) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            // Already shutting down — second-signal idempotency.
            return;
        }
        let mut agents: Vec<RunningAgent> = {
            let mut inner = self.inner.lock().unwrap();
            inner.agents.drain().map(|(_, a)| a).collect()
        };

        // Phase 1: SIGTERM each child's process group. Some shells
        // (notably the bash/zsh configurations that intercept SIGHUP)
        // honour SIGTERM as a clean shutdown signal, so this gives the
        // agent a chance to flush state. We use `killpg` rather than
        // `kill` so descendants of shell-wrapped commands (the actual
        // agent plus anything it spawned) get the signal too — see the
        // PRD #92 F5 rationale on `force_kill_child_and_wait`.
        //
        // PRD #92 F8: the killpg logic + `pid_to_pgid` boundary check is
        // shared with the single-pane Ctrl+W path via
        // `crate::platform::proc` (PRD #42 M1), so the two paths can't
        // drift on what counts as a valid pgid or how a failed killpg
        // is logged.
        for agent in &mut agents {
            crate::platform::proc::send_sigterm_to_child_group(
                &mut agent.child,
                "shutdown-all-graceful-sigterm",
            );
        }

        // Phase 2: poll each child's `try_wait` until all have exited or
        // the grace window elapses. Polling avoids the obvious "sleep for
        // grace then SIGKILL" alternative — agents that exit promptly
        // don't have to wait around for the slowest sibling.
        let deadline = std::time::Instant::now() + grace;
        loop {
            let all_exited = agents
                .iter_mut()
                .all(|a| matches!(a.child.try_wait(), Ok(Some(_))));
            if all_exited {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Phase 3: SIGKILL any survivor and reap. `force_kill_child_and_wait`
        // is no-op-safe on an already-exited child (ESRCH is logged-but-
        // ignored and `wait` returns the cached status), so this loop is
        // safe to run unconditionally.
        for mut agent in agents {
            crate::platform::proc::force_kill_child_and_wait(&mut agent.child);
        }

        self.change_notify.notify_one();
    }
}

impl Drop for AgentPtyRegistry {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PRD #42 M1: the `pid_to_pgid` boundary-check unit tests moved with the
    // function to `crate::platform::proc` (see `src/platform/proc/unix.rs`).

    // PRD #76 M2.11 fixup 4 — pin the canonical name resolver so the UI
    // helper, the controller's new-pane path, and the rename path all
    // converge on the same rules. Regressions here would resurrect the
    // fixup-3 reviewer P2 / auditor LOW divergence between
    // `ui.pane_display_names` and `AgentRecord.display_name`.

    #[test]
    fn resolve_display_name_prefers_trimmed_form_name() {
        assert_eq!(
            resolve_display_name(Some("  foo  "), Some("vim")),
            "foo",
            "surrounding whitespace must be stripped from a valid form name"
        );
        assert_eq!(
            resolve_display_name(Some("agent-1"), Some("vim")),
            "agent-1"
        );
    }

    #[test]
    fn resolve_display_name_whitespace_only_form_falls_through_to_command() {
        assert_eq!(resolve_display_name(Some("   "), Some("vim")), "vim");
        assert_eq!(resolve_display_name(Some(""), Some("htop")), "htop");
        assert_eq!(resolve_display_name(Some("\t  \n"), Some("ls")), "ls");
    }

    #[test]
    fn resolve_display_name_no_inputs_falls_back_to_shell() {
        assert_eq!(resolve_display_name(None, None), "shell");
        assert_eq!(resolve_display_name(Some("   "), None), "shell");
        assert_eq!(resolve_display_name(None, Some("   ")), "shell");
    }

    #[test]
    fn resolve_display_name_rejects_control_char_form_name() {
        // Form Name with ANSI ESC must fail `is_valid_display_name` and
        // fall through to the command — the daemon would drop the same
        // string, so the UI map must never store it.
        assert_eq!(
            resolve_display_name(Some("\x1b[31mevil"), Some("vim")),
            "vim",
            "control-byte form name must fall through to command"
        );
    }

    #[test]
    fn resolve_display_name_rejects_control_char_command_falls_to_shell() {
        // Command with real ESC byte (the auditor LOW case): form Name
        // empty so we fall through to command, which fails validation,
        // so the final fallback "shell" wins.
        let evil_cmd = "echo \x1b[31m";
        assert_eq!(
            resolve_display_name(Some(""), Some(evil_cmd)),
            "shell",
            "control-byte command must fall through to shell, not be stored verbatim"
        );
        assert_eq!(resolve_display_name(None, Some(evil_cmd)), "shell");
    }

    /// Round-12 auditor #2: orchestration_cwd must be validated.
    /// Hostile inputs (NUL bytes, control chars, oversized strings,
    /// relative paths) should make validate_tab_membership return
    /// None so spawn_agent surfaces an `AgentPtyError::Validation`
    /// instead of echoing the bad bytes back via agent_records.
    #[test]
    fn validate_tab_membership_rejects_orchestration_cwd_with_nul_byte() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some("/proj/\0evil".into()),
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_rejects_orchestration_cwd_with_control_char() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some("/proj/\x1b[31m".into()),
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_rejects_relative_orchestration_cwd() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            // Not absolute — orchestration_cwd is the project root,
            // relative paths would either fail filesystem ops later
            // or quietly collide with other orchs whose resolved
            // cwd happens to match.
            orchestration_cwd: Some("relative/proj".into()),
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_rejects_oversized_orchestration_cwd() {
        let oversized = "/".to_string() + &"a".repeat(CWD_MAX_LEN);
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some(oversized),
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_accepts_well_formed_orchestration_cwd() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some("/home/user/project-a".into()),
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_some());
    }

    // PRD #111 auditor BLOCKER: a hostile / buggy daemon sending an
    // absurd role_index would push the TUI synthesis path into an OOM
    // allocation. Reject at the wire boundary so every downstream
    // consumer is protected.
    #[test]
    fn validate_tab_membership_rejects_oversized_role_index() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: ORCHESTRATION_ROLE_INDEX_MAX + 1,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_accepts_role_index_at_ceiling() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: ORCHESTRATION_ROLE_INDEX_MAX,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_some());
    }

    // PRD #111 auditor suggestion: role_name flows to tab labels, so
    // ANSI / control bytes must be rejected the same way display_name
    // is. Empty role_name stays accepted — it's the older-daemon
    // wire shape, handled by the synthesis placeholder fallback.
    #[test]
    fn validate_tab_membership_rejects_role_name_with_ansi_escape() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "\x1b[31mpwn".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_rejects_role_name_with_nul_byte() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "co\0der".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    // Greptile PR #160 P1: display_title flows to the tab label like
    // name/role_name, so a control-byte value must be neutralised. Unlike
    // those identity fields it's cosmetic with a `None` fallback, so an
    // invalid value is nulled out (membership preserved) rather than
    // rejecting the whole membership and stranding the orchestration tab.
    #[test]
    fn validate_tab_membership_nulls_out_display_title_with_ansi_escape() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: Some("\x1b[31mpwn".into()),
        };
        let validated = validate_tab_membership(tm).expect("membership preserved");
        match validated {
            TabMembership::Orchestration { display_title, .. } => {
                assert_eq!(display_title, None, "invalid display_title nulled out");
            }
            _ => panic!("expected Orchestration variant"),
        }
    }

    #[test]
    fn validate_tab_membership_nulls_out_display_title_with_nul_byte() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: Some("My\0Run".into()),
        };
        let validated = validate_tab_membership(tm).expect("membership preserved");
        match validated {
            TabMembership::Orchestration { display_title, .. } => {
                assert_eq!(display_title, None);
            }
            _ => panic!("expected Orchestration variant"),
        }
    }

    #[test]
    fn validate_tab_membership_preserves_well_formed_display_title() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: Some("My Custom Run".into()),
        };
        let validated = validate_tab_membership(tm).expect("membership preserved");
        match validated {
            TabMembership::Orchestration { display_title, .. } => {
                assert_eq!(display_title.as_deref(), Some("My Custom Run"));
            }
            _ => panic!("expected Orchestration variant"),
        }
    }

    #[test]
    fn validate_tab_membership_accepts_empty_role_name() {
        // Older daemons predating the inline role_name field omit it,
        // so #[serde(default)] produces an empty string. Synthesis
        // falls back to `role-{i}`; validation must let it through.
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: String::new(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
        };
        assert!(validate_tab_membership(tm).is_some());
    }
}

// PRD #42 M8/review B1: these tests spawn real PTYs running `/bin/sh` / `sh -c`
// (and kill agents via `libc`), none of which exist on Windows. Gate the whole
// block to Unix so the Windows `cargo nextest run` step compiles and does not
// panic. The pure-logic tests above (`resolve_display_name_*`,
// `validate_tab_membership_*`) stay cross-platform. No Unix coverage is lost —
// every test here still runs on Unix.
#[cfg(all(test, unix))]
mod spawn_tests {
    use super::*;
    use crate::event::OrchestrationSurfaceRole;
    use std::time::Duration;

    // ---------------------------------------------------------------------
    // PRD #120 — validate_orchestration_surface (the live-surface wire path
    // analogue of validate_tab_membership). H1/M1/L2 coverage.
    // ---------------------------------------------------------------------

    fn surface_role(role_index: usize, role_name: &str) -> OrchestrationSurfaceRole {
        OrchestrationSurfaceRole {
            pane_id: format!("pane-{role_index}"),
            role_index,
            role_name: role_name.into(),
            is_start_role: role_index == 0,
        }
    }

    fn well_formed_surface() -> OrchestrationSurface {
        OrchestrationSurface {
            name: "issue-work".into(),
            cwd: "/work/github-issues/.worktrees/issue-1".into(),
            display_title: None,
            roles: vec![surface_role(0, "orchestrator"), surface_role(1, "worker")],
        }
    }

    #[test]
    fn validate_orchestration_surface_accepts_well_formed() {
        assert!(validate_orchestration_surface(well_formed_surface()).is_some());
    }

    // H1: a role_index over the OOM cap must not reach synthesis (which would
    // size a `max_index + 1` placeholder vec). The offending role is dropped;
    // the surviving roles still build the tab.
    #[test]
    fn validate_orchestration_surface_drops_role_over_index_cap() {
        let mut surface = well_formed_surface();
        surface
            .roles
            .push(surface_role(ORCHESTRATION_ROLE_INDEX_MAX + 1, "rogue"));
        // A pathological 1e9 index — the OOM the cap exists to prevent.
        surface.roles.push(surface_role(1_000_000_000, "oom"));
        let validated =
            validate_orchestration_surface(surface).expect("valid roles survive the drop");
        assert_eq!(validated.roles.len(), 2, "over-cap roles dropped");
        assert!(
            validated
                .roles
                .iter()
                .all(|r| r.role_index <= ORCHESTRATION_ROLE_INDEX_MAX)
        );
    }

    #[test]
    fn validate_orchestration_surface_accepts_role_index_at_ceiling() {
        let mut surface = well_formed_surface();
        surface
            .roles
            .push(surface_role(ORCHESTRATION_ROLE_INDEX_MAX, "edge"));
        let validated = validate_orchestration_surface(surface).expect("ceiling index accepted");
        assert_eq!(validated.roles.len(), 3);
    }

    // If EVERY role is over the cap the surface can only build a dead tab, so
    // it's rejected outright.
    #[test]
    fn validate_orchestration_surface_rejects_when_all_roles_over_cap() {
        let surface = OrchestrationSurface {
            name: "issue-work".into(),
            cwd: "/work/issue-1".into(),
            display_title: None,
            roles: vec![surface_role(ORCHESTRATION_ROLE_INDEX_MAX + 1, "rogue")],
        };
        assert!(validate_orchestration_surface(surface).is_none());
    }

    #[test]
    fn validate_orchestration_surface_rejects_empty_roles() {
        let mut surface = well_formed_surface();
        surface.roles.clear();
        assert!(validate_orchestration_surface(surface).is_none());
    }

    // M1: name feeds the tab label and is the bucket identity — a control-byte
    // value rejects the whole surface (no safe fallback for an identity).
    #[test]
    fn validate_orchestration_surface_rejects_name_with_ansi_escape() {
        let mut surface = well_formed_surface();
        surface.name = "\x1b[31mpwn".into();
        assert!(validate_orchestration_surface(surface).is_none());
    }

    #[test]
    fn validate_orchestration_surface_rejects_name_with_nul_byte() {
        let mut surface = well_formed_surface();
        surface.name = "iss\0ue".into();
        assert!(validate_orchestration_surface(surface).is_none());
    }

    // L2: cwd drives load_project_config and keys the bucket — control bytes,
    // NUL, oversized, and relative paths are all rejected.
    #[test]
    fn validate_orchestration_surface_rejects_cwd_with_control_char() {
        let mut surface = well_formed_surface();
        surface.cwd = "/work/\x1b[31mevil".into();
        assert!(validate_orchestration_surface(surface).is_none());
    }

    #[test]
    fn validate_orchestration_surface_rejects_cwd_with_nul_byte() {
        let mut surface = well_formed_surface();
        surface.cwd = "/work/\0evil".into();
        assert!(validate_orchestration_surface(surface).is_none());
    }

    #[test]
    fn validate_orchestration_surface_rejects_relative_cwd() {
        let mut surface = well_formed_surface();
        surface.cwd = "relative/work".into();
        assert!(validate_orchestration_surface(surface).is_none());
    }

    #[test]
    fn validate_orchestration_surface_rejects_oversized_cwd() {
        let mut surface = well_formed_surface();
        surface.cwd = "/".to_string() + &"a".repeat(CWD_MAX_LEN);
        assert!(validate_orchestration_surface(surface).is_none());
    }

    // M1: role_name flows to the role card/label like name does — drop a role
    // whose non-empty role_name smuggles control bytes.
    #[test]
    fn validate_orchestration_surface_drops_role_with_ansi_role_name() {
        let mut surface = well_formed_surface();
        surface.roles.push(surface_role(2, "\x1b[31mpwn"));
        let validated = validate_orchestration_surface(surface).expect("clean roles survive");
        assert_eq!(validated.roles.len(), 2);
        assert!(validated.roles.iter().all(|r| r.role_name != "\x1b[31mpwn"));
    }

    // An empty role_name is the older-daemon wire shape — synthesis falls back
    // to a `role-{i}` placeholder, so it must NOT be dropped.
    #[test]
    fn validate_orchestration_surface_keeps_role_with_empty_role_name() {
        let surface = OrchestrationSurface {
            name: "issue-work".into(),
            cwd: "/work/issue-1".into(),
            display_title: None,
            roles: vec![surface_role(0, "")],
        };
        let validated = validate_orchestration_surface(surface).expect("empty role_name accepted");
        assert_eq!(validated.roles.len(), 1);
    }

    // M1: display_title is cosmetic with a defined `None` fallback (→ name), so
    // an invalid value is nulled out — the surface is preserved.
    #[test]
    fn validate_orchestration_surface_nulls_out_display_title_with_control_bytes() {
        let mut surface = well_formed_surface();
        surface.display_title = Some("\x1b[31mpwn".into());
        let validated = validate_orchestration_surface(surface).expect("surface preserved");
        assert_eq!(
            validated.display_title, None,
            "invalid display_title nulled out, not rejected"
        );
    }

    #[test]
    fn validate_orchestration_surface_preserves_well_formed_display_title() {
        let mut surface = well_formed_surface();
        surface.display_title = Some("issue-work · issue-1".into());
        let validated = validate_orchestration_surface(surface).expect("surface preserved");
        assert_eq!(
            validated.display_title.as_deref(),
            Some("issue-work · issue-1")
        );
    }

    #[test]
    fn spawn_default_shell_works() {
        let pty = spawn(SpawnOptions::default()).expect("spawn should succeed");
        let mut child = pty.child;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn spawn_rejects_zero_rows() {
        let Err(err) = spawn(SpawnOptions {
            rows: 0,
            cols: 80,
            ..SpawnOptions::default()
        }) else {
            panic!("spawn must reject rows=0");
        };
        assert!(
            matches!(err, AgentPtyError::Validation(_)),
            "expected Validation, got {err:?}"
        );
    }

    #[test]
    fn spawn_rejects_zero_cols() {
        let Err(err) = spawn(SpawnOptions {
            rows: 24,
            cols: 0,
            ..SpawnOptions::default()
        }) else {
            panic!("spawn must reject cols=0");
        };
        assert!(
            matches!(err, AgentPtyError::Validation(_)),
            "expected Validation, got {err:?}"
        );
    }

    #[test]
    fn spawn_clamps_oversized_rows() {
        let pty = spawn(SpawnOptions {
            rows: u16::MAX,
            cols: 80,
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed when rows are oversized — they must clamp");
        let size = pty.master.get_size().expect("get_size should succeed");
        assert_eq!(
            size.rows, PTY_RESIZE_DIM_MAX,
            "rows must be clamped to PTY_RESIZE_DIM_MAX, not u16::MAX"
        );
        let mut child = pty.child;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn spawn_clamps_oversized_cols() {
        let pty = spawn(SpawnOptions {
            rows: 24,
            cols: u16::MAX,
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed when cols are oversized — they must clamp");
        let size = pty.master.get_size().expect("get_size should succeed");
        assert_eq!(
            size.cols, PTY_RESIZE_DIM_MAX,
            "cols must be clamped to PTY_RESIZE_DIM_MAX, not u16::MAX"
        );
        let mut child = pty.child;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn registry_spawn_and_close() {
        let registry = AgentPtyRegistry::new();
        assert!(registry.is_empty());

        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        assert_eq!(registry.len(), 1);
        assert_eq!(registry.agent_ids(), vec![id.clone()]);

        registry.close_agent(&id).expect("close should succeed");
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_resize_rejects_zero_dims() {
        let registry = AgentPtyRegistry::new();
        let id = registry.spawn_agent(SpawnOptions::default()).unwrap();
        for (rows, cols) in [(0u16, 80u16), (24u16, 0u16), (0u16, 0u16)] {
            let err = registry.resize(&id, rows, cols).unwrap_err();
            assert!(matches!(err, AgentPtyError::Resize(_)));
        }
        registry.shutdown_all();
    }

    #[test]
    fn registry_resize_unknown_errors() {
        let registry = AgentPtyRegistry::new();
        let err = registry.resize("nope", 50, 200).unwrap_err();
        assert!(matches!(err, AgentPtyError::NotFound(_)));
    }

    #[test]
    fn registry_resize_succeeds_on_known_agent() {
        // Verifying the resulting kernel-level size requires a child that
        // reads TIOCGWINSZ — the integration test in tests/daemon_protocol.rs
        // covers that. Here we just confirm the method returns Ok for a
        // valid id and non-zero dims, i.e. the portable_pty resize ioctl
        // didn't error.
        let registry = AgentPtyRegistry::new();
        let id = registry.spawn_agent(SpawnOptions::default()).unwrap();
        registry
            .resize(&id, 50, 200)
            .expect("resize should succeed");
        registry.shutdown_all();
    }

    #[test]
    fn registry_rejects_duplicate_pane_id_env() {
        // CodeRabbit MAJOR (PRD #93 round-9): two agents must never
        // share a `pane_id_env`. `write_to_pane_and_submit` keys off
        // that string, so a second spawn with the same id would silently misroute
        // every subsequent delegate/work-done write to whichever entry
        // `values().find(...)` happened to hand back first.
        let registry = AgentPtyRegistry::new();
        let id1 = registry
            .spawn_agent(SpawnOptions {
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-x".to_string())],
                ..SpawnOptions::default()
            })
            .expect("first spawn should succeed");

        let err = registry
            .spawn_agent(SpawnOptions {
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-x".to_string())],
                ..SpawnOptions::default()
            })
            .expect_err("duplicate pane_id_env spawn must fail");
        match err {
            AgentPtyError::DuplicatePaneId(p) => assert_eq!(p, "pane-x"),
            other => panic!("expected DuplicatePaneId, got {other:?}"),
        }

        // Registry must still have exactly one agent — the rejection
        // can't have leaked the spawned child.
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.agent_ids(), vec![id1]);
        registry.shutdown_all();
    }

    #[test]
    fn set_agent_type_learns_from_event_and_is_upgrade_only() {
        // The "No agent on reconnect" fix: the common interactive flow spawns
        // a shell (so `from_command` → `None`), and the real type only ever
        // arrives via a hook event. `set_agent_type` must land that type in
        // the registry so `agent_records` / `list_agents` reports it on a
        // fresh `connect` — but it must never overwrite a known type or
        // downgrade to `None`, matching `apply_event`'s strict upgrade.
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-x".to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        // Spawn-time guess is None — this is the "No agent" state.
        let type_of = |r: &AgentPtyRegistry| r.agent_records()[0].agent_type.clone();
        assert_eq!(type_of(&registry), None, "shell spawn starts as None");

        // A hook reveals the real type → registry upgrades None → Some.
        registry.set_agent_type("pane-x", &AgentType::ClaudeCode);
        assert_eq!(type_of(&registry), Some(AgentType::ClaudeCode));

        // None never downgrades a known type.
        registry.set_agent_type("pane-x", &AgentType::None);
        assert_eq!(type_of(&registry), Some(AgentType::ClaudeCode));

        // A different concrete type never overwrites an already-known one.
        registry.set_agent_type("pane-x", &AgentType::OpenCode);
        assert_eq!(type_of(&registry), Some(AgentType::ClaudeCode));

        // Unknown / absent pane id is a harmless no-op (events from
        // unmanaged panes must not panic or touch another agent).
        registry.set_agent_type("pane-unknown", &AgentType::OpenCode);
        registry.set_agent_type("", &AgentType::OpenCode);
        assert_eq!(type_of(&registry), Some(AgentType::ClaudeCode));
        assert_eq!(registry.len(), 1);

        let _ = id;
        registry.shutdown_all();
    }

    /// PRD #201 native prompt delivery: the daemon-side seed store. A seed set
    /// for a pane is pullable exactly once; whichever taker (the native
    /// `get-seed` pull or the PTY-injection fallback) runs first delivers, and
    /// the native path is observably distinguished from the fallback.
    #[test]
    fn pending_seed_set_take_and_native_flag() {
        let registry = AgentPtyRegistry::new();
        let _id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-seed".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        // No seed set yet → both takers return None, and native is false.
        assert_eq!(registry.take_pending_seed_native("pane-seed"), None);
        assert!(!registry.seed_delivered_native("pane-seed"));

        // Set a seed, then the NATIVE pull takes it and marks it native.
        registry.set_pending_seed(
            "pane-seed",
            "Read .dot-agent-deck/worker-task-coder.md for your task.",
        );
        assert!(
            !registry.seed_delivered_native("pane-seed"),
            "not delivered until pulled"
        );
        assert_eq!(
            registry.take_pending_seed_native("pane-seed").as_deref(),
            Some("Read .dot-agent-deck/worker-task-coder.md for your task."),
        );
        assert!(
            registry.seed_delivered_native("pane-seed"),
            "native pull marks the flag"
        );
        // Cleared after one take — a second pull (or the fallback) gets nothing.
        assert_eq!(registry.take_pending_seed_native("pane-seed"), None);
        assert_eq!(registry.take_pending_seed_fallback("pane-seed"), None);

        // The FALLBACK take delivers when native did NOT run, and does NOT set
        // the native flag (so a test can tell native from the safety net).
        registry.set_pending_seed("pane-seed", "kickoff");
        assert_eq!(
            registry.take_pending_seed_fallback("pane-seed").as_deref(),
            Some("kickoff")
        );
        assert!(
            !registry.seed_delivered_native("pane-seed"),
            "fallback delivery must NOT be reported as native"
        );
        assert_eq!(registry.take_pending_seed_native("pane-seed"), None);

        // Exactly-once arbitration: once the fallback wins, the native pull
        // gets nothing (and vice-versa) — the two can never both deliver.
        registry.set_pending_seed("pane-seed", "second");
        assert_eq!(
            registry.take_pending_seed_fallback("pane-seed").as_deref(),
            Some("second")
        );
        assert_eq!(registry.take_pending_seed_native("pane-seed"), None);

        // Setting a fresh seed overwrites an unconsumed one and resets the flag.
        registry.set_pending_seed("pane-seed", "first");
        registry.set_pending_seed("pane-seed", "freshest");
        assert_eq!(
            registry.take_pending_seed_native("pane-seed").as_deref(),
            Some("freshest")
        );

        // Blank seeds are ignored; unknown panes are harmless no-ops.
        registry.set_pending_seed("pane-seed", "   ");
        assert_eq!(registry.take_pending_seed_native("pane-seed"), None);
        registry.set_pending_seed("pane-unknown", "orphan");
        assert_eq!(registry.take_pending_seed_native("pane-unknown"), None);
        assert!(!registry.seed_delivered_native("pane-unknown"));

        registry.shutdown_all();
    }

    #[tokio::test]
    async fn registry_allows_pane_id_reuse_when_prior_agent_has_exited() {
        // Round-10 auditor #3: the duplicate-pane-id check must mirror
        // `live_count`'s contract — a dead-but-not-yet-reaped registry
        // entry doesn't block reuse of its `pane_id_env`. Without the
        // `!exited.load(...)` filter, a previously-crashed worker's
        // entry would hold its pane id hostage until something else
        // explicitly removed it.
        let registry = Arc::new(AgentPtyRegistry::new());
        let id1 = registry
            .spawn_agent(SpawnOptions {
                command: Some("/usr/bin/true"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "pane-recycle".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("first spawn should succeed");

        // Wait for the reader thread to observe EOF and set `exited`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if registry.live_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            registry.live_count(),
            0,
            "test prerequisite: /usr/bin/true must have exited"
        );
        assert_eq!(registry.len(), 1, "exited entry must still be in registry");

        // Now: reuse the same pane_id_env. The exited agent shouldn't
        // block this — only a live agent would.
        let id2 = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "pane-recycle".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("reuse of an exited agent's pane_id_env must succeed");
        assert_ne!(id1, id2);

        registry.shutdown_all();
    }

    #[tokio::test]
    async fn agent_records_filters_exited_entries() {
        // Round-11 reviewer #A: agent_records is the hydration source.
        // An exited-but-not-reaped entry must not show up — the TUI
        // would otherwise materialize a ghost pane for a dead agent
        // (or race a fresh agent that reused the same pane_id_env).
        let registry = Arc::new(AgentPtyRegistry::new());
        let _id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/usr/bin/true"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "ghost".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn /usr/bin/true");

        // Wait for `exited` to flip.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if registry.live_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(registry.live_count(), 0);
        assert_eq!(
            registry.len(),
            1,
            "exited entry still present in agents map"
        );
        assert!(
            registry.agent_records().is_empty(),
            "agent_records must drop exited entries so hydration doesn't materialize ghost panes"
        );

        registry.shutdown_all();
    }

    /// PRD #20 R20-008: `subscribe` captures the target's identity
    /// (`agent_id`, `pane_id_env`) and liveness token (`exited`) ATOMICALLY with
    /// the writer, under the single registry lock. This is what lets the attach
    /// handler stop using the racy post-lock `pane_id_env_for_agent` lookup that
    /// could resolve to the `<agent-gone>` sentinel after a concurrent removal —
    /// and `pane_writable("<agent-gone>")` defaults to `Live`, so a teardown-time
    /// frame could still reach the dead writer.
    ///
    /// Proves the fix deterministically: the captured `pane_id_env` is a REAL
    /// value that is unaffected by removing the entry afterward (no post-removal
    /// lookup is needed), and the shared `exited` token the handler now checks
    /// before every write flips `true` once the killed child's PTY EOFs.
    #[tokio::test]
    async fn subscribe_captures_target_identity_atomically() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "pane-attach-handle".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        let handle = registry.subscribe(&id).expect("subscribe to live agent");
        assert_eq!(handle.agent_id, id, "handle carries the exact agent id");
        assert_eq!(
            handle.pane_id_env.as_deref(),
            Some("pane-attach-handle"),
            "handle captures the real pane_id_env — never the <agent-gone> sentinel"
        );
        assert!(
            !handle.exited.load(Ordering::SeqCst),
            "a freshly-attached live agent is not exited"
        );

        // Remove the entry (kills + reaps the child) — the crux race window.
        registry.close_agent(&id).expect("close the agent");

        // The captured identity is IMMUNE to the removal: the handler no longer
        // needs a post-removal lookup that would default the pane to
        // `<agent-gone>` (and thus `pane_writable` → Live).
        assert_eq!(
            handle.pane_id_env.as_deref(),
            Some("pane-attach-handle"),
            "captured pane_id_env must survive removal, not degrade to a sentinel"
        );
        assert!(
            registry.subscribe(&id).is_err(),
            "the entry is gone after close, so a fresh subscribe fails"
        );

        // The shared `exited` token the input path now checks before every write
        // flips to true once the killed child's PTY EOFs — so a teardown-time
        // frame arriving on the cached writer is rejected.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if handle.exited.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            handle.exited.load(Ordering::SeqCst),
            "the captured exited token must flip true once the target dies, so the \
             input path rejects a frame to the dead writer"
        );

        registry.shutdown_all();
    }

    // PRD #127 C3 — `pane_is_live` reports liveness for the SPECIFIC pane
    // (by pane_id_env), so the reuse path never re-delivers into a dead pane.
    #[tokio::test]
    async fn pane_is_live_tracks_specific_pane() {
        let registry = Arc::new(AgentPtyRegistry::new());
        // A short-lived agent whose pane we'll watch flip dead.
        registry
            .spawn_agent(SpawnOptions {
                command: Some("/usr/bin/true"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "watch-me".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn /usr/bin/true");

        // Unknown pane is never live.
        assert!(!registry.pane_is_live("no-such-pane"));

        // Wait for it to exit, then the specific pane must read as not-live.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if !registry.pane_is_live("watch-me") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !registry.pane_is_live("watch-me"),
            "an exited pane must report as not-live so reuse spawns fresh"
        );

        registry.shutdown_all();
    }

    #[tokio::test]
    async fn write_to_pane_and_submit_skips_exited_agent_and_routes_to_live_reuser() {
        // Round-11 reviewer #A: the symmetric guard for the spawn-side
        // exited filter added in round 10. Without filtering on the
        // WRITE side, `write_to_pane_and_submit(pane_id_env=X)` could
        // still find the dead entry first and route delegate/work-done
        // bytes into a closed PTY whose pump thread already saw EOF.
        let registry = Arc::new(AgentPtyRegistry::new());
        let _dead = registry
            .spawn_agent(SpawnOptions {
                command: Some("/usr/bin/true"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "reuse-me".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn dead agent");

        // Wait for the dead agent's reader to see EOF and flip `exited`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if registry.live_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(registry.live_count(), 0, "dead agent must have exited");

        // Reuse the same pane_id_env for a fresh agent. `/bin/sh` will
        // stay alive long enough to receive a write.
        let live_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "reuse-me".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn live agent reusing the pane_id_env");
        assert_eq!(registry.live_count(), 1);

        // Take a snapshot before the write so we can detect bytes
        // arriving on the live agent's scrollback specifically.
        let before = registry.snapshot(&live_id).unwrap();

        // Operational write must route to the live agent, not the
        // dead one. We can't easily prove "dead agent received
        // nothing" because its writer is gone — but we CAN prove the
        // live one did receive something. The dead agent's writer
        // would error out anyway, so a misroute would surface as Err.
        registry
            .write_to_pane_and_submit("reuse-me", "echo round11-routing-marker")
            .await
            .expect("write_to_pane_and_submit to a live reuser must succeed");

        // Allow the PTY to echo the input back into scrollback.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut found = false;
        while tokio::time::Instant::now() < deadline {
            let snap = registry.snapshot(&live_id).unwrap();
            if snap
                .windows(b"round11-routing-marker".len())
                .any(|w| w == b"round11-routing-marker")
                && snap.len() > before.len()
            {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            found,
            "write_to_pane_and_submit must have landed bytes in the LIVE reuser's scrollback, not the exited entry's"
        );

        registry.shutdown_all();
    }

    /// `write_to_pane_notice` must skip both the `SUBMIT_DELAY` sleep
    /// and the trailing CR — the byte sequence an agent TUI treats as
    /// "Enter". Used by the `handle_delegate`-side spawn-failure
    /// notice so the orchestrator LLM doesn't process the diagnostic
    /// as a user prompt.
    ///
    /// Timing is the test signal: `write_to_pane_and_submit` waits
    /// the full `SUBMIT_DELAY` (150 ms) between payload and CR, so
    /// the call can't return in less than that. `write_to_pane_notice`
    /// writes payload + `\n` and returns immediately. PTY line
    /// discipline normalizes CR/LF in the program-visible input
    /// stream, so we can't distinguish the two writes by what
    /// `cat -u` echoes back — but the SUBMIT_DELAY gate is
    /// observable from the caller's wall clock.
    #[tokio::test]
    async fn write_to_pane_notice_skips_submit_delay() {
        let registry = AgentPtyRegistry::new();
        let _id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "no-submit".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn cat");

        let start = tokio::time::Instant::now();
        registry
            .write_to_pane_notice("no-submit", "notice")
            .await
            .expect("write_to_pane_notice");
        let no_submit_elapsed = start.elapsed();
        assert!(
            no_submit_elapsed < SUBMIT_DELAY,
            "write_to_pane_notice must skip the SUBMIT_DELAY sleep; took {no_submit_elapsed:?} \
             (>= {SUBMIT_DELAY:?})"
        );

        let start = tokio::time::Instant::now();
        registry
            .write_to_pane_and_submit("no-submit", "prompt")
            .await
            .expect("write_to_pane_and_submit");
        let submit_elapsed = start.elapsed();
        assert!(
            submit_elapsed >= SUBMIT_DELAY,
            "write_to_pane_and_submit must wait at least SUBMIT_DELAY before the CR; \
             took {submit_elapsed:?} (< {SUBMIT_DELAY:?})"
        );

        registry.shutdown_all();
    }

    /// PRD #92 F9 followup-4 (auditor S2): freeze the KNOWN LIMITATION
    /// documented on `write_to_pane_notice` — that calling notice then
    /// submit on the same pane leaves the notice bytes uncommitted in
    /// the agent's stdin, so the next submit's CR submits them fused
    /// to the new prompt. The contract is doc-only otherwise; this
    /// test pins the daemon-side half (bytes land in order with only
    /// `\n` — never `\r` — between them) so a future change that
    /// accidentally swaps the notice terminator, inserts a separator,
    /// or "fixes" the accumulation gets caught.
    ///
    /// Stub: a raw-mode `cat` (`stty -echo -icanon -icrnl -opost`)
    /// that pumps stdin bytes verbatim to stdout. With default
    /// canonical mode, `/bin/cat` would close the canonical line on
    /// the notice's `\n` and emit two separate echo lines, hiding
    /// whether the daemon emitted a fusing CR between the writes —
    /// raw mode strips that line discipline so the assertion is
    /// unambiguous.
    ///
    /// TEST-SIDE LIMITATION: the downstream agent-TUI behavior
    /// (claude / codex buffering visible input until CR, then
    /// submitting the entire accumulated buffer as one prompt) lives
    /// in the agent process, not the daemon — we can't exercise it
    /// without a real agent. The assertion below pins the two
    /// daemon-side guarantees that make that downstream accumulation
    /// possible: notice bytes precede the submit bytes in the PTY
    /// scrollback, and the bytes between them contain only `\n`
    /// (no `\r`, so no early submit signal is emitted between them).
    #[tokio::test]
    async fn write_to_pane_notice_bytes_precede_next_submit_with_only_lf_between() {
        let registry = AgentPtyRegistry::new();
        // The shell prints `RAW-READY` *after* stty applies and *before* exec
        // into cat, so the test can poll the scrollback for that marker and
        // know the slave's termios is in raw mode before issuing the notice /
        // submit writes. On slow Linux CI runners a fixed sleep is not enough
        // — if `\n` lands while OPOST/ONLCR is still active, the kernel
        // translates it to `\r\n` in the master scrollback and the no-`\r`
        // assertion below trips on the ONLCR artifact even though the daemon
        // never emitted a CR.
        let _id = registry
            .spawn_agent(SpawnOptions {
                command: Some(
                    "stty -echo -icanon -icrnl -opost min 1 time 0 && \
                     printf RAW-READY && exec cat -u",
                ),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "accumulate".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn raw-mode cat shell");
        let agent_id = registry.agent_ids()[0].clone();

        // Wait for the shell to apply `stty` and print the readiness marker.
        // `printf` is a builtin in both bash and dash so no fork is needed
        // between stty completing and the marker landing, and the marker is
        // pure alphanumeric+hyphen so OPOST translation does not affect its
        // appearance even on the off chance stty hadn't applied yet.
        let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut raw_ready = false;
        while tokio::time::Instant::now() < ready_deadline {
            let snap = registry.snapshot(&agent_id).unwrap_or_default();
            if snap.windows(b"RAW-READY".len()).any(|w| w == b"RAW-READY") {
                raw_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            raw_ready,
            "shell never printed RAW-READY — stty/exec cat -u didn't apply in time"
        );

        registry
            .write_to_pane_notice("accumulate", "NOTICE-MARKER")
            .await
            .expect("write_to_pane_notice");
        registry
            .write_to_pane_and_submit("accumulate", "USER-PROMPT")
            .await
            .expect("write_to_pane_and_submit");

        // Master scrollback should contain the exact byte sequence
        // the daemon wrote: `NOTICE-MARKER\nUSER-PROMPT\r` (raw cat
        // echoes each input byte verbatim). The substring check is
        // tolerant of any startup banner the shell emitted before
        // stty took effect; the ORDER check is what pins the contract.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut found = false;
        let mut last = Vec::new();
        let mut between_start = 0usize;
        let mut between_end = 0usize;
        while tokio::time::Instant::now() < deadline {
            last = registry.snapshot(&agent_id).unwrap_or_default();
            let notice_at = last
                .windows(b"NOTICE-MARKER".len())
                .position(|w| w == b"NOTICE-MARKER");
            let prompt_at = last
                .windows(b"USER-PROMPT".len())
                .position(|w| w == b"USER-PROMPT");
            if let (Some(n), Some(p)) = (notice_at, prompt_at)
                && n < p
            {
                between_start = n + b"NOTICE-MARKER".len();
                between_end = p;
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            found,
            "scrollback must contain NOTICE-MARKER followed by USER-PROMPT — \
             proves the daemon delivered notice + submit bytes to the agent's \
             stdin in order (the prerequisite for the documented accumulation \
             behavior). Last snapshot: {:?}",
            String::from_utf8_lossy(&last)
        );

        // Tighter check: the slice between the end of NOTICE-MARKER and the
        // start of USER-PROMPT must contain no `\r` byte. Without this, a
        // regression that swapped `write_to_pane_notice`'s terminator from
        // `\n` to `\r` would leave both substrings intact and ordered, so
        // the order check alone would silently pass while the bug existed.
        let between = &last[between_start..between_end];
        assert!(
            !between.contains(&b'\r'),
            "between NOTICE-MARKER and USER-PROMPT the daemon must only \
             emit `\\n` (the notice terminator), never `\\r` — a `\\r` here \
             would be an early submit signal that breaks the accumulation \
             contract. Bytes between: {:?}",
            String::from_utf8_lossy(between)
        );

        registry.shutdown_all();
    }

    /// PRD #92 F9 followup-3: `close_agent` must NOT prune the
    /// `dispatch_mutexes` entry for the closed agent's `pane_id_env`.
    /// Pruning was tried in followup-2 and reverted because it
    /// re-opened the followup-1 race: an in-flight dispatcher holds
    /// an `Arc<AsyncMutex>` already cloned out of the map, so a fresh
    /// dispatcher after the close would `or_insert_with` a *different*
    /// `AsyncMutex` for the same `pane_id_env` and the two would stop
    /// serializing against each other. This regression test guards
    /// against a future re-introduction of pruning.
    #[tokio::test]
    async fn close_agent_does_not_prune_dispatch_mutex_entry() {
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "must-not-be-pruned".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("spawn sh");
        // Populate the dispatch_mutexes entry by borrowing the lock.
        let arc_before = registry.pane_dispatch_lock("must-not-be-pruned");
        assert_eq!(
            registry.dispatch_mutexes.lock().unwrap().len(),
            1,
            "lock-borrow must populate dispatch_mutexes"
        );

        registry.close_agent(&id).expect("close should succeed");
        assert_eq!(
            registry.dispatch_mutexes.lock().unwrap().len(),
            1,
            "close_agent must NOT prune the dispatch_mutexes entry — \
             pruning re-opens the followup-1 race where two dispatchers \
             across a close+respawn end up with different AsyncMutex \
             instances for the same pane_id_env"
        );

        // The post-close lookup must return the *same* AsyncMutex
        // instance the in-flight dispatcher already holds — that's
        // the whole point of not pruning. Two dispatchers across a
        // close+respawn must serialize against the same mutex.
        let arc_after = registry.pane_dispatch_lock("must-not-be-pruned");
        assert!(
            Arc::ptr_eq(&arc_before, &arc_after),
            "post-close pane_dispatch_lock must return the same Arc \
             so an in-flight dispatcher and a fresh dispatcher hold \
             the same AsyncMutex instance"
        );

        registry.shutdown_all();
    }

    #[test]
    fn registry_write_to_pane_and_submit_routes_to_correct_agent_by_pane_id() {
        // CodeRabbit MAJOR (PRD #93 round-9) regression guard: with
        // distinct pane_id_envs, `write_to_pane_and_submit(pane_id,
        // bytes)` must land in *that* agent's PTY and not leak into a sibling.
        // Mirrors the production routing path delegate/work-done uses.
        // We can't easily read PTY bytes from a `/bin/sh` so we
        // confirm structurally: the registry must contain both agents
        // and their `pane_id_env`s must be the values we set.
        let registry = AgentPtyRegistry::new();
        let id_a = registry
            .spawn_agent(SpawnOptions {
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-a".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn a");
        let id_b = registry
            .spawn_agent(SpawnOptions {
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-b".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn b");

        let records = registry.agent_records();
        let rec_a = records.iter().find(|r| r.id == id_a).unwrap();
        let rec_b = records.iter().find(|r| r.id == id_b).unwrap();
        assert_eq!(rec_a.pane_id_env.as_deref(), Some("pane-a"));
        assert_eq!(rec_b.pane_id_env.as_deref(), Some("pane-b"));
        registry.shutdown_all();
    }

    #[test]
    fn registry_close_unknown_errors() {
        let registry = AgentPtyRegistry::new();
        assert!(matches!(
            registry.close_agent("does-not-exist"),
            Err(AgentPtyError::NotFound(_))
        ));
    }

    #[test]
    fn registry_assigns_sequential_ids() {
        let registry = AgentPtyRegistry::new();
        let id1 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        let id2 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        let n1: u64 = id1.parse().unwrap();
        let n2: u64 = id2.parse().unwrap();
        assert_eq!(n2, n1 + 1);
        registry.shutdown_all();
    }

    /// Returns true if `kill(pid, 0)` reports the process is gone (ESRCH).
    /// `kill(pid, 0)` performs an existence check without actually signalling.
    ///
    /// PRD #42 M2: `kill(pid, 0)` is POSIX (no Windows analogue), so this
    /// liveness helper and the three Drop/shutdown tests that use it are gated
    /// to Unix. The same teardown logic on Windows is exercised via Job-Object
    /// reaping under PRD #163.
    #[cfg(unix)]
    fn pid_is_dead(pid: u32) -> bool {
        let r = unsafe { libc::kill(pid as i32, 0) };
        if r == 0 {
            return false;
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        errno == Some(libc::ESRCH)
    }

    #[cfg(unix)]
    #[test]
    fn registry_shutdown_all_clears_state() {
        let registry = AgentPtyRegistry::new();
        let id1 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        let id2 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        assert_eq!(registry.len(), 2);

        // Capture child PIDs so we can verify they're actually gone after
        // shutdown_all (not just absent from the registry map).
        let pids: Vec<u32> = {
            let inner = registry.inner.lock().unwrap();
            [&id1, &id2]
                .into_iter()
                .map(|id| inner.agents.get(id).unwrap().child.process_id().unwrap())
                .collect()
        };

        registry.shutdown_all();
        assert!(registry.is_empty());

        for pid in &pids {
            assert!(
                pid_is_dead(*pid),
                "pid {pid} should be dead after shutdown_all"
            );
        }

        // Idempotent.
        registry.shutdown_all();
    }

    #[tokio::test]
    async fn live_count_excludes_exited_agent_after_child_dies() {
        // PRD #93 round-2 reviewer REV-3: the daemon's idle monitor calls
        // `live_count()` (not `len()`) so an agent whose child exited but
        // whose registry entry hasn't been removed doesn't pin the daemon
        // up past its idle window. Test: spawn a command that exits
        // immediately, wait for the reader thread to observe EOF and set
        // the `exited` flag, then assert `live_count` is 0 even though
        // `len` is still 1.
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/usr/bin/true"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");
        assert_eq!(registry.len(), 1);

        // Wait up to a few seconds for the reader thread to drain to EOF
        // and set `exited`. /usr/bin/true exits quickly, but the PTY drain +
        // OS scheduling can take a couple of hundred ms on a loaded box.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if registry.live_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            registry.live_count(),
            0,
            "registry.live_count must drop to 0 once the child has exited and the reader sees EOF"
        );
        assert_eq!(
            registry.len(),
            1,
            "len() still counts the exited entry — only live_count filters"
        );

        // Cleanup leaves the registry empty so other tests can't observe
        // the leftover entry via shared global state.
        registry.close_agent(&id).unwrap();
    }

    #[tokio::test]
    async fn change_notify_fires_on_spawn_and_close_and_agent_exit() {
        // PRD #93 round-2 reviewer REV-1: the registry signals
        // `change_notify` on spawn, close, and (via pump_reader) when the
        // child exits. Without these signals an edge-triggered idle
        // monitor would miss transitions and either fire too early or
        // never re-arm.
        let registry = Arc::new(AgentPtyRegistry::new());
        let notify = registry.change_notify();

        // Spawn → must notify.
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .expect("spawn must signal change_notify");

        // Close → must notify.
        registry.close_agent(&id).expect("close should succeed");
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .expect("close must signal change_notify");

        // Agent dies on its own after a short delay (no explicit close) →
        // must notify via pump_reader on EOF. The brief `sleep` (shell-wrapped
        // because it's multi-word) keeps the child alive long enough to drain
        // the spawn signal *first*, so the exit signal can't coalesce with it.
        // The old `/usr/bin/true` exited instantly, so under load its
        // exit-notify could merge with the spawn-notify — `Notify` collapses
        // multiple pending `notify_one` calls into a single permit — and the
        // drain then ate the only permit, making the exit wait time out.
        let _id2 = registry
            .spawn_agent(SpawnOptions {
                command: Some("sleep 0.5"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");
        // Drain the spawn signal while the child is still sleeping — no exit
        // notify has fired yet, so this consumes only the spawn permit.
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .expect("spawn must signal change_notify");
        // Now the child exits on its own → pump_reader must signal on EOF.
        tokio::time::timeout(Duration::from_secs(5), notify.notified())
            .await
            .expect("agent exit must signal change_notify");
    }

    #[cfg(unix)]
    #[test]
    fn registry_drop_kills_agents() {
        // Constructing-and-dropping a registry with a live agent must not
        // hang and must terminate the child. We capture the PID before the
        // registry goes out of scope, then verify the kernel reaped it.
        let pid: u32;
        {
            let registry = AgentPtyRegistry::new();
            let id = registry.spawn_agent(SpawnOptions::default()).unwrap();
            pid = registry
                .inner
                .lock()
                .unwrap()
                .agents
                .get(&id)
                .unwrap()
                .child
                .process_id()
                .unwrap();
        }
        assert!(pid_is_dead(pid), "pid {pid} should be dead after Drop");
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_drop_kills_orphan_child() {
        // Models the leak scenario the in-`spawn()` ChildGuard now covers:
        // a child has been spawned, but a *later* fallible step (the real
        // ones being `take_writer` / `try_clone_reader`) errors out before
        // the child can be moved into the returned AgentPty. Dropping the
        // guard on that error path must force-kill and reap the child so
        // no orphan PID is left behind.
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let cmd = CommandBuilder::new(&default_shell);
        let child = pair.slave.spawn_command(cmd).expect("spawn should succeed");
        drop(pair.slave);
        let pid = child.process_id().expect("child should expose a pid");

        let guard = ChildGuard::new(child);
        // Drop the master *before* the guard so any PTY I/O the child is
        // blocked on unblocks before SIGKILL — matching the production
        // shutdown order.
        drop(pair.master);
        drop(guard);

        assert!(
            pid_is_dead(pid),
            "pid {pid} should be dead after ChildGuard drop"
        );
    }

    #[test]
    fn spawn_options_env_reaches_child() {
        // Spawn a shell that exits with a status determined by a value passed
        // through SpawnOptions::env. If the env var fails to propagate, the
        // child exits 99 instead of 42 and the assertion below fires.
        let pty = spawn(SpawnOptions {
            command: Some("sh -c 'exit ${DOT_AGENT_DECK_PANE_ID:-99}'"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.into(), "42".into())],
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
        let mut child = pty.child;
        let status = child.wait().expect("wait should succeed");
        assert_eq!(
            status.exit_code(),
            42,
            "child did not see DOT_AGENT_DECK_PANE_ID env var"
        );
    }

    /// Test mutex covering temporary process-env mutation. `std::env::set_var`
    /// is process-global, so any test that pokes at the environment must run
    /// serialized to avoid leaking the value into a sibling test's spawn.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn spawn_scrubs_via_daemon_env_from_child() {
        // Set the var on the parent process, then spawn — the child must NOT
        // see it (this protects against the inheritance footgun where a
        // daemon launched with DOT_AGENT_DECK_VIA_DAEMON=1 hands the flag to
        // every agent it spawns, so an agent that shells out to
        // `dot-agent-deck` would itself try to act as a stream client).
        let _g = ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        // SAFETY: tests in this module are serialized by ENV_TEST_LOCK and
        // we restore the prior value before releasing the lock, so the
        // process-global env mutation is invisible to other tests.
        let prior = std::env::var(DOT_AGENT_DECK_VIA_DAEMON).ok();
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_VIA_DAEMON, "1");
        }

        // Child exits 0 if the var is absent (the default branch of the
        // `${VAR:+...}` form); 1 if it inherited the value from the parent.
        let pty = spawn(SpawnOptions {
            command: Some("sh -c 'exit ${DOT_AGENT_DECK_VIA_DAEMON:+1}'"),
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
        let mut child = pty.child;
        let status = child.wait().expect("wait should succeed");

        // Restore the prior env state before asserting so a failure doesn't
        // leak the var into subsequent tests within the same process.
        unsafe {
            match prior {
                Some(v) => std::env::set_var(DOT_AGENT_DECK_VIA_DAEMON, v),
                None => std::env::remove_var(DOT_AGENT_DECK_VIA_DAEMON),
            }
        }

        assert_eq!(
            status.exit_code(),
            0,
            "child saw DOT_AGENT_DECK_VIA_DAEMON — agent_pty::spawn must scrub it"
        );
    }

    #[test]
    fn spawn_scrubs_pane_id_env_from_child() {
        // Mirror of the VIA_DAEMON scrub test for PANE_ID. The footgun: a
        // daemon spawned as a child of an existing deck pane would inherit
        // that pane's id and tag every agent it later spawns with the wrong
        // pane (so hooks would route events to the wrong tab).
        let _g = ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        // SAFETY: serialized by ENV_TEST_LOCK; prior value is restored
        // before the lock is released.
        let prior = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_PANE_ID, "stale-pane");
        }

        // Spawn without setting PANE_ID via opts.env — the child must not
        // observe the inherited value. Exit 0 if absent, 1 if inherited.
        let pty = spawn(SpawnOptions {
            command: Some("sh -c 'exit ${DOT_AGENT_DECK_PANE_ID:+1}'"),
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
        let mut child = pty.child;
        let status = child.wait().expect("wait should succeed");

        unsafe {
            match prior {
                Some(v) => std::env::set_var(DOT_AGENT_DECK_PANE_ID, v),
                None => std::env::remove_var(DOT_AGENT_DECK_PANE_ID),
            }
        }

        assert_eq!(
            status.exit_code(),
            0,
            "child saw inherited DOT_AGENT_DECK_PANE_ID — agent_pty::spawn must scrub it"
        );
    }

    #[test]
    fn spawn_opts_env_overrides_pane_id_scrub() {
        // The scrub must not clobber a deliberately-supplied PANE_ID via
        // opts.env — embedded_pane relies on this so daemon-spawned agents
        // get tagged with the right pane id even when the daemon's own env
        // happens to carry a stale one.
        let _g = ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        // SAFETY: serialized by ENV_TEST_LOCK; prior value is restored
        // before the lock is released.
        let prior = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_PANE_ID, "stale-pane");
        }

        let pty = spawn(SpawnOptions {
            command: Some("sh -c 'exit ${DOT_AGENT_DECK_PANE_ID:-99}'"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.into(), "42".into())],
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
        let mut child = pty.child;
        let status = child.wait().expect("wait should succeed");

        unsafe {
            match prior {
                Some(v) => std::env::set_var(DOT_AGENT_DECK_PANE_ID, v),
                None => std::env::remove_var(DOT_AGENT_DECK_PANE_ID),
            }
        }

        assert_eq!(
            status.exit_code(),
            42,
            "opts.env PANE_ID was clobbered — scrub must run before opts.env is applied"
        );
    }

    // ---------------------------------------------------------------------
    // PRD #104 M1: daemon stores and reports current PTY dims via
    // `AgentRecord.rows/cols`. Without these, the client's vt100 parser
    // initialises every reattached pane at 24×80 and snapshots that were
    // emitted at a wider geometry get clamped — scrolled-back rows are
    // permanently corrupted (PRD #104 problem statement).
    // ---------------------------------------------------------------------

    #[test]
    fn agent_record_round_trips_explicit_rows_cols() {
        let rec = AgentRecord {
            id: "1".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 120,
            cols: 40,
            live: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"], 120);
        assert_eq!(v["cols"], 40);
        let back: AgentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rows, 120);
        assert_eq!(back.cols, 40);
    }

    #[test]
    fn agent_record_without_rows_cols_fields_deserializes_as_zero() {
        // Forward compat: an older daemon predating PRD #104 omits these
        // fields entirely. `#[serde(default)]` makes them decode as 0,
        // which the hydration path detects and falls back to the 24×80
        // placeholder for.
        let legacy_json = r#"{
            "id": "1",
            "pane_id_env": null,
            "display_name": null,
            "cwd": null
        }"#;
        let back: AgentRecord = serde_json::from_str(legacy_json)
            .expect("older daemon shape must decode via #[serde(default)] on rows/cols");
        assert_eq!(back.rows, 0);
        assert_eq!(back.cols, 0);
    }

    #[test]
    fn spawn_at_120x40_surfaces_dims_via_agent_records() {
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                rows: 120,
                cols: 40,
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");
        let records = registry.agent_records();
        let rec = records.iter().find(|r| r.id == id).expect("agent missing");
        assert_eq!(rec.rows, 120);
        assert_eq!(rec.cols, 40);
        registry.shutdown_all();
    }

    #[test]
    fn resize_updates_dims_reported_via_agent_records() {
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                rows: 24,
                cols: 80,
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");
        registry
            .resize(&id, 100, 30)
            .expect("resize should succeed");
        let records = registry.agent_records();
        let rec = records.iter().find(|r| r.id == id).expect("agent missing");
        assert_eq!(rec.rows, 100);
        assert_eq!(rec.cols, 30);
        registry.shutdown_all();
    }

    // ---------------------------------------------------------------------
    // PRD #104 M3: clearing scrollback on resize. A snapshot returned to
    // a fresh subscriber always covers a single (rows, cols) epoch.
    // ---------------------------------------------------------------------

    /// Push `bytes` into `registry`'s agent `id` by writing through the
    /// PTY master and spinning until the reader thread surfaces `bytes`
    /// verbatim in the bus's scrollback. Pulled out of
    /// `resize_clears_scrollback` so the sibling A1 test
    /// (`resize_with_unchanged_dims_preserves_scrollback`) can reuse it
    /// without duplicating the spin-and-write boilerplate.
    ///
    /// We search for the literal byte run rather than just "snapshot
    /// grew" because the R2 residual gap (pre-resize `pump_reader`
    /// bytes that land after `clear_scrollback`) can otherwise make a
    /// fresh write look stuck behind a stale baseline.
    fn write_and_wait_for_scrollback(registry: &AgentPtyRegistry, id: &str, bytes: &[u8]) {
        let writer = {
            let inner = registry.inner.lock().unwrap();
            let agent = inner.agents.get(id).expect("agent must exist");
            agent.writer.clone()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use std::io::Write as _;
            let mut w = writer.lock().await;
            w.write_all(bytes).unwrap();
            let _ = w.flush();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            let snap = registry.snapshot(id).unwrap();
            if snap.windows(bytes.len()).any(|w| w == bytes) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let snap = registry.snapshot(id).unwrap();
        panic!(
            "test prerequisite: bytes {:?} never surfaced in scrollback within 3s; \
             current snapshot len={}",
            String::from_utf8_lossy(bytes),
            snap.len()
        );
    }

    #[test]
    fn resize_clears_scrollback() {
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                // Use `cat` so the child stays alive long enough for us
                // to feed bytes through the master side and observe the
                // reader thread push them to scrollback.
                command: Some("/bin/cat"),
                rows: 24,
                cols: 80,
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        // Write bytes through the agent's writer; the kernel echoes them
        // back through the master, where pump_reader appends to
        // `AgentBus::scrollback`. Spin briefly until non-empty so the
        // test doesn't race the reader thread.
        write_and_wait_for_scrollback(&registry, &id, b"hello");
        assert!(
            !registry.snapshot(&id).unwrap().is_empty(),
            "test prerequisite: pre-resize snapshot should have echoed bytes"
        );

        registry
            .resize(&id, 30, 100)
            .expect("resize should succeed");
        assert!(
            registry.snapshot(&id).unwrap().is_empty(),
            "resize must drop scrollback so the next subscriber sees a single-epoch snapshot"
        );

        // Bytes pushed after the resize must repopulate snapshot — the
        // bus is still functional, only the historical buffer was cleared.
        write_and_wait_for_scrollback(&registry, &id, b"fresh");
        assert!(
            !registry.snapshot(&id).unwrap().is_empty(),
            "post-resize writes must reach the (cleared) scrollback"
        );

        registry.shutdown_all();
    }

    // PRD #104 A1 (auditor): a `resize(id, same_rows, same_cols)` must
    // be a true no-op — including leaving scrollback untouched. The
    // UI's per-frame resize sweep calls `resize_pane_pty` on every
    // unchanged tick, and clearing every time would wipe in-flight
    // scrollback bytes before a fresh subscriber could observe them.
    #[test]
    fn resize_with_unchanged_dims_preserves_scrollback() {
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                rows: 24,
                cols: 80,
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        write_and_wait_for_scrollback(&registry, &id, b"keep-me");
        let pre = registry.snapshot(&id).unwrap();
        assert!(!pre.is_empty(), "test prerequisite: scrollback non-empty");

        // Same dims — must not touch scrollback or any of the registry
        // bookkeeping that resize would normally refresh.
        registry
            .resize(&id, 24, 80)
            .expect("no-op resize should succeed");
        let post = registry.snapshot(&id).unwrap();
        assert_eq!(
            pre, post,
            "no-op resize must leave scrollback bytes untouched"
        );
        // The captured dims must also match what was already stored —
        // the no-op path skips the refresh too, but the result is the
        // same because the values weren't changing in the first place.
        let records = registry.agent_records();
        let rec = records.iter().find(|r| r.id == id).expect("agent missing");
        assert_eq!((rec.rows, rec.cols), (24, 80));

        registry.shutdown_all();
    }

    // ---------------------------------------------------------------------
    // PRD #104 R3 (reviewer): `pty_rows` / `pty_cols` are now
    // wire-visible via `AgentRecord`, so the spawn-time capture site
    // must apply the same `[1, PTY_RESIZE_DIM_MAX]` clamp `resize()`
    // applies. Without this, a caller-supplied oversized value would
    // surface to the client unchanged.
    // ---------------------------------------------------------------------

    #[test]
    fn spawn_clamps_oversized_rows_cols_in_captured_dims() {
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                rows: PTY_RESIZE_DIM_MAX + 1,
                cols: PTY_RESIZE_DIM_MAX + 100,
                ..SpawnOptions::default()
            })
            .expect("spawn should clamp + succeed");
        let records = registry.agent_records();
        let rec = records.iter().find(|r| r.id == id).expect("agent missing");
        assert_eq!(rec.rows, PTY_RESIZE_DIM_MAX);
        assert_eq!(rec.cols, PTY_RESIZE_DIM_MAX);
        registry.shutdown_all();
    }

    #[test]
    fn spawn_at_u16_max_rows_cols_clamps_not_panics() {
        let registry = AgentPtyRegistry::new();
        let id = registry
            .spawn_agent(SpawnOptions {
                rows: u16::MAX,
                cols: u16::MAX,
                ..SpawnOptions::default()
            })
            .expect("spawn should clamp u16::MAX cleanly");
        let records = registry.agent_records();
        let rec = records.iter().find(|r| r.id == id).expect("agent missing");
        assert_eq!(rec.rows, PTY_RESIZE_DIM_MAX);
        assert_eq!(rec.cols, PTY_RESIZE_DIM_MAX);
        registry.shutdown_all();
    }

    // ---------------------------------------------------------------------
    // PRD #104 RN1 (reviewer): `AgentRecord.rows/cols` now use
    // `skip_serializing_if = "is_zero_u16"` so a daemon that hasn't
    // recorded real dims yet emits the pre-PRD wire shape. The serde
    // round-trip still has to work for both new (non-zero) and legacy
    // (zero / absent) cases.
    // ---------------------------------------------------------------------

    #[test]
    fn agent_record_omits_rows_cols_when_zero_on_the_wire() {
        let rec = AgentRecord {
            id: "1".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
            live: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("rows"),
            "rows=0 must be omitted from the wire payload so pre-PRD clients keep decoding"
        );
        assert!(
            !obj.contains_key("cols"),
            "cols=0 must be omitted from the wire payload so pre-PRD clients keep decoding"
        );
        // Round-trip via deserialize still produces 0 thanks to
        // `#[serde(default)]`.
        let back: AgentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rows, 0);
        assert_eq!(back.cols, 0);
    }

    // -----------------------------------------------------------------------
    // PRD #20 R20-004 (finding #3): delivery-ledger idempotency + partial-write
    // ambiguity. Coder-authored targeted coverage for the W3-Pass-1 items the
    // tester left (redtests.md "Harness Gaps": partial-write fault injection).
    // -----------------------------------------------------------------------

    /// A `std::io::Write` that accepts at most `budget` bytes total, then errors —
    /// the fault seam the tester noted the registry lacked. Lets us drive
    /// `deliver_payload_and_submit` through the "nothing written / partial /
    /// complete" branches without a real (unfaultable) PTY writer.
    struct FaultyWriter {
        budget: usize,
        written: usize,
    }

    impl std::io::Write for FaultyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.written >= self.budget {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "faulty writer budget exhausted",
                ));
            }
            let n = buf.len().min(self.budget - self.written);
            self.written += n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn deliver_payload_classifies_partial_write_as_ambiguous() {
        // Payload fully written AND submit CR written → Applied.
        let mut w = FaultyWriter {
            budget: b"hello".len() + 1,
            written: 0,
        };
        assert_eq!(
            deliver_payload_and_submit(&mut w, b"hello").await,
            PayloadDelivery::Applied
        );

        // First byte can't be written (0 bytes reached the target) → a clean,
        // retryable transport failure, NOT ambiguous.
        let mut w = FaultyWriter {
            budget: 0,
            written: 0,
        };
        assert!(matches!(
            deliver_payload_and_submit(&mut w, b"hello").await,
            PayloadDelivery::CleanFailure(_)
        ));

        // Some payload bytes reached the target, then the writer errored →
        // AMBIGUOUS (must not be blind-retried into a duplicate).
        let mut w = FaultyWriter {
            budget: 2,
            written: 0,
        };
        assert_eq!(
            deliver_payload_and_submit(&mut w, b"hello").await,
            PayloadDelivery::Ambiguous
        );

        // Payload fully written but the submit CR fails → still AMBIGUOUS (the
        // target holds un-submitted payload bytes).
        let mut w = FaultyWriter {
            budget: b"hello".len(),
            written: 0,
        };
        assert_eq!(
            deliver_payload_and_submit(&mut w, b"hello").await,
            PayloadDelivery::Ambiguous
        );
    }

    #[tokio::test]
    async fn delivery_ledger_replays_delivered_and_ambiguous_but_retries_non_delivery() {
        use crate::event::SendResult;
        let reg = AgentPtyRegistry::new();
        let fp = AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), "pane", "text");

        // First admission proceeds; record a DELIVERED outcome.
        let permit = match reg.admit_delivery("did-applied", fp).await {
            DeliveryAdmission::Proceed(p) => p,
            DeliveryAdmission::Replay(_) => panic!("first admission must Proceed, got Replay"),
            DeliveryAdmission::Conflict => panic!("first admission must Proceed, got Conflict"),
        };
        reg.record_delivery_outcome(&permit, SendResult::Applied);
        drop(permit);
        // A retry with the SAME id+fingerprint REPLAYS (no re-submit).
        assert!(matches!(
            reg.admit_delivery("did-applied", fp).await,
            DeliveryAdmission::Replay(SendResult::Applied)
        ));

        // AMBIGUOUS is cached too — a partial write must not be blind-retried.
        let permit = match reg.admit_delivery("did-ambiguous", fp).await {
            DeliveryAdmission::Proceed(p) => p,
            _ => panic!("expected Proceed"),
        };
        reg.record_delivery_outcome(&permit, SendResult::Ambiguous);
        drop(permit);
        assert!(matches!(
            reg.admit_delivery("did-ambiguous", fp).await,
            DeliveryAdmission::Replay(SendResult::Ambiguous)
        ));

        // A NON-delivered outcome is FORGOTTEN — a later retry re-attempts (a
        // history-only role that becomes live must still get its prompt).
        let permit = match reg.admit_delivery("did-history", fp).await {
            DeliveryAdmission::Proceed(p) => p,
            _ => panic!("expected Proceed"),
        };
        reg.record_delivery_outcome(&permit, SendResult::HistoryOnly);
        drop(permit);
        assert!(matches!(
            reg.admit_delivery("did-history", fp).await,
            DeliveryAdmission::Proceed(_)
        ));
    }

    #[tokio::test]
    async fn delivery_ledger_conflicting_fingerprint_reuse_is_refused() {
        use crate::event::SendResult;
        let reg = AgentPtyRegistry::new();
        let fp_a = AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), "pane", "payload-a");
        let fp_b = AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), "pane", "payload-b");
        assert_ne!(fp_a, fp_b, "distinct payloads must fingerprint differently");

        let permit = match reg.admit_delivery("shared-id", fp_a).await {
            DeliveryAdmission::Proceed(p) => p,
            _ => panic!("expected Proceed"),
        };
        reg.record_delivery_outcome(&permit, SendResult::Applied);
        drop(permit);

        // Reusing the SAME id with a DIFFERENT fingerprint must be a Conflict,
        // never a false replay of the first (unrelated) result.
        assert!(matches!(
            reg.admit_delivery("shared-id", fp_b).await,
            DeliveryAdmission::Conflict
        ));
    }

    /// PRD #20 R20-006 (finding #7): removal-after-authorization barrier. Hold
    /// the target writer externally so a guarded send blocks AFTER its pre-lock
    /// identity gate but BEFORE the write; remove the agent (registry entry gone)
    /// while it waits; then release. The post-writer-lock re-resolution must find
    /// NO current owner for the pane and return `Stale` with NO bytes written —
    /// closing the window where a close/respawn lands after authorization.
    #[tokio::test]
    async fn guarded_send_rejects_agent_removal_after_writer_lock() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let id = reg
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "pane-removal-barrier".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("spawn agent");

        // Acquire the EXACT writer the guarded send will contend for, and hold it.
        let target = reg
            .writer_target_for_pane("pane-removal-barrier")
            .expect("live target for pane");
        let guard = target.writer.lock().await;

        let reg_for_task = reg.clone();
        let mut task = tokio::spawn(async move {
            reg_for_task
                .write_and_submit_guarded(
                    "pane-removal-barrier",
                    "printf 'REMOVED-AFTER-AUTH'",
                    None,
                    // Liveness always "ok" — the ONLY thing that must reject is
                    // the removal re-resolution under the held writer.
                    || async { true },
                )
                .await
        });

        // Precondition: the send is parked on the held writer (authorized, not
        // yet written).
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut task)
                .await
                .is_err(),
            "precondition: guarded send must block on the held writer"
        );

        // Remove the agent WHILE the send holds authorization but waits for the
        // writer.
        reg.close_agent(&id).expect("close agent");
        drop(guard);

        let result = task.await.unwrap().expect("guarded send result");
        assert_eq!(
            result,
            GuardedSend::Stale,
            "a target removed while the send waited for its writer must be refused as Stale (no bytes)"
        );

        reg.shutdown_all();
    }

    #[test]
    fn delivery_ledger_lru_touch_and_forget() {
        let mut ledger = DeliveryLedger::default();
        let mk = |fp| DeliveryRecord {
            fingerprint: fp,
            lock: Arc::new(AsyncMutex::new(())),
            result: None,
        };
        ledger.records.insert("a".into(), mk(1));
        ledger.touch("a");
        ledger.records.insert("b".into(), mk(2));
        ledger.touch("b");
        ledger.records.insert("c".into(), mk(3));
        ledger.touch("c");
        // Touch "a" → it becomes most-recent, so the LRU front is now "b".
        ledger.touch("a");
        assert_eq!(
            ledger.order.iter().cloned().collect::<Vec<_>>(),
            vec!["b".to_string(), "c".to_string(), "a".to_string()],
            "touch must move an id to the most-recent (back) position"
        );
        // forget drops from both maps.
        ledger.forget("c");
        assert!(!ledger.records.contains_key("c"));
        assert_eq!(
            ledger.order.iter().cloned().collect::<Vec<_>>(),
            vec!["b".to_string(), "a".to_string()]
        );
    }
}
