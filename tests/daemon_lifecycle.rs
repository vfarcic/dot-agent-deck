//! PRD #76, M2.5 — three lifecycle paths for stream-backed panes:
//!
//! 1. **Stop** (`Ctrl+W` / `close_pane`): registry entry must be removed.
//!    Already covered end-to-end in `local_attach::close_pane_stops_agent_in_daemon`;
//!    re-asserted here as a focused regression test alongside the new
//!    detach path so all three lifecycle modes live in one place.
//! 2. **Detach** (`detach_pane` / quit-dialog "Detach"): registry entry
//!    must survive *and* the daemon must observe an explicit `KIND_DETACH`
//!    frame (counted via `AgentPtyRegistry::detach_count`).
//! 3. **Drop** (TUI exit / pane drop without close or detach): registry
//!    entry must survive; the daemon observes the implicit detach as
//!    socket EOF, which intentionally does *not* bump `detach_count`.
//!
//! The harness mirrors `local_attach`: an in-process attach server
//! bound to a tempdir socket, with a process-wide `BIND_LOCK` because
//! `bind_attach_listener` flips the umask while binding.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;
use dot_agent_deck::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
use dot_agent_deck::state::{AppState, SharedState};
use dot_agent_deck::tab::TabManager;
use dot_agent_deck::ui::run_post_loop_teardown;

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

/// Test 1: `close_pane` (the Ctrl+W path) must issue `stop-agent` so the
/// daemon SIGKILLs the child and removes it from the registry. This is the
/// "kill the agent" semantic — the *opposite* of detach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_pane_removes_agent_from_registry() {
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

    assert_eq!(server.registry.len(), 1, "agent should be registered");

    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close).unwrap())
        .await
        .unwrap();

    let stopped = wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
        server.registry.is_empty()
    })
    .await;
    assert!(
        stopped,
        "close_pane (Ctrl+W) on a stream-backed pane must clear the registry — agent leaked"
    );

    // Stop is *not* a voluntary detach, so the detach counter stays at 0.
    assert_eq!(
        server.registry.detach_count(),
        0,
        "stop-agent must not bump detach_count — that counter tracks voluntary detach only"
    );
}

/// Test 2: `detach_pane` must leave the agent alive in the registry and
/// the daemon must observe an explicit `KIND_DETACH` frame (i.e.
/// `detach_count > 0`). This is the central new lifecycle behavior the
/// M2.5 milestone introduces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_pane_leaves_agent_running_and_emits_detach_frame() {
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

    // Capture the OS-level pid before detach so we can assert the child
    // is still alive after — registry membership alone could be a stale
    // entry, the same pattern m1_3 catches with `kill(pid, 0)`.
    let agent_ids = server.registry.agent_ids();
    assert_eq!(agent_ids.len(), 1);
    let pid = server
        .registry
        .child_pid(&agent_ids[0])
        .expect("daemon-side child should expose a pid");

    let baseline_detach = server.registry.detach_count();

    let ctrl_for_detach = ctrl.clone();
    let pane_id_for_detach = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_detach.detach_pane(&pane_id_for_detach).unwrap())
        .await
        .unwrap();

    // The daemon-side handler increments `detach_count` synchronously when
    // it reads the DETACH frame, but the read happens on a server task
    // that may not have run yet by the time `detach_pane` returns. Poll.
    let saw_frame = wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
        server.registry.detach_count() > baseline_detach
    })
    .await;
    assert!(
        saw_frame,
        "daemon never observed an explicit KIND_DETACH frame after detach_pane (count stayed at {baseline_detach})"
    );

    // Survival: registry entry must still be present.
    assert!(
        !server.registry.is_empty(),
        "detach_pane must leave the agent in the daemon registry"
    );

    // Belt-and-suspenders: assert OS-level liveness, the same way
    // `local_attach::dropping_controller_detaches_but_agent_survives`
    // catches the "registry entry survives but child was killed" inversion.
    let kill_rc = unsafe { libc::kill(pid as i32, 0) };
    assert_eq!(
        kill_rc,
        0,
        "pid {pid} must still be alive after detach_pane (errno={:?})",
        std::io::Error::last_os_error().raw_os_error()
    );

    // Cleanup: don't leak `sleep 30` past test exit.
    for id in server.registry.agent_ids() {
        let _ = server.registry.close_agent(&id);
    }
}

