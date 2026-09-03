//! Reusable PTY-spawn primitive shared by the TUI and the daemon.
//!
//! Both the TUI process (`embedded_pane`) and the daemon (`daemon`) need to
//! spawn agent processes attached to a PTY and own the child + master handles
//! for the lifetime of the agent. This module extracts that core so it isn't
//! trapped inside the TUI path. The daemon piece is the foundation for Phase 1
//! (M1.2 streaming attach protocol) — see PRD #76 lines 140–146.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, oneshot};

use crate::event::{AgentType, OrchestrationSurface};
use crate::pane_input::{PaneInputError, SUBMIT_DELAY, encode_pane_payload, escape_bytes_for_log};
use crate::state::Ownership;

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

/// Hook-ingestion endpoint override read by [`crate::config::socket_path`].
///
/// The daemon injects its OWN bound hook-socket path into every agent it
/// spawns ([`AgentPtyRegistry::spawn_agent`]) so a child never has to
/// re-resolve the endpoint from ambient environment. Everything downstream of
/// a spawn that emits events — `dot-agent-deck wrap`, the `hook` /
/// `agent-event` verbs, an agent's installed hook script — resolves this var
/// at *emit* time, so without the injection a child inherits whatever
/// `XDG_RUNTIME_DIR` its grandparent happened to carry. That is exactly how a
/// test-spawned agent's events used to land in the developer's *live* daemon
/// and surface as phantom dashboard cards: the test overrode
/// [`DOT_AGENT_DECK_PANE_ID`] but not the socket, so the events arrived at the
/// real deck tagged with a pane id it had never heard of.
///
/// A caller-supplied value always wins — the injection only fills the gap.
pub const DOT_AGENT_DECK_SOCKET: &str = "DOT_AGENT_DECK_SOCKET";

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
/// always timed out for pi, at its then-10s value), plus a small margin for
/// Node/Bun boot. Independent of that constant's current value — pi no longer
/// waits on the readiness gate at all (it returns early to the native seed
/// path), so PRD #225 M4's retune does not move this grace.
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

/// Process-wide one-shot guard so the daemon logs a single line the first time
/// it has to clamp a resize request, rather than one per frame for the whole
/// life of a very wide terminal. See [`AgentPtyRegistry::resize`].
static OVERSIZED_RESIZE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Normalize a requested PTY geometry to what the child process will actually
/// be given: each axis clamped to [`PTY_RESIZE_DIM_MAX`].
///
/// **Issue #747 — this is the one place the cap is spelled.** The cap used to
/// live only at the far end of the resize path ([`AgentPtyRegistry::resize`]),
/// which clamped silently and returned `Ok`. The client applied no bound at
/// all, so on a terminal wider than the cap it would parse and render the
/// agent's output at (say) 4198 columns while the child had been told to wrap
/// at 4096 — a pane full of rewrapped, misaligned content, with nothing logged
/// and no error anywhere. Every participant in a resize now normalizes through
/// this function, so "the width the client parses at" and "the width the child
/// was given" are the same number by construction:
///
/// 1. `FrameLayout::pane_target_dims` (`src/ui.rs`) — the layout-derived target
///    `resize_panes_to_layout` drives every pane from.
/// 2. `EmbeddedPaneController::resize_pane_pty` (`src/embedded_pane.rs`) — the
///    local vt100 parser, and the `(rows, cols)` put on the pane's resize watch
///    channel and forwarded to the daemon as `AttachRequest::Resize`.
/// 3. `TerminalWidget::render` (`src/terminal_widget.rs`) — the PRD #84
///    invariant-3 guard, which compares the parser against the *capped* inner
///    area so an over-cap pane is not reported as a contract violation.
/// 4. [`AgentPtyRegistry::resize`] below — still the enforcing boundary, since
///    a same-uid attach-socket peer is under no obligation to pre-clamp.
///
/// A pane whose drawn area exceeds the cap therefore renders the child's full
/// 4096 columns through `TerminalWidget`'s `min(area, screen)` path and leaves
/// the remaining columns blank. That is the honest outcome: the child has no
/// more columns to show.
pub fn clamp_pty_dims(rows: u16, cols: u16) -> (u16, u16) {
    (rows.min(PTY_RESIZE_DIM_MAX), cols.min(PTY_RESIZE_DIM_MAX))
}

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
        /// PRD #140 M1.0: a per-TAB instance token, minted once when the
        /// orchestration tab is created and stamped on every role pane in
        /// that tab. Opaque to the daemon — it never parses or interprets
        /// the value, it only compares it for equality when deciding which
        /// panes belong to the same routing group
        /// ([`crate::state::OrchestrationIdentity`]).
        ///
        /// Distinct from the other three identity-ish fields: `name` is the
        /// CONFIG identity (which orchestration this is), `orchestration_cwd`
        /// is the DIRECTORY disambiguator (round-11 auditor #C), and
        /// `display_title` is presentation-only. Neither `name` nor
        /// `orchestration_cwd` distinguishes two tabs of the SAME
        /// orchestration opened from the SAME directory — that pair produces
        /// byte-identical `(name, cwd)` identities and the daemon
        /// cross-delivers delegate/work-done between them (issue #140). The
        /// instance token is what makes each tab its own routing group.
        ///
        /// `Option<String>` with `#[serde(default, skip_serializing_if)]` so
        /// older peers round-trip cleanly: a client predating this field
        /// sends nothing and the daemon falls back to the `(name, cwd)`
        /// identity, exactly the pre-#140 behaviour.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orchestration_id: Option<String>,
    },
}

/// PRD #140 M1.2/M1.3: mint a fresh per-tab orchestration instance token.
///
/// Called ONCE per orchestration tab (before the `for role in config.roles`
/// loop) and stamped on every role pane's
/// [`TabMembership::Orchestration::orchestration_id`], so all roles of one tab
/// share a token and no two tabs ever do. The value is opaque — only equality
/// matters — but it must satisfy [`is_valid_display_name`] so it survives
/// [`validate_tab_membership`] at the wire boundary.
///
/// Uniqueness follows the same recipe as `ui::mint_delivery_id`: a per-PROCESS
/// nonce (pid hashed with the epoch nanos at first use, so two processes — and
/// a pid reused across restarts — never collide) combined with a global
/// monotonic counter (so two tabs within one process never collide). The token
/// deliberately does NOT need to be reproducible across restarts: a live tab
/// re-hydrates its id from the daemon echo, it is never regenerated.
pub fn mint_orchestration_id() -> String {
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nonce = *NONCE.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut h);
        if let Ok(dur) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            dur.as_nanos().hash(&mut h);
        }
        h.finish()
    });
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("orch-{nonce:016x}-{seq}")
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
        orchestration_id,
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
        // PRD #140 M1.1: the instance token gets the same control-byte /
        // size discipline as role_name and orchestration_cwd — it is echoed
        // back through `list_agents` and logged, so a hostile same-user peer
        // could otherwise smuggle escape sequences through it.
        //
        // An invalid token REJECTS the whole membership rather than being
        // nulled out the way `display_title` is. `display_title` is cosmetic
        // with a defined fallback; the instance token is a ROUTING key, and
        // silently dropping it would merge two same-`(name, cwd)` tabs back
        // into one routing group — reintroducing exactly the cross-delivery
        // this PRD fixes, invisibly.
        if let Some(id) = orchestration_id.as_deref()
            && !is_valid_display_name(id)
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
/// control characters, AND an absolute path for this platform
/// ([`is_absolute_project_path`]).
///
/// Round-12 auditor #2: the orchestration_cwd field is treated as
/// the project root, so being absolute is part of the contract — a
/// relative or empty value would either fail the daemon's later
/// filesystem operations or quietly collide with sibling
/// orchestrations whose own resolved cwd happens to match. Reject up
/// front instead.
pub fn is_valid_orchestration_cwd(value: &str) -> bool {
    is_valid_cwd(value) && is_absolute_project_path(value)
}

/// Whether `value` is an absolute project-root path **on this platform** — the
/// only platform-dependent half of [`is_valid_orchestration_cwd`] and
/// [`validate_orchestration_surface`], kept as one `cfg` seam so the
/// classification rules themselves ([`is_posix_absolute_path`],
/// [`is_windows_absolute_path`]) stay pure data and are unit-tested on every
/// platform.
///
/// PRD #163 review: the rule used to be a bare `starts_with('/')` everywhere,
/// which is correct on Unix and rejects *every* legitimate Windows working
/// directory — a Windows daemon's own `current_dir()` is a drive-letter path
/// (`C:\proj`) and a network project root is a UNC path (`\\server\share\proj`).
/// The failure was silent in the worst way: an orchestration pane's
/// `TabMembership` was dropped to `None` and a live `OrchestrationSurface` was
/// discarded outright, so orchestration tabs simply never rehydrated on Windows.
///
/// - **Unix** keeps the historical rule byte-for-byte: a leading `/`, nothing
///   else.
/// - **Windows** accepts that *plus* its two native absolute forms. The POSIX
///   form stays valid there on purpose rather than as laziness: a Windows TUI
///   attached to a remote Unix daemon (`remotes.toml`) receives POSIX project
///   roots, and this same validator runs on that receive path.
fn is_absolute_project_path(value: &str) -> bool {
    #[cfg(unix)]
    {
        is_posix_absolute_path(value)
    }
    #[cfg(windows)]
    {
        is_posix_absolute_path(value) || is_windows_absolute_path(value)
    }
}

/// A POSIX absolute path: a leading `/`. The historical rule, and on Unix still
/// the only accepted form.
pub fn is_posix_absolute_path(value: &str) -> bool {
    value.starts_with('/')
}

/// A Windows absolute path, in the two rooted forms Win32 resolves without
/// consulting any per-process current directory:
///
/// - **UNC / device** — two leading separators: `\\server\share\proj`,
///   `//server/share/proj`, `\\?\C:\proj`. Either separator is accepted because
///   Win32 treats `/` and `\` interchangeably in paths.
/// - **Drive-letter rooted** — `C:\proj` or `C:/proj`.
///
/// Deliberately *not* accepted, because neither is absolute:
///
/// - `C:proj` — drive-*relative*: it resolves against that drive's own current
///   directory, so two orchestrations could resolve it to different real roots
///   (exactly the collision the absolute-path contract exists to prevent).
/// - `\proj` — rooted on the *current* drive, so it is likewise not a stable
///   project identity.
pub fn is_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let is_sep = |b: u8| b == b'\\' || b == b'/';
    // `\\server\share`, `//server/share`, `\\?\C:\…`
    if bytes.len() >= 2 && is_sep(bytes[0]) && is_sep(bytes[1]) {
        return true;
    }
    // `C:\proj` / `C:/proj` — the separator is required (see the doc comment).
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && is_sep(bytes[2])
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
    /// PRD #163 M3 — the OS grouping that makes "tear down the agent *and*
    /// everything it spawned" possible, established at spawn and handed to every
    /// teardown helper. Zero-sized on Unix (the child is already its own process
    /// group thanks to `portable-pty`'s `setsid`, which `killpg` addresses by
    /// pid); on Windows it owns the agent's Job Object handle, whose membership is
    /// only inherited forward — so it must be created here, at spawn, and live as
    /// long as the agent. See [`crate::platform::proc::AgentProcessGroup`].
    pub process_group: crate::platform::proc::AgentProcessGroup,
}

/// PRD #92 F8: hardcoded grace window between SIGTERM and the SIGKILL
/// fallback used by the single-pane Ctrl+W path
/// ([`crate::platform::proc::terminate_child_with_grace_and_wait`]) and as the
/// poll budget in the daemon-wide [`AgentPtyRegistry::shutdown_all_graceful`].
/// 3 s matches the F1 graceful-shutdown grace, which is the natural
/// sibling. Hardcoded as a constant for now (one symbol to find) rather
/// than lifted to `DashboardConfig` until a real user need surfaces.
/// `pub` so the wrapper-escalation test can assert against the real deadline
/// instead of duplicating the number — a change here must keep that test honest.
pub const AGENT_TERMINATE_GRACE: Duration = Duration::from_secs(3);

/// Divisor giving the wrapper's grace from the deck's. See
/// [`WRAP_TERMINATE_GRACE`] for why this is a fraction and not a thin
/// subtraction.
pub(crate) const WRAP_GRACE_DIVISOR: u32 = 2;

/// The wrapper's own SIGTERM→SIGKILL grace, deliberately SHORTER than
/// [`AGENT_TERMINATE_GRACE`].
///
/// A wrapped agent sits two levels below the deck, in a process group the deck
/// cannot signal: portable-pty `setsid`s the wrapper, then the wrapper
/// `setsid`s the agent again so it can own the inner PTY as its controlling
/// terminal ([`crate::wrap`]). So `killpg(wrapper_pgid, …)` reaches the wrapper
/// ONLY — the agent is reachable exclusively via the wrapper forwarding to its
/// own child group.
///
/// Both graces used to be `AGENT_TERMINATE_GRACE`, which made teardown a race
/// the wrapper could not win: the deck SIGTERMs the wrapper at T0 and SIGKILLs
/// it at T0+grace, while the wrapper forwarded SIGTERM at ~T0 and armed its own
/// escalation for ~T0+grace. Both deadlines fell together and the wrapper's is
/// only checked on a reap-loop tick, so the deck killed the wrapper first and an
/// agent that had not exited on SIGTERM — a wedged agent, or any interactive
/// shell, which ignores SIGTERM — was orphaned to init.
///
/// Halving makes the wrapper always escalate first, so the agent is dead before
/// the deck's SIGKILL removes the only process that can signal it.
///
/// A fraction, not a thin subtraction: the wrapper's chain is observe-signal →
/// forward → wait out its grace → `SIGKILL` → reap, and the *observe* step
/// depends on where the reap loop sits in its 50 ms cadence and how loaded the
/// host is. A host running dozens of agents can stretch that tick, and any
/// overrun puts the agent back to being orphaned — so the headroom should not be
/// a couple of scheduler quanta. Half the budget cannot be eaten that way.
/// Measured through `close_agent`, the wrapper escalates 1.503 s after observing
/// SIGTERM, against the deck's 3.0 s.
///
/// The cost is deliberate: a slow-but-honest agent gets half as long to exit on
/// SIGTERM before the wrapper escalates. Orphaning a live agent process is the
/// worse outcome, and the deck's own grace is unchanged as the outer bound.
pub(crate) const WRAP_TERMINATE_GRACE: Duration =
    Duration::from_millis(AGENT_TERMINATE_GRACE.as_millis() as u64 / WRAP_GRACE_DIVISOR as u64);

// The ordering above is load-bearing, not stylistic: if these ever became equal
// (or inverted) the orphan bug returns silently, so pin it at compile time.
const _: () = assert!(
    WRAP_TERMINATE_GRACE.as_millis() < AGENT_TERMINATE_GRACE.as_millis(),
    "the wrapper must escalate to SIGKILL strictly before the deck kills the wrapper"
);

// PRD #42 M1: the process-group teardown helpers (`pid_to_pgid`,
// `signal_child_pgroup_or_fallback`, `force_kill_child_and_wait`,
// `terminate_child_with_grace_and_wait`) moved to `crate::platform::proc`,
// where the Unix `killpg`/SIGTERM→SIGKILL logic lives behind the platform seam
// and a Windows Job-Object backend lands in PRD #163. Call sites below use
// `crate::platform::proc::*`.

fn force_kill_and_wait(pty: &mut AgentPty) {
    crate::platform::proc::force_kill_child_and_wait(&mut pty.child, &pty.process_group);
}

/// RAII guard that owns a freshly-spawned child between the `spawn_command`
/// call and the point at which ownership is handed off to an [`AgentPty`].
/// If the guard is dropped while still holding the child (e.g. because a
/// later step in [`spawn`] like `take_writer` or `try_clone_reader` returned
/// an error, or a panic unwound through the spawn path), the child is
/// force-killed and reaped so no orphan process is left behind.
///
/// It carries the child's [`crate::platform::proc::AgentProcessGroup`] alongside
/// it (PRD #163 M3) so this early-failure teardown reaps the whole descendant
/// tree, exactly like the registry's later teardown paths — otherwise a spawn
/// that failed *after* the child had already forked would leak its descendants
/// on Windows.
struct ChildGuard {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    process_group: crate::platform::proc::AgentProcessGroup,
}

impl ChildGuard {
    fn new(
        child: Box<dyn portable_pty::Child + Send + Sync>,
        process_group: crate::platform::proc::AgentProcessGroup,
    ) -> Self {
        Self {
            child: Some(child),
            process_group,
        }
    }

