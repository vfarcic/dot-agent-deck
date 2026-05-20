use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    // PRD #93 M1.3 race protection. The pre-existing code unconditionally
    // unlinked any file at `socket_path` before binding. Two `daemon serve`
    // processes racing each other would both see the other's socket as
    // "stale," remove it, and bind a fresh inode — silently rebinding the
    // path away from the still-running winner and leaving its clients
    // stranded.
    //
    // Probe-connect first: if connecting succeeds, another daemon owns
    // this path and we must lose the race. Otherwise the socket is stale
    // (its binder died without unlinking) and we can safely remove it.
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
        tokio::spawn(async move {
            if let Err(e) = crate::daemon_protocol::run_attach_server_with_counter(
                &path,
                registry,
                attach_event_tx,
                attach_counter,
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
    let idle_handle = match (idle_shutdown, is_external_mode) {
        (Some(window), true) => {
            let counter = client_count.clone();
            let registry = pty_registry.clone();
            let shutdown_signal = shutdown.clone();
            Some(tokio::spawn(async move {
                run_idle_monitor(counter, registry, window, shutdown_signal).await;
            }))
        }
        _ => None,
    };

    let result = run_hook_loop(listener, state, event_tx, is_external_mode, shutdown).await;

    if let Some(h) = attach_handle {
        h.abort();
    }
    if let Some(h) = idle_handle {
        h.abort();
    }
    drop(pty_registry);

    result
}

/// PRD #93 M1.2 idle monitor. Polls every `poll_interval` for the
/// joint-zero condition (no attached clients *and* no managed agents);
/// once it has held continuously for `threshold`, fires `shutdown` and
/// exits.
///
/// `poll_interval` is derived from `threshold` so tests with short
/// windows finish quickly while the production 30s window only wakes a
/// couple of times per second.
async fn run_idle_monitor(
    client_count: Arc<AtomicUsize>,
    pty_registry: Arc<AgentPtyRegistry>,
    threshold: Duration,
    shutdown: Arc<Notify>,
) {
    // Sub-second floor keeps test runs fast; cap at 1s so production wakes
    // are cheap. `threshold / 4` gives at least 3–4 samples per window even
    // for short test thresholds.
    let poll_interval =
        std::cmp::min(threshold / 4, Duration::from_secs(1)).max(Duration::from_millis(50));
    let mut idle_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(poll_interval).await;
        let clients = client_count.load(Ordering::SeqCst);
        let agents = pty_registry.len();
        if clients == 0 && agents == 0 {
            let started = idle_since.get_or_insert_with(Instant::now);
            if started.elapsed() >= threshold {
                info!(
                    threshold_secs = threshold.as_secs(),
                    "Daemon idle window elapsed (no clients, no agents); signaling shutdown"
                );
                shutdown.notify_one();
                return;
            }
        } else {
            // Either a client (re)connected or an agent is alive — reset the
            // clock so the next idle stretch is measured from scratch.
            idle_since = None;
        }
    }
}

async fn run_hook_loop(
    listener: UnixListener,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    is_external_mode: bool,
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
                                        // a brief reconnect window — and
                                        // the delegate is lost (no replay
                                        // buffer; a recent-delegates buffer
                                        // is the follow-up if this race
                                        // ever bites a user in practice).
                                        // Log so an operator can correlate.
                                        if event_tx
                                            .send(BroadcastMsg::Delegate(signal.clone()))
                                            .is_err()
                                        {
                                            warn!(
                                                pane_id = %signal.pane_id,
                                                "delegate dropped: no attached TUI subscribers (reconnect race?)"
                                            );
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
                                    state.write().await.handle_work_done(signal);
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

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                Duration::from_millis(150),
                monitor_shutdown,
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
        let client_count = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(AgentPtyRegistry::new());
        let shutdown = Arc::new(Notify::new());

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        let threshold = Duration::from_millis(300);
        let handle = tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                threshold,
                monitor_shutdown,
            )
            .await;
        });

        // Half-way: bump client count so the idle accumulator resets.
        tokio::time::sleep(Duration::from_millis(150)).await;
        client_count.fetch_add(1, Ordering::SeqCst);

        // Wait *past* the original threshold deadline. With the reset, no
        // notification should arrive yet.
        let early = tokio::time::timeout(Duration::from_millis(250), shutdown.notified()).await;
        assert!(
            early.is_err(),
            "idle monitor must not fire while a client is attached"
        );

        // Drop the client; monitor should fire again after `threshold` elapses.
        client_count.fetch_sub(1, Ordering::SeqCst);
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

        let monitor_shutdown = shutdown.clone();
        let monitor_clients = client_count.clone();
        let monitor_registry = registry.clone();
        let threshold = Duration::from_millis(150);
        let handle = tokio::spawn(async move {
            run_idle_monitor(
                monitor_clients,
                monitor_registry,
                threshold,
                monitor_shutdown,
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
}
