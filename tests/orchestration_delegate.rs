//! End-to-end tests for daemon-side orchestration dispatch.
//!
//! PRD #93 round-5: the daemon owns delegate/work-done dispatch entirely.
//! When a worker's hook socket receives a `Delegate` signal from an
//! orchestrator pane, the daemon resolves the target role's pane, builds
//! a file-backed task prompt, and writes the one-liner directly into the
//! target pane's PTY via [`AgentPtyRegistry::write_to_pane`]. The PTY
//! scrollback is the "journal" surface — the bytes survive any number of
//! detach/reattach cycles via the standard pane snapshot replay.
//!
//! Previous rounds had a `BroadcastMsg::Delegate` / `BroadcastMsg::WorkDone`
//! hop between daemon and TUI guarded by a `PendingBroadcasts` replay
//! buffer and a `try_recv` salvage path; all of that is gone. The tests
//! below pin the new contract: direct PTY writes, scoped to the
//! orchestration tab, surviving subscriber detach.
//!
//! The harness spawns an integer-named agent (`cat -u`) per role. `cat -u`
//! echoes stdin verbatim to stdout, which in turn surfaces on the PTY
//! master and lands in the registry's per-agent scrollback bus. That's
//! what we read via `pty_registry.snapshot(agent_id)` to assert "the
//! prompt arrived in the pane's scrollback".

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, TabMembership, is_valid_pane_id_env,
};
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::event::{DaemonMessage, DelegateSignal, WorkDoneSignal};
use dot_agent_deck::state::{AppState, SharedState};

mod common;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

/// Owns the spawned daemon coroutine plus the live clones of the daemon's
/// `state` and `pty_registry`. Tests hold onto these to drive PTY reads
/// (`pty_registry.snapshot`) and inspect the daemon-side role map directly
/// — they're the same Arcs the daemon's hook loop mutates.
struct DaemonHandle {
    _dir: TempDir,
    hook_path: PathBuf,
    attach_path: PathBuf,
    state: SharedState,
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
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hook.sock");
        let attach = dir.path().join("attach.sock");
        (dir, hook, attach)
    };

    let state: SharedState = Arc::new(RwLock::new(AppState::default()));
    let daemon = Daemon::with_attach(state.clone(), attach_path.clone()).with_idle_shutdown(None);
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
        hook_path,
        attach_path,
        state,
        pty_registry,
        handle,
    }
}

async fn send_delegate(hook_path: &PathBuf, signal: &DelegateSignal) {
    let mut stream = UnixStream::connect(hook_path).await.expect("connect hook");
    let msg = DaemonMessage::Delegate(signal.clone());
    let mut json = serde_json::to_vec(&msg).unwrap();
    json.push(b'\n');
    stream.write_all(&json).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn send_work_done(hook_path: &PathBuf, signal: &WorkDoneSignal) {
    let mut stream = UnixStream::connect(hook_path).await.expect("connect hook");
    let msg = DaemonMessage::WorkDone(signal.clone());
    let mut json = serde_json::to_vec(&msg).unwrap();
    json.push(b'\n');
    stream.write_all(&json).await.unwrap();
    stream.shutdown().await.unwrap();
}

/// Spawn an orchestration role agent attached to a PTY running `cat -u`.
/// `cat -u` echoes its stdin verbatim to stdout, which the registry
/// captures in the agent's scrollback bus — that's the surface the daemon
/// writes the delegate prompt to and the surface our assertions read.
///
/// Returns the daemon-side agent id; the pane_id is the caller-supplied
/// `pane_id` argument (passed via the `DOT_AGENT_DECK_PANE_ID` env so the
/// daemon registry mirrors it on `pane_id_env`).
async fn start_role_pane(
    daemon: &DaemonHandle,
    orchestration_name: &str,
    role_name: &str,
    is_start_role: bool,
    role_index: usize,
    pane_id: &str,
    cwd: &str,
) -> String {
    assert!(is_valid_pane_id_env(pane_id), "test pane_id must be valid");
    let client = DaemonClient::new(daemon.attach_path.clone());
    client
        .start_agent(StartAgentOptions {
            command: Some("cat -u".to_string()),
            cwd: Some(cwd.to_string()),
            display_name: Some(role_name.to_string()),
            rows: 24,
            cols: 80,
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: orchestration_name.to_string(),
                role_index,
                role_name: role_name.to_string(),
                is_start_role,
            }),
            agent_type: None,
        })
        .await
        .expect("start_agent")
}