/// Test 3: dropping the pane (or the whole controller) without calling
/// `close_pane` or `detach_pane` is the "implicit detach" path —
/// `StreamBackend::drop` aborts the I/O task, the socket closes, and the
/// daemon sees EOF. The agent must survive and the explicit-detach
/// counter must *not* increment (an EOF is not a `KIND_DETACH` frame).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_pane_leaves_agent_running_without_detach_frame() {
    let server = start_server().await;

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

    let agent_ids = server.registry.agent_ids();
    assert_eq!(agent_ids.len(), 1);
    let pid = server.registry.child_pid(&agent_ids[0]).unwrap();
    let baseline_detach = server.registry.detach_count();

    drop(ctrl);

    // Survival: agent must still be in the registry.
    let alive = wait_for(
        Duration::from_millis(500),
        Duration::from_millis(20),
        || !server.registry.is_empty(),
    )
    .await;
    assert!(
        alive,
        "implicit-detach drop path must leave the daemon-side agent alive"
    );

    // OS-level liveness — same belt-and-suspenders rationale as test 2.
    let kill_rc = unsafe { libc::kill(pid as i32, 0) };
    assert_eq!(
        kill_rc, 0,
        "pid {pid} must still be alive after pane drop (implicit detach)"
    );

    // The drop path must NOT have sent a KIND_DETACH frame: that's the
    // whole point of distinguishing implicit-vs-explicit detach. Give the
    // daemon a generous window to process any frame that *might* have
    // been sent before asserting equality.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        server.registry.detach_count(),
        baseline_detach,
        "drop path must not emit an explicit KIND_DETACH frame (counter must stay at baseline)"
    );

    for id in server.registry.agent_ids() {
        let _ = server.registry.close_agent(&id);
    }
}

/// `detach_all_streams` is the surface the QuitConfirm "Detach" option
/// calls; verify it fans out to every stream-backed pane in one shot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_all_streams_emits_one_detach_per_pane() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Two stream-backed panes against the same daemon.
    for _ in 0..2 {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'sleep 30'"), None).unwrap()
        })
        .await
        .unwrap();
    }
    assert_eq!(server.registry.len(), 2);
    let baseline_detach = server.registry.detach_count();

    let ctrl_for_detach = ctrl.clone();
    let errs = tokio::task::spawn_blocking(move || ctrl_for_detach.detach_all_streams())
        .await
        .unwrap();
    assert!(
        errs.is_empty(),
        "detach_all_streams reported per-pane errors: {errs:?}"
    );

    // Both panes should have produced a KIND_DETACH frame.
    let saw_two = wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
        server.registry.detach_count() >= baseline_detach + 2
    })
    .await;
    assert!(
        saw_two,
        "expected detach_count to grow by 2 (one per pane), got {}",
        server.registry.detach_count() - baseline_detach
    );

    // Both agents should still be alive — detach is "leave running".
    assert_eq!(server.registry.len(), 2);

    for id in server.registry.agent_ids() {
        let _ = server.registry.close_agent(&id);
    }
}

fn one_role_orchestration_config() -> OrchestrationConfig {
    OrchestrationConfig {
        name: "test-orch".to_string(),
        roles: vec![OrchestrationRoleConfig {
            name: "tester".to_string(),
            command: "sh -c 'sleep 30'".to_string(),
            start: true,
            description: None,
            prompt_template: None,
            clear: true,
        }],
    }
}

