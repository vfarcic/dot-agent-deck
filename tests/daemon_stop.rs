//! PRD #103 Phase 3 integration tests for `dot-agent-deck daemon stop`
//! and `daemon restart`.
//!
//! Unlike most daemon tests in this crate, these spawn a *real* child
//! process (the test binary itself, invoked as
//! `<CARGO_BIN_EXE_dot-agent-deck> daemon serve`) rather than an
//! in-process tokio task. That's load-bearing: `daemon stop` calls
//! `peer_pid()` on its attach socket and SIGTERMs whatever PID that
//! returns. With an in-process daemon the PID would be the test
//! runner itself — SIGTERM-ing the test runner kills the test.
//! Subprocess-based tests get a real, isolated PID we can safely
//! signal.
//!
//! Each test gets an isolated tempdir and points the daemon's socket
//! / state / lock paths inside it via the existing
//! `DOT_AGENT_DECK_{ATTACH_SOCKET,SOCKET,STATE_DIR,LOCK_DIR}` env
//! overrides — no cross-test interference.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::net::UnixStream;

use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_stop::{StopError, StopOutcome, run_daemon_restart, run_daemon_stop};

struct SubprocessDaemon {
    _dir: TempDir,
    attach_path: PathBuf,
    child: Child,
}

impl Drop for SubprocessDaemon {
    fn drop(&mut self) {
        // Belt-and-suspenders cleanup so a failing test doesn't leak
        // the daemon subprocess. Calling kill() on an already-exited
        // child is harmless (returns InvalidInput on Linux); wait()
        // reaps in either case.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_subprocess_daemon() -> SubprocessDaemon {
    let dir = tempfile::tempdir().unwrap();
    let attach_path = dir.path().join("attach.sock");
    let hook_path = dir.path().join("hook.sock");
    let state_dir = dir.path().join("state");
    let lock_dir = dir.path().join("lock");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::create_dir_all(&lock_dir).unwrap();

    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let mut child = Command::new(bin)
        .args(["daemon", "serve"])
        .env("DOT_AGENT_DECK_SOCKET", &hook_path)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &attach_path)
        .env("DOT_AGENT_DECK_STATE_DIR", &state_dir)
        .env("DOT_AGENT_DECK_LOCK_DIR", &lock_dir)
        // Silence the daemon's tracing output so test logs stay
        // readable; we don't inspect stdout/stderr.
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dot-agent-deck daemon serve subprocess");

    // Poll the attach socket: bind, then trial-connect to confirm the
    // listener is accepting. 15s is generous headroom for a cold
    // cargo-build cache hit on slow CI.
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if attach_path.exists() && UnixStream::connect(&attach_path).await.is_ok() {
            return SubprocessDaemon {
                _dir: dir,
                attach_path,
                child,
            };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Reap the subprocess before panicking — otherwise clippy's
    // zombie_processes lint trips, and more importantly the child
    // would outlive this test as an orphan.
    let _ = child.kill();
    let _ = child.wait();
    panic!(
        "subprocess daemon failed to bind {} within 15s",
        attach_path.display()
    );
}

#[tokio::test]
async fn stop_with_no_daemon_running_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let attach_path = dir.path().join("nonexistent.sock");
    let outcome = run_daemon_stop(&attach_path, false).await.unwrap();
    assert_eq!(outcome, StopOutcome::NoDaemonRunning);
}

