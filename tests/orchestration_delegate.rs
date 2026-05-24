//! End-to-end tests for daemon-side orchestration dispatch.
//!
//! PRD #93 round-5: the daemon owns delegate/work-done dispatch entirely.
//! When a worker's hook socket receives a `Delegate` signal from an
//! orchestrator pane, the daemon resolves the target role's pane, builds
//! a file-backed task prompt, and writes the one-liner directly into the
//! target pane's PTY via [`AgentPtyRegistry::write_to_pane_and_submit`]. The PTY
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
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::event::{
    AgentEvent, AgentType, DaemonMessage, DelegateSignal, EventType, WorkDoneSignal,
};
use dot_agent_deck::pane::{AgentSpawnOptions, PaneController};
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
    let daemon = Daemon::with_attach(state.clone(), attach_path.clone())
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

/// PRD #92 F9 followup-6: inject a synthetic `SessionStart`
/// `AgentEvent` for `pane_id` over the daemon's hook socket. Mirrors
/// the wire shape a real agent's hook script would send. Used by the
/// fast-path dispatch test to drive the post-respawn write off the
/// 10 s timeout fallback and onto the event-observed path — a `cat -u`
/// stub agent never emits `SessionStart` on its own, so the test has
/// to forge it.
///
/// PRD #92 F9 followup-7: the dispatch task's `wait_for_session_start`
/// filter is `(pane_id, agent_id)` since followup-7, so a forged event
/// MUST carry the NEW agent's id or it will be rejected as if it came
/// from the OLD (pre-respawn) agent. Callers pass the post-respawn
/// `agent_id_for_pane(...)` lookup.
async fn send_session_start(hook_path: &PathBuf, pane_id: &str, agent_id: Option<&str>) {
    let event = AgentEvent {
        session_id: format!("synthetic-{pane_id}"),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: Utc::now(),
        user_prompt: None,
        metadata: std::collections::HashMap::new(),
        pane_id: Some(pane_id.to_string()),
        agent_id: agent_id.map(str::to_string),
    };
    let mut stream = UnixStream::connect(hook_path).await.expect("connect hook");
    let mut json = serde_json::to_vec(&event).unwrap();
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
    // For the common case where every role pane shares the
    // orchestration's cwd, `orchestration_cwd == cwd`. The round-9 #2
    // regression test (`delegate_writes_task_file_to_each_workers_own_cwd`)
    // exercises divergence via `start_role_pane_with_orch_cwd`.
    start_role_pane_with_orch_cwd(
        daemon,
        orchestration_name,
        role_name,
        is_start_role,
        role_index,
        pane_id,
        cwd,
        cwd,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_role_pane_with_orch_cwd(
    daemon: &DaemonHandle,
    orchestration_name: &str,
    role_name: &str,
    is_start_role: bool,
    role_index: usize,
    pane_id: &str,
    cwd: &str,
    orchestration_cwd: &str,
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
                orchestration_cwd: Some(orchestration_cwd.to_string()),
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

    // Round-11 auditor #C: the orchestration's identity is shared
    // across all role panes via TabMembership.orchestration_cwd —
    // each pane's *own* `cwd` (per-pane cwd, used for the file
    // write) can still diverge, which is exactly what this test
    // pins. Both panes carry `orchestration_cwd = orch_cwd` so the
    // daemon groups them as the same orchestration.
    let _orch_agent_id = start_role_pane_with_orch_cwd(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &orch_cwd,
        &orch_cwd,
    )
    .await;
    let coder_agent_id = start_role_pane_with_orch_cwd(
        &daemon,
        "tdd-cycle",
        "coder",
        false,
        1,
        "coder-pane",
        &worker_cwd,
        &orch_cwd,
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
    let _coder_agent_id_initial =
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

    // PRD #92 F9: the coder role defaults to `clear = true`, so this
    // delegate triggers a respawn of the coder agent. The pre-respawn
    // agent id is stale by the time the prompt lands — key the wait
    // on the stable `pane_id_env` instead.
    //
    // PRD #92 F9 followup-6: the post-respawn write now waits for the
    // new agent's `SessionStart` event (10 s timeout fallback).
    // `cat -u` never emits `SessionStart`, so this test always lands
    // on the fallback path — budget is 15 s (10 s wait + spawn / write
    // latency + jitter).
    let _ = wait_for_in_pane_snapshot(
        &daemon.pty_registry,
        "coder-pane",
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(20),
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

/// CodeRabbit round-10 #1: an UNNAMED orchestration (no `name` field
/// or `name = ""` in `.dot-agent-deck.toml`) must still pick up
/// `prompt_template` wrapping. The TUI's tab construction uses the
/// cwd basename as the orchestration name; the daemon's
/// `load_project_config` must apply the SAME fallback so its
/// `orchestrations.iter().find(|o| o.name == ...)` matches.
///
/// Pre-round-10 the loader returned `OrchestrationConfig.name = ""`
/// verbatim, and `handle_delegate` looked up against the
/// basename-resolved name carried in `TabMembership` — a mismatch
/// that silently dropped prompt_template wrapping for the unnamed
/// case.
#[tokio::test]
async fn delegate_applies_prompt_template_for_unnamed_orchestration() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();
    // The daemon's loader resolves an empty/missing config name to the
    // cwd basename — same fallback the TUI applies when building
    // `TabMembership::Orchestration.name`. Both ends key on this
    // resolved value.
    let resolved_name = std::path::Path::new(&cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .expect("tempdir must have a basename");

    // No `name = ...` line — relies on the loader's fallback.
    let config_toml = r#"
[[orchestrations]]

[[orchestrations.roles]]
name = "orchestrator"
command = "cat -u"
start = true

[[orchestrations.roles]]
name = "coder"
command = "cat -u"
prompt_template = "You are the coder for an unnamed orchestration."
"#;
    std::fs::write(
        std::path::Path::new(&cwd).join(".dot-agent-deck.toml"),
        config_toml,
    )
    .unwrap();

    let _orch_agent_id = start_role_pane(
        &daemon,
        &resolved_name,
        "orchestrator",
        true,
        0,
        "orch-pane-2",
        &cwd,
    )
    .await;
    let _coder_agent_id_initial = start_role_pane(
        &daemon,
        &resolved_name,
        "coder",
        false,
        1,
        "coder-pane-2",
        &cwd,
    )
    .await;

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane-2".into(),
            task: "Implement the parser".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // PRD #92 F9: coder role defaults to `clear = true` so this
    // delegate respawns the agent. Use the pane-keyed wait so the
    // post-respawn rotation doesn't strand the assertion on a stale id.
    //
    // PRD #92 F9 followup-6: post-respawn write waits up to 10 s for
    // the new agent's `SessionStart`; `cat -u` never emits one so we
    // budget for the fallback path.
    let _ = wait_for_in_pane_snapshot(
        &daemon.pty_registry,
        "coder-pane-2",
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(20),
    )
    .await;

    let task_file = std::path::Path::new(&cwd).join(".dot-agent-deck/worker-task-coder.md");
    let body = std::fs::read_to_string(&task_file).unwrap();
    assert!(
        body.contains("You are the coder for an unnamed orchestration."),
        "unnamed orchestration must still pick up prompt_template wrapping; got:\n{body}"
    );
}

/// CodeRabbit round-11 auditor #C: two distinct unnamed
/// orchestrations whose cwd-basenames happen to match must NOT
/// cross-route delegates. Pre-round-11 the orchestration identity in
/// `pane_orchestration_map` was just the resolved name, so
/// `~/a/foo/` and `~/b/foo/` both resolved to `"foo"` and a delegate
/// from A's orchestrator could land in B's coder. Round 11 scopes
/// the identity by `(name, cwd)` so the lookup distinguishes them.
#[tokio::test]
async fn delegate_does_not_cross_route_between_same_basename_orchestrations() {
    let daemon = spawn_daemon().await;
    // Two distinct cwds whose basenames are identical (the
    // resolve_orchestration_name fallback would collapse them).
    let parent_a = tempfile::tempdir().unwrap();
    let cwd_a = parent_a.path().join("collision");
    std::fs::create_dir_all(&cwd_a).unwrap();
    let parent_b = tempfile::tempdir().unwrap();
    let cwd_b = parent_b.path().join("collision");
    std::fs::create_dir_all(&cwd_b).unwrap();
    assert_ne!(cwd_a, cwd_b);

    let cwd_a_str = cwd_a.to_string_lossy().into_owned();
    let cwd_b_str = cwd_b.to_string_lossy().into_owned();
    // Both directories' resolved basename is "collision" — the
    // collision the auditor flagged.
    let shared_name = "collision";

    // Both A and B are unnamed orchestrations (TUI fills in basename
    // for the TabMembership name). Spawn role panes in each.
    let _orch_a = start_role_pane(
        &daemon,
        shared_name,
        "orchestrator",
        true,
        0,
        "orch-a",
        &cwd_a_str,
    )
    .await;
    let coder_a = start_role_pane(
        &daemon,
        shared_name,
        "coder",
        false,
        1,
        "coder-a",
        &cwd_a_str,
    )
    .await;
    let _orch_b = start_role_pane(
        &daemon,
        shared_name,
        "orchestrator",
        true,
        0,
        "orch-b",
        &cwd_b_str,
    )
    .await;
    let coder_b = start_role_pane(
        &daemon,
        shared_name,
        "coder",
        false,
        1,
        "coder-b",
        &cwd_b_str,
    )
    .await;

    // Orchestrator A delegates to "coder". Only coder-a should receive.
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-a".into(),
            task: "marker-for-A".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    let snap_a = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_a,
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&snap_a).contains("worker-task-coder"),
        "coder-a must receive the prompt for A's delegate"
    );

    // After the wait above, give the daemon a beat in case any
    // mis-routing was in flight, then assert coder-b is untouched.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap_b = daemon.pty_registry.snapshot(&coder_b).unwrap();
    assert!(
        !String::from_utf8_lossy(&snap_b).contains("worker-task-coder"),
        "coder-b must NOT receive the delegate aimed at orchestration A"
    );
    // And the task file must land in A's cwd, NOT B's.
    let task_a = cwd_a.join(".dot-agent-deck/worker-task-coder.md");
    let task_b = cwd_b.join(".dot-agent-deck/worker-task-coder.md");
    assert!(task_a.exists(), "task file must land in A's cwd");
    assert!(
        !task_b.exists(),
        "task file must NOT also land in B's cwd; spurious file at {}",
        task_b.display()
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

/// PRD #93 round-6: the daemon's `write_to_pane_and_submit` must follow
/// the same submit contract as the TUI's `EmbeddedPaneController::write_to_pane`
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
async fn write_to_pane_and_submit_emits_cr_after_single_line_prompt() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    daemon
        .pty_registry
        .write_to_pane_and_submit("coder-pane", "hello world")
        .await
        .expect("write_to_pane_and_submit should succeed for a known pane");

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
async fn write_to_pane_and_submit_wraps_multiline_in_bracketed_paste() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let coder_agent_id =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    daemon
        .pty_registry
        .write_to_pane_and_submit("coder-pane", "line1\nline2\nline3")
        .await
        .expect("write_to_pane_and_submit should succeed for a known pane");

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
async fn write_to_pane_and_submit_serializes_concurrent_writes_per_pane() {
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
            .write_to_pane_and_submit("coder-pane", &payload_a_for_task)
            .await
            .expect("write A");
    });
    let write_b = tokio::spawn(async move {
        registry_b
            .write_to_pane_and_submit("coder-pane", &payload_b_for_task)
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
/// concurrent `write_to_pane_and_submit` calls to different panes
/// should run in parallel — about one `SUBMIT_DELAY` of wall clock, not two.
///
/// We allow a generous slack on the upper bound because the daemon
/// background work (spawn threads, broadcast push) can add jitter; the
/// purpose is to catch a regression that serializes across panes
/// (e.g. a global writer lock), which would push the total to roughly
/// 2× `SUBMIT_DELAY` and well past the threshold below.
#[tokio::test]
async fn write_to_pane_and_submit_concurrent_writes_to_different_panes_run_in_parallel() {
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
    let write_a = tokio::spawn(async move {
        registry_a
            .write_to_pane_and_submit("orch-pane", "alpha")
            .await
    });
    let write_b = tokio::spawn(async move {
        registry_b
            .write_to_pane_and_submit("coder-pane", "beta")
            .await
    });
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

/// `kill(pid, 0)` is a non-destructive liveness probe: returns 0 if the
/// process exists, `-1`/ESRCH if it has been reaped. Used by the F9
/// tests to confirm the old worker child is actually dead after a
/// respawn, not just removed from the registry. Same shape as
/// `tests/process_group_kill.rs::pid_is_alive`.
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: signal 0 probes existence without delivering anything.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Look up a registry agent id by `pane_id_env`. The F9 respawn path
/// rotates the registry id but keeps the pane_id_env, so tests that
/// want the CURRENT agent's id after a respawn key off pane_id_env
/// (the stable identity the TUI keeps on its pane card).
fn agent_id_for_pane(registry: &AgentPtyRegistry, pane_id_env: &str) -> Option<String> {
    registry
        .agent_records()
        .into_iter()
        .find(|r| r.pane_id_env.as_deref() == Some(pane_id_env))
        .map(|r| r.id)
}

/// Same as [`wait_for_in_snapshot`] but keys by `pane_id_env` and
/// re-resolves the agent id on every poll. Tests that exercise a
/// `clear = true` delegate need this: the F9 respawn rotates the
/// registry agent id between the test's `start_role_pane` call (which
/// captured the pre-respawn id) and the prompt-arrival check. A
/// pane-keyed wait survives the rotation.
async fn wait_for_in_pane_snapshot(
    registry: &AgentPtyRegistry,
    pane_id_env: &str,
    needle: &str,
    timeout: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(agent_id) = agent_id_for_pane(registry, pane_id_env)
            && let Ok(snap) = registry.snapshot(&agent_id)
            && snap.windows(needle.len()).any(|w| w == needle.as_bytes())
        {
            return snap;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let last = agent_id_for_pane(registry, pane_id_env)
        .and_then(|id| registry.snapshot(&id).ok())
        .unwrap_or_default();
    panic!(
        "needle {:?} not found in pane {} scrollback within {:?}; last snapshot: {:?}",
        needle,
        pane_id_env,
        timeout,
        String::from_utf8_lossy(&last)
    );
}

/// PRD #92 F9: `clear = true` (the parse default) means the worker
/// agent's CONTEXT is cleared between delegations, not just its
/// screen. Implemented daemon-side as a kill-the-old-child + spawn-a-
/// fresh-one dance (`AgentPtyRegistry::respawn_agent_for_pane`)
/// before the new task's prompt write. The test pins the contract
/// at four layers:
///
///   1. The old child process is dead after the second delegation
///      (`kill(pid, 0)` returns ESRCH).
///   2. A new child is spawned with a different PID.
///   3. The pane_id_env stays the same across the respawn so the
///      dashboard card / write-to-pane routing keeps working.
///   4. The new agent receives the second delegation's prompt — i.e.
///      the respawn isn't tearing things down without rebuilding.
#[tokio::test]
async fn delegate_respawns_worker_agent_when_role_clear_is_true() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    // No `clear` line means default `true` per
    // `OrchestrationRoleConfig::clear` serde default — pinned by
    // `project_config::tests::orchestration_clear_defaults_to_true`.
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
    let coder_agent_id_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;
    let pid_initial = daemon
        .pty_registry
        .child_pid(&coder_agent_id_initial)
        .expect("freshly-spawned coder agent must expose a pid") as i32;

    // First delegation. With clear=true defaulting on the coder role,
    // the daemon respawns the coder agent before writing the prompt.
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "task ALPHA".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Wait for the second-generation agent to come up. After respawn
    // the new agent id is rotated but the pane_id_env stays the
    // same, so we look up by pane_id_env. The 5 s budget covers
    // SIGTERM grace (up to 3 s in pathological cases) plus the spawn
    // latency; PRD #92 F9 followup-6 made dispatch async, so we also
    // absorb the tokio::spawn boundary between `send_delegate` and
    // the dispatch task starting.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut coder_agent_id_after_first = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Some(id) = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
            && id != coder_agent_id_initial
        {
            coder_agent_id_after_first = id;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        !coder_agent_id_after_first.is_empty(),
        "registry never produced a new agent id for coder-pane after respawn"
    );

    let pid_after_first = daemon
        .pty_registry
        .child_pid(&coder_agent_id_after_first)
        .expect("respawned coder agent must expose a new pid") as i32;
    assert_ne!(
        pid_initial, pid_after_first,
        "respawn must replace the child process; got the same pid on both sides"
    );

    // The original child must be dead. The terminate helper polls
    // try_wait for up to AGENT_TERMINATE_GRACE (3 s) so the kernel
    // has definitively reaped the old child by the time the respawn
    // returns.
    //
    // Theoretical PID-reuse race. After `wait()` reaps the old PID,
    // the kernel could in principle reassign that integer to an
    // unrelated process before `pid_is_alive` polls it, in which case
    // the assertion would observe "alive" and the test would falsely
    // fail. Stable in practice — the test machine's PID space rolls
    // slowly enough that reuse within the same test tick is
    // improbable — but if this ever flakes, switch to capturing the
    // exit status via a child-tracking test hook rather than
    // `kill(pid, 0)`.
    assert!(
        !pid_is_alive(pid_initial),
        "old child pid {pid_initial} is still alive after respawn — \
         the terminate-with-grace helper didn't reach SIGKILL or wait()"
    );

    // The pane_id_env stays. agent_records on the registry must
    // still show exactly one live entry for coder-pane.
    let live_records: Vec<_> = daemon
        .pty_registry
        .agent_records()
        .into_iter()
        .filter(|r| r.pane_id_env.as_deref() == Some("coder-pane"))
        .collect();
    assert_eq!(
        live_records.len(),
        1,
        "after respawn there must be exactly one live agent bound to \
         coder-pane; got {live_records:?}"
    );

    // The first delegation's prompt must land in the NEW agent's
    // scrollback (write_to_pane_and_submit routes by pane_id_env
    // which the respawn preserved). Use the new agent_id for the snapshot.
    //
    // PRD #92 F9 followup-6: the prompt write follows a 10 s
    // `SessionStart` wait that `cat -u` never satisfies, so allow for
    // the fallback path (10 s + spawn / write latency + jitter).
    let _ = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id_after_first,
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(20),
    )
    .await;

    // Fire a second delegation to prove the respawn loop is idempotent
    // (not a one-shot path). Capture another pid roll-over.
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "task BRAVO".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut coder_agent_id_after_second = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Some(id) = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
            && id != coder_agent_id_after_first
        {
            coder_agent_id_after_second = id;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        !coder_agent_id_after_second.is_empty(),
        "second delegation should produce a second respawn — registry agent id never rotated"
    );
    let pid_after_second = daemon
        .pty_registry
        .child_pid(&coder_agent_id_after_second)
        .expect("post-second-respawn agent must expose a pid") as i32;
    assert_ne!(
        pid_after_first, pid_after_second,
        "second delegation must respawn again; pid didn't change"
    );
    assert!(
        !pid_is_alive(pid_after_first),
        "second-respawn must kill the first-respawn child"
    );

    // The second task file content reflects the most-recent delegation.
    let task_file = std::path::Path::new(&cwd).join(".dot-agent-deck/worker-task-coder.md");
    let body = std::fs::read_to_string(&task_file).unwrap();
    assert_eq!(
        body, "task BRAVO",
        "task file should reflect the second delegation's task body"
    );
}

/// PRD #92 F9: the inverse — `clear = false` (the `release` role's
/// opt-out) must NOT respawn the worker agent. The agent process
/// stays alive across delegations and accumulates scrollback /
/// context. Pre-baseline contract for the release role's
/// walkthrough-style flow.
#[tokio::test]
async fn delegate_does_not_respawn_worker_when_role_clear_is_false() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

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
clear = false
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
    let pid_initial = daemon
        .pty_registry
        .child_pid(&coder_agent_id)
        .expect("coder agent must expose a pid") as i32;

    // Fire two delegations back-to-back. With clear=false the daemon
    // should skip the respawn path entirely and just write the prompt
    // into the existing PTY — the agent's pid stays put across both.
    for label in ["task ALPHA", "task BRAVO"] {
        send_delegate(
            &daemon.hook_path,
            &DelegateSignal {
                pane_id: "orch-pane".into(),
                task: label.into(),
                to: vec!["coder".into()],
                timestamp: Utc::now(),
            },
        )
        .await;
        // Wait for each prompt to land before firing the next, so
        // an out-of-order observation doesn't flake the test.
        let _ = wait_for_in_snapshot(
            &daemon.pty_registry,
            &coder_agent_id,
            "Read .dot-agent-deck/worker-task-coder.md",
            Duration::from_secs(5),
        )
        .await;
    }

    // The registry still shows the same agent id — no respawn rotated it.
    let current_id = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
        .expect("coder-pane should still have a live agent");
    assert_eq!(
        current_id, coder_agent_id,
        "clear=false must NOT rotate the registry agent id"
    );

    // The pid is the same — same process, just written to twice.
    let pid_after = daemon
        .pty_registry
        .child_pid(&coder_agent_id)
        .expect("coder agent must still expose a pid") as i32;
    assert_eq!(
        pid_initial, pid_after,
        "clear=false must NOT replace the child process; pid changed"
    );
    assert!(
        pid_is_alive(pid_initial),
        "clear=false worker process should still be alive after delegations"
    );
}

/// PRD #92 F9 bus-rotation sanity check: after a `clear = true`
/// respawn, the new agent gets a fresh broadcast bus that doesn't
/// inherit the previous agent's scrollback. With `cat -u` as the
/// test agent, "memory" is just bytes the old agent echoed back to
/// its stdout — those bytes were captured by the old bus and the
/// new bus is empty. This is a useful regression guard against
/// refactors that would key the bus off `pane_id_env` (which the
/// respawn preserves) instead of the agent process itself.
///
/// The previous name of this test
/// (`delegate_respawn_clears_agent_scrollback_when_role_clear_is_true`)
/// implied it proved an LLM's conversation history clears across
/// `clear = true`, which it does NOT — the `cat -u` stub has no
/// concept of context, and real LLM agents (claude with `--continue`,
/// opencode with session autoload) may reload their previous session
/// on startup, defeating the agent-process-reset semantics of
/// `clear = true`. The audit doc covers that limitation; this test
/// is purely the bus-rotation
/// invariant.
#[tokio::test]
async fn delegate_respawn_rotates_agent_bus_when_role_clear_is_true() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

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
    let coder_agent_id_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    // Seed the old agent's scrollback with a recognizable marker.
    // `cat -u` echoes it back, so it lands in the bus history.
    daemon
        .pty_registry
        .write_to_pane_and_submit("coder-pane", "PRE_RESPAWN_SCROLLBACK_MARKER")
        .await
        .expect("seed write should succeed");
    let _ = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id_initial,
        "PRE_RESPAWN_SCROLLBACK_MARKER",
        Duration::from_secs(5),
    )
    .await;

    // Trigger respawn via a delegation.
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "POST_RESPAWN_TASK".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Wait for the rotated agent id.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut coder_agent_id_after = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Some(id) = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
            && id != coder_agent_id_initial
        {
            coder_agent_id_after = id;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        !coder_agent_id_after.is_empty(),
        "respawn must rotate the agent id"
    );

    // Wait for the new prompt to land so we know the new agent is up
    // and dispatch has flushed.
    //
    // PRD #92 F9 followup-6: the post-respawn write waits up to 10 s
    // for `SessionStart`; `cat -u` never emits one, so allow for the
    // fallback path here.
    let new_snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_agent_id_after,
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(20),
    )
    .await;

    // The new agent's scrollback must NOT contain the pre-respawn
    // marker — its bus was fresh-allocated by the respawn. This is
    // bus-rotation, not LLM-context-clear: the bytes the previous
    // process echoed into the old bus stay there until that
    // RunningAgent is dropped, and the new RunningAgent gets its
    // own empty AgentBus. See the test-level doc comment for why a
    // real LLM agent's behavior is out of scope here.
    assert!(
        !String::from_utf8_lossy(&new_snap).contains("PRE_RESPAWN_SCROLLBACK_MARKER"),
        "fresh agent must not inherit the previous agent's scrollback; \
         got snapshot = {:?}",
        String::from_utf8_lossy(&new_snap)
    );
}

/// Two concurrent `clear = true` delegate
/// signals targeting the same worker pane must both reach the
/// worker. Pre-fix, the hook loop spawned a fresh tokio task per
/// accepted connection and `handle_delegate` only took
/// `state.read()` — so two parallel connections could race the
/// `registry.remove` + `spawn_agent` window inside
/// `respawn_agent_for_pane`: the second call observed `NotFound`,
/// logged a warn, and silently dropped its task prompt.
///
/// The fix adds a per-pane dispatch mutex acquired at the entry of
/// the worker-loop in `handle_delegate`. With the mutex, the two
/// delegates serialize: both respawns succeed sequentially, both
/// task file writes land, and the second delegate's content
/// reaches the final fresh agent's scrollback.
#[tokio::test]
async fn concurrent_clear_true_delegates_both_reach_worker() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

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
    let coder_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;
    let pid_initial = daemon
        .pty_registry
        .child_pid(&coder_initial)
        .expect("freshly-spawned coder has a pid");

    // Fire two delegates in parallel via independent connections.
    // Each `send_delegate` opens its own UnixStream, which is the
    // multi-connection shape the daemon's per-connection task model
    // races on (run_hook_loop spawns a fresh tokio task per accept).
    let hook_a = daemon.hook_path.clone();
    let hook_b = daemon.hook_path.clone();
    let send_a = tokio::spawn(async move {
        send_delegate(
            &hook_a,
            &DelegateSignal {
                pane_id: "orch-pane".into(),
                task: "task ALPHA".into(),
                to: vec!["coder".into()],
                timestamp: Utc::now(),
            },
        )
        .await;
    });
    let send_b = tokio::spawn(async move {
        send_delegate(
            &hook_b,
            &DelegateSignal {
                pane_id: "orch-pane".into(),
                task: "task BRAVO".into(),
                to: vec!["coder".into()],
                timestamp: Utc::now(),
            },
        )
        .await;
    });
    let (a, b) = tokio::join!(send_a, send_b);
    a.unwrap();
    b.unwrap();

    // Both delegates serialize behind the per-pane mutex. The first
    // respawn rolls the PID, the second respawn rolls it again. We
    // assert by waiting for the PID to differ from `pid_initial`
    // AND from the next observed value — i.e. two distinct rolls.
    // Per-roll budget: 30 s each. The actual respawn (terminate
    // grace + spawn) is sub-second on a healthy dev box; the wider
    // budget absorbs CI overload without flaking.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut pid_after_first: u32 = 0;
    while tokio::time::Instant::now() < deadline {
        if let Some(id) = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
            && let Some(pid) = daemon.pty_registry.child_pid(&id)
            && pid != pid_initial
        {
            pid_after_first = pid;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_ne!(
        pid_after_first, 0,
        "first respawn never rolled the coder pid past {pid_initial}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut pid_after_second: u32 = 0;
    while tokio::time::Instant::now() < deadline {
        if let Some(id) = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
            && let Some(pid) = daemon.pty_registry.child_pid(&id)
            && pid != pid_after_first
            && pid != pid_initial
        {
            pid_after_second = pid;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_ne!(
        pid_after_second, 0,
        "second respawn never rolled the coder pid past {pid_after_first} — \
         the second concurrent delegate's respawn was silently dropped (race \
         in handle_delegate, mutex regression?)"
    );

    // Both task file writes ran; the file's content reflects the
    // second-to-run delegate. We don't pin which of A / B that was
    // — connection scheduling is arbitrary. The point is that
    // BOTH bodies were written at some point; the surviving content
    // is the one written last.
    let task_file = std::path::Path::new(&cwd).join(".dot-agent-deck/worker-task-coder.md");
    let body = std::fs::read_to_string(&task_file).unwrap();
    assert!(
        body == "task ALPHA" || body == "task BRAVO",
        "task file must contain one of the two delegated bodies; got: {body}"
    );

    // The post-second-respawn agent's scrollback must contain the
    // worker-task one-liner — proving the second delegate's prompt
    // write actually reached the new agent (the bug pre-fix was
    // that the second delegate's write fell through with NotFound).
    //
    // PRD #92 F9 followup-6: dispatch is now async and the prompt
    // write waits up to 10 s for `SessionStart` on each respawn. Two
    // serialized clear=true delegates against `cat -u` therefore land
    // through the fallback path twice, so the wait budget must cover
    // 2 × 10 s plus the spawn / write latency. The exact landing time
    // of the second prompt is the metric under test.
    let final_id =
        agent_id_for_pane(&daemon.pty_registry, "coder-pane").expect("coder pane has live agent");
    let _ = wait_for_in_snapshot(
        &daemon.pty_registry,
        &final_id,
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(30),
    )
    .await;
}

/// PRD #92 F9 followup-6: post-respawn dispatch is event-driven —
/// the daemon defers the task-prompt write until the freshly-spawned
/// agent emits a `SessionStart` hook event (10 s timeout fallback).
/// This test pins the fast path: once `SessionStart` arrives, the
/// prompt write happens promptly (well under the 10 s fallback).
///
/// A regression that re-introduces a fixed delay, or that wires the
/// dispatch task to a stale receiver that misses the event, would
/// have the prompt stuck behind the 10 s wait — easy to spot below.
///
/// The race avoidance under test: the dispatch task must subscribe
/// to the hook-event broadcast BEFORE the respawn forks the new
/// process. Otherwise a SessionStart sent immediately after respawn
/// could land before the receiver attaches and the task would still
/// hit the 10 s fallback. We drive that by waiting for the new
/// agent_id to roll (which proves the respawn returned, which
/// proves subscribe-before-spawn already ran) and only THEN
/// injecting SessionStart.
#[tokio::test]
async fn delegate_clear_true_writes_prompt_promptly_after_session_start() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

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
    let coder_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "fast-path task".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Wait for the respawn to have happened. The dispatch task
    // subscribed BEFORE this respawn returned, so injecting
    // SessionStart any time after this point is safe.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match agent_id_for_pane(&daemon.pty_registry, "coder-pane") {
            Some(id) if id != coder_initial => break,
            _ => tokio::time::sleep(Duration::from_millis(15)).await,
        }
    }
    let coder_new = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
        .expect("coder pane has live agent after respawn");
    assert_ne!(
        coder_new, coder_initial,
        "respawn never rotated the agent id within 5 s"
    );

    // Forge a SessionStart for the coder pane scoped to the NEW
    // agent's id (followup-7: the dispatch task's wait filter is
    // `(pane_id, agent_id)`, so a `None` agent_id no longer matches).
    // The dispatch task is currently sleeping inside
    // `wait_for_session_start` against the daemon-wide hook broadcast;
    // this event lands on every live receiver, including that task's,
    // and unblocks the prompt write.
    let send_at = tokio::time::Instant::now();
    send_session_start(&daemon.hook_path, "coder-pane", Some(&coder_new)).await;

    // The prompt should land promptly — well under the 10 s fallback.
    // 3 s tolerates fs + PTY plumbing on a loaded CI box but stays
    // sharply below the 10 s timeout, so a regression to the fallback
    // path is unambiguous.
    let _ = wait_for_in_pane_snapshot(
        &daemon.pty_registry,
        "coder-pane",
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(3),
    )
    .await;
    let elapsed = send_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "prompt landed at {elapsed:?} after SessionStart — \
         expected the event-driven fast path (< 5 s) but the wait \
         appears to be falling through the 10 s SessionStart timeout"
    );
}

/// PRD #92 F9 followup-7: the post-respawn dispatch task's
/// `wait_for_session_start` filter must reject `SessionStart` events
/// whose `agent_id` matches the OLD (pre-respawn) agent. Without
/// this filter, a late `SessionStart` from the OLD agent emitted
/// within the subscribe→kill window would unblock the wait and the
/// dispatch would write the prompt while the NEW agent is still
/// booting — exactly the bug followup-6's broadcast filter (pane_id
/// only) couldn't see.
///
/// Construction: capture OLD agent_id before delegate, send delegate,
/// poll for respawn completion (rotates agent_id), capture NEW
/// agent_id. Then:
///   1. Forge a SessionStart with the OLD agent_id. The dispatch
///      task's receiver receives it; with the followup-7 filter the
///      event is rejected and the wait keeps blocking — the prompt
///      must NOT appear in the pane scrollback.
///   2. Forge a SessionStart with the NEW agent_id. The wait
///      unblocks and the prompt IS written.
///
/// The OLD event in step 1 reproduces reviewer #5's concern that the
/// existing followup-6 test claimed to exercise this race but did
/// not — it only sent a single SessionStart with no agent_id, which
/// happened to unblock the wait regardless of whether the filter
/// scoped by agent_id.
#[tokio::test]
async fn delegate_clear_true_rejects_session_start_from_old_agent_id() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

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
    let coder_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "race-ordering task".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Wait for respawn to rotate the agent id — proves the dispatch
    // task is past respawn and now blocked inside wait_for_session_start.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match agent_id_for_pane(&daemon.pty_registry, "coder-pane") {
            Some(id) if id != coder_initial => break,
            _ => tokio::time::sleep(Duration::from_millis(15)).await,
        }
    }
    let coder_new = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
        .expect("coder pane has live agent after respawn");
    assert_ne!(
        coder_new, coder_initial,
        "respawn never rotated the agent id within 5 s"
    );

    // Step 1: forge a SessionStart with the OLD agent_id. Followup-7's
    // filter requires `agent_id == NEW`, so this event must be
    // rejected and the prompt must NOT yet appear.
    send_session_start(&daemon.hook_path, "coder-pane", Some(&coder_initial)).await;

    // Give the daemon enough time to deliver-and-reject the OLD event.
    // 300ms is generous: broadcast::Sender::send is sync-immediate and
    // the dispatch task's receiver loop is just a serde decode plus the
    // three-line filter. If the filter ever erroneously accepted the
    // OLD event the prompt write would already be queued; the
    // wait_for_in_pane_snapshot below would observe it well under our
    // 1 s budget. Conversely if rejection works correctly, scrollback
    // stays empty during this window.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap_after_old = daemon
        .pty_registry
        .snapshot(&coder_new)
        .expect("snapshot of NEW coder agent");
    let prompt_marker = "Read .dot-agent-deck/worker-task-coder.md";
    assert!(
        !snap_after_old
            .windows(prompt_marker.len())
            .any(|w| w == prompt_marker.as_bytes()),
        "OLD-agent SessionStart must NOT unblock the dispatch wait; \
         snapshot at +300ms = {:?}",
        String::from_utf8_lossy(&snap_after_old)
    );

    // Step 2: forge a SessionStart with the NEW agent_id. Filter
    // matches, wait unblocks, prompt is written into the new agent's
    // PTY scrollback.
    send_session_start(&daemon.hook_path, "coder-pane", Some(&coder_new)).await;

    let _ = wait_for_in_pane_snapshot(
        &daemon.pty_registry,
        "coder-pane",
        prompt_marker,
        Duration::from_secs(3),
    )
    .await;
}

