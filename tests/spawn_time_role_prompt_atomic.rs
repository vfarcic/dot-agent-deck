//! PRD #100 regression: the orchestrator spawn-time role-prompt
//! injection (`ui.rs::pane.write_and_submit_to_pane(start_pane_id, &prompt)`)
//! must produce the same atomic byte stream as the working
//! daemon-initiated orchestration-delegate path
//! (`AgentPtyRegistry::write_to_pane_and_submit`).
//!
//! The bug: the legacy `PaneController::write_to_pane` queued two
//! `KIND_STREAM_IN` frames (payload, then `\r`) separated by a
//! `std::thread::sleep(SUBMIT_DELAY)`. The daemon's per-agent writer
//! mutex was released between the frames, so a concurrent
//! daemon-initiated `write_to_pane_and_submit` on the same pane
//! (e.g. a sibling worker's work-done feedback firing while the
//! orchestrator's role prompt was still in flight) could land its
//! full payload + CR sequence into the gap. The receiving agent then
//! saw `[user payload][daemon payload][daemon \r][user \r]` — the
//! daemon's CR submitted the fused line; the user's trailing CR
//! landed in an empty input box and was rendered as a newline. This
//! is the "Enter inserted a newline into the orchestrator's input
//! instead of submitting" symptom the PRD #100 issue reports.
//!
//! This test pins the contract at the controller layer (one level
//! below the `ui.rs:3733` call site, which is buried in a render
//! loop and not directly drivable from an integration test). It
//! drives a concurrent two-writer race against a `cat -u` PTY:
//!
//! - Task A: `EmbeddedPaneController::write_and_submit_to_pane`
//!   (the new atomic RPC path — `WriteAndSubmit` over the wire,
//!   handled in-process by `AgentPtyRegistry::write_to_pane_and_submit`).
//! - Task B: `AgentPtyRegistry::write_to_pane_and_submit` direct
//!   in-process call (matches the daemon-initiated work-done
//!   feedback path at `state.rs::handle_work_done`).
//!
//! With the fix, both writers' payload+CR sequences serialize on the
//! per-agent writer mutex — neither fuses into the other. `cat -u`
//! echoes both as CLEAN separate lines.
//!
//! Toggle-verify: temporarily swap task A's `write_and_submit_to_pane`
//! call to `write_to_pane` (the legacy two-frames-with-gap path) and
//! the `ROLE-PROMPT-MARKER\r\n` contiguous-bytes assertion below
//! fails — the daemon-initiated CR submits the fused line, so
//! `ROLE-PROMPT-MARKER` is followed by `DAEMON-FEEDBACK-MARKER`
//! rather than `\r\n`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::{AgentSpawnOptions, PaneController};
use dot_agent_deck::state::{AppState, SharedState};

mod common;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct DaemonHandle {
    _dir: TempDir,
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
        let attach = dir.path().join("attach.sock");
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
    while tokio::time::Instant::now() < deadline {
        if attach_path.exists() && UnixStream::connect(&attach_path).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        attach_path.exists(),
        "attach socket did not appear within 5s"
    );

    DaemonHandle {
        _dir: dir,
        attach_path,
        pty_registry,
        handle,
    }
}

