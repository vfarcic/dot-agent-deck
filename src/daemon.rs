use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, broadcast};
use tracing::{debug, error, info, warn};

use crate::platform::ipc::{IpcListener, IpcStream};

use crate::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_EXIT_WHEN_ORPHANED, DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS,
    DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS,
};
use crate::config_validation::escape_id_for_log;
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
///
/// Issue #1109: the signal path stays UNGUARDED on purpose — it does not run
/// the [`crate::daemon_stop::stop_refusal`] policy `daemon stop` and the
/// `StopDaemon` wire verb run, and it never refuses. A SIGTERM is how a service
/// manager, a container runtime or a session logout asks a daemon to stop, and
/// one that argues back is escalated to SIGKILL on the sender's clock, losing
/// both the graceful drain below and any chance to say what it lost. What it
/// gained instead is DISCLOSURE: `state` is threaded in for
/// [`crate::daemon_stop::log_teardown_inventory`], which names the agents and
/// the orchestration roles at stake before the drain empties both. The full
/// argument, including the two shapes that were rejected, is in
/// `docs/develop/daemon-teardown-paths.md`.
///
/// PRD #1487: `restart` is told first, so a stop wins over a restart under way
/// or already accepted — no successor starts after a signal asked the daemon
/// to stop. The watch lives until `run_daemon_with` has decided its successor
/// plan and finished acting on it (audit A2), so a signal is never lost in
/// that window:
///
/// - before the plan is decided, it is a stop like any other: the plan is
///   then [`crate::daemon_restart::SuccessorPlan::Nothing`] and the stop is
///   disclosed;
/// - after the daemon committed to a restart, it is logged and not acted on —
///   the daemon is already exiting into its successor, and a stop cannot be
///   taken instead;
/// - either way a second signal force-exits at once, so a wedged teardown or
///   successor start can still be ended with `pkill`.
///
/// `registry` is weak so the watch never keeps the registry alive past the
/// `drop` in `run_daemon_with` that tears it down; a signal after that has no
/// agents left to name or drain.
fn spawn_termination_signal_watch(
    shutdown: Arc<Notify>,
    registry: std::sync::Weak<AgentPtyRegistry>,
    state: SharedState,
    restart: Arc<crate::daemon_restart::RestartControl>,
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
            if restart.stop_wins("signal") {
                warn!(
                    signal = sig,
                    "daemon received termination signal; initiating graceful shutdown \
                     (every managed agent will be stopped)"
                );
                stop_for_signal(&shutdown, &registry, &state).await;
            } else {
                warn!(
                    signal = sig,
                    "termination signal after this daemon committed to its restart; it keeps \
                     exiting into its successor — a second signal exits at once"
                );
            }

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
            // DURING the drain is buffered by tokio's signal stream and handled
            // as soon as the drain returns, so the hatch is delayed by at most
            // `AGENT_TERMINATE_GRACE`, never lost.
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
            // Same decision as the Unix arm above; see its comment.
            if restart.stop_wins("signal") {
                warn!(
                    signal = "CTRL_C",
                    "daemon received termination signal; initiating graceful shutdown \
                     (every managed agent will be stopped)"
                );
                stop_for_signal(&shutdown, &registry, &state).await;
            } else {
                warn!(
                    signal = "CTRL_C",
                    "termination signal after this daemon committed to its restart; it keeps \
                     exiting into its successor — a second signal exits at once"
                );
            }

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

/// What a termination signal that won does: disclose, drain, release the hook
/// loop. Shared by both platforms' arms of [`spawn_termination_signal_watch`].
async fn stop_for_signal(
    shutdown: &Notify,
    registry: &std::sync::Weak<AgentPtyRegistry>,
    state: &SharedState,
) {
    // The registry is gone once `run_daemon_with` dropped it, and its `Drop`
    // already stopped every agent; there is nothing left to name or drain.
    if let Some(registry) = registry.upgrade() {
        // Issue #1109: say WHICH, before the drain below empties the registry
        // this reads. Ordered ahead of the drain for that reason and not merely
        // for tidiness — `agent_records` filters to live agents, so the same
        // call after `shutdown_all_graceful` reports an empty deck no matter
        // what was running.
        crate::daemon_stop::log_teardown_inventory(state, &registry, "signal").await;

        // Drain managed agents with the SAME grace the `KIND_SHUTDOWN` handler
        // gives them, BEFORE releasing the hook loop. Notifying `shutdown`
        // alone is not enough: the loop returns, `run_daemon_with` drops the
        // registry, and `Drop` calls `shutdown_all` — the SIGKILL-WITHOUT-grace
        // path, which `shutdown_all_graceful`'s own docs scope to "idle shutdown
        // and test cleanup". Idle shutdown only fires with no agents left, so
        // force-killing there costs nothing; a signal is a DELIBERATE stop that
        // routinely lands on live agents, so it belongs on the graceful path.
        // (Greptile P1 on the first draft, which notified and returned — agents
        // lost the grace this change promised.)
        //
        // `spawn_blocking` mirrors `daemon_protocol`'s KIND_SHUTDOWN arm: the
        // drain blocks while it polls for each child to exit. Idempotent via
        // the registry's `shutting_down` latch, whose docs already anticipate
        // "a SIGTERM landing during shutdown".
        let _ = tokio::task::spawn_blocking(move || {
            registry.shutdown_all_graceful(crate::agent_pty::AGENT_TERMINATE_GRACE);
        })
        .await;
    }
    shutdown.notify_one();
}

/// PRD #1487 e2e seam (audit A2): hold the daemon between releasing its
/// sockets and deciding its successor plan, so a process-level test can deliver
/// a signal inside that window, which no outside timing reaches reliably.
/// `DOT_AGENT_DECK_E2E_SUCCESSOR_PLAN_GATE` names a directory: the daemon
/// creates `entered` in it, then waits — at most a minute — for `release`.
/// Compiled only into the `e2e` build, like the subscriber seams in `main.rs`,
/// so a shipped binary has no switch that holds its exit.
#[cfg(feature = "e2e")]
async fn e2e_successor_plan_gate() {
    let Some(dir) = std::env::var_os("DOT_AGENT_DECK_E2E_SUCCESSOR_PLAN_GATE") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = std::fs::write(dir.join("entered"), b"");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !dir.join("release").exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
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
pub(crate) fn lock_root(override_root: Option<&Path>) -> PathBuf {
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
    /// PRD #1487: this daemon's restart state — what it recorded about its own
    /// binary, the lock that serialises `restart-daemon` requests, and the
    /// accepted successor [`run_daemon_with`] spawns once the sockets are
    /// released. Every constructor records no install
    /// ([`crate::daemon_restart::InstallRecord::unresolved`]), so an in-process
    /// or test daemon never spawns its own harness; `daemon serve` sets the real
    /// one through [`Self::with_restart_control`].
    pub restart_control: Arc<crate::daemon_restart::RestartControl>,
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
            restart_control: Arc::default(),
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
            restart_control: Arc::default(),
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

    /// PRD #1487: use this restart control — `daemon serve` passes one built
    /// from [`crate::daemon_restart::InstallRecord::capture`]; a test passes one
    /// whose record points at a script of its own.
    pub fn with_restart_control(
        mut self,
        control: Arc<crate::daemon_restart::RestartControl>,
    ) -> Self {
        self.restart_control = control;
        self
    }
}

pub async fn run_daemon(socket_path: &Path, state: SharedState) -> Result<(), DaemonError> {
    run_daemon_with(socket_path, Daemon::new(state)).await
}

/// PRD #1666: the environment settings of the typed delivery the inbox
/// replaced. Each one used to tune a wait or a retry of a prompt the deck typed
/// into a pane; the deck types nothing but the doorbell now, so none of them
/// does anything.
pub const REMOVED_DELIVERY_SETTINGS: &[&str] = &[
    "DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS",
    "DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS",
    "DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS",
    "DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS",
    "DOT_AGENT_DECK_SEED_FALLBACK_SECS",
    "DOT_AGENT_DECK_SESSION_START_WAIT_MS",
];

/// Log one `warn!` for each [`REMOVED_DELIVERY_SETTINGS`] entry `is_set`
/// reports, and return their names.
fn warn_removed_delivery_settings(is_set: impl Fn(&str) -> bool) -> Vec<&'static str> {
    let set: Vec<&'static str> = REMOVED_DELIVERY_SETTINGS
        .iter()
        .copied()
        .filter(|name| is_set(name))
        .collect();
    for name in &set {
        warn!(
            setting = %name,
            "{name} no longer has any effect: messages go through the agent's inbox"
        );
    }
    set
}

#[cfg(test)]
mod removed_delivery_settings_tests {
    use super::*;

    /// PRD #1666 (D-7): a removed delivery setting that is set is named, and
    /// one that is not set, or any other variable, is not.
    #[test]
    fn removed_delivery_settings_that_are_set_are_named() {
        let named = warn_removed_delivery_settings(|name| {
            name == "DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS" || name == "DOT_AGENT_DECK_SOCKET"
        });
        assert_eq!(named, ["DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS"]);
        assert!(warn_removed_delivery_settings(|_| false).is_empty());
    }
}

/// Same as `run_daemon` but lets callers (and tests) inject a pre-built
/// `Daemon` so they can hold a clone of the PTY registry alongside it.
/// If `daemon.attach_socket_path` is set, the M1.2 streaming attach server
/// is spawned alongside the hook-ingestion loop and aborted when this
/// function returns.
pub async fn run_daemon_with(socket_path: &Path, daemon: Daemon) -> Result<(), DaemonError> {
    // PRD #1666 part 2 (G-fix6): a daemon names its process on every `Hello`,
    // and a client binds each action on an agent to that name. A process that
    // can mint no name a successor could not repeat refuses to start, and says
    // why, rather than serve agents no client could safely act on.
    if let Err(reason) = crate::daemon_protocol::try_daemon_instance_id() {
        tracing::error!(
            reason,
            "the daemon cannot identify its process; not starting"
        );
        return Err(DaemonError::Io(std::io::Error::other(format!(
            "the daemon cannot identify its process, so it is not starting: {reason}"
        ))));
    }
    // PRD #1666 (decision D-7): settings of the typed delivery the inbox
    // replaced do nothing now; say so once rather than ignore them silently.
    warn_removed_delivery_settings(|name| std::env::var_os(name).is_some());
    // PRD #819 M3: capture this daemon's own working directory ONCE, here, at
    // startup — the first of the four seeds its project enumeration draws on.
    // A process's cwd is not guaranteed stable, so reading it lazily at request
    // time would make the answer depend on when it was asked. It is only a
    // seed: a daemon started by systemd/launchd with `cwd=/` contributes
    // nothing, and every candidate is revalidated before it is offered.
    crate::project_resolve::capture_daemon_startup_cwd();
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
    //
    // PRD #741 M3: the presence is named rather than derived from the platform.
    // A daemon binds the endpoint it serves on, so it is always the LOCAL one —
    // there is no arm of this function that could be handed a remote deck's
    // address, and naming the constant says so at the call site.
    if crate::platform::ipc::stale_endpoint_artifact(
        crate::platform::ipc::LOCAL_ENDPOINT_PRESENCE,
        socket_path,
    ) {
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

    // Issue #1121: this is a bind site, so it is one of the places that owns
    // creating the fallback endpoint directory. A no-op for every other
    // endpoint — an override, an `$XDG_RUNTIME_DIR` path, a test's tempdir
    // socket, a Windows named pipe — see `ensure_endpoint_dir`.
    crate::endpoint_resolve::ensure_endpoint_dir(socket_path)?;
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
    // PRD #1666: the inbox, addressed by this deck's persisted id. A state
    // directory that cannot hold the id costs it persistence, not the inbox.
    {
        let state_dir = crate::config::state_dir();
        let deck = crate::inbox::DeckId::load_or_create(&state_dir).unwrap_or_else(|error| {
            warn!(%error, "could not load or create the deck id; using one for this run only");
            crate::inbox::DeckId::generate()
        });
        let hub = Arc::new(crate::inbox_hub::InboxHub::from_env(deck));
        // PR #1735 fifth review (B-05): the saved bodies of long messages are
        // listed for this run, and those earlier runs left are taken over.
        hub.open_spill_ledger(&state_dir);
        // PRD #1666 §3: this run's receipt spool, with the instances earlier
        // runs left reaped, a step at a time. A spool that cannot start costs
        // suppression, which every card it affects says, with the reason, not
        // delivery.
        match crate::inbox_receipt::Spool::start(&state_dir) {
            Ok(spool) => {
                hub.set_spool(spool);
                hub.spawn_spool_reaper();
            }
            Err(error) => {
                warn!(%error, "could not start the inbox receipt spool; deck messages may repeat");
                hub.set_spool_error(&error.to_string());
            }
        }
        // PRD #1666 part 3, round 5 (plan D3, D14): the store that keeps
        // reports and dispatch results whose recipient may be gone, with the
        // files of daemon runs that ended taken over. A directory it cannot
        // use makes every report it would keep refused, which it logs.
        let retained_dir = std::path::absolute(&state_dir).unwrap_or_else(|_| state_dir.clone());
        let retained = crate::retained_store::RetainedStore::open(
            &retained_dir,
            crate::retained_store::StoreConfig::from_env(),
            crate::retained_store::Now::current(),
        );
        hub.set_retained_store(std::sync::Arc::clone(&retained));
        drop(crate::retained_store::spawn_maintenance(
            std::sync::Arc::clone(&hub),
            retained,
        ));
        pty_registry.install_inbox(hub);
    }
    let inbox_ticker = pty_registry.inbox().spawn_ticker();
    // PRD #1589 D8: and which attach endpoint, so an agent's `close` (and every
    // other attach-socket call it makes) reaches THIS daemon. The configured
    // primary path as given, or `None` for a hook-only daemon — whose children
    // are then pointed nowhere rather than at the default endpoint.
    pty_registry.set_attach_socket(daemon.attach_socket_path.clone());
    let state = daemon.state;
    // PRD #1666 part 3, round 5-wake (plan §3.4′): the doorbell's
    // re-projection publisher, before anything can ask it to run.
    let doorbell_publisher = pty_registry.spawn_doorbell_publisher();
    // PRD #1666 S4: the doorbell, for agents no inbox component is armed for.
    let doorbell = crate::doorbell::spawn(
        Arc::clone(&pty_registry),
        state.clone(),
        crate::doorbell::DoorbellConfig::from_env(),
    );
    // PRD #1666 S11: hands an offered job back in its pool to the next taker.
    let offer_assigner =
        crate::inbox_pubsub::spawn_offer_assigner(Arc::clone(&pty_registry), state.clone());
    let restart_control = daemon.restart_control;
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
    // Issue #1540: the deck's last New-agent-form command, loaded from this
    // daemon's state directory so it survives a restart. A missing or bad file
    // — and, on Unix, a symlink or a file another user owns — loads as "none"
    // and never stops the daemon; see `crate::last_command`.
    // Read synchronously, like `schedules.toml` below: one small, bounded,
    // `O_NONBLOCK` read before the daemon serves anything.
    {
        let store = crate::last_command::LastCommandStore::load(
            crate::last_command::LastCommandStore::default_path(),
        );
        state.write().await.set_last_command_store(Arc::new(store));
    }
    let event_tx = daemon.event_tx;
    // PRD #1741: OTLP export of agent runs, read from this process's
    // environment. Off unless it names an endpoint: no task, no client, and no
    // variable added to any agent.
    let telemetry = crate::telemetry::start_for_daemon(&event_tx, &pty_registry);
    // PRD #1666 S3: every inbox state change reaches attached clients.
    pty_registry.inbox().set_broadcast(event_tx.clone());
    // PRD #1666 part 2 §2.7: what a card tells the user to run when an
    // agent's hooks run a dot-agent-deck too old for its messages, composed
    // from this deck's own install path.
    {
        let deck = state.read().await.hook_binaries.deck().clone();
        for agent in [
            crate::event::AgentType::ClaudeCode,
            crate::event::AgentType::Codex,
        ] {
            let remedy = crate::hook_binary::remedy_for(&deck, false, std::slice::from_ref(&agent));
            pty_registry
                .inbox()
                .set_agent_hooks_remedy(&agent, remedy.command);
        }
    }
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
    let signal_handle = spawn_termination_signal_watch(
        shutdown.clone(),
        Arc::downgrade(&pty_registry),
        state.clone(),
        restart_control.clone(),
    );

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
        // Issue #1121: the attach endpoint's own bind site. Idempotent with the
        // hook endpoint's call above — in the fallback case both sockets live
        // in the same per-uid directory.
        crate::endpoint_resolve::ensure_endpoint_dir(&path)?;
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
        let attach_restart = restart_control.clone();
        // Issue #1794: a daemon whose attach server ended must not stay up
        // serving hooks only, since then no client can reach it and no
        // replacement can start. Put the listener back; stop when that fails.
        let lost = crate::attach_recovery::stop_after_attach_lost(
            shutdown.clone(),
            pty_registry.clone(),
            state.clone(),
            restart_control.clone(),
        );
        Some(tokio::spawn(crate::attach_recovery::supervise(
            path,
            crate::config::state_dir(),
            listener,
            move |listener| {
                crate::daemon_protocol::serve_attach_with_restart(
                    listener,
                    registry.clone(),
                    attach_event_tx.clone(),
                    attach_counter.clone(),
                    attach_state.clone(),
                    Some(attach_shutdown.clone()),
                    attach_scheduler.clone(),
                    attach_reuse.clone(),
                    attach_worktrees.clone(),
                    attach_restart.clone(),
                )
            },
            move || lost,
        )))
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

    // Issue #714: unconditional for the same reason — a Codex turn that its
    // provider's usage limit ended says so only in its rollout, which no hook
    // reports. Idle unless a Codex turn is armed.
    let codex_rollout_handle = {
        let registry = pty_registry.clone();
        let monitor_state = state.clone();
        let monitor_event_tx = event_tx.clone();
        tokio::spawn(async move {
            run_codex_rollout_monitor(registry, monitor_state, monitor_event_tx).await;
        })
    };

    // PRD #1401: unconditional — every agent on a branch with a pull request
    // gets a badge, and this is the only place that answer comes from. It runs
    // `gh` only for a live session on a non-default branch of a GitHub
    // repository; the `git` probes that find each session's branch still run
    // for every live session with a working directory, once an interval and
    // sooner after a turn ends.
    let pull_request_handle = {
        let registry = pty_registry.clone();
        let monitor_state = state.clone();
        let monitor_event_tx = event_tx.clone();
        let interval = crate::pull_request::refresh_interval_from(
            std::env::var(crate::pull_request::DOT_AGENT_DECK_PR_REFRESH_SECS_ENV)
                .ok()
                .as_deref(),
        );
        tokio::spawn(async move {
            crate::pull_request::run_pull_request_monitor(
                registry,
                monitor_state,
                monitor_event_tx,
                interval,
            )
            .await;
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
    telemetry.shutdown(Duration::from_secs(3)).await;

    if let Some(h) = attach_handle {
        h.abort();
    }
    if let Some(h) = idle_handle {
        h.abort();
    }
    shell_activity_handle.abort();
    codex_rollout_handle.abort();
    pull_request_handle.abort();
    scheduler_handle.abort();
    inbox_ticker.abort();
    offer_assigner.abort();
    doorbell.abort();
    if let Some(publisher) = doorbell_publisher {
        publisher.abort();
    }
    if let Some(h) = orphan_handle {
        h.abort();
    }
    if let Some(h) = max_lifetime_handle {
        h.abort();
    }
    // The signal watch is NOT aborted here (PRD #1487 audit A2): a signal
    // between now and the end of the successor work must still claim its stop
    // before the plan is decided, and a second one must still force-exit. It is
    // aborted once that work is done, below.
    drop(pty_registry);

    // PRD #1487: an accepted `restart-daemon` in `Installed` mode left its
    // verified target here. Spawn it only now — the hook loop has returned,
    // the attach servers are aborted and the aliases unlinked — because the
    // successor's bind refuses while this daemon's socket is alive. It inherits
    // this process's environment, so it binds the same endpoint. If the spawn
    // fails no daemon is left running: the agents were stopped by consent, and
    // the next client lazy-spawns from its own binary.
    //
    // Under a service manager the successor is not ours to start: systemd
    // would kill it with this unit's cgroup. `daemon serve` reads
    // `handed_to_supervisor` and exits non-zero instead, so the manager runs
    // the unit's own command — the installed build — again.
    // A `StopDaemon` still writing its acknowledgement holds a pending stop
    // claim; wait for it so a stop that is about to be withdrawn does not
    // cancel the restart, bounded so a stalled peer cannot hold the exit. The
    // plan reads the claims as they are when it is decided: one still pending
    // counts as a stop, one withdrawn by then does not.
    #[cfg(feature = "e2e")]
    e2e_successor_plan_gate().await;
    restart_control
        .settle_stop_claims(crate::daemon_restart::STOP_CLAIM_SETTLE)
        .await;
    match restart_control.take_successor_plan() {
        crate::daemon_restart::SuccessorPlan::Nothing => {}
        crate::daemon_restart::SuccessorPlan::Spawn(target) => {
            // Audit A5: the drain took a while, and an installer may have
            // replaced the file since it was verified. Spawn it only if it is
            // still the verified file, or it verifies again as the same
            // version; otherwise start nothing — the client's Verifying stage
            // reports that no new daemon answered, and the next client
            // lazy-spawns its own.
            //
            // Taking the plan committed the restart, so a stop handled from
            // here on changes nothing (`RestartControl::take_successor_plan`).
            // The re-check can run `--version` for up to its timeout and the
            // spawn forks, so both run on a blocking worker, not the executor
            // (Qodo 4201540974).
            let control = restart_control.clone();
            let spawning = target.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let check = control.recheck_successor(&spawning)?;
                let spawned = crate::daemon_attach::spawn_restart_successor(
                    &crate::config::state_dir(),
                    &spawning,
                );
                Ok::<_, String>((check, spawned))
            })
            .await
            .unwrap_or_else(|e| Err(format!("the successor spawn task failed ({e})")));
            match outcome {
                Err(reason) => error!(
                    target = %target.display(),
                    %reason,
                    "not spawning the successor daemon; none is running until a client starts one"
                ),
                Ok((check, spawned)) => {
                    if let crate::daemon_restart::SuccessorCheck::Reverified(version) = &check {
                        warn!(
                            target = %target.display(),
                            %version,
                            "the installed build changed after it was verified; it verified again"
                        );
                    }
                    match spawned {
                        Ok(pid) => {
                            info!(pid, target = %target.display(), "successor daemon spawned")
                        }
                        Err(e) => error!(
                            target = %target.display(),
                            error = %e,
                            "could not spawn the successor daemon; none is running until a client starts one"
                        ),
                    }
                }
            }
        }
        crate::daemon_restart::SuccessorPlan::LeaveToSupervisor(supervisor) => info!(
            ?supervisor,
            exit_status = crate::daemon_restart::SUPERVISED_RESTART_EXIT,
            "restart accepted under a service manager; exiting for it to start the installed build"
        ),
    }
    if let Some(h) = signal_handle {
        h.abort();
    }

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
        // `None` here even when the task DOES carry a `shape` (issue #835): the
        // target is resolved per fire, inside the callback below, not once at
        // registration. See the resolution block there for why.
        resolved_target: None,
        // Unchanged behaviour: the prompt is delivered verbatim. Giving this path
        // the orchestrator context is #222's work, not this PR's — and when it is
        // done, the value that belongs here is `Unattended`: a scheduled task
        // fires with no one at the pane (issue #703).
        compose_orchestrator_context: None,
        seed_reason: None,
    };
    let new_tab_per_fire = task.new_tab_per_fire;
    // Issue #835: the task's declared spawn shape, `None` for every task that does
    // not set one. Parsed at LOAD (`config::validate_task`), so an entry that
    // reaches here carries a value this re-parse cannot reject — but it is
    // re-parsed rather than stored as a `SpawnShapeOverride` so `ScheduledTask`
    // stays a plain serde struct with no spawn-layer type in it.
    let shape = task.shape.clone();
    Arc::new(move || {
        let registry = registry.clone();
        let reuse = reuse.clone();
        let mut req = req.clone();
        let shape = shape.clone();
        // PRD #127 finding #2: hand the daemon-wide hook-event broadcast to the
        // fire so a fresh single-agent card surfaces LIVE to an already-attached
        // TUI (see `crate::spawn::surface_spawned_pane`).
        let event_tx = event_tx.clone();
        let state = state.clone();
        Box::pin(async move {
            let notifier = crate::scheduler::StderrNotifier;
            // Issue #835: resolve the declared shape against the target dir's
            // config AS IT STANDS NOW.
            //
            // At fire time, not at registration: a schedule is authored once and
            // fires for months, and `working_dir`'s `.dot-agent-deck.toml` can gain,
            // rename or drop an orchestration in between. Resolving at registration
            // would pin the answer to whatever the config said when the daemon last
            // reloaded. (`dispatch` resolves caller-side for the opposite reason —
            // its worktree's config is a HEAD checkout that differs from the
            // caller's; a schedule has no such split, it simply targets a directory.)
            //
            // A shape that cannot be resolved ABANDONS the fire through the same
            // `Notifier` seam every other fire failure uses. It must never fall back
            // to the config-derived target: silently spawning a shape other than the
            // one the author wrote is precisely the defect this field removes, and a
            // fallback here would reproduce it with extra steps.
            if let Some(raw) = shape.as_deref() {
                // Scoped: the trait is needed only for this `notify` call, and the
                // module deliberately imports `Scheduler` alone.
                use crate::scheduler::Notifier as _;

                let dir = Path::new(&req.working_dir);
                // `load_config_for_dir` flattens a PARSE failure into `None`,
                // which is the right reading for the config-derived path ("no
                // usable config" → single-agent card) but the wrong one here: a
                // declared `shape = "orchestration"` would then be refused with
                // "no orchestration with roles is defined", sending an operator
                // hunting for a missing section when the real fault is a broken
                // file. Read the `Result` so the two causes stay distinguishable
                // in the notification, which is the only record an unattended
                // fire leaves.
                let resolved = crate::spawn::SpawnShapeOverride::parse(raw).and_then(|over| {
                    let config = crate::project_config::load_project_config(dir).map_err(|e| {
                        format!("could not read {}/.dot-agent-deck.toml: {e}", dir.display())
                    })?;
                    crate::spawn::decide_target_with_override(
                        config.as_ref(),
                        dir,
                        req.command.as_deref(),
                        Some(&over),
                    )
                });
                match resolved {
                    Ok(target) => req.resolved_target = Some(target),
                    Err(message) => {
                        let message = format!("shape {raw:?}: {message}");
                        warn!(
                            task = %req.task_name,
                            dir = %req.working_dir,
                            "scheduled fire abandoned: {message}"
                        );
                        notifier.notify(crate::scheduler::NotifyEvent::SpawnFailed {
                            task: req.task_name.clone(),
                            message,
                        });
                        return;
                    }
                }
            }
            let debounce = crate::spawn::reuse_debounce();
            loop {
                match crate::spawn::spawn_or_reuse(
                    req.clone(),
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
                    Ok(()) => break,
                    // PRD #1487 re-check, reviewer R3: refused because a restart
                    // holds the admission freeze. Not a failure of this task, so
                    // it is not reported as one: retried if the restart is called
                    // off, and left to the successor daemon's next fire if it
                    // goes ahead.
                    Err(crate::spawn::SpawnError::DaemonRestarting) => {
                        info!(
                            task = %req.task_name,
                            "scheduled fire deferred: the daemon is restarting"
                        );
                        if !registry.wait_for_admission().await {
                            info!(
                                task = %req.task_name,
                                "the daemon is going down; this fire is left to its successor"
                            );
                            break;
                        }
                        info!(task = %req.task_name, "restart called off; firing again");
                    }
                    Err(e) => {
                        // Already surfaced via the notifier; log for the operator.
                        warn!(error = %e, "scheduled spawn failed");
                        break;
                    }
                }
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
///
/// Issue #860: a timer that wakes on an UNMOVED generation but finds the
/// joint-zero gate busy declines to signal shutdown — and used to die there,
/// while the monitor's `armed` flag stayed true. The monitor's next reading
/// then found the daemon idle with (as far as it knew) a timer already in
/// flight, so it armed nothing, and no later reading ever would: the daemon
/// was immortal with zero clients, zero agents and zero pending schedules.
/// Reaching that state needs only a transient the monitor does not observe,
/// which tokio's `Notify` makes ordinary — it stores at most ONE permit, so a
/// connect and a disconnect landing between the monitor's readings coalesce
/// into a single wake-up whose reading is the settled, idle state, while the
/// timer waking on its own deadline sees the busy moment between them. The
/// declining timer therefore now clears `armed` (shared, generation-guarded)
/// and wakes the monitor, which arms a fresh window. Retrying is the only
/// change: the gate itself is untouched, so a shutdown still requires the
/// full joint-zero re-check to pass under an unmoved generation.
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
    // Issue #860: shared with every in-flight timer, not a local `bool`. A
    // timer that DECLINES to signal shutdown (its joint-zero re-check found
    // the gate busy) clears this and wakes the monitor, so the monitor arms a
    // fresh timer. While this was a local `bool` the monitor went on believing
    // a timer was still in flight and never armed another one — leaving the
    // daemon immortal with no clients, no agents and no pending schedules.
    let armed = Arc::new(AtomicBool::new(false));

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
            if !armed.load(Ordering::SeqCst) {
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
                let timer_armed = armed.clone();
                let timer_wake = change_notify.clone();
                let dur = threshold;
                // Set before spawning: the timer must never observe a stale
                // `false` and re-arm on the monitor's behalf.
                armed.store(true, Ordering::SeqCst);
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
                        return;
                    }
                    // Issue #860: the gate was busy at the deadline, so this
                    // timer declines. Hand arming back to the monitor rather
                    // than dying silently — otherwise nothing ever arms
                    // another timer and the daemon never exits, however long
                    // it stays idle afterwards.
                    //
                    // Claiming the decline is ONE atomic step, not a re-read
                    // of the generation followed by a store. Arming bumps the
                    // generation, so a successful compare-exchange proves no
                    // replacement timer was armed between this timer's
                    // deadline and this instant — and by bumping the
                    // generation itself it retires this timer's own claim in
                    // the same operation. A plain `load() == my_gen` guard
                    // (Greptile P1 on PR #865) leaves a window: a timer
                    // descheduled between the load and the store can clear a
                    // REPLACEMENT timer's `armed` flag, and the monitor's next
                    // busy reading would then skip its generation bump —
                    // that branch only fires when `armed` — leaving the
                    // replacement live across a busy period that should have
                    // invalidated it, so it could fire on a window the daemon
                    // did not stay idle through.
                    if gen_check
                        .compare_exchange(my_gen, my_gen + 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        debug!(
                            threshold_secs = dur.as_secs(),
                            "daemon idle timer declined (gate busy at the deadline); re-arming"
                        );
                        timer_armed.store(false, Ordering::SeqCst);
                        timer_wake.notify_one();
                    }
                });
            }
        } else if armed.load(Ordering::SeqCst) {
            // 0→1 transition: invalidate the in-flight timer by bumping
            // the generation. The timer task is still scheduled; it'll
            // wake at its old deadline, see the mismatch, and exit
            // silently. No await needed.
            generation.fetch_add(1, Ordering::SeqCst);
            armed.store(false, Ordering::SeqCst);
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
pub async fn ingest_event(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    event: AgentEvent,
) {
    ingest_event_unless(state, event_tx, registry, event, false, None, |_| false).await;
}

/// Issue #1637: record which `dot-agent-deck` sent an admitted hook line
/// (its `deck_build` / `deck_exe` stamp, or the absence of one), and broadcast
/// the notices when that changes them. Under the state's write guard, so a
/// `SubscribeEventsWithSnapshot` reply holds a change or its stream does.
/// Answers whether the line carried a stamp.
async fn observe_hook_sender(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    agent_type: &crate::event::AgentType,
    line: &str,
) -> bool {
    let sender = crate::hook_binary::HookLineSender::from_line(line);
    let stamped = sender.is_reported();
    // The steady state — an agent's hooks sending the stamp they sent last time
    // — changes nothing, so it is settled under a read guard and never queues
    // behind or blocks the event's own write.
    if !state
        .read()
        .await
        .hook_binaries
        .would_change(agent_type, &sender)
    {
        return stamped;
    }
    let mut state = state.write().await;
    if state.hook_binaries.observe(agent_type, &sender) {
        let _ = event_tx.send(BroadcastMsg::HookBinaryNotice(
            crate::hook_binary::HookBinaryNotices {
                notices: state.hook_binaries.notices(),
            },
        ));
    }
    stamped
}

/// Issue #1637: every [`crate::hook_binary::HOOK_PIN_REFRESH_INTERVAL`], read
/// the agents' hook configs again for the binaries they pin, and apply what
/// changed to the notices. `daemon serve` runs it for the daemon's lifetime.
///
/// The configs are read, and any new pin probed, on a detached thread before
/// the state lock is taken ([`crate::hook_binary::PinRefresh::collect`],
/// [`spawn_detached_collection`]); the result is then applied under the write
/// lock in one step, which touches no file ([`apply_hook_pin_refresh`]). The
/// first read is one interval after start, since startup has just read the
/// same configs.
///
/// The thread is detached rather than a `spawn_blocking` task because the
/// runtime's teardown waits for every blocking task it started, so a read
/// stuck in the filesystem would hold the daemon's exit (audit A9). Aborting
/// this task drops only the wait; the thread finishes, or is ended with the
/// process. Only one read runs at a time: this loop waits for it, and a read
/// left running by an aborted loop holds [`HOOK_PIN_COLLECTING`] until it ends.
pub async fn run_hook_pin_refresh(state: SharedState, event_tx: broadcast::Sender<BroadcastMsg>) {
    let period = crate::hook_binary::HOOK_PIN_REFRESH_INTERVAL;
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let deck = state.read().await.hook_binaries.deck().clone();
        let Some(collected) = spawn_detached_collection(&HOOK_PIN_COLLECTING, move || {
            crate::hook_binary::PinRefresh::collect(&deck)
        }) else {
            tracing::warn!(
                "hook pin refresh: the previous read of the agents' hook configs has not finished; skipped"
            );
            continue;
        };
        let refresh = match collected.await {
            Ok(refresh) => refresh,
            Err(_) => {
                tracing::warn!("hook pin refresh: reading the agents' hook configs failed");
                continue;
            }
        };
        apply_hook_pin_refresh(&state, &event_tx, refresh).await;
    }
}

/// Set while a hook-pin read started by [`run_hook_pin_refresh`] is running.
static HOOK_PIN_COLLECTING: AtomicBool = AtomicBool::new(false);

/// Run `collect` on a detached thread and return a receiver for its result,
/// unless `in_flight` says a previous one is still running, or no thread
/// could be made: `None` then, and nothing runs. `in_flight` is set until
/// `collect` returns or panics; a panic drops the sender, which the receiver
/// reports as an error.
fn spawn_detached_collection<T: Send + 'static>(
    in_flight: &'static AtomicBool,
    collect: impl FnOnce() -> T + Send + 'static,
) -> Option<tokio::sync::oneshot::Receiver<T>> {
    struct Release(&'static AtomicBool);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    if in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let release = Release(in_flight);
    let spawned = std::thread::Builder::new()
        .name("hook-pin-refresh".into())
        .spawn(move || {
            // Released before the result is sent, so a caller that has the
            // result may start the next one; a panic releases it unwinding.
            let value = collect();
            drop(release);
            let _ = tx.send(value);
        });
    match spawned {
        Ok(_) => Some(rx),
        Err(e) => {
            // The closure, and with it `release`, was dropped with the error.
            tracing::warn!("hook pin refresh: could not start a thread to read the configs: {e}");
            None
        }
    }
}

/// Apply one [`crate::hook_binary::PinRefresh`] under the state's write guard,
/// and broadcast the notices when that changes them.
async fn apply_hook_pin_refresh(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    refresh: crate::hook_binary::PinRefresh,
) {
    let mut state = state.write().await;
    if state.hook_binaries.apply_refresh(refresh) {
        let _ = event_tx.send(BroadcastMsg::HookBinaryNotice(
            crate::hook_binary::HookBinaryNotices {
                notices: state.hook_binaries.notices(),
            },
        ));
    }
}

/// [`ingest_event`] for a raw event the hook socket's provenance gate admitted
/// (issue #318). `unproven` is the gate's verdict that the event comes from an
/// outside agent — a pane, or a paneless agent, this daemon never issued a hook
/// capability token for — and is stamped on the event for attached clients
/// ([`crate::event::UNPROVEN_METADATA_KEY`]).
///
/// `attested_agent` is the agent the event's token was minted for, when the
/// gate attested it. For an event that names no agent it is what admission is
/// judged by (audit finding 2), and it is asked again here, under the state
/// lock and before the fan-out: the gate asked first, but a successor can
/// claim the pane in between, and an attached client — which has no registry —
/// would then credit the untagged report to that successor. Asking again
/// narrows that window without closing it, since a respawn is not ordered
/// against this lock; what closes it on a client is the attested-owner marker
/// stamped in [`ingest_event_unless`] (`ATTESTED_OWNER_METADATA_KEY`). A report
/// the second asking drops is logged with its reason code.
async fn ingest_hook_event(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    event: AgentEvent,
    unproven: bool,
    attested_agent: Option<String>,
) {
    let untagged_claim = match (&attested_agent, &event.agent_id, &event.pane_id) {
        (Some(owner), None, Some(pane)) => Some((owner.clone(), pane.clone())),
        _ => None,
    };
    let replaced = || {
        untagged_claim.as_ref().is_some_and(|(owner, pane)| {
            !crate::hook_provenance::HookTokenDirectory::token_owner_speaks_for_pane(
                &**registry,
                owner,
                pane,
            )
        })
    };
    // PRD #1666 part 3, round 5-wake (plan §4.6): the generation's first
    // `UserPromptSubmit`, attested to it, closes its header window.
    if event.user_prompt.is_some()
        && let Some(owner) = attested_agent.as_deref()
    {
        registry.close_release_window(owner);
    }
    let event_type = event.event_type.clone();
    let claimed_pane = event.pane_id.clone();
    let ingested = ingest_event_unless(
        state,
        event_tx,
        registry,
        event,
        unproven,
        attested_agent.as_deref(),
        |_| replaced(),
    )
    .await;
    if !ingested {
        // Qodo, PR #1559: logged like the gate's own refusals — the escaped
        // pane and a stable reason code, never the token — so a report the
        // under-lock re-check drops does not vanish without a trace.
        warn!(
            verb = "agent_event",
            event_type = ?event_type,
            claimed_pane = %escape_id_for_log(claimed_pane.as_deref().unwrap_or("<none>")),
            reason = "token_generation_replaced",
            "hook socket: dropped a status event naming no agent whose hook \
             capability token's generation no longer holds the pane it names; \
             see docs/develop/hook-provenance.md"
        );
    }
}

/// [`ingest_event`], except that `stale` is asked once the `AppState` write
/// lock is held, with the state it guards, and a `true` drops the event before
/// it is broadcast or applied. Asked under that lock so nothing the lock orders
/// can slip between the verdict and the apply — which is also why it gets the
/// state: the shell-activity monitor records its sample there in the same hold
/// (issue #1678). Returns whether the event was ingested.
///
/// An event is ingested holding the question lifecycle of the pane it lands
/// on (PRD #1631, audit R1–R3 —
/// [`crate::agent_pty::QuestionAnswers::lock_lifecycle`]): any event can
/// raise, replace or clear the pane's question, so none may land between a
/// question transition's check and its commit. That is the pane the event
/// names, or — for one that names none — the pane of the session it resolves
/// to (audit T2): a pane-less event is applied to its session by id and can
/// clear a pane-backed session's question just as well. The resolution is read
/// before the lifecycle is taken, so it is checked again under the `AppState`
/// lock that applies the event, and the lifecycle retaken if a concurrent
/// event moved the session in between. A pane-less event whose session has no
/// pane takes no lifecycle. A caller that already holds the lifecycle uses
/// [`ingest_event_in_lifecycle`] instead.
///
/// `unproven` and `attested_agent` as for [`ingest_hook_event`]; `false` and
/// `None` for every event the daemon synthesises itself.
pub(crate) async fn ingest_event_unless<F>(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    mut event: AgentEvent,
    unproven: bool,
    attested_agent: Option<&str>,
    mut stale: F,
) -> bool
where
    F: FnOnce(&mut crate::state::AppState) -> bool,
{
    loop {
        let pane = match event.pane_id.clone() {
            Some(pane_id) => Some(pane_id),
            None => state.read().await.session_pane(&event.session_id),
        };
        let lifecycle = match pane.as_deref() {
            Some(pane_id) => Some(registry.question_answers().lock_lifecycle(pane_id).await),
            None => None,
        };
        if event.pane_id.is_none() {
            registry
                .question_answers()
                .barrier("ingest:paneless_resolved")
                .await;
        }
        match ingest_event_core(
            state,
            event_tx,
            registry,
            lifecycle.as_ref().map(|held| held.pane_id()),
            event,
            None,
            unproven,
            attested_agent,
            stale,
        )
        .await
        {
            Ok(ingested) => return ingested,
            Err(unapplied) => (event, stale) = *unapplied,
        }
    }
}

/// [`ingest_event_unless`] for a caller that already holds `lifecycle`, the
/// question lifecycle of the pane the event names. `registration` is the generation of the hold this event
/// publishes, captured when the hold was registered (audit R1): the question is
/// stamped with it, and the event is refused — not ingested at all — when that
/// registration is no longer the one held for its pane and id.
pub(crate) async fn ingest_event_in_lifecycle(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    lifecycle: &crate::agent_pty::QuestionLifecycle,
    event: AgentEvent,
    registration: Option<u64>,
    stale: impl FnOnce(&mut crate::state::AppState) -> bool,
) -> bool {
    debug_assert!(
        event
            .pane_id
            .as_deref()
            .is_some_and(|pane_id| pane_id == lifecycle.pane_id()),
        "an event is ingested under its own pane's question lifecycle"
    );
    // Every caller's event names the lifecycle's pane, so it resolves to it.
    ingest_event_core(
        state,
        event_tx,
        registry,
        Some(lifecycle.pane_id()),
        event,
        registration,
        false,
        None,
        stale,
    )
    .await
    .unwrap_or(false)
}

/// The body of [`ingest_event_unless`]; its caller holds the question
/// lifecycle of `held`, the pane it resolved the event to (`None`: none). Under
/// the `AppState` write lock, before anything else, the event is resolved again
/// — the pane it names, else its session's — and handed back unapplied, with
/// `stale` unasked, when that is not `held` (audit T2). Handed back boxed,
/// since an [`AgentEvent`] makes the pair large and it comes back only when the
/// resolve moved.
#[allow(clippy::too_many_arguments)]
async fn ingest_event_core<F>(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    held: Option<&str>,
    mut event: AgentEvent,
    registration: Option<u64>,
    unproven: bool,
    attested_agent: Option<&str>,
    stale: F,
) -> Result<bool, Box<(AgentEvent, F)>>
where
    F: FnOnce(&mut crate::state::AppState) -> bool,
{
    let mut state = state.write().await;
    // Audit T2: the pane whose question this event can change, which is the
    // one whose lifecycle must be held.
    let question_pane = match event.pane_id.clone() {
        Some(pane_id) => Some(pane_id),
        None => state.session_pane(&event.session_id),
    };
    if question_pane.as_deref() != held {
        return Err(Box::new((event, stale)));
    }
    if stale(&mut state) {
        return Ok(false);
    }
    // PRD #1631 (audit R1): an event publishing a hold is ingested only while
    // that registration is still the one held for its pane and question id.
    if let Some(generation) = registration {
        let current = match (event.pane_id.as_deref(), event.question()) {
            (Some(pane_id), Some(question)) => registry
                .question_holds()
                .generation_of(pane_id, &question.id),
            _ => None,
        };
        if current != Some(generation) {
            warn!(
                pane_id = ?event.pane_id.as_deref().map(escape_id_for_log),
                generation,
                ?current,
                "question: a held question's registration was superseded before it was \
                 published; its event is not applied"
            );
            return Ok(false);
        }
    }
    // Issue #770: half of the orphan verdict (and, round 3, all of the
    // no-live-agent one stamped below), asked of the registry UNDER the
    // `AppState` write lock, at the stamp that spends it (round-2 audit finding
    // 3). Sampled before the lock, as it once was, the answer went stale while
    // the event waited: the deck could spawn an agent on the pane in between,
    // and the report was then stamped as coming from an orphaned role pane —
    // which an attached client lets update the pane's card — although a live
    // agent of the deck's own now held it. Taking the registry's mutex while
    // holding this lock is the nesting this path already takes: `stale()`
    // above, the quota latch below and `apply_event`'s ownership oracle all
    // ask the registry under it, and no registry path waits on this lock while
    // holding the registry's mutex (`spawn_agent` releases it before any
    // caller takes the state lock to register a role).
    //
    // Issue #318: a reserved spawn counts as the daemon's too. Its token
    // resolves from the reservation, so its first report can arrive before the
    // record is published or the role registered, and must not be badged.
    let daemon_owns_pane = event
        .pane_id
        .as_deref()
        .is_some_and(|pane_id| registry.has_live_or_reserved_pane(pane_id));
    // Issue #714: keep the registry's per-agent quota-block latch in step with
    // the card. A `QuotaBlocked` latches a fresh epoch for the pane's live
    // owner — the key the orchestrator notice below is claimed and re-checked
    // against — and work evidence lifts it, the latch's half of the rule
    // `apply_event` applies to the card. Only the generation the event names is
    // credited. Done UNDER the `AppState` write lock on purpose: a block and a
    // work event are applied to the latch in the same order as to the card, so
    // the notice's write-time re-check can never see a latch the card
    // contradicts. Taking the registry inside this lock is the nesting
    // `apply_event`'s own ownership oracle already takes on every event, so it
    // adds no new lock order.
    let mut reported_block = None;
    // An outside agent's report moves no latch of the deck's own (audit
    // finding 3): its pane may have been issued to a deck agent after the gate
    // classified it, and the latch would then credit the forgery to that
    // agent.
    //
    // A work event lifts the latch of the generation that SENT it: the one it
    // names, or for a report that named none, the one its token was minted for
    // (round-2 audit suggestion 5). The pane alone would credit an untagged
    // report from a replaced generation to whoever holds the pane now.
    if !unproven && let Some(pane_id) = event.pane_id.as_deref() {
        let generation = event.agent_id.as_deref().or(attested_agent);
        if event.event_type == crate::event::EventType::QuotaBlocked {
            if let Some(agent_id) = event.agent_id.as_deref()
                && let Some(epoch) = registry.note_quota_block(pane_id, agent_id)
            {
                reported_block = Some((pane_id.to_string(), agent_id.to_string(), epoch));
            }
        } else if crate::quota_block::is_work_evidence(&event) {
            registry.quota_note_work_event(pane_id, generation);
        }
    }
    // PRD #1666: an Idle from the pane's own agent that ends a turn it
    // reported starting ends what a run of the user's Escape keys could have
    // been holding, the interrupt of that turn, so the doorbell stops reading
    // those keys as a draft (`DraftTracker::turn_ended`). Under this lock,
    // like the quota latch, so it reaches the tracker in the order the card
    // sees it.
    //
    // A turn starts with a report that the agent is working: thinking, a
    // tool starting, compacting or a permission request (r8k R8G-02). A tool
    // FINISHING starts nothing: a ToolEnd that arrives after its turn's Idle
    // would otherwise open a turn the next Idle "ends", releasing Escapes
    // that opened a menu meanwhile (Qodo r8, PR #1735), and the daemon cannot
    // tell it from the first report of a turn whose start it never saw. So a
    // turn whose start report is missing (lost, or its hook switched off)
    // releases nothing when it ends, and its Escapes hold until a submit or
    // the user's clear, the Enter the user docs give. An Idle with no turn
    // under way releases nothing: a repeated Idle (OpenCode sends two) must not
    // release the Escapes that opened a menu on an agent already Idle. A
    // line `dot-agent-deck wrap` classified from the child's output proves
    // only that it printed something, or stopped printing, so it neither
    // starts nor ends a turn: a wrapped Codex's quiet-output Idle does not
    // end a turn its hooks reported (Qodo r8). A wrapper-only agent
    // therefore has no turn end to release on, and its Escapes hold until a
    // submit or the user's clear.
    //
    // Only the generation that sent it counts, as for the quota latch: the
    // one it names, or the one its token was minted for, and the registry
    // applies it only while that generation is the pane's live owner, or
    // keeps it for a spawn reserved on the pane (r8g R8G-01). A report with
    // neither touches nothing. An event the daemon authored is no turn of the
    // agent's, and a subagent's turn is not the main thread's the user
    // interrupted.
    let turn_edge = match event.event_type {
        _ if event.is_wrapper_output_classified() => None,
        crate::event::EventType::Idle => Some(crate::agent_pty::TurnEdge::Ended),
        crate::event::EventType::Thinking
        | crate::event::EventType::ToolStart
        | crate::event::EventType::Compacting
        | crate::event::EventType::PermissionRequest => Some(crate::agent_pty::TurnEdge::Started),
        _ => None,
    };
    // PRD #1666 part 3 (Q30): a prompt the agent's own prompt hook reported
    // taking (Claude Code's `UserPromptSubmit`, a Thinking with its prompt,
    // or the deck's own line it recognised). It is the composer model's only
    // evidence that an Enter on a box it could not follow submitted it: a
    // turn starting is not, since an Enter on a box of blanks submits
    // nothing and a turn can start for another reason. The same generation
    // and live-owner checks as the turn edge, and before it.
    let prompt_report = match event.event_type {
        _ if event.is_wrapper_output_classified() => None,
        crate::event::EventType::Thinking if crate::inbox_prompt::is_deck_message(&event) => {
            Some(crate::composer_model::PromptReport::Deck)
        }
        crate::event::EventType::Thinking => event
            .user_prompt
            .as_deref()
            .map(crate::composer_model::PromptReport::User),
        _ => None,
    };
    if !unproven
        && let Some(report) = prompt_report
        && !event.is_daemon_synthetic()
        && !event.is_from_subagent()
        && let Some(pane_id) = event.pane_id.as_deref()
        && let Some(generation) = event.agent_id.as_deref().or(attested_agent)
    {
        registry.note_prompt_report(pane_id, generation, report);
    }
    // PRD #1666 part 3, round 5-wake (plan rev 2 §6): the wrapper's report of
    // how the command it ran exited, on its final event, for the notices its
    // generation's exit owes. The same generation checks as the turn edge;
    // the registry keeps it only for a wrapped generation, and the hook socket
    // only from that generation's own child
    // ([`keep_wrapper_report_from_own_child`]). That is not an authenticated
    // wrapper identity: a same-user process is inside the accepted trust
    // boundary (`crate::hook_provenance`).
    if !unproven
        && !event.is_daemon_synthetic()
        && let Some(ended) = event
            .metadata
            .get(crate::event::WRAPPER_CHILD_EXIT_METADATA_KEY)
        && let Some(pane_id) = event.pane_id.as_deref()
        && let Some(generation) = event.agent_id.as_deref().or(attested_agent)
    {
        registry.note_wrapper_child_exit(pane_id, generation, ended);
    }
    if !unproven
        && let Some(edge) = turn_edge
        && !event.is_daemon_synthetic()
        && !event.is_from_subagent()
        && let Some(pane_id) = event.pane_id.as_deref()
        && let Some(generation) = event.agent_id.as_deref().or(attested_agent)
        && let Some(seen) = registry.note_turn(pane_id, generation, edge)
    {
        // PRD #1666 G3: the inbox's turn clock, after the registry's locks
        // are released (the inbox asks the registry under its own lock) and
        // inside this `AppState` write lock, which serializes ingest, so
        // edges reach it in the order the registry numbered them. A reserved
        // spawn's edge counts too: its id is the generation's, so a turn it
        // reported before publication is on its clock from the start.
        registry
            .inbox()
            .note_turn(pane_id, generation, seen, std::time::Instant::now());
    }
    // The other half, plus the stamp: is this an orchestration role pane whose
    // role registration a daemon restart destroyed while its agent survived?
    // The verdict is the daemon's alone — any inbound value is dropped first,
    // because `metadata` rides an unauthenticated same-uid socket and a
    // producer must not be able to paint its own card (see
    // `ORCHESTRATION_ORPHANED_METADATA_KEY`).
    //
    // Stamped HERE, before the fan-out, so it reaches attached TUIs even when
    // the daemon's own `apply_event` declines the event — which is precisely
    // what happens for an orphan: admission control asks the registry, the
    // registry has never heard of the pane, and a non-`SessionStart` frame is
    // dropped. The card that needs the badge is an attached TUI's.
    state.stamp_orchestration_orphan(&mut event, daemon_owns_pane);
    // Issue #320: the registry's generation verdict, stamped for the same
    // reason and at the same point as the orphan marker — an attached TUI has
    // no registry, and without it ordered a takeover by the frame's type and
    // producer clock, so a late frame from the OUTGOING generation retired the
    // live card there while the daemon refused it here. See
    // `PANE_GENERATION_METADATA_KEY`.
    state.stamp_pane_generation(&mut event);
    // PRD #1223: the pane-closed marker is the daemon's alone too — it makes an
    // attached TUI drop the pane — so a producer's copy never reaches the
    // fan-out. The daemon's own removal is broadcast directly and never passes
    // through here (`crate::spawn::surface_attach_stopped_agent`).
    event
        .metadata
        .remove(crate::event::DAEMON_PANE_CLOSED_METADATA_KEY);
    // Issue #1678: so is the background-retirement marker, which makes an
    // attached client clear a card's background copy. The daemon's own report
    // is broadcast directly by the shell-activity monitor and never ingested.
    event
        .metadata
        .remove(crate::event::BACKGROUND_RETIRED_METADATA_KEY);
    // Issues #601 / #697: both unproven markers are the daemon's alone too. The
    // first comes only from the hook gate's own verdict; the second is only
    // ever on an eviction the daemon broadcasts below and never ingests.
    event
        .metadata
        .remove(crate::event::UNPROVEN_EVICTED_METADATA_KEY);
    // Round 3: the no-live-agent verdict is the orphan stamp's first half
    // without its role-pane half — `daemon_owns_pane`, sampled above under this
    // lock — so a client attached across a restart keeps a PLAIN pane's
    // survivor reporting too. Only on an unproven report naming a pane, and
    // the daemon's alone like the rest.
    event
        .metadata
        .remove(crate::event::DAEMON_NO_LIVE_AGENT_METADATA_KEY);
    // Greptile, PR #1559: the agent an attested frame's token was minted for,
    // stamped on the frame for attached clients — the daemon's alone like the
    // rest. The generation re-check above and this broadcast are not ordered
    // against a respawn, so a successor can claim the pane in between; a
    // client, which has no registry, refuses a marked frame that names no
    // agent when the card it would land on belongs to another agent. See
    // `ATTESTED_OWNER_METADATA_KEY`.
    event
        .metadata
        .remove(crate::event::ATTESTED_OWNER_METADATA_KEY);
    if let Some(owner) = attested_agent {
        event.metadata.insert(
            crate::event::ATTESTED_OWNER_METADATA_KEY.to_string(),
            owner.to_string(),
        );
    }
    if unproven {
        event.metadata.insert(
            crate::event::UNPROVEN_METADATA_KEY.to_string(),
            crate::event::UNPROVEN_METADATA_VALUE.to_string(),
        );
        if event.pane_id.is_some() && !daemon_owns_pane {
            event.metadata.insert(
                crate::event::DAEMON_NO_LIVE_AGENT_METADATA_KEY.to_string(),
                crate::event::UNPROVEN_METADATA_VALUE.to_string(),
            );
        }
    } else {
        event.metadata.remove(crate::event::UNPROVEN_METADATA_KEY);
    }
    // PRD #1631 (Qodo #1561): a question from a session the pane has moved
    // past is neither applied nor fanned out.
    if state.strip_superseded_question(question_pane.as_deref(), &mut event, registry) {
        debug!(
            pane_id = ?question_pane.as_deref().map(escape_id_for_log),
            "question: a question from a session the pane has moved past is not applied"
        );
    }
    // PRD #1631 (audit A4): the question's revision is the daemon's, stamped
    // here under the state lock that applies it, before the fan-out — so every
    // client's copy and the snapshot carry the same one.
    stamp_question_revision(
        &state,
        registry,
        question_pane.as_deref(),
        &mut event,
        registration,
    );
    // Issue #1678: the aggregate is the daemon's alone, whatever arrived.
    event.background = None;
    event.background_revision = None;
    // Issue #1678: apply FIRST, then broadcast — still under this one write-lock
    // hold, so `SubscribeEventsWithSnapshot` ordering is unchanged — because the
    // event that changes a card's background work has to carry the aggregate
    // the apply computed. The broadcast copy is taken before the apply, so it
    // is the same frame clients have always received, and it is sent whether
    // or not the local apply accepts the event.
    let mut broadcast = event.clone();
    // Issue #447: `apply_event` plus the orchestrator-facing consumer of a
    // delegated worker's `WaitingForInput` — see the method's doc, including
    // why the pane's live agent is read there, under this lock, and not before
    // it like `daemon_owns_pane` above.
    state.apply_hook_event_watching_waiting(event, registry, attested_agent);
    // Stamped on the frame that changed the aggregate, and on every shell frame
    // of a pane that reports its own turns — the client's cue that the frame
    // is background work and not a status (an older daemon never stamps one).
    // And on every later event of a card whose aggregate has changed at least
    // once, so a turn end that leaves the count as it was (a same-agent
    // `/clear` keeps a running command) still tells clients what is running.
    if let Some(applied) = state.take_applied_background()
        && (applied.changed
            || applied.revision > 0
            || (applied.reporting
                && matches!(
                    broadcast.event_type,
                    crate::event::EventType::ShellBusy | crate::event::EventType::ShellIdle
                )))
    {
        broadcast.background = Some(applied.work);
        broadcast.background_revision = Some(applied.revision);
    }
    let _ = event_tx.send(BroadcastMsg::Event(broadcast));
    // Issue #697: an outside agent's card evicted to make room for this one
    // reaches attached clients too, in order and under the same lock, so a
    // client that attached later than the daemon started — and so holds a
    // different set of outside cards — drops exactly the card the daemon did.
    // It also drains announcements a registration queued without
    // broadcasting them (`AppState::register_pane`).
    state.announce_unproven_evictions(event_tx);
    // PRD #1631: a held question that is no longer the pane's pending one —
    // this event answered, cleared or replaced it — lets its producer go, so
    // the producer prints no decision and the agent's own prompt is the one
    // that counts. Under the state lock, so the verdict and the apply cannot
    // be split by another event, and under the pane's question lifecycle, so
    // every hold this sees is already published (audit R3): the one kept is
    // the pending question's own registration, by id AND generation.
    if let Some(pane_id) = question_pane.as_deref() {
        let pending = note_pending_question(&state, registry, pane_id);
        let keep = pending.as_ref().map(|q| (q.id.clone(), q.revision));
        let reason = if keep.is_some() {
            crate::question::ReleaseReason::Superseded
        } else {
            crate::question::ReleaseReason::Cleared
        };
        registry.question_holds().release_pane_except(
            pane_id,
            keep.as_ref().map(|(id, revision)| (id.as_str(), *revision)),
            reason,
        );
    }
    drop(state);
    if let Some((pane_id, agent_id, epoch)) = reported_block {
        notify_orchestrator_of_quota_block(registry, &pane_id, &agent_id, epoch);
    }
    Ok(true)
}

/// PRD #1631 (audit A4/N1/T1): record which question is pending on `pane_id`
/// now, as `state` has it — which registration, and the human-input sequence
/// when it arrived, for the keys channel's "did a human type into the pane
/// since?", and what a queued answer key is checked against on the PTY thread
/// ([`crate::agent_pty::AnswerGate`]). Called after every change to the pane's
/// pending question, under the pane's question lifecycle and the `AppState`
/// write lock that made it. Returns the pending question.
pub(crate) fn note_pending_question(
    state: &crate::state::AppState,
    registry: &AgentPtyRegistry,
    pane_id: &str,
) -> Option<crate::question::PendingQuestion> {
    let pending = state.pending_question_on_pane(pane_id);
    registry.question_answers().note_pending(
        pane_id,
        pending.as_ref().map(|q| (q.id.as_str(), q.revision)),
        registry.human_input_counter(),
    );
    pending
}

/// PRD #1631 (audit A4): stamp the question `event` raises with the daemon's
/// revision for it, overwriting whatever a producer put there.
///
/// - The event publishing a held question takes `registration`, the generation
///   its own hold captured when it was registered — the registration the held
///   reply will be checked against. Never one looked up in the hold map by id
///   (audit R1): an event that is not that registration's — a repeat, or an
///   unheld event under a held id — must not borrow its revision.
/// - Otherwise a repeat of the question already pending on the pane, with the
///   same id and content ([`crate::question::PendingQuestion::same_content`]),
///   keeps that revision — and keeps a keyboard-only channel the daemon set on
///   it after typing part of an answer, so a repeat cannot make a half-typed
///   form answerable again. (The `answer_queued` mark of an answer a fence
///   holds is kept by the state's apply, which reads the question off the
///   event, sanitized of the mark: Q35-17.)
/// - Anything else is new content and gets a fresh revision; a hold on the old
///   one is then released by the reconciliation that follows the apply.
///
/// `pane_id` is the pane the question lands on: the one the event names, or
/// its session's when it names none (audit T2).
fn stamp_question_revision(
    state: &crate::state::AppState,
    registry: &AgentPtyRegistry,
    pane_id: Option<&str>,
    event: &mut AgentEvent,
    registration: Option<u64>,
) {
    let (Some(pane_id), Some(mut question)) = (pane_id, event.question()) else {
        return;
    };
    let current = state
        .pending_question_on_pane(pane_id)
        .filter(|current| current.id == question.id && current.same_content(&question));
    question.revision = match (registration, &current) {
        (Some(generation), _) => Some(generation),
        (None, Some(current)) if current.revision.is_some() => current.revision,
        _ => Some(registry.question_holds().mint_revision()),
    };
    if let Some(current) = current
        && current.revision == question.revision
        && current.channel == crate::question::AnswerChannel::Unsupported
    {
        question.channel = crate::question::AnswerChannel::Unsupported;
    }
    event.set_question(&question);
}

/// PRD #1631 (audit A6): how long the daemon holds a question for its producer
/// before it lets go on its own. Every shipped producer stops waiting first —
/// Claude Code's hook at [`crate::hook::CLAUDE_HOLD_DEADLINE`] (3570 s) and an
/// `await-answer` child at [`crate::hook::AWAIT_ANSWER_DEADLINE`] (3600 s) — so
/// this is the backstop for a producer that does not: past it the hold is
/// released, the question is dropped and every attached client is told, exactly
/// as when a producer's connection closes.
pub const QUESTION_HOLD_DEADLINE: Duration = Duration::from_secs(3600 + 120);

/// PRD #1631 (audit A6): the most questions the daemon holds at once, across
/// every pane. A held question does NOT occupy one of the
/// [`MAX_CONCURRENT_HOOK_CONNECTIONS`] slots — it gives its slot back once the
/// hold is registered — so lifecycle and completion hooks (the `PostToolUse`
/// that clears a question answered by keyboard among them) are never queued
/// behind waiting prompts. This budget is what bounds the held connections
/// instead. One pane holds at most one question at a time (a newer one
/// releases the older), so 64 is twice the hook pool and well above any
/// orchestration this repository defines; a question raised past it is
/// applied without its question, so the card still reads Needs Input and the
/// prompt is answered by keyboard.
pub const MAX_HELD_QUESTIONS: usize = 64;

/// PRD #1631: how long the daemon waits, after writing an answer to a producer
/// that acknowledges (`QuestionSignal::ack` — OpenCode's plugin), for its
/// report of whether the agent took it. The plugin posts to OpenCode's local
/// reply API, which answers in milliseconds; past this the deck's answer is
/// reported unconfirmed. Kept well inside the desktop's own 15 s wait for the
/// answer's report.
pub const HELD_REPLY_ACK_WAIT: Duration = Duration::from_secs(5);

/// The most bytes read from a holding producer's connection while it waits.
/// A holding producer sends nothing more, so any byte at all means it is gone
/// or misbehaving; the small cap keeps a held connection from costing a
/// [`crate::bounded_read::MAX_HOOK_LINE_BYTES`] buffer.
const HELD_CONNECTION_READ_CAP: usize = 1024;

/// The hook loop's bounds — the production values in [`Self::production`],
/// smaller ones from tests that need to reach them.
#[derive(Clone, Copy, Debug)]
struct HookLoopLimits {
    idle_timeout: Duration,
    max_connections: usize,
    hold_deadline: Duration,
    max_holds: usize,
    ack_wait: Duration,
    /// Issue #1664: how long an earlier connection that has sent nothing can
    /// hold back a later event ([`crate::hook_event_order::PREDECESSOR_WAIT`]).
    event_order_wait: Duration,
}

impl HookLoopLimits {
    fn production(idle_timeout: Duration) -> Self {
        Self {
            idle_timeout,
            max_connections: MAX_CONCURRENT_HOOK_CONNECTIONS,
            hold_deadline: QUESTION_HOLD_DEADLINE,
            max_holds: MAX_HELD_QUESTIONS,
            ack_wait: HELD_REPLY_ACK_WAIT,
            event_order_wait: crate::hook_event_order::PREDECESSOR_WAIT,
        }
    }
}

/// What [`handle_question_signal`] needs from the connection it runs on.
struct QuestionConnection<'a, R, W> {
    reader: &'a mut R,
    /// Where the `held` line goes, for a producer that asked for it
    /// (`QuestionSignal::confirm_hold`).
    writer: &'a mut W,
    /// The connection's hook-pool slot, given back once a hold is registered
    /// (audit A6).
    slot: &'a mut Option<tokio::sync::OwnedSemaphorePermit>,
    holds: &'a Arc<tokio::sync::Semaphore>,
    hold_deadline: Duration,
    /// The bound on the `held` line's write: the connection's idle timeout,
    /// as for every other reply on it (issue #1976).
    write_timeout: Duration,
}

/// PRD #1631: apply a producer's [`crate::event::QuestionSignal`] and, for a
/// held question, wait for its answer.
///
/// `attested_agent` is the agent the provenance gate attested for the pane
/// (audit A5/A9): the event is stamped with it, a hold is registered as its,
/// and a message the gate let through WITHOUT attesting one — an unknown pane,
/// or a missing token under `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` — has its
/// question metadata removed and is applied as a plain status event.
///
/// `unproven` is the gate's verdict that the message comes from an outside
/// agent (`Provenance::Unattested`). With `attested_agent` it is the whole
/// admission verdict, and a status the message applies is ingested under it
/// exactly as a raw hook event with that verdict is ([`ingest_hook_event`]):
/// stamped unproven, so an attached client keeps it to an outside card within
/// its budget, and never typing a deck agent's record. The type the event
/// reports is written only for the generation that sent it — the attested
/// agent, else the agent the event names — and only while that generation
/// still holds the pane, as the raw-event path writes it; an outside agent's
/// report writes none, since the deck may have spawned its own agent on the
/// pane since the gate classified it.
///
/// The hold is registered and its event applied under ONE acquisition of the
/// pane's question lifecycle ([`register_and_publish_held`], audit R1/R3), so
/// no other transition on the pane ever sees the hold registered but not yet
/// published: an event clearing the question lands before the registration or
/// after the publication, and a late cleanup of an earlier registration can
/// only run once this one is pending. After the apply the question must be the
/// pane's pending one at this registration's revision, or it was not accepted
/// (it did not survive sanitizing, or the event was refused) and the producer
/// is released at once. Once held, the connection gives its hook-pool slot back
/// and waits under [`MAX_HELD_QUESTIONS`] and the hold deadline instead (audit
/// A6). A producer that asked (`QuestionSignal::confirm_hold`) is told at
/// that point, with a `held` line, so it can tell this daemon from an older one
/// that never answers a `question` at all.
///
/// Returns the reply line to write — with, for an answer, where to report
/// what became of it ([`crate::agent_pty::ReplyDelivery`]) — or `None` when
/// the producer's connection closed while it waited, or its `held` line could
/// not be written within `write_timeout` (issue #1976): the producer is gone, so
/// the question goes too (Claude Code kills its hook when the keyboard answers
/// No or Esc [observed]); the status is left for the next event to decide.
async fn handle_question_signal<R, W>(
    signal: crate::event::QuestionSignal,
    attested_agent: Option<String>,
    unproven: bool,
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    conn: QuestionConnection<'_, R, W>,
) -> Option<crate::agent_pty::HeldReply>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use crate::question::{QuestionReply, ReleaseReason};
    let reply = |reply: QuestionReply| crate::agent_pty::HeldReply {
        reply,
        delivery: None,
    };
    let crate::event::QuestionSignal {
        pane_id,
        mut event,
        hold,
        confirm_hold,
        ..
    } = signal;
    // The pane the provenance gate attested, not whatever the event claims —
    // and the agent it attested, which is who owns the question.
    event.pane_id = Some(pane_id.clone());
    match &attested_agent {
        Some(agent_id) => event.agent_id = Some(agent_id.clone()),
        None => strip_question_metadata(&mut event),
    }
    admit_producer_event(&mut event);
    let mut question_id = event.question().map(|q| q.id);
    info!(
        pane_id = %escape_id_for_log(&pane_id),
        question_id = ?question_id,
        hold,
        attested = attested_agent.is_some(),
        event_type = ?event.event_type,
        "Received question"
    );
    let mut holding = None;
    if let (Some(id), true, Some(agent_id)) = (&question_id, hold, &attested_agent) {
        match Arc::clone(conn.holds).try_acquire_owned() {
            Ok(budget) => {
                holding = Some((id.clone(), agent_id.clone(), budget));
            }
            Err(_) => {
                // No budget: the event still applies, without its question,
                // so the card reads Needs Input and the keyboard answers it.
                warn!(
                    pane_id = %escape_id_for_log(&pane_id),
                    question_id = %id,
                    limit = MAX_HELD_QUESTIONS,
                    "question: the daemon already holds as many questions as it allows; \
                     this one is shown as a plain prompt and answered by keyboard"
                );
                event.metadata.remove(crate::event::QUESTION_METADATA_KEY);
                question_id = None;
            }
        }
    }
    if !unproven {
        registry.set_agent_type_for_generation(
            &pane_id,
            attested_agent.as_deref().or(event.agent_id.as_deref()),
            &event.agent_type,
        );
    }
    let Some((id, agent_id, _budget)) = holding else {
        ingest_hook_event(state, event_tx, registry, event, unproven, attested_agent).await;
        return Some(reply(QuestionReply::released(
            question_id.as_deref().unwrap_or(""),
            ReleaseReason::NotHeld,
        )));
    };
    let Some(hold) =
        register_and_publish_held(state, event_tx, registry, &pane_id, &agent_id, event).await
    else {
        return Some(reply(QuestionReply::released(&id, ReleaseReason::NotHeld)));
    };
    // Held: the hook-pool slot goes back, so this wait never queues another
    // producer's hook (audit A6). `_budget` bounds it instead.
    drop(conn.slot.take());
    if confirm_hold
        && let Ok(json) = serde_json::to_string(&QuestionReply::held(&id))
        && write_hook_reply(conn.writer, &format!("{json}\n"), conn.write_timeout)
            .await
            .is_err()
    {
        // Issue #1976: a producer that cannot be written to within the idle
        // timeout is released as one that hung up, so it cannot park this
        // handler, and its hold-budget permit, before the deadline below.
        drop_held_question(
            state,
            event_tx,
            registry,
            &pane_id,
            &id,
            hold.generation,
            "the producer did not read its held line",
        )
        .await;
        return None;
    }
    // Any read outcome means the producer is gone or misbehaving: a holding
    // producer sends nothing more on this connection.
    let gone = crate::bounded_read::read_capped_line(conn.reader, HELD_CONNECTION_READ_CAP);
    tokio::select! {
        held = hold.rx => Some(held.unwrap_or_else(|_| reply(QuestionReply::released(&id, ReleaseReason::Cleared)))),
        _ = gone => {
            drop_held_question(state, event_tx, registry, &pane_id, &id, hold.generation, "the producer stopped waiting before an answer").await;
            None
        }
        _ = tokio::time::sleep(conn.hold_deadline) => {
            drop_held_question(state, event_tx, registry, &pane_id, &id, hold.generation, "the daemon's hold deadline passed").await;
            Some(reply(QuestionReply::released(&id, ReleaseReason::Cleared)))
        }
    }
}

/// PRD #1631: write a held connection's one reply line and, for an answer,
/// report what became of it to the deck's `AnswerQuestion` handler waiting on
/// `delivery`. A producer that asked to acknowledge (`ack`, the OpenCode
/// plugin's child) then writes one [`crate::question::ReplyAck`] line, read
/// within `ack_wait`; any other producer acts on the line itself, so a written
/// line is a delivered answer, as it always was. A line whose write or flush
/// failed, or did not finish within `write_timeout`, is unconfirmed for every
/// producer.
async fn write_held_reply<R, W>(
    reader: &mut R,
    writer: &mut W,
    held: crate::agent_pty::HeldReply,
    ack: bool,
    ack_wait: Duration,
    write_timeout: Duration,
) where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use crate::agent_pty::ReplyDelivery;
    let Ok(json) = serde_json::to_string(&held.reply) else {
        return;
    };
    // Issue #1693: bounded like every other reply on the connection.
    let written = write_hook_reply(writer, &format!("{json}\n"), write_timeout)
        .await
        .is_ok();
    let Some(delivery) = held.delivery else {
        return;
    };
    // A line that could not be written may or may not have reached the
    // producer, whether or not it acknowledges (PR #1698 review).
    let outcome = if !written {
        ReplyDelivery::Unconfirmed
    } else if !ack {
        ReplyDelivery::Delivered
    } else {
        match tokio::time::timeout(
            ack_wait,
            crate::bounded_read::read_capped_line(reader, HELD_CONNECTION_READ_CAP),
        )
        .await
        {
            Ok(Ok(Some(line))) => {
                match serde_json::from_str::<crate::question::ReplyAck>(line.trim()) {
                    Ok(ack) if ack.delivered => ReplyDelivery::Delivered,
                    Ok(ack) => ReplyDelivery::NotAccepted(ack.detail_for_client()),
                    Err(_) => ReplyDelivery::Unconfirmed,
                }
            }
            _ => ReplyDelivery::Unconfirmed,
        }
    };
    let _ = delivery.send(outcome);
}

/// PRD #1631 (audit R1/R3): register a hold for the question `event` raises on
/// `pane_id` as `agent_id`'s, and publish it — both under one acquisition of
/// the pane's question lifecycle, with the event stamped with the generation
/// this registration captured. Returns the hold when the question is then
/// pending at that revision; otherwise the registration is forgotten, still
/// under the lifecycle, and `None` returned.
pub async fn register_and_publish_held(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    agent_id: &str,
    event: AgentEvent,
) -> Option<crate::agent_pty::QuestionHold> {
    let id = event.question()?.id;
    let lifecycle = registry.question_answers().lock_lifecycle(pane_id).await;
    let hold = registry.question_holds().hold(pane_id, agent_id, &id);
    registry
        .question_answers()
        .barrier("question_signal:registered")
        .await;
    ingest_event_in_lifecycle(
        state,
        event_tx,
        registry,
        &lifecycle,
        event,
        Some(hold.generation),
        |_| false,
    )
    .await;
    let pending = state
        .read()
        .await
        .pending_question_on_pane(pane_id)
        .map(|q| (q.id, q.revision));
    if pending != Some((id.clone(), Some(hold.generation))) {
        registry
            .question_holds()
            .forget(pane_id, &id, hold.generation);
        return None;
    }
    Some(hold)
}

/// PRD #1631: drop a held question that will not be answered — its producer's
/// connection closed, or its hold deadline passed — when the hold
/// `generation` registered is still the one in place. Told to every attached
/// client, not only applied here: a client still showing the question would
/// offer an answer the daemon can only refuse. The event names the question
/// and asserts no status (see `QUESTION_RELEASED_BY_DECK_METADATA_KEY`).
///
/// **Under the pane's question lifecycle, and revision-checked (audit A5/R3).**
/// The whole cleanup — the `forget`, the release event and the fallback clear —
/// holds the lifecycle, so a same-id replacement registers either before it
/// (and the `forget` finds the generation replaced and stops) or after it,
/// never in between with its hold registered but not yet published. The
/// release event is still applied only if the pane's pending question is this
/// revision under the state write lock that applies it, and the fallback clear
/// names the revision too.
async fn drop_held_question(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    id: &str,
    generation: u64,
    why: &'static str,
) {
    let lifecycle = registry.question_answers().lock_lifecycle(pane_id).await;
    if !registry.question_holds().forget(pane_id, id, generation) {
        return;
    }
    registry
        .question_answers()
        .barrier("drop_held_question:forgotten")
        .await;
    let revision = Some(generation);
    let owner = state
        .read()
        .await
        .pending_question_owner(pane_id, id, revision);
    if let Some((session_id, agent_type, agent_id)) = owner {
        let event = deck_question_event(
            session_id,
            agent_type,
            Some(pane_id.to_string()),
            agent_id,
            id,
            crate::event::EventType::Unknown,
            crate::event::QUESTION_RELEASED_BY_DECK_METADATA_KEY,
        );
        let replaced = |s: &mut crate::state::AppState| {
            s.pending_question_on_pane(pane_id)
                .is_none_or(|q| q.id != id || q.revision != revision)
        };
        ingest_event_in_lifecycle(state, event_tx, registry, &lifecycle, event, None, replaced)
            .await;
    }
    // The fallback for an event the card refused.
    let cleared = {
        let mut guard = state.write().await;
        let cleared = guard.clear_pending_question_revision(pane_id, id, revision);
        note_pending_question(&guard, registry, pane_id);
        cleared
    };
    info!(
        pane_id = %escape_id_for_log(pane_id),
        question_id = %id,
        cleared,
        why,
        "question: released without an answer; the question is cleared"
    );
}

/// PRD #1631 (audit A9): remove what a question frame may only carry when the
/// daemon's provenance gate attested it — the question itself and the
/// "answered" marker that clears one. Applied to every bare `AgentEvent` on
/// the hook socket and to a `question` message the gate let through without
/// attesting a pane. The rest of the frame, its status included, is applied as
/// it always was, which is what keeps an older hook binary's bare events
/// working.
fn strip_question_metadata(event: &mut AgentEvent) {
    event.metadata.remove(crate::event::QUESTION_METADATA_KEY);
    event
        .metadata
        .remove(crate::event::QUESTION_RESOLVED_METADATA_KEY);
}

/// PRD #1666 part 3, round 5-wake (plan rev 2 §6): keep the wrapper's report
/// of how the command it ran exited
/// ([`crate::event::WRAPPER_CHILD_EXIT_METADATA_KEY`]) only on a frame the
/// generation's own child sent: the process the deck started in the pane,
/// which is the wrapper. What runs below it inherits the pane, the generation
/// and the hook capability, so without this a `dot-agent-deck wrap` nested
/// inside the agent, reporting its own child's exit first, would be the
/// generation's first report and win over the outer wrapper's. Its report is
/// dropped, and the capture keeps the outer wrapper's, or the deck's own
/// status. `peer` is the connecting process as the hook socket accepted it.
///
/// The peer is the child only when its pid and its start time, read when the
/// connection was accepted, are the child's as the deck spawned it, so a
/// process that later reuses a reaped child's pid is not taken for it; a start
/// time either side could not read drops the report.
///
/// This tells the deck's child from its descendants. It does not authenticate
/// the report: a process running as the deck's user can still impersonate the
/// wrapper, which is inside the accepted trust boundary
/// (`crate::hook_provenance`), and the report only chooses a notice's words.
fn keep_wrapper_report_from_own_child(
    event: &mut AgentEvent,
    peer: Option<AcceptedPeer>,
    attested_agent: Option<&str>,
    registry: &AgentPtyRegistry,
) {
    if !event
        .metadata
        .contains_key(crate::event::WRAPPER_CHILD_EXIT_METADATA_KEY)
    {
        return;
    }
    let own_child = event
        .agent_id
        .as_deref()
        .or(attested_agent)
        .and_then(|generation| registry.child_identity(generation));
    let from_own_child = match (own_child, peer) {
        (Some((pid, Some(started))), Some(peer)) => {
            peer.pid == pid && peer.started == Some(started)
        }
        _ => false,
    };
    if !from_own_child {
        event
            .metadata
            .remove(crate::event::WRAPPER_CHILD_EXIT_METADATA_KEY);
    }
}

/// PRD #1631: an event the daemon itself ingests about the question
/// `question_id` — its answer (`Thinking`, or `Unknown` for a deny, marked
/// [`crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY`]) or its release
/// when the producer stopped waiting (`Unknown`, marked
/// [`crate::event::QUESTION_RELEASED_BY_DECK_METADATA_KEY`]). Both carry
/// [`crate::event::QUESTION_RESOLVED_METADATA_KEY`], so applying the event
/// drops the question in this daemon and in every attached client alike.
pub(crate) fn deck_question_event(
    session_id: String,
    agent_type: crate::event::AgentType,
    pane_id: Option<String>,
    agent_id: Option<String>,
    question_id: &str,
    event_type: crate::event::EventType,
    marker_key: &str,
) -> AgentEvent {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        crate::event::QUESTION_RESOLVED_METADATA_KEY.to_string(),
        question_id.to_string(),
    );
    metadata.insert(
        marker_key.to_string(),
        crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.to_string(),
    );
    AgentEvent {
        session_id,
        agent_type,
        event_type,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata,
        pane_id,
        agent_id,
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    }
}

/// PRD #1631 (PR #1698 review): the event the daemon ingests after it typed
/// part of `question`'s answer and stopped, carrying the question as it now
/// stands — keyboard-only, at a new revision — so every attached client's
/// fold shows what the daemon's snapshot does. An `Unknown`: the card's
/// status is left alone, and reading mode announces nothing from it.
pub(crate) fn deck_keyboard_only_event(
    session_id: String,
    agent_type: crate::event::AgentType,
    pane_id: Option<String>,
    agent_id: Option<String>,
    question: &crate::question::PendingQuestion,
) -> AgentEvent {
    let mut event = AgentEvent {
        session_id,
        agent_type,
        event_type: crate::event::EventType::Unknown,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: std::collections::HashMap::new(),
        pane_id,
        agent_id,
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    };
    event.set_question(question);
    event
}

/// Issue #714: the one notice to the orchestrator of a worker whose block
/// `epoch` was just reported, if the worker still owes a `work-done`.
///
/// Claimed here and WRITTEN on a task of its own
/// ([`AgentPtyRegistry::spawn_worker_blocked_notice`]): every pane's hooks pass
/// through [`ingest_event`], and a write stalled on another pane's writer must
/// not hold any of them up. Returns the delivery's handle, `None` when no notice
/// was claimed.
///
/// The block is re-checked against the latch before the notice is claimed
/// ([`AgentPtyRegistry::quota_block_current`]) and again, writer-held, right
/// before it is written: a genuine work hook can clear the card between the
/// apply and the claim, and a worker that is working again must not be reported
/// as blocked. A notice suppressed before the claim stays owed, and so does one
/// refused at the write (the claim is released), so a later block of the same
/// delegation still reports.
fn notify_orchestrator_of_quota_block(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    agent_id: &str,
    epoch: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if !registry.quota_block_current(pane_id, agent_id, epoch) {
        debug!(
            pane_id = %pane_id,
            agent_id = %agent_id,
            "quota: the block cleared before the orchestrator was told; no blocked-worker notice"
        );
        return None;
    }
    registry.spawn_worker_blocked_notice(pane_id, agent_id, epoch, None)
}

/// Issue #714: normalise a PRODUCER's raw event before it is ingested.
///
/// Runs after the raw-event provenance gate
/// ([`crate::hook_provenance::classify_event`], issue #318), so a refused event
/// never reaches it. A producer's `quota_blocked` that the gate admitted is
/// kept, and what it may do depends on the gate's verdict rather than on
/// anything decided here: an attested report naming its agent latches that
/// agent's quota block (the gate refuses one naming any agent but its token's),
/// and the orchestrator notice (fixed text) is claimed only for a delegation
/// bound to that agent. Under `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` a tokenless
/// report on a pane that was issued a token is admitted the same way, at the
/// cost [`crate::hook_provenance::Policy::WarnOnly`] describes. An outside
/// agent's report (`Unattested`) reaches its own unproven card and moves no
/// latch, so it can raise no notice ([`ingest_hook_event`]). What this
/// function guards is the shape of what is stored, normalised here for every
/// admitted event:
///
/// * on a `quota_blocked`, the kind is mapped onto a known value, the reset is
///   kept only when it parses and lies in a sane window, and the detail is
///   scrubbed ([`normalize_quota_blocked_metadata`]);
/// * every other event type loses every `quota_blocked_*` key, so a reason can
///   only ever travel with the status it explains;
/// * `pull_request` (PRD #1401), the daemon's report of a card's pull request,
///   is stripped from every producer frame, and the hook loop drops a
///   producer's `pull_request` event outright;
/// * `surfaced_inbox` (PRD #1666 §2.8), the inbox a card-surfacing start
///   carries, which a client takes as the pane's, is stripped from every
///   producer frame;
/// * `quota_blocked_source` and `quota_blocked_lifted`, the daemon's own
///   markers (its Codex-rollout block, and the lift after a pane restart), are
///   stripped from every producer frame;
/// * the Codex rollout keys survive only on the Codex events that carry them
///   (`SessionStart`, `UserPromptSubmit` → `Thinking`, `Stop` → `Idle`), and
///   only within their bounds ([`crate::codex_rollout_tail::admissible_path`],
///   [`crate::codex_rollout_tail::admissible_turn_id`]) — an empty or oversized
///   value loses its key, the event stays. Never on a subagent's event (issue
///   #1354, [`crate::event::SUBAGENT_ID_METADATA_KEY`]): a thread-spawned Codex
///   subagent keeps the parent's session but names its own rollout and turn,
///   and arming on it would move the parent's watch onto the subagent's log. The hook CLI applies the same
///   bounds, but a raw frame on the socket need not have come through it, and
///   these values are copied into the arm queue before anything else looks at
///   them;
/// * the daemon-authored background-work aggregate (`background`,
///   `background_revision`, issue #1678) is stripped from every producer frame;
/// * `received_by_deck_at_ns` (issue #1981 M8,
///   [`crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY`]) is set to now, over
///   any copy the producer sent: it marks the event as a producer's for
///   telemetry's hook latency. Every producer ingress admits here — the hook
///   socket's raw events and its `question` messages — so no event the daemon
///   builds itself carries it.
fn admit_producer_event(event: &mut AgentEvent) {
    use crate::quota_block::{
        QUOTA_BLOCKED_LIFTED_METADATA_KEY, QUOTA_BLOCKED_METADATA_KEYS,
        QUOTA_BLOCKED_SOURCE_METADATA_KEY,
    };
    event.metadata.remove(QUOTA_BLOCKED_SOURCE_METADATA_KEY);
    event.metadata.remove(QUOTA_BLOCKED_LIFTED_METADATA_KEY);
    // PRD #1666 long-turn delivery: only the daemon lifts its own notice.
    event
        .metadata
        .remove(crate::event::DELIVERY_NOTICE_CLEARED_METADATA_KEY);
    // PRD #1666 S6: nor does a producer say a prompt is the deck's own
    // message; the raw-event ingress stamps that after this, and no other
    // ingress does.
    event
        .metadata
        .remove(crate::inbox_prompt::DECK_MESSAGE_METADATA_KEY);
    // PRD #1631: the deck's own answered marker never comes from a producer,
    // and a question is rewritten sanitized — or dropped — so every reader of
    // the fan-out sees the question the snapshot will hold.
    event
        .metadata
        .remove(crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY);
    event
        .metadata
        .remove(crate::event::QUESTION_RELEASED_BY_DECK_METADATA_KEY);
    // Issue #1981 M8: when the daemon received the event, over any copy the
    // producer sent, so telemetry counts hook latency for producers' events
    // alone and no producer can forge or skew it.
    event
        .metadata
        .remove(crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY);
    if let Some(now) = chrono::Utc::now().timestamp_nanos_opt() {
        event.metadata.insert(
            crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY.to_string(),
            now.to_string(),
        );
    }
    if event
        .metadata
        .contains_key(crate::event::QUESTION_METADATA_KEY)
    {
        match event.question() {
            Some(question) => event.set_question(&question),
            None => {
                event.metadata.remove(crate::event::QUESTION_METADATA_KEY);
            }
        }
    }
    // PRD #1401: the pull request a card's badge opens is the daemon's alone.
    event
        .metadata
        .remove(crate::event::PULL_REQUEST_METADATA_KEY);
    event
        .metadata
        .remove(crate::event::PULL_REQUEST_REVISION_METADATA_KEY);
    // PRD #1666 §2.8: so is the inbox a card-surfacing start carries, which a
    // client takes as the pane's inbox.
    event
        .metadata
        .remove(crate::event::SURFACED_INBOX_METADATA_KEY);
    // Issue #1678: the background aggregate is the daemon's alone; a producer's
    // copy would otherwise reach every attached client's card.
    event.background = None;
    event.background_revision = None;
    if event.event_type == crate::event::EventType::QuotaBlocked {
        normalize_quota_blocked_metadata(
            &mut event.metadata,
            chrono::Utc::now().timestamp_millis(),
        );
    } else {
        for key in QUOTA_BLOCKED_METADATA_KEYS {
            event.metadata.remove(key);
        }
    }
    let codex_rollout_event = event.agent_type == crate::event::AgentType::Codex
        && !event.is_from_subagent()
        && matches!(
            event.event_type,
            crate::event::EventType::SessionStart
                | crate::event::EventType::Thinking
                | crate::event::EventType::Idle
        );
    use crate::codex_rollout_tail::{
        CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY, admissible_path,
        admissible_turn_id,
    };
    let drop_path = !codex_rollout_event
        || event
            .metadata
            .get(CODEX_TRANSCRIPT_PATH_METADATA_KEY)
            .is_some_and(|p| !admissible_path(p));
    if drop_path {
        event.metadata.remove(CODEX_TRANSCRIPT_PATH_METADATA_KEY);
    }
    let drop_turn = !codex_rollout_event
        || event
            .metadata
            .get(CODEX_TURN_ID_METADATA_KEY)
            .is_some_and(|t| !admissible_turn_id(t));
    if drop_turn {
        event.metadata.remove(CODEX_TURN_ID_METADATA_KEY);
    }
}

/// Issue #714: normalise the `quota_blocked_*` keys of a `quota_blocked` event
/// at time `now_ms` — an unknown or missing kind becomes `unknown`, a reset that
/// does not parse as epoch milliseconds within the plausible window around now
/// ([`crate::quota_block::reset_at_is_plausible`]) is removed
/// (the event stays), and the detail is scrubbed and bounded.
fn normalize_quota_blocked_metadata(
    metadata: &mut std::collections::HashMap<String, String>,
    now_ms: i64,
) {
    use crate::quota_block::{
        BlockedKind, QUOTA_BLOCKED_DETAIL_METADATA_KEY, QUOTA_BLOCKED_KIND_METADATA_KEY,
        QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY, scrub_detail,
    };
    let kind = metadata
        .get(QUOTA_BLOCKED_KIND_METADATA_KEY)
        .map_or(BlockedKind::Unknown, |k| BlockedKind::from_wire(k));
    metadata.insert(
        QUOTA_BLOCKED_KIND_METADATA_KEY.to_string(),
        kind.as_wire().to_string(),
    );
    if !metadata
        .get(QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY)
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|at| crate::quota_block::reset_at_is_plausible(at, now_ms))
    {
        metadata.remove(QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY);
    }
    match metadata
        .get(QUOTA_BLOCKED_DETAIL_METADATA_KEY)
        .map(|d| scrub_detail(d))
    {
        Some(detail) if !detail.is_empty() => {
            metadata.insert(QUOTA_BLOCKED_DETAIL_METADATA_KEY.to_string(), detail);
        }
        _ => {
            metadata.remove(QUOTA_BLOCKED_DETAIL_METADATA_KEY);
        }
    }
}

/// PRD #1497: publish the final reply a hook-socket `line` carries beside its
/// turn-ending `event` ([`crate::turn_reply::TURN_REPLY_LINE_KEY`]).
///
/// Only for an attested or legacy-admitted event (the caller has excluded an
/// outside agent's), only for a turn end — `Idle`, or the `Error` /
/// `QuotaBlocked` a failed turn becomes — and only when the event names its
/// agent and that agent is the live owner of the pane it names, so a payload
/// cannot speak for another pane's agent.
///
/// Review RV-B1: a Codex turn is reported twice — by its `Stop` hook, whose
/// payload names no turn, and by its rollout's `task_complete`, which does —
/// and the hub can drop the second report only when both name the turn (and
/// only within the bounds [`crate::turn_reply::TurnReplyHub::publish`]
/// defines). So the turn a
/// Codex `UserPromptSubmit` (a `Thinking` carrying
/// [`crate::codex_rollout_tail::CODEX_TURN_ID_METADATA_KEY`]) begins is
/// recorded here, and a Codex turn end whose reply names no turn is given it.
/// One source per turn was the alternative: suppressing the `Stop` reply would
/// leave only the rollout, which is read every
/// [`crate::codex_rollout_tail::POLL_INTERVAL`] and only when Codex named a
/// rollout to watch, and suppressing the rollout's would lose the errored turn
/// it alone reports. Matching on the reply's text was not considered: two turns
/// can end with the same words.
fn publish_hook_turn_reply(
    registry: &AgentPtyRegistry,
    event: &AgentEvent,
    attested_agent: Option<&str>,
    line: &str,
) {
    use crate::event::{AgentType, EventType};
    if event.is_daemon_synthetic() {
        return;
    }
    let (Some(pane_id), Some(agent_id)) = (event.pane_id.as_deref(), event.agent_id.as_deref())
    else {
        return;
    };
    if attested_agent.is_some_and(|attested| attested != agent_id) {
        return;
    }
    let codex = event.agent_type == AgentType::Codex;
    if codex && event.event_type == EventType::Thinking {
        if let Some(turn_id) = event
            .metadata
            .get(crate::codex_rollout_tail::CODEX_TURN_ID_METADATA_KEY)
            && registry.is_live_owner(pane_id, agent_id)
        {
            registry.turn_replies().begin_turn(agent_id, turn_id);
        }
        return;
    }
    if !matches!(
        event.event_type,
        EventType::Idle | EventType::Error | EventType::QuotaBlocked
    ) {
        return;
    }
    let Some(mut reply) = crate::turn_reply::reply_from_line(line) else {
        return;
    };
    if codex && registry.is_live_owner(pane_id, agent_id) {
        let begun = registry.turn_replies().take_begun_turn(agent_id);
        reply.turn_id = reply.turn_id.or(begun);
    }
    registry.publish_turn_reply(pane_id, agent_id, reply);
}

/// Issue #714: queue the Codex rollout tailer's side of a Codex hook event
/// (`crate::codex_rollout_tail`). `SessionStart` / `UserPromptSubmit` name the
/// rollout and the turn to watch; a native `Stop` for the watched turn disarms
/// it — but only a `Stop` whose hook `line` carried the turn's final reply
/// ([`crate::turn_reply::reply_from_line`]). A `Stop` without one leaves the
/// turn armed, so its `task_complete` is still read for the reply (PRD #1497
/// re-audit R3) and disarms it then, but it queues
/// [`crate::codex_rollout_tail::ArmCommand::StoppedWithoutReply`], which retires
/// the watch with nothing delivered if that record has not come within
/// [`crate::codex_rollout_tail::DRAIN_AFTER_STOP`]. A hook CLI older than PRD
/// #1497 attaches no reply to any `Stop`, so under one every Codex turn takes
/// that path. A `Stop` that names no turn queues nothing.
/// `crate::codex_rollout_tail`'s module doc lists every way a watch ends.
/// Only for an event whose pane and agent
/// name the pane's LIVE owner, so a payload can never make the daemon read a
/// file on another pane's behalf. The file itself is opened and read by
/// [`run_codex_rollout_monitor`], never here.
fn queue_codex_rollout_arm(registry: &AgentPtyRegistry, event: &AgentEvent, line: &str) {
    use crate::codex_rollout_tail::{
        ArmCommand, ArmRequest, CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY,
    };
    if event.agent_type != crate::event::AgentType::Codex
        || event.is_daemon_synthetic()
        || event.is_wrapper_output_classified()
    {
        return;
    }
    let (Some(pane_id), Some(agent_id)) = (event.pane_id.as_deref(), event.agent_id.as_deref())
    else {
        return;
    };
    let path = event
        .metadata
        .get(CODEX_TRANSCRIPT_PATH_METADATA_KEY)
        .cloned();
    let turn_id = event.metadata.get(CODEX_TURN_ID_METADATA_KEY).cloned();
    let command = match event.event_type {
        crate::event::EventType::Idle => {
            let Some(turn_id) = turn_id else {
                return;
            };
            // The rollout is the turn's only report of its reply when the
            // `Stop` carried none (`crate::hook::extract_codex_hook_turn_reply`),
            // so the watch stays armed, but only for the bounded drain.
            if crate::turn_reply::reply_from_line(line).is_none() {
                ArmCommand::StoppedWithoutReply {
                    pane_id: pane_id.to_string(),
                    agent_id: agent_id.to_string(),
                    turn_id,
                }
            } else {
                ArmCommand::Disarm {
                    pane_id: pane_id.to_string(),
                    agent_id: agent_id.to_string(),
                    turn_id,
                }
            }
        }
        crate::event::EventType::SessionStart | crate::event::EventType::Thinking
            if path.is_some() || turn_id.is_some() =>
        {
            ArmCommand::Arm(ArmRequest {
                pane_id: pane_id.to_string(),
                agent_id: agent_id.to_string(),
                session_id: event.session_id.clone(),
                path,
                turn_id,
            })
        }
        _ => return,
    };
    if !registry.is_live_owner(pane_id, agent_id) {
        debug!(
            pane_id = %escape_id_for_log(pane_id),
            "codex rollout: ignored an arm from an agent that is not the pane's live owner"
        );
        return;
    }
    registry.codex_rollout_arms().push(command);
}

/// Issues #714 and #1359: the daemon half of Codex failed-turn detection
/// (`crate::codex_rollout_tail`), and PRD #1497's source of a Codex turn's final
/// reply when no `Stop` hook reports it (an errored turn runs none).
///
/// Every [`crate::codex_rollout_tail::POLL_INTERVAL`] it applies the arm
/// commands the hook loop queued, then polls every armed tailer on a blocking
/// thread — the file I/O happens there, outside every lock the daemon holds —
/// and reports each failed turn it finds as ONE event through [`ingest_event`],
/// the same path, lock order and orchestrator notice a producer's report takes:
/// a `QuotaBlocked` for a usage limit, an `Error` for anything else. The event
/// is filed under the arming hook's session id and carries the registry agent;
/// a `QuotaBlocked` is also marked with
/// [`crate::quota_block::QUOTA_BLOCKED_SOURCE_METADATA_KEY`] as the daemon's own.
///
/// A poll with no armed tailer does no I/O at all. No internal shutdown signal —
/// torn down by `.abort()` in `run_daemon_with`'s cleanup, like the
/// shell-activity monitor.
async fn run_codex_rollout_monitor(
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
) {
    let mut tailers = crate::codex_rollout_tail::CodexRolloutTailers::default();
    loop {
        tokio::time::sleep(crate::codex_rollout_tail::POLL_INTERVAL).await;
        for command in registry.codex_rollout_arms().drain() {
            tailers.apply(command);
        }
        let failures;
        (tailers, failures) = poll_codex_rollouts(&registry, tailers).await;
        // PRD #1497: a watched turn's reply, published before its failure (if
        // any) is reported, as the hook loop publishes a reply before its
        // event. It is dropped as a duplicate of the turn's `Stop` report only
        // under the conditions `crate::turn_reply::TurnReplyHub::publish`
        // defines (both name the turn, nothing else recorded between them).
        for found in tailers.take_replies() {
            registry.publish_turn_reply(&found.pane_id, &found.agent_id, found.reply);
        }
        for failure in failures {
            report_codex_rollout_failure(&state, &event_tx, &registry, failure).await;
        }
    }
}

/// Issue #1359: report one failed turn a poll found — unless Codex has moved
/// on to another turn since the poll began.
///
/// The poll runs on a blocking thread while the hook loop keeps going, so the
/// agent's next `UserPromptSubmit` can put the card on Thinking before this
/// failure is ingested, and the older turn's `Error` (or `QuotaBlocked`) would
/// then repaint the newer turn's card (Qodo and Greptile on PR #1375). The hook
/// loop queues that prompt's arm before it applies the prompt, and nothing
/// drains the queue until the next tick, so a newer arm still queued when the
/// `AppState` write lock is held is the proof: the failure is dropped. A
/// prompt that has not reached the queue by then has not reached the card
/// either, and lands after the failure.
async fn report_codex_rollout_failure(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &Arc<AgentPtyRegistry>,
    failure: crate::codex_rollout_tail::CodexTurnFailure,
) {
    use crate::quota_signals::FailureOutcome;
    let pane_id = failure.pane_id.clone();
    let agent_id = failure.agent_id.clone();
    let turn_id = failure.turn_id.clone();
    let outcome = failure.outcome.clone();
    let event = codex_rollout_failure_event(failure);
    let superseded = |_: &mut crate::state::AppState| {
        registry
            .codex_rollout_arms()
            .supersedes(&agent_id, &turn_id)
    };
    if !ingest_event_unless(state, event_tx, registry, event, false, None, superseded).await {
        tracing::debug!(
            agent_id = %escape_id_for_log(&agent_id),
            "codex rollout: dropped a failed turn that a newer turn superseded"
        );
        return;
    }
    // Logged only once the card really changed, so a dropped stale failure
    // never reads as a status change (Qodo on PR #1375).
    match outcome {
        FailureOutcome::Blocked { kind, .. } => warn!(
            pane_id = %escape_id_for_log(&pane_id),
            agent_id = %escape_id_for_log(&agent_id),
            kind = kind.as_wire(),
            "quota: the Codex session log records a usage-limit failure; marked Blocked"
        ),
        FailureOutcome::Error => warn!(
            pane_id = %escape_id_for_log(&pane_id),
            agent_id = %escape_id_for_log(&agent_id),
            "the Codex session log records a failed turn; marked Error"
        ),
    }
}

/// Issue #714: one poll of `tailers` on a blocking thread, handing the set back
/// with the failed turns found. A poll that panicked loses the set rather than the
/// monitor: every agent's next Codex prompt arms afresh.
async fn poll_codex_rollouts(
    registry: &Arc<AgentPtyRegistry>,
    mut tailers: crate::codex_rollout_tail::CodexRolloutTailers,
) -> (
    crate::codex_rollout_tail::CodexRolloutTailers,
    Vec<crate::codex_rollout_tail::CodexTurnFailure>,
) {
    let registry = Arc::clone(registry);
    tokio::task::spawn_blocking(move || {
        let failures = tailers.tick(|pane_id, agent_id| registry.is_live_owner(pane_id, agent_id));
        (tailers, failures)
    })
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "codex rollout: a poll failed; every tailer was reset");
        Default::default()
    })
}

/// Issues #714 and #1359: the event the daemon reports for a failed turn read
/// from a Codex rollout — `QuotaBlocked` for a usage limit, `Error` for any
/// other failure. An `Error` carries no provider text, like the Claude Code
/// `StopFailure` it gives parity with: the card shows the status, and the
/// message stays in the rollout it came from.
pub(crate) fn codex_rollout_failure_event(
    failure: crate::codex_rollout_tail::CodexTurnFailure,
) -> AgentEvent {
    use crate::quota_block::{
        QUOTA_BLOCKED_DETAIL_METADATA_KEY, QUOTA_BLOCKED_KIND_METADATA_KEY,
        QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY, QUOTA_BLOCKED_SOURCE_CODEX_ROLLOUT,
        QUOTA_BLOCKED_SOURCE_METADATA_KEY,
    };
    use crate::quota_signals::FailureOutcome;
    let now = chrono::Utc::now();
    let mut metadata = std::collections::HashMap::new();
    let event_type = match failure.outcome {
        FailureOutcome::Blocked { kind, resets_at_ms } => {
            metadata.insert(
                QUOTA_BLOCKED_KIND_METADATA_KEY.to_string(),
                kind.as_wire().to_string(),
            );
            if let Some(at) = resets_at_ms {
                metadata.insert(
                    QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY.to_string(),
                    at.to_string(),
                );
            }
            if let Some(message) = failure.message {
                metadata.insert(QUOTA_BLOCKED_DETAIL_METADATA_KEY.to_string(), message);
            }
            normalize_quota_blocked_metadata(&mut metadata, now.timestamp_millis());
            metadata.insert(
                QUOTA_BLOCKED_SOURCE_METADATA_KEY.to_string(),
                QUOTA_BLOCKED_SOURCE_CODEX_ROLLOUT.to_string(),
            );
            crate::event::EventType::QuotaBlocked
        }
        FailureOutcome::Error => crate::event::EventType::Error,
    };
    AgentEvent {
        session_id: failure.session_id,
        agent_type: crate::event::AgentType::Codex,
        event_type,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: now,
        user_prompt: None,
        metadata,
        pane_id: Some(failure.pane_id),
        agent_id: Some(failure.agent_id),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    }
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
    // PRD #1666 §2.8: a message the inbox could not deliver reaches its
    // recipient's card through this same sink.
    let card_registry = Arc::downgrade(registry);
    registry
        .inbox()
        .set_card_notice_sink(std::sync::Arc::new(move |notice| {
            if let Some(registry) = card_registry.upgrade() {
                registry.publish_delivery_notice(notice);
            }
        }));
    // PR #1735 fifth review (N4): the inbox's own notices to an agent are
    // posted only while that agent still holds its pane, asked under the
    // inbox's lock.
    let live_registry = Arc::downgrade(registry);
    registry
        .inbox()
        .set_live_agent_check(std::sync::Arc::new(move |pane_id, agent_id| {
            live_registry.upgrade().is_some_and(|registry| {
                registry.pane_current_agent_id(pane_id).as_deref() == Some(agent_id)
            })
        }));
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
            // PRD #1666 long-turn delivery: lift only a card that still shows
            // the deck's not-delivered notice, as far as the daemon's own copy
            // says. A client that predates the lift reads the event as a plain
            // Thinking, so it is sent only where Thinking is what the card
            // should now show. A card only attached clients hold (#424 F5) is
            // not the daemon's to check, and gets it; current clients apply
            // the same rule themselves.
            if notice.cleared
                && guard
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.delivery_notice().is_none())
            {
                tracing::debug!(
                    pane_id = %notice.pane_id,
                    delivery_id = %notice.delivery_id,
                    "delivery cleared: the card no longer shows the deck's notice"
                );
                return;
            }
            let mut metadata = std::collections::HashMap::new();
            // PRD #1666 long-turn delivery: a notice that a reported message
            // reached the agent after all lifts the card's not-delivered
            // Error (`AppState::apply_status_transition` moves only that one).
            let (event_type, tool_detail) = if notice.cleared {
                metadata.insert(
                    crate::event::DELIVERY_NOTICE_CLEARED_METADATA_KEY.to_string(),
                    crate::event::DELIVERY_NOTICE_CLEARED_METADATA_VALUE.to_string(),
                );
                (crate::event::EventType::Thinking, None)
            } else {
                metadata.insert(
                    crate::event::DELIVERY_NOTICE_METADATA_KEY.to_string(),
                    notice.detail.to_string(),
                );
                (
                    crate::event::EventType::Error,
                    Some(notice.detail.to_string()),
                )
            };
            let event = AgentEvent {
                session_id,
                // The daemon is not the agent, and must not claim to be one:
                // `apply_event` only fills a session's type when it is still
                // unknown, so `None` cannot overwrite a real agent type.
                agent_type: crate::event::AgentType::None,
                event_type,
                tool_name: None,
                tool_detail,
                cwd: None,
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata,
                pane_id: Some(notice.pane_id.clone()),
                agent_id: Some(notice.agent_id.clone()),
                agent_version: None,
                schema_version: None,
                live_target: None,
                background: None,
                background_revision: None,
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
    run_shell_activity_monitor_with(pty_registry, state, event_tx, |roots| {
        let roots = roots.to_vec();
        async move { crate::platform::proc::process_table_async(&roots).await }
    })
    .await
}

/// Issue #1678 D7: the spawn a shell-activity reading belongs to — pane, registry
/// agent and PTY child together — so a reading never outlives the spawn it was
/// taken for, and a successor in the same pane starts from nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShellActivityKey {
    pane_id: String,
    agent_id: String,
    shell_pid: i32,
}

impl ShellActivityKey {
    fn of(candidate: &crate::agent_pty::ShellActivityCandidate) -> Self {
        Self {
            pane_id: candidate.pane_id.clone(),
            agent_id: candidate.agent_id.clone(),
            shell_pid: candidate.shell_pid,
        }
    }
}

/// What the shell-activity monitor last read for one spawn.
#[derive(Debug, Clone)]
struct ShellActivityReading {
    busy: bool,
    /// The qualifying POSIX sessions of the last valid sample (issue #1678 D4).
    sessions: std::collections::HashSet<i32>,
    /// Whether the spawn's turn had ended when it was last read, so the
    /// monitor emits once when a turn ends with a command still running.
    turn_ended: bool,
}

/// [`AgentPtyRegistry::classify_command_sessions`], owned, for the monitor loop.
fn classify_owned(
    candidates: &[crate::agent_pty::ShellActivityCandidate],
    table: &[crate::platform::proc::ProcessInfo],
) -> Vec<(
    crate::agent_pty::ShellActivityCandidate,
    std::collections::HashSet<i32>,
)> {
    AgentPtyRegistry::classify_command_sessions(candidates, table)
        .into_iter()
        .map(|(candidate, sessions)| (candidate.clone(), sessions))
        .collect()
}

// ── The shell-activity poll's three time constants ──
//
// Module-level rather than function-local since issue #862, because
// `SamplingHealth` below derives its backoff from `POLL_INTERVAL` and reports
// `MAX_TABLE_AGE` in its log line. Used by `run_shell_activity_monitor_with`
// and by nothing else.

// PRD #370 Open Question (poll cadence): 500ms is a first-cut balance
// between feeling responsive and negligible overhead (one registry lock
// + one `ps -A` sample per tick, reused across every live pane, plus a
// `getsid` per row).
//
// Issue #493 measured the sample it pays for AS IT THEN WAS — the old
// `pid=,ppid=,tty=,args=` column set: ~49ms of wall time per `ps -A` on an idle
// 16-core Linux box with ~620 processes (release build), i.e. ~10% of one core
// at 2Hz, of which only ~1.4ms was this process's own CPU (the `getsid` loop
// plus parsing). It is why skipping the sample when no pane needs it is worth
// the guard below rather than merely tidy. It is NOT the current cost of the
// sample; see the next paragraph.
//
// ── PRD #386 M5, answered by issue #862: 500ms CONFIRMED, not revised ──
//
// The cadence was never what stalled the signal; the `args` column was — it
// made `ps` read `/proc/<pid>/cmdline` AND `/proc/<pid>/environ` for every
// process on the machine, both of which take the target's `mmap_lock` (see
// `PS_TABLE_ARGS` in `platform/proc/unix.rs`). With the argv column deferred to
// the handful of pids that actually need it, the bulk sample measures (Linux
// 7.0.0, 16 cores, debug build, warm, ~380-480 processes):
//
//   idle (load 0.5)                         12-14ms p50, vs a 12.5ms
//                                           `ps -p 1` fork/exec floor
//   CPU-bound build (load 12.4)             19ms p50, 23ms p90, 40ms max
//   build + saturated I/O (load 18,
//     13-17 procs in D-state,
//     io_full_avg10 up to 87)               15ms mean, 19ms max
//
// against 21ms p50 / 46ms p50 / 60ms mean respectively for the old column set,
// whose worst observed sample was 104ms. So at 2Hz this is ~3-4% of one core of
// WALL time and, per `/usr/bin/time`, below that tool's 10ms resolution of
// measurable CPU — where the old column set cost 60ms of CPU per sample.
// Relaxing to 1s would halve an already-negligible cost and halve the signal's
// responsiveness, which is the wrong trade for a badge a user watches. The full
// measurement — including the 19-20s field sample this is a response to, and
// what could NOT be reproduced — is in
// `prds/386-descendant-scan-shell-activity-signal.md` (M5).
const POLL_INTERVAL: Duration = Duration::from_millis(500);

// Issue #429: an upper bound on how long a tick WAITS for a sample —
// deliberately not a bound on how long the sample's child may live (see
// `inflight` below). Generous next to the 12-19ms a healthy sample takes across
// the whole load range measured for #862, so ordinary load does not trip it —
// but a machine wedged hard enough does, which is what `SamplingHealth` is for.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);

// How old a sample's table may be and still be worth classifying against. It
// exists to stop a long-overrunning sample's answer from being applied to a
// machine that has moved on; discarding an ANSWERED sample is free, since that
// child is already finished, so unlike abandoning an un-answered one it cannot
// accumulate.
//
// A healthy sample answers in tens of milliseconds, so this trips only when
// something is genuinely wrong — and it does trip: issue #862 recorded a 19-20s
// sample under a real build storm, and this constant discarding it (correctly)
// is what left every pane's status alone for the duration.
const MAX_TABLE_AGE: Duration = Duration::from_secs(3);

/// Why one shell-activity tick got no usable process table (issue #862).
///
/// The three cases differ in what they say about the machine and in whether the
/// sample that produced them is *finished*, which is what decides whether the
/// next one is held off — see [`SamplingHealth::record_trouble`].
#[derive(Debug, Clone, Copy)]
enum SamplingTrouble {
    /// The sample answered, but so long after it started that its table
    /// describes a machine that has since moved on (past `MAX_TABLE_AGE`).
    /// Finished, so a fresh one would be started immediately without a hold-off.
    StaleTable { age: Duration },
    /// The sample answered `None` — `ps` could not be run, or produced nothing
    /// parseable. Finished, same as above.
    Failed,
    /// The sample has not answered within `SAMPLE_TIMEOUT` and is being retained
    /// for the next tick to await. **Not** finished: the retention already
    /// bounds the `ps` children to one, so there is nothing to hold off.
    Overran { timeout: Duration, panes: usize },
}

/// The shell-activity poll's sampling health across consecutive ticks, and the
/// two things issue #862 derives from it: **how long to wait before starting the
/// next sample**, and **how much to say about it in the log**.
///
/// It exists because the pre-#862 loop did neither. A machine under sustained
/// pressure produced a sample that blew `MAX_TABLE_AGE`, warned, was discarded,
/// and was replaced on the very next tick by a fresh `ps` just as likely to
/// wedge — so the daemon kept one `ps` alive against a machine it was
/// contributing load to, and narrated every cycle. The field episode recorded in
/// #862 logged **1716** `shell-activity` warnings in one day that way.
///
/// Both halves are deliberately conservative:
///
/// - The hold-off applies **only to starting a new sample**, never to awaiting a
///   retained one. A retained sample's age is measured from when it *started*,
///   so deferring the await could push a perfectly healthy answer past
///   `MAX_TABLE_AGE` and discard it for being collected late rather than for
///   being late — turning a throttle into a second failure mode.
/// - The log is coalesced into an **episode**, not silenced: one warning on
///   entry naming the cause, one heartbeat per [`Self::HEARTBEAT`] while it
///   persists carrying the running counts, and one line on recovery. An episode
///   of duration `T` therefore costs `2 + T / HEARTBEAT` lines rather than one
///   per poll cycle. Going quiet altogether would trade a findable problem for
///   an invisible one, which is the wrong direction for a signal whose whole
///   failure mode is silence.
#[derive(Debug, Default)]
struct SamplingHealth {
    /// When the current degraded episode began, or `None` when healthy.
    since: Option<tokio::time::Instant>,
    /// When this episode last emitted a line, so the heartbeat can be spaced.
    logged_at: Option<tokio::time::Instant>,
    /// Finished-but-unusable samples in this episode. Drives the backoff, so an
    /// overrun (which is retained, not finished) deliberately does not bump it.
    backoff_steps: u32,
    /// Per-cause counts for this episode, reported on the heartbeat and on
    /// recovery so one line says what the whole episode consisted of.
    stale_tables: u32,
    failures: u32,
    overruns: u32,
    /// The earliest instant a new sample may be started.
    hold_until: Option<tokio::time::Instant>,
}

impl SamplingHealth {
    /// The longest a hold-off ever grows to. Eight seconds keeps a wedged
    /// machine to roughly one `ps` per eight seconds instead of one per 500 ms,
    /// while still recovering the signal within one badge-refresh of the machine
    /// coming back — the pane status this feeds is something a user watches.
    const BACKOFF_MAX: Duration = Duration::from_secs(8);
    /// How often a persisting episode re-states itself.
    const HEARTBEAT: Duration = Duration::from_secs(300);

    /// Whether a new sample may be started on this tick.
    fn may_start_sample(&self) -> bool {
        !self
            .hold_until
            .is_some_and(|until| tokio::time::Instant::now() < until)
    }

    /// Record a tick that got no usable table, and log it if this episode has
    /// something new to say.
    fn record_trouble(&mut self, trouble: SamplingTrouble) {
        let now = tokio::time::Instant::now();
        let entering = self.since.is_none();
        let since = *self.since.get_or_insert(now);
        match trouble {
            SamplingTrouble::StaleTable { .. } => self.stale_tables += 1,
            SamplingTrouble::Failed => self.failures += 1,
            SamplingTrouble::Overran { .. } => self.overruns += 1,
        }
        // Only a FINISHED sample earns a hold-off; a retained one is already
        // the single `ps` this loop is willing to have outstanding.
        if !matches!(trouble, SamplingTrouble::Overran { .. }) {
            self.backoff_steps = self.backoff_steps.saturating_add(1);
            let step = POLL_INTERVAL
                .checked_mul(1u32 << self.backoff_steps.saturating_sub(1).min(8))
                .unwrap_or(Self::BACKOFF_MAX)
                .min(Self::BACKOFF_MAX);
            self.hold_until = Some(now + step);
        }

        let due = self
            .logged_at
            .is_none_or(|at| now.duration_since(at) >= Self::HEARTBEAT);
        if !entering && !due {
            return;
        }
        self.logged_at = Some(now);
        let episode_ms = now.duration_since(since).as_millis();
        let next_sample_in_ms = self
            .hold_until
            .map(|until| until.saturating_duration_since(now).as_millis())
            .unwrap_or(0);
        // One message shape for entry and heartbeat alike, so a log reader can
        // grep one string and get the whole episode. `cause` names what this
        // particular tick hit; the counts say what the episode has consisted of.
        let cause = match trouble {
            SamplingTrouble::StaleTable { .. } => "sample answered too late to trust",
            SamplingTrouble::Failed => "sample produced no usable table",
            SamplingTrouble::Overran { .. } => "sample overran its deadline",
        };
        let (age_ms, timeout_ms, panes) = match trouble {
            SamplingTrouble::StaleTable { age } => (Some(age.as_millis()), None, None),
            SamplingTrouble::Failed => (None, None, None),
            SamplingTrouble::Overran { timeout, panes } => {
                (None, Some(timeout.as_millis()), Some(panes))
            }
        };
        warn!(
            cause,
            age_ms,
            timeout_ms,
            panes,
            episode_ms,
            stale_tables = self.stale_tables,
            failures = self.failures,
            overruns = self.overruns,
            next_sample_in_ms,
            max_age_ms = MAX_TABLE_AGE.as_millis(),
            "shell-activity: no usable process table; leaving every pane's status \
             alone and backing off before the next sample (classifying current pids \
             against a stale table can misattribute a reused pid, and a wedged `ps` \
             says nothing about the panes). This line repeats at most every 300s \
             while the condition lasts"
        );
    }

    /// Record a tick that got a usable table, closing any episode in progress.
    fn record_healthy(&mut self) {
        let Some(since) = self.since else {
            return;
        };
        tracing::info!(
            episode_ms = since.elapsed().as_millis(),
            stale_tables = self.stale_tables,
            failures = self.failures,
            overruns = self.overruns,
            "shell-activity: process-table sampling recovered; the signal is live again"
        );
        *self = Self::default();
    }
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
/// the one-child-at-a-time invariant described on `inflight` below and the
/// hold-off described on [`SamplingHealth`].
///
/// It receives the tick's **roots** — the candidate panes' shell pids — because
/// which command lines the sample reads is derived from them (issue #862); see
/// [`AgentPtyRegistry::shell_activity_roots`].
async fn run_shell_activity_monitor_with<S, F>(
    pty_registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    sample: S,
) where
    S: Fn(&[i32]) -> F,
    F: std::future::Future<Output = Option<Vec<crate::platform::proc::ProcessInfo>>>,
{
    let mut last_known: std::collections::HashMap<ShellActivityKey, ShellActivityReading> =
        std::collections::HashMap::new();
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
    // Whether the in-flight sample has already been reported as overrunning, so
    // a permanently-wedged `ps` logs once rather than every 2.5s forever.
    let mut inflight_reported = false;

    // Issue #862 (PRD #386 M5's third option): how long to wait before starting
    // the next sample after an unusable one, and how much to say about it. The
    // reasoning — and why the hold-off gates only the START of a sample and
    // never the AWAIT of a retained one — is on `SamplingHealth` itself, so it
    // lives in one place rather than two that can drift.
    let mut health = SamplingHealth::default();

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

        // Issue #1678 D3/D5 (A3, A4): retire the background work of every spawn
        // the registry no longer holds live on its pane, before and regardless
        // of whether this tick's sample answers. Retirement is a lifecycle
        // fact, not a process-table reading: a spawn can exit before its first
        // valid sample, on a platform whose sampler always answers `None`, or
        // while another pane keeps sampling in a failure episode, and its
        // subagents would otherwise stay counted. A spawn the registry still
        // holds live on its pane — published and not exited, or reserved for
        // that pane and not yet published (R1) — is not touched here, so an
        // unknown sample keeps its commands (D4). The
        // retirement reaches attached clients as the daemon's report, sent
        // under the same write lock that applied it.
        let departed_work = {
            let state = state.read().await;
            state
                .background_spawns_with_work()
                .into_iter()
                .any(|(pane, agent)| !pty_registry.agent_is_live_on_pane(&agent, &pane))
        };
        if departed_work {
            let mut state = state.write().await;
            for event in state.retire_departed_background_spawns(|pane, agent| {
                pty_registry.agent_is_live_on_pane(agent, pane)
            }) {
                let _ = event_tx.send(BroadcastMsg::Event(event));
            }
        }

        let snapshot = if candidates.is_empty() {
            // No pane, so no sample — and an empty reading rather than a
            // skipped tick, because "there are no panes" is a fact we just
            // established under the lock, not a failure to observe. Falling
            // through with an empty snapshot lets the departed-spawn sweep below clear
            // `last_known`, which is what makes a later reuse of the same pane
            // id start edge-detection from a clean slate.
            //
            // Deliberately does NOT touch `health` (issue #862). "There are no
            // panes" says nothing about whether the machine can be sampled, and
            // clearing the hold-off here would let pane churn defeat it — close
            // the last pane, reopen it, and a wedged machine gets forked at
            // again immediately, which is the same reasoning `inflight`'s
            // unconditional retention rests on. The cost is bounded and worth
            // naming: a pane opened during a degraded episode can wait up to
            // `SamplingHealth::BACKOFF_MAX` for its first shell-activity
            // reading, during which its badge is whatever its own hook events
            // say — which is the normal source anyway, this signal being the
            // backstop for the gaps between them.
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
                    // Issue #862: hold off STARTING a new sample while the
                    // backoff is in effect. Nothing is in flight here (that is
                    // the `None` arm), so there is no answer to collect late and
                    // no `MAX_TABLE_AGE` interaction — the tick simply has no
                    // opinion, exactly like a timed-out one, and `last_known` is
                    // left untouched so no spurious edge is emitted when the
                    // reading resumes.
                    if !health.may_start_sample() {
                        continue;
                    }
                    inflight_reported = false;
                    (
                        tokio::time::Instant::now(),
                        candidates.clone(),
                        // The roots the argv phase reads command lines for
                        // (issue #862) — this tick's candidate panes and nothing
                        // else. Captured with the sample, so a retained one's
                        // command lines describe the panes it was started for,
                        // which is the same set the `was_resumed` filter below
                        // restricts the classification to.
                        Box::pin(sample(&AgentPtyRegistry::shell_activity_roots(&candidates))),
                    )
                }
            };
            match tokio::time::timeout(SAMPLE_TIMEOUT, pending.as_mut()).await {
                Ok(Some(table)) => {
                    // See `MAX_TABLE_AGE`: a sample that answered this late
                    // describes a machine that has since moved on. No opinion.
                    let age = started.elapsed();
                    if age > MAX_TABLE_AGE {
                        health.record_trouble(SamplingTrouble::StaleTable { age });
                        continue;
                    }
                    health.record_healthy();
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
                                    ShellActivityKey::of(then) == ShellActivityKey::of(current)
                                })
                            })
                            .cloned()
                            .collect();
                        classify_owned(&unchanged, &table)
                    } else {
                        classify_owned(&candidates, &table)
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
                Ok(None) => {
                    health.record_trouble(SamplingTrouble::Failed);
                    continue;
                }
                Err(_elapsed) => {
                    // `inflight_reported` still bounds this to one entry per
                    // retained sample; `record_trouble` then bounds the whole
                    // EPISODE, across however many samples it spans (#862).
                    if !inflight_reported {
                        inflight_reported = true;
                        health.record_trouble(SamplingTrouble::Overran {
                            timeout: SAMPLE_TIMEOUT,
                            panes: candidates.len(),
                        });
                    }
                    inflight = Some((started, at_start, pending));
                    continue;
                }
            }
        };
        // Issue #1678 D7: drop the readings of spawns that left the candidate
        // set since the last poll (closed, respawned, exited) so a later reuse
        // of the same pane starts edge-detection from a clean slate instead of
        // inheriting a stale reading — keyed by spawn identity, so a successor
        // in the same pane is never measured against its predecessor. A spawn
        // that is still a candidate keeps its last known reading even when this
        // sample could not answer for it (D4: unknown is not zero). Its
        // background work was retired by the lifecycle sweep above.
        let current: std::collections::HashSet<ShellActivityKey> =
            candidates.iter().map(ShellActivityKey::of).collect();
        last_known.retain(|key, _| current.contains(key));

        for (candidate, sessions) in snapshot {
            let key = ShellActivityKey::of(&candidate);
            let busy = !sessions.is_empty();
            let previous = last_known.get(&key);
            let busy_changed = previous.map(|seen| seen.busy) != Some(busy);
            let sessions_changed = previous.is_none_or(|seen| seen.sessions != sessions);
            let previously_ended = previous.is_some_and(|seen| seen.turn_ended);

            // Cheap path, and the overwhelmingly common one: a pane whose scan
            // reads idle and whose reading did not just change has nothing to
            // report, so it never takes the state lock at all.
            if !busy_changed && !sessions_changed && !busy {
                continue;
            }

            // PRD #386 M6b: the trigger is edge-driven PLUS level-aware for a
            // pane whose agent does NOT report its own turns. A purely
            // edge-triggered monitor emits exactly one `ShellBusy` per busy
            // window — at the rising edge — so when Claude Code's Bash tool
            // backgrounds a command at its 120s cap and the `Stop` hook lands as
            // `Idle`, nothing ever re-promoted. So such a pane also re-emits when
            // the scan reads busy AND the session's status has regressed to
            // `Idle`/`Unknown`; `apply_event`'s `ShellBusy` arm still decides what
            // (if anything) to promote. It cannot spam: one `ShellBusy` lands, the
            // card reads `Working`, and the next poll sends nothing.
            //
            // Issue #1678: a pane whose agent DOES report its turns (D1) is the
            // opposite case. Its card legitimately stays `Idle` after `Stop`, so
            // that predicate would fire every 500 ms for as long as the command
            // runs. It emits only when the sampled session set changes (the
            // aggregate's command count or ages move), on a busy edge, and once
            // when its turn ends while busy — the `ShellBusy` an older
            // event-folding client has always received at that moment.
            let (session_id, view, status_regressed) = {
                let state = state.read().await;
                let Some(session_id) = state.pane_hook_session_id(&candidate.pane_id) else {
                    // A bare shell pane that never emitted an agent event has no
                    // session to update. Its reading is still recorded, as before.
                    last_known.insert(
                        key,
                        ShellActivityReading {
                            busy,
                            sessions,
                            turn_ended: previously_ended,
                        },
                    );
                    continue;
                };
                // PRD #386 M6b: the pane's CURRENT CARD — NOT
                // `sessions[session_id]`. `pane_hook_session_id` is the pane's
                // latest hook GENERATION and is the authoritative value for
                // `AgentEvent.session_id` (it is what the daemon's send guard
                // compares against); it is NOT a key into `sessions`, because a
                // same-agent `/clear` rolls the generation forward while
                // `apply_event`'s reuse guard keeps the card under its stable id.
                // The card is resolved with the newest-by-`last_activity` rule
                // the rest of the daemon uses (`pane_session_id`).
                let card = state
                    .pane_session_id(&candidate.pane_id)
                    .and_then(|card_id| state.sessions.get(&card_id));
                let view = state.background_view(&candidate.pane_id, &candidate.agent_id);
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
                (session_id, view, regressed)
            };

            let emit = if view.reports_turn_boundaries {
                busy_changed || sessions_changed || (busy && view.turn_ended && !previously_ended)
            } else {
                busy_changed || status_regressed
            };
            last_known.insert(
                key.clone(),
                ShellActivityReading {
                    busy,
                    sessions: sessions.clone(),
                    turn_ended: view.turn_ended,
                },
            );

            if !emit {
                // Nothing to tell a client, but a changed set still moves the
                // first-seen ages the aggregate will report once the pane's
                // turn ends (D5) — recorded only for the spawn that was sampled.
                if sessions_changed {
                    let mut guard = state.write().await;
                    if pty_registry.shell_activity_candidate_current(&candidate) {
                        guard.observe_command_sessions(
                            &candidate.pane_id,
                            &candidate.agent_id,
                            &sessions,
                        );
                    }
                }
                continue;
            }

            if !busy_changed && !view.reports_turn_boundaries {
                // Only ever the corrective re-emit — a transition logs
                // nothing, and a steady-state busy pane reaches here at most
                // once per regression, so this cannot become a per-tick line
                // even at `RUST_LOG=debug`. It is the one place a "why is the
                // badge still Idle?" investigation needs to look.
                debug!(
                    pane_id = %candidate.pane_id,
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
                pane_id: Some(candidate.pane_id.clone()),
                // PRD #386 M6b: the pane's agent id, which is what lets an
                // attached TUI resolve the event onto the card it already renders
                // for this pane instead of minting a phantom session beside it.
                // Issue #1678 D7: the id of the spawn that was SAMPLED, never
                // that of whatever card holds the pane by now — the sample is
                // revalidated against it below.
                agent_id: Some(candidate.agent_id.clone()),
                agent_version: None,
                schema_version: None,
                live_target: None,
                background: None,
                background_revision: None,
            };

            // One ordered ingestion step (apply + broadcast under a single
            // write-lock hold), exactly as the hook loop below does it — see
            // `ingest_event` for the interleaving this closes. Issue #1678 D7:
            // under that same hold, a sample whose spawn has been replaced or
            // has exited is dropped before it touches anything, and the sampled
            // set is recorded before the event that publishes it is applied.
            let ingested = ingest_event_unless(
                &state,
                &event_tx,
                &pty_registry,
                event,
                false,
                None,
                |state| {
                    if !pty_registry.shell_activity_candidate_current(&candidate) {
                        return true;
                    }
                    state.observe_command_sessions(
                        &candidate.pane_id,
                        &candidate.agent_id,
                        &sessions,
                    );
                    false
                },
            )
            .await;
            if !ingested {
                last_known.remove(&key);
            }
        }
    }
}

/// PRD #1589 D2: the `(agent id, cwd)` a `dispatch` is recorded and run under,
/// resolved as ONE pair from one record.
///
/// For an attested sender the pair comes from the attested generation's OWN
/// record, and only while that generation still holds the pane it claims. The
/// earlier lookup found "whatever live record names this pane" after the gate,
/// so a retained predecessor token could attest A while the lookup found its
/// successor B, pairing the dispatch with B's identity and cwd (auditor B1).
/// Now A is refused outright, and B is never paired with A's claim.
///
/// The token-less compatibility path is unchanged: a pane never issued a token,
/// or a message admitted under `DOT_AGENT_DECK_HOOK_PROVENANCE=warn`, still
/// resolves by pane, because there is no attested generation to bind to.
pub(crate) fn resolve_dispatch_caller(
    registry: &crate::agent_pty::AgentPtyRegistry,
    pane_id: &str,
    provenance: &crate::hook_provenance::Provenance,
) -> Option<(String, String)> {
    match provenance {
        crate::hook_provenance::Provenance::Attested { agent_id, .. } => {
            if registry.pane_current_agent_id(pane_id).as_deref() != Some(agent_id.as_str()) {
                return None;
            }
            let record = registry.agent_record_any(agent_id)?;
            if record.pane_id_env.as_deref() != Some(pane_id) {
                return None;
            }
            record.cwd.map(|cwd| (record.id, cwd))
        }
        _ => registry
            .agent_records()
            .into_iter()
            .find(|r| r.pane_id_env.as_deref() == Some(pane_id))
            .and_then(|r| r.cwd.map(|cwd| (r.id, cwd))),
    }
}

/// Issue #319: how many hook-socket connections the daemon serves at once.
///
/// **Where the number comes from.** A hook connection is short-lived by
/// construction — the bundled `hook` subcommand connects, writes one JSON line
/// and exits — so legitimate concurrency is set by how many producers can be
/// mid-send at the same instant, not by how many panes exist. The longest-held
/// connections are the reply-bearing verbs, and the slowest of those is
/// `dispatch`, which creates a git worktree and spawns an agent inside the
/// connection task. This repository's largest orchestration defines 6 roles, so
/// 32 is over five times the widest single unit it can start, and a hook event
/// queued behind them is *delayed* rather than discarded by the daemon (see
/// [`accept_hook_connection`] for what the one residual loss case is).
///
/// It is also the second factor in the daemon's worst-case hook-ingest
/// footprint: 32 connections x
/// [`MAX_HOOK_LINE_BYTES`](crate::bounded_read::MAX_HOOK_LINE_BYTES) is 256 MiB
/// of line buffer, against no bound at all before this. That product is the
/// reason the line cap sits below the attach socket's `MAX_FRAME_LEN` rather
/// than matching it.
///
/// What the cap does NOT bound is how long one connection may hold its slot:
/// there is no read timeout on this socket, so a peer that connects and never
/// writes holds a permit until it goes away. That is a deliberate scope line —
/// #903 and #319 ask for the two allocation bounds, and a same-uid producer
/// that wants to make the daemon unavailable has cheaper ways (it can signal the
/// daemon's process directly). Bounding *availability* needs an idle timeout and
/// belongs with #318's provenance work, which is where "which producer is doing
/// this?" becomes answerable at all.
pub const MAX_CONCURRENT_HOOK_CONNECTIONS: usize = 32;

/// Wait for a free connection slot, then accept one hook connection.
///
/// The permit is taken **before** `accept`, which is what makes this
/// backpressure rather than admission control: at the cap the daemon simply
/// stops accepting, and the next producer's connection waits in the kernel's
/// listen backlog until a slot frees. The daemon itself therefore discards
/// nothing, and — because a hook send is a `connect`, a small write and an
/// exit — the producer does not even block: its line sits in the socket buffer
/// and is read when the daemon gets to it. Rejecting the connection instead
/// would have been simpler and would have thrown away a legitimate event every
/// time a burst outran the cap.
///
/// The one loss case left is a burst deep enough to fill the *listen backlog*
/// as well, where `connect` fails at the producer. That is a better place for
/// it to surface than here: the producer gets an error it can report or retry,
/// rather than a write that appears to succeed into a daemon that will never
/// read it.
///
/// Cancellation-safe for the `tokio::select!` it is polled in: dropping this
/// future releases the permit, whether it was cancelled waiting for a slot or
/// waiting for a connection.
///
/// `at_cap` is the caller's latch, so the saturation warning fires on the
/// transition into saturation instead of once per waiting connection.
async fn accept_hook_connection(
    listener: &IpcListener,
    conn_limit: &Arc<tokio::sync::Semaphore>,
    limit: usize,
    at_cap: &mut bool,
) -> io::Result<(tokio::sync::OwnedSemaphorePermit, IpcStream)> {
    let permit = match Arc::clone(conn_limit).try_acquire_owned() {
        Ok(permit) => {
            *at_cap = false;
            permit
        }
        Err(_) => {
            if !*at_cap {
                *at_cap = true;
                warn!(
                    limit,
                    "hook socket at its concurrent-connection cap; further connections wait in \
                     the listen backlog until a slot frees — events are delayed, not dropped"
                );
            }
            // Unreachable in practice: the semaphore is owned by the loop and
            // never closed, so `acquire_owned` can only fail after a `close()`
            // nothing calls. Surfaced as an error rather than unwrapped so a
            // future close ends the loop instead of panicking it.
            Arc::clone(conn_limit)
                .acquire_owned()
                .await
                .map_err(io::Error::other)?
        }
    };
    let stream = listener.accept().await?;
    Ok((permit, stream))
}

/// How long one hook connection may go without completing a message before the
/// daemon reclaims its slot.
///
/// **This exists because [`MAX_CONCURRENT_HOOK_CONNECTIONS`] made a stalled
/// connection expensive.** Before that cap, a peer that connected and never
/// wrote cost one parked task and nothing else, and hook ingest carried on
/// around it. With 32 slots, 32 such peers stop ingest altogether — so the cap
/// on its own would have traded an unbounded-memory failure for an availability
/// one that is *cheaper* to reach. Found by Greptile on the PR that added the
/// cap, and correctly: the reclaim path is part of the bound, not a separate
/// nicety.
///
/// **60 seconds is over an order of magnitude above anything legitimate.**
/// Every producer this project ships is single-shot: `hook::send_to_socket`
/// connects, writes one line, flushes and drops the stream, and the
/// reply-bearing verbs (`get-seed`, `delegate`, `list-targets`) write,
/// half-close, read one reply under their own 5s client-side bound, and close.
/// None of them holds a connection idle for even a second.
///
/// **It is an idle bound, not a lifetime.** It wraps one `read_capped_line`
/// call — "read one message" — which gives two properties from one timer: a
/// peer that sends nothing is reclaimed, and so is one that *drips* bytes
/// without ever completing a line (the shape `error/socket/005` pins on the
/// client side, where an idle timeout alone was not enough because every byte
/// re-armed it). The timer restarts per message, so a hypothetical third-party
/// producer that keeps one connection open and streams events is unaffected as
/// long as its gaps stay under a minute — the one behaviour this narrows for
/// anything not shipped here, and stated rather than hidden.
///
/// What it does not close is a peer that behaves *just* well enough — one
/// complete message a minute, or a reconnect each time a slot frees. Telling
/// that apart from a real producer needs to know which producer it is, which is
/// #318's provenance work. What this closes is the leaked or stalled
/// connection, which is the case reachable by accident.
const HOOK_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How much of a rejected hook line reaches the log. See the call site in
/// [`run_hook_loop`] for why this is clamped at all.
const MALFORMED_LOG_PREFIX_BYTES: usize = 512;

/// Clamp a producer-supplied line to [`MALFORMED_LOG_PREFIX_BYTES`] for
/// logging, marking the cut so a truncated line is never mistaken for the whole
/// payload. Cuts on a char boundary, because the line is arbitrary UTF-8 and
/// slicing mid-character would panic.
fn clamp_for_log(line: &str) -> std::borrow::Cow<'_, str> {
    if line.len() <= MALFORMED_LOG_PREFIX_BYTES {
        return std::borrow::Cow::Borrowed(line);
    }
    let end = (0..=MALFORMED_LOG_PREFIX_BYTES)
        .rev()
        .find(|&i| line.is_char_boundary(i))
        .unwrap_or(0);
    std::borrow::Cow::Owned(format!("{}…<truncated>", &line[..end]))
}

/// Issue #1082: one producer-supplied hook LINE, bounded *and* escaped, ready
/// for a log sink — the daemon's two raw-line diagnostics both go through here.
///
/// Composed from the two halves that already exist rather than respelled:
/// [`clamp_for_log`] answers "how much of it reaches the log" and
/// [`crate::config_validation::escape_for_terminal`] answers "which of its
/// characters a terminal would ACT on". The bound therefore stays issue #903's
/// [`MALFORMED_LOG_PREFIX_BYTES`] — 512 bytes, with a cut marker — rather than
/// [`crate::config_validation::MAX_QUOTED_VALUE_CHARS`], which is sized for a
/// value that is not prose and would leave 120 characters of a JSON hook event:
/// enough to lose the very `event_type` the `raw_line` warning exists to show.
/// That is why this calls the escaper directly instead of
/// [`crate::config_validation::escape_id_for_log`] — bounding is already done,
/// and doing it twice would be the truncation this exists to avoid.
///
/// Escaping AFTER bounding, deliberately: escaping first would spend the budget
/// on escape expansions rather than on payload. The expansion is still bounded
/// — 512 bytes of ESC becomes 4096 characters and no more — which is the same
/// trade [`crate::config_validation::escape_field_for_log`] makes.
///
/// Issue #318: the hook capability token is taken out FIRST
/// ([`redact_hook_token`]). Both raw-line diagnostics log a line a producer
/// wrote, every first-party producer now puts its pane's token on it, and a
/// capability in `deck.log` is exactly the leak the token check exists to
/// avoid — already a live one for a `DaemonMessage` line that failed to decode.
///
/// PRD #1497 audit AU-B3: and so is a turn's final reply. A turn-ending line
/// carries the agent's reply under [`crate::turn_reply::TURN_REPLY_LINE_KEY`]
/// (a hook's `Stop`, the OpenCode plugin's idle report, Pi's `agent-event`),
/// and that text reaches only `subscribe-turn-replies` connections — never the
/// log, whichever diagnostic the line trips ([`redact_hook_token`] says how).
fn hook_line_for_log(line: &str) -> String {
    crate::config_validation::escape_for_terminal(&clamp_for_log(&redact_hook_token(line)))
        .into_owned()
}

/// Issue #318: `line` with the hook capability token taken out, for a log.
///
/// A line that parses as JSON is walked whole (Greptile, PR #1559): every
/// member named `token` is removed at any depth that survives parsing, and
/// every contiguous run of a token's length or more of hex digits in any other
/// string — or in a member's name — is masked, so a capability nested under a
/// field the daemon does not know, or inside a metadata map, is caught as well
/// as the top-level one. It works on the decoded values, so a JSON-escaped
/// spelling is caught too, and every other member — the `event_type` a
/// diagnostic is about — survives.
///
/// What is logged for such a line is always the RE-SERIALIZED value, never the
/// raw text, even when the walk changed nothing: parsing keeps only the last of
/// a set of duplicate members, so an earlier occurrence holding a capability is
/// invisible to the walk and would come back with the raw line (round-5 audit,
/// PR #1559). Re-serializing also normalises the line, which a diagnostic does
/// not need preserved.
///
/// The masking is shape-based, not a decoder: a capability re-encoded some
/// other way — base64, percent-encoding, hex split by separators — under a key
/// other than `token` is not recognised, and a legitimate value of the same
/// shape, such as a SHA-256 digest, is masked along with a real one. A line
/// that does not parse cannot be walked: every such hex run in it is masked,
/// and a line containing a `\u` escape, which could spell a token in a form
/// the mask does not recognise, is not logged at all beyond saying so.
///
/// A turn reply is withheld the same two ways (PRD #1497): in a line that
/// parses, every [`crate::turn_reply::TURN_REPLY_LINE_KEY`] member at any depth
/// is replaced by its serialized length; in one that does not, nothing from the
/// first occurrence of that key onwards is logged, only how many bytes were
/// withheld. Every producer serializes the key after the event's identifiers
/// (`serde_json` writes members in sorted order), so what is logged of such a
/// line is still the part a diagnostic needs.
fn redact_hook_token(line: &str) -> std::borrow::Cow<'_, str> {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) {
        withhold_turn_replies(&mut value);
        redact_json_capabilities(&mut value);
        return std::borrow::Cow::Owned(value.to_string());
    }
    if line.contains("\\u") {
        return std::borrow::Cow::Borrowed("<withheld: an unparseable line with a \\u escape>");
    }
    let (head, withheld) = match line.find(crate::turn_reply::TURN_REPLY_LINE_KEY) {
        Some(at) => (&line[..at], Some(line.len() - at)),
        None => (line, None),
    };
    let head = match mask_capability_runs(head) {
        Some(masked) => std::borrow::Cow::Owned(masked),
        None => std::borrow::Cow::Borrowed(head),
    };
    match withheld {
        None => head,
        Some(bytes) => std::borrow::Cow::Owned(format!(
            "{head}<withheld: a turn reply and the {bytes} bytes from it on>"
        )),
    }
}

/// [`redact_hook_token`]'s turn-reply half over a parsed line: every
/// [`crate::turn_reply::TURN_REPLY_LINE_KEY`] member, at any depth, becomes a
/// note of its serialized length.
fn withhold_turn_replies(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => items.iter_mut().for_each(withhold_turn_replies),
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if key == crate::turn_reply::TURN_REPLY_LINE_KEY {
                    *item = serde_json::Value::String(format!(
                        "<withheld: {} bytes>",
                        item.to_string().len()
                    ));
                } else {
                    withhold_turn_replies(item);
                }
            }
        }
        _ => {}
    }
}

/// [`redact_hook_token`]'s walk over a parsed line: removes every `token`
/// member and masks contiguous capability-length hex runs in every string and
/// member name, at every depth parsing kept. Returns whether anything changed.
fn redact_json_capabilities(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => match mask_capability_runs(text) {
            Some(masked) => {
                *text = masked;
                true
            }
            None => false,
        },
        // Every item is visited: a short-circuiting `any` would stop at the
        // first redaction and log whatever followed it.
        serde_json::Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= redact_json_capabilities(item);
            }
            changed
        }
        serde_json::Value::Object(map) => {
            let mut changed = map.remove("token").is_some();
            let entries = std::mem::take(map);
            for (key, mut item) in entries {
                changed |= redact_json_capabilities(&mut item);
                let key = match mask_capability_runs(&key) {
                    Some(masked) => {
                        changed = true;
                        masked
                    }
                    None => key,
                };
                map.insert(key, item);
            }
            changed
        }
        _ => false,
    }
}

/// `text` with every run of [`crate::hook_provenance::TOKEN_LEN`] or more hex
/// digits replaced by `<redacted>`, or `None` when it holds no such run.
fn mask_capability_runs(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        let run = bytes[i..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        if run >= crate::hook_provenance::TOKEN_LEN {
            out.push_str("<redacted>");
            changed = true;
            i += run;
        } else if run > 0 {
            out.push_str(&text[i..i + run]);
            i += run;
        } else {
            let ch = text[i..].chars().next().expect("i is on a char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    changed.then_some(out)
}

/// PRD #1666: the refusal code for an inbox request no token attests.
const INBOX_UNATTESTED: &str = "unattested";
const INBOX_UNATTESTED_MESSAGE: &str = "inbox requests need the hook capability token dot-agent-deck gave this pane's agent, \
     and this one carried none the deck could check; run it from inside the agent's own pane";
/// PRD #1666: the refusal code for an inbox request naming another agent.
const INBOX_WRONG_AGENT: &str = "wrong_agent";
const INBOX_WRONG_AGENT_MESSAGE: &str =
    "this request names an agent other than the one its hook capability token was issued to";

/// PRD #1666: `ack <m-id>` — acknowledge an inbox message for the agent the
/// token attests.
fn handle_inbox_ack(
    registry: &AgentPtyRegistry,
    signal: &crate::event::AckSignal,
    agent_id: &str,
) -> crate::event::AckDelivery {
    if !crate::inbox::is_valid_message_id(&signal.delivery_id) {
        warn!(
            pane_id = %escape_id_for_log(&signal.pane_id),
            id_len = signal.delivery_id.len(),
            "Received an ack whose id is not shaped like an inbox message id; ignored"
        );
        return crate::event::AckDelivery::Unknown;
    }
    let outcome = registry.inbox().ack(
        &signal.delivery_id,
        &signal.pane_id,
        agent_id,
        crate::inbox::AckHow::Explicit,
    );
    info!(
        pane_id = %escape_id_for_log(&signal.pane_id),
        message_id = %signal.delivery_id,
        outcome = ?outcome,
        "Received inbox ack"
    );
    outcome.into()
}

/// PRD #1666 part 3 (R-1): why the deck refused an inbox request whose token
/// attests the pane, each cause with its own code and a message naming the
/// remedy ([`Self::message`]).
///
/// Membership of a pane is the pane's current capability, which the gate
/// ahead of these checks has resolved token → spawn record → pane, plus the
/// deck's own OS user, as the kernel recorded it for the connection. Where
/// the caller sits in the process table is not membership: Claude Code can
/// run a conversation in a background session host outside the process tree
/// the deck recorded for the pane, and that host's tool shell and hooks are
/// still the pane's agent. Process identity still decides which instance is
/// the agent's receiver ([`ComponentUse::Receiver`]), and whether the
/// connecting process is still the one that connected
/// ([`still_the_accepted_peer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComponentRefusal {
    /// The connecting process runs as another OS user than the deck's.
    OtherUser,
    /// Neither the pane's agent process tree nor the deck's own user can be
    /// shown: a platform that does not read the client's user, where the
    /// caller is outside the tree or the pane has no agent process.
    NotMember,
    /// The deck cannot tell which process made the connection, or that the
    /// process is still the one that made it.
    Unidentified,
    /// The receiver instance the request names is neither the caller nor one
    /// of its parents.
    NotCaller,
    /// An agent of the same kind as the receiver the request names sits
    /// between it and the caller (audit F3).
    NestedAgent,
    /// The receiver the request names is, or resolves through shells to, a
    /// process the agent itself runs under: the deck, a terminal, init.
    AboveAgent,
}

impl ComponentRefusal {
    fn code(self) -> &'static str {
        match self {
            ComponentRefusal::OtherUser => "inbox_other_user",
            ComponentRefusal::NotMember => "inbox_not_agent_process",
            ComponentRefusal::Unidentified => "inbox_peer_unidentified",
            ComponentRefusal::NotCaller => "inbox_component_not_caller",
            ComponentRefusal::NestedAgent => "inbox_nested_agent",
            ComponentRefusal::AboveAgent => "inbox_receiver_above_agent",
        }
    }

    /// The cause and what to do about it, for the caller. Fixed text: it never
    /// echoes a token, a pid or anything the request carried.
    fn message(self) -> &'static str {
        match self {
            ComponentRefusal::OtherUser => {
                "this process runs as a different OS user from the deck, and only the deck's own \
                 user may use a pane's inbox; run it as the user that runs the deck"
            }
            ComponentRefusal::NotMember => {
                "this process is not the pane's agent or one of its processes, and the deck cannot \
                 confirm here that it runs as the deck's own user; run it from the agent's own pane"
            }
            ComponentRefusal::Unidentified => {
                "the deck could not identify the process that made this connection: it may have \
                 exited or handed the connection to another process, so it may not take or settle \
                 the agent's messages; run the command again from the agent's own pane, and keep \
                 it running until the deck answers"
            }
            ComponentRefusal::NotCaller => {
                "the receiver this request names is not the process that sent it or one of its \
                 parents, so the deck will not register it; run the inbox command from the agent's \
                 own integration"
            }
            ComponentRefusal::NestedAgent => {
                "an agent of the same kind sits between this process and the agent it names, so a \
                 nested agent cannot act as that agent's receiver; run the receiver from the \
                 pane's own agent, not from an agent it started"
            }
            ComponentRefusal::AboveAgent => {
                "the receiver this request names is, or runs under shells that lead to, a process \
                 the agent itself runs under, such as the deck or a terminal, so the deck will not \
                 register it; run the inbox command from the agent's own integration, not from a \
                 shell outside the agent"
            }
        }
    }
}

/// PRD #1666 part 2: the refusal code of a receipt verb asking for what is
/// not served on receipts — a wait other than Claude's once-waiter, or a
/// fetch a stream bound or the CLI made (H4 moves the CLI).
const INBOX_RECEIPT_UNSUPPORTED: &str = "unsupported";

/// [`INBOX_RECEIPT_UNSUPPORTED`]'s message.
const INBOX_RECEIPT_UNSUPPORTED_MESSAGE: &str =
    "this receiver is not served on one-shot receipts, so nothing was handed to it";

/// TEST knob, honoured only in a build with the `e2e` feature: inbox
/// components may connect from outside the pane's agent process tree, as the
/// synthetic lane-1 tests that drive the inbox wire from the test process do.
const ENV_TEST_UNBOUND_COMPONENTS: &str = "DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS";

/// Which inbox verb a component is admitted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComponentUse {
    /// A post, or taking or settling the agent's mail: any member of the pane
    /// may use its token, tools included.
    Cli,
    /// Arming a waiter or announcing a wake: registering the agent's native
    /// receiver ([`receiver_instance`]).
    Receiver,
}

/// How much the deck must be able to show about a component for an inbox
/// verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    /// Refused only on a positive "no"; a check that cannot say admits. For
    /// the verbs that post or settle a job.
    BestEffort,
    /// PR #1735 second review: admitted only on positive evidence. For the
    /// verbs that take or settle the agent's own mail — fetch, ack, get-seed
    /// and arming a waiter — and, since the third review, announcing a wake,
    /// which pins the agent's receiver: a caller can make its identity
    /// unreadable on purpose, since a connection whose creator exited still
    /// reports that creator as its peer.
    Required,
}

/// [`admit_inbox_component_with`] on this platform's process table, unless
/// the `e2e`-only [`ENV_TEST_UNBOUND_COMPONENTS`] knob is set.
/// [`Evidence::Required`] falls back to [`Evidence::BestEffort`] on a
/// platform without [`crate::platform::proc::ANCESTRY_LOOKUP`], where no
/// caller could prove anything.
fn admit_inbox_component(
    peer: Option<AcceptedPeer>,
    claimed: Option<u32>,
    root: Option<u32>,
    used_as: ComponentUse,
    evidence: Evidence,
) -> Result<Option<u32>, ComponentRefusal> {
    #[cfg(feature = "e2e")]
    if std::env::var(ENV_TEST_UNBOUND_COMPONENTS)
        .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
    {
        return Ok(claimed);
    }
    #[cfg(not(feature = "e2e"))]
    let _ = ENV_TEST_UNBOUND_COMPONENTS;
    let evidence = if crate::platform::proc::ANCESTRY_LOOKUP {
        evidence
    } else {
        Evidence::BestEffort
    };
    admit_inbox_component_with(peer, claimed, root, used_as, evidence, &PLATFORM_LOOKUPS)
}

/// PR #1735 third review (F2): the connecting process as the daemon accepted
/// its hook connection — the pid the kernel reported, and when that process
/// started ([`crate::platform::proc::process_start_of`]), `None` where that
/// cannot be read. A connection keeps reporting the process that made it after
/// that process exits, and its pid can then be reused; the start time tells
/// the process that connected from a later one holding the same number.
///
/// PRD #1666 part 3 (R-1): and whether it runs as the deck's own user, as the
/// kernel recorded it when the connection was made
/// ([`crate::platform::peercred::peer_is_same_user`]), `None` where that
/// cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AcceptedPeer {
    pid: u32,
    started: Option<u64>,
    same_user: Option<bool>,
}

impl AcceptedPeer {
    /// The peer `pid` names, read now, as the connection is accepted, running
    /// as the deck's own user or not (`same_user`).
    fn accept(pid: Option<u32>, same_user: Option<bool>) -> Option<Self> {
        pid.map(|pid| Self {
            pid,
            started: crate::platform::proc::process_start_of(pid),
            same_user,
        })
    }
}

/// The process-table reads the inbox admission makes, injectable so a test
/// can stand in a process table — a reused pid included.
struct ProcessLookups {
    descends: fn(u32, u32) -> Option<bool>,
    parent_of: fn(u32) -> Option<u32>,
    executable_of: fn(u32) -> Option<std::path::PathBuf>,
    started: fn(u32) -> Option<u64>,
}

/// This platform's process table.
const PLATFORM_LOOKUPS: ProcessLookups = ProcessLookups {
    descends: crate::platform::proc::descends_from,
    parent_of: crate::platform::proc::parent_pid_of,
    executable_of: crate::platform::proc::executable_of,
    started: crate::platform::proc::process_start_of,
};

/// Whether the connecting process `peer` may use, for `used_as`, the token of
/// the generation whose agent process is `root`; answers the receiver
/// instance to arm or pin under, or `claimed` for [`ComponentUse::Cli`].
///
/// [`admit_component`] decides membership and the receiver. Then, PR #1735
/// third review (F2), the peer must still be the process accepted
/// ([`still_the_accepted_peer`]): read after the process-table walks, the
/// same start time means that process was alive from the accept through the
/// walks, so they read its ancestry and not a later process's that reuses its
/// pid.
fn admit_inbox_component_with(
    peer: Option<AcceptedPeer>,
    claimed: Option<u32>,
    root: Option<u32>,
    used_as: ComponentUse,
    evidence: Evidence,
    lookups: &ProcessLookups,
) -> Result<Option<u32>, ComponentRefusal> {
    let admitted = admit_component(
        peer,
        claimed,
        root,
        used_as,
        evidence,
        lookups.descends,
        lookups.parent_of,
        lookups.executable_of,
    )?;
    if let Some(peer) = peer {
        still_the_accepted_peer(peer, lookups.started, evidence)?;
    }
    Ok(admitted)
}

/// PR #1735 third review (F2): whether `peer.pid` still names the process
/// accepted as `peer`. A different start time is a positive "no" and refuses
/// under either [`Evidence`]; a start time that cannot be read now, or could
/// not be when the connection was accepted, refuses only
/// [`Evidence::Required`].
fn still_the_accepted_peer(
    peer: AcceptedPeer,
    started_of: fn(u32) -> Option<u64>,
    evidence: Evidence,
) -> Result<(), ComponentRefusal> {
    match (peer.started, started_of(peer.pid)) {
        (Some(then), Some(now)) if then == now => Ok(()),
        (Some(_), Some(_)) => Err(ComponentRefusal::Unidentified),
        _ if evidence == Evidence::Required => Err(ComponentRefusal::Unidentified),
        _ => Ok(()),
    }
}

/// PR #1735 review: whether the connecting process `peer` may take or settle
/// `agent_id`'s own mail — `inbox_fetch`, `inbox_committed`, `ack` and the
/// `get_seed` alias — on the same terms as the verbs that post
/// ([`ComponentUse::Cli`]), but only on positive evidence
/// ([`Evidence::Required`]). Logs a refusal.
fn mail_taker_admitted(
    registry: &AgentPtyRegistry,
    peer: Option<AcceptedPeer>,
    agent_id: &str,
    verb: &str,
    pane_id: &str,
) -> Result<(), ComponentRefusal> {
    let admitted = admit_inbox_component(
        peer,
        None,
        registry.child_pid(agent_id),
        ComponentUse::Cli,
        Evidence::Required,
    )
    .map(|_| ());
    if let Err(refusal) = admitted {
        warn!(
            verb,
            pane_id = %escape_id_for_log(pane_id),
            reason = refusal.code(),
            "hook socket: refused an inbox verb from a process the deck cannot admit for the \
             pane"
        );
    }
    note_admission(registry, pane_id, agent_id, admitted);
    admitted
}

/// PRD #1666 part 3 (R-2): tell the inbox how the deck answered `agent_id`'s
/// own request to take its messages or register its receiver in `pane_id`: a
/// refusal raises that card's notice, an admission ends it. Only while
/// `agent_id` holds the pane, and the request's capability attested
/// `agent_id` before admission ran, so a request naming another agent's pane
/// never marks that agent's card.
fn note_admission(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    agent_id: &str,
    admitted: Result<(), ComponentRefusal>,
) {
    if registry.pane_current_agent_id(pane_id).as_deref() != Some(agent_id) {
        return;
    }
    match admitted {
        Ok(()) => registry.inbox().admission_admitted(pane_id, agent_id),
        Err(refusal) => registry
            .inbox()
            .admission_refused(pane_id, agent_id, refusal.code()),
    }
}

/// PRD #1666 part 3 (R-1): admit `peer` for `used_as` the token of the
/// generation whose agent process is `root`. `claimed` is the receiver
/// instance a [`ComponentUse::Receiver`] request names (the process that runs
/// its watcher, which the stream is keyed by).
///
/// **Membership.** The pane's agent process tree is membership, and so is the
/// deck's own OS user: a peer outside the tree is admitted when the kernel
/// recorded it as the deck's user ([`AcceptedPeer::same_user`]), since the
/// gate ahead of this admitted its token as the pane's current capability.
/// Another user outside the tree is refused under either [`Evidence`]
/// ([`ComponentRefusal::OtherUser`]); inside it, the tree admits whatever the
/// user reads. Where the user cannot be read (no peer,
/// or a platform that does not read it), the process tree alone decides, as
/// it did before: a positive "outside" refuses, and so, under
/// [`Evidence::Required`], does a walk that cannot say, a pane with no agent
/// process and a peer the kernel does not identify.
///
/// **The receiver** ([`ComponentUse::Receiver`]). The instance a request
/// names must be the caller or one of its parents, and not a process above
/// the agent's own ([`ComponentRefusal::AboveAgent`]), so a member cannot
/// register an arbitrary process as the agent's receiver; it is pinned through
/// hook shells to the process that runs them ([`receiver_instance`]), and what
/// that resolves to must not be above the agent's either, read on positive
/// evidence under [`Evidence::Required`]; and a
/// process between the caller and that instance running the instance's own
/// executable — an agent of the same kind, started inside the one whose
/// receiver it names — is refused ([`nested_between`], audit F3).
#[allow(clippy::too_many_arguments)]
fn admit_component(
    peer: Option<AcceptedPeer>,
    claimed: Option<u32>,
    root: Option<u32>,
    used_as: ComponentUse,
    evidence: Evidence,
    descends: impl Fn(u32, u32) -> Option<bool>,
    parent_of: impl Fn(u32) -> Option<u32> + Copy,
    executable_of: impl Fn(u32) -> Option<std::path::PathBuf> + Copy,
) -> Result<Option<u32>, ComponentRefusal> {
    let required = evidence == Evidence::Required;
    let Some(peer) = peer else {
        return if required {
            Err(ComponentRefusal::Unidentified)
        } else {
            Ok(claimed)
        };
    };
    let in_tree = root.map(|root| descends(peer.pid, root));
    match (in_tree, peer.same_user) {
        (Some(Some(true)), _) | (_, Some(true)) => {}
        (_, Some(false)) => return Err(ComponentRefusal::OtherUser),
        (Some(Some(false)), None) => return Err(ComponentRefusal::NotMember),
        (None, None) if required => return Err(ComponentRefusal::NotMember),
        (Some(None), None) if required => return Err(ComponentRefusal::Unidentified),
        (_, None) => {}
    }
    let Some(claimed) = claimed.filter(|_| used_as == ComponentUse::Receiver) else {
        return Ok(claimed);
    };
    // Whether the agent `root` descends from `candidate`. A walk that ran out
    // of steps is no answer here (round 1's U3): `descends` reads it as "no",
    // which suits a membership check that refuses on "no", not this one,
    // which admits on it.
    let below = |root: u32, candidate: u32| match descends(root, candidate) {
        Some(false)
            if crate::platform::proc::ancestry_with(root, candidate, parent_of)
                == crate::platform::proc::Ancestry::TooDeep =>
        {
            None
        }
        answer => answer,
    };
    // Nor a process above the agent — the deck, a terminal, init — which the
    // caller descends from as every process of the pane does.
    if claimed <= 1 {
        return Err(ComponentRefusal::AboveAgent);
    }
    if let Some(root) = root.filter(|&root| claimed != root) {
        match below(root, claimed) {
            Some(false) => {}
            Some(true) => return Err(ComponentRefusal::AboveAgent),
            None if required => return Err(ComponentRefusal::Unidentified),
            None => {}
        }
    }
    match descends(peer.pid, claimed) {
        Some(true) => {}
        Some(false) => return Err(ComponentRefusal::NotCaller),
        None if required => return Err(ComponentRefusal::Unidentified),
        None => {}
    }
    let instance = receiver_instance(claimed, root, parent_of, executable_of);
    // PRD #1666 part 3, round 1: the resolved instance is held to what a
    // named one is. In the tree the walk stops at the agent; a caller outside
    // it can name a shell on a branch it shares with the agent, which would
    // climb to the terminal or the deck above it.
    if let Some(root) = root.filter(|&root| instance != claimed && instance != root) {
        match below(root, instance) {
            Some(false) => {}
            Some(true) => return Err(ComponentRefusal::AboveAgent),
            None if required => return Err(ComponentRefusal::Unidentified),
            None => {}
        }
    }
    if nested_between(peer.pid, instance, parent_of, executable_of) == Some(true) {
        return Err(ComponentRefusal::NestedAgent);
    }
    Ok(Some(instance))
}

/// Audit F3: the process an agent's receiver is pinned by — `claimed`, or,
/// when `claimed` is a shell the agent runs a hook through, the nearest
/// ancestor that is not a shell. Inside the agent's tree the walk stops at its
/// own process `root`; from a shell outside it nothing here bounds the walk,
/// so [`admit_component`] refuses a result above `root`.
///
/// Claude Code runs every hook through a new `sh` that does not `exec` it
/// (measured on 2.1.299: `claude ── sh ── hook`), so its announcement and each
/// of its waiters name a different shell; the agent above them is the one
/// receiver. Without `root` there is nothing to bound the walk, so `claimed`
/// stands, as it does where a process cannot be read or the shells run deeper
/// than the walk reads (`MAX_SHELLS`) without reaching one that is not a shell.
fn receiver_instance(
    claimed: u32,
    root: Option<u32>,
    parent_of: impl Fn(u32) -> Option<u32>,
    executable_of: impl Fn(u32) -> Option<std::path::PathBuf>,
) -> u32 {
    /// More shells than any hook runner stacks.
    const MAX_SHELLS: usize = 8;
    let Some(root) = root else {
        return claimed;
    };
    let mut current = claimed;
    for _ in 0..MAX_SHELLS {
        if current == root || !executable_of(current).is_some_and(|exe| is_shell(&exe)) {
            return current;
        }
        match parent_of(current) {
            Some(parent) if parent > 1 => current = parent,
            _ => return claimed,
        }
    }
    claimed
}

/// Whether `exe` is a POSIX-style shell, by its file name.
fn is_shell(exe: &std::path::Path) -> bool {
    let name = exe.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    matches!(
        name,
        "sh" | "dash" | "bash" | "zsh" | "ksh" | "mksh" | "ash" | "fish" | "busybox"
    )
}

/// Audit F3: whether a process strictly between `peer` and `claimed` — one of
/// `peer`'s ancestors below `claimed` — runs the same executable as
/// `claimed`: an agent of the same kind nested inside the one it names, as a
/// tool can start. `None` when the walk cannot say: a process exited, or this
/// platform cannot read a parent or an executable.
fn nested_between(
    peer: u32,
    claimed: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    executable_of: impl Fn(u32) -> Option<std::path::PathBuf>,
) -> Option<bool> {
    /// Bounds the walk, as `descends_from`'s; nothing keeps a real tree
    /// shallower, and a walk that runs out answers "not nested".
    const MAX_DEPTH: usize = crate::platform::proc::MAX_ANCESTRY_DEPTH;
    if peer == claimed {
        return Some(false);
    }
    let kind = executable_of(claimed)?;
    let mut current = parent_of(peer)?;
    for _ in 0..MAX_DEPTH {
        if current == claimed || current <= 1 {
            return Some(false);
        }
        if executable_of(current)? == kind {
            return Some(true);
        }
        current = parent_of(current)?;
    }
    Some(false)
}

/// PRD #1666 part 3 (H5): the answer to `get-seed`, which takes no message
/// any more. Until H5 it handed the agent's oldest queued message over and
/// acknowledged it in the same step, the verb an extension from before the
/// inbox read its seed with; a seed is the pane's first inbox message, which
/// the agent's own receiver takes. Every caller is answered at once with no
/// seed and a refusal an older reader ignores, and nothing is taken or
/// acknowledged. A Pi generation (`attested`, the pane's `live` agent) asking
/// runs an extension from before the receipt stream, so its card says to
/// restart the agent (G10).
fn get_seed_withdrawn(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    attested: Option<&str>,
    live: Option<&str>,
) -> crate::event::GetSeedResponse {
    if let Some(agent) = attested.filter(|agent| live == Some(*agent))
        && pi_generation(registry, pane_id, agent)
    {
        registry.inbox().refuse_old_adapter(pane_id, agent);
    }
    crate::event::GetSeedResponse::withdrawn()
}

/// PRD #1666 G10: whether `agent_id` in `pane_id` is a Pi generation: started
/// as Pi, or whose extension announced itself.
fn pi_generation(registry: &AgentPtyRegistry, pane_id: &str, agent_id: &str) -> bool {
    registry.spawn_agent_type(agent_id) == Some(crate::event::AgentType::Pi)
        || registry.inbox().announced_component(pane_id, agent_id)
            == Some(crate::event::InboxComponent::PiExtension)
}

/// The budget a fetch asked for: `max_bytes` when set, else `max_chars`.
fn fetch_budget(req: &crate::event::InboxFetchRequest) -> Option<crate::event::InboxBudget> {
    req.max_json_bytes
        .map(crate::event::InboxBudget::JsonBytes)
        .or(req.max_bytes.map(crate::event::InboxBudget::Bytes))
        .or(req.max_chars.map(crate::event::InboxBudget::Chars))
}

/// PRD #1666 part 3 (H4): the hook component an `inbox_receipt_fetch` is
/// served as, or `None` when the receipt verb does not serve it (it is
/// refused `unsupported`, nothing issued). A hook names its component, and one
/// without it is Claude Code's prompt hook (part 2's); only Claude Code's and
/// Codex's hooks fetch this way. A model-run `inbox fetch` names none: it is
/// served as the hook of the generation's agent, `agent`, the spawn's type
/// else the one its hooks reported (a routing hint for the budget and the
/// log, not authority: the fetch is admitted like any other). A fetch a
/// stream bound (`rx`) is that stream's doorbell substitution, on the legacy
/// verb, never this one.
fn receipt_fetch_component(
    req: &crate::event::InboxFetchRequest,
    agent: Option<crate::event::AgentType>,
) -> Option<crate::event::InboxComponent> {
    use crate::event::{AgentType, InboxComponent, InboxFetchConsumer};
    if req.rx.is_some() {
        return None;
    }
    match req.consumer {
        InboxFetchConsumer::Cli => Some(match agent {
            Some(AgentType::ClaudeCode) => InboxComponent::ClaudeHooks,
            Some(AgentType::Codex) => InboxComponent::CodexHooks,
            Some(AgentType::Devin) => InboxComponent::DevinHooks,
            _ => InboxComponent::Other,
        }),
        InboxFetchConsumer::DoorbellHook | InboxFetchConsumer::TurnEndHook => {
            match req.component.unwrap_or(InboxComponent::ClaudeHooks) {
                component @ (InboxComponent::ClaudeHooks | InboxComponent::CodexHooks) => {
                    Some(component)
                }
                _ => None,
            }
        }
    }
}

/// PRD #1666 G10, part 3 (H5): whether `req`, a fetch on the legacy verb, is
/// a doorbell fetch from an OpenCode plugin or Pi extension from before the
/// receipt stream (no stream bound): refused as an out-of-date adapter.
fn old_adapter_doorbell(
    registry: &AgentPtyRegistry,
    req: &crate::event::InboxFetchRequest,
    agent_id: &str,
) -> bool {
    use crate::event::{AgentType, InboxComponent, InboxFetchConsumer};
    req.consumer == InboxFetchConsumer::DoorbellHook
        && req.rx.is_none()
        && (registry.spawn_agent_type(agent_id) == Some(AgentType::OpenCode)
            || pi_generation(registry, &req.pane_id, agent_id)
            || matches!(
                registry.inbox().announced_component(&req.pane_id, agent_id),
                Some(InboxComponent::OpenCodePlugin | InboxComponent::OpenCodeMini)
            ))
}

/// PRD #1666 part 3 (H5): whether `req`, a fetch on the legacy verb, is an
/// agent hook's: a doorbell or turn-end fetch with no stream bound, from no
/// out-of-date adapter. It meets the one-shot floor ([`crate::inbox_hub::serve_hook`]):
/// one from an older dot-agent-deck, declaring no integration, is refused.
fn legacy_verb_hook_fetch(
    registry: &AgentPtyRegistry,
    req: &crate::event::InboxFetchRequest,
    agent_id: &str,
) -> bool {
    req.consumer != crate::event::InboxFetchConsumer::Cli
        && req.rx.is_none()
        && !old_adapter_doorbell(registry, req, agent_id)
}

/// PRD #1666 part 3 (H5): the hook component a legacy-verb hook fetch from
/// `agent_id` comes from, for its card's remedy and its budget: the agent's
/// type (the spawn's, else the one its hooks reported, a hint and not
/// authority), else a Claude component it announced, else Claude Code's hooks
/// for a fetch declaring the one-shot integration (only Claude's hook did, on
/// this verb, in part 2), else none the deck can name.
fn legacy_hook_component(
    registry: &AgentPtyRegistry,
    req: &crate::event::InboxFetchRequest,
    agent_id: &str,
) -> crate::event::InboxComponent {
    use crate::event::{AgentType, InboxComponent};
    if let Some(component) = req.component {
        return component;
    }
    match registry
        .spawn_agent_type(agent_id)
        .or_else(|| registry.reported_agent_type(agent_id))
    {
        Some(AgentType::ClaudeCode) => InboxComponent::ClaudeHooks,
        Some(AgentType::Codex) => InboxComponent::CodexHooks,
        Some(AgentType::Devin) => InboxComponent::DevinHooks,
        _ => match registry.inbox().announced_component(&req.pane_id, agent_id) {
            Some(InboxComponent::ClaudeHooks | InboxComponent::ClaudeWaiter) => {
                InboxComponent::ClaudeHooks
            }
            _ if req.hook_integration.is_some() => InboxComponent::ClaudeHooks,
            _ => InboxComponent::Other,
        },
    }
}

/// PRD #1666 part 3 (H5): what an `inbox fetch` from an older dot-agent-deck
/// is told: it fetches the way the deck no longer serves.
const CLI_FETCH_TOO_OLD: &str = "this dot-agent-deck is older than the deck daemon and can no longer take messages, so none was taken and they stay waiting: run inbox fetch with the dot-agent-deck the daemon runs, or upgrade dot-agent-deck";

fn handle_inbox_fetch(
    registry: &AgentPtyRegistry,
    req: &crate::event::InboxFetchRequest,
    agent_id: &str,
) -> crate::event::InboxWaitReply {
    use crate::event::InboxFetchConsumer;
    let budget = fetch_budget(req);
    let doorbell = req
        .doorbell
        .as_deref()
        .filter(|id| crate::inbox::is_valid_message_id(id));
    if req.doorbell.is_some() && doorbell.is_none() {
        return crate::event::InboxWaitReply::messages(Vec::new());
    }
    // PRD #1666 §4, §11: a doorbell substitution by a receipt component binds
    // its attempts to that component's own stream; one from an OpenCode
    // plugin below the floor is refused, and its card says why.
    if let Some(rx) = req.rx {
        if req
            .adapter_integration
            .is_none_or(|declared| declared < crate::inbox_hub::ADAPTER_INTEGRATION)
        {
            registry.inbox().refuse_old_adapter(&req.pane_id, agent_id);
            return crate::event::InboxWaitReply::refused(
                "adapter_too_old",
                crate::inbox_display::ADAPTER_TOO_OLD,
            );
        }
        let fetched = registry
            .inbox()
            .receipt_fetch(&req.pane_id, agent_id, rx, doorbell, budget);
        info!(
            pane_id = %escape_id_for_log(&req.pane_id),
            consumer = ?req.consumer,
            rx,
            count = fetched.as_ref().map_or(0, Vec::len),
            refused = ?fetched.as_ref().err(),
            "Received inbox fetch"
        );
        return match fetched {
            Ok(messages) => crate::event::InboxWaitReply::messages(messages),
            Err(code) => crate::event::InboxWaitReply::refused(
                code,
                "the deck could not issue to that receiver",
            ),
        };
    }
    // PRD #1666 G10: Pi's too — a plain doorbell fetch is an extension from
    // before the receipt stream.
    if old_adapter_doorbell(registry, req, agent_id) {
        registry.inbox().refuse_old_adapter(&req.pane_id, agent_id);
        return crate::event::InboxWaitReply::refused(
            "adapter_too_old",
            crate::inbox_display::ADAPTER_TOO_OLD,
        );
    }
    // PRD #1666 part 3 (H5): the legacy fetch is withdrawn. Whatever it
    // asked, `ack` included, nothing is taken or acknowledged and the mail
    // stays queued for the agent's own receiver; an agent's hook meets the
    // one-shot floor before it gets here.
    warn!(
        pane_id = %escape_id_for_log(&req.pane_id),
        consumer = ?req.consumer,
        ack = req.ack,
        "inbox: refused a fetch the way only an older dot-agent-deck asks; nothing was taken"
    );
    match req.consumer {
        InboxFetchConsumer::Cli => {
            crate::event::InboxWaitReply::refused("cli_too_old", CLI_FETCH_TOO_OLD)
        }
        InboxFetchConsumer::DoorbellHook | InboxFetchConsumer::TurnEndHook => {
            crate::event::InboxWaitReply::refused(
                "hooks_too_old",
                crate::inbox_display::HOOKS_TOO_OLD,
            )
        }
    }
}

/// PRD #1666: `inbox_list` — the caller's own inbox, or with `all` an
/// orchestrator's whole orchestration.
async fn handle_inbox_list(
    registry: &AgentPtyRegistry,
    state: &SharedState,
    req: &crate::event::InboxListRequest,
    agent_id: &str,
) -> Result<crate::event::InboxListReply, Box<crate::event::InboxWaitReply>> {
    let hub = registry.inbox();
    let panes: Vec<String> = if req.all {
        let state = state.read().await;
        if !state.orchestrator_pane_ids.contains(&req.pane_id) {
            return Err(Box::new(crate::event::InboxWaitReply::refused(
                "not_orchestrator",
                "only an orchestrator pane may list its orchestration's inboxes",
            )));
        }
        let mut panes = vec![req.pane_id.clone()];
        if let Some(identity) = state.pane_orchestration_map.get(&req.pane_id) {
            let mut members: Vec<String> = state
                .pane_orchestration_map
                .iter()
                .filter(|(pane, other)| *pane != &req.pane_id && other.id == identity.id)
                .map(|(pane, _)| pane.clone())
                .collect();
            members.sort();
            panes.extend(members);
        }
        panes
    } else {
        vec![req.pane_id.clone()]
    };
    // PRD #1666 S5: the caller's own pane lists the caller's own inbox — what
    // its agent generation may take — not a predecessor's history there.
    let messages = panes
        .iter()
        .flat_map(|pane| {
            let own = *pane == req.pane_id;
            hub.list(pane).into_iter().filter(move |message| {
                !own || message
                    .bind_agent
                    .as_deref()
                    .is_none_or(|bound| bound == agent_id)
            })
        })
        .collect();
    Ok(crate::event::InboxListReply {
        messages,
        armed: hub.armed(&req.pane_id, agent_id),
        wake: hub.announced_wake(&req.pane_id, agent_id),
        component: hub.summary(&req.pane_id, agent_id).component,
    })
}

/// [`HOOK_CONNECTION_IDLE_TIMEOUT`], or its override in milliseconds from
/// [`HOOK_IDLE_TIMEOUT_ENV`]. The override is a TEST knob: it lets the e2e
/// tests prove that an inbox waiter outlives the idle window without spending
/// the production minute. A zero or unparsable value is ignored with a warning.
fn hook_idle_timeout_from_env() -> Duration {
    match std::env::var(HOOK_IDLE_TIMEOUT_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => {
                warn!(
                    variable = HOOK_IDLE_TIMEOUT_ENV,
                    "ignoring a hook idle-timeout override that is not a positive number of \
                     milliseconds"
                );
                HOOK_CONNECTION_IDLE_TIMEOUT
            }
        },
        Err(_) => HOOK_CONNECTION_IDLE_TIMEOUT,
    }
}

/// Test-only override of [`HOOK_CONNECTION_IDLE_TIMEOUT`], in milliseconds.
const HOOK_IDLE_TIMEOUT_ENV: &str = "DOT_AGENT_DECK_HOOK_IDLE_TIMEOUT_MS";

async fn run_hook_loop(
    listener: IpcListener,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    pty_registry: Arc<AgentPtyRegistry>,
    shutdown: Arc<Notify>,
    worktree_registry: crate::issue_dispatch_run::WorktreeRegistry,
) -> Result<(), DaemonError> {
    run_hook_loop_with_idle_timeout(
        listener,
        state,
        event_tx,
        pty_registry,
        shutdown,
        worktree_registry,
        hook_idle_timeout_from_env(),
    )
    .await
}

/// [`run_hook_loop`] with the idle bound supplied rather than read from
/// [`HOOK_CONNECTION_IDLE_TIMEOUT`].
///
/// The seam exists so `hooks/ingest/003` can assert that a stalled connection's
/// slot is actually reclaimed without spending the production minute on it.
/// A parameter rather than an environment knob deliberately: a knob would be
/// reachable in production too, and the value is not something an operator has
/// any reason to tune (contrast `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS`, which is
/// a documented production setting).
#[allow(clippy::too_many_arguments)]
async fn run_hook_loop_with_idle_timeout(
    listener: IpcListener,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    pty_registry: Arc<AgentPtyRegistry>,
    shutdown: Arc<Notify>,
    worktree_registry: crate::issue_dispatch_run::WorktreeRegistry,
    idle_timeout: Duration,
) -> Result<(), DaemonError> {
    run_hook_loop_with_limits(
        listener,
        state,
        event_tx,
        pty_registry,
        shutdown,
        worktree_registry,
        HookLoopLimits::production(idle_timeout),
    )
    .await
}

/// Issue #1693: write one reply `line` to a hook connection and flush it,
/// giving up after `deadline` — the connection's idle timeout.
///
/// The read on a hook connection is bounded by that timeout and the reply
/// writes used to have no deadline, so a peer that sent lines without reading
/// its replies filled the socket's send buffer and parked the connection's
/// task in `write_all` for good, holding its hook-pool slot. With the write
/// bounded too, a peer that does not read loses its slot the way a peer that
/// does not write already does. On an error the caller drops the connection.
async fn write_hook_reply<W>(
    writer: &mut W,
    line: &str,
    deadline: Duration,
) -> Result<(), ReplyWriteError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let write = async {
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await
    };
    match tokio::time::timeout(deadline, write).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            // A client that hung up before its reply is ordinary.
            debug!(error = %e, "hook socket: reply write failed; dropping connection");
            Err(ReplyWriteError::Failed)
        }
        Err(_elapsed) => {
            warn!(
                idle_timeout_ms = deadline.as_millis(),
                "hook socket: reclaiming a connection that did not read its replies within \
                 the idle window — the peer kept the connection open while the daemon's \
                 replies filled its socket buffer, so it is stalled or misbehaving"
            );
            Err(ReplyWriteError::TimedOut)
        }
    }
}

/// Why [`write_hook_reply`] gave up on a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyWriteError {
    /// The write or flush failed, usually because the peer hung up.
    Failed,
    /// The write did not finish within the idle timeout: the peer is not
    /// reading.
    TimedOut,
}

/// [`run_hook_loop`] with every bound supplied — the seam the PRD #1631 hold
/// tests use to reach the connection cap, the hold budget and the hold
/// deadline without spending their production values.
#[allow(clippy::too_many_arguments)]
async fn run_hook_loop_with_limits(
    listener: IpcListener,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    pty_registry: Arc<AgentPtyRegistry>,
    shutdown: Arc<Notify>,
    worktree_registry: crate::issue_dispatch_run::WorktreeRegistry,
    limits: HookLoopLimits,
) -> Result<(), DaemonError> {
    let idle_timeout = limits.idle_timeout;
    // Issue #319: bound how many hook connections are being served at once.
    // Every accepted connection used to get its own `tokio::spawn` with nothing
    // capping how many could be outstanding, so a producer that opened
    // connections faster than they finished grew the daemon's task set and its
    // per-connection buffers without limit.
    let conn_limit = Arc::new(tokio::sync::Semaphore::new(limits.max_connections));
    // PRD #1631 (audit A6): held questions wait under a budget of their own,
    // not in the connection pool above.
    let hold_limit = Arc::new(tokio::sync::Semaphore::new(limits.max_holds));
    // Issue #1664: the order connections are accepted in, so one session's
    // events apply in the order they were sent.
    let event_order = crate::hook_event_order::HookEventOrder::new(limits.event_order_wait);
    // Whether the cap is currently holding, so the warning below fires on the
    // transition into saturation rather than once per waiting connection.
    let mut at_cap = false;
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
                pty_registry
                    .question_holds()
                    .release_all(crate::question::ReleaseReason::Shutdown);
                return Ok(());
            }
            accept_res = accept_hook_connection(&listener, &conn_limit, limits.max_connections, &mut at_cap) => match accept_res {
            Ok((permit, stream)) => {
                // Issue #1664: taken here, in accept order, before the task
                // that may run ahead of an earlier connection's is spawned.
                let mut event_order_ticket = Some(event_order.admit());
                let hold_limit = Arc::clone(&hold_limit);
                let state = state.clone();
                let event_tx = event_tx.clone();
                let pty_registry = pty_registry.clone();
                let worktree_registry = worktree_registry.clone();
                tokio::spawn(async move {
                    // Issue #319: the permit rides INTO the task and is dropped
                    // when it returns, so "connections being served" is exactly
                    // what the semaphore counts. Holding it in the loop above
                    // instead would bound accepts rather than tasks, which is
                    // not the thing that grows.
                    // PRD #1666: an `inbox_wait` connection gives its slot back
                    // (`take`) when it moves into the waiter pool; a held
                    // question gives its slot back while it waits (PRD #1631,
                    // audit A6).
                    let mut permit = Some(permit);
                    // PRD #201: split so the read-only `get-seed` verb can write
                    // a reply back on the same connection. The write half now
                    // serves every `DaemonMessage` arm — `delegate` since PR
                    // #466, `restart_role` / `spawn_role` / `list_targets`
                    // since #868 and PRD #220, and `work_done` / `dispatch`
                    // since issue #1129, whose line comes from the provenance
                    // gate above rather than from an arm. Raw `AgentEvent`
                    // traffic is still answered by nothing.
                    // PRD #1666 hardening M2: the process on the other end,
                    // as the kernel reports it, for the inbox verbs that bind
                    // a component to the pane's agent.
                    //
                    // PR #1735 third review (F2): recorded with when that
                    // process started, so every later check names the process
                    // that connected, not whatever reuses its pid. Part 3
                    // (R-1): and whether it runs as the deck's user, which
                    // with the pane's capability makes it a member of the pane.
                    let peer_pid = AcceptedPeer::accept(
                        crate::platform::peercred::peer_pid(&stream).ok(),
                        crate::platform::peercred::peer_is_same_user(&stream),
                    );
                    // PRD #1666 part 3 (Q35-14b): the connection's descriptor,
                    // for an editor helper's `SO_PEERPIDFD` and for what is
                    // pending on it at its read deadline. Used only by
                    // synchronous calls in this task, while the halves below,
                    // which own it, are alive; never handed to another task
                    // (Q84: what crosses to a blocking task is an `OwnedFd`).
                    #[cfg(target_os = "linux")]
                    let composer_socket = std::os::fd::AsRawFd::as_raw_fd(&stream);
                    #[cfg(target_os = "linux")]
                    let mut composer_conn: Option<crate::composer_probe::observe::Connection> =
                        None;
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut reader = tokio::io::BufReader::new(read_half);
                    let mut line_buf = crate::bounded_read::CappedLine::default();

                    // Issue #903 (duplicate #319): this used to be
                    // `reader.lines()` driven by `next_line()`, which grows its
                    // buffer until a newline arrives or the peer goes away — so
                    // a same-uid producer could make the daemon allocate an
                    // arbitrarily large `String` (and then `serde_json` allocate
                    // the parsed fields on top of it) BEFORE any admission
                    // control decided whether the event was even for a pane this
                    // daemon owns. `read_capped_line` resolves the same three
                    // outcomes under a ceiling; see `MAX_HOOK_LINE_BYTES` for
                    // where the number comes from.
                    loop {
                        // The `timeout` is what keeps a stalled connection from
                        // holding its slot forever — see
                        // `HOOK_CONNECTION_IDLE_TIMEOUT`. It wraps the READ and
                        // nothing else, so a connection is never reclaimed while
                        // the daemon is the one working: `dispatch`'s worktree
                        // creation and `delegate`'s `clear = true` respawn both
                        // run in the arms below, outside this call.
                        // PRD #1666 part 3 (Q84, Q85): a registered composer
                        // connection whose helper's exit the observer saw
                        // reads until its read deadline, one instant for the
                        // whole connection, and ends there; what it hadn't
                        // read by then makes its record's drain uncertain.
                        // `line_buf` keeps a partial line across the race.
                        #[cfg(target_os = "linux")]
                        let read = crate::composer_probe::observe::read_line(
                            composer_conn.as_ref(),
                            &mut line_buf,
                            &mut reader,
                            // SAFETY: the connection's own descriptor, owned
                            // by the halves alive in this task for the whole
                            // read.
                            unsafe { std::os::fd::BorrowedFd::borrow_raw(composer_socket) },
                            crate::bounded_read::MAX_HOOK_LINE_BYTES,
                        );
                        #[cfg(not(target_os = "linux"))]
                        let read = async {
                            Some(
                                line_buf
                                    .read(&mut reader, crate::bounded_read::MAX_HOOK_LINE_BYTES)
                                    .await,
                            )
                        };
                        let line = match tokio::time::timeout(idle_timeout, read)
                            .await
                            .map(|read| read.ok_or(()))
                        {
                            // The read deadline: the connection ends.
                            Ok(Err(())) => break,
                            Ok(Ok(read)) => Ok(read),
                            Err(elapsed) => Err(elapsed),
                        };
                        // A composer connection ending mid-line or on a line
                        // it refused leaves its record's evidence unread.
                        #[cfg(target_os = "linux")]
                        if let Some(conn) = composer_conn.as_ref()
                            && (line_buf.partial() || matches!(line, Ok(Err(_))))
                        {
                            conn.lose();
                        }
                        let line = match line
                        {
                            Err(_elapsed) => {
                                warn!(
                                    idle_timeout_ms = idle_timeout.as_millis(),
                                    "hook socket: reclaiming a connection that sent no \
                                     complete message within the idle window — every \
                                     shipped producer writes one line and closes, so this \
                                     is a leaked or stalled peer"
                                );
                                break;
                            }
                            Ok(Ok(Some(line))) => line,
                            // Peer closed — the ordinary end of every
                            // fire-and-forget send.
                            Ok(Ok(None)) => break,
                            Ok(Err(crate::bounded_read::CappedLineError::TooLong {
                                limit,
                                line_bytes,
                            })) => {
                                // Refused, not truncated, and never silently: a
                                // prefix of a JSON object can parse, so applying
                                // one would mean acting on a half-populated
                                // event. The connection goes because the peer is
                                // mid-message and there is no resynchronisation
                                // point — the next byte it sends is still part of
                                // a message we have already declined.
                                //
                                // The producer is deliberately NOT named. #903's
                                // suggested shape was to name "the peer's pane
                                // id", but the pane id lives in the payload that
                                // was just refused; the only identity available
                                // here is the peer's OS credentials, and binding
                                // hook-event provenance is #318's surface, not
                                // this one. Byte counts, never bytes: the
                                // content is attacker-controlled and a log is
                                // the wrong place to reproduce it.
                                warn!(
                                    limit_bytes = limit,
                                    line_bytes,
                                    "hook socket: refused an over-long line and dropped the \
                                     connection — a producer sent this many bytes with no \
                                     newline, so the message was declined whole rather than \
                                     truncated into a partially-populated event"
                                );
                                break;
                            }
                            Ok(Err(crate::bounded_read::CappedLineError::Io(e))) => {
                                // Includes non-UTF-8, which `next_line()` also
                                // reported as `InvalidData` and which the old
                                // `while let Ok(Some(..))` ended the loop on
                                // just as silently. Kept at debug: a client
                                // vanishing mid-write is ordinary.
                                debug!(
                                    error = %e,
                                    "hook socket: read failed; dropping connection"
                                );
                                break;
                            }
                        };
                        // Issue #1664: a raw event waits for the events of its
                        // session or pane accepted ahead of it, and holds its
                        // turn until this line is handled; an `inbox_announce`
                        // keeps the place for the event after it, and anything
                        // else gives it up. See `crate::hook_event_order`.
                        let _event_turn = crate::hook_event_order::turn_for_line(
                            &mut event_order_ticket,
                            &line,
                            // Issue #2043: so a line the provenance gate below
                            // refuses, or admits unattested, holds nobody's
                            // attested event back.
                            &*pty_registry,
                        )
                        .await;
                        if let Ok(msg) = serde_json::from_str::<DaemonMessage>(&line) {
                            // Issue #1077: ONE provenance gate for every verb on
                            // this socket, ahead of the match rather than inside
                            // each arm.
                            //
                            // Here rather than in the handlers, deliberately.
                            // `handle_delegate` / `handle_work_done` and the
                            // role verbs are also called directly — by the TUI,
                            // and by the test harnesses that drive the routing
                            // decision without a socket — and provenance is a
                            // property of *how a message arrived*, not of the
                            // routing it asks for. Gating at the boundary is
                            // also what makes "every `DaemonMessage` is
                            // attested" a statement about one function instead
                            // of seven.
                            //
                            // Read `crate::hook_provenance` before changing any
                            // of this, including the part that says plainly what
                            // it does not defend against.
                            let provenance = crate::hook_provenance::classify(
                                msg.claimed_pane(),
                                msg.presented_token(),
                                &*pty_registry,
                            );
                            // PR #1735 second review: the room a dispatch's
                            // result will need, held from the moment the
                            // dispatch is accepted (below) until the result is
                            // posted in the `Dispatch` arm.
                            let mut dispatch_result_room = None;
                            // PRD #1666 part 3, round 5 (plan §3.2): and the
                            // room the result's file and fallback body need,
                            // reserved before anything starts.
                            let mut dispatch_result_store = None;
                            match crate::hook_provenance::admits(
                                &provenance,
                                crate::hook_provenance::policy(),
                            ) {
                                Err(refusal) => {
                                    // The claimed pane id is logged and the
                                    // token never is — logging a capability
                                    // would put it in `deck.log`, which is
                                    // world-of-the-same-uid readable and is
                                    // exactly the surface this check exists to
                                    // take the token off.
                                    //
                                    // Issue #1082: and the pane id it DOES log
                                    // goes through `escape_id_for_log`, as every
                                    // producer-supplied id on this socket now
                                    // does. This is the first line the surface
                                    // writes about a message and the one most
                                    // likely to be read under suspicion, so it is
                                    // the worst place to let a raw LF forge a
                                    // following line, a CR overwrite this one, or
                                    // a bidi override reorder it. Turning the
                                    // subscriber's own ANSI styling off escapes
                                    // nothing INSIDE a field value and is not a
                                    // substitute — see
                                    // `crate::config_validation::escape_field_for_log`.
                                    warn!(
                                        verb = msg.verb(),
                                        claimed_pane = %escape_id_for_log(msg.claimed_pane()),
                                        reason = refusal.code(),
                                        "hook socket: refused a message whose hook \
                                         capability token does not attest the pane it \
                                         names; see docs/develop/hook-provenance.md"
                                    );
                                    if let Some(json) = msg.provenance_refusal_reply(
                                        refusal.code(),
                                        &refusal.caller_message(),
                                    ) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    continue;
                                }
                                Ok(()) => {
                                    // Issue #1129: `dispatch` gets its
                                    // acknowledgement HERE, ahead of the
                                    // handler, so the caller learns whether the
                                    // gate admitted it without waiting for work
                                    // it is not waiting on (`work_done` answers
                                    // in its own arm since PRD #1631). `dispatch`'s
                                    // handler is awaited inline below and spends
                                    // a whole worktree-create-and-spawn; an ack
                                    // written after it would park the calling
                                    // agent for the duration. Every other verb
                                    // returns `None` here and answers in its own
                                    // arm, which is what keeps "exactly one line
                                    // per message" true.
                                    // PR #1735 second review: a dispatch is
                                    // accepted only with room held for its
                                    // result, so a deck that fills up while it
                                    // runs cannot lose the answer; with no
                                    // room now the caller is told so and
                                    // nothing is dispatched.
                                    if matches!(msg, DaemonMessage::Dispatch(_)) {
                                        match pty_registry.inbox().reserve_reply() {
                                            Ok(room) => dispatch_result_room = Some(room),
                                            Err(error) => {
                                                warn!(
                                                    claimed_pane = %escape_id_for_log(msg.claimed_pane()),
                                                    %error,
                                                    "hook socket: refused a dispatch; the deck \
                                                     has no room for its result"
                                                );
                                                let refusal = crate::event::SignalAck::refused(
                                                    "deck_full",
                                                    &format!(
                                                        "{error}, so it could not take the result \
                                                         of this dispatch; nothing was dispatched"
                                                    ),
                                                );
                                                if let Ok(json) = serde_json::to_string(&refusal) {
                                                    let line = format!("{json}\n");
                                                    if write_hook_reply(
                                                        &mut write_half,
                                                        &line,
                                                        idle_timeout,
                                                    )
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                continue;
                                            }
                                        }
                                        match crate::dispatch_return::admit_dispatch_result(
                                            &pty_registry,
                                        ) {
                                            Ok(room) => dispatch_result_store = room,
                                            Err((code, message)) => {
                                                warn!(
                                                    claimed_pane = %escape_id_for_log(msg.claimed_pane()),
                                                    code,
                                                    "hook socket: refused a dispatch; the deck \
                                                     cannot keep its result"
                                                );
                                                let refusal =
                                                    crate::event::SignalAck::refused(code, &message);
                                                if let Ok(json) = serde_json::to_string(&refusal) {
                                                    let line = format!("{json}\n");
                                                    if write_hook_reply(
                                                        &mut write_half,
                                                        &line,
                                                        idle_timeout,
                                                    )
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                continue;
                                            }
                                        }
                                    }
                                    if let Some(json) = msg.provenance_ack_reply() {
                                        let line = format!("{json}\n");
                                        // Issue #1693: this ack precedes the
                                        // work, and a CLI older than #1129
                                        // sends `dispatch` and hangs up without
                                        // reading. Its write FAILS, and its
                                        // dispatch must still run, so only a
                                        // timeout — a peer holding the
                                        // connection open without reading —
                                        // drops the connection here.
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            == Err(ReplyWriteError::TimedOut)
                                        {
                                            break;
                                        }
                                    }
                                    if matches!(
                                        provenance,
                                        crate::hook_provenance::Provenance::Refused(
                                            crate::hook_provenance::Refusal::Missing
                                        )
                                    ) {
                                        // Only reachable under
                                        // `DOT_AGENT_DECK_HOOK_PROVENANCE=warn`.
                                        // Warned per message rather than once at
                                        // startup: the operator needs to know
                                        // WHICH pane is sending unattested, since
                                        // the remedy is to update the
                                        // `dot-agent-deck` binary that pane
                                        // invokes.
                                        warn!(
                                            verb = msg.verb(),
                                            claimed_pane = %escape_id_for_log(msg.claimed_pane()),
                                            "hook socket: acting on a message with no hook \
                                             capability token because \
                                             DOT_AGENT_DECK_HOOK_PROVENANCE=warn; this pane's \
                                             dot-agent-deck binary is older than the daemon \
                                             that spawned it"
                                        );
                                    }
                                }
                            }
                            // PRD #1666: the inbox verbs act only for an agent
                            // the token ATTESTS. A pane never issued a token is
                            // admitted above for its status events, but has no
                            // agent this daemon can bind a message to.
                            let attested_agent = match &provenance {
                                crate::hook_provenance::Provenance::Attested { agent_id, .. } => {
                                    Some(agent_id.clone())
                                }
                                _ => None,
                            };
                            if msg.requires_attested_agent() && attested_agent.is_none() {
                                warn!(
                                    verb = msg.verb(),
                                    claimed_pane = %escape_id_for_log(msg.claimed_pane()),
                                    "hook socket: refused an inbox request that no hook \
                                     capability token attests"
                                );
                                if let Some(json) = msg.provenance_refusal_reply(
                                    INBOX_UNATTESTED,
                                    INBOX_UNATTESTED_MESSAGE,
                                ) {
                                    let line = format!("{json}\n");
                                    if write_hook_reply(&mut write_half, &line, idle_timeout)
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                continue;
                            }
                            if let (Some(claimed), Some(attested)) =
                                (msg.claimed_agent(), attested_agent.as_deref())
                                && claimed != attested
                            {
                                warn!(
                                    verb = msg.verb(),
                                    claimed_pane = %escape_id_for_log(msg.claimed_pane()),
                                    "hook socket: refused an inbox request naming an agent \
                                     other than the one its token attests"
                                );
                                if let Some(json) = msg.provenance_refusal_reply(
                                    INBOX_WRONG_AGENT,
                                    INBOX_WRONG_AGENT_MESSAGE,
                                ) {
                                    let line = format!("{json}\n");
                                    if write_hook_reply(&mut write_half, &line, idle_timeout)
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                continue;
                            }
                            // And whether it comes from an outside agent: the
                            // rest of the admission verdict, which a question's
                            // status keeps exactly as a raw event's does.
                            let unproven = matches!(
                                provenance,
                                crate::hook_provenance::Provenance::Unattested
                            );
                            // PRD #1666 part 2: the one-shot wait and fetch on
                            // receipts have verbs of their own, which an older
                            // daemon cannot decode; past the gate they share
                            // the legacy verbs' arms.
                            let receipt_verb = matches!(
                                msg,
                                DaemonMessage::InboxReceiptWait(_)
                                    | DaemonMessage::InboxReceiptFetch(_)
                            );
                            match msg {
                                DaemonMessage::InboxReceipts(_) => {
                                    if attested_agent.is_none() {
                                        continue;
                                    }
                                    let reply = crate::event::InboxWaitReply {
                                        one_shot_receipts: Some(
                                            crate::inbox_hub::ONE_SHOT_RECEIPTS,
                                        ),
                                        one_shot_senders: crate::inbox_hub::ONE_SHOT_SENDERS
                                            .to_vec(),
                                        ..crate::event::InboxWaitReply::default()
                                    };
                                    if let Ok(json) = serde_json::to_string(&reply) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::InboxWait(req)
                                | DaemonMessage::InboxReceiptWait(req) => {
                                    let Some(agent_id) = attested_agent else {
                                        continue;
                                    };
                                    // Only Claude's once-waiter waits on
                                    // receipts; anything else asking to is
                                    // handed nothing.
                                    if receipt_verb
                                        && !(req.mode == crate::event::InboxWaitMode::Once
                                            && req.component
                                                == crate::event::InboxComponent::ClaudeWaiter)
                                    {
                                        let reply = crate::event::InboxWaitReply::refused(
                                            INBOX_RECEIPT_UNSUPPORTED,
                                            INBOX_RECEIPT_UNSUPPORTED_MESSAGE,
                                        );
                                        if let Ok(json) = serde_json::to_string(&reply) {
                                            let line = format!("{json}\n");
                                            if write_hook_reply(
                                                &mut write_half,
                                                &line,
                                                idle_timeout,
                                            )
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        continue;
                                    }
                                    let hub = Arc::clone(pty_registry.inbox());
                                    if pty_registry.pane_current_agent_id(&req.pane_id).as_deref()
                                        != Some(agent_id.as_str())
                                    {
                                        let reply = crate::event::InboxWaitReply::not_in_deck();
                                        if let Ok(json) = serde_json::to_string(&reply) {
                                            let line = format!("{json}\n");
                                            if write_hook_reply(
                                                &mut write_half,
                                                &line,
                                                idle_timeout,
                                            )
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        continue;
                                    }
                                    let admitted = admit_inbox_component(
                                        peer_pid,
                                        req.pid,
                                        pty_registry.child_pid(&agent_id),
                                        ComponentUse::Receiver,
                                        // A waiter is handed the agent's mail.
                                        Evidence::Required,
                                    );
                                    note_admission(
                                        &pty_registry,
                                        &req.pane_id,
                                        &agent_id,
                                        admitted.map(|_| ()),
                                    );
                                    let instance = match admitted {
                                        Ok(instance) => instance,
                                        Err(refusal) => {
                                            warn!(
                                                pane_id = %escape_id_for_log(&req.pane_id),
                                                component = ?req.component,
                                                reason = refusal.code(),
                                                "hook socket: refused an inbox wait the deck \
                                                 cannot admit for the pane"
                                            );
                                            let reply = crate::event::InboxWaitReply::refused(
                                                refusal.code(),
                                                refusal.message(),
                                            );
                                            if let Ok(json) = serde_json::to_string(&reply) {
                                                let line = format!("{json}\n");
                                                if write_hook_reply(
                                                    &mut write_half,
                                                    &line,
                                                    idle_timeout,
                                                )
                                                    .await
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                            continue;
                                        }
                                    };
                                    info!(
                                        pane_id = %escape_id_for_log(&req.pane_id),
                                        mode = ?req.mode,
                                        component = ?req.component,
                                        source = ?req.session.as_ref().and_then(|s| s.source.as_deref()),
                                        pid = ?req.pid,
                                        "Received inbox wait"
                                    );
                                    // Out of the hook-ingest pool, and out of its
                                    // idle timeout: a waiter is idle by design, and
                                    // the waiter pool bounds it instead.
                                    drop(permit.take());
                                    let arm = crate::inbox_hub::ReceiptArm {
                                        adapter_integration: req.adapter_integration,
                                        doorbell_only: req.doorbell_only,
                                        resumed: req.resumed,
                                        hook_integration: req.hook_integration,
                                    };
                                    crate::inbox_hub::serve_waiter(
                                        hub,
                                        req.pane_id,
                                        agent_id,
                                        req.mode,
                                        req.component,
                                        instance,
                                        arm,
                                        reader,
                                        write_half,
                                        |agent: &str| pty_registry.nudge_repaint(agent),
                                    )
                                    .await;
                                    return;
                                }
                                DaemonMessage::InboxAnnounce(req) => {
                                    let Some(agent_id) = attested_agent else {
                                        continue;
                                    };
                                    // PR #1735 third review (F1): an announce
                                    // pins the receiver and opens the
                                    // native-wake grace, so it needs the same
                                    // positive evidence as arming a waiter: an
                                    // unproven one could pin a dead process and
                                    // lock the real integration out.
                                    let admitted = admit_inbox_component(
                                        peer_pid,
                                        req.pid,
                                        pty_registry.child_pid(&agent_id),
                                        ComponentUse::Receiver,
                                        Evidence::Required,
                                    );
                                    note_admission(
                                        &pty_registry,
                                        &req.pane_id,
                                        &agent_id,
                                        admitted.map(|_| ()),
                                    );
                                    let instance = match admitted {
                                        Ok(instance) => instance,
                                        Err(refusal) => {
                                        warn!(
                                            pane_id = %escape_id_for_log(&req.pane_id),
                                            component = ?req.component,
                                            reason = refusal.code(),
                                            "hook socket: refused an inbox announce the deck \
                                             cannot admit for the pane"
                                        );
                                        if let Ok(json) =
                                            serde_json::to_string(&crate::event::SignalAck::refused(
                                                refusal.code(),
                                                refusal.message(),
                                            ))
                                        {
                                            let line = format!("{json}\n");
                                            if write_hook_reply(
                                                &mut write_half,
                                                &line,
                                                idle_timeout,
                                            )
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        continue;
                                    }
                                    };
                                    let announced = pty_registry.inbox().announce(
                                        &req.pane_id,
                                        &agent_id,
                                        req.wake,
                                        req.component,
                                        instance,
                                    );
                                    info!(
                                        pane_id = %escape_id_for_log(&req.pane_id),
                                        component = ?req.component,
                                        wake = ?req.wake,
                                        pid = ?req.pid,
                                        refused = ?announced.err(),
                                        "Received inbox announce"
                                    );
                                    let ack = match announced {
                                        Ok(()) => crate::event::SignalAck::accepted(),
                                        Err(error) => crate::event::SignalAck::refused(
                                            error.code(),
                                            &error.message(),
                                        ),
                                    };
                                    if let Ok(json) = serde_json::to_string(&ack) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::InboxFetch(req)
                                | DaemonMessage::InboxReceiptFetch(req) => {
                                    let Some(agent_id) = attested_agent else {
                                        continue;
                                    };
                                    // PRD #1666 part 3, round 5-wake (plan
                                    // §1.4): a prompt hook's ring evidence,
                                    // handed on whether or not the fetch
                                    // grants anything.
                                    if let Some(token) = req.doorbell_ring.as_deref() {
                                        pty_registry.note_doorbell_evidence(
                                            crate::agent_pty::RingEvidence {
                                                pane_id: req.pane_id.clone(),
                                                agent_id: agent_id.clone(),
                                                message_id: req.doorbell.clone(),
                                                token: token.to_string(),
                                                shape: req
                                                    .doorbell_prompt
                                                    .unwrap_or(crate::event::PromptShape::Unknown),
                                            },
                                        );
                                    }
                                    // Part 2 §2.4, §2.7: a hook's fetch is a
                                    // one-shot receiver of its own, held on
                                    // this connection for its controls; below
                                    // the integration floor nothing is issued
                                    // ([`crate::inbox_hub::serve_hook`]). On the
                                    // receipt verb it says so itself; a legacy
                                    // fetch is routed. PRD #1666 part 3 (H4):
                                    // Claude Code's and Codex's hooks and a
                                    // model-run `inbox fetch` are served on
                                    // receipts; a fetch a stream bound is not.
                                    let mut component = crate::event::InboxComponent::ClaudeHooks;
                                    let one_shot = if receipt_verb {
                                        let served = receipt_fetch_component(
                                            &req,
                                            pty_registry
                                                .spawn_agent_type(&agent_id)
                                                .or_else(|| {
                                                    pty_registry.reported_agent_type(&agent_id)
                                                }),
                                        );
                                        let Some(served) = served else {
                                            let reply = crate::event::InboxWaitReply::refused(
                                                INBOX_RECEIPT_UNSUPPORTED,
                                                INBOX_RECEIPT_UNSUPPORTED_MESSAGE,
                                            );
                                            if let Ok(json) = serde_json::to_string(&reply) {
                                                let line = format!("{json}\n");
                                                if write_hook_reply(
                                                    &mut write_half,
                                                    &line,
                                                    idle_timeout,
                                                )
                                                    .await
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                            continue;
                                        };
                                        component = served;
                                        true
                                    } else if legacy_verb_hook_fetch(&pty_registry, &req, &agent_id) {
                                        // PRD #1666 part 3 (H5): no legacy
                                        // service is left, so a hook's fetch
                                        // on the legacy verb meets the
                                        // one-shot floor at once, and the
                                        // card names that agent's hooks.
                                        component =
                                            legacy_hook_component(&pty_registry, &req, &agent_id);
                                        true
                                    } else {
                                        false
                                    };
                                    if one_shot {
                                        if let Err(refusal) = mail_taker_admitted(
                                            &pty_registry,
                                            peer_pid,
                                            &agent_id,
                                            "inbox_fetch",
                                            &req.pane_id,
                                        ) {
                                            let reply = crate::event::InboxWaitReply::refused(
                                                refusal.code(),
                                                refusal.message(),
                                            );
                                            if let Ok(json) = serde_json::to_string(&reply) {
                                                let line = format!("{json}\n");
                                                if write_hook_reply(
                                                    &mut write_half,
                                                    &line,
                                                    idle_timeout,
                                                )
                                                    .await
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                            continue;
                                        }
                                        let hub = Arc::clone(pty_registry.inbox());
                                        info!(
                                            pane_id = %escape_id_for_log(&req.pane_id),
                                            consumer = ?req.consumer,
                                            "Received a hook receiver's inbox fetch"
                                        );
                                        drop(permit.take());
                                        crate::inbox_hub::serve_hook(
                                            hub,
                                            req.pane_id.clone(),
                                            agent_id,
                                            component,
                                            crate::inbox_hub::HookFetch {
                                                named: req
                                                    .doorbell
                                                    .clone()
                                                    .filter(|id| crate::inbox::is_valid_message_id(id)),
                                                budget: fetch_budget(&req),
                                                hook_integration: req.hook_integration,
                                                consumer: req.consumer,
                                            },
                                            reader,
                                            write_half,
                                        )
                                        .await;
                                        return;
                                    }
                                    let reply = match mail_taker_admitted(
                                        &pty_registry,
                                        peer_pid,
                                        &agent_id,
                                        "inbox_fetch",
                                        &req.pane_id,
                                    ) {
                                        Ok(()) => handle_inbox_fetch(&pty_registry, &req, &agent_id),
                                        Err(refusal) => crate::event::InboxWaitReply::refused(
                                            refusal.code(),
                                            refusal.message(),
                                        ),
                                    };
                                    if let Ok(json) = serde_json::to_string(&reply) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::InboxCommitted(req) => {
                                    let Some(agent_id) = attested_agent else {
                                        continue;
                                    };
                                    // Part 2 §1.3: admitted from a member of the
                                    // pane (part 3, R-1: its current capability
                                    // and the deck's user), as `ack` is, for the
                                    // generation the token attests. That makes
                                    // it a same-user authority, not
                                    // proof that Claude's hook sent it; what it
                                    // may commit is bounded by the attempt's own
                                    // witness instead (R2-A01): released by the
                                    // deck, not positively not handed, and its
                                    // commit receipt on its own cap in the spool.
                                    // A generation whose receipts the deck
                                    // cannot read (no spool, or no scanner) is
                                    // the exception: it is degraded already, and
                                    // there the first two alone decide.
                                    let admitted = mail_taker_admitted(
                                        &pty_registry,
                                        peer_pid,
                                        &agent_id,
                                        "inbox_committed",
                                        &req.pane_id,
                                    );
                                    let reply = if let Err(refusal) = admitted {
                                        crate::event::InboxWaitReply::refused(
                                            refusal.code(),
                                            refusal.message(),
                                        )
                                    } else {
                                        let applied = pty_registry.inbox().one_shot_committed(
                                            &req.pane_id,
                                            &agent_id,
                                            &req.attempts,
                                            req.degraded.as_deref(),
                                        );
                                        info!(
                                            pane_id = %escape_id_for_log(&req.pane_id),
                                            attempts = req.attempts.len(),
                                            applied,
                                            degraded = req.degraded.is_some(),
                                            "Received a prompt's exact commits"
                                        );
                                        crate::event::InboxWaitReply::default()
                                    };
                                    if let Ok(json) = serde_json::to_string(&reply) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::InboxList(req) => {
                                    let Some(agent_id) = attested_agent else {
                                        continue;
                                    };
                                    let json = match handle_inbox_list(
                                        &pty_registry,
                                        &state,
                                        &req,
                                        &agent_id,
                                    )
                                    .await
                                    {
                                        Ok(reply) => serde_json::to_string(&reply),
                                        Err(refusal) => serde_json::to_string(&refusal),
                                    };
                                    if let Ok(json) = json {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::InboxPublish(_)
                                | DaemonMessage::InboxOffer(_)
                                | DaemonMessage::InboxComplete(_)
                                | DaemonMessage::InboxRelease(_) => {
                                    let Some(agent_id) = attested_agent else {
                                        continue;
                                    };
                                    // Hardening M2 for the verbs that reach
                                    // other agents or move a job between them,
                                    // as part 3 (R-1) widened it: only a member
                                    // of the pane may use its token.
                                    if let Err(refusal) = admit_inbox_component(
                                        peer_pid,
                                        None,
                                        pty_registry.child_pid(&agent_id),
                                        ComponentUse::Cli,
                                        Evidence::BestEffort,
                                    ) {
                                        warn!(
                                            verb = msg.verb(),
                                            pane_id = %escape_id_for_log(msg.claimed_pane()),
                                            reason = refusal.code(),
                                            "hook socket: refused an inbox post the deck cannot \
                                             admit for the pane"
                                        );
                                        if let Some(json) = msg.provenance_refusal_reply(
                                            refusal.code(),
                                            refusal.message(),
                                        ) {
                                            let line = format!("{json}\n");
                                            if write_hook_reply(
                                                &mut write_half,
                                                &line,
                                                idle_timeout,
                                            )
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        continue;
                                    }
                                    let json = match &msg {
                                        DaemonMessage::InboxPublish(req) => serde_json::to_string(
                                            &crate::inbox_pubsub::handle_publish(
                                                &pty_registry,
                                                &state,
                                                req,
                                                &agent_id,
                                            )
                                            .await,
                                        ),
                                        DaemonMessage::InboxOffer(req) => serde_json::to_string(
                                            &crate::inbox_pubsub::handle_offer(
                                                &pty_registry,
                                                &state,
                                                req,
                                                &agent_id,
                                            )
                                            .await,
                                        ),
                                        DaemonMessage::InboxComplete(req) => serde_json::to_string(
                                            &crate::inbox_pubsub::handle_claim(
                                                &pty_registry,
                                                req,
                                                &agent_id,
                                                true,
                                            ),
                                        ),
                                        DaemonMessage::InboxRelease(req) => serde_json::to_string(
                                            &crate::inbox_pubsub::handle_claim(
                                                &pty_registry,
                                                req,
                                                &agent_id,
                                                false,
                                            ),
                                        ),
                                        _ => continue,
                                    };
                                    if let Ok(json) = json {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::Delegate(signal) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&signal.pane_id),
                                        targets = ?signal.to,
                                        "Received delegate signal"
                                    );
                                    // PRD #93 round-5: one path for both
                                    // modes. The daemon owns the role map
                                    // and the inboxes, so it posts the task
                                    // to the worker's inbox itself — no
                                    // broadcast hop, no detach-window loss
                                    // surface (PRD #1666).
                                    //
                                    // PRD #92 F9 followup-6: pass the
                                    // daemon-wide event sender too, for the
                                    // events a per-target dispatch task
                                    // sends about a `clear = true`
                                    // replacement (a replaced worker's lifted
                                    // quota block, an eviction notice).
                                    // Issue #606: `_with_state` so a `clear = true`
                                    // respawn that had to RE-CREATE the worker pane
                                    // (its record removed by a concurrent close) can
                                    // put the role registration back. The read guard
                                    // below is released before the detached dispatch
                                    // task ever takes the write lock.
                                    // Issue #580 review (Qodo, #1285): the
                                    // sender the gate attested, so the busy
                                    // check cannot re-resolve a predecessor's
                                    // delegate onto a successor that took the
                                    // pane in between.
                                    let sender_agent_id = match &provenance {
                                        crate::hook_provenance::Provenance::Attested {
                                            agent_id,
                                            ..
                                        } => Some(agent_id.clone()),
                                        _ => None,
                                    };
                                    // Issue #1387: the free function, which
                                    // reads the project config with no guard
                                    // held, so a read that hangs cannot stall
                                    // hook ingestion behind it.
                                    let resp = crate::state::handle_attested_delegate(
                                        signal,
                                        &state,
                                        &pty_registry,
                                        &event_tx,
                                        sender_agent_id.as_deref(),
                                    )
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
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::RestartRole(signal) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&signal.pane_id),
                                        role = %escape_id_for_log(&signal.role),
                                        force = signal.force,
                                        "Received restart-role signal"
                                    );
                                    // Issue #868: same reply-on-same-connection
                                    // pattern as `Delegate` above — the caller
                                    // is a REQUEST, not fire-and-forget, since
                                    // it is the only place that knows whether
                                    // the restart actually happened.
                                    //
                                    // Fix-round M1/F2: `handle_restart_role_with_state`
                                    // is a free function taking `&state` directly
                                    // (like `handle_spawn_role_with_state` below), not
                                    // an `AppState` method called through a pre-held
                                    // read guard — that guard used to stay locked for
                                    // the whole respawn. See the function's own doc.
                                    let resp = crate::state::handle_restart_role_with_state(
                                        signal,
                                        &state,
                                        &pty_registry,
                                        &event_tx,
                                    )
                                    .await;
                                    if let Ok(json) = serde_json::to_string(&resp) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::SpawnRole(signal) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&signal.pane_id),
                                        role = %escape_id_for_log(&signal.role),
                                        "Received spawn-role signal"
                                    );
                                    // Issue #868: same reply-on-same-connection
                                    // pattern as `RestartRole` above. Unlike
                                    // that handler, `handle_spawn_role_with_state`
                                    // is a free function that takes the
                                    // `SharedState` handle directly — it
                                    // manages its own short-lived read/write
                                    // lock acquisitions internally, so `&state`
                                    // is passed here rather than an
                                    // already-held read guard.
                                    let resp = crate::state::handle_spawn_role_with_state(
                                        signal,
                                        &state,
                                        &pty_registry,
                                        &event_tx,
                                    )
                                    .await;
                                    if let Ok(json) = serde_json::to_string(&resp) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::Dispatch(signal) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&signal.pane_id),
                                        // The name arrives raw off the hook socket
                                        // — nothing between the producer and this
                                        // line rejects a control or bidi character
                                        // (`sanitize_name` runs later, and only on
                                        // the copy that becomes a path). PR #1081
                                        // review, Greptile finding 3: this is the
                                        // SOURCE of the two fields that finding
                                        // named, so it is escaped here too.
                                        name = %crate::config_validation::escape_field_for_log(
                                            &signal.name,
                                            crate::config_validation::MAX_QUOTED_VALUE_CHARS,
                                        ),
                                        "Received dispatch signal"
                                    );
                                    use crate::dispatch::{self, DispatchContext};

                                    use std::path::PathBuf;

                                    // Phase 1: resolve the caller's (agent id, cwd)
                                    // from ONE `AgentRecord` in the PTY registry, not
                                    // from AppState::pane_cwd_map. pane_cwd_map is only
                                    // populated for orchestration panes; dashboard panes
                                    // (including a dispatcher pane) never get an entry
                                    // there, which would make every dispatch from a
                                    // dispatcher pane a silent no-op.
                                    //
                                    // Issue #617 (finding 3): the agent id is captured
                                    // HERE, from the same record as the cwd, and carried
                                    // through the slow phase below so the result can be
                                    // delivered to the agent that ASKED rather than to
                                    // whoever holds its pane id when the work finishes.
                                    // Reading both from one record is what makes them a
                                    // consistent pair; two lookups could straddle a
                                    // hand-over and pair one agent's cwd with another's
                                    // identity.
                                    //
                                    // PRD #1589 D2: and for an attested sender the
                                    // pair is the attested generation's own, while
                                    // it still holds the pane — see
                                    // `resolve_dispatch_caller`.
                                    let caller = resolve_dispatch_caller(
                                        &pty_registry,
                                        &signal.pane_id,
                                        &provenance,
                                    );
                                    let (caller_agent_id, cwd) = match caller {
                                        Some(c) => c,
                                        None => {
                                            warn!(
                                                pane_id = %escape_id_for_log(&signal.pane_id),
                                                "dispatch from unknown pane"
                                            );
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
                                    // default inside `handle_dispatch`. Issue #1602:
                                    // a fallback only — `handle_dispatch` first
                                    // reuses the caller pane's own configured
                                    // command (`resolve_single_unit_launch`).
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
                                        // PRD #220 M2.0: the caller identity captured
                                        // above, carried into the dispatch so the unit
                                        // it starts can report back when it finishes.
                                        // The SAME pair the acknowledgement below is
                                        // bound to — retained rather than used once and
                                        // discarded, which is the whole of the return
                                        // edge's addressing.
                                        caller: Some(
                                            crate::dispatch_return::DispatchCaller {
                                                pane_id: signal.pane_id.clone(),
                                                agent_id: caller_agent_id.clone(),
                                                unit_name: signal.name.clone(),
                                            },
                                        ),
                                    };
                                    let task = signal.task.as_deref().unwrap_or_default();
                                    let result = dispatch::handle_dispatch(
                                        &ctx,
                                        &signal.name,
                                        task,
                                        signal.shape.as_ref(),
                                    )
                                    .await;

                                    // PRD #1666 S5: the acknowledgement goes to the
                                    // caller's inbox, bound to the caller generation
                                    // captured before the dispatch ran.
                                    let Some(room) = dispatch_result_room.take() else {
                                        continue;
                                    };
                                    // PRD #1666 part 3, round 5 (plan §3.2):
                                    // the result is saved, or held in memory,
                                    // before it is posted.
                                    let record = crate::dispatch_return::retain_dispatch_result(
                                        &pty_registry,
                                        dispatch_result_store.take(),
                                        &signal.pane_id,
                                        &caller_agent_id,
                                        &signal.name,
                                        &result.message,
                                        result.success,
                                        result.unit_id.clone(),
                                    )
                                    .await;
                                    crate::dispatch_return::post_dispatch_result(
                                        &pty_registry,
                                        room,
                                        &signal.pane_id,
                                        &caller_agent_id,
                                        &signal.name,
                                        result.success,
                                        &result.message,
                                        record,
                                    );
                                }
                                DaemonMessage::WorkDone(signal) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&signal.pane_id),
                                        done = signal.done,
                                        "Received work-done signal"
                                    );
                                    // PRD #1631: the `work-done` CLI is acked
                                    // HERE, from the result of the report's
                                    // whole handling, not at the provenance
                                    // gate. PRD #1666 part 3, round 5: the
                                    // legacy verb runs the task-scoped
                                    // transaction, `--done` as a terminal
                                    // report and anything else as a note
                                    // (plan D1), and an unconfirmed outcome
                                    // closes the connection with no line.
                                    let (claimed_pane, terminal) =
                                        (signal.pane_id.clone(), signal.done);
                                    let completion = crate::task_completion::complete_legacy(
                                        &state,
                                        &pty_registry,
                                        signal,
                                        &provenance,
                                    )
                                    .await;
                                    // Said once per report, because an older
                                    // CLI's `work-done` used to complete the
                                    // worker's task and now does not. The
                                    // cross-version harness reads this line
                                    // (`xtask/cross-version/src/breaks.rs`).
                                    if !terminal
                                        && matches!(
                                            &completion,
                                            crate::task_completion::Completion::Reply(reply)
                                                if reply.disposition
                                                    == crate::event::WorkDoneDisposition::Uncorrelated
                                        )
                                    {
                                        warn!(
                                            verb = "work_done",
                                            claimed_pane = %escape_id_for_log(&claimed_pane),
                                            reason = "no_task",
                                            "hook socket: delivered a work-done that names no task as a note that completes no task; a dot-agent-deck CLI older than this daemon sends that, and `work-done --task-id` completes a task"
                                        );
                                    }
                                    pty_registry
                                        .pause_point(crate::task_completion::PAUSE_COMMITTED)
                                        .await;
                                    let Some(ack) = completion.to_signal_ack() else {
                                        break;
                                    };
                                    if let Ok(json) = serde_json::to_string(&ack) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    pty_registry
                                        .pause_point(crate::task_completion::PAUSE_ACKED)
                                        .await;
                                }
                                DaemonMessage::HookCapabilities(_) => {
                                    // PRD #1666 part 3 (Q35-14b): the composer
                                    // capability, only to an installed
                                    // generation's helper copy, named by this
                                    // connection's `SO_PEERPIDFD` (Q81), and
                                    // only within the connection budgets.
                                    #[cfg(target_os = "linux")]
                                    let composer_probe = match attested_agent
                                        .as_deref()
                                        .and_then(|agent| pty_registry.composer_observer(agent))
                                    {
                                        Some(observer) => {
                                            // SAFETY: the connection's own
                                            // descriptor, owned by the halves
                                            // alive in this task, borrowed
                                            // only for this synchronous call.
                                            let socket = unsafe {
                                                std::os::fd::BorrowedFd::borrow_raw(composer_socket)
                                            };
                                            // The peer pidfd, an `OwnedFd`, is
                                            // what crosses to the blocking task.
                                            let peer =
                                                crate::composer_probe::attest::peer_pidfd_of(socket);
                                            composer_conn = tokio::task::spawn_blocking(move || {
                                                observer.connect(peer)
                                            })
                                            .await
                                            .ok()
                                            .flatten();
                                            composer_conn
                                                .as_ref()
                                                .map(|_| crate::event::COMPOSER_PROBE)
                                        }
                                        None => None,
                                    };
                                    #[cfg(not(target_os = "linux"))]
                                    let composer_probe = None;
                                    // PRD #1666 part 3, round 5 (plan D1): the
                                    // question, answered on this connection,
                                    // which then carries the mutation.
                                    let reply = crate::event::HookCapabilitiesReply {
                                        task_scoped_completion: Some(
                                            crate::event::TASK_SCOPED_COMPLETION,
                                        ),
                                        // PRD #1666 part 3, round 5-wake:
                                        // `input_mode` reports are stored.
                                        input_mode_report: Some(
                                            crate::input_mode::INPUT_MODE_REPORT,
                                        ),
                                        composer_probe,
                                        ..Default::default()
                                    };
                                    if let Ok(json) = serde_json::to_string(&reply) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::ComposerInvocation(request) => {
                                    // PRD #1666 part 3 (Q35-14b): served only
                                    // on a connection this daemon told the
                                    // capability, by the attested agent's
                                    // observer; anything else is unsupported,
                                    // and none of its request is read.
                                    #[cfg(target_os = "linux")]
                                    let served = composer_conn.take().filter(|conn| {
                                        attested_agent.as_deref().is_some_and(|agent| {
                                            pty_registry
                                                .composer_observer(agent)
                                                .is_some_and(|o| Arc::ptr_eq(&o, conn.observer()))
                                        })
                                    });
                                    #[cfg(target_os = "linux")]
                                    let Some(conn) = served
                                    else {
                                        let reply =
                                            crate::event::ComposerInvocationReply::unsupported();
                                        if let Ok(json) = serde_json::to_string(&reply) {
                                            let line = format!("{json}\n");
                                            if write_hook_reply(&mut write_half, &line, idle_timeout)
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        continue;
                                    };
                                    #[cfg(not(target_os = "linux"))]
                                    {
                                        let _ = request;
                                        let reply =
                                            crate::event::ComposerInvocationReply::unsupported();
                                        if let Ok(json) = serde_json::to_string(&reply) {
                                            let line = format!("{json}\n");
                                            if write_hook_reply(&mut write_half, &line, idle_timeout)
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        continue;
                                    }
                                    #[cfg(target_os = "linux")]
                                    {
                                        let editor_bearing = request.decision
                                            != crate::event::ComposerDecision::Armed;
                                        let (conn, outcome) = match tokio::task::spawn_blocking(
                                            move || {
                                                let outcome =
                                                    conn.observer().invocation(&conn, &request);
                                                (conn, outcome)
                                            },
                                        )
                                        .await
                                        {
                                            Ok(answered) => answered,
                                            Err(_) => break,
                                        };
                                        let reply = crate::event::ComposerInvocationReply {
                                            outcome: Some(outcome),
                                            ..Default::default()
                                        };
                                        if let Ok(json) = serde_json::to_string(&reply) {
                                            let line = format!("{json}\n");
                                            if write_hook_reply(&mut write_half, &line, idle_timeout)
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        // The helper's report comes after its
                                        // editor, which runs as long as the
                                        // user edits: this connection gives its
                                        // hook slot back and holds its composer
                                        // slot instead, and reads one bounded
                                        // line, or the end.
                                        // It reads until its helper's exit
                                        // plus the read deadline (Q84, Q85);
                                        // what it hadn't read by then, or a
                                        // report it refused or couldn't
                                        // parse, makes the drain uncertain.
                                        if editor_bearing {
                                            permit.take();
                                            let line = match crate::composer_probe::observe::read_line(
                                                Some(&conn),
                                                &mut line_buf,
                                                &mut reader,
                                                // SAFETY: the connection's own
                                                // descriptor, owned by the
                                                // halves alive in this task for
                                                // the whole read.
                                                unsafe {
                                                    std::os::fd::BorrowedFd::borrow_raw(
                                                        composer_socket,
                                                    )
                                                },
                                                crate::composer_probe::observe::REPORT_BYTES,
                                            )
                                            .await
                                            {
                                                Some(Ok(line)) => line,
                                                Some(Err(_)) => {
                                                    conn.lose();
                                                    None
                                                }
                                                None => None,
                                            };
                                            if let Some(line) = line {
                                                match serde_json::from_str::<
                                                    crate::event::EditorReport,
                                                >(line.trim())
                                                {
                                                    Ok(report) => {
                                                        let _ = tokio::task::spawn_blocking(
                                                            move || {
                                                                conn.observer().report(&conn, report);
                                                            },
                                                        )
                                                        .await;
                                                    }
                                                    Err(_) => conn.lose(),
                                                }
                                            }
                                        }
                                        break;
                                    }
                                }
                                DaemonMessage::InputMode(report) => {
                                    // PRD #1666 part 3, round 5-wake (plan
                                    // §1.3): the generation's input-mode
                                    // store, and nothing else.
                                    let verdict = match (attested_agent.as_deref(), peer_pid) {
                                        (Some(agent_id), Some(peer)) => match peer.started {
                                            Some(start) => pty_registry
                                                .accept_input_mode(
                                                    agent_id,
                                                    crate::input_mode::Observer {
                                                        pid: peer.pid,
                                                        start,
                                                    },
                                                    crate::input_mode::ModeSample {
                                                        seq: report.seq,
                                                        icanon: report.icanon,
                                                        echo: report.echo,
                                                        sampled_mono_ns: report.sampled_mono_ns,
                                                        sample_done_mono_ns: report
                                                            .sample_done_mono_ns,
                                                    },
                                                )
                                                .map(|_| ()),
                                            None => Err("peer_start_unreadable"),
                                        },
                                        (None, _) => Err("unattested"),
                                        (_, None) => Err("peer_unreadable"),
                                    };
                                    let ack = match verdict {
                                        Ok(()) => crate::event::SignalAck::accepted(),
                                        Err(code) => crate::event::SignalAck::refused(
                                            code,
                                            "the input-mode report was not stored",
                                        ),
                                    };
                                    if let Ok(json) = serde_json::to_string(&ack) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::WorkDoneTask(request) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&request.pane_id),
                                        mode = ?std::mem::discriminant(&request.mode),
                                        "Received a task-scoped work-done"
                                    );
                                    let completion = crate::task_completion::complete(
                                        &state,
                                        &pty_registry,
                                        crate::task_completion::CompletionRequest {
                                            pane_id: request.pane_id,
                                            mode: request.mode,
                                            report: request.report,
                                        },
                                        &provenance,
                                    )
                                    .await;
                                    pty_registry
                                        .pause_point(crate::task_completion::PAUSE_COMMITTED)
                                        .await;
                                    let crate::task_completion::Completion::Reply(reply) =
                                        completion
                                    else {
                                        // Unconfirmed: no line, and the
                                        // connection closes.
                                        break;
                                    };
                                    if let Ok(json) = serde_json::to_string(&reply) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    pty_registry
                                        .pause_point(crate::task_completion::PAUSE_ACKED)
                                        .await;
                                }
                                DaemonMessage::Ack(signal) => {
                                    // Issue #1383: the id is producer-supplied, so
                                    // it is validated before it reaches a lookup or
                                    // a log field. Answered HERE, after the lookup
                                    // (PR #1414 review), so the worker is told
                                    // whether its id matched rather than only that
                                    // the gate admitted it — this verb's one line.
                                    let attested_agent_id = match &provenance {
                                        crate::hook_provenance::Provenance::Attested {
                                            agent_id,
                                            ..
                                        } => Some(agent_id.as_str()),
                                        _ => None,
                                    };
                                    // PRD #1666: every id is an inbox message's,
                                    // acknowledged for the agent the token attests,
                                    // and only from a member of its pane (PR #1735
                                    // review; part 3, R-1).
                                    let ack = match attested_agent_id {
                                        Some(agent_id) => match mail_taker_admitted(
                                            &pty_registry,
                                            peer_pid,
                                            agent_id,
                                            "ack",
                                            &signal.pane_id,
                                        ) {
                                            Err(refusal) => crate::event::SignalAck::refused(
                                                refusal.code(),
                                                refusal.message(),
                                            ),
                                            Ok(()) => crate::event::SignalAck::acknowledged(
                                                handle_inbox_ack(&pty_registry, &signal, agent_id),
                                            ),
                                        },
                                        None => crate::event::SignalAck::acknowledged(
                                            crate::event::AckDelivery::Unknown,
                                        ),
                                    };
                                    if let Ok(json) = serde_json::to_string(&ack) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::GetSeed(req) => {
                                    // PRD #1666 part 3 (H5): `get-seed` takes
                                    // no message any more
                                    // ([`get_seed_withdrawn`]): every caller is
                                    // answered at once with no seed. Only a
                                    // member of the pane, for the agent its
                                    // token attests, marks a Pi generation's
                                    // card (G10, PR #1735 review).
                                    let attested = attested_agent.as_deref().filter(|agent| {
                                        mail_taker_admitted(
                                            &pty_registry,
                                            peer_pid,
                                            agent,
                                            "get_seed",
                                            &req.pane_id,
                                        )
                                        .is_ok()
                                    });
                                    let resp = get_seed_withdrawn(
                                        &pty_registry,
                                        &req.pane_id,
                                        attested,
                                        pty_registry
                                            .pane_current_agent_id(&req.pane_id)
                                            .as_deref(),
                                    );
                                    info!(
                                        pane_id = %escape_id_for_log(&req.pane_id),
                                        agent_id = ?req.agent_id,
                                        "Refused a get-seed request: get-seed takes no message any more"
                                    );
                                    if let Ok(json) = serde_json::to_string(&resp) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                DaemonMessage::Question(signal) => {
                                    // PRD #1631: one line back, always — the
                                    // answer, or a release — and for a held
                                    // question not until one of them happens.
                                    // A connection that closes first (the
                                    // agent killed its hook, the keyboard
                                    // answered) takes the question with it.
                                    //
                                    // Audit A6/N2: and then the connection
                                    // ENDS. A question connection carries one
                                    // request, as every producer sends it; a
                                    // held one has given its hook-pool slot
                                    // back and its hold budget goes with the
                                    // handler, so a socket kept open past the
                                    // reply would otherwise read and dispatch
                                    // more lines outside both bounds.
                                    let ack = signal.ack;
                                    let mut signal = *signal;
                                    keep_wrapper_report_from_own_child(
                                        &mut signal.event,
                                        peer_pid,
                                        attested_agent.as_deref(),
                                        &pty_registry,
                                    );
                                    let Some(held) = handle_question_signal(
                                        signal,
                                        attested_agent.clone(),
                                        unproven,
                                        &state,
                                        &event_tx,
                                        &pty_registry,
                                        QuestionConnection {
                                            reader: &mut reader,
                                            writer: &mut write_half,
                                            slot: &mut permit,
                                            holds: &hold_limit,
                                            hold_deadline: limits.hold_deadline,
                                            write_timeout: idle_timeout,
                                        },
                                    )
                                    .await
                                    else {
                                        break;
                                    };
                                    write_held_reply(
                                        &mut reader,
                                        &mut write_half,
                                        held,
                                        ack,
                                        limits.ack_wait,
                                        idle_timeout,
                                    )
                                    .await;
                                    break;
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
                                        pane_id = %escape_id_for_log(&req.pane_id),
                                        resolved_cwd = ?cwd,
                                        "Received list-targets request"
                                    );
                                    let resp = crate::dispatch::list_targets_response(
                                        cwd.as_deref().map(std::path::Path::new),
                                    );
                                    if let Ok(json) = serde_json::to_string(&resp) {
                                        let line = format!("{json}\n");
                                        if write_hook_reply(&mut write_half, &line, idle_timeout)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                        } else if let Ok(mut event) = serde_json::from_str::<AgentEvent>(&line) {
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
                                session_id = %escape_id_for_log(&event.session_id),
                                event_type = ?event.event_type,
                                pane_id = ?event.pane_id,
                                agent_type = ?event.agent_type,
                                tool_name = ?event.tool_name,
                                tool_detail = ?event.tool_detail,
                                "Received event"
                            );
                            // Issue #318: the provenance gate, for raw events.
                            // Every `DaemonMessage` has gone through #1077's since
                            // that issue; a status report naming a pane is
                            // checked against that pane's hook capability token
                            // the same way. Here, ahead of everything below,
                            // because a refused event must reach nothing: no
                            // registry side effect (the Codex rollout arm, the
                            // agent type), no `AppState`, and above all no
                            // broadcast — an attached TUI applies what it is
                            // sent with no registry of its own, so a refusal
                            // after the fan-out would refuse nothing there.
                            //
                            // The token is read off the same line and never
                            // becomes part of the event, so nothing this event
                            // reaches — the journal, the broadcast, a log — can
                            // carry it. A `token` that is not a string is
                            // treated as a malformed token rather than as none.
                            let presented = match serde_json::from_str::<
                                crate::event::PresentedToken,
                            >(&line)
                            {
                                Ok(presented) => presented.token,
                                Err(_) => Some(String::new()),
                            };
                            let provenance = crate::hook_provenance::classify_event(
                                event.pane_id.as_deref(),
                                event.agent_id.as_deref(),
                                presented.as_deref(),
                                &*pty_registry,
                            );
                            drop(presented);
                            match crate::hook_provenance::admits(
                                &provenance,
                                crate::hook_provenance::policy(),
                            ) {
                                Err(refusal) => {
                                    // Logged like the `DaemonMessage` refusal:
                                    // the escaped pane and a stable reason code,
                                    // never the token. No reply line — a raw
                                    // event is fire-and-forget, and a hook's
                                    // output is what its agent reads.
                                    warn!(
                                        verb = "agent_event",
                                        event_type = ?event.event_type,
                                        claimed_pane = %escape_id_for_log(
                                            event.pane_id.as_deref().unwrap_or("<none>")
                                        ),
                                        reason = refusal.code(),
                                        "hook socket: refused a status event whose hook \
                                         capability token does not attest the pane it \
                                         names; see docs/develop/hook-provenance.md"
                                    );
                                    continue;
                                }
                                Ok(()) => {
                                    if matches!(
                                        provenance,
                                        crate::hook_provenance::Provenance::Refused(
                                            crate::hook_provenance::Refusal::Missing
                                        )
                                    ) {
                                        // Only reachable under
                                        // `DOT_AGENT_DECK_HOOK_PROVENANCE=warn`,
                                        // and warned per event for the reason the
                                        // `DaemonMessage` gate gives.
                                        warn!(
                                            verb = "agent_event",
                                            event_type = ?event.event_type,
                                            claimed_pane = %escape_id_for_log(
                                                event.pane_id.as_deref().unwrap_or("<none>")
                                            ),
                                            "hook socket: acting on a status event with no \
                                             hook capability token because \
                                             DOT_AGENT_DECK_HOOK_PROVENANCE=warn; this pane's \
                                             dot-agent-deck binary is older than the daemon \
                                             that spawned it"
                                        );
                                    }
                                }
                            }
                            // An outside agent's event: admitted, but only to
                            // its own unproven card (issue #601).
                            let unproven = matches!(
                                provenance,
                                crate::hook_provenance::Provenance::Unattested
                            );
                            let attested_agent = match &provenance {
                                crate::hook_provenance::Provenance::Attested { agent_id, .. } => {
                                    Some(agent_id.clone())
                                }
                                _ => None,
                            };
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
                                    session_id = %escape_id_for_log(&event.session_id),
                                    pane_id = ?event.pane_id,
                                    raw_line = %hook_line_for_log(&line),
                                    "Event carries an unrecognized event_type — decoded as \
                                     Unknown and otherwise ignored; check the hook for a typo"
                                );
                            }
                            // PRD #1631 (audit A9): a bare frame is unattested,
                            // so it may report a status but neither raise a
                            // question nor clear one; producers send those as
                            // `question` messages through the provenance gate.
                            strip_question_metadata(&mut event);
                            // Issue #714: normalise the quota-block keys, and hand
                            // a Codex event's rollout and turn to the tailer.
                            admit_producer_event(&mut event);
                            keep_wrapper_report_from_own_child(
                                &mut event,
                                peer_pid,
                                attested_agent.as_deref(),
                                &pty_registry,
                            );
                            // PRD #1666 S6: a prompt that is the deck's own
                            // message is acknowledged and labelled here, for the
                            // agent the token attests, before anything reads it.
                            // After the admission above, which discards a
                            // producer's own marker.
                            crate::inbox_prompt::stamp_deck_message_prompt(
                                &mut event,
                                attested_agent.as_deref(),
                                pty_registry.inbox(),
                            );
                            // PRD #1401: a pull request report is the
                            // daemon's alone — its URL is what a card's badge
                            // opens — so a producer's copy goes no further.
                            if event.event_type == crate::event::EventType::PullRequest {
                                warn!(
                                    verb = "agent_event",
                                    claimed_pane = %escape_id_for_log(
                                        event.pane_id.as_deref().unwrap_or("<none>")
                                    ),
                                    reason = "daemon_authored_event_type",
                                    "hook socket: dropped a producer's pull_request event; \
                                     only the daemon reports an agent's pull request"
                                );
                                continue;
                            }
                            // An outside agent's rollout is not this daemon's to
                            // tail; the arm is for panes it spawned.
                            if !unproven {
                                queue_codex_rollout_arm(&pty_registry, &event, &line);
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
                                        pane_id = %escape_id_for_log(pane_id),
                                        session_id = %escape_id_for_log(&event.session_id),
                                        agent_type = ?event.agent_type,
                                        "SessionStart for a pane this daemon did not spawn — \
                                         a foreign agent is posting here (a test run inheriting \
                                         DOT_AGENT_DECK_SOCKET is the usual cause); it will \
                                         draw an outside agent's card with no local pane"
                                    );
                                }
                                // Never for an outside agent's event (audit
                                // finding 3): the pane may have been issued to
                                // a deck agent since the gate classified it,
                                // and this would write that agent's record.
                                //
                                // And only onto the record of the generation
                                // that SENT it (round-2 audit finding 4): the
                                // token's owner, else the agent the event names,
                                // and only while that generation still speaks
                                // for the pane, decided under the registry's
                                // own lock. A replaced generation's report is
                                // attested to its own token and may still reach
                                // its own card, but it must not type the
                                // successor's record that now holds the pane.
                                if !unproven {
                                    pty_registry.set_agent_type_for_generation(
                                        pane_id,
                                        attested_agent
                                            .as_deref()
                                            .or(event.agent_id.as_deref()),
                                        &event.agent_type,
                                    );
                                }
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
                            //
                            // PRD #1497: a turn-ending event's final reply goes
                            // to `subscribe-turn-replies` connections, published
                            // BEFORE the event is broadcast, so a client that
                            // has seen this turn end and subscribes afterwards
                            // is not handed its reply as if it were new.
                            if !unproven {
                                publish_hook_turn_reply(
                                    &pty_registry,
                                    &event,
                                    attested_agent.as_deref(),
                                    &line,
                                );
                            }
                            // Issue #1637: which deck binary sent this line,
                            // read off the line like the token, and never part
                            // of the event.
                            let stamped =
                                observe_hook_sender(&state, &event_tx, &event.agent_type, &line)
                                    .await;
                            // PRD #1666 part 3 (R-3): Claude Code's and
                            // Codex's hooks from before the deck's messages
                            // never ask for them, and their lines carry no
                            // stamp. Only a line attested to its generation
                            // says whose card.
                            if matches!(
                                event.agent_type,
                                crate::event::AgentType::ClaudeCode
                                    | crate::event::AgentType::Codex
                            ) && let (Some(pane_id), Some(agent_id)) =
                                (event.pane_id.as_deref(), attested_agent.as_deref())
                            {
                                pty_registry.inbox().observe_hook_stamp(
                                    pane_id,
                                    agent_id,
                                    &event.agent_type,
                                    stamped,
                                );
                            }
                            ingest_hook_event(
                                &state,
                                &event_tx,
                                &pty_registry,
                                event,
                                unproven,
                                attested_agent,
                            )
                            .await;
                        } else {
                            // PRD #1666 part 3 (Q87): on a registered composer
                            // connection a line it can't parse, half an
                            // invocation ended by the peer's close among them,
                            // is evidence unread, so its helper's exit drains
                            // only uncertain. Other connections are unchanged.
                            #[cfg(target_os = "linux")]
                            if let Some(conn) = composer_conn.as_ref() {
                                conn.lose();
                            }
                            // The line is producer-controlled, and issue #903
                            // is about not letting a producer make the daemon
                            // spend unbounded resources on a message it is
                            // going to reject. Logging it whole is the same
                            // defect one step later: at the new 8 MiB ceiling
                            // a malformed payload would write 8 MiB into the
                            // deck log, so the read cap alone would have moved
                            // the sink rather than closed it. A prefix plus the
                            // true length keeps the line diagnosable — a real
                            // hook payload is a JSON one-liner well under the
                            // prefix, so nothing legitimate is even elided.
                            warn!(
                                line_bytes = line.len(),
                                "Malformed event: {}",
                                hook_line_for_log(&line)
                            );
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

/// PRD #1589: the two hook-socket seams the `close` verb depends on — the
/// completion mark bound to the attested sender, and the `dispatch` caller
/// resolved as one attested pair.
#[cfg(all(test, unix))]
mod close_verb_seam_tests {
    use super::*;
    use crate::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
    use crate::hook_provenance::Provenance;

    fn spawn(registry: &Arc<AgentPtyRegistry>, pane: &str, cwd: &str, command: &str) -> String {
        registry
            .spawn_agent(SpawnOptions {
                command: Some(command),
                cwd: Some(cwd),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn")
    }

    fn wait_exited(registry: &AgentPtyRegistry, agent: &str) {
        for _ in 0..400 {
            if !registry.agent_is_live(agent) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the child never exited");
    }

    /// Scenario: a dispatcher A exits and B takes its pane in another
    /// directory. A `dispatch` attested to A is refused rather than paired
    /// with B's identity and cwd; one attested to B runs as B, in B's cwd; and
    /// the token-less compatibility path still resolves by pane.
    #[test]
    fn a_dispatch_runs_as_the_attested_generation_with_its_own_cwd() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let a = spawn(
            &registry,
            "disp",
            &a_dir.path().to_string_lossy(),
            "/usr/bin/true",
        );
        wait_exited(&registry, &a);
        let b = spawn(
            &registry,
            "disp",
            &b_dir.path().to_string_lossy(),
            "sleep 30",
        );
        let b_cwd = b_dir.path().to_string_lossy().into_owned();
        assert_eq!(
            resolve_dispatch_caller(
                &registry,
                "disp",
                &Provenance::Attested {
                    agent_id: a,
                    facts: Default::default()
                }
            ),
            None,
            "a superseded generation must not dispatch as its successor"
        );
        assert_eq!(
            resolve_dispatch_caller(
                &registry,
                "disp",
                &Provenance::Attested {
                    agent_id: b.clone(),
                    facts: Default::default(),
                }
            ),
            Some((b.clone(), b_cwd.clone()))
        );
        assert_eq!(
            resolve_dispatch_caller(&registry, "disp", &Provenance::Unattested),
            Some((b, b_cwd)),
            "the token-less compatibility path is unchanged"
        );
        registry.shutdown_all();
    }
}

/// Issue #714: producer admission of the quota-block contract. Platform
/// independent — it touches no socket and spawns nothing.
#[cfg(test)]
mod quota_admission_tests {
    use super::*;
    use crate::event::AgentType;
    use spec::spec;

    pub(super) fn quota_frame(event_type: crate::event::EventType) -> AgentEvent {
        AgentEvent {
            session_id: "s".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some("pane-1".to_string()),
            agent_id: Some("1".to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
            background: None,
            background_revision: None,
        }
    }

    /// Issue #1981 M8 (PR #2010 review): every event admitted from a producer
    /// carries the daemon's receipt stamp, set when it was admitted and over
    /// any copy the producer sent, so a producer can neither forge one nor
    /// skew the latency read from it. The event the daemon ingests after
    /// typing part of an answer is the daemon's own and carries none, so
    /// telemetry does not count it as an agent's hook.
    #[test]
    fn the_receipt_stamp_is_the_daemons_alone() {
        use crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY;
        let stamp = |event: &AgentEvent| {
            event
                .metadata
                .get(RECEIVED_BY_DECK_AT_METADATA_KEY)
                .map(|v| v.parse::<i64>().expect("a stamp is Unix nanoseconds"))
        };
        let question: crate::question::PendingQuestion =
            serde_json::from_value(serde_json::json!({
                "id": "q1",
                "kind": "permission",
                "questions": [{"prompt": "Allow?", "options": [
                    {"index": 1, "label": "Yes", "role": "allow_once"},
                    {"index": 2, "label": "No", "role": "deny"},
                ]}],
                "raised_at_ms": 1,
                "channel": "unsupported",
            }))
            .unwrap();
        let made = deck_keyboard_only_event(
            "s".to_string(),
            AgentType::ClaudeCode,
            Some("pane-1".to_string()),
            Some("1".to_string()),
            &question,
        );
        assert_eq!(stamp(&made), None, "{:?}", made.metadata);
        let before = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let mut plain = quota_frame(crate::event::EventType::Thinking);
        admit_producer_event(&mut plain);
        // A copy claiming the epoch would read as decades of latency, and one
        // far ahead as none.
        let mut forged = [0, i64::MAX].map(|claimed| {
            let mut forged = quota_frame(crate::event::EventType::Unknown);
            forged.metadata.insert(
                RECEIVED_BY_DECK_AT_METADATA_KEY.to_string(),
                claimed.to_string(),
            );
            admit_producer_event(&mut forged);
            forged
        });
        let after = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        for admitted in std::iter::once(&mut plain).chain(forged.iter_mut()) {
            let at = stamp(admitted).expect("an admitted event is stamped");
            assert!(
                (before..=after).contains(&at),
                "{at} not in {before}..={after}"
            );
        }
    }

    /// Scenario: Post a `quota_blocked` event on the hook socket as a producer
    /// would, with a forged daemon source marker, an unknown kind and an
    /// out-of-range reset, and a `thinking` event carrying quota-reason keys.
    /// The block is admitted with its kind normalised and the marker and bad
    /// reset stripped; the thinking frame loses every quota key and paints no
    /// reason.
    #[spec("status/blocked/006")]
    #[test]
    fn status_blocked_006_producer_quota_blocked_is_admitted_and_normalised() {
        use crate::quota_block::{
            QUOTA_BLOCKED_DETAIL_METADATA_KEY, QUOTA_BLOCKED_KIND_METADATA_KEY,
            QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY, QUOTA_BLOCKED_SOURCE_METADATA_KEY,
        };
        let now = chrono::Utc::now().timestamp_millis();
        let with = |event_type, pairs: &[(&str, String)]| {
            let mut event = quota_frame(event_type);
            for (k, v) in pairs {
                event.metadata.insert(k.to_string(), v.clone());
            }
            event
                .metadata
                .insert("other".to_string(), "kept".to_string());
            event
        };

        // A producer's block is admitted and normalised.
        let mut block = with(
            crate::event::EventType::QuotaBlocked,
            &[
                (QUOTA_BLOCKED_KIND_METADATA_KEY, "made_up_kind".to_string()),
                (
                    QUOTA_BLOCKED_SOURCE_METADATA_KEY,
                    "codex_rollout".to_string(),
                ),
                (
                    QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY,
                    (now + 3_600_000).to_string(),
                ),
                (
                    QUOTA_BLOCKED_DETAIL_METADATA_KEY,
                    "limit\u{202e} hit\n".to_string(),
                ),
            ],
        );
        let raw = serde_json::to_string(&block).unwrap();
        assert!(raw.contains("\"quota_blocked\""), "{raw}");
        admit_producer_event(&mut block);
        assert!(
            !block.is_daemon_synthetic(),
            "a producer cannot claim the daemon's source"
        );
        assert!(
            !block
                .metadata
                .contains_key(QUOTA_BLOCKED_SOURCE_METADATA_KEY)
        );
        assert_eq!(block.metadata[QUOTA_BLOCKED_KIND_METADATA_KEY], "unknown");
        assert_eq!(
            block.metadata[QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY],
            (now + 3_600_000).to_string()
        );
        assert_eq!(
            block.metadata[QUOTA_BLOCKED_DETAIL_METADATA_KEY],
            "limit hit"
        );
        assert_eq!(block.metadata["other"], "kept");

        // Resets outside the window, or not a number, are dropped — not the event.
        for bad in [
            (now - 2 * 3_600_000).to_string(),
            (now + 500 * 86_400_000).to_string(),
            "soon".to_string(),
        ] {
            let mut event = with(
                crate::event::EventType::QuotaBlocked,
                &[
                    (QUOTA_BLOCKED_KIND_METADATA_KEY, "usage_limit".to_string()),
                    (QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY, bad.clone()),
                ],
            );
            admit_producer_event(&mut event);
            assert_eq!(
                event.metadata[QUOTA_BLOCKED_KIND_METADATA_KEY],
                "usage_limit"
            );
            assert!(
                !event
                    .metadata
                    .contains_key(QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY),
                "{bad} kept"
            );
        }

        // Every other event type loses every quota key.
        let mut smuggled = with(
            crate::event::EventType::Thinking,
            &[
                (
                    QUOTA_BLOCKED_KIND_METADATA_KEY,
                    "credits_depleted".to_string(),
                ),
                (QUOTA_BLOCKED_DETAIL_METADATA_KEY, "forged".to_string()),
                (QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY, now.to_string()),
                (
                    QUOTA_BLOCKED_SOURCE_METADATA_KEY,
                    "codex_rollout".to_string(),
                ),
            ],
        );
        admit_producer_event(&mut smuggled);
        // Issue #1981 M8: admission adds the daemon's receipt stamp.
        assert!(
            smuggled
                .metadata
                .remove(crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY)
                .is_some()
        );
        assert_eq!(
            smuggled.metadata.keys().collect::<Vec<_>>(),
            vec!["other"],
            "{:?}",
            smuggled.metadata
        );

        // End to end through the state: the admitted block paints the card; the
        // stripped frame would not have.
        let mut state = crate::state::AppState::default();
        state.register_pane("pane-1".to_string());
        state.apply_event(smuggled);
        assert_eq!(
            state.sessions["s"].status,
            crate::state::SessionStatus::Thinking
        );
        assert!(state.sessions["s"].blocked.is_none());
        state.apply_event(block);
        assert_eq!(
            state.sessions["s"].status,
            crate::state::SessionStatus::Blocked
        );
        let reason = state.sessions["s"].blocked.clone().expect("a reason");
        assert_eq!(reason.kind, crate::state::BlockedKind::Unknown);
        assert_eq!(reason.resets_at_ms, Some(now + 3_600_000));

        // The Codex rollout keys survive only on the Codex events that carry them.
        let codex_keys = [
            (
                crate::codex_rollout_tail::CODEX_TRANSCRIPT_PATH_METADATA_KEY,
                "/x/rollout-a.jsonl".to_string(),
            ),
            (
                crate::codex_rollout_tail::CODEX_TURN_ID_METADATA_KEY,
                "t".to_string(),
            ),
        ];
        // Each count includes the daemon's receipt stamp (issue #1981 M8).
        let mut claude = with(crate::event::EventType::Thinking, &codex_keys);
        admit_producer_event(&mut claude);
        assert_eq!(claude.metadata.len(), 2, "{:?}", claude.metadata);
        let mut codex = with(crate::event::EventType::Thinking, &codex_keys);
        codex.agent_type = AgentType::Codex;
        admit_producer_event(&mut codex);
        assert_eq!(codex.metadata.len(), 4, "{:?}", codex.metadata);
        // Issue #1354: a thread-spawned Codex subagent's hooks carry the
        // parent's session and the subagent's own rollout and turn, so they
        // lose both keys rather than re-arm the parent's watch.
        let mut subagent = with(crate::event::EventType::Thinking, &codex_keys);
        subagent.agent_type = AgentType::Codex;
        subagent.metadata.insert(
            crate::event::SUBAGENT_ID_METADATA_KEY.to_string(),
            "a7c1".to_string(),
        );
        admit_producer_event(&mut subagent);
        assert_eq!(subagent.metadata.len(), 3, "{:?}", subagent.metadata);
        assert!(subagent.is_from_subagent());
    }

    /// PRD #1666 long-turn delivery: only the daemon says a message it reported
    /// not delivered reached the agent after all. A producer frame carrying the
    /// `delivery_notice_cleared` key loses it at the hook ingress, so it cannot
    /// lift the card's not-delivered Error. The frame is a Codex wrapper's
    /// output frame, which asserts no status of its own, so the card moves only
    /// if the key survives.
    #[test]
    fn a_producer_frame_cannot_clear_the_decks_delivery_notice() {
        use crate::event::{
            DELIVERY_NOTICE_CLEARED_METADATA_KEY, DELIVERY_NOTICE_CLEARED_METADATA_VALUE,
            DELIVERY_NOTICE_METADATA_KEY, EventType, WRAPPER_OUTPUT_CLASSIFIED_METADATA_KEY,
            WRAPPER_OUTPUT_CLASSIFIED_METADATA_VALUE,
        };
        use crate::state::{AppState, SessionStatus};
        let notice = crate::inbox_display::NOTICE_MAX_DELIVER;
        let codex = |event_type| {
            let mut event = quota_frame(event_type);
            event.agent_type = AgentType::Codex;
            event
        };
        let noticed = || {
            let mut state = AppState::default();
            state.register_pane("pane-1".to_string());
            state.apply_event(codex(EventType::SessionStart));
            let mut report = codex(EventType::Error);
            report.agent_type = AgentType::None;
            report.tool_detail = Some(notice.to_string());
            report
                .metadata
                .insert(DELIVERY_NOTICE_METADATA_KEY.to_string(), notice.to_string());
            state.apply_daemon_report_event(report);
            assert_eq!(state.sessions["s"].delivery_notice(), Some(notice));
            state
        };
        let forged = || {
            let mut event = codex(EventType::Thinking);
            event.metadata.insert(
                WRAPPER_OUTPUT_CLASSIFIED_METADATA_KEY.to_string(),
                WRAPPER_OUTPUT_CLASSIFIED_METADATA_VALUE.to_string(),
            );
            event.metadata.insert(
                DELIVERY_NOTICE_CLEARED_METADATA_KEY.to_string(),
                DELIVERY_NOTICE_CLEARED_METADATA_VALUE.to_string(),
            );
            event
        };

        // The control: unadmitted, the key would lift the card.
        let mut unadmitted = noticed();
        unadmitted.apply_event(forged());
        assert_eq!(unadmitted.sessions["s"].status, SessionStatus::Thinking);

        let mut admitted = forged();
        admit_producer_event(&mut admitted);
        assert!(
            !admitted
                .metadata
                .contains_key(DELIVERY_NOTICE_CLEARED_METADATA_KEY),
            "{:?}",
            admitted.metadata
        );
        assert!(!admitted.is_delivery_notice_cleared());
        assert!(!admitted.is_daemon_synthetic());
        let mut state = noticed();
        state.apply_event(admitted);
        let card = &state.sessions["s"];
        assert_eq!(card.status, SessionStatus::Error);
        assert_eq!(card.delivery_notice(), Some(notice));
    }

    /// Scenario (PRD #1666 §2.8): a producer frame forges the daemon's
    /// card-surfacing start for its pane, carrying an inbox with an
    /// integration warning and queued messages. Unadmitted, a client would
    /// take that inbox as the pane's and draw it on the card; after the hook
    /// ingress's admission the inbox is gone and the card shows none.
    #[test]
    fn a_producer_frame_cannot_carry_a_surfaced_inbox() {
        use crate::event::{
            CARD_SURFACE_SESSION_START_ORIGIN, SESSION_START_ORIGIN_METADATA_KEY,
            SURFACED_AGENT_ID_METADATA_KEY, SURFACED_INBOX_METADATA_KEY,
        };
        use crate::state::AppState;
        let forged = || {
            let mut event = quota_frame(crate::event::EventType::SessionStart);
            let inbox = crate::inbox_hub::InboxSummary {
                queued: 3,
                fetched: 0,
                claimed: 0,
                undeliverable: 0,
                last_acked_at: None,
                wake: crate::inbox_hub::WakePath::Doorbell,
                component: None,
                wake_stalled: false,
                wake_hold: None,
                hold_detail: None,
                notices: crate::inbox_hub::InboxCardNotices {
                    integration: Some(crate::inbox_display::IntegrationNotice::RefreshFailedPi),
                    ..Default::default()
                },
            };
            for (key, value) in [
                (
                    SESSION_START_ORIGIN_METADATA_KEY,
                    CARD_SURFACE_SESSION_START_ORIGIN.to_string(),
                ),
                (SURFACED_AGENT_ID_METADATA_KEY, "1".to_string()),
                (
                    SURFACED_INBOX_METADATA_KEY,
                    serde_json::to_string(&inbox).unwrap(),
                ),
            ] {
                event.metadata.insert(key.to_string(), value);
            }
            event
        };
        let shown = |event: AgentEvent| {
            let mut state = AppState::default();
            state.apply_event(event);
            state
                .sessions
                .values()
                .find_map(crate::state::SessionState::card_inbox)
        };

        // The control: unadmitted, the forged inbox reaches the card.
        assert!(shown(forged()).is_some());

        let mut admitted = forged();
        admit_producer_event(&mut admitted);
        assert!(
            !admitted.metadata.contains_key(SURFACED_INBOX_METADATA_KEY),
            "{:?}",
            admitted.metadata
        );
        assert_eq!(shown(admitted), None);
    }

    /// Scenario (PRD #1666 audit M-A1): a producer frame carries the deck's
    /// own `deck_message` marker beside a prompt it typed. Unadmitted, the
    /// marker would keep that prompt out of the card's prompt history; after
    /// the hook ingress's admission the marker is gone and the prompt is
    /// recorded like any other.
    #[test]
    fn a_producer_frame_cannot_label_its_prompt_as_a_deck_message() {
        use crate::inbox_prompt::{DECK_MESSAGE_METADATA_KEY, is_deck_message};
        use crate::state::AppState;
        let prompt = "please fix the build";
        let forged = || {
            let mut event = quota_frame(crate::event::EventType::Thinking);
            event.user_prompt = Some(prompt.to_string());
            event
                .metadata
                .insert(DECK_MESSAGE_METADATA_KEY.to_string(), "1".to_string());
            event
        };
        let recorded = |event: AgentEvent| {
            let mut state = AppState::default();
            state.register_pane("pane-1".to_string());
            state.apply_event(event);
            let card = &state.sessions["s"];
            (card.last_user_prompt.clone(), card.first_prompts.clone())
        };

        // The control: unadmitted, the marker hides the prompt.
        assert_eq!(recorded(forged()), (None, Vec::new()));

        let mut admitted = forged();
        admit_producer_event(&mut admitted);
        assert!(!is_deck_message(&admitted), "{:?}", admitted.metadata);
        assert_eq!(
            recorded(admitted),
            (Some(prompt.to_string()), vec![prompt.to_string()])
        );
    }

    /// Issue #1359: a failed Codex turn read from the rollout becomes one
    /// event for the card — `QuotaBlocked`, marked as the daemon's own, for a
    /// usage limit, and `Error`, with no quota metadata and no provider text,
    /// for any other failure — and each paints the card it names.
    #[test]
    fn codex_rollout_failures_report_blocked_or_error() {
        use crate::codex_rollout_tail::CodexTurnFailure;
        use crate::quota_block::QUOTA_BLOCKED_SOURCE_METADATA_KEY;
        use crate::quota_signals::FailureOutcome;
        let failure = |outcome| CodexTurnFailure {
            pane_id: "pane-1".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: "s".to_string(),
            turn_id: "t1".to_string(),
            outcome,
            message: Some("provider text".to_string()),
        };
        let mut state = crate::state::AppState::default();
        state.register_pane("pane-1".to_string());
        state.apply_event(AgentEvent {
            agent_type: AgentType::Codex,
            agent_id: Some("agent-1".to_string()),
            ..quota_frame(crate::event::EventType::Thinking)
        });
        assert_eq!(
            state.sessions["s"].status,
            crate::state::SessionStatus::Thinking
        );

        let error = codex_rollout_failure_event(failure(FailureOutcome::Error));
        assert_eq!(error.event_type, crate::event::EventType::Error);
        assert_eq!(error.agent_type, AgentType::Codex);
        assert_eq!(error.session_id, "s");
        assert_eq!(error.pane_id.as_deref(), Some("pane-1"));
        assert_eq!(error.agent_id.as_deref(), Some("agent-1"));
        assert!(error.metadata.is_empty(), "{:?}", error.metadata);
        assert!(error.tool_detail.is_none());
        state.apply_event(error);
        assert_eq!(
            state.sessions["s"].status,
            crate::state::SessionStatus::Error
        );

        let block = codex_rollout_failure_event(failure(FailureOutcome::Blocked {
            kind: crate::quota_block::BlockedKind::CreditsDepleted,
            resets_at_ms: None,
        }));
        assert_eq!(block.event_type, crate::event::EventType::QuotaBlocked);
        assert!(
            block
                .metadata
                .contains_key(QUOTA_BLOCKED_SOURCE_METADATA_KEY)
        );
        state.apply_event(block);
        assert_eq!(
            state.sessions["s"].status,
            crate::state::SessionStatus::Blocked
        );
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

    /// Scenario: PRD #1666 part 3 (Q30). A user presses Enter on a box the input model lost track of (Up, then Enter) in a pane whose agent is live, and the daemon ingests that agent's events: working reports with no prompt (thinking, a tool starting), a prompt report from the pane's previous generation, and a subagent's prompt report. None of them proves the box empty. The live agent's own prompt report (a thinking event carrying the prompt its hook took) does; so does the deck's own line it recognised, but only for the deck's own submit, never for the user's Enter.
    #[spec("inbox/doorbell/059")]
    #[tokio::test]
    async fn doorbell_059_the_daemon_confirms_an_unknown_enter_only_from_its_agents_prompt_report()
    {
        use crate::composer_model::{InputProof, Semantics};
        let pane = "prompt-report";
        let registry = Arc::new(AgentPtyRegistry::new());
        let owner = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    pane.to_string(),
                )]),
                agent_type: Some(AgentType::ClaudeCode),
                ..SpawnOptions::default()
            })
            .expect("spawn stand-in");
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(pane.to_string());
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let event = |event_type, agent: &str, prompt: Option<&str>| {
            let mut event = super::quota_admission_tests::quota_frame(event_type);
            event.pane_id = Some(pane.to_string());
            event.agent_id = Some(agent.to_string());
            event.user_prompt = prompt.map(str::to_string);
            event
        };
        use crate::event::EventType::{Idle, Thinking, ToolStart};
        registry
            .write_user_bytes(pane, &owner, b"\x1b[A\r")
            .await
            .unwrap();
        assert_eq!(
            registry.input_proof(pane, Some(Semantics::ClaudeCode)),
            InputProof::Unknown
        );
        for (what, frame) in [
            ("a thinking report", event(Thinking, &owner, None)),
            ("a tool starting", event(ToolStart, &owner, None)),
            ("the turn ending", event(Idle, &owner, None)),
            (
                "a predecessor's prompt",
                event(Thinking, "a-predecessor", Some("old prompt")),
            ),
            ("a subagent's prompt", {
                let mut frame = event(Thinking, &owner, Some("sub prompt"));
                frame.metadata.insert(
                    crate::event::SUBAGENT_ID_METADATA_KEY.to_string(),
                    "sub-1".to_string(),
                );
                frame
            }),
        ] {
            ingest_event(&state, &event_tx, &registry, frame).await;
            assert_eq!(
                registry.input_proof(pane, Some(Semantics::ClaudeCode)),
                InputProof::Unknown,
                "{what}"
            );
        }
        // The deck's own line, reported as the deck's: settles the deck's
        // submit, not the user's Enter.
        let mut deck = event(Thinking, &owner, Some("✉ task"));
        deck.metadata.insert(
            crate::inbox_prompt::DECK_MESSAGE_METADATA_KEY.to_string(),
            "1".to_string(),
        );
        ingest_event(&state, &event_tx, &registry, deck).await;
        assert_eq!(
            registry.input_proof(pane, Some(Semantics::ClaudeCode)),
            InputProof::Unknown,
            "the deck's"
        );
        ingest_event(
            &state,
            &event_tx,
            &registry,
            event(Thinking, &owner, Some("the recalled prompt")),
        )
        .await;
        assert_eq!(
            registry.input_proof(pane, Some(Semantics::ClaudeCode)),
            InputProof::Empty
        );
        registry.shutdown_all();
    }

    /// Scenario: PRD #1666 part 3 (Q32). Type three spaces, Enter, x and Ctrl+U into a Codex pane and into a Claude Code pane. Codex keeps the blanks after its cursor, so its box still holds three spaces; Claude Code's Ctrl+U takes the whole line. Each generation's doorbell reads its own agent's editing rules, so the Codex pane's hold says there may be a draft and the Claude Code pane's does not, and a pane read with no agent's rules holds a draft too.
    #[spec("inbox/doorbell/059")]
    #[tokio::test]
    async fn doorbell_059_the_hold_reads_the_box_as_the_panes_agent_edits_it() {
        use crate::composer_model::Semantics;
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut panes = Vec::new();
        for (pane, agent_type) in [
            ("hold-codex", AgentType::Codex),
            ("hold-claude", AgentType::ClaudeCode),
        ] {
            let owner = registry
                .spawn_agent(SpawnOptions {
                    command: Some("/bin/cat"),
                    env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                        DOT_AGENT_DECK_PANE_ID.to_string(),
                        pane.to_string(),
                    )]),
                    agent_type: Some(agent_type),
                    ..SpawnOptions::default()
                })
                .expect("spawn stand-in");
            registry
                .write_user_bytes(pane, &owner, b"   \rx\x15")
                .await
                .unwrap();
            panes.push((pane, owner));
        }
        let hold = |(pane, owner): &(&str, String)| {
            registry.pane_input_hold(pane, registry.doorbell_input_semantics(pane, owner))
        };
        assert_eq!(
            registry.doorbell_input_semantics(panes[0].0, &panes[0].1),
            Some(Semantics::Codex)
        );
        assert!(hold(&panes[0]).draft, "Codex keeps the blanks");
        assert_eq!(
            registry.doorbell_input_semantics(panes[1].0, &panes[1].1),
            Some(Semantics::ClaudeCode)
        );
        assert!(!hold(&panes[1]).draft, "Claude Code's box is empty");
        assert!(registry.pane_input_hold(panes[1].0, None).draft);
        registry.shutdown_all();
    }

    /// Issue #714: the Codex rollout tailer is armed only for an event whose
    /// pane and agent name the pane's live owner, and only by a genuine Codex
    /// hook — not by the wrapper's output classifier.
    #[test]
    fn codex_rollout_arm_needs_the_live_owner() {
        use crate::codex_rollout_tail::{
            ArmCommand, CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY,
        };
        let registry = Arc::new(AgentPtyRegistry::new());
        let owner = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "codex-arm".to_string(),
                )]),
                agent_type: Some(AgentType::Codex),
                ..SpawnOptions::default()
            })
            .expect("spawn stand-in");
        let prompt = |agent: &str| {
            let mut event =
                super::quota_admission_tests::quota_frame(crate::event::EventType::Thinking);
            event.agent_type = AgentType::Codex;
            event.pane_id = Some("codex-arm".to_string());
            event.agent_id = Some(agent.to_string());
            event.metadata.insert(
                CODEX_TRANSCRIPT_PATH_METADATA_KEY.to_string(),
                "/x/rollout-a.jsonl".to_string(),
            );
            event
                .metadata
                .insert(CODEX_TURN_ID_METADATA_KEY.to_string(), "t1".to_string());
            event
        };
        queue_codex_rollout_arm(&registry, &prompt("someone-else"), "");
        let mut classified = prompt(&owner);
        classified.metadata.insert(
            crate::event::WRAPPER_OUTPUT_CLASSIFIED_METADATA_KEY.to_string(),
            crate::event::WRAPPER_OUTPUT_CLASSIFIED_METADATA_VALUE.to_string(),
        );
        queue_codex_rollout_arm(&registry, &classified, "");
        assert!(registry.codex_rollout_arms().drain().is_empty());

        queue_codex_rollout_arm(&registry, &prompt(&owner), "");
        let mut stop = prompt(&owner);
        stop.event_type = crate::event::EventType::Idle;
        queue_codex_rollout_arm(
            &registry,
            &stop,
            r#"{"turn_reply":{"text":"All tests pass."}}"#,
        );
        let queued = registry.codex_rollout_arms().drain();
        assert!(
            matches!(&queued[..], [ArmCommand::Arm(req), ArmCommand::Disarm { turn_id, .. }]
                if req.turn_id.as_deref() == Some("t1") && turn_id == "t1"),
            "{queued:?}"
        );
        registry.shutdown_all();
    }

    /// Scenario (PRD #1497 re-audit R3): a Codex turn `t1` is armed, and its
    /// `Stop` arrives naming `t1` but without a reply (no
    /// `last_assistant_message`, or a hook CLI older than PRD #1497), BEFORE
    /// Codex has written the turn's `task_complete`. The `Stop` queues no
    /// disarm, so the next poll still reads the rollout, hands the turn's
    /// reply once and disarms it. A `Stop` that did carry the reply disarms the turn as
    /// before, and its later `task_complete` hands nothing — the hook's frame
    /// was the turn's one.
    #[test]
    fn codex_stop_without_a_reply_keeps_the_rollout_read_for_that_turn() {
        use crate::codex_rollout_tail::{
            CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY, CodexRolloutTailers,
        };
        use std::io::Write as _;
        let registry = Arc::new(AgentPtyRegistry::new());
        let owner = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "codex-r3".to_string(),
                )]),
                agent_type: Some(AgentType::Codex),
                ..SpawnOptions::default()
            })
            .expect("spawn stand-in");
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T05-00-00-r3.jsonl");
        let append = |text: &str| {
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&rollout)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        };
        append("{\"type\":\"session_meta\"}\n");
        let event = |event_type, turn: &str| {
            let mut event = super::quota_admission_tests::quota_frame(event_type);
            event.agent_type = AgentType::Codex;
            event.pane_id = Some("codex-r3".to_string());
            event.agent_id = Some(owner.clone());
            event.metadata.insert(
                CODEX_TRANSCRIPT_PATH_METADATA_KEY.to_string(),
                rollout.to_string_lossy().into_owned(),
            );
            event
                .metadata
                .insert(CODEX_TURN_ID_METADATA_KEY.to_string(), turn.to_string());
            event
        };
        let complete = |turn: &str, text: &str| {
            format!(
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{turn}\",\"last_agent_message\":\"{text}\"}}}}\n"
            )
        };
        let live = |pane: &str, agent: &str| registry.is_live_owner(pane, agent);
        let mut tailers = CodexRolloutTailers::default();
        let poll = |tailers: &mut CodexRolloutTailers| {
            for command in registry.codex_rollout_arms().drain() {
                tailers.apply(command);
            }
            tailers.tick(live);
            tailers.take_replies()
        };

        // The Stop carries no reply: the turn stays armed through the next
        // poll, which reads the task_complete Codex writes after it.
        queue_codex_rollout_arm(
            &registry,
            &event(crate::event::EventType::Thinking, "t1"),
            "",
        );
        queue_codex_rollout_arm(
            &registry,
            &event(crate::event::EventType::Idle, "t1"),
            r#"{"event_type":"idle"}"#,
        );
        assert!(poll(&mut tailers).is_empty(), "nothing written yet");
        append(&complete("t1", "All 42 tests pass."));
        let replies = poll(&mut tailers);
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0].reply.turn_id.as_deref(), Some("t1"));
        assert_eq!(replies[0].reply.text, "All 42 tests pass.");
        assert!(
            !tailers.is_armed(&owner),
            "the turn's task_complete disarms it"
        );
        assert!(poll(&mut tailers).is_empty(), "handed once");

        // The Stop carries the reply: the turn is disarmed, and its
        // task_complete hands nothing more.
        queue_codex_rollout_arm(
            &registry,
            &event(crate::event::EventType::Thinking, "t2"),
            "",
        );
        queue_codex_rollout_arm(
            &registry,
            &event(crate::event::EventType::Idle, "t2"),
            r#"{"event_type":"idle","turn_reply":{"text":"Done."}}"#,
        );
        assert!(poll(&mut tailers).is_empty());
        append(&complete("t2", "Done."));
        assert!(poll(&mut tailers).is_empty(), "a disarmed turn is not read");
        assert!(!tailers.is_armed(&owner));
        registry.shutdown_all();
    }

    /// Scenario (PRD #1497 re-audit R3): a Codex turn `t1` is armed and its
    /// `Stop` arrives without a reply, but Codex never writes the turn's
    /// `task_complete`. The `Stop` queues the bounded drain; once
    /// `DRAIN_AFTER_STOP` has passed the next poll retires the watch, closes
    /// the rollout and hands nothing, and a `task_complete` written later is
    /// not read.
    #[test]
    fn codex_stop_without_a_reply_retires_the_watch_after_the_drain() {
        use crate::codex_rollout_tail::{
            ArmCommand, CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY,
            CodexRolloutTailers, DRAIN_AFTER_STOP,
        };
        use std::io::Write as _;
        let registry = Arc::new(AgentPtyRegistry::new());
        let owner = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "codex-drain".to_string(),
                )]),
                agent_type: Some(AgentType::Codex),
                ..SpawnOptions::default()
            })
            .expect("spawn stand-in");
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T11-00-00-dr.jsonl");
        let append = |text: &str| {
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&rollout)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        };
        append("{\"type\":\"session_meta\"}\n");
        let event = |event_type| {
            let mut event = super::quota_admission_tests::quota_frame(event_type);
            event.agent_type = AgentType::Codex;
            event.pane_id = Some("codex-drain".to_string());
            event.agent_id = Some(owner.clone());
            event.metadata.insert(
                CODEX_TRANSCRIPT_PATH_METADATA_KEY.to_string(),
                rollout.to_string_lossy().into_owned(),
            );
            event
                .metadata
                .insert(CODEX_TURN_ID_METADATA_KEY.to_string(), "t1".to_string());
            event
        };
        let live = |pane: &str, agent: &str| registry.is_live_owner(pane, agent);
        let mut tailers = CodexRolloutTailers::default();

        queue_codex_rollout_arm(&registry, &event(crate::event::EventType::Thinking), "");
        queue_codex_rollout_arm(
            &registry,
            &event(crate::event::EventType::Idle),
            r#"{"event_type":"idle"}"#,
        );
        let queued = registry.codex_rollout_arms().drain();
        assert!(
            matches!(&queued[..], [ArmCommand::Arm(_), ArmCommand::StoppedWithoutReply { turn_id, .. }] if turn_id == "t1"),
            "{queued:?}"
        );
        for command in queued {
            tailers.apply(command);
        }
        assert!(tailers.tick(live).is_empty());
        assert!(tailers.is_armed(&owner), "the drain has only just begun");
        assert!(tailers.holds_file(&owner));

        let after_drain = std::time::Instant::now() + DRAIN_AFTER_STOP;
        assert!(tailers.tick_at(after_drain, live).is_empty());
        assert!(
            !tailers.is_armed(&owner),
            "no task_complete within the drain"
        );
        assert!(!tailers.holds_file(&owner), "the rollout is closed");
        assert!(tailers.take_replies().is_empty());

        append(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"t1\",\"last_agent_message\":\"Too late.\"}}\n",
        );
        assert!(tailers.tick(live).is_empty());
        assert!(
            tailers.take_replies().is_empty(),
            "the retired turn hands nothing"
        );
        registry.shutdown_all();
    }

    /// Issue #1359 (Qodo on PR #1375): a poll that found turn `t1`'s failure
    /// can finish after Codex's next `UserPromptSubmit` has put the card on
    /// Thinking. That failure is stale and must not repaint the card. The hook
    /// loop queues a prompt's arm before it applies the prompt, so the newer
    /// arm is still in the queue when the failure is reported. A re-arm of the
    /// same turn does not supersede it, and with no newer arm it is an Error.
    #[tokio::test]
    async fn a_codex_failure_superseded_by_a_newer_turn_does_not_repaint_the_card() {
        use crate::codex_rollout_tail::{
            CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY, CodexTurnFailure,
        };
        use crate::quota_signals::FailureOutcome;
        let registry = Arc::new(AgentPtyRegistry::new());
        let owner = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "codex-race".to_string(),
                )]),
                agent_type: Some(AgentType::Codex),
                ..SpawnOptions::default()
            })
            .expect("spawn stand-in");
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane("codex-race".to_string());
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let prompt = |turn: &str| {
            let mut event =
                super::quota_admission_tests::quota_frame(crate::event::EventType::Thinking);
            event.agent_type = AgentType::Codex;
            event.pane_id = Some("codex-race".to_string());
            event.agent_id = Some(owner.clone());
            event.metadata.insert(
                CODEX_TRANSCRIPT_PATH_METADATA_KEY.to_string(),
                "/x/rollout-a.jsonl".to_string(),
            );
            event
                .metadata
                .insert(CODEX_TURN_ID_METADATA_KEY.to_string(), turn.to_string());
            event
        };
        // The hook loop's order for a prompt: queue its arm, then apply it.
        let submit = |turn: &'static str| {
            let event = prompt(turn);
            queue_codex_rollout_arm(&registry, &event, "");
            ingest_event(&state, &event_tx, &registry, event)
        };
        let failure = || CodexTurnFailure {
            pane_id: "codex-race".to_string(),
            agent_id: owner.clone(),
            session_id: "s".to_string(),
            turn_id: "t1".to_string(),
            outcome: FailureOutcome::Error,
            message: None,
        };
        let status = || async { state.read().await.sessions["s"].status.clone() };

        // Turn t1 is armed and the monitor has drained it; a poll finds its
        // failure while turn t2 is submitted.
        submit("t1").await;
        registry.codex_rollout_arms().drain();
        submit("t2").await;
        report_codex_rollout_failure(&state, &event_tx, &registry, failure()).await;
        assert_eq!(
            status().await,
            crate::state::SessionStatus::Thinking,
            "the older turn's failure repainted the newer turn's card"
        );

        // A re-arm of the same turn does not supersede its own failure.
        registry.codex_rollout_arms().drain();
        submit("t1").await;
        report_codex_rollout_failure(&state, &event_tx, &registry, failure()).await;
        assert_eq!(status().await, crate::state::SessionStatus::Error);

        // Control: nothing newer queued, so the failure is reported.
        registry.codex_rollout_arms().drain();
        submit("t1").await;
        registry.codex_rollout_arms().drain();
        report_codex_rollout_failure(&state, &event_tx, &registry, failure()).await;
        assert_eq!(status().await, crate::state::SessionStatus::Error);
        registry.shutdown_all();
    }

    /// Scenario: Authenticate oversized raw Codex frames with the live pane's
    /// capability and verify admission retains each event while bounding its metadata.
    /// Issue #714 (audit A1): raw frames written straight to the hook socket —
    /// not built by the hook CLI, so none of its bounds applied — carrying a
    /// megabyte `codex_transcript_path` or `codex_turn_id` for a live Codex
    /// owner. Admission drops each oversized or empty key, keeps the event and
    /// any in-bounds key, and nothing oversized reaches the arm queue.
    #[tokio::test]
    async fn raw_oversized_codex_rollout_frames_are_bounded_at_admission() {
        use crate::codex_rollout_tail::{
            ArmCommand, CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY,
            MAX_METADATA_BYTES, MAX_TURN_ID_BYTES,
        };
        let registry = Arc::new(AgentPtyRegistry::new());
        let owner = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "codex-raw".to_string(),
                )]),
                agent_type: Some(AgentType::Codex),
                ..SpawnOptions::default()
            })
            .expect("spawn stand-in");

        let dir = tempfile::tempdir().unwrap();
        // Plain bind, for the umask reason given in
        // `run_hook_loop_persists_agent_type_into_registry`.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod tempdir");
        let sock = dir.path().join("hook.sock");
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        // The registry ownership oracle, installed as `run_daemon_with` does,
        // so the owner's frames are admitted to the state at all.
        let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
        {
            let mut st = state.write().await;
            st.set_agent_ownership(Arc::downgrade(&ownership));
            st.register_pane("codex-raw".to_string());
        }
        let (event_tx, mut rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let (registry, state) = (registry.clone(), state.clone());
            let shutdown = Arc::new(Notify::new());
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move { run_hook_loop(listener, state, event_tx, registry, shutdown, wtr).await }
        });

        let huge_path = format!("/x/{}/rollout-a.jsonl", "p".repeat(1024 * 1024));
        let huge_turn = "t".repeat(1024 * 1024);
        let valid_path = "/x/rollout-a.jsonl";
        let frame = |session: &str, path: &str, turn: &str| {
            serde_json::json!({
                "session_id": session,
                "agent_type": "codex",
                "event_type": "thinking",
                "timestamp": "2026-09-26T12:00:00Z",
                "pane_id": "codex-raw",
                "agent_id": owner,
                "token": registry.hook_token_of(&owner).expect("owner capability"),
                "metadata": {
                    CODEX_TRANSCRIPT_PATH_METADATA_KEY: path,
                    CODEX_TURN_ID_METADATA_KEY: turn,
                },
            })
        };
        let frames = [
            frame("raw-both-huge", &huge_path, &huge_turn),
            frame("raw-huge-path", &huge_path, "t-ok"),
            frame(
                "raw-just-over",
                &"q".repeat(MAX_METADATA_BYTES + 1),
                &"u".repeat(MAX_TURN_ID_BYTES + 1),
            ),
            frame("raw-empty", "", ""),
            frame("raw-valid", valid_path, "t-valid"),
        ];
        let mut stream = UnixStream::connect(&sock)
            .await
            .expect("connect hook socket");
        for f in &frames {
            stream
                .write_all(format!("{f}\n").as_bytes())
                .await
                .expect("write hook line");
        }
        stream.flush().await.unwrap();

        // Every frame is relayed — an invalid key costs the key, not the event —
        // and each carries only the keys that were in bounds.
        let mut relayed = std::collections::HashMap::new();
        while relayed.len() < frames.len() {
            let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("every frame is relayed")
                .expect("broadcast open");
            if let BroadcastMsg::Event(event) = msg {
                relayed.insert(event.session_id.clone(), event);
            }
        }
        // A value too long to print is summarised, so a regression reports
        // its size instead of dumping a megabyte into the failure.
        let keys = |session: &str| {
            let meta = &relayed[session].metadata;
            let shown = |key: &str| {
                meta.get(key).map(|v| {
                    if v.len() > 64 {
                        format!("<{} bytes>", v.len())
                    } else {
                        v.clone()
                    }
                })
            };
            (
                shown(CODEX_TRANSCRIPT_PATH_METADATA_KEY),
                shown(CODEX_TURN_ID_METADATA_KEY),
            )
        };
        assert_eq!(keys("raw-both-huge"), (None, None));
        assert_eq!(keys("raw-huge-path"), (None, Some("t-ok".to_string())));
        assert_eq!(keys("raw-just-over"), (None, None));
        assert_eq!(keys("raw-empty"), (None, None));
        assert_eq!(
            keys("raw-valid"),
            (Some(valid_path.to_string()), Some("t-valid".to_string()))
        );

        let queued = registry.codex_rollout_arms().drain();
        let arms: Vec<(Option<&str>, Option<&str>)> = queued
            .iter()
            .map(|c| match c {
                ArmCommand::Arm(req) => (req.path.as_deref(), req.turn_id.as_deref()),
                ArmCommand::Disarm { .. } | ArmCommand::StoppedWithoutReply { .. } => {
                    panic!("no Stop was sent: {c:?}")
                }
            })
            .collect();
        assert_eq!(
            arms,
            vec![(None, Some("t-ok")), (Some(valid_path), Some("t-valid"))],
            "only in-bounds values are queued"
        );
        assert!(queued.iter().all(|c| c.byte_len() < 4 * MAX_METADATA_BYTES));

        handle.abort();
        let _ = handle.await;
        registry.shutdown_all();
    }

    /// PRD #1223: the pane-closed marker is daemon-authoritative. A producer
    /// posting it on the hook socket must not reach an attached TUI with it —
    /// that would let any same-uid process make a TUI drop a live pane.
    #[tokio::test]
    async fn ingest_strips_a_producer_supplied_pane_closed_marker() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, mut rx) = broadcast::channel(8);
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            crate::event::DAEMON_PANE_CLOSED_METADATA_KEY.to_string(),
            crate::event::DAEMON_PANE_CLOSED_METADATA_VALUE.to_string(),
        );
        let forged = AgentEvent {
            session_id: "pane-victim".to_string(),
            agent_type: AgentType::None,
            event_type: crate::event::EventType::SessionEnd,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata,
            pane_id: Some("victim".to_string()),
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
            background: None,
            background_revision: None,
        };
        assert!(forged.is_daemon_pane_closed(), "precondition");
        ingest_event(&state, &event_tx, &registry, forged).await;
        let BroadcastMsg::Event(relayed) = rx.try_recv().expect("the event is relayed") else {
            panic!("expected an event");
        };
        assert!(
            !relayed.is_daemon_pane_closed(),
            "the relayed copy must not carry the marker: {:?}",
            relayed.metadata
        );
    }

    /// Scenario (PRD #1666 audit M-A1): a producer sends an unheld `question`
    /// envelope whose status event carries a prompt and the deck's own
    /// `deck_message` marker, and no question. The event the daemon relays
    /// carries no marker, and the card records the prompt in its history, as
    /// the same event on the raw hook ingress would.
    #[tokio::test]
    async fn a_question_envelope_cannot_label_its_prompt_as_a_deck_message() {
        use crate::inbox_prompt::{DECK_MESSAGE_METADATA_KEY, is_deck_message};
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane("pane-1".to_string());
        let (event_tx, mut rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let prompt = "please fix the build";
        let mut event =
            super::quota_admission_tests::quota_frame(crate::event::EventType::Thinking);
        event.agent_id = None;
        event.user_prompt = Some(prompt.to_string());
        event
            .metadata
            .insert(DECK_MESSAGE_METADATA_KEY.to_string(), "1".to_string());
        let signal = crate::event::QuestionSignal {
            pane_id: "pane-1".to_string(),
            token: None,
            event,
            hold: false,
            ack: false,
            confirm_hold: false,
        };
        let mut reader = tokio::io::BufReader::new(tokio::io::empty());
        let mut writer = tokio::io::sink();
        let mut slot = None;
        let holds = Arc::new(tokio::sync::Semaphore::new(1));
        let reply = handle_question_signal(
            signal,
            None,
            false,
            &state,
            &event_tx,
            &registry,
            QuestionConnection {
                reader: &mut reader,
                writer: &mut writer,
                slot: &mut slot,
                holds: &holds,
                hold_deadline: Duration::from_secs(5),
                write_timeout: Duration::from_secs(5),
            },
        )
        .await;
        assert!(reply.is_some(), "an unheld question is released at once");
        let BroadcastMsg::Event(relayed) = rx.try_recv().expect("the event is relayed") else {
            panic!("expected an event");
        };
        assert!(!is_deck_message(&relayed), "{:?}", relayed.metadata);
        let state = state.read().await;
        let card = &state.sessions["s"];
        assert_eq!(card.last_user_prompt.as_deref(), Some(prompt));
        assert_eq!(card.first_prompts, vec![prompt.to_string()]);
        drop(state);
        registry.shutdown_all();
    }

    /// Issue #714 (audit N1/N2): a Codex worker stand-in and an orchestrator
    /// stand-in (both `cat`), the daemon's `AppState` with the registry
    /// ownership oracle installed as `run_daemon_with` installs it, and an
    /// outstanding delegation from the orchestrator to the worker bound to the
    /// worker's agent id — everything a blocked-worker notice needs to be owed.
    struct QuotaNoticeFixture {
        registry: Arc<AgentPtyRegistry>,
        state: SharedState,
        event_tx: broadcast::Sender<BroadcastMsg>,
        worker_pane: &'static str,
        worker_id: String,
        orch_pane: &'static str,
        orch_id: String,
    }

    impl QuotaNoticeFixture {
        const NOTICE: &'static str = "delegated worker blocked by a provider usage limit";

        async fn new(worker_pane: &'static str, orch_pane: &'static str) -> Self {
            let registry = Arc::new(AgentPtyRegistry::new());
            let spawn = |pane: &str, agent_type: AgentType| {
                registry
                    .spawn_agent(SpawnOptions {
                        command: Some("/bin/cat"),
                        env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                            DOT_AGENT_DECK_PANE_ID.to_string(),
                            pane.to_string(),
                        )]),
                        agent_type: Some(agent_type),
                        ..SpawnOptions::default()
                    })
                    .expect("spawn stand-in")
            };
            let worker_id = spawn(worker_pane, AgentType::Codex);
            let orch_id = spawn(orch_pane, AgentType::ClaudeCode);
            let state: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            {
                let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
                let mut st = state.write().await;
                st.set_agent_ownership(Arc::downgrade(&ownership));
                st.register_pane(worker_pane.to_string());
                st.register_pane(orch_pane.to_string());
            }
            let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let fx = Self {
                registry,
                state,
                event_tx,
                worker_pane,
                worker_id,
                orch_pane,
                orch_id,
            };
            fx.arm_delegation();
            fx
        }

        fn arm_delegation(&self) {
            let armed = self
                .registry
                .arm_outstanding_delegation(
                    self.worker_pane,
                    "coder",
                    self.orch_pane,
                    &self.orch_id,
                    None,
                )
                .expect("arm the delegation");
            self.registry.bind_delegation_worker_agent_id(
                self.worker_pane,
                armed.seq,
                &self.worker_id,
            );
        }

        fn worker_event(&self, event_type: crate::event::EventType) -> AgentEvent {
            AgentEvent {
                session_id: format!("{}-session", self.worker_pane),
                agent_type: AgentType::Codex,
                event_type,
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata: std::collections::HashMap::new(),
                pane_id: Some(self.worker_pane.to_string()),
                agent_id: Some(self.worker_id.clone()),
                agent_version: None,
                schema_version: None,
                live_target: None,
                background: None,
                background_revision: None,
            }
        }

        /// The worker's own `quota_blocked` report.
        fn quota_blocked(&self) -> AgentEvent {
            let mut event = self.worker_event(crate::event::EventType::QuotaBlocked);
            event.metadata.insert(
                crate::quota_block::QUOTA_BLOCKED_KIND_METADATA_KEY.to_string(),
                "usage_limit".to_string(),
            );
            event
        }

        /// The worker reports a block through [`ingest_event`] — latch, card and
        /// orchestrator notice, the production path. Returns the latched epoch.
        async fn report_block(&self) -> u64 {
            ingest_event(
                &self.state,
                &self.event_tx,
                &self.registry,
                self.quota_blocked(),
            )
            .await;
            self.registry
                .quota_published_epoch(self.worker_pane, &self.worker_id)
                .expect("the report latched a block")
        }

        /// Only the first half of [`ingest_event`] for a block — the latch and
        /// the card, under the `AppState` write lock — so a test can interleave
        /// a work hook before the notice is attempted.
        async fn latch_block(&self) -> u64 {
            let mut state = self.state.write().await;
            let epoch = self
                .registry
                .note_quota_block(self.worker_pane, &self.worker_id)
                .expect("the worker owns its pane");
            state.apply_event(self.quota_blocked());
            epoch
        }

        async fn work_hook(&self) {
            let thinking = self.worker_event(crate::event::EventType::Thinking);
            ingest_event(&self.state, &self.event_tx, &self.registry, thinking).await;
        }

        async fn blocked_cards(&self) -> usize {
            self.state
                .read()
                .await
                .sessions
                .values()
                .filter(|s| s.status == crate::state::SessionStatus::Blocked)
                .count()
        }

        /// [`Self::notices`], polled until at least one notice has been posted
        /// and the count has stopped moving.
        async fn settled_notices(&self) -> usize {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut last = self.notices().await;
            loop {
                let now = self.notices().await;
                if now >= 1 && now == last {
                    return now;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the notice never reached the orchestrator"
                );
                last = now;
            }
        }

        /// PRD #1666 S5: how many blocked-worker notices have been posted to
        /// the orchestrator's inbox, after giving an in-flight post time to land.
        async fn notices(&self) -> usize {
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.registry
                .inbox()
                .rendered(self.orch_pane)
                .into_iter()
                .filter(|delivery| delivery.text.contains(Self::NOTICE))
                .count()
        }
    }

    /// Failure bound for an await that runs while a test holds the
    /// orchestrator's pane writer.
    const HELD_WRITER_BOUND: Duration = Duration::from_secs(10);

    /// Await `fut` while the test holds the orchestrator's pane writer in
    /// `held`. A regression that makes `fut` wait on that writer would
    /// otherwise deadlock the test, so past [`HELD_WRITER_BOUND`] the guard is
    /// released and the test fails naming `what`.
    async fn bounded_while_holding<G, T>(
        held: &mut Option<G>,
        what: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        match tokio::time::timeout(HELD_WRITER_BOUND, fut).await {
            Ok(value) => value,
            Err(_) => {
                drop(held.take());
                panic!(
                    "{what} did not finish within {HELD_WRITER_BOUND:?} while the \
                     orchestrator's pane writer was held"
                );
            }
        }
    }

    /// Issue #714 (audit N2, R2): a genuine work hook that clears the block
    /// after it was applied, but before the orchestrator is told, suppresses the
    /// notice — whether it lands before the notice is claimed or between the
    /// claim and the write. Either way the notice stays owed: a second block of
    /// the SAME delegation, reported by the worker, then delivers exactly one
    /// notice, and a third block of it delivers none.
    #[tokio::test]
    async fn quota_block_cleared_before_the_notice_is_not_reported() {
        let fx = QuotaNoticeFixture::new("quota-cleared-worker", "quota-cleared-orch").await;

        // latch → work hook → claim: nothing is claimed or written.
        let epoch = fx.latch_block().await;
        assert_eq!(
            fx.blocked_cards().await,
            1,
            "precondition: the block applied"
        );
        fx.work_hook().await;
        assert_eq!(
            fx.blocked_cards().await,
            0,
            "precondition: the hook cleared it"
        );
        assert!(
            notify_orchestrator_of_quota_block(&fx.registry, fx.worker_pane, &fx.worker_id, epoch)
                .is_none(),
            "a cleared block claimed a notice"
        );
        assert_eq!(fx.notices().await, 0, "a cleared block was reported");

        // latch → claim → work hook → write: the writer-held re-check refuses.
        let epoch = fx.latch_block().await;
        let notice = fx
            .registry
            .claim_worker_blocked_notice(fx.worker_pane, &fx.worker_id)
            .expect("the first suppression left the notice owed");
        fx.work_hook().await;
        fx.registry
            .deliver_worker_blocked_notice(fx.worker_pane, &fx.worker_id, epoch, notice)
            .await;
        assert_eq!(
            fx.notices().await,
            0,
            "a block cleared after the claim was reported"
        );

        // A second block of the SAME delegation (not re-armed): the refused
        // claim was released, so the notice is posted — once — and claimed.
        fx.report_block().await;
        let delivered = fx.settled_notices().await;
        assert_eq!(delivered, 1, "expected exactly one notice");
        assert!(
            fx.registry
                .claim_worker_blocked_notice(fx.worker_pane, &fx.worker_id)
                .is_none(),
            "the delivered notice was not claimed"
        );

        // A third block of the same delegation reports nothing more.
        fx.work_hook().await;
        fx.report_block().await;
        assert_eq!(fx.notices().await, delivered, "the delegation re-notified");
        fx.registry.shutdown_all();
    }

    /// Issue #714 (review): every pane's hooks pass through `ingest_event`, so
    /// ingesting a worker's block must not wait on the orchestrator's pane
    /// writer. Hold that writer and ingest the worker's `quota_blocked`: it
    /// returns at once with the card Blocked, and — PRD #1666 S5 — the notice is
    /// a post to the orchestrator's inbox, which waits on no writer either: it
    /// is there, once, while the writer is still held.
    #[tokio::test]
    async fn quota_publish_does_not_wait_on_a_busy_orchestrator_writer() {
        let fx = QuotaNoticeFixture::new("quota-busy-worker", "quota-busy-orch").await;
        let writer = fx
            .registry
            .agent_writer(&fx.orch_id)
            .expect("the orchestrator's writer");
        let mut held = Some(writer.lock().await);

        bounded_while_holding(&mut held, "ingesting a block", fx.report_block()).await;
        assert_eq!(fx.blocked_cards().await, 1);
        let delivered =
            bounded_while_holding(&mut held, "posting the notice", fx.settled_notices()).await;
        assert_eq!(delivered, 1, "expected exactly one notice");
        assert!(
            fx.registry
                .claim_worker_blocked_notice(fx.worker_pane, &fx.worker_id)
                .is_none(),
            "the delivered notice keeps its claim"
        );
        drop(held.take());
        fx.registry.shutdown_all();
    }

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
                background: None,
                background_revision: None,
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
            cleared: false,
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

    /// PRD #1666 long-turn delivery: through the production sink, a "delivered
    /// after all" notice lifts the daemon's card from the deck's not-delivered
    /// Error to Thinking and broadcasts the lift; once the card no longer shows
    /// that notice, a second lift is not broadcast at all, so a client that
    /// predates the lift never reads it as a stray Thinking.
    #[tokio::test]
    async fn a_delivered_after_all_notice_lifts_only_a_card_showing_the_decks_notice() {
        const PANE_ID: &str = "late-ack-card";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE_ID.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn the recipient pane");
        let daemon_state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        {
            let mut state = daemon_state.write().await;
            state.register_pane(PANE_ID.to_string());
            state.apply_event(AgentEvent {
                session_id: "gen-late".to_string(),
                agent_type: AgentType::Pi,
                event_type: crate::event::EventType::SessionStart,
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata: std::collections::HashMap::new(),
                pane_id: Some(PANE_ID.to_string()),
                agent_id: Some(agent_id.clone()),
                agent_version: None,
                schema_version: None,
                live_target: None,
                background: None,
                background_revision: None,
            });
        }
        let (event_tx, mut attached_rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        install_delivery_notice_sink(&registry, daemon_state.clone(), event_tx.clone());
        let notice = |cleared| DeliveryNotice {
            pane_id: PANE_ID.to_string(),
            agent_id: agent_id.clone(),
            delivery_id: "m-k3xzq2ab-0123456789abcdef".to_string(),
            session_id: None,
            detail: if cleared {
                crate::inbox_display::NOTICE_DELIVERED_LATE
            } else {
                crate::inbox_display::NOTICE_MAX_DELIVER
            },
            cleared,
        };
        let next_event = |rx: &mut broadcast::Receiver<BroadcastMsg>| {
            let rx = rx.resubscribe();
            async move {
                let mut rx = rx;
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if let Ok(BroadcastMsg::Event(event)) = rx.recv().await
                            && event.pane_id.as_deref() == Some(PANE_ID)
                        {
                            break event;
                        }
                    }
                })
                .await
                .ok()
            }
        };
        let status = |state: &crate::state::AppState| state.sessions["gen-late"].status.clone();

        let reported = next_event(&mut attached_rx);
        registry.publish_delivery_notice(notice(false));
        let reported = reported
            .await
            .expect("the not-delivered report is broadcast");
        assert_eq!(reported.event_type, crate::event::EventType::Error);
        assert_eq!(
            status(&*daemon_state.read().await),
            crate::state::SessionStatus::Error
        );

        let lifted = next_event(&mut attached_rx);
        registry.publish_delivery_notice(notice(true));
        let lifted = lifted.await.expect("the lift is broadcast");
        assert!(lifted.is_delivery_notice_cleared(), "{lifted:?}");
        assert_eq!(
            status(&*daemon_state.read().await),
            crate::state::SessionStatus::Thinking
        );

        // The sink applies notices one at a time in the order they arrive, so
        // a second report published after the second lift is the next event
        // broadcast only if that lift broadcast nothing.
        let next = next_event(&mut attached_rx);
        registry.publish_delivery_notice(notice(true));
        registry.publish_delivery_notice(notice(false));
        let next = next.await;
        registry.shutdown_all();
        let next = next.expect("the second report is broadcast");
        assert!(
            !next.is_delivery_notice_cleared() && next.event_type == crate::event::EventType::Error,
            "a card that no longer shows the notice gets no lift: {next:?}"
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
        let agent_id = registry
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
            "token": registry.hook_token_of(&agent_id).expect("shell capability"),
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

    /// Scenario: PRD #1666 part 3, round 5-wake. A wrapped generation's final event names how the wrapper's command exited. The report is kept when the deck's own child sent it, and dropped when anything else did: a `dot-agent-deck wrap` nested below it (another pid with the same pane, generation and token), a later process given the child's pid (the same pid, started at another time), a connection whose sender or start time cannot be read, or a generation the registry does not hold.
    #[spec("inbox/status/032")]
    #[test]
    fn status_032_only_the_decks_own_child_reports_its_commands_exit() {
        let pane = "wrapper-report";
        let registry = Arc::new(AgentPtyRegistry::new());
        let generation = registry
            .spawn_agent(SpawnOptions {
                command: Some("sleep 30"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn");
        let child = registry.child_pid(&generation).expect("child pid");
        let born = crate::platform::proc::process_start_of(child).expect("child start time");
        let peer = |pid, started| {
            Some(AcceptedPeer {
                pid,
                started,
                same_user: Some(true),
            })
        };
        let kept = |peer, agent: &str| {
            let mut event =
                super::quota_admission_tests::quota_frame(crate::event::EventType::Idle);
            event.pane_id = Some(pane.to_string());
            event.agent_id = Some(agent.to_string());
            event.metadata.insert(
                crate::event::WRAPPER_CHILD_EXIT_METADATA_KEY.to_string(),
                "5".to_string(),
            );
            keep_wrapper_report_from_own_child(&mut event, peer, None, &registry);
            event
                .metadata
                .contains_key(crate::event::WRAPPER_CHILD_EXIT_METADATA_KEY)
        };
        assert!(
            kept(peer(child, Some(born)), &generation),
            "the deck's own child"
        );
        assert!(
            !kept(peer(child + 1, Some(born)), &generation),
            "a nested wrapper"
        );
        assert!(
            !kept(peer(child, Some(born + 1)), &generation),
            "a later process reusing the child's pid"
        );
        assert!(
            !kept(peer(child, None), &generation),
            "a sender whose start time could not be read"
        );
        assert!(!kept(None, &generation), "an unreadable sender");
        assert!(
            !kept(peer(child, Some(born)), "another-generation"),
            "another generation"
        );
        registry.shutdown_all();
    }

    /// Scenario: PRD #1666 part 3, round 5-wake. Through the real hook socket, a process that is not the deck's child (as a `dot-agent-deck wrap` nested inside the agent is) sends the wrapped generation's final event, attested by the generation's own token and naming its command's exit. The event itself is applied, so the pane learns its agent type, but its exit report is not recorded for the generation, leaving that to the deck's own wrapper.
    #[spec("inbox/status/032")]
    #[tokio::test]
    async fn status_032_a_nested_wrappers_exit_report_is_dropped_at_the_socket() {
        let pane = "nested-wrapper";
        let registry = Arc::new(AgentPtyRegistry::new());
        let generation = registry
            .spawn_agent(SpawnOptions {
                command: Some("sleep 30"),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    pane.to_string(),
                )]),
                ..SpawnOptions::default()
            })
            .expect("spawn");
        assert_eq!(registry.wrapper_report_for_test(&generation), None);
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod tempdir");
        let sock = dir.path().join("hook.sock");
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(pane.to_string());
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move {
                run_hook_loop(
                    listener,
                    state,
                    event_tx,
                    registry,
                    Arc::new(Notify::new()),
                    wtr,
                )
                .await
            }
        });
        let event = serde_json::json!({
            "session_id": "nested-sess",
            "agent_type": "codex",
            "event_type": "idle",
            "timestamp": "2026-10-26T12:00:00Z",
            "pane_id": pane,
            "agent_id": generation,
            "metadata": { "wrapper_child_exit": "7" },
            "token": registry.hook_token_of(&generation).expect("capability"),
        });
        let mut stream = UnixStream::connect(&sock).await.expect("connect");
        stream
            .write_all(format!("{event}\n").as_bytes())
            .await
            .expect("write hook line");
        stream.flush().await.unwrap();
        // The event's agent type reaching the registry is what says the
        // event itself was ingested.
        let mut applied = false;
        for _ in 0..80 {
            if registry.agent_records()[0].agent_type == Some(AgentType::Codex) {
                applied = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(applied, "the event itself was ingested");
        assert_eq!(
            registry.wrapper_report_for_test(&generation),
            None,
            "a report from below the deck's child is not the generation's"
        );
        handle.abort();
        let _ = handle.await;
        registry.shutdown_all();
    }

    // -----------------------------------------------------------------------
    // The hook socket's two ingest bounds (issues #903 / #319)
    // -----------------------------------------------------------------------

    /// A real `run_hook_loop` driven against a real hook socket, with the
    /// daemon-side `AppState` the tests below read their verdict from.
    ///
    /// Deliberately binds WITHOUT `bind_socket`, for the reason spelled out in
    /// `run_hook_loop_persists_agent_type_into_registry` above: that helper
    /// flips the process-global umask around `bind`, and under `cargo test`
    /// (where all lib tests share one process) that window races concurrent
    /// tempdir creation in other tests. Socket permissions are irrelevant to
    /// what these two assert.
    struct HookLoopFixture {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        state: SharedState,
        handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
    }

    impl HookLoopFixture {
        /// The loop at its production idle bound. Every assertion below
        /// finishes in seconds, so the real minute never elapses.
        fn start() -> Self {
            Self::start_with_idle_timeout(HOOK_CONNECTION_IDLE_TIMEOUT)
        }

        fn start_with_idle_timeout(idle_timeout: Duration) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod tempdir");
            let socket = dir.path().join("hook.sock");
            let listener = IpcListener::from_tokio_listener(
                UnixListener::bind(&socket).expect("bind hook socket"),
            );
            let state: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let handle = tokio::spawn({
                let state = state.clone();
                let registry = Arc::new(AgentPtyRegistry::new());
                let shutdown = Arc::new(Notify::new());
                let wtr = crate::issue_dispatch_run::new_worktree_registry();
                async move {
                    run_hook_loop_with_idle_timeout(
                        listener,
                        state,
                        event_tx,
                        registry,
                        shutdown,
                        wtr,
                        idle_timeout,
                    )
                    .await
                }
            });
            Self {
                _dir: dir,
                socket,
                state,
                handle,
            }
        }

        /// Poll until `session_id` has a card in the daemon's `AppState`.
        /// Bounded so a regression (the event never applied) fails fast rather
        /// than hanging the tier.
        async fn wait_for_session(&self, session_id: &str) {
            for _ in 0..80 {
                if self.state.read().await.sessions.contains_key(session_id) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("session {session_id:?} never reached the daemon's AppState");
        }

        async fn has_session(&self, session_id: &str) -> bool {
            self.state.read().await.sessions.contains_key(session_id)
        }

        /// Assert `session_id` stays absent for a bounded window. Only ever
        /// used *after* a happens-after ordering fact has been established, so
        /// it confirms "refused" rather than betting on "not yet".
        async fn assert_session_stays_absent(&self, session_id: &str) {
            for _ in 0..20 {
                assert!(
                    !self.has_session(session_id).await,
                    "session {session_id:?} must not reach AppState"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    /// One `session_start` line, padded with `padding` bytes of ASCII filler in
    /// its metadata so the caller can put the serialized line at an exact
    /// length. Returns the line WITHOUT its trailing newline, which is what
    /// `read_capped_line` measures.
    fn padded_session_start(session_id: &str, padding: usize) -> String {
        serde_json::json!({
            "session_id": session_id,
            "agent_type": "claude_code",
            "event_type": "session_start",
            "timestamp": "2026-09-08T12:00:00Z",
            "pane_id": format!("pane-{session_id}"),
            "metadata": { "padding": "x".repeat(padding) },
        })
        .to_string()
    }

    /// How much padding puts `padded_session_start`'s line at exactly `target`
    /// bytes. The filler is plain ASCII, so JSON encoding grows it 1:1 and the
    /// difference between the unpadded line and the target IS the padding.
    fn padding_for_line_len(session_id: &str, target: usize) -> usize {
        let base = padded_session_start(session_id, 0).len();
        target
            .checked_sub(base)
            .expect("the target line length must exceed the envelope")
    }

    /// Scenario: Drive the real `run_hook_loop` against a real hook socket and write two `session_start` lines that differ only in length — one of exactly `MAX_HOOK_LINE_BYTES`, one a single byte longer. The line at the cap must produce a card; the line over it must produce none, must not be truncated into a partial event, and must not stop the daemon serving the next connection.
    #[spec("hooks/ingest/001")]
    #[tokio::test]
    async fn ingest_001_over_long_hook_line_is_refused_at_the_production_cap() {
        use crate::bounded_read::MAX_HOOK_LINE_BYTES;

        let fixture = HookLoopFixture::start();

        // Exactly at the cap: accepted, because the cap is inclusive and
        // because refusing here would truncate a legitimate producer — a
        // `work-done` report is large by design (issues #508 / #509).
        let at_cap = padded_session_start(
            "at-cap",
            padding_for_line_len("at-cap", MAX_HOOK_LINE_BYTES),
        );
        assert_eq!(
            at_cap.len(),
            MAX_HOOK_LINE_BYTES,
            "the at-cap fixture must sit exactly on the boundary"
        );
        let mut stream = UnixStream::connect(&fixture.socket)
            .await
            .expect("connect hook socket");
        stream
            .write_all(format!("{at_cap}\n").as_bytes())
            .await
            .expect("a line at the cap must be readable end to end");
        stream.flush().await.unwrap();
        fixture.wait_for_session("at-cap").await;
        drop(stream);

        // One byte over: refused. The write may well fail partway — the daemon
        // stops reading and drops the connection the moment the line crosses
        // the cap, which is the point — so its outcome is deliberately not
        // asserted on.
        let over_cap = padded_session_start(
            "over-cap",
            padding_for_line_len("over-cap", MAX_HOOK_LINE_BYTES + 1),
        );
        assert_eq!(over_cap.len(), MAX_HOOK_LINE_BYTES + 1);
        let mut stream = UnixStream::connect(&fixture.socket)
            .await
            .expect("connect hook socket");
        let _ = stream.write_all(format!("{over_cap}\n").as_bytes()).await;
        let _ = stream.flush().await;
        drop(stream);

        // The happens-after fact that makes "absent" mean "refused": a THIRD
        // connection's ordinary event lands after the over-cap one was sent, so
        // the loop has demonstrably moved on. Before the fix the over-cap line
        // was simply buffered and applied like any other.
        let ordinary = padded_session_start("after-refusal", 0);
        let mut stream = UnixStream::connect(&fixture.socket)
            .await
            .expect("connect hook socket");
        stream
            .write_all(format!("{ordinary}\n").as_bytes())
            .await
            .expect("write ordinary hook line");
        stream.flush().await.unwrap();
        fixture.wait_for_session("after-refusal").await;
        drop(stream);

        fixture.assert_session_stays_absent("over-cap").await;

        fixture.handle.abort();
        let _ = fixture.handle.await;
    }

    #[test]
    fn clamp_for_log_passes_a_short_line_through_unchanged() {
        let line = r#"{"session_id":"s","event_type":"idle"}"#;
        assert_eq!(clamp_for_log(line), line);
    }

    #[test]
    fn clamp_for_log_marks_a_long_line_as_truncated() {
        const MARKER: &str = "…<truncated>";
        // A line at the ceiling the read cap allows — the case the clamp
        // exists for. A line only a byte or two over the prefix comes back
        // slightly LONGER than it went in, because the marker costs more than
        // the bytes dropped; what is bounded is the producer's own contribution,
        // not the rendered string, so that is what this asserts.
        let line = "x".repeat(crate::bounded_read::MAX_HOOK_LINE_BYTES);
        let got = clamp_for_log(&line);
        assert!(
            got.ends_with(MARKER),
            "a clamped line must say so, or it reads as the whole payload"
        );
        assert_eq!(
            got.len(),
            MALFORMED_LOG_PREFIX_BYTES + MARKER.len(),
            "the log line must carry at most the prefix plus the marker, \
             whatever the producer sent"
        );
    }

    /// The line is arbitrary UTF-8 from a producer, so the cut must land on a
    /// char boundary. Slicing mid-character panics — inside the hook loop's
    /// spawned task, which would take the connection down silently.
    #[test]
    fn clamp_for_log_cuts_on_a_char_boundary() {
        // A 3-byte character repeated puts a character across every offset that
        // is not a multiple of 3, including the prefix boundary.
        for pad in 0..3 {
            let line = format!(
                "{}{}",
                "a".repeat(pad),
                "€".repeat(MALFORMED_LOG_PREFIX_BYTES)
            );
            let got = clamp_for_log(&line);
            assert!(got.ends_with("…<truncated>"), "pad {pad} should clamp");
        }
    }

    /// Issue #1082: the bound `hook_line_for_log` keeps is the one judgement in
    /// that sweep, so it is pinned rather than only argued in a doc comment.
    ///
    /// The `raw_line` warning exists to show an operator the typo in an
    /// `event_type` a hook wrote. An ordinary hook event is already longer than
    /// [`crate::config_validation::MAX_QUOTED_VALUE_CHARS`], and its keys
    /// serialize in sorted order, so `event_type` sits past character 120 — the
    /// value bound would therefore cut away the one field the diagnostic is
    /// about, turning the fix into a lost diagnostic. Both halves are asserted:
    /// what this bound keeps, and what the other one would have dropped.
    #[test]
    fn hook_line_for_log_keeps_the_event_type_the_value_bound_would_cut() {
        use crate::config_validation::{MAX_QUOTED_VALUE_CHARS, escape_field_for_log};

        let line = serde_json::json!({
            "agent_type": "claude_code",
            "cwd": "/home/dev/code/dot-agent-deck-dispatch-issue-1082/xtask/linkage-check",
            "event_type": "sessoin_start",
            "metadata": { "bash_command": "cargo nextest run --workspace" },
            "pane_id": "pane-7f3c1a9e-4b21-4d8a-9c55-0e6f2b1d3a47",
            "session_id": "7f3c1a9e-4b21-4d8a-9c55-0e6f2b1d3a47",
            "timestamp": "2026-09-08T12:00:00Z",
        })
        .to_string();

        assert!(
            hook_line_for_log(&line).contains("sessoin_start"),
            "the typo this warning exists to surface must survive the bound: {line}"
        );
        assert!(
            !escape_field_for_log(&line, MAX_QUOTED_VALUE_CHARS).contains("sessoin_start"),
            "if the value bound also kept it, this site would have no reason to differ \
             from the other twenty and the doc comment above would be wrong: {line}"
        );
    }

    /// Issue #1082: and it still escapes. `clamp_for_log` alone bounded the line
    /// without touching a single byte a terminal acts on, which is the whole
    /// defect — a CR inside a rejected payload overwrote the line reporting it.
    #[test]
    fn hook_line_for_log_escapes_what_a_terminal_would_act_on() {
        let hostile = "junk\rovershoot\u{1b}[2Jcleared\u{202e}reversed\u{85}c1";
        let got = hook_line_for_log(hostile);

        assert!(
            !got.chars().any(|c| c.is_control()),
            "a raw CR overwrites the line being written: {got:?}"
        );
        assert!(
            !got.chars().any(crate::untrusted_text::is_bidi_format_char),
            "a bidi override reorders the line in whatever renders it: {got:?}"
        );
        assert!(
            got.contains("overshoot") && got.contains("cleared") && got.contains("reversed"),
            "escaping preserves the evidence rather than dropping it: {got:?}"
        );
        // Bounding stays `clamp_for_log`'s job, and doing it twice is what the
        // helper's doc says it must not do.
        let long = "x".repeat(crate::bounded_read::MAX_HOOK_LINE_BYTES);
        assert!(
            hook_line_for_log(&long).ends_with("…<truncated>"),
            "the byte bound and its marker must survive composition"
        );
    }

    /// Issue #318: neither raw-line diagnostic may put a hook capability token
    /// into the log, whether the line parses or not.
    #[test]
    fn hook_line_for_log_never_logs_a_token() {
        let token = "0123456789abcdef".repeat(4);
        let parsed = serde_json::json!({
            "event_type": "sessoin_start",
            "pane_id": "p",
            "session_id": "s",
            "token": token,
        })
        .to_string();
        let got = hook_line_for_log(&parsed);
        assert!(!got.contains(&token), "{got}");
        assert!(got.contains("sessoin_start"), "{got}");

        let escaped = parsed.replace("\"token\"", "\"\\u0074oken\"");
        assert!(!hook_line_for_log(&escaped).contains(&token));

        let malformed = format!("{{\"message_type\":\"delegate\",\"token\":\"{token}\",\"to\":7");
        let got = hook_line_for_log(&malformed);
        assert!(!got.contains(&token), "{got}");
        assert!(got.contains("<redacted>"), "{got}");

        let split = format!("{{\"token\":\"\\u0030{}\"", &token[1..]);
        assert!(!hook_line_for_log(&split).contains(&token[1..]));
    }

    /// Issue #318 (Greptile, PR #1559): a capability nested anywhere in a line
    /// that parses — a `token` member below the top level, or a token-shaped
    /// string under any key, including inside a metadata map of an event type
    /// the daemon does not recognise — never reaches the log either.
    #[test]
    fn hook_line_for_log_never_logs_a_nested_token() {
        let token = "0123456789abcdef".repeat(4);
        let nested = serde_json::json!({
            "event_type": "sessoin_start",
            "pane_id": "p",
            "session_id": "s",
            "extra": { "token": token },
            "list": [ { "deep": { "token": token } } ],
            "metadata": { "capability": token, "note": format!("cap={token}") },
        })
        .to_string();
        let got = hook_line_for_log(&nested);
        assert!(!got.contains(&token), "{got}");
        assert!(got.contains("sessoin_start"), "{got}");
        assert!(got.contains("capability"), "the key survives: {got}");

        let under_a_token_key = serde_json::json!({
            "event_type": "unknown_kind",
            "metadata": { "token": "not-a-capability" },
        })
        .to_string();
        let got = hook_line_for_log(&under_a_token_key);
        assert!(
            !got.contains("not-a-capability"),
            "a token member is withheld at any depth whatever it holds: {got}"
        );

        let as_a_key = serde_json::json!({ "metadata": { token.clone(): 1 } }).to_string();
        assert!(!hook_line_for_log(&as_a_key).contains(&token));
    }

    /// Issue #318 (round-5 audit, PR #1559): parsing keeps only the LAST of a
    /// set of duplicate members, so a capability inside an earlier occurrence
    /// is invisible to the walk — and must not come back by way of the raw
    /// line. Covers an overwritten parent holding a nested `token`, a repeated
    /// nested key whose first value is the token, and a duplicate name spelled
    /// with an escape that decodes to the same key.
    #[test]
    fn hook_line_for_log_never_logs_a_token_in_a_discarded_duplicate_member() {
        let token = "0123456789abcdef".repeat(4);

        let overwritten_parent = format!(
            "{{\"event_type\":\"sessoin_start\",\"extra\":{{\"token\":\"{token}\"}},\"extra\":null}}"
        );
        let got = hook_line_for_log(&overwritten_parent);
        assert!(!got.contains(&token), "overwritten parent: {got}");
        assert!(got.contains("sessoin_start"), "{got}");

        let duplicate_nested_value = format!(
            "{{\"event_type\":\"sessoin_start\",\"metadata\":{{\"capability\":\"{token}\",\"capability\":\"x\"}}}}"
        );
        let got = hook_line_for_log(&duplicate_nested_value);
        assert!(!got.contains(&token), "duplicate nested value key: {got}");

        let escaped_duplicate_name = format!(
            "{{\"event_type\":\"sessoin_start\",\"extra\":{{\"token\":\"{token}\"}},\"\\u0065xtra\":null}}"
        );
        let got = hook_line_for_log(&escaped_duplicate_name);
        assert!(!got.contains(&token), "escaped duplicate name: {got}");
    }

    /// Scenario: Open `MAX_CONCURRENT_HOOK_CONNECTIONS` hook connections, each sending one `session_start` and then staying open, then open one more and send an event on it. The extra event must not be applied while every slot is held, and must be applied — not dropped — as soon as one of the held connections closes.
    #[spec("hooks/ingest/002")]
    #[tokio::test]
    async fn ingest_002_concurrent_hook_connections_are_capped_without_losing_events() {
        let fixture = HookLoopFixture::start();

        // Fill every slot. Each connection proves it was ACCEPTED by landing an
        // event, then stays open — so its task is parked in the read and its
        // permit is genuinely held.
        let mut held = Vec::new();
        for i in 0..MAX_CONCURRENT_HOOK_CONNECTIONS {
            let session = format!("held-{i:02}");
            let mut stream = UnixStream::connect(&fixture.socket)
                .await
                .expect("connect hook socket");
            stream
                .write_all(format!("{}\n", padded_session_start(&session, 0)).as_bytes())
                .await
                .expect("write hook line");
            stream.flush().await.unwrap();
            fixture.wait_for_session(&session).await;
            held.push(stream);
        }

        // One more. Its connect succeeds (the kernel's listen backlog takes it)
        // but the loop cannot accept it, because it is waiting for a permit.
        let mut blocked = UnixStream::connect(&fixture.socket)
            .await
            .expect("connect hook socket");
        blocked
            .write_all(format!("{}\n", padded_session_start("over-limit", 0)).as_bytes())
            .await
            .expect("the write lands in the socket buffer even unaccepted");
        blocked.flush().await.unwrap();

        // Unlike `ingest_001`'s negative, this one has no happens-after to lean
        // on — it is the bound itself, so the window is the assertion. It
        // cannot flake red: with the cap absent the event is applied at once,
        // and with the cap present nothing can apply it until a slot frees
        // below.
        fixture.assert_session_stays_absent("over-limit").await;

        // Free one slot. The bound is backpressure, not admission control, so
        // the event was delayed rather than dropped and must now arrive.
        drop(held.pop().expect("a held connection"));
        fixture.wait_for_session("over-limit").await;

        drop(held);
        fixture.handle.abort();
        let _ = fixture.handle.await;
    }

    /// Scenario: Fill every hook-connection slot with peers that connect and then send nothing at all, and drive the loop at a short idle bound. An event written on one more connection must still be applied, which can only happen once the daemon reclaims a stalled peer's slot.
    #[spec("hooks/ingest/003")]
    #[tokio::test]
    async fn ingest_003_a_stalled_connection_does_not_hold_its_slot_forever() {
        // Short enough that the test finishes in well under a second; the
        // production bound is a minute and is asserted only by construction
        // (`run_hook_loop` passes `HOOK_CONNECTION_IDLE_TIMEOUT`). What is
        // under test is the reclaim, not the number.
        let fixture = HookLoopFixture::start_with_idle_timeout(Duration::from_millis(250));

        // Every slot taken by a peer that never writes a byte — the shape that
        // made the connection cap a new availability failure mode before this
        // bound existed.
        let mut stalled = Vec::new();
        for _ in 0..MAX_CONCURRENT_HOOK_CONNECTIONS {
            stalled.push(
                UnixStream::connect(&fixture.socket)
                    .await
                    .expect("connect hook socket"),
            );
        }

        let mut live = UnixStream::connect(&fixture.socket)
            .await
            .expect("connect hook socket");
        live.write_all(format!("{}\n", padded_session_start("after-reclaim", 0)).as_bytes())
            .await
            .expect("the write lands in the socket buffer even unaccepted");
        live.flush().await.unwrap();

        // The only route to this event being applied is a stalled peer losing
        // its slot: `ingest_002` pins that a connection which is merely OPEN
        // and idle-but-live keeps its permit, so nothing else here can free
        // one. Without the reclaim this hangs to the poll bound and fails.
        fixture.wait_for_session("after-reclaim").await;

        drop(stalled);
        fixture.handle.abort();
        let _ = fixture.handle.await;
    }

    /// A producer-supplied string built from every family this sweep is about:
    /// a raw LF (forges a whole following log line), a raw CR (overwrites the
    /// line being written), ESC + a CSI sequence (clears the screen of whatever
    /// renders it), a C1 control (U+0085 NEL, which some terminals still act
    /// on), and a bidi override (U+202E, which reorders the line without
    /// changing a byte). The readable words between them are what the evidence
    /// assertions look for: escaping must PRESERVE the string, not drop it.
    const FORGING_ID: &str =
        "p1\nINFO forged-line\rovershoot\u{1b}[2Jcleared\u{202e}reversed\u{85}c1";

    /// Every line the daemon wrote while a subscriber was installed, one entry
    /// per rendered event with its trailing newline removed.
    fn captured_log_lines(raw: &str) -> Vec<&str> {
        raw.split('\n').filter(|l| !l.is_empty()).collect()
    }

    /// Scenario: Drive the real `run_hook_loop` against a real hook socket and send, on
    /// one connection, a message of every `DaemonMessage` verb plus a raw `AgentEvent`,
    /// an event whose `event_type` is a typo, and a line that is not JSON at all —
    /// every one of them carrying ids built from LF, CR, ESC, a C1 control and a bidi
    /// override. Capture the daemon's real `tracing` output: no line it writes may
    /// contain any of those characters, while still naming the evidence and the site.
    #[spec("hooks/ingest/005")]
    #[tokio::test]
    async fn ingest_005_producer_supplied_ids_cannot_forge_a_daemon_log_line() {
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CapturedLog {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
            type Writer = CapturedLog;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        // `#[tokio::test]` is a current-thread runtime, so the `run_hook_loop`
        // task below is polled on THIS thread and sees this thread-local
        // subscriber. `with_ansi(false)` so the only escape bytes that could
        // appear in the capture are ones the daemon itself wrote.
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with_ansi(false)
            .finish();
        let subscriber_guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let fixture = HookLoopFixture::start();

        // Every verb on one connection, because the loop reads lines from a
        // connection SEQUENTIALLY: that makes the sentinel at the end a
        // happens-after fact for all of them, which separate connections (one
        // task each) would not give.
        //
        // The registry is fresh, so no pane was ever issued a hook token and
        // `hook_provenance::classify` returns `Unattested` for a claimed pane it
        // has never heard of — admitted, which is exactly the shape this class
        // is about: an unattested claim reaches the arms and gets logged. The
        // first line below is the other half, a malformed token, which is
        // REFUSED and logged by the provenance gate itself.
        let ts = "2026-09-08T12:00:00Z";
        let lines = vec![
            // Refused by the provenance gate → `claimed_pane` on the refusal.
            serde_json::json!({
                "message_type": "work_done", "pane_id": FORGING_ID,
                "task": "t", "done": false, "token": "not-a-token", "timestamp": ts,
            }),
            serde_json::json!({
                "message_type": "delegate", "pane_id": FORGING_ID,
                "task": "t", "to": ["coder"], "timestamp": ts,
            }),
            serde_json::json!({
                "message_type": "restart_role", "pane_id": FORGING_ID,
                "role": FORGING_ID, "force": false, "timestamp": ts,
            }),
            serde_json::json!({
                "message_type": "spawn_role", "pane_id": FORGING_ID,
                "role": FORGING_ID, "timestamp": ts,
            }),
            serde_json::json!({
                "message_type": "dispatch", "pane_id": FORGING_ID,
                "name": "unit", "timestamp": ts,
            }),
            serde_json::json!({
                "message_type": "work_done", "pane_id": FORGING_ID,
                "task": "t", "done": false, "timestamp": ts,
            }),
            serde_json::json!({
                "message_type": "get_seed", "pane_id": FORGING_ID, "timestamp": ts,
            }),
            serde_json::json!({
                "message_type": "list_targets", "pane_id": FORGING_ID, "timestamp": ts,
            }),
            // A raw `AgentEvent`: the `Received event` line, and — because this
            // pane is not one the daemon spawned — the foreign-SessionStart
            // warning too.
            serde_json::json!({
                "session_id": FORGING_ID, "agent_type": "claude_code",
                "event_type": "session_start", "timestamp": ts,
                "pane_id": FORGING_ID, "metadata": {},
            }),
            // An unrecognized `event_type` → the warning that logs the WHOLE
            // raw line back.
            serde_json::json!({
                "session_id": FORGING_ID, "agent_type": "claude_code",
                "event_type": "sessoin_start", "timestamp": ts,
                "pane_id": FORGING_ID, "metadata": {},
            }),
        ];

        let mut stream = UnixStream::connect(&fixture.socket)
            .await
            .expect("connect hook socket");
        for line in &lines {
            stream
                .write_all(format!("{line}\n").as_bytes())
                .await
                .expect("write hook line");
        }
        // Not JSON at all → the `Malformed event:` branch, which interpolates
        // the line into the MESSAGE rather than into a field. A literal LF is
        // unreachable here (it would end the line), so this one carries the
        // other four families.
        stream
            .write_all("this is not json \r\u{1b}[2Jcleared\u{202e}reversed\u{85}c1\n".as_bytes())
            .await
            .expect("write malformed line");
        // The happens-after fact: an ordinary event on the SAME connection,
        // after all of the above, whose card proves the loop processed them.
        stream
            .write_all(format!("{}\n", padded_session_start("forge-sentinel", 0)).as_bytes())
            .await
            .expect("write sentinel");
        stream.flush().await.unwrap();
        fixture.wait_for_session("forge-sentinel").await;

        drop(subscriber_guard);
        fixture.handle.abort();
        let _ = fixture.handle.await;

        let raw = String::from_utf8(captured.0.lock().unwrap().clone())
            .expect("captured log must be valid UTF-8");
        let log_lines = captured_log_lines(&raw);
        assert!(
            !log_lines.is_empty(),
            "the subscriber captured nothing, so this test would pass vacuously"
        );

        // The property, stated over the REAL output rather than over the helper:
        // nothing a terminal or a line-oriented reader acts on survives into any
        // line the daemon wrote.
        //
        // Collected rather than asserted per line, deliberately: an `assert!`
        // inside the loop stops at the FIRST unescaped site and hides however
        // many others there are, which is the wrong shape of failure for a
        // sweep. Reverting one call site must name that one call site.
        let forged: Vec<&&str> = log_lines
            .iter()
            .filter(|line| {
                line.chars().any(|c| c.is_control())
                    || line.chars().any(crate::untrusted_text::is_bidi_format_char)
            })
            .collect();
        assert!(
            forged.is_empty(),
            "a raw LF forges a whole log line, a raw CR overwrites the one being written, \
             ESC clears the screen of whatever renders it and a bidi override reorders it; \
             none may survive into a daemon log line, but {} did: {forged:#?}",
            forged.len()
        );

        // Escaping preserves the evidence rather than dropping it — a value
        // that silently loses characters reads as a DIFFERENT value.
        for needle in ["forged-line", "overshoot", "cleared", "reversed"] {
            assert!(
                raw.contains(needle),
                "escaping must keep the evidence readable; {needle:?} missing from {raw:?}"
            );
        }
        assert!(
            raw.contains("\\n") && raw.contains("\\r") && raw.contains("\\u{1b}"),
            "the escaped spellings are what preserve the evidence: {raw:?}"
        );

        // And the sweep itself: every site this issue escaped must actually have
        // been reached, or the loop above proves nothing about it. Naming them
        // here is what makes a site quietly dropped from the sweep fail.
        for site in [
            "hook socket: refused a message whose hook",
            "Received delegate signal",
            "Received restart-role signal",
            "Received spawn-role signal",
            "Received dispatch signal",
            "dispatch from unknown pane",
            "Received work-done signal",
            "work-done from unknown pane",
            "Refused a get-seed request",
            "Received list-targets request",
            "Received event",
            "unrecognized event_type",
            "SessionStart for a pane this daemon did not spawn",
            "Malformed event:",
            "action from unknown pane",
        ] {
            assert!(
                raw.contains(site),
                "the hostile message never reached {site:?}, so this test does not \
                 cover that site: {raw:?}"
            );
        }
    }

    /// PRD #1497 audit AU-B3: a turn-ending line carries the agent's private
    /// reply, and no diagnostic that logs the raw line may put it in the log.
    /// Drives the real `run_hook_loop` with lines built by the production
    /// producer (`agent_event_cli_line`, the shape Pi's `agent-event` and every
    /// hook's `Stop` send): one whose `event_type` is a typo (the
    /// unrecognized-event warning), one cut off inside the reply (unparseable,
    /// the `Malformed event:` warning), and one that parses but is not an event.
    /// The sentinel never reaches the captured log, while each warning still
    /// names the line's identifiers.
    #[tokio::test]
    async fn hook_diagnostics_never_log_a_turn_reply() {
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CapturedLog {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
            type Writer = CapturedLog;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        const SENTINEL: &str = "PRIVATE_REPLY_SENTINEL_1497_e71f";
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with_ansi(false)
            .finish();
        let subscriber_guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);
        let fixture = HookLoopFixture::start();

        let event: AgentEvent = serde_json::from_value(serde_json::json!({
            "session_id": "reply-log-session", "agent_type": "pi", "event_type": "idle",
            "timestamp": "2026-10-08T12:00:00Z", "pane_id": "reply-log-pane", "metadata": {},
        }))
        .unwrap();
        let reply = crate::daemon_protocol::FinalReply {
            turn_id: None,
            text: format!("{SENTINEL}: all tests pass. {}", "padding ".repeat(80)),
            failed: false,
        };
        let line = crate::hook::agent_event_cli_line(&event, None, Some(&reply)).unwrap();
        assert!(
            line.contains(SENTINEL),
            "the producer put the reply on the line"
        );
        let unknown = line.replace("\"event_type\":\"idle\"", "\"event_type\":\"idel\"");
        assert_ne!(unknown, line, "the typo must land in the line");
        let cut = line[..line.find(SENTINEL).unwrap() + SENTINEL.len()].to_owned();
        let not_an_event = line.replace(
            "\"session_id\":\"reply-log-session\"",
            "\"session_id\":7,\"note\":\"reply-log-session\"",
        );
        assert_ne!(not_an_event, line);

        let mut stream = UnixStream::connect(&fixture.socket)
            .await
            .expect("connect hook socket");
        for line in [&unknown, &cut, &not_an_event] {
            stream
                .write_all(format!("{line}\n").as_bytes())
                .await
                .expect("write hook line");
        }
        stream
            .write_all(format!("{}\n", padded_session_start("reply-log-sentinel", 0)).as_bytes())
            .await
            .expect("write sentinel");
        stream.flush().await.unwrap();
        fixture.wait_for_session("reply-log-sentinel").await;

        drop(subscriber_guard);
        fixture.handle.abort();
        let _ = fixture.handle.await;

        let raw = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            raw.contains("unrecognized event_type") && raw.matches("Malformed event:").count() == 2,
            "every diagnostic was reached, or this proves nothing: {raw}"
        );
        assert!(
            !raw.contains(SENTINEL) && !raw.contains("all tests pass"),
            "a turn reply reached the log: {raw}"
        );
        assert!(
            raw.contains("reply-log-session") && raw.contains("<withheld"),
            "the diagnostics still name the line and say what was withheld: {raw}"
        );
    }

    // ── Issue #1159: why the two tests below no longer race a 2-second window ──
    //
    // Both of them need the shell-activity monitor to REPORT a real detached
    // child. They used to type a `sleep 2` into the pane and assert the status
    // inside 3s (and the broadcast inside 5s), which made the stimulus a
    // two-second window the signal had to be caught inside.
    //
    // That window is shorter than the silence this monitor is DESIGNED to
    // produce. `SAMPLE_TIMEOUT` lets one tick wait 2s for a sample;
    // `MAX_TABLE_AGE` discards an answer older than 3s; `SamplingHealth` then
    // holds the next sample off for up to `BACKOFF_MAX` = 8s. So a SINGLE sample
    // that overruns its deadline — the outcome
    // `shell_activity_monitor_leaves_statuses_alone_when_the_sample_times_out`
    // exists to pin as CORRECT — swallowed the whole window, and both tests then
    // failed at their full bound having seen no busy edge at all.
    //
    // Measured by injecting exactly that one wedge into the real sampler (a
    // first sample delayed 3.5s, nothing else changed): the first test failed at
    // 3.10s with the status still `Idle` and one sample started, the second at
    // 5.18s with no `ShellBusy` — reproducing #1159's reported `3.076s` and
    // `5.089s` from one cause. That shared cause is also the answer to why they
    // never failed SEPARATELY: one degraded sampling episode outlasts both
    // windows at once, and the five siblings in this family inject their own
    // sampler, so a wedged `ps` cannot reach them.
    //
    // So the child now lives until the test KILLS it. The rising edge waits on a
    // condition that stays true, and the falling edge is made true by
    // construction rather than by a `sleep` expiring. Same shape as
    // `shell_activity_004`'s ready-marker gate (PR #390) and as #1148's
    // `delegate_034` fix: make the state real, do not widen the wait. Nothing
    // about what these tests assert changed — only whether the thing asserted is
    // still there to be observed, and the third test below —
    // `..._reports_a_real_detached_child_after_an_unusable_sample` — pins that
    // property directly, with the wedge injected rather than waited for.

    /// Issue #1159: the ceiling on how long these tests wait for the
    /// shell-activity signal to report a detached child that is STILL RUNNING.
    ///
    /// **Not a window the child has to be caught inside** — the child outlives
    /// the wait — so this bounds "the signal never arrived at all" and is paid
    /// only on the failure path. Derived from the monitor's own constants rather
    /// than picked: one degraded sampling episode can legitimately swallow
    /// `MAX_TABLE_AGE` (3s) and then a `SamplingHealth::BACKOFF_MAX` hold-off
    /// (8s), so ~11s of silence is correct behaviour and any smaller bound
    /// asserts against the product's own contract. 30s is ~2.7x that and half of
    /// nextest's 60s slow-timeout period, so a genuinely dead signal still
    /// reports inside one slow window with its diagnostics intact.
    const SHELL_ACTIVITY_SIGNAL_BUDGET: Duration = Duration::from_secs(30);

    /// Issue #1159: the detached child's backstop lifetime, for the one path
    /// that skips [`DetachedPaneChild::end`] — the test process being `SIGKILL`ed.
    ///
    /// Every ordinary path ends the child explicitly, including a panic, which
    /// runs `Drop`. The only wait that needs the child ALIVE is the rising edge,
    /// bounded by one [`SHELL_ACTIVITY_SIGNAL_BUDGET`] — a child that expired
    /// mid-wait would resurrect exactly the race this replaces — so 120s leaves
    /// 4x headroom over the longest such wait, while staying short enough that a
    /// `SIGKILL`ed run's orphan is gone long before anyone looks for it.
    const DETACHED_CHILD_BACKSTOP_SECS: &str = "120";

    /// Issue #1159: a `setsid`-detached descendant of a pane's shell, typed into
    /// the pane's own PTY, which lives until this test ends it.
    ///
    /// The topology is PRD #386's and unchanged: an interactive `/bin/sh` has job
    /// control on and makes each foreground job its own process-group leader, and
    /// `setsid(2)` fails with `EPERM` for a process that already leads a group —
    /// so `python3` must detach a *child*, not itself — and the parent's
    /// `waitpid` keeps the pane occupied for the child's whole life. What #1159
    /// changed is the child's LIFETIME and the fact that it reports its pid back,
    /// so the test can end it on purpose instead of waiting out a `sleep`.
    ///
    /// The pid arrives through a file written with `os.write` on a raw descriptor
    /// — unbuffered, so the digits are in the file before the next statement runs
    /// — and then `os.rename`d into place, so a reader cannot observe the final
    /// path at `open(2)` before it has content. That TOCTOU is not hypothetical
    /// in this area: it is the unrelated defect #1159's own report separates
    /// itself from (`an_unwrapped_agent_spawn_carries_a_lifetime_tag`, #1104).
    struct DetachedPaneChild {
        /// The detached child's pid, as its own parent reported it.
        pid: i32,
        /// Whether [`Self::end`] has already run, so `Drop` after an explicit
        /// end sends nothing — one `SIGKILL` per child rather than two.
        ended: bool,
        /// Keeps the pid file's directory alive for this value's lifetime.
        _dir: tempfile::TempDir,
    }

    impl DetachedPaneChild {
        /// Type the stimulus into `agent_id`'s PTY and return once the child
        /// exists and has reported its pid.
        async fn launch(registry: &Arc<AgentPtyRegistry>, agent_id: &str) -> Self {
            use std::io::Write as _;

            let dir = tempfile::tempdir().expect("temp dir for the detached child's pid file");
            let partial = dir.path().join("child.pid.partial");
            let final_path = dir.path().join("child.pid");
            // Both paths are interpolated into a Python single-quoted string
            // inside a shell double-quoted word, so a `TMPDIR` carrying any of
            // these would rewrite the command rather than name a file. The two
            // file names are ours; only the base can misbehave, and it comes from
            // `std::env::temp_dir()`. Refusing here rather than escaping for both
            // quoting layers: an exotic base is a broken environment, not a case
            // to support, and a named refusal beats hand-rolled double escaping
            // that nothing in this repo exercises (greptile, PR #1192).
            for path in [&partial, &final_path] {
                let text = path.to_string_lossy();
                assert!(
                    !text.contains(['\'', '"', '$', '`', '\\', '\n']),
                    "this test's temp base cannot be embedded in a shell command: {text:?} \
                     contains one of ' \" $ ` \\ or a newline. Point TMPDIR somewhere plainer."
                );
            }
            let command = format!(
                "python3 -c \"import os; pid = os.fork(); \
                 (os.setsid(), os.execv('/bin/sleep', ['sleep', '{backstop}'])) if pid == 0 \
                 else (os.write(os.open('{partial}', os.O_WRONLY | os.O_CREAT), \
                 str(pid).encode()), os.rename('{partial}', '{final_path}'), \
                 os.waitpid(pid, 0))\"\n",
                backstop = DETACHED_CHILD_BACKSTOP_SECS,
                partial = partial.display(),
                final_path = final_path.display(),
            );
            {
                let writer = registry
                    .agent_writer(agent_id)
                    .expect("spawned agent must be in the registry");
                let mut w = writer.lock().await;
                w.write_all(command.as_bytes())
                    .expect("write detached-child command");
                w.flush().expect("flush");
            }

            // Waiting for the pid file SEPARATELY from the status/broadcast wait
            // is what makes a failure here legible, and #1159 was filed with no
            // captured message at all. A shell that never ran the command fails
            // on THIS assertion, naming the PTY's own output, rather than
            // consuming the signal's budget and reading like a monitor defect.
            let deadline = tokio::time::Instant::now() + SHELL_ACTIVITY_SIGNAL_BUDGET;
            loop {
                if let Some(pid) = std::fs::read_to_string(&final_path)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                {
                    return Self {
                        pid,
                        ended: false,
                        _dir: dir,
                    };
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the pane's shell never launched the detached child within {:?} — nothing \
                     readable at {}. The pane's PTY saw: {:?}",
                    SHELL_ACTIVITY_SIGNAL_BUDGET,
                    final_path.display(),
                    registry
                        .snapshot(agent_id)
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }

        /// End the child, so a pane with no out-of-session descendant is true by
        /// construction rather than by a `sleep` having expired.
        ///
        /// Goes through the product's own [`crate::platform::proc::force_kill_pid`]
        /// rather than `libc::kill` directly: it refuses pid 0 and anything that
        /// would resolve to a non-positive `pid_t`, so this cannot broadcast to a
        /// process group. The error is discarded because `ESRCH` on a pid that has
        /// already gone is the expected reading, not a fault.
        ///
        /// Aimed at a pid this test's own stimulus created, and reached while that
        /// pid is still the child's: every wait that runs before this one is
        /// bounded by [`SHELL_ACTIVITY_SIGNAL_BUDGET`] and the child sleeps for 4x
        /// that, so it has not exited and its pid has not been reissued. Same
        /// shape as `shell_activity_004`'s `KillOnDrop`.
        fn end(&mut self) {
            if self.ended {
                return;
            }
            self.ended = true;
            let _ = crate::platform::proc::force_kill_pid(self.pid as u32);
        }
    }

    impl Drop for DetachedPaneChild {
        fn drop(&mut self) {
            self.end();
        }
    }

    /// Issue #1159: wait until `session_id`'s card reports `want`, bounded by
    /// [`SHELL_ACTIVITY_SIGNAL_BUDGET`], returning the last status observed so
    /// the caller's own `assert_eq!` prints it.
    async fn wait_for_session_status(
        state: &SharedState,
        session_id: &str,
        want: crate::state::SessionStatus,
    ) -> crate::state::SessionStatus {
        let deadline = tokio::time::Instant::now() + SHELL_ACTIVITY_SIGNAL_BUDGET;
        loop {
            let current = state.read().await.sessions[session_id].status.clone();
            if current == want || tokio::time::Instant::now() >= deadline {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
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
    ///
    /// **What #1159 changed here, and why the pipeline assertions are again
    /// unchanged.** The detached child used to be a `sleep 2` and the falling
    /// edge used to be it expiring, which made the rising edge a 2-second window
    /// the signal had to be caught inside — shorter than the silence this monitor
    /// is designed to produce, so one slow sample failed the test. Now the child
    /// outlives the wait and the test KILLS it to drive the falling edge. See the
    /// block comment above [`SHELL_ACTIVITY_SIGNAL_BUDGET`] for the measurement.
    #[tokio::test]
    async fn shell_activity_monitor_reflects_a_real_detached_shell_command() {
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
            background: None,
            background_revision: None,
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
        // nothing but the raw shell. `DetachedPaneChild` carries the topology and
        // why it is the one PRD #386's scan fires on; it returns once the child
        // genuinely exists, so everything below is asserting about the MONITOR
        // rather than about whether the shell got round to running anything.
        let mut child = DetachedPaneChild::launch(&registry, &agent_id).await;

        // Rising edge. The child is still running and stays running until the
        // `end()` below, so a sample the monitor legitimately discarded or held
        // off is a slower answer here, never a missed one (issue #1159).
        let current =
            wait_for_session_status(&state, "sess-370", crate::state::SessionStatus::Working).await;
        assert_eq!(
            current,
            crate::state::SessionStatus::Working,
            "the monitor must promote the session to Working while the detached \
             child runs, with zero agent-emitted events involved"
        );

        // Falling edge, made real by construction: the child is ended here, so
        // "the pane has no out-of-session descendant left" is a fact this test
        // established rather than a `sleep` it hoped had expired in time.
        child.end();
        let current =
            wait_for_session_status(&state, "sess-370", crate::state::SessionStatus::Idle).await;
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
    /// covers that). Spawn a real `/bin/sh` pane, seed it a hook
    /// `SessionStart`, then seed a SECOND `SessionStart` under the SAME agent
    /// with a NEW hook session id — a same-agent `/clear` / thread restart, so
    /// the pane's hook generation rolls forward to `sess-21-gen2` while
    /// `apply_event`'s reuse guard keeps the card under the stable
    /// `sess-21-gen1`. Run the real `run_shell_activity_monitor`, subscribe to
    /// its broadcast, and type a `setsid`-detached `sleep` into the PTY (the
    /// topology PRD #386's descendant scan fires on). Every event
    /// it broadcasts must report the CURRENT hook generation as its
    /// `session_id` AND carry the owning agent's id — an unstamped event cannot
    /// be remapped onto a reconnected TUI's hydrated card and mints a phantom
    /// session instead. Since issue #1678 (D7) that id is the SAMPLED spawn's
    /// registry id rather than whatever the pane's card carries, so the card is
    /// seeded with the real spawn id, as every daemon-spawned agent's hooks are.
    ///
    /// **What #1159 changed here.** The detached child outlives the wait for the
    /// broadcast instead of being a `sleep 2` the `ShellBusy` had to be caught
    /// inside; the stamped-shape assertions are untouched. The block comment
    /// above [`SHELL_ACTIVITY_SIGNAL_BUDGET`] has the measurement.
    #[tokio::test]
    async fn shell_activity_monitor_stamps_the_owning_agent_across_a_session_rollover() {
        const PANE: &str = "pane-21";
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
            agent_id: Some(spawned.clone()),
            agent_version: None,
            schema_version: None,
            live_target: None,
            background: None,
            background_revision: None,
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
        // topology a real Claude Bash-tool child has), `execv` it into a `sleep`,
        // and have the parent `waitpid` for it — see `DetachedPaneChild` for the
        // full rationale. A plain foreground `sleep` typed into the pane's own
        // PTY was busy under #370's `tcgetpgrp` body but is deliberately NOT
        // busy under #386's scan, so it would never produce the `ShellBusy`
        // this test needs to inspect. The child outlives the wait below, so the
        // `ShellBusy` is an event this test waits for rather than one it has to
        // be listening at the right two seconds to hear (issue #1159).
        let _child = DetachedPaneChild::launch(&registry, &spawned).await;

        // Read broadcasts until the busy transition arrives (the monitor also
        // emits the pane's initial idle edge), asserting the stamped shape on
        // EVERY event it publishes — a single unstamped one is enough to mint
        // a phantom card on a reconnected TUI.
        let mut saw_busy = false;
        let deadline = tokio::time::Instant::now() + SHELL_ACTIVITY_SIGNAL_BUDGET;
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
                Some(spawned.as_str()),
                "the synthesized {:?} must carry the sampled spawn's agent id \
                 across a same-agent restart; an event with `None` cannot be \
                 remapped onto any hydrated TUI card",
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

    /// Scenario: issue #1159's own mechanism, as a test rather than as a comment.
    /// Spawn a real `/bin/sh` pane with a seeded hook session, run the real
    /// monitor but wrap its real process-table sampler so the FIRST sample takes
    /// 3.5s — long enough to overrun `SAMPLE_TIMEOUT`, and long enough that when
    /// the retained future finally answers its table is past `MAX_TABLE_AGE` and
    /// is correctly discarded, which then arms a `SamplingHealth` hold-off. Type
    /// the same `setsid`-detached child the two tests above use. The session must
    /// still reach `Working`: a degraded sampling episode may DELAY this signal
    /// (that is `SamplingHealth`'s whole design) but must not lose it once the
    /// machine answers again.
    ///
    /// This is the failing case #1159 was: with a child that expires after two
    /// seconds, this exact injection reproduced both reported failures — 3.10s
    /// with the status still `Idle`, and 5.18s with no `ShellBusy` — against the
    /// reported 3.076s and 5.089s. The product was right both times, so the test
    /// that survives it is the deliverable, and this one fails if the recovery
    /// path it depends on ever stops working.
    #[tokio::test]
    async fn shell_activity_monitor_reports_a_real_detached_child_after_an_unusable_sample() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-1159".to_string())],
                agent_type: None,
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);

        state.write().await.apply_event(AgentEvent {
            session_id: "sess-1159".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: crate::event::EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some("pane-1159".to_string()),
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
            background: None,
            background_revision: None,
        });

        // One wedged sample, then honest ones. The sampler is the REAL
        // `process_table_async` — only its first answer is delayed — so what this
        // test exercises after the wedge is the production classification path,
        // not a fixture.
        let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            let samples = samples.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move |roots| {
                    let roots = roots.to_vec();
                    let nth = samples.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        if nth == 0 {
                            tokio::time::sleep(Duration::from_millis(3_500)).await;
                        }
                        crate::platform::proc::process_table_async(&roots).await
                    }
                })
                .await
            }
        });

        let mut child = DetachedPaneChild::launch(&registry, &agent_id).await;
        let current =
            wait_for_session_status(&state, "sess-1159", crate::state::SessionStatus::Working)
                .await;
        assert_eq!(
            current,
            crate::state::SessionStatus::Working,
            "a sampling episode that overran SAMPLE_TIMEOUT and then blew \
             MAX_TABLE_AGE may delay the shell-activity signal, but the monitor \
             must still report a child that is STILL RUNNING once a usable sample \
             lands — {} samples were started",
            samples.load(std::sync::atomic::Ordering::SeqCst)
        );

        child.end();
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
                run_shell_activity_monitor_with(registry, state, event_tx, move |_roots| {
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

    /// Scenario: issue #862. Spawn a real `/bin/sh` pane so there IS a candidate
    /// to classify, then run the real shell-activity monitor with a sample that
    /// always answers `None` — a `ps` that cannot be run at all — and count how
    /// many samples it starts over a fixed window. The count must be bounded by
    /// the exponential hold-off (500ms, 1s, 2s, 4s, 8s, capped) rather than one
    /// per 500ms tick, and the pane's status must be left exactly where it was.
    /// Before the fix a permanently unusable sample forked a fresh `ps` twice a
    /// second against a machine the daemon was itself adding load to, and warned
    /// about it every cycle — which is how one day of `deck.log` accumulated the
    /// 1716 `shell-activity` warnings recorded in the issue.
    #[tokio::test]
    async fn shell_activity_monitor_backs_off_after_repeated_unusable_samples() {
        const PANE: &str = "pane-862";
        const SESSION: &str = "sess-862";

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
            background: None,
            background_revision: None,
        };
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
            "precondition: the pane must start out reading Working, so a status the \
             backoff wrongly changed would be visible"
        );

        let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let samples = samples.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move |_roots| {
                    samples.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Answers immediately, and unusably. Unlike the wedged-`ps`
                    // case above this sample FINISHES, so nothing is retained
                    // and the only thing standing between it and a fresh fork
                    // on the very next tick is the hold-off.
                    async { None }
                })
                .await
            }
        });

        // 6s of wall clock. Without the hold-off the loop starts one sample per
        // 500ms tick: 12. With it the starts fall at roughly t=0.5, 1.0, 2.0 and
        // 4.0 (each hold-off doubling from the 500ms base), so 4-5 depending on
        // where the ticks land.
        const WINDOW: Duration = Duration::from_millis(6_000);
        tokio::time::sleep(WINDOW).await;
        let started = samples.load(std::sync::atomic::Ordering::SeqCst);

        assert!(
            started >= 2,
            "the backoff must throttle the sample, not stop it: only {started} sample(s) \
             started in {WINDOW:?}, so the signal would never recover on its own"
        );
        assert!(
            started <= 7,
            "{started} samples started in {WINDOW:?} — an unthrottled 500ms poll would \
             start ~12, so this is not backing off; a `ps` fork twice a second against \
             an already-struggling machine is what issue #862 is about"
        );
        assert_eq!(
            state.read().await.sessions[SESSION].status,
            crate::state::SessionStatus::Working,
            "a sample that produced no usable table says nothing about the pane, so the \
             status must be left exactly as it was — the backoff changes WHEN the next \
             sample runs and nothing about how a missing answer is interpreted"
        );
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "and no event may be synthesized from a sample that produced no table"
        );

        monitor_handle.abort();
        let _ = monitor_handle.await;
        registry.shutdown_all();
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
            background: None,
            background_revision: None,
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
                run_shell_activity_monitor_with(registry, state, event_tx, move |_roots| {
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
            background: None,
            background_revision: None,
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
                command_line: crate::platform::proc::CommandLine::Read("/bin/sh".to_string()),
            },
            crate::platform::proc::ProcessInfo {
                pid: shell_pid + 1,
                ppid: shell_pid,
                session_id: shell_pid + 1,
                has_controlling_tty: false,
                session_leader: true,
                command_line: crate::platform::proc::CommandLine::Read(
                    "detached-thing".to_string(),
                ),
            },
        ];

        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move |_roots| {
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

    /// Await the next "a sample started" signal from a stub sampler (issue
    /// #1133), bounded so a monitor that stops ticking fails with a message
    /// instead of hanging the run.
    ///
    /// On a paused clock the bound costs no real time — the clock advances only
    /// while every task is parked, so reaching 30s there means nothing else in
    /// the runtime was going to happen.
    async fn next_sample_start(rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>, what: &str) {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(())) => {}
            Ok(None) => panic!("the shell-activity monitor stopped before {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
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
    /// The sample lands 2.5s old, inside `MAX_TABLE_AGE` — but if anything pushed
    /// it past that bound, the freshness guard would swallow the whole answer and
    /// B would stay `Idle` for a reason having nothing to do with identity
    /// matching. A reaching `Working` proves the answer was accepted, so B
    /// staying `Idle` can only be the identity filter. (Measured while writing
    /// this: an earlier version closed pane A here, and `close_agent`'s SIGTERM
    /// grace window — `/bin/sh` ignores SIGTERM — blocked the current-thread
    /// runtime long enough that the sample landed at 3.4s and the test passed
    /// entirely via the freshness guard.)
    ///
    /// Issue #1133: that 2.5s is now arithmetic rather than an estimate, and the
    /// scenario is sequenced on observable events rather than on wall-clock
    /// guesses. Two changes, because the test had two separate dependencies on
    /// real time and neither fixes the other:
    ///
    /// - **The stub sampler announces itself.** It sends on `sample_started_rx`
    ///   every time the monitor calls it, which the monitor does only after
    ///   resolving that tick's candidates. Waiting for the first send is what
    ///   makes B "a pane that appeared after the sample started" by construction
    ///   rather than by a 700ms sleep guessing where the tick landed. Waiting for
    ///   the *second* is what makes the read point "after the tick that collected
    ///   the late answer finished classifying": a tick that resumes a retained
    ///   sample starts none of its own, so the next start can only come from the
    ///   tick after it.
    /// - **`start_paused`.** Polling cannot protect `MAX_TABLE_AGE`, because that
    ///   budget is spent by the MONITOR between starting a sample and collecting
    ///   it — under load its own loop drifts and a healthy answer is discarded
    ///   for being collected late. On tokio's virtual clock the budget is
    ///   arithmetic instead: the sample starts at 500ms (one `POLL_INTERVAL`),
    ///   overruns `SAMPLE_TIMEOUT` at 2500ms, and is collected on the next tick
    ///   at 3000ms — 2500ms old, 500ms inside the 3s bound, independent of how
    ///   much real time any of it took. Nothing here needs a real elapsed second:
    ///   the process table is a stub, and the two `/bin/sh` panes exist only to
    ///   supply live pids for it to name. The clock auto-advances while every
    ///   task is parked, so the run also costs milliseconds rather than the 3.5s
    ///   of sleeps it replaces.
    #[tokio::test(start_paused = true)]
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
            background: None,
            background_revision: None,
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

        // One send per sample the monitor STARTS, which is what both waits below
        // are sequenced on. An mpsc rather than a `Notify` because they have to
        // COUNT: `Notify` stores at most one permit, so a second start landing
        // before the test woke would coalesce into the first and the second wait
        // would return without the tick it names having happened.
        let (sample_started_tx, mut sample_started_rx) =
            tokio::sync::mpsc::unbounded_channel::<()>();

        let monitor_handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let late_table = late_table.clone();
            async move {
                run_shell_activity_monitor_with(registry, state, event_tx, move |_roots| {
                    // Sent from the closure body rather than from the future it
                    // returns: the monitor resolves the tick's candidates and
                    // only then calls this, so a send means the candidate set
                    // this sample will be judged against is already fixed — the
                    // exact instant after which a new pane is "late".
                    let _ = sample_started_tx.send(());
                    let late_table = late_table.clone();
                    async move {
                        // Longer than SAMPLE_TIMEOUT (2s) so the sample is
                        // RETAINED rather than answered on its first tick, and
                        // ready by the resumed tick — which lands it 2.5s old,
                        // inside MAX_TABLE_AGE (3s). That is the window where the
                        // freshness bound alone would let it through, so it is
                        // the window the identity filter has to cover. On the
                        // paused clock every figure in that sentence is exact
                        // rather than a target this sleep is aiming at.
                        tokio::time::sleep(Duration::from_millis(2_100)).await;
                        Some(late_table.lock().unwrap().clone())
                    }
                })
                .await
            }
        });

        // Wait for the first tick to resolve its candidates (pane A only) and
        // start the sample, then add pane B while that sample is still in
        // flight. Deliberately no `close_agent` — see this test's doc comment.
        next_sample_start(
            &mut sample_started_rx,
            "the monitor's first tick to start a sample, against pane A alone",
        )
        .await;
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
                    command_line: crate::platform::proc::CommandLine::Read("/bin/sh".to_string()),
                },
                crate::platform::proc::ProcessInfo {
                    pid: pid + 100_000,
                    ppid: pid,
                    session_id: pid + 100_000,
                    has_controlling_tty: false,
                    session_leader: true,
                    command_line: crate::platform::proc::CommandLine::Read(
                        "detached-thing".to_string(),
                    ),
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

        // The tick that resumes a retained sample starts none of its own, so the
        // next sample start is the first observable event that can only happen
        // AFTER the late answer was collected and its whole snapshot applied.
        // That is the honest read point in both directions: were the identity
        // filter gone, B's promotion would already have happened by here, so
        // waiting on an event rather than sleeping does not weaken the test.
        next_sample_start(
            &mut sample_started_rx,
            "a second sample to start — the tick after the one that collected the \
             late answer",
        )
        .await;

        // Stop the monitor before reading, so no later tick can classify B off a
        // sample it was a candidate for from the start.
        monitor_handle.abort();
        let _ = monitor_handle.await;

        let (status_a, status_b) = {
            let guard = state.read().await;
            (
                guard.sessions[SESSION_A].status.clone(),
                guard.sessions[SESSION_B].status.clone(),
            )
        };
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

    /// Scenario: Run the shell-activity monitor with a stub sampler against a real reporting Claude pane whose turn has ended, and have the first sample replace that pane's agent with a successor before it answers busy for the old one. The late sample is dropped under the state lock: nothing is broadcast as busy, and the successor's card keeps its Idle status and no background work.
    #[spec("status/background/009")]
    #[tokio::test(start_paused = true)]
    async fn background_009_a_late_sample_from_a_replaced_spawn_touches_nothing() {
        const PANE: &str = "pane-1678";
        let registry = Arc::new(AgentPtyRegistry::new());
        let spawn = |registry: &Arc<AgentPtyRegistry>| {
            registry
                .spawn_agent(SpawnOptions {
                    command: Some("/bin/sh"),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                    agent_type: Some(AgentType::ClaudeCode),
                    ..SpawnOptions::default()
                })
                .expect("spawn pane agent")
        };
        let old = spawn(&registry);
        let old_pid = registry.child_pid(&old).expect("old spawn pid") as i32;

        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
        state
            .write()
            .await
            .set_agent_ownership(Arc::downgrade(&ownership));
        let hook = |session: &str, agent: &str, event_type| AgentEvent {
            session_id: session.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(PANE.to_string()),
            agent_id: Some(agent.to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
            background: None,
            background_revision: None,
        };
        {
            let mut guard = state.write().await;
            guard.apply_event(hook(
                "sess-old",
                &old,
                crate::event::EventType::SessionStart,
            ));
            guard.apply_event(hook("sess-old", &old, crate::event::EventType::Idle));
        }
        let (event_tx, mut rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let successor: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let monitor = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let successor = successor.clone();
            async move {
                let sampler_registry = registry.clone();
                let sampler_state = state.clone();
                run_shell_activity_monitor_with(registry, state, event_tx, move |_roots| {
                    let _ = started_tx.send(());
                    let first = samples.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    let registry = sampler_registry.clone();
                    let state = sampler_state.clone();
                    let successor = successor.clone();
                    let old = old.clone();
                    async move {
                        let leader = |pid: i32| crate::platform::proc::ProcessInfo {
                            pid,
                            ppid: 1,
                            session_id: pid,
                            has_controlling_tty: true,
                            session_leader: true,
                            command_line: crate::platform::proc::CommandLine::NotSampled,
                        };
                        if !first {
                            // The successor, sampled for itself: idle.
                            let pid = successor
                                .lock()
                                .unwrap()
                                .as_deref()
                                .and_then(|id| registry.child_pid(id))
                                .map_or(0, |pid| pid as i32);
                            return Some(vec![leader(pid)]);
                        }
                        // While this sample is in flight the pane's agent is
                        // replaced and the successor reports its own ended turn.
                        registry.close_agent(&old).expect("close the old spawn");
                        let new = spawn(&registry);
                        {
                            let mut guard = state.write().await;
                            guard.apply_event(hook(
                                "sess-new",
                                &new,
                                crate::event::EventType::SessionStart,
                            ));
                            guard.apply_event(hook(
                                "sess-new",
                                &new,
                                crate::event::EventType::Idle,
                            ));
                        }
                        *successor.lock().unwrap() = Some(new);
                        // ...and the sample answers busy for the OLD spawn, with
                        // the measured Claude fingerprint on its detached shell.
                        Some(vec![
                            leader(old_pid),
                            crate::platform::proc::ProcessInfo {
                                pid: old_pid + 100_000,
                                ppid: old_pid,
                                session_id: old_pid + 100_000,
                                has_controlling_tty: false,
                                session_leader: true,
                                command_line: crate::platform::proc::CommandLine::Read(
                                    "bash -c sleep 9 shell-snapshots/snapshot-x && eval "
                                        .to_string(),
                                ),
                            },
                        ])
                    }
                })
                .await
            }
        });

        // The third sample can only start after the tick that classified the
        // late answer, and the one after it, have finished.
        for what in [
            "the first sample",
            "the successor's sample",
            "a third sample",
        ] {
            next_sample_start(&mut started_rx, what).await;
        }
        monitor.abort();
        let _ = monitor.await;

        let new = successor
            .lock()
            .unwrap()
            .clone()
            .expect("successor spawned");
        while let Ok(msg) = rx.try_recv() {
            if let BroadcastMsg::Event(event) = msg {
                assert_ne!(
                    event.event_type,
                    crate::event::EventType::ShellBusy,
                    "the old spawn's late busy sample must not be broadcast: {event:?}"
                );
                assert!(
                    event.background.is_none_or(|work| work.is_empty()),
                    "no frame may carry the old spawn's command: {event:?}"
                );
            }
        }
        let guard = state.read().await;
        let live = guard
            .live_session_for(&new, Some(PANE))
            .expect("the successor's card");
        assert_eq!(live.status, crate::state::SessionStatus::Idle);
        assert_eq!(
            live.background, None,
            "the successor must not inherit the old spawn's sample"
        );
        drop(guard);
        registry.shutdown_all();
    }

    /// Scenario: Let a reporting pane exit with a background subagent before its first successful command sample, while sampling is unsupported, or during another pane's failure/backoff episode. Also poll while an attested child's publication is paused, with and without Stop: its subagent must survive publication without a retirement broadcast, while a reservation that is actually abandoned and exited spawns lose their work.
    #[spec("status/background/013")]
    #[tokio::test(start_paused = true)]
    async fn background_013_exit_retirement_does_not_depend_on_command_samples() {
        let mut failures = Vec::new();
        for mode in [
            "before-first-sample",
            "unsupported-sampler",
            "failure-backoff",
        ] {
            let registry = Arc::new(AgentPtyRegistry::new());
            let spawn = |pane: &str| {
                registry
                    .spawn_agent(SpawnOptions {
                        command: Some("/bin/sh"),
                        env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane.into())],
                        agent_type: Some(AgentType::ClaudeCode),
                        ..Default::default()
                    })
                    .unwrap()
            };
            let old = spawn("retirement-old");
            let other = (mode != "before-first-sample").then(|| spawn("retirement-live"));
            let state: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
            {
                let mut guard = state.write().await;
                guard.set_agent_ownership(Arc::downgrade(&ownership));
                for (pane, agent) in std::iter::once(("retirement-old", &old))
                    .chain(other.as_ref().map(|id| ("retirement-live", id)))
                {
                    for kind in ["session_start", "subagent_start", "idle"] {
                        let event = serde_json::from_value(serde_json::json!({
                            "session_id": pane, "pane_id": pane, "agent_id": agent,
                            "agent_type": "claude_code", "event_type": kind, "timestamp": chrono::Utc::now(),
                            "metadata": {crate::event::SUBAGENT_ID_METADATA_KEY: "background-child"}
                        })).unwrap();
                        // Only the subagent start is attributed to a child.
                        let mut event: AgentEvent = event;
                        if kind != "subagent_start" {
                            event.metadata.clear();
                        }
                        guard.apply_event(event);
                    }
                    assert_eq!(
                        guard
                            .live_session_for(agent, Some(pane))
                            .unwrap()
                            .background
                            .unwrap()
                            .subagents,
                        1
                    );
                }
            }
            let pids: Vec<i32> = [&old]
                .into_iter()
                .chain(other.iter())
                .map(|id| registry.child_pid(id).unwrap() as i32)
                .collect();
            let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
            let monitor = tokio::spawn({
                let registry = registry.clone();
                let state = state.clone();
                let calls = std::sync::atomic::AtomicUsize::new(0);
                async move {
                    run_shell_activity_monitor_with(registry, state, event_tx, move |_roots| {
                        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                        let _ = started_tx.send(());
                        let table = (mode == "failure-backoff" && first).then(|| {
                            pids.iter()
                                .map(|&pid| crate::platform::proc::ProcessInfo {
                                    pid,
                                    ppid: 1,
                                    session_id: pid,
                                    has_controlling_tty: true,
                                    session_leader: true,
                                    command_line: crate::platform::proc::CommandLine::NotSampled,
                                })
                                .collect()
                        });
                        async move { table }
                    })
                    .await;
                }
            });
            if mode != "before-first-sample" {
                next_sample_start(&mut started_rx, "the initial sample").await;
                if mode == "failure-backoff" {
                    next_sample_start(&mut started_rx, "the failing sample before exit").await;
                }
            }
            // Kill only the test's exact child pid; no SessionEnd hook or pane
            // close is sent, and the old card stays in the state for inspection.
            // The exit watcher uses a real OS thread, so its deadline must use
            // wall time rather than this test's auto-advancing paused clock.
            unsafe {
                libc::kill(registry.child_pid(&old).unwrap() as i32, libc::SIGKILL);
            }
            tokio::task::spawn_blocking({
                let registry = registry.clone();
                let old = old.clone();
                move || {
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while registry.agent_is_live(&old) {
                        assert!(std::time::Instant::now() < deadline, "agent exit deadline");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            })
            .await
            .unwrap();
            assert!(!registry.agent_is_live(&old));
            for _ in 0..8 {
                tokio::time::advance(POLL_INTERVAL).await;
                tokio::task::yield_now().await;
            }
            monitor.abort();
            let _ = monitor.await;
            let guard = state.read().await;
            let live = guard
                .live_session_for(&old, Some("retirement-old"))
                .unwrap();
            if live.background.is_some() {
                failures.push(format!(
                    "{mode}: exited card still shows {:?}",
                    live.background
                ));
            }
            if let Some(other) = other.as_ref() {
                assert_eq!(
                    guard
                        .live_session_for(other, Some("retirement-live"))
                        .unwrap()
                        .background
                        .unwrap()
                        .subagents,
                    1,
                    "unknown samples must preserve the live pane's background"
                );
            }
            drop(guard);
            registry.shutdown_all();
        }
        for ended_before_publication in [false, true] {
            use std::os::unix::fs::PermissionsExt;

            let temp = crate::test_temp::tempdir().unwrap();
            let identity = temp.path().join("identity");
            let script = temp.path().join("pending-worker.sh");
            crate::test_isolation::write_script(&script, "#!/bin/sh\nprintf '%s\\n%s\\n' \"$DOT_AGENT_DECK_AGENT_ID\" \"$DOT_AGENT_DECK_PANE_CAPABILITY\" > \"$BACKGROUND_TEST_IDENTITY\"\nexec /bin/cat\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let registry = Arc::new(AgentPtyRegistry::new());
            let (reached, release) = registry.pause_next_publish_for_test();
            let spawning = tokio::task::spawn_blocking({
                let registry = registry.clone();
                move || {
                    registry.spawn_agent(SpawnOptions {
                        command: Some(script.to_str().unwrap()),
                        env: vec![
                            (DOT_AGENT_DECK_PANE_ID.into(), "pending-background".into()),
                            (
                                "BACKGROUND_TEST_IDENTITY".into(),
                                identity.to_string_lossy().into_owned(),
                            ),
                        ],
                        agent_type: Some(AgentType::ClaudeCode),
                        ..Default::default()
                    })
                }
            });
            tokio::task::spawn_blocking(move || {
                reached
                    .recv_timeout(Duration::from_secs(10))
                    .expect("child forked before publication")
            })
            .await
            .unwrap();
            let identity = temp.path().join("identity");
            let (agent, token) = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    if let Ok(text) = std::fs::read_to_string(&identity) {
                        let lines: Vec<_> = text.lines().collect();
                        if lines.len() == 2 && !lines[0].is_empty() && !lines[1].is_empty() {
                            break (lines[0].to_string(), lines[1].to_string());
                        }
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child identity deadline"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
            .await
            .unwrap();
            assert!(
                registry.hook_token_of(&agent).is_none(),
                "no RunningAgent is published yet"
            );
            let (abandoned, abandoned_token) =
                registry.reserve_spawn_for_test(Some("abandoned-background"));
            let state: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
            state
                .write()
                .await
                .set_agent_ownership(Arc::downgrade(&ownership));
            let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            for (pane, id, capability, ended) in [
                (
                    "pending-background",
                    &agent,
                    &token,
                    ended_before_publication,
                ),
                ("abandoned-background", &abandoned, &abandoned_token, true),
            ] {
                use crate::hook_provenance::{Provenance, classify_event};
                assert!(
                    matches!(
                        classify_event(Some(pane), Some(id), Some(capability), &*registry),
                        Provenance::Attested { .. }
                    ),
                    "reserved child's reports are attested"
                );
                for kind in ["session_start", "subagent_start", "idle"] {
                    if kind == "idle" && !ended {
                        continue;
                    }
                    let mut event: AgentEvent = serde_json::from_value(serde_json::json!({
                        "session_id": pane, "pane_id": pane, "agent_id": id,
                        "agent_type": "claude_code", "event_type": kind, "timestamp": chrono::Utc::now()
                    })).unwrap();
                    if kind == "subagent_start" {
                        event.metadata.insert(
                            crate::event::SUBAGENT_ID_METADATA_KEY.into(),
                            "pending-child".into(),
                        );
                    }
                    ingest_hook_event(&state, &event_tx, &registry, event, false, Some(id.clone()))
                        .await;
                }
            }
            while events.try_recv().is_ok() {}
            let monitor = tokio::spawn(run_shell_activity_monitor_with(
                registry.clone(),
                state.clone(),
                event_tx.clone(),
                |_roots| async { None },
            ));
            for _ in 0..8 {
                tokio::time::advance(POLL_INTERVAL).await;
                tokio::task::yield_now().await;
            }
            while let Ok(BroadcastMsg::Event(event)) = events.try_recv() {
                if event.is_background_retirement() {
                    failures.push(format!("pending Stop={ended_before_publication}: admitted reservation received a premature retirement: agent={:?}, background={:?}, revision={:?}", event.agent_id, event.background, event.background_revision));
                }
            }
            if ended_before_publication {
                let before = state
                    .read()
                    .await
                    .live_session_for(&agent, Some("pending-background"))
                    .unwrap();
                if before.background.as_ref().map(|b| b.subagents) != Some(1) {
                    failures.push(format!(
                        "pending Stop=true: unpublished card lost its subagent: {:?}",
                        before.background
                    ));
                }
            }
            let reserved = state
                .read()
                .await
                .live_session_for(&abandoned, Some("abandoned-background"))
                .unwrap();
            if reserved.background.as_ref().map(|b| b.subagents) != Some(1) {
                failures.push(format!("pending Stop={ended_before_publication}: reservation lost its subagent before abandonment: {:?}", reserved.background));
            }
            release.send(()).unwrap();
            let published = tokio::time::timeout(Duration::from_secs(30), spawning)
                .await
                .expect("publication deadline")
                .unwrap()
                .unwrap();
            assert_eq!(published, agent);
            let idle: AgentEvent = serde_json::from_value(serde_json::json!({
                "session_id": "pending-background", "pane_id": "pending-background", "agent_id": agent,
                "agent_type": "claude_code", "event_type": "idle", "timestamp": chrono::Utc::now()
            })).unwrap();
            ingest_hook_event(
                &state,
                &event_tx,
                &registry,
                idle,
                false,
                Some(agent.clone()),
            )
            .await;
            registry.release_spawn_for_test(&abandoned);
            for _ in 0..8 {
                tokio::time::advance(POLL_INTERVAL).await;
                tokio::task::yield_now().await;
            }
            monitor.abort();
            let _ = monitor.await;
            let mut abandoned_clear = false;
            while let Ok(message) = events.try_recv() {
                if let BroadcastMsg::Event(event) = message
                    && event.is_background_retirement()
                {
                    if event.agent_id.as_deref() == Some(abandoned.as_str()) {
                        abandoned_clear = event
                            .background
                            .as_ref()
                            .is_some_and(|b| b.commands == 0 && b.subagents == 0);
                    } else if event.agent_id.as_deref() == Some(agent.as_str()) {
                        failures.push(format!("pending Stop={ended_before_publication}: published spawn received a retirement"));
                    }
                }
            }
            if !abandoned_clear {
                failures.push(format!("pending Stop={ended_before_publication}: removing the abandoned reservation must emit its explicit-zero clear"));
            }
            let guard = state.read().await;
            let after = guard
                .live_session_for(&agent, Some("pending-background"))
                .unwrap();
            if after.background.as_ref().map(|b| b.subagents) != Some(1) {
                failures.push(format!("pending Stop={ended_before_publication}: published card lost its admitted subagent: {:?}", after.background));
            }
            assert!(
                guard
                    .live_session_for(&abandoned, Some("abandoned-background"))
                    .unwrap()
                    .background
                    .is_none(),
                "an abandoned reservation must retire once removed"
            );
            drop(guard);
            registry.shutdown_all();
        }
        assert!(
            failures.is_empty(),
            "spawn retirement must distinguish pending admission from departure:\n{}",
            failures.join("\n")
        );
    }

    /// Scenario: A background-bearing spawn exits before any successor reserves its pane, and the daemon serializes its retirement. A client hydrates a newer successor before draining that frame without the retirement marker: the frame must make no legacy generation claim, preserve the successor's card, and mint no departed card.
    #[spec("status/background/015")]
    #[tokio::test]
    async fn background_015_retirement_is_safe_for_a_marker_unaware_client() {
        use crate::event::{EventType, GenerationVerdict};
        use crate::state::AppState;

        let pane = "legacy-retirement-pane";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane.into())],
                agent_type: Some(AgentType::ClaudeCode),
                ..Default::default()
            })
            .unwrap();
        let mut daemon = AppState::default();
        let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
        daemon.set_agent_ownership(Arc::downgrade(&ownership));
        for kind in ["session_start", "subagent_start", "idle"] {
            let mut event: AgentEvent = serde_json::from_value(serde_json::json!({
                "session_id": "departed-conversation", "pane_id": pane, "agent_id": agent,
                "agent_type": "claude_code", "event_type": kind,
                "timestamp": chrono::Utc::now() - chrono::Duration::seconds(5)
            }))
            .unwrap();
            if kind == "subagent_start" {
                event.metadata.insert(
                    crate::event::SUBAGENT_ID_METADATA_KEY.into(),
                    "legacy-child".into(),
                );
            }
            daemon.apply_event(event);
        }
        assert_eq!(
            daemon
                .live_session_for(&agent, Some(pane))
                .unwrap()
                .background
                .unwrap()
                .subagents,
            1
        );
        // Retain the exited registry record and reserve no successor until
        // after the retirement is authored, matching the queue/hydration race.
        unsafe {
            libc::kill(registry.child_pid(&agent).unwrap() as i32, libc::SIGKILL);
        }
        tokio::task::spawn_blocking({
            let registry = registry.clone();
            let agent = agent.clone();
            move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while registry.agent_is_live(&agent) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "legacy fixture exit deadline"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })
        .await
        .unwrap();
        let retired = daemon.retire_departed_background_spawns(|pane, agent| {
            registry.agent_is_live_on_pane(agent, pane)
        });
        assert_eq!(retired.len(), 1, "daemon authors exactly one clear");
        let wire = serde_json::to_value(&retired[0]).unwrap();
        let carrier: AgentEvent = serde_json::from_value(wire.clone()).unwrap();
        assert!(carrier.is_background_retirement());
        assert_eq!(carrier.event_type, EventType::ShellIdle);
        assert_eq!(carrier.background.as_ref().unwrap().subagents, 0);
        let mut failures = Vec::new();
        if carrier.pane_generation_verdict() != Some(GenerationVerdict::Displaced) {
            failures.push(format!("serialized retirement must carry the legacy non-claiming Displaced verdict; got {:?}", carrier.pane_generation_verdict()));
        }

        let mut successor = daemon.live_session_for(&agent, Some(pane)).unwrap();
        successor.background = None;
        successor.background_revision = None;
        successor.last_activity_ms = Some(chrono::Utc::now().timestamp_millis());
        // Verified against v0.46.0 src/state.rs:14949-14953 (Displaced
        // admission), 14703-14710 (Current supersedes), 15542-15569
        // (pane retirement), and 16093-16125 (card creation). For this
        // marker-less ShellIdle, the current fold reaches the same generation
        // gates before status/card mutation. Removing the background fields
        // additionally models v0.46's serde ignoring unknown additive fields.
        for ignore_background in [false, true] {
            for shared_key in [false, true] {
                let mut client = AppState::default();
                client.register_pane(pane.into());
                client.seed_hydrated_session(
                    pane.into(),
                    None,
                    None,
                    Some("legacy-successor".into()),
                    Some(&successor),
                );
                let installed_key = client.sessions.keys().next().unwrap().clone();
                let successor_key = if shared_key {
                    "departed-conversation".into()
                } else {
                    installed_key.clone()
                };
                if shared_key {
                    let mut card = client.sessions.remove(&installed_key).unwrap();
                    card.session_id = successor_key.clone();
                    client.sessions.insert(successor_key.clone(), card);
                }
                let before =
                    serde_json::to_value(client.sessions[&successor_key].live_snapshot()).unwrap();
                let mut legacy_wire = wire.clone();
                legacy_wire["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .remove(crate::event::BACKGROUND_RETIRED_METADATA_KEY);
                if ignore_background {
                    legacy_wire.as_object_mut().unwrap().remove("background");
                    legacy_wire
                        .as_object_mut()
                        .unwrap()
                        .remove("background_revision");
                }
                let legacy_carrier: AgentEvent = serde_json::from_value(legacy_wire).unwrap();
                assert!(!legacy_carrier.is_background_retirement());
                // Positive control: v0.46's early Displaced rejection and
                // this tree's must both leave the hydrated successor intact.
                let mut control = client.clone();
                let mut displaced = legacy_carrier.clone();
                displaced.metadata.insert(
                    crate::event::PANE_GENERATION_METADATA_KEY.into(),
                    crate::event::PANE_GENERATION_DISPLACED.into(),
                );
                control.apply_event(displaced);
                assert_eq!(control.sessions.len(), 1);
                assert_eq!(
                    control.sessions[&successor_key].agent_id.as_deref(),
                    Some("legacy-successor")
                );
                client.apply_event(legacy_carrier);
                let kept = client
                    .sessions
                    .get(&successor_key)
                    .filter(|card| card.agent_id.as_deref() == Some("legacy-successor"));
                let minted_departed = client
                    .sessions
                    .values()
                    .any(|card| card.agent_id.as_deref() == Some(agent.as_str()));
                if let Some(kept) = kept.filter(|_| !minted_departed) {
                    assert_eq!(client.sessions.len(), 1);
                    assert_eq!(
                        serde_json::to_value(kept.live_snapshot()).unwrap(),
                        before,
                        "retirement must preserve the successor's live fields"
                    );
                } else {
                    failures.push(format!("marker-unaware replay ignore_background={ignore_background}, shared_key={shared_key}: successor was evicted or departed card minted; cards={:?}", client.sessions.values().map(|card| (&card.session_id, &card.agent_id)).collect::<Vec<_>>()));
                }
            }
        }
        registry.shutdown_all();
        assert!(
            failures.is_empty(),
            "retirement must be safe when the client ignores its marker:\n{}",
            failures.join("\n")
        );
    }

    /// The connecting process `pid` for [`admit_component`], running as the
    /// deck's user or not (`same_user`); its start time is
    /// [`still_the_accepted_peer`]'s business, not this function's.
    fn peer(pid: u32, same_user: Option<bool>) -> Option<AcceptedPeer> {
        Some(AcceptedPeer {
            pid,
            started: None,
            same_user,
        })
    }

    /// PRD #1666 hardening M2, where the deck cannot read the caller's user
    /// (a platform that does not, as on Windows today): a component is
    /// admitted only from the pane's agent process tree, and a receiver only
    /// under an instance that is the caller's own ancestry; where the
    /// platform cannot say, best effort does not check.
    #[test]
    fn without_the_callers_user_the_agent_process_tree_decides() {
        // 1 ── 10 (agent) ── 11 (plugin host) ── 12 (watcher CLI)
        //  └── 20 (unrelated) ── 21
        let parents = |pid: u32| match pid {
            10 | 20 => Some(1),
            11 => Some(10),
            12 => Some(11),
            21 => Some(20),
            _ => None,
        };
        let descends = |pid, root| crate::platform::proc::descends_from_with(pid, root, parents);
        let admit = |peer_pid: u32, claimed, root, used_as, evidence| {
            admit_component(
                peer(peer_pid, None),
                claimed,
                root,
                used_as,
                evidence,
                descends,
                parents,
                |_| None,
            )
        };
        use ComponentUse::{Cli, Receiver};
        use Evidence::{BestEffort, Required};
        assert_eq!(
            admit(12, Some(11), Some(10), Receiver, BestEffort),
            Ok(Some(11))
        );
        assert_eq!(admit(12, None, Some(10), Cli, BestEffort), Ok(None));
        assert_eq!(
            admit(21, None, Some(10), Cli, BestEffort),
            Err(ComponentRefusal::NotMember)
        );
        assert_eq!(
            admit(21, Some(20), Some(10), Receiver, BestEffort),
            Err(ComponentRefusal::NotMember)
        );
        // A receiver names the caller or one of its parents, never a process
        // the caller does not descend from nor one above the agent.
        assert_eq!(
            admit(12, Some(20), Some(10), Receiver, BestEffort),
            Err(ComponentRefusal::NotCaller)
        );
        assert_eq!(
            admit(11, Some(12), Some(10), Receiver, BestEffort),
            Err(ComponentRefusal::NotCaller)
        );
        assert_eq!(
            admit(12, Some(1), Some(10), Receiver, BestEffort),
            Err(ComponentRefusal::AboveAgent)
        );
        assert_eq!(
            admit(12, Some(10), Some(10), Receiver, BestEffort),
            Ok(Some(10))
        );
        // Cannot say: unknown pids, or no agent process to check against.
        assert_eq!(
            admit(99, Some(98), Some(10), Receiver, BestEffort),
            Ok(Some(98))
        );
        assert_eq!(
            admit(21, Some(20), None, Receiver, BestEffort),
            Ok(Some(20))
        );
        // PR #1735 second review: the verbs that take the agent's mail need
        // positive evidence. A peer whose ancestry cannot be read — it exited,
        // as a connection's creator can on purpose — is refused, and so are a
        // missing peer and a pane with no agent process; the agent's own tree
        // is still admitted.
        assert_eq!(admit(12, None, Some(10), Cli, Required), Ok(None));
        assert_eq!(
            admit(12, Some(11), Some(10), Receiver, Required),
            Ok(Some(11))
        );
        assert_eq!(admit(10, None, Some(10), Cli, Required), Ok(None));
        assert_eq!(
            admit(99, None, Some(10), Cli, Required),
            Err(ComponentRefusal::Unidentified)
        );
        assert_eq!(
            admit(12, None, None, Cli, Required),
            Err(ComponentRefusal::NotMember)
        );
        assert_eq!(
            admit(12, Some(98), Some(10), Receiver, Required),
            Err(ComponentRefusal::NotCaller)
        );
        assert_eq!(
            admit_component(
                None,
                None,
                Some(10),
                Cli,
                Required,
                descends,
                parents,
                |_| None
            ),
            Err(ComponentRefusal::Unidentified)
        );
        assert_eq!(
            admit_component(
                None,
                None,
                Some(10),
                Cli,
                BestEffort,
                descends,
                parents,
                |_| None
            ),
            Ok(None)
        );
    }

    /// PRD #1666 part 3 (R-1): a process outside the agent's process tree is
    /// a member of the pane when it runs as the deck's own user, since the
    /// gate ahead of this admitted its token as the pane's current
    /// capability — Claude Code's background session host, whose tool shell
    /// and hooks the deck's recorded agent did not start, is one. It takes
    /// and settles mail on positive evidence, and registers a receiver under
    /// an instance that is its own ancestry; another user outside the tree is
    /// refused under either evidence, even with the token.
    #[test]
    fn a_capability_holder_outside_the_tree_must_be_the_decks_user() {
        // 1 ── 10 (agent, the deck's front end)
        //  └── 30 (background host) ── 31 sh (tool shell) ── 32 CLI
        //  └── 40 (another user's process)
        let parents = |pid: u32| match pid {
            10 | 30 | 40 => Some(1),
            31 => Some(30),
            32 => Some(31),
            _ => None,
        };
        let executables = |pid: u32| {
            Some(std::path::PathBuf::from(match pid {
                10 | 30 => "/opt/claude",
                31 => "/bin/sh",
                32 => "/opt/dot-agent-deck",
                _ => return None,
            }))
        };
        let descends = |pid, root| crate::platform::proc::descends_from_with(pid, root, parents);
        let admit = |peer, claimed, used_as, evidence| {
            admit_component(
                peer,
                claimed,
                Some(10),
                used_as,
                evidence,
                descends,
                parents,
                executables,
            )
        };
        use ComponentUse::{Cli, Receiver};
        use Evidence::{BestEffort, Required};
        for evidence in [Required, BestEffort] {
            assert_eq!(admit(peer(32, Some(true)), None, Cli, evidence), Ok(None));
            // The tool shell's receiver resolves through the shell to the host.
            assert_eq!(
                admit(peer(32, Some(true)), Some(31), Receiver, evidence),
                Ok(Some(30))
            );
            for claimed in [None, Some(30)] {
                let used_as = if claimed.is_some() { Receiver } else { Cli };
                assert_eq!(
                    admit(peer(40, Some(false)), claimed, used_as, evidence),
                    Err(ComponentRefusal::OtherUser),
                    "{evidence:?} {used_as:?}"
                );
                assert_eq!(
                    admit(peer(32, Some(false)), claimed, used_as, evidence),
                    Err(ComponentRefusal::OtherUser),
                    "{evidence:?} {used_as:?}"
                );
            }
            // Its own receiver, not the deck's agent nor another process's.
            assert_eq!(
                admit(peer(32, Some(true)), Some(10), Receiver, evidence),
                Err(ComponentRefusal::NotCaller)
            );
            assert_eq!(
                admit(peer(32, Some(true)), Some(40), Receiver, evidence),
                Err(ComponentRefusal::NotCaller)
            );
            assert_eq!(
                admit(peer(32, Some(true)), Some(1), Receiver, evidence),
                Err(ComponentRefusal::AboveAgent)
            );
        }
        // The agent's own tree stays a member whatever the user reads.
        assert_eq!(admit(peer(10, Some(false)), None, Cli, Required), Ok(None));
        // No root to check against: the user alone decides.
        assert_eq!(
            admit_component(
                peer(32, Some(true)),
                None,
                None,
                Cli,
                Required,
                descends,
                parents,
                executables
            ),
            Ok(None)
        );
    }

    /// PRD #1666 part 3, round 1 (reviewer R1, auditor U2): a capability
    /// holder outside the agent's tree names a shell it shares a branch with
    /// — a sibling of the agent under the terminal's shell, or a shell the
    /// deck itself started — and the walk through shells would climb to the
    /// terminal or the deck, above the agent. The resolved receiver is held to
    /// what a named one is, so it is refused as above the agent; Claude Code's
    /// background host, outside the tree and under init or the deck, still
    /// resolves to itself.
    #[test]
    fn a_shell_outside_the_tree_cannot_resolve_to_a_process_above_the_agent() {
        // 1 ── 5 terminal ── 6 bash ──┬── 10 agent
        //                             └── 30 sh ── 31 CLI (another pane's tool)
        //   └── 2 deck ──┬── 11 agent (a second deck pane)
        //                ├── 20 sh ── 21 CLI
        //                └── 40 claude (re-parented to the deck) ── 41 sh ── 42 CLI
        //   └── 50 claude (re-parented to init) ── 51 sh ── 52 CLI
        let parents = |pid: u32| match pid {
            2 | 5 | 50 => Some(1),
            6 => Some(5),
            10 | 30 => Some(6),
            31 => Some(30),
            11 | 20 | 40 => Some(2),
            21 => Some(20),
            41 => Some(40),
            42 => Some(41),
            51 => Some(50),
            52 => Some(51),
            _ => None,
        };
        let executables = |pid: u32| {
            Some(std::path::PathBuf::from(match pid {
                5 => "/usr/bin/kitty",
                6 => "/bin/bash",
                10 | 11 | 40 | 50 => "/opt/claude",
                2 | 31 | 21 | 42 | 52 => "/opt/dot-agent-deck",
                20 | 30 | 41 | 51 => "/bin/sh",
                _ => return None,
            }))
        };
        let descends = |pid, root| crate::platform::proc::descends_from_with(pid, root, parents);
        let admit = |peer_pid, claimed, root, evidence| {
            admit_component(
                peer(peer_pid, Some(true)),
                claimed,
                Some(root),
                ComponentUse::Receiver,
                evidence,
                descends,
                parents,
                executables,
            )
        };
        let above = |result: Result<Option<u32>, ComponentRefusal>| {
            result.is_err_and(|refusal| refusal.code() == "inbox_receiver_above_agent")
        };
        for evidence in [Evidence::Required, Evidence::BestEffort] {
            // The sibling shell climbs bash 6 to the terminal 5, above agent 10.
            let sibling = admit(31, Some(30), 10, evidence);
            assert!(above(sibling), "{evidence:?}: {sibling:?}");
            // A shell the deck started climbs to the deck, above agent 11.
            let deck = admit(21, Some(20), 11, evidence);
            assert!(above(deck), "{evidence:?}: {deck:?}");
            // Named directly, the processes above the agent are refused the same.
            for claimed in [6, 5, 1] {
                let direct = admit(31, Some(claimed), 10, evidence);
                assert!(above(direct), "{evidence:?} {claimed}: {direct:?}");
            }
            // Claude Code outside the tree resolves to itself, whether it was
            // re-parented to the deck that is above the agent or to init.
            assert_eq!(admit(42, Some(41), 11, evidence), Ok(Some(40)));
            assert_eq!(admit(52, Some(51), 10, evidence), Ok(Some(50)));
            // In the tree the walk still stops at the agent.
            assert_eq!(admit(42, Some(41), 40, evidence), Ok(Some(40)));
        }
        // Where the agent's own ancestry cannot be read, a walk that moved
        // cannot show it stayed below the agent: refused on positive evidence.
        let unreadable = |pid: u32| if pid == 10 { None } else { parents(pid) };
        let without_10 =
            |pid, root| crate::platform::proc::descends_from_with(pid, root, unreadable);
        let resolve = |evidence| {
            admit_component(
                peer(52, Some(true)),
                Some(51),
                Some(10),
                ComponentUse::Receiver,
                evidence,
                without_10,
                unreadable,
                executables,
            )
        };
        assert_eq!(
            resolve(Evidence::Required),
            Err(ComponentRefusal::Unidentified)
        );
        assert_eq!(resolve(Evidence::BestEffort), Ok(Some(50)));
    }

    /// PRD #1666 part 3, round 2 (round 1's U3): a walk from the agent that
    /// runs out of steps before it meets the terminal says nothing about
    /// whether the terminal is above the agent. Under required evidence the
    /// receiver is unidentified, whether a sibling shell resolves to the
    /// terminal or the terminal is named directly; the same tree a few
    /// processes shallower is refused as above the agent.
    #[test]
    fn a_walk_too_deep_to_place_the_receiver_is_not_evidence_it_is_below_the_agent() {
        // 1 ── 5 terminal ── 6 bash ──┬── 1000 ── 1001 ── … ── 10 agent
        //                             └── 30 sh ── 31 CLI
        let tree = |depth: u32| {
            move |pid: u32| match pid {
                5 => Some(1),
                6 => Some(5),
                30 | 1000 => Some(6),
                31 => Some(30),
                10 => Some(1000 + depth - 1),
                pid if (1001..1000 + depth).contains(&pid) => Some(pid - 1),
                _ => None,
            }
        };
        let executables = |pid: u32| {
            Some(std::path::PathBuf::from(match pid {
                5 => "/usr/bin/kitty",
                6 => "/bin/bash",
                10 => "/opt/claude",
                31 => "/opt/dot-agent-deck",
                30 => "/bin/sh",
                _ => "/usr/bin/env",
            }))
        };
        let admit = |depth: u32, claimed, evidence| {
            let parents = tree(depth);
            admit_component(
                peer(31, Some(true)),
                Some(claimed),
                Some(10),
                ComponentUse::Receiver,
                evidence,
                |pid, root| crate::platform::proc::descends_from_with(pid, root, parents),
                parents,
                executables,
            )
        };
        let deep = crate::platform::proc::MAX_ANCESTRY_DEPTH as u32 + 8;
        for claimed in [30, 5] {
            assert_eq!(
                admit(deep, claimed, Evidence::Required),
                Err(ComponentRefusal::Unidentified),
                "{claimed}"
            );
            // The control: shallow enough to read, it is placed above the agent.
            assert_eq!(
                admit(4, claimed, Evidence::Required),
                Err(ComponentRefusal::AboveAgent),
                "{claimed}"
            );
        }
    }

    /// PRD #1666 part 3, round 3: a receiver named directly, not resolved
    /// through a shell, whose place relative to the agent cannot be read
    /// because a parent on the agent's walk is unreadable. Under required
    /// evidence it is unidentified, where it used to be admitted; under best
    /// effort it is still admitted. The terminal is named so the resolved
    /// instance is the named process and only the direct check decides.
    #[test]
    fn a_directly_named_receiver_whose_walk_is_unreadable_is_unidentified() {
        // 1 ── 5 terminal ── 6 bash ── 30 sh ── 31 CLI
        //      ? ── 1000 ── 10 agent   (1000's parent cannot be read)
        let parents = |pid: u32| match pid {
            5 => Some(1),
            6 => Some(5),
            30 => Some(6),
            31 => Some(30),
            10 => Some(1000),
            _ => None,
        };
        let executables = |pid: u32| {
            Some(std::path::PathBuf::from(match pid {
                5 => "/usr/bin/kitty",
                6 => "/bin/bash",
                10 => "/opt/claude",
                30 => "/bin/sh",
                31 => "/opt/dot-agent-deck",
                _ => "/usr/bin/env",
            }))
        };
        let admit = |evidence| {
            admit_component(
                peer(31, Some(true)),
                Some(5),
                Some(10),
                ComponentUse::Receiver,
                evidence,
                |pid, root| crate::platform::proc::descends_from_with(pid, root, parents),
                parents,
                executables,
            )
        };
        assert_eq!(
            crate::platform::proc::ancestry_with(10, 5, parents),
            crate::platform::proc::Ancestry::Unreadable
        );
        assert_eq!(
            admit(Evidence::Required),
            Err(ComponentRefusal::Unidentified)
        );
        assert_eq!(admit(Evidence::BestEffort), Ok(Some(5)));
    }

    /// Every refusal names its own cause, and a fixed message that says what
    /// to do about it and echoes nothing the request carried.
    #[test]
    fn each_component_refusal_has_its_own_code_and_a_remedy() {
        let all = [
            ComponentRefusal::OtherUser,
            ComponentRefusal::NotMember,
            ComponentRefusal::Unidentified,
            ComponentRefusal::NotCaller,
            ComponentRefusal::NestedAgent,
            ComponentRefusal::AboveAgent,
        ];
        let codes: std::collections::HashSet<_> = all.iter().map(|r| r.code()).collect();
        assert_eq!(codes.len(), all.len(), "one code per cause");
        for refusal in all {
            assert!(refusal.code().starts_with("inbox_"), "{refusal:?}");
            assert!(
                refusal.message().contains("; run "),
                "{refusal:?} names a remedy: {}",
                refusal.message()
            );
        }
    }

    /// PR #1735 third review (F2): a connection's creator exited and its pid
    /// was reused by a process inside the agent's tree. The pid now descends
    /// from the agent, but it names a process that started later than the one
    /// accepted, so every inbox verb refuses it — taking mail, posting, arming
    /// and announcing alike. The accepted process itself is still admitted; one
    /// whose start time cannot be read now, or could not be at the accept, is
    /// refused only where positive evidence is required.
    #[test]
    fn a_reused_peer_pid_is_refused_though_it_descends_from_the_agent() {
        // 1 ── 10 (agent) ── 11 (hook shell) ── 12 (hook CLI, started at 2000).
        // The connection was accepted from an earlier 12, started at 1000.
        let lookups = ProcessLookups {
            descends: |pid, root| {
                crate::platform::proc::descends_from_with(pid, root, |pid| match pid {
                    10 => Some(1),
                    11 => Some(10),
                    12 => Some(11),
                    _ => None,
                })
            },
            parent_of: |pid| match pid {
                10 => Some(1),
                11 => Some(10),
                12 => Some(11),
                _ => None,
            },
            executable_of: |_| None,
            started: |pid| match pid {
                10 => Some(100),
                11 => Some(1500),
                12 => Some(2000),
                _ => None,
            },
        };
        let admit = |peer, claimed, used_as, evidence| {
            admit_inbox_component_with(peer, claimed, Some(10), used_as, evidence, &lookups)
        };
        let reused = AcceptedPeer {
            pid: 12,
            started: Some(1000),
            same_user: Some(true),
        };
        let accepted = AcceptedPeer {
            pid: 12,
            started: Some(2000),
            same_user: Some(true),
        };
        let unread_at_accept = AcceptedPeer {
            pid: 12,
            started: None,
            same_user: Some(true),
        };
        for evidence in [Evidence::Required, Evidence::BestEffort] {
            for (used_as, claimed) in [
                (ComponentUse::Cli, None),
                (ComponentUse::Receiver, Some(11)),
            ] {
                assert_eq!(
                    admit(Some(reused), claimed, used_as, evidence),
                    Err(ComponentRefusal::Unidentified),
                    "{used_as:?} {evidence:?}"
                );
                assert_eq!(
                    admit(Some(accepted), claimed, used_as, evidence),
                    Ok(claimed),
                    "{used_as:?} {evidence:?}"
                );
            }
        }
        assert!(
            admit(
                Some(unread_at_accept),
                None,
                ComponentUse::Cli,
                Evidence::Required
            )
            .is_err()
        );
        assert!(
            admit(
                Some(unread_at_accept),
                None,
                ComponentUse::Cli,
                Evidence::BestEffort
            )
            .is_ok()
        );
        let gone = AcceptedPeer {
            pid: 12,
            started: Some(2000),
            same_user: Some(true),
        };
        let without_12 = ProcessLookups {
            started: |pid| (pid != 12).then_some(1),
            ..lookups
        };
        assert!(
            admit_inbox_component_with(
                Some(gone),
                None,
                Some(10),
                ComponentUse::Cli,
                Evidence::Required,
                &without_12
            )
            .is_err(),
            "a start time that cannot be read now refuses where evidence is required"
        );
    }

    /// The real process table: this test process reads its own start time, the
    /// same each time, on a platform that has one.
    #[test]
    fn a_process_start_time_is_stable_for_one_process() {
        let me = std::process::id();
        let first = crate::platform::proc::process_start_of(me);
        assert_eq!(first, crate::platform::proc::process_start_of(me));
        if crate::platform::proc::ANCESTRY_LOOKUP {
            assert!(first.is_some(), "this platform reads start times");
        }
    }

    /// Audit F3 (b): a component registering the agent's native receiver is
    /// refused when a process between it and the instance it names runs that
    /// instance's executable — an agent of the same kind, started inside the
    /// one whose receiver it claims to be. The launch shapes the deck's own
    /// integrations use (the watcher a direct child of its agent, a hook run
    /// through a shell, an agent under a wrapper of its own kind) still pass,
    /// and so does a walk that cannot say. A hook run through a shell is
    /// pinned by the agent above the shell, so every hook of one agent names
    /// the same receiver.
    #[test]
    fn a_nested_agent_of_the_same_kind_cannot_name_the_outer_agent_as_its_receiver() {
        // 1 ── 10 claude ── 11 bash (a tool) ── 12 claude (nested) ── 13 sh ── 14 CLI
        //   │            │                 └── 19 CLI (a waiter the tool runs)
        //   │            ├── 15 sh ── 16 CLI (the outer agent's hook)
        //   │            └── 17 sh ── 18 CLI (its next hook, in a new shell)
        //   └── 20 node (npx) ── 21 sh ── 22 node (pi) ── 23 CLI (its watcher)
        let parents = |pid: u32| match pid {
            10 | 20 => Some(1),
            11 | 15 | 17 => Some(10),
            12 | 19 => Some(11),
            13 => Some(12),
            14 => Some(13),
            16 => Some(15),
            18 => Some(17),
            21 => Some(20),
            22 => Some(21),
            23 => Some(22),
            _ => None,
        };
        let executables = |pid: u32| {
            let exe = match pid {
                10 | 12 => "/opt/claude",
                11 => "/bin/bash",
                13 | 15 | 17 | 21 => "/bin/sh",
                14 | 16 | 18 | 19 | 23 => "/opt/dot-agent-deck",
                20 | 22 => "/usr/bin/node",
                _ => return None,
            };
            Some(std::path::PathBuf::from(exe))
        };
        let descends = |pid, root| crate::platform::proc::descends_from_with(pid, root, parents);
        let receiver =
            |peer_pid: Option<u32>,
             claimed,
             root,
             executables: &dyn Fn(u32) -> Option<std::path::PathBuf>| {
                admit_component(
                    peer_pid.and_then(|pid| peer(pid, None)),
                    claimed,
                    root,
                    ComponentUse::Receiver,
                    Evidence::BestEffort,
                    descends,
                    parents,
                    executables,
                )
            };
        let admit = |peer, claimed, root| receiver(peer, claimed, root, &executables);

        // The outer agent's hooks, each through its own shell, naming the
        // shell or the agent: all are the agent's one receiver.
        assert_eq!(admit(Some(16), Some(15), Some(10)), Ok(Some(10)));
        assert_eq!(admit(Some(18), Some(17), Some(10)), Ok(Some(10)));
        assert_eq!(admit(Some(16), Some(10), Some(10)), Ok(Some(10)));
        // Pi's watcher, a direct child of Pi under an npx wrapper that runs node too.
        assert_eq!(admit(Some(23), Some(22), Some(20)), Ok(Some(22)));
        // The nested agent of the same kind naming the outer agent as its
        // instance is refused.
        assert_eq!(
            admit(Some(14), Some(10), Some(10)),
            Err(ComponentRefusal::NestedAgent)
        );
        // Naming its own shell, or itself, it is the nested agent's own
        // instance, which the pin refuses once the outer agent has armed or
        // announced (`InboxHub::arm`).
        assert_eq!(admit(Some(14), Some(13), Some(10)), Ok(Some(12)));
        assert_eq!(admit(Some(14), Some(12), Some(10)), Ok(Some(12)));
        // Audit D1, the boundary: a waiter the agent's tool shell runs resolves
        // to the agent, the same receiver as its hooks.
        assert_eq!(admit(Some(19), Some(11), Some(10)), Ok(Some(10)));
        // A walk that cannot say does not refuse, and pins what was named.
        assert_eq!(
            receiver(Some(14), Some(10), Some(10), &|_| None),
            Ok(Some(10))
        );
        assert_eq!(
            receiver(Some(16), Some(15), Some(10), &|_| None),
            Ok(Some(15))
        );
    }

    /// Review nit 1: a chain of shells deeper than the walk reads, below the
    /// agent, pins what was named — as a walk that cannot say does — rather
    /// than whichever shell the walk stopped on.
    #[test]
    fn a_shell_chain_deeper_than_the_walk_pins_the_named_instance() {
        // 1 ── 10 agent ── 11 sh ── 12 sh ── … ── 30 sh ── 31 CLI
        let parents = |pid: u32| match pid {
            10 => Some(1),
            11..=31 => Some(pid - 1),
            _ => None,
        };
        let executables = |pid: u32| {
            Some(std::path::PathBuf::from(match pid {
                10 => "/opt/agent",
                11..=30 => "/bin/sh",
                31 => "/opt/dot-agent-deck",
                _ => return None,
            }))
        };
        assert_eq!(receiver_instance(30, Some(10), parents, executables), 30);
        assert_eq!(receiver_instance(12, Some(10), parents, executables), 10);
    }

    /// Audit F3 (b), on this machine's real process table: a shell started
    /// inside a shell, with a process below it, is seen as nested between that
    /// process and the outer shell, and not between it and its own parent.
    #[cfg(target_os = "linux")]
    #[test]
    fn nested_between_reads_the_real_process_table() {
        use std::io::BufRead;
        let mut outer = std::process::Command::new("sh")
            .arg("-c")
            .arg("sh -c 'sleep 30 & echo $!; wait'; :")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start the outer shell");
        let mut line = String::new();
        std::io::BufReader::new(outer.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let caller: u32 = line
            .trim()
            .parse()
            .expect("the nested shell names its child");
        let parent = crate::platform::proc::parent_pid_of;
        let nested = parent(caller).expect("the caller's parent");
        assert_eq!(parent(nested), Some(outer.id()));
        let seen = |claimed| {
            nested_between(
                caller,
                claimed,
                parent,
                crate::platform::proc::executable_of,
            )
        };
        assert_eq!(seen(outer.id()), Some(true));
        assert_eq!(seen(nested), Some(false));
        // SAFETY: `caller` is the `sleep` this test started, still running.
        unsafe { libc::kill(caller as libc::pid_t, libc::SIGKILL) };
        let _ = outer.wait();
    }

    /// PRD #1666 part 3 (H5): `get-seed` takes nothing for any caller — not
    /// for the attested live agent either — and answers each with no seed
    /// and the withdrawal; the message stays queued with no hand-over
    /// counted.
    #[test]
    fn get_seed_takes_nothing_for_any_caller() {
        let registry = AgentPtyRegistry::new();
        let hub = Arc::clone(registry.inbox());
        let id = hub
            .post_direct(
                crate::inbox::Recipient::bound("p", "new"),
                crate::inbox::Content::from_deck("seed", "task", "the successor's task"),
            )
            .unwrap();
        for (attested, live) in [
            (Some("old"), Some("new")),
            (None, Some("new")),
            (Some("new"), Some("newer")),
            (Some("new"), Some("new")),
        ] {
            let answer = get_seed_withdrawn(&registry, "p", attested, live);
            assert!(answer.seed.is_none(), "{attested:?} {live:?}");
            assert_eq!(
                answer.refused.as_deref(),
                Some(crate::event::GET_SEED_REMOVED),
                "{attested:?} {live:?}"
            );
            let listed = hub.list("p").into_iter().find(|m| m.id == id).unwrap();
            assert_eq!(
                (listed.state, listed.deliveries),
                (crate::inbox::StateName::Queued, 0),
                "{attested:?} {live:?}"
            );
        }
        assert_eq!(hub.integration_notice("p", "new"), None);
    }

    /// Scenario: A Pi generation whose extension predates the receipt stream holds a queued deck message. Its stream without the deck's integration, its plain doorbell fetch and its get-seed are each refused as an out-of-date adapter: the message stays queued and the card says to restart the agent. An OpenCode generation's stream is refused the same way, and a Claude hook's plain doorbell fetch is no adapter's: it meets the hooks' one-shot floor, and the legacy fetch takes nothing.
    #[spec("inbox/boundary/030")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boundary_030_a_pi_extension_from_before_the_receipt_stream_is_refused() {
        use crate::event::{InboxComponent, InboxWaitMode};
        use crate::inbox_display::IntegrationNotice;
        use tokio::io::{AsyncBufReadExt, BufReader};
        let registry = AgentPtyRegistry::new();
        let hub = Arc::clone(registry.inbox());
        let queued = |pane: &str, id: &crate::inbox::MessageId| {
            hub.list(pane)
                .iter()
                .any(|m| m.id == *id && m.state == crate::inbox::StateName::Queued)
        };
        let post = |pane: &str, agent: &str| {
            hub.post_direct(
                crate::inbox::Recipient::bound(pane, agent),
                crate::inbox::Content::from_deck("seed", "task", "the task"),
            )
            .unwrap()
        };
        // An old stream, for Pi and for OpenCode.
        for (pane, component) in [
            ("pi", InboxComponent::PiExtension),
            ("oc", InboxComponent::OpenCodePlugin),
        ] {
            let id = post(pane, "a");
            let (client, server) = tokio::io::duplex(1 << 16);
            let (read, write) = tokio::io::split(server);
            let serve = tokio::spawn(crate::inbox_hub::serve_waiter(
                Arc::clone(&hub),
                pane.to_string(),
                "a".to_string(),
                InboxWaitMode::Stream,
                component,
                None,
                crate::inbox_hub::ReceiptArm::default(),
                BufReader::new(read),
                write,
                |_: &str| {},
            ));
            let mut lines = BufReader::new(client).lines();
            let reply: crate::event::InboxWaitReply =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(reply.refused.as_deref(), Some("adapter_too_old"), "{pane}");
            serve.await.unwrap();
            assert!(queued(pane, &id), "{pane}: the message stays queued");
            assert_eq!(
                hub.integration_notice(pane, "a"),
                Some(IntegrationNotice::AdapterTooOld),
                "{pane}"
            );
        }
        // A plain doorbell fetch and get-seed from a Pi generation.
        hub.announce(
            "p",
            "a",
            crate::event::InboxWake::Native,
            InboxComponent::PiExtension,
            None,
        )
        .unwrap();
        let id = post("p", "a");
        let fetch = |pane: &str| -> crate::event::InboxFetchRequest {
            serde_json::from_value(serde_json::json!({
                "pane_id": pane, "consumer": "doorbell_hook",
                "doorbell": id.as_str(), "max_chars": 65536
            }))
            .unwrap()
        };
        let reply = handle_inbox_fetch(&registry, &fetch("p"), "a");
        assert_eq!(reply.refused.as_deref(), Some("adapter_too_old"));
        assert!(
            get_seed_withdrawn(&registry, "p", Some("a"), Some("a"))
                .seed
                .is_none()
        );
        assert!(queued("p", &id), "no seed, no fetch: it stays queued");
        assert_eq!(
            hub.integration_notice("p", "a"),
            Some(IntegrationNotice::AdapterTooOld)
        );
        // A Claude hook's plain doorbell fetch is no adapter's: it meets the
        // one-shot floor as Claude's (PRD #1666 part 3, H5), never the
        // legacy fetch.
        hub.announce(
            "c",
            "a",
            crate::event::InboxWake::Native,
            InboxComponent::ClaudeHooks,
            None,
        )
        .unwrap();
        let claude = post("c", "a");
        let mut req = fetch("c");
        req.doorbell = Some(claude.as_str().to_string());
        assert!(!old_adapter_doorbell(&registry, &req, "a"));
        assert!(legacy_verb_hook_fetch(&registry, &req, "a"));
        assert_eq!(
            legacy_hook_component(&registry, &req, "a"),
            InboxComponent::ClaudeHooks
        );
        assert!(!legacy_verb_hook_fetch(&registry, &fetch("p"), "a"));
        let reply = handle_inbox_fetch(&registry, &req, "a");
        assert!(reply.messages.is_none());
        assert!(queued("c", &claude), "the legacy fetch takes nothing");
    }

    // ---------------------------------------------------------------------
    // Issue #1077: the hook-socket provenance gate, driven through the REAL
    // `run_hook_loop` over a REAL socket against REAL spawned agents.
    //
    // These are the tests of the gate as it actually sits: the decision matrix
    // itself is unit-tested in `crate::hook_provenance`, but only here is the
    // wiring exercised — that the daemon reads the token off the message, looks
    // it up in the live registry, refuses on the connection, and does not run
    // the handler.
    // ---------------------------------------------------------------------

    /// One two-role orchestration behind a live hook loop: an orchestrator pane
    /// and a worker pane, both `cat`, with the role maps the delegate path
    /// routes on.
    struct ProvenanceFixture {
        /// The directory both panes run in, and so the one whose
        /// `.dot-agent-deck.toml` every orchestration verb resolves roles from.
        cwd: tempfile::TempDir,
        _dir: tempfile::TempDir,
        sock: std::path::PathBuf,
        registry: Arc<AgentPtyRegistry>,
        orchestrator_token: String,
        worker_token: String,
        worker_agent: String,
        orchestrator_agent: String,
        /// The loop's own state, so a test can watch hook ingestion land.
        state: SharedState,
        events: broadcast::Receiver<crate::event::BroadcastMsg>,
        handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
    }

    const PROV_ORCH_PANE: &str = "prov-orchestrator-pane";
    const PROV_WORKER_PANE: &str = "prov-worker-pane";

    /// Held by every test here that sets `DOT_AGENT_DECK_HOOK_PROVENANCE` and
    /// by every one that relies on it being unset (enforce): the hook loop
    /// reads it per message, and `cargo test` runs these on shared threads.
    static HOOK_PROVENANCE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Sets `DOT_AGENT_DECK_HOOK_PROVENANCE` for one test and restores what was
    /// there. Hold [`HOOK_PROVENANCE_ENV_LOCK`] for its whole life.
    struct HookProvenanceEnv(Option<std::ffi::OsString>);

    impl HookProvenanceEnv {
        fn set(value: &str) -> Self {
            let var = crate::hook_provenance::DOT_AGENT_DECK_HOOK_PROVENANCE;
            let previous = std::env::var_os(var);
            // SAFETY: every test that reads or writes this variable holds
            // `HOOK_PROVENANCE_ENV_LOCK`.
            unsafe { std::env::set_var(var, value) };
            Self(previous)
        }
    }

    impl Drop for HookProvenanceEnv {
        fn drop(&mut self) {
            let var = crate::hook_provenance::DOT_AGENT_DECK_HOOK_PROVENANCE;
            // SAFETY: as in `set`.
            match self.0.take() {
                Some(previous) => unsafe { std::env::set_var(var, previous) },
                None => unsafe { std::env::remove_var(var) },
            }
        }
    }

    impl ProvenanceFixture {
        async fn start() -> Self {
            use crate::state::OrchestrationIdentity;

            let cwd = tempfile::tempdir().unwrap();
            let cwd_str = cwd.path().to_string_lossy().into_owned();
            let registry = Arc::new(AgentPtyRegistry::new());
            let orchestrator_agent = registry
                .spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    cwd: Some(&cwd_str),
                    env: vec![(
                        DOT_AGENT_DECK_PANE_ID.to_string(),
                        PROV_ORCH_PANE.to_string(),
                    )],
                    ..SpawnOptions::default()
                })
                .expect("spawn orchestrator stub");
            let worker_agent = registry
                .spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    cwd: Some(&cwd_str),
                    env: vec![(
                        DOT_AGENT_DECK_PANE_ID.to_string(),
                        PROV_WORKER_PANE.to_string(),
                    )],
                    ..SpawnOptions::default()
                })
                .expect("spawn worker stub");

            let orchestration = OrchestrationIdentity {
                id: "prov-instance".to_string(),
                name: "prov-orchestration".to_string(),
            };
            let mut app = crate::state::AppState::default();
            let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
            app.set_agent_ownership(Arc::downgrade(&ownership));
            for (pane, role, is_orch) in [
                (PROV_ORCH_PANE, "orchestrator", true),
                (PROV_WORKER_PANE, "worker", false),
            ] {
                app.register_pane(pane.to_string());
                app.pane_role_map.insert(pane.to_string(), role.to_string());
                app.pane_orchestration_map
                    .insert(pane.to_string(), orchestration.clone());
                app.pane_cwd_map.insert(pane.to_string(), cwd_str.clone());
                if is_orch {
                    app.orchestrator_pane_ids.insert(pane.to_string());
                }
            }

            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod tempdir");
            // The retained store the daemon opens at its start: a report is
            // accepted only once kept there (PRD #1666 part 3, round 5).
            std::fs::create_dir_all(dir.path().join("state")).expect("state dir");
            registry
                .inbox()
                .set_retained_store(crate::retained_store::RetainedStore::open(
                    &dir.path().join("state"),
                    crate::retained_store::StoreConfig::default(),
                    crate::retained_store::Now::current(),
                ));
            let sock = dir.path().join("hook.sock");
            let listener = IpcListener::from_tokio_listener(
                UnixListener::bind(&sock).expect("bind hook socket"),
            );
            let state: SharedState = Arc::new(tokio::sync::RwLock::new(app));
            let (event_tx, events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let shutdown = Arc::new(Notify::new());
            let handle = tokio::spawn({
                let registry = registry.clone();
                let state = state.clone();
                let wtr = crate::issue_dispatch_run::new_worktree_registry();
                async move { run_hook_loop(listener, state, event_tx, registry, shutdown, wtr).await }
            });

            Self {
                orchestrator_token: registry
                    .hook_token_of(&orchestrator_agent)
                    .expect("orchestrator token"),
                worker_token: registry.hook_token_of(&worker_agent).expect("worker token"),
                worker_agent,
                orchestrator_agent,
                registry,
                sock,
                cwd,
                _dir: dir,
                state,
                events,
                handle,
            }
        }

        /// Half-close and read to EOF: the loop has processed the line when this
        /// returns, so absence assertions need no arbitrary sleep. Raw events
        /// remain fire-and-forget and must produce no response bytes.
        async fn agent_event(&self, pane: &str, session: &str, kind: &str, token: Option<&str>) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut payload = serde_json::json!({
                "session_id": session,
                "pane_id": pane,
                "agent_id": if pane == PROV_ORCH_PANE { self.orchestrator_agent.as_str() } else { "foreign-agent" },
                "agent_type": "pi",
                "event_type": kind,
                "timestamp": chrono::Utc::now(),
            });
            if let Some(token) = token {
                payload["token"] = serde_json::Value::String(token.to_string());
            }
            let mut stream = UnixStream::connect(&self.sock).await.expect("connect");
            stream
                .write_all(format!("{payload}\n").as_bytes())
                .await
                .expect("write event");
            stream.shutdown().await.expect("half-close");
            let mut reply = String::new();
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut reply))
                .await
                .expect("event connection completed")
                .expect("read EOF");
            assert!(reply.is_empty(), "AgentEvent must remain silent: {reply}");
        }

        fn next_event(&mut self) -> crate::event::AgentEvent {
            let BroadcastMsg::Event(event) = self.events.try_recv().expect("event broadcast")
            else {
                panic!("expected a raw event broadcast");
            };
            event
        }

        /// Send one `delegate` line and read the daemon's reply.
        async fn delegate(
            &self,
            claimed_pane: &str,
            token: Option<&str>,
        ) -> crate::event::DelegateResponse {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let msg = crate::event::DaemonMessage::Delegate(crate::event::DelegateSignal {
                pane_id: claimed_pane.to_string(),
                task: "PROVENANCE-TASK".to_string(),
                to: vec!["worker".to_string()],
                supersede: false,
                timestamp: chrono::Utc::now(),
                token: token.map(str::to_string),
            });
            let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
            let mut stream = UnixStream::connect(&self.sock).await.expect("connect");
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
            let mut buf = String::new();
            stream.read_to_string(&mut buf).await.unwrap();
            serde_json::from_str(buf.trim()).unwrap_or_else(|e| {
                panic!("delegate reply was not a DelegateResponse ({e}): {buf:?}")
            })
        }

        /// Send one `work_done` line from the WORKER's pane and read the
        /// daemon's acknowledgement (issue #1129).
        ///
        /// Deliberately reads the connection the same way [`Self::delegate`]
        /// does — to EOF after a half-close — so "the daemon wrote nothing" is a
        /// result this helper can return rather than a hang.
        async fn work_done(
            &self,
            claimed_pane: &str,
            token: Option<&str>,
            report: &str,
        ) -> Option<crate::event::SignalAck> {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let msg = crate::event::DaemonMessage::WorkDone(crate::event::WorkDoneSignal {
                pane_id: claimed_pane.to_string(),
                task: report.to_string(),
                done: false,
                timestamp: chrono::Utc::now(),
                token: token.map(str::to_string),
            });
            let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
            let mut stream = UnixStream::connect(&self.sock).await.expect("connect");
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
            let mut buf = String::new();
            stream.read_to_string(&mut buf).await.unwrap();
            if buf.trim().is_empty() {
                return None;
            }
            Some(
                serde_json::from_str(buf.trim()).unwrap_or_else(|e| {
                    panic!("work_done reply was not a SignalAck ({e}): {buf:?}")
                }),
            )
        }

        /// Whether the ORCHESTRATOR's PTY has seen `needle` yet, polled for
        /// `budget`. `handle_work_done` writes its feedback there, so this is
        /// how a work-done that ran is told from one that was refused.
        /// PRD #1666 S5: whether `needle` reached the orchestrator — in its
        /// inbox, where the daemon now posts, or typed into its PTY, which it no
        /// longer does — polled for `budget`.
        async fn orchestrator_saw(&self, needle: &str, budget: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + budget;
            loop {
                if pane_inbox_contains(&self.registry, PROV_ORCH_PANE, needle) {
                    return true;
                }
                if let Ok(bytes) = self.registry.snapshot(&self.orchestrator_agent)
                    && String::from_utf8_lossy(&bytes).contains(needle)
                {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        /// Whether the worker has been handed the delegated task yet, polled
        /// for `budget`. PRD #1666 S5: `handle_delegate` posts the task to the
        /// worker's inbox, so its presence there (or, as before, in the PTY) is
        /// proof the handler ran and its absence (after a wait) is proof it did
        /// not.
        async fn worker_saw_pointer(&self, budget: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + budget;
            loop {
                if pane_inbox_contains(&self.registry, PROV_WORKER_PANE, "worker-task-worker.md")
                    || pane_inbox_contains(
                        &self.registry,
                        PROV_WORKER_PANE,
                        "from the orchestrator]",
                    )
                {
                    return true;
                }
                if let Ok(bytes) = self.registry.snapshot(&self.worker_agent)
                    && String::from_utf8_lossy(&bytes).contains("worker-task-worker.md")
                {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        async fn stop(self) {
            self.handle.abort();
            let _ = self.handle.await;
            self.registry.shutdown_all();
        }
    }

    /// PRD #1666 S5: whether any message in `pane_id`'s inbox, rendered whole,
    /// contains `needle`.
    /// PRD #1666 part 3, round 5: a hook connection carrying a task-scoped
    /// `work-done`, as the CLI sends it: the capabilities question, its
    /// answer, then the report.
    struct TaskReport {
        reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
        _writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl TaskReport {
        async fn send(fx: &ProvenanceFixture, task_id: &str, report: &str) -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let stream = UnixStream::connect(&fx.sock).await.expect("connect");
            let (read, mut write) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(read);
            let token = Some(fx.worker_token.clone());
            let question = crate::event::DaemonMessage::HookCapabilities(
                crate::event::HookCapabilitiesRequest {
                    pane_id: PROV_WORKER_PANE.to_string(),
                    token: token.clone(),
                },
            );
            write
                .write_all(format!("{}\n", serde_json::to_string(&question).unwrap()).as_bytes())
                .await
                .unwrap();
            let mut answer = String::new();
            reader.read_line(&mut answer).await.unwrap();
            assert_eq!(
                crate::hook::confirms_task_scoped_completion(&answer),
                Ok(()),
                "{answer:?}"
            );
            let mutation =
                crate::event::DaemonMessage::WorkDoneTask(crate::event::WorkDoneTaskRequest {
                    pane_id: PROV_WORKER_PANE.to_string(),
                    token,
                    mode: crate::event::WorkDoneMode::Task {
                        task_id: task_id.to_string(),
                    },
                    report: report.to_string(),
                    timestamp: chrono::Utc::now(),
                });
            write
                .write_all(format!("{}\n", serde_json::to_string(&mutation).unwrap()).as_bytes())
                .await
                .unwrap();
            TaskReport {
                reader,
                _writer: write,
            }
        }

        /// The next line, if one arrives within `budget`.
        async fn line_within(&mut self, budget: Duration) -> Option<String> {
            use tokio::io::AsyncBufReadExt;
            let mut line = String::new();
            match tokio::time::timeout(budget, self.reader.read_line(&mut line)).await {
                Ok(Ok(n)) if n > 0 => Some(line),
                _ => None,
            }
        }
    }

    /// Delegate the fixture's task and answer its id, once posted.
    async fn delegated_task(fx: &ProvenanceFixture) -> String {
        let response = fx
            .delegate(PROV_ORCH_PANE, Some(&fx.orchestrator_token))
            .await;
        assert!(response.error.is_none(), "{response:?}");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(task) = fx.registry.inbox().open_tasks_of(PROV_WORKER_PANE).pop() {
                return task;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the task was never posted"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Send a legacy `work_done` with no token and read its acknowledgement.
    async fn legacy_unattested(fx: &ProvenanceFixture, done: bool) -> crate::event::SignalAck {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let msg = DaemonMessage::WorkDone(crate::event::WorkDoneSignal {
            pane_id: PROV_WORKER_PANE.to_string(),
            task: "LEGACY-033".to_string(),
            done,
            timestamp: chrono::Utc::now(),
            token: None,
        });
        let mut stream = UnixStream::connect(&fx.sock).await.expect("connect");
        stream
            .write_all(format!("{}\n", serde_json::to_string(&msg).unwrap()).as_bytes())
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await.unwrap();
        serde_json::from_str(buf.trim()).unwrap_or_else(|e| panic!("{e}: {buf:?}"))
    }

    /// Scenario: Under the `warn` escape hatch, an older CLI's token-less `work-done --done` is refused `unattested` and changes nothing; its token-less report without `--done` is delivered as a note that completes nothing, so the worker's task stays open and its commission owed.
    #[spec("inbox/protocol/033")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn protocol_033_the_legacy_verb_is_gated_by_what_it_reports() {
        let _lock = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        let _env = HookProvenanceEnv::set("warn");
        let fx = ProvenanceFixture::start().await;
        let task = delegated_task(&fx).await;
        let refused = legacy_unattested(&fx, true).await;
        assert!(!refused.accepted, "{refused:?}");
        assert_eq!(refused.reason.as_deref(), Some("unattested"), "{refused:?}");
        assert_eq!(
            fx.registry.inbox().open_tasks_of(PROV_WORKER_PANE),
            vec![task.clone()]
        );
        let note = legacy_unattested(&fx, false).await;
        assert!(note.accepted, "{note:?}");
        assert!(
            fx.orchestrator_saw("LEGACY-033", Duration::from_secs(10))
                .await
        );
        assert_eq!(
            fx.registry.inbox().open_tasks_of(PROV_WORKER_PANE),
            vec![task]
        );
        assert!(fx.registry.owes_delegation_commission(PROV_WORKER_PANE));
        fx.stop().await;
    }

    /// Scenario: A worker's task report over the real hook socket. Paused after its save, no byte is readable on the connection for 300 ms; a save failure injected meanwhile makes the only line `report_save_failed`. Paused at `work-done-committed`, no byte is readable while the report is already in the orchestrator's inbox and the task already retired; released, exactly one line, `accepted`.
    #[spec("orchestration/work-done/038")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn work_done_038_the_answer_is_written_only_after_the_commit() {
        let fx = ProvenanceFixture::start().await;
        let task = delegated_task(&fx).await;

        let (reached, release) = fx
            .registry
            .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
        let mut report = TaskReport::send(&fx, &task, "first try").await;
        reached.await.unwrap();
        assert_eq!(report.line_within(Duration::from_millis(300)).await, None);
        fx.registry.inject_fault_for_test(
            crate::task_completion::FAULT_FAIL_SAVE,
            crate::agent_pty::TestFault::Fail,
        );
        release.send(()).unwrap();
        let line = report
            .line_within(Duration::from_secs(10))
            .await
            .expect("a line");
        let refused: crate::event::WorkDoneTaskReply = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            refused.code.as_deref(),
            Some("report_save_failed"),
            "{line}"
        );

        let (reached, release) = fx
            .registry
            .pause_at_for_test(crate::task_completion::PAUSE_COMMITTED);
        let mut report = TaskReport::send(&fx, &task, "WORK-DONE-038-REPORT").await;
        reached.await.unwrap();
        assert_eq!(report.line_within(Duration::from_millis(300)).await, None);
        assert!(pane_inbox_contains(
            &fx.registry,
            PROV_ORCH_PANE,
            "WORK-DONE-038-REPORT"
        ));
        assert!(
            fx.registry
                .inbox()
                .open_tasks_of(PROV_WORKER_PANE)
                .is_empty()
        );
        release.send(()).unwrap();
        let line = report
            .line_within(Duration::from_secs(10))
            .await
            .expect("a line");
        let accepted: crate::event::WorkDoneTaskReply = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            accepted.disposition,
            crate::event::WorkDoneDisposition::Accepted,
            "{line}"
        );
        assert_eq!(
            report.line_within(Duration::from_millis(200)).await,
            None,
            "one line"
        );
        fx.stop().await;
    }

    /// Scenario: Paused right after the answer's line is written: the line is readable, and by then the task's commission arm and delivery watch are already gone and the task is reported. The answer never precedes any part of the commit.
    #[spec("orchestration/work-done/039")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn work_done_039_the_answer_follows_every_part_of_the_commit() {
        let fx = ProvenanceFixture::start().await;
        let task = delegated_task(&fx).await;
        let (reached, release) = fx
            .registry
            .pause_at_for_test(crate::task_completion::PAUSE_ACKED);
        let mut report = TaskReport::send(&fx, &task, "done").await;
        reached.await.unwrap();
        let line = report
            .line_within(Duration::from_secs(5))
            .await
            .expect("written");
        assert!(line.contains("accepted"), "{line}");
        assert!(!fx.registry.owes_delegation_commission(PROV_WORKER_PANE));
        assert!(!fx.registry.inbox().watched_tasks_for_test().contains(&task));
        assert!(
            fx.registry
                .inbox()
                .open_tasks_of(PROV_WORKER_PANE)
                .is_empty()
        );
        release.send(()).unwrap();
        fx.stop().await;
    }

    fn pane_inbox_contains(registry: &AgentPtyRegistry, pane_id: &str, needle: &str) -> bool {
        registry
            .inbox()
            .rendered(pane_id)
            .into_iter()
            .any(|delivery| delivery.text.contains(needle))
    }

    /// Scenario: A Pi worker reports a turn starting before its receipt watcher registers, is then handed a deck message it holds, and the hub's own clock runs with a short not-started window. Nothing shows while the turn runs; once the worker's own attested Idle ends it, and while more Idles keep arriving, the card lists the message as one that may not have reached the agent. A predecessor's Thinking, a ToolEnd and an unattested Thinking leave the row; the worker's attested Thinking withdraws it, and after a Cancel it never returns.
    #[spec("inbox/boundary/044")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boundary_044_a_pi_workers_attested_turns_drive_the_not_started_row() {
        use crate::event::{InboxComponent, InboxWaitMode};
        use crate::inbox_display::AttentionKind;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let _lock = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        // SAFETY: nextest runs each test in its own process; the hub reads it
        // once, when it is created below.
        unsafe { std::env::set_var(crate::inbox_hub::ENV_NOT_STARTED_MS, "300") };
        let fx = ProvenanceFixture::start().await;
        let (pane, agent) = (PROV_WORKER_PANE, fx.worker_agent.clone());
        let hub = Arc::clone(fx.registry.inbox());
        let send = |agent_id: String, kind: &'static str, token: Option<String>| {
            let sock = fx.sock.clone();
            async move {
                let mut payload = serde_json::json!({
                    "session_id": "pi-session", "pane_id": pane, "agent_id": agent_id,
                    "agent_type": "pi", "event_type": kind, "timestamp": chrono::Utc::now(),
                });
                if let Some(token) = token {
                    payload["token"] = serde_json::Value::String(token);
                }
                let mut stream = UnixStream::connect(&sock).await.unwrap();
                stream
                    .write_all(format!("{payload}\n").as_bytes())
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
                let mut reply = String::new();
                let _ = stream.read_to_string(&mut reply).await;
            }
        };
        let own = || Some(fx.worker_token.clone());
        let row = || {
            hub.summary(pane, &agent)
                .notices
                .attention
                .iter()
                .find(|item| item.kind == AttentionKind::NotStarted)
                .cloned()
        };
        let quiet_for = |span: Duration| {
            let row = &row;
            async move {
                let until = std::time::Instant::now() + span;
                while std::time::Instant::now() < until {
                    assert!(row().is_none(), "the row stayed off");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        };
        let raised = || {
            let row = &row;
            async move {
                let until = std::time::Instant::now() + Duration::from_secs(5);
                while row().is_none() {
                    assert!(std::time::Instant::now() < until, "the row appears");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        };
        // A turn starts before the watcher registers.
        send(agent.clone(), "thinking", own()).await;
        let (client, server) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(server);
        let serve = tokio::spawn(crate::inbox_hub::serve_waiter(
            Arc::clone(&hub),
            pane.to_string(),
            agent.clone(),
            InboxWaitMode::Stream,
            InboxComponent::PiExtension,
            None,
            crate::inbox_hub::ReceiptArm {
                adapter_integration: Some(crate::inbox_hub::ADAPTER_INTEGRATION),
                ..Default::default()
            },
            BufReader::new(read),
            write,
            |_: &str| {},
        ));
        let (from, mut to) = tokio::io::split(client);
        let mut lines = BufReader::new(from).lines();
        let id = hub
            .post_direct(
                crate::inbox::Recipient::bound(pane, &agent),
                crate::inbox::Content::from_deck("seed", "task", "the task"),
            )
            .unwrap();
        let handed = loop {
            let line = lines.next_line().await.unwrap().unwrap();
            let reply: crate::event::InboxWaitReply = serde_json::from_str(&line).unwrap();
            if let Some(messages) = reply.messages {
                break messages;
            }
        };
        let at = handed[0].attempt.clone().expect("a receipt attempt");
        let r = serde_json::json!({"id": id.as_str(), "a": at.a, "e": at.e});
        let control =
            serde_json::json!({"possess": {"seq": 1, "seen": at.d, "held": [r]}, "handed": [r]});
        to.write_all(format!("{control}\n").as_bytes())
            .await
            .unwrap();
        quiet_for(Duration::from_millis(900)).await;
        // The worker's own Idle ends the turn; the window then runs out while
        // more Idles keep arriving, since a repeated Idle does not restart it.
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while row().is_none() {
            assert!(
                std::time::Instant::now() < until,
                "the row appears while repeated Idles arrive"
            );
            send(agent.clone(), "idle", own()).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for (who, kind, token) in [
            ("a-predecessor".to_string(), "thinking", None),
            (agent.clone(), "tool_end", own()),
            (agent.clone(), "thinking", None),
        ] {
            send(who.clone(), kind, token).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(row().is_some(), "{who} {kind}: the row stays");
        }
        // An attested predecessor generation's edge is the registry's to
        // refuse: it names an agent that no longer owns the pane.
        assert_eq!(
            fx.registry
                .note_turn(pane, "a-predecessor", crate::agent_pty::TurnEdge::Started),
            None,
            "a non-owner's turn edge is not numbered for the clock"
        );
        send(agent.clone(), "thinking", own()).await;
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while row().is_some() {
            assert!(
                std::time::Instant::now() < until,
                "a started turn withdraws it"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        send(agent.clone(), "idle", own()).await;
        raised().await;
        let item = row().unwrap();
        hub.inbox_action(pane, &agent, id.as_str(), item.revision, false, "tui-1");
        quiet_for(Duration::from_millis(900)).await;
        drop(to);
        drop(lines);
        let _ = serve.await;
    }

    async fn assert_agent_event_refused(kind: &str, token_kind: &str, policy: &str) {
        let _lock = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        let _policy = HookProvenanceEnv::set(policy);
        let mut fx = ProvenanceFixture::start().await;
        fx.agent_event(
            PROV_ORCH_PANE,
            "prov-card",
            "idle",
            Some(&fx.orchestrator_token),
        )
        .await;
        assert!(
            fx.state.read().await.sessions.contains_key("prov-card"),
            "own-token control must create a card"
        );
        assert!(
            fx.events.try_recv().is_ok(),
            "own-token control must reach attach clients"
        );
        let before = format!("{:?}", fx.state.read().await.sessions);
        let token = match token_kind {
            "missing" => None,
            "sibling" => Some(fx.worker_token.as_str()),
            "unknown" => Some("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
            "malformed" => Some("not-a-capability"),
            _ => unreachable!(),
        };
        fx.agent_event(PROV_ORCH_PANE, "prov-card", kind, token)
            .await;
        let after = format!("{:?}", fx.state.read().await.sessions);
        let broadcast = fx.events.try_recv().ok();
        fx.stop().await;
        assert!(
            before == after && broadcast.is_none(),
            "{policy}: {token_kind} {kind} must leave the pane's card unchanged and never reach attach clients; changed={} broadcast={broadcast:?}",
            before != after
        );
    }

    /// Scenario: An outside status sender names a daemon-spawned pane without its token. Its running report must change neither the card nor the attach-client stream, while the own-token control lands.
    #[tokio::test]
    async fn hook_provenance_agent_event_refuses_missing_status_token() {
        assert_agent_event_refused("thinking", "missing", "enforce").await;
    }

    /// Scenario: An outside sender forges SessionStart for a daemon-spawned pane without its token. The existing card and attach-client stream must stay unchanged.
    #[tokio::test]
    async fn hook_provenance_agent_event_refuses_missing_session_start_token() {
        assert_agent_event_refused("session_start", "missing", "enforce").await;
    }

    /// Scenario: A status sender names one pane but presents its sibling's genuine token. Neither that pane's card nor attach clients may receive the event.
    #[tokio::test]
    async fn hook_provenance_agent_event_refuses_sibling_token() {
        assert_agent_event_refused("thinking", "sibling", "enforce").await;
    }

    /// Scenario: A status sender presents a well-formed capability the daemon never minted. Its event must leave the named pane's card and attach stream unchanged.
    #[tokio::test]
    async fn hook_provenance_agent_event_refuses_unknown_token() {
        assert_agent_event_refused("thinking", "unknown", "enforce").await;
    }

    /// Scenario: A status sender presents a malformed capability. Its event must leave the named pane's card and attach stream unchanged.
    #[tokio::test]
    async fn hook_provenance_agent_event_refuses_malformed_token() {
        assert_agent_event_refused("thinking", "malformed", "enforce").await;
    }

    /// Scenario: Enable the older-CLI warn policy and send a token-less status for a spawned pane. It must update the card and reach attach clients.
    #[tokio::test]
    async fn hook_provenance_agent_event_warn_admits_missing_token() {
        let _lock = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        let _policy = HookProvenanceEnv::set("warn");
        let mut fx = ProvenanceFixture::start().await;
        fx.agent_event(PROV_ORCH_PANE, "warn-card", "thinking", None)
            .await;
        assert_eq!(
            fx.state.read().await.sessions["warn-card"].status,
            crate::state::SessionStatus::Thinking
        );
        assert!(
            fx.events.try_recv().is_ok(),
            "warn compatibility must reach attach clients"
        );
        fx.stop().await;
    }

    /// Scenario: Enable warn compatibility and present a sibling's valid capability. It must still be refused before changing the card or reaching attach clients.
    #[tokio::test]
    async fn hook_provenance_agent_event_warn_refuses_sibling_token() {
        assert_agent_event_refused("thinking", "sibling", "warn").await;
    }

    /// Scenario: Enable warn compatibility and present an unknown well-formed capability. It must still be refused before changing the card or reaching attach clients.
    #[tokio::test]
    async fn hook_provenance_agent_event_warn_refuses_unknown_token() {
        assert_agent_event_refused("thinking", "unknown", "warn").await;
    }

    /// Scenario: Enable warn compatibility and present a malformed capability. It must still be refused before changing the card or reaching attach clients.
    #[tokio::test]
    async fn hook_provenance_agent_event_warn_refuses_malformed_token() {
        assert_agent_event_refused("thinking", "malformed", "warn").await;
    }

    /// Scenario: A genuine external agent announces an unknown pane without any deck capability, then reports running. Its foreign card must appear and update on both the daemon and attach-client stream.
    #[tokio::test]
    async fn hook_provenance_agent_event_admits_foreign_session_and_status() {
        let _lock = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        let _policy = HookProvenanceEnv::set("enforce");
        let mut fx = ProvenanceFixture::start().await;
        fx.agent_event("external-pane", "external-card", "session_start", None)
            .await;
        fx.agent_event("external-pane", "external-card", "thinking", None)
            .await;
        let state = fx.state.read().await;
        assert_eq!(
            state.sessions["external-card"].status,
            crate::state::SessionStatus::Thinking
        );
        assert_eq!(
            state.sessions["external-card"].pane_id.as_deref(),
            Some("external-pane")
        );
        drop(state);
        assert_eq!(fx.next_event().session_id, "external-card");
        assert_eq!(fx.next_event().session_id, "external-card");
        fx.stop().await;
    }

    /// Scenario: Flood the real hook socket with external SessionStarts while an attached client applies the daemon's broadcasts. Both views must retain at most 256 foreign cards, evict the oldest external card, and preserve the daemon-spawned card outside that budget.
    #[tokio::test]
    async fn hook_provenance_foreign_eviction_reaches_attach_clients() {
        let _lock = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        let _policy = HookProvenanceEnv::set("enforce");
        let mut fx = ProvenanceFixture::start().await;
        let mut client = crate::state::AppState::default();
        client.register_pane(PROV_ORCH_PANE.to_string());
        fx.agent_event(
            PROV_ORCH_PANE,
            "managed-card",
            "idle",
            Some(&fx.orchestrator_token),
        )
        .await;
        client.apply_event(fx.next_event());
        for index in 0..=256 {
            fx.agent_event(
                &format!("flood-pane-{index}"),
                &format!("flood-card-{index}"),
                "session_start",
                None,
            )
            .await;
            while let Ok(message) = fx.events.try_recv() {
                match message {
                    BroadcastMsg::Event(event) => {
                        client.apply_event(event);
                    }
                    _ => panic!("unexpected broadcast during foreign hook ingestion"),
                }
            }
        }
        let daemon = fx.state.read().await;
        let daemon_foreign =
            daemon.sessions.len() - usize::from(daemon.sessions.contains_key("managed-card"));
        let client_foreign =
            client.sessions.len() - usize::from(client.sessions.contains_key("managed-card"));
        let managed_survives = daemon.sessions.contains_key("managed-card")
            && client.sessions.contains_key("managed-card");
        let oldest_evicted = !daemon.sessions.contains_key("flood-card-0")
            && !client.sessions.contains_key("flood-card-0");
        drop(daemon);
        fx.stop().await;
        assert!(
            daemon_foreign <= 256 && client_foreign <= 256 && managed_survives && oldest_evicted,
            "issue #697: foreign eviction must reach both daemon and attach-client cards; daemon_foreign={daemon_foreign}, client_foreign={client_foreign}, managed_survives={managed_survives}, oldest_evicted={oldest_evicted}"
        );
    }

    /// Scenario: run the real hook loop against two live orchestration panes and
    /// send the orchestrator's `delegate` carrying the hook capability token the
    /// daemon minted for that pane. The daemon must act on it exactly as before
    /// — the worker role resolves and the task pointer reaches the worker's PTY
    /// — because a gate that refuses legitimate traffic is worse than the bug it
    /// closes.
    #[tokio::test]
    async fn hook_provenance_admits_a_delegate_carrying_its_own_pane_s_token() {
        let fx = ProvenanceFixture::start().await;
        let token = fx.orchestrator_token.clone();
        let resp = fx.delegate(PROV_ORCH_PANE, Some(&token)).await;
        assert_eq!(resp.error, None, "an attested delegate must not be refused");
        assert_eq!(
            resp.delivered,
            vec!["worker".to_string()],
            "the attested delegate must still route: {resp:?}"
        );
        assert!(
            fx.worker_saw_pointer(Duration::from_secs(20)).await,
            "the daemon never wrote the delegate pointer into the worker's PTY, so the gate \
             let the message through but the delegation did not happen"
        );
        fx.stop().await;
    }

    /// Scenario: the same delegate, naming the same orchestrator pane, with no
    /// token — the shape any process on this box could send after reading a pane
    /// id off `daemon status`. The daemon must refuse it on the connection and
    /// must not write anything into the worker's PTY.
    #[tokio::test]
    async fn hook_provenance_refuses_a_delegate_with_no_token() {
        let _enforce = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        let fx = ProvenanceFixture::start().await;
        let resp = fx.delegate(PROV_ORCH_PANE, None).await;
        let err = resp
            .error
            .clone()
            .expect("a refusal must be reported to the caller");
        assert!(
            err.contains("hook capability token"),
            "the refusal must say why, so the one legitimate cause (an older CLI in the pane) \
             is diagnosable: {err}"
        );
        assert!(
            resp.delivered.is_empty(),
            "a refused delegate must claim nothing was delivered: {resp:?}"
        );
        assert!(
            !fx.worker_saw_pointer(Duration::from_secs(2)).await,
            "the refused delegate still reached the worker's PTY — the gate ran but the \
             handler ran too"
        );
        fx.stop().await;
    }

    /// Scenario: the forgery a token-holding sibling would actually attempt —
    /// the WORKER's own, entirely valid token, presented alongside the
    /// ORCHESTRATOR's pane id. This is the case a naive "does the message carry
    /// a token we minted?" check would wave through, and it is why the daemon
    /// resolves token → pane instead.
    #[tokio::test]
    async fn hook_provenance_refuses_a_sibling_s_valid_token_naming_another_pane() {
        let fx = ProvenanceFixture::start().await;
        let sibling = fx.worker_token.clone();
        let resp = fx.delegate(PROV_ORCH_PANE, Some(&sibling)).await;
        assert!(
            resp.error.is_some(),
            "a valid token must not authorise a pane it was not minted for: {resp:?}"
        );
        assert!(
            !resp
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(PROV_WORKER_PANE),
            "the refusal must not tell the caller which pane the token belongs to"
        );
        assert!(
            !fx.worker_saw_pointer(Duration::from_secs(2)).await,
            "the cross-pane forgery reached the worker's PTY"
        );
        fx.stop().await;
    }

    /// Scenario: a well-formed token this daemon never minted — the shape an
    /// agent that outlived the daemon which started it presents, and the shape a
    /// guess presents. Refused, and refused without the worker's PTY seeing
    /// anything.
    #[tokio::test]
    async fn hook_provenance_refuses_a_token_this_daemon_never_minted() {
        let fx = ProvenanceFixture::start().await;
        let resp = fx.delegate(PROV_ORCH_PANE, Some(&"ab".repeat(32))).await;
        assert!(
            resp.error.is_some(),
            "a token from some other daemon must not attest anything here: {resp:?}"
        );
        assert!(!fx.worker_saw_pointer(Duration::from_secs(2)).await);
        fx.stop().await;
    }

    /// Scenario: issue #1129 — the worker's own `work-done`, carrying the token
    /// its spawn was given, is admitted AND is now acknowledged on the
    /// connection. The verb answered nothing at all before this, so the
    /// acknowledgement is the whole of what is new; the handler must still run,
    /// which is what the orchestrator's PTY proves.
    #[tokio::test]
    async fn hook_provenance_acknowledges_an_attested_work_done() {
        let fx = ProvenanceFixture::start().await;
        let token = fx.worker_token.clone();
        let ack = fx
            .work_done(PROV_WORKER_PANE, Some(&token), "WORKDONE-ATTESTED-5a1c")
            .await
            .expect("an attested work-done must be acknowledged, not answered with silence");
        assert!(
            ack.is_signal_ack(),
            "the ack must identify itself, or an older CLI would read any line as one: {ack:?}"
        );
        assert!(
            ack.accepted,
            "an attested work-done must be admitted: {ack:?}"
        );
        assert_eq!(ack.error, None, "an admission carries no error: {ack:?}");
        assert!(
            fx.orchestrator_saw("WORKDONE-ATTESTED-5a1c", Duration::from_secs(20))
                .await,
            "the ack was written but the handler never ran — the report never reached the \
             orchestrator"
        );
        fx.stop().await;
    }

    /// Scenario: issue #1129 — the same `work-done`, naming the same worker
    /// pane, with no token. Before this the daemon refused it and wrote nothing
    /// back, so the sender exited 0 on a report that was dropped. It must now be
    /// told, and the report must still not reach the orchestrator.
    #[tokio::test]
    async fn hook_provenance_tells_the_sender_a_work_done_was_refused() {
        let _enforce = HOOK_PROVENANCE_ENV_LOCK.lock().await;
        let fx = ProvenanceFixture::start().await;
        let ack = fx
            .work_done(PROV_WORKER_PANE, None, "WORKDONE-FORGED-2b7e")
            .await
            .expect("a refused work-done must be reported to the caller, not dropped silently");
        assert!(
            ack.is_signal_ack(),
            "the refusal must identify itself: {ack:?}"
        );
        assert!(
            !ack.accepted,
            "a refused work-done must not report acceptance: {ack:?}"
        );
        assert_eq!(
            ack.reason.as_deref(),
            Some("missing_token"),
            "the ack must carry the same greppable code the daemon put in its own warn line:              {ack:?}"
        );
        let err = ack
            .error
            .clone()
            .expect("a refusal must say why, so an older CLI in the pane is diagnosable");
        assert!(
            err.contains("hook capability token"),
            "the refusal must name the mechanism: {err}"
        );
        assert!(
            !fx.orchestrator_saw("WORKDONE-FORGED-2b7e", Duration::from_secs(2))
                .await,
            "the refused work-done still reached the orchestrator's PTY — the caller was told              but the handler ran anyway"
        );
        fx.stop().await;
    }

    /// Scenario: issue #1129 — an OLDER `dot-agent-deck` in a pane writes its
    /// `work-done` line and closes the connection without ever reading a reply,
    /// because the binary predates the acknowledgement. The daemon writes the
    /// ack into a socket nobody is reading and, on a closed peer, into one that
    /// will error. Neither may wedge the hook loop: the next connection must
    /// still be served.
    ///
    /// This is the half of the cross-version pairing no assertion about the CLI
    /// can reach, since the point is a sender that is not this build.
    #[tokio::test]
    async fn an_ack_nobody_reads_does_not_wedge_the_hook_loop() {
        use tokio::io::AsyncWriteExt;
        let fx = ProvenanceFixture::start().await;
        // Three connections that write and vanish, the way a pre-#1129 CLI
        // does. One is not enough: a write that merely lands in the socket
        // buffer proves nothing about a peer that is already gone.
        for i in 0..3 {
            let msg = crate::event::DaemonMessage::WorkDone(crate::event::WorkDoneSignal {
                pane_id: PROV_WORKER_PANE.to_string(),
                task: format!("WORKDONE-DEAF-{i}"),
                done: false,
                timestamp: chrono::Utc::now(),
                token: None,
            });
            let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
            let mut stream = UnixStream::connect(&fx.sock).await.expect("connect");
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            drop(stream);
        }
        // The loop still serves: an attested work-done is acknowledged and acted
        // on afterwards.
        let token = fx.worker_token.clone();
        let ack = fx
            .work_done(PROV_WORKER_PANE, Some(&token), "WORKDONE-AFTER-DEAF-3c9f")
            .await
            .expect("the hook loop stopped answering after writing acks nobody read");
        assert!(ack.accepted, "{ack:?}");
        assert!(
            fx.orchestrator_saw("WORKDONE-AFTER-DEAF-3c9f", Duration::from_secs(20))
                .await,
            "the hook loop answered but stopped handling after the unread acks"
        );
        fx.stop().await;
    }

    /// Send one `work_done` line from the worker's pane without half-closing,
    /// and return the connection for reading its reply line by line.
    async fn open_work_done(fx: &ProvenanceFixture, report: &str) -> UnixStream {
        use tokio::io::AsyncWriteExt;
        let msg = crate::event::DaemonMessage::WorkDone(crate::event::WorkDoneSignal {
            pane_id: PROV_WORKER_PANE.to_string(),
            task: report.to_string(),
            done: false,
            timestamp: chrono::Utc::now(),
            token: Some(fx.worker_token.clone()),
        });
        let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
        let mut stream = UnixStream::connect(&fx.sock).await.expect("connect");
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        stream
    }

    /// Scenario: PRD #1631 — the daemon's state lock is held, so a worker's
    /// `work-done` cannot get as far as queueing its report for the
    /// orchestrator. No acknowledgement may reach the worker meanwhile: an ack
    /// written before the report is queued lets the worker's next `work-done`
    /// be queued ahead of it (`orchestration/work-done/010`). Once the lock is
    /// released the report is acknowledged and delivered.
    #[tokio::test]
    async fn work_done_is_acknowledged_only_once_its_report_is_queued() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let fx = ProvenanceFixture::start().await;
        let held = fx.state.write().await;
        let stream = open_work_done(&fx, "WORKDONE-QUEUED-FIRST-7d2e").await;
        let mut reply = BufReader::new(stream);
        let mut line = String::new();
        let early =
            tokio::time::timeout(Duration::from_millis(1500), reply.read_line(&mut line)).await;
        assert!(
            early.is_err(),
            "the work-done was acknowledged before its report could be queued: {line:?}"
        );
        drop(held);
        tokio::time::timeout(Duration::from_secs(20), reply.read_line(&mut line))
            .await
            .expect("the work-done was never acknowledged after the lock was released")
            .expect("read the acknowledgement");
        let ack: crate::event::SignalAck = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("work_done reply was not a SignalAck ({e}): {line:?}"));
        assert!(ack.is_signal_ack() && ack.accepted, "{ack:?}");
        assert!(
            fx.orchestrator_saw("WORKDONE-QUEUED-FIRST-7d2e", Duration::from_secs(20))
                .await,
            "the acknowledged report never reached the orchestrator's PTY"
        );
        fx.stop().await;
    }

    /// Scenario: PR #1735 fifth review (N3) — the orchestrator is busy and its
    /// inbox is full when a worker reports `work-done`. The worker is told its
    /// report was accepted, so the report must reach the orchestrator's inbox
    /// anyway, rather than be refused there and kept only in the daemon log.
    #[tokio::test]
    async fn a_work_done_reaches_an_orchestrator_whose_inbox_is_full() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let fx = ProvenanceFixture::start().await;
        let orchestrator =
            crate::inbox::Recipient::bound(PROV_ORCH_PANE, fx.orchestrator_agent.as_str());
        while fx
            .registry
            .inbox()
            .post_direct(
                orchestrator.clone(),
                crate::inbox::Content::from_deck("seed", "filler", "filler"),
            )
            .is_ok()
        {}
        let stream = open_work_done(&fx, "WORKDONE-FULL-INBOX-91c4").await;
        let mut reply = BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(20), reply.read_line(&mut line))
            .await
            .expect("the work-done was never answered")
            .expect("read the answer");
        let ack: crate::event::SignalAck = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("work_done reply was not a SignalAck ({e}): {line:?}"));
        assert!(ack.is_signal_ack() && ack.accepted, "{ack:?}");
        assert!(
            fx.orchestrator_saw("WORKDONE-FULL-INBOX-91c4", Duration::from_secs(20))
                .await,
            "an accepted report never reached the orchestrator whose inbox was full"
        );
        fx.stop().await;
    }

    /// Write one message line to the hook socket at `sock`, half-close, and read
    /// the whole reply. A free function so it can run on its own task.
    async fn send_hook_message(sock: PathBuf, msg: crate::event::DaemonMessage) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
        let mut stream = UnixStream::connect(&sock).await.expect("connect");
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await.unwrap();
        buf
    }

    /// Issue #1387: a `.dot-agent-deck.toml` whose read never completes — a FIFO
    /// nobody writes to, standing in for a hung network mount. Opening it for
    /// reading blocks until a writer appears, which here is a watchdog thread
    /// the test releases, or that releases itself after [`Self::WATCHDOG`] if
    /// the test never gets the chance to.
    struct BlockedConfig {
        release: std::sync::mpsc::Sender<()>,
        released: Arc<std::sync::atomic::AtomicBool>,
        watchdog: std::thread::JoinHandle<bool>,
    }

    impl BlockedConfig {
        /// How long the read is left hanging when the test cannot release it —
        /// which is what happens when the read blocks the test's own runtime
        /// thread. Past [`INGEST_DEADLINE`] on purpose: an event a loaded
        /// runner is slow to ingest fails that deadline, rather than landing
        /// after this release and reading as "only ingested once released".
        const WATCHDOG: Duration = Duration::from_secs(30);

        fn plant(dir: &std::path::Path) -> Self {
            use std::os::unix::ffi::OsStrExt;
            use std::os::unix::fs::OpenOptionsExt;
            let path = dir.join(crate::project_config::CONFIG_FILE_NAME);
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `c_path` is a valid NUL-terminated path for the call.
            let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            assert_eq!(
                rc,
                0,
                "mkfifo {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
            let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (release, rx) = std::sync::mpsc::channel::<()>();
            let watchdog = std::thread::spawn({
                let released = released.clone();
                move || {
                    let _ = rx.recv_timeout(Self::WATCHDOG);
                    released.store(true, std::sync::atomic::Ordering::SeqCst);
                    // Every later read finds an ordinary (empty) file…
                    let parked = path.with_extension("parked-fifo");
                    std::fs::rename(&path, &parked).expect("move the FIFO aside");
                    std::fs::write(&path, "").expect("write an ordinary config");
                    // …and every read already parked on the FIFO is let go: a
                    // writer's open wakes the blocked readers, and its close
                    // hands them EOF. A non-blocking open for writing fails with
                    // ENXIO when no reader holds the FIFO, which is how this
                    // reports whether a read was actually parked on it.
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&parked)
                    {
                        Ok(writer) => {
                            std::thread::sleep(Duration::from_millis(200));
                            drop(writer);
                            true
                        }
                        Err(_) => false,
                    }
                }
            });
            Self {
                release,
                released,
                watchdog,
            }
        }

        fn released(&self) -> bool {
            self.released.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Let the parked reads go, and report whether any read was parked. The
        /// join waits out the watchdog's own file work, so it runs on the
        /// blocking pool rather than on the test's runtime thread.
        async fn release(self) -> bool {
            let _ = self.release.send(());
            let watchdog = self.watchdog;
            tokio::task::spawn_blocking(move || watchdog.join().expect("FIFO watchdog panicked"))
                .await
                .expect("joining the FIFO watchdog")
        }
    }

    /// How long [`ingest_session`] waits for the card. Inside
    /// [`BlockedConfig::WATCHDOG`], so a card that is merely slow is reported
    /// as missing rather than as ingested only after the release.
    const INGEST_DEADLINE: Duration = Duration::from_secs(20);

    /// Write one `session_start` for `session` on its own connection and wait,
    /// up to [`INGEST_DEADLINE`], for its card to reach the daemon's
    /// `AppState`. Whether it did.
    async fn ingest_session(fx: &ProvenanceFixture, session: &str) -> bool {
        use tokio::io::AsyncWriteExt;
        let mut stream = UnixStream::connect(&fx.sock).await.expect("connect");
        let line = format!("{}\n", padded_session_start(session, 0));
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        drop(stream);
        let deadline = tokio::time::Instant::now() + INGEST_DEADLINE;
        loop {
            if fx.state.read().await.sessions.contains_key(session) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn ingest_006_delegate(token: String) -> crate::event::DaemonMessage {
        crate::event::DaemonMessage::Delegate(crate::event::DelegateSignal {
            pane_id: PROV_ORCH_PANE.to_string(),
            task: "INGEST-006-TASK".to_string(),
            to: vec!["worker".to_string()],
            supersede: false,
            timestamp: chrono::Utc::now(),
            token: Some(token),
        })
    }

    /// Scenario: issue #1387 — run the real hook loop against a two-role
    /// orchestration whose `.dot-agent-deck.toml` is a FIFO nobody writes to, and
    /// make each daemon path that resolves a role from that file hang reading it:
    /// the `delegate`, `restart-role` and `spawn-role` handlers, and a delegate's
    /// detached dispatch task. While each hangs, a `session_start` written on
    /// another connection must still become a card.
    #[spec("hooks/ingest/006")]
    #[tokio::test]
    async fn ingest_006_a_verb_stuck_reading_the_project_config_does_not_stall_ingest() {
        use crate::event::DaemonMessage;

        type Verb = fn(String) -> DaemonMessage;
        let verbs: [(&str, Verb); 3] = [
            ("delegate", ingest_006_delegate),
            ("restart-role", |token| {
                DaemonMessage::RestartRole(crate::event::RestartRoleSignal {
                    pane_id: PROV_ORCH_PANE.to_string(),
                    role: "worker".to_string(),
                    // The worker is healthy, and without `--force` the restart
                    // is refused before it ever reads the config.
                    force: true,
                    token: Some(token),
                    timestamp: chrono::Utc::now(),
                })
            }),
            ("spawn-role", |token| {
                DaemonMessage::SpawnRole(crate::event::SpawnRoleSignal {
                    pane_id: PROV_ORCH_PANE.to_string(),
                    role: "worker".to_string(),
                    token: Some(token),
                    timestamp: chrono::Utc::now(),
                })
            }),
        ];

        for (verb, message) in verbs {
            let fx = ProvenanceFixture::start().await;
            let blocked = BlockedConfig::plant(fx.cwd.path());
            let reply = tokio::spawn(send_hook_message(
                fx.sock.clone(),
                message(fx.orchestrator_token.clone()),
            ));
            // Long enough for the verb to reach its read of the config and park
            // on the FIFO there.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let ingested = ingest_session(&fx, &format!("ingest-006-{verb}")).await;
            let ingested_while_blocked = ingested && !blocked.released();
            let verb_was_still_waiting = !reply.is_finished();

            let a_read_was_parked = blocked.release().await;
            let reply = tokio::time::timeout(Duration::from_secs(20), reply)
                .await
                .unwrap_or_else(|_| panic!("{verb}: no reply once the config read was released"))
                .expect("the verb's sender task panicked");
            fx.stop().await;

            assert!(
                a_read_was_parked,
                "{verb}: nothing was reading the FIFO when it was released, so this run never \
                 put a blocked config read in the verb's way and proves nothing"
            );
            assert!(
                ingested_while_blocked,
                "{verb}: hook ingestion made no progress while the config read was blocked \
                 (ingested at all: {ingested}) — the verb held the AppState lock (or the \
                 runtime's thread) across a read of .dot-agent-deck.toml"
            );
            assert!(
                verb_was_still_waiting,
                "{verb}: the verb had already answered ({reply:?}) when the event landed, so its \
                 config read was not what the event had to get past"
            );
        }

        // The delegate's detached dispatch task reads the file again once it
        // holds the worker pane's dispatch locks. Hold the order lock so the
        // delegate is answered while the config is still absent, put the FIFO
        // there, then let the queued dispatch run into it.
        let label = "delegate dispatch";
        let fx = ProvenanceFixture::start().await;
        let order = fx.registry.pane_dispatch_order_lock(PROV_WORKER_PANE);
        let held = order.lock().await;
        let reply = send_hook_message(
            fx.sock.clone(),
            ingest_006_delegate(fx.orchestrator_token.clone()),
        )
        .await;
        let reply: crate::event::DelegateResponse = serde_json::from_str(reply.trim())
            .unwrap_or_else(|e| {
                panic!("delegate reply was not a DelegateResponse ({e}): {reply:?}")
            });
        assert_eq!(reply.delivered, vec!["worker".to_string()], "{reply:?}");
        let blocked = BlockedConfig::plant(fx.cwd.path());
        drop(held);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ingested = ingest_session(&fx, "ingest-006-dispatch").await;
        let ingested_while_blocked = ingested && !blocked.released();
        let a_read_was_parked = blocked.release().await;
        fx.stop().await;
        assert!(
            a_read_was_parked,
            "{label}: nothing was reading the FIFO when it was released, so the dispatch never \
             reached its config read and this case proves nothing"
        );
        assert!(
            ingested_while_blocked,
            "{label}: hook ingestion made no progress while the dispatch task's config read was \
             blocked (ingested at all: {ingested}) — it read .dot-agent-deck.toml on the \
             runtime's thread"
        );
    }

    /// Scenario: issue #530 — an attested `dispatch` that its handler then
    /// REJECTS (it names an orchestration the caller's directory does not
    /// define) is still acknowledged as `accepted`, and that acknowledgement is
    /// the only line the connection ever carries. The rejection reaches the
    /// caller as a `dispatch:` message in its own inbox instead.
    ///
    /// This pins what `dot-agent-deck dispatch`'s exit status actually asserts,
    /// which the CLI's help, the dispatcher seed and both dispatcher-mode pages
    /// now state: exit 0 is the provenance gate admitting the request, not the
    /// worktree, the spawn, the prompt's delivery or its confirmation. A change
    /// that starts answering after the handler makes this fail, and is then a
    /// change to a documented contract rather than a silent one.
    #[tokio::test]
    async fn a_dispatch_its_handler_rejects_is_still_acknowledged_as_accepted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let fx = ProvenanceFixture::start().await;
        const MISSING: &str = "no-such-orchestration-530";
        let msg = crate::event::DaemonMessage::Dispatch(crate::event::DispatchSignal {
            pane_id: PROV_ORCH_PANE.to_string(),
            name: "exit-status-probe".to_string(),
            task: Some("never started".to_string()),
            shape: Some(crate::event::DispatchShape::Orchestration {
                name: Some(MISSING.to_string()),
            }),
            timestamp: chrono::Utc::now(),
            token: Some(fx.orchestrator_token.clone()),
        });
        let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
        let mut stream = UnixStream::connect(&fx.sock).await.expect("connect");
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();
        // Read to EOF: the hook loop awaits the handler inline on this
        // connection, so EOF arrives only once the dispatch has been decided.
        // Anything the handler wrote back would therefore be in `buf` too.
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await.unwrap();
        let lines: Vec<&str> = buf.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "a dispatch must answer exactly one line, the gate's ack: {buf:?}"
        );
        let ack: crate::event::SignalAck = serde_json::from_str(lines[0])
            .unwrap_or_else(|e| panic!("dispatch reply was not a SignalAck ({e}): {buf:?}"));
        assert!(ack.is_signal_ack(), "{ack:?}");
        assert!(
            ack.accepted,
            "the ack is written at the gate, before the handler rejects the shape, so it must \
             read as accepted — which is exactly why exit 0 cannot mean the unit started: {ack:?}"
        );
        assert!(
            fx.orchestrator_saw(MISSING, Duration::from_secs(20)).await,
            "the handler's rejection must reach the caller's inbox, the one place the \
             dispatch's outcome is reported"
        );
        fx.stop().await;
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
                background: None,
                background_revision: None,
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

    /// Issue #320: one frame as a generation's hook would post it, naming its
    /// pane and its registry agent id.
    fn takeover_frame_320(
        pane: &str,
        agent_id: &str,
        session_id: &str,
        event_type: crate::event::EventType,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> AgentEvent {
        AgentEvent {
            session_id: session_id.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp,
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(pane.to_string()),
            agent_id: Some(agent_id.to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
            background: None,
            background_revision: None,
        }
    }

    /// Issue #320: a pane that has changed hands, observed from both ends of
    /// the fan-out — the daemon's own `AppState` (registry oracle installed, as
    /// `run_daemon_with` installs it) and an attached TUI's (no oracle, fed
    /// every relayed frame the way `spawn_event_subscriber` feeds it).
    struct Takeover320 {
        registry: Arc<AgentPtyRegistry>,
        _ownership: Arc<dyn crate::state::AgentOwnership>,
        daemon: SharedState,
        tui: crate::state::AppState,
        event_tx: broadcast::Sender<BroadcastMsg>,
        rx: broadcast::Receiver<BroadcastMsg>,
        pane: String,
        old: String,
        new: String,
    }

    impl Takeover320 {
        /// The OUTGOING generation `old` runs on the pane, opens its card and
        /// exits; the INCOMING generation `new` then claims the pane. Nothing
        /// from `new` has been ingested yet.
        async fn new(pane: &str) -> Self {
            let mut fixture = Self::start(pane, "/usr/bin/true").await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while fixture.registry.live_count() != 0 {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the outgoing generation's child never exited"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            fixture.new = fixture
                .registry
                .spawn_agent(Self::spawn_options(pane, "/bin/cat"))
                .expect("the pane must be reusable once its child is gone");
            fixture
        }

        /// The `clear = true` shape instead: `respawn_agent_for_pane` lifts the
        /// outgoing generation's record out of the registry BEFORE the incoming
        /// one is published, so there is no record left to carry a handover
        /// flag when the outgoing agent's late frame is read.
        async fn by_respawn(pane: &str) -> Self {
            let mut fixture = Self::start(pane, "/bin/cat").await;
            fixture.new = fixture
                .registry
                .respawn_agent_for_pane(pane, "/bin/cat")
                .await
                .expect("respawn the pane's agent in place");
            fixture
        }

        fn spawn_options(pane: &str, command: &'static str) -> SpawnOptions<'static> {
            SpawnOptions {
                command: Some(command),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    pane.to_string(),
                )]),
                ..SpawnOptions::default()
            }
        }

        /// The outgoing generation runs `command` on the pane and opens its
        /// card with a `SessionStart`.
        async fn start(pane: &str, command: &'static str) -> Self {
            let registry = Arc::new(AgentPtyRegistry::new());
            let old = registry
                .spawn_agent(Self::spawn_options(pane, command))
                .expect("spawn the outgoing generation");
            let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
            let daemon: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            daemon
                .write()
                .await
                .set_agent_ownership(Arc::downgrade(&ownership));
            let mut tui = crate::state::AppState::default();
            // The TUI registers the panes it draws; the daemon never does for
            // an ordinary spawn — the registry is its answer.
            tui.register_pane(pane.to_string());
            let (event_tx, rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let mut fixture = Self {
                registry,
                _ownership: ownership,
                daemon,
                tui,
                event_tx,
                rx,
                pane: pane.to_string(),
                old,
                new: String::new(),
            };
            let (old, pane) = (fixture.old.clone(), fixture.pane.clone());
            fixture
                .ingest(takeover_frame_320(
                    &pane,
                    &old,
                    "old-session",
                    crate::event::EventType::SessionStart,
                    chrono::Utc::now() - chrono::Duration::seconds(60),
                ))
                .await;
            fixture
        }

        /// Post `event` through the daemon's real ingestion and relay what the
        /// daemon broadcast into the TUI's state.
        async fn ingest(&mut self, event: AgentEvent) -> AgentEvent {
            ingest_event(&self.daemon, &self.event_tx, &self.registry, event).await;
            let BroadcastMsg::Event(relayed) = self.rx.try_recv().expect("the frame is relayed")
            else {
                panic!("expected an event on the fan-out");
            };
            self.tui.apply_event(relayed.clone());
            relayed
        }

        /// Every card on the pane, by the generation it names, on each side.
        async fn owners(&self) -> (Vec<Option<String>>, Vec<Option<String>>) {
            let on_pane = |state: &crate::state::AppState| {
                let mut owners: Vec<Option<String>> = state
                    .sessions
                    .values()
                    .filter(|s| s.pane_id.as_deref() == Some(self.pane.as_str()))
                    .map(|s| s.agent_id.clone())
                    .collect();
                owners.sort();
                owners
            };
            (on_pane(&*self.daemon.read().await), on_pane(&self.tui))
        }

        async fn assert_one_live_card(&self, after: &str) {
            let want = vec![Some(self.new.clone())];
            let (daemon, tui) = self.owners().await;
            assert_eq!(
                daemon, want,
                "daemon: after {after}, the pane must carry the live generation's card \
                 and nothing else"
            );
            assert_eq!(
                tui, want,
                "attached TUI: after {after}, the pane must carry the live generation's \
                 card and nothing else — a frame was ordered by its type or its \
                 producer clock instead of by the pane's generation"
            );
        }

        /// The incoming generation announces itself with a `SessionStart`
        /// stamped EARLIER than the outgoing card's last activity — case B's
        /// shape, where a start's producer clock is not ordering evidence — and
        /// must still take the pane over on both sides.
        async fn take_over(&mut self) {
            let (new, pane) = (self.new.clone(), self.pane.clone());
            let relayed = self
                .ingest(takeover_frame_320(
                    &pane,
                    &new,
                    "new-session",
                    crate::event::EventType::SessionStart,
                    chrono::Utc::now() - chrono::Duration::seconds(120),
                ))
                .await;
            assert_eq!(
                relayed.pane_generation_verdict(),
                Some(crate::event::GenerationVerdict::Current),
                "the incoming generation's frame is relayed as the pane's current one"
            );
            self.assert_one_live_card("the incoming SessionStart").await;
        }
    }

    impl Drop for Takeover320 {
        fn drop(&mut self) {
            self.registry.shutdown_all();
        }
    }

    /// Issue #320: a late `SessionStart` from the OUTGOING generation — the
    /// frame PRD #92 F9 followup-7 documents a slow-booting old agent firing —
    /// arrives after the incoming one owns the pane. It announces a generation
    /// the registry has already seen displaced, so it must neither retire the
    /// live card nor add one beside it, on either side of the fan-out.
    #[tokio::test]
    async fn a_late_outgoing_session_start_cannot_retire_the_live_card() {
        let mut deck = Takeover320::new("takeover-start-320").await;
        deck.take_over().await;

        let (old, pane) = (deck.old.clone(), deck.pane.clone());
        deck.ingest(takeover_frame_320(
            &pane,
            &old,
            "old-session",
            crate::event::EventType::SessionStart,
            chrono::Utc::now(),
        ))
        .await;
        deck.assert_one_live_card("the outgoing generation's late SessionStart")
            .await;
    }

    /// Issue #320, the sibling class #284 added: an outgoing NON-start frame
    /// stamped at-or-newer than the live card's high-water mark. The producer
    /// clock says "newer"; the registry says "displaced", and the registry is
    /// what decides.
    #[tokio::test]
    async fn a_late_outgoing_frame_stamped_newer_cannot_retire_the_live_card() {
        let mut deck = Takeover320::new("takeover-newer-320").await;
        deck.take_over().await;

        let (old, pane) = (deck.old.clone(), deck.pane.clone());
        deck.ingest(takeover_frame_320(
            &pane,
            &old,
            "old-session",
            crate::event::EventType::Thinking,
            chrono::Utc::now() + chrono::Duration::hours(1),
        ))
        .await;
        deck.assert_one_live_card("the outgoing generation's late, newer-stamped Thinking")
            .await;
    }

    /// Issue #320, the other direction of the same discriminator: the incoming
    /// generation's first frame is an ordinary status report (Pi's shape — it
    /// sends no `SessionStart`) stamped OLDER than the outgoing card's last
    /// activity. The registry names it the pane's current generation, so it
    /// retires the stale card on both sides; the producer clock is not weighed.
    #[tokio::test]
    async fn the_current_generation_supersedes_whatever_its_first_frame_is_stamped() {
        let mut deck = Takeover320::new("takeover-older-320").await;

        let (new, pane) = (deck.new.clone(), deck.pane.clone());
        deck.ingest(takeover_frame_320(
            &pane,
            &new,
            "new-session",
            crate::event::EventType::Thinking,
            chrono::Utc::now() - chrono::Duration::seconds(120),
        ))
        .await;
        deck.assert_one_live_card("the incoming generation's older-stamped first frame")
            .await;
    }

    /// Issue #320, on the path PRD #92 F9 followup-7 is actually about: a
    /// `clear = true` respawn replaces the pane's agent IN PLACE, removing the
    /// outgoing record before the incoming one is published. The outgoing
    /// agent's late `SessionStart` must still be recognised as displaced.
    #[tokio::test]
    async fn a_late_frame_from_a_generation_replaced_in_place_cannot_retire_the_live_card() {
        let mut deck = Takeover320::by_respawn("takeover-respawn-320").await;
        deck.take_over().await;

        let (old, pane) = (deck.old.clone(), deck.pane.clone());
        let relayed = deck
            .ingest(takeover_frame_320(
                &pane,
                &old,
                "old-session",
                crate::event::EventType::SessionStart,
                chrono::Utc::now(),
            ))
            .await;
        assert_eq!(
            relayed.pane_generation_verdict(),
            Some(crate::event::GenerationVerdict::Displaced),
            "the registry published the outgoing generation on this pane, so it knows \
             it was replaced even though its record is gone"
        );
        deck.assert_one_live_card("the replaced generation's late SessionStart")
            .await;
    }

    /// Issue #320: the generation marker is the REGISTRY's answer and nobody
    /// else's. A producer's own copy is removed before the fan-out, and an agent
    /// id the registry never published on the pane — an invented one, as a
    /// stand-in or a forgery would carry — is relayed with no verdict at all,
    /// so it is ordered exactly as it was before this marker existed.
    #[tokio::test]
    async fn the_generation_marker_is_the_registrys_answer_only() {
        let mut deck = Takeover320::new("takeover-marker-320").await;
        deck.take_over().await;
        let pane = deck.pane.clone();

        let mut forged = takeover_frame_320(
            &pane,
            "an-id-the-registry-never-minted",
            "forged-session",
            crate::event::EventType::Thinking,
            chrono::Utc::now(),
        );
        forged.metadata.insert(
            crate::event::PANE_GENERATION_METADATA_KEY.to_string(),
            crate::event::PANE_GENERATION_CURRENT.to_string(),
        );
        let relayed = deck.ingest(forged).await;
        assert_eq!(
            relayed
                .metadata
                .get(crate::event::PANE_GENERATION_METADATA_KEY),
            None,
            "a producer-supplied marker must not survive, and an id the registry never \
             published on the pane gets no verdict of its own"
        );

        // A pane the registry holds nothing for gets no verdict either, whatever
        // the producer claimed.
        let mut elsewhere = takeover_frame_320(
            "a-pane-the-registry-never-heard-of",
            &deck.new.clone(),
            "elsewhere-session",
            crate::event::EventType::Thinking,
            chrono::Utc::now(),
        );
        elsewhere.metadata.insert(
            crate::event::PANE_GENERATION_METADATA_KEY.to_string(),
            crate::event::PANE_GENERATION_DISPLACED.to_string(),
        );
        let relayed = deck.ingest(elsewhere).await;
        assert_eq!(relayed.pane_generation_verdict(), None);
    }
}

/// Issue #860: the idle monitor's arming state machine.
///
/// These drive the real [`run_idle_monitor`] against real `AgentPtyRegistry` /
/// `Scheduler` values on a paused-time runtime, so every wake-up is the
/// production one and only the clock is synthetic.
#[cfg(test)]
mod idle_monitor_tests {
    use super::*;
    use crate::scheduler::{Notifier, NotifyEvent, Scheduler};
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    struct SilentNotifier;
    impl Notifier for SilentNotifier {
        fn notify(&self, _event: NotifyEvent) {}
    }

    /// The three keep-alive inputs the idle gate reads, wired the way
    /// `run_daemon_with` wires them, plus a watcher that records the daemon's
    /// shutdown signal.
    struct Harness {
        clients: Arc<AtomicUsize>,
        registry: Arc<AgentPtyRegistry>,
        scheduler: Arc<Scheduler>,
        change: Arc<Notify>,
        signalled: Arc<AtomicBool>,
    }

    impl Harness {
        /// True once the monitor has told the daemon to exit.
        fn shut_down(&self) -> bool {
            self.signalled.load(Ordering::SeqCst)
        }
    }

    /// Hand the runtime enough polls for a just-spawned task to reach its
    /// first await — in particular for an armed timer to REGISTER its
    /// `sleep`. Advancing the clock before that registration would leave the
    /// timer's deadline in the future, which is how the first version of the
    /// regression test below passed vacuously.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    fn spawn_monitor(threshold: Duration) -> Harness {
        let clients = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(AgentPtyRegistry::new());
        let scheduler = Arc::new(Scheduler::new(Arc::new(SilentNotifier)));
        let shutdown = Arc::new(Notify::new());
        let change = registry.change_notify();

        let signalled = Arc::new(AtomicBool::new(false));
        tokio::spawn({
            let shutdown = shutdown.clone();
            let signalled = signalled.clone();
            async move {
                shutdown.notified().await;
                signalled.store(true, Ordering::SeqCst);
            }
        });

        tokio::spawn(run_idle_monitor(
            clients.clone(),
            registry.clone(),
            threshold,
            shutdown,
            change.clone(),
            scheduler.clone(),
        ));

        Harness {
            clients,
            registry,
            scheduler,
            change,
            signalled,
        }
    }

    /// Scenario (control): a daemon that is idle from its first reading, with
    /// nothing ever disturbing the gate, signals shutdown once the window
    /// elapses. This is `lifecycle/daemon-idle/001` at unit altitude, and it
    /// is here to prove the harness can observe a shutdown at all — without
    /// it the regression test below could pass vacuously.
    #[tokio::test(start_paused = true)]
    async fn idle_monitor_shuts_down_an_undisturbed_idle_daemon() {
        let threshold = Duration::from_secs(30);
        let h = spawn_monitor(threshold);
        settle().await;
        assert!(!h.shut_down(), "must not fire before the window elapses");

        tokio::time::sleep(threshold + Duration::from_secs(1)).await;
        settle().await;
        assert!(
            h.shut_down(),
            "an undisturbed idle daemon must signal shutdown after its window"
        );
    }

    /// Scenario (issue #860): the idle timer reaches its deadline while the
    /// gate is *transiently* busy — a client is connected across it — so the
    /// timer's own joint-zero re-check declines to signal shutdown. The
    /// monitor never sees that transient: tokio's `Notify` stores at most ONE
    /// permit, so a connect and a disconnect that land while the monitor is
    /// between readings coalesce into a single wake-up whose reading is the
    /// SETTLED (idle) state. Modelled here by mutating the counter and
    /// delivering only the disconnect edge's notify, which is exactly what
    /// the monitor observes in that case.
    ///
    /// The daemon is then idle again, permanently, with nothing left to
    /// disturb it — the reported orphan's state, whose sockets were unlinked
    /// so nothing could connect even in principle. It must still shut down.
    #[tokio::test(start_paused = true)]
    async fn idle_monitor_rearms_after_its_timer_declines_a_transiently_busy_gate() {
        let threshold = Duration::from_secs(30);
        let h = spawn_monitor(threshold);

        // First reading: idle. The monitor arms a timer for `threshold`, and
        // `settle` lets that timer register its sleep.
        settle().await;

        // A client is connected across the timer's deadline. The monitor is
        // parked with no permit, so it cannot observe this.
        h.clients.store(1, Ordering::SeqCst);
        tokio::time::sleep(threshold + Duration::from_secs(1)).await;
        settle().await;
        assert!(
            !h.shut_down(),
            "precondition: the timer must have DECLINED at its deadline — a \
             busy gate must never shut the daemon down. If this fires, the \
             timer did not run while the gate was busy and the rest of this \
             test proves nothing."
        );

        // ...and is gone again. This edge's notify is the single wake-up the
        // monitor gets, and it reads the settled, idle state.
        h.clients.store(0, Ordering::SeqCst);
        h.change.notify_one();
        settle().await;

        assert_eq!(
            h.clients.load(Ordering::SeqCst),
            0,
            "precondition: no clients"
        );
        assert_eq!(h.registry.live_count(), 0, "precondition: no agents");
        assert!(h.scheduler.is_empty(), "precondition: no pending schedules");

        tokio::time::sleep(threshold * 4).await;
        settle().await;
        assert!(
            h.shut_down(),
            "a daemon that is idle again after its timer declined must still \
             shut down; instead it stayed up for four more windows with no \
             clients, no agents and no pending schedules"
        );
    }
}

/// Issues #318 / #601 / #697, audit round: regressions for the admission and
/// retention defects the provenance audit found, each driven through the real
/// hook socket so the gate, the fan-out and `AppState` are all in the path.
/// Every event here is either token-bearing or for a pane or agent this daemon
/// never issued a token for, so none of them depends on
/// `DOT_AGENT_DECK_HOOK_PROVENANCE`.
#[cfg(all(test, unix))]
mod hook_provenance_audit_tests {
    use super::*;
    use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, PtyWritePause, SpawnOptions};
    use crate::state::{AppState, MAX_UNPROVEN_SESSIONS, SessionStatus};
    use spec::spec;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    struct AuditDeck {
        _dir: tempfile::TempDir,
        sock: std::path::PathBuf,
        registry: Arc<AgentPtyRegistry>,
        state: SharedState,
        events: broadcast::Receiver<BroadcastMsg>,
        handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
    }

    fn spawn(registry: &Arc<AgentPtyRegistry>, pane: Option<&str>, command: &str) -> String {
        registry
            .spawn_agent(SpawnOptions {
                command: Some(command),
                env: pane
                    .map(|pane| vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())])
                    .unwrap_or_default(),
                ..SpawnOptions::default()
            })
            .expect("spawn a stand-in agent")
    }

    async fn wait_until_nothing_lives(registry: &AgentPtyRegistry) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.live_count() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the stand-in never exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn line(
        session: &str,
        pane: Option<&str>,
        agent: Option<&str>,
        kind: &str,
        token: Option<&str>,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "session_id": session,
            "agent_type": "pi",
            "event_type": kind,
            "timestamp": chrono::Utc::now(),
        });
        if let Some(pane) = pane {
            payload["pane_id"] = pane.into();
        }
        if let Some(agent) = agent {
            payload["agent_id"] = agent.into();
        }
        if let Some(token) = token {
            payload["token"] = token.into();
        }
        payload
    }

    impl AuditDeck {
        async fn start(registry: Arc<AgentPtyRegistry>) -> Self {
            let mut app = AppState::default();
            let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
            app.set_agent_ownership(Arc::downgrade(&ownership));
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod tempdir");
            let sock = dir.path().join("hook.sock");
            let listener = IpcListener::from_tokio_listener(
                UnixListener::bind(&sock).expect("bind hook socket"),
            );
            let state: SharedState = Arc::new(tokio::sync::RwLock::new(app));
            let (event_tx, events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let handle = tokio::spawn({
                let registry = registry.clone();
                let state = state.clone();
                let shutdown = Arc::new(Notify::new());
                let wtr = crate::issue_dispatch_run::new_worktree_registry();
                async move { run_hook_loop(listener, state, event_tx, registry, shutdown, wtr).await }
            });
            Self {
                _dir: dir,
                sock,
                registry,
                state,
                events,
                handle,
            }
        }

        /// Write one raw line and wait for the loop to finish with it (a raw
        /// event answers nothing, so EOF is the completion signal).
        async fn send(&self, payload: serde_json::Value) {
            let mut stream = self.open(payload).await;
            Self::finish(&mut stream).await;
        }

        /// Send an unheld question through the real socket and wait for its
        /// one reply, which barriers both registry mutation and broadcast.
        async fn question(&self, pane: &str, event: serde_json::Value, token: Option<&str>) {
            let payload = serde_json::to_value(crate::event::DaemonMessage::Question(Box::new(
                crate::event::QuestionSignal {
                    pane_id: pane.to_string(),
                    token: token.map(str::to_string),
                    event: event_of(event),
                    hold: false,
                    ack: false,
                    confirm_hold: false,
                },
            )))
            .unwrap();
            let mut stream = self.open(payload).await;
            let mut reply = String::new();
            tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut reply))
                .await
                .expect("question connection completed")
                .expect("read question reply");
            serde_json::from_str::<crate::question::QuestionReply>(reply.trim())
                .expect("an admitted unheld question returns a release");
        }

        async fn open(&self, payload: serde_json::Value) -> UnixStream {
            let mut stream = UnixStream::connect(&self.sock).await.expect("connect");
            stream
                .write_all(format!("{payload}\n").as_bytes())
                .await
                .expect("write event");
            stream.shutdown().await.expect("half-close");
            stream
        }

        async fn finish(stream: &mut UnixStream) {
            let mut reply = String::new();
            tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut reply))
                .await
                .expect("event connection completed")
                .expect("read EOF");
            assert!(reply.is_empty(), "a raw event answers nothing: {reply}");
        }

        /// Apply every broadcast so far to `client`, or discard them; returns
        /// how many there were.
        fn drain(&mut self, mut client: Option<&mut AppState>) -> usize {
            let mut count = 0;
            while let Ok(message) = self.events.try_recv() {
                if let BroadcastMsg::Event(event) = message {
                    count += 1;
                    if let Some(client) = client.as_deref_mut() {
                        client.apply_event(event);
                    }
                }
            }
            count
        }

        async fn card(&self, session: &str) -> Option<crate::state::SessionState> {
            self.state.read().await.sessions.get(session).cloned()
        }

        async fn flood(&mut self, prefix: &str, count: usize) {
            for index in 0..count {
                let pane = format!("{prefix}-pane-{index}");
                self.send(line(
                    &format!("{prefix}-card-{index}"),
                    Some(&pane),
                    None,
                    "session_start",
                    None,
                ))
                .await;
                self.drain(None);
            }
        }

        async fn stop(self) {
            self.handle.abort();
            let _ = self.handle.await;
            self.registry.shutdown_all();
        }
    }

    /// Scenario: A spawn is reserved and handed its token, but its record is not
    /// published yet — the instant its child is being forked. The child's first
    /// `SessionStart`, carrying that token, must be attested and draw its card;
    /// a token this daemon never minted, presented for the same pane, paneless
    /// spawn included, must still be refused.
    #[tokio::test]
    async fn hook_provenance_audit_a_token_minted_for_an_in_flight_spawn_attests_its_first_report()
    {
        use crate::hook_provenance::{Provenance, Refusal, classify_event, mint};
        let pane = "audit-in-flight-pane";
        let registry = Arc::new(AgentPtyRegistry::new());
        let (agent, token) = registry.reserve_spawn_for_test(Some(pane));
        let (paneless, paneless_token) = registry.reserve_spawn_for_test(None);
        assert!(
            registry.hook_token_of(&agent).is_none(),
            "precondition: no record is published for the reserved spawn"
        );

        assert_eq!(
            classify_event(Some(pane), Some(&agent), Some(&token), &*registry),
            Provenance::Attested {
                agent_id: agent.clone(),
                facts: Default::default(),
            },
            "a token minted for an in-flight spawn names that spawn"
        );
        assert_eq!(
            classify_event(None, Some(&paneless), Some(&paneless_token), &*registry),
            Provenance::Attested {
                agent_id: paneless.clone(),
                facts: Default::default(),
            },
            "the paneless in-flight spawn resolves the same way"
        );
        let never_minted = mint();
        assert_eq!(
            classify_event(Some(pane), Some(&agent), Some(&never_minted), &*registry),
            Provenance::Refused(Refusal::UnknownToken),
            "control: a token this daemon never minted is still unknown"
        );
        assert_eq!(
            classify_event(None, Some(&paneless), Some(&never_minted), &*registry),
            Provenance::Refused(Refusal::UnknownToken),
            "control: the paneless spawn too"
        );

        let deck = AuditDeck::start(registry).await;
        deck.send(line(
            "in-flight-card",
            Some(pane),
            Some(&agent),
            "session_start",
            Some(&token),
        ))
        .await;
        assert!(
            deck.card("in-flight-card").await.is_some(),
            "the in-flight spawn's first SessionStart must be applied"
        );
        deck.send(line(
            "forged-card",
            Some(pane),
            Some(&agent),
            "session_start",
            Some(&never_minted),
        ))
        .await;
        assert!(
            deck.card("forged-card").await.is_none(),
            "control: a SessionStart carrying a never-minted token is refused"
        );
        deck.stop().await;
    }

    /// Scenario: An ordinary daemon-spawned pane reports its own start with its
    /// token, then a token-less sender names that card's key with no pane and
    /// no agent — first a running report, then an outside flood, then a
    /// session end. The managed card must keep its status, survive the flood
    /// and survive the end.
    #[tokio::test]
    async fn hook_provenance_audit_paneless_unproven_event_cannot_touch_a_paned_managed_card() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = spawn(&registry, Some("audit-managed-pane"), "cat");
        let token = registry.hook_token_of(&agent).expect("token");
        let mut deck = AuditDeck::start(registry).await;
        deck.send(line(
            "managed-card",
            Some("audit-managed-pane"),
            Some(&agent),
            "session_start",
            Some(&token),
        ))
        .await;
        let before = deck.card("managed-card").await.expect("managed card");

        deck.send(line("managed-card", None, None, "thinking", None))
            .await;
        let after_status = deck.card("managed-card").await.expect("managed card");
        assert_eq!(
            after_status.status, before.status,
            "a paneless unproven status must not drive a paned managed card"
        );

        deck.flood("audit-flood-a", MAX_UNPROVEN_SESSIONS + 1).await;
        assert!(
            deck.card("managed-card").await.is_some(),
            "a managed card must never be evicted by an outside flood"
        );

        deck.send(line("managed-card", None, None, "session_end", None))
            .await;
        assert!(
            deck.card("managed-card").await.is_some(),
            "a paneless unproven session end must not remove a paned managed card"
        );
        deck.stop().await;
    }

    /// Scenario: A daemon-spawned agent with no pane reports its start with its
    /// token. An outside agent then draws a card on an invented pane and sends
    /// a running report, a flood and a session end under the paneless managed
    /// card's key. The managed card must stay paneless, keep its status, and
    /// survive both the flood and the end.
    #[tokio::test]
    async fn hook_provenance_audit_paned_unproven_event_cannot_touch_a_paneless_managed_card() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = spawn(&registry, None, "cat");
        let token = registry.hook_token_of(&agent).expect("token");
        let mut deck = AuditDeck::start(registry).await;
        deck.send(line(
            "paneless-card",
            None,
            Some(&agent),
            "session_start",
            Some(&token),
        ))
        .await;
        let before = deck.card("paneless-card").await.expect("paneless card");
        assert_eq!(before.pane_id, None, "precondition: a paneless card");

        deck.send(line(
            "outside-card",
            Some("audit-invented-pane"),
            None,
            "session_start",
            None,
        ))
        .await;
        deck.send(line(
            "paneless-card",
            Some("audit-invented-pane"),
            None,
            "thinking",
            None,
        ))
        .await;
        let after_status = deck.card("paneless-card").await.expect("paneless card");
        assert_eq!(
            (after_status.pane_id.as_deref(), &after_status.status),
            (None, &before.status),
            "an unproven event must neither relocate nor drive a paneless managed card"
        );

        deck.flood("audit-flood-b", MAX_UNPROVEN_SESSIONS + 1).await;
        assert!(
            deck.card("paneless-card").await.is_some(),
            "a managed card must never be evicted by an outside flood"
        );

        deck.send(line(
            "paneless-card",
            Some("audit-invented-pane"),
            None,
            "session_end",
            None,
        ))
        .await;
        assert!(
            deck.card("paneless-card").await.is_some(),
            "an unproven session end must not remove a paneless managed card"
        );
        deck.stop().await;
    }

    /// Scenario: Agent A exits on a pane and agent B replaces it while A's
    /// record and token are retained. A sender holding A's token posts a
    /// running report, a start and a session end for the pane with no agent
    /// id. B's card must be unchanged, no new card may appear, and nothing may
    /// reach attach clients.
    #[tokio::test]
    async fn hook_provenance_audit_absent_identity_cannot_speak_for_a_successor() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let old = spawn(&registry, Some("audit-gen-pane"), "/usr/bin/true");
        let old_token = registry.hook_token_of(&old).expect("old token");
        wait_until_nothing_lives(&registry).await;
        let new = spawn(&registry, Some("audit-gen-pane"), "cat");
        let new_token = registry.hook_token_of(&new).expect("new token");
        assert!(
            crate::hook_provenance::HookTokenDirectory::owner_of_hook_token(&*registry, &old_token)
                .is_some(),
            "precondition: the replaced generation's token still names its spawn"
        );
        let mut deck = AuditDeck::start(registry).await;
        deck.send(line(
            "successor-card",
            Some("audit-gen-pane"),
            Some(&new),
            "session_start",
            Some(&new_token),
        ))
        .await;
        deck.drain(None);
        let before = deck.card("successor-card").await.expect("successor card");

        for (session, kind) in [
            ("successor-card", "thinking"),
            ("stale-start", "session_start"),
            ("successor-card", "session_end"),
        ] {
            deck.send(line(
                session,
                Some("audit-gen-pane"),
                None,
                kind,
                Some(&old_token),
            ))
            .await;
            let after = deck.card("successor-card").await;
            assert_eq!(
                after
                    .as_ref()
                    .map(|card| (&card.status, card.agent_id.as_deref())),
                Some((&before.status, Some(new.as_str()))),
                "{kind}: a replaced generation's token must not drive its successor's card"
            );
            assert!(
                deck.card("stale-start").await.is_none(),
                "{kind}: no card may be drawn on the stale token"
            );
            assert_eq!(
                deck.drain(None),
                0,
                "{kind}: a refused event must not reach attach clients"
            );
        }
        deck.stop().await;
    }

    /// Scenario: Agent A exits on a pane with no successor. Its late running
    /// report and session end, carrying A's own token but no agent id, must
    /// still update and then remove A's card — the lone retiree's final report.
    #[tokio::test]
    async fn hook_provenance_audit_lone_retiree_absent_identity_still_reports() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = spawn(&registry, Some("audit-lone-pane"), "/usr/bin/true");
        let token = registry.hook_token_of(&agent).expect("token");
        wait_until_nothing_lives(&registry).await;
        let mut deck = AuditDeck::start(registry).await;
        deck.send(line(
            "lone-card",
            Some("audit-lone-pane"),
            Some(&agent),
            "session_start",
            Some(&token),
        ))
        .await;
        deck.send(line(
            "lone-card",
            Some("audit-lone-pane"),
            None,
            "thinking",
            Some(&token),
        ))
        .await;
        assert_eq!(
            deck.card("lone-card").await.map(|card| card.status),
            Some(SessionStatus::Thinking),
            "a lone retiree's untagged report must still land"
        );
        deck.send(line(
            "lone-card",
            Some("audit-lone-pane"),
            None,
            "session_end",
            Some(&token),
        ))
        .await;
        assert!(
            deck.card("lone-card").await.is_none(),
            "a lone retiree's untagged session end must still remove its card"
        );
        assert_eq!(deck.drain(None), 3, "all three reached attach clients");
        deck.stop().await;
    }

    /// Scenario: A token-less start for a pane nobody has spawned is
    /// classified as an outside agent's, and before it is applied the deck
    /// spawns its own agent on that pane. Applied afterwards, the start must
    /// not draw a card on the now-managed pane — neither in the daemon nor on
    /// an attached client that already registered the pane as its own.
    #[tokio::test]
    async fn hook_provenance_audit_unproven_verdict_survives_a_token_issued_before_apply() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut deck = AuditDeck::start(registry.clone()).await;
        let lock = deck.state.write().await;
        let mut stream = deck
            .open(line(
                "race-card",
                Some("audit-race-pane"),
                None,
                "session_start",
                None,
            ))
            .await;
        // Let the loop classify the line and block on the state lock.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let agent = spawn(&registry, Some("audit-race-pane"), "cat");
        drop(lock);
        AuditDeck::finish(&mut stream).await;
        assert!(
            registry.hook_token_of(&agent).is_some(),
            "precondition: the pane now holds a token"
        );
        assert!(
            deck.card("race-card").await.is_none(),
            "an unproven verdict must not become proven because the pane was \
             issued a token before the event was applied"
        );
        let mut client = AppState::default();
        client.register_pane("audit-race-pane".to_string());
        deck.drain(Some(&mut client));
        assert!(
            !client.sessions.contains_key("race-card"),
            "an attached client must read the daemon's unproven marker as \
             authoritative, not its own registration of the pane"
        );
        deck.stop().await;
    }

    /// Scenario: A generation's prompt hook sends doorbell fetches to the daemon's real hook socket while a doorbell transaction listens on the pane: one carrying a ring token for a message that is not queued, one from an older hook with no ring, and one carrying a ring but attested by another pane's token. Only the first reaches the pane's evidence channel, with the generation, message, token and shape, although its fetch granted nothing.
    #[spec("inbox/doorbell/046")]
    #[tokio::test]
    async fn doorbell_046_a_hooks_ring_evidence_reaches_the_panes_channel() {
        use crate::event::PromptShape;
        const PANE: &str = "pane-ring-evidence";
        const OTHER: &str = "pane-ring-other";
        const MESSAGE: &str = "m-abcdefgh-0123456789abcdef";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = spawn(&registry, Some(PANE), "/bin/cat");
        let token = registry
            .hook_token_of(&agent)
            .expect("the generation's token");
        let other = spawn(&registry, Some(OTHER), "/bin/cat");
        let other_token = registry.hook_token_of(&other).expect("the other token");
        let mut evidence = registry.subscribe_doorbell_evidence(PANE);
        let deck = AuditDeck::start(registry.clone()).await;
        let fetch = |token: &str, agent: &str, ring: Option<&str>| {
            let mut line = serde_json::json!({
                "message_type": "inbox_fetch", "pane_id": PANE, "agent_id": agent,
                "token": token, "consumer": "doorbell_hook", "doorbell": MESSAGE,
                "max_chars": 4096
            });
            if let Some(ring) = ring {
                line["doorbell_ring"] = ring.into();
                line["doorbell_prompt"] = "exact".into();
            }
            line
        };
        // A fetch is answered (here refused: these stand-ins run no deck
        // hooks); the evidence is handed on before its answer either way.
        let ask = |payload: serde_json::Value| {
            let deck = &deck;
            async move {
                use tokio::io::AsyncBufReadExt;
                let stream = deck.open(payload).await;
                let mut reply = String::new();
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::io::BufReader::new(stream).read_line(&mut reply),
                )
                .await;
            }
        };
        ask(fetch(&token, &agent, Some("abcdfg"))).await;
        let got = tokio::time::timeout(Duration::from_secs(5), evidence.recv())
            .await
            .expect("the evidence arrives")
            .expect("the channel is open");
        assert_eq!(
            got,
            crate::agent_pty::RingEvidence {
                pane_id: PANE.to_string(),
                agent_id: agent.clone(),
                message_id: Some(MESSAGE.to_string()),
                token: "abcdfg".to_string(),
                shape: PromptShape::Exact,
            }
        );
        ask(fetch(&token, &agent, None)).await;
        ask(fetch(&other_token, &other, Some("zzzzzz"))).await;
        // A later report from the generation is the next thing on the channel.
        ask(fetch(&token, &agent, Some("bcdfgi"))).await;
        let next = tokio::time::timeout(Duration::from_secs(5), evidence.recv())
            .await
            .expect("the evidence arrives")
            .expect("the channel is open");
        assert_eq!(
            next.token, "bcdfgi",
            "no evidence came from the older hook or the other pane"
        );
        deck.handle.abort();
        registry.shutdown_all();
    }

    /// Scenario: A generation has an open doorbell attempt and nothing listens on its pane's evidence channel. Through the daemon's real hook socket its prompt hook reports the attempt's token shaped with other text, then another pane's hook reports the same token, then its own hook reports it exact. Only the last moves the generation's latch cell, on receipt, to submitted; the cell never waits for a transaction to drain the channel.
    #[spec("inbox/doorbell/046")]
    #[tokio::test]
    async fn doorbell_046_the_hook_arm_resolves_the_open_attempt_on_receipt() {
        use crate::doorbell_txn::{Latch, Resolution, RingToken};
        const PANE: &str = "pane-ring-cell";
        const OTHER: &str = "pane-ring-cell-other";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = spawn(&registry, Some(PANE), "/bin/cat");
        let token = registry.hook_token_of(&agent).expect("the token");
        let other = spawn(&registry, Some(OTHER), "/bin/cat");
        let other_token = registry.hook_token_of(&other).expect("the other token");
        let doorbell = registry.generation_doorbell(&agent).expect("its doorbell");
        let attempt = doorbell.counter().allocate().unwrap();
        doorbell
            .ring()
            .record(attempt, RingToken::parse("abcdfg").unwrap());
        doorbell.cell().open(attempt).unwrap();
        let deck = AuditDeck::start(registry.clone()).await;
        let fetch = |token: &str, agent: &str, pane: &str, shape: &str| {
            serde_json::json!({
                "message_type": "inbox_fetch", "pane_id": pane, "agent_id": agent,
                "token": token, "consumer": "doorbell_hook",
                "doorbell": "m-abcdefgh-0123456789abcdef", "max_chars": 4096,
                "doorbell_ring": "abcdfg", "doorbell_prompt": shape
            })
        };
        let ask = |payload: serde_json::Value| {
            let deck = &deck;
            async move {
                use tokio::io::AsyncBufReadExt;
                let stream = deck.open(payload).await;
                let mut reply = String::new();
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::io::BufReader::new(stream).read_line(&mut reply),
                )
                .await;
            }
        };
        ask(fetch(&token, &agent, PANE, "with_other_text")).await;
        assert_eq!(doorbell.cell().load(), Latch::Open(attempt), "not exact");
        ask(fetch(&other_token, &other, OTHER, "exact")).await;
        assert_eq!(
            doorbell.cell().load(),
            Latch::Open(attempt),
            "another generation's hook"
        );
        ask(fetch(&token, &agent, PANE, "exact")).await;
        assert_eq!(
            doorbell.cell().load(),
            Latch::Resolved(attempt, Resolution::Submitted)
        );
        deck.handle.abort();
        registry.shutdown_all();
    }

    /// Scenario: A generation is spawned, and status events reach the daemon's real hook socket: one carrying no prompt, then a prompt with no hook token, then a prompt attested by the generation's own token. Only the attested prompt closes the generation's header window, at the first prompt; the others leave its scanner reading.
    #[spec("inbox/doorbell/055")]
    #[tokio::test]
    async fn doorbell_055_an_attested_prompt_closes_the_header_window() {
        use crate::release_scanner::WindowClose;
        const PANE: &str = "pane-release-prompt";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = spawn(&registry, Some(PANE), "/bin/cat");
        let token = registry
            .hook_token_of(&agent)
            .expect("the generation's token");
        let deck = AuditDeck::start(registry.clone()).await;
        let status = |prompt: Option<&str>, token: Option<&str>| {
            let mut payload = line(PANE, Some(PANE), Some(&agent), "thinking", token);
            if let Some(prompt) = prompt {
                payload["user_prompt"] = prompt.into();
            }
            payload
        };
        let open = |registry: &AgentPtyRegistry| registry.release_capture(&agent).unwrap();
        deck.send(status(None, Some(&token))).await;
        assert!(
            open(&registry).2,
            "a status event with no prompt left the window open"
        );
        deck.send(status(Some("hello"), None)).await;
        assert!(open(&registry).2, "an unattested prompt closed the window");
        deck.send(status(Some("hello"), Some(&token))).await;
        let (_, closed, alive) = open(&registry);
        assert!(!alive);
        assert_eq!(closed, Some(WindowClose::FirstPrompt));
        deck.handle.abort();
        registry.shutdown_all();
    }

    /// PRD #1666 part 3, round 5-wake: the variable that turns this test
    /// binary into 051's helper process, with its [`ModeHelper`] as JSON.
    const MODE_HELPER: &str = "DAD_TEST_INPUT_MODE_HELPER";
    const MODE_HELPER_TEST: &str = "daemon::hook_provenance_audit_tests::input_mode_helper_process";

    /// A pane's child for 051: a shell that runs each line typed into its PTY,
    /// so every helper it starts is in the pane's session.
    const MODE_SHELL: &str = "while read -r line; do eval \"$line\"; done";

    /// What one 051 helper process sends, and how.
    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct ModeHelper {
        sock: std::path::PathBuf,
        out: std::path::PathBuf,
        pane: String,
        token: Option<String>,
        seq: u64,
        /// `plain`; `setsid` (from a session of its own, a nested wrapper's
        /// position); `handoff` (connect, hand the connection to a child and
        /// exit, so the peer is gone when its line is read); `handoff_child`.
        how: String,
        #[serde(default)]
        fd: Option<i32>,
        #[serde(default)]
        parent: Option<u32>,
        /// Batch mode, for 052: reports `(seq, icanon, echo)` sent by this one
        /// process, batch `k` once `gate/go-<k>` exists, each on its own
        /// connection, its replies written to `gate/out-<k>.json`.
        #[serde(default)]
        batches: Vec<Vec<(u64, bool, bool)>>,
        #[serde(default)]
        gate: Option<std::path::PathBuf>,
    }

    /// A batch's result: the reporting process and the daemon's replies.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct ModeBatchOut {
        pid: u32,
        replies: Vec<String>,
    }

    /// Send one `input_mode` report on a connection of its own, stamped now,
    /// and return the daemon's reply.
    fn send_mode_report(helper: &ModeHelper, seq: u64, icanon: bool, echo: bool) -> String {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&helper.sock).expect("connect");
        let sampled = crate::input_mode::monotonic_ns().expect("the clock");
        let done = crate::input_mode::monotonic_ns().expect("the clock");
        let report = DaemonMessage::InputMode(crate::event::InputModeReport {
            pane_id: helper.pane.clone(),
            token: helper.token.clone(),
            seq,
            icanon,
            echo,
            sampled_mono_ns: sampled,
            sample_done_mono_ns: done,
        });
        let line = serde_json::to_string(&report).unwrap();
        stream.write_all(format!("{line}\n").as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        reply
    }

    /// Write `value` to `path` whole: a reader polling for `path` never sees
    /// it half written.
    fn write_whole(path: &std::path::Path, value: &impl serde::Serialize) {
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, serde_json::to_vec(value).unwrap()).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    /// A helper's result: the connecting process and the daemon's reply.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct ModeHelperOut {
        pid: u32,
        reply: String,
    }

    impl ModeHelperOut {
        fn ack(&self) -> crate::event::SignalAck {
            serde_json::from_str(self.reply.trim()).expect("one ack line")
        }
    }

    /// Not a check of its own: without [`MODE_HELPER`] it returns at once.
    /// 051 starts this test binary inside a pane's session with it set, and
    /// this process sends one `input_mode` report, both bits clear, stamped
    /// from its own clock.
    #[test]
    fn input_mode_helper_process() {
        use std::io::{Read, Write};
        use std::os::fd::{FromRawFd, IntoRawFd};
        let Ok(helper) = std::env::var(MODE_HELPER) else {
            return;
        };
        let helper: ModeHelper = serde_json::from_str(&helper).expect("the helper's spec");
        if let Some(gate) = &helper.gate {
            for (k, batch) in helper.batches.iter().enumerate() {
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                while !gate.join(format!("go-{k}")).exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "batch {k} never opened"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                let replies = batch
                    .iter()
                    .map(|&(seq, icanon, echo)| send_mode_report(&helper, seq, icanon, echo))
                    .collect();
                write_whole(
                    &gate.join(format!("out-{k}.json")),
                    &ModeBatchOut {
                        pid: std::process::id(),
                        replies,
                    },
                );
            }
            return;
        }
        let (mut stream, peer) = if helper.how == "handoff_child" {
            let parent = helper.parent.expect("the connecting process");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            // SAFETY: signal 0 only checks that the pid exists.
            while unsafe { libc::kill(parent as libc::pid_t, 0) } == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the connecting process stayed"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            // SAFETY: the parent cleared close-on-exec on this connected
            // socket for this process, and nothing else here owns it.
            let stream =
                unsafe { std::os::unix::net::UnixStream::from_raw_fd(helper.fd.expect("fd")) };
            (stream, parent)
        } else {
            if helper.how == "setsid" {
                // SAFETY: no arguments; this process is not a group leader.
                assert_ne!(unsafe { libc::setsid() }, -1, "a session of its own");
            }
            let stream = std::os::unix::net::UnixStream::connect(&helper.sock).expect("connect");
            if helper.how == "handoff" {
                let fd = stream.into_raw_fd();
                // SAFETY: clears close-on-exec on a descriptor this process owns.
                assert_ne!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, -1);
                let child = ModeHelper {
                    how: "handoff_child".to_string(),
                    fd: Some(fd),
                    parent: Some(std::process::id()),
                    ..helper
                };
                std::process::Command::new(std::env::current_exe().expect("the test binary"))
                    .args([
                        "--exact",
                        MODE_HELPER_TEST,
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(MODE_HELPER, serde_json::to_string(&child).unwrap())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("hand the connection on");
                std::process::exit(0);
            }
            (stream, std::process::id())
        };
        let sampled = crate::input_mode::monotonic_ns().expect("the clock");
        let done = crate::input_mode::monotonic_ns().expect("the clock");
        let report = DaemonMessage::InputMode(crate::event::InputModeReport {
            pane_id: helper.pane.clone(),
            token: helper.token.clone(),
            seq: helper.seq,
            icanon: false,
            echo: false,
            sampled_mono_ns: sampled,
            sample_done_mono_ns: done,
        });
        let line = serde_json::to_string(&report).unwrap();
        stream.write_all(format!("{line}\n").as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        write_whole(&helper.out, &ModeHelperOut { pid: peer, reply });
    }

    /// 051's fixture: a deck on the real hook socket and the panes whose
    /// shells start the helpers.
    struct ModeDeck {
        deck: AuditDeck,
        dir: tempfile::TempDir,
        runs: std::sync::atomic::AtomicU32,
    }

    impl ModeDeck {
        async fn start(registry: Arc<AgentPtyRegistry>) -> Self {
            Self {
                deck: AuditDeck::start(registry).await,
                dir: tempfile::tempdir().unwrap(),
                runs: std::sync::atomic::AtomicU32::new(0),
            }
        }

        /// Type `line` into `agent`'s shell.
        async fn type_line(&self, agent: &str, line: &str) {
            let writer = self.deck.registry.agent_writer(agent).expect("writer");
            writer
                .lock()
                .await
                .write_user(format!("{line}\n").as_bytes())
                .await
                .expect("type into the pane's shell");
        }

        /// Start a helper from `agent`'s shell, sending for `pane` with
        /// `token`, and wait for its result.
        async fn report(
            &self,
            agent: &str,
            pane: &str,
            token: Option<&str>,
            seq: u64,
            how: &str,
        ) -> ModeHelperOut {
            let run = self.runs.fetch_add(1, Ordering::Relaxed);
            let out = self.dir.path().join(format!("helper-{run}.json"));
            let helper = ModeHelper {
                sock: self.deck.sock.clone(),
                out: out.clone(),
                pane: pane.to_string(),
                token: token.map(str::to_string),
                seq,
                how: how.to_string(),
                fd: None,
                parent: None,
                batches: Vec::new(),
                gate: None,
            };
            self.start_helper(agent, &helper).await;
            Self::wait_for(&out, &format!("helper {run} ({how})")).await
        }

        /// Type the command that starts `helper` into `agent`'s shell.
        async fn start_helper(&self, agent: &str, helper: &ModeHelper) {
            let exe = std::env::current_exe().expect("the test binary");
            self.type_line(
                agent,
                &format!(
                    "{MODE_HELPER}='{}' '{}' --exact {MODE_HELPER_TEST} --nocapture --test-threads=1 >/dev/null 2>&1",
                    serde_json::to_string(helper).unwrap(),
                    exe.display()
                ),
            )
            .await;
        }

        async fn wait_for<T: serde::de::DeserializeOwned>(out: &std::path::Path, what: &str) -> T {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                if let Ok(bytes) = std::fs::read(out) {
                    return serde_json::from_slice(&bytes).expect("the helper's result");
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{what} never reported"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Start one helper from `agent`'s shell that sends `batches` as one
        /// process, each batch when [`Self::open_batch`] opens it.
        async fn start_batches(
            &self,
            agent: &str,
            pane: &str,
            token: &str,
            batches: Vec<Vec<(u64, bool, bool)>>,
        ) -> std::path::PathBuf {
            let run = self.runs.fetch_add(1, Ordering::Relaxed);
            let gate = self.dir.path().join(format!("gate-{run}"));
            std::fs::create_dir(&gate).unwrap();
            let helper = ModeHelper {
                sock: self.deck.sock.clone(),
                out: gate.join("unused.json"),
                pane: pane.to_string(),
                token: Some(token.to_string()),
                seq: 0,
                how: "plain".to_string(),
                fd: None,
                parent: None,
                batches,
                gate: Some(gate.clone()),
            };
            self.start_helper(agent, &helper).await;
            gate
        }

        /// Let batch `k` go, and wait for its replies.
        async fn open_batch(gate: &std::path::Path, k: usize) -> ModeBatchOut {
            std::fs::write(gate.join(format!("go-{k}")), b"").unwrap();
            Self::wait_for(&gate.join(format!("out-{k}.json")), &format!("batch {k}")).await
        }
    }

    fn mode_pane(registry: &Arc<AgentPtyRegistry>, pane: &str) -> (String, String) {
        let agent = spawn(registry, Some(pane), MODE_SHELL);
        let token = registry
            .hook_token_of(&agent)
            .expect("the generation's token");
        (agent, token)
    }

    /// Scenario: A pane's shell starts a helper process that reports both bits clear on the daemon's real hook socket with the generation's token, then a second helper in the same session reports. The first binds the generation's evidence to itself and sets the settle anchor; the second is refused as `input_mode_conflict`, the store records the conflict naming both processes, and from then on the evidence reads `input_mode_conflict`, so the first helper's next report is refused too.
    #[spec("inbox/doorbell/051")]
    #[tokio::test]
    async fn doorbell_051_a_second_reporter_in_the_session_is_a_conflict() {
        use crate::input_mode::ModeClause;
        const PANE: &str = "pane-mode-conflict";
        let registry = Arc::new(AgentPtyRegistry::new());
        let (agent, token) = mode_pane(&registry, PANE);
        let deck = ModeDeck::start(registry.clone()).await;
        let first = deck.report(&agent, PANE, Some(&token), 1, "plain").await;
        assert!(first.ack().accepted, "the first helper: {}", first.reply);
        let store = registry.input_mode_store(&agent).unwrap();
        let bound = store.observer().expect("bound");
        assert_eq!(bound.pid, first.pid);
        assert!(store.anchor().is_some(), "both bits clear set the anchor");
        let second = deck.report(&agent, PANE, Some(&token), 2, "plain").await;
        let ack = second.ack();
        assert!(!ack.accepted);
        assert_eq!(ack.reason.as_deref(), Some("input_mode_conflict"));
        let store = registry.input_mode_store(&agent).unwrap();
        let (a, b) = store.conflict().expect("the conflict is recorded");
        assert_eq!((a.pid, b.pid), (first.pid, second.pid));
        assert_eq!(store.sticky(), Some(ModeClause::Conflict));
        let now = crate::input_mode::monotonic_ns().unwrap();
        assert_eq!(store.usable(now), Err(ModeClause::Conflict));
        assert_eq!(
            store.usable(now).unwrap_err().as_str(),
            "input_mode_conflict"
        );
        let again = deck.report(&agent, PANE, Some(&token), 3, "plain").await;
        assert_eq!(again.ack().reason.as_deref(), Some("input_mode_conflict"));
        assert_eq!(
            registry
                .input_mode_store(&agent)
                .unwrap()
                .latest()
                .unwrap()
                .seq,
            1
        );
        deck.deck.handle.abort();
        registry.shutdown_all();
    }

    /// Scenario: A pane's shell first prints a line shaped exactly like an `input_mode` report into the pane's output, then starts helpers that report on the real hook socket from a session of their own, with no token, with another pane's token, and by connecting and exiting before their line is read. Each is refused and the generation's evidence stays empty; a helper reporting properly afterwards is accepted and binds, so the refusals were not the fixture's.
    #[spec("inbox/doorbell/051")]
    #[tokio::test]
    async fn doorbell_051_reports_from_outside_the_generation_are_refused() {
        const PANE: &str = "pane-mode-outside";
        const OTHER: &str = "pane-mode-other";
        let registry = Arc::new(AgentPtyRegistry::new());
        let (agent, token) = mode_pane(&registry, PANE);
        let (_other, other_token) = mode_pane(&registry, OTHER);
        let deck = ModeDeck::start(registry.clone()).await;
        let shaped =
            serde_json::to_string(&DaemonMessage::InputMode(crate::event::InputModeReport {
                pane_id: PANE.to_string(),
                token: Some(token.clone()),
                seq: 1,
                icanon: false,
                echo: false,
                sampled_mono_ns: 1,
                sample_done_mono_ns: 2,
            }))
            .unwrap();
        deck.type_line(&agent, &format!("printf '%s\\n' '{shaped}'"))
            .await;
        let empty = |what: &str| {
            let store = registry.input_mode_store(&agent).unwrap();
            assert!(
                store.observer().is_none() && store.latest().is_none() && store.sticky().is_none(),
                "{what} changed the store: {store:?}"
            );
        };
        // The shell runs one line at a time, so this helper's result also
        // means the printed line has gone out.
        let nested = deck.report(&agent, PANE, Some(&token), 2, "setsid").await;
        assert_ne!(nested.pid, 0);
        let ack = nested.ack();
        assert!(!ack.accepted);
        assert_eq!(ack.reason.as_deref(), Some("peer_session"));
        empty("a report-shaped line in the pane's output, or another session");
        let untokened = deck.report(&agent, PANE, None, 3, "plain").await;
        assert!(!untokened.ack().accepted, "{}", untokened.reply);
        empty("a report with no token");
        let wrong = deck
            .report(&agent, PANE, Some(&other_token), 4, "plain")
            .await;
        assert!(!wrong.ack().accepted, "{}", wrong.reply);
        empty("another pane's token");
        let gone = deck.report(&agent, PANE, Some(&token), 5, "handoff").await;
        assert!(!gone.ack().accepted, "{}", gone.reply);
        empty("a peer that exited before its line was read");
        let proper = deck.report(&agent, PANE, Some(&token), 6, "plain").await;
        assert!(proper.ack().accepted, "the control: {}", proper.reply);
        let store = registry.input_mode_store(&agent).unwrap();
        assert_eq!(store.observer().map(|o| o.pid), Some(proper.pid));
        assert_eq!(store.latest().map(|s| s.seq), Some(6));
        deck.deck.handle.abort();
        registry.shutdown_all();
    }

    /// Scenario: A pane's shell draws a Codex start-up header and a helper in its session binds the generation's evidence; then the pane is restarted. The new generation starts with an empty store and no release latched; a helper in the new session reporting with the old generation's token is refused and leaves it empty; a helper reporting with the new token binds it to itself, with its own anchor.
    #[spec("inbox/doorbell/051")]
    #[tokio::test]
    async fn doorbell_051_a_respawned_generation_starts_its_evidence_afresh() {
        use crate::release_scanner::ScanLatch;
        const PANE: &str = "pane-mode-respawn";
        let registry = Arc::new(AgentPtyRegistry::new());
        let (old, old_token) = mode_pane(&registry, PANE);
        let deck = ModeDeck::start(registry.clone()).await;
        let header = deck.dir.path().join("header.bin");
        std::fs::write(
            &header,
            "\x1b[2;3H>_ \x1b[1mOpenAI Codex\x1b[22m\x1b[2m (v0.164.0)\x1b[22m\r\n",
        )
        .unwrap();
        deck.type_line(&old, &format!("cat '{}'", header.display()))
            .await;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !matches!(
            registry.release_capture(&old).unwrap().0,
            ScanLatch::Latched { .. }
        ) {
            assert!(
                std::time::Instant::now() < deadline,
                "the header never latched"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let bound = deck.report(&old, PANE, Some(&old_token), 1, "plain").await;
        assert!(bound.ack().accepted, "{}", bound.reply);
        let old_anchor = registry.input_mode_store(&old).unwrap().anchor();
        assert!(old_anchor.is_some());

        let new = registry
            .respawn_agent_for_pane(PANE, MODE_SHELL)
            .await
            .expect("restart the pane");
        assert_ne!(new, old);
        let token = registry.hook_token_of(&new).expect("the new token");
        let store = registry.input_mode_store(&new).unwrap();
        assert!(store.observer().is_none() && store.anchor().is_none());
        assert_eq!(
            registry.release_capture(&new).unwrap().0,
            ScanLatch::Pending
        );
        let stale = deck.report(&new, PANE, Some(&old_token), 2, "plain").await;
        assert!(!stale.ack().accepted, "{}", stale.reply);
        let store = registry.input_mode_store(&new).unwrap();
        assert!(store.observer().is_none() && store.latest().is_none());
        let fresh = deck.report(&new, PANE, Some(&token), 1, "plain").await;
        assert!(fresh.ack().accepted, "{}", fresh.reply);
        let store = registry.input_mode_store(&new).unwrap();
        assert_eq!(store.observer().map(|o| o.pid), Some(fresh.pid));
        assert_ne!(store.anchor(), old_anchor);
        assert!(store.anchor().is_some());
        deck.deck.handle.abort();
        registry.shutdown_all();
    }

    /// Every broadcast waiting on `events`, counted and dropped.
    fn broadcasts(events: &mut broadcast::Receiver<BroadcastMsg>) -> u64 {
        let mut count = 0;
        loop {
            match events.try_recv() {
                Ok(_) => count += 1,
                Err(broadcast::error::TryRecvError::Lagged(missed)) => count += missed,
                Err(_) => return count,
            }
        }
    }

    /// Scenario: A card is put through Working, Blocked, WaitingForInput and Idle on the real hook socket, and at each one a helper in the pane's session, one process throughout, sends reports that go raw, cooked, raw and raw again. Every report is accepted and stored, yet the card, its last activity and journal included, is exactly what it was before them, nothing is broadcast to attached clients, and the settle anchor is set by the first raw report and never moves.
    #[spec("inbox/doorbell/052")]
    #[tokio::test]
    async fn doorbell_052_reports_move_nothing_but_the_mode_store() {
        const PANE: &str = "pane-mode-quiet";
        const SESSION: &str = "mode-quiet-card";
        let registry = Arc::new(AgentPtyRegistry::new());
        let (agent, token) = mode_pane(&registry, PANE);
        let mut deck = ModeDeck::start(registry.clone()).await;
        let event = |kind: &str| line(SESSION, Some(PANE), Some(&agent), kind, Some(&token));
        deck.deck.send(event("session_start")).await;
        let statuses = [
            ("tool_start", SessionStatus::Working),
            ("quota_blocked", SessionStatus::Blocked),
            ("waiting_for_input", SessionStatus::WaitingForInput),
            ("idle", SessionStatus::Idle),
        ];
        let batch = |k: u64| {
            let base = 1 + 4 * k;
            vec![
                (base, false, false),
                (base + 1, true, true),
                (base + 2, false, false),
                (base + 3, false, false),
            ]
        };
        let gate = deck
            .start_batches(&agent, PANE, &token, (0..4).map(batch).collect())
            .await;
        let mut anchor = None;
        for (k, (kind, status)) in statuses.into_iter().enumerate() {
            deck.deck.send(event(kind)).await;
            let card = deck.deck.card(SESSION).await.expect("the card");
            assert_eq!(card.status, status, "precondition: {kind}");
            let before = format!("{card:?}");
            broadcasts(&mut deck.deck.events);
            let out = ModeDeck::open_batch(&gate, k).await;
            for reply in &out.replies {
                let ack: crate::event::SignalAck = serde_json::from_str(reply.trim()).unwrap();
                assert!(ack.accepted, "{kind}: {reply}");
            }
            let after = format!("{:?}", deck.deck.card(SESSION).await.unwrap());
            assert_eq!(after, before, "{kind}: the card moved");
            assert_eq!(broadcasts(&mut deck.deck.events), 0, "{kind}: a broadcast");
            let store = registry.input_mode_store(&agent).unwrap();
            assert_eq!(store.latest().map(|s| s.seq), Some(4 * k as u64 + 4));
            assert_eq!(store.observer().map(|o| o.pid), Some(out.pid));
            match k {
                0 => anchor = store.anchor(),
                _ => assert_eq!(store.anchor(), anchor, "{kind}: the anchor moved"),
            }
        }
        assert!(anchor.is_some());
        deck.deck.handle.abort();
        registry.shutdown_all();
    }

    /// Scenario: A stand-in for an older wrapper reports a session start and an idle status for its pane on the real hook socket, and sends neither a capabilities probe nor any input-mode report. Each event is broadcast once and the card reads Idle, the generation's input-mode store stays empty, so its evidence reads `input_mode_unverified`, and the doorbell's hold clock reads as it does with no evidence at all: the generation is admitted and no stall is raised.
    #[spec("inbox/doorbell/052")]
    #[tokio::test]
    async fn doorbell_052_an_older_wrapper_leaves_the_evidence_unverified() {
        const PANE: &str = "pane-mode-older";
        const SESSION: &str = "mode-older-card";
        let registry = Arc::new(AgentPtyRegistry::new());
        let (agent, token) = mode_pane(&registry, PANE);
        let mut deck = AuditDeck::start(registry.clone()).await;
        for kind in ["session_start", "idle"] {
            deck.send(line(SESSION, Some(PANE), Some(&agent), kind, Some(&token)))
                .await;
        }
        assert_eq!(broadcasts(&mut deck.events), 2);
        assert_eq!(
            deck.card(SESSION).await.map(|card| card.status),
            Some(SessionStatus::Idle)
        );
        let store = registry.input_mode_store(&agent).unwrap();
        assert!(store.observer().is_none() && store.latest().is_none());
        let now = crate::input_mode::monotonic_ns().unwrap();
        assert_eq!(
            store.usable(now).map_err(|clause| clause.as_str()),
            Err("input_mode_unverified")
        );
        assert!(
            deck.state
                .read()
                .await
                .doorbell_admission(&agent, PANE)
                .is_some(),
            "the doorbell still admits the generation"
        );
        assert!(!registry.inbox().wake_stalled(PANE, &agent));
        deck.handle.abort();
        registry.shutdown_all();
    }

    fn assert_bounded(state: &AppState, what: &str) {
        for (map, size) in state.retained_map_sizes() {
            assert!(
                size <= MAX_UNPROVEN_SESSIONS,
                "{what}: `{map}` retains {size} entries, beyond the \
                 {MAX_UNPROVEN_SESSIONS}-entry unproven budget"
            );
        }
    }

    /// Scenario: Flood the real hook socket with far more outside agents than
    /// the card budget, each on its own invented pane, then run many outside
    /// start/end pairs on fresh panes. Every per-pane and per-agent map the
    /// daemon keeps must stay within the budget, not only the visible cards.
    #[tokio::test]
    async fn hook_provenance_audit_outside_flood_bounds_every_retained_map() {
        let mut deck = AuditDeck::start(Arc::new(AgentPtyRegistry::new())).await;
        deck.flood("audit-big", MAX_UNPROVEN_SESSIONS * 3).await;
        assert_bounded(&*deck.state.read().await, "after the flood");
        for index in 0..MAX_UNPROVEN_SESSIONS * 2 {
            let pane = format!("audit-pair-pane-{index}");
            let session = format!("audit-pair-card-{index}");
            deck.send(line(&session, Some(&pane), None, "session_start", None))
                .await;
            deck.send(line(&session, Some(&pane), None, "session_end", None))
                .await;
            deck.drain(None);
        }
        let state = deck.state.read().await;
        assert_bounded(&state, "after the start/end pairs");
        let sizes = state.retained_map_sizes();
        drop(state);
        deck.stop().await;
        let waiting = sizes
            .iter()
            .find(|(map, _)| *map == "waiting_superseded_sessions")
            .map(|(_, size)| *size);
        assert_eq!(
            waiting,
            Some(0),
            "no waiting-watch history may exist for a pane no agent of the \
             deck's own ever held: {sizes:?}"
        );
    }

    /// Scenario: Repeatedly draw an outside card with no pane and then move it
    /// onto a fresh invented pane, then end it. Nothing per pane may be left
    /// behind. Then, with a client that attached late and so holds a smaller
    /// set, move one more card from no pane onto a pane and let the daemon
    /// evict it: the client must drop that card too.
    #[tokio::test]
    async fn hook_provenance_audit_paneless_to_paned_bookkeeping_follows_the_card() {
        let mut deck = AuditDeck::start(Arc::new(AgentPtyRegistry::new())).await;
        for index in 0..MAX_UNPROVEN_SESSIONS * 2 {
            let pane = format!("audit-move-pane-{index}");
            let session = format!("audit-move-card-{index}");
            deck.send(line(&session, None, None, "session_start", None))
                .await;
            deck.send(line(&session, Some(&pane), None, "session_start", None))
                .await;
            deck.send(line(&session, Some(&pane), None, "session_end", None))
                .await;
            deck.drain(None);
        }
        {
            let state = deck.state.read().await;
            let left: Vec<_> = state
                .retained_map_sizes()
                .into_iter()
                .filter(|(_, size)| *size != 0)
                .collect();
            assert!(
                left.is_empty(),
                "ended outside cards must leave nothing per pane: {left:?}"
            );
        }

        // The daemon fills its budget before the client attaches.
        deck.flood("audit-early", MAX_UNPROVEN_SESSIONS - 1).await;
        let mut client = AppState::default();
        deck.send(line("audit-mover", None, None, "session_start", None))
            .await;
        deck.send(line(
            "audit-mover",
            Some("audit-mover-pane"),
            None,
            "session_start",
            None,
        ))
        .await;
        deck.drain(Some(&mut client));
        assert_eq!(
            client
                .sessions
                .get("audit-mover")
                .and_then(|card| card.pane_id.as_deref()),
            Some("audit-mover-pane"),
            "precondition: the client holds the moved card on its pane"
        );
        // Refresh every earlier card so the moved one is least recently
        // active, then admit one more to make the daemon evict it.
        for index in 0..MAX_UNPROVEN_SESSIONS - 1 {
            deck.send(line(
                &format!("audit-early-card-{index}"),
                Some(&format!("audit-early-pane-{index}")),
                None,
                "thinking",
                None,
            ))
            .await;
        }
        deck.send(line(
            "audit-last",
            Some("audit-last-pane"),
            None,
            "session_start",
            None,
        ))
        .await;
        deck.drain(Some(&mut client));
        let daemon_has = deck.card("audit-mover").await.is_some();
        deck.stop().await;
        assert!(
            !daemon_has,
            "precondition: the daemon evicted the moved card"
        );
        assert!(
            !client.sessions.contains_key("audit-mover"),
            "the daemon's eviction must name the card's current pane, so a \
             client holding a different set drops it as well"
        );
    }

    fn event_of(payload: serde_json::Value) -> AgentEvent {
        serde_json::from_value(payload).expect("a well-formed event")
    }

    /// Scenario: A token-less producer sends unheld question messages with an
    /// invented outer pane, first SessionStart then a status-only fallback.
    /// Their broadcasts stay unproven, so a registry-free client keeps those
    /// cards outside its managed panes and within its outside-card budget.
    #[spec("question/detect/013")]
    #[tokio::test]
    async fn question_detect_013_unattested_questions_keep_the_outside_card_budget() {
        let mut deck = AuditDeck::start(Arc::new(AgentPtyRegistry::new())).await;
        let mut client = AppState::default();
        let mut failures = Vec::new();
        for kind in ["session_start", "waiting_for_input"] {
            let pane = format!("outside-question-{kind}");
            // Control: the equivalent raw-event path marks its broadcast.
            let raw_pane = format!("raw-control-{kind}");
            deck.send(line(
                &format!("raw-{kind}"),
                Some(&raw_pane),
                None,
                kind,
                None,
            ))
            .await;
            let BroadcastMsg::Event(raw) = deck.events.try_recv().expect("raw event broadcast")
            else {
                panic!("expected a raw-event control");
            };
            assert!(
                raw.metadata
                    .contains_key(crate::event::UNPROVEN_METADATA_KEY)
            );

            // A status-only fallback updates an outside session that has
            // already started; it does not create a session by itself.
            if kind == "waiting_for_input" {
                deck.send(line(
                    &format!("question-{kind}"),
                    Some(&pane),
                    None,
                    "session_start",
                    None,
                ))
                .await;
                deck.drain(Some(&mut client));
                assert!(client.sessions.contains_key(&format!("question-{kind}")));
                assert!(!client.managed_pane_ids.contains(&pane));
            }

            deck.question(
                &pane,
                line(
                    &format!("question-{kind}"),
                    Some("invented-inner-pane"),
                    None,
                    kind,
                    None,
                ),
                None,
            )
            .await;
            let BroadcastMsg::Event(event) =
                deck.events.try_recv().expect("question status broadcast")
            else {
                panic!("expected a question status event");
            };
            assert_eq!(event.pane_id.as_deref(), Some(pane.as_str()));
            if !event
                .metadata
                .contains_key(crate::event::UNPROVEN_METADATA_KEY)
            {
                failures.push(format!("{kind}: question broadcast lost daemon_unproven"));
            }
            client.apply_event(event);
            assert!(
                client.sessions.contains_key(&format!("question-{kind}")),
                "the outside card is still shown for {kind}"
            );
            if client.managed_pane_ids.contains(&pane) {
                failures.push(format!(
                    "{kind}: registry-free client promoted the invented pane to managed"
                ));
            }
        }
        // Feed a real raw-event outside flood into the same registry-free
        // client; both earlier question cards must count toward its budget.
        for index in 0..MAX_UNPROVEN_SESSIONS + 1 {
            let pane = format!("question-budget-flood-{index}");
            deck.send(line(&pane, Some(&pane), None, "session_start", None))
                .await;
            deck.drain(Some(&mut client));
        }
        if client.sessions.len() > MAX_UNPROVEN_SESSIONS {
            failures.push(format!(
                "outside-card budget exceeded: {} > {MAX_UNPROVEN_SESSIONS}",
                client.sessions.len()
            ));
        }
        deck.stop().await;
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    /// Scenario: A exits on several panes and B replaces it with an unknown
    /// type. Unheld status-only questions carrying A's tokens must leave B's
    /// types unknown; a fresh control agent's own question learns its type.
    #[spec("question/detect/014")]
    #[tokio::test]
    async fn question_detect_014_a_departed_owners_question_cannot_type_its_successor() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut displaced = Vec::new();
        for index in 0..16 {
            let pane = format!("question-successor-{index}");
            let old = spawn(&registry, Some(&pane), "/usr/bin/true");
            let token = registry.hook_token_of(&old).expect("old token");
            displaced.push((pane, old, token));
        }
        wait_until_nothing_lives(&registry).await;
        let successors: Vec<_> = displaced
            .iter()
            .map(|(pane, _, _)| spawn(&registry, Some(pane), "cat"))
            .collect();
        let type_of = |id: &str| {
            registry
                .agent_records()
                .into_iter()
                .find(|r| r.id == id)
                .expect("live successor")
                .agent_type
        };
        for successor in &successors {
            assert_eq!(
                type_of(successor),
                None,
                "precondition: the successor's type is unknown"
            );
        }
        let mut deck = AuditDeck::start(registry.clone()).await;
        for (index, (pane, old, token)) in displaced.iter().enumerate() {
            deck.question(
                pane,
                line(
                    &format!("departed-question-{index}"),
                    Some(pane),
                    Some(old),
                    "thinking",
                    None,
                ),
                Some(token),
            )
            .await;
        }
        deck.drain(None);
        let typed: Vec<_> = successors
            .iter()
            .filter(|id| type_of(id).is_some())
            .cloned()
            .collect();
        // A fresh pane isolates the positive control from the same
        // pane-only lookup defect being reproduced on the reused panes.
        let pane = "question-own-type-control";
        let own_agent = spawn(&registry, Some(pane), "cat");
        let own_token = registry.hook_token_of(&own_agent).expect("successor token");
        deck.question(
            pane,
            line(
                "own-question",
                Some(pane),
                Some(&own_agent),
                "thinking",
                None,
            ),
            Some(&own_token),
        )
        .await;
        let own_type = type_of(&own_agent);
        deck.stop().await;
        assert_eq!(
            own_type,
            Some(crate::event::AgentType::Pi),
            "a fresh agent's own question still records its type"
        );
        assert!(
            typed.is_empty(),
            "a departed owner's status-only question typed {} successors: {typed:?}",
            typed.len()
        );
    }

    /// Scenario: An outside question is admitted for an unknown pane, and the
    /// deck spawns an agent on that pane immediately after admission. The
    /// outside producer's reported type must not become the fresh agent's type.
    #[spec("question/detect/015")]
    #[tokio::test]
    async fn question_detect_015_an_unattested_question_cannot_type_an_agent_spawned_after_admission()
     {
        use std::sync::Mutex;
        use tracing_subscriber::prelude::*;
        const PANE: &str = "question-admission-race";

        struct SpawnOnQuestion {
            registry: Arc<AgentPtyRegistry>,
            spawned: Arc<Mutex<Option<String>>>,
        }
        #[derive(Default)]
        struct QuestionLog {
            message: String,
        }
        impl tracing::field::Visit for QuestionLog {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                }
            }
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpawnOnQuestion {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = QuestionLog::default();
                event.record(&mut fields);
                // This handler log occurs after the real provenance gate and
                // before the registry type write. Spawn synchronously here to
                // force that interleaving, without a sleep or production seam.
                if fields.message == "Received question" && self.spawned.lock().unwrap().is_none() {
                    let agent = spawn(&self.registry, Some(PANE), "cat");
                    *self.spawned.lock().unwrap() = Some(agent);
                }
            }
        }
        let registry = Arc::new(AgentPtyRegistry::new());
        let spawned = Arc::new(Mutex::new(None));
        let subscriber = tracing_subscriber::registry().with(SpawnOnQuestion {
            registry: registry.clone(),
            spawned: spawned.clone(),
        });
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);
        let deck = AuditDeck::start(registry.clone()).await;
        deck.question(
            PANE,
            line("outside-race-question", Some(PANE), None, "thinking", None),
            None,
        )
        .await;
        let fresh = spawned
            .lock()
            .unwrap()
            .clone()
            .expect("the fresh agent was spawned after admission");
        let learned = registry
            .agent_records()
            .into_iter()
            .find(|r| r.id == fresh)
            .expect("the fresh agent is live")
            .agent_type;
        deck.stop().await;
        assert_eq!(
            learned, None,
            "the already-admitted Unattested question typed the fresh agent"
        );
    }

    /// Scenario: A token-less running report for a role-shaped pane nobody
    /// has spawned waits on the state lock, and meanwhile the deck spawns its
    /// own agent on that pane. A client that registered the pane holds the
    /// new agent's card. The report must reach no client stamped as coming
    /// from an orphaned role pane, and the client's card must keep its
    /// identity, its status and no orphan badge.
    #[tokio::test]
    async fn hook_provenance_audit_orphan_stamp_is_decided_under_the_state_lock() {
        const PANE: &str = "sched-audit-race-1-r0";
        assert!(
            crate::spawn::is_orchestration_role_pane_id(PANE),
            "precondition: a role-shaped pane"
        );
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut deck = AuditDeck::start(registry.clone()).await;
        let lock = deck.state.write().await;
        let mut stream = deck
            .open(line("live-card", Some(PANE), None, "thinking", None))
            .await;
        // Let the loop classify the line and block on the state lock.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let agent = spawn(&registry, Some(PANE), "cat");
        drop(lock);
        AuditDeck::finish(&mut stream).await;

        let mut client = AppState::default();
        client.register_pane(PANE.to_string());
        client.apply_event(event_of(line(
            "live-card",
            Some(PANE),
            Some(&agent),
            "session_start",
            None,
        )));
        let mut stamped_orphaned = 0;
        while let Ok(message) = deck.events.try_recv() {
            if let BroadcastMsg::Event(event) = message {
                stamped_orphaned += usize::from(event.is_orchestration_orphaned());
                client.apply_event(event);
            }
        }
        deck.stop().await;
        assert_eq!(
            stamped_orphaned, 0,
            "a pane the deck spawned on before the report was applied is not \
             an orphan, whatever it was when the report arrived"
        );
        let card = client.sessions.get("live-card").expect("the live card");
        assert_eq!(card.agent_id.as_deref(), Some(agent.as_str()));
        assert_eq!(card.status, SessionStatus::Idle);
        assert!(!card.orchestration_orphaned, "the live card was badged");
        assert_eq!(client.sessions.len(), 1);
    }

    /// Scenario: The deck has reserved a spawn on a role-shaped pane but not
    /// yet published its record or registered its role, and the new agent's
    /// first report arrives — once attested to the reservation's token, once
    /// unproven. Neither may reach a client stamped as coming from an orphaned
    /// role pane, nor as naming a pane with no live agent. The same reports for
    /// a role-shaped pane nobody reserved are still stamped.
    #[tokio::test]
    async fn hook_provenance_audit_reserved_role_pane_is_not_stamped_orphaned() {
        const RESERVED: &str = "sched-audit-reserved-1-r0";
        const ORPHAN: &str = "sched-audit-orphan-1-r0";
        for pane in [RESERVED, ORPHAN] {
            assert!(
                crate::spawn::is_orchestration_role_pane_id(pane),
                "precondition: {pane} is role-shaped"
            );
        }
        let registry = Arc::new(AgentPtyRegistry::new());
        let (reserved_id, _token) = registry.reserve_spawn_for_test(Some(RESERVED));
        assert!(
            !registry.has_live_pane(RESERVED),
            "precondition: the reservation is not published"
        );
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        for (pane, expect_stamped) in [(RESERVED, false), (ORPHAN, true)] {
            for (unproven, attested) in [(false, Some(reserved_id.as_str())), (true, None)] {
                let attested = attested.filter(|_| pane == RESERVED);
                ingest_event_unless(
                    &state,
                    &event_tx,
                    &registry,
                    event_of(line("reserved-card", Some(pane), None, "thinking", None)),
                    unproven,
                    attested,
                    |_| false,
                )
                .await;
                let Ok(BroadcastMsg::Event(event)) = events.try_recv() else {
                    panic!("{pane} unproven={unproven}: the report was broadcast");
                };
                assert_eq!(
                    event.is_orchestration_orphaned(),
                    expect_stamped,
                    "{pane} unproven={unproven}: orphan stamp"
                );
                assert_eq!(
                    event.is_daemon_no_live_agent(),
                    expect_stamped && unproven,
                    "{pane} unproven={unproven}: no-live-agent stamp"
                );
            }
        }
        registry.shutdown_all();
    }

    /// Scenario: A client stayed attached across a daemon restart and still
    /// shows the card of a plain pane's surviving agent; the new daemon
    /// spawned nothing on that pane. The survivor's token-less running report
    /// must update that card, without an orphan badge. A report naming an
    /// invented agent must not, and a start under a fresh key must draw no
    /// card.
    #[tokio::test]
    async fn hook_provenance_audit_plain_survivor_report_reaches_an_attached_client() {
        const PANE: &str = "audit-plain-survivor-1";
        const SURVIVOR: &str = "plain-survivor-agent";
        assert!(
            !crate::spawn::is_orchestration_role_pane_id(PANE),
            "precondition: a plain pane"
        );
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut deck = AuditDeck::start(registry).await;
        let mut client = AppState::default();
        client.register_pane(PANE.to_string());
        client.apply_event(event_of(line(
            "survivor-card",
            Some(PANE),
            Some(SURVIVOR),
            "session_start",
            None,
        )));

        deck.send(line(
            "survivor-card",
            Some(PANE),
            Some(SURVIVOR),
            "thinking",
            None,
        ))
        .await;
        deck.drain(Some(&mut client));
        let card = client.sessions.get("survivor-card").expect("the card");
        assert_eq!(
            card.status,
            SessionStatus::Thinking,
            "a plain pane's restart survivor must keep reporting to an attached client"
        );
        assert!(!card.orchestration_orphaned, "a plain pane has no role");

        deck.send(line(
            "survivor-card",
            Some(PANE),
            Some("invented-agent"),
            "idle",
            None,
        ))
        .await;
        deck.send(line(
            "fresh-key",
            Some(PANE),
            Some("invented-agent"),
            "session_start",
            None,
        ))
        .await;
        deck.drain(Some(&mut client));
        deck.stop().await;
        assert_eq!(
            client.sessions["survivor-card"].status,
            SessionStatus::Thinking,
            "an invented agent must not drive the survivor's card"
        );
        assert_eq!(client.sessions.len(), 1, "no card may be drawn");
    }

    /// Scenario: A token-less running report for a plain pane nobody has
    /// spawned waits on the state lock, and meanwhile the deck spawns its own
    /// agent on that pane. A client that registered the pane holds the new
    /// agent's card. The report must reach no client stamped as coming from a
    /// pane the daemon holds no live agent on, and the client's card must keep
    /// its identity and its status.
    #[tokio::test]
    async fn hook_provenance_audit_no_live_agent_stamp_is_decided_under_the_state_lock() {
        const PANE: &str = "audit-plain-race-1";
        assert!(
            !crate::spawn::is_orchestration_role_pane_id(PANE),
            "precondition: a plain pane"
        );
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut deck = AuditDeck::start(registry.clone()).await;
        let lock = deck.state.write().await;
        let mut stream = deck
            .open(line("live-card", Some(PANE), None, "thinking", None))
            .await;
        // Let the loop classify the line and block on the state lock.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let agent = spawn(&registry, Some(PANE), "cat");
        drop(lock);
        AuditDeck::finish(&mut stream).await;

        let mut client = AppState::default();
        client.register_pane(PANE.to_string());
        client.apply_event(event_of(line(
            "live-card",
            Some(PANE),
            Some(&agent),
            "session_start",
            None,
        )));
        let mut stamped = 0;
        let mut relayed = 0;
        while let Ok(message) = deck.events.try_recv() {
            if let BroadcastMsg::Event(event) = message {
                relayed += 1;
                stamped += usize::from(event.is_daemon_no_live_agent());
                client.apply_event(event);
            }
        }
        deck.stop().await;
        assert!(relayed > 0, "precondition: the report was relayed");
        assert_eq!(
            stamped, 0,
            "a pane the deck spawned on before the report was applied holds a \
             live agent, whatever it held when the report arrived"
        );
        let card = client.sessions.get("live-card").expect("the live card");
        assert_eq!(card.agent_id.as_deref(), Some(agent.as_str()));
        assert_eq!(card.status, SessionStatus::Idle);
        assert_eq!(client.sessions.len(), 1);
    }

    /// Scenario: On each of several panes agent A exits and agent B replaces
    /// it, B's type still unknown. A sender holding A's own token posts a
    /// start naming A and a type. B's recorded type must stay unknown on every
    /// pane; B's own start still records its type.
    #[tokio::test]
    async fn hook_provenance_audit_displaced_tagged_start_cannot_type_its_successor() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut displaced = Vec::new();
        for index in 0..16 {
            let pane = format!("audit-type-pane-{index}");
            let old = spawn(&registry, Some(&pane), "/usr/bin/true");
            let token = registry.hook_token_of(&old).expect("old token");
            displaced.push((pane, old, token));
        }
        wait_until_nothing_lives(&registry).await;
        let successors: Vec<String> = displaced
            .iter()
            .map(|(pane, _, _)| spawn(&registry, Some(pane), "cat"))
            .collect();
        let type_of = |agent: &str| {
            registry
                .agent_records()
                .into_iter()
                .find(|record| record.id == agent)
                .expect("successor record")
                .agent_type
        };
        for successor in &successors {
            assert_eq!(type_of(successor), None, "precondition: type unknown");
        }
        let mut deck = AuditDeck::start(registry.clone()).await;
        for (index, (pane, old, token)) in displaced.iter().enumerate() {
            deck.send(line(
                &format!("displaced-start-{index}"),
                Some(pane),
                Some(old),
                "session_start",
                Some(token),
            ))
            .await;
        }
        deck.drain(None);
        let typed: Vec<&String> = successors
            .iter()
            .filter(|successor| type_of(successor).is_some())
            .collect();
        assert!(
            typed.is_empty(),
            "a displaced generation's start typed its successor on {} of {} \
             panes: {typed:?}",
            typed.len(),
            successors.len()
        );

        let (pane, _, _) = &displaced[0];
        let own_token = registry.hook_token_of(&successors[0]).expect("token");
        deck.send(line(
            "successor-start",
            Some(pane),
            Some(&successors[0]),
            "session_start",
            Some(&own_token),
        ))
        .await;
        assert_eq!(
            type_of(&successors[0]),
            Some(crate::event::AgentType::Pi),
            "the successor's own start still records its type"
        );
        deck.stop().await;
    }

    /// Scenario: Agent A exits and agent B replaces it on a pane, and B's
    /// quota-block latch is set. A work report with no agent id, attested to
    /// A's token, is ingested as if the replacement happened just after the
    /// under-lock check passed. B's latch must survive: the report is A's.
    #[tokio::test]
    async fn hook_provenance_audit_untagged_work_report_cannot_lift_a_successors_block() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let old = spawn(&registry, Some("audit-latch-pane"), "/usr/bin/true");
        wait_until_nothing_lives(&registry).await;
        let new = spawn(&registry, Some("audit-latch-pane"), "cat");
        let epoch = registry
            .note_quota_block("audit-latch-pane", &new)
            .expect("precondition: B latched");
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (event_tx, _events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let work = event_of(line(
            "old-card",
            Some("audit-latch-pane"),
            None,
            "thinking",
            None,
        ));
        assert!(
            crate::quota_block::is_work_evidence(&work),
            "precondition: work evidence"
        );
        ingest_event_unless(
            &state,
            &event_tx,
            &registry,
            work,
            false,
            Some(&old),
            |_| false,
        )
        .await;
        let latched = registry.quota_block_current("audit-latch-pane", &new, epoch);
        registry.shutdown_all();
        assert!(
            latched,
            "a report attested to the replaced generation must not lift its \
             successor's quota block"
        );
    }

    /// Scenario: For each of Claude Code, Codex, OpenCode and Pi, agent A
    /// exits and agent B replaces it on a pane; B reports Thinking and the
    /// user presses Escape twice into B. A's Idle naming A, and an Idle naming
    /// no agent but attested to A's token, must leave B's Escapes holding the
    /// doorbell, and so must A's Thinking then Idle after B's own Idle and a
    /// second Escape pair, and B's own Idle with no Thinking of B's before it;
    /// B's own Idle releases them once B has reported Thinking.
    #[spec("inbox/doorbell/020")]
    #[tokio::test]
    async fn doorbell_020_a_replaced_generations_idle_cannot_release_its_successors_escapes() {
        for agent_type in ["claude_code", "codex", "opencode", "pi"] {
            let pane = format!("escape-generation-{agent_type}");
            let registry = Arc::new(AgentPtyRegistry::new());
            let old = spawn(&registry, Some(&pane), "/usr/bin/true");
            wait_until_nothing_lives(&registry).await;
            let new = spawn(&registry, Some(&pane), "cat");
            let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
            let (event_tx, _events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let ingest = async |kind: &str, agent: Option<&str>, attested: Option<&str>| {
                let mut payload = line(&format!("{pane}-session"), Some(&pane), agent, kind, None);
                payload["agent_type"] = agent_type.into();
                ingest_event_unless(
                    &state,
                    &event_tx,
                    &registry,
                    event_of(payload),
                    false,
                    attested,
                    |_| false,
                )
                .await;
            };
            let writer = registry.agent_writer(&new).expect("the successor's writer");
            let escape_twice = async || {
                writer
                    .lock()
                    .await
                    .write_user(&[0x1b, 0x1b])
                    .await
                    .expect("Escape twice reaches the successor");
            };

            ingest("thinking", Some(&new), None).await;
            escape_twice().await;
            assert!(
                registry.draft_pending(&pane),
                "{agent_type}: precondition: the successor's interrupt holds"
            );
            ingest("idle", Some(&old), None).await;
            let after_tagged = registry.draft_pending(&pane);
            ingest("idle", None, Some(&old)).await;
            let after_attested = registry.draft_pending(&pane);
            ingest("idle", Some(&new), None).await;
            let after_own = registry.draft_pending(&pane);

            escape_twice().await;
            ingest("thinking", Some(&old), None).await;
            ingest("idle", Some(&old), None).await;
            let after_old_turn = registry.draft_pending(&pane);
            ingest("idle", Some(&new), None).await;
            let after_own_idle_alone = registry.draft_pending(&pane);
            ingest("thinking", Some(&new), None).await;
            ingest("idle", Some(&new), None).await;
            let after_own_again = registry.draft_pending(&pane);
            registry.shutdown_all();

            assert!(
                after_tagged,
                "{agent_type}: an Idle naming the replaced generation released \
                 its successor's Escapes"
            );
            assert!(
                after_attested,
                "{agent_type}: an untagged Idle attested to the replaced \
                 generation released its successor's Escapes"
            );
            assert!(
                !after_own,
                "{agent_type}: the successor's own Idle must release its Escapes"
            );
            assert!(
                after_old_turn,
                "{agent_type}: the replaced generation's Thinking then Idle \
                 released the successor's Escapes on an Idle agent"
            );
            assert!(
                after_own_idle_alone,
                "{agent_type}: the successor's Idle with no turn under way \
                 released the Escapes that opened a menu on an Idle agent"
            );
            assert!(
                !after_own_again,
                "{agent_type}: the successor's own turn ending must release them again"
            );
        }
    }

    /// PRD #1666 (r8k R8G-02): one pane with a live `cat` agent of
    /// `agent_type`, and the daemon's own ingest path for its hook reports.
    struct TurnPane {
        registry: Arc<AgentPtyRegistry>,
        state: SharedState,
        event_tx: broadcast::Sender<BroadcastMsg>,
        pane: String,
        agent: String,
        agent_type: &'static str,
    }

    impl TurnPane {
        fn new(pane: &str, agent_type: &'static str) -> Self {
            let registry = Arc::new(AgentPtyRegistry::new());
            let agent = spawn(&registry, Some(pane), "cat");
            Self::with(registry, pane, agent, agent_type)
        }

        fn with(
            registry: Arc<AgentPtyRegistry>,
            pane: &str,
            agent: String,
            agent_type: &'static str,
        ) -> Self {
            let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            Self {
                registry,
                state: Arc::new(tokio::sync::RwLock::new(AppState::default())),
                event_tx,
                pane: pane.to_string(),
                agent,
                agent_type,
            }
        }

        /// The agent reports `kind` (`thinking`, `idle`, …) about itself.
        async fn report(&self, kind: &str) {
            self.report_with(kind, serde_json::json!({})).await;
        }

        /// `kind` as `dot-agent-deck wrap` classifies it from the agent's
        /// output, in its name.
        async fn report_classified(&self, kind: &str) {
            let mut metadata = serde_json::Map::new();
            metadata.insert(
                crate::event::WRAPPER_OUTPUT_CLASSIFIED_METADATA_KEY.into(),
                crate::event::WRAPPER_OUTPUT_CLASSIFIED_METADATA_VALUE.into(),
            );
            self.report_with(kind, metadata.into()).await;
        }

        async fn report_with(&self, kind: &str, metadata: serde_json::Value) {
            let mut payload = line(
                &format!("{}-session", self.pane),
                Some(&self.pane),
                Some(&self.agent),
                kind,
                None,
            );
            payload["agent_type"] = self.agent_type.into();
            payload["metadata"] = metadata;
            ingest_event_unless(
                &self.state,
                &self.event_tx,
                &self.registry,
                event_of(payload),
                false,
                None,
                |_| false,
            )
            .await;
        }

        /// The user's keys `bytes`, through the agent's writer.
        async fn keys(&self, bytes: &[u8]) {
            let writer = self
                .registry
                .agent_writer(&self.agent)
                .expect("the agent's writer");
            writer
                .lock()
                .await
                .write_user(bytes)
                .await
                .expect("the keys reach the agent");
        }

        /// [`Self::keys`] on a task of its own, for a write parked at a seam.
        fn keys_in_background(&self, bytes: &'static [u8]) -> tokio::task::JoinHandle<()> {
            let writer = self
                .registry
                .agent_writer(&self.agent)
                .expect("the agent's writer");
            tokio::spawn(async move {
                writer
                    .lock()
                    .await
                    .write_user(bytes)
                    .await
                    .expect("the keys reach the agent");
            })
        }

        fn holds(&self) -> bool {
            self.registry.draft_pending(&self.pane)
        }
    }

    /// Wait, off the runtime, for a test seam's "reached" signal.
    async fn reached(rx: std::sync::mpsc::Receiver<()>, what: &'static str) {
        tokio::task::spawn_blocking(move || {
            rx.recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("{what}"))
        })
        .await
        .unwrap();
    }

    /// Scenario: For each of Claude Code, Codex, OpenCode and Pi, the agent
    /// finishes a turn (Thinking, then Idle), the user presses Escape twice
    /// on the Idle agent — a menu — and the agent then reports Idle twice
    /// more, as OpenCode does. The repeated Idle ends no turn, so the Escapes
    /// must keep holding the doorbell; the agent's next Thinking then Idle
    /// releases them.
    #[spec("inbox/doorbell/021")]
    #[tokio::test]
    async fn doorbell_021_a_repeated_idle_releases_no_menus_escapes() {
        for agent_type in ["claude_code", "codex", "opencode", "pi"] {
            let pane = TurnPane::new(&format!("repeated-idle-{agent_type}"), agent_type);
            pane.report("thinking").await;
            pane.report("idle").await;
            pane.keys(&[0x1b, 0x1b]).await;
            pane.report("idle").await;
            pane.report("idle").await;
            let after_repeats = pane.holds();
            pane.report("thinking").await;
            pane.report("idle").await;
            let after_turn = pane.holds();
            pane.registry.shutdown_all();

            assert!(
                after_repeats,
                "{agent_type}: a repeated Idle released a menu's Escapes"
            );
            assert!(
                !after_turn,
                "{agent_type}: the agent's next turn ending must release them"
            );
        }
    }

    /// Scenario: The agent reports Thinking and the user presses Escape twice.
    /// The PTY takes both keys, and the agent's Idle is ingested after the
    /// write returned but before the keys are recorded. The Escapes reached
    /// the agent before its turn ended, so they were the interrupt, and must
    /// not hold the doorbell once recorded.
    #[spec("inbox/doorbell/022")]
    #[tokio::test]
    async fn doorbell_022_an_interrupt_recorded_after_its_idle_holds_nothing() {
        let pane = TurnPane::new("escape-recorded-late", "opencode");
        pane.report("thinking").await;
        let (at_record, release) = pane
            .registry
            .pause_next_pty_write_for_test(&pane.pane, PtyWritePause::BeforeRecord);
        let write = pane.keys_in_background(&[0x1b, 0x1b]);
        reached(at_record, "the write never reached its record").await;
        pane.report("idle").await;
        release.send(()).unwrap();
        write.await.unwrap();
        let holds = pane.holds();
        pane.registry.shutdown_all();
        assert!(
            !holds,
            "an interrupt recorded after the Idle that ended its turn held the doorbell"
        );
    }

    /// Scenario: The agent reports Thinking and the user presses Escape twice,
    /// and the agent's Idle is ingested before the PTY write: once with the
    /// whole write parked before it, and once while the second part of a
    /// write the PTY took one byte at a time is still in progress (three
    /// Escapes, the last two after the Idle). Escapes that reach an agent
    /// already Idle open a menu, so in both the Escapes must hold the doorbell.
    #[spec("inbox/doorbell/023")]
    #[tokio::test]
    async fn doorbell_023_escapes_written_after_the_idle_keep_the_menu_held() {
        let pane = TurnPane::new("escape-written-late", "opencode");
        pane.report("thinking").await;
        let (at_write, release) = pane
            .registry
            .pause_next_pty_write_for_test(&pane.pane, PtyWritePause::BeforeWrite);
        let write = pane.keys_in_background(&[0x1b, 0x1b]);
        reached(at_write, "the write never reached the PTY").await;
        pane.report("idle").await;
        release.send(()).unwrap();
        write.await.unwrap();
        let whole_write_holds = pane.holds();
        pane.registry.shutdown_all();

        let pane = TurnPane::new("escape-partial-write", "opencode");
        let second_part = Arc::new(SecondPartGate::default());
        let _displaced = pane
            .registry
            .replace_agent_writer_for_test(
                &pane.agent,
                Box::new(OneByteThenGated {
                    calls: 0,
                    gate: second_part.clone(),
                }),
            )
            .await;
        pane.report("thinking").await;
        let write = pane.keys_in_background(&[0x1b, 0x1b, 0x1b]);
        second_part.wait_until_reached().await;
        pane.report("idle").await;
        second_part.release();
        write.await.unwrap();
        let partial_write_holds = pane.holds();
        pane.registry.shutdown_all();

        assert!(
            whole_write_holds,
            "Escapes written after the Idle were read as the ended turn's interrupt"
        );
        assert!(
            partial_write_holds,
            "the part of a write the PTY took after the Idle was read as the \
             ended turn's interrupt"
        );
    }

    /// A writer that takes one byte on its first call, then parks its second
    /// call until [`SecondPartGate::release`] and takes the rest.
    struct OneByteThenGated {
        calls: usize,
        gate: Arc<SecondPartGate>,
    }

    impl std::io::Write for OneByteThenGated {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == 1 {
                return Ok(buf.len().min(1));
            }
            if self.calls == 2 {
                self.gate.park();
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct SecondPartGate {
        state: std::sync::Mutex<(bool, bool)>,
        wake: std::sync::Condvar,
    }

    impl SecondPartGate {
        fn park(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.wake.notify_all();
            while !state.1 {
                state = self.wake.wait(state).unwrap();
            }
        }

        async fn wait_until_reached(self: &Arc<Self>) {
            let gate = self.clone();
            tokio::task::spawn_blocking(move || {
                let state = gate.state.lock().unwrap();
                let (state, timeout) = gate
                    .wake
                    .wait_timeout_while(state, Duration::from_secs(10), |state| !state.0)
                    .unwrap();
                assert!(
                    !timeout.timed_out() && state.0,
                    "the write's second part never started"
                );
            })
            .await
            .unwrap();
        }

        fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.wake.notify_all();
        }
    }

    /// Scenario: For each of Claude Code, Codex, OpenCode and Pi, a freshly
    /// started agent reports Thinking before the user has typed anything into
    /// its pane, the user presses Escape twice, and the agent reports Idle.
    /// That first turn counts, so the interrupt must not hold the doorbell.
    #[spec("inbox/doorbell/024")]
    #[tokio::test]
    async fn doorbell_024_a_first_turn_reported_before_any_input_is_a_turn() {
        for agent_type in ["claude_code", "codex", "opencode", "pi"] {
            let pane = TurnPane::new(&format!("fresh-stream-{agent_type}"), agent_type);
            pane.report("thinking").await;
            pane.keys(&[0x1b, 0x1b]).await;
            pane.report("idle").await;
            let holds = pane.holds();
            pane.registry.shutdown_all();
            assert!(
                !holds,
                "{agent_type}: the first turn's Thinking, reported before any \
                 input, was lost and its interrupt held the doorbell"
            );
        }
    }

    /// Scenario: An agent is started on a pane and reports Thinking while its
    /// start is still being registered, before the deck publishes it. Once
    /// it is published, the user presses Escape twice and the agent reports
    /// Idle: the early Thinking began that turn, so the interrupt must not
    /// hold the doorbell.
    #[spec("inbox/doorbell/025")]
    #[tokio::test]
    async fn doorbell_025_a_turn_reported_before_publication_is_kept() {
        let temp = crate::test_temp::tempdir().unwrap();
        let identity = temp.path().join("identity");
        let script = temp.path().join("reserved-agent.sh");
        crate::test_isolation::write_script(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$DOT_AGENT_DECK_AGENT_ID\" > \"$TURN_TEST_IDENTITY\"\nexec /bin/cat\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry = Arc::new(AgentPtyRegistry::new());
        let (forked, publish) = registry.pause_next_publish_for_test();
        let spawning = tokio::task::spawn_blocking({
            let registry = registry.clone();
            let identity = identity.clone();
            move || {
                registry.spawn_agent(SpawnOptions {
                    command: Some(script.to_str().unwrap()),
                    env: vec![
                        (
                            DOT_AGENT_DECK_PANE_ID.into(),
                            "busy-before-publication".into(),
                        ),
                        (
                            "TURN_TEST_IDENTITY".into(),
                            identity.to_string_lossy().into_owned(),
                        ),
                    ],
                    ..Default::default()
                })
            }
        });
        reached(forked, "the child never forked").await;
        let agent = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(text) = std::fs::read_to_string(&identity)
                    && text.ends_with('\n')
                {
                    break text.trim().to_string();
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the child never wrote its id"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .await
        .unwrap();
        let pane = TurnPane::with(registry, "busy-before-publication", agent, "opencode");
        assert!(
            pane.registry.agent_writer(&pane.agent).is_none(),
            "precondition: the agent is not published yet"
        );
        pane.report("thinking").await;
        publish.send(()).unwrap();
        let published = spawning.await.unwrap().expect("the spawn publishes");
        assert_eq!(published, pane.agent);
        pane.keys(&[0x1b, 0x1b]).await;
        pane.report("idle").await;
        let holds = pane.holds();
        pane.registry.shutdown_all();
        assert!(
            !holds,
            "a Thinking reported before publication was lost and the first \
             turn's interrupt held the doorbell"
        );
    }

    /// Scenario: The agent reports Thinking and the user's interrupt arrives
    /// as two separate Escape writes; the agent reports Idle and the
    /// interrupt must not hold the doorbell. On an Idle agent two separate
    /// Escape writes are a menu and hold it. And when the first of two
    /// Escapes is recorded only after the Idle that ended its turn (it
    /// reached the agent before), the second Escape, written after, does not
    /// pair with it and holds nothing.
    #[spec("inbox/doorbell/026")]
    #[tokio::test]
    async fn doorbell_026_split_escapes_pair_only_within_one_turn() {
        let pane = TurnPane::new("split-interrupt", "opencode");
        pane.report("thinking").await;
        pane.keys(&[0x1b]).await;
        pane.keys(&[0x1b]).await;
        pane.report("idle").await;
        let interrupt_holds = pane.holds();
        pane.keys(&[0x1b]).await;
        pane.keys(&[0x1b]).await;
        let menu_holds = pane.holds();
        pane.registry.shutdown_all();

        let pane = TurnPane::new("split-across-the-idle", "opencode");
        pane.report("thinking").await;
        let (at_record, release) = pane
            .registry
            .pause_next_pty_write_for_test(&pane.pane, PtyWritePause::BeforeRecord);
        let write = pane.keys_in_background(&[0x1b]);
        reached(at_record, "the first Escape never reached its record").await;
        pane.report("idle").await;
        release.send(()).unwrap();
        write.await.unwrap();
        pane.keys(&[0x1b]).await;
        let across_holds = pane.holds();
        pane.registry.shutdown_all();

        assert!(
            !interrupt_holds,
            "an interrupt split across two writes held the doorbell"
        );
        assert!(
            menu_holds,
            "two Escape writes on an Idle agent are a menu and must hold"
        );
        assert!(
            !across_holds,
            "an Escape after the Idle paired with one that reached the agent \
             before it"
        );
    }

    /// Scenario: The agent reports Thinking and the user presses Escape
    /// twice. The PTY takes both keys, and the agent's Idle — the turn the
    /// Escapes interrupted ending — is ingested after the write returned but
    /// before the deck reads the turn count for those keys. The deck cannot
    /// tell that Idle from one that came before the keys, so the Escapes
    /// must hold the doorbell until the user's Enter — pressed twice, since
    /// an Enter right after an Escape reads as Alt+Enter.
    #[spec("inbox/doorbell/027")]
    #[tokio::test]
    async fn doorbell_027_an_idle_between_the_write_and_its_count_holds_the_doorbell() {
        let pane = TurnPane::new("escape-idle-before-count", "opencode");
        pane.report("thinking").await;
        let (after_write, release) = pane
            .registry
            .pause_next_pty_write_for_test(&pane.pane, PtyWritePause::AfterWrite);
        let write = pane.keys_in_background(&[0x1b, 0x1b]);
        reached(after_write, "the write never returned").await;
        pane.report("idle").await;
        release.send(()).unwrap();
        write.await.unwrap();
        let held = pane.holds();
        pane.keys(b"\r").await;
        pane.keys(b"\r").await;
        let after_enter = pane.holds();
        pane.registry.shutdown_all();
        assert!(
            held,
            "an Idle ingested between the write and its turn-count reading \
             released the Escapes"
        );
        assert!(!after_enter, "the user's Enter must release them");
    }

    /// Scenario: Agent A is sent keys, and its PTY takes them, but the deck
    /// has not recorded them yet when A exits on its own and agent B replaces
    /// it on the pane. B finishes a turn and the user presses Escape twice
    /// on Idle B, which holds the doorbell, and A's Escapes, recorded late,
    /// must not release B's. When A's late keys are text instead, B, with
    /// nothing typed, must hold nothing, since A's keys are not B's input.
    #[spec("inbox/doorbell/028")]
    #[tokio::test]
    async fn doorbell_028_a_replaced_agents_late_keys_do_not_reach_its_successors_input() {
        async fn late_keys_after_handover(
            pane_id: &str,
            late: &'static [u8],
            successor: impl AsyncFnOnce(&TurnPane),
        ) -> bool {
            let temp = crate::test_temp::tempdir().unwrap();
            let exit_file = temp.path().join("exit");
            let script = temp.path().join("exits-on-request.sh");
            crate::test_isolation::write_script(
                &script,
                "#!/bin/sh\nwhile [ ! -e \"$EXIT_FILE\" ]; do sleep 0.01; done\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let registry = Arc::new(AgentPtyRegistry::new());
            let old = registry
                .spawn_agent(SpawnOptions {
                    command: Some(script.to_str().unwrap()),
                    env: vec![
                        (DOT_AGENT_DECK_PANE_ID.into(), pane_id.into()),
                        ("EXIT_FILE".into(), exit_file.to_string_lossy().into_owned()),
                    ],
                    ..Default::default()
                })
                .expect("spawn the agent that exits");
            let old_pane = TurnPane::with(registry.clone(), pane_id, old, "opencode");
            let (at_record, release) =
                registry.pause_next_pty_write_for_test(pane_id, PtyWritePause::BeforeRecord);
            let write = old_pane.keys_in_background(late);
            reached(at_record, "the old agent's write never reached its record").await;
            std::fs::write(&exit_file, b"").unwrap();
            wait_until_nothing_lives(&registry).await;
            let new = spawn(&registry, Some(pane_id), "cat");
            let pane = TurnPane::with(registry.clone(), pane_id, new, "opencode");
            successor(&pane).await;
            release.send(()).unwrap();
            write.await.unwrap();
            let holds = pane.holds();
            registry.shutdown_all();
            holds
        }

        let menu_holds = late_keys_after_handover(
            "late-escapes-after-handover",
            &[0x1b, 0x1b],
            async |pane: &TurnPane| {
                pane.report("thinking").await;
                pane.report("idle").await;
                pane.keys(&[0x1b, 0x1b]).await;
                assert!(pane.holds(), "precondition: the successor's menu holds");
            },
        )
        .await;
        let text_holds =
            late_keys_after_handover("late-text-after-handover", b"a", async |_: &TurnPane| {})
                .await;

        assert!(
            menu_holds,
            "the replaced agent's Escapes, recorded late, released its \
             successor's menu"
        );
        assert!(
            !text_holds,
            "the replaced agent's keys, recorded late, were taken as its \
             successor's draft"
        );
    }

    /// Scenario: For each of Claude Code, Codex, OpenCode and Pi, the agent
    /// reports Thinking and the user presses Escape twice. An Idle that
    /// `dot-agent-deck wrap` classified from the agent's output then arrives,
    /// as a wrapped Codex's quiet output sends it. Output going quiet is not
    /// the agent reporting its turn over, so the Escapes must keep holding
    /// the doorbell; the agent's own Idle then releases them.
    #[spec("inbox/doorbell/029")]
    #[tokio::test]
    async fn doorbell_029_a_wrapper_classified_idle_ends_no_reported_turn() {
        for agent_type in ["claude_code", "codex", "opencode", "pi"] {
            let pane = TurnPane::new(&format!("classified-idle-{agent_type}"), agent_type);
            pane.report("thinking").await;
            pane.keys(&[0x1b, 0x1b]).await;
            pane.report_classified("idle").await;
            let after_classified = pane.holds();
            pane.report("idle").await;
            let after_own = pane.holds();
            pane.registry.shutdown_all();

            assert!(
                after_classified,
                "{agent_type}: a wrapper-classified Idle ended the turn the \
                 agent reported, releasing its Escapes"
            );
            assert!(
                !after_own,
                "{agent_type}: the agent's own Idle must release them"
            );
        }
    }

    /// Scenario: For each of Claude Code, Codex, OpenCode and Pi, the agent
    /// finishes a turn (Thinking, then Idle) and the user presses Escape
    /// twice on the Idle agent — a menu. A ToolEnd of that finished turn
    /// then arrives late, followed by another Idle. A tool finishing starts
    /// no turn, so the Escapes must keep holding the doorbell; the agent's
    /// next Thinking then Idle releases them.
    #[spec("inbox/doorbell/030")]
    #[tokio::test]
    async fn doorbell_030_a_late_tool_end_starts_no_turn() {
        for agent_type in ["claude_code", "codex", "opencode", "pi"] {
            let pane = TurnPane::new(&format!("late-tool-end-{agent_type}"), agent_type);
            pane.report("thinking").await;
            pane.report("idle").await;
            pane.keys(&[0x1b, 0x1b]).await;
            pane.report("tool_end").await;
            pane.report("idle").await;
            let after_late_tool_end = pane.holds();
            pane.report("thinking").await;
            pane.report("idle").await;
            let after_turn = pane.holds();
            pane.registry.shutdown_all();

            assert!(
                after_late_tool_end,
                "{agent_type}: a late ToolEnd then Idle released a menu's Escapes"
            );
            assert!(
                !after_turn,
                "{agent_type}: the agent's next turn ending must release them"
            );
        }
    }

    /// Scenario: Agent A exits and agent B replaces it on a pane, and an
    /// attached client already shows B's card. A's reports with no agent id —
    /// a running report under B's card key, one under a key of A's own, and a
    /// session end under B's card key — are ingested as if the replacement
    /// happened just after the under-lock check passed, and relayed to the
    /// client. B's card on the client must keep its status and must not be
    /// removed.
    #[tokio::test]
    async fn hook_provenance_audit_untagged_relay_cannot_reach_a_successors_client_card() {
        const PANE: &str = "audit-relay-pane";
        const SHARED: &str = "audit-relay-pane-session";
        let registry = Arc::new(AgentPtyRegistry::new());
        let old = spawn(&registry, Some(PANE), "/usr/bin/true");
        wait_until_nothing_lives(&registry).await;
        let new = spawn(&registry, Some(PANE), "cat");
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let mut client = AppState::default();
        client.register_pane(PANE.to_string());
        client.apply_event(event_of(line(
            SHARED,
            Some(PANE),
            Some(&new),
            "session_start",
            None,
        )));

        for (session, kind) in [
            (SHARED, "thinking"),
            ("audit-relay-old-key", "thinking"),
            (SHARED, "session_end"),
        ] {
            ingest_event_unless(
                &state,
                &event_tx,
                &registry,
                event_of(line(session, Some(PANE), None, kind, None)),
                false,
                Some(&old),
                |_| false,
            )
            .await;
            let mut relayed = 0;
            while let Ok(message) = events.try_recv() {
                if let BroadcastMsg::Event(event) = message {
                    relayed += 1;
                    client.apply_event(event);
                }
            }
            assert_eq!(relayed, 1, "{session}/{kind}: precondition: relayed");
            let card = client.sessions.get(SHARED);
            assert_eq!(
                card.map(|card| (&card.status, card.agent_id.as_deref())),
                Some((&SessionStatus::Idle, Some(new.as_str()))),
                "{session}/{kind}: a replaced generation's untagged report must \
                 not drive or remove its successor's card on a client"
            );
            assert_eq!(
                client.sessions.len(),
                1,
                "{session}/{kind}: no card may be drawn"
            );
        }
        registry.shutdown_all();
    }

    /// Scenario: Agent A exits on a pane with no successor, and an attached
    /// client shows A's card. A's running report and session end, attested to
    /// A's token but naming no agent, are relayed to the client. The report
    /// must update A's card and the end must remove it, as before the
    /// attested-owner marker existed.
    #[tokio::test]
    async fn hook_provenance_audit_untagged_relay_still_reaches_its_own_client_card() {
        const PANE: &str = "audit-relay-lone-pane";
        const CARD: &str = "audit-relay-lone-card";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = spawn(&registry, Some(PANE), "/usr/bin/true");
        wait_until_nothing_lives(&registry).await;
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let mut client = AppState::default();
        client.register_pane(PANE.to_string());
        client.apply_event(event_of(line(
            CARD,
            Some(PANE),
            Some(&agent),
            "session_start",
            None,
        )));
        let mut relay = |client: &mut AppState| {
            while let Ok(message) = events.try_recv() {
                if let BroadcastMsg::Event(event) = message {
                    assert_eq!(event.attested_owner(), Some(agent.as_str()));
                    client.apply_event(event);
                }
            }
        };
        ingest_hook_event(
            &state,
            &event_tx,
            &registry,
            event_of(line(CARD, Some(PANE), None, "thinking", None)),
            false,
            Some(agent.clone()),
        )
        .await;
        relay(&mut client);
        assert_eq!(
            client.sessions.get(CARD).map(|card| card.status.clone()),
            Some(SessionStatus::Thinking),
            "a lone retiree's untagged report must still reach its own card"
        );
        ingest_hook_event(
            &state,
            &event_tx,
            &registry,
            event_of(line(CARD, Some(PANE), None, "session_end", None)),
            false,
            Some(agent.clone()),
        )
        .await;
        relay(&mut client);
        assert!(
            !client.sessions.contains_key(CARD),
            "a lone retiree's untagged end must still remove its own card"
        );
        registry.shutdown_all();
    }

    /// Scenario: Agent A exits and agent B replaces it on a pane, but an
    /// attached client still shows A's card. B's running reports with no agent
    /// id — under A's card key, and under a fresh key — are attested to B and
    /// relayed. The client refuses both and keeps A's card as it was; a report
    /// from B that names B then reaches the client and draws B's card.
    ///
    /// This pins a deliberate fail-closed trade rather than a defect: accepting
    /// an untagged report onto a card of another agent's would reopen the
    /// late-A-onto-B case `..._cannot_reach_a_successors_client_card` closes,
    /// and the client has no registry to tell the two directions apart. The
    /// recovery is a tagged report (`DOT_AGENT_DECK_AGENT_ID` in the agent's
    /// environment) or a client resynchronisation, as
    /// `docs/develop/hook-provenance.md` records.
    #[tokio::test]
    async fn hook_provenance_audit_current_owner_untagged_report_is_refused_on_a_predecessors_client_card()
     {
        const PANE: &str = "audit-owner-pane";
        const SHARED: &str = "audit-owner-pane-session";
        const FRESH: &str = "audit-owner-fresh-key";
        let registry = Arc::new(AgentPtyRegistry::new());
        let old = spawn(&registry, Some(PANE), "/usr/bin/true");
        wait_until_nothing_lives(&registry).await;
        let new = spawn(&registry, Some(PANE), "cat");
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let mut client = AppState::default();
        client.register_pane(PANE.to_string());
        client.apply_event(event_of(line(
            SHARED,
            Some(PANE),
            Some(&old),
            "session_start",
            None,
        )));

        for session in [SHARED, FRESH] {
            ingest_hook_event(
                &state,
                &event_tx,
                &registry,
                event_of(line(session, Some(PANE), None, "thinking", None)),
                false,
                Some(new.clone()),
            )
            .await;
            let mut relayed = 0;
            while let Ok(message) = events.try_recv() {
                if let BroadcastMsg::Event(event) = message {
                    relayed += 1;
                    assert_eq!(event.attested_owner(), Some(new.as_str()));
                    client.apply_event(event);
                }
            }
            assert_eq!(
                relayed, 1,
                "{session}: the daemon admits the owner's report"
            );
            assert_eq!(
                client
                    .sessions
                    .get(SHARED)
                    .map(|card| (&card.status, card.agent_id.as_deref())),
                Some((&SessionStatus::Idle, Some(old.as_str()))),
                "{session}: the client refuses an untagged report onto another \
                 agent's card, even from the pane's current owner"
            );
            assert_eq!(client.sessions.len(), 1, "{session}: no card may be drawn");
        }

        ingest_hook_event(
            &state,
            &event_tx,
            &registry,
            event_of(line(SHARED, Some(PANE), Some(&new), "thinking", None)),
            false,
            Some(new.clone()),
        )
        .await;
        while let Ok(message) = events.try_recv() {
            if let BroadcastMsg::Event(event) = message {
                client.apply_event(event);
            }
        }
        assert!(
            client.sessions.values().any(|card| {
                card.agent_id.as_deref() == Some(new.as_str())
                    && card.status == SessionStatus::Thinking
            }),
            "a report naming the owner is the recovery: {:?}",
            client.sessions
        );
        registry.shutdown_all();
    }

    /// Scenario: A producer puts its own attested-owner marker on a report.
    /// Ingested without a token, the broadcast frame carries no marker; ingested
    /// attested to agent A, it carries A and not the producer's value.
    #[tokio::test]
    async fn hook_provenance_audit_attested_owner_marker_is_the_daemons_alone() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let forged = || {
            let mut payload = line("forged-card", Some("forged-pane"), None, "thinking", None);
            payload["metadata"] = serde_json::json!({
                crate::event::ATTESTED_OWNER_METADATA_KEY: "forged-owner",
            });
            event_of(payload)
        };
        for (unproven, attested, expected) in [
            (true, None, None),
            (false, None, None),
            (false, Some("agent-a"), Some("agent-a")),
        ] {
            ingest_event_unless(
                &state,
                &event_tx,
                &registry,
                forged(),
                unproven,
                attested,
                |_| false,
            )
            .await;
            let Ok(BroadcastMsg::Event(event)) = events.try_recv() else {
                panic!("the report was broadcast");
            };
            assert_eq!(
                event.attested_owner(),
                expected,
                "unproven={unproven} attested={attested:?}"
            );
        }
        registry.shutdown_all();
    }

    /// Scenario: Agent A exits and agent B replaces it on a pane. A report
    /// attested to A's token that names no agent reaches the daemon's under-lock
    /// re-check after B claimed the pane. It must be dropped, broadcast nowhere,
    /// and logged with the escaped pane and the reason code, never silently.
    #[tokio::test]
    async fn hook_provenance_audit_late_replaced_untagged_report_is_logged() {
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for CapturedLog {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
            type Writer = CapturedLog;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
            .with_ansi(false)
            .finish();
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        const PANE: &str = "audit-late-replaced-pane";
        let registry = Arc::new(AgentPtyRegistry::new());
        let old = spawn(&registry, Some(PANE), "/usr/bin/true");
        let old_token = registry.hook_token_of(&old).expect("old token");
        wait_until_nothing_lives(&registry).await;
        let _new = spawn(&registry, Some(PANE), "cat");
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        ingest_hook_event(
            &state,
            &event_tx,
            &registry,
            event_of(line("late-card", Some(PANE), None, "thinking", None)),
            false,
            Some(old.clone()),
        )
        .await;
        registry.shutdown_all();
        assert!(
            events.try_recv().is_err(),
            "a dropped report reaches no client"
        );
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let line = log
            .lines()
            .find(|line| line.contains("token_generation_replaced"))
            .unwrap_or_else(|| panic!("no refusal logged: {log}"));
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("verb=\"agent_event\""), "{line}");
        assert!(line.contains("Thinking"), "{line}");
        assert!(line.contains(PANE), "{line}");
        assert!(
            !log.contains(&old_token),
            "the token is never logged: {log}"
        );
    }
}

/// PRD #1631: a producer holding a question on the real hook loop.
#[cfg(all(test, unix))]
mod question_hold_tests {
    use super::*;
    use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
    use crate::event::{AgentEvent, AgentType, EventType};
    use crate::question::{QuestionAnswer, QuestionReply, ReleaseReason, ReplyOutcome};
    use spec::spec;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    const PANE: &str = "pane-held-question";
    const SESSION: &str = "held-question-session";

    fn question_event(agent_id: &str, id: &str) -> AgentEvent {
        let mut event = AgentEvent {
            session_id: SESSION.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::PermissionRequest,
            tool_name: Some("Bash".to_string()),
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(PANE.to_string()),
            agent_id: Some(agent_id.to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
            background: None,
            background_revision: None,
        };
        let question = crate::question::claude_permission_request(
            id.to_string(),
            "Bash",
            None,
            Some("touch x".into()),
            None,
            None,
            1,
            None,
        )
        .question;
        event.set_question(&question);
        event
    }

    async fn wait_for(
        state: &SharedState,
        what: &str,
        test: impl Fn(&crate::state::AppState) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !test(&*state.read().await) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Scenario: On the real hook loop, a producer holds a question. When its
    /// connection closes with no answer — Claude Code killing its hook on a
    /// keyboard No — the daemon drops the question and leaves the card on Needs
    /// Input for the next event to decide. A second producer holds the next
    /// question, the deck answers it, and the answer arrives on that producer's
    /// connection as one `answered` line.
    #[spec("question/hold/003")]
    #[tokio::test]
    async fn question_hold_003_a_closed_hold_clears_and_an_open_one_hears_the_answer() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        let token = registry.hook_token_of(&agent).expect("a hook token");
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hook.sock");
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(PANE.to_string());
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move {
                run_hook_loop(
                    listener,
                    state,
                    event_tx,
                    registry,
                    Arc::new(Notify::new()),
                    wtr,
                )
                .await
            }
        });
        let hold = |id: &str| {
            let msg =
                crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
                    pane_id: PANE.to_string(),
                    token: Some(token.clone()),
                    event: question_event(&agent, id),
                    hold: true,
                    ack: false,
                    confirm_hold: false,
                }));
            let sock = sock.clone();
            async move {
                let mut stream = UnixStream::connect(&sock).await.expect("connect");
                let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
                stream.write_all(line.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
                stream
            }
        };
        let pending = |id: &'static str| {
            move |s: &crate::state::AppState| {
                s.pending_question_id_on_pane(PANE).as_deref() == Some(id)
            }
        };

        let first = hold("q-first").await;
        wait_for(
            &state,
            "the first question to be pending",
            pending("q-first"),
        )
        .await;
        assert!(registry.question_holds().is_held(PANE, "q-first"));
        drop(first);
        wait_for(&state, "the closed hold to clear its question", |s| {
            s.pending_question_id_on_pane(PANE).is_none()
        })
        .await;
        assert_eq!(
            state.read().await.sessions[SESSION].status,
            crate::state::SessionStatus::WaitingForInput,
            "a closed hold says nothing about where the agent went"
        );
        assert!(!registry.question_holds().is_held(PANE, "q-first"));

        let second = hold("q-second").await;
        wait_for(
            &state,
            "the second question to be pending",
            pending("q-second"),
        )
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !registry.question_holds().is_held(PANE, "q-second") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the hold never registered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        crate::daemon_protocol::answer_question(
            &registry,
            &state,
            &event_tx,
            &agent,
            "q-second",
            &[QuestionAnswer {
                question_index: 0,
                option_indices: vec![1],
                text: None,
            }],
            false,
        )
        .await
        .expect("the deck answers the held question");
        let mut reader = tokio::io::BufReader::new(second);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .expect("the answer arrives")
            .unwrap();
        let reply: QuestionReply = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(reply.outcome, ReplyOutcome::Answered);
        assert_eq!(reply.question_id, "q-second");
        assert!(reply.answers_for("q-second").is_some());

        handle.abort();
        registry.shutdown_all();
    }

    /// Scenario: On the real hook loop, a held question whose producer asks
    /// the daemon to confirm the hold gets a `held` line as soon as the hold
    /// is registered, and the reply that ends the hold after it. A producer
    /// that does not ask gets no `held` line: the first line it reads is the
    /// reply that ends its hold.
    #[spec("question/hold/022")]
    #[tokio::test]
    async fn question_hold_022_a_producer_that_asks_is_told_the_hold_started() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        let token = registry.hook_token_of(&agent).expect("a hook token");
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hook.sock");
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(PANE.to_string());
        let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move {
                run_hook_loop(
                    listener,
                    state,
                    event_tx,
                    registry,
                    Arc::new(Notify::new()),
                    wtr,
                )
                .await
            }
        });
        let hold = |id: &str, confirm_hold: bool| {
            let msg =
                crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
                    pane_id: PANE.to_string(),
                    token: Some(token.clone()),
                    event: question_event(&agent, id),
                    hold: true,
                    ack: false,
                    confirm_hold,
                }));
            let sock = sock.clone();
            async move {
                let mut stream = UnixStream::connect(&sock).await.expect("connect");
                let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
                stream.write_all(line.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
                tokio::io::BufReader::new(stream)
            }
        };
        async fn next_reply(
            reader: &mut tokio::io::BufReader<UnixStream>,
            what: &str,
        ) -> QuestionReply {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                .await
                .unwrap_or_else(|_| panic!("{what} arrives"))
                .unwrap();
            serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("{what}: {e}: {line:?}"))
        }

        let mut confirmed = hold("q-confirmed", true).await;
        assert_eq!(
            next_reply(&mut confirmed, "the held line").await,
            QuestionReply::held("q-confirmed")
        );
        assert!(registry.question_holds().is_held(PANE, "q-confirmed"));

        let mut unconfirmed = hold("q-unconfirmed", false).await;
        assert_eq!(
            next_reply(&mut confirmed, "the superseded release").await,
            QuestionReply::released("q-confirmed", ReleaseReason::Superseded),
            "the reply that ends a confirmed hold follows its held line"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !registry.question_holds().is_held(PANE, "q-unconfirmed") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the unconfirmed hold never registered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let mut newest = hold("q-newest", true).await;
        assert_eq!(
            next_reply(&mut unconfirmed, "the unconfirmed hold's first line").await,
            QuestionReply::released("q-unconfirmed", ReleaseReason::Superseded),
            "a producer that did not ask is never sent a held line"
        );
        assert_eq!(
            next_reply(&mut newest, "the newest held line").await,
            QuestionReply::held("q-newest")
        );

        drop(newest);
        handle.abort();
        registry.shutdown_all();
    }

    /// The status event and the pending question one producer report yields on
    /// the real hook loop: `raw` parsed by the hook CLI's one parser
    /// ([`crate::hook::hook_payload_event`]) for `agent`, held by the daemon,
    /// and read back — the frame the status stream carried for the pane while
    /// the question was still held, and the pane's pending question.
    async fn one_parse_on_the_hook_loop(
        agent_label: &str,
        raw: &str,
    ) -> (AgentEvent, crate::question::PendingQuestion) {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        let token = registry.hook_token_of(&agent).expect("a hook token");
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hook.sock");
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(PANE.to_string());
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move {
                run_hook_loop(
                    listener,
                    state,
                    event_tx,
                    registry,
                    Arc::new(Notify::new()),
                    wtr,
                )
                .await
            }
        });

        // What the hook process builds; the pane and agent come from the
        // environment the deck spawned it with, stood in for here.
        let mut event = crate::hook::hook_payload_event(agent_label, raw)
            .expect("the payload maps to an event");
        event.pane_id = Some(PANE.to_string());
        event.agent_id = Some(agent.clone());
        let raised = event.question().expect("the same parse raised a question");
        let msg = crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
            pane_id: PANE.to_string(),
            token: Some(token),
            event,
            hold: true,
            ack: false,
            confirm_hold: false,
        }));
        let mut producer = UnixStream::connect(&sock).await.expect("connect");
        let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
        producer.write_all(line.as_bytes()).await.unwrap();
        producer.flush().await.unwrap();

        let announced = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Ok(BroadcastMsg::Event(e)) if e.pane_id.as_deref() == Some(PANE) => break e,
                    Ok(_) => {}
                    Err(e) => panic!("the status stream ended: {e}"),
                }
            }
        })
        .await
        .expect("the status stream carries the prompt while it is held");
        wait_for(&state, "the question to be pending", |s| {
            s.pending_question_id_on_pane(PANE).as_deref() == Some(raised.id.as_str())
        })
        .await;
        assert!(
            registry.question_holds().is_held(PANE, &raised.id),
            "the producer is still holding the question"
        );
        let pending = state
            .read()
            .await
            .pending_question_on_pane(PANE)
            .expect("a pending question");
        let session = state.read().await.sessions[&announced.session_id]
            .status
            .clone();
        assert_eq!(session, crate::state::SessionStatus::WaitingForInput);
        drop(producer);
        handle.abort();
        registry.shutdown_all();
        (announced, pending)
    }

    /// Scenario: One Claude Code `PermissionRequest` payload, and one OpenCode
    /// `permission.asked` as the deck's plugin hands it to `await-answer`, are
    /// each parsed once by the hook CLI and held on the real hook loop. While
    /// the hook is still held, the status stream carries a `PermissionRequest`
    /// naming the tool and what it would run — what reading mode announces —
    /// and the pane's pending question is the question that same frame carries,
    /// about the same tool, with the card on Needs Input.
    #[spec("question/detect/012")]
    #[tokio::test]
    async fn question_detect_012_one_parse_yields_the_announced_status_and_the_question() {
        let fixture = |name: &str| {
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/agent-questions")
                    .join(name),
            )
            .expect("read fixture")
        };
        let opencode = serde_json::json!({
            "session_id": "ses_efbc02f39ffeP3cw5BFH7xv6VM",
            "event": "permission.asked",
            "properties": serde_json::from_str::<serde_json::Value>(
                &fixture("opencode-permission-asked.json")
            )
            .unwrap(),
            "cwd": "/work",
        })
        .to_string();
        for (agent_label, raw, tool, detail) in [
            (
                "claude-code",
                fixture("claude-permission-bash.json"),
                "Bash",
                "touch created_m1.txt",
            ),
            ("opencode", opencode, "bash", "touch oc_a.txt"),
        ] {
            let (announced, pending) = one_parse_on_the_hook_loop(agent_label, &raw).await;
            // What reading mode reads off the status stream: the type, and the
            // tool and detail it says the agent wants.
            assert_eq!(
                announced.event_type,
                EventType::PermissionRequest,
                "{agent_label}"
            );
            assert!(
                !announced
                    .metadata
                    .contains_key(crate::event::UNPROVEN_METADATA_KEY),
                "{agent_label}: the pane's own report, which reading mode reads"
            );
            assert_eq!(announced.tool_name.as_deref(), Some(tool), "{agent_label}");
            assert_eq!(
                announced.tool_detail.as_deref(),
                Some(detail),
                "{agent_label}"
            );
            // The snapshot's question is the one that frame carried.
            assert_eq!(
                announced.question().as_ref(),
                Some(&pending),
                "{agent_label}"
            );
            let asked = pending.tool.as_ref().expect("the question names its tool");
            assert_eq!(asked.name, tool, "{agent_label}");
            assert_eq!(asked.detail.as_deref(), Some(detail), "{agent_label}");
        }
    }

    /// Scenario: On the real hook loop, a producer holds a question and an
    /// attached client mirrors the daemon's event stream. The producer's
    /// connection closes with no answer (Claude Code killing its hook on a
    /// keyboard No); the daemon tells every attached client, so the mirror drops
    /// the question too and keeps Needs Input, and the frame it sent is an
    /// `unknown` event an older client reads as a no-op.
    #[spec("question/hold/004")]
    #[tokio::test]
    async fn question_hold_004_a_closed_hold_is_broadcast_without_moving_the_status() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        let token = registry.hook_token_of(&agent).expect("a hook token");
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hook.sock");
        let listener =
            IpcListener::from_tokio_listener(UnixListener::bind(&sock).expect("bind hook socket"));
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(PANE.to_string());
        let (event_tx, mut events) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            let wtr = crate::issue_dispatch_run::new_worktree_registry();
            async move {
                run_hook_loop(
                    listener,
                    state,
                    event_tx,
                    registry,
                    Arc::new(Notify::new()),
                    wtr,
                )
                .await
            }
        });

        let msg = crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
            pane_id: PANE.to_string(),
            token: Some(token.clone()),
            event: question_event(&agent, "q-gone"),
            hold: true,
            ack: false,
            confirm_hold: false,
        }));
        let mut producer = UnixStream::connect(&sock).await.expect("connect");
        let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
        producer.write_all(line.as_bytes()).await.unwrap();
        producer.flush().await.unwrap();

        // The attached client: an `AppState` fed only what the daemon fans out.
        let mut mirror = crate::state::AppState::default();
        mirror.register_pane(PANE.to_string());
        let mut next_frame = async || loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("the daemon fans the frame out")
                .expect("broadcast open");
            if let BroadcastMsg::Event(event) = msg {
                return event;
            }
        };
        let raised = next_frame().await;
        assert!(raised.question().is_some());
        mirror.apply_event(raised);
        assert_eq!(
            mirror.pending_question_id_on_pane(PANE).as_deref(),
            Some("q-gone")
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !registry.question_holds().is_held(PANE, "q-gone") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the hold never registered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        drop(producer);
        let released = next_frame().await;
        assert_eq!(released.resolved_question_id(), Some("q-gone"));
        assert!(released.is_daemon_synthetic());
        let wire = serde_json::to_value(&released).unwrap();
        assert_eq!(
            wire["event_type"], "unknown",
            "an older client must read the release as a no-op"
        );
        mirror.apply_event(released);
        assert!(
            mirror.pending_question_id_on_pane(PANE).is_none(),
            "the attached client drops the question the daemon dropped"
        );
        assert_eq!(
            mirror.sessions[SESSION].status,
            crate::state::SessionStatus::WaitingForInput,
            "the release says nothing about where the agent went"
        );
        wait_for(&state, "the daemon to drop the question", |s| {
            s.pending_question_id_on_pane(PANE).is_none()
        })
        .await;
        assert_eq!(
            state.read().await.sessions[SESSION].status,
            crate::state::SessionStatus::WaitingForInput
        );

        handle.abort();
        registry.shutdown_all();
    }

    /// A hook loop with chosen bounds, and agents on panes of their own.
    struct Deck {
        registry: Arc<AgentPtyRegistry>,
        state: SharedState,
        event_tx: broadcast::Sender<BroadcastMsg>,
        sock: std::path::PathBuf,
        _dir: tempfile::TempDir,
        handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
    }

    struct Agent {
        pane: String,
        id: String,
        token: String,
    }

    impl Deck {
        async fn start(limits: HookLoopLimits) -> Self {
            let registry = Arc::new(AgentPtyRegistry::new());
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("hook.sock");
            let listener = IpcListener::from_tokio_listener(
                UnixListener::bind(&sock).expect("bind hook socket"),
            );
            let state: SharedState =
                Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
            let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let handle = tokio::spawn({
                let registry = registry.clone();
                let state = state.clone();
                let event_tx = event_tx.clone();
                let wtr = crate::issue_dispatch_run::new_worktree_registry();
                async move {
                    run_hook_loop_with_limits(
                        listener,
                        state,
                        event_tx,
                        registry,
                        Arc::new(Notify::new()),
                        wtr,
                        limits,
                    )
                    .await
                }
            });
            Self {
                registry,
                state,
                event_tx,
                sock,
                _dir: dir,
                handle,
            }
        }

        async fn agent(&self, pane: &str) -> Agent {
            let id = self
                .registry
                .spawn_agent(SpawnOptions {
                    command: Some("/bin/sh"),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn shell agent");
            let token = self.registry.hook_token_of(&id).expect("a hook token");
            self.state.write().await.register_pane(pane.to_string());
            Agent {
                pane: pane.to_string(),
                id,
                token,
            }
        }

        /// A producer holding `question_id` for `agent`, its connection open.
        async fn hold(&self, agent: &Agent, question_id: &str) -> UnixStream {
            self.hold_with(agent, question_id, "touch x").await
        }

        /// [`Self::hold`], for a Bash permission whose command is `detail` —
        /// the question's content.
        async fn hold_with(&self, agent: &Agent, question_id: &str, detail: &str) -> UnixStream {
            self.hold_signal(agent, question_id, detail, false).await
        }

        /// A producer that acknowledges (`ack`, as the OpenCode plugin's
        /// child does) holding `question_id` for `agent`.
        async fn hold_acking(&self, agent: &Agent, question_id: &str) -> UnixStream {
            self.hold_signal(agent, question_id, "touch x", true).await
        }

        async fn hold_signal(
            &self,
            agent: &Agent,
            question_id: &str,
            detail: &str,
            ack: bool,
        ) -> UnixStream {
            let mut event = self.question_on(agent, question_id, detail);
            let question = event.question().unwrap();
            event.set_question(&question);
            let msg =
                crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
                    pane_id: agent.pane.clone(),
                    token: Some(agent.token.clone()),
                    event,
                    hold: true,
                    ack,
                    confirm_hold: false,
                }));
            let mut stream = UnixStream::connect(&self.sock).await.expect("connect");
            let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            stream
        }

        /// `agent`'s Bash permission `question_id` for the command `detail`,
        /// as an event on its pane.
        fn question_on(&self, agent: &Agent, question_id: &str, detail: &str) -> AgentEvent {
            let mut event = question_event(&agent.id, question_id);
            event.pane_id = Some(agent.pane.clone());
            event.session_id = format!("{}-session", agent.pane);
            let mut question = event.question().unwrap();
            question.tool.as_mut().unwrap().detail = Some(detail.to_string());
            event.set_question(&question);
            event
        }

        /// The question pending on `agent`'s pane, whole.
        async fn pending(&self, agent: &Agent) -> Option<crate::question::PendingQuestion> {
            self.state
                .read()
                .await
                .pending_question_on_pane(&agent.pane)
        }

        /// Answer `agent`'s `question_id` naming `revision`, as a client that
        /// read that revision would.
        async fn answer_revision(
            &self,
            agent: &Agent,
            question_id: &str,
            revision: Option<u64>,
        ) -> Result<(), crate::question::AnswerRefusal> {
            crate::daemon_protocol::answer_question_at(
                &self.registry,
                &self.state,
                &self.event_tx,
                &agent.id,
                question_id,
                revision,
                &[QuestionAnswer {
                    question_index: 0,
                    option_indices: vec![1],
                    text: None,
                }],
                false,
            )
            .await
        }

        async fn until_held(&self, agent: &Agent, question_id: &str) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !self
                .registry
                .question_holds()
                .is_held(&agent.pane, question_id)
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the hold on {} never registered",
                    agent.pane
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        async fn answer(
            &self,
            agent: &Agent,
            question_id: &str,
        ) -> Result<(), crate::question::AnswerRefusal> {
            crate::daemon_protocol::answer_question(
                &self.registry,
                &self.state,
                &self.event_tx,
                &agent.id,
                question_id,
                &[QuestionAnswer {
                    question_index: 0,
                    option_indices: vec![1],
                    text: None,
                }],
                false,
            )
            .await
        }
    }

    impl Drop for Deck {
        fn drop(&mut self) {
            self.handle.abort();
            self.registry.shutdown_all();
        }
    }

    fn limits() -> HookLoopLimits {
        HookLoopLimits::production(HOOK_CONNECTION_IDLE_TIMEOUT)
    }

    async fn reply_on(stream: UnixStream) -> Option<QuestionReply> {
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => Some(serde_json::from_str(line.trim()).unwrap()),
            _ => None,
        }
    }

    /// The producer's one reply line, and whether the daemon then closed the
    /// connection: a producer that keeps its end open and writes another line
    /// is read no further (audit A6/N2).
    async fn reply_then_closed(stream: UnixStream, more: &AgentEvent) -> (QuestionReply, bool) {
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .expect("a reply")
            .expect("a reply");
        assert!(n > 0, "a reply line");
        let reply = serde_json::from_str(line.trim()).unwrap();
        // The producer misbehaves: it keeps the socket and sends more.
        let _ = reader
            .get_mut()
            .write_all(format!("{}\n", serde_json::to_string(more).unwrap()).as_bytes())
            .await;
        let mut rest = String::new();
        let closed = matches!(
            tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut rest)).await,
            Ok(Ok(0)) | Ok(Err(_))
        );
        (reply, closed)
    }

    async fn no_reply_within(stream: &mut UnixStream, budget: Duration) -> bool {
        let mut byte = [0u8; 1];
        use tokio::io::AsyncReadExt;
        tokio::time::timeout(budget, stream.read(&mut byte))
            .await
            .is_err()
    }

    /// Scenario: Two agents on two panes each wait on a question that happens
    /// to carry the same id. Answering the first agent's question reaches only
    /// the producer on its own pane; the second agent's producer keeps waiting
    /// and is still held, and an answer naming the wrong agent for a hold is
    /// never delivered.
    #[spec("question/hold/006")]
    #[tokio::test]
    async fn question_hold_006_identical_ids_on_two_panes_stay_apart() {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-a6-a").await;
        let b = deck.agent("pane-a6-b").await;
        let on_a = deck.hold(&a, "q-same").await;
        deck.until_held(&a, "q-same").await;
        let mut on_b = deck.hold(&b, "q-same").await;
        deck.until_held(&b, "q-same").await;
        assert!(
            deck.registry
                .question_holds()
                .answer(
                    &b.pane,
                    &a.id,
                    "q-same",
                    deck.registry
                        .question_holds()
                        .generation_of(&b.pane, "q-same"),
                    QuestionReply::answered("q-same", Vec::new())
                )
                .is_none(),
            "a hold is answered only for the agent that registered it"
        );
        assert!(deck.registry.question_holds().is_held(&b.pane, "q-same"));

        deck.answer(&a, "q-same").await.expect("answered");
        let reply = reply_on(on_a).await.expect("pane A's producer hears it");
        assert_eq!(reply.outcome, ReplyOutcome::Answered);
        assert!(
            no_reply_within(&mut on_b, Duration::from_millis(300)).await,
            "pane B's producer heard nothing"
        );
        assert!(deck.registry.question_holds().is_held(&b.pane, "q-same"));
        assert_eq!(
            deck.state
                .read()
                .await
                .pending_question_id_on_pane(&b.pane)
                .as_deref(),
            Some("q-same")
        );
    }

    /// Scenario: A producer holds a question, and a second registration for the
    /// same pane and question replaces it. The first producer is told it was
    /// superseded; when its handler then lets go late — its connection closing,
    /// or its deadline passing — the replacement's hold and the pending
    /// question are untouched, and the answer reaches the replacement.
    #[spec("question/hold/007")]
    #[tokio::test]
    async fn question_hold_007_a_superseded_handler_cannot_drop_its_replacement() {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-a7").await;
        let first = deck.hold(&a, "q-again").await;
        deck.until_held(&a, "q-again").await;
        let first_generation = {
            // The registration in place now is the first one; a late forget
            // naming it is what a superseded handler would do.
            let probe = deck
                .registry
                .question_holds()
                .hold("pane-probe", &a.id, "q-probe");
            probe.generation - 1
        };
        let second = deck.hold(&a, "q-again").await;
        let superseded = reply_on(first).await.expect("the first producer is let go");
        assert_eq!(
            superseded.reason,
            Some(crate::question::ReleaseReason::Superseded)
        );
        deck.until_held(&a, "q-again").await;

        drop_held_question(
            &deck.state,
            &deck.event_tx,
            &deck.registry,
            &a.pane,
            "q-again",
            first_generation,
            "a superseded handler letting go late",
        )
        .await;
        assert!(deck.registry.question_holds().is_held(&a.pane, "q-again"));
        assert_eq!(
            deck.state
                .read()
                .await
                .pending_question_id_on_pane(&a.pane)
                .as_deref(),
            Some("q-again")
        );
        deck.answer(&a, "q-again").await.expect("answered");
        let reply = reply_on(second).await.expect("the replacement hears it");
        assert_eq!(reply.outcome, ReplyOutcome::Answered);
    }

    /// Scenario: A producer holding a question goes away, and the daemon starts
    /// dropping its question. Right after it lets go of that producer's hold —
    /// before it clears the question — a new producer asks the same question id
    /// on the same pane. The new one is not even registered until the old
    /// cleanup has finished; it is then registered and shown at its own
    /// revision, nothing the old cleanup did touches it, and the deck's answer
    /// reaches it.
    #[spec("question/hold/011")]
    #[tokio::test]
    async fn question_hold_011_a_late_cleanup_cannot_clear_a_same_id_replacement() {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-a5-late").await;
        let first = deck.hold(&a, "q-late").await;
        deck.until_held(&a, "q-late").await;
        let old_revision = deck.pending(&a).await.unwrap().revision;
        let (cleaned, resume_cleanup) = deck
            .registry
            .question_answers()
            .arm_barrier("drop_held_question:forgotten");
        drop(first);
        cleaned.await.expect("the old cleanup let go of its hold");
        let (registered, resume_registration) = deck
            .registry
            .question_answers()
            .arm_barrier("question_signal:registered");
        let second = deck.hold(&a, "q-late").await;
        // Audit R3: the replacement cannot reach its registration while the
        // old cleanup is between its `forget` and its clear.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !deck.registry.question_holds().is_held(&a.pane, "q-late"),
            "the replacement registered inside the old cleanup"
        );
        resume_cleanup.send(()).unwrap();
        registered.await.expect("the replacement registers next");
        assert_eq!(
            deck.pending(&a).await,
            None,
            "the old cleanup finished before the replacement registered"
        );
        resume_registration.send(()).unwrap();
        deck.until_held(&a, "q-late").await;
        let replacement = loop {
            if let Some(q) = deck.pending(&a).await {
                break q;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_ne!(replacement.revision, old_revision);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(deck.registry.question_holds().is_held(&a.pane, "q-late"));
        assert_eq!(
            deck.pending(&a).await.map(|q| (q.id, q.revision)),
            Some(("q-late".to_string(), replacement.revision)),
            "the replacement's question survives the old cleanup"
        );
        deck.answer_revision(&a, "q-late", replacement.revision)
            .await
            .expect("answered");
        let reply = reply_on(second).await.expect("the replacement hears it");
        assert_eq!(reply.outcome, ReplyOutcome::Answered);
    }

    /// Scenario: A producer asks a question and is registered, but is delayed
    /// before its question is shown; a second producer asks the same question
    /// id on the same pane with a different command. The second waits until
    /// the first is shown, then replaces it: the first producer is let go, the
    /// pane shows the second command at the second registration's revision, an
    /// answer naming the first revision is refused, and the answer reaches the
    /// second producer. A delayed event from a superseded registration is never
    /// applied, and an event repeating a held id with other content never takes
    /// the hold's revision.
    #[spec("question/hold/012")]
    #[tokio::test]
    async fn question_hold_012_a_superseded_registration_cannot_publish_or_lend_its_revision() {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-r1").await;
        let (registered, resume) = deck
            .registry
            .question_answers()
            .arm_barrier("question_signal:registered");
        let first = deck.hold_with(&a, "q-r1", "touch first").await;
        registered
            .await
            .expect("the first registration is in place");
        let first_generation = deck
            .registry
            .question_holds()
            .generation_of(&a.pane, "q-r1")
            .expect("registered");
        let second = deck.hold_with(&a, "q-r1", "rm -rf second").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            deck.registry
                .question_holds()
                .generation_of(&a.pane, "q-r1"),
            Some(first_generation),
            "the second registration waits for the first to be published"
        );
        resume.send(()).unwrap();
        let superseded = reply_on(first).await.expect("the first producer is let go");
        assert_eq!(
            superseded.reason,
            Some(crate::question::ReleaseReason::Superseded)
        );
        let shown = loop {
            if let Some(q) = deck.pending(&a).await
                && q.revision != Some(first_generation)
            {
                break q;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            shown.tool.as_ref().and_then(|t| t.detail.as_deref()),
            Some("rm -rf second"),
            "the pane shows the second producer's question"
        );
        let second_generation = deck
            .registry
            .question_holds()
            .generation_of(&a.pane, "q-r1");
        assert_eq!(shown.revision, second_generation);
        assert!(matches!(
            deck.answer_revision(&a, "q-r1", Some(first_generation))
                .await,
            Err(crate::question::AnswerRefusal::Stale { .. })
        ));

        // The defense at ingestion: an event from the superseded registration,
        // ingested late under the lifecycle, is refused outright.
        let late = {
            let lifecycle = deck
                .registry
                .question_answers()
                .lock_lifecycle(&a.pane)
                .await;
            ingest_event_in_lifecycle(
                &deck.state,
                &deck.event_tx,
                &deck.registry,
                &lifecycle,
                deck.question_on(&a, "q-r1", "touch first"),
                Some(first_generation),
                |_| false,
            )
            .await
        };
        assert!(!late, "a superseded registration's event is not applied");
        assert_eq!(
            deck.pending(&a)
                .await
                .map(|q| (q.revision, q.tool.and_then(|t| t.detail))),
            Some((second_generation, Some("rm -rf second".to_string())))
        );

        deck.answer_revision(&a, "q-r1", second_generation)
            .await
            .expect("answered");
        let reply = reply_on(second)
            .await
            .expect("the second producer hears it");
        assert_eq!(reply.outcome, ReplyOutcome::Answered);

        // An unheld event under a held id, with other content, gets a revision
        // of its own and lets the hold go: the hold's revision never describes
        // content its producer did not send.
        let b = deck.agent("pane-r1-b").await;
        let held = deck.hold_with(&b, "q-r1b", "touch held").await;
        deck.until_held(&b, "q-r1b").await;
        let held_revision = deck
            .registry
            .question_holds()
            .generation_of(&b.pane, "q-r1b");
        ingest_event(
            &deck.state,
            &deck.event_tx,
            &deck.registry,
            deck.question_on(&b, "q-r1b", "rm -rf other"),
        )
        .await;
        let now = deck.pending(&b).await.expect("pending");
        assert_ne!(now.revision, held_revision);
        assert!(now.revision.is_some());
        let released = reply_on(held).await.expect("the held producer is let go");
        assert_eq!(released.outcome, ReplyOutcome::Released);
        assert!(!deck.registry.question_holds().is_held(&b.pane, "q-r1b"));
    }

    /// Scenario: The deck answers a held question, and right after the answer
    /// reaches its producer — before the deck's own event clears the question —
    /// the agent asks the same question id again. The new question is not
    /// registered until the deck's clearing event is applied, and that event
    /// does not touch it: the new question stays held and pending at its own
    /// revision.
    #[spec("question/hold/013")]
    #[tokio::test]
    async fn question_hold_013_an_answer_s_clearing_event_cannot_release_a_new_registration() {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-r3-answer").await;
        let first = deck.hold(&a, "q-again").await;
        deck.until_held(&a, "q-again").await;
        let (delivered, resume) = deck
            .registry
            .question_answers()
            .arm_barrier("answer_question:delivered");
        let answering = tokio::spawn({
            let registry = deck.registry.clone();
            let state = deck.state.clone();
            let event_tx = deck.event_tx.clone();
            let agent_id = a.id.clone();
            async move {
                crate::daemon_protocol::answer_question(
                    &registry,
                    &state,
                    &event_tx,
                    &agent_id,
                    "q-again",
                    &[QuestionAnswer {
                        question_index: 0,
                        option_indices: vec![1],
                        text: None,
                    }],
                    false,
                )
                .await
            }
        });
        delivered.await.expect("the answer reached its holder");
        let reply = reply_on(first).await.expect("the first producer hears it");
        assert_eq!(reply.outcome, ReplyOutcome::Answered);
        let second = deck.hold(&a, "q-again").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !deck.registry.question_holds().is_held(&a.pane, "q-again"),
            "the new question registered before the answer's clearing event"
        );
        resume.send(()).unwrap();
        answering.await.unwrap().expect("answered");
        deck.until_held(&a, "q-again").await;
        let again = loop {
            if let Some(q) = deck.pending(&a).await {
                break q;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(deck.registry.question_holds().is_held(&a.pane, "q-again"));
        assert_eq!(
            deck.pending(&a).await.map(|q| q.revision),
            Some(again.revision),
            "the new question survives the answer's clearing event"
        );
        drop(second);
    }

    /// Scenario: With the hook connection pool cut to two, three agents each
    /// hold a question open. A completion hook for the first agent's tool still
    /// gets in, clears that question and lets its producer go — held questions
    /// give their pool slot back. With the hold budget cut to one, a question
    /// past it still turns the card Needs Input but is not held or offered.
    /// Every question connection ends with its reply — cleared, not held,
    /// superseded or released at the deadline — so a producer that keeps its
    /// socket open and sends more has it read by nothing, however many do.
    #[spec("question/hold/008")]
    #[tokio::test]
    async fn question_hold_008_held_questions_never_starve_the_hook_pool() {
        let deck = Deck::start(HookLoopLimits {
            max_connections: 2,
            ..limits()
        })
        .await;
        let agents = [
            deck.agent("pane-a8-1").await,
            deck.agent("pane-a8-2").await,
            deck.agent("pane-a8-3").await,
        ];
        let mut producers = Vec::new();
        for (i, agent) in agents.iter().enumerate() {
            let id = format!("q-sat-{i}");
            producers.push(deck.hold(agent, &id).await);
            deck.until_held(agent, &id).await;
        }
        assert_eq!(deck.registry.question_holds().len(), 3);

        // The completion hook: a plain `PostToolUse` for the first agent's Bash.
        let mut done = question_event(&agents[0].id, "unused");
        done.metadata.clear();
        done.event_type = EventType::ToolEnd;
        done.pane_id = Some(agents[0].pane.clone());
        done.session_id = format!("{}-session", agents[0].pane);
        let mut stream = UnixStream::connect(&deck.sock).await.expect("connect");
        // Issue #318: the pane's own raw report, its hook token presented.
        let line = format!(
            "{}\n",
            crate::event::agent_event_line(&done, Some(&agents[0].token)).unwrap()
        );
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        drop(stream);
        wait_for(
            &deck.state,
            "the completion hook to clear the first question",
            |s| s.pending_question_id_on_pane("pane-a8-1").is_none(),
        )
        .await;
        let first = producers.remove(0);
        // A line the first producer sends after its reply, if it were read,
        // would raise a question on its pane again.
        let mut sneaked = question_event(&agents[0].id, "q-sneaked");
        sneaked.pane_id = Some(agents[0].pane.clone());
        sneaked.session_id = format!("{}-session", agents[0].pane);
        let (released, closed) = reply_then_closed(first, &sneaked).await;
        assert_eq!(
            released.reason,
            Some(crate::question::ReleaseReason::Cleared)
        );
        assert!(closed, "the connection ends with its reply");
        assert_eq!(deck.registry.question_holds().len(), 2);

        // The hold budget.
        let deck = Deck::start(HookLoopLimits {
            max_holds: 1,
            ..limits()
        })
        .await;
        let one = deck.agent("pane-a8-b1").await;
        let two = deck.agent("pane-a8-b2").await;
        let _held = deck.hold(&one, "q-budget-1").await;
        deck.until_held(&one, "q-budget-1").await;
        let over = deck.hold(&two, "q-budget-2").await;
        let mut sneaked = question_event(&two.id, "q-budget-sneaked");
        sneaked.pane_id = Some(two.pane.clone());
        sneaked.session_id = format!("{}-session", two.pane);
        let (reply, closed) = reply_then_closed(over, &sneaked).await;
        assert_eq!(reply.reason, Some(crate::question::ReleaseReason::NotHeld));
        assert!(closed, "a question refused a hold ends its connection too");
        assert!(
            !deck
                .registry
                .question_holds()
                .is_held(&two.pane, "q-budget-2")
        );
        {
            let state = deck.state.read().await;
            assert_eq!(state.pending_question_id_on_pane(&two.pane), None);
            assert_eq!(
                state.sessions["pane-a8-b2-session"].status,
                crate::state::SessionStatus::WaitingForInput
            );
        }

        // Past both bounds: more producers than the pool holds connections and
        // the budget holds questions, each released — superseded, or at the
        // hold deadline — and each keeping its socket open afterwards. Every
        // one is closed with its reply, so none can go on sending lines that
        // nothing accounts for, and the pool still serves a fresh hook.
        let deck = Deck::start(HookLoopLimits {
            max_connections: 2,
            max_holds: 2,
            hold_deadline: Duration::from_millis(300),
            ..limits()
        })
        .await;
        let mut kept_open = Vec::new();
        for round in 0..3 {
            let agent = deck.agent(&format!("pane-a8-c{round}")).await;
            let superseded = deck.hold(&agent, "q-churn").await;
            deck.until_held(&agent, "q-churn").await;
            let replacement = deck.hold(&agent, "q-churn").await;
            let mut sneaked = question_event(&agent.id, "q-churn-sneaked");
            sneaked.pane_id = Some(agent.pane.clone());
            sneaked.session_id = format!("{}-session", agent.pane);
            let (reply, closed) = reply_then_closed(superseded, &sneaked).await;
            assert_eq!(
                reply.reason,
                Some(crate::question::ReleaseReason::Superseded)
            );
            assert!(closed, "round {round}: a superseded hold's connection ends");
            let (reply, closed) = reply_then_closed(replacement, &sneaked).await;
            assert_eq!(
                reply.reason,
                Some(crate::question::ReleaseReason::Cleared),
                "round {round}: released at the deadline"
            );
            assert!(
                closed,
                "round {round}: a deadline release ends its connection"
            );
            kept_open.push(agent);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        {
            let state = deck.state.read().await;
            for agent in &kept_open {
                assert_eq!(
                    state.pending_question_id_on_pane(&agent.pane),
                    None,
                    "nothing sent after a reply was read"
                );
            }
        }
        assert!(deck.registry.question_holds().is_empty());
        let fresh = deck.agent("pane-a8-fresh").await;
        let producer = deck.hold(&fresh, "q-fresh").await;
        deck.until_held(&fresh, "q-fresh").await;
        drop(producer);
    }

    /// Scenario: A process sends the hook socket a bare event carrying a
    /// question, and another carrying an "answered" marker. The card takes the
    /// status, but the question metadata neither raises nor clears a question:
    /// that comes only through the provenance gate. The same question sent as an unheld `question`
    /// message with the pane's token is pending at once, and an attested
    /// "answered" marker clears it; one for a pane the deck never issued a token
    /// to has its question removed.
    #[spec("question/detect/011")]
    #[tokio::test]
    async fn question_detect_011_bare_frames_cannot_raise_or_clear_a_question() {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-a9-detect").await;
        let session = format!("{}-session", a.pane);
        let frame = |question_id: &str| {
            let mut event = question_event(&a.id, question_id);
            event.pane_id = Some(a.pane.clone());
            event.session_id = session.clone();
            event
        };
        let send = |line: String| {
            let sock = deck.sock.clone();
            async move {
                let mut stream = UnixStream::connect(&sock).await.expect("connect");
                stream
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                stream.shutdown().await.unwrap();
                let mut reader = tokio::io::BufReader::new(stream);
                let mut reply = String::new();
                let _ = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut reply))
                    .await;
                reply
            }
        };
        let unheld = |event: AgentEvent, token: Option<String>, pane: &str| {
            serde_json::to_string(&crate::event::DaemonMessage::Question(Box::new(
                crate::event::QuestionSignal {
                    pane_id: pane.to_string(),
                    token,
                    event,
                    hold: false,
                    ack: false,
                    confirm_hold: false,
                },
            )))
            .unwrap()
        };

        // Issue #318: the pane's own raw report, its hook token presented, so
        // the provenance gate admits it — and still it raises no question.
        send(crate::event::agent_event_line(&frame("q-bare"), Some(&a.token)).unwrap()).await;
        wait_for(&deck.state, "the bare frame's status", |s| {
            s.sessions
                .get(&session)
                .is_some_and(|x| x.status == crate::state::SessionStatus::WaitingForInput)
        })
        .await;
        assert_eq!(
            deck.state.read().await.pending_question_id_on_pane(&a.pane),
            None,
            "a bare frame raises no question"
        );

        let reply = send(unheld(frame("q-gated"), Some(a.token.clone()), &a.pane)).await;
        let reply: QuestionReply = serde_json::from_str(reply.trim()).expect("one reply line");
        assert!(!reply.refused());
        assert_eq!(
            deck.state
                .read()
                .await
                .pending_question_id_on_pane(&a.pane)
                .as_deref(),
            Some("q-gated")
        );

        let mut answered = frame("unused");
        answered.metadata.clear();
        answered.event_type = EventType::Thinking;
        answered.metadata.insert(
            crate::event::QUESTION_RESOLVED_METADATA_KEY.to_string(),
            "q-gated".to_string(),
        );
        send(crate::event::agent_event_line(&answered, Some(&a.token)).unwrap()).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            deck.state
                .read()
                .await
                .pending_question_id_on_pane(&a.pane)
                .as_deref(),
            Some("q-gated"),
            "a bare 'answered' marker clears nothing"
        );
        send(unheld(answered, Some(a.token.clone()), &a.pane)).await;
        wait_for(&deck.state, "the attested marker to clear it", |s| {
            s.pending_question_id_on_pane("pane-a9-detect").is_none()
        })
        .await;

        // A pane the deck never issued a token to: admitted unattested, and
        // its question removed.
        deck.state
            .write()
            .await
            .register_pane("pane-stranger".to_string());
        let mut stranger = frame("q-stranger");
        stranger.pane_id = Some("pane-stranger".into());
        stranger.agent_id = None;
        stranger.session_id = "stranger-session".into();
        send(unheld(stranger, None, "pane-stranger")).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            deck.state
                .read()
                .await
                .pending_question_id_on_pane("pane-stranger"),
            None
        );
    }

    /// Scenario: A producer holds a question and never lets go. When the
    /// daemon's own hold deadline passes, the producer is told to stop waiting,
    /// the question is dropped, and every attached client is told — so nothing
    /// waits on the daemon forever.
    #[spec("question/hold/009")]
    #[tokio::test]
    async fn question_hold_009_the_daemon_releases_a_hold_at_its_deadline() {
        let deck = Deck::start(HookLoopLimits {
            hold_deadline: Duration::from_millis(300),
            ..limits()
        })
        .await;
        let mut events = deck.event_tx.subscribe();
        let a = deck.agent("pane-a9").await;
        let producer = deck.hold(&a, "q-deadline").await;
        deck.until_held(&a, "q-deadline").await;
        let reply = reply_on(producer).await.expect("released at the deadline");
        assert_eq!(reply.outcome, ReplyOutcome::Released);
        assert!(
            !deck
                .registry
                .question_holds()
                .is_held(&a.pane, "q-deadline")
        );
        assert_eq!(
            deck.state.read().await.pending_question_id_on_pane(&a.pane),
            None
        );
        let released = loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("the release is broadcast")
                .expect("broadcast open");
            if let BroadcastMsg::Event(event) = msg
                && event.resolved_question_id() == Some("q-deadline")
            {
                break event;
            }
        };
        assert!(released.is_daemon_synthetic());
    }

    /// Scenario: A producer that reports whether the agent took the answer —
    /// the OpenCode plugin's child — holds a question. The deck answers; the
    /// producer hears it and reports that OpenCode refused the reply, so the
    /// deck's answer is refused as not sent, saying why, and the same question
    /// raised again can be answered again, this time reported taken. A
    /// producer that reports nothing in time leaves the answer unconfirmed.
    #[spec("question/hold/015")]
    #[tokio::test]
    async fn question_hold_015_an_answer_the_agent_does_not_take_is_reported_and_answerable_again()
    {
        let deck = Deck::start(HookLoopLimits {
            ack_wait: Duration::from_millis(300),
            ..limits()
        })
        .await;
        let a = deck.agent("pane-ack").await;
        let acknowledge = |producer: UnixStream, ack: Option<&'static str>| async move {
            let mut reader = tokio::io::BufReader::new(producer);
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                .await
                .expect("the answer arrives")
                .unwrap();
            let reply: QuestionReply = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(reply.outcome, ReplyOutcome::Answered);
            if let Some(ack) = ack {
                let mut producer = reader.into_inner();
                producer
                    .write_all(format!("{ack}\n").as_bytes())
                    .await
                    .unwrap();
                producer.flush().await.unwrap();
                producer
            } else {
                reader.into_inner()
            }
        };

        // OpenCode refuses the reply: the deck says so, and why.
        let producer = deck.hold_acking(&a, "per_ack").await;
        deck.until_held(&a, "per_ack").await;
        let (answered, _producer) = tokio::join!(
            deck.answer(&a, "per_ack"),
            acknowledge(
                producer,
                Some(r#"{"delivered":false,"detail":"OpenCode answered 500\u202e"}"#)
            )
        );
        match answered {
            Err(crate::question::AnswerRefusal::WriteFailed { detail }) => {
                assert!(detail.contains("OpenCode answered 500"), "{detail}");
                assert!(detail.contains("asking again"), "{detail}");
                assert!(
                    !detail.contains('\u{202e}'),
                    "the detail is sanitized: {detail:?}"
                );
            }
            other => panic!("not reported as not sent: {other:?}"),
        }

        // The plugin raises it again under the same id: answerable, and taken.
        let producer = deck.hold_acking(&a, "per_ack").await;
        deck.until_held(&a, "per_ack").await;
        let (answered, _producer) = tokio::join!(
            deck.answer(&a, "per_ack"),
            acknowledge(producer, Some(r#"{"delivered":true}"#))
        );
        assert_eq!(answered, Ok(()), "the re-raised question is answered");
        assert_eq!(
            deck.answer(&a, "per_ack").await,
            Err(crate::question::AnswerRefusal::NoPendingQuestion),
            "and, once taken, not answered twice"
        );

        // No report in time: unconfirmed, never "answered".
        let producer = deck.hold_acking(&a, "per_silent").await;
        deck.until_held(&a, "per_silent").await;
        let (answered, _producer) =
            tokio::join!(deck.answer(&a, "per_silent"), acknowledge(producer, None));
        assert_eq!(answered, Err(crate::question::AnswerRefusal::Unconfirmed));
        assert!(
            deck.registry
                .question_answers()
                .is_answered(&a.pane, "per_silent", None),
            "an unconfirmed answer may have been taken, so it is not offered again"
        );
    }

    /// Scenario: A producer holds a question on a pane, and the pane then moves
    /// to a new session that has no question — the card a respawned agent gets.
    /// The next event on the pane, here a late frame from the session it left,
    /// lets the old question's producer go, and the old question is no longer
    /// the pane's pending one.
    #[spec("question/hold/018")]
    #[tokio::test]
    async fn question_hold_018_a_session_switch_releases_the_predecessors_hold() {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-switch").await;
        let producer = deck.hold(&a, "q-old").await;
        deck.until_held(&a, "q-old").await;
        let asked_at = {
            let state = deck.state.read().await;
            state.sessions[&format!("{}-session", a.pane)].last_activity
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        // The session switch: the pane's new session, newer and without a
        // question, beside the old one's card.
        deck.state.write().await.insert_placeholder_session(
            a.pane.clone(),
            None,
            Some(AgentType::ClaudeCode),
            Some("successor-agent".into()),
        );

        let mut late = deck.question_on(&a, "q-old", "touch x");
        late.metadata.remove(crate::event::QUESTION_METADATA_KEY);
        late.event_type = EventType::ToolStart;
        late.timestamp = asked_at - chrono::Duration::seconds(1);
        ingest_event(&deck.state, &deck.event_tx, &deck.registry, late).await;

        let reply = reply_on(producer)
            .await
            .expect("the old question's producer is let go");
        assert_eq!(reply.outcome, ReplyOutcome::Released);
        assert!(!deck.registry.question_holds().is_held(&a.pane, "q-old"));
        assert_eq!(
            deck.pending(&a).await,
            None,
            "the old question is not pending"
        );
        assert_eq!(
            deck.state.read().await.pending_question_id_on_pane(&a.pane),
            None
        );
        assert_eq!(
            deck.answer(&a, "q-old").await,
            Err(crate::question::AnswerRefusal::ChannelGone),
            "the old question is not answered"
        );
    }

    /// Scenario: An agent waits on a held question, then starts a new hook
    /// session on the same pane (a `/clear`), which lets the old question's
    /// producer go. Late frames from the old session — a question of its own
    /// and a tool starting — then arrive stamped newer than anything the new
    /// session sent. The old question comes back nowhere: not as the pane's
    /// question, not on the agent's snapshot, and not to an answer.
    #[spec("question/hold/023")]
    #[tokio::test]
    async fn question_hold_023_a_late_event_from_a_superseded_session_does_not_revive_its_question()
    {
        let deck = Deck::start(limits()).await;
        let a = deck.agent("pane-revive").await;
        let old_session = format!("{}-session", a.pane);
        let producer = deck.hold(&a, "q-old").await;
        deck.until_held(&a, "q-old").await;

        // The same agent starts a new conversation on the pane.
        let mut start = deck.question_on(&a, "q-old", "touch x");
        start.metadata.remove(crate::event::QUESTION_METADATA_KEY);
        start.event_type = EventType::SessionStart;
        start.tool_name = None;
        start.session_id = format!("{}-new-session", a.pane);
        start.timestamp = chrono::Utc::now() + chrono::Duration::seconds(1);
        ingest_event(&deck.state, &deck.event_tx, &deck.registry, start).await;
        let reply = reply_on(producer)
            .await
            .expect("the old question's producer is let go");
        assert_eq!(reply.outcome, ReplyOutcome::Released);
        assert_eq!(deck.pending(&a).await, None);

        // Late frames from the old conversation, stamped newest: a question
        // of its own, which is stripped, and a tool starting.
        let mut late_question = deck.question_on(&a, "q-late", "touch y");
        late_question.timestamp = chrono::Utc::now() + chrono::Duration::seconds(5);
        assert_eq!(late_question.session_id, old_session);
        ingest_event(&deck.state, &deck.event_tx, &deck.registry, late_question).await;
        let mut late_tool = deck.question_on(&a, "q-old", "touch x");
        late_tool
            .metadata
            .remove(crate::event::QUESTION_METADATA_KEY);
        late_tool.event_type = EventType::ToolStart;
        late_tool.timestamp = chrono::Utc::now() + chrono::Duration::seconds(6);
        ingest_event(&deck.state, &deck.event_tx, &deck.registry, late_tool).await;

        assert_eq!(deck.pending(&a).await, None, "the old question is not back");
        {
            let state = deck.state.read().await;
            // Why it cannot come back: a session the pane moved past is one the
            // same agent left (`is_superseded_session`), and the reuse guard
            // files every session of one agent on one pane on the same card, so
            // there is no older card for a late frame to make the newest.
            assert_eq!(
                state
                    .sessions
                    .values()
                    .filter(|s| s.pane_id.as_deref() == Some(a.pane.as_str()))
                    .count(),
                1,
                "both conversations are on the agent's one card"
            );
            assert_eq!(state.pending_question_id_on_pane(&a.pane), None);
            assert_eq!(state.pending_question_for(&a.id, Some(&a.pane)), None);
            let snapshot = state
                .live_session_for(&a.id, Some(&a.pane))
                .expect("the agent's card");
            assert_eq!(
                snapshot.pending_question, None,
                "the snapshot shows no question"
            );
        }
        assert_eq!(
            deck.answer(&a, "q-old").await,
            Err(crate::question::AnswerRefusal::NoPendingQuestion),
            "the old question is not answerable"
        );
    }

    /// Scenario: An OpenCode subagent's child session asks a permission, and
    /// the plugin's `await-answer` child holds it on the deck as the plugin
    /// sends it: filed under the main session with the child's id. The deck
    /// shows it pending, attributed to that subagent, with the card on Needs
    /// Input; the deck answers it and the child reports OpenCode took the
    /// reply. OpenCode's own `permission.replied` from the child, carrying the
    /// request id, then takes the card off Needs Input.
    #[spec("question/hold/019")]
    #[tokio::test]
    async fn question_hold_019_an_opencode_subagents_permission_prompt_is_held_and_answered() {
        let deck = Deck::start(HookLoopLimits {
            ack_wait: Duration::from_millis(2000),
            ..limits()
        })
        .await;
        let a = deck.agent("pane-oc-subagent").await;
        let session = format!("{}-session", a.pane);
        let decode = |payload: serde_json::Value| {
            let input: crate::hook::OpenCodeHookInput = serde_json::from_value(payload).unwrap();
            let (mut event, question) =
                crate::hook::build_opencode_event_with_question(input).expect("an event");
            event.pane_id = Some(a.pane.clone());
            event.agent_id = Some(a.id.clone());
            (event, question)
        };
        let mut properties: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/agent-questions/opencode-permission-asked.json"),
            )
            .unwrap(),
        )
        .unwrap();
        properties["sessionID"] = "child".into();
        let request_id = properties["id"].as_str().unwrap().to_string();

        // What the plugin hands `await-answer` for the child's prompt.
        let (asked, question) = decode(serde_json::json!({
            "session_id": session,
            "subagent_id": "child",
            "event": "permission.asked",
            "properties": properties,
            "prompt": "touch oc_a.txt",
            "cwd": "/work",
        }));
        let question = question.expect("the child's prompt is a question");
        assert!(asked.is_from_subagent(), "the ask is the subagent's");
        assert_eq!(question.subagent_id.as_deref(), Some("child"));
        let msg = crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
            pane_id: a.pane.clone(),
            token: Some(a.token.clone()),
            event: asked,
            hold: true,
            ack: true,
            confirm_hold: false,
        }));
        let mut producer = UnixStream::connect(&deck.sock).await.expect("connect");
        producer
            .write_all(format!("{}\n", serde_json::to_string(&msg).unwrap()).as_bytes())
            .await
            .unwrap();
        producer.flush().await.unwrap();
        deck.until_held(&a, &request_id).await;

        let pending = deck.pending(&a).await.expect("the prompt is pending");
        assert_eq!(pending.id, request_id);
        assert_eq!(
            pending.subagent_id.as_deref(),
            Some("child"),
            "attributed to the subagent"
        );
        {
            let state = deck.state.read().await;
            let card = &state.sessions[&session];
            assert_eq!(card.status, crate::state::SessionStatus::WaitingForInput);
            assert_eq!(
                card.subagent_wait.as_ref().map(|w| w.subagent_ids.clone()),
                Some(vec!["child".to_string()]),
                "the wait is the subagent's"
            );
        }

        let acknowledge = async move {
            let mut reader = tokio::io::BufReader::new(producer);
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                .await
                .expect("the answer arrives")
                .unwrap();
            let reply: QuestionReply = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(reply.outcome, ReplyOutcome::Answered);
            assert!(reply.answers_for(&request_id).is_some());
            let mut producer = reader.into_inner();
            producer.write_all(b"{\"delivered\":true}\n").await.unwrap();
            producer.flush().await.unwrap();
            producer
        };
        let (answered, _producer) = tokio::join!(deck.answer(&a, &pending.id), acknowledge);
        assert_eq!(answered, Ok(()), "the subagent's prompt is answered");

        // OpenCode reports the child's prompt answered, as the plugin sends it.
        let (replied, _) = decode(serde_json::json!({
            "session_id": session,
            "subagent_id": "child",
            "event": "permission.replied",
            "request_id": pending.id,
            "prompt": "touch oc_a.txt",
            "cwd": "/work",
        }));
        assert!(replied.is_subagent_permission_reply());
        assert_eq!(replied.resolved_question_id(), Some(pending.id.as_str()));
        ingest_event(&deck.state, &deck.event_tx, &deck.registry, replied).await;
        wait_for(&deck.state, "the card to leave Needs Input", |s| {
            s.sessions[&session].status != crate::state::SessionStatus::WaitingForInput
        })
        .await;
        assert_eq!(deck.pending(&a).await, None);
    }

    /// Scenario: A held producer that does not acknowledge — Claude Code's
    /// hook, Pi's extension — closes its connection just as the deck answers,
    /// so writing the answer fails. The deck reports the answer unconfirmed,
    /// never delivered; a write that succeeds is still reported delivered.
    #[spec("question/hold/020")]
    #[tokio::test]
    async fn question_hold_020_an_answer_the_producer_never_received_is_not_reported_delivered() {
        use crate::agent_pty::{HeldReply, ReplyDelivery};
        let deliver = |closed: bool| async move {
            let (ours, theirs) = tokio::io::duplex(64);
            if closed {
                drop(theirs);
            } else {
                // Kept open (and drained) so the write lands.
                tokio::spawn(async move {
                    let mut theirs = tokio::io::BufReader::new(theirs);
                    let mut line = String::new();
                    let _ = theirs.read_line(&mut line).await;
                });
            }
            let (read_half, mut write_half) = tokio::io::split(ours);
            let mut reader = tokio::io::BufReader::new(read_half);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let held = HeldReply {
                reply: QuestionReply::answered("q-noack", Vec::new()),
                delivery: Some(tx),
            };
            write_held_reply(
                &mut reader,
                &mut write_half,
                held,
                false,
                Duration::from_millis(100),
                HOOK_CONNECTION_IDLE_TIMEOUT,
            )
            .await;
            rx.await.expect("an outcome is always reported")
        };
        assert_eq!(
            deliver(true).await,
            ReplyDelivery::Unconfirmed,
            "a reply that could not be written is not delivered"
        );
        assert_eq!(
            deliver(false).await,
            ReplyDelivery::Delivered,
            "a written reply to a producer that does not acknowledge is delivered"
        );
    }
}

#[cfg(test)]
mod hook_pin_refresh_tests {
    use super::*;
    use crate::event::AgentType;
    use crate::hook_binary::{DeckIdentity, HookBinaryState, HookPin, PinRefresh};

    /// Scenario (issue #1637 audit A9): a hook-config read is stuck, as one on
    /// a FIFO with no writer would be. While it runs a second read is refused
    /// rather than started; aborting the task waiting on it and shutting its
    /// runtime down both finish at once instead of waiting for the read; once
    /// the read ends, a new one may start.
    #[test]
    fn a_stuck_collection_blocks_neither_shutdown_nor_runs_twice() {
        static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
        let (unblock_tx, unblock_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let waiter = runtime.spawn(async move {
            let collected = spawn_detached_collection(&IN_FLIGHT, move || {
                started_tx.send(()).unwrap();
                let _ = unblock_rx.recv();
                1
            })
            .expect("the first collection starts");
            collected.await
        });
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the collection started");
        assert!(
            spawn_detached_collection(&IN_FLIGHT, || 2).is_none(),
            "a second collection is not started while the first runs"
        );

        waiter.abort();
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(runtime);
            let _ = dropped_tx.send(());
        });
        dropped_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the runtime shut down while the collection was still stuck");
        assert!(IN_FLIGHT.load(Ordering::Acquire), "still running");

        unblock_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while IN_FLIGHT.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "the collection never ended"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let next = spawn_detached_collection(&IN_FLIGHT, || 3).expect("a new collection starts");
        assert_eq!(next.blocking_recv(), Ok(3));
    }

    /// Scenario (issue #1637): the daemon knows Codex's hooks are pinned to a
    /// copy it cannot run, and shows a notice. A refresh then reads Codex's
    /// config and finds it names no deck binary any more: applying it under
    /// the state lock clears the notice and broadcasts the new, empty list
    /// once; the same refresh again broadcasts nothing.
    #[tokio::test]
    async fn a_refresh_that_changes_the_notices_broadcasts_them_once() {
        let deck = DeckIdentity {
            exe: None,
            version: "0.46.0".into(),
            self_install_path: None,
        };
        let mut initial = crate::state::AppState::default();
        initial.hook_binaries = HookBinaryState::from_startup(
            deck.clone(),
            &[HookPin {
                agent: AgentType::Codex,
                config: std::path::PathBuf::from("/cfg/hooks.json"),
                binary: crate::test_paths::abs("/nonexistent/dad-1637/dot-agent-deck"),
            }],
            None,
        );
        assert_eq!(initial.hook_binaries.notices().len(), 1);
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(initial));
        let (event_tx, mut rx) = broadcast::channel(8);
        let none_pinned = || PinRefresh::classify(&deck, vec![(AgentType::Codex, Vec::new())]);

        apply_hook_pin_refresh(&state, &event_tx, none_pinned()).await;
        match rx.try_recv() {
            Ok(BroadcastMsg::HookBinaryNotice(sent)) => assert!(sent.notices.is_empty()),
            other => panic!("expected one notice broadcast, got {other:?}"),
        }
        apply_hook_pin_refresh(&state, &event_tx, none_pinned()).await;
        assert!(
            rx.try_recv().is_err(),
            "an unchanged refresh broadcasts nothing"
        );
        assert!(state.read().await.hook_binaries.notices().is_empty());
    }
}

/// PRD #1666 part 3 (Q35): the hook loop's `composer_invocation` arm, while
/// the verb is declared and not served (`inbox/probe/016`).
#[cfg(all(test, unix))]
mod composer_invocation_tests {
    use super::*;
    use crate::event::{
        ComposerDecision, ComposerInvocationReply, ComposerInvocationRequest,
        HookCapabilitiesReply, HookCapabilitiesRequest,
    };
    use crate::state::AppState;
    use spec::spec;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    const PANE: &str = "composer-pane";

    struct Deck {
        _dir: tempfile::TempDir,
        sock: std::path::PathBuf,
        registry: Arc<AgentPtyRegistry>,
        handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
    }

    impl Deck {
        async fn start(registry: Arc<AgentPtyRegistry>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("hook.sock");
            let listener = IpcListener::from_tokio_listener(
                UnixListener::bind(&sock).expect("bind hook socket"),
            );
            let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
            let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let handle = tokio::spawn({
                let registry = registry.clone();
                let shutdown = Arc::new(Notify::new());
                let wtr = crate::issue_dispatch_run::new_worktree_registry();
                async move { run_hook_loop(listener, state, event_tx, registry, shutdown, wtr).await }
            });
            Self {
                _dir: dir,
                sock,
                registry,
                handle,
            }
        }

        /// Write `message` on a fresh connection, close the write half, and
        /// read every line the daemon answers until it closes.
        async fn exchange(&self, message: &DaemonMessage) -> Vec<String> {
            let mut stream = UnixStream::connect(&self.sock).await.expect("connect");
            stream
                .write_all(format!("{}\n", serde_json::to_string(message).unwrap()).as_bytes())
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
            let mut answer = String::new();
            tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut answer))
                .await
                .expect("the daemon answers and closes")
                .unwrap();
            answer.lines().map(str::to_string).collect()
        }

        async fn stop(self) {
            self.handle.abort();
            let _ = self.handle.await;
            self.registry.shutdown_all();
        }
    }

    fn invocation(token: Option<&str>) -> DaemonMessage {
        DaemonMessage::ComposerInvocation(ComposerInvocationRequest {
            pane_id: PANE.to_string(),
            token: token.map(str::to_string),
            generation: 1,
            decision: ComposerDecision::Armed,
            claim: None,
            seg_seen: Some(1),
            native_seen: None,
            argv: vec!["/tmp/claude-1000/prompt.md".to_string()],
        })
    }

    /// Scenario: A helper with a token that attests the pane's agent sends a `composer_invocation` straight to the hook socket, skipping the capability check: the daemon answers exactly one line, refused `unsupported` with no outcome, so nothing reads as the deck's invocation or as no arm. The same request with no token is refused by the provenance gate, also with no outcome.
    #[spec("inbox/probe/016")]
    #[tokio::test]
    async fn probe_016_the_daemon_answers_an_invocation_unsupported() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (_agent, token) = registry.reserve_spawn_for_test(Some(PANE));
        let deck = Deck::start(registry).await;

        let answered = deck.exchange(&invocation(Some(&token))).await;
        assert_eq!(answered.len(), 1, "one line per message: {answered:?}");
        let reply: ComposerInvocationReply = serde_json::from_str(&answered[0]).unwrap();
        assert_eq!(reply, ComposerInvocationReply::unsupported());
        assert_eq!(
            reply.code.as_deref(),
            Some(crate::event::COMPOSER_INVOCATION_UNSUPPORTED)
        );

        let unattested = deck.exchange(&invocation(None)).await;
        assert_eq!(unattested.len(), 1, "{unattested:?}");
        let refused: ComposerInvocationReply = serde_json::from_str(&unattested[0]).unwrap();
        assert_eq!(refused.outcome, None, "{refused:?}");
        assert!(refused.code.is_some(), "{refused:?}");
        assert_ne!(
            refused.code.as_deref(),
            Some(crate::event::COMPOSER_INVOCATION_UNSUPPORTED),
            "the gate refuses it before the arm"
        );
        deck.stop().await;
    }

    /// Scenario: A helper asks the daemon's hook socket which verbs it serves, as the shared sender does before an invocation: the answer names task-scoped completion and input-mode reports but not the composer probe, so the sender would withhold the invocation.
    #[spec("inbox/probe/016")]
    #[tokio::test]
    async fn probe_016_the_daemon_withholds_the_composer_capability() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (_agent, token) = registry.reserve_spawn_for_test(Some(PANE));
        let deck = Deck::start(registry).await;
        let question = DaemonMessage::HookCapabilities(HookCapabilitiesRequest {
            pane_id: PANE.to_string(),
            token: Some(token),
        });
        let mut stream = UnixStream::connect(&deck.sock).await.expect("connect");
        stream
            .write_all(format!("{}\n", serde_json::to_string(&question).unwrap()).as_bytes())
            .await
            .unwrap();
        let mut answer = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::BufReader::new(&mut stream).read_line(&mut answer),
        )
        .await
        .expect("the daemon answers the question")
        .unwrap();
        let reply: HookCapabilitiesReply = serde_json::from_str(answer.trim()).unwrap();
        assert_eq!(
            reply.task_scoped_completion,
            Some(crate::event::TASK_SCOPED_COMPLETION),
            "control: the answer is the daemon's real one: {answer:?}"
        );
        assert_eq!(reply.composer_probe, None, "{answer:?}");
        assert!(crate::hook::confirms_composer_probe(&answer).is_err());
        drop(stream);
        deck.stop().await;
    }
}

// Unix-only, like `hook_ingestion_tests`: it drives the loop over a Unix socket.
#[cfg(all(test, unix))]
#[path = "daemon_hook_reply_deadline_tests.rs"]
mod hook_reply_deadline_tests;

// Linux-only: the composer probe's observer and `SO_PEERPIDFD` are.
#[cfg(all(test, target_os = "linux"))]
#[path = "daemon_composer_reader_tests.rs"]
mod composer_reader_tests;

// Unix-only, like `question_hold_tests`, which it sits beside.
#[cfg(all(test, unix))]
#[path = "daemon_held_confirmation_deadline_tests.rs"]
mod held_confirmation_deadline_tests;

// Issue #1664: one session's hook events apply in the order they were sent.
// Unix-only, like `hook_reply_deadline_tests`: it drives the loop over a Unix
// socket.
#[cfg(all(test, unix))]
#[path = "daemon_hook_event_order_tests.rs"]
mod hook_event_order_tests;