    fn take(
        mut self,
    ) -> (
        Box<dyn portable_pty::Child + Send + Sync>,
        crate::platform::proc::AgentProcessGroup,
    ) {
        let child = self.child.take().expect("ChildGuard already taken");
        // `self` still drops after this (it owns the group), and its `Drop` is a
        // no-op once the child is gone, so hand the real group out and leave the
        // guard with the empty `Default` one.
        (child, std::mem::take(&mut self.process_group))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            crate::platform::proc::force_kill_child_and_wait(&mut child, &self.process_group);
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
    // Issue #747: through the shared helper, so this stays a mirror of
    // `resize` by construction rather than by two copies staying in step.
    let (rows, cols) = clamp_pty_dims(opts.rows, opts.cols);

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
    // The endpoint vars, for the same reason as `PANE_ID` above but with a
    // worse failure mode: a mistagged pane id merely misroutes within one
    // deck, whereas an inherited endpoint points the child at a *different
    // deck's daemon* entirely.
    //
    // Observed in production on 2026-07-29. `cargo test-e2e` run from inside
    // a deck pane inherits that pane's `DOT_AGENT_DECK_SOCKET`. A test that
    // builds a bare `AgentPtyRegistry` (no `hook_socket`, so the injector
    // below at `spawn_agent` has nothing to fill the gap with) and spawns a
    // real agent without pinning its own socket therefore hands that agent
    // the developer's LIVE daemon: its `SessionStart` was ingested by the
    // real deck, which painted a card for the test's synthetic pane id
    // ("worker-pane") on the user's dashboard.
    //
    // `ATTACH_SOCKET` matters more, not less: the attach endpoint is what
    // `daemon stop` speaks, so an inherited one lets a child address the
    // control plane of a daemon it does not own.
    //
    // Scrubbing is safe because every legitimate producer supplies the value
    // explicitly and therefore still wins under the scrub-then-overlay order:
    // `spawn_agent` injects the registry's own `hook_socket`, and callers that
    // pin their own (tests, `respawn_agent_for_pane` replaying `spawn_env`)
    // pass it through `opts.env`. What changes is only the *unpinned* case,
    // which goes from "silently addresses whichever daemon happens to be in
    // the ambient environment" to "no endpoint" — a child that emits nowhere
    // rather than into a stranger's deck.
    //
    // `DOT_AGENT_DECK_STATE_DIR` is deliberately NOT scrubbed: no agent-side
    // flow reads it (the CLI paths agents invoke — `delegate`, `work-done` —
    // are endpoint-only), so removing it would be churn without a failure
    // mode to point at.
    cmd.env_remove(DOT_AGENT_DECK_SOCKET);
    cmd.env_remove("DOT_AGENT_DECK_ATTACH_SOCKET");

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

    // PRD #163 M3: adopt the child into the OS grouping its teardown will use, as
    // early as possible. This is a no-op on Unix (`portable-pty` already `setsid`'d
    // the child into its own process group, which `killpg` addresses by pid) and
    // creates + populates the agent's Job Object on Windows. It has to happen here
    // rather than at teardown because job membership is inherited forward only: a
    // job joined later would not contain the descendants the child had already
    // spawned. Infallible by contract — a Windows job quirk degrades teardown to a
    // single-process kill (logged) instead of failing an otherwise-healthy spawn.
    let process_group = crate::platform::proc::AgentProcessGroup::adopt(child.process_id());

    // Wrap the freshly-spawned child in an RAII guard *before* any fallible
    // step below: a failure in `take_writer` / `try_clone_reader` (or a
    // panic between them) would otherwise orphan the child. The guard is
    // taken on the success path and its child moved into the AgentPty.
    let child_guard = ChildGuard::new(child, process_group);

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

    let (child, process_group) = child_guard.take();
    Ok(AgentPty {
        child,
        master: pair.master,
        writer,
        reader,
        process_group,
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
///
/// Worker-exit sweep: loop exit is also the daemon's earliest, unconditional
/// "this process is gone" signal, so — for a process that died WITHOUT the
/// daemon's own doing — it retires any armed
/// `OutstandingDelegation`/`SilenceWatchRecord` for this pane via
/// [`AgentPtyRegistry::sweep_delegations_on_exit`] — instead of leaving
/// either record armed for its full timeout window when the pane's process
/// exited on its own, without ever calling `work-done` or going through an
/// explicit `StopAgent` close. `pane_id_env` is `None` for a spawn that
/// never carried one (most unit-test fixtures), in which case there is
/// nothing in the tracker keyed by it and the sweep is skipped.
///
/// When the sweep returns a non-empty `Vec<OutstandingDelegation>`,
/// each record for which `pane_id` (the pane that just reached EOF) was the
/// **worker** side — i.e. `record.orchestrator_pane_id != pane_id`, which rules
/// out records this pane only touched as the *orchestrator* that issued them —
/// gets [`AgentPtyRegistry::deliver_worker_exited_notice`]'s "exited without
/// work-done" notice delivered to its orchestrator pane. That delivery is
/// `async` (it goes through the identity-guarded
/// [`AgentPtyRegistry::write_notice_guarded`]), but this function runs on a
/// bare `std::thread` with no `tokio` runtime context of its own, so it cannot
/// simply `.await` it. `runtime_handle` is a [`tokio::runtime::Handle`]
/// captured with `try_current()` (never `current()`, which panics outside a
/// runtime) at the SAME moment this thread was spawned — see
/// [`AgentPtyRegistry::spawn_agent`]'s call site — and used here to
/// `handle.spawn` the notice delivery onto that runtime instead. A `None`
/// handle means `spawn_agent` itself ran with no runtime in scope (every
/// production spawn happens inside the daemon's async request handling or a
/// `tokio::spawn`ed dispatch task, so this is a synchronous unit-test fixture
/// that never exercises this path) — in that case the notice cannot be
/// delivered at all, which is logged rather than attempted.
///
/// "Without the daemon's own doing" is load-bearing: [`AgentPtyRegistry::close_agent`]
/// and [`AgentPtyRegistry::respawn_agent_for_pane`] BOTH remove the agent's
/// entry from the registry BEFORE killing its child, so by the time THIS
/// thread's `read` unblocks on the resulting EOF, `agent_id` no longer names
/// a live entry — checked via [`AgentPtyRegistry::is_agent_still_registered`]
/// — and the sweep is skipped. Both of those callers already own the sweep
/// decision for their own kill: `close_agent` deliberately performs none on
/// its own, the `StopAgent` daemon-protocol handler wraps it with an explicit
/// `begin_pane_close`/`finish_pane_close` when IT wants one, and respawn
/// deliberately lets an outstanding delegation carry forward to whichever
/// agent next occupies the pane. Only a still-registered entry — nothing
/// else has removed it, so nothing else has decided what to do about its
/// death yet — is this thread's own natural-exit signal to act on.
///
/// `registry` is a [`Weak`] reference, deliberately NOT an owned `Arc`: this
/// thread is detached and only exits once the child's PTY reaches EOF, so an
/// owned `Arc<AgentPtyRegistry>` held here would keep the registry alive for
/// as long as the thread runs — which would be a reference cycle, not
/// merely backwards: `AgentPtyRegistry`'s own `Drop` is what calls
/// `shutdown_all` to kill any still-live children in the first place, so a
/// strong ref held by a thread that only exits once its child is killed
/// means neither side can ever finish. A `Weak` upgrade fails harmlessly
/// if the registry has already been dropped by the time EOF is observed —
/// there is nothing left to sweep in that case either way.
///
/// Once `upgrade()` succeeds, though, a genuine strong `Arc` exists for the
/// rest of this EOF handling, and a clone of it is moved into the
/// `handle.spawn`ed notice-delivery task. If that upgraded `Arc` (or its
/// clone) turns out to be the LAST strong reference — the daemon has
/// already released its own — dropping it runs `AgentPtyRegistry::drop` →
/// `shutdown_all` on this reader thread, or on a tokio worker when the
/// spawned task is later dropped. Harmless in practice (`shutdown_all` is a
/// drain plus kills, and the daemon's own `Arc` outlives this in the normal
/// case), but it means `Weak` does not remove this thread from the
/// registry's lifetime entirely — only from keeping it alive unconditionally.
#[allow(clippy::too_many_arguments)]
fn pump_reader(
    mut reader: Box<dyn std::io::Read + Send>,
    bus: Arc<AgentBus>,
    exited: Arc<AtomicBool>,
    change_notify: Arc<Notify>,
    registry: Weak<AgentPtyRegistry>,
    agent_id: String,
    pane_id_env: Option<String>,
    runtime_handle: Option<tokio::runtime::Handle>,
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
    // Issue #584: release anyone waiting on THIS agent's liveness before the
    // delegation sweep below, so a delegate parked on its replacement's
    // readiness learns the replacement is gone at EOF rather than at the end of
    // a fixed 30 s window. A dropped registry leaves nothing to wake.
    if let Some(registry) = registry.upgrade() {
        registry.signal_agent_exit(&agent_id);
    }
    if let Some(pane_id) = pane_id_env.as_deref()
        && let Some(registry) = registry.upgrade()
        && registry.is_agent_still_registered(&agent_id)
    {
        let swept = registry.sweep_delegations_on_exit(pane_id, &agent_id);
        // Worker-exit sweep: only the records for which THIS pane was the WORKER
        // side warrant an "exited without work-done" notice — a record this
        // pane touched only as the orchestrator that issued it (its own
        // `orchestrator_pane_id == pane_id`) means a DIFFERENT, still-live
        // worker pane's delegation, and the orchestrator that would receive
        // the notice is the pane that just exited, so there is nobody to
        // notify.
        let worker_exits: Vec<OutstandingDelegation> = swept
            .into_iter()
            .filter(|delegation| delegation.orchestrator_pane_id != pane_id)
            .collect();
        if !worker_exits.is_empty() {
            match runtime_handle {
                Some(handle) => {
                    let pane_id = pane_id.to_string();
                    let registry = registry.clone();
                    handle.spawn(async move {
                        for delegation in worker_exits {
                            registry
                                .deliver_worker_exited_notice(&pane_id, delegation)
                                .await;
                        }
                    });
                }
                None => {
                    tracing::warn!(
                        pane_id = %pane_id,
                        count = worker_exits.len(),
                        "pane EOF: a worker exited without work-done, but no tokio runtime \
                         handle was captured at spawn time, so its notice could not be delivered"
                    );
                }
            }
        }
    }
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
    pub writer: Arc<AsyncMutex<PaneWriter>>,
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

/// Issue #424 H5 (reviewer MEDIUM): a guarded send's outcome, carrying the one
/// refusal reason [`GuardedSend`] flattens away.
///
/// The detached confirmation loop pre-checks the user-input clock itself so it
/// can report the specific stop on the pane's card, but the user can type
/// between that check and the writer-held backstop. The backstop then refuses —
/// correctly — and the caller saw only `Stale`, which it logs as "target went
/// stale" and returns on, publishing no `DeliveryNotice` and no Error card. That
/// is the promised terminal report going missing precisely in the race the
/// backstop exists for.
///
/// A separate type rather than a sixth `GuardedSend` variant on purpose:
/// `GuardedSend` is the vocabulary the wire mapping and all three delivery paths
/// already classify, and a user-input refusal IS a `Stale` in that vocabulary —
/// refused with no bytes written. Nothing about the contract changes; only the
/// detached loop asks the finer question. See [`Self::outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedSendDetail {
    /// Exactly what [`GuardedSend`] reports, verbatim.
    Outcome(GuardedSend),
    /// Refused because the user has typed into this pane since the bytes this
    /// send would repeat (a replacement payload) or submit (a blind probe)
    /// landed there. Terminal, and reportable.
    RefusedUserInput,
}

impl GuardedSendDetail {
    /// Flatten to the vocabulary every existing caller and the wire mapping use.
    pub fn outcome(self) -> GuardedSend {
        match self {
            Self::Outcome(outcome) => outcome,
            Self::RefusedUserInput => GuardedSend::Stale,
        }
    }
}

/// PRD #20 R20-003/R20-006: the live target that currently owns a pane, resolved
/// atomically for the identity-guarded send path. Bundles the shared writer with
/// the identity/liveness needed to re-validate after the writer lock is acquired.
struct PaneWriterTarget {
    writer: Arc<AsyncMutex<PaneWriter>>,
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
/// its configured terminator to a PTY writer, classifying WHERE a write error
/// struck. The terminator is the [`SubmitMode`]'s tail — a submit CR for
/// [`SubmitMode::Submit`], a bare LF for [`SubmitMode::Notice`] (PRD #249 M3) —
/// so the classification is written in terms of "payload then tail", not of the
/// CR specifically.
#[derive(Debug, PartialEq, Eq)]
enum PayloadDelivery {
    /// Payload and the mode's terminator both fully written.
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

/// PRD #249 M3: the [`SubmitMode::Notice`] counterpart of
/// [`deliver_payload_and_submit`] — payload, then a single `\n`, with no
/// `SUBMIT_DELAY` and no CR. Shares the partial-write classification for the same
/// reason: a notice whose payload landed but whose LF did not leaves the target
/// holding un-terminated bytes, which a blind retry would duplicate into the
/// pane.
///
/// No submit delay because there is nothing to keep the terminator from fusing
/// to: LF is not an Enter for the agents this project drives, so the pause that
/// `SUBMIT_DELAY` exists to create has no meaning here (matching
/// [`AgentPtyRegistry::write_to_pane_notice`]'s unguarded path).
async fn deliver_payload_as_notice(
    w: &mut (dyn std::io::Write + Send),
    payload: &[u8],
) -> PayloadDelivery {
    match write_all_tracked(w, payload) {
        WriteProgress::Complete => {}
        WriteProgress::Partial => return PayloadDelivery::Ambiguous,
        WriteProgress::NothingWritten(e) => return PayloadDelivery::CleanFailure(e),
    }
    let _ = w.flush();
    match write_all_tracked(w, b"\n") {
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
    /// PRD #163 M3 — the agent's descendant-tree grouping, moved here from
    /// [`AgentPty::process_group`] at insert time and handed to every teardown
    /// path (`close_agent`, `respawn_agent_for_pane`, `shutdown_all`,
    /// `shutdown_all_graceful`). Zero-sized on Unix; the agent's Job Object
    /// handle on Windows, which must stay alive for as long as the agent does or
    /// `TerminateJobObject` would have nothing to terminate.
    pub process_group: crate::platform::proc::AgentProcessGroup,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub writer: Arc<AsyncMutex<PaneWriter>>,
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
    ///
    /// PRD #225 M2: this is the OBSERVED / display identity — it starts as the
    /// spawn-time identity and is upgraded in place by
    /// [`AgentPtyRegistry::set_agent_type`] when a hook event reveals the real
    /// agent. It drives the badge ONLY. The launch shape is decided from
    /// [`RunningAgent::spawn_agent_type`], so learning a type from hooks can
    /// never rewrite the exec line.
    pub agent_type: Option<AgentType>,
    /// PRD #225 M2: the SPAWN-TIME identity that drove this pane's launch shape
    /// — i.e. the caller-supplied [`SpawnOptions::agent_type`] as it was at this
    /// child's spawn. It is the LAUNCH-side field:
    /// [`AgentPtyRegistry::respawn_agent_for_pane`] reads it and
    /// [`AgentPtyRegistry::set_agent_type`] never writes it, which is what keeps
    /// a hook-learned badge out of the exec line.
    ///
    /// Defect 2 was exactly the absence of this split: a `devbox run codex-big`
    /// pane spawns UNWRAPPED (`AgentType::from_command` can't see through the
    /// launcher), Codex's native hooks then teach the registry `Some(Codex)`
    /// purely for the badge, and the first respawn replayed that learned value
    /// into `SpawnOptions::agent_type` — so `spawn` resolved a Wrapper-strategy
    /// agent and the SAME pane came back up as `dot-agent-deck wrap --agent
    /// codex -- devbox run codex-big`. A value recorded for display must not
    /// change how the pane launches.
    ///
    /// It is a FALLBACK, not an override: a respawn derives the wrap decision
    /// from the command it is actually launching and consults this field only
    /// when that command implies no agent type (the `devbox run codex-big`
    /// shape). See the invariant spelled out in
    /// [`AgentPtyRegistry::respawn_agent_for_pane`] for why the derivation has to
    /// come first. So the value tracks the identity of the command each child was
    /// launched with — stable for a pane whose role command never changes, and
    /// re-derived (not stale) for one whose command was edited.
    ///
    /// `None` means "no explicit identity, and the command implied none" — the
    /// launch decision then falls back to parsing the command in [`spawn`], which
    /// is deterministic for the same command and so reproduces the same exec
    /// line.
    pub spawn_agent_type: Option<AgentType>,
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
    /// Issue #454 round-3 audit (finding 4): set once, and never cleared, when a
    /// LATER generation is published onto this record's `pane_id_env`.
    ///
    /// This is what makes disownership MONOTONE. The retirement rule in
    /// [`AgentPtyRegistry::generation_ownership`] lets an exited generation keep
    /// answering for its own pane until another generation claims it, and the
    /// first implementation of "another generation claims it" was a *live*
    /// lookup — so ownership came BACK when the successor exited in turn:
    /// `A` exits on `P`, `B` takes `P`, `B` exits, and `A` was suddenly the
    /// pane's owner again with neither record reaped. A retired generation
    /// regaining its pane is exactly the resurrection the round-2 fix set out to
    /// forbid, and it re-opened the stale-report chain behind it.
    ///
    /// Registry ids only ever increment and are never reused, so "a later
    /// generation has claimed this pane" is a monotone predicate: once true it
    /// can never become false again. Recording it on the record it disowns makes
    /// it exactly that, needs no clock, no reaper and no second map, and is
    /// bounded by the records themselves — a reaped record takes its flag with
    /// it.
    ///
    /// Set at PUBLISH, not at reservation. A reservation that never becomes an
    /// agent did not change the pane's hands, and ending the predecessor's grace
    /// period on a spawn that failed would drop the final `SessionEnd` this
    /// grace exists to catch. The pending reservation still disowns the
    /// predecessor for as long as it is outstanding — see
    /// [`AgentPtyRegistry::pane_claimed_by_other`] — so the window is covered at
    /// both ends without making a failed spawn permanent.
    pub pane_handed_over: bool,
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
    /// PRD #745 M11: the wall-clock instant at which **this registry forked
    /// this child**, or `None` when the registry did not fork it.
    ///
    /// It is an OBSERVATION, not an inference. [`AgentPtyRegistry::spawn_agent`]
    /// stamps it immediately after `spawn(opts)` returns — the first moment the
    /// process exists — and nothing ever rewrites it. It is the only site that
    /// writes `Some`; every other way a record comes into being leaves it
    /// `None`, which is why the absent case needs no guard anyone has to
    /// remember.
    ///
    /// **`DateTime<Utc>`, not `Instant`.** A monotonic instant is meaningless in
    /// another process, so it cannot cross the wire at all; the value is
    /// serialized as epoch milliseconds on [`AgentRecord::spawned_at_ms`], the
    /// unit `Date.now()` speaks.
    ///
    /// **A respawn does not carry it over, and that is the feature.** A
    /// `clear = true` delegate removes this record outright and
    /// [`AgentPtyRegistry::spawn_agent`] mints a fresh one
    /// (see [`AgentPtyRegistry::respawn_agent_for_pane_declared`]), so a
    /// restarted worker reports the age of its CURRENT iteration while a role
    /// nobody has restarted — an orchestrator, typically — reports its whole
    /// lifetime. No "which duration is this?" flag is needed, because the two
    /// answers come from two different records.
    pub spawned_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl RunningAgent {
    /// PRD #386 M3: `true` when this pane's PTY child has a transitive
    /// descendant sitting in a **different POSIX session** than the child
    /// itself — i.e. something the agent `setsid`-detached off the pane's
    /// terminal is still alive, so the pane is actively busy even if no
    /// agent-emitted hook/wrapper event says so (the gap this closes: an agent
    /// shelling out to a long-running command with no event in between reports
    /// stale `Idle`).
    ///
    /// **This replaced PRD #370's `tcgetpgrp` body, which never fired in any
    /// real pane.** Claude Code runs its Bash-tool child on pipes, in a session
    /// of its own, off the pane's PTY entirely — so the pane's foreground pgid
    /// never moves and the old body computed `pid != pid` → `Some(false)`,
    /// permanently. The descendant scan asks the question the process topology
    /// can actually answer; see [`crate::platform::proc::descendant_shell_activity`]
    /// for the discriminator and for the CI trap it must never fall into.
    ///
    /// `shapes` is the optional argv cross-check, and it is a **veto**: pass
    /// only the shapes that were measured against *this* pane's agent kind, and
    /// an empty slice for an agent whose shape has never been measured (see
    /// [`crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES`] and
    /// [`AgentPtyRegistry::shell_foreground_busy_snapshot`], which does that
    /// selection). Handing every pane one agent's fingerprint would silently
    /// suppress the signal for all the others.
    ///
    /// `None` when there is no signal to act on: the platform can't enumerate
    /// processes at all (`crate::platform::proc::process_table` returns `None`
    /// unconditionally on Windows — see that module), this child's own pid is
    /// unavailable, or the child is not in the sampled table. Callers must treat
    /// `None` as "no opinion", never as "not busy".
    pub fn shell_foreground_busy(
        &self,
        shapes: &[crate::platform::proc::ShellToolShape],
    ) -> Option<bool> {
        let table = crate::platform::proc::process_table()?;
        self.shell_activity_in(&table, shapes)
    }

    /// The classification half of [`Self::shell_foreground_busy`], against a
    /// table the caller already sampled.
    ///
    /// Split out so the daemon's poll loop pays for **one** `ps -A` per tick
    /// rather than one per pane (PRD #386's Technical Approach, Route A:
    /// "parsed once into a table and reused for *every* pane in that poll"),
    /// and so every pane in a tick is classified against one consistent sample.
    fn shell_activity_in(
        &self,
        table: &[crate::platform::proc::ProcessInfo],
        shapes: &[crate::platform::proc::ShellToolShape],
    ) -> Option<bool> {
        let shell_pid = self.child.process_id()? as i32;
        crate::platform::proc::descendant_shell_activity(table, shell_pid, shapes)
    }
}

/// PRD #386 M3, Open Question 2 — which entry of
/// [`crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES`], if any, applies to a
/// pane running `agent_type`.
///
/// Only Claude's shell-tool argv shape was ever measured, so only a Claude pane
/// selects one; every other agent kind (and every pane whose kind is not known
/// yet, including a bare shell pane) selects nothing and is classified by the
/// structural session-id test alone. That asymmetry is the whole point: the argv
/// cross-check is a veto, so applying Claude's fingerprint to a Codex/OpenCode/Pi
/// pane would reject a genuinely detached descendant and leave the pane reading
/// `Idle` with nothing logged — a silent false negative, which is exactly the
/// #370 failure mode this PRD exists to end. Structural-only over-triggers at
/// worst, which is visible and fixable.
///
/// Keyed off [`crate::platform::proc::ShellToolShape::agent`] rather than a
/// string literal so the catalog and this mapping cannot drift apart.
fn shell_tool_shape_key(agent_type: Option<&AgentType>) -> Option<&'static str> {
    match agent_type {
        Some(AgentType::ClaudeCode) => Some(crate::platform::proc::CLAUDE_BASH_TOOL_SHAPE.agent),
        _ => None,
    }
}

/// One live pane a shell-activity sample could classify, resolved under the
/// registry lock by [`AgentPtyRegistry::shell_activity_candidates`] and then
/// classified without it by [`AgentPtyRegistry::classify_shell_activity`].
///
/// Deliberately **owned and lock-free**: it exists so the "is there anything to
/// classify?" question (issue #493) and the fork/exec that answers it can be
/// separated, with the registry lock held for neither the sample nor the
/// classification. Carrying the per-pane `shapes` rather than the whole catalog
/// keeps PRD #386's Open Question 2 resolved in exactly one place — the pane's
/// agent kind is only visible under the lock, so the selection has to happen
/// here (see [`shell_tool_shape_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellActivityCandidate {
    /// The pane's spawn-time `DOT_AGENT_DECK_PANE_ID`.
    pub pane_id: String,
    /// The pid of the pane's PTY child — the root of the descendant walk.
    pub shell_pid: i32,
    /// The argv cross-check shapes measured against *this* pane's agent kind.
    /// Empty for every kind that has never been measured, which leaves the
    /// structural session-id test standing alone.
    pub shapes: Vec<crate::platform::proc::ShellToolShape>,
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
    /// PRD #745 M11: when the daemon spawned this agent's process, as a count
    /// of milliseconds since the Unix epoch (UTC). Copied from
    /// [`RunningAgent::spawned_at`].
    ///
    /// **Spawn time, not [`crate::state::SessionState::started_at`], and the
    /// difference is the point.** `started_at` is EVENT-derived: a session only
    /// exists once a hook event has arrived, so an agent that has never emitted
    /// one has no start instant at all — which is exactly the agent whose
    /// uptime a reader most wants — and the hydration path invents it as
    /// `Utc::now()` when `pane_started_at` has no entry
    /// (`AppState::insert_placeholder_session`). A spawn is something the
    /// daemon DID, so it is signal-independent and needs no inference.
    ///
    /// **Never invented.** `None` means the daemon cannot vouch for a spawn:
    /// a record minted from an older daemon's id-only `ListAgents` reply
    /// (`daemon_client::list_agents`), a peer predating this field, or the
    /// synthetic test seam. Every consumer renders nothing for it — no dash, no
    /// placeholder — which is the same disposition
    /// [`crate::state::SessionSnapshot::last_activity_ms`] takes and the reason
    /// a duration is shippable here where one built on `started_at` was not.
    ///
    /// **Epoch milliseconds for the same four reasons M9 recorded**, and the
    /// unit is in the NAME because a seconds/milliseconds slip is a ×1000
    /// error: an integer has no format for two peers to disagree about, it is
    /// what `Date.now()` speaks, chrono's own `DateTime<Utc>` serialization
    /// emits RFC 3339 at nanosecond precision no JavaScript consumer can
    /// represent, and a pre-formatted `"3h"` would bake the client's rounding
    /// and vocabulary into the daemon's contract.
    ///
    /// Additive optional, so no `PROTOCOL_VERSION` bump — the do-not-bump case
    /// this module's own policy names (`crate::daemon_protocol`), the same
    /// basis `live` and `last_activity_ms` were added on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_at_ms: Option<i64>,
}

/// Skip-predicate for `AgentRecord::rows` / `AgentRecord::cols`
/// serialization. Pulled out as a named helper so the two `#[serde]`
/// attributes share one symbol — closure literals aren't allowed in
/// `skip_serializing_if`.
fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

/// Issue #424 F1: what this daemon's guarded sends have put into one pane and
/// believe is still sitting in its input box. See [`PaneInputState`].
#[derive(Default)]
struct AutomaticWrite {
    /// The last SUBMIT-mode guarded write of ours into this pane — a payload or
    /// a bare probe CR. This is the clock the submit-only probe asks about: a
    /// probe is blind, so all it needs to know is whether the user has typed
    /// since the bytes it is about to submit were put there.
    ///
    /// Issue #424 H2 (both reviewers): a [`SubmitMode::Notice`] deliberately
    /// does NOT advance it. A notice is LF-terminated, so it accumulates in the
    /// input box *above* whatever the user has typed rather than replacing it —
    /// the documented [`AgentPtyRegistry::write_to_pane_notice`] contract. An
    /// any-write clock therefore let an ordinary orchestrator notice landing
    /// between the user's draft and a later blind probe make that draft look
    /// older than our last write, and the probe then submitted draft + notice as
    /// one turn. The silent-worker notice is a production `Notice` caller that
    /// fires inside the 60 s confirmation window, so that interleaving is
    /// ordinary, not hypothetical.
    submitted_at: Option<Instant>,
    /// ONE ENTRY PER GUARDED PAYLOAD WRITE that no delivery has released yet,
    /// oldest first — a multiset, not a set.
    ///
    /// Issue #424 H3 (both reviewers): this used to be a single "last payload"
    /// slot, which failed in BOTH directions. An independent guarded submit of
    /// different bytes replaced the slot, so an older delivery's replacement no
    /// longer matched anything and was admitted — a different automatic submit
    /// launched exactly the laundering a notice would have. And the slot was
    /// never cleared, so once the user typed, that payload was refused into that
    /// pane forever, which is the prompt-loss half of #424 itself.
    ///
    /// Issue #424 S2 (both reviewers): keying it per DISTINCT payload was still
    /// not delivery-scoped. Two deliveries writing the SAME bytes into one pane
    /// deduplicated into one entry, so the first of them to finish released the
    /// other's guard as well — after which the survivor's replacement was
    /// admitted on top of an unsent draft and submitted both. One entry per
    /// WRITE, released one at a time, gives each delivery its own unit of guard
    /// without needing a delivery id at a seam that has none: N live writes of
    /// the same bytes need N releases before the bytes stop being guarded.
    /// Cleared on the lifecycle points in [`PaneInputState::note_user_bytes`] /
    /// [`PaneInputState::forget_payload`] / [`PaneInputState::forget_pane`],
    /// plus the [`PAYLOAD_RECORD_TTL`] backstop.
    payloads: Vec<PayloadWrite>,
}

/// The bytes of one automatic payload write, and when they landed.
///
/// The digest is of the ENCODED payload (post
/// [`crate::pane_input::encode_pane_payload`]) — the bytes that actually reached
/// the input box, which is what a repeat would double. A 64-bit hash rather than
/// the text itself so the map stays small no matter how long a prompt is; the
/// only consequence of the vanishingly unlikely collision is that one delivery
/// is refused and reported, never that one is silently sent.
#[derive(Clone, Copy)]
struct PayloadWrite {
    digest: u64,
    at: Instant,
    /// The user has SUBMITTED this pane's input box since these bytes were
    /// written, so they are no longer sitting in it.
    ///
    /// Issue #424 S2: the entry is MARKED rather than removed. It guards
    /// nothing from here on, but it stays until its own delivery releases it,
    /// so that release consumes THIS entry instead of silently consuming a
    /// later delivery's live one — the same shared-ownership fail-open the
    /// per-write multiset exists to close.
    drained: bool,
}

/// Issue #424 H3: how long a payload record can still be guarding a live
/// delivery. [`crate::prompt_delivery::AUTOMATIC_PROMPT_DEADLINE`] bounds every
/// retry chain in the daemon, so a record older than it belongs to a delivery
/// that has already reached a terminal outcome and can only refuse an unrelated
/// future one. The explicit clears are the primary lifecycle; this is the
/// backstop that keeps a delivery whose completion this daemon never observes
/// (a TUI-confirmed one) from bricking its own payload.
const PAYLOAD_RECORD_TTL: Duration = crate::prompt_delivery::AUTOMATIC_PROMPT_DEADLINE;

/// Issue #424 H3: how many unreleased payload writes one pane may have on
/// record. Deliveries into a single pane are serialized by the writer and
/// bounded by the deadline above, so this is a runaway backstop rather than an
/// operational limit; the OLDEST record is evicted, which can only ever admit a
/// repeat, never refuse a first write.
const MAX_PAYLOAD_RECORDS_PER_PANE: usize = 8;

/// Issue #424 S1: the bytes an xterm-style client sends around a bracketed
/// paste. Both markers share the first four bytes, so the scanner below matches
/// that prefix once and disambiguates on the fifth.
const PASTE_MARKER_PREFIX: &[u8] = b"\x1b[20";

/// Issue #424 S1 (both reviewers): where one pane's user-input stream is, as far
/// as the submit-drain needs to know.
///
/// The drain has to answer "did the user SUBMIT the input box", and the only
/// evidence the daemon has is the bytes it forwards. Two separate things make
/// "scan for a CR or an LF" the wrong answer, and each one produced the same
/// unsafe outcome: the records of a box that still held our payload AND the
/// user's fresh draft were cleared, after which the replacement no longer
/// recognized itself as a repeat and submitted payload + draft + payload as one
/// turn — the precise outcome the guard exists to prevent.
///
/// **Framing.** The real TUI forwards `ESC[200~…\n…ESC[201~` when the child
/// advertises bracketed paste. Those newlines are EDITOR CONTENT, and an agent
/// TUI in paste mode stores them without submitting anything. That is what
/// `matched` / `is_start` / `in_paste` track.
///
/// **The keypress behind the byte.** Outside a paste, whether a byte submits is
/// not this module's call to make: it is fixed by the deck's own forwarding
/// contract, so it is asked of [`crate::ui::user_byte_submits_input_box`], which
/// sits next to the encoder that produced the byte. `Ctrl+J` is forwarded as
/// exactly an LF and `Alt+Enter` as exactly `ESC` `CR`, both of which are
/// NEWLINE keys the user pressed to keep typing — reading them as submissions is
/// how a plain draft, with no paste and no attacker anywhere near it, reached
/// the doubled-submit above.
///
/// The state is carried ACROSS calls because a paste — and equally an
/// `Alt+Enter` — arrives as however many writes the client happens to make; a
/// marker or a two-byte frame split between two of them still matches.
/// Interleaving from two attached clients can leave `in_paste` stuck true or
/// misattribute one client's ESC to another's CR, and both of those suppress
/// drains — that direction only refuses a later same-payload delivery (reported,
/// and bounded by [`PAYLOAD_RECORD_TTL`]), never admits a doubled one.
#[derive(Default)]
struct UserInputStream {
    /// How many bytes of a paste marker have matched so far.
    matched: usize,
    /// Once the fifth byte disambiguates, whether it is the START marker.
    is_start: bool,
    in_paste: bool,
    /// The byte before the one being fed, which is all
    /// [`crate::ui::user_byte_submits_input_box`] needs to tell a plain `Enter`
    /// from the ESC-prefixed `Alt+Enter`. `None` only at the very start of the
    /// stream, where no prefix can have been sent.
    preceding: Option<u8>,
}

impl UserInputStream {
    /// Feed the user's bytes; `true` if any of them submits the input box.
    ///
    /// Every byte is fed, deliberately: this is a state machine, so a
    /// short-circuiting `any` would stop tracking paste state at the first
    /// terminator and mis-read the rest of the buffer.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut submitted = false;
        for byte in bytes {
            submitted |= self.feed_byte(*byte);
        }
        submitted
    }

    /// Feed one byte; `true` if it submits, OUTSIDE a bracketed paste.
    fn feed_byte(&mut self, byte: u8) -> bool {
        // Unconditional, and before the marker matcher's early returns: every
        // byte is somebody's predecessor, including the ones consumed as part
        // of a paste marker.
        let preceding = self.preceding.replace(byte);
        loop {
            if self.matched < PASTE_MARKER_PREFIX.len() {
                if PASTE_MARKER_PREFIX[self.matched] == byte {
                    self.matched += 1;
                    return false;
                }
            } else if self.matched == PASTE_MARKER_PREFIX.len() {
                if byte == b'0' || byte == b'1' {
                    self.is_start = byte == b'0';
                    self.matched += 1;
                    return false;
                }
            } else if byte == b'~' {
                self.in_paste = self.is_start;
                self.matched = 0;
                return false;
            }
            if self.matched == 0 {
                break;
            }
            // Not a marker after all. Restart the match at this byte — no
            // marker byte repeats its own prefix, so one retry is enough.
            self.matched = 0;
        }
        !self.in_paste && crate::ui::user_byte_submits_input_box(preceding, byte)
    }
}

/// Hash the exact bytes a guarded send hands the PTY. In-process comparison
/// only — never persisted, never sent on the wire — so `DefaultHasher`'s
/// across-releases instability does not matter.
fn payload_digest(payload: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.hash(&mut hasher);
    hasher.finish()
}

/// Issue #424 F1/H1: what each pane's input box is holding, as far as this
/// daemon can tell — the two clocks the guarded send consults, under ONE mutex.
///
/// They live together because the transitions that matter are joint: recording a
/// user keystroke and dropping the payload records that keystroke invalidated is
/// one fact about one input box, and a delivery deciding whether to write must
/// not be able to observe half of it.
///
/// Shared (`Arc`) with every agent's [`PaneWriter`], which is what makes the
/// user-input clock ATOMIC with respect to writer handoff — see H1 on
/// [`PaneWriter`].
#[derive(Default)]
struct PaneInputState {
    /// PRD #127 M2.2: the last time a *user* keystroke reached a pane, keyed by
    /// `pane_id_env`. Only bytes written through [`PaneWriter`]'s `Write` impl
    /// (the attach STREAM_IN path) and the explicit
    /// [`AgentPtyRegistry::note_user_input`] update it — daemon-initiated writes
    /// go through [`PaneWriter::daemon`] and do not, so a scheduled delivery
    /// never resets its own debounce clock. In-memory, monotonically growing by
    /// `pane_id_env` seen (negligible).
    user_input_at: HashMap<String, Instant>,
    /// Issue #424 F1: what THIS daemon's guarded sends put into each pane.
    automatic: HashMap<String, AutomaticWrite>,
    /// Issue #424 S1: where each pane's user-input stream is, so that neither a
    /// newline inside a paste nor a newline KEY is read as a submission. See
    /// [`UserInputStream`].
    input: HashMap<String, UserInputStream>,
}

/// A pane id the clocks deliberately ignore: empty, or one of the
/// `<no-pane>` / `<agent-gone>` sentinels, none of which name a real input box.
fn is_sentinel_pane_id(pane_id_env: &str) -> bool {
    pane_id_env.is_empty() || pane_id_env.starts_with('<')
}

impl PaneInputState {
    /// Record that a USER keystroke reached `pane_id_env`.
    fn note_user_input(&mut self, pane_id_env: &str) {
        if is_sentinel_pane_id(pane_id_env) {
            return;
        }
        self.user_input_at
            .insert(pane_id_env.to_string(), Instant::now());
    }

    /// Record the user's actual bytes — the stamp above, plus the one thing the
    /// bytes themselves tell us that a bare clock cannot.
    ///
    /// Issue #424 H3: a terminator SUBMITS the input box. Whatever we had put
    /// there is now the agent's problem and not ours, so every payload record
    /// for this pane stops guarding — which is what lets an ordinary later
    /// delivery of the same fixed text (a delegate worker pointer is
    /// deliberately the same one-line path across hand-offs) be admitted instead
    /// of matching a finished delivery's digest and being refused before writing
    /// a byte. The user-input clock still advances, so the blind probe stays
    /// refused: the box the probe wanted to submit is gone either way.
    ///
    /// Issue #424 S1: "a submission" is decided by [`UserInputStream`] — paste
    /// framing here, the keypress behind the byte in
    /// [`crate::ui::user_byte_submits_input_box`] — and never by scanning for a
    /// raw CR/LF. A multi-line paste carries newlines the agent's editor STORES,
    /// and so do the `Ctrl+J` and `Alt+Enter` the deck deliberately forwards as
    /// newline keys; reading any of them as a submission drained the records of
    /// a box that still held both our payload and the user's draft.
    fn note_user_bytes(&mut self, pane_id_env: &str, bytes: &[u8]) {
        if is_sentinel_pane_id(pane_id_env) || bytes.is_empty() {
            return;
        }
        self.note_user_input(pane_id_env);
        let submitted = self
            .input
            .entry(pane_id_env.to_string())
            .or_default()
            .feed(bytes);
        if submitted && let Some(entry) = self.automatic.get_mut(pane_id_env) {
            for written in &mut entry.payloads {
                written.drained = true;
            }
        }
    }

    /// Record that a guarded send in `mode` just put `payload` into
    /// `pane_id_env`.
    fn note_automatic_write(&mut self, pane_id_env: &str, mode: SubmitMode, payload: &[u8]) {
        if is_sentinel_pane_id(pane_id_env) || !matches!(mode, SubmitMode::Submit) {
            return;
        }
        let at = Instant::now();
        let entry = self.automatic.entry(pane_id_env.to_string()).or_default();
        // An empty SUBMIT payload — a probe — advances the clock without
        // touching the recorded payloads. It wrote no bytes, so it left the box
        // holding whatever the last payload write put there.
        entry.submitted_at = Some(at);
        if payload.is_empty() {
            return;
        }
        // Housekeeping: an entry past the TTL can no longer refuse anything
        // (`user_typed_since_writing` ignores it), so it is only occupying a
        // slot the cap below would otherwise spend evicting a live one.
        entry
            .payloads
            .retain(|written| at.duration_since(written.at) < PAYLOAD_RECORD_TTL);
        // Issue #424 S2: PUSHED, never merged into an equal-digest entry. Two
        // deliveries writing the same bytes need two releases, or the first to
        // finish silently disarms the second — see [`AutomaticWrite::payloads`].
        entry.payloads.push(PayloadWrite {
            digest: payload_digest(payload),
            at,
            drained: false,
        });
        while entry.payloads.len() > MAX_PAYLOAD_RECORDS_PER_PANE {
            entry.payloads.remove(0);
        }
    }

    /// Release ONE record of `payload` for `pane_id_env` — a delivery that wrote
    /// those bytes has reached a terminal outcome, so its unit of guard is no
    /// longer protecting a retry and must not refuse an unrelated future
    /// delivery of the same text.
    ///
    /// Issue #424 S2: exactly one, the OLDEST — the entry this delivery is most
    /// likely to have written, and the one a submitted-then-rewritten payload
    /// leaves behind as [`PayloadWrite::drained`]. Removing every equal-digest
    /// entry (what this did) disarmed a CONCURRENT delivery's guard, after which
    /// its replacement was admitted on top of an unsent draft and submitted
    /// both.
    fn forget_payload(&mut self, pane_id_env: &str, payload: &[u8]) {
        let digest = payload_digest(payload);
        if let Some(entry) = self.automatic.get_mut(pane_id_env)
            && let Some(index) = entry
                .payloads
                .iter()
                .position(|written| written.digest == digest)
        {
            entry.payloads.remove(index);
        }
    }

    /// Drop everything recorded for `pane_id_env` — a different agent now owns
    /// that pane, so the previous occupant's input box no longer exists and its
    /// records could only refuse the newcomer's first delivery.
    fn forget_pane(&mut self, pane_id_env: &str) {
        self.automatic.remove(pane_id_env);
        self.input.remove(pane_id_env);
    }

    fn last_user_input_at(&self, pane_id_env: &str) -> Option<Instant> {
        self.user_input_at.get(pane_id_env).copied()
    }

    /// Would a blind submit CR into `pane_id_env` submit something other than
    /// the payload we put there? See
    /// [`AgentPtyRegistry::user_typed_since_automatic_write`].
    fn user_typed_since_submitting(&self, pane_id_env: &str) -> bool {
        let Some(typed) = self.last_user_input_at(pane_id_env) else {
            return false;
        };
        match self
            .automatic
            .get(pane_id_env)
            .and_then(|entry| entry.submitted_at)
        {
            Some(submitted) => typed > submitted,
            // Nothing of ours is in that pane, so a blind CR can only submit
            // whatever the user put there.
            None => true,
        }
    }

    /// Would writing `payload` into `pane_id_env` REPEAT bytes an unfinished
    /// delivery already put there, after the user has typed since? See
    /// [`AgentPtyRegistry::user_typed_since_writing_payload`].
    fn user_typed_since_writing(&self, pane_id_env: &str, payload: &[u8]) -> bool {
        let Some(typed) = self.last_user_input_at(pane_id_env) else {
            return false;
        };
        let digest = payload_digest(payload);
        let now = Instant::now();
        self.automatic.get(pane_id_env).is_some_and(|entry| {
            entry.payloads.iter().any(|written| {
                // Issue #424 S2: a DRAINED entry is retained for its owner to
                // release but no longer describes the input box — the user
                // submitted, so those bytes left it.
                !written.drained
                    && written.digest == digest
                    && typed > written.at
                    && now.duration_since(written.at) < PAYLOAD_RECORD_TTL
            })
        })
    }
}

/// Issue #424 H1 (both reviewers): one agent's PTY writer, wrapped so that bytes
/// written by anyone OTHER than the daemon's own send paths are recognized as
/// user input at the instant they are written — while the writer lock is still
/// held.
///
/// The attach input path used to write and flush the user's bytes under this
/// writer, DROP it, and only then stamp the user-input clock. A guarded
/// automatic sender queued on the same writer acquired it in that gap, read the
/// stale clock, passed the guard, and appended its replacement + CR (or fired a
/// blind probe CR) — submitting the very draft the guard exists to protect. No
/// attacker required; that is ordinary concurrency between an attached client
/// and a scheduled delivery.
///
/// Making the stamp part of the write closes the gap by construction: every
/// writer of these bytes holds this mutex, so the clock a guarded send reads
/// under the writer can no longer be older than bytes that are already in the
/// PTY. The daemon's own writes take [`PaneWriter::daemon`], which bypasses the
/// observation — they are not user input, and recording them as such would make
/// every delivery refuse itself.
pub struct PaneWriter {
    inner: Box<dyn std::io::Write + Send>,
    /// The pane whose input box these bytes land in. `None` for a daemon-side
    /// agent that carries no pane id — nothing keys off it, so nothing to
    /// observe.
    pane_id_env: Option<String>,
    state: Arc<Mutex<PaneInputState>>,
}

impl PaneWriter {
    fn new(
        inner: Box<dyn std::io::Write + Send>,
        pane_id_env: Option<String>,
        state: Arc<Mutex<PaneInputState>>,
    ) -> Self {
        Self {
            inner,
            pane_id_env,
            state,
        }
    }

    /// Write as the DAEMON: the bytes are ours, so they are not user input and
    /// must not advance the user-input clock. Every daemon-initiated write into
    /// a pane goes through here; everything that reaches the plain
    /// [`std::io::Write`] impl is somebody else typing.
    fn daemon(&mut self) -> &mut (dyn std::io::Write + Send) {
        &mut *self.inner
    }
}

impl std::io::Write for PaneWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        if written > 0
            && let Some(pane_id) = self.pane_id_env.as_deref()
        {
            self.state
                .lock()
                .unwrap()
                .note_user_bytes(pane_id, &buf[..written]);
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
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
    /// PRD #127 M2.2 (deliver-on-idle) + issue #424 F1: what each pane's input
    /// box is holding — the user-keystroke clock the scheduler's reuse path
    /// debounces on, and the record of what THIS daemon's guarded sends put
    /// there. Together they answer the one question a submit-only probe and the
    /// one bounded replacement payload have to ask before they fire — "is what
    /// the target is holding still OUR payload, or has the user typed since?" —
    /// out of clocks the daemon owns rather than ones a producer can assert.
    ///
    /// An `Arc` because every agent's [`PaneWriter`] holds the same state: that
    /// is what makes the user-input stamp atomic with respect to writer handoff
    /// (H1). See [`PaneInputState`], [`Self::user_typed_since_automatic_write`]
    /// and [`Self::user_typed_since_writing_payload`].
    pane_input: Arc<Mutex<PaneInputState>>,
    /// Issue #424 F4: agents whose pane declared BOOT PROVENANCE before their
    /// spawn-time prompt was written — a `wrapper_fork`-origin `SessionStart`
    /// that the readiness gate skipped
    /// ([`crate::state::SessionStartWait::launcher_handoff`]) — mapped to the
    /// `AgentType` that declaration named.
    ///
    /// Issue #666: the TYPE is retained, not just the fact. A bare "this pane
    /// declared something" cannot answer "does the post-write declaration AGREE
    /// with what we already believed", which is what stops a declared type from
    /// GRANTING privilege (#424 F4) — see
    /// [`crate::prompt_delivery::AgentStartRearm`] and
    /// [`Self::pre_write_believed_agent_type`].
    ///
    /// Lives here because the fact is discovered in `crate::spawn::deliver`,
    /// before the write, and needed by the detached confirmation loop, after it
    /// — two functions that share nothing but this registry. Keyed by AGENT id,
    /// not pane id: pane ids are reused across spawns, and a previous
    /// occupant's launcher declaration must not grant standing to the next
    /// delivery. Grows by agents spawned in one daemon's lifetime, like
    /// [`Self::user_input_at`] (negligible: one short string each).
    launcher_handoff_agents: Mutex<HashMap<String, AgentType>>,
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
    /// The hook-ingestion socket this registry's daemon is bound to, injected
    /// into spawned children as [`DOT_AGENT_DECK_SOCKET`]. `None` for a
    /// registry with no owning daemon (in-process unit tests), in which case
    /// no injection happens and children resolve the endpoint the old way.
    hook_socket: Mutex<Option<PathBuf>>,
    /// Issue #424 (reviewer blocker 3 / auditor MEDIUM): where a daemon-side
    /// delivery failure is REPORTED, so the report is durable state on the
    /// pane's card rather than bytes typed into the agent's input buffer.
    /// `None` for a registry with no owning daemon (in-process unit tests),
    /// where publishing is a silent no-op. See [`DeliveryNotice`].
    delivery_notice_sink: Mutex<Option<DeliveryNoticeSink>>,
    /// PRD #126: delegations that are still awaiting a `work-done` plus the set
    /// of panes currently mid-close. Both live under ONE mutex so "mark this
    /// pane closing AND drop its outstanding records" is a single atomic
    /// transition — see [`AgentPtyRegistry::begin_pane_close`]. A two-mutex
    /// version would leave a window where a concurrent `handle_delegate` arms
    /// between the mark and the drop.
    ///
    /// Lives here rather than on `AppState` because both hook handlers run
    /// under a `read()` guard on the shared state — putting a mutable map on
    /// `AppState` would force those hot call sites to `write()`. Same
    /// interior-mutability shape as `dispatch_mutexes` / `user_input_at` /
    /// `delivery_ledger`.
    delegations: Mutex<DelegationTracker>,
    /// PRD #126: monotonic generation stamped onto each
    /// [`OutstandingDelegation`]. A re-delegation to the same worker pane
    /// overwrites the record with a *newer* seq, so delegation #1's still
    /// sleeping watch task fails its seq-conditional take and expires
    /// silently instead of firing a premature prompt against delegation #2.
    /// Same trick (and the same reason to prefer it over `JoinHandle::abort`)
    /// as the daemon idle monitor's generation counter: cancellation is one
    /// atomic operation with no await and no race against the timer's wake-up.
    ///
    /// Reviewer nit (PRD #126 M1 review, finding 7): `fetch_add` wraps, so a
    /// seq collision is not *mathematically* impossible — it needs ~2^64 arms
    /// while one stale watch task is still sleeping. At one delegation per
    /// nanosecond that is ~585 years inside a single timeout window, so
    /// exhaustion is unreachable in practice rather than prevented by
    /// construction. Deliberately NOT guarded with a checked
    /// `fetch_update`/exhaustion proof: that is real complexity for a state no
    /// running daemon can reach, and the drop-driven cancellation below now
    /// retires stale tasks long before they could collide anyway.
    delegation_seq: AtomicU64,
}

/// PRD #126: the outstanding-delegation side state — records plus the
/// mid-close pane set. See [`AgentPtyRegistry::delegations`].
#[derive(Default)]
struct DelegationTracker {
    /// Keyed by the *worker's* `pane_id_env`; at most one (the newest)
    /// delegation per worker pane.
    records: HashMap<String, OutstandingDelegation>,
    /// PRD #249 M3 review (finding B4/S4): the silent-worker watches, keyed by
    /// the *worker's* `pane_id_env`; at most one (the newest) per worker pane.
    /// Separate from `records` because the two watches have different clocks —
    /// the idle watch starts when the delegate is issued, the silence watch only
    /// once the task pointer has actually been written — but the same three
    /// cancellation events resolve both.
    silence_watches: HashMap<String, SilenceWatchRecord>,
    /// Issue #448: the COMMISSION ledger — how many delegations the orchestrator
    /// has issued to each worker pane that no `work-done` has been credited to
    /// yet. Keyed by the *worker's* `pane_id_env`.
    ///
    /// A third map rather than a field on `records` because it must answer a
    /// question neither watch can: *did the orchestrator ask for this at all?*
    /// Both of those maps are populated only when their detector is switched on
    /// and their panes are healthy, so an ABSENT record there means "no
    /// delegation, or a delegation whose detector is off, or one armed while a
    /// pane was closing" — three states that must be told apart, because in the
    /// disabled-detector one the completion is entirely genuine. This ledger is
    /// armed for every delegate the daemon dispatches regardless of either
    /// timeout, so `Unsolicited` here means what it says.
    commissions: HashMap<String, DelegationCommission>,
    /// Panes between [`AgentPtyRegistry::begin_pane_close`] and
    /// [`AgentPtyRegistry::finish_pane_close`]. Arming is refused for a pane in
    /// this set (as worker *or* as orchestrator), which is what closes the
    /// arm-after-cancel race: the SIGTERM grace window is up to
    /// `AGENT_TERMINATE_GRACE` long, and a delegate landing inside it must not
    /// leave a record that nothing removes.
    closing_panes: HashSet<String>,
    /// PRD #249 round-6 review (Greptile, the readiness buffer): live senders
    /// handed out by [`AgentPtyRegistry::pane_close_signal`], keyed by the pane
    /// whose close they announce. Dropping a sender IS the signal — the same
    /// drop-to-cancel discipline as both watches' `_cancel` channels — and
    /// [`AgentPtyRegistry::begin_pane_close`] drops every sender for the pane it
    /// marks. Lets an in-flight wait (the M1 readiness gate) abandon promptly
    /// instead of sleeping out its remainder against a target that is gone.
    close_waiters: HashMap<String, Vec<oneshot::Sender<()>>>,
}

/// PRD #249 M3 review (finding B4/S4): one armed silent-worker watch — the
/// "did this delegated worker emit anything at all?" diagnostic.
///
/// Exists so the watch can be **cancelled**. Without it the detached task ran to
/// its deadline no matter what happened in between, so a hookless worker could
/// receive the pointer, report `work-done`, and *still* be reported as possibly
/// undelivered — a diagnostic that fires after positive proof of delivery is
/// worse than no diagnostic, because it trains operators to ignore it.
struct SilenceWatchRecord {
    /// Generation of this record — see [`AgentPtyRegistry::delegation_seq`],
    /// whose counter is shared. Proof of ownership for the conditional take in
    /// [`AgentPtyRegistry::cancel_silence_watch_if`], so a stale watch can never
    /// disarm a newer delegation's.
    seq: u64,
    /// PRD #249 round-6 review (Greptile, `handle_work_done`): how many OLDER
    /// watches for this same worker pane were superseded without a `work-done`
    /// ever being credited to them. The exact counterpart of
    /// [`OutstandingDelegation::superseded`], and it exists for the exact same
    /// defect: `WorkDoneSignal` carries no delegation generation, so an
    /// unconditional cancel let a late/duplicated/retried completion from
    /// delegation N disarm delegation N+1's watch — and if N+1's pointer never
    /// landed, the undelivered-prompt detector was then silently disabled for
    /// precisely the case it exists to surface. Completions are therefore
    /// applied oldest-first by [`AgentPtyRegistry::retire_silence_watch`].
    superseded: u32,
    /// Pane of the orchestrator that issued the delegate, so closing the
    /// ORCHESTRATOR cancels the watch too — its notice would otherwise be aimed
    /// at a pane id a later, unrelated agent can inherit.
    orchestrator_pane_id: String,
    /// Mirrors [`OutstandingDelegation::worker_agent_id`] — the
    /// worker's registry agent id, when known at arm time. Unlike the idle
    /// delegation, [`AgentPtyRegistry::arm_silence_watch`] is called from
    /// `dispatch_one_owned` AFTER any `clear = true` respawn has already
    /// resolved the worker's identity, so this is set directly at arm time
    /// rather than bound later.
    worker_agent_id: Option<String>,
    /// The live end of the watch task's cancellation channel. Never *sent* on:
    /// the task selects on it and exits as soon as it resolves, which happens
    /// when this record leaves the map (work-done, supersede, pane close, or the
    /// watch's own conditional take). Mirrors
    /// [`OutstandingDelegation::_watch_cancel`].
    _cancel: oneshot::Sender<()>,
}

/// Issue #448: the commission ledger's per-worker-pane entry — how many
/// delegations that pane still owes a `work-done` for.
///
/// A count, not a per-delegation record, because the only question it answers is
/// whether the orchestrator commissioned *anything* that is still unanswered.
/// `WorkDoneSignal` carries no delegation generation (see
/// [`AgentPtyRegistry::retire_outstanding_delegation`]), so a completion cannot
/// be matched to a specific delegation anyway — and the two maps that do carry
/// generations already own every accounting decision that depends on knowing
/// *which* one.
struct DelegationCommission {
    /// Delegations dispatched to this worker pane that no completion has been
    /// credited to yet. Saturating, like [`OutstandingDelegation::superseded`].
    outstanding: u32,
    /// Pane of the orchestrator that issued them, so closing the ORCHESTRATOR
    /// clears the ledger as well as the two watches — a commission is owed to a
    /// specific orchestrator, and a pane id freed by a close can be inherited by
    /// an unrelated agent that commissioned nothing.
    orchestrator_pane_id: String,
}

/// Issue #448: did the orchestrator actually commission the work a `work-done`
/// reports? See [`AgentPtyRegistry::retire_delegation_commission`].
///
/// This is deliberately NOT derived from [`DelegationRetirement`]. Its `Nothing`
/// arm is not a reliable proxy for "never delegated": the idle detector arms no
/// record when `worker_response_timeout_minutes = 0`, when the orchestrator pane
/// has no live registry agent, or when either pane is mid-close — and in the
/// first of those the completion is genuine and must still be reported as such.
/// Reading `Nothing` as "unsolicited" would silently mislabel every completion
/// in every project that has turned the detector off.
// `Clone, Copy` for parity with its sibling `state::WorkDoneReportChannel`
// (issue #448 review, finding 7 minor): both are small plain enums describing one
// completion, and a caller that has to `match`-and-rebuild one but not the other
// is an avoidable papercut. No behavioural effect today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkDoneProvenance {
    /// The orchestrator had at least one unanswered delegation to this worker
    /// pane; one is now credited to this completion.
    Solicited {
        /// Commissions still unanswered after this one — non-zero only when the
        /// orchestrator re-delegated before the worker reported, which its
        /// protocol forbids.
        remaining: u32,
    },
    /// Nothing was outstanding: the orchestrator commissioned no work this
    /// completion could be answering. The commonest cause is a human tasking the
    /// worker directly — the `work-done` instruction survives in the worker's
    /// context from an earlier delegation (`work_done_footer`), so it runs again
    /// for work the orchestrator never asked for.
    Unsolicited,
}

/// PRD #249 M3 review (finding B4/S4): handed back by
/// [`AgentPtyRegistry::arm_silence_watch`] to the caller that spawns the watch
/// task — the record's generation and the cancellation channel the task must
/// select on alongside its event wait.
#[derive(Debug)]
pub struct ArmedSilenceWatch {
    pub seq: u64,
    pub cancel: oneshot::Receiver<()>,
}

/// PRD #126: one delegation the daemon is still waiting on a `work-done` for.
/// Carries everything the watch task needs to compose, authorize and deliver
/// the idle prompt without re-entering `AppState`.
#[derive(Debug)]
pub struct OutstandingDelegation {
    /// Generation of this record — see [`AgentPtyRegistry::delegation_seq`].
    pub seq: u64,
    /// The delegated role name (as registered in `pane_role_map`).
    pub role: String,
    /// Pane of the orchestrator that issued the delegate; the idle prompt is
    /// submitted here.
    pub orchestrator_pane_id: String,
    /// PRD #126 M1 audit (finding 2): the orchestrator's **registry agent id**
    /// as of arming time. A pane id is only a string — after the orchestrator
    /// is closed another agent (possibly in an unrelated orchestration) can be
    /// spawned onto the same `pane_id_env`, and an unguarded write would submit
    /// this orchestration's idle text into that stranger's session, where it
    /// may be acted on with tools. Delivery therefore goes through
    /// [`AgentPtyRegistry::write_and_submit_guarded`] with this id as the
    /// expected target, so a rebind yields `WrongSession` and zero bytes.
    pub orchestrator_agent_id: String,
    /// The daemon's routing identity for the orchestration the delegate
    /// belonged to (`AppState::pane_orchestration_map`'s value), when known.
    /// Re-checked at delivery time against the orchestrator pane's live registry
    /// membership (see [`AgentPtyRegistry::pane_orchestration`]) so a pane that
    /// has been re-homed into a *different* orchestration is refused as well.
    ///
    /// PRD #140 integration: this used to be the orchestration **name** alone.
    /// Once two tabs of the same orchestration in the same directory became two
    /// distinct routing groups, a name was no longer an orchestration identity —
    /// both tabs answer the same name, so a name-only recheck could not tell
    /// them apart. It now carries the whole
    /// [`crate::state::OrchestrationIdentity`], whose `Instance` variant is the
    /// per-tab token #140 routes on.
    pub orchestration: Option<crate::state::OrchestrationIdentity>,
    /// When the delegation was armed, for the elapsed-time wording.
    pub armed_at: Instant,
    /// PRD #126 M1 review (finding 6): how many OLDER delegations to this same
    /// worker pane were superseded without ever reporting `work-done`. The
    /// orchestrator protocol forbids re-delegating before a worker reports, so
    /// this is normally 0; when it is not, a late `work-done` from delegation
    /// #1 retires one superseded delegation (decrementing this) instead of
    /// clobbering delegation #2's still-live record — which used to leave the
    /// newest delegation silent forever with no nudge.
    superseded: u32,
    /// The worker's registry agent id, bound once it is known
    /// rather than at arm time — `None` until [`AgentPtyRegistry::bind_delegation_worker_agent_id`]
    /// sets it. This record is armed synchronously in `AppState::handle_delegate`,
    /// BEFORE the dispatch task that may respawn the worker pane even starts
    /// running, so the eventual worker identity (the respawn's fresh agent, or
    /// whoever already owns the pane on a `clear = false` delegate) is not yet
    /// knowable at arm time. `pump_reader`'s EOF sweep only retires a record
    /// via its WORKER-side match when this field is bound AND matches the
    /// agent that just exited — an unbound record belongs to a delegation
    /// whose identity has not resolved yet, so the exiting agent (necessarily
    /// some OTHER, previous occupant of the pane) cannot be it. Left unmatched
    /// this way, an unbound record simply falls through to its own timer
    /// instead of being drained by a stranger's death.
    worker_agent_id: Option<String>,
    /// PRD #126 M1 review (finding 2) / audit (finding 3): the live end of the
    /// watch task's cancellation channel. Never *sent* on — the watch task
    /// selects on it and exits as soon as it resolves, which happens when this
    /// record is dropped out of the map (work-done, supersede, pane close, or
    /// the timer's own take). That turns every cancellation into an immediate
    /// task teardown instead of leaving an `Arc<AgentPtyRegistry>` and its
    /// owned strings sleeping out the full (default two-hour) timeout.
    _watch_cancel: oneshot::Sender<()>,
}

/// PRD #126: the orchestration membership of the live agent on a pane, as
/// [`AgentPtyRegistry::pane_orchestration`] reads it back out of the registry's
/// `tab_membership`. Deliberately the raw membership fields rather than a
/// [`crate::state::OrchestrationIdentity`]: the daemon folds
/// `orchestration_cwd.or(StartAgent.cwd)` into that identity's `NameCwd` variant
/// at `StartAgent` time, and re-deriving it here from the membership alone would
/// invent a *different* cwd for the same pane and turn a healthy revalidation
/// into a refusal. The comparison rules live in
/// [`crate::state::orchestration_still_matches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOrchestration {
    /// `TabMembership::Orchestration::name` — the orchestration's config name.
    pub name: String,
    /// PRD #140's per-tab instance token (`orchestration_id`), when the client
    /// that spawned this pane stamped one. `None` for a pre-#140 client.
    pub instance_id: Option<String>,
    /// The shared per-tab orchestration cwd, when the client sent one.
    pub cwd: Option<String>,
}

/// PRD #126: handed back by [`AgentPtyRegistry::arm_outstanding_delegation`] to
/// the caller that spawns the watch task: the record's generation (proof of
/// ownership for the seq-conditional take) and the cancellation channel the
/// task must select on alongside its sleep.
#[derive(Debug)]
pub struct ArmedDelegation {
    pub seq: u64,
    pub cancel: oneshot::Receiver<()>,
}

/// PRD #126: what a `work-done` did to a worker pane's outstanding delegation.
/// See [`AgentPtyRegistry::retire_outstanding_delegation`].
#[derive(Debug)]
pub enum DelegationRetirement {
    /// Nothing was outstanding for that pane (the common case: no delegation,
    /// or one already resolved).
    Nothing,
    /// The pane's only outstanding delegation was retired; dropping the
    /// returned record cancels its watch.
    Retired(OutstandingDelegation),
    /// A *superseded* (older) delegation was retired. The newest record and its
    /// watch stay armed — see `OutstandingDelegation::superseded`.
    RetiredSuperseded {
        role: String,
        /// Generation of the record left armed.
        seq: u64,
        /// Superseded delegations still unaccounted for after this one.
        remaining: u32,
    },
}

/// PRD #249 round-6 review (Greptile): what a `work-done` did to a worker pane's
/// silent-worker watch. See [`AgentPtyRegistry::retire_silence_watch`] — the
/// three variants exist so `handle_work_done` can log *which* delegation the
/// completion was credited to, which is the only way the oldest-first accounting
/// is diagnosable from a daemon log.
#[derive(Debug)]
pub enum SilenceWatchRetirement {
    /// No watch was armed for that pane — the detector is disabled, the pointer
    /// was never delivered, or a close/notice already consumed the record.
    Nothing,
    /// The watch belonging to this completion was disarmed; dropping its record
    /// cancels the task, so no notice can follow proof of delivery.
    Cancelled { seq: u64 },
    /// The completion was credited to an older, *superseded* delegation. The
    /// newest watch stays armed — see [`SilenceWatchRecord::superseded`].
    KeptNewer {
        /// Generation of the watch left armed.
        seq: u64,
        /// Superseded watches still unaccounted for after this one.
        remaining: u32,
    },
}

/// Everything a pane needs in order to be created from NOTHING — the case where
/// [`AgentPtyRegistry::respawn_or_recreate_agent_for_pane`] finds no record to
/// lift an identity out of.
///
/// A respawn normally captures all of this from the record it replaces. When
/// that record is gone — a `StopAgent` removed it mid-close (issue #606), or the
/// pane's previous agent died and was reaped — only the CALLER knows what the
/// pane is for, so the caller supplies it. The delegate path fills it from the
/// role's `.dot-agent-deck.toml` entry plus the pane's known cwd and
/// orchestration membership.
#[derive(Debug, Clone, Default)]
pub struct PaneRecreateIdentity {
    pub cwd: Option<String>,
    pub display_name: Option<String>,
    pub tab_membership: Option<TabMembership>,
    /// What agent this pane runs, as the caller knows it RIGHT NOW.
    ///
    /// This is the LAUNCH-side identity for a re-creation, and it outranks
    /// deriving the type from the command — the reverse of the rule
    /// [`AgentPtyRegistry::respawn_agent_for_pane`] applies to a pane's frozen
    /// [`RunningAgent::spawn_agent_type`]. The two are not in conflict, because
    /// they are not the same kind of value:
    ///
    /// * `spawn_agent_type` was captured at a PREVIOUS spawn. The command
    ///   handed to a respawn may have been edited since, so honoring the frozen
    ///   value over the command is how PRD #225 finding 1's "Claude launched
    ///   wrapped as Codex" happens. It is therefore a fallback only.
    /// * This field is supplied by the caller in the SAME pass that supplied
    ///   the `command` beside it. `crate::state`'s delegate path re-reads
    ///   `.dot-agent-deck.toml` on every delegate and fills both from the role
    ///   entry it just read, so the identity cannot be stale against that
    ///   command. Issue #308's `agent = "…"` declaration is exactly such a
    ///   value, and it exists to answer what the command cannot.
    ///
    /// **Every caller must keep that contract**: fill this from a fresh read of
    /// whatever declares the pane's identity, never from a stored, learned or
    /// previously-frozen value. A hook-LEARNED type must never arrive here —
    /// replaying an observed badge into a launch decision is PRD #225 Defect 2,
    /// and this field is a route to it.
    ///
    /// `None` means "the caller does not know", and the type is derived from
    /// the command exactly as before.
    pub agent_type: Option<AgentType>,
    /// Extra environment for the fresh child. The caller MUST include
    /// `DOT_AGENT_DECK_PANE_ID`; without it the new agent is not bound to the
    /// pane and nothing can route to it.
    ///
    /// A respawn replays the previous child's whole `spawn_env`; a re-creation
    /// has no previous child to read one from, so anything not listed here is
    /// gone. That costs nothing today — every producer of an orchestration role
    /// pane (`spawn::spawn`'s `pane_env`, and the TUI's `create_stream_pane`)
    /// passes the pane id and nothing else, and `spawn_agent` injects the
    /// registry's own hook socket and agent id itself — but a producer that
    /// starts supplying role env has to supply it here too.
    pub env: Vec<(String, String)>,
}

/// What [`AgentPtyRegistry::respawn_or_recreate_agent_for_pane`] did.
#[derive(Debug, Clone)]
pub struct PaneRespawn {
    /// The registry id now occupying the pane.
    pub agent_id: String,
    /// `true` when the pane had no record left and a fresh agent was created
    /// from [`PaneRecreateIdentity`] instead of being respawned from one. The
    /// delegate path uses this to restore the daemon-side role registration a
    /// completed close took with it.
    pub recreated: bool,
}

/// How long [`AgentPtyRegistry::respawn_or_recreate_agent_for_pane`] waits for
/// an in-flight `StopAgent` to release the pane before deciding the pane is
/// genuinely free.
///
/// Twice [`AGENT_TERMINATE_GRACE`], because that grace is only the child-kill
/// half of a close: the handler also unregisters the pane and drops its hold
/// afterwards, and on a loaded host those steps sit behind the same runtime the
/// grace just occupied. Over-waiting costs a delayed delegate; under-waiting
/// puts us back at issue #606, where the pane is re-created while its
/// predecessor's cleanup is still running and the cleanup then deletes the
/// newcomer's state.
const PANE_CLOSE_SETTLE_TIMEOUT: Duration = Duration::from_secs(6);

/// Poll cadence for [`PANE_CLOSE_SETTLE_TIMEOUT`]. Matches the 50 ms cadence
/// `terminate_child_with_grace_and_wait` polls `try_wait` at, so the wait
/// resolves within one tick of the close it is waiting on.
const PANE_CLOSE_SETTLE_POLL: Duration = Duration::from_millis(50);

struct RegistryInner {
    next_id: u64,
    agents: HashMap<String, RunningAgent>,
    /// Issue #454: spawns that have been ADMITTED but whose `RunningAgent` is
    /// not in `agents` yet — keyed by the pre-allocated agent id, valued by the
    /// spawn's validated `pane_id_env` (`None` for a paneless agent).
    ///
    /// [`AgentPtyRegistry::spawn_agent`] launches the child BEFORE it can take
    /// this lock to publish the agent, so between those two points the daemon
    /// owns a running child it cannot yet recognise. That gap is not
    /// theoretical: the child's very first action can be
    /// `dot-agent-deck agent-event --type running`, and the daemon's admission
    /// check ([`crate::state::AppState::apply_event`]) would drop the report as
    /// coming from a pane nobody owns — leaving `daemon status` and reconnect at
    /// `live = None` with no later event to repair it, which is issue #454 all
    /// over again for a wrapper that never emits `SessionStart`.
    ///
    /// The reservation is taken BEFORE the child exists and released under the
    /// SAME lock acquisition that inserts into `agents`, so ownership is
    /// continuously observable: every path that answers "do we own this?" sees
    /// either the reservation or the agent, never neither. It is released on
    /// every failure path too (see [`SpawnReservation`]), including a panic
    /// inside the spawn itself.
    ///
    /// Round-2 audit: a reservation is also EXCLUSIVE on its pane id. Taking one
    /// for a pane another reservation or another live agent already claims fails
    /// the spawn outright, under this same lock — so "at most one generation
    /// claims a pane" holds at every instant, which is the invariant
    /// [`AgentPtyRegistry::owns_generation`]'s retirement rule rests on.
    pending_spawns: HashMap<String, Option<String>>,
    /// Issue #454 round-3 review (blocker 1): panes whose SCOPED CLEANUP is
    /// currently in progress, keyed by pane id.
    ///
    /// `StopAgent` authorises a pane-scoped teardown — dropping the pane's
    /// delegations, cancelling its provisional prompt, and taking its role, cwd,
    /// orchestrator marker and routing identity back out of `AppState` — on the
    /// strength of "nobody else holds this pane". That authorisation was
    /// check-then-act: it was decided before `close_agent`, which can spend the
    /// whole three-second termination grace, and acted on afterwards, so a spawn
    /// reserving the pane anywhere in between had its freshly registered state
    /// deleted by its predecessor's close.
    ///
    /// Rather than revalidate at each of the four steps — which only shrinks the
    /// window, and cannot close the last one because the registry lock and the
    /// `AppState` write lock are different locks — the authorisation is made
    /// DURABLE: taking it also blocks any new generation from claiming the pane
    /// until the cleanup finishes. See [`AgentPtyRegistry::hold_pane_for_cleanup`].
    ///
    /// This costs almost nothing in practice, because the pane is already
    /// unavailable for most of the same window: a LIVE agent on it fails the
    /// reservation's exclusivity test anyway, and the only genuinely new
    /// exclusion is the short tail between a dead child and its record being
    /// dropped by `close_agent`.
    cleanup_holds: HashSet<String>,
    /// Issue #584: one-shot waiters for "this AGENT's PTY reached EOF", keyed by
    /// registry id.
    ///
    /// The sibling of [`DelegationTracker::close_waiters`], and it exists for the
    /// same reason: a wait that is really about a target's liveness must not be
    /// expressed as a poll. The delegate's post-respawn readiness wait sat on a
    /// fixed 30 s deadline, so a replacement worker that died two seconds into
    /// its boot still cost the full window — and the pointer was then refused by
    /// the identity gate with `NoLiveTarget` and dropped in silence. Resolved by
    /// [`AgentPtyRegistry::signal_agent_exit`], which `pump_reader` calls in the
    /// same breath as setting `exited`.
    ///
    /// A `oneshot` rather than a poll loop deliberately: the delegate path is
    /// exercised on a PAUSED Tokio clock by `orchestration/delegate/011`, and a
    /// polling task's `sleep` would let the runtime's auto-advance move that
    /// clock underneath the test.
    exit_waiters: HashMap<String, Vec<oneshot::Sender<()>>>,
}

/// Issue #454: RAII holder for a [`RegistryInner::pending_spawns`] entry.
///
/// `Drop` releases it by taking the registry lock, which is correct for every
/// path that is NOT already holding it. The success path *is* — `spawn_agent`
/// holds `inner` from the post-spawn acquisition through `agents.insert` — so it
/// calls [`Self::release_locked`] instead, which consumes the guard and disarms
/// `Drop` (a second lock acquisition on a `std::sync::Mutex` would deadlock).
struct SpawnReservation<'a> {
    registry: &'a AgentPtyRegistry,
    id: Option<String>,
}

impl<'a> SpawnReservation<'a> {
    /// Release the reservation while the caller already holds the registry lock.
    fn release_locked(mut self, inner: &mut RegistryInner) {
        if let Some(id) = self.id.take() {
            inner.pending_spawns.remove(&id);
        }
    }
}

impl Drop for SpawnReservation<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // A poisoned lock means some other thread panicked mid-mutation;
            // there is nothing useful to do here and panicking in `Drop` would
            // abort. The stale entry is bounded by one per panicking spawn.
            if let Ok(mut inner) = self.registry.inner.lock() {
                inner.pending_spawns.remove(&id);
            }
        }
    }
}

