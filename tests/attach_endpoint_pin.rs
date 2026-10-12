// Unix-only: the harness binds Unix sockets and the stand-in agent is `sh`.
#![cfg(unix)]
//! PRD #1589 D8: every agent a daemon spawns is handed THAT daemon's attach
//! endpoint as `DOT_AGENT_DECK_ATTACH_SOCKET`, so an agent's `close` (or any
//! other attach-socket call it makes) reaches the daemon that spawned it, never
//! the default endpoint — which, under a sandbox, a test harness or a second
//! deck, is some other daemon.
//!
//! Two real daemons run in this process, each on its own non-default hook and
//! attach sockets. Each starts an agent through its own attach socket; the
//! agent writes the attach endpoint it was handed to a file. Then one agent is
//! respawned in place, and the new generation must still be pointed at its own
//! daemon.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID};
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::state::{AppState, SharedState};

mod common;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct DaemonHandle {
    dir: TempDir,
    attach_path: PathBuf,
    pty_registry: Arc<AgentPtyRegistry>,
    handle: JoinHandle<()>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.handle.abort();
        self.pty_registry.shutdown_all();
    }
}

async fn spawn_daemon() -> DaemonHandle {
    common::init_test_env();
    let (dir, hook_path, attach_path) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = common::race_safe_tempdir();
        let hook = dir.path().join("hook.sock");
        let attach = dir.path().join("own-attach.sock");
        (dir, hook, attach)
    };
    let state: SharedState = Arc::new(RwLock::new(AppState::default()));
    let daemon = Daemon::with_attach(state, attach_path.clone())
        .with_idle_shutdown(None)
        .with_lock_dir_override(common::lock_dir_path());
    let pty_registry = daemon.pty_registry.clone();
    let hook_for_daemon = hook_path.clone();
    let handle = tokio::spawn(async move {
        let _ = run_daemon_with(&hook_for_daemon, daemon).await;
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(attach_path.exists() && UnixStream::connect(&attach_path).await.is_ok()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "attach socket was not accepting connections within 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    DaemonHandle {
        dir,
        attach_path,
        pty_registry,
        handle,
    }
}

/// A stand-in agent that records the attach endpoint it was handed, then
/// stays alive.
fn reporting_command(out: &Path) -> String {
    format!(
        "sh -c 'printf \"%s\" \"${{DOT_AGENT_DECK_ATTACH_SOCKET:-<unset>}}\" > {}; sleep 30'",
        out.display()
    )
}

async fn read_reported(out: &Path) -> String {
    for _ in 0..400 {
        if let Ok(v) = std::fs::read_to_string(out)
            && !v.is_empty()
        {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the agent never reported its attach endpoint");
}

/// Scenario: two daemons run side by side on their own non-default sockets.
/// An agent started through each daemon's attach socket is handed that
/// daemon's own attach path, and a respawned generation of one of them still
/// is — so `close` from either agent reaches its own daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_daemons_agents_are_pointed_at_its_own_attach_endpoint() {
    let first = spawn_daemon().await;
    let second = spawn_daemon().await;
    assert_ne!(first.attach_path, second.attach_path);

    let mut seen = Vec::new();
    for (daemon, pane) in [(&first, "pin-first"), (&second, "pin-second")] {
        let out = daemon.dir.path().join("reported.txt");
        let command = reporting_command(&out);
        DaemonClient::new(daemon.attach_path.clone())
            .start_agent(StartAgentOptions {
                command: Some(command.clone()),
                cwd: Some(daemon.dir.path().to_string_lossy().into_owned()),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                ..StartAgentOptions::default()
            })
            .await
            .expect("start an agent through this daemon's attach socket");
        seen.push((daemon, pane, out, command));
    }
    for (daemon, _, out, _) in &seen {
        assert_eq!(
            read_reported(out).await,
            daemon.attach_path.to_string_lossy(),
            "an agent must be handed its own daemon's attach endpoint"
        );
    }

    let (daemon, pane, out, command) = &seen[1];
    std::fs::remove_file(out).expect("clear the first generation's report");
    daemon
        .pty_registry
        .respawn_agent_for_pane(pane, command)
        .await
        .expect("respawn the agent in place");
    assert_eq!(
        read_reported(out).await,
        daemon.attach_path.to_string_lossy(),
        "a respawned generation must still reach its own daemon"
    );
}