/// Poll the agent's scrollback (via the daemon's registry, in-process) for
/// `needle`. Returns the full snapshot on success, panics on timeout. The
/// indirection through the registry avoids re-implementing the snapshot
/// wire protocol and keeps the test focused on "did the bytes arrive in
/// the PTY scrollback" rather than "does the snapshot frame round-trip"
/// (covered separately in `tests/daemon_protocol.rs`).
async fn wait_for_in_snapshot(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &str,
    timeout: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(snap) = registry.snapshot(agent_id)
            && snap.windows(needle.len()).any(|w| w == needle.as_bytes())
        {
            return snap;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let last = registry.snapshot(agent_id).unwrap_or_default();
    panic!(
        "needle {:?} not found in agent {} scrollback within {:?}; last snapshot: {:?}",
        needle,
        agent_id,
        timeout,
        String::from_utf8_lossy(&last)
    );
}

/// Headline test: a delegate signal from the orchestrator's hook socket
/// must land as a worker-task prompt directly in the target role pane's
/// PTY scrollback — no broadcast hop, no buffer.
#[tokio::test]
async fn daemon_writes_delegate_prompt_to_target_role_pane() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let _orch_agent_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &cwd,
    )
    .await;
    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    // Sanity: the daemon-side role map was populated by StartAgent.
    {
        let st = daemon.state.read().await;
        assert_eq!(
            st.pane_role_map.get("orch-pane").map(String::as_str),
            Some("orchestrator")
        );
        assert_eq!(
            st.pane_role_map.get("coder-pane").map(String::as_str),
            Some("coder")
        );
        assert!(st.orchestrator_pane_ids.contains("orch-pane"));
        assert!(!st.orchestrator_pane_ids.contains("coder-pane"));
    }

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "Implement the auth module".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // The daemon writes a file-backed one-liner to the worker pane. Pin
    // the one-liner shape — the per-role file path is what surfaces in
    // the scrollback (Claude Code would otherwise fragment a multi-line
    // task across multiple prompts). The work-done footer
    // (round-7 restore) is the last block of the payload, so waiting on
    // it guarantees the file reference has already flushed too.
    let snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id,
        "dot-agent-deck work-done",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&snap).contains("Read .dot-agent-deck/worker-task-coder.md"),
        "expected the worker-task one-liner in coder pane scrollback"
    );
    assert!(
        String::from_utf8_lossy(&snap).contains("dot-agent-deck work-done"),
        "delegate prompt must include the work-done footer so the worker signals completion"
    );

    // And the task file itself must exist alongside it.
    let task_file = std::path::Path::new(&cwd).join(".dot-agent-deck/worker-task-coder.md");
    assert!(
        task_file.exists(),
        "daemon should write the task body to a file the worker can read in one shot"
    );
    let body = std::fs::read_to_string(&task_file).unwrap();
    assert_eq!(body, "Implement the auth module");
}

/// Symmetric guard: a work-done signal from a worker must write the
/// per-role summary file AND inject the "Worker {role} has completed..."
/// feedback directly into the orchestrator pane's PTY.
#[tokio::test]
async fn daemon_writes_work_done_feedback_to_orchestrator_pane() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let orch_agent_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &cwd,
    )
    .await;
    let _coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    send_work_done(
        &daemon.hook_path,
        &WorkDoneSignal {
            pane_id: "coder-pane".into(),
            task: "implemented the auth module".into(),
            done: false,
            timestamp: Utc::now(),
        },
    )
    .await;

    // Summary file is the user-visible artifact the orchestrator will be
    // prompted to read.
    let summary = std::path::Path::new(&cwd).join(".dot-agent-deck/work-done-coder.md");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if summary.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        summary.exists(),
        "daemon must write the work-done summary file"
    );
    assert_eq!(
        std::fs::read_to_string(&summary).unwrap(),
        "implemented the auth module"
    );

    // Feedback one-liner must appear in the orchestrator pane's scrollback.
    let snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &orch_agent_id,
        "Worker coder has completed their task.",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&snap).contains(".dot-agent-deck/work-done-coder.md"),
        "feedback must point the orchestrator at the per-role summary file"
    );
}

