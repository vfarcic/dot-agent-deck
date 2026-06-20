use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::net::UnixListener;
use tokio::sync::{Notify, broadcast};
use tracing::{error, info, warn};

use crate::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_EXIT_WHEN_ORPHANED, DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS,
    DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS,
};
use crate::error::DaemonError;
use crate::event::{AgentEvent, BroadcastMsg, DaemonMessage};
use crate::scheduler::Scheduler;
use crate::state::SharedState;

/// PRD #93 M1.2: default idle-shutdown window. The daemon exits this many
/// seconds after the last attached client disconnects *and* no managed
/// agents remain. Configurable via [`DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS`];
/// `0` disables the timer entirely (the "always on" / legacy remote
/// behavior).
pub const DEFAULT_IDLE_SHUTDOWN_SECS: u64 = 30;

/// Resolve the configured idle-shutdown window from the environment.
/// Returns `None` when disabled (env var explicitly `0`), `Some(secs)`
/// otherwise. Unparseable values fall back to
/// [`DEFAULT_IDLE_SHUTDOWN_SECS`] so a typo doesn't accidentally disable
/// the timer.
pub fn idle_shutdown_from_env() -> Option<Duration> {
    let secs = match std::env::var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS) {
        Ok(v) => v.parse::<u64>().unwrap_or(DEFAULT_IDLE_SHUTDOWN_SECS),
        Err(_) => DEFAULT_IDLE_SHUTDOWN_SECS,
    };
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

// ---------------------------------------------------------------------------
// Test-only self-defense: orphan watchdog + max-lifetime backstop.
//
// These exist so an idle-disabled TEST daemon (`IDLE_SHUTDOWN_SECS=0`) can't
// leak to PID 1 when the test process dies without running its cleanup `Drop`
// (SIGKILL / panic-abort / nextest timeout / Ctrl-C). Both are env-gated and
// OFF by default, so production detached/lazy-spawned daemons are unaffected.
// ---------------------------------------------------------------------------

/// Parse a truthy env flag value: `1` / `true` / `yes` / `on`
/// (case-insensitive, surrounding whitespace ignored). Everything else
/// (including unset → empty, `0`, `false`) is false.
pub fn parse_bool_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Parse the max-lifetime backstop: `Some(Duration)` for a positive integer
/// number of seconds, `None` otherwise (unset, empty, `0`, or unparseable —
/// meaning "no cap").
pub fn parse_max_lifetime_secs(value: &str) -> Option<Duration> {
    match value.trim().parse::<u64>() {
        Ok(secs) if secs > 0 => Some(Duration::from_secs(secs)),
        _ => None,
    }
}

/// The orphan decision: a daemon should exit when its current parent is `init`
/// (pid 1 — reparented after the original parent died) OR differs from the
/// parent captured at startup (covers a sub-reaper that isn't pid 1). Pure so
/// the policy is unit-testable without a real fork.
pub fn should_exit_orphaned(original_ppid: i32, current_ppid: i32) -> bool {
    current_ppid == 1 || current_ppid != original_ppid
}

/// The calling process's parent pid. Wraps `getppid(2)` (async-signal-safe,
/// infallible) so the single `unsafe` lives in one place.
fn current_ppid() -> i32 {
    // SAFETY: `getppid(2)` has no failure mode and no side effects.
    unsafe { libc::getppid() }
}

/// Daemon-wide broadcast capacity for hook-event `BroadcastMsg`s forwarded
/// to attached TUIs (PRD #76 M2.17). Generous so a slow client doesn't
/// drop events during a normal burst; a subscriber that falls further
/// behind than this is signalled via `RecvError::Lagged` and the
/// per-connection forwarder drops the connection (the TUI reconnects).
///
/// PRD #93 round-5: only hook events ride this channel now —
/// orchestration signals (delegate / work-done) bypass it entirely by
/// being written directly into the target pane's PTY. The previous
/// `PendingBroadcasts` replay buffer, salvage loop, and test gate are
/// gone; the PTY scrollback is the journal.
const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Mode for the daemon's Unix socket: owner-only read/write. Without this the
/// socket file inherits the process umask, which on most systems leaves it
/// world-connectable. See PRD #76 line 298.
const SOCKET_MODE: u32 = 0o600;

/// umask is process-global, so serialize the bind-with-restrictive-umask
/// dance to keep concurrent tests from racing each other's restore. NOTE:
/// this lock only serializes *cooperating* callers that go through
/// `bind_socket`. Any other code path that calls `umask(2)` directly
/// bypasses the lock and can still race with the swap-and-restore here —
/// so don't treat this as a process-global umask guard.
static UMASK_LOCK: Mutex<()> = Mutex::new(());

