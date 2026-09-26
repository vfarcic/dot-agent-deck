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
fn spawn_termination_signal_watch(
    shutdown: Arc<Notify>,
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
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
            // Issue #1109: say WHICH, before the drain below empties the
            // registry this reads. Ordered ahead of the drain for that reason
            // and not merely for tidiness — `agent_records` filters to live
            // agents, so the same call after `shutdown_all_graceful` reports an
            // empty deck no matter what was running.
            crate::daemon_stop::log_teardown_inventory(&state, &registry, "signal").await;

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
            // Issue #1109: same disclosure, same position, as the Unix arm
            // above; see its comment for why it precedes the drain.
            crate::daemon_stop::log_teardown_inventory(&state, &registry, "signal").await;
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
    /// Issue #1211: the pre-#1121 hook spelling to ALSO bind, best-effort,
    /// beside the primary hook endpoint — see
    /// [`crate::endpoint_resolve::legacy_hook_alias`]. `None` (every
    /// constructor's default, and every test's) binds nothing extra; only
    /// `daemon serve` sets it, through [`Self::with_legacy_aliases`].
    pub legacy_socket_path: Option<PathBuf>,
    /// Issue #1211: [`Self::legacy_socket_path`]'s sibling for the attach
    /// endpoint. Ignored when [`Self::attach_socket_path`] is `None` — a daemon
    /// that serves no attach protocol has nothing to alias.
    pub legacy_attach_socket_path: Option<PathBuf>,
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
            legacy_socket_path: None,
            legacy_attach_socket_path: None,
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
            legacy_socket_path: None,
            legacy_attach_socket_path: None,
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

    /// Issue #1211: ALSO bind these pre-#1121 spellings, best-effort, beside
    /// the primary endpoints — `hook` beside the hook socket, `attach` beside
    /// the attach socket. `daemon serve` passes
    /// [`crate::endpoint_resolve::legacy_hook_alias`] /
    /// [`crate::endpoint_resolve::legacy_attach_alias`]; a test passes paths
    /// under its own tempdir. A failure to bind either is a warning, never a
    /// daemon-start failure — see [`run_daemon_with`].
    pub fn with_legacy_aliases(mut self, hook: Option<PathBuf>, attach: Option<PathBuf>) -> Self {
        self.legacy_socket_path = hook;
        self.legacy_attach_socket_path = attach;
        self
    }
}

/// How long the legacy-alias bind waits for the per-socket start lock before
/// giving the alias up. The lock is held by a starting daemon only across its
/// own probe → unlink → bind, so a wait longer than this means something is
/// wrong with the holder — and the alias is not worth delaying the daemon's
/// primary attach endpoint for.
const LEGACY_ALIAS_LOCK_TIMEOUT: Duration = Duration::from_secs(1);

/// The legacy aliases a daemon ended up holding: the listeners to serve and the
/// guards that unlink each alias again when dropped.
#[derive(Default)]
struct LegacyListeners {
    hook: Option<IpcListener>,
    attach: Option<IpcListener>,
    aliases: Vec<crate::endpoint_resolve::LegacyAlias>,
}