/// CodeRabbit #2 (PRD #93 round-9): a delegate must land the task file
/// in *each worker's* cwd, not the orchestrator's. Earlier rounds
/// captured the orchestrator's cwd once and reused it for every
/// target, which silently misrouted when role panes were started in
/// different directories.
#[tokio::test]
async fn delegate_writes_task_file_to_each_workers_own_cwd() {
    let daemon = spawn_daemon().await;
    let orch_cwd_dir = tempfile::tempdir().unwrap();
    let orch_cwd = orch_cwd_dir.path().to_string_lossy().into_owned();
    let worker_cwd_dir = tempfile::tempdir().unwrap();
    let worker_cwd = worker_cwd_dir.path().to_string_lossy().into_owned();
    assert_ne!(
        orch_cwd, worker_cwd,
        "test setup: orchestrator and worker cwds must differ"
    );

    let _orch_agent_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &orch_cwd,
    )
    .await;
    let coder_agent_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "coder",
        false,
        1,
        "coder-pane",
        &worker_cwd,
    )
    .await;

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "Implement the auth module".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Worker should see the one-liner in its own scrollback.
    let _ = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id,
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(5),
    )
    .await;

    // The task file MUST exist in the worker's cwd…
    let worker_task =
        std::path::Path::new(&worker_cwd).join(".dot-agent-deck/worker-task-coder.md");
    assert!(
        worker_task.exists(),
        "task file must land in the worker's cwd, not the orchestrator's; expected {}",
        worker_task.display()
    );

    // …and MUST NOT exist in the orchestrator's cwd.
    let orch_task = std::path::Path::new(&orch_cwd).join(".dot-agent-deck/worker-task-coder.md");
    assert!(
        !orch_task.exists(),
        "task file must not be written to the orchestrator's cwd; spurious file at {}",
        orch_task.display()
    );
}

/// CodeRabbit #3 (PRD #93 round-9): a role config's `prompt_template`
/// must wrap the task body in the file the worker reads. Round 5
/// dropped this when dispatch moved daemon-side; the daemon now
/// re-loads `.dot-agent-deck/config.toml` from the worker's cwd to
/// apply per-role wrapping (approach (b) from the brief).
#[tokio::test]
async fn delegate_wraps_task_with_role_prompt_template() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    // Write a project config file with a prompt_template on the coder
    // role. The daemon's `lookup_orchestration_role` parses
    // `<cwd>/.dot-agent-deck.toml` via `load_project_config`.
    let config_toml = r#"
[[orchestrations]]
name = "tdd-cycle"

[[orchestrations.roles]]
name = "orchestrator"
command = "cat -u"
start = true

[[orchestrations.roles]]
name = "coder"
command = "cat -u"
prompt_template = "You are the coder. Always run tests before finishing."
"#;
    std::fs::write(
        std::path::Path::new(&cwd).join(".dot-agent-deck.toml"),
        config_toml,
    )
    .unwrap();

    let _orch_agent_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &cwd,
    )
    .await;
    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "Implement the auth module".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    let _ = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id,
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(5),
    )
    .await;

    let task_file = std::path::Path::new(&cwd).join(".dot-agent-deck/worker-task-coder.md");
    let body = std::fs::read_to_string(&task_file).unwrap();
    assert!(
        body.contains("You are the coder. Always run tests before finishing."),
        "task file must include the role's prompt_template; got:\n{body}"
    );
    assert!(
        body.contains("## Task"),
        "wrapped file must include the `## Task` header that separates the template from the task body; got:\n{body}"
    );
    assert!(
        body.contains("Implement the auth module"),
        "wrapped file must still include the original task body; got:\n{body}"
    );
}

/// Anti-spoofing: a delegate from a worker pane (or any non-orchestrator
/// pane) must be dropped daemon-side and produce no PTY write.
#[tokio::test]
async fn delegate_from_non_orchestrator_is_rejected_daemon_side() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let _orch_agent_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &cwd,
    )
    .await;
    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    // Spoof: a worker pane sends a delegate to another worker. The
    // daemon's `handle_delegate` must reject it because `coder-pane` is
    // not in `orchestrator_pane_ids`.
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "coder-pane".into(),
            task: "spoofed task".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Hook loop is async; give it time to process and drop.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let snap = daemon.pty_registry.snapshot(&coder_agent_id).unwrap();
    assert!(
        !String::from_utf8_lossy(&snap).contains("spoofed task"),
        "spoofed delegate from a non-orchestrator pane must not reach any PTY"
    );
    assert!(
        !std::path::Path::new(&cwd)
            .join(".dot-agent-deck/worker-task-coder.md")
            .exists(),
        "spoofed delegate must not produce a task file either"
    );
}