/// Lock file path for a daemon socket. Used to serialize concurrent
/// `daemon serve` starts against the same `socket_path` (PRD #93 round-2
/// auditor BLOCKER). Each socket gets a dedicated `.lock` file derived
/// deterministically from its path so daemons at different paths don't
/// contend with each other.
///
/// PRD #93 round-4 auditor BLOCKER: the lock file is rooted in a
/// user-owned directory regardless of where the socket lives. When the
/// socket falls back to `/tmp` (no `XDG_RUNTIME_DIR`), a sibling `.lock`
/// in `/tmp` is world-creatable: a local non-privileged user can
/// pre-create `/tmp/<socket-name>.lock` (or symlink it elsewhere) and
/// hold an exclusive `flock` on it forever, DoS-ing daemon startup for
/// the target user. Anchoring the lock under `$XDG_RUNTIME_DIR` (when
/// set) or `~/.cache/dot-agent-deck` (mkdir 0700) eliminates that vector
/// — the parent dir is not world-writable, so a foreign uid can't
/// pre-create the lock entry. The socket itself stays where it is.
///
/// The filename is `{basename}-{hash}.lock` where `hash` is a stable hash
/// of the *full* socket path. The hash keeps two unrelated daemons
/// (e.g. tests with different tempdirs but the same socket basename)
/// from contending on the same lock — without it, parallel tests using
/// `hook.sock` would all serialize through one global lock file.
fn lock_path_for(socket_path: &Path, override_root: Option<&Path>) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    socket_path.as_os_str().hash(&mut hasher);
    let hash = hasher.finish();
    let basename = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("daemon");
    lock_root(override_root).join(format!("{basename}-{hash:016x}.lock"))
}

