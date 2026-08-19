use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{Notify, broadcast};
use tracing::{debug, error, info, warn};

use crate::platform::ipc::{IpcListener, IpcStream};

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

/// Exit status when a SECOND termination signal cuts a graceful shutdown short.
/// `128 + SIGTERM(15)`, the shell convention for "died on signal 15", so a
/// supervisor or script reads it the same way it read the pre-handler behaviour.
const EXIT_FORCED_BY_SECOND_SIGNAL: i32 = 143;

/// Spawn the production termination-signal watch, routing SIGTERM/SIGINT into
/// the shared `shutdown` notify so a stop reuses the ONE audited teardown path
/// (sockets unlinked, `AgentPtyRegistry` dropped, tasks aborted).
///
/// Why this exists: `daemon stop` / `daemon restart` terminate the daemon with
/// SIGTERM (see [`crate::daemon_stop`]), and the build-version handshake
/// SIGTERMs it silently on the no-agents path. With no handler installed the
/// default disposition applied — the process died instantly, so its owned
/// agents died by PTY hangup instead of an orderly registry teardown and,
/// worst of all, **nothing was logged**. A daemon that vanished mid-session
/// left no evidence of whether it was stopped, crashed, or was OOM-killed;
/// reconstructing one real incident took kernel logs and an external watchdog
/// to establish something the daemon itself should have said in one line.
///
/// Logged at `warn!` (not `info!`) for the same reason the give-up warnings in
/// `embedded_pane` are: losing the daemon terminates every managed agent, so
/// it is a user-visible outcome that must survive a default log filter.
fn spawn_termination_signal_watch(
    shutdown: Arc<Notify>,
    registry: Arc<AgentPtyRegistry>,
) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // Registering can only fail if the runtime can't install the handler
        // (no signal driver). That is not fatal — the daemon simply keeps the
        // pre-existing default disposition — so log and carry on.
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "could not install SIGTERM handler; termination will not be logged");
                return None;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "could not install SIGINT handler; termination will not be logged");
                return None;
            }
        };
        Some(tokio::spawn(async move {
            let sig = tokio::select! {
                _ = sigterm.recv() => "SIGTERM",
                _ = sigint.recv() => "SIGINT",
            };
            warn!(
                signal = sig,
                "daemon received termination signal; initiating graceful shutdown \
                 (every managed agent will be stopped)"
            );

            // Drain managed agents with the SAME grace the `KIND_SHUTDOWN`
            // handler gives them, BEFORE releasing the hook loop. Notifying
            // `shutdown` alone is not enough: the loop returns, `run_daemon_with`
            // drops the registry, and `Drop` calls `shutdown_all` — the
            // SIGKILL-WITHOUT-grace path, which `shutdown_all_graceful`'s own docs
            // scope to "idle shutdown and test cleanup". Idle shutdown only fires
            // with no agents left, so force-killing there costs nothing; a signal
            // is a DELIBERATE stop that routinely lands on live agents, so it
            // belongs on the graceful path. (Greptile P1 on the first draft, which
            // notified and returned — agents lost the grace this change promised.)
            //
            // `spawn_blocking` mirrors `daemon_protocol`'s KIND_SHUTDOWN arm: the
            // drain blocks while it polls for each child to exit. Idempotent via
            // the registry's `shutting_down` latch, whose docs already anticipate
            // "a SIGTERM landing during shutdown".
            let draining = registry.clone();
            let _ = tokio::task::spawn_blocking(move || {
                draining.shutdown_all_graceful(crate::agent_pty::AGENT_TERMINATE_GRACE);
            })
            .await;
            shutdown.notify_one();

            // Escape hatch, and the reason this task keeps waiting instead of
            // returning here. Installing a handler REPLACES the default
            // disposition for the life of the process: once the first signal is
            // consumed, tokio's handler stays installed, so every later SIGTERM
            // would be quietly swallowed by a stream nobody reads. Before this
            // change SIGTERM always killed the daemon outright, so a wedged
            // shutdown could still be ended with `pkill dot-agent-deck` — which
            // sends SIGTERM by default and is the escape hatch the in-repo audit
            // notes call the only way to stop a daemon. A second signal
            // therefore force-exits, preserving that. A second signal arriving
            // DURING the drain above is buffered by tokio's signal stream and
            // handled as soon as the drain returns, so the hatch is delayed by at
            // most `AGENT_TERMINATE_GRACE`, never lost.
            let again = tokio::select! {
                _ = sigterm.recv() => "SIGTERM",
                _ = sigint.recv() => "SIGINT",
            };
            warn!(
                signal = again,
                "second termination signal while shutting down; exiting immediately \
                 without finishing teardown"
            );
            std::process::exit(EXIT_FORCED_BY_SECOND_SIGNAL);
        }))
    }
    #[cfg(windows)]
    {
        Some(tokio::spawn(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                warn!(error = %e, "could not await Ctrl-C; termination will not be logged");
                return;
            }
            warn!(
                signal = "CTRL_C",
                "daemon received termination signal; initiating graceful shutdown \
                 (every managed agent will be stopped)"
            );
            // Same graceful drain as the Unix arm above; see its comment.
            let draining = registry.clone();
            let _ = tokio::task::spawn_blocking(move || {
                draining.shutdown_all_graceful(crate::agent_pty::AGENT_TERMINATE_GRACE);
            })
            .await;
            shutdown.notify_one();

            // Same second-signal escape hatch as the Unix arm above.
            if tokio::signal::ctrl_c().await.is_ok() {
                warn!(
                    signal = "CTRL_C",
                    "second termination signal while shutting down; exiting immediately \
                     without finishing teardown"
                );
                std::process::exit(EXIT_FORCED_BY_SECOND_SIGNAL);
            }
        }))
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
/// PRD #163 M1: the platform tail (the `XDG_RUNTIME_DIR`-then-`~/.cache` chain
/// above, `%LOCALAPPDATA%\dot-agent-deck\locks` on Windows) lives in
/// [`crate::platform::paths::lock_root_default`]; Unix resolution is unchanged.
/// Both overrides below are still checked FIRST, so they stay authoritative.
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
    crate::platform::paths::lock_root_default()
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
///
/// PRD #42 M2: the transport is abstracted behind
/// [`crate::platform::ipc::IpcStream`]; on Unix this is the same
/// `UnixStream::connect` liveness probe, unchanged.
async fn probe_socket_alive(path: &Path) -> bool {
    IpcStream::connect(path).await.is_ok()
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
    /// PRD #120 M2.4: in-memory map of dispatched issue-agent id → its per-issue
    /// worktree. The issue-dispatch fire flow records each spawned pane here;
    /// the attach server's `StopAgent` handler consults it on close so the
    /// worktree is `git worktree remove`d (the clone is preserved). Shared
    /// between the scheduler callback factory and the attach server; wiped on
    /// restart (the worktree-exists idempotency signal reclaims entries).
    pub worktree_registry: crate::issue_dispatch_run::WorktreeRegistry,
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
            worktree_registry: crate::issue_dispatch_run::new_worktree_registry(),
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
            worktree_registry: crate::issue_dispatch_run::new_worktree_registry(),
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
        crate::platform::fsperm::ensure_owner_only_dir(parent)?;
    }
    let _start_lock = crate::platform::lock::acquire_spawn_lock(&lock_path).await?;

    // PRD #163 M4: the probe-remove-bind dance above is inherently about a
    // *filesystem* endpoint. On Windows the endpoint is a `\\.\pipe\` name with no
    // inode: `exists()` is permanently false and `remove_file` would error rather
    // than clear anything, so `stale_endpoint_artifact` short-circuits the whole
    // block. Nothing is lost — the singleton guard there is
    // `first_pipe_instance(true)` inside `IpcListener::bind`, which reports
    // `AddrInUse` for exactly the case this branch exists to catch.
    if crate::platform::ipc::stale_endpoint_artifact(socket_path) {
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

    // PRD #42 M2: `IpcListener::bind` performs the umask-before-bind dance and
    // the defense-in-depth 0o600 restate (both folded in from the former
    // `bind_socket` + post-bind `set_permissions`), so the socket inode is
    // owner-only exactly as before.
    let listener = IpcListener::bind(socket_path)?;
    // Lock has done its job: subsequent starters' probe-connect will now
    // succeed against this listener and return AddrInUse without needing
    // to contend on the lock. Dropping releases the flock and closes the
    // fd; the `.lock` file itself stays on disk (cheap, empty, reused on
    // next start).
    drop(_start_lock);
    info!("Daemon listening on {}", socket_path.display());

    // Hold the registry for the lifetime of the loop so its Drop fires
    // (killing any owned agents) when this future is dropped/aborted.
    let pty_registry = daemon.pty_registry;
    // Tell the registry which hook endpoint we just bound, so every agent it
    // spawns is handed that path explicitly instead of re-resolving it from
    // inherited environment when it emits. See `DOT_AGENT_DECK_SOCKET`.
    pty_registry.set_hook_socket(socket_path.to_path_buf());
    let state = daemon.state;
    // Issue #454: teach this daemon's `AppState` to resolve "do I own the agent
    // this event names?" against the registry rather than against a set it
    // would have to maintain by hand — see `crate::state::AgentOwnership`.
    // Installed here so a HOOK-ONLY daemon (no attach server, `Daemon::new`)
    // gets it too; `serve_attach_with_counter` installs the same registry again
    // for the harnesses that serve the attach protocol without this function.
    //
    // WEAKLY, and that is load-bearing (round-2 reviewer blocker A): the
    // registry owns the delivery-notice sink installed a few lines below, whose
    // closure holds a strong `SharedState`. A strong reference from `AppState`
    // back to the registry closes the cycle
    // `AppState -> AgentPtyRegistry -> sink -> SharedState -> AppState`, and the
    // `drop(pty_registry)` at the end of this function then releases nothing —
    // so `AgentPtyRegistry::drop`, the RAII teardown that kills this daemon's
    // PTYs when the task is aborted or an accept loop errors out, never runs.
    // `pty_registry` below is the strong reference that keeps the oracle
    // answerable, and it lives exactly as long as this daemon does.
    {
        let ownership: Arc<dyn crate::state::AgentOwnership> = pty_registry.clone();
        state
            .write()
            .await
            .set_agent_ownership(Arc::downgrade(&ownership));
    }
    let event_tx = daemon.event_tx;
    // Issue #424: give the spawn-time delivery path a way to REPORT a failed
    // delivery as state on the pane's card instead of typing a diagnostic line
    // into the agent's input buffer. See `install_delivery_notice_sink`.
    install_delivery_notice_sink(&pty_registry, state.clone(), event_tx.clone());
    let client_count = daemon.client_count;
    let idle_shutdown = daemon.idle_shutdown;
    let scheduler = daemon.scheduler;
    let reuse_registry = daemon.reuse_registry;
    let worktree_registry = daemon.worktree_registry;

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
                worktree_registry.clone(),
                event_tx.clone(),
                state.clone(),
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

    // Production termination watch: route SIGTERM/SIGINT through the same
    // `shutdown` notify. Armed unconditionally — unlike the two backstops
    // below, this is not test-only: `daemon stop` IS a SIGTERM.
    let signal_handle = spawn_termination_signal_watch(shutdown.clone(), pty_registry.clone());

    // Test-only orphan watchdog: when `DOT_AGENT_DECK_EXIT_WHEN_ORPHANED` is
    // truthy, gracefully shut down (via the SAME `shutdown` signal the idle
    // monitor uses — so sockets/agents tear down cleanly) once this daemon is
    // orphaned. OFF by default; production daemons never set the var.
    let orphan_handle = if std::env::var(DOT_AGENT_DECK_EXIT_WHEN_ORPHANED)
        .map(|v| parse_bool_flag(&v))
        .unwrap_or(false)
    {
        let original_ppid = crate::platform::proc::current_ppid();
        let shutdown_signal = shutdown.clone();
        info!(
            original_ppid,
            "exit-when-orphaned watchdog armed (test-only safety net)"
        );
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let cur = crate::platform::proc::current_ppid();
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
        let attach_worktrees = worktree_registry.clone();
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
                attach_worktrees,
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

    // PRD #370 M2: unconditional, unlike the idle monitor above — every
    // daemon needs this regardless of the idle-shutdown config, since it's
    // the only signal source for a role's shelled-out foreground command.
    let shell_activity_handle = {
        let registry = pty_registry.clone();
        let monitor_state = state.clone();
        let monitor_event_tx = event_tx.clone();
        tokio::spawn(async move {
            run_shell_activity_monitor(registry, monitor_state, monitor_event_tx).await;
        })
    };

    let result = run_hook_loop(
        listener,
        state,
        event_tx,
        pty_registry.clone(),
        shutdown,
        worktree_registry.clone(),
    )
    .await;

    if let Some(h) = attach_handle {
        h.abort();
    }
    if let Some(h) = idle_handle {
        h.abort();
    }
    shell_activity_handle.abort();
    scheduler_handle.abort();
    if let Some(h) = orphan_handle {
        h.abort();
    }
    if let Some(h) = max_lifetime_handle {
        h.abort();
    }
    if let Some(h) = signal_handle {
        h.abort();
    }
    // Issue #424 (reviewer finding B9): a spawn-time prompt's confirmation loop
    // must not outlive the daemon that owns the PTY it re-submits into. The loop
    // also ends on its own when the broadcast sender drops (`PromptWatch::Closed`),
    // but that is a race against the next backoff window expiring, and this is
    // the deterministic half.
    crate::spawn::cancel_all_prompt_confirmations();
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
    worktrees: crate::issue_dispatch_run::WorktreeRegistry,
    event_tx: broadcast::Sender<BroadcastMsg>,
    state: crate::state::SharedState,
) -> impl FnMut(&crate::config::ScheduledTask) -> crate::scheduler::Callback {
    move |task| {
        make_schedule_callback(
            task,
            registry.clone(),
            reuse.clone(),
            worktrees.clone(),
            event_tx.clone(),
            state.clone(),
        )
    }
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
    worktrees: crate::issue_dispatch_run::WorktreeRegistry,
    event_tx: broadcast::Sender<BroadcastMsg>,
    // So a scheduled fire that opens an ORCHESTRATION registers its roles for
    // delegate routing, exactly as an interactive or dispatched one does.
    state: crate::state::SharedState,
) -> crate::scheduler::Callback {
    // PRD #120: an `issue_dispatch` task runs the GitHub-dispatch FLOW instead of
    // the single #127 spawn — enumerate the repo's open issues and dispatch one
    // agent per issue into a per-issue worktree (composing the existing spawn
    // primitive + the pure `crate::issue_dispatch` helpers). The presence of the
    // `issue_dispatch` sub-table is the task-type discriminator.
    if let Some(cfg) = task.issue_dispatch.clone() {
        let task_name = task.name.clone();
        let working_dir = task.working_dir.clone();
        let prompt_template = task.prompt.clone();
        // An explicit `command` on the task wins; otherwise a single-agent
        // dispatch (a clone with no orchestration config) falls back to the
        // global `default_command`. Orchestration clones ignore this entirely.
        let task_command = task.command.clone();
        return Arc::new(move || {
            let registry = registry.clone();
            let worktrees = worktrees.clone();
            let event_tx = event_tx.clone();
            let task_name = task_name.clone();
            let working_dir = working_dir.clone();
            let prompt_template = prompt_template.clone();
            let cfg = cfg.clone();
            let task_command = task_command.clone();
            let state = state.clone();
            Box::pin(async move {
                let notifier = crate::scheduler::StderrNotifier;
                // PRD #120 (flag redesign 2026-06-24): a configured `issue_dispatch`
                // task runs UNCONDITIONALLY — the `experimental` flag no longer gates
                // the dispatch behavior, only the new-pane modal creation option
                // (a render-seam presentation switch; see `features::show_*`). The
                // task-type discriminator is purely the presence of the
                // `issue_dispatch` sub-table. (#127's non-issue_dispatch spawn path
                // below is untouched.)
                let default_command = task_command.or_else(|| {
                    let dc = crate::config::DashboardConfig::load().default_command;
                    let dc = dc.trim().to_string();
                    if dc.is_empty() { None } else { Some(dc) }
                });
                crate::issue_dispatch_run::run_issue_dispatch(
                    &task_name,
                    &working_dir,
                    &prompt_template,
                    &cfg,
                    default_command,
                    &registry,
                    &worktrees,
                    &notifier,
                    Some(&event_tx),
                    Some(&state),
                )
                .await;
            })
        });
    }

    let req = crate::spawn::SpawnRequest {
        task_name: task.name.clone(),
        working_dir: task.working_dir.clone(),
        command: task.command.clone(),
        prompt: task.prompt.clone(),
        // `None`: a scheduled task's shape still comes from its working dir's
        // config. The PRD #220 selector is a `dispatch`-only surface.
        resolved_target: None,
        // Unchanged behaviour: the prompt is delivered verbatim. Giving this path
        // the orchestrator context is #222's work, not this PR's.
        compose_orchestrator_context: false,
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
        let state = state.clone();
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
                Some(&state),
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

/// PRD #386 (reviewer finding): ingest one event as a SINGLE ordered daemon
/// operation — fan it out to attached clients and apply it to the daemon's
/// own `AppState` under one write-lock acquisition, so every consumer
/// observes events in the order the daemon applied them.
///
/// **Why it has to be one step.** Both producers — the shell-activity monitor
/// above and the hook loop below — used to `send` and then *separately*
/// `await` the state write lock, with the await sitting between the two. Two
/// concurrent producers could therefore interleave: the monitor broadcasts
/// `ShellBusy` and yields at `state.write().await`, a hook connection
/// broadcasts `Idle` and wins the lock, and the daemon applies `Idle` then
/// `ShellBusy` — ending at `Working` — while an attached TUI consumed
/// `ShellBusy` then `Idle` and renders `Idle`. Nothing corrects it
/// afterwards, which is what makes it worth fixing rather than tolerating:
/// the monitor's level-aware re-emit (see `run_shell_activity_monitor`)
/// tests the DAEMON's status, which is already `Working`, so no further
/// event is ever synthesized and the pane the user is looking at stays wrong
/// until the next unrelated edge. That is the same user-visible failure this
/// PRD exists to repair, and the same shape as the mis-addressed synthesized
/// event fixed in the monitor.
///
/// The non-atomicity is **pre-existing** — it is how the pipeline already
/// handled any two concurrent events, including two real hooks arriving on
/// separate connections. What #386 changes is how often the window is
/// reachable, by adding a second, timer-driven producer that emits precisely
/// when a real `Stop`-driven `Idle` is in flight.
///
/// Holding the guard across `send` is safe and adds no blocking:
/// `broadcast::Sender::send` is synchronous, never waits on a receiver, and
/// errs only when there are no subscribers (the expected standalone-daemon
/// case). The property the old fan-out-before-apply comments protected —
/// that the broadcast happens whether or not the local `apply_event` accepts
/// the event, e.g. for an unmanaged pane id — is unchanged: both run under
/// the same guard, unconditionally.
async fn ingest_event(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    event: AgentEvent,
) {
    let mut state = state.write().await;
    let _ = event_tx.send(BroadcastMsg::Event(event.clone()));
    state.apply_event(event);
}

/// Issue #424 (reviewer blocker 3): teach the registry how to turn a
/// [`DeliveryNotice`] into durable, client-visible STATE.
///
/// The spawn-time delivery path runs deep inside `crate::spawn` with only an
/// `AgentPtyRegistry` in hand, so the daemon installs the ability rather than
/// the path reaching for it. What lands is ONE synthetic `AgentEvent` pushed
/// through [`ingest_event`] — the same single ordered operation every real hook
/// event takes — so the daemon's own `AppState` records it (a client attaching
/// later still sees it) and every attached client renders it live. The card's
/// status becomes `Error`; the delivery id stays in the log, where the detail
/// belongs.
///
/// Five properties are deliberate:
///
/// * **Identity is re-validated AT INGESTION** (issue #424 D3, both reviewers).
///   `publish_delivery_notice` checks that the delivery's agent still owns the
///   pane, but that check happens before an asynchronous handoff: the sink
///   schedules a detached task which reads state later and ingests later still.
///   A pane closed and rebound inside that window used to receive the
///   predecessor's report anyway — and because the stale `Error` carries the
///   PREDECESSOR agent id with a CURRENT timestamp, `apply_event` could read it
///   as a superseding generation, retire the successor's card and recreate
///   predecessor state under it. So the registry owner is re-checked here, and
///   the whole check → build → broadcast → apply sequence runs under ONE held
///   write lock, which is also what closes the second (read-to-ingest) race: the
///   session id the event is stamped with can no longer be resolved from a
///   snapshot that a genuine `SessionStart` invalidates before the apply.
/// * **A same-agent conversation successor is refused too.** The registry id
///   survives a `/clear`, so identity alone would let a predecessor delivery's
///   late report mark a successor conversation's card. A notice that names the
///   generation it was written for is dropped unless that generation is still
///   current; one that names none (an unbound delivery on a launcher pane)
///   carries no such constraint because there is nothing to compare.
/// * **The event never moves the pane's GENERATION.** Not by construction from
///   the stamped id — that was the old argument, and it was wrong in the
///   read-to-ingest race — but because it is applied through
///   [`AppState::apply_daemon_report_event`], which snapshots and restores the
///   pane's generation entry around the apply. It cannot advance it, cannot roll
///   it back, and cannot establish one on a placeholder-only pane.
/// * **It addresses the card the CLIENTS have, not only the one the daemon has**
///   (issue #424 F5). A scheduled/dispatch pane is surfaced to attached TUIs by
///   `crate::spawn::surface_spawned_pane` through the event broadcast alone, so
///   the daemon can legitimately hold no `pane_hook_session` and no `sessions`
///   entry for a pane every attached client is rendering. When neither resolves,
///   the report is addressed by PANE ID — the `session_id`
///   `surface_spawned_pane` stamps on that card — instead of being dropped to
///   the log. It is still never a card for an UNKNOWN pane: the registry
///   ownership re-check above has already proved the pane is live and belongs to
///   this delivery's agent.
/// * **It carries the registry `agent_id`**, so `apply_event`'s reuse guard
///   lands it on that agent's existing card instead of creating a sibling.
///
/// The registry is captured WEAKLY: it owns the sink, so an `Arc` here would be a
/// reference cycle that keeps the registry (and every PTY it holds) alive for the
/// process's lifetime.
fn install_delivery_notice_sink(
    registry: &Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
) {
    let weak_registry = Arc::downgrade(registry);
    registry.set_delivery_notice_sink(std::sync::Arc::new(move |notice| {
        let state = state.clone();
        let event_tx = event_tx.clone();
        let registry = weak_registry.clone();
        tokio::spawn(async move {
            // ONE write lock for the whole operation: re-validate, resolve
            // the target card, broadcast, apply. Nothing sampled here can go
            // stale before the event lands, which is the difference between
            // this and the read-then-ingest version it replaces.
            let mut guard = state.write().await;
            let Some(registry) = registry.upgrade() else {
                return;
            };
            if registry.pane_current_agent_id(&notice.pane_id).as_deref()
                != Some(notice.agent_id.as_str())
            {
                tracing::debug!(
                    pane_id = %notice.pane_id,
                    delivery_id = %notice.delivery_id,
                    "delivery notice dropped at ingestion; the pane no longer \
                     belongs to this agent"
                );
                return;
            }
            let current_generation = guard.pane_hook_session_id(&notice.pane_id);
            if let Some(bound) = notice.session_id.as_deref()
                && current_generation.as_deref() != Some(bound)
            {
                tracing::debug!(
                    pane_id = %notice.pane_id,
                    delivery_id = %notice.delivery_id,
                    "delivery notice dropped at ingestion; the conversation it \
                     was written for is no longer current"
                );
                return;
            }
            let session_id = current_generation
                .or_else(|| {
                    guard
                        .sessions
                        .values()
                        .find(|session| session.pane_id.as_deref() == Some(&notice.pane_id))
                        .map(|session| session.session_id.clone())
                })
                // Issue #424 F5 (reviewer blocker): a fresh hookless scheduled /
                // dispatch pane has a card in every ATTACHED client and none in
                // the daemon's own `AppState`, because
                // `crate::spawn::surface_spawned_pane` publishes it through the
                // event broadcast ONLY and never applies it locally. Resolving
                // solely from daemon state therefore took the log-only branch for
                // exactly the population that fills the 256-task cap — hookless
                // confirmations hold their slots the full deadline — so the one
                // delivery that most needed the report was the one that could not
                // receive it, and under the default no-subscriber logging setup
                // the visible card stayed clean.
                //
                // The card those clients are showing is identified by the PANE
                // ID: that is the `session_id` `surface_spawned_pane` stamps. So
                // that is what the report is addressed to. This does not weaken
                // the "never mint a card for an unknown pane" property it
                // replaces — the immediately preceding check has already proved
                // this pane is a live registry pane owned by this exact delivery's
                // agent — and applying it locally as well keeps the daemon's own
                // state consistent with what it just told every client, so a
                // client attaching afterwards sees the failure too.
                .unwrap_or_else(|| {
                    tracing::debug!(
                        pane_id = %notice.pane_id,
                        delivery_id = %notice.delivery_id,
                        "delivery notice has no daemon-side card; addressing the \
                         broadcast-surfaced card by pane id"
                    );
                    notice.pane_id.clone()
                });
            let mut metadata = std::collections::HashMap::new();
            metadata.insert(
                crate::event::DELIVERY_NOTICE_METADATA_KEY.to_string(),
                notice.detail.to_string(),
            );
            let event = AgentEvent {
                session_id,
                // The daemon is not the agent, and must not claim to be one:
                // `apply_event` only fills a session's type when it is still
                // unknown, so `None` cannot overwrite a real agent type.
                agent_type: crate::event::AgentType::None,
                event_type: crate::event::EventType::Error,
                tool_name: None,
                tool_detail: Some(notice.detail.to_string()),
                cwd: None,
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata,
                pane_id: Some(notice.pane_id.clone()),
                agent_id: Some(notice.agent_id.clone()),
                agent_version: None,
                schema_version: None,
                live_target: None,
            };
            let _ = event_tx.send(BroadcastMsg::Event(event.clone()));
            guard.apply_daemon_report_event(event);
        });
    }));
}

/// PRD #370 M2 / PRD #386 M3: periodically scans every live pane's PTY child
/// for a transitive descendant detached into a POSIX session of its own — see
/// [`crate::agent_pty::RunningAgent::shell_foreground_busy`] — and
/// synthesizes `ShellBusy`/`ShellIdle` events through the SAME pipeline real
/// hook events use (`event_tx` broadcast + `AppState::apply_event`), so a
/// pane running a foreground shell command (e.g. a role's `cargo build`)
/// reads `Working` even when no agent-emitted hook/wrapper event fires in
/// between. Per pane the trigger is edge-driven (a busy/idle transition) PLUS
/// level-aware (PRD #386 M6b — the scan reads busy while the session's status
/// has regressed to `Idle`/`Unknown`, as it does when Claude Code backgrounds
/// a command at its 120s Bash cap and the resulting `Stop` hook lands as
/// `Idle`), never every tick, so this never floods attached clients with
/// redundant events; `apply_event`'s own precedence rules (see its
/// `ShellBusy`/`ShellIdle` arms) are what keep it from ever clobbering a real
/// status.
///
/// Skips any pane with no already-known session
/// (`AppState::pane_hook_session_id` returns `None`) — a bare shell pane
/// that has never emitted a single agent event has no `SessionState` to
/// update at all. Documented M2 scope boundary (PRD #370), not a bug: this
/// mechanism promotes an agent's OWN idle gaps, not a shell nobody's
/// tracking. No internal shutdown signal — like `scheduler_handle`, this
/// task is torn down by `.abort()` in `run_daemon_with`'s cleanup.
async fn run_shell_activity_monitor(
    pty_registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
) {
    run_shell_activity_monitor_with(pty_registry, state, event_tx, || {
        crate::platform::proc::process_table_async()
    })
    .await
}

/// [`run_shell_activity_monitor`] with the process-table sample injected, so the
/// two decisions that are *about* sampling can be tested without a wedged
/// filesystem or an empty machine (issues #493 and #429): that no sample is
/// taken at all when there is no live pane, and that a sample which blows its
/// deadline leaves every pane's status alone.
///
/// `sample` is deliberately **not** given the timeout — the deadline is applied
/// here, around whatever the sampler returns, so a test sampler that never
/// completes exercises the real timeout path rather than a stubbed one. That
/// also lets a test count how many samples were *started*, which is what pins
/// the one-child-at-a-time invariant described on `inflight` below.
async fn run_shell_activity_monitor_with<S, F>(
    pty_registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    sample: S,
) where
    S: Fn() -> F,
    F: std::future::Future<Output = Option<Vec<crate::platform::proc::ProcessInfo>>>,
{
    // PRD #370 Open Question (poll cadence): 500ms is a first-cut balance
    // between feeling responsive and negligible overhead (one registry lock
    // + one `ps -A` sample per tick, reused across every live pane, plus a
    // `getsid` per row). PRD #386 M5 is where that cost gets measured and the
    // cadence confirmed or revised; left unchanged here deliberately, so M5
    // measures the shape that actually shipped.
    //
    // Issue #493 measured the sample it pays for: ~49ms of wall time per `ps -A`
    // on an idle 16-core Linux box with ~620 processes (release build), i.e.
    // ~10% of one core at 2Hz — of which only ~1.4ms is this process's own CPU
    // (the `getsid` loop plus parsing); the rest is the `ps` child and waiting
    // on it. That is the number M5 wants for the Route A vs. Route B (native
    // enumeration) question, and it is why skipping the sample when no pane
    // needs it is worth the guard below rather than merely tidy.
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    // Issue #429: an upper bound on how long a tick WAITS for a sample —
    // deliberately not a bound on how long the sample's child may live (see
    // `inflight` below). Generous next to the ~49ms a healthy `ps -A` takes, so
    // ordinary load never trips it.
    const SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);
    let mut last_known: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    // The sample still in flight from an earlier tick, if any (#500 review, P1).
    //
    // This exists so a wedged `ps` cannot ACCUMULATE. The obvious shape —
    // `timeout(d, sample())`, drop the future on expiry, start a fresh one next
    // tick — is wrong for the exact case the timeout is for: a process in
    // uninterruptible sleep does not act on the `SIGKILL` that `kill_on_drop`
    // sends until it leaves D-state, so the abandoned `ps` stays on the process
    // table. Retrying every 2.5s would then add one undead `ps` per cycle
    // (~24/minute), consuming pids and table entries — turning a stalled signal
    // into a resource leak.
    //
    // So the timeout bounds the WAIT, not the child: on expiry the in-flight
    // future is retained here and re-awaited on the next tick, which keeps the
    // hard invariant that **at most one `ps` child exists at a time**. A sample
    // that answers `None` is dropped, so the next tick starts fresh.
    //
    // Retention is unconditional — including on a tick with no candidates, which
    // does not poll it. Dropping it there would look tidier but reopens the
    // accumulation path above through pane churn (close to zero, reopen, and a
    // second `ps` joins the first undead one), and #493 already guarantees a
    // paneless daemon starts no sample at all. The residual is one retained
    // child, which is the same child we would be waiting on anyway.
    //
    // The `Instant` is the sample's START, and it is what makes retention safe
    // (#500 review, round 2). A retained sample's table describes the machine as
    // it was when `ps` began, so a sample that finally answers after an arbitrary
    // gap — a wedge that outlasts every pane, then a new pane opening — would
    // classify TODAY's pids against a table from before they existed. Almost
    // always that is harmless (`descendant_shell_activity` returns `None` for a
    // pid the table lacks, so the pane is skipped), but under pid reuse a new
    // pane inherits a dead process's descendants and, because `last_known` has no
    // entry for it yet, that wrong reading emits immediately. So a table older
    // than `MAX_TABLE_AGE` is discarded rather than trusted.
    // Carries the candidate set as it was when the sample STARTED alongside it,
    // so a late answer can be checked against the panes it could actually have
    // observed rather than against whatever is open when it lands — see the
    // `was_resumed` branch below.
    #[allow(clippy::type_complexity)]
    let mut inflight: Option<(
        tokio::time::Instant,
        Vec<crate::agent_pty::ShellActivityCandidate>,
        std::pin::Pin<Box<F>>,
    )> = None;
    // How old a sample's table may be and still be worth classifying against.
    // A healthy sample answers in ~49ms and a heavily loaded one in a few
    // hundred, so this never trips in normal operation; it exists purely to stop
    // a long-overrunning sample's answer from being applied to a machine that has
    // moved on. Discarding an ANSWERED sample is free — that child is already
    // finished, so unlike abandoning an un-answered one it cannot accumulate.
    const MAX_TABLE_AGE: Duration = Duration::from_secs(3);
    // Whether the in-flight sample has already been reported as overrunning, so
    // a permanently-wedged `ps` logs once rather than every 2.5s forever.
    let mut inflight_reported = false;

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        // PRD #386 M3: the CATALOG of measured shapes, not a set applied to
        // every pane — `shell_activity_candidates` selects from it per pane by
        // agent kind, so a Claude pane gets the one shape measured against
        // Claude Code and an agent whose shell-tool shape has never been
        // measured gets none (structural session-id test alone). Passing
        // Claude's fingerprint to a Codex/OpenCode/Pi pane would veto a
        // genuinely detached descendant and leave the pane silently reading
        // `Idle`.
        //
        // Issue #493: resolve WHO there is to classify before sampling the
        // machine. This lock-only pass is what makes the `ps` fork conditional
        // — it used to be unconditional and first, so a daemon with zero panes
        // forked `ps -A` twice a second to classify nobody, and the daemon's
        // idle shutdown does not bound that (it needs no clients AND no agents,
        // so a TUI attached with no panes polled forever). The candidates are
        // owned, so the registry lock is already released here and the sample
        // below still never runs under it.
        let candidates = pty_registry
            .shell_activity_candidates(crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES);

        let snapshot = if candidates.is_empty() {
            // No pane, so no sample — and an empty reading rather than a
            // skipped tick, because "there are no panes" is a fact we just
            // established under the lock, not a failure to observe. Falling
            // through with an empty snapshot lets the `retain` below clear
            // `last_known`, which is what makes a later reuse of the same pane
            // id start edge-detection from a clean slate.
            Vec::new()
        } else {
            // Resume the sample already in flight, or start the tick's own. Only
            // ever one of the two, which is what bounds the `ps` children to one
            // — see `inflight`'s declaration.
            let resumed = inflight.take();
            let was_resumed = resumed.is_some();
            let (started, at_start, mut pending) = match resumed {
                Some(resumed) => resumed,
                None => {
                    inflight_reported = false;
                    (
                        tokio::time::Instant::now(),
                        candidates.clone(),
                        Box::pin(sample()),
                    )
                }
            };
            match tokio::time::timeout(SAMPLE_TIMEOUT, pending.as_mut()).await {
                Ok(Some(table)) => {
                    // See `MAX_TABLE_AGE`: a sample that answered this late
                    // describes a machine that has since moved on. No opinion.
                    let age = started.elapsed();
                    if age > MAX_TABLE_AGE {
                        warn!(
                            age_ms = age.as_millis(),
                            max_age_ms = MAX_TABLE_AGE.as_millis(),
                            "shell-activity: discarding a process-table sample that answered too \
                             late to trust; leaving every pane's status alone (classifying current \
                             pids against a stale table can misattribute a reused pid)"
                        );
                        continue;
                    }
                    if was_resumed {
                        // A retained sample's table was taken when `at_start`
                        // was the truth, so a pid in it means what it meant
                        // THEN. `MAX_TABLE_AGE` bounds how far back that is, but
                        // a bound is not an identity check: a pane can be
                        // replaced inside the window, and if its shell's pid is
                        // reused the replacement would be classified by numeric
                        // pid alone against the departed pane's descendants —
                        // and since `last_known` has no entry for it, that wrong
                        // reading emits at once (#500 review, round 3).
                        //
                        // So classify only panes whose IDENTITY is unchanged
                        // since the sample began — same pane id AND same shell
                        // pid. A respawn in the same slot keeps the pane id but
                        // takes a new pid; a fresh pane brings a new pane id.
                        // Either way the pair differs and the pane is left to
                        // the next sample, which is the honest answer: this
                        // table predates it and cannot describe it.
                        //
                        // Only on the resumed path. A sample started this tick
                        // has `at_start == candidates` by construction, so the
                        // filter would be a no-op — the common case pays
                        // nothing.
                        let unchanged: Vec<crate::agent_pty::ShellActivityCandidate> = candidates
                            .iter()
                            .filter(|current| {
                                at_start.iter().any(|then| {
                                    then.pane_id == current.pane_id
                                        && then.shell_pid == current.shell_pid
                                })
                            })
                            .cloned()
                            .collect();
                        AgentPtyRegistry::classify_shell_activity(&unchanged, &table)
                    } else {
                        AgentPtyRegistry::classify_shell_activity(&candidates, &table)
                    }
                }
                // ── The load-bearing decision of issue #429 ──
                //
                // BOTH arms below mean "no opinion", and neither may become
                // `Some(false)`.
                //
                // `descendant_shell_activity` draws that distinction on purpose
                // and callers are documented to treat `None` as "leave the
                // pane's status alone". A timed-out sample is a statement about
                // `ps`, not about the pane: if a `ps` wedges in D-state on a
                // stuck filesystem, every pane is still exactly as busy as it
                // was a moment ago. Collapsing the timeout to "not busy" would
                // synthesize a `ShellIdle` for every pane the deck is running
                // and silently flip them all to `Idle` — which is precisely the
                // stale-`Idle` bug PRD #386 exists to fix, reintroduced with a
                // new trigger and no log line to find it by.
                //
                // So skip the whole tick: `last_known` is left untouched (a
                // `retain` against an empty snapshot would make every pane look
                // new next tick and re-emit a spurious edge for each one) and
                // nothing is emitted. The reading simply resumes on the next
                // sample that answers.
                //
                // The two arms differ only in what happens to the sample itself:
                // an answered-but-failed sample is finished, so it is dropped and
                // the next tick starts a fresh one; an overrunning sample is
                // RETAINED, so the next tick waits on the same `ps` instead of
                // spawning a second one.
                Ok(None) => continue,
                Err(_elapsed) => {
                    if !inflight_reported {
                        inflight_reported = true;
                        warn!(
                            timeout_ms = SAMPLE_TIMEOUT.as_millis(),
                            panes = candidates.len(),
                            "shell-activity: process-table sample overran its deadline; leaving \
                             every pane's status alone and continuing to wait on the SAME sample \
                             (a wedged `ps` says nothing about the panes, and starting another \
                             would only pile up unkillable children)"
                        );
                    }
                    inflight = Some((started, at_start, pending));
                    continue;
                }
            }
        };
        let seen: std::collections::HashSet<&str> = snapshot
            .iter()
            .map(|(pane_id, _)| pane_id.as_str())
            .collect();
        // Drop panes that disappeared from the registry since the last poll
        // (closed / respawned) so a later reuse of the same pane id starts
        // edge-detection from a clean slate instead of inheriting a stale
        // busy/idle reading.
        last_known.retain(|pane_id, _| seen.contains(pane_id.as_str()));

        for (pane_id, busy) in snapshot {
            let changed = last_known.insert(pane_id.clone(), busy) != Some(busy);

            // Cheap path, and the overwhelmingly common one: a pane whose scan
            // reads idle and whose reading did not just change has nothing to
            // report, so it never takes the state lock at all.
            if !changed && !busy {
                continue;
            }

            // PRD #386 M6b: the trigger is edge-driven PLUS level-aware. A
            // purely edge-triggered monitor emits exactly one `ShellBusy` per
            // busy window — at the rising edge — which is the whole reported
            // bug: Claude Code's Bash tool backgrounds a command at its 120s
            // cap, the agent ends its turn, the real `Stop` hook maps to
            // `EventType::Idle` (`src/hook.rs`) and knocks the pane back to
            // `Idle` while the command runs on. The scan still reads busy, but
            // it read busy *before* `Stop` too, so there is no new edge and
            // nothing ever re-promotes: measured at ~9.7 minutes of wrong
            // badge for a ~700s command.
            //
            // So also re-emit when the scan reads busy AND the session's
            // status has actually regressed to `Idle`/`Unknown`. That is a
            // monitor-side correction only — it adds no precedence rule and
            // changes no wire format; `apply_event`'s `ShellBusy` arm still
            // decides what (if anything) to promote, and still promotes
            // exactly `Idle`/`Unknown`, so a real `WaitingForInput`/`Error`/
            // `Thinking`/`Working` is never overridden by this signal.
            //
            // It cannot spam the pipeline either: the re-emit is conditioned
            // on the very status the emitted event corrects. One `ShellBusy`
            // lands, `apply_event` moves the session to `Working`, and the
            // next poll reads `Working` and sends nothing — a steady-state
            // busy pane is silent until something knocks it back to `Idle`
            // again.
            let (session_id, agent_id, status_regressed) = {
                let state = state.read().await;
                let Some(session_id) = state.pane_hook_session_id(&pane_id) else {
                    continue;
                };
                // PRD #386 M6b: the pane's CURRENT CARD —
                // NOT `sessions[session_id]`. Both values read below describe
                // the session this event will actually land on, and that is
                // the card, not the hook generation.
                //
                // `pane_hook_session_id` is the pane's latest hook GENERATION
                // and is the authoritative value for `AgentEvent.session_id`
                // (it is what the daemon's send guard compares against); it is
                // NOT a key into `sessions`. A same-agent `/clear` / thread
                // restart rolls that generation forward while `apply_event`'s
                // same-agent reuse guard deliberately keeps the CARD under its
                // stable id (see `AppState::apply_event`'s "ORIGINAL hook
                // session_id" comment and `Self::pane_hook_session_id`'s doc),
                // so after a rollover the two diverge and a
                // `sessions[generation]` lookup MISSES. The card is therefore
                // resolved with the same newest-by-`last_activity` rule the
                // rest of the daemon uses for "which session owns this pane"
                // (`pane_session_id`, the resolution behind `pane_writable`),
                // which returns a real card id — so the `sessions` lookup here
                // is keyed correctly by construction.
                let card = state
                    .pane_session_id(&pane_id)
                    .and_then(|card_id| state.sessions.get(&card_id));
                // PRD #386 M6b: carry the pane's agent id. `apply_event`'s
                // same-agent reuse guard matches an incoming event onto an
                // existing card for the pane ONLY when the two `agent_id`s
                // agree, and it is the client (not the daemon) where that
                // matters: the DAEMON's card is keyed by the hook session id
                // this event already carries, so it resolves either way, but
                // an attached TUI mints its card at spawn time under a
                // `pane-<id>` key with the spawn `agent_id` on it, and the
                // real hook events are remapped onto THAT card. A synthesized
                // event with `agent_id: None` failed the guard, missed the
                // card, and created a SECOND, phantom session under the raw
                // hook id — so the daemon read `Working` while the dashboard
                // the user is looking at kept rendering the real card as
                // `Idle` (plus a stray extra card). Measured directly: in a
                // `006` run the TUI resolved every real event onto its card
                // and ONLY `ShellBusy` onto a session of its own. A TUI that
                // RECONNECTS reaches the same failure from the other
                // direction: it keys the pane's card by the hydration-minted
                // `pane-{pane_id}`, so the reuse guard is again the only
                // thing that can remap the event onto it.
                //
                // Taken from the pane's own card rather than invented, so it
                // is exactly the id that card was minted with; `None` when no
                // card resolves yet, which is the pre-existing behaviour and
                // no worse than it. Fails SAFE either way: emitting no agent
                // id is only a missed remap, whereas emitting a WRONG one
                // would route a live pane's shell status onto someone else's
                // card.
                let agent_id = card.and_then(|card| card.agent_id.clone());
                // A pane with no card yet is left to the rising edge alone —
                // there is no status to have regressed, and guessing one would
                // emit on every tick until the session materializes.
                let regressed = busy
                    && card.is_some_and(|card| {
                        matches!(
                            card.status,
                            crate::state::SessionStatus::Idle
                                | crate::state::SessionStatus::Unknown
                        )
                    });
                (session_id, agent_id, regressed)
            };

            if !changed && !status_regressed {
                continue;
            }

            if !changed {
                // Only ever the corrective re-emit — a transition logs
                // nothing, and a steady-state busy pane reaches here at most
                // once per regression, so this cannot become a per-tick line
                // even at `RUST_LOG=debug`. It is the one place a "why is the
                // badge still Idle?" investigation needs to look.
                debug!(
                    pane_id = %pane_id,
                    "shell-activity: re-emitting ShellBusy — scan still reads busy while the \
                     session fell back to Idle/Unknown"
                );
            }

            let event = AgentEvent {
                session_id,
                // Deliberate: `AppState::apply_event` only ever UPGRADES a
                // session's `agent_type` FROM `None`, never overwrites a
                // known type with it — so this never regresses a real,
                // hook-learned agent type.
                agent_type: crate::event::AgentType::None,
                event_type: if busy {
                    crate::event::EventType::ShellBusy
                } else {
                    crate::event::EventType::ShellIdle
                },
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata: std::collections::HashMap::new(),
                pane_id: Some(pane_id),
                // The pane's own agent id — see where it is read above. This
                // is what lets an attached TUI resolve the event onto the
                // card it already renders for this pane instead of minting a
                // phantom session beside it.
                agent_id,
                agent_version: None,
                schema_version: None,
                live_target: None,
            };

            // One ordered ingestion step (broadcast + apply under a single
            // write-lock hold), exactly as the hook loop below does it — see
            // `ingest_event` for the interleaving this closes.
            ingest_event(&state, &event_tx, event).await;
        }
    }
}

async fn run_hook_loop(
    listener: IpcListener,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    pty_registry: Arc<AgentPtyRegistry>,
    shutdown: Arc<Notify>,
    worktree_registry: crate::issue_dispatch_run::WorktreeRegistry,
) -> Result<(), DaemonError> {
    loop {
        tokio::select! {
            // PRD #93 M1.2: a notified shutdown wins over a fresh `accept` —
            // we return Ok so `run_daemon_with` cleans up sockets and aborts
            // the attach + idle tasks. The accept future inside the select
            // is dropped, which doesn't leak the listener (only the
            // partially-built tokio future).
            _ = shutdown.notified() => {
                // Deliberately does NOT name a cause: `shutdown` is notified by
                // the idle monitor, the termination-signal watch, the orphan
                // watchdog, and the max-lifetime backstop. Each logs its own
                // reason before notifying, so naming one here (this used to say
                // "on idle shutdown") mislabels the other three.
                info!("Daemon hook loop exiting on shutdown signal");
                return Ok(());
            }
            accept_res = listener.accept() => match accept_res {
            Ok(stream) => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                let pty_registry = pty_registry.clone();
                let worktree_registry = worktree_registry.clone();
                tokio::spawn(async move {
                    // PRD #201: split so the read-only `get-seed` verb can write
                    // a reply back on the same connection. Every other message
                    // on this socket is fire-and-forget, so the write half is
                    // only ever used by the `GetSeed` arm below.
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let reader = tokio::io::BufReader::new(read_half);
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
                                    let resp = state
                                        .read()
                                        .await
                                        .handle_delegate(signal, &pty_registry, &event_tx)
                                        .await;
                                    // Answer on the same connection, like
                                    // `GetSeed` / `ListTargets`. Delegate used to
                                    // be fire-and-forget, so a delegation that
                                    // routed nowhere was invisible to the
                                    // orchestrator that issued it. Best-effort:
                                    // a caller that has already gone away (an
                                    // older CLI, which never reads) just makes
                                    // this a no-op.
                                    if let Ok(json) = serde_json::to_string(&resp) {
                                        let line = format!("{json}\n");
                                        let _ = write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
                                    }
                                }
                                DaemonMessage::Dispatch(signal) => {
                                    info!(
                                        pane_id = %signal.pane_id,
                                        name = %signal.name,
                                        "Received dispatch signal"
                                    );
                                    use crate::dispatch::{self, DispatchContext};

                                    use std::path::PathBuf;

                                    // Phase 1: resolve caller cwd from the PTY registry's
                                    // AgentRecord.cwd, not AppState::pane_cwd_map.
                                    // pane_cwd_map is only populated for orchestration
                                    // panes; mode panes (including the dispatcher mode)
                                    // never get an entry there, which would make every
                                    // dispatch from a mode pane a silent no-op.
                                    let cwd = {
                                        let records = pty_registry.agent_records();
                                        records
                                            .iter()
                                            .find(|r| r.pane_id_env.as_deref() == Some(&signal.pane_id))
                                            .and_then(|r| r.cwd.clone())
                                    };
                                    let cwd = match cwd {
                                        Some(c) => c,
                                        None => {
                                            warn!(pane_id = %signal.pane_id, "dispatch from unknown pane");
                                            continue;
                                        }
                                    };

                                    // Phase 2: do the slow I/O (git worktree + spawn)
                                    // OUTSIDE any AppState lock so concurrent hook
                                    // processing is never stalled.
                                    // The deck's configured default command, so a
                                    // single-agent dispatch starts an AGENT rather
                                    // than `$SHELL`. Same resolution as the
                                    // issue-dispatch arm above; empty → the Claude
                                    // default inside `handle_dispatch`.
                                    let default_command = {
                                        let dc = crate::config::DashboardConfig::load()
                                            .default_command
                                            .trim()
                                            .to_string();
                                        if dc.is_empty() { None } else { Some(dc) }
                                    };
                                    let ctx = DispatchContext {
                                        working_dir: PathBuf::from(&cwd),
                                        registry: pty_registry.clone(),
                                        event_tx: event_tx.clone(),
                                        worktrees: worktree_registry.clone(),
                                        default_command,
                                        // So a dispatched ORCHESTRATION's roles are
                                        // registered for delegate routing — without
                                        // this its orchestrator gets the delegation
                                        // protocol and no way to use it.
                                        state: Some(state.clone()),
                                    };
                                    let task = signal.task.as_deref().unwrap_or_default();
                                    let result = dispatch::handle_dispatch(
                                        &ctx,
                                        &signal.name,
                                        task,
                                        signal.shape.as_ref(),
                                    )
                                    .await;

                                    // Deliver result to the caller pane (doesn't need
                                    // any AppState lock — uses the PTY registry).
                                    if let Err(e) = pty_registry
                                        .write_to_pane_and_submit(&signal.pane_id, &result.message)
                                        .await
                                    {
                                        warn!(
                                            pane_id = %signal.pane_id,
                                            error = %e,
                                            "dispatch: failed to write result into caller pane"
                                        );
                                    }
                                }
                                DaemonMessage::WorkDone(signal) => {
                                    info!(
                                        pane_id = %signal.pane_id,
                                        done = signal.done,
                                        "Received work-done signal"
                                    );
                                    state.read().await.handle_work_done(signal, &pty_registry).await;
                                }
                                DaemonMessage::GetSeed(req) => {
                                    // PRD #201 native prompt delivery: hand the
                                    // pane's pending seed to the caller (the
                                    // extension's `get-seed`) and CLEAR it, so
                                    // the daemon's PTY-injection safety net
                                    // won't also deliver it. `take_..._native`
                                    // marks the delivery as native for the
                                    // real-pi e2e proof. `None` → `{"seed":null}`.
                                    let seed =
                                        pty_registry.take_pending_seed_native(&req.pane_id);
                                    info!(
                                        pane_id = %req.pane_id,
                                        has_seed = seed.is_some(),
                                        "Received get-seed request"
                                    );
                                    let resp =
                                        crate::event::GetSeedResponse { seed };
                                    if let Ok(json) = serde_json::to_string(&resp) {
                                        let line = format!("{json}\n");
                                        let _ =
                                            write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
                                    }
                                }
                                DaemonMessage::ListTargets(req) => {
                                    // PRD #220: the shape menu, computed HERE so it
                                    // comes from the same cwd and the same config
                                    // the dispatch will use. Resolving the cwd from
                                    // `AgentRecord.cwd` — not from the CLI's own
                                    // `current_dir()` — is the whole point: those
                                    // two diverge whenever the agent has `cd`'d, and
                                    // a menu that disagrees with the spawn sends the
                                    // user to a target that cannot start.
                                    let cwd = {
                                        let records = pty_registry.agent_records();
                                        records
                                            .iter()
                                            .find(|r| r.pane_id_env.as_deref() == Some(&req.pane_id))
                                            .and_then(|r| r.cwd.clone())
                                    };
                                    info!(
                                        pane_id = %req.pane_id,
                                        resolved_cwd = ?cwd,
                                        "Received list-targets request"
                                    );
                                    let resp = crate::dispatch::list_targets_response(
                                        cwd.as_deref().map(std::path::Path::new),
                                    );
                                    if let Ok(json) = serde_json::to_string(&resp) {
                                        let line = format!("{json}\n");
                                        let _ =
                                            write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
                                    }
                                }
                            }
                        } else if let Ok(event) = serde_json::from_str::<AgentEvent>(&line) {
                            // `tool_name`/`tool_detail` are logged so a post-mortem can
                            // name the command an agent was running, not just that it ran
                            // one. Four "fleet death" investigations (2026-07-28 23:05,
                            // 07-29 01:54, 07-29 02:09, 08-08 03:05) stalled on exactly
                            // this gap: the daemon logged `ToolStart` with a session id
                            // while the command text lived only in the agent's own
                            // transcript — and a process killed mid-tool never flushes
                            // that entry. In the 08-08 case the ToolStart landed 0.838s
                            // before the daemon took a SIGTERM, so the best-correlated
                            // command was the one piece of evidence permanently lost.
                            // `tool_detail` is already first-line-only and truncated to
                            // 120 chars by `hook::extract_tool_detail`, which bounds the
                            // added log volume; the untruncated command remains in
                            // `metadata["bash_command"]` for anyone who needs it.
                            info!(
                                session_id = %event.session_id,
                                event_type = ?event.event_type,
                                pane_id = ?event.pane_id,
                                agent_type = ?event.agent_type,
                                tool_name = ?event.tool_name,
                                tool_detail = ?event.tool_detail,
                                "Received event"
                            );
                            // The `#[serde(other)]` catch-all on `EventType`
                            // (PRD #386, precedent PRD #201's `AgentType`
                            // retrofit) is a deliberate forward-compat win —
                            // an unrecognized `event_type` no longer fails the
                            // whole decode the way it did before. That also
                            // means a genuine typo in a hand-written hook now
                            // silently decodes to `Unknown` and changes
                            // nothing visible, where it used to be reported as
                            // a malformed event. Restore that diagnostic here,
                            // at the one place the daemon still has the raw
                            // line the unrecognized value came from.
                            if event.event_type == crate::event::EventType::Unknown {
                                warn!(
                                    session_id = %event.session_id,
                                    pane_id = ?event.pane_id,
                                    raw_line = %line,
                                    "Event carries an unrecognized event_type — decoded as \
                                     Unknown and otherwise ignored; check the hook for a typo"
                                );
                            }
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
                                // A SessionStart naming a pane this daemon never
                                // spawned is always wrong, and silently so: it
                                // registers a card no local pane backs, which
                                // surfaces on the dashboard and is then retired
                                // again — the "ghost agent that appeared and
                                // disappeared" report. The usual cause is another
                                // deck's agent posting here, most often a test
                                // child that inherited an ambient
                                // `DOT_AGENT_DECK_SOCKET`.
                                //
                                // Warn rather than drop the event: the pane may
                                // legitimately belong to a client whose agent this
                                // daemon does not own, and refusing hooks would
                                // break that. Naming it is what was missing —
                                // without this line the only trace is a card
                                // flickering past, and the log shows an ordinary
                                // `Received event`.
                                if event.event_type == crate::event::EventType::SessionStart
                                    && !pty_registry.has_live_pane(pane_id)
                                {
                                    warn!(
                                        pane_id = %pane_id,
                                        session_id = %event.session_id,
                                        agent_type = ?event.agent_type,
                                        "SessionStart for a pane this daemon did not spawn — \
                                         a foreign agent is posting here (a test run inheriting \
                                         DOT_AGENT_DECK_SOCKET is the usual cause); it will \
                                         register a card with no local pane"
                                    );
                                }
                                pty_registry.set_agent_type(pane_id, &event.agent_type);
                            }
                            // Fan out to subscribed attach connections and
                            // apply locally as ONE ordered operation, so a
                            // client can never observe two concurrent events
                            // in a different order than the daemon applied
                            // them (PRD #386 — see `ingest_event`). The
                            // broadcast still happens whether or not the
                            // local `apply_event` accepts the event (e.g. an
                            // unmanaged pane id); `send` returns Err only
                            // when there are no subscribers, which is
                            // expected and ignored.
                            //
                            // The registry update above deliberately stays
                            // *ahead* of the fan-out: it is daemon-local
                            // bookkeeping read by `list_agents` on a
                            // different connection, so doing it first only
                            // means a client that reacts to the event by
                            // listing agents sees the fresher answer.
                            ingest_event(&state, &event_tx, event).await;
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

// PRD #42 M2/review: these tests bind a real Unix socket, chmod it via
// `PermissionsExt`, and spawn `/bin/sh` agents — none of which exist on
// Windows. Gate the whole block to Unix so the Windows `cargo nextest run`
// step compiles (mirrors `agent_pty::spawn_tests`). No Unix coverage is lost.
#[cfg(all(test, unix))]
mod hook_ingestion_tests {
    use super::*;
    use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, DeliveryNotice, SpawnOptions};
    use crate::event::AgentType;
    use spec::spec;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{UnixListener, UnixStream};

    /// Scenario: Surface a hookless scheduled pane only through the daemon's live broadcast, leaving daemon AppState intentionally empty, then publish the exact delivery notice used when the 256-watch cap rejects the next confirmation. The already-visible attached-TUI card must receive an Error event through the production sink.
    #[spec("scheduler/dispatch/017")]
    #[tokio::test]
    async fn dispatch_017_cap_notice_reaches_broadcast_only_card() {
        const PANE_ID: &str = "broadcast-only-cap-card";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE_ID.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn hookless scheduled pane");
        let daemon_state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, mut attached_rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        install_delivery_notice_sink(&registry, daemon_state.clone(), event_tx.clone());

        // This is the topology produced by `surface_spawned_pane`: the attached
        // client sees and applies the card, while the daemon never applies the
        // synthetic start to its own AppState.
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            crate::event::DISPLAY_NAME_METADATA_KEY.to_string(),
            "cap-card".to_string(),
        );
        event_tx
            .send(BroadcastMsg::Event(AgentEvent {
                session_id: PANE_ID.to_string(),
                agent_type: AgentType::None,
                event_type: crate::event::EventType::SessionStart,
                tool_name: None,
                tool_detail: None,
                cwd: Some("/tmp/broadcast-only-cap-card".to_string()),
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata,
                pane_id: Some(PANE_ID.to_string()),
                agent_id: None,
                agent_version: None,
                schema_version: None,
                live_target: None,
            }))
            .expect("surface broadcast-only card");
        let BroadcastMsg::Event(surface) = attached_rx.recv().await.expect("surface event") else {
            panic!("expected card surface event");
        };
        let mut attached_state = crate::state::AppState::default();
        attached_state.register_pane(PANE_ID.to_string());
        attached_state.apply_event(surface);
        assert!(
            attached_state
                .sessions
                .values()
                .any(|session| session.pane_id.as_deref() == Some(PANE_ID)),
            "precondition: the attached TUI already has a visible card"
        );
        assert!(
            daemon_state.read().await.sessions.is_empty(),
            "precondition: the broadcast-only card is absent from daemon AppState"
        );

        registry.publish_delivery_notice(DeliveryNotice {
            pane_id: PANE_ID.to_string(),
            agent_id: agent_id.clone(),
            delivery_id: "cap-exhausted-257".to_string(),
            session_id: None,
            detail: "a spawn-time prompt was written into this pane but the daemon is already watching its maximum number of unconfirmed deliveries, so this one is NOT being confirmed or retried; check whether the pane acted on its task",
        });
        let report = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if let BroadcastMsg::Event(event) = attached_rx
                    .recv()
                    .await
                    .expect("delivery-notice broadcast channel")
                    && event.event_type == crate::event::EventType::Error
                {
                    break event;
                }
            }
        })
        .await;
        registry.shutdown_all();
        let report = report.expect(
            "the production delivery-notice sink must broadcast cap exhaustion to the already-visible card",
        );
        attached_state.apply_event(report);
        assert!(
            attached_state.sessions.values().any(|session| {
                session.pane_id.as_deref() == Some(PANE_ID)
                    && session.status == crate::state::SessionStatus::Error
            }),
            "the attached TUI's broadcast-only card must visibly become Error"
        );
    }

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
        // Deliberately bind WITHOUT `bind_socket`: that helper flips the
        // process-global umask to 0o177 around `bind`, and under CI's
        // `cargo test` (all lib tests share one process) that window races
        // concurrent tempdir creation in other tests, leaving a dir without
        // its search bit → `PermissionDenied` on bind. `cargo test-fast`
        // (nextest, process-per-test) hides this. A plain bind keeps this
        // test from perturbing the shared umask; the `set_permissions` below
        // immunizes our own tempdir against another test's flip. Socket perms
        // are irrelevant to what this test asserts.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod tempdir");
        let sock = dir.path().join("hook.sock");
        // Wrap the plain `UnixListener` as an `IpcListener` without going
        // through `IpcListener::bind` (whose umask flip is what the comment
        // above deliberately avoids). `run_hook_loop` takes an `IpcListener`.
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let shutdown = Arc::new(Notify::new());

        let handle = tokio::spawn({
            let registry = registry.clone();
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move { run_hook_loop(listener, state, event_tx, registry, shutdown, wtr).await }
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
        // Await the aborted task so it drops its `registry` Arc clone before
        // we tear the registry down — strictly sequences cleanup instead of
        // racing `shutdown_all` against the still-live loop task.
        let _ = handle.await;
        registry.shutdown_all();
    }

    /// Scenario: PRD #370's whole point, end to end, **restimulated for PRD
    /// #386 M3**. Spawn a real `/bin/sh` pane, seed it a known session the way
    /// a real hook `SessionStart` would (so `AppState::pane_hook_session_id`
    /// can resolve it), run the real `run_shell_activity_monitor` against it,
    /// then type a command into the pane's PTY directly (no agent hooks
    /// involved at all) that launches a genuinely `setsid`-detached child. The
    /// monitored session's status must flip to `Working` while that child runs
    /// and revert to `Idle` once it exits — proving the daemon-synthesized
    /// `ShellBusy`/`ShellIdle` signal reaches `AppState` through the exact
    /// pipeline this PRD reports missing.
    ///
    /// **What #386 changed here, and why the pipeline assertions are unchanged.**
    /// This test used to type a plain `sleep 2`, which #370's `tcgetpgrp` body
    /// read as busy. #386 replaced that body with a descendant scan that fires
    /// on a descendant in a POSIX session of its own, and a job typed into the
    /// pane's own PTY stays in the pane's session — deliberately not busy, since
    /// counting it would also count every long-lived MCP/`caffeinate` child a
    /// real agent pane carries and pin the pane at `Working` forever (see
    /// `shell_foreground_busy_ignores_a_non_detached_foreground_child`). Only
    /// the *stimulus* moved to the topology a real Claude Bash-tool call has;
    /// what this test proves — pane → monitor → synthesized event → `AppState`
    /// status — is exactly what it always proved.
    #[tokio::test]
    async fn shell_activity_monitor_reflects_a_real_detached_shell_command() {
        use std::io::Write as _;

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-370".to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);

        // Seed exactly as a real hook SessionStart would — this is what
        // populates BOTH `AppState.sessions` and the `pane_hook_session_id`
        // correlation the monitor depends on to resolve "which session does
        // pane-370's shell activity belong to."
        state.write().await.apply_event(AgentEvent {
            session_id: "sess-370".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: crate::event::EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some("pane-370".to_string()),
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
        });
        assert_eq!(
            state.read().await.sessions["sess-370"].status,
            crate::state::SessionStatus::Idle
        );

        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            async move { run_shell_activity_monitor(registry, state, event_tx).await }
        });

        // Type the command directly into the pane's PTY — no agent, no hook,
        // nothing but the raw shell. It `fork`s, `setsid`s the child (detaching
        // it from the pane's controlling terminal into a session of its own,
        // exactly as Claude Code's Bash-tool child does) and `execv`s it into a
        // 2-second `sleep`, then waits for it. The `fork` matters: an
        // interactive `/bin/sh` has job control on and makes each foreground
        // job its own process-group leader, and `setsid(2)` fails with EPERM
        // for a process that already leads a group — so python must detach a
        // *child*, not itself. The parent's `waitpid` keeps the pane occupied
        // for the child's whole life, and the child exiting on its own is what
        // drives the falling edge below.
        {
            let writer = registry
                .agent_writer(&agent_id)
                .expect("spawned agent must be in the registry");
            let mut w = writer.lock().await;
            w.write_all(
                b"python3 -c \"import os; pid = os.fork(); \
                  (os.setsid(), os.execv('/bin/sleep', ['sleep', '2'])) if pid == 0 \
                  else os.waitpid(pid, 0)\"\n",
            )
            .expect("write detached-child command");
            w.flush().expect("flush");
        }

        let status = |state: SharedState| async move {
            state.read().await.sessions["sess-370"].status.clone()
        };

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut current = status(state.clone()).await;
        while current != crate::state::SessionStatus::Working
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
            current = status(state.clone()).await;
        }
        assert_eq!(
            current,
            crate::state::SessionStatus::Working,
            "the monitor must promote the session to Working while the detached \
             child runs, with zero agent-emitted events involved"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        let mut current = status(state.clone()).await;
        while current != crate::state::SessionStatus::Idle && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
            current = status(state.clone()).await;
        }
        assert_eq!(
            current,
            crate::state::SessionStatus::Idle,
            "the monitor must revert the session to Idle once the detached child \
             exits and the pane has no out-of-session descendant left"
        );

        monitor_handle.abort();
        let _ = monitor_handle.await;
        registry.shutdown_all();
    }

    /// Scenario: PRD #386 M6b's agent-id stamping across a session rollover —
    /// the EMITTED SHAPE rather than the local effect
    /// (`shell_activity_monitor_reflects_a_real_detached_shell_command` above
    /// covers that). Spawn a real `/bin/sh` pane owned by `agent-21`, seed it a hook
    /// `SessionStart`, then seed a SECOND `SessionStart` under the SAME agent
    /// with a NEW hook session id — a same-agent `/clear` / thread restart, so
    /// the pane's hook generation rolls forward to `sess-21-gen2` while
    /// `apply_event`'s reuse guard keeps the card under the stable
    /// `sess-21-gen1`. Run the real `run_shell_activity_monitor`, subscribe to
    /// its broadcast, and type a `setsid`-detached `sleep` into the PTY (the
    /// topology PRD #386's descendant scan fires on). Every event
    /// it broadcasts must report the CURRENT hook generation as its
    /// `session_id` AND carry `agent_id: Some("agent-21")` resolved from the
    /// pane's card — an unstamped event cannot be remapped onto a reconnected
    /// TUI's hydrated card and mints a phantom session instead.
    #[tokio::test]
    async fn shell_activity_monitor_stamps_the_owning_agent_across_a_session_rollover() {
        use std::io::Write as _;

        const PANE: &str = "pane-21";
        const AGENT: &str = "agent-21";
        const GEN1: &str = "sess-21-gen1";
        const GEN2: &str = "sess-21-gen2";

        let registry = Arc::new(AgentPtyRegistry::new());
        let spawned = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, mut rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);

        let session_start = |session_id: &str| AgentEvent {
            session_id: session_id.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: crate::event::EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(PANE.to_string()),
            agent_id: Some(AGENT.to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        };

        // Generation 1, then the same-agent restart that rolls the generation
        // over. Both frames carry the SAME `agent_id`, which is exactly what
        // makes `apply_event`'s reuse guard keep one stable card.
        state.write().await.apply_event(session_start(GEN1));
        state.write().await.apply_event(session_start(GEN2));
        {
            let guard = state.read().await;
            assert_eq!(
                guard.pane_hook_session_id(PANE).as_deref(),
                Some(GEN2),
                "precondition: the same-agent restart advances the pane's hook generation"
            );
            assert!(
                guard.sessions.contains_key(GEN1) && !guard.sessions.contains_key(GEN2),
                "precondition: the CARD stays under the stable id, so the hook \
                 generation is NOT a key into `sessions` — this divergence is \
                 what a `sessions[generation]` lookup silently misses"
            );
        }

        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            async move { run_shell_activity_monitor(registry, state, event_tx).await }
        });

        // The stimulus is the one PRD #386's descendant scan actually fires on:
        // `fork`, `setsid` the child into a POSIX session of its own (the
        // topology a real Claude Bash-tool child has), `execv` it into a short
        // `sleep`, and have the parent `waitpid` for it — see
        // `shell_activity_monitor_reflects_a_real_detached_shell_command` above
        // for the full rationale. A plain `sleep 2` typed into the pane's own
        // PTY was busy under #370's `tcgetpgrp` body but is deliberately NOT
        // busy under #386's scan, so it would never produce the `ShellBusy`
        // this test needs to inspect.
        {
            let writer = registry
                .agent_writer(&spawned)
                .expect("spawned agent must be in the registry");
            let mut w = writer.lock().await;
            w.write_all(
                b"python3 -c \"import os; pid = os.fork(); \
                  (os.setsid(), os.execv('/bin/sleep', ['sleep', '2'])) if pid == 0 \
                  else os.waitpid(pid, 0)\"\n",
            )
            .expect("write detached-child command");
            w.flush().expect("flush");
        }

        // Read broadcasts until the busy transition arrives (the monitor also
        // emits the pane's initial idle edge), asserting the stamped shape on
        // EVERY event it publishes — a single unstamped one is enough to mint
        // a phantom card on a reconnected TUI.
        let mut saw_busy = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !saw_busy && tokio::time::Instant::now() < deadline {
            let Ok(Ok(BroadcastMsg::Event(event))) =
                tokio::time::timeout(Duration::from_millis(500), rx.recv()).await
            else {
                continue;
            };
            assert_eq!(
                event.session_id, GEN2,
                "the synthesized event must report the pane's CURRENT hook \
                 generation, which is what the daemon's send guard compares against"
            );
            assert_eq!(
                event.agent_id.as_deref(),
                Some(AGENT),
                "the synthesized {:?} must carry the owning agent id resolved \
                 from the pane's CARD; a `sessions[hook_generation]` lookup \
                 misses after a same-agent restart and re-emits `None`, which \
                 no hydrated TUI card can be remapped onto",
                event.event_type
            );
            saw_busy = event.event_type == crate::event::EventType::ShellBusy;
        }
        assert!(
            saw_busy,
            "the monitor must broadcast a ShellBusy while the detached child runs"
        );

        monitor_handle.abort();
        let _ = monitor_handle.await;
        registry.shutdown_all();
    }

    /// Scenario: issue #493. Run the real shell-activity monitor against an
    /// EMPTY registry — no panes at all, the state a daemon sits in whenever a
    /// TUI is attached with nothing open — with the process-table sample
    /// replaced by a counting stub, and let it tick several times. The sampler
    /// must never be called: with nobody to classify there is nothing a process
    /// table could say, so the `ps -A` fork (plus its `getsid` per row) must not
    /// happen at all. Before the fix `process_table()` was the FIRST statement
    /// of the snapshot, so this same run forked `ps` twice a second forever —
    /// the daemon's idle shutdown does not bound it, since that requires no
    /// clients *and* no agents.
    #[tokio::test]
    async fn shell_activity_monitor_never_samples_the_process_table_with_no_live_panes() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let registry = Arc::new(AgentPtyRegistry::new());
        assert_eq!(
            registry.live_count(),
            0,
            "precondition: the registry must be empty, so there is no candidate pane"
        );

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, mut rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);

        let samples = Arc::new(AtomicUsize::new(0));
        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let samples = samples.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move || {
                    let samples = samples.clone();
                    async move {
                        samples.fetch_add(1, AtomicOrdering::SeqCst);
                        None
                    }
                })
                .await
            }
        });

        // Comfortably more than four 500ms poll intervals, so a monitor that
        // samples unconditionally would have done so several times over.
        tokio::time::sleep(Duration::from_millis(2_200)).await;

        assert_eq!(
            samples.load(AtomicOrdering::SeqCst),
            0,
            "the monitor must not sample the process table when no live pane exists — \
             every sample here is a `ps -A` fork spent classifying nobody"
        );
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "and with no panes there is nothing to emit either"
        );

        monitor_handle.abort();
        let _ = monitor_handle.await;
    }

    /// Scenario: issue #429's load-bearing decision. Spawn a real `/bin/sh`
    /// pane, seed it a hook session and drive that session to `Working`, then
    /// run the real shell-activity monitor with a process-table sample that
    /// NEVER completes — a `ps` wedged in D-state on a stuck filesystem. The
    /// monitor's own timeout must fire and be treated as "no opinion": the
    /// session stays `Working` and no event is broadcast at all. A timeout
    /// collapsed to `Some(false)` would instead synthesize a `ShellIdle` for
    /// every pane the deck is running and silently flip them all to `Idle` —
    /// the exact stale-`Idle` bug PRD #386 exists to fix.
    #[tokio::test]
    async fn shell_activity_monitor_leaves_statuses_alone_when_the_sample_times_out() {
        const PANE: &str = "pane-429";
        const SESSION: &str = "sess-429";

        let registry = Arc::new(AgentPtyRegistry::new());
        registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, mut rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);

        let event = |event_type: crate::event::EventType| AgentEvent {
            session_id: SESSION.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(PANE.to_string()),
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
        };
        // `SessionStart` creates the card (and the `pane_hook_session_id`
        // correlation the monitor needs); `ShellBusy` promotes it to `Working`,
        // which is the status a wrongly-collapsed timeout would knock down.
        state
            .write()
            .await
            .apply_event(event(crate::event::EventType::SessionStart));
        state
            .write()
            .await
            .apply_event(event(crate::event::EventType::ShellBusy));
        assert_eq!(
            state.read().await.sessions[SESSION].status,
            crate::state::SessionStatus::Working,
            "precondition: the pane must start out reading Working, so a spurious \
             ShellIdle would be visible"
        );

        let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let samples = samples.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move || {
                    samples.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // The wedged `ps`: a sample that never answers. The
                    // monitor's SAMPLE_TIMEOUT is what has to end this tick.
                    std::future::pending::<Option<Vec<crate::platform::proc::ProcessInfo>>>()
                })
                .await
            }
        });

        // Long enough for the 500ms interval + 2s deadline to elapse TWICE, so a
        // monitor that abandoned the overrunning sample would have started a
        // second one by now.
        tokio::time::sleep(Duration::from_millis(5_600)).await;

        assert_eq!(
            state.read().await.sessions[SESSION].status,
            crate::state::SessionStatus::Working,
            "a timed-out process-table sample says nothing about the pane, so the \
             status must be left exactly as it was — collapsing the timeout to \
             \"not busy\" is what silently flips every busy pane to Idle"
        );
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "and no ShellIdle (or any other event) may be synthesized from a sample \
             that never answered"
        );
        // PR #500 review (P1): the deadline bounds the WAIT, not the child. A
        // `ps` wedged in uninterruptible sleep ignores the `SIGKILL` that
        // dropping the future sends, so abandoning it per tick would leave one
        // undead `ps` behind every 2.5s. The monitor must keep waiting on the
        // SAME sample instead — exactly one sample started, however long it
        // overruns.
        assert_eq!(
            samples.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an overrunning sample must be re-awaited, not abandoned and replaced — \
             starting a fresh `ps` per tick piles up unkillable D-state children"
        );

        monitor_handle.abort();
        let _ = monitor_handle.await;
        registry.shutdown_all();
    }

    /// Scenario: PR #500 review, round 2. Because an overrunning sample is
    /// retained rather than abandoned, it can answer arbitrarily late — after a
    /// wedge that outlasted every pane, with a new pane since opened. Its table
    /// then describes a machine that no longer exists, and under pid reuse a new
    /// pane would inherit a dead process's descendants; since `last_known` has no
    /// entry for a new pane, that wrong reading emits immediately.
    ///
    /// So: spawn a real `/bin/sh` pane whose session reads `Idle`, and hand the
    /// monitor a sampler that answers only after 4s (past `MAX_TABLE_AGE`) with a
    /// table that says this very pane is busy. The stale answer must be discarded
    /// — the session stays `Idle` and nothing is broadcast. A monitor that trusted
    /// it would promote the pane to `Working` off a table it should not believe.
    #[tokio::test]
    async fn shell_activity_monitor_discards_a_sample_that_answers_too_late_to_trust() {
        const PANE: &str = "pane-500-stale";
        const SESSION: &str = "sess-500-stale";

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        let shell_pid = registry
            .child_pid(&agent_id)
            .expect("spawned agent must expose a pid") as i32;

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, mut rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        state.write().await.apply_event(AgentEvent {
            session_id: SESSION.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: crate::event::EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(PANE.to_string()),
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
        });
        assert_eq!(
            state.read().await.sessions[SESSION].status,
            crate::state::SessionStatus::Idle,
            "precondition: the pane starts Idle, so a wrongly-trusted busy table would show"
        );

        // A table that WOULD classify this pane as busy: the pane's own shell as
        // session leader, plus a descendant in a session of its own. The pane
        // carries no agent kind, so no argv shape applies and the structural test
        // stands alone — this is unambiguously `Some(true)`.
        let busy_table = vec![
            crate::platform::proc::ProcessInfo {
                pid: shell_pid,
                ppid: 1,
                session_id: shell_pid,
                has_controlling_tty: true,
                session_leader: true,
                argv: "/bin/sh".to_string(),
            },
            crate::platform::proc::ProcessInfo {
                pid: shell_pid + 1,
                ppid: shell_pid,
                session_id: shell_pid + 1,
                has_controlling_tty: false,
                session_leader: true,
                argv: "detached-thing".to_string(),
            },
        ];

        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move || {
                    let busy_table = busy_table.clone();
                    async move {
                        // Answers eventually, but far past MAX_TABLE_AGE — the
                        // late-wedge-recovery shape, compressed.
                        tokio::time::sleep(Duration::from_secs(4)).await;
                        Some(busy_table)
                    }
                })
                .await
            }
        });

        // Past the 500ms interval + the sampler's 4s, with margin, so the stale
        // answer has definitely been received and judged.
        tokio::time::sleep(Duration::from_millis(5_200)).await;

        assert_eq!(
            state.read().await.sessions[SESSION].status,
            crate::state::SessionStatus::Idle,
            "a sample that answered past MAX_TABLE_AGE describes a machine that has \
             moved on and must be discarded, not applied — trusting it attributes a \
             stale table's descendants to today's pids"
        );
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "and nothing may be broadcast off a table that was not trusted"
        );

        monitor_handle.abort();
        let _ = monitor_handle.await;
        registry.shutdown_all();
    }

    /// Scenario: PR #500 review, round 3 — the residual inside `MAX_TABLE_AGE`.
    /// A freshness bound is not an identity check: a pane can be replaced while a
    /// retained sample is still in flight, and if the replacement's shell pid is a
    /// reused one the table would classify it by numeric pid alone against the
    /// DEPARTED pane's descendants.
    ///
    /// Real pid reuse cannot be forced in a test, so the same shape is built
    /// directly: pane A is open when the sample starts, the sample overruns, and
    /// pane B is spawned while it is still in flight. The table the sample
    /// finally returns names **both** pids as busy — B's row standing in for what
    /// a reused pid would look like. A (unchanged since the sample began) must be
    /// promoted to `Working`; B (which did not exist then) must stay `Idle`.
    ///
    /// Asserting on A is what makes this test honest rather than merely green.
    /// The sample lands ~2.5s old, inside `MAX_TABLE_AGE` — but if anything
    /// slowed the run enough to push it past that bound, the freshness guard
    /// would swallow the whole answer and B would stay `Idle` for a reason having
    /// nothing to do with identity matching. A reaching `Working` proves the
    /// answer was accepted, so B staying `Idle` can only be the identity filter.
    /// (Measured while writing this: an earlier version closed pane A here, and
    /// `close_agent`'s SIGTERM grace window — `/bin/sh` ignores SIGTERM — blocked
    /// the current-thread runtime long enough that the sample landed at 3.4s and
    /// the test passed entirely via the freshness guard.)
    #[tokio::test]
    async fn shell_activity_monitor_ignores_a_pane_that_appeared_after_the_sample_started() {
        const PANE_A: &str = "pane-500-a";
        const PANE_B: &str = "pane-500-b";
        const SESSION_A: &str = "sess-500-a";
        const SESSION_B: &str = "sess-500-b";

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_a = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE_A.to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn pane A");
        let pid_a = registry
            .child_pid(&agent_a)
            .expect("pane A must expose a pid") as i32;

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);

        let session_start = |session_id: &str, pane_id: &str| AgentEvent {
            session_id: session_id.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: crate::event::EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(pane_id.to_string()),
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
        };
        state
            .write()
            .await
            .apply_event(session_start(SESSION_A, PANE_A));

        // The table the sample will eventually return, filled in only once pane B
        // exists — so it can name B's real pid, which is what a reused pid would
        // look like to the classifier.
        let late_table: Arc<std::sync::Mutex<Vec<crate::platform::proc::ProcessInfo>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let late_table = late_table.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move || {
                    let late_table = late_table.clone();
                    async move {
                        // Longer than SAMPLE_TIMEOUT (2s) so the sample is
                        // RETAINED rather than answered on its first tick, and
                        // ready by the resumed tick — which lands it ~2.5s old,
                        // inside MAX_TABLE_AGE (3s). That is the window where the
                        // freshness bound alone would let it through, so it is
                        // the window the identity filter has to cover.
                        tokio::time::sleep(Duration::from_millis(2_100)).await;
                        Some(late_table.lock().unwrap().clone())
                    }
                })
                .await
            }
        });

        // Let the first tick resolve candidates (pane A only) and start the
        // sample, then add pane B while that sample is still in flight.
        // Deliberately no `close_agent` — see this test's doc comment.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let agent_b = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE_B.to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn pane B");
        let pid_b = registry
            .child_pid(&agent_b)
            .expect("pane B must expose a pid") as i32;
        state
            .write()
            .await
            .apply_event(session_start(SESSION_B, PANE_B));

        // Both panes read busy in the table: a shell as session leader plus a
        // descendant in a session of its own. Neither pane carries an agent kind,
        // so no argv shape applies and the structural test stands alone.
        let busy_pair = |pid: i32| {
            [
                crate::platform::proc::ProcessInfo {
                    pid,
                    ppid: 1,
                    session_id: pid,
                    has_controlling_tty: true,
                    session_leader: true,
                    argv: "/bin/sh".to_string(),
                },
                crate::platform::proc::ProcessInfo {
                    pid: pid + 100_000,
                    ppid: pid,
                    session_id: pid + 100_000,
                    has_controlling_tty: false,
                    session_leader: true,
                    argv: "detached-thing".to_string(),
                },
            ]
        };
        *late_table.lock().unwrap() = busy_pair(pid_a)
            .into_iter()
            .chain(busy_pair(pid_b))
            .collect();

        {
            let guard = state.read().await;
            assert_eq!(
                guard.sessions[SESSION_A].status,
                crate::state::SessionStatus::Idle,
                "precondition: both panes start Idle"
            );
            assert_eq!(
                guard.sessions[SESSION_B].status,
                crate::state::SessionStatus::Idle,
                "precondition: both panes start Idle"
            );
        }

        // Past the resumed tick that receives the late answer.
        tokio::time::sleep(Duration::from_millis(2_800)).await;

        let (status_a, status_b) = {
            let guard = state.read().await;
            (
                guard.sessions[SESSION_A].status.clone(),
                guard.sessions[SESSION_B].status.clone(),
            )
        };
        monitor_handle.abort();
        let _ = monitor_handle.await;
        registry.shutdown_all();

        assert_eq!(
            status_a,
            crate::state::SessionStatus::Working,
            "pane A was already open when the sample started, so the sample's verdict \
             about it is trustworthy and must be applied — this also proves the answer \
             was ACCEPTED rather than swallowed by the freshness guard, without which \
             pane B's assertion below would pass for the wrong reason"
        );
        assert_eq!(
            status_b,
            crate::state::SessionStatus::Idle,
            "pane B did not exist when the sample started, so the pid naming it in that \
             table cannot be known to be B's — which under pid reuse is exactly how a \
             replacement pane inherits a departed one's descendants"
        );
    }

    /// Scenario: PRD #201 native prompt delivery over the hook socket. Spawn a
    /// shell agent tagged with a pane id, stash a seed for it via
    /// `set_pending_seed`, run the real `run_hook_loop`, then send a `get_seed`
    /// request line and read the reply. The daemon must reply with the exact
    /// seed, mark it delivered-native, and clear it — a second request replies
    /// `{"seed":null}`. This is the request/response path the pi extension's
    /// `get-seed` verb rides (the one hook-socket message that reads a reply).
    #[tokio::test]
    async fn run_hook_loop_answers_get_seed_and_clears_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let registry = Arc::new(AgentPtyRegistry::new());
        registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-gs".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        registry.set_pending_seed(
            "pane-gs",
            "Acknowledge your role and wait for instructions.",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod tempdir");
        let sock = dir.path().join("hook.sock");
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn({
            let registry = registry.clone();
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move { run_hook_loop(listener, state, event_tx, registry, shutdown, wtr).await }
        });

        // Helper: send one get_seed request line and read the single reply line.
        async fn ask_get_seed(sock: &std::path::Path, pane_id: &str) -> String {
            let req = crate::event::DaemonMessage::GetSeed(crate::event::GetSeedRequest {
                pane_id: pane_id.to_string(),
            });
            let line = format!("{}\n", serde_json::to_string(&req).unwrap());
            let mut stream = UnixStream::connect(sock).await.expect("connect");
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
            let mut buf = String::new();
            stream.read_to_string(&mut buf).await.unwrap();
            buf
        }

        // First pull: the daemon returns the seed…
        let reply = ask_get_seed(&sock, "pane-gs").await;
        let resp: crate::event::GetSeedResponse =
            serde_json::from_str(reply.trim()).expect("parse get-seed reply");
        assert_eq!(
            resp.seed.as_deref(),
            Some("Acknowledge your role and wait for instructions.")
        );
        // …marks it delivered-native and clears it.
        assert!(
            registry.seed_delivered_native("pane-gs"),
            "answering get-seed must mark the delivery native"
        );

        // Second pull: nothing left — the seed was delivered exactly once.
        let reply2 = ask_get_seed(&sock, "pane-gs").await;
        let resp2: crate::event::GetSeedResponse =
            serde_json::from_str(reply2.trim()).expect("parse second get-seed reply");
        assert!(
            resp2.seed.is_none(),
            "seed must be cleared after the first pull"
        );

        // Unknown pane → null, harmless.
        let reply3 = ask_get_seed(&sock, "pane-unknown").await;
        let resp3: crate::event::GetSeedResponse =
            serde_json::from_str(reply3.trim()).expect("parse unknown-pane get-seed reply");
        assert!(resp3.seed.is_none());

        handle.abort();
        let _ = handle.await;
        registry.shutdown_all();
    }

    /// Is `pid` gone? Mirrors `agent_pty::spawn_tests::pid_is_dead`, which is
    /// private to that module. Unix-only, like this whole block.
    fn pid_is_dead(pid: u32) -> bool {
        let r = unsafe { libc::kill(pid as i32, 0) };
        if r == 0 {
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// Round-2 reviewer, blocker A: the daemon's two back-references must not
    /// close a cycle, because `AgentPtyRegistry::drop` is what kills this
    /// daemon's PTYs.
    ///
    /// `run_daemon_with` builds exactly this shape: the registry owns the
    /// delivery-notice sink, whose installed closure holds a strong
    /// `SharedState`; and `AppState` holds the registry as its ownership oracle.
    /// With a strong `Arc` on the second edge the loop closes —
    /// `AppState -> AgentPtyRegistry -> sink -> SharedState -> AppState` — and
    /// the explicit `drop(pty_registry)` at the end of `run_daemon_with`
    /// releases one reference out of a set that keeps each other alive. The
    /// registry's documented RAII guarantee ("dropping or aborting the daemon
    /// kills its PTYs") then silently does not hold, and the whole daemon state
    /// is retained for the process's lifetime. Signal and protocol shutdown
    /// drain explicitly and are unaffected; an ABORTED task or an accept loop
    /// that returns an error — which the contract explicitly covers — is not.
    ///
    /// So the teardown is observed rather than argued: after the last reference
    /// a daemon would hold goes away, the registry is really gone AND the child
    /// it owned is really dead. The oracle is exercised first, so a test that
    /// "passes" by never having wired the thing up cannot happen.
    #[tokio::test]
    async fn dropping_the_daemons_registry_still_reaps_its_children() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(16);

        const PANE_ID: &str = "teardown-pane-454";
        let agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE_ID.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn a long-lived child");
        let pid = registry
            .child_pid(&agent_id)
            .expect("the child must expose a pid");

        // The two back-references, installed exactly as `run_daemon_with`
        // installs them.
        install_delivery_notice_sink(&registry, state.clone(), event_tx.clone());
        {
            let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
            state
                .write()
                .await
                .set_agent_ownership(Arc::downgrade(&ownership));
        }

        // Precondition: the oracle actually answers through that edge. Without
        // this the assertions below would also pass on a daemon that never
        // installed one.
        {
            let mut guard = state.write().await;
            guard.apply_event(AgentEvent {
                session_id: format!("{PANE_ID}-session"),
                agent_type: AgentType::Pi,
                event_type: crate::event::EventType::Thinking,
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata: Default::default(),
                pane_id: Some(PANE_ID.to_string()),
                agent_id: Some(agent_id.clone()),
                agent_version: None,
                schema_version: None,
                live_target: None,
            });
            assert!(
                guard
                    .sessions
                    .values()
                    .any(|s| s.pane_id.as_deref() == Some(PANE_ID)),
                "precondition: the installed oracle must admit the spawned \
                 pane's own report"
            );
        }

        // Everything a daemon would drop when its task is aborted.
        let weak = Arc::downgrade(&registry);
        drop(registry);

        assert!(
            weak.upgrade().is_none(),
            "the registry must be dropped once the daemon lets go of it — a \
             strong reference from AppState back to it closes a cycle through \
             the delivery-notice sink, and `AgentPtyRegistry::drop` then never \
             runs at all"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !pid_is_dead(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "pid {pid} is still alive after the registry was dropped; the \
                 RAII teardown that kills this daemon's PTYs did not run"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The state outlives the registry, and its oracle now answers "not
        // owned" instead of dangling.
        assert!(
            state.read().await.sessions.len() == 1,
            "the daemon state itself is unaffected by the registry going away"
        );
    }
}
