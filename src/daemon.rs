use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Daemon-wide broadcast capacity for `BroadcastMsg`s forwarded to attached
/// TUIs (PRD #76 M2.17 / M2.19). Generous so a slow client doesn't drop
/// events during a normal burst; a subscriber that falls further behind
/// than this is signalled via `RecvError::Lagged` and the per-connection
/// forwarder drops the connection (the TUI reconnects).
const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Maximum number of `BroadcastMsg`s the daemon keeps in [`PendingBroadcasts`]
/// while no TUI is attached. Bounds the worst-case memory cost of a runaway
/// worker (or an orchestrator firing many delegates) during a detach window.
/// Beyond this, the oldest entries are evicted — the alternative (unbounded
/// growth) would let a single misbehaving pane balloon daemon memory.
const PENDING_BROADCAST_CAP: usize = 256;

/// Static variant name for a `BroadcastMsg`. Used in eviction logs so an
/// operator who sees the buffer overflow warning can tell at a glance
/// whether a Delegate signal, a WorkDone, or an Event was dropped without
/// having to parse the structured `Debug` impl. Cheap (`&'static str`),
/// matches the names used in the wire-format `#[serde(rename = ...)]`.
fn broadcast_variant_name(msg: &BroadcastMsg) -> &'static str {
    match msg {
        BroadcastMsg::Event(_) => "event",
        BroadcastMsg::Delegate(_) => "delegate",
        BroadcastMsg::WorkDone(_) => "work_done",
    }
}

/// PRD #93 round-3 test instrumentation: pause point inside
/// [`crate::daemon_protocol::handle_subscribe_events`] between the main
/// select loop break and the receiver salvage step. Production code
/// never installs this; the regression test for the detach-race salvage
/// path drives the race against `broadcast::send` deterministically by
/// installing a gate, waiting until `reached` fires (the handler is
/// parked between loop-break and salvage), pushing a message into the
/// dying rx via the hook socket, then signalling `proceed` so the
/// handler runs salvage with the buffered message still in rx.
///
/// Exposed unconditionally (rather than `#[cfg(test)]`) because
/// integration tests under `tests/` compile against the lib crate
/// without the `test` cfg and need to install a gate. The production
/// cost is one mutex acquire + `Option` check per subscribe-events
/// disconnect — negligible.
#[derive(Debug, Default)]
#[doc(hidden)]
pub struct SubscribeEventsTestGate {
    /// Handler fires `notify_one` after the main loop breaks and before
    /// salvage runs. The test awaits this to confirm rx is alive and no
    /// longer being polled.
    pub reached: Notify,
    /// Test fires `notify_one` to release the handler into the salvage
    /// step.
    pub proceed: Notify,
}

/// Bounded replay buffer for orchestration `BroadcastMsg`s (Delegate /
/// WorkDone) that arrived while no TUI was subscribed.
///
/// In external-daemon mode the daemon is a dumb pipe: it cannot validate
/// these signals locally (no role map), so it forwards them to the
/// attached TUI over the broadcast channel. When the user detaches the
/// deck and a worker calls `dot-agent-deck delegate` or `work-done`,
/// `broadcast::Sender::send` returns `Err` because zero subscribers
/// exist — and without this buffer the signal is silently lost forever.
///
/// We record only on send-Err (so a normally-connected subscriber never
/// sees a duplicate), and drain on the next [`AttachRequest::SubscribeEvents`]
/// connection before joining the live stream.
///
/// Events (`BroadcastMsg::Event`) are **not** buffered: the event stream
/// is continuous and the TUI's `apply_event` already tolerates loss on a
/// reconnect (it pulls a fresh `list_agents` snapshot to rebuild state).
/// Buffering events would also balloon memory unboundedly on a busy
/// project — orchestration signals are the rare, irreplaceable case.
#[derive(Debug, Default)]
pub struct PendingBroadcasts {
    inner: Mutex<VecDeque<BroadcastMsg>>,
    /// PRD #93 round-3 test instrumentation; production code never sets
    /// this. See [`SubscribeEventsTestGate`].
    subscribe_events_test_gate: Mutex<Option<Arc<SubscribeEventsTestGate>>>,
}

