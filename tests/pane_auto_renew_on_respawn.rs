//! PRD #92 F12 — TUI auto-renews its per-pane attach subscription when
//! the daemon respawns the agent behind that pane (F9 `clear=true`
//! delegate flow).
//!
//! Each test spins up an in-process attach server bound to a tempdir
//! socket, builds an `EmbeddedPaneController` against that server, and
//! exercises the post-Closed re-resolution + re-attach path against a
//! real `AgentPtyRegistry`. F9 fixes the daemon side; F12 makes the
//! result visible end-to-end. Pre-fix the controller's vt100 parser
//! never sees the NEW agent's bytes — only a manual detach + re-attach
//! revealed them.
//!
//! ## Scope (auditor #10)
//!
//! These tests deliberately bypass the F9 delegate path and drive
//! [`AgentPtyRegistry`] directly. They verify the TUI-side
//! controller behavior in isolation: STREAM_END on the OLD agent →
//! lookup by `pane_id_env` → re-attach to the NEW agent → vt100
//! parser sees the NEW agent's bytes → shared `agent_id` swapped so
//! the next `stop-agent` / `resize-agent` targets the NEW id.
//!
//! They do NOT exercise the full F9 delegate → respawn → reattach
//! end-to-end chain. The daemon-side coverage lives in
//! `tests/orchestration_delegate.rs` (for example
//! `delegate_respawns_worker_agent_when_role_clear_is_true` and
//! `respawn_failure_surfaces_visible_error_in_orchestrator_pane`).
//! The F9 tests + the F12 tests together are the end-to-end story;
//! neither file alone proves it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;

// `bind_attach_listener` flips the process-global umask while binding;
// share the lock across tempdir+bind so the narrowed umask never leaks
// into other concurrent tests.
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

fn screen_contains(ctrl: &EmbeddedPaneController, pane_id: &str, needle: &str) -> bool {
    let Some(screen) = ctrl.get_screen(pane_id) else {
        return false;
    };
    let parser = screen.lock().unwrap();
    parser.screen().contents().contains(needle)
}

/// Spawn an agent directly on the registry with `DOT_AGENT_DECK_PANE_ID`
/// bound to `pane_id`. Used to simulate F9's daemon-side respawn (the
/// daemon kills the OLD agent then spawns a fresh one under the same
/// pane_id_env) without going through the controller's `create_pane`,
/// which would allocate a fresh pane.
fn respawn_under_pane_id(registry: &AgentPtyRegistry, pane_id: &str, command: &str) -> String {
    registry
        .spawn_agent(SpawnOptions {
            command: Some(command),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            ..Default::default()
        })
        .expect("respawn_under_pane_id: spawn_agent should succeed")
}

/// F12 happy path. After the daemon respawns the agent under a pane (the
/// real-world trigger is F9's `clear=true` second delegate), the
/// controller's vt100 parser must transition from the OLD agent's bytes
/// to the NEW agent's bytes without any manual detach + re-attach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_reattaches_after_daemon_respawn() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // OLD agent prints a distinctive banner and then sleeps so it's still
    // attached when we kill it from the registry side.
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'echo F12_OLD_BANNER; sleep 30'"), None)
                .expect("create_pane should succeed")
        })
        .await
        .unwrap()
    };

    // The OLD agent's banner must reach the vt100 parser through the
    // controller's existing happy-path subscription before we trigger
    // the respawn — otherwise we don't actually exercise the "switch
    // mid-stream" transition.
    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw_old = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(20),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "F12_OLD_BANNER"),
    )
    .await;
    assert!(
        saw_old,
        "expected OLD agent banner before triggering respawn"
    );

    // Simulate the daemon-side respawn that F9 performs on the second
    // `clear=true` delegate: kill the OLD agent then spawn a NEW one
    // under the SAME pane_id_env. The controller's io_task must notice
    // the OLD attach's STREAM_END, look up the NEW agent via
    // `list_agents`, and re-attach without any external intervention.
    let old_ids = server.registry.agent_ids();
    assert_eq!(old_ids.len(), 1, "exactly one daemon-side agent expected");
    let old_id = old_ids[0].clone();
    server
        .registry
        .close_agent(&old_id)
        .expect("close OLD agent");
    let _new_id = respawn_under_pane_id(
        &server.registry,
        &pane_id,
        "sh -c 'echo F12_NEW_BANNER; sleep 30'",
    );

    // The NEW agent's banner reaches the vt100 parser → F12 satisfied.
    // The pre-fix behavior was: io_task exits silently on STREAM_END and
    // the pane view freezes on F12_OLD_BANNER + whatever shell prompt
    // bytes preceded it.
    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw_new = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(20),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "F12_NEW_BANNER"),
    )
    .await;
    assert!(
        saw_new,
        "expected NEW agent banner to reach pane view via auto-reattach (F12 broken: subscriber exited on STREAM_END)"
    );

    drop(ctrl);
}