/// When a `clear = true` respawn fails
/// AFTER the terminate phase already disposed of the previous
/// child (the most-likely cause: a role config whose `command`
/// no longer resolves), the operator must see a visible error in
/// the orchestrator pane scrollback — not just two log lines
/// somewhere off-screen.
///
/// We stub the failure by writing a role config whose `coder`
/// command points at a non-existent path. The initial coder pane
/// is spawned via the test harness with `cat -u`, so it comes up
/// successfully; the first delegate then triggers a respawn whose
/// `spawn_agent` step fails because the new command can't be
/// exec'd. The error must surface in the orchestrator's scrollback.
#[tokio::test]
async fn respawn_failure_surfaces_visible_error_in_orchestrator_pane() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    // The coder role's command points at a path that doesn't exist
    // on disk — `portable_pty::spawn_command` returns Err on
    // `execvp`, which propagates as `AgentPtyError::Spawn` out of
    // `respawn_agent_for_pane`.
    let config_toml = r#"
[[orchestrations]]
name = "tdd-cycle"

[[orchestrations.roles]]
name = "orchestrator"
command = "cat -u"
start = true

[[orchestrations.roles]]
name = "coder"
command = "/nonexistent/dot-agent-deck-test-bin-12345"
"#;
    std::fs::write(
        std::path::Path::new(&cwd).join(".dot-agent-deck.toml"),
        config_toml,
    )
    .unwrap();

    let orch_id = start_role_pane(
        &daemon,
        "tdd-cycle",
        "orchestrator",
        true,
        0,
        "orch-pane",
        &cwd,
    )
    .await;
    // Initial coder still spawns successfully (test harness uses
    // `cat -u`, not the role config's bogus command).
    let _coder_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "task that will trigger a doomed respawn".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // The orchestrator pane's scrollback must contain the visible
    // notice. `cat -u` echoes whatever lands on its stdin back to
    // stdout, so the notice arrives in the bus via the standard
    // echo path.
    let snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &orch_id,
        "respawn failed for role 'coder'",
        Duration::from_secs(10),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&snap).contains("respawn failed for role 'coder'"),
        "orchestrator pane scrollback must include the respawn-failure notice; \
         snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );
}