impl PendingBroadcasts {
    /// Empty buffer with capacity reserved up to [`PENDING_BROADCAST_CAP`].
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(PENDING_BROADCAST_CAP)),
            subscribe_events_test_gate: Mutex::new(None),
        }
    }

    /// PRD #93 round-3 test instrumentation; production code never calls
    /// this. Install a gate that the subscribe-events handler awaits on
    /// between its main loop break and the receiver salvage step. See
    /// [`SubscribeEventsTestGate`].
    #[doc(hidden)]
    pub fn install_subscribe_events_test_gate(&self, gate: Option<Arc<SubscribeEventsTestGate>>) {
        *self
            .subscribe_events_test_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = gate;
    }

    /// PRD #93 round-3 test instrumentation; production code returns
    /// `None`. Cloned out under a short-held lock so the caller can
    /// `await` on the gate without holding the mutex.
    #[doc(hidden)]
    pub fn subscribe_events_test_gate(&self) -> Option<Arc<SubscribeEventsTestGate>> {
        self.subscribe_events_test_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Push `msg` onto the buffer, evicting the oldest entry if the cap is
    /// reached. The caller is expected to invoke this only on send-Err
    /// (zero subscribers): a normally-attached TUI receives the signal
    /// live and recording here would duplicate it on reconnect.
    ///
    /// Eviction is logged at `warn!`: a silent drop hides the case where a
    /// long detach storm + a runaway worker push the buffer past its cap
    /// and the orchestrator quietly misses signals. The log lets operators
    /// correlate "we lost a delegate/work-done" with "buffer overflowed."
    pub fn record(&self, msg: BroadcastMsg) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() >= PENDING_BROADCAST_CAP
            && let Some(dropped) = g.pop_front()
        {
            let dropped_variant = broadcast_variant_name(&dropped);
            let incoming_variant = broadcast_variant_name(&msg);
            warn!(
                dropped_variant,
                incoming_variant,
                depth = g.len() + 1,
                cap = PENDING_BROADCAST_CAP,
                "PendingBroadcasts buffer at cap — evicting oldest entry (signal will not replay on reattach)"
            );
        }
        g.push_back(msg);
    }

    /// Re-enqueue a batch of messages at the *front* of the buffer,
    /// preserving the original FIFO order between the entries in `msgs`.
    /// Used by the `SubscribeEvents` replay drain to put messages back
    /// when a write to the client fails partway through — without this,
    /// `drain()`-then-fail-mid-write would silently lose every entry the
    /// drain pulled out (PRD #93 reviewer/auditor finding on adb13e9).
    ///
    /// If re-enqueuing would exceed [`PENDING_BROADCAST_CAP`], newer
    /// entries already in the buffer (pushed at the back by concurrent
    /// `record` calls during the drain window) are evicted first via
    /// `pop_back`. This preserves the older, failed-to-deliver batch the
    /// caller is trying to save — they're the ones the next subscriber
    /// most needs to receive in FIFO order.
    pub fn push_front_batch(&self, msgs: Vec<BroadcastMsg>) {
        if msgs.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        // Iterate in reverse so the original head of `msgs` ends up at the
        // front of the buffer (each push_front prepends one element).
        for msg in msgs.into_iter().rev() {
            while g.len() >= PENDING_BROADCAST_CAP {
                // Newer entries (push_back-ed during the drain window) lose
                // out so the failed-batch FIFO ordering is preserved.
                if let Some(dropped) = g.pop_back() {
                    let dropped_variant = broadcast_variant_name(&dropped);
                    warn!(
                        dropped_variant,
                        depth = g.len() + 1,
                        cap = PENDING_BROADCAST_CAP,
                        "PendingBroadcasts buffer at cap during requeue — evicting newest entry"
                    );
                } else {
                    break;
                }
            }
            g.push_front(msg);
        }
    }

    /// Atomically take every buffered entry. Called by the attach server
    /// when a new `SubscribeEvents` subscriber joins, before it enters
    /// the live `recv` loop. The first subscriber to attach after a
    /// detach window drains the queue; later subscribers (rare —
    /// typically only one TUI is attached at a time) see nothing, which
    /// matches the at-most-one-handler invariant the TUI side enforces
    /// for orchestrator feedback anyway.
    pub fn drain(&self) -> Vec<BroadcastMsg> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.drain(..).collect()
    }
}

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

