use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use tokio::io::AsyncBufReadExt;
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::error::DaemonError;
use crate::event::{AgentEvent, DaemonMessage};
use crate::state::SharedState;

/// Mode for the daemon's Unix socket: owner-only read/write. Without this the
/// socket file inherits the process umask, which on most systems leaves it
/// world-connectable. See PRD #76 line 298.
const SOCKET_MODE: u32 = 0o600;

/// Bundle of daemon state. Owns the hook-event `SharedState` and the agent
/// PTY registry. M1.1 keeps `run_daemon` as the public entrypoint with the
/// existing signature; the registry is constructed inside it. Future
/// milestones (M1.2 streaming attach protocol) will wire the registry into
/// the socket loop.
pub struct Daemon {
    pub state: SharedState,
    pub pty_registry: Arc<AgentPtyRegistry>,
}

impl Daemon {
    pub fn new(state: SharedState) -> Self {
        Self {
            state,
            pty_registry: Arc::new(AgentPtyRegistry::new()),
        }
    }
}

pub async fn run_daemon(socket_path: &Path, state: SharedState) -> Result<(), DaemonError> {
    run_daemon_with(socket_path, Daemon::new(state)).await
}

/// Same as `run_daemon` but lets callers (and tests) inject a pre-built
/// `Daemon` so they can hold a clone of the PTY registry alongside it. The
/// registry is held for the lifetime of the daemon coroutine; on drop it
/// kills any agents it still owns.
pub async fn run_daemon_with(socket_path: &Path, daemon: Daemon) -> Result<(), DaemonError> {
    // Clean up stale socket file
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    // Restrict the socket to owner-only access. Without this, the socket file
    // mode follows the process umask and is typically world-connectable —
    // which means any local user could deliver hook events into another
    // user's daemon.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    info!("Daemon listening on {}", socket_path.display());

    // Hold the registry for the lifetime of the loop so its Drop fires
    // (killing any owned agents) when this future is dropped/aborted.
    let _pty_registry = daemon.pty_registry;
    let state = daemon.state;

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
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
                                    state.write().await.handle_delegate(signal);
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

    #[tokio::test]
    async fn socket_is_chmod_0600_after_bind() {
        let dir = tempfile::tempdir().unwrap();
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
}