/// A single delegate that fans out to N workers must respawn them
/// concurrently, not sequentially. Pre-F9-followup-2, the per-target
/// loop in `handle_delegate` was `.await`-sequential — an N-worker
/// fan-out paid `(respawn + ready-wait) × N` wall-clock. The fix
/// gives every target its own `tokio::spawn` (PRD #92 F9 followup-6)
/// so different panes' respawn+wait windows overlap. Per-pane work
/// still serializes against itself via the per-pane dispatch mutex
/// (see `concurrent_clear_true_delegates_both_reach_worker`).
///
/// The wall-clock signal is end-to-end "time until the prompt
/// landed in every worker's new bus" rather than just "PID rolled":
/// the PID rolls early in dispatch (inside `respawn_agent_for_pane`),
/// so a PID-roll race is too noisy to bound usefully on a fast
/// machine. Each worker's full per-pane cost is therefore
/// `respawn + SessionStart wait + prompt write`. With `cat -u`
/// agents that never emit `SessionStart`, that wait always lands on
/// the [`crate::state::SESSION_START_WAIT_TIMEOUT`] (10 s) fallback,
/// so a 3-pane sequential regression would pay ~30 s while concurrent
/// dispatch completes in roughly one pane's worth of that. We
/// measure the 1-pane baseline first to set the upper bound;
/// concurrent 3-pane must finish within 1.5 × baseline. A
/// sequential regression (3 × baseline) is well outside that bound.
#[tokio::test]
async fn concurrent_fan_out_respawns_overlap_across_panes() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

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

