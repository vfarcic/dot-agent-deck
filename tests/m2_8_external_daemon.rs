//! M2.8 — TUI external-daemon mode integration test.
//!
//! Exercises the full PRD #76 M2.8 lazy-spawn-on-attach flow against the
//! cargo-built `dot-agent-deck` binary (rather than the in-process
//! `current_exe()` that the production helper resolves to — in tests
//! `current_exe()` is the cargo-test harness, not our binary).
//!
//! What this validates:
//! - `ensure_daemon_running` + `spawn_daemon_serve_detached_with_exe`
//!   together fork-exec a real daemon, the loop polls until its attach
//!   socket appears, and the trust check passes (mode 0o600 + uid match
//!   + is-socket).
//! - The post-bootstrap socket is reachable by the existing `DaemonClient`
//!   API: `list_agents` round-trips and the daemon survives detach (the
//!   "agents survive TUI exit" property from PRD #76 line 199 is rooted
//!   in the daemon being a session leader outside our process group, which
//!   `setsid(2)` provides).

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use dot_agent_deck::daemon_attach::{ensure_daemon_running, spawn_daemon_serve_detached_with_exe};
use dot_agent_deck::daemon_client::DaemonClient;

// Same umask-narrowing serialization as the other integration test
// binaries — `bind_socket` flips the process-global umask while binding,
// and a tempdir created inside that window inherits 0o600. Hold this
// across tempdir creation so the socket-mode check below is meaningful.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

fn dot_agent_deck_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dot-agent-deck"))
}

/// Best-effort SIGTERM-then-SIGKILL of the spawned daemon. We can't `wait`
/// the child because `spawn_daemon_serve_detached_with_exe` drops the
/// `Child` handle (its whole point is to detach), so `kill(2)` by pid is
/// the cleanest cleanup available. SIGTERM gives the daemon a chance to
/// unbind sockets; the short pause + SIGKILL is a safety net for tests
/// that deadlock the graceful path.
fn kill_daemon(pid: u32) {
    // SAFETY: pid is a u32 captured from a successful spawn; kill(2) with
    // a non-existent pid simply returns ESRCH, which we ignore. No memory
    // is touched.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(150));
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[tokio::test]
async fn lazy_spawn_binds_trusted_socket_and_serves_list_agents() {
    let dir = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        tempfile::tempdir().unwrap()
    };
    let state_dir = dir.path().to_path_buf();
    let attach_path = state_dir.join("attach.sock");
    let hook_path = state_dir.join("hook.sock");

    // The spawned daemon needs the env vars resolved at its own startup —
    // attach + hook socket paths and a state-dir for its log/lock files.
    // We pass them on the child's environment via the cmd.env() chain in
    // a wrapper closure: spawn_daemon_serve_detached_with_exe doesn't
    // accept env, so we set them on the test process before invoking it.
    // Tests in this binary share env across cases; we scope the mutation
    // by restoring afterwards.
    let prev_attach = std::env::var("DOT_AGENT_DECK_ATTACH_SOCKET").ok();
    let prev_hook = std::env::var("DOT_AGENT_DECK_SOCKET").ok();
    let prev_state = std::env::var("DOT_AGENT_DECK_STATE_DIR").ok();
    // SAFETY: tests in a single test binary share env; this whole test
    // binary serializes via the lock above for the bind-mode-sensitive
    // setup, and we restore the previous values before returning.
    unsafe {
        std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", &attach_path);
        std::env::set_var("DOT_AGENT_DECK_SOCKET", &hook_path);
        std::env::set_var("DOT_AGENT_DECK_STATE_DIR", &state_dir);
    }

    let exe = dot_agent_deck_bin();
    let state_for_spawn = state_dir.clone();
    let captured_pid = std::sync::Arc::new(std::sync::Mutex::new(None::<u32>));
    let pid_slot = captured_pid.clone();

    let result = ensure_daemon_running(
        &attach_path,
        &state_dir,
        move || {
            let pid = spawn_daemon_serve_detached_with_exe(&state_for_spawn, &exe)?;
            *pid_slot.lock().unwrap() = Some(pid);
            Ok(())
        },
        Duration::from_millis(50),
        Duration::from_secs(10),
    )
    .await;

    // Whatever happens next, restore env before any panic so subsequent
    // tests don't observe our state.
    let restore_env = || {
        // SAFETY: same scope as the set_var above; restoring previous values.
        unsafe {
            match &prev_attach {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", v),
                None => std::env::remove_var("DOT_AGENT_DECK_ATTACH_SOCKET"),
            }
            match &prev_hook {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SOCKET", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SOCKET"),
            }
            match &prev_state {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_STATE_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_STATE_DIR"),
            }
        }
    };

    let pid = captured_pid.lock().unwrap().take();
    if let Err(e) = result {
        restore_env();
        if let Some(pid) = pid {
            kill_daemon(pid);
        }
        panic!("ensure_daemon_running failed: {e}");
    }
    let pid = pid.expect("spawn closure must have recorded a pid on success");

    // Trust check: file is a Unix socket, mode 0o600, owned by us.
    let meta = std::fs::metadata(&attach_path).expect("attach socket should exist");
    use std::os::unix::fs::FileTypeExt;
    assert!(meta.file_type().is_socket(), "attach path must be a socket");
    assert_eq!(
        meta.mode() & 0o777,
        0o600,
        "attach socket must be mode 0o600 (got 0o{:o})",
        meta.mode() & 0o777
    );
    // SAFETY: getuid is async-signal-safe and infallible.
    let our_uid = unsafe { libc::getuid() };
    assert_eq!(meta.uid(), our_uid, "attach socket must be owned by us");

    // Daemon is reachable via DaemonClient: list_agents on a fresh daemon
    // returns an empty list and demonstrates the protocol round-trips.
    let client = DaemonClient::new(attach_path.clone());
    let agents = match client.list_agents().await {
        Ok(a) => a,
        Err(e) => {
            restore_env();
            kill_daemon(pid);
            panic!("DaemonClient::list_agents failed: {e}");
        }
    };
    assert!(
        agents.is_empty(),
        "fresh daemon should have no agents; got {agents:?}"
    );

    // Daemon survives the closure that spawned it (i.e., the spawn
    // returned and we are now operating against the long-lived process).
    // A second list_agents after a short sleep proves the daemon hasn't
    // exited just because the spawning closure finished.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let agents_after = match client.list_agents().await {
        Ok(a) => a,
        Err(e) => {
            restore_env();
            kill_daemon(pid);
            panic!("daemon must survive the spawn closure exiting: {e}");
        }
    };
    assert!(agents_after.is_empty());

    restore_env();
    kill_daemon(pid);
}