async fn wait_for_bytes_in_snapshot(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &[u8],
    timeout: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(snap) = registry.snapshot(agent_id)
            && snap.windows(needle.len()).any(|w| w == needle)
        {
            return snap;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    registry.snapshot(agent_id).unwrap_or_default()
}

/// PRD #100 regression: concurrent client-initiated atomic write and
/// daemon-initiated `write_to_pane_and_submit` against the same pane
/// must serialize — neither writer's payload may fuse with the other
/// across the legacy two-`STREAM_IN`-frame gap.
///
/// Sequence:
///   1. Spawn a `cat -u` agent via the controller (so the pane is
///      registered on both the local controller AND the daemon).
///   2. Spawn task A: `EmbeddedPaneController::write_and_submit_to_pane`
///      with "ROLE-PROMPT-MARKER". (Routes through the new atomic
///      `WriteAndSubmit` RPC.)
///   3. After a short delay (designed to land inside the legacy
///      path's mid-sequence mutex gap — for the OLD `write_to_pane`),
///      kick off task B: `AgentPtyRegistry::write_to_pane_and_submit`
///      with "DAEMON-FEEDBACK-MARKER". (Matches `handle_work_done`'s
///      direct in-process call.)
///   4. Wait for both to complete and assert `cat -u`'s scrollback
///      contains `ROLE-PROMPT-MARKER\r\n` as a CONTIGUOUS byte
///      sequence — proves the atomic boundary held and no daemon
///      bytes were spliced into the user payload before the submit CR.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_time_role_prompt_is_atomic_against_concurrent_daemon_write() {
    let daemon = spawn_daemon().await;

    let controller = Arc::new(EmbeddedPaneController::new(
        daemon.attach_path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Spawn via the controller so the pane is registered on BOTH the
    // local controller (`self.panes`) AND the daemon registry. This
    // mirrors the production spawn-time setup and makes the legacy
    // `write_to_pane` path (used in the toggle-verify edit) reachable
    // — without local registration, `queue_stream_input` would fail
    // with "pane not found" before the atomicity assertion runs.
    let controller_for_spawn = controller.clone();
    let (pane_id, _resolved) = tokio::task::spawn_blocking(move || {
        controller_for_spawn
            .create_pane_with_options(Some("cat -u"), None, AgentSpawnOptions::default())
            .expect("create_pane_with_options")
    })
    .await
    .expect("join spawn task");

    // Look up the daemon-side agent id by matching pane_id_env. The
    // controller-spawned agent's pane_id_env equals the locally
    // allocated `pane_id` (see `create_stream_pane`).
    let agent_id = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let records = daemon.pty_registry.agent_records();
            if let Some(rec) = records
                .iter()
                .find(|r| r.pane_id_env.as_deref() == Some(pane_id.as_str()))
            {
                break rec.id.clone();
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("daemon registry never surfaced agent for pane_id {pane_id}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    let role_prompt = "ROLE-PROMPT-MARKER";
    let daemon_feedback = "DAEMON-FEEDBACK-MARKER";

    let controller_for_task_a = controller.clone();
    let pane_for_task_a = pane_id.clone();
    let role_prompt_for_task_a = role_prompt.to_string();
    let task_a = tokio::task::spawn_blocking(move || {
        controller_for_task_a
            .write_and_submit_to_pane(&pane_for_task_a, &role_prompt_for_task_a)
            .expect("write_and_submit_to_pane on task A")
    });

    // Kick task B partway into task A's expected write window. For
    // the OLD `write_to_pane` (toggle-verify), task A holds no mutex
    // between its two `STREAM_IN` frames and its `std::thread::sleep`
    // is 150 ms — 40 ms lands the daemon-initiated write deep inside
    // that gap. For the new atomic RPC, the daemon holds the writer
    // mutex end-to-end, so task B blocks until task A releases.
    tokio::time::sleep(Duration::from_millis(40)).await;

    let registry_for_task_b = daemon.pty_registry.clone();
    let pane_for_task_b = pane_id.clone();
    let daemon_feedback_for_task_b = daemon_feedback.to_string();
    let task_b = tokio::spawn(async move {
        registry_for_task_b
            .write_to_pane_and_submit(&pane_for_task_b, &daemon_feedback_for_task_b)
            .await
            .expect("write_to_pane_and_submit on task B")
    });

    task_a.await.expect("join task A");
    task_b.await.expect("join task B");

    // Both writers' payloads must echo cleanly back through cat -u's
    // stdout. The crucial assertion is that `ROLE-PROMPT-MARKER\r\n`
    // appears as a CONTIGUOUS byte sequence — a fused failure mode
    // would have `ROLE-PROMPT-MARKERDAEMON-FEEDBACK-MARKER` between
    // the marker and the CRLF.
    // Wait for the LATER of the two echoes — task B runs after task
    // A, so `DAEMON-FEEDBACK-MARKER\r\n` is the trailing event. Once
    // its echo lands the scrollback is guaranteed to contain the full
    // serialized output of both writers (whatever order they fused
    // in).
    let snap = wait_for_bytes_in_snapshot(
        &daemon.pty_registry,
        &agent_id,
        b"DAEMON-FEEDBACK-MARKER\r\n",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        snap.windows(b"ROLE-PROMPT-MARKER\r\n".len())
            .any(|w| w == b"ROLE-PROMPT-MARKER\r\n"),
        "atomic contract violated: ROLE-PROMPT-MARKER\\r\\n must appear as a contiguous \
         sequence in the cat -u echo, proving no daemon bytes were spliced into the user \
         payload before the submit CR. snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );
    assert!(
        snap.windows(b"DAEMON-FEEDBACK-MARKER\r\n".len())
            .any(|w| w == b"DAEMON-FEEDBACK-MARKER\r\n"),
        "daemon-initiated write must also surface its own clean echo line; \
         absence indicates the two writes never serialized. snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );
}