/// The headline empirical test that rounds 1-4 could not pass cleanly:
/// a worker fires `work-done` while no TUI subscriber is attached, and a
/// fresh subscriber attached later must still see the feedback.
///
/// Under the broadcast hop this was a multi-round bug (the daemon's
/// `event_tx.send` returned Err with zero subscribers, the
/// `PendingBroadcasts` replay buffer plus salvage loop tried to plug
/// every detach race). Under the new design the orchestrator pane's PTY
/// scrollback retains the feedback indefinitely, so a reattach reads it
/// back via `pty_registry.snapshot` regardless of subscriber state at
/// the moment of the signal.
#[tokio::test]
async fn work_done_survives_subscriber_detach_and_reattach() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let orch_agent_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &cwd,
    )
    .await;
    let _coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    // Open and immediately drop a subscriber. Wait long enough for the
    // daemon-side per-connection task to observe the EOF.
    let client = DaemonClient::new(daemon.attach_path.clone());
    {
        let _initial = client.subscribe_events().await.expect("initial subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Worker fires while no subscriber is attached.
    send_work_done(
        &daemon.hook_path,
        &WorkDoneSignal {
            pane_id: "coder-pane".into(),
            task: "implemented under detached deck".into(),
            done: false,
            timestamp: Utc::now(),
        },
    )
    .await;

    // The feedback must already be in the orchestrator pane's
    // scrollback — the daemon wrote it directly into the PTY, no
    // broadcast involved.
    let snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &orch_agent_id,
        "Worker coder has completed their task.",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&snap).contains(".dot-agent-deck/work-done-coder.md"),
        "feedback bytes must include the per-role summary path"
    );

    // Fresh subscriber reattaches and reads the same scrollback — the
    // user-visible "after reattach the orchestrator pane shows the
    // feedback" path. We assert via the registry rather than
    // round-tripping the snapshot frame for the same reason as
    // [`daemon_writes_delegate_prompt_to_target_role_pane`].
    let _reattach = client.subscribe_events().await.expect("reattach subscribe");
    let snap2 = daemon.pty_registry.snapshot(&orch_agent_id).unwrap();
    assert!(
        String::from_utf8_lossy(&snap2).contains("Worker coder has completed their task."),
        "reattached subscriber must still observe the feedback in the orchestrator pane's scrollback"
    );
}

/// Poll the agent's scrollback for an arbitrary byte pattern (not just
/// a UTF-8 needle) and return the full snapshot once it matches. Used by
/// the round-6 tests below to wait for control-byte patterns
/// (`\x1b[201~`, `\r\n`) that `wait_for_in_snapshot`'s string interface
/// can't express conveniently.
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
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let last = registry.snapshot(agent_id).unwrap_or_default();
    panic!(
        "byte pattern {:?} not found in agent {} scrollback within {:?}; last snapshot: {:?}",
        needle,
        agent_id,
        timeout,
        String::from_utf8_lossy(&last)
    );
}

/// PRD #93 round-6: the daemon's `write_to_pane` must follow the same
/// submit contract as the TUI's `EmbeddedPaneController::write_to_pane`
/// — write the encoded prompt, wait `SUBMIT_DELAY`, then write a carriage
/// return. Without the trailing CR the agent TUI sees the prompt sitting
/// in its input box and never starts processing.
///
/// We exercise the contract by writing through the registry directly
/// (the same call site `AppState::handle_delegate` uses) and asserting
/// that the prompt text *and* a submit-induced newline land in the
/// role pane's scrollback. The `cat -u` agent reads the line and writes
/// it back to its stdout once the submit CR is interpreted as Enter
/// (ICRNL flips the input `\r` to `\n`, which closes the canonical line;
/// cat then echoes the buffered line and ONLCR rewrites the trailing
/// `\n` to `\r\n` on the master side). Without the round-6 fix the
/// daemon never writes the CR, the line never closes, and cat's stdout
/// stays empty — so the test fails at exactly the layer round-6 broke.
#[tokio::test]
async fn write_to_pane_emits_submit_cr_after_single_line_prompt() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    daemon
        .pty_registry
        .write_to_pane("coder-pane", "hello world")
        .await
        .expect("write_to_pane should succeed for a known pane");

    // Look for cat -u's stdout output: "hello world\r\n" (the slave
    // termios processed the submitted line — ICRNL on input made it a
    // complete line, ONLCR on output appended the CR before the LF).
    // Polling, not a single read, because pump_reader pushes bytes to
    // the bus on its own thread and may lag the write by a few ms.
    let snap = wait_for_bytes_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id,
        b"hello world\r\n",
        Duration::from_secs(5),
    )
    .await;

    assert!(
        snap.windows(b"hello world\r\n".len())
            .any(|w| w == b"hello world\r\n"),
        "scrollback must contain the submitted line with its trailing CRLF; \
         snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );
}

