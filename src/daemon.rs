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

use crate::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS};
use crate::error::DaemonError;
use crate::event::{AgentEvent, BroadcastMsg, DaemonMessage};
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
fn lock_path_for(socket_path: &Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    socket_path.as_os_str().hash(&mut hasher);
    let hash = hasher.finish();
    let basename = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("daemon");
    lock_root().join(format!("{basename}-{hash:016x}.lock"))
}

/// User-owned root directory for daemon lock files. Mirrors the socket
/// resolution order (`XDG_RUNTIME_DIR` first, then a HOME-anchored
/// fallback) but never lands in `/tmp`. Falls back to `~/.cache/dot-agent-deck`
/// when `XDG_RUNTIME_DIR` is unset — that path is owner-only (we mkdir
/// 0700) and is the standard freedesktop user cache root.
///
/// Tests can pin a deterministic root via `DOT_AGENT_DECK_LOCK_DIR` so
/// they don't pollute `$HOME/.cache` or contend with the user's real
/// daemon.
fn lock_root() -> PathBuf {
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
    /// `true` when this daemon shares its `state` Arc with a TUI in the
    /// same process (the local-mode in-TUI-process daemon).
    ///
    /// PRD #93 round-5: orchestration dispatch is now identical in both
    /// modes — the daemon always owns the role map and writes the prompt
    /// directly into the target pane's PTY. The `in_process` flag now
    /// only gates idle-shutdown (in-process daemons die with the TUI;
    /// firing idle-shutdown from underneath the TUI would race its own
    /// drop path).
    pub in_process: bool,
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
    /// shutdown entirely — the daemon stays up indefinitely.
    ///
    /// Idle shutdown is meaningful only for the standalone (`!in_process`)
    /// daemon: the in-process daemon's lifetime is already tied to the
    /// TUI's, and exiting it on idle would race the still-running TUI.
    /// The standalone constructor [`with_attach`] populates this from
    /// [`idle_shutdown_from_env`]; [`with_attach_in_process`] forces it
    /// to `None` (the daemon dies with its TUI either way).
    pub idle_shutdown: Option<Duration>,
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
            in_process: false,
            event_tx,
            client_count: Arc::new(AtomicUsize::new(0)),
            // Hook-only daemons don't accept attaches, so idle-shutdown
            // would only fire when agents == 0 — and they have no PTY
            // registry consumers either. Leave the timer off; callers
            // that want it can opt in via [`with_idle_shutdown`].
            idle_shutdown: None,
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
            in_process: false,
            event_tx,
            client_count: Arc::new(AtomicUsize::new(0)),
            idle_shutdown: idle_shutdown_from_env(),
        }
    }

    /// Same as [`with_attach`](Self::with_attach) but flags the daemon as
    /// sharing `state` with a TUI in the same process. PRD #93 round-5:
    /// the dispatch routing no longer branches on this flag — the daemon
    /// owns the role map and writes directly into target PTYs in both
    /// modes. The flag survives only to keep idle-shutdown off when the
    /// daemon's lifetime is already tied to the TUI's. Used by the
    /// local-mode TUI's in-process daemon (`run_tui_session` when the
    /// PRD #93 escape-hatch
    /// [`crate::agent_pty::DOT_AGENT_DECK_LOCAL_DAEMON`] is set).
    pub fn with_attach_in_process(state: SharedState, attach_path: PathBuf) -> Self {
        let mut daemon = Self::with_attach(state, attach_path);
        daemon.in_process = true;
        // In-process daemons die with the TUI; the idle monitor would
        // race the TUI thread by calling exit on an empty registry while
        // the TUI is still alive. Force off.
        daemon.idle_shutdown = None;
        daemon
    }

    /// PRD #93 M1.2 fluent override of the idle-shutdown window. Pass
    /// `None` to disable; pass `Some(dur)` to override the env-derived
    /// default. Useful for tests that want a short window without setting
    /// process-global env vars.
    pub fn with_idle_shutdown(mut self, dur: Option<Duration>) -> Self {
        self.idle_shutdown = dur;
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
    let lock_path = lock_path_for(socket_path);
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

    // PRD #93 round-5: orchestration dispatch is now unified across
    // in-process and external modes — both call
    // `state.handle_delegate` / `state.handle_work_done` against the
    // daemon's own `AppState`, which now owns the role map (populated
    // at `StartAgent` time) and the PTY registry. The previous
    // `is_external_mode` branching only kept the in-process variant on
    // the direct-call path while the external variant rode a broadcast
    // hop; that bifurcation was the root of every detach-window
    // round-1..4 fix. With one code path, there is nothing to lose.
    let is_in_process = daemon.in_process;

    // PRD #93 M1.2 shutdown signal — `Notify` is single-shot/level-triggered
    // enough for our needs: the idle monitor notifies once when the timer
    // expires, the hook loop's `select!` arm wakes up, and the loop exits.
    let shutdown = Arc::new(Notify::new());

    // Optionally spawn the M1.2 streaming attach server with the shared
    // client counter. We hold its JoinHandle and abort it on exit so it
    // doesn't outlive the daemon.
    let attach_handle = daemon.attach_socket_path.map(|path| {
        let registry = pty_registry.clone();
        let attach_event_tx = event_tx.clone();
        let attach_counter = client_count.clone();
        let attach_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::daemon_protocol::run_attach_server_with_counter(
                &path,
                registry,
                attach_event_tx,
                attach_counter,
                attach_state,
            )
            .await
            {
                error!("attach protocol server error: {e}");
            }
        })
    });

    // PRD #93 M1.2 idle monitor — runs only for the standalone daemon
    // path. In-process daemons skip this because their lifetime is tied
    // to the TUI (the TUI's `Drop` aborts the daemon task and unlinks
    // the sockets directly).
    //
    // PRD #93 round-2 reviewer REV-1: monitor is edge-triggered — it
    // shares the registry's `change_notify` so transitions on both sides
    // (attach counter via `ClientGuard`, registry via spawn/close/exit)
    // wake it immediately. No polling cadence to race against a brief
    // reconnect.
    let idle_handle = match (idle_shutdown, is_in_process) {
        (Some(window), false) => {
            let counter = client_count.clone();
            let registry = pty_registry.clone();
            let shutdown_signal = shutdown.clone();
            let notify = pty_registry.change_notify();
            Some(tokio::spawn(async move {
                run_idle_monitor(counter, registry, window, shutdown_signal, notify).await;
            }))
        }
        _ => None,
    };

    let result = run_hook_loop(listener, state, event_tx, pty_registry.clone(), shutdown).await;

    if let Some(h) = attach_handle {
        h.abort();
    }
    if let Some(h) = idle_handle {
        h.abort();
    }
    drop(pty_registry);

    result
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
        let is_idle = clients == 0 && agents == 0;

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
                    if counter.load(Ordering::SeqCst) == 0 && registry.live_count() == 0 {
                        info!(
                            threshold_secs = dur.as_secs(),
                            "Daemon idle window elapsed (no clients, no agents); signaling shutdown"
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
                                    state.read().await.handle_delegate(signal, &pty_registry).await;
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
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::RwLock;

    use crate::agent_pty::SpawnOptions;
    use crate::state::AppState;

    /// `tempfile::tempdir()` calls `mkdir(2)` with mode `0o700 & ~umask`. The
    /// `bind_socket` path above briefly flips the process-global umask to
    /// `0o177`, so a concurrent `mkdir` in a sibling test can land during
    /// that window and produce a directory with mode `0o600` — no execute
    /// bit, breaking lookups for files inside. Re-apply 0o700 after creation
    /// so these tests can run in parallel without stat'ing into a non-x dir.
    fn race_safe_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[tokio::test]
    async fn socket_is_0600_immediately_after_bind() {
        // Proves the umask-before-bind change closes the TOCTOU window: the
        // socket inode must already be 0o600 at `bind(2)` time, with no
        // reliance on a post-bind chmod.
        let dir = race_safe_tempdir();
        let sock_path = dir.path().join("immediate.sock");

        let _listener = bind_socket(&sock_path).expect("bind should succeed");

        let meta = std::fs::metadata(&sock_path).expect("socket file should exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, SOCKET_MODE,
            "expected 0600 immediately after bind (no chmod), got {mode:o}"
        );
    }

    #[tokio::test]
    async fn socket_is_chmod_0600_after_bind() {
        let dir = race_safe_tempdir();
        let sock_path = dir.path().join("test.sock");
        let state = Arc::new(RwLock::new(AppState::default()));

        let sock = sock_path.clone();
        let handle = tokio::spawn(async move {
            run_daemon(&sock, state).await.unwrap();
        });

        // Wait for bind + chmod to land.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let meta = std::fs::metadata(&sock_path).expect("socket file should exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE, "expected 0600, got {mode:o}");

        handle.abort();
    }

    #[test]
    fn daemon_owns_pty_registry() {
        // M1.1 capability check: a Daemon can be built and drives an
        // AgentPtyRegistry that spawns and cleans up agent PTYs. This is the
        // in-process surface; the wire protocol that exposes it lands in M1.2.
        let state = Arc::new(RwLock::new(AppState::default()));
        let daemon = Daemon::new(state);

        let id = daemon
            .pty_registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        assert_eq!(daemon.pty_registry.agent_ids(), vec![id.clone()]);
        daemon.pty_registry.close_agent(&id).unwrap();
        assert!(daemon.pty_registry.is_empty());
    }

    #[tokio::test]
    async fn idle_monitor_signals_when_clients_and_agents_zero() {
        // PRD #93 M1.2: with both counters at zero from the start, the
        // idle monitor must signal `shutdown` within `threshold`. We give
        // it a small slack window above `threshold` so a slow CI box doesn't
        // flake on the sleep cadence.
        let client_count = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(AgentPtyRegistry::new());
        let shutdown = Arc::new(Notify::new());
        let change_notify = registry.change_notify();

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        let monitor_notify = change_notify.clone();
        tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                Duration::from_millis(150),
                monitor_shutdown,
                monitor_notify,
            )
            .await;
        });

        tokio::time::timeout(Duration::from_secs(2), shutdown.notified())
            .await
            .expect("idle monitor must signal shutdown when clients and agents are both zero");
    }

    #[tokio::test]
    async fn idle_monitor_resets_when_client_appears() {
        // The idle window must restart if a client connects mid-window.
        // We bump the counter halfway through the window; the monitor must
        // *not* fire by the original threshold and must require another
        // full window of zero-clients-zero-agents from the drop point.
        //
        // PRD #93 round-2 reviewer REV-1: the monitor is now edge-triggered,
        // so the test signals `change_notify` after each counter mutation
        // — exactly the way production's `ClientGuard` does. A test that
        // mutated the counter without notifying would never wake the
        // monitor and the assertion would be vacuous.
        let client_count = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(AgentPtyRegistry::new());
        let shutdown = Arc::new(Notify::new());
        let change_notify = registry.change_notify();

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        let monitor_notify = change_notify.clone();
        let threshold = Duration::from_millis(300);
        let handle = tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                threshold,
                monitor_shutdown,
                monitor_notify,
            )
            .await;
        });

        // Half-way: bump client count so the idle accumulator resets.
        tokio::time::sleep(Duration::from_millis(150)).await;
        client_count.fetch_add(1, Ordering::SeqCst);
        change_notify.notify_one();

        // Wait *past* the original threshold deadline. With the reset, no
        // notification should arrive yet.
        let early = tokio::time::timeout(Duration::from_millis(250), shutdown.notified()).await;
        assert!(
            early.is_err(),
            "idle monitor must not fire while a client is attached"
        );

        // Drop the client; monitor should fire again after `threshold` elapses.
        client_count.fetch_sub(1, Ordering::SeqCst);
        change_notify.notify_one();
        tokio::time::timeout(threshold * 4, shutdown.notified())
            .await
            .expect("idle monitor must re-fire once clients drop back to zero");

        handle.abort();
    }

    #[tokio::test]
    async fn idle_monitor_holds_while_agents_alive() {
        // An agent surviving past TUI exit is exactly the case the PRD calls
        // out: the daemon must stay up to host it. Simulate by registering an
        // agent (real PTY) and verifying the monitor never fires within a
        // multiple of the threshold.
        let client_count = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");
        let shutdown = Arc::new(Notify::new());
        let change_notify = registry.change_notify();

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        let monitor_notify = change_notify.clone();
        let threshold = Duration::from_millis(150);
        let handle = tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                threshold,
                monitor_shutdown,
                monitor_notify,
            )
            .await;
        });

        let res = tokio::time::timeout(threshold * 5, shutdown.notified()).await;
        assert!(
            res.is_err(),
            "idle monitor must not fire while an agent is alive in the registry"
        );

        handle.abort();
        registry.close_agent(&id).unwrap();
    }

    #[test]
    fn idle_shutdown_env_parses_disabled() {
        // STATE_DIR_ENV_LOCK guards the process-global state-dir env mutations
        // used elsewhere in the suite. We reuse it as a coarse lock for any
        // env-var mutation in the daemon tests so we don't race other tests
        // that twiddle the same process-global env.
        let _g = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS).ok();
        // SAFETY: lock held; setting and restoring env vars under that lock.
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS, "0");
        }
        assert!(
            idle_shutdown_from_env().is_none(),
            "explicit 0 must disable idle shutdown"
        );
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS, "42");
        }
        assert_eq!(idle_shutdown_from_env(), Some(Duration::from_secs(42)));
        unsafe {
            std::env::set_var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS, "not-a-number");
        }
        assert_eq!(
            idle_shutdown_from_env(),
            Some(Duration::from_secs(DEFAULT_IDLE_SHUTDOWN_SECS)),
            "unparseable values must fall back to the default, not silently disable"
        );
        unsafe {
            std::env::remove_var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS);
        }
        assert_eq!(
            idle_shutdown_from_env(),
            Some(Duration::from_secs(DEFAULT_IDLE_SHUTDOWN_SECS))
        );
        // SAFETY: restoring prior env state.
        unsafe {
            match prev {
                Some(v) => std::env::set_var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS, v),
                None => std::env::remove_var(DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS),
            }
        }
    }

    #[tokio::test]
    async fn run_daemon_with_refuses_to_clobber_live_socket() {
        // PRD #93 M1.3: if another daemon is already bound at the hook
        // socket path, the new daemon must refuse to start rather than
        // unlinking the live inode and silently rebinding. The probe-
        // connect distinguishes a live winner from a stale crash leftover.
        let dir = race_safe_tempdir();
        let sock_path = dir.path().join("hook.sock");

        // Pretend-winner: a real listener bound at the path. We hold the
        // handle so the inode stays connectable for the duration of the
        // test (matching what a healthy daemon's hook loop would look like
        // to an outside prober).
        let _winner = bind_socket(&sock_path).expect("winner bind should succeed");

        let state = Arc::new(RwLock::new(AppState::default()));
        let daemon =
            Daemon::with_attach(state, dir.path().join("attach.sock")).with_idle_shutdown(None);
        let err = run_daemon_with(&sock_path, daemon)
            .await
            .expect_err("second daemon must refuse to start while the first is live");
        match err {
            DaemonError::Io(e) => {
                assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
            }
            other => panic!("expected AddrInUse Io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_daemon_with_recovers_from_stale_socket() {
        // Bind-and-drop simulates a daemon that died without unlinking.
        // The inode survives on disk but `connect(2)` returns
        // ECONNREFUSED, so the new daemon may safely unlink and rebind.
        let dir = race_safe_tempdir();
        let sock_path = dir.path().join("hook.sock");
        {
            let _stale = bind_socket(&sock_path).expect("stale bind should succeed");
        }
        assert!(
            sock_path.exists(),
            "precondition: stale socket file must remain after listener drop"
        );

        let state = Arc::new(RwLock::new(AppState::default()));
        let daemon = Daemon::with_attach(state, dir.path().join("attach.sock"))
            .with_idle_shutdown(Some(Duration::from_millis(200)));
        let res = tokio::time::timeout(Duration::from_secs(3), run_daemon_with(&sock_path, daemon))
            .await
            .expect("daemon must finish despite stale inode");
        res.expect("daemon should reclaim a stale socket and exit cleanly on idle");
    }

    #[tokio::test]
    async fn run_daemon_with_exits_on_idle() {
        // End-to-end: a daemon constructed with a short idle window must
        // exit on its own (no abort, no panic) once no clients are
        // connected. We verify by joining the future and checking it
        // returns Ok within a bounded multiple of the window.
        let dir = race_safe_tempdir();
        let sock_path = dir.path().join("hook.sock");
        let attach_path = dir.path().join("attach.sock");
        let state = Arc::new(RwLock::new(AppState::default()));

        let daemon = Daemon::with_attach(state, attach_path.clone())
            .with_idle_shutdown(Some(Duration::from_millis(200)));
        let res = tokio::time::timeout(Duration::from_secs(3), run_daemon_with(&sock_path, daemon))
            .await
            .expect("daemon must exit within bounded window of idle threshold");
        res.expect("daemon should return Ok on idle shutdown");
    }

    #[tokio::test]
    async fn run_daemon_with_serializes_concurrent_starts_on_lock() {
        // PRD #93 round-2 auditor BLOCKER: two `run_daemon_with` calls
        // against the same socket path must serialize on the sibling
        // `.lock` file's flock(2). Exactly one should bind successfully;
        // the loser must fail with AddrInUse against the winner's live
        // socket (the live-socket probe inside the lock catches it).
        // Without the flock, both could race past the probe-and-remove
        // and both bind a fresh inode, silently clobbering each other.
        let dir = race_safe_tempdir();
        let sock_path = Arc::new(dir.path().join("hook.sock"));
        let attach1 = dir.path().join("attach1.sock");
        let attach2 = dir.path().join("attach2.sock");

        let sock1 = sock_path.clone();
        let h1 = tokio::spawn(async move {
            let state = Arc::new(RwLock::new(AppState::default()));
            let daemon = Daemon::with_attach(state, attach1)
                .with_idle_shutdown(Some(Duration::from_secs(2)));
            run_daemon_with(&sock1, daemon).await
        });

        // Give the first task a moment to bind so the second's probe
        // observes a live socket. Without the head-start the test would
        // become a coin-flip — either ordering is technically legal under
        // the flock (one wins, one loses), but pinning the head-start
        // gives the assertion below a stable winner.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let sock2 = sock_path.clone();
        let h2 = tokio::spawn(async move {
            let state = Arc::new(RwLock::new(AppState::default()));
            let daemon = Daemon::with_attach(state, attach2)
                .with_idle_shutdown(Some(Duration::from_secs(2)));
            run_daemon_with(&sock2, daemon).await
        });

        // The second must fail with AddrInUse — the lock ensured the
        // probe ran AFTER the first bound, so the live-socket probe in
        // the second task observed a connectable socket.
        let r2 = tokio::time::timeout(Duration::from_secs(3), h2)
            .await
            .expect("loser must return within a bounded window")
            .expect("loser task should not panic");
        match r2 {
            Err(DaemonError::Io(e)) => {
                assert_eq!(
                    e.kind(),
                    io::ErrorKind::AddrInUse,
                    "loser must surface AddrInUse, got {e:?}"
                );
            }
            Ok(()) => panic!("second daemon must NOT bind while the first is live"),
            Err(other) => panic!("expected AddrInUse Io error, got {other:?}"),
        }

        // Clean up the first task. It's still running its idle window;
        // aborting drops it. The lock file remains on disk (cheap, empty)
        // — that's expected, the lock is held via flock not file presence.
        h1.abort();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn run_daemon_with_uses_user_owned_lock_dir() {
        // PRD #93 round-4 auditor BLOCKER: the lock path must root in a
        // user-owned directory, NEVER under the world-writable `/tmp`
        // even when the socket itself falls back there. We pin the lock
        // root to a tempdir via DOT_AGENT_DECK_LOCK_DIR (test-only env
        // hook) and assert the resolved lock lives inside it.
        let _g = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_lock = std::env::var("DOT_AGENT_DECK_LOCK_DIR").ok();

        let dir = race_safe_tempdir();
        let lock_dir = dir.path().join("locks");
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_LOCK_DIR", &lock_dir);
        }

        let sock_path = dir.path().join("hook.sock");
        let attach_path = dir.path().join("attach.sock");
        let lock_path = lock_path_for(&sock_path);
        assert!(
            lock_path.starts_with(&lock_dir),
            "lock path must root in the user-owned lock dir, got {}",
            lock_path.display()
        );
        assert!(
            !lock_path.starts_with("/tmp/dot-agent-deck"),
            "lock path must not land under any predictable /tmp prefix, got {}",
            lock_path.display()
        );

        let state = Arc::new(RwLock::new(AppState::default()));
        let daemon = Daemon::with_attach(state, attach_path)
            .with_idle_shutdown(Some(Duration::from_millis(150)));
        let res = tokio::time::timeout(Duration::from_secs(3), run_daemon_with(&sock_path, daemon))
            .await
            .expect("daemon must exit within bounded window of idle threshold");
        res.expect("daemon should return Ok on idle shutdown");

        assert!(
            lock_path.exists(),
            "lock file should remain on disk for the next start"
        );
        let parent_mode =
            std::fs::metadata(&lock_dir).expect("lock dir must exist after daemon start");
        assert_eq!(
            parent_mode.permissions().mode() & 0o777,
            0o700,
            "lock dir must be 0o700 so a foreign uid can't pre-create entries inside it"
        );

        // SAFETY: same env lock held; restoring previous value.
        unsafe {
            match prev_lock {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_LOCK_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_LOCK_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn lock_dir_falls_back_to_home_cache_when_xdg_unset() {
        // PRD #93 round-4 auditor BLOCKER regression: with both
        // XDG_RUNTIME_DIR and DOT_AGENT_DECK_LOCK_DIR unset, the lock
        // root must resolve under $HOME/.cache/dot-agent-deck — never
        // /tmp. Without this, the auditor's DoS vector (a foreign uid
        // pre-creating /tmp/<sock>.sock.lock) re-opens.
        let _g = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_lock = std::env::var("DOT_AGENT_DECK_LOCK_DIR").ok();
        let prev_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let prev_home = std::env::var("HOME").ok();
        let home_dir = tempfile::tempdir().expect("home tempdir");
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::remove_var("DOT_AGENT_DECK_LOCK_DIR");
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("HOME", home_dir.path());
        }

        let resolved = lock_root();
        let expected = home_dir.path().join(".cache").join("dot-agent-deck");
        assert_eq!(
            resolved, expected,
            "lock root must fall back to $HOME/.cache/dot-agent-deck when XDG and override are unset"
        );
        // Even when the socket path itself is in /tmp, the lock must
        // root under our configured HOME (specifically `.cache/...`),
        // not next to the socket in /tmp where a foreign uid can
        // pre-create files.
        let lock_path = lock_path_for(std::path::Path::new("/tmp/dot-agent-deck-1000.sock"));
        assert!(
            lock_path.starts_with(&expected),
            "lock must root under $HOME/.cache/dot-agent-deck even when the socket lives in /tmp, got {}",
            lock_path.display()
        );

        // SAFETY: same env lock held; restoring previous values.
        unsafe {
            match prev_lock {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_LOCK_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_LOCK_DIR"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn daemon_startup_unaffected_by_pre_created_tmp_lock() {
        // PRD #93 round-4 auditor BLOCKER DoS regression: a local
        // foreign uid pre-creating a file at the OLD `/tmp` lock path
        // (and holding an exclusive flock on it forever) must NOT block
        // daemon startup for the target user. With the lock now anchored
        // under a user-owned root, the daemon's flock target lives
        // elsewhere — the pre-created /tmp file is irrelevant.
        let _g = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_lock = std::env::var("DOT_AGENT_DECK_LOCK_DIR").ok();

        let dir = race_safe_tempdir();
        let lock_dir = dir.path().join("locks");
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_LOCK_DIR", &lock_dir);
        }

        // Pre-create a file at the OLD sibling-lock path (what the
        // pre-round-4 code would have used) and hold a flock on it for
        // the duration of the test. If lock_path_for still pointed
        // there, the daemon's `acquire_spawn_lock` would block on this
        // flock indefinitely and the test would time out.
        let sock_path = dir.path().join("hook.sock");
        let old_lock_path = {
            let mut s = sock_path.as_os_str().to_owned();
            s.push(".lock");
            std::path::PathBuf::from(s)
        };
        let attacker_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&old_lock_path)
            .expect("attacker can create file at sibling lock path");
        use std::os::unix::io::AsRawFd;
        // SAFETY: valid fd; LOCK_EX | LOCK_NB returns immediately. We
        // hold the file across the daemon run.
        let rc = unsafe { libc::flock(attacker_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "attacker must successfully flock the old path");

        let attach_path = dir.path().join("attach.sock");
        let state = Arc::new(RwLock::new(AppState::default()));
        let daemon = Daemon::with_attach(state, attach_path)
            .with_idle_shutdown(Some(Duration::from_millis(150)));
        let res = tokio::time::timeout(Duration::from_secs(3), run_daemon_with(&sock_path, daemon))
            .await
            .expect(
                "daemon must NOT block on the foreign-held /tmp lock — \
                 the lock now lives under a user-owned dir",
            );
        res.expect("daemon should return Ok on idle shutdown");

        // Tidy up: release attacker's flock by dropping the file.
        drop(attacker_file);

        // SAFETY: same env lock held; restoring previous value.
        unsafe {
            match prev_lock {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_LOCK_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_LOCK_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn idle_monitor_survives_rapid_disconnect_reconnect_cycle() {
        // PRD #93 round-4 reviewer BLOCKER: the round-2 abort-based
        // cancel was racy. The reviewer's specific scenario:
        // counter goes 1→0 (timer scheduled at T+N), 0→1 (cancel
        // queued), 1→0 again (a new timer should run at T+N+ε). But
        // the *first* timer can still wake at its original deadline T+N
        // — between the timer task's wake-up and the monitor's
        // abort/cancel landing — and fire shutdown spuriously, even
        // though a client was attached for part of the window.
        //
        // The generation-counter fix invalidates any in-flight timer
        // synchronously on every 0→1 transition; on the next 1→0 a
        // fresh timer starts from zero. We drive the precise 1→0→1→0
        // sequence with a sub-window sleep between transitions to land
        // the old timer's wake inside the new window, and assert
        // shutdown does NOT fire before the new timer's deadline.
        let client_count = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(AgentPtyRegistry::new());
        let shutdown = Arc::new(Notify::new());
        let change_notify = registry.change_notify();

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        let monitor_notify = change_notify.clone();
        let threshold = Duration::from_millis(200);
        let handle = tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                threshold,
                monitor_shutdown,
                monitor_notify,
            )
            .await;
        });

        // T=0: counter starts at 0 (idle) → monitor arms timer A at deadline ~T+200ms.
        change_notify.notify_one();
        // Wait close to the deadline but not past it: 150ms < 200ms.
        // Crucially, the *next* transitions must land BEFORE the abort
        // path of the old code could possibly land — under the round-2
        // abort approach, timer A's wake at T+200 was racing with the
        // cancel. Under the generation counter, the new timer is
        // scheduled at T+150+ε, deadline T+350+ε, and timer A's wake
        // at T+200 sees the generation has moved and bails.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // 0→1: invalidate timer A.
        client_count.fetch_add(1, Ordering::SeqCst);
        change_notify.notify_one();
        // Brief reconnect — sub-window, just long enough to make the
        // monitor process the 0→1 transition and bump the generation.
        tokio::time::sleep(Duration::from_millis(10)).await;
        // 1→0: schedule timer B at deadline ~T+360.
        client_count.fetch_sub(1, Ordering::SeqCst);
        change_notify.notify_one();

        // Wait through timer A's original deadline (T+200) plus a slack
        // margin. Under the abort-race bug, timer A fires here and
        // shutdown lands ~T+200. Under the generation-counter fix,
        // timer A's wake sees the moved generation and exits silently;
        // only timer B can fire shutdown, and it's not due until ~T+360.
        let res = tokio::time::timeout(Duration::from_millis(120), shutdown.notified()).await;
        assert!(
            res.is_err(),
            "idle monitor must NOT fire on the OLD timer's deadline after a reconnect+disconnect cycle"
        );

        // Sanity: timer B does eventually fire (joint-zero still holds).
        tokio::time::timeout(threshold * 4, shutdown.notified())
            .await
            .expect("the freshly-armed timer must still fire once its own deadline arrives");

        handle.abort();
    }

    #[tokio::test]
    async fn idle_monitor_survives_brief_reconnect_inside_window() {
        // PRD #93 round-2 reviewer REV-1: an edge-triggered monitor must
        // tolerate a disconnect+reconnect that hits the joint-zero gate
        // for less than `threshold`. Simulates: client present, drops,
        // monitor arms timer; well before timer fires, client reconnects;
        // shutdown must NOT fire even after timer would have expired.
        let client_count = Arc::new(AtomicUsize::new(1));
        let registry = Arc::new(AgentPtyRegistry::new());
        let shutdown = Arc::new(Notify::new());
        let change_notify = registry.change_notify();

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        let monitor_notify = change_notify.clone();
        let threshold = Duration::from_millis(300);
        let handle = tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                threshold,
                monitor_shutdown,
                monitor_notify,
            )
            .await;
        });

        // Give the monitor one notify so it parks on `change_notify`. The
        // start state is (1 client, 0 agents) — not idle — so on the first
        // loop iteration the timer stays None and the monitor parks.
        change_notify.notify_one();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Client disconnects: counter→0, signal.
        client_count.fetch_sub(1, Ordering::SeqCst);
        change_notify.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Reconnect before threshold elapses.
        client_count.fetch_add(1, Ordering::SeqCst);
        change_notify.notify_one();

        // Wait well past the original timer's expiry. With the abort
        // path working, shutdown must NOT fire — the monitor saw the
        // counter go back to 1 and aborted the timer.
        let res = tokio::time::timeout(threshold * 3, shutdown.notified()).await;
        assert!(
            res.is_err(),
            "edge-triggered monitor must NOT fire shutdown when a client reconnects inside the idle window"
        );

        handle.abort();
    }
}
