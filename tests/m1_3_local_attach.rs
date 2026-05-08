//! PRD #76, M1.3 — proof that the TUI's stream-backed pane drives a local
//! daemon end-to-end and that the daemon survives the TUI viewer exiting.
//!
//! Each test spins up an in-process attach server bound to a tempdir socket,
//! builds an `EmbeddedPaneController` in `RemoteDeckLocal` mode against that
//! server, and exercises the byte path daemon → STREAM_OUT → vt100 parser as
//! well as the keystroke path vt100 → STREAM_IN → daemon-side PTY.
//!
//! The "daemon survival" test (the central M1.3 property — PRD line 199)
//! drops the controller while the agent is still running and asserts the
//! daemon-side registry still owns the agent.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;

// Mirrors `tests/daemon_protocol.rs`: `bind_attach_listener` flips the
// process-global umask while binding. Holding this lock across tempdir+bind
// keeps the umask narrowing invisible to other concurrent tests.
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
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task).await;
    });

    Server {
        _dir: dir,
        path,
        registry,
        handle,
    }
}

/// Wait until `pred` returns true, polling `interval` each tick. Returns
/// `false` if `timeout` elapses first.
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

fn screen_contains(ctrl: &EmbeddedPaneController, pane_id: &str, needle: &str) -> bool {
    let Some(screen) = ctrl.get_screen(pane_id) else {
        return false;
    };
    let parser = screen.lock().unwrap();
    parser.screen().contents().contains(needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_pane_receives_daemon_output() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // create_pane is blocking → it `block_on`s the daemon client. Run it on
    // a blocking thread so the runtime keeps polling the in-process server.
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'echo M1_3_HELLO; sleep 5'"), None)
                .expect("create_pane should succeed in remote-deck-local mode")
        })
        .await
        .unwrap()
    };

    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(50),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "M1_3_HELLO"),
    )
    .await;
    assert!(
        saw,
        "expected agent stdout 'M1_3_HELLO' to reach the vt100 screen via STREAM_OUT"
    );

    // Drop the controller cleanly; agents must remain alive. Done via Ctrl+W
    // semantics in a separate test.
    drop(ctrl);

    // Verify daemon-side agent is still present after the controller drop.
    // The drop closes the attach socket but does NOT issue stop-agent.
    assert_eq!(server.registry.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_pane_keystrokes_reach_agent() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // `cat` echoes its stdin — perfect for proving keystrokes round-trip
    // daemon ← STREAM_IN ← controller and back out as STREAM_OUT.
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.create_pane(Some("cat"), None).unwrap())
            .await
            .unwrap()
    };

    // Give the daemon a tick to wire up the attach.
    tokio::time::sleep(Duration::from_millis(50)).await;

    ctrl.write_raw_bytes(&pane_id, b"M1_3_INPUT\n")
        .expect("write_raw_bytes should succeed for stream-backed pane");

    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(50),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "M1_3_INPUT"),
    )
    .await;
    assert!(
        saw,
        "expected echoed keystroke 'M1_3_INPUT' to reach the vt100 screen via STREAM_OUT"
    );

    // Drop the controller — agent survives.
    drop(ctrl);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_controller_detaches_but_agent_survives() {
    let server = start_server().await;

    // Start a long-running agent on a blocking thread (`create_pane`
    // `block_on`s the daemon client). Hand the controller back out of the
    // closure rather than dropping it inside, so we can capture the
    // agent's PID *before* the deliberate drop below.
    let ctrl = tokio::task::spawn_blocking({
        let server_path = server.path.clone();
        let runtime = tokio::runtime::Handle::current();
        move || -> EmbeddedPaneController {
            let ctrl = EmbeddedPaneController::with_remote_deck(server_path, runtime);
            ctrl.create_pane(Some("sh -c 'sleep 30'"), None).unwrap();
            ctrl
        }
    })
    .await
    .unwrap();

    // Capture the agent's OS-level PID before dropping the controller. A
    // regression that killed the daemon-side child while leaving the
    // registry entry intact would slip past a registry-only assertion —
    // the same flaw pattern that the M1.2 `slow_client` test was rewritten
    // to catch.
    let agent_ids = server.registry.agent_ids();
    assert_eq!(agent_ids.len(), 1, "exactly one daemon-side agent expected");
    let pid = server
        .registry
        .child_pid(&agent_ids[0])
        .expect("daemon-side child should expose a pid");

    // Simulate TUI exit. Drop closes the attach socket but does NOT issue
    // stop-agent; the daemon must treat the close as implicit detach (PRD
    // #76 line 199 — agents survive the viewer).
    drop(ctrl);

    // Survival property: the daemon-side registry still owns the agent.
    let registry_alive = wait_for(
        Duration::from_millis(500),
        Duration::from_millis(20),
        || !server.registry.is_empty(),
    )
    .await;
    assert!(
        registry_alive,
        "daemon-side agent must outlive the TUI controller (M1.3 survival property)"
    );

    // Registry membership is necessary but not sufficient — assert the
    // child is still alive at the OS level. `kill(pid, 0)` is the standard
    // existence check: returns 0 if the process is alive, -1/ESRCH if the
    // kernel has reaped it.
    let kill_rc = unsafe { libc::kill(pid as i32, 0) };
    assert_eq!(
        kill_rc,
        0,
        "pid {pid} must still be alive after controller drop (errno={:?})",
        std::io::Error::last_os_error().raw_os_error()
    );

    // Cleanup so the test doesn't leak the `sleep 30` child past test exit.
    for id in server.registry.agent_ids() {
        let _ = server.registry.close_agent(&id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_pane_stops_agent_in_daemon() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'sleep 30'"), None).unwrap()
        })
        .await
        .unwrap()
    };

    assert_eq!(server.registry.len(), 1);

    // Capture the agent's OS-level PID before issuing close so we can
    // assert it actually dies, not just that the registry entry vanishes.
    let agent_ids = server.registry.agent_ids();
    let pid = server
        .registry
        .child_pid(&agent_ids[0])
        .expect("daemon-side child should expose a pid");

    // close_pane on a stream-backed pane sends `stop-agent` over the protocol.
    // This is the Ctrl+W path and the only way an agent should be killed by
    // the controller (as opposed to a plain detach on TUI exit).
    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close).unwrap())
        .await
        .unwrap();

    // Wait briefly for the registry to drop the entry; stop-agent is async.
    let stopped = wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
        server.registry.is_empty()
    })
    .await;
    assert!(
        stopped,
        "Ctrl+W on a stream-backed pane must issue stop-agent — registry should empty"
    );

    // Symmetry with `dropping_controller_detaches_but_agent_survives`:
    // assert OS-level death, not just registry absence. `force_kill_child_and_wait`
    // SIGKILLs and `wait()`s before the registry entry is dropped, so the
    // kernel has already reaped the child by the time we see is_empty()
    // — kill(pid, 0) should immediately report ESRCH.
    let kill_rc = unsafe { libc::kill(pid as i32, 0) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(
        kill_rc, -1,
        "pid {pid} should be dead after stop-agent (got rc={kill_rc})"
    );
    assert_eq!(
        errno,
        Some(libc::ESRCH),
        "pid {pid} should report ESRCH after stop-agent (got errno={errno:?})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_pane_surfaces_stop_agent_error() {
    // Inject a `stop-agent` failure by tearing down the daemon between
    // attach and Ctrl+W. The close path must surface the error rather
    // than silently degrade to detach (which would leak the agent on the
    // remote with no signal to the user).
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'sleep 30'"), None).unwrap()
        })
        .await
        .unwrap()
    };

    // Tear down the daemon: aborts the listener task and deletes the
    // socket file via the TempDir's Drop. A subsequent `connect` from
    // close_pane's stop-agent call will fail with ENOENT/ECONNREFUSED.
    drop(server);

    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    let result = tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close))
        .await
        .unwrap();

    assert!(
        result.is_err(),
        "close_pane on a stream-backed pane must return Err when stop-agent fails — got Ok (silent degrade to detach)"
    );

    // The pane is retained on failure so the user can retry Ctrl+W.
    assert!(
        ctrl.pane_ids().contains(&pane_id),
        "failed close should leave the pane in the registry for retry"
    );
}