/// PRD #93 round-6: a multi-line prompt must be wrapped in
/// bracketed-paste markers so the receiving agent TUI treats it as one
/// paste rather than fragmenting it into N submissions (one per
/// embedded newline). Mirrors the TUI's `encode_pane_payload` contract.
///
/// We assert the markers appear in `cat -u`'s stdout output (the raw
/// ESC form, `\x1b[200~ ... \x1b[201~`). The slave's *echo* of the same
/// input also surfaces on the master, but ECHOCTL on the slave termios
/// renders ESC as the two-byte `^[` literal there — so the only place
/// the raw markers reach the master is the stdout path, and only after
/// the round-6 submit CR closes the line and lets `cat` flush.
#[tokio::test]
async fn write_to_pane_wraps_multiline_in_bracketed_paste() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    daemon
        .pty_registry
        .write_to_pane("coder-pane", "line1\nline2\nline3")
        .await
        .expect("write_to_pane should succeed for a known pane");

    // Wait for the END marker in cat's stdout (raw ESC form) — that's
    // the last byte of the encoded payload, so its presence implies the
    // full payload + submit CR sequence has flowed through.
    let snap = wait_for_bytes_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id,
        b"\x1b[201~",
        Duration::from_secs(5),
    )
    .await;

    assert!(
        snap.windows(b"\x1b[200~".len()).any(|w| w == b"\x1b[200~"),
        "bracketed-paste START marker (ESC[200~) missing from scrollback: {:?}",
        String::from_utf8_lossy(&snap)
    );
    assert!(
        snap.windows(b"\x1b[201~".len()).any(|w| w == b"\x1b[201~"),
        "bracketed-paste END marker (ESC[201~) missing from scrollback: {:?}",
        String::from_utf8_lossy(&snap)
    );
    // And the wrapped content must sit between the markers (we don't
    // pin the exact slice because ONLCR rewrites the embedded LFs to
    // CRLF on the way out — but `line2` is unambiguous).
    assert!(
        snap.windows(b"line2".len()).any(|w| w == b"line2"),
        "middle content line missing from scrollback: {:?}",
        String::from_utf8_lossy(&snap)
    );
}

