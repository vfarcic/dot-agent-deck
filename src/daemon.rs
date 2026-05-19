use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncBufReadExt;
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::error::DaemonError;
use crate::event::{AgentEvent, BroadcastMsg, DaemonMessage};
use crate::state::SharedState;

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
        }
    }

    /// Daemon configured to also serve the M1.2 streaming attach protocol
    /// on `attach_path`. Hook ingestion still uses the path passed to
    /// `run_daemon_with`. Used by `daemon serve` and tests — the daemon's
    /// `state` is not shared with any TUI, so delegate signals are routed
    /// via the broadcast for a subscribing TUI to handle.
    pub fn with_attach(state: SharedState, attach_path: PathBuf) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Self {
            state,
            pty_registry: Arc::new(AgentPtyRegistry::new()),
            attach_socket_path: Some(attach_path),
            in_process: false,
            event_tx,
        }
    }

    /// Same as [`with_attach`](Self::with_attach) but flags the daemon as
    /// sharing `state` with a TUI in the same process. Delegate signals
    /// take the direct `state.handle_delegate` path so the local TUI sees
    /// them without needing to subscribe to its own daemon's broadcast.
    /// Used by the local-mode TUI's in-process daemon (`run_tui_session`
    /// when `via_daemon` is false).
    pub fn with_attach_in_process(state: SharedState, attach_path: PathBuf) -> Self {
        let mut daemon = Self::with_attach(state, attach_path);
        daemon.in_process = true;
        daemon
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
    // Clean up stale socket file
    if socket_path.exists() {
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

    // Route delegates by daemon mode, not by attach-socket presence: the
    // in-process TUI daemon ALSO binds an attach socket (so a future
    // `connect` from another laptop could reach it), but its `state`
    // is shared with the TUI, so a direct call is correct and the
    // broadcast would have no subscriber. Standalone `daemon serve`
    // and tests construct via the default `with_attach` (in_process=false)
    // so the broadcast path runs and any attached TUI's subscriber
    // re-runs role validation against its own state.
    let is_external_mode = !daemon.in_process;

    // Optionally spawn the M1.2 streaming attach server. We hold its
    // JoinHandle and abort it on exit so it doesn't outlive the daemon.
    let attach_handle = daemon.attach_socket_path.map(|path| {
        let registry = pty_registry.clone();
        let attach_event_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) =
                crate::daemon_protocol::run_attach_server(&path, registry, attach_event_tx).await
            {
                error!("attach protocol server error: {e}");
            }
        })
    });

    let result = run_hook_loop(listener, state, event_tx, is_external_mode).await;

    if let Some(h) = attach_handle {
        h.abort();
    }
    drop(pty_registry);

    result
}

async fn run_hook_loop(
    listener: UnixListener,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    is_external_mode: bool,
) -> Result<(), DaemonError> {
    loop {
        match listener.accept().await {
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
        }
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
}