/// Regression: PRD #76. The post-loop teardown that runs when the TUI
/// event loop exits used to unconditionally close every non-dashboard
/// tab — which for stream-backed orchestration panes issued `stop-agent`
/// against the daemon and SIGKILL'd the agent children. Per spec, both
/// Quit and Detach must leave remote agents running. The fix gates the
/// teardown on `EmbeddedPaneController::is_external_daemon()`.
///
/// This test exercises the gate directly: spawn an orchestration agent
/// via a `with_remote_deck` controller, call the teardown helper, and
/// assert the daemon's registry still owns the agent (and that no
/// explicit `KIND_DETACH` frame was emitted — the disconnect on TUI exit
/// is implicit, via socket EOF when pane Drop runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_loop_teardown_in_external_daemon_mode_preserves_orchestration_agents() {
    let server = start_server().await;
    let ctrl: Arc<dyn PaneController> = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));
    let state: SharedState = Arc::new(RwLock::new(AppState::default()));

    let mut tab_manager = TabManager::new(Arc::clone(&ctrl));

    let cfg = one_role_orchestration_config();
    tab_manager = tokio::task::spawn_blocking(move || {
        tab_manager
            .open_orchestration_tab(&cfg, "/tmp", None, (24, 80))
            .expect("open_orchestration_tab must succeed against the test daemon");
        tab_manager
    })
    .await
    .unwrap();

    // Sanity: the daemon registered the orchestration role agent.
    assert_eq!(
        server.registry.len(),
        1,
        "orchestration role agent should be registered on the daemon before teardown"
    );
    let agent_ids_before = server.registry.agent_ids();
    let pid = server
        .registry
        .child_pid(&agent_ids_before[0])
        .expect("daemon-side child must expose a pid");
    let baseline_detach = server.registry.detach_count();

    // Pre-condition: gate must report external-daemon for this controller.
    // (Belt-and-suspenders against a future refactor inverting the flag.)
    let embedded = ctrl
        .as_any()
        .downcast_ref::<EmbeddedPaneController>()
        .expect("controller is EmbeddedPaneController");
    assert!(
        embedded.is_external_daemon(),
        "with_remote_deck controller must report is_external_daemon=true"
    );

    let pane_for_teardown = Arc::clone(&ctrl);
    let state_for_teardown = Arc::clone(&state);
    tab_manager = tokio::task::spawn_blocking(move || {
        run_post_loop_teardown(&pane_for_teardown, &state_for_teardown, &mut tab_manager);
        tab_manager
    })
    .await
    .unwrap();

    // Drop the controller too — in the real Quit path, `pane` would also
    // go out of scope shortly after teardown. This is what closes the
    // attach sockets and lets the daemon observe implicit-detach EOF.
    drop(ctrl);

    // Survival: query the daemon over the wire (the externally observable
    // surface the user-visible bug manifested through) — not the in-process
    // registry. The original symptom was that `list_agents` no longer
    // included the orchestration agent after Quit; a registry-direct check
    // would skip the IPC boundary and the wire serialization, so a future
    // refactor that breaks the client→daemon listing path while leaving
    // the registry intact would let this test pass while the user-visible
    // bug regressed. Give the daemon a small window to process any
    // in-flight socket activity from the implicit-detach drop first.
    let client = DaemonClient::new(server.path.clone());
    let orchestration_agent_id = agent_ids_before[0].clone();
    let still_alive = wait_for(
        Duration::from_millis(500),
        Duration::from_millis(20),
        || {
            // `wait_for` takes a sync predicate; run the async list_agents
            // call on the current runtime via block_in_place so this stays
            // a one-line predicate without restructuring the helper.
            let listed = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(client.list_agents())
            });
            matches!(listed, Ok(records)
                if records.iter().any(|r| r.id == orchestration_agent_id))
        },
    )
    .await;
    assert!(
        still_alive,
        "orchestration agent {orchestration_agent_id} missing from client.list_agents() after post-loop teardown — Quit/Detach must leave remote agents observable via the daemon"
    );

    // OS-level liveness — the registry could in principle hold a stale
    // entry even though the child was SIGKILL'd; `kill(pid, 0)` catches
    // that inversion. Same belt-and-suspenders pattern m1_3 uses.
    let kill_rc = unsafe { libc::kill(pid as i32, 0) };
    assert_eq!(
        kill_rc,
        0,
        "pid {pid} must still be alive after post-loop teardown (errno={:?})",
        std::io::Error::last_os_error().raw_os_error()
    );

    // The teardown must NOT have emitted an explicit `KIND_DETACH` frame.
    // Quit's implicit-detach contract is "daemon observes socket EOF",
    // distinct from the Detach path's explicit `detach_all_streams`.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        server.registry.detach_count(),
        baseline_detach,
        "post-loop teardown must not emit KIND_DETACH frames (implicit-detach only)"
    );

    // Cleanup: don't leak the orchestration role child past test exit.
    for id in server.registry.agent_ids() {
        let _ = server.registry.close_agent(&id);
    }

    // `tab_manager` is intentionally moved across spawn_blocking boundaries
    // above to keep the orchestration tab alive past the teardown call.
    // Drop it explicitly so clippy doesn't flag the binding as unused.
    drop(tab_manager);
}

/// Companion test: in local-deck mode the teardown loop must still run.
/// Local panes are in-process PTY children that would leak past TUI exit
/// otherwise, so the gate must not over-apply to non-external-daemon
/// controllers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_loop_teardown_in_local_deck_mode_closes_orchestration_tab() {
    let ctrl: Arc<dyn PaneController> = Arc::new(EmbeddedPaneController::new());
    let state: SharedState = Arc::new(RwLock::new(AppState::default()));
    let mut tab_manager = TabManager::new(Arc::clone(&ctrl));

    let cfg = one_role_orchestration_config();
    tab_manager = tokio::task::spawn_blocking(move || {
        tab_manager
            .open_orchestration_tab(&cfg, "/tmp", None, (24, 80))
            .expect("open_orchestration_tab must succeed in local-deck mode");
        tab_manager
    })
    .await
    .unwrap();

    // The orchestration tab is now sitting alongside the dashboard.
    assert_eq!(
        tab_manager.tab_count(),
        2,
        "expected dashboard + orchestration tab before teardown"
    );

    let pane_for_teardown = Arc::clone(&ctrl);
    let state_for_teardown = Arc::clone(&state);
    let tab_manager = tokio::task::spawn_blocking(move || {
        run_post_loop_teardown(&pane_for_teardown, &state_for_teardown, &mut tab_manager);
        tab_manager
    })
    .await
    .unwrap();

    assert_eq!(
        tab_manager.tab_count(),
        1,
        "local-deck teardown must close the orchestration tab back to Dashboard-only"
    );

    // Controller-side: orchestration role pane must be gone (the loop's
    // close_pane path is what cleans up the local PTY child).
    let panes = ctrl.list_panes().expect("list_panes after teardown");
    assert!(
        panes.is_empty(),
        "local-deck teardown must close the role pane (PTY child should not leak past TUI exit) — got {panes:?}"
    );
}