#[tokio::test]
async fn stop_stale_daemon_recovers_via_peer_pid_and_sigterm() {
    // PRD #103 line 211 regression guard: this is the entire raison
    // d'être of the command. The recovery flow must work via
    //   (a) peer_pid()  — non-zero PID
    //   (b) ListAgents  — existing variant, supported by every daemon
    //   (c) SIGTERM     — process dies, socket stops accepting
    // and NONE of those depend on protocol surface added by this PRD,
    // which is why the same command can rescue a stale v0.24.x daemon.
    let mut daemon = spawn_subprocess_daemon().await;
    let pid_before = daemon.child.id();
    let attach_path = daemon.attach_path.clone();

    let outcome = run_daemon_stop(&attach_path, false)
        .await
        .expect("daemon stop must succeed against a clean daemon");
    match outcome {
        StopOutcome::Stopped { pid } => {
            // (a) peer_pid returned a non-zero PID and (b) it matched
            // the subprocess we spawned. ListAgents succeeded (we got
            // past it to the SIGTERM stage). SIGTERM took the daemon
            // down (we observed Stopped).
            assert_eq!(
                pid, pid_before,
                "peer_pid must return the daemon's subprocess PID"
            );
            assert_ne!(pid, 0, "peer_pid must return a non-zero PID");
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
    // Daemon process must be gone: wait() reaps it without hanging.
    let exit_status =
        tokio::task::spawn_blocking(move || daemon.child.wait().expect("child must reap"))
            .await
            .unwrap();
    assert!(
        !exit_status.success() || exit_status.success(),
        "exit status irrelevant — what matters is wait() returned, got {exit_status:?}"
    );
    // The daemon doesn't unlink its socket on SIGTERM exit; the next
    // `daemon serve` would clean it up. We assert the listener is
    // dead by trying a fresh connect.
    let connect_result = UnixStream::connect(&attach_path).await;
    assert!(
        connect_result.is_err(),
        "after daemon stop, attach socket must reject new connects"
    );
}

#[tokio::test]
async fn stop_refuses_when_live_agents_present_without_force() {
    let daemon = spawn_subprocess_daemon().await;
    let client = DaemonClient::new(daemon.attach_path.clone());

    // Spawn a long-lived agent. `sleep 60` is portable across Linux
    // and macOS and outlives the test trivially.
    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sleep 60".into()),
            ..StartAgentOptions::default()
        })
        .await
        .expect("StartAgent must succeed");
    assert!(!agent_id.is_empty(), "agent_id must be populated");

    let err = run_daemon_stop(&daemon.attach_path, false)
        .await
        .expect_err("daemon stop without --force must refuse");
    match err {
        StopError::LiveAgents { ids } => {
            assert!(
                ids.iter().any(|id| id == &agent_id),
                "refusal must list the live agent id; got {ids:?}"
            );
        }
        other => panic!("expected StopError::LiveAgents, got {other:?}"),
    }

    // Subprocess must still be running — peer_pid + SIGTERM never
    // fired. A fresh connect must succeed.
    let still_alive = UnixStream::connect(&daemon.attach_path).await;
    assert!(
        still_alive.is_ok(),
        "daemon must still be running after refusal: {still_alive:?}"
    );
}

#[tokio::test]
async fn stop_with_force_terminates_daemon_even_with_live_agents() {
    let mut daemon = spawn_subprocess_daemon().await;
    let client = DaemonClient::new(daemon.attach_path.clone());

    let _agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sleep 60".into()),
            ..StartAgentOptions::default()
        })
        .await
        .expect("StartAgent must succeed");

    // --force = true. The data-loss guard is bypassed; SIGTERM (and
    // SIGKILL on timeout) takes the daemon down regardless of live
    // agents.
    let outcome = run_daemon_stop(&daemon.attach_path, true)
        .await
        .expect("daemon stop --force must succeed");
    match outcome {
        StopOutcome::Stopped { .. } | StopOutcome::ForceKilled { .. } => {}
        other => panic!("expected Stopped or ForceKilled, got {other:?}"),
    }
    let _ = daemon.child.wait();
    let connect_result = UnixStream::connect(&daemon.attach_path).await;
    assert!(
        connect_result.is_err(),
        "after --force stop, attach socket must reject new connects"
    );
}

#[tokio::test]
async fn restart_stops_existing_daemon_and_allows_relaunch() {
    let mut daemon = spawn_subprocess_daemon().await;
    let pid_before = daemon.child.id();
    let attach_path = daemon.attach_path.clone();

    let outcome = run_daemon_restart(&attach_path, false)
        .await
        .expect("daemon restart must succeed against a clean daemon");
    match outcome {
        StopOutcome::Stopped { pid } => assert_eq!(pid, pid_before),
        other => panic!("expected Stopped, got {other:?}"),
    }
    let _ = daemon.child.wait();

    // Lazy-respawn: kick off a second daemon at the same paths to
    // confirm the first one is well and truly gone (otherwise this
    // second spawn would fail under flock contention or AddrInUse).
    // We can't reuse spawn_subprocess_daemon's tempdir because the
    // env vars need to point at the SAME paths — re-spawn with a
    // hand-rolled Command.
    let hook_path = attach_path.with_file_name("hook.sock");
    let state_dir = attach_path.with_file_name("state");
    let lock_dir = attach_path.with_file_name("lock");
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let mut second = Command::new(bin)
        .args(["daemon", "serve"])
        .env("DOT_AGENT_DECK_SOCKET", &hook_path)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &attach_path)
        .env("DOT_AGENT_DECK_STATE_DIR", &state_dir)
        .env("DOT_AGENT_DECK_LOCK_DIR", &lock_dir)
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("second daemon spawn");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut up = false;
    while Instant::now() < deadline {
        if UnixStream::connect(&attach_path).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = second.kill();
    let _ = second.wait();
    assert!(
        up,
        "second daemon must come up at the same socket path after restart"
    );
}