/// F12 give-up path. After the OLD agent dies and no NEW agent is ever
/// spawned for the pane, the io_task must exit cleanly within the
/// retry window (~300 ms) rather than loop forever or pin a wedged
/// socket. Externally observable signal: the input channel goes dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gives_up_when_no_live_agent_remains_for_pane() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'echo F12_GIVEUP_OLD; sleep 30'"), None)
                .expect("create_pane")
        })
        .await
        .unwrap()
    };

    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw_old = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(20),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "F12_GIVEUP_OLD"),
    )
    .await;
    assert!(saw_old, "expected OLD agent banner before kill");

    // Sanity check: while the io_task is alive, `write_raw_bytes`
    // succeeds because the input channel's receiver half is still owned
    // by the task. We use this as the "io_task alive" probe in the
    // post-kill assertion below.
    assert!(
        ctrl.write_raw_bytes(&pane_id, b"").is_ok(),
        "io_task should be alive before kill"
    );

    let old_id = server.registry.agent_ids().pop().expect("one agent");
    server
        .registry
        .close_agent(&old_id)
        .expect("close OLD agent");

    // The io_task must observe STREAM_END, attempt list_agents
    // REATTACH_MAX_LOOKUP_ATTEMPTS times (~300 ms total) with no
    // matching record, and then exit. After it exits, `input_rx` drops
    // and `write_raw_bytes` returns `CommandFailed` because the
    // unbounded channel reports a closed receiver.
    let ctrl_probe = ctrl.clone();
    let pane_for_probe = pane_id.clone();
    let exited = wait_for(
        Duration::from_secs(2),
        Duration::from_millis(20),
        move || ctrl_probe.write_raw_bytes(&pane_for_probe, b"").is_err(),
    )
    .await;
    assert!(
        exited,
        "io_task must exit after the lookup retry budget elapses with no live agent"
    );

    drop(ctrl);
}

/// F12 respawn-in-flight race. The OLD agent's death and the NEW
/// agent's registration aren't simultaneous: between them is a brief
/// window where `list_agents` returns nothing for this pane. The
/// io_task's REATTACH_LOOKUP_BACKOFF (100 ms × 3 attempts) must absorb
/// that gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backoff_absorbs_respawn_in_flight_race() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'echo F12_RACE_OLD; sleep 30'"), None)
                .expect("create_pane")
        })
        .await
        .unwrap()
    };

    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw_old = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(20),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "F12_RACE_OLD"),
    )
    .await;
    assert!(saw_old, "expected OLD agent banner before respawn race");

    // Kill the OLD agent. The io_task will immediately fire its first
    // `list_agents` lookup and find no match for this pane. Sleep
    // enough to land between attempt 0 (immediate) and attempt 1 (after
    // REATTACH_LOOKUP_BACKOFF = 100 ms), then spawn the NEW agent so
    // attempt 1 or 2 picks it up.
    let old_id = server.registry.agent_ids().pop().expect("one agent");
    server
        .registry
        .close_agent(&old_id)
        .expect("close OLD agent");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _new_id = respawn_under_pane_id(
        &server.registry,
        &pane_id,
        "sh -c 'echo F12_RACE_NEW; sleep 30'",
    );

    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw_new = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(20),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "F12_RACE_NEW"),
    )
    .await;
    assert!(
        saw_new,
        "expected NEW agent banner to reach pane view via backoff-absorbed re-attach"
    );

    drop(ctrl);
}

/// F12 negative regression. With no respawn happening, the existing
/// happy path must still work: STREAM_OUT bytes flow into the vt100
/// parser exactly as they did pre-F12. The auto-reattach loop only
/// fires on STREAM_END / EOF, so a long-lived stream stays in the
/// single-session branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_respawn_keeps_existing_happy_path() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'echo F12_REGRESSION; sleep 30'"), None)
                .expect("create_pane")
        })
        .await
        .unwrap()
    };

    let ctrl_for_wait = ctrl.clone();
    let pane_id_for_wait = pane_id.clone();
    let saw = wait_for(
        Duration::from_secs(5),
        Duration::from_millis(20),
        move || screen_contains(&ctrl_for_wait, &pane_id_for_wait, "F12_REGRESSION"),
    )
    .await;
    assert!(
        saw,
        "expected single-session STREAM_OUT bytes to reach pane view (happy path regression)"
    );

    // The io_task must still be alive (no Closed observed → no
    // re-attach attempts → no give-up). `write_raw_bytes` succeeds iff
    // the input channel's receiver is still drained by the task.
    assert!(
        ctrl.write_raw_bytes(&pane_id, b"").is_ok(),
        "io_task should be alive in the no-respawn happy path"
    );

    drop(ctrl);
}