/// PRD #93 round-8 (auditor HIGH): two concurrent writes to the *same*
/// pane must not interleave. Earlier rounds released the writer mutex
/// around `SUBMIT_DELAY`, so two delegates could fuse as
/// `payload_A + payload_B + CR + CR` — the slave's canonical line then
/// contained both payloads, cat printed them as a single fused line,
/// and the second prompt never reached the worker as its own input.
///
/// We use distinct, easy-to-grep run-length payloads so a single
/// canonical "complete line" assertion suffices: cat -u with the
/// PTY's default termios echoes each *complete* line back as
/// `<payload>\r\n` on its stdout (ICRNL → LF closes the canonical
/// line, cat reads it, ONLCR rewrites the trailing LF to CRLF on
/// output). With the fix both `AAAA…\r\n` and `BBBB…\r\n` appear as
/// contiguous slices. Without it the slave sees `AAAA…BBBB…\r\r`
/// and only the fused `AAAA…BBBB…\r\n` reaches the master — neither
/// individual `payload\r\n` substring is present.
#[tokio::test]
async fn write_to_pane_serializes_concurrent_writes_per_pane() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    let payload_a = "A".repeat(20);
    let payload_b = "B".repeat(20);

    let registry_a = daemon.pty_registry.clone();
    let registry_b = daemon.pty_registry.clone();
    let payload_a_for_task = payload_a.clone();
    let payload_b_for_task = payload_b.clone();
    let write_a = tokio::spawn(async move {
        registry_a
            .write_to_pane("coder-pane", &payload_a_for_task)
            .await
            .expect("write A");
    });
    let write_b = tokio::spawn(async move {
        registry_b
            .write_to_pane("coder-pane", &payload_b_for_task)
            .await
            .expect("write B");
    });
    let (a, b) = tokio::join!(write_a, write_b);
    a.unwrap();
    b.unwrap();

    let needle_a = format!("{payload_a}\r\n");
    let needle_b = format!("{payload_b}\r\n");

    // Wait for both submitted lines to surface on cat's stdout. If the
    // writes interleaved, the slave sees `AAA…BBB…\r\r` and the two
    // payloads collapse into a single canonical line — `payload\r\n`
    // for each one will never appear.
    let snap = wait_for_bytes_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id,
        needle_a.as_bytes(),
        Duration::from_secs(5),
    )
    .await;
    let snap = if snap
        .windows(needle_b.len())
        .any(|w| w == needle_b.as_bytes())
    {
        snap
    } else {
        wait_for_bytes_in_snapshot(
            &daemon.pty_registry,
            &coder_agent_id,
            needle_b.as_bytes(),
            Duration::from_secs(5),
        )
        .await
    };

    // Each payload appearing followed by its own CRLF is the signature
    // of a serialized submit: cat's canonical mode delivered each line
    // to cat *separately*, so cat wrote two distinct `payload\r\n` lines
    // to its stdout. Without the round-8 fix, the slave would see
    // `AAA…BBB…\r\r` as one canonical line + an empty one — cat would
    // emit `AAA…BBB…\r\n\r\n` and neither `AAA…\r\n` nor `BBB…\r\n`
    // would appear individually.
    //
    // (The slave's input *echo* of B's incoming bytes can land on the
    // master *between* cat's two stdout writes, producing apparent
    // interleaving in the snapshot — that's a master-side rendering
    // artifact of how echo and stdout race, not evidence of fused
    // daemon writes. The two `payload\r\n` substrings are the cleanest
    // assertion that doesn't depend on that race.)
    assert!(
        snap.windows(needle_a.len())
            .any(|w| w == needle_a.as_bytes()),
        "payload_A\\r\\n missing from scrollback — concurrent writes interleaved: {:?}",
        String::from_utf8_lossy(&snap)
    );
    assert!(
        snap.windows(needle_b.len())
            .any(|w| w == needle_b.as_bytes()),
        "payload_B\\r\\n missing from scrollback — concurrent writes interleaved: {:?}",
        String::from_utf8_lossy(&snap)
    );
}

/// PRD #93 round-8: the per-pane lock must not serialize writes across
/// *different* panes. Each agent owns its own `writer` mutex, so two
/// concurrent `write_to_pane` calls to different panes should run in
/// parallel — about one `SUBMIT_DELAY` of wall clock, not two.
///
/// We allow a generous slack on the upper bound because the daemon
/// background work (spawn threads, broadcast push) can add jitter; the
/// purpose is to catch a regression that serializes across panes
/// (e.g. a global writer lock), which would push the total to roughly
/// 2× `SUBMIT_DELAY` and well past the threshold below.
#[tokio::test]
async fn write_to_pane_concurrent_writes_to_different_panes_run_in_parallel() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let _orch_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &cwd,
    )
    .await;
    let _coder_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    let registry_a = daemon.pty_registry.clone();
    let registry_b = daemon.pty_registry.clone();
    let start = std::time::Instant::now();
    let write_a = tokio::spawn(async move { registry_a.write_to_pane("orch-pane", "alpha").await });
    let write_b = tokio::spawn(async move { registry_b.write_to_pane("coder-pane", "beta").await });
    let (a, b) = tokio::join!(write_a, write_b);
    a.unwrap().expect("write to orch-pane");
    b.unwrap().expect("write to coder-pane");
    let elapsed = start.elapsed();

    // SUBMIT_DELAY is 150ms; serial would be ~300ms, parallel ~150ms.
    // 250ms upper bound leaves room for task scheduling jitter without
    // letting an across-pane serialization regression slip through.
    assert!(
        elapsed < Duration::from_millis(250),
        "two concurrent writes to different panes took {:?} — expected ~SUBMIT_DELAY \
         (~150ms), suggesting the per-pane lock is over-serializing",
        elapsed
    );
}