[[orchestrations.roles]]
name = "reviewer"
command = "cat -u"

[[orchestrations.roles]]
name = "auditor"
command = "cat -u"
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

    // Helper: wait until the agent currently bound to `pane` has the
    // worker-task one-liner in its bus snapshot, then return the
    // elapsed time. Resolves the agent id post-respawn (the registry
    // id rolls; pane_id_env is stable) so we observe the *new*
    // agent's bus, not the dead one's.
    async fn snapshot_contains_prompt(
        registry: Arc<AgentPtyRegistry>,
        pane: &'static str,
        from: tokio::time::Instant,
        timeout: Duration,
    ) -> Option<Duration> {
        let deadline = from + timeout;
        let needle = b"Read .dot-agent-deck/worker-task-";
        loop {
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            if let Some(id) = agent_id_for_pane(&registry, pane)
                && let Ok(snap) = registry.snapshot(&id)
                && snap.windows(needle.len()).any(|w| w == needle)
            {
                return Some(from.elapsed());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // Baseline: single-worker delegate. Spawn one worker, fire one
    // delegate, time until the post-respawn prompt is visible.
    let _coder_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    let single_start = tokio::time::Instant::now();
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "baseline".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;
    let single_elapsed = snapshot_contains_prompt(
        daemon.pty_registry.clone(),
        "coder-pane",
        single_start,
        Duration::from_secs(20),
    )
    .await
    .expect("baseline single-pane respawn never produced a visible prompt");

    // Spawn the other two role panes for the 3-way fan-out.
    let _reviewer_initial = start_role_pane(
        &daemon,
        "tdd-cycle",
        "reviewer",
        false,
        2,
        "reviewer-pane",
        &cwd,
    )
    .await;
    let _auditor_initial = start_role_pane(
        &daemon,
        "tdd-cycle",
        "auditor",
        false,
        3,
        "auditor-pane",
        &cwd,
    )
    .await;

    // Wait until each worker's bus is back to a quiet steady-state
    // (no in-flight respawn) before timing the fan-out. The baseline
    // delegate above already finished — its prompt is visible — but
    // we still want a clean t=0 reference for the fan-out.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fan-out: one delegate targeting all three roles.
    let fanout_start = tokio::time::Instant::now();
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "fanout".into(),
            to: vec!["coder".into(), "reviewer".into(), "auditor".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Wait for the prompt to land in all three workers' new buses
    // concurrently. `join_all` over the per-pane waits matches the
    // dispatch shape under test: all three completion times are
    // observed in parallel, so the longest one dominates.
    let panes: [&'static str; 3] = ["coder-pane", "reviewer-pane", "auditor-pane"];
    let waits = panes.iter().map(|pane| {
        snapshot_contains_prompt(
            daemon.pty_registry.clone(),
            pane,
            fanout_start,
            Duration::from_secs(20),
        )
    });
    let elapsed_per_pane: Vec<Option<Duration>> = futures_util::future::join_all(waits).await;
    let fanout_elapsed = fanout_start.elapsed();
    for (pane, el) in panes.iter().zip(elapsed_per_pane.iter()) {
        assert!(
            el.is_some(),
            "fan-out: {pane} prompt never became visible within {fanout_elapsed:?}"
        );
    }

    // Concurrent 3-pane fan-out completes in ~1 × the single-pane
    // baseline (each pane's SessionStart wait + write overlaps).
    // Sequential 3-pane would be ~3 × baseline. 1.5 × is a tight
    // bound that catches a regression to sequential dispatch while
    // tolerating some CI scheduling jitter.
    let upper_bound = single_elapsed.mul_f32(1.5);
    assert!(
        fanout_elapsed < upper_bound,
        "fan-out wall-clock {fanout_elapsed:?} >= 1.5 × single-pane baseline \
         {single_elapsed:?} (upper bound {upper_bound:?}) — \
         per-target loop reverted to sequential dispatch?"
    );
}

/// A `clear = true` respawn must preserve
/// the full env vec that was passed to the original `spawn_agent`,
/// not just `DOT_AGENT_DECK_PANE_ID`. Without the capture, any
/// role-supplied extra env var (or anything the orchestrator
/// passed through at StartAgent time) silently disappeared on
/// respawn — the new agent ran with a leaner env than the
/// original.
///
/// The test agent is `sh -c 'echo MARKER=$MY_RESPAWN_VAR; cat -u'`
/// — on each spawn it prints the value of `MY_RESPAWN_VAR` and
/// then becomes a passthrough echo (so the rest of the dispatch
/// flow still works). We pass `MY_RESPAWN_VAR=preserve-me-please`
/// at the initial spawn; after the respawn, the new agent's
/// scrollback must show `MARKER=preserve-me-please`.
#[tokio::test]
async fn respawn_preserves_original_spawn_env() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    let marker_value = "preserve-me-please";
    let role_command = "sh -c 'echo MARKER=$MY_RESPAWN_VAR; exec cat -u'";

    // Role config: coder's command is the env-printing shell. The
    // initial spawn uses the same command, so the marker appears on
    // both generations.
    let config_toml = format!(
        r#"
[[orchestrations]]
name = "tdd-cycle"

[[orchestrations.roles]]
name = "orchestrator"
command = "cat -u"
start = true

[[orchestrations.roles]]
name = "coder"
command = "{role_command}"
"#
    );
    std::fs::write(
        std::path::Path::new(&cwd).join(".dot-agent-deck.toml"),
        config_toml,
    )
    .unwrap();

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
    // Spawn the coder directly via DaemonClient so we can inject
    // the extra env var. `start_role_pane` only passes
    // DOT_AGENT_DECK_PANE_ID; we want both.
    let client = DaemonClient::new(daemon.attach_path.clone());
    let coder_initial = client
        .start_agent(StartAgentOptions {
            command: Some(role_command.to_string()),
            cwd: Some(cwd.clone()),
            display_name: Some("coder".to_string()),
            rows: 24,
            cols: 80,
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), "coder-pane".to_string()),
                ("MY_RESPAWN_VAR".to_string(), marker_value.to_string()),
            ],
            tab_membership: Some(TabMembership::Orchestration {
                name: "tdd-cycle".to_string(),
                role_index: 1,
                role_name: "coder".to_string(),
                is_start_role: false,
                orchestration_cwd: Some(cwd.clone()),
            }),
            agent_type: None,
        })
        .await
        .expect("start_agent for env-capture coder");

    // Initial spawn observed the marker.
    let _ = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_initial,
        &format!("MARKER={marker_value}"),
        Duration::from_secs(5),
    )
    .await;

    // Delegate triggers a respawn; the new agent must see the same
    // marker — i.e. the captured env was replayed.
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "task".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut coder_after = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Some(id) = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
            && id != coder_initial
        {
            coder_after = id;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        !coder_after.is_empty(),
        "respawn never rotated the agent id"
    );

    let snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_after,
        &format!("MARKER={marker_value}"),
        Duration::from_secs(5),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&snap).contains(&format!("MARKER={marker_value}")),
        "respawned agent must see the same env as the initial spawn; \
         snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );
}