/// Issue #1211: bind the pre-#1121 spellings beside this daemon's primary
/// endpoints, so a client from before #1121 — which looks nowhere else — finds
/// this daemon, runs the build-version handshake over the connection, and gets
/// the mismatch prompt instead of silently lazy-spawning a second daemon that
/// then restores the saved orchestration a second time.
///
/// **Best-effort by construction: this function cannot fail.** Every problem is
/// logged and costs only the alias, because the primary endpoint is already
/// bound and is what this build's own clients use. That is the property that
/// keeps the alias from reopening #1121's wedge — a foreign entry squatting the
/// old spelling now denies discovery to obsolete clients and nothing more. See
/// [`crate::endpoint_resolve`]'s module docs.
///
/// The two paths are treated as one daemon's **pair**: if another daemon
/// already answers at either, neither is bound, so an older build's still-live
/// daemon keeps both of its addresses and old clients are not split between two
/// daemons.
///
/// Serialised on the per-socket `flock` a starting daemon takes for its hook
/// path, keyed on the old hook spelling — which is the path a pre-#1121 daemon
/// on the fallback arm locks. [`lock_path_for`] and [`lock_root`] are
/// byte-identical in v0.41.0, so a v0.41.0 daemon starting at the same moment
/// cannot interleave its own probe → unlink → bind with this one. That is
/// checked for v0.41.0 only, and it also rests on the two builds'
/// `DefaultHasher` agreeing on the lock file's name. Where either does not
/// hold the two do not serialise, and two starts in the same instant can race
/// for the old spelling exactly as two pre-#1121 daemons always could.
async fn bind_legacy_aliases(
    hook: Option<PathBuf>,
    attach: Option<PathBuf>,
    lock_override: Option<&Path>,
) -> LegacyListeners {
    let wanted: Vec<(&'static str, PathBuf)> = [("hook", hook), ("attach", attach)]
        .into_iter()
        .filter_map(|(kind, path)| path.map(|path| (kind, path)))
        .collect();
    let Some((_, lock_key)) = wanted.first() else {
        return LegacyListeners::default();
    };

    let lock_path = lock_path_for(lock_key, lock_override);
    let _lock = match tokio::time::timeout(
        LEGACY_ALIAS_LOCK_TIMEOUT,
        crate::platform::lock::acquire_spawn_lock(&lock_path),
    )
    .await
    {
        Ok(Ok(lock)) => lock,
        Ok(Err(source)) => {
            warn!(
                "not binding the pre-#1121 endpoint aliases: could not take their start lock {}: \
                 {source}. Clients from before #1121 will not find this daemon; newer clients are \
                 unaffected.",
                lock_path.display()
            );
            return LegacyListeners::default();
        }
        Err(_elapsed) => {
            warn!(
                "not binding the pre-#1121 endpoint aliases: their start lock {} was still held \
                 after {LEGACY_ALIAS_LOCK_TIMEOUT:?}. Clients from before #1121 will not find this \
                 daemon; newer clients are unaffected.",
                lock_path.display()
            );
            return LegacyListeners::default();
        }
    };

    // Pass 1: clear each path, before binding either, so an occupied one can
    // veto the pair.
    let mut ready = Vec::new();
    for (kind, path) in wanted {
        let probe_path = path.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            crate::endpoint_resolve::prepare_legacy_alias(&probe_path)
        })
        .await
        .unwrap_or_else(|join| {
            Err(crate::endpoint_resolve::LegacyAliasSkip::Io(
                io::Error::other(join),
            ))
        });
        match prepared {
            Ok(()) => ready.push((kind, path)),
            Err(crate::endpoint_resolve::LegacyAliasSkip::Occupied) => {
                warn!(
                    "not binding the pre-#1121 endpoint aliases: another daemon already answers \
                     at {} and keeps both of its addresses",
                    path.display()
                );
                return LegacyListeners::default();
            }
            Err(skip) => warn!(
                "not binding the pre-#1121 {kind} endpoint alias at {}: {skip}. Clients from \
                 before #1121 will not find this daemon there; newer clients are unaffected.",
                path.display()
            ),
        }
    }

    // Pass 2: bind what was cleared.
    let mut bound = LegacyListeners::default();
    for (kind, path) in ready {
        match IpcListener::bind(&path) {
            Ok(listener) => {
                // Deliberately NOT the primary's "Attach protocol listening"
                // wording: that line is how operators and the cross-version
                // harness count daemons, and an alias is not a second daemon.
                info!(
                    "Also listening at the pre-#1121 {kind} endpoint {} for older clients",
                    path.display()
                );
                bound
                    .aliases
                    .push(crate::endpoint_resolve::LegacyAlias::adopt(path));
                match kind {
                    "hook" => bound.hook = Some(listener),
                    _ => bound.attach = Some(listener),
                }
            }
            Err(source) => warn!(
                "not binding the pre-#1121 {kind} endpoint alias at {}: {source}. Clients from \
                 before #1121 will not find this daemon there; newer clients are unaffected.",
                path.display()
            ),
        }
    }
    bound
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

    // Issue #1211: the pre-#1121 spellings, bound best-effort beside the
    // primary — after the primary hook endpoint, so a failure here can never
    // cost the deck its own endpoint, and before the primary attach endpoint,
    // so a client that lazy-spawned this daemon and is polling for that attach
    // socket sees the daemon whole when it appears. Served further down, once
    // the primary attach server is up; `legacy` is dropped at the very end,
    // which is what unlinks each alias (early returns included).
    let legacy = bind_legacy_aliases(
        daemon.legacy_socket_path.clone(),
        daemon
            .legacy_attach_socket_path
            .clone()
            .filter(|_| daemon.attach_socket_path.is_some()),
        daemon.lock_dir_override.as_deref(),
    )
    .await;
    let LegacyListeners {
        hook: legacy_hook_listener,
        attach: legacy_attach_listener,
        aliases: legacy_aliases,
    } = legacy;

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
    let signal_handle =
        spawn_termination_signal_watch(shutdown.clone(), pty_registry.clone(), state.clone());

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

    // Issue #1211: serve the legacy aliases with exactly the state the primary
    // endpoints serve, so an older client that found this daemon at the old
    // spelling reaches the same registry, the same orchestration maps and the
    // same attached-client count (it keeps the daemon from idling out like any
    // other attached client).
    //
    // The alias hook loop gets a `Notify` of its OWN, never notified, and is
    // aborted below instead. `shutdown` is signalled with `notify_one`, which
    // wakes exactly one waiter: a second hook loop waiting on it could take the
    // signal meant for the primary loop, and the daemon would then never exit.
    // The attach alias shares `shutdown` safely — the attach server only ever
    // NOTIFIES it (a `KIND_SHUTDOWN` from an older client's `Stop` must still
    // stop this daemon), and never waits on it.
    let legacy_hook_handle = legacy_hook_listener.map(|listener| {
        let state = state.clone();
        let event_tx = event_tx.clone();
        let registry = pty_registry.clone();
        let worktrees = worktree_registry.clone();
        tokio::spawn(async move {
            if let Err(e) = run_hook_loop(
                listener,
                state,
                event_tx,
                registry,
                Arc::new(Notify::new()),
                worktrees,
            )
            .await
            {
                warn!("pre-#1121 hook endpoint alias stopped serving: {e}");
            }
        })
    });
    let legacy_attach_handle = legacy_attach_listener.map(|listener| {
        let registry = pty_registry.clone();
        let attach_event_tx = event_tx.clone();
        let attach_counter = client_count.clone();
        let attach_state = state.clone();
        let attach_shutdown = shutdown.clone();
        let attach_scheduler = scheduler.clone();
        let attach_reuse = reuse_registry.clone();
        let attach_worktrees = worktree_registry.clone();
        tokio::spawn(async move {
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
                warn!("pre-#1121 attach endpoint alias stopped serving: {e}");
            }
        })
    });

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
    if let Some(h) = legacy_hook_handle {
        h.abort();
    }
    if let Some(h) = legacy_attach_handle {
        h.abort();
    }
    // Issue #1211: unlink the aliases now rather than leave them for a next
    // start that may never bind them — see `LegacyAlias`. Explicit so the
    // ordering is read here, not inferred from where a binding goes out of
    // scope.
    drop(legacy_aliases);
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
        // `None` here even when the task DOES carry a `shape` (issue #835): the
        // target is resolved per fire, inside the callback below, not once at
        // registration. See the resolution block there for why.
        resolved_target: None,
        // Unchanged behaviour: the prompt is delivered verbatim. Giving this path
        // the orchestrator context is #222's work, not this PR's — and when it is
        // done, the value that belongs here is `Unattended`: a scheduled task
        // fires with no one at the pane (issue #703).
        compose_orchestrator_context: None,
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
async fn ingest_event(
    state: &SharedState,
    event_tx: &broadcast::Sender<BroadcastMsg>,
    registry: &AgentPtyRegistry,
    mut event: AgentEvent,
) {
    // Issue #770: half of the orphan verdict, asked of the registry BEFORE the
    // `AppState` write lock is taken. Sequencing, not style: `has_live_pane`
    // takes the registry's own mutex, and every other path in the daemon that
    // holds both takes the registry first (`spawn` registers a role only after
    // `spawn_agent` has returned and released it). Asking here keeps that order
    // rather than introducing the one nesting that would reverse it.
    let daemon_owns_pane = event
        .pane_id
        .as_deref()
        .is_some_and(|pane_id| registry.has_live_pane(pane_id));
    let mut state = state.write().await;
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
    // PRD #1223: the pane-closed marker is the daemon's alone too — it makes an
    // attached TUI drop the pane — so a producer's copy never reaches the
    // fan-out. The daemon's own removal is broadcast directly and never passes
    // through here (`crate::spawn::surface_attach_stopped_agent`).
    event
        .metadata
        .remove(crate::event::DAEMON_PANE_CLOSED_METADATA_KEY);
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
    run_shell_activity_monitor_with(pty_registry, state, event_tx, |roots| {
        let roots = roots.to_vec();
        async move { crate::platform::proc::process_table_async(&roots).await }
    })
    .await
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

        let snapshot = if candidates.is_empty() {
            // No pane, so no sample — and an empty reading rather than a
            // skipped tick, because "there are no panes" is a fact we just
            // established under the lock, not a failure to observe. Falling
            // through with an empty snapshot lets the `retain` below clear
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
            ingest_event(&state, &event_tx, &pty_registry, event).await;
        }
    }
}

