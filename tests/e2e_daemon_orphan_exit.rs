#![cfg(feature = "e2e")]

//! L2 lifecycle test for the daemon-side "exit-when-orphaned" watchdog
//! (PRD #77 catalog `lifecycle/orphan-exit/001`).
//!
//! The e2e harness spawns idle-disabled (`DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0`)
//! `daemon serve` processes and only cleans them up in `Drop`, which never runs
//! on SIGKILL / panic-abort / nextest-timeout / Ctrl-C — so daemons used to
//! orphan to PID 1 and live for hours. The fix is an env-gated daemon-side
//! watchdog: with `DOT_AGENT_DECK_EXIT_WHEN_ORPHANED=1` the daemon captures its
//! parent pid at startup and gracefully exits once it is orphaned. This test
//! proves it: it runs the daemon under a short-lived intermediate `sh` parent
//! (so the test can orphan the daemon without killing itself), kills that
//! parent, and asserts the daemon process terminates within a few seconds —
//! even though idle shutdown is disabled, so ONLY the watchdog can kill it.
//!
//! Decision 21: all bounded polling lives in `common` (`wait_until` /
//! `process_running`), never as a raw `sleep` in this test body.

mod common;

use std::time::Duration;

use spec::spec;

/// Scenario: Launch `dot-agent-deck daemon serve` (idle shutdown DISABLED) from
/// a short-lived intermediate `sh` parent that backgrounds it and records its
/// pid, with `DOT_AGENT_DECK_EXIT_WHEN_ORPHANED=1`. Wait for the daemon to bind
/// its attach socket (watchdog armed), then SIGKILL the intermediate parent so
/// the daemon is orphaned to init. Assert the daemon process terminates within
/// a few seconds — proving the watchdog gracefully shuts an orphaned, otherwise
/// never-exiting test daemon down instead of leaking it to PID 1.
#[spec("lifecycle/orphan-exit/001")]
#[test]
fn orphan_exit_001_orphaned_daemon_self_exits() {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let dir = common::race_safe_tempdir();
    let work = dir.path();
    let home = work.join("home");
    std::fs::create_dir_all(&home).expect("create HOME");
    let attach_socket = work.join("attach.sock");
    let hook_socket = work.join("hook.sock");
    let state_dir = work.join("state");
    let pidfile = work.join("daemon.pid");

    // Intermediate parent: a shell that backgrounds the daemon (so the daemon's
    // parent is THIS shell, not the test), records the daemon pid, then sleeps.
    // Killing the shell orphans the daemon without touching the test process.
    let script = format!(
        "'{bin}' daemon serve >/dev/null 2>&1 &\necho $! > '{pid}'\nsleep 60\n",
        bin = bin,
        pid = pidfile.display(),
    );

    let path_env = std::env::var("PATH").unwrap_or_default();
    let mut parent = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env_clear()
        .env("PATH", &path_env)
        .env("HOME", &home)
        .env("DOT_AGENT_DECK_SOCKET", &hook_socket)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &attach_socket)
        .env("DOT_AGENT_DECK_STATE_DIR", &state_dir)
        // Disable idle shutdown so ONLY the orphan watchdog can end the daemon.
        .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
        // The behavior under test.
        .env("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED", "1")
        .spawn()
        .expect("spawn intermediate sh parent");

    // Read the daemon pid once the shell has backgrounded it.
    let read_pid = || -> Option<i32> {
        std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
    };
    assert!(
        common::wait_until(Duration::from_secs(10), || read_pid().is_some()),
        "intermediate parent never wrote the daemon pid"
    );
    let daemon_pid = read_pid().expect("daemon pid recorded");

    // Wait for the daemon to bind its attach socket — by then the watchdog has
    // captured its original parent pid and is polling.
    assert!(
        common::wait_until(Duration::from_secs(10), || attach_socket.exists()),
        "daemon never bound its attach socket"
    );
    assert!(
        common::process_running(daemon_pid),
        "precondition: the daemon must be alive before we orphan it"
    );

    // Orphan the daemon: SIGKILL the intermediate parent (Drop never gets a
    // chance — exactly the leak scenario). The daemon reparents to init.
    let _ = parent.kill();
    let _ = parent.wait();

    // The watchdog (polling ~1/s) must notice the orphaning and gracefully
    // shut the daemon down within a few seconds — despite idle being disabled.
    assert!(
        common::wait_until(Duration::from_secs(10), || !common::process_running(
            daemon_pid
        )),
        "orphaned daemon (pid {daemon_pid}) did not self-exit — the \
         exit-when-orphaned watchdog failed to fire"
    );

    // Defensive: if somehow still alive, don't leak it out of the test.
    if common::process_running(daemon_pid) {
        // SAFETY: best-effort cleanup kill.
        unsafe {
            libc::kill(daemon_pid, libc::SIGKILL);
        }
    }
}