/// A `clear = true` respawn must replay
/// the last-known PTY size, not reset to the 24×80 default.
/// Without the capture, the new agent's first output briefly
/// wrapped or truncated until the TUI's next resize call landed.
#[tokio::test]
async fn respawn_preserves_last_known_pty_size() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

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
"#;
    std::fs::write(
        std::path::Path::new(&cwd).join(".dot-agent-deck.toml"),
        config_toml,
    )
    .unwrap();

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
    let coder_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;

    // Resize the coder pane to a non-default geometry.
    daemon
        .pty_registry
        .resize(&coder_initial, 40, 100)
        .expect("resize should succeed");
    assert_eq!(
        daemon.pty_registry.pty_size_for_pane("coder-pane"),
        Some((40, 100)),
        "resize should update the captured size"
    );

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "task that triggers respawn".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Wait for the respawn to land.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut coder_after = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Some(id) = agent_id_for_pane(&daemon.pty_registry, "coder-pane")
            && id != coder_initial
        {
            coder_after = id;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        !coder_after.is_empty(),
        "respawn never rotated the agent id"
    );

    // The new agent must come up at the captured size, not 24×80.
    assert_eq!(
        daemon.pty_registry.pty_size_for_pane("coder-pane"),
        Some((40, 100)),
        "respawned PTY must come up at the captured size (40×100), not the default"
    );
}