/// User-owned root directory for daemon lock files. Mirrors the socket
/// resolution order (`XDG_RUNTIME_DIR` first, then a HOME-anchored
/// fallback) but never lands in `/tmp`. Falls back to `~/.cache/dot-agent-deck`
/// when `XDG_RUNTIME_DIR` is unset — that path is owner-only (we mkdir
/// 0700) and is the standard freedesktop user cache root.
///
/// `override_root` is the per-`Daemon` builder-supplied override
/// (round-11 reviewer #B): tests pass it via
/// [`Daemon::with_lock_dir_override`] to pin the resolved root at a
/// per-binary tempdir. Production never supplies one — production
/// `Daemon::new` / `Daemon::with_attach` leave the field at `None`,
/// and there is no public way to set a process-wide override.
/// Subprocess daemons (spawned via `dot-agent-deck daemon serve`)
/// inherit `DOT_AGENT_DECK_LOCK_DIR` from their parent's environment,
/// so the env-var fallback still applies when the override is absent.
fn lock_root(override_root: Option<&Path>) -> PathBuf {
    if let Some(p) = override_root {
        return p.to_path_buf();
    }
    if let Ok(explicit) = std::env::var("DOT_AGENT_DECK_LOCK_DIR") {
        return PathBuf::from(explicit);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime_dir.is_empty()
    {
        return PathBuf::from(runtime_dir).join("dot-agent-deck");
    }
    crate::config::dirs_home()
        .join(".cache")
        .join("dot-agent-deck")
}

/// Create `dir` (recursively) with mode 0o700 and re-apply the mode to
/// pre-existing directories — same defense-in-depth pattern as
/// `daemon_attach::prepare_state_dir`. We invoke this just before
/// acquiring the spawn lock so a fresh install on a system without a
/// runtime dir or `~/.cache/dot-agent-deck` succeeds without manual setup.
fn ensure_lock_root(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// PRD #93 M1.3 live-socket probe. Used by [`run_daemon_with`] to
/// distinguish a still-running daemon from a stale inode left behind by a
/// crashed daemon. Returns `true` only when `connect(2)` actually succeeds
/// — any error (typically `ECONNREFUSED` from a stale inode whose binder
/// is dead) returns false. The connection is dropped immediately.
///
/// This is a copy of [`crate::daemon_attach::probe_socket_alive`]'s logic
/// rather than a re-export to keep the daemon module's run loop
/// independent of the lazy-spawn machinery.
async fn probe_socket_alive(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// Bind a Unix listener at `path` with the socket inode created at 0o600
/// directly. Setting umask before `bind(2)` closes the TOCTOU window between
/// `bind` and a post-bind `chmod`: without this, a local attacker could
/// connect via the world-readable inode that exists between the two calls.
///
/// `pub(crate)` so the M1.2 attach-protocol server (`daemon_protocol`) and
/// the M2.4 `connect` bridge (`crate::connect`) can reuse the same
/// restrictive-umask bind dance for their sockets.
pub(crate) fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    let _guard = UMASK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: `umask(2)` is a thread-safe libc call that simply swaps a
    // per-process value. We restore the previous mask immediately after
    // `bind` so other code (file creation elsewhere) is unaffected.
    //
    // The kernel creates the socket inode with mode 0o777 & ~umask, so a
    // mask of 0o177 strips the owner-execute bit and all group/other bits
    // and produces 0o600 directly. (0o077 would yield 0o700 — owner exec is
    // meaningless on a socket but the existing chmod target is 0o600, so we
    // match it.)
    let prev = unsafe { libc::umask(0o177) };
    let result = UnixListener::bind(path);
    unsafe {
        libc::umask(prev);
    }
    result
}

/// Bundle of daemon state. Owns the hook-event `SharedState` and the agent
/// PTY registry, plus the path of the M1.2 streaming-attach socket. The
/// registry is held for the lifetime of the daemon coroutine; on drop it
/// kills any agents it still owns.
pub struct Daemon {
    pub state: SharedState,
    pub pty_registry: Arc<AgentPtyRegistry>,
    /// `None` means "do not start the streaming attach server". This is the
    /// default for the legacy `run_daemon` convenience entrypoint and for
    /// tests that only exercise hook ingestion. Production callers
    /// (`main.rs`) populate this from `config::attach_socket_path()`.
    pub attach_socket_path: Option<PathBuf>,
    /// Daemon-wide broadcast of hook events (PRD #76 M2.17). The hook
    /// loop wraps every successfully-parsed `AgentEvent` in
    /// `BroadcastMsg::Event` and publishes it here; the attach server
    /// hands each `SubscribeEvents` connection its own `Receiver`.
    ///
    /// PRD #93 round-5: this used to carry `Delegate` / `WorkDone`
    /// variants too — the daemon's "dumb pipe" in external mode. With
    /// the orchestration logic moved daemon-side, only hook events ride
    /// this channel now.
    pub event_tx: broadcast::Sender<BroadcastMsg>,
    /// PRD #93 M1.2 attached-client gauge, shared with the attach server.
    /// Incremented at `accept` time, decremented when the connection task
    /// exits, used by the idle monitor to decide when the daemon may exit.
    pub client_count: Arc<AtomicUsize>,
    /// PRD #93 M1.2 idle-shutdown window. When `Some`, the daemon's idle
    /// monitor signals shutdown after the configured duration of zero
    /// attached clients *and* zero managed agents. `None` disables idle
    /// shutdown entirely — the daemon stays up indefinitely. PRD #93
    /// Phase 2 deleted the in-process variant that used to force this
    /// off; the standalone constructor [`with_attach`] is now the only
    /// path and it picks up [`idle_shutdown_from_env`].
    pub idle_shutdown: Option<Duration>,
    /// Round-11 reviewer #B: optional lock-file root override for
    /// in-process tests. When `Some`, [`run_daemon_with`] resolves
    /// the per-socket `.lock` file under this directory instead of
    /// consulting `DOT_AGENT_DECK_LOCK_DIR` / `XDG_RUNTIME_DIR` /
    /// `~/.cache/dot-agent-deck`. Production callers leave it at
    /// `None`; tests set it via [`Self::with_lock_dir_override`].
    ///
    /// Replaces the round-10 `pub static LOCK_DIR_OVERRIDE`. A
    /// per-daemon field has no production API surface — without a
    /// builder call there is no way to pin the lock dir, so a
    /// production binary cannot have its lock root steered by code
    /// elsewhere in the process. Subprocess daemons (spawned via
    /// `dot-agent-deck daemon serve`) inherit the
    /// `DOT_AGENT_DECK_LOCK_DIR` env var from their parent's
    /// environment, so the env-var fallback in `lock_root` continues
    /// to serve them.
    pub lock_dir_override: Option<PathBuf>,
    /// PRD #127 M1.3/M1.4: the daemon-hosted scheduler. `run_daemon_with`
    /// loads the global `schedules.toml`, registers each enabled task on this
    /// scheduler, spawns its firing loop, and shares it with the attach server
    /// (for `ReloadSchedules`/`RunNow`) and the idle monitor (a registered
    /// enabled task is a third keep-alive condition). Constructed empty; tests
    /// that don't serve schedules simply never populate the config.
    pub scheduler: Arc<Scheduler>,
    /// PRD #127 M2.2: in-memory tab-reuse registry keyed by scheduled task
    /// name. A `new_tab_per_fire = false` task reuses the same tab each fire;
    /// shared between the startup registration and the `ReloadSchedules`
    /// handler so a reloaded task keeps reusing its tab. Wiped on restart
    /// (not persisted) — the first post-restart fire spawns fresh.
    pub reuse_registry: crate::spawn::ReuseRegistry,
}

impl Daemon {
    /// Hook-only daemon, no streaming attach server. Preserves the M1.1
    /// behavior for callers that don't need the M1.2 protocol.
    pub fn new(state: SharedState) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Self {
            state,
            pty_registry: Arc::new(AgentPtyRegistry::new()),
            attach_socket_path: None,
            event_tx,
            client_count: Arc::new(AtomicUsize::new(0)),
            // Hook-only daemons don't accept attaches, so idle-shutdown
            // would only fire when agents == 0 — and they have no PTY
            // registry consumers either. Leave the timer off; callers
            // that want it can opt in via [`with_idle_shutdown`].
            idle_shutdown: None,
            lock_dir_override: None,
            scheduler: Arc::new(Scheduler::with_stderr_notifier()),
            reuse_registry: crate::spawn::new_reuse_registry(),
        }
    }

    /// Daemon configured to also serve the M1.2 streaming attach protocol
    /// on `attach_path`. Hook ingestion still uses the path passed to
    /// `run_daemon_with`. Used by `daemon serve` and tests.
    ///
    /// PRD #93 M1.2: idle shutdown defaults to the environment-configured
    /// window ([`idle_shutdown_from_env`]) so an auto-spawned daemon
    /// gracefully exits after its TUI detaches. Tests that don't want
    /// idle shutdown should call [`Self::with_idle_shutdown`] with `None`
    /// (or rely on the in-process constructor, which forces it off).
    pub fn with_attach(state: SharedState, attach_path: PathBuf) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Self {
            state,
            pty_registry: Arc::new(AgentPtyRegistry::new()),
            attach_socket_path: Some(attach_path),
            event_tx,
            client_count: Arc::new(AtomicUsize::new(0)),
            idle_shutdown: idle_shutdown_from_env(),
            lock_dir_override: None,
            scheduler: Arc::new(Scheduler::with_stderr_notifier()),
            reuse_registry: crate::spawn::new_reuse_registry(),
        }
    }

    /// PRD #93 M1.2 fluent override of the idle-shutdown window. Pass
    /// `None` to disable; pass `Some(dur)` to override the env-derived
    /// default. Useful for tests that want a short window without setting
    /// process-global env vars.
    pub fn with_idle_shutdown(mut self, dur: Option<Duration>) -> Self {
        self.idle_shutdown = dur;
        self
    }

    /// Round-11 reviewer #B fluent override: pin the daemon's lock-file
    /// root at `dir` instead of resolving via `DOT_AGENT_DECK_LOCK_DIR`
    /// / `XDG_RUNTIME_DIR` / `~/.cache/dot-agent-deck`. Used by
    /// in-process tests so each test binary's daemons all share one
    /// writable tempdir; production never calls this. Pass `None` to
    /// clear a previously-set override.
    pub fn with_lock_dir_override(mut self, dir: Option<PathBuf>) -> Self {
        self.lock_dir_override = dir;
        self
    }
}