/// Issue #454 round-3 review (blocker 1): RAII holder for a
/// [`RegistryInner::cleanup_holds`] entry — the durable form of "this pane is
/// still the stopping agent's to give up".
///
/// Held for the WHOLE of `StopAgent`'s pane-scoped cleanup and released on every
/// exit from it, including the early `?` returns and a panic. While it is held,
/// no new generation can reserve the pane, so the authorisation that granted it
/// cannot go stale under the cleanup that acts on it. Owns an `Arc` rather than
/// borrowing the registry because it lives across `.await` points.
pub struct PaneCleanupHold {
    registry: Arc<AgentPtyRegistry>,
    pane_id: String,
}

impl PaneCleanupHold {
    /// The pane this hold authorises cleanup of.
    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }
}

impl Drop for PaneCleanupHold {
    fn drop(&mut self) {
        // Same reasoning as `SpawnReservation::drop`: a poisoned lock means
        // another thread panicked mid-mutation, and panicking in `Drop` would
        // abort. A leaked hold blocks reuse of ONE pane id on a registry that is
        // already unable to answer any ownership question at all.
        if let Ok(mut inner) = self.registry.inner.lock() {
            inner.cleanup_holds.remove(&self.pane_id);
        }
    }
}

/// Issue #424 (reviewer blocker 3 / auditor MEDIUM): one daemon-authored report
/// that an automatic prompt delivery FAILED on `pane_id`.
///
/// This is the replacement for writing a diagnostic line into the agent's own
/// input buffer. That mechanism (`write_notice_guarded`) is retained for the two
/// orchestrator-pane notices that still take it — `compose_worker_exited_notice`
/// and `compose_respawn_no_live_worker_notice`; issue #702 moved PRD #249's
/// silence notice off it onto the submitted path — but its own contract says LF may be
/// interpreted as Enter and that a later ordinary submit sends
/// `notice + newline + user prompt` as ONE turn — pinned by the passing
/// regression `write_to_pane_notice_bytes_precede_next_submit_with_only_lf_between`.
/// Written into the very pane whose prompt handling is in doubt (and which may
/// already hold swallowed seed bytes), the notice could submit as an agent turn
/// or ride along with the user's next Enter. A delivery failure must not be
/// reported by a mechanism that can itself become a task.
///
/// So the report travels as STATE instead: the daemon turns it into one
/// synthetic [`crate::event::AgentEvent`] on the pane's existing card, through
/// the same ingest path every real hook event uses (`daemon::ingest_event`), so
/// it lands in the daemon's own `AppState` *and* is broadcast to attached
/// clients. The card's status becomes `Error` and stays there until the agent
/// itself asserts something newer. No new wire field and no protocol change:
/// this rides the fan-out `spawn::surface_spawned_pane` already uses.
#[derive(Debug, Clone)]
pub struct DeliveryNotice {
    /// The pane whose delivery failed.
    pub pane_id: String,
    /// The EXACT registry agent the prompt was written for. The report is
    /// dropped unless this agent still owns the pane.
    pub agent_id: String,
    /// The logical delivery id, for correlating the card against the log.
    pub delivery_id: String,
    /// Issue #424 D3: the hook GENERATION the delivery was bound to, when it had
    /// one.
    ///
    /// The identity check on `agent_id` catches a pane that was rebound to a
    /// different agent, but a same-agent conversation successor — a `/clear`, a
    /// thread restart — keeps the registry id and would still take the
    /// predecessor delivery's report on its card. `Some(generation)` makes the
    /// sink require that generation to still be current before it reports;
    /// `None` (an unbound delivery, e.g. a launcher pane that never announced a
    /// conversation) carries no such constraint, because there is nothing to
    /// name.
    pub session_id: Option<String>,
    /// FIXED, daemon-authored text. Nothing a repository, a prompt or a role
    /// controls may be interpolated here — that rule outlives the transport,
    /// because the text still reaches a human-readable surface.
    pub detail: &'static str,
}

/// Issue #424: the daemon's sink for [`DeliveryNotice`]s, installed via
/// [`AgentPtyRegistry::set_delivery_notice_sink`]. A closure rather than a
/// concrete type because publishing needs the daemon's `SharedState` and event
/// broadcast, neither of which the registry owns.
pub type DeliveryNoticeSink = Arc<dyn Fn(DeliveryNotice) + Send + Sync>;

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