/// When the role config lookup returns
/// None for a `clear = true` delegate target (typically because
/// the user edited `.dot-agent-deck.toml` mid-orchestration and
/// the role name diverged), the operator's intended respawn is
/// silently dropped. The fix is observability: a `tracing::warn!`
/// so the cause is at least discoverable in the daemon log. This
/// test exercises the BEHAVIOR (no respawn happens, prompt still
/// lands) — the warn itself is covered by `cargo test -- --nocapture`
/// inspection or a tracing-test layer in a future suite, since
/// adding a tracing-test dev-dep just for this test would
/// overweight the dependency footprint.
#[tokio::test]
async fn delegate_with_missing_role_config_skips_respawn_and_still_delivers_prompt() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    // Config: only the orchestrator role is listed. The coder pane
    // is spawned manually via the test harness (which doesn't
    // require a config), so pane_role_map has "coder" but
    // lookup_orchestration_role("coder") returns None.
    let config_toml = r#"
[[orchestrations]]
name = "tdd-cycle"

[[orchestrations.roles]]
name = "orchestrator"
command = "cat -u"
start = true
"#;
    std::fs::write(
        std::path::Path::new(&cwd).join(".dot-agent-deck.toml"),
        config_toml,
    )
    .unwrap();

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
    let coder_initial =
        start_role_pane(&daemon, "tdd-cycle", "coder", false, 1, "coder-pane", &cwd).await;
    let pid_initial = daemon
        .pty_registry
        .child_pid(&coder_initial)
        .expect("coder has a pid");

    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "task with missing role config".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // Prompt still lands — the no-respawn path runs
    // write_to_pane_and_submit against the existing (unchanged) coder agent.
    let _ = wait_for_in_snapshot(
        &daemon.pty_registry,
        &coder_initial,
        "Read .dot-agent-deck/worker-task-coder.md",
        Duration::from_secs(5),
    )
    .await;

    // No respawn happened — the PID is unchanged.
    let pid_after = daemon
        .pty_registry
        .child_pid(&coder_initial)
        .expect("coder still alive");
    assert_eq!(
        pid_initial, pid_after,
        "missing role_config must NOT respawn (the warn fired and we fell through)"
    );
}