/// Sibling lock file path for a daemon socket. Used to serialize concurrent
/// `daemon serve` starts against the same `socket_path` (PRD #93 round-2
/// auditor BLOCKER). Each socket gets a dedicated `.lock` file so daemons
/// at different paths don't contend with each other — and the hook socket
/// and attach socket end up on the same lock path scheme without further
/// coordination (each `run_daemon_with` only protects its own bind).
fn lock_path_for(socket_path: &Path) -> PathBuf {
    let mut s = socket_path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
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
    /// same process (the local-mode in-TUI-process daemon). The delegate
    /// router uses this to decide between a direct `state.handle_delegate`
    /// call (fast path, no socket round-trip, role-validation runs against
    /// the TUI's own role map) and a `BroadcastMsg::Delegate` over the
    /// attach socket (external-daemon mode, where the daemon's own state
    /// has no role map and a subscribing TUI runs the guard instead).
    ///
    /// Defaults to `false`; the in-process constructor `with_attach_in_process`
    /// flips it explicitly. Standalone `daemon serve` and tests use the
    /// default so the broadcast path is exercised end-to-end.
    pub in_process: bool,
    /// Daemon-wide broadcast of hook messages (PRD #76 M2.17 for events,
    /// extended in M2.19 to carry delegate signals). The hook loop wraps
    /// every successfully-parsed payload in a `BroadcastMsg` variant and
    /// publishes it here; the attach server hands each
    /// `SubscribeEvents` connection its own `Receiver`. Delegate signals
    /// in external-daemon mode rely on this bridge because the daemon's
    /// own `AppState` has no role map to validate against — see
    /// `BroadcastMsg::Delegate` and `state::handle_delegate`. In the
    /// in-process daemon path the receiver is unused — the TUI shares
    /// `state` directly.
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
    /// Bounded ring buffer of orchestration `BroadcastMsg`s (Delegate /
    /// WorkDone) that arrived while no TUI was subscribed. Populated in
    /// external-daemon mode only — see [`PendingBroadcasts`] for the
    /// rationale, and the Delegate / WorkDone arms in `run_hook_loop`
    /// for the recording site. Drained by the next `SubscribeEvents`
    /// subscriber before it joins the live stream.
    pub pending_broadcasts: Arc<PendingBroadcasts>,
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
            pending_broadcasts: Arc::new(PendingBroadcasts::new()),
        }
    }

    /// Daemon configured to also serve the M1.2 streaming attach protocol
    /// on `attach_path`. Hook ingestion still uses the path passed to
    /// `run_daemon_with`. Used by `daemon serve` and tests — the daemon's
    /// `state` is not shared with any TUI, so delegate signals are routed
    /// via the broadcast for a subscribing TUI to handle.
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
            pending_broadcasts: Arc::new(PendingBroadcasts::new()),
        }
    }

    /// Same as [`with_attach`](Self::with_attach) but flags the daemon as
    /// sharing `state` with a TUI in the same process. Delegate signals
    /// take the direct `state.handle_delegate` path so the local TUI sees
    /// them without needing to subscribe to its own daemon's broadcast.
    /// Used by the local-mode TUI's in-process daemon (`run_tui_session`
    /// when the PRD #93 escape-hatch [`crate::agent_pty::DOT_AGENT_DECK_LOCAL_DAEMON`]
    /// is set).
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
    // Fix: hold an exclusive `flock(2)` over a sibling `{socket}.lock`
    // file across the entire probe → remove → bind sequence. The
    // `daemon_attach::ensure_daemon_running` path already uses this same
    // primitive on `<state_dir>/spawn.lock` for the launcher side; we
    // reuse it here so the two halves of the racing pair share one
    // serialization point. The lock is released as soon as `bind_socket`
    // succeeds — afterwards, any further start attempt's probe will see
    // the live socket and return AddrInUse without needing the lock.
    let lock_path = lock_path_for(socket_path);
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
    let pending_broadcasts = daemon.pending_broadcasts;

    // Route delegates by daemon mode, not by attach-socket presence: the
    // in-process TUI daemon ALSO binds an attach socket (so a future
    // `connect` from another laptop could reach it), but its `state`
    // is shared with the TUI, so a direct call is correct and the
    // broadcast would have no subscriber. Standalone `daemon serve`
    // and tests construct via the default `with_attach` (in_process=false)
    // so the broadcast path runs and any attached TUI's subscriber
    // re-runs role validation against its own state.
    let is_external_mode = !daemon.in_process;

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
        let attach_pending = pending_broadcasts.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::daemon_protocol::run_attach_server_with_counter(
                &path,
                registry,
                attach_event_tx,
                attach_counter,
                attach_pending,
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
    let idle_handle = match (idle_shutdown, is_external_mode) {
        (Some(window), true) => {
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

    let result = run_hook_loop(
        listener,
        state,
        event_tx,
        is_external_mode,
        pending_broadcasts,
        shutdown,
    )
    .await;

    if let Some(h) = attach_handle {
        h.abort();
    }
    if let Some(h) = idle_handle {
        h.abort();
    }
    drop(pty_registry);

    result
}

/// PRD #93 M1.2 idle monitor — edge-triggered.
///
/// Originally a polling loop (round 1). Round-2 reviewer REV-1 flagged the
/// reconnect-race: between two polls a client could disconnect+reconnect
/// briefly, and if the poll cadence happened to land in the zero-clients
/// window the timer would start; if a follow-up poll happened to miss
/// the reconnect-then-disconnect cycle the daemon could fire shutdown
/// while a TUI was actively re-attaching.
///
/// Edge-triggered fixes this: every counter transition (client connect /
/// disconnect, agent spawn / close / exit) signals `change_notify`. The
/// monitor parks on that signal and re-evaluates the joint-zero gate. On
/// entering the idle state, it spawns a child timer task that fires
/// shutdown after `threshold` *iff* the gate still holds when the timer
/// expires. On leaving the idle state, it aborts the pending timer.
///
/// The child timer's re-check on expiry is the second line of defense:
/// it handles the race where the timer's `sleep(threshold)` future is
/// just about to complete when a reconnect arrives, and our abort loses
/// the race with the wake-up. Re-reading the counters before signaling
/// shutdown keeps that race from misfiring.
async fn run_idle_monitor(
    client_count: Arc<AtomicUsize>,
    pty_registry: Arc<AgentPtyRegistry>,
    threshold: Duration,
    shutdown: Arc<Notify>,
    change_notify: Arc<Notify>,
) {
    // RAII guard so aborting `run_idle_monitor` (the outer
    // `run_daemon_with` cleanup path) doesn't leak a still-running timer
    // task. tokio JoinHandle::Drop is a *detach*, not an abort — without
    // this guard a timer scheduled just before abort would keep running
    // and possibly fire a stray `shutdown.notify_one()` against a
    // subsequent daemon's listener.
    struct TimerGuard(Option<tokio::task::JoinHandle<()>>);
    impl TimerGuard {
        fn arm(&mut self, h: tokio::task::JoinHandle<()>) {
            // Abort any pre-existing timer before storing the new one.
            if let Some(prev) = self.0.take() {
                prev.abort();
            }
            self.0 = Some(h);
        }
        fn disarm(&mut self) {
            if let Some(h) = self.0.take() {
                h.abort();
            }
        }
    }
    impl Drop for TimerGuard {
        fn drop(&mut self) {
            self.disarm();
        }
    }
    let mut timer = TimerGuard(None);

    loop {
        let clients = client_count.load(Ordering::SeqCst);
        let agents = pty_registry.live_count();
        let is_idle = clients == 0 && agents == 0;

        if is_idle {
            if timer.0.is_none() {
                // 1→0 transition (or fresh-startup idle): arm the timer.
                let counter = client_count.clone();
                let registry = pty_registry.clone();
                let shutdown_signal = shutdown.clone();
                let dur = threshold;
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(dur).await;
                    // Re-check just before firing — covers the narrow race
                    // where an abort loses to the sleep completing. Without
                    // it, a connect arriving in that gap would still fire
                    // shutdown.
                    if counter.load(Ordering::SeqCst) == 0 && registry.live_count() == 0 {
                        info!(
                            threshold_secs = dur.as_secs(),
                            "Daemon idle window elapsed (no clients, no agents); signaling shutdown"
                        );
                        shutdown_signal.notify_one();
                    }
                });
                timer.arm(handle);
            }
        } else {
            // 0→1 transition (or fresh-startup non-idle): tear down any
            // pending timer so the next idle window starts from scratch.
            timer.disarm();
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
    is_external_mode: bool,
    pending_broadcasts: Arc<PendingBroadcasts>,
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
                let pending_broadcasts = pending_broadcasts.clone();
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
                                    // PRD #76 M2.19: route delegates by
                                    // mode using the deterministic
                                    // attach-socket flag (not the
                                    // broadcast's delivery count, which
                                    // briefly reports zero subscribers
                                    // during TUI startup, reconnect, and
                                    // lagged-stream teardown).
                                    if is_external_mode {
                                        // External-daemon mode: the TUI-side
                                        // AppState owns the role map and
                                        // validates the delegate. Daemon is
                                        // a dumb pipe. If `send` returns
                                        // Err right now there are zero
                                        // attached subscribers — typically
                                        // because the user has detached
                                        // the deck. Buffer the signal in
                                        // `pending_broadcasts` so the next
                                        // attaching TUI replays it; without
                                        // this, `dot-agent-deck delegate`
                                        // issued during a detach window
                                        // would be silently lost forever.
                                        if event_tx
                                            .send(BroadcastMsg::Delegate(signal.clone()))
                                            .is_err()
                                        {
                                            warn!(
                                                pane_id = %signal.pane_id,
                                                "delegate buffered: no attached TUI subscribers (will replay on reattach)"
                                            );
                                            pending_broadcasts
                                                .record(BroadcastMsg::Delegate(signal));
                                        }
                                    } else {
                                        // In-process daemon mode: TUI
                                        // shares `state` directly, so the
                                        // daemon-local call is the real
                                        // enqueue path. The broadcast has
                                        // no subscribers in this mode and
                                        // would be a no-op.
                                        state.write().await.handle_delegate(signal);
                                    }
                                }
                                DaemonMessage::WorkDone(signal) => {
                                    info!(
                                        pane_id = %signal.pane_id,
                                        done = signal.done,
                                        "Received work-done signal"
                                    );
                                    // Same external-vs-in-process split as
                                    // Delegate above: the daemon's local
                                    // `pane_role_map` / `pane_cwd_map` are
                                    // empty in external mode, so the TUI has
                                    // to be the one running `handle_work_done`
                                    // (resolves role, writes summary file,
                                    // enqueues feedback for the orchestrator
                                    // pane).
                                    if is_external_mode {
                                        // Same detach-loss risk as the
                                        // Delegate arm above (and the real
                                        // user-reported bug here): a worker
                                        // calling `dot-agent-deck work-done`
                                        // while the deck is detached must
                                        // not vanish — the orchestrator
                                        // depends on the feedback to know
                                        // the task is finished. Buffer on
                                        // send-Err and let the next attached
                                        // TUI replay it.
                                        if event_tx
                                            .send(BroadcastMsg::WorkDone(signal.clone()))
                                            .is_err()
                                        {
                                            warn!(
                                                pane_id = %signal.pane_id,
                                                "work-done buffered: no attached TUI subscribers (will replay on reattach)"
                                            );
                                            pending_broadcasts
                                                .record(BroadcastMsg::WorkDone(signal));
                                        }
                                    } else {
                                        state.write().await.handle_work_done(signal);
                                    }
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

    #[tokio::test]
    async fn delegate_routes_direct_in_in_process_mode() {
        // Regression for "delegate silently no-op in local-mode TUI":
        // `with_attach_in_process` must route delegates via the shared
        // state's `handle_delegate`, not via a broadcast that has no
        // subscriber in this mode.
        use crate::event::{DaemonMessage, DelegateSignal};
        use chrono::Utc;
        use tokio::io::AsyncWriteExt;

        let dir = race_safe_tempdir();
        let hook_path = dir.path().join("hook.sock");
        let attach_path = dir.path().join("attach.sock");

        let state = Arc::new(RwLock::new(AppState::default()));
        // Mimic the TUI's pane registration so handle_delegate accepts the signal.
        {
            let mut st = state.write().await;
            st.register_pane("orch".into());
            st.pane_role_map
                .insert("orch".into(), "orchestrator".into());
            st.orchestrator_pane_ids.insert("orch".into());
        }

        let daemon_state = state.clone();
        let attach_for_daemon = attach_path.clone();
        let hook_for_daemon = hook_path.clone();
        let handle = tokio::spawn(async move {
            let daemon = Daemon::with_attach_in_process(daemon_state, attach_for_daemon);
            let _ = run_daemon_with(&hook_for_daemon, daemon).await;
        });

        // Wait for the hook socket to bind.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if hook_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(hook_path.exists(), "hook socket did not appear");

        // Send a delegate signal. No subscriber on the broadcast — this
        // must still land in `state.delegate_events` via the direct path.
        let signal = DelegateSignal {
            pane_id: "orch".into(),
            task: "test".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        };
        let msg = DaemonMessage::Delegate(signal);
        let mut json = serde_json::to_vec(&msg).unwrap();
        json.push(b'\n');
        let mut stream = tokio::net::UnixStream::connect(&hook_path).await.unwrap();
        stream.write_all(&json).await.unwrap();
        stream.shutdown().await.unwrap();

        // Wait briefly for the hook loop to enqueue.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut saw = false;
        while tokio::time::Instant::now() < deadline {
            if !state.read().await.delegate_events.is_empty() {
                saw = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.abort();
        assert!(
            saw,
            "in-process daemon must enqueue delegate via direct handle_delegate; \
             a broadcast-only path would silently drop it (no subscriber in local mode)"
        );
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
    async fn run_daemon_with_uses_sibling_lock_file() {
        // Sanity: the lock path is deterministic and lives next to the
        // socket, not somewhere global. A future refactor that puts the
        // lock under a per-user dir would still need this invariant for
        // the per-socket race-protection to be meaningful.
        let dir = race_safe_tempdir();
        let sock_path = dir.path().join("hook.sock");
        let attach_path = dir.path().join("attach.sock");
        let lock_path = lock_path_for(&sock_path);
        assert_eq!(lock_path, dir.path().join("hook.sock.lock"));

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

    #[test]
    fn pending_broadcasts_logs_warn_on_eviction() {
        // PRD #93 round-2 reviewer/auditor #6: silent eviction at cap was
        // hiding the "long detach storm + runaway worker dropped a signal"
        // case. The record() helper now logs a warn! when the buffer is
        // full. We can't easily assert on the logger output without a
        // tracing subscriber, but we can at least verify the eviction
        // *behavior* — the buffer never exceeds the cap and the oldest
        // entry is the one that's dropped.
        use crate::event::DelegateSignal;
        use chrono::Utc;

        let buf = PendingBroadcasts::new();
        // Push CAP+1 distinguishable entries.
        for i in 0..=PENDING_BROADCAST_CAP {
            buf.record(BroadcastMsg::Delegate(DelegateSignal {
                pane_id: format!("p-{i}"),
                task: format!("t-{i}"),
                to: vec![],
                timestamp: Utc::now(),
            }));
        }

        let drained = buf.drain();
        assert_eq!(
            drained.len(),
            PENDING_BROADCAST_CAP,
            "buffer must cap at PENDING_BROADCAST_CAP entries; got {}",
            drained.len()
        );

        // The first push (p-0) must have been evicted. The remaining
        // entries should be p-1 .. p-CAP in FIFO order.
        match &drained[0] {
            BroadcastMsg::Delegate(s) => {
                assert_eq!(
                    s.pane_id, "p-1",
                    "oldest survivor must be p-1 — p-0 should have been evicted"
                );
            }
            other => panic!("expected Delegate, got {other:?}"),
        }
    }

    #[test]
    fn pending_broadcasts_push_front_batch_preserves_fifo() {
        // PRD #93 round-2 reviewer/auditor #5: re-enqueueing a failed
        // drain batch must preserve the FIFO order between entries so a
        // fresh subscriber drains them in the same order as the wedged
        // one would have.
        use crate::event::DelegateSignal;
        use chrono::Utc;

        let buf = PendingBroadcasts::new();
        // Simulate: drain pulled out [a, b, c]; write of `b` failed, so
        // we re-enqueue [b, c] (a was successfully written). The next
        // subscriber must drain [b, c] in that order.
        let mk = |id: &str| {
            BroadcastMsg::Delegate(DelegateSignal {
                pane_id: id.into(),
                task: "t".into(),
                to: vec![],
                timestamp: Utc::now(),
            })
        };
        buf.push_front_batch(vec![mk("b"), mk("c")]);
        let drained = buf.drain();
        assert_eq!(drained.len(), 2);
        match (&drained[0], &drained[1]) {
            (BroadcastMsg::Delegate(d0), BroadcastMsg::Delegate(d1)) => {
                assert_eq!(d0.pane_id, "b", "first replayed must be the failed one");
                assert_eq!(d1.pane_id, "c", "second replayed must follow in FIFO");
            }
            other => panic!("expected two Delegate entries, got {other:?}"),
        }

        // Re-enqueued batch must sit at the FRONT, even if newer entries
        // arrived via `record` during the drain window.
        buf.record(mk("d"));
        buf.record(mk("e"));
        buf.push_front_batch(vec![mk("b"), mk("c")]);
        let drained = buf.drain();
        let ids: Vec<&str> = drained
            .iter()
            .map(|m| match m {
                BroadcastMsg::Delegate(d) => d.pane_id.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            ids,
            vec!["b", "c", "d", "e"],
            "failed-batch must sit at the front, in FIFO order, ahead of entries recorded during the drain window"
        );
    }
}
