//! PRD #92 F5 — descendant-process kill semantics.
//!
//! Before F5, the daemon's kill paths sent `kill(pid, SIGKILL)` to the
//! direct child only. Commands containing a space were launched via
//! `$SHELL -c <cmd>`, so the registered PID was the shell's — `kill`
//! tore down the shell, but every process the shell had spawned (the
//! actual agent, language servers, file watchers, etc.) was orphaned to
//! init and survived. The daemon and TUI both thought the kill
//! succeeded; the user found stale processes hanging around after
//! Ctrl+W.
//!
//! F5 switches the kill paths to `killpg(pgid, SIGKILL)` (and `killpg
//! SIGTERM` for the graceful escalation phase) so the entire process
//! group dies together. `portable-pty` already makes every PTY child a
//! session leader via `setsid()` in its `pre_exec`, so the child's PID
//! equals its session ID and process-group ID — no additional spawn-path
//! setup needed.
//!
//! This test launches a shell-wrapped agent whose shell spawns a
//! long-lived descendant (`sh -c 'sleep 30 & echo $! > pid_file ; wait
//! "$pid"'`), reads the descendant PID once the shell has written it,
//! closes the pane via the controller (which goes through `close_agent`
//! → `force_kill_child_and_wait`), and asserts that both the shell PID
//! and the descendant PID are dead (`kill(pid, 0)` returns ESRCH) within
//! a bounded wait.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct Server {
    _dir: TempDir,
    path: PathBuf,
    registry: Arc<AgentPtyRegistry>,
    handle: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_server() -> Server {
    let registry = Arc::new(AgentPtyRegistry::new());

    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };

    let registry_for_task = registry.clone();
    let (event_tx, _) = tokio::sync::broadcast::channel(16);
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task, event_tx).await;
    });

    Server {
        _dir: dir,
        path,
        registry,
        handle,
    }
}

/// `kill(pid, 0)` returns 0 if the process exists (regardless of whether
/// the caller could actually signal it), and `-1` with ESRCH if it
/// doesn't. We use it as a non-destructive liveness probe for the
/// shell + descendant during the post-close polling loop.
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: `kill(pid, 0)` does not signal — it only probes. ESRCH
    // is the expected outcome once the kernel has reaped the process.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        true
    } else {
        // EPERM means the process exists but we can't signal it — for
        // tests this never happens (same user owns everything) but
        // treat it as alive to be safe.
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

/// Poll a predicate until either it returns true or the timeout elapses.
async fn wait_for<F: FnMut() -> bool>(timeout: Duration, interval: Duration, mut pred: F) -> bool {
    let start = tokio::time::Instant::now();
    while tokio::time::Instant::now() - start < timeout {
        if pred() {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    pred()
}

/// PRD #92 F5: closing a shell-wrapped agent must reap both the shell
/// itself and every descendant the shell had spawned. Pre-F5 the daemon
/// sent `kill(shell_pid, SIGKILL)` only; the descendant survived as an
/// orphan re-parented to init. Post-F5 it sends `killpg(shell_pid,
/// SIGKILL)`, taking the whole group down together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_pane_reaps_shell_descendants() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Use a shared tempdir for the pid-relay file. The shell writes its
    // `sleep` child's pid to this file via `echo $! > FILE` so the test
    // can capture it without relying on `/proc/<pid>/task/children` or
    // `pgrep -P` (both Linux-only and brittle in CI).
    let pid_dir = tempfile::tempdir().unwrap();
    let pid_file = pid_dir.path().join("descendant.pid");

    // `sh -c 'sleep 30 & echo $! > FILE; wait "$pid"'`
    //
    // Why this shape: `sleep 30 &` backgrounds the child; `$!` is the
    // child's PID and the shell writes it to FILE; then the shell
    // `wait`s on it so the shell process keeps the PTY open and the
    // daemon sees the registered (shell) PID stay alive — both the
    // shell and the descendant must die together when F5's killpg
    // signals the group.
    let cmd = format!("sleep 30 & echo $! > {} ; wait $!", pid_file.display());
    let cmd_for_pane = cmd.clone();
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.create_pane(Some(&cmd_for_pane), None).unwrap())
            .await
            .unwrap()
    };

    // Capture the daemon-registered (shell) PID via the registry.
    let agent_ids = server.registry.agent_ids();
    assert_eq!(agent_ids.len(), 1, "exactly one agent should be registered");
    let shell_pid = server
        .registry
        .child_pid(&agent_ids[0])
        .expect("daemon-side child should expose a pid") as i32;

    // Wait for the shell to write the descendant pid into the relay file.
    // The shell does `echo $!` after backgrounding `sleep 30`, so the
    // file appears within a few PTY tick cycles. 3s is generous.
    let pid_file_ready = wait_for(Duration::from_secs(3), Duration::from_millis(20), || {
        pid_file.exists()
            && std::fs::metadata(&pid_file)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
    })
    .await;
    assert!(
        pid_file_ready,
        "shell never wrote the descendant pid to {} — was the shell still starting?",
        pid_file.display()
    );

    let descendant_pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .expect("descendant pid file should contain a numeric PID");

    // Sanity-check: both the shell and the descendant are alive before
    // the close. Otherwise the test would pass for the wrong reason
    // (a descendant that died on its own would look like a successful
    // F5 fix even on the pre-F5 code).
    assert!(
        pid_is_alive(shell_pid),
        "shell pid {shell_pid} should be alive before close"
    );
    assert!(
        pid_is_alive(descendant_pid),
        "descendant pid {descendant_pid} should be alive before close"
    );

    // Trigger the close path (Ctrl+W ➜ EmbeddedPaneController::close_pane
    // ➜ daemon StopAgent ➜ AgentPtyRegistry::close_agent ➜
    // force_kill_child_and_wait, which post-F5 uses `killpg`).
    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close).unwrap())
        .await
        .unwrap();

    // Both PIDs must die together. Generous timeout because the OS
    // delivery of SIGKILL is async, but well under the descendant's
    // `sleep 30` so we know the kill is doing the work (not the timer).
    let shell_dead = wait_for(Duration::from_secs(3), Duration::from_millis(20), || {
        !pid_is_alive(shell_pid)
    })
    .await;
    assert!(
        shell_dead,
        "shell pid {shell_pid} should be dead within 3s after close — kill (or killpg) failed"
    );

    let descendant_dead = wait_for(Duration::from_secs(3), Duration::from_millis(20), || {
        !pid_is_alive(descendant_pid)
    })
    .await;
    assert!(
        descendant_dead,
        "descendant pid {descendant_pid} should be dead within 3s after close — PRD #92 F5 regression: killpg is not signalling the whole process group"
    );
}