/// PRD #92 F12 followup-3 — end-to-end regression for the F12 reattach
/// budget. The unit-level F12 tests in
/// `tests/pane_auto_renew_on_respawn.rs` exercise the controller's
/// `resolve_and_reattach` loop in isolation: they drive
/// [`AgentPtyRegistry::close_agent`] + [`AgentPtyRegistry::spawn_agent`]
/// directly, with a tens-of-millisecond gap between close and spawn.
/// That window fits inside even a regressed retry budget, so the unit
/// tests passed at tip `7c6356f` (200 ms F12 budget) — the bug was
/// invisible until the production respawn path with a SIGTERM-trapping
/// worker manifested it manually.
///
/// This test exercises the real path:
///   - the daemon's [`AppState::handle_delegate`] dispatches against
///     a `clear = true` role, triggering
///     [`AgentPtyRegistry::respawn_agent_for_pane`];
///   - `respawn_agent_for_pane` drops the master immediately (firing
///     STREAM_END to the attached [`EmbeddedPaneController`]) and only
///     THEN waits inside `terminate_child_with_grace_and_wait` for the
///     SIGTERM-trapping worker to finish its 2 s `sleep` before
///     spawning the replacement;
///   - the controller's io_task therefore observes STREAM_END ~2 s
///     before the new agent is registered, and its
///     `resolve_and_reattach` budget must be wide enough to cover
///     that gap.
///
/// ## Worker recipe
///
/// `sh -c "trap 'sleep 2; exit' TERM; echo OLD_BANNER; sleep 30"`
///
/// - The `trap` clause forces the SIGTERM-to-exit gap to ~2 s, which
///   is what stretches `respawn_agent_for_pane` past the regressed F12
///   budget.
/// - `echo OLD_BANNER` proves the OLD subscription is alive before we
///   trigger the respawn (so the failure can only be "lost the NEW
///   subscription", not "never had one in the first place").
/// - `sleep 30` keeps the OLD shell alive past the daemon's master
///   drop. Earlier sketches used `exec cat -u` here, but `exec`
///   replaces the shell process — and once the shell is gone the
///   SIGTERM trap is gone too, so the respawn gap collapses to ~ms
///   and the bug becomes invisible. A bare `sleep` doesn't read
///   stdin (immune to master close) and the shell stays alive to
///   run the trap when SIGTERM arrives via `killpg`.
/// - The daemon's post-respawn prompt write reaches the controller's
///   vt100 parser via PTY ECHO: bytes written to the master are
///   echoed back on the master's read side by the kernel's TTY
///   driver, so no in-pane reader (e.g. `cat -u`) is required.
///
/// ## Expected failure mode if the budget regresses
///
/// At tip `7c6356f` (`REATTACH_LOOKUP_TOTAL_BUDGET = 200 ms`):
///   1. `respawn_agent_for_pane` drops the master at t≈0.
///   2. The controller's io_task observes Closed and enters
///      `resolve_and_reattach`, which polls `list_agents` ~3 times
///      over ~300 ms and gives up — the NEW agent does not register
///      until terminate_with_grace returns (~2 s in).
///   3. The io_task exits. The pane's vt100 parser has no live
///      subscription.
///   4. The daemon completes the respawn, then `write_to_pane_and_submit`
///      flushes the worker-task one-liner into the NEW agent's PTY.
///      Cat echoes it; the bytes reach the daemon's per-agent bus, but
///      nothing is subscribed on the controller side. The parser's
///      visible screen never contains the one-liner.
///   5. The polling assertion below times out with `saw_prompt = false`.
///
/// At tip `96762b3` (10 s exponential-backoff budget):
///   - The reattach loop polls until ~2 s in, finds the NEW agent,
///     re-attaches, and the new bytes flow into the vt100 parser
///     within the assertion window.
///
/// ## Marker strategy
///
/// The daemon's prompt write produces a fixed one-liner
/// (`Read .dot-agent-deck/worker-task-coder.md for your task.`) — the
/// caller-supplied task body is written to the per-role `.md` file,
/// NOT to the worker's stdin, so DelegateSignal.task itself never
/// reaches the PTY scrollback. The one-liner is therefore the marker
/// that proves the post-respawn bytes were observed: it does not
/// appear in the screen until the daemon's respawn + reattach cycle
/// completes, so its presence is unambiguous evidence that the F12
/// reattach won the race.
///
/// ## Wall-clock budget
///
/// - ~2 s for the SIGTERM trap.
/// - ~ms for the fresh PTY spawn.
/// - up to 10 s for the daemon's `SessionStart` fallback wait (this
///   test's `cat -u` worker never emits one, so the prompt write
///   always lands on the timeout path — same as
///   `delegate_respawns_worker_agent_when_role_clear_is_true`).
///
/// Total: ~12–13 s on a quiet box. The 30 s assertion budget below
/// absorbs CI jitter without letting a true regression slip through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f12_e2e_pane_renders_new_agent_after_sigterm_trap_respawn() {
    let daemon = spawn_daemon().await;
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_string_lossy().into_owned();

    // SIGTERM-trapping shell. The diagnostic recipe used
    // `exec cat -u` here, but `exec` replaces the shell — the trap
    // belongs to the shell, so after exec there's no trap left to
    // delay anything. We instead run a long `sleep` and rely on the
    // PTY's ECHO behavior to surface the daemon's prompt write in
    // the vt100 parser (no stdin reader is required: bytes written
    // to the master are echoed back on the master's read side, and
    // the per-agent broadcast forwards them to the controller).
    //
    // Shape: shell sets the trap, echoes OLD_BANNER, then forks
    // `sleep 30` and waits. When the daemon's respawn:
    //   1. drops the master — the PTY closes from the daemon side,
    //      but `sleep` doesn't read stdin so it's unaffected;
    //   2. invokes `terminate_child_with_grace_and_wait`, which
    //      `killpg`s SIGTERM to the shell's process group. `sleep`
    //      receives SIGTERM and dies (no trap); the shell receives
    //      SIGTERM, returns from `wait`, and runs its trap
    //      (`sleep 2; exit`). 2 s later the shell exits and the
    //      grace helper's `try_wait` returns.
    //
    // The OLD agent's exit therefore takes ~2 s after the master
    // drop — exactly the gap F12's reattach budget must cover.
    //
    // TOML quoting: the value is a basic string, so embedded `"` are
    // escaped with `\"`. Single quotes inside the shell argument
    // group the trap action without needing further escapes.
    let sigterm_trap_shell = r#"sh -c "trap 'sleep 2; exit' TERM; echo OLD_BANNER; sleep 30""#;
    let sigterm_trap_shell_toml = sigterm_trap_shell.replace('"', "\\\"");

    let config_toml = format!(
        r#"
[[orchestrations]]
name = "tdd-cycle"

[[orchestrations.roles]]
name = "orchestrator"
command = "cat -u"
start = true

[[orchestrations.roles]]
name = "coder"
command = "{sigterm_trap_shell_toml}"
"#
    );
    std::fs::write(
        std::path::Path::new(&cwd).join(".dot-agent-deck.toml"),
        config_toml,
    )
    .unwrap();

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

    // The TUI-side controller owns the subscription whose loss is the
    // F12 bug. Point it at the daemon's already-bound attach socket
    // (no second listener) and create the coder pane through it so
    // the StartAgent → attach handshake mirrors the production path.
    let ctrl = Arc::new(EmbeddedPaneController::new(
        daemon.attach_path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let coder_pane_id = {
        let ctrl = ctrl.clone();
        let cwd_for_create = cwd.clone();
        let shell = sigterm_trap_shell.to_string();
        tokio::task::spawn_blocking(move || {
            let opts = AgentSpawnOptions {
                display_name: Some("coder"),
                tab_membership: Some(TabMembership::Orchestration {
                    name: "tdd-cycle".to_string(),
                    role_index: 1,
                    role_name: "coder".to_string(),
                    is_start_role: false,
                    orchestration_cwd: Some(cwd_for_create.clone()),
                }),
                rows: 24,
                cols: 80,
                agent_type: None,
            };
            ctrl.create_pane_with_options(Some(&shell), Some(&cwd_for_create), opts)
                .expect("create_pane_with_options should succeed")
                .0
        })
        .await
        .expect("spawn_blocking for create_pane")
    };

    // Helper: check the controller's vt100 parser for `needle`.
    // Visible-screen contents only — same surface
    // `tests/pane_auto_renew_on_respawn.rs::screen_contains` uses.
    fn screen_contains(ctrl: &EmbeddedPaneController, pane_id: &str, needle: &str) -> bool {
        let Some(screen) = ctrl.get_screen(pane_id) else {
            return false;
        };
        let parser = screen.lock().unwrap();
        parser.screen().contents().contains(needle)
    }

    async fn wait_screen<F>(timeout: Duration, mut pred: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        pred()
    }

    // Baseline: the OLD subscription must be alive before we trigger
    // the respawn. Without this assertion a "saw_prompt = false"
    // failure later could mean either "lost the NEW subscription"
    // (the F12 bug) or "never had the OLD subscription"
    // (an unrelated wiring break).
    let ctrl_for_wait = ctrl.clone();
    let pane_for_wait = coder_pane_id.clone();
    let saw_old = wait_screen(Duration::from_secs(5), move || {
        screen_contains(&ctrl_for_wait, &pane_for_wait, "OLD_BANNER")
    })
    .await;
    assert!(
        saw_old,
        "OLD agent banner never reached the vt100 parser — the controller's \
         initial subscription is broken (this is a precondition for the F12 \
         assertion below)"
    );

    // Fire one delegate. The coder role defaults to `clear = true`,
    // so the daemon respawns the worker before writing the prompt.
    // With the SIGTERM-trapping shell the respawn opens a ~2 s gap
    // between master-drop (STREAM_END) and the new agent's
    // registration — exactly the gap the F12 budget must absorb.
    send_delegate(
        &daemon.hook_path,
        &DelegateSignal {
            pane_id: "orch-pane".into(),
            task: "F12 e2e regression task body".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        },
    )
    .await;

    // The prompt one-liner is the marker of "post-respawn bytes
    // reached the vt100 parser". At tip 7c6356f the controller's
    // io_task gives up inside the 200 ms budget and never re-attaches
    // — `cat -u`'s echo of the prompt reaches the daemon's per-agent
    // bus but has no subscriber on the controller side, and this
    // assertion times out.
    let ctrl_for_wait = ctrl.clone();
    let pane_for_wait = coder_pane_id.clone();
    let saw_prompt = wait_screen(Duration::from_secs(30), move || {
        screen_contains(
            &ctrl_for_wait,
            &pane_for_wait,
            "Read .dot-agent-deck/worker-task-coder.md",
        )
    })
    .await;
    assert!(
        saw_prompt,
        "post-respawn prompt one-liner never reached the vt100 parser — \
         F12 reattach budget couldn't absorb the SIGTERM-trap respawn gap; \
         see test doc comment for the failure mode at tip 7c6356f"
    );

    drop(ctrl);
}
