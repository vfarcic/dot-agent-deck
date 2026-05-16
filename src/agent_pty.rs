//! Reusable PTY-spawn primitive shared by the TUI and the daemon.
//!
//! Both the TUI process (`embedded_pane`) and the daemon (`daemon`) need to
//! spawn agent processes attached to a PTY and own the child + master handles
//! for the lifetime of the agent. This module extracts that core so it isn't
//! trapped inside the TUI path. The daemon piece is the foundation for Phase 1
//! (M1.2 streaming attach protocol) — see PRD #76 lines 140–146.

use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

/// Trigger flag the deck client honors to mean "the daemon is already
/// running; attach over its stream socket instead of spawning one." The
/// read site (in `main.rs`) and the scrub site (in [`spawn`] below) share
/// this constant so two string literals can't drift apart.
pub const DOT_AGENT_DECK_VIA_DAEMON: &str = "DOT_AGENT_DECK_VIA_DAEMON";

/// Per-pane id the TUI injects into agent children so hooks running inside
/// the agent (or anything that shells out via `dot-agent-deck`) can route
/// events back to the originating pane. Defined here for the same
/// drift-safety reason as [`DOT_AGENT_DECK_VIA_DAEMON`], and so the daemon
/// scrub site below can reference it by name.
pub const DOT_AGENT_DECK_PANE_ID: &str = "DOT_AGENT_DECK_PANE_ID";

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
    Orchestration { name: String, role_index: usize },
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
pub fn validate_tab_membership(tm: TabMembership) -> Option<TabMembership> {
    if is_valid_display_name(tm.name()) {
        Some(tm)
    } else {
        None
    }
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

/// Forcefully terminate the child and reap it. SIGKILL is preferred over
/// `portable_pty::Child::kill()` (which sends SIGHUP) because a shell can
/// ignore SIGHUP — some distros' bash/zsh configurations do exactly that —
/// leaving the subsequent `wait()` to block forever. SIGKILL cannot be
/// caught or ignored, so the kernel tears the process down and `wait()`
/// returns promptly. Callers should drop the master/writer/reader handles
/// before invoking this so any I/O blocked on the PTY unblocks first.
fn force_kill_child_and_wait(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    if let Some(pid) = child.process_id() {
        // SAFETY: `kill(2)` is async-signal-safe; sending SIGKILL to a pid we
        // just learned from `process_id()` cannot affect any other process
        // until the kernel reaps the child below.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        if rc != 0 {
            // Log so a weakened cleanup guarantee is observable. ESRCH on an
            // already-reaped child is benign; anything else means the child
            // may outlive us.
            let err = std::io::Error::last_os_error();
            tracing::warn!(pid, error = %err, "SIGKILL failed in force_kill_child_and_wait");
        }
    } else {
        // No PID exposed — fall back to portable-pty's signaller.
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn force_kill_and_wait(pty: &mut AgentPty) {
    force_kill_child_and_wait(&mut pty.child);
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
            force_kill_child_and_wait(&mut child);
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
    let pty_system = NativePtySystem::default();

    let pair = pty_system
        .openpty(PtySize {
            rows: opts.rows,
            cols: opts.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| AgentPtyError::Open(e.to_string()))?;

    let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let mut cmd = match opts.command {
        Some(c) if c.contains(' ') => {
            let mut cb = CommandBuilder::new(&default_shell);
            cb.arg("-c");
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

    for (k, v) in &opts.env {
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
fn pump_reader(mut reader: Box<dyn std::io::Read + Send>, bus: Arc<AgentBus>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bus.push(buf[..n].to_vec()),
            Err(_) => break,
        }
    }
}

/// Snapshot of the writer + bus needed to attach a streaming client.
/// Returned by [`AgentPtyRegistry::subscribe`].
pub struct AttachHandle {
    pub snapshot: Vec<u8>,
    pub rx: broadcast::Receiver<Arc<Vec<u8>>>,
    pub writer: Arc<AsyncMutex<Box<dyn std::io::Write + Send>>>,
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
}

/// In-process registry of agent PTYs owned by the daemon. M1.1 only exposed
/// the in-process API; M1.2 wires it to the streaming attach protocol via
/// [`AgentBus`] and [`AttachHandle`].
pub struct AgentPtyRegistry {
    inner: Mutex<RegistryInner>,
    /// Total number of explicit `KIND_DETACH` frames the daemon has observed
    /// across all attach-stream connections. Plain socket close (implicit
    /// detach) does *not* increment this — only the M2.5 explicit-detach
    /// keybinding path does. Surfaced for tests asserting "the client meant
    /// to detach, not just disconnect," and lightweight observability if a
    /// future status command wants it.
    detach_count: AtomicU64,
}

struct RegistryInner {
    next_id: u64,
    agents: HashMap<String, RunningAgent>,
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
            detach_count: AtomicU64::new(0),
        }
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
                let name_len = tm.name().len();
                match validate_tab_membership(tm) {
                    Some(v) => Some(v),
                    None => {
                        return Err(AgentPtyError::Validation(format!(
                            "tab_membership.name fails is_valid_display_name (len={name_len})"
                        )));
                    }
                }
            }
            None => None,
        };

        // Defense in depth: `spawn` already protects the child internally
        // via its own `ChildGuard`, so any failure or panic *inside* spawn
        // cannot orphan the child. This outer `PtyGuard` covers the
        // remaining gap — between `spawn` returning the `AgentPty` and the
        // `agents.insert` below — where lock poisoning on `inner.lock()`
        // would otherwise drop the `AgentPty` without killing the child
        // (`AgentPty` has no `Drop`).
        let guard = PtyGuard::new(spawn(opts)?);
        let mut inner = self.inner.lock().unwrap();

        let pty = guard.take();
        let AgentPty {
            child,
            master,
            writer,
            reader,
        } = pty;

        let bus = Arc::new(AgentBus::new());
        let bus_for_thread = bus.clone();
        // Detached thread: exits when the PTY returns EOF (child killed).
        std::thread::spawn(move || pump_reader(reader, bus_for_thread));

        let agent = RunningAgent {
            child,
            master,
            writer: Arc::new(AsyncMutex::new(writer)),
            bus,
            pane_id_env,
            display_name,
            cwd: cwd_stored,
            tab_membership,
        };

        let id = inner.next_id.to_string();
        inner.next_id += 1;
        inner.agents.insert(id.clone(), agent);
        Ok(id)
    }

    /// Stop an agent: SIGKILL the child, reap it, drop its handles. Any
    /// streaming subscribers will observe their broadcast receiver close
    /// shortly after (once the reader thread sees EOF and drops its bus
    /// reference).
    pub fn close_agent(&self, id: &str) -> Result<(), AgentPtyError> {
        let mut agent = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .agents
                .remove(id)
                .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?
        };
        force_kill_child_and_wait(&mut agent.child);
        Ok(())
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
        Ok(AttachHandle {
            snapshot,
            rx,
            writer: agent.writer.clone(),
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
        let inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        agent
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AgentPtyError::Resize(e.to_string()))
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

    /// All currently-owned agents as `(id, pane_id_env)` records, sorted
    /// ascending by id. M2.x rehydration relies on the captured
    /// `pane_id_env` to rebind the TUI's local pane id to whatever value
    /// the agent's child process already carries in its environment —
    /// without this, hook events emitted by the agent would be silently
    /// dropped after a reconnect (see `RunningAgent::pane_id_env`).
    pub fn agent_records(&self) -> Vec<AgentRecord> {
        let inner = self.inner.lock().unwrap();
        let mut records: Vec<AgentRecord> = inner
            .agents
            .iter()
            .map(|(id, agent)| AgentRecord {
                id: id.clone(),
                pane_id_env: agent.pane_id_env.clone(),
                display_name: agent.display_name.clone(),
                cwd: agent.cwd.clone(),
                tab_membership: agent.tab_membership.clone(),
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

    /// Number of agents currently owned by the registry.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().agents.is_empty()
    }

    /// SIGKILL every agent and drain the registry. Idempotent.
    pub fn shutdown_all(&self) {
        let agents: Vec<RunningAgent> = {
            let mut inner = self.inner.lock().unwrap();
            inner.agents.drain().map(|(_, a)| a).collect()
        };
        for mut agent in agents {
            force_kill_child_and_wait(&mut agent.child);
        }
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

    #[test]
    fn spawn_default_shell_works() {
        let pty = spawn(SpawnOptions::default()).expect("spawn should succeed");
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
    fn pid_is_dead(pid: u32) -> bool {
        let r = unsafe { libc::kill(pid as i32, 0) };
        if r == 0 {
            return false;
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        errno == Some(libc::ESRCH)
    }

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
}