/// Issue #454: the registry IS the daemon's ownership authority — see
/// [`crate::state::AgentOwnership`] for why the daemon cannot keep an accurate
/// copy of this by hand, and [`AgentPtyRegistry::generation_ownership`] for the
/// properties that make asking here correct.
impl crate::state::AgentOwnership for AgentPtyRegistry {
    fn generation_ownership(
        &self,
        pane_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> crate::state::Ownership {
        AgentPtyRegistry::generation_ownership(self, pane_id, agent_id)
    }
}

impl AgentPtyRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                next_id: 1,
                agents: HashMap::new(),
                pending_spawns: HashMap::new(),
                cleanup_holds: HashSet::new(),
                exit_waiters: HashMap::new(),
            }),
            dispatch_mutexes: Mutex::new(HashMap::new()),
            detach_count: AtomicU64::new(0),
            change_notify: Arc::new(Notify::new()),
            shutting_down: AtomicBool::new(false),
            pane_input: Arc::new(Mutex::new(PaneInputState::default())),
            launcher_handoff_agents: Mutex::new(HashMap::new()),
            delivery_ledger: Mutex::new(DeliveryLedger::default()),
            hook_socket: Mutex::new(None),
            delivery_notice_sink: Mutex::new(None),
            delegations: Mutex::new(DelegationTracker::default()),
            delegation_seq: AtomicU64::new(1),
        }
    }

    /// Record the hook-ingestion socket the owning daemon bound, so
    /// [`spawn_agent`](Self::spawn_agent) can inject it into every child as
    /// [`DOT_AGENT_DECK_SOCKET`]. Called once from
    /// [`crate::daemon::run_daemon_with`] right after the bind succeeds.
    ///
    /// Idempotent and last-writer-wins: a daemon binds exactly one hook
    /// socket for its lifetime, so a second call would carry the same path.
    pub fn set_hook_socket(&self, path: PathBuf) {
        *self.hook_socket.lock().unwrap() = Some(path);
    }

    /// Issue #424: install the daemon's sink for [`DeliveryNotice`]s. Called
    /// once from [`crate::daemon::run_daemon_with`]; a registry without one
    /// (every in-process unit test) simply drops notices.
    pub fn set_delivery_notice_sink(&self, sink: DeliveryNoticeSink) {
        *self.delivery_notice_sink.lock().unwrap() = Some(sink);
    }

    /// Issue #424 (reviewer blocker 3): report a delivery failure against the
    /// pane it happened on, as DAEMON-SIDE STATE.
    ///
    /// Guarded by the same identity rule every write on this path uses: the
    /// EXACT agent the prompt was written for must still own the pane. A pane
    /// that exited and was respawned belongs to a stranger, and a stale report
    /// against it would mark the successor's card in error for a delivery that
    /// was never its.
    ///
    /// Synchronous and non-blocking — no writer lock, no PTY, no `await` — so a
    /// caller can run it inside an absolute deadline without the deadline
    /// becoming advisory (reviewer HIGH: the in-pane notice it replaces awaited
    /// a writer lock with no timeout, which is what let a registered task
    /// outlive the one deadline B9 established).
    ///
    /// Issue #424 D3: the check below is an EARLY-OUT, not the authorization.
    /// The sink is asynchronous — it schedules a task that reads and ingests
    /// state later — so this answer can be stale by the time anything lands. The
    /// sink RE-VALIDATES the same identity at ingestion, under the state write
    /// lock that applies the event; see `crate::daemon::install_delivery_notice_sink`.
    /// Both exist because the cheap check here suppresses the overwhelmingly
    /// common case without scheduling anything at all.
    pub fn publish_delivery_notice(&self, notice: DeliveryNotice) {
        if self.pane_current_agent_id(&notice.pane_id).as_deref() != Some(notice.agent_id.as_str())
        {
            tracing::debug!(
                pane_id = %notice.pane_id,
                delivery_id = %notice.delivery_id,
                "delivery notice suppressed; the pane no longer belongs to this agent"
            );
            return;
        }
        let sink = self.delivery_notice_sink.lock().unwrap().clone();
        match sink {
            Some(sink) => sink(notice),
            None => tracing::debug!(
                pane_id = %notice.pane_id,
                delivery_id = %notice.delivery_id,
                "no delivery-notice sink installed; the report stays in the log only"
            ),
        }
    }

    /// PRD #126: record that `role`'s worker pane has just been delegated to
    /// and owes a `work-done`. Returns the record's generation (proof of
    /// ownership for the watch task's seq-conditional take) plus the
    /// cancellation channel that task must select on — see
    /// [`ArmedDelegation`] and [`Self::take_outstanding_delegation_if`].
    ///
    /// Returns `None` — arming REFUSED, no record, and the caller must not
    /// spawn a watch — when either the worker pane or the orchestrator pane is
    /// mid-close ([`Self::begin_pane_close`]). That is the arm-after-cancel
    /// guard: a `StopAgent` spends up to `AGENT_TERMINATE_GRACE` terminating
    /// the child, and a delegate landing inside that window must not leave
    /// behind a record that the close has already swept past.
    ///
    /// Overwrites any previous record for the pane — the freshest delegation is
    /// the one the timer watches — but carries the older one forward in
    /// `OutstandingDelegation::superseded` rather than forgetting it, so a
    /// late `work-done` retires the *oldest* outstanding delegation instead of
    /// disarming the newest. Dropping the replaced record here also cancels its
    /// watch task immediately.
    pub fn arm_outstanding_delegation(
        &self,
        worker_pane_id: &str,
        role: &str,
        orchestrator_pane_id: &str,
        orchestrator_agent_id: &str,
        orchestration: Option<&crate::state::OrchestrationIdentity>,
    ) -> Option<ArmedDelegation> {
        let mut tracker = self.delegations.lock().unwrap();
        if tracker.closing_panes.contains(worker_pane_id)
            || tracker.closing_panes.contains(orchestrator_pane_id)
        {
            return None;
        }
        let seq = self.delegation_seq.fetch_add(1, Ordering::SeqCst);
        let superseded = tracker
            .records
            .get(worker_pane_id)
            .map_or(0, |prev| prev.superseded.saturating_add(1));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        tracker.records.insert(
            worker_pane_id.to_string(),
            OutstandingDelegation {
                seq,
                role: role.to_string(),
                orchestrator_pane_id: orchestrator_pane_id.to_string(),
                orchestrator_agent_id: orchestrator_agent_id.to_string(),
                orchestration: orchestration.cloned(),
                armed_at: Instant::now(),
                superseded,
                worker_agent_id: None,
                _watch_cancel: cancel_tx,
            },
        );
        Some(ArmedDelegation {
            seq,
            cancel: cancel_rx,
        })
    }

    /// Bind the worker's registry agent id onto an already-armed
    /// [`OutstandingDelegation`] once it becomes known — after a `clear = true`
    /// respawn resolves, or immediately for a `clear = false` delegate. `seq`
    /// guards against binding a DIFFERENT (superseded, or already-retired)
    /// delegation than the one the caller resolved this identity for: if the
    /// record for `worker_pane_id` no longer exists, or a newer delegation has
    /// since replaced it, this is a no-op. See [`OutstandingDelegation::worker_agent_id`]
    /// for why the sweep needs this bound before it will ever act on a
    /// worker-side match.
    pub fn bind_delegation_worker_agent_id(
        &self,
        worker_pane_id: &str,
        seq: u64,
        worker_agent_id: &str,
    ) {
        let mut tracker = self.delegations.lock().unwrap();
        if let Some(record) = tracker.records.get_mut(worker_pane_id)
            && record.seq == seq
        {
            record.worker_agent_id = Some(worker_agent_id.to_string());
        }
    }

    /// PRD #249 M3 review (finding B4/S4): register the silent-worker watch for
    /// `worker_pane_id` and hand back the generation + cancellation channel its
    /// task must select on. The caller arms this BEFORE writing the task pointer
    /// so a `work-done` that lands inside the write's own `SUBMIT_DELAY` window
    /// cancels the watch instead of racing it.
    ///
    /// Returns `None` — no record, and the caller must not spawn a watch — when
    /// either pane is mid-close ([`Self::begin_pane_close`]), for the same
    /// arm-after-cancel reason as [`Self::arm_outstanding_delegation`].
    ///
    /// Inserting REPLACES any previous watch for the pane, and dropping the
    /// replaced record cancels its task immediately: that is the supersession
    /// cancellation. An older "did anything happen?" question is answered by the
    /// newer delegate's own write, so the *task* carries nothing forward — but
    /// the replaced record's unaccounted-for completion count does (PRD #249
    /// round-6 review; see [`SilenceWatchRecord::superseded`]), because the
    /// `work-done` that belonged to the superseded delegation may still be in
    /// flight and must not be credited to this new watch.
    ///
    /// `worker_agent_id` is the worker's registry agent id, when
    /// the caller already knows it — see [`SilenceWatchRecord::worker_agent_id`].
    ///
    /// **Issue #687: on the `clear = true` path this is called EARLIER than the
    /// pointer write** — the moment the respawn establishes the new generation's
    /// ownership of the pane, rather than ~30 s later after the `SessionStart`
    /// wait and readiness buffer. Nothing about this function changed; what
    /// changed is when the caller invokes the supersession the paragraph above
    /// describes, because leaving it until the write meant the REPLACED
    /// generation's watch stayed armed throughout its replacement's startup and
    /// could fire against a delegation that was already live. The returned
    /// `ArmedSilenceWatch` is then carried through the dispatch and either handed
    /// to the watch task or released by `seq` — see
    /// `crate::state::release_reserved_silence_watch` and the silent-worker
    /// no-delivery invariant on `dispatch_one_owned`.
    pub fn arm_silence_watch(
        &self,
        worker_pane_id: &str,
        orchestrator_pane_id: &str,
        worker_agent_id: Option<&str>,
    ) -> Option<ArmedSilenceWatch> {
        let mut tracker = self.delegations.lock().unwrap();
        if tracker.closing_panes.contains(worker_pane_id)
            || tracker.closing_panes.contains(orchestrator_pane_id)
        {
            return None;
        }
        let seq = self.delegation_seq.fetch_add(1, Ordering::SeqCst);
        let superseded = tracker
            .silence_watches
            .get(worker_pane_id)
            .map_or(0, |prev| prev.superseded.saturating_add(1));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        tracker.silence_watches.insert(
            worker_pane_id.to_string(),
            SilenceWatchRecord {
                seq,
                superseded,
                orchestrator_pane_id: orchestrator_pane_id.to_string(),
                worker_agent_id: worker_agent_id.map(str::to_string),
                _cancel: cancel_tx,
            },
        );
        Some(ArmedSilenceWatch {
            seq,
            cancel: cancel_rx,
        })
    }

    /// Issue #448: record that the orchestrator has commissioned work from
    /// `worker_pane_id` and owes itself a `work-done` for it. Returns whether the
    /// commission was recorded.
    ///
    /// Armed for EVERY delegate the daemon dispatches, deliberately independent
    /// of both `worker_response_timeout_minutes` (PRD #126) and
    /// `delegate_no_event_window` (PRD #249). That independence is the whole
    /// point: those two knobs govern whether the daemon *watches* for an answer,
    /// while this ledger records that an answer is owed. Deriving "was this
    /// solicited?" from either watch made a project with the idle detector turned
    /// off indistinguishable from a worker nobody delegated to.
    ///
    /// Returns `false` — nothing recorded — when either pane is mid-close
    /// ([`Self::begin_pane_close`]), the same arm-after-cancel guard as
    /// [`Self::arm_outstanding_delegation`] and [`Self::arm_silence_watch`]: the
    /// close sweep has already passed, so an entry armed now would never be
    /// swept, and a phantom commission makes a later unsolicited completion read
    /// as solicited. Failing to record fails safe in the other direction (a
    /// genuine completion is *labelled* unsolicited rather than dropped), which
    /// is why this is a refusal and not a queue.
    ///
    /// Unlike the two watches, arming does not REPLACE a previous entry — it
    /// increments it. Two unanswered delegations to one worker are two
    /// commissions, so two completions are credited before a third is called
    /// unsolicited.
    pub fn arm_delegation_commission(
        &self,
        worker_pane_id: &str,
        orchestrator_pane_id: &str,
    ) -> bool {
        let mut tracker = self.delegations.lock().unwrap();
        if tracker.closing_panes.contains(worker_pane_id)
            || tracker.closing_panes.contains(orchestrator_pane_id)
        {
            return false;
        }
        let entry = tracker
            .commissions
            .entry(worker_pane_id.to_string())
            .or_insert_with(|| DelegationCommission {
                outstanding: 0,
                orchestrator_pane_id: orchestrator_pane_id.to_string(),
            });
        entry.outstanding = entry.outstanding.saturating_add(1);
        // Last delegate wins: a pane id that has changed hands (orchestrator
        // closed, successor spawned onto the same id) must not leave the ledger
        // pointing its close sweep at the dead pane.
        entry.orchestrator_pane_id = orchestrator_pane_id.to_string();
        true
    }

    /// Issue #448: credit a `work-done` from `worker_pane_id` against the
    /// commission ledger, and report whether the orchestrator had actually asked
    /// for anything — see [`WorkDoneProvenance`].
    ///
    /// The last commission for a pane removes its entry rather than leaving a
    /// zero behind, so the map tracks live debt instead of every worker pane that
    /// has ever been delegated to.
    pub fn retire_delegation_commission(&self, worker_pane_id: &str) -> WorkDoneProvenance {
        let mut tracker = self.delegations.lock().unwrap();
        let Some(outstanding) = tracker
            .commissions
            .get(worker_pane_id)
            .map(|entry| entry.outstanding)
        else {
            return WorkDoneProvenance::Unsolicited;
        };
        if outstanding <= 1 {
            tracker.commissions.remove(worker_pane_id);
            // A `0` entry cannot normally exist — this branch removes an entry as
            // it reaches zero — so the `Unsolicited` half is defense in depth
            // against a future arming path that leaves one behind.
            return if outstanding == 0 {
                WorkDoneProvenance::Unsolicited
            } else {
                WorkDoneProvenance::Solicited { remaining: 0 }
            };
        }
        let remaining = outstanding - 1;
        tracker
            .commissions
            .get_mut(worker_pane_id)
            .expect("entry present under the same lock")
            .outstanding = remaining;
        WorkDoneProvenance::Solicited { remaining }
    }

    /// Issue #448 review (finding 1): release ONE commission armed for
    /// `worker_pane_id` because the delegate that armed it never reached the
    /// worker. Returns whether an entry was found to release.
    ///
    /// The ledger's counterpart to [`Self::cancel_silence_watch_if`], and it
    /// exists for the same reason: the commission is armed in the synchronous
    /// fan-out, BEFORE the guarded send that may then refuse. Without it, a
    /// delegate that was never delivered leaves a debt standing forever — a
    /// worker owing a completion for work it was never given — and a later,
    /// genuinely uncommissioned `work-done` spends that phantom entry and is
    /// reported as `Solicited`. That is #448 and its summary-file clobber,
    /// reproduced through the very ledger added to prevent them.
    ///
    /// DECREMENTS rather than removing the entry: two delegations may be
    /// outstanding to one worker and only one of them failed, so dropping the
    /// whole entry would discard a sibling delegation's genuine commission and
    /// mislabel ITS completion as unsolicited. Saturating for the same
    /// defense-in-depth reason as [`Self::retire_delegation_commission`], and
    /// the entry is removed as it reaches zero so the map keeps tracking live
    /// debt rather than every pane ever delegated to.
    pub fn release_delegation_commission(&self, worker_pane_id: &str) -> bool {
        let mut tracker = self.delegations.lock().unwrap();
        let Some(entry) = tracker.commissions.get_mut(worker_pane_id) else {
            return false;
        };
        entry.outstanding = entry.outstanding.saturating_sub(1);
        if entry.outstanding == 0 {
            tracker.commissions.remove(worker_pane_id);
        }
        true
    }

    /// PRD #249 M3 review (finding B4): a `work-done` arrived from
    /// `worker_pane_id`, so ONE silent-worker watch is resolved — a completion is
    /// positive proof the pointer landed, and `work-done` is a CLI signal rather
    /// than an `AgentEvent`, so the watch's own event wait would never see it.
    ///
    /// PRD #249 round-6 review (Greptile, `handle_work_done`): "one" is
    /// deliberate, and this used to be an unconditional `remove`. Because
    /// [`Self::arm_silence_watch`] replaces the record, the map holds the NEWEST
    /// delegation's watch — so a `work-done` belonging to delegation N (late,
    /// duplicated or retried) disarmed delegation N+1's watch, and if N+1's
    /// pointer genuinely never landed no notice was ever emitted: the
    /// undelivered-prompt detector was silently disabled for exactly the failure
    /// it exists to surface.
    ///
    /// Completions are therefore applied oldest-first, the same accounting
    /// [`Self::retire_outstanding_delegation`] uses for the idle detector and for
    /// the same reason (no generation on the wire). Deliberately NOT keyed to the
    /// idle detector's record: the two detectors are independently switchable, so
    /// with `worker_response_timeout = 0` there is no delegation record to derive
    /// a generation from, and the silence watch must still cancel on a timely
    /// completion.
    ///
    /// It inherits PRD #126's accepted hole in the other direction: an
    /// OUT-OF-ORDER completion (the newest task reports while an older one never
    /// does) is credited to the older watch, so the newest stays armed and may
    /// emit one notice for work that is actually done. A discardable,
    /// self-describing notice is strictly safer than silence, and it only occurs
    /// in a state the orchestrator protocol already forbids (re-delegating before
    /// the worker reports).
    pub fn retire_silence_watch(&self, worker_pane_id: &str) -> SilenceWatchRetirement {
        let mut tracker = self.delegations.lock().unwrap();
        let Some(record) = tracker.silence_watches.get_mut(worker_pane_id) else {
            return SilenceWatchRetirement::Nothing;
        };
        if record.superseded > 0 {
            record.superseded -= 1;
            return SilenceWatchRetirement::KeptNewer {
                seq: record.seq,
                remaining: record.superseded,
            };
        }
        let seq = record.seq;
        tracker
            .silence_watches
            .remove(worker_pane_id)
            .expect("watch present under the same lock");
        SilenceWatchRetirement::Cancelled { seq }
    }

    /// PRD #249 M3 review (finding B4): cancel `worker_pane_id`'s silent-worker
    /// watch **only if** it is still generation `seq`. Two callers need the
    /// conditional form: the dispatch path cleaning up after a write that was
    /// refused or failed, and the watch task itself consuming its own record
    /// before it reports — a `false` there means work-done, a supersede or a
    /// pane close already resolved this delegation while the window ran, and the
    /// notice must be suppressed. Mirrors
    /// [`Self::take_outstanding_delegation_if`]: one mutex, exactly one winner.
    pub fn cancel_silence_watch_if(&self, worker_pane_id: &str, seq: u64) -> bool {
        let mut tracker = self.delegations.lock().unwrap();
        if tracker
            .silence_watches
            .get(worker_pane_id)
            .is_some_and(|w| w.seq == seq)
        {
            tracker.silence_watches.remove(worker_pane_id);
            true
        } else {
            false
        }
    }

    /// PRD #126: atomically take the outstanding delegation for
    /// `worker_pane_id` **only if** it is still generation `seq`. This single
    /// operation is what makes the detector correct on all three fronts:
    ///
    /// * **cancellation** — `work-done` (or a pane close) already took the
    ///   record, so the timer's take returns `None` and it is a silent no-op;
    /// * **one-shot** — the record is gone after the take, so one delegation
    ///   can never produce two prompts;
    /// * **re-delegation** — a newer delegate replaced the record with a
    ///   higher `seq`, so the stale timer's take fails the generation check
    ///   and leaves the newer record for the newer timer.
    ///
    /// Nothing is removed when the seq does not match.
    pub fn take_outstanding_delegation_if(
        &self,
        worker_pane_id: &str,
        seq: u64,
    ) -> Option<OutstandingDelegation> {
        let mut tracker = self.delegations.lock().unwrap();
        if tracker
            .records
            .get(worker_pane_id)
            .is_some_and(|d| d.seq == seq)
        {
            tracker.records.remove(worker_pane_id)
        } else {
            None
        }
    }

    /// PRD #126: a `work-done` arrived from `worker_pane_id`, so one outstanding
    /// delegation is resolved and owes no idle prompt.
    ///
    /// PRD #126 M1 review (finding 6): "one" is deliberate. `WorkDoneSignal`
    /// carries no delegation generation, so the daemon cannot tell *which*
    /// delegation a completion belongs to. It used to remove the record
    /// outright, which meant a late `work-done` from a superseded delegation
    /// disarmed the newest one and the second task could then go silent forever
    /// — the exact failure the detector exists to prevent. Now completions are
    /// applied oldest-first: while superseded delegations remain unaccounted
    /// for, a `work-done` retires one of THEM and the newest record (with its
    /// armed watch) survives.
    ///
    /// Remaining hole, deliberately accepted: with no generation on the wire,
    /// an OUT-OF-ORDER completion (the newest task reports while an older one
    /// never does) is still credited to the older delegation, so the newest
    /// record stays armed and produces one idle prompt for work that is
    /// actually done. That failure direction is a discardable, self-describing
    /// nudge — strictly safer than silence — and it only occurs in a state the
    /// orchestrator protocol already forbids (re-delegating before the worker
    /// reports).
    pub fn retire_outstanding_delegation(&self, worker_pane_id: &str) -> DelegationRetirement {
        let mut tracker = self.delegations.lock().unwrap();
        let Some(record) = tracker.records.get_mut(worker_pane_id) else {
            return DelegationRetirement::Nothing;
        };
        if record.superseded > 0 {
            record.superseded -= 1;
            return DelegationRetirement::RetiredSuperseded {
                role: record.role.clone(),
                seq: record.seq,
                remaining: record.superseded,
            };
        }
        DelegationRetirement::Retired(
            tracker
                .records
                .remove(worker_pane_id)
                .expect("record present under the same lock"),
        )
    }

    /// PRD #126 M1 review (finding 1) / audit (finding 2): begin a race-safe
    /// pane close. Atomically marks `pane_id` as closing and drops every
    /// outstanding delegation that touches it — as the *worker* (keyed by the
    /// pane) **and** as the *orchestrator* (records pointing their idle prompt
    /// at it). Returns the dropped records for logging; dropping them cancels
    /// their watch tasks.
    ///
    /// Called BEFORE child termination, which matters because `close_agent`
    /// removes the registry entry and may then spend up to
    /// `AGENT_TERMINATE_GRACE` in the SIGTERM grace loop: a timer firing in
    /// that window used to inject the very nudge a deliberate close exists to
    /// suppress. While the mark is set, [`Self::arm_outstanding_delegation`]
    /// refuses, so a concurrent delegate cannot re-arm behind the sweep.
    ///
    /// The old cancellation was also keyed by worker pane ONLY, so closing the
    /// ORCHESTRATOR left every worker's timer armed pointing at a pane id that a
    /// later, unrelated agent could inherit — hence the sweep over both roles.
    ///
    /// PRD #249 M3 review (finding B4/S4): the sweep covers silent-worker
    /// watches by the same two roles, for the same reason — a closed worker owes
    /// no proof of life, and a closed orchestrator has no pane to be told in.
    pub fn begin_pane_close(&self, pane_id: &str) -> Vec<OutstandingDelegation> {
        let mut tracker = self.delegations.lock().unwrap();
        tracker.closing_panes.insert(pane_id.to_string());
        // PRD #249 round-6 review (Greptile): wake anything waiting on this pane
        // BEFORE the up-to-`AGENT_TERMINATE_GRACE` termination starts, by dropping
        // its senders. Same ordering argument as the sweeps below.
        drop(tracker.close_waiters.remove(pane_id));
        let cancelled_watches = Self::drain_silence_watches_touching(&mut tracker, pane_id);
        if cancelled_watches > 0 {
            tracing::debug!(
                pane_id = %pane_id,
                cancelled_watches,
                "pane close: cancelled silent-worker watches touching this pane"
            );
        }
        let dropped_commissions = Self::drain_commissions_touching(&mut tracker, pane_id);
        if dropped_commissions > 0 {
            tracing::debug!(
                pane_id = %pane_id,
                dropped_commissions,
                "pane close: dropped delegation commissions touching this pane"
            );
        }
        Self::drain_delegations_touching(&mut tracker, pane_id)
    }

    /// PRD #126: finish the close transition opened by
    /// [`Self::begin_pane_close`]. Performs one final sweep (belt-and-braces
    /// against anything armed in the window) and then clears the closing mark.
    ///
    /// `closed` distinguishes the outcomes but the rollback is deliberately
    /// asymmetric: on a FAILED close the pane is un-marked so the still-live
    /// agent can be delegated to again, but the records swept at `begin` are
    /// **not** restored. Losing a watch fails safe (no idle prompt for a worker
    /// whose pane we just tried to kill); resurrecting one could nag about a
    /// delegation whose pane the user explicitly asked to close.
    pub fn finish_pane_close(&self, pane_id: &str, closed: bool) -> Vec<OutstandingDelegation> {
        let mut tracker = self.delegations.lock().unwrap();
        drop(tracker.close_waiters.remove(pane_id));
        Self::drain_silence_watches_touching(&mut tracker, pane_id);
        Self::drain_commissions_touching(&mut tracker, pane_id);
        let swept = Self::drain_delegations_touching(&mut tracker, pane_id);
        if !closed {
            tracing::debug!(
                pane_id = %pane_id,
                "pane close failed; clearing the closing mark without restoring dropped delegations"
            );
        }
        tracker.closing_panes.remove(pane_id);
        swept
    }

    /// PRD #126: whether `pane_id` is between [`Self::begin_pane_close`] and
    /// [`Self::finish_pane_close`]. The idle watch re-checks this immediately
    /// before writing (inside the guarded-send revalidation) so a close that
    /// started after the record was taken still suppresses the prompt.
    pub fn is_pane_closing(&self, pane_id: &str) -> bool {
        self.delegations
            .lock()
            .unwrap()
            .closing_panes
            .contains(pane_id)
    }

    /// PRD #249 round-6 review (Greptile, the readiness buffer): a future that
    /// resolves when `pane_id`'s close BEGINS, so a wait already in flight can be
    /// cancelled instead of sleeping out its remainder against a target that is
    /// being torn down. Deliberately the same shape as the two watches'
    /// cancellation channels — a `oneshot` the caller `select!`s on, resolved by
    /// the sender being DROPPED, never sent on.
    ///
    /// A pane that is ALREADY mid-close gets a pre-resolved receiver, which closes
    /// the register-after-`begin_pane_close` race the way
    /// [`Self::arm_outstanding_delegation`]'s refusal closes the arm-after-cancel
    /// one. A close that fully completed *before* the caller asked (mark set and
    /// cleared again) is not signalled — for that window the caller still relies
    /// on its identity-guarded write refusing, which is where the correctness
    /// lives either way.
    ///
    /// Senders whose receiver has already been dropped are pruned on each call, so
    /// the waiter list tracks live waits rather than growing with every delegate.
    pub fn pane_close_signal(&self, pane_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let mut tracker = self.delegations.lock().unwrap();
        if tracker.closing_panes.contains(pane_id) {
            // Dropping `tx` here resolves `rx` immediately.
            return rx;
        }
        let waiters = tracker
            .close_waiters
            .entry(pane_id.to_string())
            .or_default();
        waiters.retain(|waiter| !waiter.is_closed());
        waiters.push(tx);
        rx
    }

    /// Issue #584: a future that resolves when `agent_id`'s PTY reaches EOF —
    /// i.e. when its child is gone.
    ///
    /// The agent-scoped sibling of [`Self::pane_close_signal`]. Resolves
    /// IMMEDIATELY when the agent is already absent or already flagged `exited`,
    /// so a caller can never park on a corpse it registered for too late.
    ///
    /// Waiters are keyed by registry id, which is generation-scoped and never
    /// reused, so this can never be satisfied by a successor on the same pane.
    pub fn agent_exit_signal(&self, agent_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.inner.lock().unwrap();
        let live = inner
            .agents
            .get(agent_id)
            .is_some_and(|a| !a.exited.load(Ordering::SeqCst));
        if !live {
            // Dropping `tx` here resolves `rx` immediately.
            return rx;
        }
        let waiters = inner.exit_waiters.entry(agent_id.to_string()).or_default();
        waiters.retain(|waiter| !waiter.is_closed());
        waiters.push(tx);
        rx
    }

    /// Resolve every [`Self::agent_exit_signal`] waiter for `agent_id`, by
    /// dropping their senders. Called by `pump_reader` at EOF.
    fn signal_agent_exit(&self, agent_id: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            // A poisoned registry lock means the daemon is already in trouble;
            // the waiters' own timeouts still bound them, so this degrades to
            // the pre-#584 behaviour rather than failing anything.
            return;
        };
        drop(inner.exit_waiters.remove(agent_id));
    }

    /// Issue #606: is a `StopAgent` currently taking this pane apart?
    ///
    /// True from the moment the close is authorised (`hold_pane_for_cleanup`)
    /// or the pane is marked closing (`begin_pane_close`) until BOTH are
    /// released. Either alone is insufficient: the hold is taken first and
    /// dropped last, while `closing_panes` is what a failed close rolls back, so
    /// a caller that consults only one of them sees a pane that looks free while
    /// the other half of the teardown is still running.
    ///
    /// This is the question [`Self::respawn_or_recreate_agent_for_pane`] asks
    /// when it finds no entry to respawn from: a pane with no agent because a
    /// close is mid-flight is a pane to WAIT for, not a hard failure.
    pub fn pane_close_in_flight(&self, pane_id: &str) -> bool {
        // The two locks are taken SEQUENTIALLY, never nested: `inner` is
        // released before `is_pane_closing` reaches for `delegations`. Nothing
        // in this file holds `delegations` while taking `inner`, and this keeps
        // it that way from the other direction too.
        let held_for_cleanup = {
            let Ok(inner) = self.inner.lock() else {
                // A poisoned registry lock is not evidence that the pane is
                // free; fall through to the closing mark rather than inventing
                // an answer that would let a spawn race a live teardown.
                return true;
            };
            inner.cleanup_holds.contains(pane_id)
        };
        held_for_cleanup || self.is_pane_closing(pane_id)
    }

    /// Remove every record that names `pane_id` as its worker key or as its
    /// orchestrator target. Caller holds the tracker lock.
    fn drain_delegations_touching(
        tracker: &mut DelegationTracker,
        pane_id: &str,
    ) -> Vec<OutstandingDelegation> {
        let keys: Vec<String> = tracker
            .records
            .iter()
            .filter(|(worker_pane, record)| {
                worker_pane.as_str() == pane_id || record.orchestrator_pane_id == pane_id
            })
            .map(|(worker_pane, _)| worker_pane.clone())
            .collect();
        keys.iter()
            .filter_map(|key| tracker.records.remove(key))
            .collect()
    }

    /// PRD #249 M3 review (finding B4/S4): the [`Self::drain_delegations_touching`]
    /// counterpart for silent-worker watches — remove every watch that names
    /// `pane_id` as its worker key or as its orchestrator target, cancelling each
    /// one by dropping its record. Returns how many were cancelled, for logging.
    /// Caller holds the tracker lock.
    fn drain_silence_watches_touching(tracker: &mut DelegationTracker, pane_id: &str) -> usize {
        let keys: Vec<String> = tracker
            .silence_watches
            .iter()
            .filter(|(worker_pane, watch)| {
                worker_pane.as_str() == pane_id || watch.orchestrator_pane_id == pane_id
            })
            .map(|(worker_pane, _)| worker_pane.clone())
            .collect();
        keys.iter()
            .filter(|key| tracker.silence_watches.remove(*key).is_some())
            .count()
    }

    /// Issue #448: the [`Self::drain_delegations_touching`] counterpart for the
    /// commission ledger — forget every commission that names `pane_id` as its
    /// worker key or as its orchestrator. Returns how many entries were dropped,
    /// for logging. Caller holds the tracker lock.
    ///
    /// Swept by BOTH roles for the same reason as the watches: a closed worker
    /// owes nothing, and a commission owed to a closed orchestrator must not
    /// survive to be credited to whichever agent inherits its pane id. A
    /// deliberate close therefore also ends the "was this solicited?" question,
    /// which is the fail-safe direction — a stale commission would launder a
    /// genuinely unsolicited later completion into a solicited one.
    fn drain_commissions_touching(tracker: &mut DelegationTracker, pane_id: &str) -> usize {
        let keys: Vec<String> = tracker
            .commissions
            .iter()
            .filter(|(worker_pane, commission)| {
                worker_pane.as_str() == pane_id || commission.orchestrator_pane_id == pane_id
            })
            .map(|(worker_pane, _)| worker_pane.clone())
            .collect();
        keys.iter()
            .filter(|key| tracker.commissions.remove(*key).is_some())
            .count()
    }

    /// The [`Self::drain_delegations_touching`] counterpart used
    /// by [`Self::sweep_delegations_on_exit`] — both sides of the match are
    /// identity-gated here, unlike the deliberate-close helper: a WORKER-side
    /// match additionally requires `record.worker_agent_id` to be bound AND
    /// equal to `exited_agent_id`, and an ORCHESTRATOR-side match additionally
    /// requires `record.orchestrator_agent_id` to equal `exited_agent_id`.
    /// Both gates close the same pane-id-reuse window: `pump_reader` sets
    /// `exited` before this sweep runs, and `spawn_agent`'s duplicate-pane-id
    /// guard permits a new agent onto the same `pane_id_env` once the previous
    /// occupant is `exited`, so without a gate a dead predecessor's sweep
    /// could drain a live successor's records — worker or orchestrator — that
    /// merely happens to share the reused pane id. A record whose worker
    /// identity has not resolved yet (`None`) is left alone by the
    /// WORKER-side arm: the agent that just exited is necessarily some OTHER,
    /// earlier occupant of the pane, not the one this delegation is for. It
    /// can still be drained by the ORCHESTRATOR-side arm, which does not
    /// depend on `worker_agent_id` at all. Caller holds the tracker lock.
    fn drain_delegations_touching_for_exit(
        tracker: &mut DelegationTracker,
        pane_id: &str,
        exited_agent_id: &str,
    ) -> Vec<OutstandingDelegation> {
        let keys: Vec<String> = tracker
            .records
            .iter()
            .filter(|(worker_pane, record)| {
                (worker_pane.as_str() == pane_id
                    && record.worker_agent_id.as_deref() == Some(exited_agent_id))
                    || (record.orchestrator_pane_id == pane_id
                        && record.orchestrator_agent_id == exited_agent_id)
            })
            .map(|(worker_pane, _)| worker_pane.clone())
            .collect();
        keys.iter()
            .filter_map(|key| tracker.records.remove(key))
            .collect()
    }

    /// The [`Self::drain_silence_watches_touching`] counterpart
    /// used by [`Self::sweep_delegations_on_exit`] — the WORKER-side match
    /// uses the same identity gate as
    /// [`Self::drain_delegations_touching_for_exit`]. The ORCHESTRATOR-side
    /// match here, unlike that sibling helper, stays unconditional on
    /// `record.orchestrator_pane_id == pane_id` alone:
    /// [`SilenceWatchRecord`] carries no `orchestrator_agent_id` field, so
    /// there is nothing to gate on without widening the struct. The
    /// consequence of pane-id reuse landing here is accepted as narrower than
    /// the delegation case — worst case is a live successor orchestrator
    /// losing its silence-watch safety net, not a misdelivery, because
    /// [`Self::write_notice_guarded`]'s own identity check is what actually
    /// prevents the notice from reaching the wrong recipient. Caller holds
    /// the tracker lock.
    fn drain_silence_watches_touching_for_exit(
        tracker: &mut DelegationTracker,
        pane_id: &str,
        exited_agent_id: &str,
    ) -> usize {
        let keys: Vec<String> = tracker
            .silence_watches
            .iter()
            .filter(|(worker_pane, watch)| {
                (worker_pane.as_str() == pane_id
                    && watch.worker_agent_id.as_deref() == Some(exited_agent_id))
                    || watch.orchestrator_pane_id == pane_id
            })
            .map(|(worker_pane, _)| worker_pane.clone())
            .collect();
        keys.iter()
            .filter(|key| tracker.silence_watches.remove(*key).is_some())
            .count()
    }

    /// Worker-exit sweep: called from `pump_reader`'s EOF branch the moment a
    /// pane's PTY reaches EOF — the daemon's earliest, unconditional signal
    /// that the pane's process is gone, independent of whether it ever called
    /// `work-done` or went through an explicit `StopAgent` close. Retires any
    /// armed `OutstandingDelegation`/`SilenceWatchRecord` touching this pane
    /// (as worker OR as orchestrator target, same as [`Self::begin_pane_close`])
    /// instead of leaving either sit armed for its full timeout window.
    ///
    /// Deliberately does NOT call [`Self::begin_pane_close`]/
    /// [`Self::finish_pane_close`] — those also mark `closing_panes` (with no
    /// natural "finish" call for a process that died on its own, which would
    /// leave the mark stuck) and drop `close_waiters` (which signal a
    /// *deliberate* close, not a natural exit). This reuses the same
    /// underlying drain helpers those two call, without either of their
    /// close-transition side effects.
    ///
    /// Deliberately does NOT drain [`DelegationCommission`] entries the way
    /// [`Self::begin_pane_close`]/[`Self::finish_pane_close`] do. This sweep's
    /// scope is retiring the two TIMEOUT watches (`OutstandingDelegation`,
    /// `SilenceWatchRecord`) so a natural exit is detected promptly instead of
    /// waiting out a timer, not the commission ledger's "was this solicited?"
    /// bookkeeping. For the WORKER side this is a settled decision: a worker
    /// that received its task pointer and then exited without reporting is a
    /// genuine, still-owed commission, not an undelivered one, so there is
    /// nothing here for the ledger's no-delivery invariant to release. The
    /// same non-drain also applies to the ORCHESTRATOR side of a natural
    /// exit, and that half is a known, accepted asymmetry rather than an
    /// oversight: [`Self::drain_commissions_touching`] is only ever invoked
    /// from the *deliberate*-close path (`begin_pane_close`/
    /// `finish_pane_close`), so a naturally-exiting orchestrator's commission
    /// entries — keyed by worker pane id — outlive the exit. If that worker
    /// pane id is later reused, an unrelated agent's genuinely-uncommissioned
    /// `work-done` is credited `Solicited` and overwrites the role's
    /// `work-done-<role>.md`. Accepted for now because the reverse (draining
    /// on natural exit here) is a larger, separately-scoped change; a
    /// deliberate close already closes the gap for the case that goes through
    /// it.
    ///
    /// Idempotent by construction: [`Self::drain_delegations_touching_for_exit`]/
    /// [`Self::drain_silence_watches_touching_for_exit`] no-op on a pane with
    /// no matching record, so a race against a near-simultaneous `work-done`
    /// or explicit close — whichever reaches the tracker first — leaves
    /// nothing for the other to retire twice.
    ///
    /// Unlike [`Self::begin_pane_close`]/[`Self::finish_pane_close`],
    /// a WORKER-side match here is additionally gated on `exited_agent_id` —
    /// see [`Self::drain_delegations_touching_for_exit`]/[`Self::drain_silence_watches_touching_for_exit`]
    /// and [`OutstandingDelegation::worker_agent_id`] for why. The two
    /// deliberate closes need no such gate: their sweep runs INSIDE the same
    /// operation that decided to end the pane, with no dispatch/respawn window
    /// in between for a fresher, not-yet-bound delegation to be mistaken for
    /// the one being closed.
    ///
    /// Private: `pump_reader`, its only caller, lives in this same module.
    fn sweep_delegations_on_exit(
        &self,
        pane_id: &str,
        exited_agent_id: &str,
    ) -> Vec<OutstandingDelegation> {
        let mut tracker = self.delegations.lock().unwrap();
        let cancelled_watches =
            Self::drain_silence_watches_touching_for_exit(&mut tracker, pane_id, exited_agent_id);
        let swept =
            Self::drain_delegations_touching_for_exit(&mut tracker, pane_id, exited_agent_id);
        if cancelled_watches > 0 || !swept.is_empty() {
            tracing::debug!(
                pane_id = %pane_id,
                cancelled_watches,
                swept_delegations = swept.len(),
                "pane EOF: retired outstanding delegation/silence-watch records for this pane"
            );
        }
        swept
    }

    /// Whether `agent_id` still names a live entry in the
    /// registry. `pump_reader`'s EOF branch uses this to tell a process that
    /// died NATURALLY (nothing has removed its entry yet — the death is news
    /// to the registry) from one whose death was the daemon's own doing:
    /// [`Self::close_agent`] and [`Self::respawn_agent_for_pane`] both
    /// remove the entry BEFORE killing its child, so by the time that kill's
    /// resulting EOF is observed, the entry is already gone and this
    /// correctly answers `false`.
    fn is_agent_still_registered(&self, agent_id: &str) -> bool {
        self.inner.lock().unwrap().agents.contains_key(agent_id)
    }

    /// Deliver the "worker exited without work-done" notice for
    /// one [`OutstandingDelegation`] [`Self::sweep_delegations_on_exit`] just
    /// swept off `worker_pane_id`. Follows exactly the guarded-write path PRD
    /// #249's silence watch already uses: compose the fixed-text notice, write
    /// it through [`Self::write_notice_guarded`] bound to the orchestrator's
    /// registry agent id captured when the delegation was armed, with a
    /// revalidation closure that refuses a pane that is mid-close or has since
    /// been re-homed into a different orchestration
    /// ([`crate::state::orchestration_still_matches`]) — the same identity
    /// guard every other daemon-authored notice in this file relies on, so a
    /// pane id freed by the exit and reused by an unrelated agent cannot
    /// receive this orchestration's diagnostics.
    ///
    /// Called from `pump_reader`'s EOF branch via a `tokio::runtime::Handle`
    /// captured at spawn time, since the reader thread itself is a bare
    /// `std::thread` with no async context of its own — see that function's
    /// doc comment for why the handle can be absent and what happens then.
    ///
    /// A worker that calls `work-done` and then exits immediately races this
    /// path against the hook-socket `work-done` message: the two have no
    /// ordering guarantee between them, so if the PTY's EOF is observed
    /// first, this notice can fire right before the genuine `work-done`
    /// arrives and finds no record left to retire. The window is small — the
    /// socket write completes before the process exits in the normal case —
    /// so this is accepted as low-probability rather than fixed with an
    /// added delivery delay.
    async fn deliver_worker_exited_notice(
        self: &Arc<Self>,
        worker_pane_id: &str,
        delegation: OutstandingDelegation,
    ) {
        let notice = crate::state::compose_worker_exited_notice(worker_pane_id);
        let orchestrator_pane_id = delegation.orchestrator_pane_id.clone();
        let expected_agent_id = delegation.orchestrator_agent_id.clone();
        let orchestration = delegation.orchestration.clone();
        let revalidate_registry = Arc::clone(self);
        let revalidate_pane = orchestrator_pane_id.clone();
        let outcome = self
            .write_notice_guarded(
                &orchestrator_pane_id,
                &notice,
                Some(&expected_agent_id),
                || async move {
                    if revalidate_registry.is_pane_closing(&revalidate_pane) {
                        return false;
                    }
                    crate::state::orchestration_still_matches(
                        orchestration.as_ref(),
                        revalidate_registry
                            .pane_orchestration(&revalidate_pane)
                            .as_ref(),
                    )
                },
            )
            .await;
        match outcome {
            Ok(GuardedSend::Applied) => tracing::info!(
                worker_pane_id = %worker_pane_id,
                role = %delegation.role,
                "pane EOF: reported a worker that exited without work-done to the orchestrator"
            ),
            // Some bytes reached the authorized target; a retry would
            // duplicate a half-written line rather than repair it.
            Ok(GuardedSend::Ambiguous) => tracing::warn!(
                pane_id = %orchestrator_pane_id,
                role = %delegation.role,
                "pane EOF: worker-exited notice delivery was ambiguous (partial write); not \
                 retried"
            ),
            Ok(refused) => tracing::debug!(
                pane_id = %orchestrator_pane_id,
                role = %delegation.role,
                expected_agent_id = %expected_agent_id,
                outcome = ?refused,
                "pane EOF: identity gate refused the worker-exited notice; nothing written"
            ),
            Err(e) => tracing::warn!(
                pane_id = %orchestrator_pane_id,
                role = %delegation.role,
                error = %e,
                "pane EOF: failed to write the worker-exited notice into the orchestrator pane"
            ),
        }
    }

    /// PRD #126 M1 audit (finding 2): the orchestration membership of the live
    /// agent on `pane_id`, per its registry `tab_membership`. `None` when no live
    /// agent owns the pane, or when it carries no orchestration membership (a
    /// dashboard/mode pane, or a pane spawned without membership metadata).
    ///
    /// The idle watch uses it twice: to refuse delivery into a pane that has
    /// since been re-homed into a *different* orchestration (because `None` is
    /// legitimate, only a positive mismatch refuses — see
    /// [`crate::state::orchestration_still_matches`]), and, at arm time, to
    /// recover the **orchestration cwd** for config resolution, which PRD #140's
    /// `Instance` routing identity no longer carries.
    pub fn pane_orchestration(&self, pane_id: &str) -> Option<PaneOrchestration> {
        let inner = self.inner.lock().unwrap();
        inner
            .agents
            .values()
            .find(|a| a.pane_id_env.as_deref() == Some(pane_id) && !a.exited.load(Ordering::SeqCst))
            .and_then(|a| match a.tab_membership.as_ref() {
                Some(TabMembership::Orchestration {
                    name,
                    orchestration_id,
                    orchestration_cwd,
                    ..
                }) => Some(PaneOrchestration {
                    name: name.clone(),
                    instance_id: orchestration_id.clone(),
                    cwd: orchestration_cwd.clone(),
                }),
                _ => None,
            })
    }

    /// PRD #127 M2.2: record that a user keystroke just reached the pane with
    /// `pane_id_env` (the deliver-on-idle debounce clock). Called from the
    /// attach-stream STREAM_IN path. Sentinel / empty pane ids are ignored.
    pub fn note_user_input(&self, pane_id_env: &str) {
        self.pane_input.lock().unwrap().note_user_input(pane_id_env);
    }

    /// PRD #127 M2.2: the last time a user keystroke reached `pane_id_env`, or
    /// `None` if none has. The reuse path compares this against the debounce
    /// window to choose deliver-now vs queue.
    pub fn last_user_input_at(&self, pane_id_env: &str) -> Option<Instant> {
        self.pane_input
            .lock()
            .unwrap()
            .last_user_input_at(pane_id_env)
    }

    /// Issue #424 F1: has a USER keystroke reached `pane_id_env` since the last
    /// time this daemon SUBMITTED into it?
    ///
    /// This is the question a SUBMIT-ONLY PROBE has to answer, and it is not the
    /// question any of the existing guards answer. Identity, generation, writer
    /// serialization and the deadline all establish WHICH PANE the delivery may
    /// touch; none of them says anything about what the pane's input editor is
    /// currently holding. Attempts 3 and later write an EMPTY payload plus a
    /// submit CR ([`crate::prompt_delivery::attempt_writes_payload`]), whose
    /// entire effect is "submit whatever is in the box". That is exactly right
    /// while the box still holds the payload we wrote and wrong the moment it
    /// does not: a user who typed an unrelated prompt, a slash command or a
    /// half-finished thought after our last write, and deliberately did not
    /// press Enter, would have it submitted for them — repeatedly, once per
    /// remaining attempt, in their own pane. That is not an attacker scenario;
    /// it is an ordinary person typing.
    ///
    /// Both clocks are the daemon's own: the user's is fed by the attach
    /// STREAM_IN path (real keystrokes, stamped by [`PaneWriter`] as the bytes
    /// are written) and ours by the guarded send itself, so nothing a producer
    /// can assert moves either one. A pane with no recorded user input answers
    /// `false` and probes normally, which is every headless scheduled and
    /// dispatch pane.
    ///
    /// Issue #424 H2: the clock compared against is the last SUBMIT-mode write,
    /// NOT any write. A [`SubmitMode::Notice`] leaves the user's draft in the
    /// box beside its own bytes, so letting one advance this clock is precisely
    /// how an ordinary orchestrator notice laundered a later blind probe into
    /// submitting that draft. See [`AutomaticWrite::submitted_at`].
    pub fn user_typed_since_automatic_write(&self, pane_id_env: &str) -> bool {
        self.pane_input
            .lock()
            .unwrap()
            .user_typed_since_submitting(pane_id_env)
    }

    /// Issue #424 F1 (replacement-payload half): would writing `text` into
    /// `pane_id_env` REPEAT bytes we already put there, after the user has typed
    /// since we put them there?
    ///
    /// This is the question the one bounded replacement payload has to answer,
    /// and it is deliberately narrower than the probe's
    /// ([`Self::user_typed_since_automatic_write`]). A probe is blind — its
    /// entire effect is a CR — so ANY user input makes it unsafe. A payload
    /// write is not blind, and refusing every payload write into a pane the user
    /// has typed in would be a cure worse than the disease:
    ///
    /// * **It would refuse the initial delivery.** With no write of ours on
    ///   record this returns `false`, so attempt 1 always proceeds. Refusing it
    ///   would re-open the very bug #424 reports — a seed prompt that never
    ///   arrives — for any pane whose user happened to type first.
    /// * **It would brick the pane permanently.** A refusal writes nothing, so
    ///   it cannot advance the automatic-write clock; a broader predicate would
    ///   therefore stay true forever once the user typed, and every later
    ///   delegate route, orchestration hand-off and deck-initiated send into
    ///   that pane would be refused for the rest of the daemon's life. Even the
    ///   user submitting their own draft would not clear it — pressing Enter is
    ///   another keystroke.
    ///
    /// Keyed on the bytes instead, the property is one the retry chain actually
    /// depends on: *the only reason to write the same payload again is that we
    /// believe our text is not in that box, and the user's keystrokes are
    /// exactly what invalidates that belief.* A genuinely NEW automatic or
    /// user-initiated delivery carries different bytes and is unaffected.
    ///
    /// Issue #424 H3 (both reviewers): the bytes are the right MATERIAL to
    /// compare, but they are not by themselves a delivery identity. The record
    /// is per WRITE rather than a single last-payload slot, so an independent
    /// guarded submit of different bytes can no longer evict an older delivery's
    /// record and launder its replacement in, and two deliveries carrying the
    /// same bytes hold two units of guard rather than sharing one (S2); and it
    /// is SCOPED TO THE LIFETIME of the delivery that wrote it rather than
    /// living forever, so the same fixed text delivered again later is a first
    /// write, not a repeat. It stops being a repeat when any of these happens:
    ///
    /// * **the user submits.** A submission through [`PaneWriter`] drains the
    ///   input box, so nothing of ours is left in it to double
    ///   ([`PaneInputState::note_user_bytes`] — decided by [`UserInputStream`],
    ///   because a newline inside a paste is editor content, and a newline KEY
    ///   is the user carrying on typing).
    /// * **the delivery reaches a terminal outcome.** The detached confirmation
    ///   loop releases each write it made when it confirms, abandons or stops,
    ///   and every one-shot caller — the delegate pointer, the idle-worker
    ///   report, a `deliver` with no event bus — releases as soon as its write
    ///   returns, because nothing will ever retry it
    ///   ([`Self::note_payload_settled`]).
    /// * **a different agent takes the pane.** A respawn into the same
    ///   `pane_id_env` is a new input box ([`Self::forget_pane_input`]).
    /// * **[`PAYLOAD_RECORD_TTL`] elapses** — the backstop for a delivery whose
    ///   completion this daemon never sees, e.g. one the TUI confirms.
    ///
    /// What deliberately remains: inside that window, an independent delivery of
    /// the *same* bytes into a pane holding an unsent draft is indistinguishable
    /// from the retry it would be doubling, and is refused. That is the closed
    /// direction — refused and reported, never silently submitted on top of what
    /// the user typed.
    ///
    /// Residual, deliberately out of scope here and tracked as **issue #544**: a
    /// new, DIFFERENT payload delivered into a pane holding an unsent user draft
    /// still concatenates with it — the long-documented limitation on
    /// [`Self::write_to_pane_and_submit`] — because the alternative is the brick
    /// above. Both reviewers ruled it a pre-existing limitation of every
    /// automatic payload rather than a regression introduced here.
    pub fn user_typed_since_writing_payload(&self, pane_id_env: &str, text: &str) -> bool {
        let Ok(payload) = encode_pane_payload(text) else {
            // A payload the encoder rejects is never written, so it can never be
            // a repeat of one that was.
            return false;
        };
        self.user_typed_since_writing_encoded(pane_id_env, &payload)
    }

    /// [`Self::user_typed_since_writing_payload`] against bytes the caller has
    /// already encoded — the form `write_and_submit_guarded` holds at the point
    /// it enforces the guard.
    fn user_typed_since_writing_encoded(&self, pane_id_env: &str, payload: &[u8]) -> bool {
        self.pane_input
            .lock()
            .unwrap()
            .user_typed_since_writing(pane_id_env, payload)
    }

    /// Issue #424 H3: ONE write of `text` into `pane_id_env` is over —
    /// confirmed, abandoned, stopped, or final the moment it returned — so its
    /// payload record is no longer protecting a retry and must not refuse an
    /// unrelated future delivery of the same bytes.
    ///
    /// Called once per PAYLOAD WRITE the caller made, not once per delivery: the
    /// detached confirmation loop's first write and its one bounded replacement
    /// each leave their own record. Issue #424 S2 — a call releases exactly one
    /// record, so a concurrent delivery of the same bytes keeps its own. See
    /// [`Self::user_typed_since_writing_payload`] for the full lifecycle.
    pub fn note_payload_settled(&self, pane_id_env: &str, text: &str) {
        let Ok(payload) = encode_pane_payload(text) else {
            return;
        };
        self.pane_input
            .lock()
            .unwrap()
            .forget_payload(pane_id_env, &payload);
    }

    /// Issue #424 H3: a different agent now owns `pane_id_env`, so drop what the
    /// previous occupant's guarded sends recorded about its input box. The pane
    /// id is reusable by design (same-pane respawn), and a stale record against
    /// it can only refuse the newcomer's first delivery.
    fn forget_pane_input(&self, pane_id_env: &str) {
        self.pane_input.lock().unwrap().forget_pane(pane_id_env);
    }

    /// Issue #424 F4: record that `agent_id`'s pane declared, before its prompt
    /// was written, that a real agent of type `declared` is starting behind a
    /// launcher. See [`Self::launcher_handoff_agents`] and
    /// [`crate::state::SessionStartWait::launcher_handoff`].
    ///
    /// Issue #666: FIRST declaration wins. A second `wrapper_fork` start naming
    /// a different type does not revise the belief — otherwise a producer that
    /// can post one could walk the pane's believed type to whatever it needs the
    /// post-write declaration to match, which is the grant #424 F4 forbids.
    pub fn note_launcher_handoff(&self, agent_id: &str, declared: AgentType) {
        self.launcher_handoff_agents
            .lock()
            .unwrap()
            .entry(agent_id.to_string())
            .or_insert(declared);
    }

    /// Issue #424 F4: whether `agent_id`'s pane made that declaration — one of
    /// the two standings on which a producer identifying itself AFTER the write
    /// may arm this delivery's retries. The other is
    /// [`Self::agent_spawned_as_reporting_agent`].
    pub fn agent_declared_launcher_handoff(&self, agent_id: &str) -> bool {
        self.launcher_handoff_agents
            .lock()
            .unwrap()
            .contains_key(agent_id)
    }

    /// Issue #666: WHICH AGENT TYPE the deck believed occupied `agent_id`'s pane
    /// before a byte of its spawn-time prompt was written, or `None` if nothing
    /// the deck can vouch for ever said.
    ///
    /// This is fact S of [`crate::prompt_delivery::AgentStartRearm`], and it is a
    /// TYPE rather than the `bool` the two accessors above answer, because the
    /// question the rearm asks is not "did anything vouch for this pane" but
    /// "does the post-write declaration AGREE with what we already believed".
    /// Without the type, a pane the deck spawned as Codex is armed by an event
    /// that merely claims to be Claude Code — a declared type GRANTING privilege,
    /// which is exactly what #424 F4 forbids and what `scheduler/dispatch/016`
    /// cases G and H pin.
    ///
    /// **The deck-spawn record wins.** It is the stronger of the two halves —
    /// [`RunningAgent::spawn_agent_type`] is the frozen launch-shape identity the
    /// spawn site supplied and no hook path can write it, while the launcher
    /// declaration is a producer assertion that is merely *not post hoc* (see
    /// [`crate::prompt_delivery::AgentStartRearm::new`] and **#543**). So a pane
    /// the deck exec'd itself cannot have its believed type revised by anything a
    /// producer posts, at any point.
    ///
    /// Deliberately NOT filtered through
    /// [`crate::prompt_delivery::agent_reports_submitted_prompt`] the way
    /// [`Self::agent_spawned_as_reporting_agent`] is: the rearm asks the strictly
    /// narrower [`crate::prompt_delivery::agent_start_precedes_first_prompt`] of
    /// whatever comes back, so pre-filtering here would only hide which type a
    /// refusal was about. The launcher half arrives already filtered, because the
    /// readiness gate withholds the declaration itself from a wrapped Pi.
    pub fn pre_write_believed_agent_type(&self, agent_id: &str) -> Option<AgentType> {
        if let Some(spawned) = self
            .inner
            .lock()
            .unwrap()
            .agents
            .get(agent_id)
            .and_then(|agent| agent.spawn_agent_type.clone())
        {
            return Some(spawned);
        }
        self.launcher_handoff_agents
            .lock()
            .unwrap()
            .get(agent_id)
            .cloned()
    }

    /// Issue #243: the FROZEN launch-shape identity `agent_id` was spawned as —
    /// the agent type the spawn site computed from the command it was about to
    /// exec ([`SpawnOptions::agent_type`]), or `None` for a command the deck could
    /// not resolve.
    ///
    /// Reads [`RunningAgent::spawn_agent_type`], NOT [`RunningAgent::agent_type`],
    /// and the difference matters for the same reason it does in
    /// [`Self::agent_spawned_as_reporting_agent`]: the readiness gate uses this to
    /// decide whether to SHORTEN its wait, and `agent_type` is upgradable in place
    /// by [`Self::set_agent_type`] from a hook event. Keying off the observed badge
    /// would let any same-user producer post one `SessionStart` claiming to be an
    /// agent that declares no pre-prompt signal and thereby talk the gate out of
    /// waiting — turning a producer assertion into control over when the deck
    /// writes a prompt. `spawn_agent_type` is what the deck itself exec'd, and no
    /// hook path can write it.
    pub fn spawn_agent_type(&self, agent_id: &str) -> Option<AgentType> {
        self.inner
            .lock()
            .unwrap()
            .agents
            .get(agent_id)
            .and_then(|agent| agent.spawn_agent_type.clone())
    }

    /// Issue #243 (audit F1): did THIS DAEMON spawn `agent_id` under
    /// `dot-agent-deck wrap` — i.e. is the frozen launch-shape identity an agent
    /// whose registry strategy is [`crate::agent_registry::IntegrationStrategy::Wrapper`]?
    ///
    /// The provenance check for the wrapper's interface-ready marker, and the
    /// reason the marker can be trusted to select a post-readiness buffer of its
    /// own at all.
    /// That marker is NOT authenticated on the wire: the daemon's hook socket
    /// accepts a raw `AgentEvent` line, `metadata` is free-form, and #243's audit
    /// reproduced a forged `wrapper_interface_ready` `SessionStart` from a bare
    /// `python3` with no deck environment. `crate::hook`'s refusal to forward the
    /// value is real but is not the chokepoint, so the daemon establishes
    /// provenance itself, at the site that acts on it.
    ///
    /// Being fair about the delta this closes: releasing the readiness GATE was
    /// already forgeable before #243 — a bare unmarked `SessionStart` satisfies
    /// `crate::state::session_start_means_ready`'s first branch — and this does not
    /// change that. What #243's round 2 newly granted was the ability to also
    /// SUPPRESS the buffer, which is the last protection against writing into a
    /// still-booting agent (#199/#249/#663), and this is what took that back.
    /// Round 3 retracted the suppression outright — the strong fact now selects a
    /// LONGER buffer (5000 ms) rather than none — so what a forgery is left
    /// reaching for is a mis-priced interval, not a suppressed one, and this
    /// check is what keeps even that out of a producer's hands.
    ///
    /// Same field, and the same argument, as [`Self::agent_spawned_as_reporting_agent`]:
    /// it reads [`RunningAgent::spawn_agent_type`], the launch-shape identity the
    /// spawn site supplied, which [`Self::set_agent_type`] — the learn-from-hook
    /// upgrade — never writes. Reading the badge instead would let a producer post
    /// one event claiming to be Codex and buy back exactly the privilege this
    /// removes. `false` for a pane the deck could not resolve to an agent, which
    /// is the direction that grants nothing: the ordinary buffer applies, exactly
    /// as it did before this issue. Note that since round 3 that is the SHORTER
    /// of the two intervals, so refusing is no longer automatically the cautious
    /// answer for an honest agent — see the case it refuses, below, and guard 2's
    /// alarm in `crate::state::dispatch_one_owned`.
    ///
    /// It answers the LAUNCH-SHAPE question rather than the readiness-class one,
    /// because `wrap_launch_command` keys the wrap decision on the same
    /// `strategy` field. An agent that declares
    /// [`crate::agent_registry::PrePromptReadiness::WrapperInterfaceReady`]
    /// without being wrapper-hosted has no wrapper to observe it, so there is no
    /// honest event for this to admit.
    ///
    /// **One honest case it refuses**, and it refuses it in the safe direction: a
    /// role command that ALREADY names the wrapper (`dot-agent-deck wrap --agent
    /// codex -- …`, which `wrap_launch_command` leaves alone rather than
    /// double-wrapping). `AgentType::from_command` cannot see an agent through
    /// that shape, so unless the pane was created with an explicit identity the
    /// frozen record is `None` and this answers `false`. A genuine wrapper is
    /// running and its event is genuine; it simply costs that pane the interface
    /// buffer, so it waits the ordinary 1000 ms rather than the 5000 ms measured
    /// against a full-screen TUI's own initialisation. The deck rewrites the
    /// command itself on every ordinary path, so this is a hand-written shape,
    /// and being priced like every non-wrapper agent is the right price for not
    /// having to trust the marker.
    pub fn agent_spawned_as_wrapper_host(&self, agent_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .agents
            .get(agent_id)
            .and_then(|agent| agent.spawn_agent_type.as_ref())
            .is_some_and(|agent_type| {
                crate::agent_registry::spec(agent_type).strategy
                    == Some(crate::agent_registry::IntegrationStrategy::Wrapper)
            })
    }

    /// Issue #570: whether THIS DAEMON spawned `agent_id` as an agent type it
    /// selected itself, and that type reports submitted prompts.
    ///
    /// The second standing for accepting a post-write producer, and the same
    /// KIND of fact as [`Self::agent_declared_launcher_handoff`]: a statement
    /// about the pane made before a byte of the prompt was written. It is a
    /// STRONGER one, because the deck did not merely observe it — the deck
    /// exec'd that command. `default_command = "claude …"` means the pane holds
    /// Claude Code because we put it there, so a Claude Code producer
    /// announcing itself on that pane a moment later is the expected occupant
    /// arriving, not an unauthenticated claim about a pane we cannot vouch for.
    ///
    /// It reads [`RunningAgent::spawn_agent_type`], NOT
    /// [`RunningAgent::agent_type`], and the difference is the whole security
    /// argument: `spawn_agent_type` is the frozen launch-shape identity the
    /// caller supplied at spawn ([`SpawnOptions::agent_type`], computed by the
    /// spawn site from the command via [`AgentType::from_command`]), and
    /// [`Self::set_agent_type`] — the learn-from-hook-event upgrade — never
    /// writes it. So no producer, honest or forged, can manufacture this
    /// standing for itself; a pane spawned as a bare shell, `cat`, a recorder
    /// stand-in or any command the deck could not resolve stays `None` and
    /// keeps refusing, which is exactly what #424 F4 protects.
    ///
    /// Note this deliberately does not make the delivery armed at write time
    /// (that stays [`crate::state::SessionStartWait::observed_producer`]'s
    /// job): it licenses accepting the producer WHEN IT ANNOUNCES ITSELF, so
    /// the replacement payload goes in when there is an agent there to receive
    /// it rather than on the retry clock. Same reasoning as the launcher
    /// handoff — see the comment at its recording site in `crate::spawn`.
    pub fn agent_spawned_as_reporting_agent(&self, agent_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .agents
            .get(agent_id)
            .and_then(|agent| agent.spawn_agent_type.as_ref())
            .is_some_and(crate::prompt_delivery::agent_reports_submitted_prompt)
    }

    /// Issue #424 F1: record that a guarded send in `mode` just put `payload`
    /// into `pane_id_env`. See [`Self::user_typed_since_automatic_write`] and
    /// [`Self::user_typed_since_writing_payload`].
    ///
    /// An empty SUBMIT payload — a probe — advances the clock without touching
    /// the recorded payloads. It wrote no bytes, so it left the box holding
    /// whatever the last payload write put there, and if that submitted
    /// cleanly the delivery is confirmed and there is no later attempt to
    /// guard. Keeping the record is the conservative half of the choice: it can
    /// only refuse a repeat, never let one through.
    ///
    /// Issue #424 H2: a [`SubmitMode::Notice`] records NOTHING. It advances no
    /// clock a submit decision reads, and its LF-terminated bytes are not a task
    /// a replacement could double — see [`AutomaticWrite::submitted_at`].
    fn note_automatic_write(&self, pane_id_env: &str, mode: SubmitMode, payload: &[u8]) {
        self.pane_input
            .lock()
            .unwrap()
            .note_automatic_write(pane_id_env, mode, payload);
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
    pub fn spawn_agent(
        self: &Arc<Self>,
        mut opts: SpawnOptions<'_>,
    ) -> Result<String, AgentPtyError> {
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

        // Point the child at THIS daemon's hook socket rather than letting it
        // re-resolve the endpoint from inherited environment at emit time.
        // A caller-supplied value wins (tests pin their own socket, and
        // `respawn_agent_for_pane` replays a `spawn_env` that already carries
        // ours), so this only fills the gap.
        if !opts.env.iter().any(|(k, _)| k == DOT_AGENT_DECK_SOCKET)
            && let Some(sock) = self.hook_socket.lock().unwrap().clone()
            && let Some(sock) = sock.to_str()
        {
            opts.env
                .push((DOT_AGENT_DECK_SOCKET.to_string(), sock.to_string()));
        }

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
        //
        // PRD #225 M2: the same value seeds BOTH registry fields, but they
        // diverge from here on — `agent_type` is the mutable display badge
        // (`set_agent_type` upgrades it from a hook event) and
        // `spawn_agent_type` is the frozen launch-shape decision the respawn
        // path replays. See `RunningAgent::spawn_agent_type`.
        let agent_type = opts.agent_type.clone();
        let spawn_agent_type = opts.agent_type.clone();

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
        //
        // Issue #454: the same acquisition RESERVES the spawn. The child is
        // launched below, before we can take this lock again to publish the
        // agent, so without the reservation there is a window in which the
        // daemon owns a running child that nothing can recognise — and a
        // wrapper whose first act is `dot-agent-deck agent-event --type running`
        // lands squarely in it. See [`RegistryInner::pending_spawns`].
        //
        // Issue #454 round-2 audit (blocker D): the reservation is EXCLUSIVE on
        // the pane id, and that is the half the first version was missing.
        // Ownership was conferred by a reservation but uniqueness was enforced
        // only by the post-fork duplicate check further down, so two concurrent
        // `StartAgent` calls for one pane both reserved it, both forked a child,
        // and BOTH were owners until the loser was rejected as
        // `DuplicatePaneId`. A loser that emitted before it was killed had its
        // event admitted against a pane whose real occupant is the winner —
        // and, once the winner is published, admitted again if it was processed
        // late. Refusing the second reservation under the same lock that grants
        // the first makes "at most one generation claims a pane" true at every
        // instant, which is what the retirement rule in
        // [`Self::owns_generation`] rests on.
        //
        // It also stops forking a child only to kill it: the rejection now
        // happens before `spawn`, not after. The post-fork check below stays —
        // it is the one that is atomic with the `agents.insert`, and this one is
        // not a substitute for it.
        let preallocated_id = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(ref candidate) = pane_id_env
                && (inner.cleanup_holds.contains(candidate.as_str())
                    || inner
                        .pending_spawns
                        .values()
                        .any(|reserved| reserved.as_deref() == Some(candidate.as_str()))
                    || inner.agents.values().any(|a| {
                        a.pane_id_env.as_deref() == Some(candidate.as_str())
                            && !a.exited.load(Ordering::SeqCst)
                    }))
            {
                // Issue #454 round 3: `cleanup_holds` is the third exclusion and
                // the one that is not about a live occupant — a `StopAgent` is
                // mid-way through taking this pane's state apart, and a
                // generation that claimed it now would have that state deleted
                // out from under it. See [`Self::hold_pane_for_cleanup`].
                return Err(AgentPtyError::DuplicatePaneId(candidate.clone()));
            }
            let id = inner.next_id.to_string();
            inner.next_id += 1;
            inner.pending_spawns.insert(id.clone(), pane_id_env.clone());
            id
        };
        let reservation = SpawnReservation {
            registry: self,
            id: Some(preallocated_id.clone()),
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
        // PRD #745 M11: the child exists as of the line above, so this is the
        // instant to record — before the lock acquisition below, which can
        // block behind any other registry operation. An OBSERVATION of when the
        // daemon forked this process, never derived from an event and never
        // recomputed; see [`RunningAgent::spawned_at`].
        let spawned_at = chrono::Utc::now();
        let mut inner = self.inner.lock().unwrap();
        // Issue #454: hand ownership over from the reservation to `agents`
        // WITHOUT releasing the lock in between — every early return below has
        // already given up on this spawn, and the success path inserts under
        // this very acquisition. Released here rather than via `Drop` because
        // `Drop` would try to take a lock this scope already holds.
        reservation.release_locked(&mut inner);

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
        //
        // Issue #454 round 2: the pre-fork reservation now applies this same
        // test, so in practice a duplicate is rejected before the child is
        // forked and this branch is unreachable for a concurrent spawn. It is
        // kept because it is the check that is ATOMIC with the insert below,
        // and because `spawn` above releases the lock in between — nothing else
        // publishes a live agent today, but this is the guarantee, not an
        // assumption about callers.
        if let Some(ref candidate) = pane_id_env
            && inner.agents.values().any(|a| {
                a.pane_id_env.as_deref() == Some(candidate.as_str())
                    && !a.exited.load(Ordering::SeqCst)
            })
        {
            return Err(AgentPtyError::DuplicatePaneId(candidate.clone()));
        }
        // Issue #424 H3: this agent is the pane's new occupant, so whatever the
        // previous one's guarded sends recorded about that input box describes a
        // box that no longer exists. Left behind it could only refuse this
        // agent's own first delivery — the same-pane-respawn half of the
        // prompt-loss finding. Done here, under the same lock as the duplicate
        // check, so the record cannot outlive the handover.
        if let Some(ref claimed) = pane_id_env {
            self.forget_pane_input(claimed);
            // Issue #454 round-3 audit (finding 4): the pane changes hands HERE,
            // under the same lock as the duplicate check and the insert below.
            // Every record still sitting on this pane is a retired generation
            // (a live one would have been refused by the check just above), and
            // each of them is disowned from this instant on — permanently, so
            // the successor exiting can never hand the pane back. See
            // [`RunningAgent::pane_handed_over`].
            for agent in inner.agents.values_mut() {
                if agent.pane_id_env.as_deref() == Some(claimed.as_str()) {
                    agent.pane_handed_over = true;
                }
            }
        }

        let pty = guard.take();
        let AgentPty {
            child,
            master,
            writer,
            reader,
            process_group,
        } = pty;

        let bus = Arc::new(AgentBus::new());
        let bus_for_thread = bus.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_thread = exited.clone();
        let notify_for_thread = self.change_notify.clone();
        // The reader thread needs a registry handle, its own
        // agent id, and the pane's id so its EOF branch can retire any armed
        // OutstandingDelegation/SilenceWatchRecord — but ONLY when this
        // agent's death is news to the registry (see `pump_reader`'s doc
        // comment on `is_agent_still_registered`). The registry handle is
        // WEAK, deliberately — see the same doc comment for why an owned
        // `Arc` here would create a reference cycle with
        // `AgentPtyRegistry`'s Drop-triggered `shutdown_all`. Clone the id
        // and pane id BEFORE either is moved (into `inner.agents.insert` /
        // `RunningAgent`) below.
        let registry_for_thread = Arc::downgrade(self);
        let agent_id_for_thread = preallocated_id.clone();
        let pane_id_env_for_thread = pane_id_env.clone();
        // Captured HERE, at spawn time, rather than inside
        // `pump_reader` itself — `Handle::try_current()` must run on a
        // thread that is currently inside a tokio runtime, and `spawn_agent`
        // (this method) is that thread; the detached reader thread below
        // never is. Deliberately `try_current()`, never `current()`: this
        // method is also called from plenty of synchronous `#[test]`
        // fixtures with no runtime in scope at all, and `current()` panics
        // in that case instead of returning `None` — see `pump_reader`'s doc
        // comment for what a `None` handle means at EOF time.
        let runtime_handle_for_thread = tokio::runtime::Handle::try_current().ok();
        // Detached thread: exits when the PTY returns EOF (child killed).
        // On exit, pump_reader sets `exited` and signals `change_notify` so
        // the idle monitor learns about the death immediately instead of
        // waiting for the next poll cycle.
        std::thread::spawn(move || {
            pump_reader(
                reader,
                bus_for_thread,
                exited_for_thread,
                notify_for_thread,
                registry_for_thread,
                agent_id_for_thread,
                pane_id_env_for_thread,
                runtime_handle_for_thread,
            )
        });

        let agent = RunningAgent {
            child,
            process_group,
            master,
            // Issue #424 H1: every byte anyone other than the daemon writes to
            // this PTY is a user keystroke, and the clock recording it has to
            // move under the same lock the write takes — see [`PaneWriter`].
            writer: Arc::new(AsyncMutex::new(PaneWriter::new(
                writer,
                pane_id_env.clone(),
                self.pane_input.clone(),
            ))),
            bus,
            pane_id_env,
            display_name,
            cwd: cwd_stored,
            tab_membership,
            agent_type,
            spawn_agent_type,
            spawn_env: captured_env,
            pty_rows: captured_rows,
            pty_cols: captured_cols,
            exited,
            // Issue #454: a fresh generation has not been handed over yet. It
            // is the one doing the taking-over, a few lines above.
            pane_handed_over: false,
            // PRD #201: a fresh agent starts with no pending seed; the seed
            // path (StartAgent `seed` at spawn / a delegate respawn) sets it
            // right after this spawn returns, before the agent's extension
            // pulls it on `session_start`.
            pending_seed: None,
            seed_delivered_native: false,
            // PRD #745 M11: the ONLY site that records a spawn instant, because
            // it is the only site that performs a spawn.
            spawned_at: Some(spawned_at),
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
    /// identity — the (expected) target agent id, the expected hook SESSION, the
    /// pane, and the exact text. A `delivery_id` is bound to its fingerprint at
    /// first admission; a later request that reuses the id with a DIFFERENT
    /// fingerprint is refused as a conflict rather than replaying the first
    /// (unrelated) result. Process-local (the ledger never crosses the wire), so
    /// `DefaultHasher` is sufficient.
    ///
    /// Issue #424, auditor LOW: `expected_session_id` used to be omitted, so an
    /// id reused with the same agent/pane/text but a DIFFERENT session replayed
    /// the cached `Applied` without ever running the new session guard —
    /// reporting a delivery into a generation nothing was written to, and
    /// directly undercutting the generation binding the rest of this fix rests
    /// on. Both sides of the comparison are computed daemon-side from the same
    /// request, so widening the hash input is not a wire change.
    pub fn delivery_fingerprint(
        expected_agent_id: Option<&str>,
        expected_session_id: Option<&str>,
        pane_id: &str,
        text: &str,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        expected_agent_id.hash(&mut h);
        expected_session_id.hash(&mut h);
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

    /// PRD #20 Greptile (paneless guarded send): resolve the live (non-exited)
    /// writer target for an AGENT id rather than for a pane. A daemon-side agent
    /// that carries no `pane_id_env` maps to the `<no-pane>` sentinel, so
    /// [`Self::writer_target_for_pane`] can never find it (it matches on
    /// `pane_id_env`, which is `None` for such an agent). The identity-guarded
    /// write-and-submit path resolves a paneless target by agent identity instead
    /// — mirroring the attach STREAM_IN input loop, which routes a paneless
    /// target's writer and writability check by agent id. Skips exited entries,
    /// like [`Self::writer_target_for_pane`].
    fn writer_target_for_agent(&self, agent_id: &str) -> Option<PaneWriterTarget> {
        let inner = self.inner.lock().unwrap();
        inner
            .agents
            .get(agent_id)
            .filter(|a| !a.exited.load(Ordering::SeqCst))
            .map(|a| PaneWriterTarget {
                writer: a.writer.clone(),
                agent_id: agent_id.to_string(),
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
    ///
    /// PRD #20 Greptile (paneless guarded send): a daemon-side agent that carries
    /// no pane maps to the `<no-pane>` sentinel and can never be resolved by pane
    /// (`writer_target_for_pane` matches on `pane_id_env`). For that sentinel the
    /// target is resolved by AGENT identity (`expected_agent_id`) instead, and the
    /// pane→agent rebind re-check is replaced by an agent-still-present re-check —
    /// mirroring the attach STREAM_IN input loop, which resolves a paneless
    /// target's writer/writability by agent id and skips the pane-owner re-check.
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
        self.write_and_submit_guarded_detailed(pane_id, text, expected_agent_id, revalidate)
            .await
            .map(GuardedSendDetail::outcome)
    }

    /// [`Self::write_and_submit_guarded`], keeping the refusal reason that
    /// method flattens into [`GuardedSend::Stale`].
    ///
    /// Issue #424 H5: the detached confirmation loop is the one caller that has
    /// a terminal REPORT to publish and can only publish it if it knows the
    /// write was refused because the user typed, rather than because the target
    /// went away. Every other caller wants the flat vocabulary and takes the
    /// method above. See [`GuardedSendDetail`].
    pub async fn write_and_submit_guarded_detailed<Fut>(
        &self,
        pane_id: &str,
        text: &str,
        expected_agent_id: Option<&str>,
        revalidate: impl FnOnce() -> Fut,
    ) -> Result<GuardedSendDetail, AgentPtyError>
    where
        Fut: std::future::Future<Output = bool>,
    {
        self.write_guarded(
            pane_id,
            text,
            SubmitMode::Submit,
            expected_agent_id,
            revalidate,
        )
        .await
    }

    /// PRD #249 M3: [`Self::write_to_pane_notice`] under
    /// [`Self::write_and_submit_guarded`]'s identity gate — the LF-terminated
    /// visibility path, but bound to an EXACT target identity.
    ///
    /// A pane id is just a string: an orchestrator that exited (or was closed)
    /// frees its `pane_id_env` for the next spawn, and an unguarded notice would
    /// then write one orchestration's diagnostics into a stranger's pane —
    /// `scheduler/idle-worker/008` and `/014` pin exactly that. The bytes are
    /// unsubmitted, so the stranger is not made to *act* on them, but they are
    /// still someone else's context and someone else's scrollback.
    ///
    /// Failure and refusal are reported through the same [`GuardedSend`] vocabulary
    /// so callers classify a refused notice the way they classify a refused prompt.
    ///
    /// Issue #702: what this path guarantees is DEFERRAL, not inertness — see
    /// [`crate::state::compose_worker_exited_notice`], which carries the whole
    /// contract for the two notices that still take this call. A caller that
    /// wants an untrusted value in its text belongs on
    /// [`Self::write_and_submit_guarded`] instead, where the text is a turn of
    /// its own rather than a prefix glued to the next one.
    pub async fn write_notice_guarded<Fut>(
        &self,
        pane_id: &str,
        text: &str,
        expected_agent_id: Option<&str>,
        revalidate: impl FnOnce() -> Fut,
    ) -> Result<GuardedSend, AgentPtyError>
    where
        Fut: std::future::Future<Output = bool>,
    {
        self.write_guarded(
            pane_id,
            text,
            SubmitMode::Notice,
            expected_agent_id,
            revalidate,
        )
        .await
        .map(GuardedSendDetail::outcome)
    }

    /// The shared body of [`Self::write_and_submit_guarded`] (payload +
    /// `SUBMIT_DELAY` + CR) and [`Self::write_notice_guarded`] (payload + LF).
    /// Only the delivery tail differs; every identity, liveness and rebind check
    /// — and the writer-held re-validation barrier that closes the TOCTOU — is
    /// common, so the two entrypoints cannot drift apart on the parts that make
    /// the send safe.
    async fn write_guarded<Fut>(
        &self,
        pane_id: &str,
        text: &str,
        mode: SubmitMode,
        expected_agent_id: Option<&str>,
        revalidate: impl FnOnce() -> Fut,
    ) -> Result<GuardedSendDetail, AgentPtyError>
    where
        Fut: std::future::Future<Output = bool>,
    {
        let is_paneless = pane_id == "<no-pane>";
        let target = if is_paneless {
            // Resolve BY agent identity; no identity → nothing to route to.
            match expected_agent_id.and_then(|id| self.writer_target_for_agent(id)) {
                Some(target) => target,
                None => return Ok(GuardedSendDetail::Outcome(GuardedSend::NoLiveTarget)),
            }
        } else {
            let Some(target) = self.writer_target_for_pane(pane_id) else {
                return Ok(GuardedSendDetail::Outcome(GuardedSend::NoLiveTarget));
            };
            target
        };
        // Pre-lock identity gate: refuse a prompt queued for a different agent
        // than the one that now owns the pane (respawn/rebind before delivery).
        // A paneless target was resolved BY `expected_agent_id`, so it can never
        // mismatch here — skip the gate.
        if !is_paneless
            && let Some(expected) = expected_agent_id
            && expected != target.agent_id
        {
            return Ok(GuardedSendDetail::Outcome(GuardedSend::WrongSession));
        }
        // Encode before locking so a bad payload doesn't pin the writer.
        let payload = encode_pane_payload(text)?;
        // Acquire the EXACT target writer, THEN re-validate — this is the
        // barrier the TOCTOU test holds open by locking the writer externally.
        let mut w = target.writer.lock().await;
        // Re-resolve identity: the pane may have rebound to a new agent, or the
        // target may have exited, while we waited for the writer. A paneless
        // agent has no pane→agent mapping to rebind, so the meaningful re-check
        // is that the agent still exists — a removal (`None`) is `Stale`.
        if is_paneless {
            if self.writer_target_for_agent(&target.agent_id).is_none() {
                return Ok(GuardedSendDetail::Outcome(GuardedSend::Stale));
            }
        } else {
            match self.writer_target_for_pane(pane_id) {
                Some(current) if current.agent_id == target.agent_id => {}
                Some(_) => return Ok(GuardedSendDetail::Outcome(GuardedSend::WrongSession)),
                None => return Ok(GuardedSendDetail::Outcome(GuardedSend::Stale)),
            }
        }
        if target.exited.load(Ordering::SeqCst) {
            return Ok(GuardedSendDetail::Outcome(GuardedSend::Stale));
        }
        // Liveness/session recheck against the authoritative session state.
        if !revalidate().await {
            return Ok(GuardedSendDetail::Outcome(GuardedSend::Stale));
        }
        // Issue #424 F1 (auditor HIGH): a SUBMIT-ONLY PROBE — an empty payload
        // whose only effect is the submit CR — must not fire once the user has
        // typed into this pane since our last submit. Every other guard on this
        // path identifies the PANE; this is the one that asks what the pane's
        // input editor is holding, which is the question a blind CR actually
        // depends on. See [`Self::user_typed_since_automatic_write`].
        //
        // Issue #424 H1: both predicates are read HERE, under the held writer,
        // and the user-input clock they read is stamped by that same writer
        // ([`PaneWriter`]). Before that, the attach path released the writer and
        // stamped afterwards, so a sender queued on the writer could acquire it
        // in between and read a clock older than bytes already in the PTY — the
        // guard passing on evidence that was stale by construction.
        //
        // Enforced HERE, at the single writer all three delivery
        // implementations funnel through, rather than in each of them: the TUI
        // paths reach the PTY through the daemon and have no way to consult this
        // clock themselves, and a per-path check is one refactor away from being
        // remembered in two places and forgotten in the third.
        //
        // Reported as `Stale` — the delivery's premise (the target still holds
        // our unsubmitted payload) no longer holds — because it is the existing
        // terminal-refusal vocabulary every caller already classifies, and no
        // bytes are written either way. `crate::spawn`'s confirmation loop asks
        // this question itself before probing so it can report the specific
        // reason on the pane's card instead of stopping silently.
        //
        // Issue #424 F1, replacement-payload half: a REPEAT of the payload we
        // already put in this pane is refused on the same evidence. Attempt 2 —
        // the one bounded replacement — exists because a launcher may CONSUME
        // attempt 1's bytes; once the user has typed since those bytes were
        // written, that premise is dead and writing them again appends our
        // prompt to their unsent draft and submits BOTH as one turn.
        //
        // Issue #424 H3: the predicate is keyed on the bytes AND scoped to the
        // lifetime of the delivery that wrote them, which is what keeps it from
        // refusing an ordinary later delivery of the same fixed text — a delegate
        // worker pointer is deliberately identical across hand-offs, so equal
        // payloads are the normal case, not an exotic one. See
        // [`Self::user_typed_since_writing_payload`] for the full lifecycle.
        if matches!(mode, SubmitMode::Submit) {
            let refuse = if payload.is_empty() {
                self.user_typed_since_automatic_write(pane_id)
            } else {
                self.user_typed_since_writing_encoded(pane_id, &payload)
            };
            if refuse {
                tracing::debug!(
                    pane_id = %pane_id,
                    agent_id = %target.agent_id,
                    payload_len = payload.len(),
                    "guarded submit refused: the user has typed into this pane \
                     since the last automatic write, so this would submit their \
                     unsent draft"
                );
                // Issue #424 H5: `Stale` to every existing caller and to the
                // wire, but the reason survives for the one caller that owes the
                // user a terminal report — see [`GuardedSendDetail`].
                return Ok(GuardedSendDetail::RefusedUserInput);
            }
        }
        // Authorized — write the payload and the mode's configured terminator,
        // holding the writer across the whole sequence (mirrors
        // `write_to_pane_internal`'s atomic submit contract).
        //
        // PRD #249 review (finding B1): the same `pane_write` byte trace the
        // unguarded [`Self::write_to_pane_internal`] emits, and for the same
        // reason — it is the surface an operator diagnosing a lost delegate is
        // told to turn on (`RUST_LOG=pane_write=trace`), and the delegate task
        // pointer now travels this path instead of that one. Emitted INSIDE the
        // writer critical section so trace order matches write order under
        // concurrent writers, with both keys so it joins against the STREAM_IN
        // trace on either.
        tracing::trace!(
            target: "pane_write",
            source = "daemon",
            guarded = true,
            mode = ?mode,
            pane_id = %pane_id,
            agent_id = %target.agent_id,
            payload_len = payload.len(),
            payload = %escape_bytes_for_log(&payload),
            "daemon write_guarded: payload bytes"
        );
        // PRD #20 R20-004 (finding #3): classify WHERE a writer error struck. A
        // failure before any byte reached the PTY is a clean, retryable transport
        // error; a partial write (payload started, or the tail — submit CR for
        // `Submit`, LF for `Notice` — failed after the payload landed) is
        // AMBIGUOUS and must not be blind-retried.
        let delivery = match mode {
            SubmitMode::Submit => deliver_payload_and_submit(w.daemon(), &payload).await,
            SubmitMode::Notice => deliver_payload_as_notice(w.daemon(), &payload).await,
        };
        match delivery {
            // Issue #424 F1: bytes of OURS are now in this pane, which is what
            // makes a later submit-only probe meaningful and a later repeat of
            // these same bytes recognizable. Recorded for the ambiguous
            // (partial) case too — something of ours landed there, and a partial
            // write is the case where a replacement is most tempting and most
            // dangerous.
            PayloadDelivery::Applied => {
                self.note_automatic_write(pane_id, mode, &payload);
                Ok(GuardedSendDetail::Outcome(GuardedSend::Applied))
            }
            PayloadDelivery::Ambiguous => {
                self.note_automatic_write(pane_id, mode, &payload);
                Ok(GuardedSendDetail::Outcome(GuardedSend::Ambiguous))
            }
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
        // Issue #424 H1: the daemon's own bytes, so they must not stamp the
        // pane's user-input clock. See [`PaneWriter::daemon`].
        w.daemon()
            .write_all(&payload)
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
                w.daemon()
                    .write_all(b"\r")
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
                w.daemon()
                    .write_all(b"\n")
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
            &agent.process_group,
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
    /// spawn. PRD #225 M2: the two agent-type fields are re-applied through
    /// DIFFERENT seams — [`RunningAgent::spawn_agent_type`] feeds
    /// [`SpawnOptions::agent_type`] (the launch side) while the observed
    /// [`RunningAgent::agent_type`] badge is restored afterwards via
    /// [`AgentPtyRegistry::set_agent_type`]. That split is what makes the invariant
    /// below hold at all: a type learned from a hook event updates the badge,
    /// never the command that gets exec'd.
    ///
    /// **Launch-shape invariant (PRD #225, review finding 1): the wrap decision
    /// for a respawn is derived from the command actually being launched; the
    /// pane's frozen spawn-time identity only fills in for a command that implies
    /// no agent type.** `command` is the CURRENT role command, so an edit to
    /// `.dot-agent-deck.toml` is honored — and the wrap decision follows that
    /// edit instead of contradicting it (no `wrap --agent codex -- claude`). A
    /// pane whose role command is unchanged therefore relaunches byte-identically,
    /// whether its identity was explicit at creation or inferred from the command.
    /// The reasoning, and the one residual limit, are at the `SpawnOptions`
    /// construction below.
    ///
    /// The TUI's pane card therefore stays put across the
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
    /// `AGENT_TERMINATE_GRACE` of `try_wait` polling, mirroring
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
    /// event (with a timeout fallback) — see
    /// [`crate::state::SESSION_START_WAIT_TIMEOUT`] for the duration and why the
    /// fallback is load-bearing.
    pub async fn respawn_agent_for_pane(
        self: &Arc<Self>,
        pane_id_env: &str,
        command: &str,
    ) -> Result<String, AgentPtyError> {
        self.respawn_agent_for_pane_declared(pane_id_env, command, None)
            .await
    }

    /// [`Self::respawn_agent_for_pane`], plus the caller's CURRENT declared
    /// identity for the pane (issue #308).
    ///
    /// `declared` is `Some` only when the caller has just re-read the pane's
    /// identity from the same source, in the same pass, as `command` — today
    /// that is the delegate path's `.dot-agent-deck.toml` re-read, whose role
    /// entry carries both the `command` and its `agent = "…"` key. Such a value
    /// is CURRENT by construction and therefore outranks both deriving from the
    /// command and the pane's frozen `spawn_agent_type`; see the contract on
    /// [`PaneRecreateIdentity::agent_type`] for why that is not the precedence
    /// PRD #225 finding 1 forbids.
    ///
    /// Threading it matters because the frozen fallback GOES STALE against this
    /// key specifically. A `devbox run codex-big` role declared as Codex lands
    /// correctly on a `clear = true` respawn either way — the command implies
    /// nothing, so the frozen `Some(Codex)` supplies it. But edit the config to
    /// `agent = "claude"` and the frozen value is the wrong answer, and it
    /// re-freezes itself on every respawn: the pane would relaunch as Codex for
    /// the rest of the session while the file says Claude. The delegate path
    /// already re-reads `command` on every delegate precisely so a config edit
    /// takes effect without recreating the pane; the declaration beside it has
    /// to follow the same rule or the two halves of one role entry disagree.
    ///
    /// `None` — every caller that has no declaration to offer, including the
    /// public [`Self::respawn_agent_for_pane`] — reproduces the pre-#308
    /// behavior exactly: derive from the command, fall back to the frozen
    /// identity.
    async fn respawn_agent_for_pane_declared(
        self: &Arc<Self>,
        pane_id_env: &str,
        command: &str,
        declared: Option<&AgentType>,
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
            process_group,
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
            // PRD #225 M2: the OBSERVED badge (possibly hook-learned). It is
            // restored onto the fresh entry AFTER the spawn, so it can't reach
            // `spawn`'s wrapper decision — only `spawn_agent_type` does.
            agent_type: observed_agent_type,
            spawn_agent_type,
            spawn_env,
            pty_rows,
            pty_cols,
            exited: _,
            // Issue #454: the OLD record is being removed outright, so its
            // handover flag has nothing left to disown. The fresh generation
            // starts `false` and takes the pane over in `spawn_agent`.
            pane_handed_over: _,
            // PRD #201: a respawn (`clear = true` delegate) drops any seed the
            // old child left unconsumed; the caller re-arms the fresh child's
            // seed via `set_pending_seed` right after this returns.
            pending_seed: _,
            seed_delivered_native: _,
            // PRD #745 M11: deliberately dropped rather than carried over. The
            // fresh child is a fresh spawn, and `spawn_agent` stamps it — which
            // is what makes a restarted worker report its CURRENT iteration
            // while an unrestarted role reports its whole lifetime.
            spawned_at: _,
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
        // The process group moves onto the blocking task too — it is what the
        // teardown's force phase reaps the descendant tree through (PRD #163 M3),
        // and it is dropped there once the old child is gone.
        let join = tokio::task::spawn_blocking(move || {
            crate::platform::proc::terminate_child_with_grace_and_wait(
                &mut child,
                AGENT_TERMINATE_GRACE,
                &process_group,
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
        //
        // PRD #225 M2: whatever identity is handed to the spawn seam, it is never
        // the (possibly hook-learned) display badge — that is restored after the
        // spawn, through the display-only `set_agent_type` seam.
        //
        // PRD #225 review finding 1 — the INVARIANT this seam enforces:
        //
        //   **A respawn's wrap decision is derived from the command it is
        //   actually launching.** The pane's frozen `spawn_agent_type` only
        //   supplies an identity that command CANNOT imply, and the
        //   hook-learned display badge never participates at all.
        //
        // The caller passes the CURRENT role command (`crate::state` re-reads
        // `.dot-agent-deck.toml` at delegate time), so the user may have edited
        // it since the pane was created. Honoring that edit is deliberate — but
        // then the wrap decision has to follow the command, or the two disagree:
        // a pane frozen as `Some(Codex)` whose command was edited to `claude`
        // would come back up as `dot-agent-deck wrap --agent codex -- claude`,
        // launching Claude wrapped as Codex. Deriving first eliminates that
        // case, and it makes `Some` and `None` behave the SAME way — before
        // this, a frozen `Some` overrode the edited command while a frozen
        // `None` silently re-derived from it inside `spawn`.
        //
        // Falling back to the frozen identity (rather than to nothing) is what
        // keeps the launch shape stable for the shape that motivated the split:
        // `devbox run codex-big` resolves to no agent type, so an explicit
        // creation-time identity is the only thing that knows the pane is Codex,
        // and dropping it would flip an initially-wrapped pane to bare on its
        // first delegate — Defect 2 in reverse. The residual limit is inherent
        // and documented: if the command implies nothing AND its underlying
        // agent changed (`devbox run codex-big` → `devbox run claude-big`), the
        // pane keeps its creation-time identity; that pane has to be recreated —
        // or, since issue #308, given an `agent = "…"` line, which the delegate
        // path re-reads on every delegate and passes as `declared` below, so an
        // edit to it takes effect on the next respawn.
        //
        // `AgentType::from_command` never yields the neutral `AgentType::None`
        // placeholder (it is absent from `agent_registry::ALL`), so a `Some`
        // here always means a real agent won the derivation.
        //
        // Issue #308: a CURRENT declaration from the caller precedes both. It is
        // not a third source competing with these two — it is the only source
        // that can answer for a launcher command at all, and unlike
        // `spawn_agent_type` it was read in the same pass as `command`, so it
        // cannot contradict it. See `respawn_agent_for_pane_declared`.
        let respawn_agent_type = declared
            .cloned()
            .or_else(|| AgentType::from_command(Some(command)))
            .or(spawn_agent_type);
        let opts = SpawnOptions {
            command: Some(command),
            cwd: cwd.as_deref(),
            display_name: display_name.as_deref(),
            rows: pty_rows,
            cols: pty_cols,
            env: spawn_env,
            tab_membership,
            agent_type: respawn_agent_type,
        };
        let new_agent_id = self.spawn_agent(opts)?;
        // Step 4 (PRD #225 M2): re-apply the observed badge so the dashboard
        // card keeps the agent label the previous child taught us (`list_agents`
        // → `AgentRecord.agent_type`) instead of reverting to "No agent" until
        // the fresh child's first hook lands. Upgrade-only, so a pane created
        // with an explicit identity keeps that identity, and a fresh child that
        // turns out to be a different agent still corrects the badge via its own
        // hooks. Deliberately AFTER the spawn: routing it through the same
        // display-only seam the hook path uses is what guarantees it cannot
        // influence the launch shape.
        if let Some(observed) = observed_agent_type {
            self.set_agent_type(pane_id_env, &observed);
        }
        Ok(new_agent_id)
    }

    /// [`Self::respawn_agent_for_pane`], but a pane with no record left is
    /// re-created rather than reported as a hard `NotFound` — issue #606.
    ///
    /// `respawn_agent_for_pane` lifts the pane's identity out of the record it
    /// is replacing, so it can only replace a record that exists. That is not
    /// the same thing as "the pane exists": [`Self::close_agent`] removes the
    /// entry BEFORE spending its termination grace, so for up to
    /// [`AGENT_TERMINATE_GRACE`] a pane that is being closed has no record at
    /// all. A `clear = true` delegate landing in that window got `NotFound`,
    /// surfaced "respawn failed" into the orchestrator pane, and left the role
    /// unreachable for the rest of the session.
    ///
    /// The ordering here is what makes the recovery safe rather than merely
    /// optimistic:
    ///
    /// 1. Try the ordinary respawn. The overwhelmingly common case has a record
    ///    and never reaches any of the rest of this.
    /// 2. On `NotFound`, WAIT for any in-flight close to release the pane
    ///    ([`Self::pane_close_in_flight`]). Creating an agent underneath a
    ///    running `StopAgent` is not merely racy — the close holds the pane
    ///    exactly so that its own `unregister_pane` cannot delete a newcomer's
    ///    state, and `spawn_agent` refuses a held pane outright.
    /// 3. Retry the respawn once. The pane may have acquired a record while we
    ///    waited (a concurrent spawn, or a close that failed and rolled back),
    ///    and replacing that record is more correct than spawning beside it.
    /// 4. Only then create a fresh agent from `identity`.
    ///
    /// Errors other than `NotFound` are returned untouched: a spawn that failed
    /// to exec, a shutting-down registry or a validation refusal are all real
    /// failures, and retrying them would just fail twice.
    pub async fn respawn_or_recreate_agent_for_pane(
        self: &Arc<Self>,
        pane_id_env: &str,
        command: &str,
        identity: &PaneRecreateIdentity,
    ) -> Result<PaneRespawn, AgentPtyError> {
        // Issue #308: the caller's identity is CURRENT (see the contract on
        // `PaneRecreateIdentity::agent_type`), so hand it to the respawn leg
        // too. Without this the ordinary `clear = true` respawn — which is the
        // overwhelmingly common outcome of this function, the re-creation below
        // being the issue-#606 recovery — would keep relaunching from the
        // pane's frozen `spawn_agent_type` and silently ignore an edited
        // `agent = "…"` line for the rest of the session.
        match self
            .respawn_agent_for_pane_declared(pane_id_env, command, identity.agent_type.as_ref())
            .await
        {
            Ok(agent_id) => {
                return Ok(PaneRespawn {
                    agent_id,
                    recreated: false,
                });
            }
            Err(AgentPtyError::NotFound(_)) => {}
            Err(other) => return Err(other),
        }

        // `tokio::time::Instant`, not `std::time::Instant`, so the clock the
        // deadline is measured against is the same one the sleep below obeys.
        // The delegate path that calls this is exercised under
        // `tokio::time::pause` (`orchestration/delegate/011`), where a std
        // `Instant` never advances while the paused sleep does — an unreachable
        // combination today, because a paused-clock test's respawn finds a
        // record and never gets here, but a spin-forever loop is not a trap to
        // leave lying in a recovery path.
        let waited_from = tokio::time::Instant::now();
        while self.pane_close_in_flight(pane_id_env)
            && waited_from.elapsed() < PANE_CLOSE_SETTLE_TIMEOUT
        {
            tokio::time::sleep(PANE_CLOSE_SETTLE_POLL).await;
        }
        if self.pane_close_in_flight(pane_id_env) {
            tracing::warn!(
                pane_id = %pane_id_env,
                waited_secs = PANE_CLOSE_SETTLE_TIMEOUT.as_secs(),
                "respawn: a close of this pane is still in flight after the settle timeout; \
                 attempting the fresh spawn anyway"
            );
        }

        match self
            .respawn_agent_for_pane_declared(pane_id_env, command, identity.agent_type.as_ref())
            .await
        {
            Ok(agent_id) => {
                return Ok(PaneRespawn {
                    agent_id,
                    recreated: false,
                });
            }
            Err(AgentPtyError::NotFound(_)) => {}
            Err(other) => return Err(other),
        }

        let mut env = identity.env.clone();
        if !env.iter().any(|(k, _)| k == DOT_AGENT_DECK_PANE_ID) {
            env.push((DOT_AGENT_DECK_PANE_ID.to_string(), pane_id_env.to_string()));
        }
        // The launch shape follows the caller's CURRENT identity for this pane,
        // falling back to deriving it from the command being launched — the same
        // order `respawn_agent_for_pane_declared` applies, so a re-created pane
        // and a respawned one exec identically. The respawn seam has a third,
        // last-resort source this one does not: the pane's FROZEN
        // `spawn_agent_type`, which exists only because a respawn HAS a
        // predecessor to have frozen it. Nothing was frozen here.
        //
        // Why the caller's identity may precede the command here, when the
        // frozen one may not there: the frozen value was captured at some
        // earlier spawn and can disagree with an edited command, which is PRD
        // #225 finding 1 (a pane frozen as Codex whose command was edited to
        // `claude` relaunching as `wrap --agent codex -- claude`). The caller's
        // identity was read in the same pass as the `command` beside it, so it
        // cannot disagree with it — see the contract on
        // [`PaneRecreateIdentity::agent_type`]. Deriving first would throw it
        // away, and it is the only thing that can know a `devbox run codex-big`
        // pane is Codex (issue #308), which is the case the identity exists for.
        //
        // What every ordering here guarantees is the same, and it is the
        // property that matters: the launch decision is never made from an
        // identity that could contradict the command it is launching.
        let agent_type = identity
            .agent_type
            .clone()
            .or_else(|| AgentType::from_command(Some(command)));
        let agent_id = self.spawn_agent(SpawnOptions {
            command: Some(command),
            cwd: identity.cwd.as_deref(),
            display_name: identity.display_name.as_deref(),
            // No prior record means no last-known geometry; the default the
            // daemon-side spawn primitive uses, corrected by the TUI's next
            // resize exactly as a freshly dispatched pane is.
            rows: 24,
            cols: 80,
            env,
            tab_membership: identity.tab_membership.clone(),
            agent_type,
        })?;
        tracing::info!(
            pane_id = %pane_id_env,
            agent_id = %agent_id,
            "respawn: the pane had no agent left to replace, so a fresh one was created for it"
        );
        Ok(PaneRespawn {
            agent_id,
            recreated: true,
        })
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
    /// reads `TIOCGWINSZ`). Non-zero values are clamped down to
    /// [`PTY_RESIZE_DIM_MAX`] — see the constant docs for the rationale.
    ///
    /// Issue #747: the clamp is no longer *silent*. It stays a clamp rather
    /// than a rejection — refusing would leave a wide terminal's pane stuck at
    /// its previous geometry, which is worse than a pane narrower than the
    /// screen — but it now emits one `warn!` per process. Our own TUI
    /// pre-clamps through [`clamp_pty_dims`], so after #747 this line firing
    /// means some *other* peer on the attach socket sent an over-cap request,
    /// which is precisely the case the cap exists for and worth seeing.
    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<(), AgentPtyError> {
        if rows == 0 || cols == 0 {
            return Err(AgentPtyError::Resize(format!(
                "rows and cols must be > 0 (got {rows}x{cols})"
            )));
        }
        let (requested_rows, requested_cols) = (rows, cols);
        let (rows, cols) = clamp_pty_dims(rows, cols);
        if (rows, cols) != (requested_rows, requested_cols)
            && !OVERSIZED_RESIZE_LOGGED.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                agent_id = %id,
                requested_rows,
                requested_cols,
                applied_rows = rows,
                applied_cols = cols,
                max = PTY_RESIZE_DIM_MAX,
                "resize request exceeded PTY_RESIZE_DIM_MAX and was clamped; the child PTY \
                 is narrower/shorter than the caller asked for (logged once per process)"
            );
        }
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

    /// Issue #686: the agent's scrollback snapshot together with the PTY dims it
    /// was written at, read under ONE lock acquisition.
    ///
    /// The pair has to be atomic. Raw PTY bytes are only interpretable as a
    /// screen when replayed at the geometry they were produced at, so a
    /// [`resize`] landing between a `snapshot` call and a
    /// [`pty_size_for_pane`] call yields bytes and dims that never coexisted —
    /// and re-wrapping a screen at the wrong width is exactly how a readable
    /// pane turns into nonsense. Taking both inside the same guard makes that
    /// unrepresentable.
    ///
    /// Keyed by AGENT id rather than pane id, unlike [`pty_size_for_pane`],
    /// because a pane outlives the agents that occupy it: a `clear = true`
    /// delegate respawns the worker, so a pane-keyed lookup can answer for a
    /// different generation than the snapshot came from.
    ///
    /// [`resize`]: Self::resize
    /// [`pty_size_for_pane`]: Self::pty_size_for_pane
    pub fn snapshot_with_pty_size(&self, id: &str) -> Result<(Vec<u8>, u16, u16), AgentPtyError> {
        let inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        Ok((agent.bus.snapshot(), agent.pty_rows, agent.pty_cols))
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

    /// Issue #454: the agent recorded under `id`, INCLUDING one whose child has
    /// already exited.
    ///
    /// [`Self::agent_records`] filters exited entries because it is the
    /// hydration source and a dead entry there materialises a ghost pane. The
    /// CLEANUP paths need the opposite: `StopAgent` reads the stopping agent's
    /// `pane_id_env` so it can take the pane's role-map entries and daemon-state
    /// registration back, and reading that through the filtered list meant a
    /// naturally-exited child produced `pane_id_env == None` and every cleanup
    /// step was skipped — permanently, since the registry entry is removed by
    /// the same handler. Cleanup is exactly the case where a dead entry is the
    /// thing you are looking for.
    pub fn agent_record_any(&self, id: &str) -> Option<AgentRecord> {
        let inner = self.inner.lock().unwrap();
        inner.agents.get(id).map(|agent| AgentRecord {
            id: id.to_string(),
            pane_id_env: agent.pane_id_env.clone(),
            display_name: agent.display_name.clone(),
            cwd: agent.cwd.clone(),
            tab_membership: agent.tab_membership.clone(),
            agent_type: agent.agent_type.clone(),
            rows: agent.pty_rows,
            cols: agent.pty_cols,
            live: None,
            // PRD #745 M11: absent unless THIS registry forked the child.
            spawned_at_ms: agent.spawned_at.map(|at| at.timestamp_millis()),
        })
    }

    /// Issue #454: does this registry own the GENERATION an event naming
    /// `(pane_id, agent_id)` comes from?
    ///
    /// This is the daemon's answer to "may this event drive my session state?"
    /// — [`crate::state::AgentOwnership`] states the rule in full and this is
    /// the only implementation of it. Four properties make the registry the
    /// right authority, and each is one the set-of-registered-ids it replaced
    /// could not hold:
    ///
    /// * it is true from BEFORE the child exists (the spawn reservation), so a
    ///   child that reports faster than its spawner returns is still owned;
    /// * it is keyed by GENERATION, so an event can be bound to the spawn it
    ///   actually came from — a set of pane ids has no way to express that, and
    ///   a pane id is explicitly reusable;
    /// * a retired generation keeps its pane until another generation claims it,
    ///   which is what lets a final `Idle`/`SessionEnd` written just before exit
    ///   still land after the PTY EOF was observed — and once a claim HAS
    ///   happened the disownership is permanent, so the successor exiting in
    ///   turn cannot hand the pane back (round-3 audit finding 4);
    /// * it cannot be grown by anything but a spawn, so repeated short-lived
    ///   panes leave nothing behind.
    ///
    /// Does not panic on a poisoned lock (auditor round-2 finding E). This sits
    /// on EVERY admission path now, and `ingest_event` has already broadcast to
    /// attached clients by the time `apply_event` runs — so a panic here would
    /// kill the per-connection task with the daemon's own state unchanged and
    /// the TUIs' updated, which is both a divergence and a repeatable local DoS.
    ///
    /// Round 3 (reviewer blocker 2): a poisoned registry answers
    /// [`Ownership::Unknown`], NOT "unclaimed". The distinction is the whole
    /// reason this returns three states — see [`crate::state::AgentOwnership`].
    /// Every caller that reads the answer as a grant treats `Unknown` exactly as
    /// it treats `Unclaimed` and denies; the one caller that reads the ABSENCE
    /// of a claim as a grant of its own must not, and could not tell the two
    /// apart while this returned a `bool`.
    pub fn generation_ownership(&self, pane_id: Option<&str>, agent_id: Option<&str>) -> Ownership {
        let Ok(inner) = self.inner.lock() else {
            tracing::error!("generation_ownership: registry lock is poisoned; cannot answer");
            return Ownership::Unknown;
        };
        match (pane_id, agent_id) {
            // The daemon's own shape: every agent it spawns is handed
            // `DOT_AGENT_DECK_AGENT_ID`, so its reports name both keys.
            (Some(pane), Some(agent)) => {
                // In flight: reserved for this pane, child not yet published.
                if inner
                    .pending_spawns
                    .get(agent)
                    .is_some_and(|reserved| reserved.as_deref() == Some(pane))
                {
                    return Ownership::Owned;
                }
                match inner.agents.get(agent) {
                    // Published, and this really is its pane.
                    Some(a) if a.pane_id_env.as_deref() == Some(pane) => {
                        // Round 3 (auditor finding 4): `pane_handed_over` is the
                        // MONOTONE half of the retirement rule and has to be
                        // read first. `pane_claimed_by_other` looks at who holds
                        // the pane NOW, which un-answers itself the moment the
                        // successor exits too — so a retired generation got its
                        // pane back once both records were dead. The flag is set
                        // as the pane changes hands and is never cleared, so the
                        // handover is permanent no matter what becomes of the
                        // successor. See [`RunningAgent::pane_handed_over`].
                        let disowned =
                            a.pane_handed_over || Self::pane_claimed_by_other(&inner, pane, agent);
                        if !a.exited.load(Ordering::SeqCst) || !disowned {
                            Ownership::Owned
                        } else {
                            Ownership::Unclaimed
                        }
                    }
                    // Either unknown, or a generation whose pane is a different
                    // one — an event that names a pane its own agent never had
                    // is not that agent's to write.
                    _ => Ownership::Unclaimed,
                }
            }
            // A producer that named no generation: a pre-F9 hook script, or any
            // wrapper that lost `DOT_AGENT_DECK_AGENT_ID` on the way (PRD #110 /
            // issue #398 keep this shape working deliberately). There is nothing
            // to bind to, so the pane is the whole answer — any generation
            // claiming it, live or retired, admits. Unchanged from round 1.
            (Some(pane), None) => {
                let claimed = inner
                    .pending_spawns
                    .values()
                    .any(|reserved| reserved.as_deref() == Some(pane))
                    || inner
                        .agents
                        .values()
                        .any(|a| a.pane_id_env.as_deref() == Some(pane));
                if claimed {
                    Ownership::Owned
                } else {
                    Ownership::Unclaimed
                }
            }
            // A daemon-side agent spawned without `DOT_AGENT_DECK_PANE_ID` is a
            // supported shape — its writability is resolved by agent identity
            // throughout the guarded-send and attach-input paths
            // (`AppState::agent_writable`), which only works if the session it
            // declares is admitted in the first place. Its events carry
            // `pane_id: None`, so pane-keyed ownership can never speak for them
            // and this is the arm that does.
            //
            // Deliberately restricted to agents that are genuinely paneless: an
            // event that dropped its `DOT_AGENT_DECK_PANE_ID` but kept its agent
            // id belongs to a pane-carrying agent, and admitting it would mint a
            // second, pane-less session card beside the pane's own.
            //
            // No liveness condition, unlike the paned arm: a registry id is
            // never reused (`next_id` only ever increments), so a retired
            // paneless generation has no successor that its late report could be
            // written against. The report can only reach its own session.
            (None, Some(agent)) => {
                let owned = inner
                    .pending_spawns
                    .get(agent)
                    .is_some_and(|reserved| reserved.is_none())
                    || inner
                        .agents
                        .get(agent)
                        .is_some_and(|a| a.pane_id_env.is_none());
                if owned {
                    Ownership::Owned
                } else {
                    Ownership::Unclaimed
                }
            }
            // Names neither key, so it names nothing this registry can own.
            // `AppState` falls back to its historical "this process manages no
            // panes at all, so it is watching EXTERNAL agents" rule.
            (None, None) => Ownership::Unclaimed,
        }
    }

    /// Issue #454 round-3 review (blocker 1): take the durable authorisation for
    /// `StopAgent`'s PANE-SCOPED cleanup of `pane_id` on behalf of `stopping_id`.
    ///
    /// Returns `None` — cleanup REFUSED — when any other generation claims the
    /// pane, or when the registry cannot be asked. Returns a
    /// [`PaneCleanupHold`] otherwise, and no new generation can reserve the pane
    /// until that hold is dropped.
    ///
    /// # Why the answer has to be durable rather than merely correct
    ///
    /// Everything `StopAgent` does with a pane id — `begin_pane_close`,
    /// `cancel_prompt_confirmation`, `unregister_pane`, `finish_pane_close` — is
    /// scoped to the PANE while the agent being stopped is not, so all of it
    /// belongs to whoever holds the pane at the moment it runs. The previous
    /// authorisation was a `pane_current_agent_id(P) == A || None` test taken
    /// once, before `close_agent`, which:
    ///
    /// * could not see a spawn that had RESERVED the pane but not published yet,
    ///   and so read "B is starting on P" as "nobody holds P" — the exact gap
    ///   `pending_spawns` exists to fill, consulted everywhere except here;
    /// * was taken before a `close_agent` that can spend the whole
    ///   `AGENT_TERMINATE_GRACE` window, and acted on afterwards — so B could
    ///   reserve, spawn, publish AND have `StartAgent` register its role, cwd,
    ///   orchestrator marker and routing identity inside the gap, all of which
    ///   the predecessor's `unregister_pane` then deleted.
    ///
    /// Revalidating at each step shrinks that window without closing it, and
    /// cannot close the last one at all: the claim is taken under the registry
    /// lock and `unregister_pane` runs under the `AppState` write lock. Holding
    /// the pane instead makes one check enough — the fact the check established
    /// is still true when the cleanup acts on it, because nothing may change it.
    ///
    /// # What it costs
    ///
    /// Almost nothing, because the pane is already unavailable for most of the
    /// same window. While the stopping agent's child is LIVE its record fails
    /// the reservation's exclusivity test on its own, and that covers the whole
    /// termination grace — the one interval this genuinely adds is the short tail
    /// between the child being dead and `close_agent` dropping its record. A
    /// spawn that lands in that tail is refused with `DuplicatePaneId`, the same
    /// error it would get one instant earlier.
    pub fn hold_pane_for_cleanup(
        self: &Arc<Self>,
        pane_id: &str,
        stopping_id: &str,
    ) -> Option<PaneCleanupHold> {
        let Ok(mut inner) = self.inner.lock() else {
            tracing::error!(
                pane_id = %pane_id,
                stopping = %stopping_id,
                "hold_pane_for_cleanup: registry lock is poisoned; refusing pane-scoped cleanup"
            );
            return None;
        };
        if Self::pane_claimed_by_other(&inner, pane_id, stopping_id) {
            tracing::debug!(
                pane_id = %pane_id,
                stopping = %stopping_id,
                "StopAgent: skipping pane-scoped cleanup; another generation already \
                 claims the pane"
            );
            return None;
        }
        // A second hold on one pane cannot happen for one agent and would be a
        // second `StopAgent` racing this one for another; refuse it the same way.
        if !inner.cleanup_holds.insert(pane_id.to_string()) {
            tracing::debug!(
                pane_id = %pane_id,
                stopping = %stopping_id,
                "StopAgent: skipping pane-scoped cleanup; another close already holds the pane"
            );
            return None;
        }
        Some(PaneCleanupHold {
            registry: Arc::clone(self),
            pane_id: pane_id.to_string(),
        })
    }

    /// Test-only: put the registry into the state a spawn is in between
    /// RESERVING `pane_id` and publishing its record.
    ///
    /// That window is what round-3 blocker 1 is about — it is invisible to
    /// `pane_current_agent_id`, so the old `StopAgent` gate read it as "nobody
    /// holds this pane" — and it is not reachable from outside this module,
    /// because `RegistryInner` is private and a real spawn passes through it too
    /// fast to schedule against. Tests in `crate::daemon_protocol` need it to
    /// drive the handler end to end; the registry's own tests reach
    /// `pending_spawns` directly.
    #[cfg(test)]
    pub(crate) fn reserve_pane_for_test(&self, agent_id: &str, pane_id: &str) {
        self.inner
            .lock()
            .expect("registry lock poisoned in a test seam")
            .pending_spawns
            .insert(agent_id.to_string(), Some(pane_id.to_string()));
    }

    /// Issue #454 (round-2 audit): does any generation OTHER than `excluded`
    /// currently claim `pane_id`?
    ///
    /// This is the boundary on a retired generation's grace period. "Claim"
    /// means a live published agent or an in-flight spawn reservation — the same
    /// two things [`Self::owns_generation`] treats as ownership — because both
    /// are generations that will write to that pane. Another RETIRED generation
    /// does not count: two corpses on one pane is a reaping question, and
    /// neither of them can be written over.
    fn pane_claimed_by_other(inner: &RegistryInner, pane_id: &str, excluded: &str) -> bool {
        inner.agents.iter().any(|(id, a)| {
            id != excluded
                && a.pane_id_env.as_deref() == Some(pane_id)
                && !a.exited.load(Ordering::SeqCst)
        }) || inner
            .pending_spawns
            .iter()
            .any(|(id, reserved)| id != excluded && reserved.as_deref() == Some(pane_id))
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
                // PRD #745 M11: absent unless THIS registry forked the child.
                // Every record this method yields is a LIVE one — the `exited`
                // filter above is what keeps a spawn instant from outliving the
                // process it describes and ticking up as a phantom uptime.
                spawned_at_ms: agent.spawned_at.map(|at| at.timestamp_millis()),
            })
            .collect();
        records.sort_by_key(|r| r.id.parse::<u64>().unwrap_or(0));
        records
    }

    /// PRD #370 M2 / PRD #386 M3: a snapshot of `(pane_id, shell_activity)` for
    /// every live agent that has both a known `pane_id_env` and a platform that
    /// can enumerate processes at all. Panes without a `pane_id_env` can't be
    /// correlated back to a session via `AppState::pane_hook_session_id`, and a
    /// pane the scan has no opinion about (Windows; see
    /// [`RunningAgent::shell_foreground_busy`]) is skipped rather than guessed
    /// at, so the daemon's poll loop only ever acts on a real signal. One lock
    /// acquisition covers every agent, matching [`Self::agent_records`]'s shape,
    /// so the poll loop doesn't take the registry lock once per pane per tick.
    ///
    /// `shapes` is the **catalog** of measured argv cross-check shapes (the
    /// daemon passes [`crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES`]), not
    /// a set applied uniformly: this is the one place that sees both a pane's
    /// agent kind and the catalog, so it is where PRD #386's Open Question 2 is
    /// resolved — each pane gets only the shapes measured against *its own*
    /// agent kind, and nothing at all when its kind has never been measured.
    /// See [`shell_tool_shape_key`] for why applying one agent's fingerprint to
    /// every pane would be a silent false negative rather than a harmless
    /// belt-and-braces check.
    ///
    /// The process table is sampled **once**, and never while the registry lock
    /// is held — one `ps -A` per tick reused for every pane (PRD #386 Route A),
    /// with no fork/exec under a lock the TUI-facing paths also take. Issue #493
    /// moved the sample to *after* the lock (see
    /// [`Self::shell_activity_candidates`]) so the ordering now also skips the
    /// sample entirely when there is nothing to classify; the "no subprocess
    /// while locked" property is preserved because the lock is released before
    /// sampling, not because the sample comes first.
    ///
    /// Returns `None` when the `ps` sample itself failed
    /// ([`crate::platform::proc::process_table`] returned `None`) — distinct
    /// from `Some(vec![])`, which means there are genuinely no live panes to
    /// report on. A caller that collapsed the two would treat a failed sample as
    /// "no panes", clearing whatever busy/idle state it tracks and re-emitting a
    /// spurious edge for every pane on the next good sample.
    ///
    /// This is the **synchronous** composition, kept as the unit-testable seam
    /// (`status/shell-activity/004`) and for callers outside an async context.
    /// The daemon's poll loop composes the same two primitives around an
    /// `async`, timeout-bounded sample instead — see `run_shell_activity_monitor`
    /// and issue #429 — because a synchronous `ps` on a Tokio worker stalls
    /// every other daemon task while it runs.
    pub fn shell_foreground_busy_snapshot(
        &self,
        shapes: &[crate::platform::proc::ShellToolShape],
    ) -> Option<Vec<(String, bool)>> {
        let candidates = self.shell_activity_candidates(shapes);
        // Issue #493: no live pane to classify, so there is nothing a process
        // table could tell us — return the empty reading WITHOUT sampling.
        // `Some(vec![])` (not `None`) is the honest answer: nothing failed.
        if candidates.is_empty() {
            return Some(Vec::new());
        }
        let table = crate::platform::proc::process_table()?;
        Some(Self::classify_shell_activity(&candidates, &table))
    }

    /// The live panes a shell-activity sample could say something about, each
    /// already paired with the argv shapes that apply to *its* agent kind
    /// (PRD #386 M3 / issue #493).
    ///
    /// This is the **lock half** of the snapshot, split out so the caller can
    /// answer "is there anything to classify?" *before* paying for a process
    /// table. It is the whole fix for issue #493: `process_table()` used to be
    /// the first statement of [`Self::shell_foreground_busy_snapshot`], so a
    /// daemon with zero panes still forked `ps -A` twice a second (plus a
    /// `getsid(2)` per row) to classify nobody — and the daemon's idle shutdown
    /// does not bound that, since it requires no clients *and* no agents, so a
    /// TUI attached with no panes open polled forever for no possible benefit.
    ///
    /// Every field is **owned**, so the registry lock is released the moment
    /// this returns and the sample (a fork/exec) runs with no lock held — the
    /// property the original sample-first ordering existed to guarantee, now
    /// guaranteed by the drop instead. `shell_pid` is read here rather than at
    /// classification time for the same reason: it is a plain field read on the
    /// child handle, so it is safe under the lock, and resolving it early means
    /// a pane whose pid is unavailable drops out before it can force a sample.
    pub fn shell_activity_candidates(
        &self,
        shapes: &[crate::platform::proc::ShellToolShape],
    ) -> Vec<ShellActivityCandidate> {
        let inner = self.inner.lock().unwrap();
        inner
            .agents
            .values()
            .filter(|agent| !agent.exited.load(Ordering::SeqCst))
            .filter_map(|agent| {
                let pane_id = agent.pane_id_env.clone()?;
                let shell_pid = agent.child.process_id()? as i32;
                let key = shell_tool_shape_key(agent.agent_type.as_ref());
                let shapes = shapes
                    .iter()
                    .copied()
                    .filter(|shape| Some(shape.agent) == key)
                    .collect();
                Some(ShellActivityCandidate {
                    pane_id,
                    shell_pid,
                    shapes,
                })
            })
            .collect()
    }

    /// The **classification half** of the snapshot: pure, lock-free work over a
    /// table the caller already sampled (PRD #386 Route A — one sample reused
    /// for every pane, and every pane in a tick classified against one
    /// consistent sample).
    ///
    /// A candidate the table has no opinion about is *dropped* rather than
    /// reported as idle — see
    /// [`crate::platform::proc::descendant_shell_activity`] for why `None` must
    /// never be folded into `Some(false)`.
    pub fn classify_shell_activity(
        candidates: &[ShellActivityCandidate],
        table: &[crate::platform::proc::ProcessInfo],
    ) -> Vec<(String, bool)> {
        candidates
            .iter()
            .filter_map(|candidate| {
                let busy = crate::platform::proc::descendant_shell_activity(
                    table,
                    candidate.shell_pid,
                    &candidate.shapes,
                )?;
                Some((candidate.pane_id.clone(), busy))
            })
            .collect()
    }

    /// PRD #370 M2 test-only seam: `inner` is private (by design — every
    /// other cross-module accessor returns an owned snapshot, never the
    /// live lock), but `daemon.rs`'s integration test needs to type into a
    /// spawned pane's PTY directly to prove the real monitor task reacts to
    /// it. `#[cfg(test)]` keeps this out of the production API surface
    /// entirely.
    #[cfg(test)]
    pub(crate) fn agent_writer(&self, id: &str) -> Option<Arc<AsyncMutex<PaneWriter>>> {
        self.inner
            .lock()
            .unwrap()
            .agents
            .get(id)
            .map(|a| a.writer.clone())
    }

    /// Issue #666 test-only seam: stamp `agent_id`'s SPAWN-TIME identity after
    /// the child is already running, exactly as [`Self::spawn_agent`] would have
    /// stamped it — both the display badge and the frozen
    /// [`RunningAgent::spawn_agent_type`] the rearm's standing is read from.
    ///
    /// It exists because those two things are not separable at the real spawn
    /// site and one of them has a side effect a test cannot want. Declaring
    /// [`SpawnOptions::agent_type`] as a Wrapper-strategy agent makes [`spawn`]
    /// launch `dot-agent-deck wrap --agent codex -- <command>` instead of
    /// `<command>` — a second real deck process between the PTY and the byte
    /// sink, which boots on its own schedule, emits its own hook events and
    /// chunks one payload write into pieces arriving over an unbounded window.
    /// `scheduler/dispatch/016` case G needs the pane's *believed type* to be
    /// Codex and nothing else; it observes raw bytes, so the wrapper is pure
    /// measurement noise there (issue #666 follow-up: it made case G flaky under
    /// load, and its hook events posted into whatever deck the ambient
    /// environment resolved).
    ///
    /// This writes the SAME field `spawn_agent` writes, so what the test under
    /// observation reads — [`Self::pre_write_believed_agent_type`],
    /// [`Self::agent_spawned_as_reporting_agent`] — is bit-for-bit what a real
    /// typed spawn would have left. The spawn-site plumbing itself stays covered
    /// by the cases that go through `SpawnOptions::agent_type` for real (A, E, F
    /// as ClaudeCode, H as OpenCode). `#[cfg(test)]` keeps it out of the
    /// production API surface, like [`Self::agent_writer`] above.
    #[cfg(test)]
    pub(crate) fn note_spawn_agent_type_for_test(&self, agent_id: &str, agent_type: AgentType) {
        let mut inner = self.inner.lock().unwrap();
        // Loud on a miss rather than a silent no-op: a fixture whose stamp did
        // not land has standing `None`, and for case G that is case B — it would
        // still refuse the rearm and still pass, having stopped testing what it
        // names.
        let agent = inner
            .agents
            .get_mut(agent_id)
            .unwrap_or_else(|| panic!("no such agent to stamp a spawn type onto: {agent_id}"));
        agent.agent_type = Some(agent_type.clone());
        agent.spawn_agent_type = Some(agent_type);
    }

    /// Issue #581 test-only seam: register a synthetic agent that owns `child`,
    /// so the shutdown phases can be driven against a child whose *reap*
    /// deliberately wedges — the stuck-NFS shape, which no real process can be
    /// coaxed into reproducing (a real child always returns from `wait` once
    /// SIGKILL lands).
    ///
    /// Everything other than `child` is inert filler: a freshly-opened PTY
    /// nobody reads from and an empty bus, because the teardown paths touch
    /// only `child` and `process_group`. `#[cfg(test)]` keeps it out of the
    /// production API surface, like [`Self::agent_writer`] above.
    #[cfg(test)]
    pub(crate) fn insert_test_agent(
        &self,
        child: Box<dyn portable_pty::Child + Send + Sync>,
    ) -> String {
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty for a synthetic test agent");
        let writer = pair
            .master
            .take_writer()
            .expect("take_writer for a synthetic test agent");
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = format!("test-agent-{}", inner.next_id);
        inner.agents.insert(
            id.clone(),
            RunningAgent {
                child,
                // `adopt(None)` is the portable "there is no group to hold"
                // constructor: a no-op ZST on Unix, and an unassigned (jobless)
                // handle on Windows, which is what makes both backends take
                // their documented `Child::kill` fallback here.
                process_group: crate::platform::proc::AgentProcessGroup::adopt(None),
                master: pair.master,
                writer: Arc::new(AsyncMutex::new(PaneWriter::new(
                    writer,
                    None,
                    self.pane_input.clone(),
                ))),
                bus: Arc::new(AgentBus::new()),
                pane_id_env: None,
                display_name: None,
                cwd: None,
                tab_membership: None,
                agent_type: None,
                spawn_agent_type: None,
                spawn_env: Vec::new(),
                pty_rows: 24,
                pty_cols: 80,
                exited: Arc::new(AtomicBool::new(false)),
                // Issue #454: `false` is the birth value — the flag latches to
                // `true` only when a *successor* takes this record's pane, and
                // this synthetic agent holds no pane at all (`pane_id_env:
                // None`), so nothing can ever hand one over.
                pane_handed_over: false,
                pending_seed: None,
                seed_delivered_native: false,
                // PRD #745 M11: this registry did not fork this child — the
                // caller did, and handed it over. There is no spawn of ours to
                // report, so the honest value is absence rather than the
                // `Utc::now()` that would make the synthetic agent look like it
                // had just been started by us.
                spawned_at: None,
            },
        );
        id
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
    ///
    /// PRD #225 M2: this writes the DISPLAY badge
    /// ([`RunningAgent::agent_type`]) and deliberately never touches
    /// [`RunningAgent::spawn_agent_type`], so a type learned from a hook event
    /// cannot change how the pane relaunches. Before that split, this
    /// display-only write leaked into the respawn's `SpawnOptions::agent_type`
    /// and silently rewrote a bare `devbox run codex-big` pane into a wrapped
    /// one on its first `clear = true` delegate (Defect 2).
    /// Whether any live agent in this registry was spawned with
    /// `DOT_AGENT_DECK_PANE_ID` == `pane_id_env`.
    ///
    /// A hook event naming a pane this daemon never spawned did not come from
    /// this deck. In practice that means another deck's agent — most often a
    /// test run whose child inherited an ambient `DOT_AGENT_DECK_SOCKET` —
    /// posting into this daemon, where it registers a card no local pane backs.
    /// Exited entries are excluded, matching every other operational lookup, so
    /// a genuine respawn racing its own first hook is not misreported.
    pub fn has_live_pane(&self, pane_id_env: &str) -> bool {
        if pane_id_env.is_empty() {
            return false;
        }
        let inner = self.inner.lock().unwrap();
        inner.agents.values().any(|a| {
            a.pane_id_env.as_deref() == Some(pane_id_env) && !a.exited.load(Ordering::SeqCst)
        })
    }

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

    /// SIGKILL every agent in `agents` — the whole descendant tree of each —
    /// and reap them all. Shared by [`Self::shutdown_all`] and phase 3 of
    /// [`Self::shutdown_all_graceful`].
    ///
    /// **The kill pass and the reap pass are separate, and that is the whole
    /// point** (issue #581). Both callers used to signal-then-wait inside one
    /// loop iteration, which makes *signal delivery* hostage to *reap latency*:
    /// a child wedged in uninterruptible kernel I/O does not die on SIGKILL
    /// until that I/O completes (the stuck-NFS case), so the loop parks in that
    /// agent's unbounded `wait()` and **every agent behind it in the vector is
    /// never signalled at all**. It failed silently — the starved agents log
    /// nothing, and every caller of these two methods is terminal, so nothing
    /// ran later to notice the still-running agent processes left behind by a
    /// shutdown that looked clean.
    ///
    /// So pass 1 signals everybody first, and only then does pass 2 reap. The
    /// reap is a shared non-blocking poll rather than a per-agent blocking
    /// `wait()`, so one wedged agent cannot hold its siblings' *reaps* hostage
    /// either — the same shape as `shutdown_all_graceful`'s own grace poll and
    /// as the wrapper's reap loop (see the "finding #12" comment in
    /// [`crate::wrap`]). The 50 ms cadence matches both, and costs at most one
    /// tick: a shutdown whose agents already exited during the grace window
    /// clears the whole vector on the first `try_wait` pass and never sleeps.
    ///
    /// **The reap is never dropped.** An agent leaves the vector only once its
    /// `try_wait` reported an exit status, or reported an error (meaning there
    /// is no status left to collect) — so this cannot trade the leaked-process
    /// bug for a leaked-zombie one. A genuinely wedged child therefore still
    /// holds this function until the kernel lets its `wait` complete, exactly as
    /// before; what changed is that it no longer takes its siblings with it.
    fn force_kill_and_reap_all(mut agents: Vec<RunningAgent>) {
        // Pass 1: signal only.
        for agent in &mut agents {
            crate::platform::proc::force_kill_child_group(&mut agent.child, &agent.process_group);
        }

        // Pass 2: reap, dropping each agent as its status is collected.
        //
        // Termination depends on `try_wait` staying `Some` once it has reported
        // an exit: phase 2 above may already have collected a child's status, and
        // this loop asks again. Both backends hold that — Unix `Child` is
        // `std::process::Child`, which caches the status and short-circuits, and
        // `WinChild::try_wait` re-reads `GetExitCodeProcess` on a handle it still
        // owns. A `Child` impl that answered `None` after reporting an exit would
        // pin its agent here forever.
        while !agents.is_empty() {
            agents.retain_mut(|agent| matches!(agent.child.try_wait(), Ok(None)));
            if agents.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGKILL every agent and drain the registry. Idempotent.
    pub fn shutdown_all(&self) {
        let agents: Vec<RunningAgent> = {
            let mut inner = self.inner.lock().unwrap();
            inner.agents.drain().map(|(_, a)| a).collect()
        };
        Self::force_kill_and_reap_all(agents);
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

        // Phase 3: SIGKILL any survivor and reap. The kill is no-op-safe on an
        // already-exited child (ESRCH is logged-but-ignored), so it runs
        // unconditionally. On Windows this is where the `TerminateJobObject`
        // backstop for each agent's descendant tree runs (PRD #163 M3) —
        // phase 1's `CTRL_BREAK_EVENT` is best-effort only.
        //
        // Issue #581: the kill pass and the reap pass are SEPARATE, and
        // [`Self::force_kill_and_reap_all`] documents why.
        Self::force_kill_and_reap_all(agents);

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

    /// Issue #424 S1: the submit drain and the key forwarder must agree, and
    /// this is the seam that makes them.
    ///
    /// PRD #227's acceptance matrix is a fact about REAL AGENTS that no unit
    /// test can re-measure. What it can do is make sure the drain never quietly
    /// stops honouring it: every row here is driven through the production
    /// encoder (`ui::keyevent_to_bytes`) rather than a hand-written byte
    /// literal, so re-encoding any of these keys — the exact change PRD #227
    /// itself made to `Enter` + SHIFT — fails here instead of silently turning a
    /// newline key back into a false drain.
    ///
    /// Both directions are pinned. Only claiming the newline keys do not submit
    /// would be satisfied by a drain that never fires at all, which would trade
    /// this bug for the fail-closed one (an ordinary later delivery of the same
    /// fixed pointer text refused as a repeat forever), so plain `Enter` and
    /// `Ctrl+M` are asserted to submit in the same breath.
    #[test]
    fn keyevent_submit_classification_matches_prd_227_matrix() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // (key, submits?, why) — the four rows of
        // `prds/done/227-modifier-aware-pane-key-forwarding.md:36-43`.
        let matrix = [
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                true,
                "plain Enter is `CR`: submit on pi and claude alike",
            ),
            (
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
                true,
                "Ctrl+M is the caret rule's own `CR` — the same byte, the same submit",
            ),
            (
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
                false,
                "Ctrl+J is `LF`: a newline for every supported agent",
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
                false,
                "Alt+Enter is `ESC CR`: a newline for claude, a submit for pi, so ambiguous",
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                false,
                "Shift+Enter is `ESC[13;2u`: the encoding verified as a newline on all four agents",
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
                false,
                "Ctrl+Enter takes the same CSI-u path",
            ),
        ];

        for (key, expected, why) in matrix {
            let frame = crate::ui::keyevent_to_bytes_for_test(&key)
                .unwrap_or_else(|| panic!("production encoding for {key:?}"));
            // A fresh stream per row, then the same row again split at every
            // byte boundary: a client is free to deliver `ESC` and `CR` in two
            // separate writes, and the classification must not depend on how
            // the frame was chopped up.
            for split in 0..=frame.len() {
                let mut stream = UserInputStream::default();
                let mut submitted = stream.feed(&frame[..split]);
                submitted |= stream.feed(&frame[split..]);
                assert_eq!(
                    submitted,
                    expected,
                    "{why}; frame={:?} split at {split}",
                    String::from_utf8_lossy(&frame)
                );
            }
        }
    }

    /// Issue #424 S1: the drain reads a keypress out of a stream, so the bytes
    /// around it are part of the question.
    ///
    /// The rows above are each a lone frame. These are the contexts that decide
    /// whether the one-byte lookback is enough: a `CR` typed at the end of real
    /// draft text still submits, the `ESC` that opened a paste marker must not
    /// leak onto a later `CR`, and a newline key inside bracketed paste is
    /// content twice over.
    #[test]
    fn submit_classification_holds_across_surrounding_stream_bytes() {
        // (bytes, submits?, why)
        let cases: [(&[u8], bool, &str); 7] = [
            (
                b"draft text\r",
                true,
                "the ordinary case: a user finishes a line and presses Enter",
            ),
            (
                b"draft text\x1b\r",
                false,
                "Alt+Enter after a draft is still Alt+Enter",
            ),
            (
                b"draft\x1b\rmore\r",
                true,
                "an Alt+Enter newline followed by more typing and a real Enter",
            ),
            (
                b"\x1b[200~pasted\rline\x1b[201~",
                false,
                "a CR inside bracketed paste is editor content",
            ),
            (
                b"\x1b[200~pasted\x1b[201~\r",
                true,
                "the Enter AFTER a paste closes submits, and the marker's own `~` is its predecessor",
            ),
            (
                b"\x1b[13;2u\r",
                true,
                "the CSI-u newline ends in `u`, so it cannot mask a following Enter",
            ),
            (
                b"\x1b\x1b\r",
                false,
                "an unproducible double ESC resolves the way every ambiguity must: no drain",
            ),
        ];

        for (bytes, expected, why) in cases {
            let mut stream = UserInputStream::default();
            assert_eq!(
                stream.feed(bytes),
                expected,
                "{why}; bytes={:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// Issue #493 at the synchronous seam: an empty registry must answer
    /// `Some(vec![])`, not `None`.
    ///
    /// Both halves matter. `Some` is the contract — `None` means "the sample
    /// failed", and a caller that saw it here would skip the tick and never
    /// clear its edge-detection map, so a reused pane id would inherit a stale
    /// busy/idle reading (the daemon's monitor depends on exactly this
    /// distinction). And the empty candidate list is what makes the answer
    /// reachable *without* sampling: `process_table()` used to be the first
    /// statement here, which is what made a paneless daemon fork `ps -A` at 2Hz
    /// forever. `shell_activity_candidates` is asserted directly because it is
    /// the guard the early return is keyed off.
    #[test]
    fn an_empty_registry_reports_no_shell_activity_without_sampling() {
        let registry = Arc::new(AgentPtyRegistry::new());
        assert!(
            registry
                .shell_activity_candidates(crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES)
                .is_empty(),
            "no agents means no candidate panes, which is the guard that skips the sample"
        );
        assert_eq!(
            registry
                .shell_foreground_busy_snapshot(crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES),
            Some(Vec::new()),
            "an empty registry is a successful reading of zero panes, NOT a failed sample"
        );
    }

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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
        };
        assert!(validate_tab_membership(tm).is_some());
    }

    // PRD #163 review: the orchestration-cwd absoluteness rule used to be a bare
    // `starts_with('/')`, which rejects every legitimate Windows working
    // directory. These pin the two pure classifiers on EVERY platform (they are
    // plain byte inspection, so Linux CI covers the Windows rule too) plus the
    // platform composition in `is_absolute_project_path`.

    #[test]
    fn posix_absolute_path_classification() {
        assert!(is_posix_absolute_path("/home/user/project-a"));
        assert!(is_posix_absolute_path("/"));
        assert!(!is_posix_absolute_path("relative/proj"));
        assert!(!is_posix_absolute_path(""));
        // A Windows path is NOT posix-absolute — that is the whole reason the
        // second classifier exists.
        assert!(!is_posix_absolute_path(r"C:\proj"));
    }

    #[test]
    fn windows_absolute_path_accepts_drive_letter_and_unc() {
        // Drive-letter rooted, both separators.
        assert!(is_windows_absolute_path(r"C:\Users\dev\project-a"));
        assert!(is_windows_absolute_path("C:/Users/dev/project-a"));
        assert!(is_windows_absolute_path(r"z:\p"));
        // UNC and the extended-length / device prefixes.
        assert!(is_windows_absolute_path(r"\\server\share\project-a"));
        assert!(is_windows_absolute_path("//server/share/project-a"));
        assert!(is_windows_absolute_path(r"\\?\C:\project-a"));
    }

    #[test]
    fn windows_absolute_path_rejects_relative_and_drive_relative() {
        assert!(!is_windows_absolute_path("relative/proj"));
        assert!(!is_windows_absolute_path(""));
        // Drive-RELATIVE: resolves against that drive's own cwd, so it is not a
        // stable project identity.
        assert!(!is_windows_absolute_path("C:proj"));
        assert!(!is_windows_absolute_path("C:"));
        // Rooted on the *current* drive — same objection.
        assert!(!is_windows_absolute_path(r"\proj"));
        // Not a drive letter.
        assert!(!is_windows_absolute_path("1:/proj"));
    }

    /// The platform composition: Unix stays byte-for-byte on the historical
    /// POSIX-only rule, Windows accepts both families (its own daemon reports
    /// `C:\…`, and a remote Unix daemon reports `/…`).
    #[test]
    fn orchestration_cwd_absoluteness_follows_the_platform() {
        assert!(is_valid_orchestration_cwd("/home/user/project-a"));
        assert!(!is_valid_orchestration_cwd("relative/proj"));

        let windows_paths = [r"C:\Users\dev\project-a", r"\\server\share\project-a"];
        for path in windows_paths {
            assert_eq!(
                is_valid_orchestration_cwd(path),
                cfg!(windows),
                "{path} must be accepted only where it is genuinely absolute"
            );
        }
        // Control bytes are still refused regardless of the path family.
        assert!(!is_valid_orchestration_cwd("C:\\proj\\\x1b[31m"));
        assert!(!is_valid_orchestration_cwd("C:\\proj\\\0evil"));
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
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
            orchestration_id: None,
        };
        assert!(validate_tab_membership(tm).is_some());
    }

    // -----------------------------------------------------------------
    // PRD #140 M1.0 / M1.1 — the per-tab orchestration instance token.
    // -----------------------------------------------------------------

    /// M1.0: the token survives a serialize → deserialize round-trip. It is
    /// the daemon's routing key, so losing it on the wire would silently
    /// merge two tabs back into one routing group.
    #[test]
    fn tab_membership_orchestration_id_survives_serde_round_trip() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 1,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some("/home/user/project-a".into()),
            display_title: Some("My Custom Run".into()),
            orchestration_id: Some("abc".into()),
        };
        let json = serde_json::to_string(&tm).expect("serialize");
        assert!(
            json.contains("\"orchestration_id\":\"abc\""),
            "token must be on the wire, got {json}"
        );
        let back: TabMembership = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, tm);
    }

    /// M1.0: `skip_serializing_if` keeps the field off the wire when absent,
    /// so a NEWER client talking to an OLDER daemon sends the pre-#140 frame
    /// shape byte for byte.
    #[test]
    fn tab_membership_omits_absent_orchestration_id_from_the_wire() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: true,
            orchestration_cwd: None,
            display_title: None,
            orchestration_id: None,
        };
        let json = serde_json::to_string(&tm).expect("serialize");
        assert!(
            !json.contains("orchestration_id"),
            "absent token must be skipped, got {json}"
        );
    }

    /// M1.0: the older wire shape (no `orchestration_id` key at all — what an
    /// OLDER client sends to a NEWER daemon) deserializes to `None`, which is
    /// the daemon's cue to fall back to the `(name, cwd)` identity.
    #[test]
    fn tab_membership_without_orchestration_id_deserializes_to_none() {
        let legacy = r#"{
            "kind": "orchestration",
            "name": "tdd-cycle",
            "role_index": 2,
            "role_name": "coder",
            "is_start_role": false,
            "orchestration_cwd": "/home/user/project-a"
        }"#;
        let tm: TabMembership = serde_json::from_str(legacy).expect("legacy shape parses");
        match tm {
            TabMembership::Orchestration {
                orchestration_id,
                orchestration_cwd,
                ..
            } => {
                assert_eq!(orchestration_id, None);
                assert_eq!(orchestration_cwd.as_deref(), Some("/home/user/project-a"));
            }
            _ => panic!("expected Orchestration variant"),
        }
    }

    /// M1.1: a control-byte token is rejected outright. Unlike `display_title`
    /// (nulled out, cosmetic), dropping a routing key silently would merge two
    /// same-`(name, cwd)` tabs into one group — the very bug #140 fixes.
    #[test]
    fn validate_tab_membership_rejects_orchestration_id_with_ansi_escape() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
            orchestration_id: Some("\x1b[31mpwn".into()),
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_rejects_orchestration_id_with_nul_byte() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
            orchestration_id: Some("orch\0-1".into()),
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_rejects_oversized_orchestration_id() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: None,
            display_title: None,
            orchestration_id: Some("a".repeat(DISPLAY_NAME_MAX_LEN + 1)),
        };
        assert!(validate_tab_membership(tm).is_none());
    }

    #[test]
    fn validate_tab_membership_accepts_well_formed_orchestration_id() {
        let tm = TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some("/home/user/project-a".into()),
            display_title: None,
            orchestration_id: Some(mint_orchestration_id()),
        };
        let validated = validate_tab_membership(tm).expect("membership preserved");
        match validated {
            TabMembership::Orchestration {
                orchestration_id, ..
            } => assert!(orchestration_id.is_some(), "token preserved verbatim"),
            _ => panic!("expected Orchestration variant"),
        }
    }

    /// M1.2/M1.3: two tabs created in the same process must never collide,
    /// and every minted token must survive the wire-boundary validation
    /// (otherwise the whole membership would be dropped at spawn).
    #[test]
    fn mint_orchestration_id_is_unique_and_wire_valid() {
        let ids: std::collections::HashSet<String> =
            (0..1000).map(|_| mint_orchestration_id()).collect();
        assert_eq!(ids.len(), 1000, "minted tokens must not collide");
        for id in &ids {
            assert!(is_valid_display_name(id), "token {id} must pass validation");
        }
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry.spawn_agent(SpawnOptions::default()).unwrap();
        for (rows, cols) in [(0u16, 80u16), (24u16, 0u16), (0u16, 0u16)] {
            let err = registry.resize(&id, rows, cols).unwrap_err();
            assert!(matches!(err, AgentPtyError::Resize(_)));
        }
        registry.shutdown_all();
    }

    #[test]
    fn registry_resize_unknown_errors() {
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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

    // ---- Issue #454: the registry as the daemon's OWNERSHIP AUTHORITY. ----
    //
    // `AppState::apply_event` admits an event only for a GENERATION this process
    // owns, and on the daemon that question is answered here. These tests pin
    // the four properties that made asking here the fix rather than maintaining
    // a second copy of the answer: ownership starts BEFORE the child exists, it
    // is keyed by generation and not by the reusable pane slot, a retired
    // generation keeps its pane exactly until another one claims it, and the
    // reservation that provides the first of those is exclusive and released on
    // every path.

    /// Reads the tri-state answer as the one thing most of these tests care
    /// about: "is this generation OWNED?". The `Unclaimed` / `Unknown` split is
    /// asserted on its own where it matters —
    /// `a_poisoned_registry_answers_unknown_rather_than_unclaimed` here, and the
    /// admission tests in `crate::state`.
    fn owns(registry: &AgentPtyRegistry, pane_id: Option<&str>, agent_id: Option<&str>) -> bool {
        registry.generation_ownership(pane_id, agent_id) == Ownership::Owned
    }

    /// The startup-race half. A spawn is owned from the moment it is RESERVED —
    /// before `spawn()` forks the child — so a wrapper whose very first act is
    /// `dot-agent-deck agent-event --type running` is already recognised when
    /// its report lands. Registering ownership after `spawn_agent` returned left
    /// that report to be dropped with nothing later to repair it, which is issue
    /// #454's symptom for any producer that never emits `SessionStart`.
    ///
    /// Asserted against the reservation directly because the window it covers is
    /// microseconds of lock-held work inside `spawn_agent` and cannot be paused
    /// from outside.
    #[test]
    fn a_reserved_spawn_is_owned_before_its_agent_is_published() {
        let registry = AgentPtyRegistry::new();
        {
            let mut inner = registry.inner.lock().unwrap();
            inner
                .pending_spawns
                .insert("77".to_string(), Some("in-flight-pane-454".to_string()));
            inner.pending_spawns.insert("78".to_string(), None);
        }
        assert!(
            owns(&registry, Some("in-flight-pane-454"), Some("77")),
            "a pane whose spawn is in flight must already be owned by the \
             generation that reserved it"
        );
        assert!(
            owns(&registry, Some("in-flight-pane-454"), None),
            "and by an untagged producer naming that pane — a hook that lost \
             DOT_AGENT_DECK_AGENT_ID has only the pane to go on"
        );
        assert!(
            owns(&registry, None, Some("78")),
            "a paneless agent whose spawn is in flight must already be owned"
        );
        assert!(
            !owns(&registry, Some("never-spawned-454"), None),
            "a pane nobody spawned is owned by nobody"
        );
        assert!(
            !owns(&registry, None, Some("77")),
            "an agent that carries a pane must not answer the PANELESS query — \
             admitting it would mint a second, pane-less card beside its own"
        );
        assert!(
            !owns(&registry, Some("in-flight-pane-454"), Some("78")),
            "and generation 78 does not own 77's pane just because 77's spawn is \
             in flight — the pair has to match"
        );
        assert!(
            !owns(&registry, None, None),
            "an event naming neither key names nothing this registry can own"
        );
    }

    /// A successful spawn hands ownership from the reservation to the published
    /// agent under one lock acquisition, leaving no reservation behind. If it
    /// leaked, the id would keep admitting events forever — the failure mode the
    /// hand-maintained set had, reproduced inside the fix.
    #[tokio::test]
    async fn a_successful_spawn_releases_its_reservation() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "reserved-ok-454".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("spawn /bin/sh");
        assert!(
            registry.inner.lock().unwrap().pending_spawns.is_empty(),
            "a published agent must not also hold a reservation"
        );
        assert!(owns(&registry, Some("reserved-ok-454"), Some(&id)));
        assert!(
            !owns(&registry, None, Some(&id)),
            "an agent with a pane id is not a paneless agent"
        );
        registry.shutdown_all();
    }

    /// Round-2 audit, blocker D — the reservation is EXCLUSIVE on its pane, and
    /// the rejection now happens BEFORE the loser forks a child.
    ///
    /// Round 1 conferred ownership with the reservation but enforced uniqueness
    /// only in the post-fork duplicate check, so two `StartAgent` calls for one
    /// pane both reserved it and both forked. The loser was an OWNER of that
    /// pane for the length of its own spawn, which is long enough for a fast
    /// child to emit — and its event was then admitted against a pane whose real
    /// occupant is the winner.
    ///
    /// `pending_spawns` holding the winner's reservation is the state that
    /// window consists of, so the test stands one there by hand and shows the
    /// second spawn cannot join it. Doing it that way is not a shortcut around a
    /// race: the reservation is taken and released under one lock hold inside
    /// `spawn_agent` and cannot be observed from outside mid-spawn.
    #[tokio::test]
    async fn a_second_reservation_for_one_pane_is_refused_before_it_forks() {
        let registry = Arc::new(AgentPtyRegistry::new());
        {
            let mut inner = registry.inner.lock().unwrap();
            inner.next_id = 900;
            inner
                .pending_spawns
                .insert("899".to_string(), Some("contested-454".to_string()));
        }

        let loser = registry.spawn_agent(SpawnOptions {
            command: Some("/bin/sh"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "contested-454".to_string(),
            )],
            ..SpawnOptions::default()
        });
        assert!(
            matches!(loser, Err(AgentPtyError::DuplicatePaneId(_))),
            "a pane already claimed by an in-flight spawn must refuse a second \
             one; got {loser:?}"
        );

        let inner = registry.inner.lock().unwrap();
        assert_eq!(
            inner.pending_spawns.len(),
            1,
            "only the first spawn may hold a reservation on the pane; \
             pending={:?}",
            inner.pending_spawns
        );
        assert!(
            inner.agents.is_empty(),
            "the refused spawn must not have forked a child at all"
        );
        drop(inner);
        assert!(
            !owns(&registry, Some("contested-454"), Some("900")),
            "the losing generation must never own the contested pane — its \
             report would otherwise be written against the winner's card"
        );

        registry.inner.lock().unwrap().pending_spawns.clear();
        registry.shutdown_all();
    }

    /// And a spawn that FAILS after taking its reservation releases it through
    /// `Drop`, which is the path no early `return` covers. A command that cannot
    /// be executed fails inside `spawn()` — after the reservation exists and
    /// before the lock that would release it explicitly is ever taken.
    #[tokio::test]
    async fn a_failed_spawn_releases_its_reservation_through_drop() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let failed = registry.spawn_agent(SpawnOptions {
            command: Some("/nonexistent/dot-agent-deck-454-no-such-binary"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "reserved-failed-454".to_string(),
            )],
            ..SpawnOptions::default()
        });
        assert!(
            failed.is_err(),
            "precondition: spawning a nonexistent command must fail; got {failed:?}"
        );
        assert!(
            registry.inner.lock().unwrap().pending_spawns.is_empty(),
            "a failed spawn must not leave a reservation admitting events for a \
             child that never started — and, now that the reservation is \
             exclusive, must not lock the pane out of ever being spawned again"
        );
        let retry = registry.spawn_agent(SpawnOptions {
            command: Some("/bin/sh"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "reserved-failed-454".to_string(),
            )],
            ..SpawnOptions::default()
        });
        assert!(
            retry.is_ok(),
            "the pane must be spawnable after the failed attempt released it; \
             got {retry:?}"
        );
        registry.shutdown_all();
    }

    /// Round-2 reviewer, blocker B — the half round 1 got backwards.
    ///
    /// The hook transport is fire-and-forget: the producer writes its final
    /// `Idle`/`SessionEnd`, flushes, and exits. `pump_reader` can observe the
    /// PTY EOF and set `exited` while those bytes are still queued on the
    /// socket. Round 1 read `exited` as instantaneous loss of ownership, so that
    /// report was dropped — and for an ORDINARY daemon pane there was no
    /// `managed_pane_ids` fallback left to catch it, because being owned is
    /// exactly what makes `apply_event` skip its `SessionStart` auto-register
    /// branch. A lost `SessionEnd` never removes its `SessionState`, so
    /// repeated short-lived agents accumulate it.
    ///
    /// So a retired generation keeps its own pane. What ENDS that is another
    /// generation claiming the pane, not the clock — pinned in the test below.
    #[tokio::test]
    async fn a_retired_generation_still_owns_its_own_pane() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/usr/bin/true"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "dead-pane-454".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("spawn /usr/bin/true");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.live_count() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the child never exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            owns(&registry, Some("dead-pane-454"), Some(&id)),
            "a generation that has exited still owns its OWN pane — its final \
             report can still be in flight, and dropping a SessionEnd leaks the \
             pane's session state forever"
        );
        assert!(
            !owns(&registry, Some("dead-pane-454"), Some("some-other-id")),
            "but only that generation: a different id naming the dead pane owns \
             nothing"
        );
        assert!(
            !owns(&registry, Some("some-other-pane-454"), Some(&id)),
            "and only that pane: the pair has to match in both directions"
        );
        assert_eq!(
            registry
                .agent_record_any(&id)
                .and_then(|r| r.pane_id_env)
                .as_deref(),
            Some("dead-pane-454"),
            "cleanup still has to be able to read a dead agent's pane id — \
             `agent_records` filters it out and `StopAgent` then skipped every \
             cleanup step, permanently"
        );

        // And reaping the record ends it. Nothing is in flight for a generation
        // whose entry is gone, and the daemon has explicitly finished with it.
        registry.close_agent(&id).expect("close the exited agent");
        assert!(
            !owns(&registry, Some("dead-pane-454"), Some(&id)),
            "a reaped generation owns nothing — otherwise a dead id keeps \
             admitting forged reports for a pane with no process behind it"
        );
        registry.shutdown_all();
    }

    /// The boundary on that grace, and the audit's blocker-D half: a retired
    /// generation owns its pane only until another generation CLAIMS it. The
    /// registry deliberately lets a live agent reuse a dead one's pane id, so
    /// without this an old generation's delayed event would be written against
    /// its successor's card.
    ///
    /// Pinning the SEQUENCE is the whole point: A exits (retired, still owner),
    /// then B spawns onto the same pane, and only then is A's ownership gone.
    #[tokio::test]
    async fn a_retired_generation_is_disowned_the_moment_its_pane_is_reused() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let opts = |command| SpawnOptions {
            command: Some(command),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "reused-pane-454".to_string(),
            )],
            ..SpawnOptions::default()
        };
        let old = registry
            .spawn_agent(opts("/usr/bin/true"))
            .expect("spawn the first generation");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.live_count() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first child never exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            owns(&registry, Some("reused-pane-454"), Some(&old)),
            "precondition: while nothing else claims the pane, the retired \
             generation still owns it"
        );

        let new = registry
            .spawn_agent(opts("/bin/sh"))
            .expect("the pane must be reusable once its child is gone");

        assert!(
            !owns(&registry, Some("reused-pane-454"), Some(&old)),
            "once a live generation holds the pane, the retired one owns \
             nothing there — its delayed report would land on the new agent's \
             card, or mint a rival session on a pane that already has one"
        );
        assert!(
            owns(&registry, Some("reused-pane-454"), Some(&new)),
            "and the live generation does own it"
        );
        assert!(
            owns(&registry, Some("reused-pane-454"), None),
            "an untagged producer still resolves by pane alone — it names no \
             generation, so there is nothing to bind and this is the PRD #110 / \
             issue #398 compatibility shape"
        );
        registry.shutdown_all();
    }

    /// Round-3 audit, finding 4: that boundary has to be MONOTONE. A retired
    /// generation must not get its pane BACK.
    ///
    /// The first implementation of "until another generation claims it" asked
    /// who holds the pane *now* — and that question un-answers itself. `A` exits
    /// on `P`, `B` claims `P`, `B` exits in turn, and with neither record reaped
    /// there is suddenly no live claimant, so `A` owned `P` again. That is the
    /// resurrection the generation-keyed rule exists to forbid, and everything
    /// downstream of admission trusts it: a re-admitted `A` report with a
    /// producer-supplied far-future timestamp becomes the pane's high-water
    /// session, which `pane_writable` then selects over a live successor.
    ///
    /// The SEQUENCE is the finding, and it is exactly the one clause the
    /// sibling test above omits — the successor has to EXIT before the
    /// assertion. Neither record is reaped, so nothing but the monotone flag can
    /// answer this.
    #[tokio::test]
    async fn a_retired_generation_stays_disowned_once_its_successor_also_exits() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let opts = || SpawnOptions {
            command: Some("/usr/bin/true"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "handback-pane-454".to_string(),
            )],
            ..SpawnOptions::default()
        };
        let wait_for_exit = |registry: Arc<AgentPtyRegistry>, which: &'static str| async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while registry.live_count() != 0 {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the {which} child never exited"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        let old = registry
            .spawn_agent(opts())
            .expect("spawn the first generation");
        wait_for_exit(Arc::clone(&registry), "first").await;

        let new = registry
            .spawn_agent(opts())
            .expect("the pane must be reusable once the first child is gone");
        assert!(
            !owns(&registry, Some("handback-pane-454"), Some(&old)),
            "precondition: the handover disowns the predecessor while the \
             successor is live"
        );
        wait_for_exit(Arc::clone(&registry), "second").await;

        // Neither record has been reaped — `close_agent` was never called for
        // either — so "who holds the pane now?" answers NOBODY, and that is
        // precisely the reading that handed the pane back.
        for (which, id) in [("predecessor", &old), ("successor", &new)] {
            assert_eq!(
                registry
                    .agent_record_any(id)
                    .and_then(|r| r.pane_id_env)
                    .as_deref(),
                Some("handback-pane-454"),
                "precondition: the {which}'s record must still be in the \
                 registry, or this test proves nothing about the retirement rule"
            );
        }
        assert!(
            !owns(&registry, Some("handback-pane-454"), Some(&old)),
            "a generation that has been handed over must stay disowned FOREVER \
             — its successor exiting is not a reason to give the pane back"
        );
        assert!(
            owns(&registry, Some("handback-pane-454"), Some(&new)),
            "the newest retired generation keeps its own grace period, exactly \
             as the sibling test above pins for a lone retiree — nothing has \
             claimed the pane after it"
        );
        registry.shutdown_all();
    }

    /// Round-3 review, blocker 1 (the ADMISSION half). `StopAgent` authorises
    /// its pane-scoped cleanup on "nobody else holds this pane", and the
    /// question it used to ask — `pane_current_agent_id` — cannot see a
    /// successor that has RESERVED the pane and not published yet. `None` came
    /// back and was read as "nobody holds it".
    ///
    /// The precondition assert is the finding stated as evidence: the old gate's
    /// own question answers `None` in exactly the state where the hold refuses.
    #[test]
    fn a_pane_a_successor_has_only_reserved_refuses_the_predecessors_cleanup_hold() {
        let registry = Arc::new(AgentPtyRegistry::new());
        {
            let mut inner = registry.inner.lock().unwrap();
            inner.pending_spawns.insert(
                "successor-454".to_string(),
                Some("stop-race-pane-454".to_string()),
            );
        }

        assert!(
            registry
                .pane_current_agent_id("stop-race-pane-454")
                .is_none(),
            "precondition: a reservation is INVISIBLE to the published-and-live \
             lookup the old gate asked, which is why it authorised"
        );
        assert!(
            registry
                .hold_pane_for_cleanup("stop-race-pane-454", "predecessor-454")
                .is_none(),
            "a pane a successor is mid-spawn onto is not the predecessor's to \
             give up — authorising here deletes the successor's role, cwd and \
             routing identity the moment it registers them"
        );
        assert!(
            registry
                .hold_pane_for_cleanup("some-other-pane-454", "predecessor-454")
                .is_some(),
            "…and an unclaimed pane still is: the refusal must be about THIS \
             pane, not about holds in general"
        );
    }

    /// Round-3 review, blocker 1 (the DURABILITY half). The authorisation was
    /// check-then-act — taken before a `close_agent` that can spend the whole
    /// three-second termination grace, and acted on afterwards in
    /// `unregister_pane` — so a successor could reserve, spawn, publish and
    /// register its whole identity inside the gap, only for the predecessor's
    /// cleanup to delete it.
    ///
    /// Revalidating at each step shrinks that window without closing it (the
    /// claim is taken under the registry lock and `unregister_pane` runs under
    /// the `AppState` write lock). So the fact the check established is made to
    /// STAY true instead: while the hold lives, nothing may claim the pane. The
    /// second half — that dropping it releases the pane — is what keeps this a
    /// bounded exclusion rather than a leak.
    #[tokio::test]
    async fn a_cleanup_hold_keeps_the_pane_out_of_a_successors_hands_until_it_is_dropped() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let opts = || SpawnOptions {
            command: Some("/bin/sh"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "held-pane-454".to_string(),
            )],
            ..SpawnOptions::default()
        };

        let hold = registry
            .hold_pane_for_cleanup("held-pane-454", "stopping-454")
            .expect("an unclaimed pane is the stopping agent's to give up");
        match registry.spawn_agent(opts()) {
            Err(AgentPtyError::DuplicatePaneId(pane)) => {
                assert_eq!(pane, "held-pane-454", "the refusal must name the held pane")
            }
            other => panic!(
                "a successor must not be able to claim a pane whose cleanup is \
                 still in flight; got {:?}",
                other.map(|id| format!("spawned as {id}"))
            ),
        }

        drop(hold);
        let id = registry.spawn_agent(opts()).expect(
            "dropping the hold must hand the pane back — the exclusion \
                     lasts for the cleanup, not for the daemon's life",
        );
        assert!(
            owns(&registry, Some("held-pane-454"), Some(&id)),
            "and the successor genuinely owns it afterwards"
        );
        registry.shutdown_all();
    }

    /// Round-2 audit, finding E: the ownership query is on EVERY admission path
    /// now, so a poisoned registry lock must DENY rather than panic.
    ///
    /// `ingest_event` has already broadcast to attached clients by the time
    /// `apply_event` runs, so a panic here kills the per-connection task with
    /// the TUIs updated and the daemon's own state not — the exact
    /// daemon/TUI divergence this issue exists to remove, plus a repeatable
    /// local DoS on every subsequent paned event.
    ///
    /// The registry is deliberately never dropped: `Drop` runs `shutdown_all`,
    /// which unwraps the same poisoned lock, and a panic inside a destructor
    /// aborts the process rather than failing the assertion. `ManuallyDrop`
    /// keeps that out of the way — including on the unwinding path a regression
    /// would take — and the registry holds no agents, so there is nothing to
    /// reap.
    #[test]
    fn a_poisoned_registry_answers_unknown_rather_than_unclaimed() {
        let registry = std::mem::ManuallyDrop::new(AgentPtyRegistry::new());
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = registry.inner.lock().unwrap();
            panic!("poison the registry lock");
        }));
        assert!(
            poisoned.is_err(),
            "precondition: the closure must have panicked"
        );
        assert!(
            registry.inner.lock().is_err(),
            "precondition: the lock must now be poisoned"
        );

        // Round 3 (reviewer blocker 2): the answer is `Unknown`, NOT
        // `Unclaimed`. Both deny, so this used to be indistinguishable from a
        // registry that had looked and found nothing — and `apply_event` read
        // that second answer as a licence to auto-register the pane. Asserting
        // the exact variant is the point of this test now; the sibling in
        // `state.rs` pins what the caller does with it.
        for (pane, agent) in [
            (Some("any-pane-454"), Some("1")),
            (Some("any-pane-454"), None),
            (None, Some("1")),
            (None, None),
        ] {
            assert_eq!(
                registry.generation_ownership(pane, agent),
                Ownership::Unknown,
                "a registry that cannot answer must say so, not panic and not \
                 report the question as unclaimed; asked ({pane:?}, {agent:?})"
            );
        }
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
        assert!(matches!(
            registry.close_agent("does-not-exist"),
            Err(AgentPtyError::NotFound(_))
        ));
    }

    #[test]
    fn registry_assigns_sequential_ids() {
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
            let registry = Arc::new(AgentPtyRegistry::new());
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

        // Same adoption the real `spawn()` does, so the guard's teardown reaps
        // the tree rather than just the direct child (PRD #163 M3; a no-op on
        // Unix, where `killpg` addresses the group by pid).
        let process_group = crate::platform::proc::AgentProcessGroup::adopt(Some(pid));
        let guard = ChildGuard::new(child, process_group);
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

    // PRD #370 M1: the pure detection primitive `foreground_pgid` is built
    // on. A real interactive shell is its own foreground process-group
    // leader while sitting at its prompt; once it forks a foreground job
    // (any command without `&`), job control makes that job the new
    // foreground process group until it finishes, then the shell reclaims
    // it. This is the OS-level fact the whole PRD hangs a "shell is busy"
    // signal on, independent of any agent-emitted hook/wrapper event.
    #[cfg(unix)]
    #[test]
    fn foreground_pgid_differs_while_a_foreground_child_runs() {
        use std::io::Write as _;

        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        // Deliberately `/bin/sh`, not `$SHELL`: whether a shell enables job
        // control (and thus forks a new foreground pgid per command) when
        // spawned this way is shell-dependent — e.g. interactive zsh here
        // did not exhibit it, while `/bin/sh` reliably does. `/bin/sh` keeps
        // this test deterministic across developer machines regardless of
        // login shell.
        let cmd = CommandBuilder::new("/bin/sh");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn should succeed");
        let shell_pid = child.process_id().expect("child should expose a pid") as i32;
        drop(pair.slave);

        // Poll for the idle baseline — right after spawn the shell may not
        // have claimed the foreground pgid yet.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut idle_pgid = crate::platform::proc::foreground_pgid(pair.master.as_ref());
        while idle_pgid != Some(shell_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            idle_pgid = crate::platform::proc::foreground_pgid(pair.master.as_ref());
        }
        assert_eq!(
            idle_pgid,
            Some(shell_pid),
            "an idle shell should be its own foreground process group"
        );

        // Start a foreground job that keeps running, so it becomes the new
        // foreground pgid until it exits.
        let mut writer = pair.master.take_writer().expect("take_writer");
        writer.write_all(b"sleep 5\n").expect("write sleep command");
        writer.flush().expect("flush");

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut busy_pgid = crate::platform::proc::foreground_pgid(pair.master.as_ref());
        while busy_pgid == Some(shell_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            busy_pgid = crate::platform::proc::foreground_pgid(pair.master.as_ref());
        }
        assert_ne!(
            busy_pgid,
            Some(shell_pid),
            "a running foreground child must not report the shell's own pgid"
        );

        // A single-pid `child.kill()` (SIGHUP) only reaches `/bin/sh` itself,
        // not the still-running `sleep 5` foreground job it forked into its
        // own process group — `sh` then blocks in its own `wait()` for that
        // child, so a plain `child.wait()` here would hang for the rest of
        // the 5 s sleep. `force_kill_child_and_wait` reaches the whole group
        // (`killpg(SIGKILL)`, same as production teardown), so `sleep 5`
        // dies too and this returns promptly. Drop the writer/master first
        // per that function's own doc, so any PTY I/O they're blocked on
        // unblocks before the kill.
        drop(writer);
        drop(pair.master);
        let group = crate::platform::proc::AgentProcessGroup::adopt(Some(shell_pid as u32));
        crate::platform::proc::force_kill_child_and_wait(&mut child, &group);
    }

    /// PRD #370 M1, **superseded by PRD #386 M3 — kept as a documented boundary
    /// case, with its assertion inverted rather than deleted.**
    ///
    /// This test typed `sleep 5` straight into the pane's PTY and asserted the
    /// pane read `Some(true)`, because #370's `tcgetpgrp` body answered "who
    /// owns the terminal's foreground". #386 replaced that body with a
    /// descendant scan that answers "is something detached into a POSIX session
    /// of its own still alive", and a job typed into the pane's own PTY is
    /// **deliberately not busy** under it: `sh` forks it into a new process
    /// *group*, but it stays in the pane's *session* on the pane's tty, exactly
    /// where every long-lived confounder a real agent pane carries also sits
    /// (`npm exec @upstash/context7-mcp`, `engram mcp`, `caffeinate -i -t 300`).
    /// Counting it would mean counting those too, which pins every pane at
    /// `Working` forever — the false positive the PRD calls worse than the stale
    /// `Idle` it replaces, because it is unfalsifiable to the user.
    ///
    /// So the behaviour change is intended, and the record of the boundary being
    /// considered is worth more than a deleted test. What the new mechanism
    /// *does* fire on — a real `setsid`-detached child on pipes, off the pane's
    /// PTY entirely, which is the topology a real Claude Bash-tool call has and
    /// the one #370 could never see — is covered by `status/shell-activity/004`
    /// in `tests/shell_activity.rs`.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_foreground_busy_ignores_a_non_detached_foreground_child() {
        use std::io::Write as _;

        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        let busy = |registry: &AgentPtyRegistry| -> Option<bool> {
            registry
                .inner
                .lock()
                .unwrap()
                .agents
                .get(&id)
                .and_then(|a| a.shell_foreground_busy(&[]))
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = busy(&registry);
        while state != Some(false) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            state = busy(&registry);
        }
        assert_eq!(state, Some(false), "an idle shell should not read busy");

        let writer = {
            let inner = registry.inner.lock().unwrap();
            inner.agents.get(&id).unwrap().writer.clone()
        };
        {
            let mut w = writer.lock().await;
            w.write_all(b"sleep 5\n").expect("write sleep command");
            w.flush().expect("flush");
        }

        // Sampled repeatedly rather than once, so this fails if the signal ever
        // rises even briefly — the old `Some(true)` assertion is inverted here,
        // not just dropped.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            assert_eq!(
                busy(&registry),
                Some(false),
                "a foreground job typed into the pane's own PTY stays in the pane's POSIX \
                 session, so PRD #386's descendant scan must NOT read it as busy — this \
                 supersedes #370's tcgetpgrp behaviour, which reported `true` here and \
                 `false` for the detached Bash-tool child that actually matters"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        registry.close_agent(&id).unwrap();
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
    fn spawn_scrubs_hook_socket_env_from_child() {
        // The endpoint counterpart of the PANE_ID scrub, and the one with
        // teeth: an inherited PANE_ID misroutes inside one deck, an inherited
        // DOT_AGENT_DECK_SOCKET points the child at a DIFFERENT deck's daemon.
        //
        // Regression guard for the 2026-07-29 production leak: `cargo test-e2e`
        // running inside a deck pane inherited that pane's socket, and a test
        // spawning a real agent without pinning its own handed the agent the
        // developer's live daemon — which ingested the test agent's
        // `SessionStart` and drew a card for it on the real dashboard.
        let _g = ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        // SAFETY: serialized by ENV_TEST_LOCK; prior value is restored
        // before the lock is released.
        let prior = std::env::var(DOT_AGENT_DECK_SOCKET).ok();
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_SOCKET, "/run/user/1000/someone-elses.sock");
        }

        // Spawn without pinning the socket via opts.env — the child must not
        // observe the inherited value. Exit 0 if absent, 1 if inherited.
        let pty = spawn(SpawnOptions {
            command: Some("sh -c 'exit ${DOT_AGENT_DECK_SOCKET:+1}'"),
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
        let mut child = pty.child;
        let status = child.wait().expect("wait should succeed");

        unsafe {
            match prior {
                Some(v) => std::env::set_var(DOT_AGENT_DECK_SOCKET, v),
                None => std::env::remove_var(DOT_AGENT_DECK_SOCKET),
            }
        }

        assert_eq!(
            status.exit_code(),
            0,
            "child saw inherited DOT_AGENT_DECK_SOCKET — agent_pty::spawn must scrub it, \
             or a test agent will post its hook events into the developer's live daemon"
        );
    }

    #[test]
    fn spawn_opts_env_overrides_hook_socket_scrub() {
        // The scrub must not clobber a deliberately-supplied socket: the
        // `spawn_agent` injector and every socket-pinning caller depend on
        // opts.env winning under the scrub-then-overlay order. Without this,
        // fixing the leak above would break every legitimate producer.
        let _g = ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        // SAFETY: serialized by ENV_TEST_LOCK; prior value is restored
        // before the lock is released.
        let prior = std::env::var(DOT_AGENT_DECK_SOCKET).ok();
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_SOCKET, "/run/user/1000/someone-elses.sock");
        }

        // Exit 0 only when the child sees exactly the pinned value.
        let pty = spawn(SpawnOptions {
            command: Some(
                "sh -c '[ \"$DOT_AGENT_DECK_SOCKET\" = /tmp/pinned.sock ] && exit 0 || exit 1'",
            ),
            env: vec![(DOT_AGENT_DECK_SOCKET.into(), "/tmp/pinned.sock".into())],
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
        let mut child = pty.child;
        let status = child.wait().expect("wait should succeed");

        unsafe {
            match prior {
                Some(v) => std::env::set_var(DOT_AGENT_DECK_SOCKET, v),
                None => std::env::remove_var(DOT_AGENT_DECK_SOCKET),
            }
        }

        assert_eq!(
            status.exit_code(),
            0,
            "opts.env DOT_AGENT_DECK_SOCKET was clobbered — the scrub must run \
             BEFORE opts.env is applied, or the daemon can't hand agents its own socket"
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
    // Hook-socket injection + pane provenance. Together these stop a child
    // from re-resolving the hook endpoint out of inherited environment: a
    // test-spawned agent whose events resolved the developer's real socket
    // used to surface as a phantom card in whatever deck the test ran inside.
    // ---------------------------------------------------------------------

    /// Read back what the child actually saw for `DOT_AGENT_DECK_SOCKET` by
    /// having it write the value to a file, so the assertion covers the real
    /// child environment rather than the registry's bookkeeping.
    fn child_observed_socket(
        registry: &Arc<AgentPtyRegistry>,
        pane_id: &str,
        extra_env: Vec<(String, String)>,
    ) -> String {
        let dir = tempfile::tempdir().expect("create tempdir");
        let out = dir.path().join("socket.txt");
        let mut env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())];
        env.extend(extra_env);
        registry
            .spawn_agent(SpawnOptions {
                command: Some(&format!(
                    "sh -c 'printf \"%s\" \"${{DOT_AGENT_DECK_SOCKET:-<unset>}}\" > {}'",
                    out.display()
                )),
                env,
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");
        // The child writes and exits promptly; poll rather than sleep a fixed
        // span so a fast machine doesn't wait and a loaded one doesn't flake.
        for _ in 0..200 {
            if let Ok(v) = std::fs::read_to_string(&out)
                && !v.is_empty()
            {
                return v;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("child never reported its DOT_AGENT_DECK_SOCKET");
    }

    #[test]
    fn spawn_agent_injects_the_daemons_hook_socket_into_the_child() {
        let registry = Arc::new(AgentPtyRegistry::new());
        registry.set_hook_socket(PathBuf::from("/tmp/dad-test-daemon.sock"));
        let observed = child_observed_socket(&registry, "pane-inject", vec![]);
        registry.shutdown_all();
        assert_eq!(
            observed, "/tmp/dad-test-daemon.sock",
            "the child must be handed the daemon's own hook socket, not left to \
             re-resolve one from inherited environment at emit time"
        );
    }

    #[test]
    fn spawn_agent_lets_a_caller_supplied_hook_socket_win() {
        let registry = Arc::new(AgentPtyRegistry::new());
        registry.set_hook_socket(PathBuf::from("/tmp/dad-test-daemon.sock"));
        let observed = child_observed_socket(
            &registry,
            "pane-explicit",
            vec![(
                DOT_AGENT_DECK_SOCKET.to_string(),
                "/tmp/dad-test-caller.sock".to_string(),
            )],
        );
        registry.shutdown_all();
        assert_eq!(
            observed, "/tmp/dad-test-caller.sock",
            "injection must only fill a gap — an explicit socket (tests pinning \
             their own, or a respawn replaying spawn_env) has to win"
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
            spawned_at_ms: None,
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
        let registry = Arc::new(AgentPtyRegistry::new());
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
            spawned_at_ms: None,
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
        let reg = Arc::new(AgentPtyRegistry::new());
        let fp = AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), None, "pane", "text");

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
        let reg = Arc::new(AgentPtyRegistry::new());
        let fp_a =
            AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), None, "pane", "payload-a");
        let fp_b =
            AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), None, "pane", "payload-b");
        assert_ne!(fp_a, fp_b, "distinct payloads must fingerprint differently");
        // Issue #424, auditor LOW: the expected SESSION is part of the identity
        // too. Omitting it let an id reused across a `/clear` replay the cached
        // `Applied` without running the new session guard.
        assert_ne!(
            AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), Some("gen-1"), "pane", "same"),
            AgentPtyRegistry::delivery_fingerprint(Some("agent-1"), Some("gen-2"), "pane", "same"),
            "distinct expected sessions must fingerprint differently"
        );

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

    /// Pin the PRIMITIVE's documented-permissive `None`
    /// behavior as an asserted fact, not merely a reader's inference from the
    /// source. `write_guarded`'s pre-lock identity gate (`if !is_paneless &&
    /// let Some(expected) = expected_agent_id && ...`) only compares
    /// identities when `expected_agent_id` is `Some` — passing `None` skips
    /// the gate entirely, and the call proceeds as an UNGUARDED write to
    /// whoever currently owns the pane. That is correct at THIS layer: the
    /// primitive is generic, and it is every caller's job never to pass
    /// `None` when it needs verified delivery — `dispatch_one_owned`'s own
    /// refusal (`dispatch_one_owned_refuses_write_when_worker_identity_is_unresolved`
    /// in `state.rs`) is exactly that caller-side responsibility. Without
    /// this test, a future reader would have to re-derive the permissive
    /// semantics from the gate's `if let` shape rather than finding them
    /// asserted.
    #[tokio::test]
    async fn guarded_send_with_no_expected_identity_writes_to_the_live_pane() {
        let reg = Arc::new(AgentPtyRegistry::new());
        // `spawn_agent` returns the REGISTRY's own agent id — a UUID, not the
        // `pane_id_env` string — and `AgentPtyRegistry::snapshot` reads
        // scrollback by that agent id, so it has to be captured here rather
        // than discarded.
        let agent_id = reg
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "pane-no-expected-identity".to_string(),
                )],
                ..SpawnOptions::default()
            })
            .expect("spawn agent");

        let outcome = reg
            .write_and_submit_guarded_detailed(
                "pane-no-expected-identity",
                "echo none-identity-permissive-marker",
                None,
                || async { true },
            )
            .await
            .expect("guarded send result");
        assert_eq!(
            outcome,
            GuardedSendDetail::Outcome(GuardedSend::Applied),
            "a None expected identity must not be refused by the primitive — it is the caller's \
             job to withhold None when it wants verification"
        );

        // Confirm bytes actually reached the live pane, not merely that the
        // outcome claims `Applied`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut found = false;
        while tokio::time::Instant::now() < deadline {
            let snap = reg.snapshot(&agent_id).unwrap_or_default();
            if snap
                .windows(b"none-identity-permissive-marker".len())
                .any(|w| w == b"none-identity-permissive-marker")
            {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            found,
            "the guarded primitive must have written the payload into the live pane when \
             expected_agent_id was None"
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

    /// PRD #126 M1 audit (finding 2): closing the ORCHESTRATOR must cancel the
    /// workers' watches. Records are keyed by worker pane, so the old
    /// pane-keyed cancellation left them armed against a pane id that a later,
    /// unrelated agent could inherit.
    #[test]
    fn begin_pane_close_cancels_records_targeting_the_closing_orchestrator() {
        let reg = Arc::new(AgentPtyRegistry::new());
        // PRD #140: the record carries the daemon's routing identity, so the
        // fixture uses the same `Instance` token shape a current client stamps.
        let orch = crate::state::OrchestrationIdentity::Instance {
            id: "instance-1".to_string(),
            name: "orch".to_string(),
        };
        let other_orch = crate::state::OrchestrationIdentity::Instance {
            id: "instance-2".to_string(),
            name: "other".to_string(),
        };
        let a = reg
            .arm_outstanding_delegation("worker-a", "coder", "orch-1", "agent-7", Some(&orch))
            .expect("arm worker-a");
        let b = reg
            .arm_outstanding_delegation("worker-b", "tester", "orch-1", "agent-7", Some(&orch))
            .expect("arm worker-b");
        let other = reg
            .arm_outstanding_delegation("worker-c", "coder", "orch-2", "agent-9", Some(&other_orch))
            .expect("arm worker-c");

        let (mut a_cancel, mut b_cancel) = (a.cancel, b.cancel);
        let dropped = reg.begin_pane_close("orch-1");
        assert_eq!(dropped.len(), 2, "both of orch-1's workers must be dropped");
        // The returned records are for the caller's logging; releasing them is
        // what resolves the watch channels, so both tasks exit immediately
        // instead of sleeping out the timeout (finding 3). Every call site drops
        // them within the same statement/block.
        drop(dropped);
        for cancel in [&mut a_cancel, &mut b_cancel] {
            assert!(
                matches!(cancel.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
                "a dropped record must resolve its watch's cancellation channel"
            );
        }
        assert!(
            reg.take_outstanding_delegation_if("worker-c", other.seq)
                .is_some(),
            "an unrelated orchestration's record must survive the close"
        );

        // Arming is refused while the pane is mid-close, as worker or as
        // orchestrator — that is the arm-after-cancel guard.
        assert!(reg.is_pane_closing("orch-1"));
        assert!(
            reg.arm_outstanding_delegation("worker-a", "coder", "orch-1", "agent-7", None)
                .is_none(),
            "a delegate landing inside the close window must not arm"
        );
        assert!(
            reg.arm_outstanding_delegation("orch-1", "coder", "orch-2", "agent-9", None)
                .is_none(),
            "the closing pane must also be refused as a worker target"
        );

        reg.finish_pane_close("orch-1", true);
        assert!(!reg.is_pane_closing("orch-1"));
        assert!(
            reg.arm_outstanding_delegation("worker-a", "coder", "orch-1", "agent-7", None)
                .is_some(),
            "after the transition completes, arming works again"
        );
    }

    /// PRD #126 M1 review (finding 6): a late `work-done` from a superseded
    /// delegation must retire THAT delegation, leaving the newest record (and
    /// its watch) armed — it used to clobber the newest record, after which the
    /// re-delegated worker could go silent forever with no nudge.
    #[test]
    fn retire_applies_work_done_to_the_oldest_outstanding_delegation() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let first = reg
            .arm_outstanding_delegation("worker", "coder", "orch", "agent-1", None)
            .expect("arm #1");
        let second = reg
            .arm_outstanding_delegation("worker", "coder", "orch", "agent-1", None)
            .expect("arm #2");
        assert!(second.seq > first.seq, "seq must be monotonic");

        // Delegation #1's late completion retires the superseded delegation.
        match reg.retire_outstanding_delegation("worker") {
            DelegationRetirement::RetiredSuperseded { remaining, seq, .. } => {
                assert_eq!(remaining, 0);
                assert_eq!(seq, second.seq, "the newest record stays armed");
            }
            other => panic!("expected a superseded retirement, got {other:?}"),
        }
        // #2's watch is still live and still owns the record.
        let taken = reg
            .take_outstanding_delegation_if("worker", second.seq)
            .expect("delegation #2 must still be outstanding");
        assert_eq!(taken.seq, second.seq);
        assert!(matches!(
            reg.retire_outstanding_delegation("worker"),
            DelegationRetirement::Nothing
        ));
    }

    /// PRD #249 round-6 review (Greptile): the silent-worker watch needs the same
    /// oldest-first accounting as the idle detector. `arm_silence_watch` replaces
    /// the record, so the map holds the NEWEST watch — an unconditional cancel on
    /// `work-done` let a stale completion from delegation N disarm delegation
    /// N+1's watch, silently switching off the undelivered-prompt detector for
    /// exactly the case it exists to surface.
    #[test]
    fn retire_silence_watch_applies_work_done_to_the_oldest_watch() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let first = reg
            .arm_silence_watch("worker", "orch", None)
            .expect("arm #1");
        let mut first_cancel = first.cancel;
        let second = reg
            .arm_silence_watch("worker", "orch", None)
            .expect("arm #2");
        let mut second_cancel = second.cancel;
        assert!(second.seq > first.seq, "seq must be monotonic");
        assert!(
            matches!(
                first_cancel.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ),
            "the superseding arm must cancel the older watch's task"
        );

        // Delegation #1's late completion is credited to the superseded watch.
        match reg.retire_silence_watch("worker") {
            SilenceWatchRetirement::KeptNewer { seq, remaining } => {
                assert_eq!(seq, second.seq, "the newest watch stays armed");
                assert_eq!(remaining, 0);
            }
            other => panic!("expected the newest watch to survive, got {other:?}"),
        }
        assert!(
            matches!(
                second_cancel.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "a stale work-done must leave the newer delegation's watch armed"
        );

        // #2's own completion does disarm it, which is the timely case.
        match reg.retire_silence_watch("worker") {
            SilenceWatchRetirement::Cancelled { seq } => assert_eq!(seq, second.seq),
            other => panic!("expected a cancellation, got {other:?}"),
        }
        assert!(
            matches!(
                second_cancel.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ),
            "the retired watch's task must be woken, not just unlinked"
        );
        assert!(matches!(
            reg.retire_silence_watch("worker"),
            SilenceWatchRetirement::Nothing
        ));
    }

    /// The identity-bound worker-side match `sweep_delegations_on_exit`
    /// (via `drain_delegations_touching_for_exit`) requires before it will
    /// retire a delegation for the exiting pane's worker side: a record
    /// armed synchronously in `handle_delegate`'s fan-out loop, before
    /// `dispatch_one_owned` has resolved the eventual worker identity, has
    /// `worker_agent_id: None`. Deleting the identity check from
    /// `drain_delegations_touching_for_exit` would leave
    /// `scheduler/idle-worker/016` green while retiring records it must not
    /// touch — this and the next two tests are what actually pins the gate.
    #[test]
    fn sweep_on_exit_leaves_an_unbound_delegation_armed() {
        let reg = Arc::new(AgentPtyRegistry::new());
        reg.arm_outstanding_delegation("worker", "coder", "orch", "orch-agent", None)
            .expect("arm delegation");

        let swept = reg.sweep_delegations_on_exit("worker", "some-agent");
        assert!(
            swept.is_empty(),
            "an unbound record must not be mistaken for a stranger's exit"
        );
    }

    /// The bound case: sweeping with a DIFFERENT agent id than the one bound
    /// leaves the record armed; sweeping with the bound agent id retires it.
    /// This is the exact narrowing `drain_delegations_touching_for_exit`'s
    /// identity gate exists for.
    #[test]
    fn sweep_on_exit_retires_only_the_bound_workers_delegation() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let armed = reg
            .arm_outstanding_delegation("worker", "coder", "orch", "orch-agent", None)
            .expect("arm delegation");
        reg.bind_delegation_worker_agent_id("worker", armed.seq, "agent-a");

        let swept = reg.sweep_delegations_on_exit("worker", "agent-b");
        assert!(
            swept.is_empty(),
            "a stranger's exit (a different agent id) must not retire this record"
        );

        let swept = reg.sweep_delegations_on_exit("worker", "agent-a");
        assert_eq!(
            swept.len(),
            1,
            "the bound worker's own exit must retire its delegation"
        );
    }

    /// The orchestrator-side mirror of the test above: sweeping the
    /// orchestrator's pane with a DIFFERENT agent id than the one the
    /// delegation was armed with leaves the record armed; sweeping with the
    /// armed `orchestrator_agent_id` retires it. This pins the
    /// `record.orchestrator_agent_id == exited_agent_id` gate in
    /// `drain_delegations_touching_for_exit` — every other test in this file
    /// sweeps pane `"worker"`, so without this test that gate could be
    /// deleted and everything, including `scheduler/idle-worker/016`, would
    /// stay green.
    #[test]
    fn sweep_on_exit_retires_only_the_bound_orchestrators_delegation() {
        let reg = Arc::new(AgentPtyRegistry::new());
        reg.arm_outstanding_delegation("worker", "coder", "orch", "orch-agent", None)
            .expect("arm delegation");

        let swept = reg.sweep_delegations_on_exit("orch", "someone-else");
        assert!(
            swept.is_empty(),
            "a stranger's exit (a different orchestrator agent id) must not retire this record"
        );

        let swept = reg.sweep_delegations_on_exit("orch", "orch-agent");
        assert_eq!(
            swept.len(),
            1,
            "the bound orchestrator's own exit must retire its delegation"
        );
    }

    /// `bind_delegation_worker_agent_id` is `seq`-guarded: a bind carrying a
    /// SUPERSEDED delegation's generation must not attach to the record that
    /// replaced it. Otherwise a slow, stale dispatch could bind a fresher
    /// delegation to the wrong worker identity.
    #[test]
    fn bind_delegation_worker_agent_id_ignores_a_stale_seq() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let first = reg
            .arm_outstanding_delegation("worker", "coder", "orch", "orch-agent", None)
            .expect("arm #1");
        reg.arm_outstanding_delegation("worker", "coder", "orch", "orch-agent", None)
            .expect("arm #2 supersedes #1");

        // #1's generation is now stale — the record for "worker" is #2's.
        reg.bind_delegation_worker_agent_id("worker", first.seq, "attacker-agent");

        // If the stale bind had taken effect, this would retire the record;
        // it must not, because the live record's `worker_agent_id` is still
        // unset.
        let swept = reg.sweep_delegations_on_exit("worker", "attacker-agent");
        assert!(
            swept.is_empty(),
            "a stale-seq bind must not attach to the delegation that superseded it"
        );
    }

    /// Silence-watch analogue of the unbound-delegation case above:
    /// `arm_silence_watch` takes the worker identity directly (no separate
    /// bind step), but an unbound (`None`) watch must still survive a
    /// sweep for an unrelated exiting agent.
    #[test]
    fn sweep_on_exit_leaves_an_unbound_silence_watch_armed() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let armed = reg
            .arm_silence_watch("worker", "orch", None)
            .expect("arm silence watch");
        let mut cancel = armed.cancel;

        let _ = reg.sweep_delegations_on_exit("worker", "some-agent");
        assert!(
            matches!(cancel.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "an unbound silence watch must not be retired by an unrelated exit"
        );
    }

    /// A silence watch bound to a worker identity at arm time survives a
    /// sweep for a DIFFERENT exiting agent, and is retired only by the
    /// agent it is actually bound to.
    #[test]
    fn sweep_on_exit_retires_only_the_bound_workers_silence_watch() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let armed = reg
            .arm_silence_watch("worker", "orch", Some("agent-a"))
            .expect("arm silence watch");
        let mut cancel = armed.cancel;

        let _ = reg.sweep_delegations_on_exit("worker", "agent-b");
        assert!(
            matches!(cancel.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "a stranger's exit (a different agent id) must not retire this watch"
        );

        let _ = reg.sweep_delegations_on_exit("worker", "agent-a");
        assert!(
            matches!(cancel.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
            "the bound worker's own exit must retire its silence watch"
        );
    }

    /// `is_agent_still_registered` is `pump_reader`'s only way to tell a
    /// NATURAL exit (nothing has removed the registry entry yet) apart from
    /// one that was the daemon's own doing (`close_agent` /
    /// `respawn_agent_for_pane`, both of which remove the entry BEFORE
    /// killing the child). Getting this wrong in the natural-exit direction
    /// means a "worker exited without work-done" notice fires on every
    /// deliberate close or `clear = true` respawn.
    #[tokio::test]
    async fn is_agent_still_registered_distinguishes_natural_exit_from_deliberate_close() {
        let registry = Arc::new(AgentPtyRegistry::new());

        // Natural exit: the child dies on its own, but nothing has told the
        // registry to remove the entry — pump_reader's EOF branch is the
        // first thing to learn about it, exactly the case the sweep must act
        // on.
        let natural_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/usr/bin/true"),
                ..SpawnOptions::default()
            })
            .expect("spawn a naturally-exiting agent");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && registry.live_count() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            registry.live_count(),
            0,
            "test prerequisite: /usr/bin/true must have exited"
        );
        assert!(
            registry.is_agent_still_registered(&natural_id),
            "a natural exit must leave the entry registered — nothing has removed it yet"
        );

        // Deliberate close: close_agent removes the entry BEFORE the kill
        // even completes, so the sweep must be skipped for this identity.
        let closing_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                ..SpawnOptions::default()
            })
            .expect("spawn an agent to close deliberately");
        registry.close_agent(&closing_id).expect("deliberate close");
        assert!(
            !registry.is_agent_still_registered(&closing_id),
            "close_agent removes the entry before its kill completes — the natural-exit EOF \
             that follows must find nothing registered and skip the sweep. \
             respawn_agent_for_pane removes its entry the same way, before its own kill, for the \
             same reason."
        );

        registry.shutdown_all();
    }

    /// Issue #448: the commission ledger answers "did the orchestrator ask for
    /// this?" on its own, so it counts delegations rather than tracking the newest
    /// — two unanswered delegations are two commissions, and only a completion
    /// beyond them is unsolicited.
    #[test]
    fn commission_ledger_credits_one_completion_per_delegation() {
        let reg = Arc::new(AgentPtyRegistry::new());
        assert_eq!(
            reg.retire_delegation_commission("worker"),
            WorkDoneProvenance::Unsolicited,
            "a worker nobody delegated to owes nothing"
        );

        assert!(reg.arm_delegation_commission("worker", "orch"));
        assert!(reg.arm_delegation_commission("worker", "orch"));
        assert_eq!(
            reg.retire_delegation_commission("worker"),
            WorkDoneProvenance::Solicited { remaining: 1 },
            "the first completion answers one of two outstanding commissions"
        );
        assert_eq!(
            reg.retire_delegation_commission("worker"),
            WorkDoneProvenance::Solicited { remaining: 0 },
            "the second answers the last one"
        );
        assert_eq!(
            reg.retire_delegation_commission("worker"),
            WorkDoneProvenance::Unsolicited,
            "a third completion is answering nothing — the defect in #448"
        );
    }

    /// Issue #448 review (finding 1): a delegate whose task pointer never
    /// reached the worker owes nothing, so its commission is released rather
    /// than left standing for a later uncommissioned completion to spend. It
    /// releases exactly ONE, so a sibling delegation that DID land still gets
    /// its completion credited.
    #[test]
    fn commission_ledger_releases_an_undelivered_delegations_commission() {
        let reg = Arc::new(AgentPtyRegistry::new());
        assert!(
            !reg.release_delegation_commission("worker"),
            "there is nothing to release for a worker nobody delegated to"
        );

        // One delegate, undelivered: the ledger must not keep the debt.
        assert!(reg.arm_delegation_commission("worker", "orch"));
        assert!(reg.release_delegation_commission("worker"));
        assert_eq!(
            reg.retire_delegation_commission("worker"),
            WorkDoneProvenance::Unsolicited,
            "a failed delegate must not leave a phantom commission for a later \
             uncommissioned work-done to spend — that is #448 through its own fix"
        );

        // Two delegates, only the second undelivered: the first is still owed.
        assert!(reg.arm_delegation_commission("worker", "orch"));
        assert!(reg.arm_delegation_commission("worker", "orch"));
        assert!(reg.release_delegation_commission("worker"));
        assert_eq!(
            reg.retire_delegation_commission("worker"),
            WorkDoneProvenance::Solicited { remaining: 0 },
            "releasing one failed delegate must not discard a sibling delegation's \
             genuine commission"
        );
        assert_eq!(
            reg.retire_delegation_commission("worker"),
            WorkDoneProvenance::Unsolicited,
            "and only the one that landed is credited"
        );
    }

    /// Issue #448: the ledger is armed for a delegate whatever the two detectors
    /// are set to, which is the property that makes `Unsolicited` mean what it
    /// says. It is swept by the same two pane roles as the watches, and refused
    /// mid-close for the same arm-after-cancel reason.
    #[test]
    fn commission_ledger_is_swept_by_either_panes_close_and_refuses_mid_close() {
        let reg = Arc::new(AgentPtyRegistry::new());
        assert!(reg.arm_delegation_commission("worker-a", "orch-1"));
        assert!(reg.arm_delegation_commission("worker-b", "orch-2"));

        // Closing the ORCHESTRATOR clears what was owed to it; an unrelated
        // orchestration's commission survives.
        drop(reg.begin_pane_close("orch-1"));
        assert_eq!(
            reg.retire_delegation_commission("worker-a"),
            WorkDoneProvenance::Unsolicited,
            "a commission owed to a closed orchestrator must not survive it"
        );
        assert_eq!(
            reg.retire_delegation_commission("worker-b"),
            WorkDoneProvenance::Solicited { remaining: 0 },
            "another orchestration's commission must be untouched by the close"
        );

        // Arming is refused while either pane is mid-close, as worker or as
        // orchestrator: a phantom commission would launder a later unsolicited
        // completion into a solicited one.
        assert!(reg.is_pane_closing("orch-1"));
        assert!(
            !reg.arm_delegation_commission("worker-a", "orch-1"),
            "a closing orchestrator must not accept new commissions"
        );
        assert!(reg.arm_delegation_commission("worker-a", "orch-live"));
        drop(reg.begin_pane_close("worker-a"));
        assert!(
            !reg.arm_delegation_commission("worker-a", "orch-live"),
            "a closing worker must not accept new commissions"
        );
        assert_eq!(
            reg.retire_delegation_commission("worker-a"),
            WorkDoneProvenance::Unsolicited,
            "the worker's own close swept its ledger entry too"
        );
    }

    /// PRD #249 round-6 review (Greptile): the M1 readiness buffer must be able
    /// to abandon a pane that starts closing mid-wait instead of sleeping out the
    /// remainder (up to the 30 s clamp) before its guarded write discovers the
    /// target is gone.
    #[test]
    fn pane_close_signal_resolves_when_the_close_begins() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let mut waiting = reg.pane_close_signal("worker");
        assert!(
            matches!(waiting.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "an open pane must not look like a closing one"
        );
        drop(reg.begin_pane_close("other"));
        assert!(
            matches!(waiting.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "an unrelated pane's close must not cancel this wait"
        );

        drop(reg.begin_pane_close("worker"));
        assert!(
            matches!(
                waiting.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ),
            "the pane's own close must wake the wait"
        );
        // Asking while the pane is already mid-close is pre-resolved, which is
        // what closes the register-after-`begin_pane_close` race.
        let mut mid_close = reg.pane_close_signal("worker");
        assert!(matches!(
            mid_close.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));

        reg.finish_pane_close("worker", true);
        let mut after = reg.pane_close_signal("worker");
        assert!(
            matches!(after.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "after the transition completes, a fresh wait is live again"
        );
        drop(after);
        // Abandoned waits are pruned rather than accumulating one sender per
        // delegate for the lifetime of the daemon.
        for _ in 0..5 {
            drop(reg.pane_close_signal("worker"));
        }
        let _live = reg.pane_close_signal("worker");
        assert_eq!(
            reg.delegations.lock().unwrap().close_waiters["worker"].len(),
            1,
            "senders whose receiver is gone must be pruned on the next call"
        );
    }

    /// PRD #126 M1 review (finding 2) / audit (finding 3): removing a record
    /// must WAKE its watch task, not just unlink it, or every superseded or
    /// completed delegation leaves a task asleep for the full timeout.
    #[test]
    fn dropping_a_record_resolves_its_watch_cancellation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let reg = Arc::new(AgentPtyRegistry::new());
            let armed = reg
                .arm_outstanding_delegation("worker", "coder", "orch", "agent-1", None)
                .expect("arm");
            match reg.retire_outstanding_delegation("worker") {
                DelegationRetirement::Retired(_) => {}
                other => panic!("expected a plain retirement, got {other:?}"),
            }
            // The sender was dropped with the record, so the watch's select arm
            // is already resolved — no sleep, no wait.
            assert!(
                armed.cancel.await.is_err(),
                "a dropped record must resolve the cancellation channel"
            );
        });
    }

    /// PRD #126 + #140: the idle watch reads the orchestrator pane's live
    /// membership back out of the registry for two decisions — the orchestration
    /// cwd it resolves the timeout from, and the identity it revalidates against
    /// immediately before submitting. Both need `orchestration_id` and
    /// `orchestration_cwd`, which a name-only accessor dropped.
    #[test]
    fn pane_orchestration_reports_the_instance_token_and_orchestration_cwd() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let id = reg
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "orch-pane".to_string())],
                tab_membership: Some(TabMembership::Orchestration {
                    name: "tdd-cycle".to_string(),
                    role_index: 0,
                    role_name: "orchestrator".to_string(),
                    is_start_role: true,
                    orchestration_cwd: Some("/home/u/project".to_string()),
                    display_title: None,
                    orchestration_id: Some("orch-aaaa-0".to_string()),
                }),
                ..SpawnOptions::default()
            })
            .expect("spawn the orchestrator stub");

        assert_eq!(
            reg.pane_orchestration("orch-pane"),
            Some(PaneOrchestration {
                name: "tdd-cycle".to_string(),
                instance_id: Some("orch-aaaa-0".to_string()),
                cwd: Some("/home/u/project".to_string()),
            })
        );
        // A pane nobody owns, and a pane whose agent is gone, both report
        // `None` — which `orchestration_still_matches` treats as "no evidence
        // of a mismatch", never as a refusal.
        assert_eq!(reg.pane_orchestration("some-other-pane"), None);
        reg.close_agent(&id).expect("close the orchestrator stub");
        assert_eq!(reg.pane_orchestration("orch-pane"), None);
    }

    // ---------------------------------------------------------------------
    // Issue #581 — one wedged agent's reap must not starve its siblings of
    // their phase-3 SIGKILL.
    // ---------------------------------------------------------------------

    /// A latch the test flips to let a deliberately-wedged reap finally
    /// complete, so the shutdown thread can always be joined.
    #[derive(Debug, Default)]
    struct WedgeGate {
        released: Mutex<bool>,
        wake: std::sync::Condvar,
    }

    impl WedgeGate {
        fn is_released(&self) -> bool {
            *self.released.lock().unwrap()
        }

        fn block_until_released(&self) {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }

    /// A [`portable_pty::Child`] whose *reap* is under the test's control while
    /// its *kill* is real.
    ///
    /// No real process can be coaxed into the shape issue #581 is about — a
    /// child wedged in uninterruptible kernel I/O, which SIGKILL cannot
    /// dislodge until the I/O completes — because a real child always returns
    /// from `wait` once SIGKILL lands. So the wedge is modelled here and only
    /// here: everything the teardown path *does* (the signal it sends, the pid
    /// it sends it to) stays production code running against a real process
    /// group in the Unix test below.
    #[derive(Debug)]
    struct WedgedChild {
        /// Reported to the teardown path as this child's pid. `Some` makes the
        /// production `killpg(SIGKILL)` land on a real process group; `None`
        /// drives the documented pid-unavailable fallback, which is
        /// `Child::kill` on both backends and therefore observable on Windows
        /// too.
        pid: Option<u32>,
        /// How many times the teardown path issued a kill through that
        /// fallback. A count, not a flag: phase 1's SIGTERM ask reaches the
        /// same fallback, so only a kill *beyond* [`PHASE_ONE_FALLBACK_KILLS`]
        /// is phase 3's. (The first draft asserted a flag and passed on the
        /// unfixed code — phase 1 had already set it for every agent.)
        kills: Arc<std::sync::atomic::AtomicUsize>,
        /// Set when the child was actually reaped (by either `wait` or a
        /// `try_wait` that reported an exit). The fix must not buy signal
        /// independence by dropping the reap.
        reaped: Arc<AtomicBool>,
        /// Until released, `wait` blocks forever and `try_wait` keeps saying
        /// "still running".
        gate: Arc<WedgeGate>,
        /// When set, the blocking `wait` never returns *at all*, however the
        /// gate stands — the unbounded-`wait` half of the same wedge, which
        /// only a `try_wait`-based reap can get past.
        wait_never_returns: bool,
    }

    impl WedgedChild {
        fn new(pid: Option<u32>, gate: Arc<WedgeGate>) -> Self {
            Self {
                pid,
                kills: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reaped: Arc::new(AtomicBool::new(false)),
                gate,
                wait_never_returns: false,
            }
        }
    }

    impl portable_pty::ChildKiller for WedgedChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            unreachable!("no teardown path clones a killer")
        }
    }

    impl portable_pty::Child for WedgedChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            if !self.gate.is_released() {
                return Ok(None);
            }
            self.reaped.store(true, Ordering::SeqCst);
            Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            if self.wait_never_returns {
                // `park` may wake spuriously, so loop: this call must never
                // return, which is the whole point of the flag.
                loop {
                    std::thread::park();
                }
            }
            self.gate.block_until_released();
            self.reaped.store(true, Ordering::SeqCst);
            Ok(portable_pty::ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            self.pid
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    /// Poll `done` until it holds or `budget` elapses. Returns what it last saw.
    fn holds_within(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        done()
    }

    /// How long a phase-3 kill is given to reach every agent before the test
    /// calls it starved. Generous — it is only ever paid on failure.
    const WEDGE_TEST_BUDGET: Duration = Duration::from_secs(10);

    /// Kills that phase 1's SIGTERM *ask* contributes per agent for a pid-less
    /// child, before phase 3 is reached at all: on Unix `killpg` needs a pid and
    /// so takes the `Child::kill` fallback, while on Windows
    /// `GenerateConsoleCtrlEvent` skips a pid-less child outright. Phase 3's
    /// force-kill is the one *on top of* this baseline, so it is the only thing
    /// a strictly-greater count can be.
    const PHASE_ONE_FALLBACK_KILLS: usize = if cfg!(unix) { 1 } else { 0 };

    /// Issue #581 (regression): phase 3 must force-kill *every* surviving
    /// agent, even when the first one it touches never finishes being reaped.
    ///
    /// Both agents are wedged, so the pre-fix serial loop
    /// (`for mut agent in agents { force_kill_child_and_wait(…) }`) issues
    /// exactly one kill whichever order `drain()` yields the map in, and then
    /// blocks forever inside that agent's `wait()`. Requiring *both* kills is
    /// therefore deterministically red on the old code and green on the new one,
    /// with no dependence on `HashMap` iteration order. This is the portable
    /// half — `pid: None` takes the documented pid-unavailable fallback, which
    /// is `Child::kill` on the Unix *and* the Windows backend.
    #[test]
    fn shutdown_all_graceful_force_kills_every_agent_even_when_a_reap_wedges() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let gates: Vec<Arc<WedgeGate>> = (0..2).map(|_| Arc::new(WedgeGate::default())).collect();
        let mut kills = Vec::new();
        let mut reaped = Vec::new();
        for gate in &gates {
            let child = WedgedChild::new(None, gate.clone());
            kills.push(child.kills.clone());
            reaped.push(child.reaped.clone());
            registry.insert_test_agent(Box::new(child));
        }

        let force_killed = |k: &Arc<std::sync::atomic::AtomicUsize>| {
            k.load(Ordering::SeqCst) > PHASE_ONE_FALLBACK_KILLS
        };
        let shutting_down = registry.clone();
        let shutdown = std::thread::spawn(move || {
            shutting_down.shutdown_all_graceful(Duration::from_millis(0));
        });

        let all_killed = holds_within(WEDGE_TEST_BUDGET, || kills.iter().all(force_killed));
        // Counted *before* the release below: letting the wedge go lets the old
        // serial loop finish delivering, so a count taken afterwards reads zero
        // even when the starvation happened.
        let starved = kills.iter().filter(|k| !force_killed(k)).count();

        // Release before asserting so the shutdown thread is always joinable,
        // failure or not.
        for gate in &gates {
            gate.release();
        }
        shutdown.join().expect("the shutdown thread must not panic");

        assert!(
            all_killed,
            "every agent must be force-killed in phase 3; a wedged sibling's reap \
             starved {} of {} agents of their kill",
            starved,
            kills.len()
        );
        assert!(
            reaped.iter().all(|r| r.load(Ordering::SeqCst)),
            "signal independence must not be bought by dropping the reap — a \
             child that is signalled and never waited on is a zombie"
        );
    }

    /// Control for the wedged tests: with nothing wedged, the very same two
    /// agents are force-killed *and* reaped, and `shutdown_all_graceful`
    /// returns on its own.
    ///
    /// Without it, "both agents were killed" could not distinguish *the wedge*
    /// starving a sibling from this whole path being broken — the pre-fix code
    /// passes this one.
    #[test]
    fn shutdown_all_graceful_force_kills_and_reaps_every_agent_when_nothing_wedges() {
        let registry = AgentPtyRegistry::new();
        let mut kills = Vec::new();
        let mut reaped = Vec::new();
        for _ in 0..2 {
            let gate = Arc::new(WedgeGate::default());
            gate.release();
            let child = WedgedChild::new(None, gate);
            kills.push(child.kills.clone());
            reaped.push(child.reaped.clone());
            registry.insert_test_agent(Box::new(child));
        }

        registry.shutdown_all_graceful(Duration::from_millis(0));

        assert!(
            kills
                .iter()
                .all(|k| k.load(Ordering::SeqCst) > PHASE_ONE_FALLBACK_KILLS),
            "with no wedge anywhere, every agent is force-killed"
        );
        assert!(
            reaped.iter().all(|r| r.load(Ordering::SeqCst)),
            "with no wedge anywhere, every agent is reaped"
        );
        assert!(
            registry.is_empty(),
            "the registry is drained by the shutdown"
        );
    }

    /// A real process in a process group of its own that **ignores SIGTERM**,
    /// plus a channel that fires when it dies.
    ///
    /// `trap ""` sets `SIG_IGN`, which `exec` preserves, so only phase 3's
    /// `killpg(SIGKILL)` can end this process — the shape phase 3 exists for,
    /// and the behaviour of any interactive shell. Death is observed through the
    /// EOF its inherited stdout pipe gets once the last holder of the write end
    /// is gone; a `kill(pid, 0)` liveness probe could not, because a killed
    /// child answers it right up until we reap the zombie.
    #[cfg(unix)]
    fn spawn_sigterm_proof_stand_in() -> (std::process::Child, std::sync::mpsc::Receiver<()>) {
        use std::os::unix::process::CommandExt as _;

        let mut proc = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"trap "" TERM; printf r; exec sleep 300"#)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            // Its own process group, so the production `killpg` reaches this
            // process and nothing else — the test runner included.
            .process_group(0)
            .spawn()
            .expect("spawn the stand-in agent process");
        let mut stdout = proc.stdout.take().expect("piped stdout");
        // Block until the shell confirms the trap is installed. Without this
        // handshake the test races `sh`'s startup against phase 1's SIGTERM and
        // the stand-in intermittently dies before it is SIGTERM-proof, which
        // makes the whole assertion pass vacuously (measured: both stand-ins
        // gone ~1 ms after spawn, no phase 3 involved).
        let mut ready = [0u8; 1];
        stdout
            .read_exact(&mut ready)
            .expect("stand-in readiness byte");
        assert_eq!(&ready, b"r", "unexpected readiness byte from the stand-in");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = stdout.read_to_end(&mut sink);
            let _ = tx.send(());
        });
        (proc, rx)
    }

    /// Issue #581 at the altitude an operator sees it: a real agent process left
    /// **alive** by a shutdown that looked clean.
    ///
    /// Same deterministic setup as the portable test — both agents wedged, so
    /// exactly one kill escapes the pre-fix loop regardless of drain order — but
    /// the pid handed to the teardown path is a real one, so the SIGKILL that
    /// does or does not arrive is the production `killpg` landing on a real
    /// process group, and the assertion is that no agent process outlived the
    /// shutdown.
    #[cfg(unix)]
    #[test]
    fn shutdown_all_graceful_kills_every_real_agent_process_even_when_a_reap_wedges() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let gates: Vec<Arc<WedgeGate>> = (0..2).map(|_| Arc::new(WedgeGate::default())).collect();
        let mut stand_ins = Vec::new();
        for gate in &gates {
            let (proc, died) = spawn_sigterm_proof_stand_in();
            registry.insert_test_agent(Box::new(WedgedChild::new(Some(proc.id()), gate.clone())));
            stand_ins.push((proc, died));
        }

        let shutting_down = registry.clone();
        let shutdown = std::thread::spawn(move || {
            shutting_down.shutdown_all_graceful(Duration::from_millis(0));
        });

        let died: Vec<bool> = stand_ins
            .iter()
            .map(|(_, died)| died.recv_timeout(WEDGE_TEST_BUDGET).is_ok())
            .collect();

        // Release before asserting so the shutdown thread is always joinable,
        // and reap the stand-ins whatever the outcome.
        for gate in &gates {
            gate.release();
        }
        shutdown.join().expect("the shutdown thread must not panic");
        for (mut proc, _) in stand_ins {
            let _ = proc.kill();
            let _ = proc.wait();
        }

        assert!(
            died.iter().all(|d| *d),
            "every agent process must receive phase 3's SIGKILL regardless of where \
             a wedged sibling sits in the drain order; per-agent died? = {died:?}"
        );
    }

    /// Issue #581, the other half of the same starvation: phase 3's *reap* must
    /// not sit behind another agent's unbounded `Child::wait` either.
    ///
    /// Both children here report their exit through `try_wait` the moment they
    /// are asked, but their blocking `wait` never returns — the shape the
    /// issue's "reap through a bounded, guaranteed-reaping helper rather than a
    /// bare `child.wait()`" recommendation is about. A reap done through a
    /// shared non-blocking poll collects both statuses and the shutdown
    /// finishes; a reap done through `wait` parks forever on whichever agent it
    /// touches first and never reaps the other.
    #[test]
    fn shutdown_all_graceful_reaps_without_parking_in_an_unbounded_wait() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut reaped = Vec::new();
        for _ in 0..2 {
            let gate = Arc::new(WedgeGate::default());
            gate.release();
            let mut child = WedgedChild::new(None, gate);
            child.wait_never_returns = true;
            reaped.push(child.reaped.clone());
            registry.insert_test_agent(Box::new(child));
        }

        let finished = Arc::new(AtomicBool::new(false));
        let shutting_down = registry.clone();
        let done = finished.clone();
        // Deliberately never joined: on a regression this thread is parked in
        // `Child::wait` forever, which is precisely what is being reported.
        std::thread::spawn(move || {
            shutting_down.shutdown_all_graceful(Duration::from_millis(0));
            done.store(true, Ordering::SeqCst);
        });

        assert!(
            holds_within(WEDGE_TEST_BUDGET, || finished.load(Ordering::SeqCst)),
            "the shutdown never finished — it is parked in a bare `Child::wait` \
             that this child never returns from, so no later agent is reaped"
        );
        assert!(
            reaped.iter().all(|r| r.load(Ordering::SeqCst)),
            "every agent must still be reaped, not merely signalled"
        );
    }
}