pub async fn run_daemon(socket_path: &Path, state: SharedState) -> Result<(), DaemonError> {
    run_daemon_with(socket_path, Daemon::new(state)).await
}

/// Same as `run_daemon` but lets callers (and tests) inject a pre-built
/// `Daemon` so they can hold a clone of the PTY registry alongside it.
/// If `daemon.attach_socket_path` is set, the M1.2 streaming attach server
/// is spawned alongside the hook-ingestion loop and aborted when this
/// function returns.
pub async fn run_daemon_with(socket_path: &Path, daemon: Daemon) -> Result<(), DaemonError> {
    // PRD #93 M1.3 / round-2 auditor BLOCKER: race protection for the
    // probe-remove-bind sequence.
    //
    // The pre-existing code unconditionally unlinked any file at
    // `socket_path` before binding. Two `daemon serve` processes racing
    // each other would both see the other's socket as "stale," remove it,
    // and bind a fresh inode — silently rebinding the path away from the
    // still-running winner and leaving its clients stranded.
    //
    // Round-1 added a probe-connect to distinguish a live winner from a
    // stale crash leftover. That helps the common case (one daemon, plus
    // a crash leftover) but is still racy: two starters can both observe
    // "exists but not alive" between their probes and proceed to both
    // remove + bind. Audit BLOCKER #1 calls this out explicitly.
    //
    // Fix: hold an exclusive `flock(2)` over a per-socket `.lock` file
    // (anchored in a user-owned directory — see `lock_path_for`) across
    // the entire probe → remove → bind sequence. The
    // `daemon_attach::ensure_daemon_running` path already uses this same
    // primitive on `<state_dir>/spawn.lock` for the launcher side; we
    // reuse it here so the two halves of the racing pair share one
    // serialization point. The lock is released as soon as `bind_socket`
    // succeeds — afterwards, any further start attempt's probe will see
    // the live socket and return AddrInUse without needing the lock.
    //
    // PRD #93 round-4 auditor BLOCKER: the lock file lives under
    // `XDG_RUNTIME_DIR` or `~/.cache/dot-agent-deck` (never `/tmp`) so a
    // local foreign uid can't pre-create the lock entry to DoS startup
    // for the target user. See `lock_path_for` for the resolution rules.
    let lock_path = lock_path_for(socket_path, daemon.lock_dir_override.as_deref());
    if let Some(parent) = lock_path.parent() {
        ensure_lock_root(parent)?;
    }
    let _start_lock = crate::daemon_attach::acquire_spawn_lock(&lock_path).await?;

    if socket_path.exists() {
        if probe_socket_alive(socket_path).await {
            return Err(DaemonError::Io(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "daemon already running at {} — refusing to clobber a live socket",
                    socket_path.display()
                ),
            )));
        }
        std::fs::remove_file(socket_path)?;
    }

    let listener = bind_socket(socket_path)?;
    // Lock has done its job: subsequent starters' probe-connect will now
    // succeed against this listener and return AddrInUse without needing
    // to contend on the lock. Dropping releases the flock and closes the
    // fd; the `.lock` file itself stays on disk (cheap, empty, reused on
    // next start).
    drop(_start_lock);
    // Defense in depth: `bind_socket` already created the inode at 0o600 via
    // umask, but restating the mode here makes the requirement explicit and
    // would cover any future code path that bypasses `bind_socket`.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    info!("Daemon listening on {}", socket_path.display());

    // Hold the registry for the lifetime of the loop so its Drop fires
    // (killing any owned agents) when this future is dropped/aborted.
    let pty_registry = daemon.pty_registry;
    let state = daemon.state;
    let event_tx = daemon.event_tx;
    let client_count = daemon.client_count;
    let idle_shutdown = daemon.idle_shutdown;
    let scheduler = daemon.scheduler;
    let reuse_registry = daemon.reuse_registry;

    // PRD #127 M1.3/M1.4: load the global `schedules.toml` and register each
    // enabled task before the idle monitor starts, so a registered schedule is
    // visible as a keep-alive condition from the daemon's first idle check.
    // Config-load errors are surfaced via the scheduler's notifier; a malformed
    // entry never blocks the daemon or the other entries. Each fire runs the
    // spawn-or-reuse path (PRD #127 M2.2).
    {
        let loaded = crate::config::LoadedSchedules::load();
        scheduler.report_config_errors(&loaded.errors);
        scheduler.reload_apply(
            &loaded.tasks,
            schedule_callback_factory(
                pty_registry.clone(),
                reuse_registry.clone(),
                event_tx.clone(),
            ),
        );
    }
    // Start the per-second cron firing loop. Held as a JoinHandle and aborted
    // on exit so it doesn't outlive the daemon.
    let scheduler_handle = {
        let scheduler = scheduler.clone();
        tokio::spawn(async move {
            scheduler.run().await;
        })
    };

    // PRD #93 M1.2 shutdown signal — `Notify` is single-shot/level-triggered
    // enough for our needs: the idle monitor notifies once when the timer
    // expires, the hook loop's `select!` arm wakes up, and the loop exits.
    let shutdown = Arc::new(Notify::new());

    // Test-only orphan watchdog: when `DOT_AGENT_DECK_EXIT_WHEN_ORPHANED` is
    // truthy, gracefully shut down (via the SAME `shutdown` signal the idle
    // monitor uses — so sockets/agents tear down cleanly) once this daemon is
    // orphaned. OFF by default; production daemons never set the var.
    let orphan_handle = if std::env::var(DOT_AGENT_DECK_EXIT_WHEN_ORPHANED)
        .map(|v| parse_bool_flag(&v))
        .unwrap_or(false)
    {
        let original_ppid = current_ppid();
        let shutdown_signal = shutdown.clone();
        info!(
            original_ppid,
            "exit-when-orphaned watchdog armed (test-only safety net)"
        );
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let cur = current_ppid();
                if should_exit_orphaned(original_ppid, cur) {
                    warn!(
                        original_ppid,
                        current_ppid = cur,
                        "daemon orphaned (parent died/changed); initiating graceful shutdown"
                    );
                    shutdown_signal.notify_one();
                    break;
                }
            }
        }))
    } else {
        None
    };

    // Test-only max-lifetime backstop: when set, gracefully self-exit after the
    // configured seconds no matter what (catches anything the orphan watchdog
    // misses, e.g. a detached daemon whose parent is already PID 1). Unset in
    // production → no cap.
    let max_lifetime_handle = std::env::var(DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS)
        .ok()
        .and_then(|v| parse_max_lifetime_secs(&v))
        .map(|dur| {
            let shutdown_signal = shutdown.clone();
            info!(
                secs = dur.as_secs(),
                "test max-lifetime backstop armed (test-only safety net)"
            );
            tokio::spawn(async move {
                tokio::time::sleep(dur).await;
                warn!(
                    secs = dur.as_secs(),
                    "daemon test max-lifetime reached; initiating graceful shutdown"
                );
                shutdown_signal.notify_one();
            })
        });

    // Optionally start the M1.2 streaming attach server with the shared
    // client counter. We hold its JoinHandle and abort it on exit so it
    // doesn't outlive the daemon.
    //
    // CodeRabbit (PRD #93 round-9): bind the attach listener INLINE
    // before spawning the accept loop, so a bind() error (e.g. a stale
    // socket the cleanup couldn't unlink, or a permission denial on the
    // parent dir) propagates up through `run_daemon_with`'s `Result`
    // instead of getting swallowed by the spawned task's `error!` log.
    // Earlier rounds spawned and discarded the future, so the
    // hook-ingestion daemon "started successfully" while no TUI could
    // ever connect to the attach socket. Returning Err here lets the
    // caller (production `main`, or a test) treat it as a daemon-start
    // failure.
    let attach_handle = if let Some(path) = daemon.attach_socket_path {
        let listener = crate::daemon_protocol::bind_attach_listener(&path)?;
        info!("Attach protocol listening on {}", path.display());
        let registry = pty_registry.clone();
        let attach_event_tx = event_tx.clone();
        let attach_counter = client_count.clone();
        let attach_state = state.clone();
        // PRD #92 F1: hand the same `shutdown` Notify the idle monitor and
        // hook loop use to the attach server. The KIND_SHUTDOWN frame
        // handler signals it after the registry's graceful drain so the
        // hook loop exits, run_daemon_with returns, and the registry's
        // Drop impl kills any survivors.
        let attach_shutdown = shutdown.clone();
        let attach_scheduler = scheduler.clone();
        let attach_reuse = reuse_registry.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = crate::daemon_protocol::serve_attach_with_counter(
                listener,
                registry,
                attach_event_tx,
                attach_counter,
                attach_state,
                Some(attach_shutdown),
                attach_scheduler,
                attach_reuse,
            )
            .await
            {
                error!("attach protocol server error: {e}");
            }
        }))
    } else {
        None
    };

    // PRD #93 M1.2 idle monitor — edge-triggered via the registry's
    // `change_notify` so transitions on both sides (attach counter via
    // `ClientGuard`, registry via spawn/close/exit) wake it
    // immediately. No polling cadence to race against a brief
    // reconnect. PRD #93 Phase 2 deleted the in-process variant that
    // used to skip this; the daemon is always standalone now.
    let idle_handle = idle_shutdown.map(|window| {
        let counter = client_count.clone();
        let registry = pty_registry.clone();
        let shutdown_signal = shutdown.clone();
        let notify = pty_registry.change_notify();
        let idle_scheduler = scheduler.clone();
        tokio::spawn(async move {
            run_idle_monitor(
                counter,
                registry,
                window,
                shutdown_signal,
                notify,
                idle_scheduler,
            )
            .await;
        })
    });

    let result = run_hook_loop(listener, state, event_tx, pty_registry.clone(), shutdown).await;

    if let Some(h) = attach_handle {
        h.abort();
    }
    if let Some(h) = idle_handle {
        h.abort();
    }
    scheduler_handle.abort();
    if let Some(h) = orphan_handle {
        h.abort();
    }
    if let Some(h) = max_lifetime_handle {
        h.abort();
    }
    drop(pty_registry);

    result
}