/// Issue #617 (finding 3): deliver a dispatch result back to the agent that
/// asked for it, bound to the registry agent id captured from the caller's
/// `AgentRecord` *before* the dispatch ran.
///
/// The write used to be inline in `run_hook_loop`'s `Dispatch` arm and used the
/// unguarded `write_to_pane_and_submit(&signal.pane_id, …)`. Between the request
/// and this delivery sits `handle_dispatch` — a git worktree creation plus an
/// agent spawn, unbounded and deliberately performed outside any `AppState` lock
/// — which is one of the widest race windows in the daemon. A pane id is a
/// recycled handle, so a caller that was closed or respawned during that work had
/// its dispatch result submitted into whatever process inherited its pane, which
/// may then act on it with its own tools.
///
/// Extracted rather than fixed in place so the delivery has a name and a seam:
/// the slow half (`handle_dispatch`) was already unit-testable and this half was
/// not, which is why the only coverage of it was end-to-end.
///
/// Every refusal is terminal and none is retried — a retry could only re-target
/// whichever process now occupies the pane. `Ambiguous` is deliberately NOT
/// folded in with the refusals: bytes of ours already reached the authorized
/// caller, so re-sending would duplicate a half-written message rather than
/// repair it, and the caller is the one place where that distinction is visible.
pub async fn deliver_dispatch_result(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    expected_agent_id: &str,
    message: &str,
) -> crate::agent_pty::GuardedSend {
    use crate::agent_pty::GuardedSend;
    match registry
        .write_and_submit_guarded(pane_id, message, expected_agent_id, || async { true })
        .await
    {
        Ok(GuardedSend::Applied) => GuardedSend::Applied,
        Ok(GuardedSend::Ambiguous) => {
            warn!(
                pane_id = %pane_id,
                agent_id = %expected_agent_id,
                "dispatch: result delivery was ambiguous (partial write); not retried"
            );
            GuardedSend::Ambiguous
        }
        Ok(refused) => {
            warn!(
                pane_id = %pane_id,
                agent_id = %expected_agent_id,
                outcome = ?refused,
                "dispatch: identity gate refused the result (the caller pane no longer belongs \
                 to the agent that requested the dispatch); nothing written"
            );
            refused
        }
        Err(e) => {
            warn!(
                pane_id = %pane_id,
                agent_id = %expected_agent_id,
                error = %e,
                "dispatch: failed to write result into caller pane"
            );
            // A transport error, not a refusal: no identity decision was
            // reached. Reported as `Stale` so callers have one vocabulary, with
            // the real cause on the `warn!` above.
            GuardedSend::Stale
        }
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
                    limit = MAX_CONCURRENT_HOOK_CONNECTIONS,
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
fn hook_line_for_log(line: &str) -> String {
    crate::config_validation::escape_for_terminal(&clamp_for_log(line)).into_owned()
}

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
        HOOK_CONNECTION_IDLE_TIMEOUT,
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
    // Issue #319: bound how many hook connections are being served at once.
    // Every accepted connection used to get its own `tokio::spawn` with nothing
    // capping how many could be outstanding, so a producer that opened
    // connections faster than they finished grew the daemon's task set and its
    // per-connection buffers without limit.
    let conn_limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HOOK_CONNECTIONS));
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
                return Ok(());
            }
            accept_res = accept_hook_connection(&listener, &conn_limit, &mut at_cap) => match accept_res {
            Ok((permit, stream)) => {
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
                    let _permit = permit;
                    // PRD #201: split so the read-only `get-seed` verb can write
                    // a reply back on the same connection. The write half now
                    // serves every `DaemonMessage` arm — `delegate` since PR
                    // #466, `restart_role` / `spawn_role` / `list_targets`
                    // since #868 and PRD #220, and `work_done` / `dispatch`
                    // since issue #1129, whose line comes from the provenance
                    // gate above rather than from an arm. Raw `AgentEvent`
                    // traffic is still answered by nothing.
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut reader = tokio::io::BufReader::new(read_half);

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
                        // creation and `delegate`'s readiness wait both run in
                        // the arms below, outside this call.
                        let read = crate::bounded_read::read_capped_line(
                            &mut reader,
                            crate::bounded_read::MAX_HOOK_LINE_BYTES,
                        );
                        let line = match tokio::time::timeout(idle_timeout, read).await
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
                                        let _ = write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
                                    }
                                    continue;
                                }
                                Ok(()) => {
                                    // Issue #1129: the two fire-and-forget verbs
                                    // get their acknowledgement HERE, ahead of
                                    // the handler, so the caller learns whether
                                    // the gate admitted it without waiting for
                                    // work it is not waiting on. `dispatch`'s
                                    // handler is awaited inline below and spends
                                    // a whole worktree-create-and-spawn; an ack
                                    // written after it would park the calling
                                    // agent for the duration. Every other verb
                                    // returns `None` here and answers in its own
                                    // arm, which is what keeps "exactly one line
                                    // per message" true.
                                    if let Some(json) = msg.provenance_ack_reply() {
                                        let line = format!("{json}\n");
                                        let _ = write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
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
                            match msg {
                                DaemonMessage::Delegate(signal) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&signal.pane_id),
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
                                        } => Some(agent_id.clone()),
                                        _ => None,
                                    };
                                    let resp = state
                                        .read()
                                        .await
                                        .handle_attested_delegate(
                                            signal,
                                            &pty_registry,
                                            &event_tx,
                                            Some(&state),
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
                                        let _ = write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
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
                                        let _ = write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
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
                                        let _ = write_half.write_all(line.as_bytes()).await;
                                        let _ = write_half.flush().await;
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
                                    // populated for orchestration panes; mode panes
                                    // (including the dispatcher mode) never get an entry
                                    // there, which would make every dispatch from a mode
                                    // pane a silent no-op.
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
                                    let caller = {
                                        let records = pty_registry.agent_records();
                                        records
                                            .iter()
                                            .find(|r| r.pane_id_env.as_deref() == Some(&signal.pane_id))
                                            .and_then(|r| r.cwd.clone().map(|cwd| (r.id.clone(), cwd)))
                                    };
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

                                    // Deliver result to the caller (doesn't need any
                                    // AppState lock — uses the PTY registry).
                                    deliver_dispatch_result(
                                        &pty_registry,
                                        &signal.pane_id,
                                        &caller_agent_id,
                                        &result.message,
                                    )
                                    .await;
                                }
                                DaemonMessage::WorkDone(signal) => {
                                    info!(
                                        pane_id = %escape_id_for_log(&signal.pane_id),
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
                                    //
                                    // Issue #916: the pane id is CALLER-SUPPLIED
                                    // and this socket authenticates nobody, so
                                    // the take is scoped by the caller's own
                                    // agent id when it presents one and skips
                                    // exited records either way — the filtering
                                    // every other registry lookup applies and
                                    // this one did not. `req.agent_id` is
                                    // `None` for a caller the daemon injected no
                                    // id into, which falls back to the pane's
                                    // live occupant; the reasoning for that
                                    // choice is on `take_pending_seed_native_for`.
                                    let seed = pty_registry.take_pending_seed_native_for(
                                        &req.pane_id,
                                        req.agent_id.as_deref(),
                                    );
                                    info!(
                                        pane_id = %escape_id_for_log(&req.pane_id),
                                        agent_id = ?req.agent_id,
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
                                        pane_id = %escape_id_for_log(&req.pane_id),
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
                                session_id = %escape_id_for_log(&event.session_id),
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
                                    session_id = %escape_id_for_log(&event.session_id),
                                    pane_id = ?event.pane_id,
                                    raw_line = %hook_line_for_log(&line),
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
                                        pane_id = %escape_id_for_log(pane_id),
                                        session_id = %escape_id_for_log(&event.session_id),
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
                            ingest_event(&state, &event_tx, &pty_registry, event).await;
                        } else {
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

    /// PRD #1223: the pane-closed marker is daemon-authoritative. A producer
    /// posting it on the hook socket must not reach an attached TUI with it —
    /// that would let any same-uid process make a TUI drop a live pane.
    #[tokio::test]
    async fn ingest_strips_a_producer_supplied_pane_closed_marker() {
        let registry = AgentPtyRegistry::new();
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
        let subscriber_guard = tracing::subscriber::set_default(subscriber);

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
            "Received get-seed request",
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
    ///
    /// **What #1159 changed here.** The detached child outlives the wait for the
    /// broadcast instead of being a `sleep 2` the `ShellBusy` had to be caught
    /// inside; the stamped-shape assertions are untouched. The block comment
    /// above [`SHELL_ACTIVITY_SIGNAL_BUDGET`] has the measurement.
    #[tokio::test]
    async fn shell_activity_monitor_stamps_the_owning_agent_across_a_session_rollover() {
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

    /// Scenario: PRD #201 native prompt delivery over the hook socket. Spawn a
    /// shell agent tagged with a pane id, stash a seed for it via
    /// `set_pending_seed`, run the real `run_hook_loop`, then send a `get_seed`
    /// request line and read the reply. The daemon must reply with the exact
    /// seed, mark it delivered-native, and clear it — a second request replies
    /// `{"seed":null}`. This is the request/response path the pi extension's
    /// `get-seed` verb rides (the one hook-socket message that reads a reply).
    ///
    /// Issue #916 added the identity arms, over the real socket rather than
    /// against the registry method: a request naming an agent that does not hold
    /// the pane gets `null` and consumes nothing, a request naming the pane's own
    /// agent is answered, and a request naming NO agent — the pre-#916 client, and
    /// any producer the daemon injected no id into — is still answered rather than
    /// refused.
    #[tokio::test]
    async fn run_hook_loop_answers_get_seed_and_clears_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_gs = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "pane-gs".to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        let token_gs = registry
            .hook_token_of(&agent_gs)
            .expect("a spawned agent carries a hook capability token");
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
        // `agent_id` is what the real CLI reads out of `DOT_AGENT_DECK_AGENT_ID`
        // (issue #916); `None` is the pre-#916 client, and the shape a producer
        // the daemon injected no id into still sends.
        async fn ask_get_seed_as(
            sock: &std::path::Path,
            pane_id: &str,
            agent_id: Option<&str>,
            token: &str,
        ) -> String {
            let req = crate::event::DaemonMessage::GetSeed(crate::event::GetSeedRequest {
                pane_id: pane_id.to_string(),
                agent_id: agent_id.map(|a| a.to_string()),
                // Issue #1077: the pane's own hook capability token, which the
                // real `get-seed` CLI reads out of `DOT_AGENT_DECK_PANE_CAPABILITY`.
                // Supplied on every arm below so each one still tests the
                // identity question #916 is about rather than being refused one
                // layer earlier for want of provenance.
                token: Some(token.to_string()),
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

        // Issue #916: a pull that NAMES an agent which does not hold this pane
        // is refused, and refused without consuming anything — the seed is still
        // there for its owner two blocks down. Asserted first for that reason:
        // after a successful pull there is nothing left to prove it did not eat.
        let stranger = ask_get_seed_as(&sock, "pane-gs", Some("agent-999"), &token_gs).await;
        let stranger_resp: crate::event::GetSeedResponse =
            serde_json::from_str(stranger.trim()).expect("parse stranger get-seed reply");
        assert!(
            stranger_resp.seed.is_none(),
            "a pull naming an agent that does not hold this pane must get nothing"
        );
        assert!(
            !registry.seed_delivered_native("pane-gs"),
            "…and must not have consumed the seed on its way to being refused"
        );

        // First pull: the daemon returns the seed…
        let reply = ask_get_seed_as(&sock, "pane-gs", Some(&agent_gs), &token_gs).await;
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
        let reply2 = ask_get_seed_as(&sock, "pane-gs", Some(&agent_gs), &token_gs).await;
        let resp2: crate::event::GetSeedResponse =
            serde_json::from_str(reply2.trim()).expect("parse second get-seed reply");
        assert!(
            resp2.seed.is_none(),
            "seed must be cleared after the first pull"
        );

        // Issue #916: the pre-#916 / id-less client still works. It presents no
        // agent id, and the take falls back to the pane's LIVE occupant rather
        // than refusing — the decision recorded on `take_pending_seed_native_for`.
        registry.set_pending_seed("pane-gs", "a second opening task");
        let legacy = ask_get_seed_as(&sock, "pane-gs", None, &token_gs).await;
        let legacy_resp: crate::event::GetSeedResponse =
            serde_json::from_str(legacy.trim()).expect("parse id-less get-seed reply");
        assert_eq!(
            legacy_resp.seed.as_deref(),
            Some("a second opening task"),
            "a caller the daemon injected no agent id into must not lose its seed"
        );

        // Unknown pane → null, harmless, whether or not an id is presented.
        //
        // Issue #1077 made these hold for a STRONGER reason than they used to:
        // the token presented here was minted for `pane-gs`, so naming any other
        // pane is refused at the boundary (`Refusal::WrongPane`) before the
        // registry is consulted at all, and a refused `get-seed` answers exactly
        // as "no seed pending" does. The assertions are unchanged because the
        // observable answer is unchanged.
        let reply3 = ask_get_seed_as(&sock, "pane-unknown", None, &token_gs).await;
        let resp3: crate::event::GetSeedResponse =
            serde_json::from_str(reply3.trim()).expect("parse unknown-pane get-seed reply");
        assert!(resp3.seed.is_none());
        let reply4 = ask_get_seed_as(&sock, "pane-unknown", Some(&agent_gs), &token_gs).await;
        let resp4: crate::event::GetSeedResponse =
            serde_json::from_str(reply4.trim()).expect("parse cross-pane get-seed reply");
        assert!(
            resp4.seed.is_none(),
            "an agent naming a pane it does not hold must get nothing"
        );

        handle.abort();
        let _ = handle.await;
        registry.shutdown_all();
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
        _cwd: tempfile::TempDir,
        _dir: tempfile::TempDir,
        sock: std::path::PathBuf,
        registry: Arc<AgentPtyRegistry>,
        orchestrator_token: String,
        worker_token: String,
        worker_agent: String,
        orchestrator_agent: String,
        handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
    }

    const PROV_ORCH_PANE: &str = "prov-orchestrator-pane";
    const PROV_WORKER_PANE: &str = "prov-worker-pane";

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

            let orchestration = OrchestrationIdentity::Instance {
                id: "prov-instance".to_string(),
                name: "prov-orchestration".to_string(),
            };
            let mut app = crate::state::AppState::default();
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
            let sock = dir.path().join("hook.sock");
            let listener = IpcListener::from_tokio_listener(
                UnixListener::bind(&sock).expect("bind hook socket"),
            );
            let state: SharedState = Arc::new(tokio::sync::RwLock::new(app));
            let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            let shutdown = Arc::new(Notify::new());
            let handle = tokio::spawn({
                let registry = registry.clone();
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
                _cwd: cwd,
                _dir: dir,
                handle,
            }
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
        async fn orchestrator_saw(&self, needle: &str, budget: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + budget;
            loop {
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

        /// Whether the worker's PTY has seen the delegate pointer yet, polled
        /// for `budget`. The pointer is what `handle_delegate` writes, so its
        /// presence is proof the handler ran and its absence (after a wait) is
        /// proof it did not.
        async fn worker_saw_pointer(&self, budget: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + budget;
            loop {
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
            "the ack was written but the handler never ran — the report never reached the              orchestrator's PTY"
        );
        fx.stop().await;
    }

    /// Scenario: issue #1129 — the same `work-done`, naming the same worker
    /// pane, with no token. Before this the daemon refused it and wrote nothing
    /// back, so the sender exited 0 on a report that was dropped. It must now be
    /// told, and the report must still not reach the orchestrator.
    #[tokio::test]
    async fn hook_provenance_tells_the_sender_a_work_done_was_refused() {
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

/// Issue #1211: the daemon's best-effort binds of the pre-#1121 endpoint
/// spellings, driven through [`bind_legacy_aliases`] against paths under a
/// tempdir — never the literal `/tmp` spellings a live deck on the host uses.
///
/// Deliberately not through [`run_daemon_with`]: that loads the operator's
/// global `schedules.toml` and installs process signal handlers, neither of
/// which a lib unit test should do. What `run_daemon_with` adds on top — the
/// aliases served by the same daemon, and removed at its exit — is covered by
/// the real binary in `tests/e2e_endpoint_fallback.rs` (`error/socket/009`
/// and `/010`).
#[cfg(all(test, unix))]
mod legacy_alias_tests {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    use super::*;

    struct Paths {
        _root: tempfile::TempDir,
        hook: PathBuf,
        attach: PathBuf,
        locks: PathBuf,
    }

    /// A tempdir restated to `0o700` after creation (the umask race every
    /// socket-binding test in this crate guards against), with a lock root
    /// inside it, as `run_daemon_with` would have created before this runs.
    fn paths() -> Paths {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod the temp root");
        let locks = root.path().join("locks");
        crate::platform::fsperm::ensure_owner_only_dir(&locks).expect("lock root");
        Paths {
            hook: root.path().join("dot-agent-deck-4242.sock"),
            attach: root.path().join("dot-agent-deck-attach-4242.sock"),
            locks,
            _root: root,
        }
    }

    fn is_owner_only_socket(path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|md| md.file_type().is_socket() && md.mode() & 0o777 == 0o600)
            .unwrap_or(false)
    }

    /// Both aliases bind as owner-only sockets a client can connect to, and
    /// both are unlinked when the daemon lets go of them.
    #[tokio::test]
    async fn both_aliases_bind_and_are_unlinked_when_released() {
        let p = paths();
        let bound =
            bind_legacy_aliases(Some(p.hook.clone()), Some(p.attach.clone()), Some(&p.locks)).await;
        assert!(bound.hook.is_some() && bound.attach.is_some());
        for path in [&p.hook, &p.attach] {
            assert!(
                is_owner_only_socket(path),
                "{} is bound 0o600",
                path.display()
            );
            assert!(
                crate::endpoint_resolve::endpoint_is_answering(path),
                "an older client connecting to {} must reach a listener",
                path.display()
            );
        }

        drop(bound);
        for path in [&p.hook, &p.attach] {
            assert!(
                std::fs::symlink_metadata(path).is_err(),
                "{} must not be left behind for an older client to probe",
                path.display()
            );
        }
    }

    /// Issue #1211 property 2: a squatted old spelling costs its OWN alias and
    /// nothing else. The function cannot fail — it returns listeners, not a
    /// `Result` — so "the daemon still starts" is a type-level fact; what this
    /// pins at runtime is that the other alias still binds, the squatter is
    /// left exactly as found, and releasing the aliases does not touch it.
    ///
    /// The last shape is an entry owned by ANOTHER uid that nobody can unlink
    /// or bind over: the root directory, root-owned on every Unix host. It is
    /// #1121's wedge shape — `bind(2)` there is `EADDRINUSE` and `unlink(2)`
    /// fails — without needing a second uid in the test, and it has the
    /// property that a bug that tried either would fail rather than pass.
    /// (Running as root it is not foreign, but it is still refused, as a
    /// directory, and still cannot be removed.)
    #[tokio::test]
    async fn a_squatted_legacy_path_costs_only_its_own_alias() {
        let p = paths();
        let live = p.locks.parent().expect("root").join("live.sock");
        let _live = std::os::unix::net::UnixListener::bind(&live).expect("bind live");
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o600)).expect("0o600");

        let file = p.attach.with_extension("file");
        std::fs::write(&file, b"squatter").expect("plant a file");
        let link = p.attach.with_extension("link");
        std::os::unix::fs::symlink(&live, &link).expect("plant a symlink");
        let directory = p.attach.with_extension("dir");
        std::fs::create_dir(&directory).expect("plant a directory");

        for squatted in [file, link, directory, PathBuf::from("/")] {
            let before = std::fs::symlink_metadata(&squatted).expect("lstat the squatter");
            let bound =
                bind_legacy_aliases(Some(p.hook.clone()), Some(squatted.clone()), Some(&p.locks))
                    .await;
            assert!(
                bound.attach.is_none(),
                "{} must not be bound over",
                squatted.display()
            );
            assert!(
                bound.hook.is_some() && is_owner_only_socket(&p.hook),
                "the unsquatted alias still binds beside {}",
                squatted.display()
            );
            drop(bound);
            let after = std::fs::symlink_metadata(&squatted)
                .unwrap_or_else(|e| panic!("{} was removed: {e}", squatted.display()));
            assert_eq!(
                (before.dev(), before.ino(), before.mode()),
                (after.dev(), after.ino(), after.mode()),
                "{} must be left exactly as found, through bind and release",
                squatted.display()
            );
            assert!(
                std::fs::symlink_metadata(&p.hook).is_err(),
                "the hook alias is still released normally"
            );
        }
    }

    /// Another daemon already answering at either old spelling — an older
    /// build's, still running — keeps BOTH of its addresses: this daemon binds
    /// neither, so older clients are never split between two daemons.
    #[tokio::test]
    async fn an_answering_daemon_at_either_path_keeps_both() {
        let p = paths();
        let _older = std::os::unix::net::UnixListener::bind(&p.attach).expect("older daemon");
        std::fs::set_permissions(&p.attach, std::fs::Permissions::from_mode(0o600)).expect("0o600");

        let bound =
            bind_legacy_aliases(Some(p.hook.clone()), Some(p.attach.clone()), Some(&p.locks)).await;
        assert!(bound.hook.is_none() && bound.attach.is_none());
        assert!(
            std::fs::symlink_metadata(&p.hook).is_err(),
            "the free hook spelling must not be taken either"
        );
        assert!(crate::endpoint_resolve::endpoint_is_answering(&p.attach));
        drop(bound);
        assert!(
            crate::endpoint_resolve::endpoint_is_answering(&p.attach),
            "releasing nothing must not unlink the older daemon's socket"
        );
    }

    /// A start lock someone else holds — the older daemon mid-bind, or a
    /// wedged process — costs the aliases and returns within the bound,
    /// rather than holding up the daemon's primary attach endpoint.
    #[tokio::test]
    async fn a_held_start_lock_costs_the_aliases_not_the_start() {
        let p = paths();
        let _held =
            crate::platform::lock::acquire_spawn_lock(&lock_path_for(&p.hook, Some(&p.locks)))
                .await
                .expect("hold the legacy start lock");

        let started = std::time::Instant::now();
        let bound =
            bind_legacy_aliases(Some(p.hook.clone()), Some(p.attach.clone()), Some(&p.locks)).await;
        assert!(bound.hook.is_none() && bound.attach.is_none());
        assert!(
            started.elapsed() < LEGACY_ALIAS_LOCK_TIMEOUT * 3,
            "gave up after {:?}",
            started.elapsed()
        );
    }

    /// No alias asked for, nothing touched — every test daemon and every
    /// non-fallback production daemon.
    #[tokio::test]
    async fn no_alias_requested_binds_nothing() {
        let p = paths();
        let bound = bind_legacy_aliases(None, None, Some(&p.locks)).await;
        assert!(bound.hook.is_none() && bound.attach.is_none() && bound.aliases.is_empty());
    }
}