/// Build a `reload_apply`-compatible callback factory bound to `registry`
/// (PRD #127 M2.3). Each enabled task gets a callback that, on every fire (cron
/// tick or run-now), calls the spawn primitive EXACTLY once with the task's
/// configured values. The registry is the daemon's live PTY registry — the
/// scheduler runs in-process in the daemon, so spawning goes straight through
/// it rather than over the attach socket.
pub(crate) fn schedule_callback_factory(
    registry: Arc<AgentPtyRegistry>,
    reuse: crate::spawn::ReuseRegistry,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> impl FnMut(&crate::config::ScheduledTask) -> crate::scheduler::Callback {
    move |task| make_schedule_callback(task, registry.clone(), reuse.clone(), event_tx.clone())
}

/// One task's firing callback: rebuild the [`crate::spawn::SpawnRequest`] from
/// the task's configured values and fire it via
/// [`crate::spawn::spawn_or_reuse`] (PRD #127 M2.2) — which reuses the task's
/// existing tab when `new_tab_per_fire == false` and a live tab is recorded,
/// or spawns a fresh one otherwise. Spawn failures (mkdir / agent-spawn) are
/// surfaced through the `StderrNotifier` seam and logged here; they never crash
/// the daemon, so a bad task's fire can't take the scheduler (or sibling tasks)
/// down. The deliver-on-idle debounce is read per-fire from the environment.
fn make_schedule_callback(
    task: &crate::config::ScheduledTask,
    registry: Arc<AgentPtyRegistry>,
    reuse: crate::spawn::ReuseRegistry,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> crate::scheduler::Callback {
    let req = crate::spawn::SpawnRequest {
        task_name: task.name.clone(),
        working_dir: task.working_dir.clone(),
        command: task.command.clone(),
        prompt: task.prompt.clone(),
    };
    let new_tab_per_fire = task.new_tab_per_fire;
    Arc::new(move || {
        let registry = registry.clone();
        let reuse = reuse.clone();
        let req = req.clone();
        // PRD #127 finding #2: hand the daemon-wide hook-event broadcast to the
        // fire so a fresh single-agent card surfaces LIVE to an already-attached
        // TUI (see `crate::spawn::surface_spawned_pane`).
        let event_tx = event_tx.clone();
        Box::pin(async move {
            let notifier = crate::scheduler::StderrNotifier;
            let debounce = crate::spawn::reuse_debounce();
            if let Err(e) = crate::spawn::spawn_or_reuse(
                req,
                new_tab_per_fire,
                &registry,
                &reuse,
                &notifier,
                debounce,
                Some(&event_tx),
            )
            .await
            {
                // Already surfaced via the notifier; log for the operator.
                warn!(error = %e, "scheduled spawn failed");
            }
        })
    })
}

/// PRD #93 M1.2 idle monitor — edge-triggered, generation-gated.
///
/// Originally a polling loop (round 1). Round-2 reviewer REV-1 flagged the
/// reconnect-race: between two polls a client could disconnect+reconnect
/// briefly, and if the poll cadence happened to land in the zero-clients
/// window the timer would start; if a follow-up poll happened to miss
/// the reconnect-then-disconnect cycle the daemon could fire shutdown
/// while a TUI was actively re-attaching.
///
/// Round-2 replaced that with edge-triggering + an in-flight timer that
/// the monitor *aborted* when the joint-zero gate broke. Round-4 reviewer
/// BLOCKER: abort is racy. Between the timer task waking from its
/// `sleep(threshold)` and the monitor's cancel landing, the timer can
/// fire and the daemon exits even though a client just reconnected. A
/// brief 1→0→1→0 transition cycle inside one window has the same
/// failure mode: the *old* timer's deadline can still fire even after
/// the monitor scheduled (or thinks it scheduled) a fresh one.
///
/// Fix: replace the abort with an `AtomicU64` generation counter. The
/// monitor increments the generation on every 1→0 transition, spawns a
/// timer task that captures the new value, sleeps `threshold`, and
/// signals shutdown only if the generation hasn't moved since (and the
/// joint-zero gate still holds). A 0→1 transition just bumps the
/// generation — the in-flight timer becomes a no-op when it wakes,
/// without any await on the cancel path.
async fn run_idle_monitor(
    client_count: Arc<AtomicUsize>,
    pty_registry: Arc<AgentPtyRegistry>,
    threshold: Duration,
    shutdown: Arc<Notify>,
    change_notify: Arc<Notify>,
    scheduler: Arc<Scheduler>,
) {
    // Generation counter shared with every in-flight timer task. Each
    // task captures the value it was spawned with; on wake it compares
    // against the current value and bails if they differ. Cancellation
    // is therefore atomic and synchronous (one `fetch_add`) — no abort,
    // no await, no race with the timer's wake-up.
    let generation = Arc::new(AtomicU64::new(0));
    let mut armed = false;

    loop {
        let clients = client_count.load(Ordering::SeqCst);
        let agents = pty_registry.live_count();
        // PRD #127 M1.4 idle carve-out: a registered ENABLED scheduled task is
        // a third keep-alive condition, so the daemon doesn't idle-GC itself
        // between fires (or before the first fire). The scheduler only ever
        // holds enabled tasks, so `is_empty()` is `no_pending_schedules`.
        let no_pending_schedules = scheduler.is_empty();
        let is_idle = clients == 0 && agents == 0 && no_pending_schedules;

        if is_idle {
            if !armed {
                // 1→0 transition (or fresh-startup idle): bump the
                // generation so any prior in-flight timer becomes a
                // no-op when it wakes, then spawn a new timer that
                // captures this generation.
                let my_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                let counter = client_count.clone();
                let registry = pty_registry.clone();
                let shutdown_signal = shutdown.clone();
                let gen_check = generation.clone();
                let timer_scheduler = scheduler.clone();
                let dur = threshold;
                tokio::spawn(async move {
                    tokio::time::sleep(dur).await;
                    if gen_check.load(Ordering::SeqCst) != my_gen {
                        // A 0→1 (or subsequent 1→0) transition has
                        // happened since we were spawned; the live
                        // timer is someone else's. Bail.
                        return;
                    }
                    // Re-check the joint-zero gate too — defense in depth
                    // for the narrow window between the generation check
                    // and the notify, where a connect could in principle
                    // land without the monitor having yet incremented
                    // the generation (the increment happens on the next
                    // `change_notify` wake-up, not synchronously with
                    // the counter mutation).
                    if counter.load(Ordering::SeqCst) == 0
                        && registry.live_count() == 0
                        && timer_scheduler.is_empty()
                    {
                        info!(
                            threshold_secs = dur.as_secs(),
                            "Daemon idle window elapsed (no clients, no agents, no pending schedules); signaling shutdown"
                        );
                        shutdown_signal.notify_one();
                    }
                });
                armed = true;
            }
        } else if armed {
            // 0→1 transition: invalidate the in-flight timer by bumping
            // the generation. The timer task is still scheduled; it'll
            // wake at its old deadline, see the mismatch, and exit
            // silently. No await needed.
            generation.fetch_add(1, Ordering::SeqCst);
            armed = false;
        }

        // Park until the next transition. Tokio Notify stores a permit if
        // notify_one was called between iterations, so a signal that lands
        // after we read the counters but before we await isn't lost.
        change_notify.notified().await;
    }
}

async fn run_hook_loop(
    listener: UnixListener,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    pty_registry: Arc<AgentPtyRegistry>,
    shutdown: Arc<Notify>,
) -> Result<(), DaemonError> {
    loop {
        tokio::select! {
            // PRD #93 M1.2: a notified shutdown wins over a fresh `accept` —
            // we return Ok so `run_daemon_with` cleans up sockets and aborts
            // the attach + idle tasks. The accept future inside the select
            // is dropped, which doesn't leak the listener (only the
            // partially-built tokio future).
            _ = shutdown.notified() => {
                info!("Daemon hook loop exiting on idle shutdown");
                return Ok(());
            }
            accept_res = listener.accept() => match accept_res {
            Ok((stream, _addr)) => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                let pty_registry = pty_registry.clone();
                tokio::spawn(async move {
                    let reader = tokio::io::BufReader::new(stream);
                    let mut lines = reader.lines();

                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Ok(msg) = serde_json::from_str::<DaemonMessage>(&line) {
                            match msg {
                                DaemonMessage::Delegate(signal) => {
                                    info!(
                                        pane_id = %signal.pane_id,
                                        targets = ?signal.to,
                                        "Received delegate signal"
                                    );
                                    // PRD #93 round-5: one path for both
                                    // modes. The daemon owns the role map
                                    // and the PTY registry, so it routes
                                    // the prompt directly into the worker
                                    // pane's PTY — no broadcast hop, no
                                    // detach-window loss surface.
                                    //
                                    // PRD #92 F9 followup-6: pass the
                                    // daemon-wide hook-event sender too so
                                    // per-target dispatch tasks can wait
                                    // for the freshly-spawned agent's
                                    // `SessionStart` event before writing
                                    // the prompt (event-driven readiness,
                                    // replacing the F9 250ms fixed delay).
                                    state
                                        .read()
                                        .await
                                        .handle_delegate(signal, &pty_registry, &event_tx)
                                        .await;
                                }
                                DaemonMessage::WorkDone(signal) => {
                                    info!(
                                        pane_id = %signal.pane_id,
                                        done = signal.done,
                                        "Received work-done signal"
                                    );
                                    state.read().await.handle_work_done(signal, &pty_registry).await;
                                }
                            }
                        } else if let Ok(event) = serde_json::from_str::<AgentEvent>(&line) {
                            info!(
                                session_id = %event.session_id,
                                event_type = ?event.event_type,
                                pane_id = ?event.pane_id,
                                agent_type = ?event.agent_type,
                                "Received event"
                            );
                            // Fan out to subscribed attach connections
                            // *before* mutating local state, so the broadcast
                            // happens whether or not the local `apply_event`
                            // accepts the event (e.g. an unmanaged pane id).
                            // `send` returns Err only when there are no
                            // subscribers — that's expected and ignored.
                            let _ = event_tx.send(BroadcastMsg::Event(event.clone()));
                            // Persist the agent type this hook revealed into
                            // the PTY registry (keyed by pane id), so a later
                            // `list_agents` — e.g. a fresh `dot-agent-deck
                            // connect` after a detach — reports the real agent
                            // instead of "No agent". The spawn-time
                            // `from_command` guess is `None` for shell-launched
                            // agents, so the hook stream is the only place the
                            // daemon ever learns the true type. Upgrade-only
                            // inside the registry; a no-op when the type is
                            // `None` or the pane id is unknown/absent.
                            if let Some(ref pane_id) = event.pane_id {
                                pty_registry.set_agent_type(pane_id, &event.agent_type);
                            }
                            state.write().await.apply_event(event);
                        } else {
                            warn!("Malformed event: {line}");
                        }
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {e}");
            }
            } // end accept_res match
        } // end tokio::select!
    }
}

#[cfg(test)]
mod orphan_watchdog_tests {
    use super::*;

    #[test]
    fn parse_bool_flag_accepts_truthy_values() {
        for v in ["1", "true", "TRUE", "Yes", " on ", "On"] {
            assert!(parse_bool_flag(v), "{v:?} should be truthy");
        }
        for v in ["", "0", "false", "no", "off", "2", "enabled"] {
            assert!(!parse_bool_flag(v), "{v:?} should be falsey");
        }
    }

    #[test]
    fn parse_max_lifetime_secs_only_positive_ints() {
        assert_eq!(
            parse_max_lifetime_secs("300"),
            Some(Duration::from_secs(300))
        );
        assert_eq!(parse_max_lifetime_secs(" 5 "), Some(Duration::from_secs(5)));
        // Unset/empty/zero/garbage → no cap.
        assert_eq!(parse_max_lifetime_secs(""), None);
        assert_eq!(parse_max_lifetime_secs("0"), None);
        assert_eq!(parse_max_lifetime_secs("-1"), None);
        assert_eq!(parse_max_lifetime_secs("abc"), None);
    }

    #[test]
    fn should_exit_orphaned_when_reparented_to_init_or_changed() {
        let original = 4242;
        // Reparented to init (pid 1) → orphaned.
        assert!(should_exit_orphaned(original, 1));
        // Parent changed to some other pid (sub-reaper) → orphaned.
        assert!(should_exit_orphaned(original, 9999));
        // Same original parent still alive → not orphaned.
        assert!(!should_exit_orphaned(original, original));
    }

    #[test]
    fn should_exit_orphaned_handles_init_originated_daemon() {
        // A daemon whose original parent was already init (detached) and stays
        // there: current == original == 1. The `== 1` clause reports orphaned,
        // which is WHY the watchdog must be left OFF for detached production /
        // TuiDeck daemons — only the harness's non-detached daemons enable it.
        assert!(should_exit_orphaned(1, 1));
    }
}

#[cfg(test)]
mod hook_ingestion_tests {
    use super::*;
    use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
    use crate::event::AgentType;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    /// Scenario: the "No agent on reconnect" fix at the daemon layer. Spawn a
    /// shell agent (so the spawn-time `from_command` guess is `None` — the
    /// "No agent" state), run the real `run_hook_loop` against a temp hook
    /// socket, then write a synthetic Claude Code `SessionStart` line tagged
    /// with that pane's id. The loop must persist the event's `agent_type`
    /// into the PTY registry, so a subsequent `list_agents` / `agent_records`
    /// (what a fresh `dot-agent-deck connect` reads) reports `ClaudeCode`
    /// instead of "No agent". No real LLM tokens — the event is injected
    /// directly onto the ingestion socket.
    #[tokio::test]
    async fn run_hook_loop_persists_agent_type_into_registry() {
        let registry = Arc::new(AgentPtyRegistry::new());
        registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-it".to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        // Spawn-time guess is None — the bug's starting state.
        assert_eq!(registry.agent_records()[0].agent_type, None);

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hook.sock");
        let listener = bind_socket(&sock).expect("bind hook socket");
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let shutdown = Arc::new(Notify::new());

        let handle = tokio::spawn({
            let registry = registry.clone();
            async move { run_hook_loop(listener, state, event_tx, registry, shutdown).await }
        });

        // Synthetic SessionStart for the shell pane, carrying the real type.
        let event = serde_json::json!({
            "session_id": "it-sess",
            "agent_type": "claude_code",
            "event_type": "session_start",
            "timestamp": "2026-06-20T12:00:00Z",
            "pane_id": "pane-it",
        });
        let mut stream = UnixStream::connect(&sock)
            .await
            .expect("connect hook socket");
        stream
            .write_all(format!("{event}\n").as_bytes())
            .await
            .expect("write hook line");
        stream.flush().await.unwrap();

        // Ingestion is async — poll the registry until the type lands,
        // bounded so a regression (type never persisted) fails fast.
        let mut learned = None;
        for _ in 0..40 {
            if let Some(rec) = registry.agent_records().into_iter().next()
                && rec.agent_type.is_some()
            {
                learned = rec.agent_type;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            learned,
            Some(AgentType::ClaudeCode),
            "hook-ingested agent_type must be persisted into the registry so \
             a fresh connect reports the real agent instead of \"No agent\""
        );

        handle.abort();
        registry.shutdown_all();
    }
}
