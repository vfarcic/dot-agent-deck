//! Regression guard for #187 / PR #188: a delegated worker pane must
//! receive a SINGLE-LINE prompt, and the `work-done` completion footer
//! must live in the worker task FILE rather than in the injected prompt.
//!
//! Why this matters: `encode_pane_payload` wraps any payload containing a
//! newline in bracketed-paste markers (`ESC[200~ … ESC[201~`). In Claude
//! Code that framing lands as a compacted block the worker never submits
//! without a manual Enter (#187). The fix keeps the injected delegate
//! prompt to one line — the single-line pointer at
//! `.dot-agent-deck/worker-task-<role>.md` — and moves the footer into the
//! task file.
//!
//! Unit tests already cover `compose_delegate_prompt` (single-line) and
//! `encode_pane_payload` (single-line → no wrap) in isolation. This test
//! exercises the REAL daemon dispatch wiring end to end — `handle_delegate`
//! → `dispatch_one_owned` → `compose_delegate_prompt` →
//! `write_to_pane_and_submit` — and asserts the bytes that actually reach a
//! worker pane's PTY plus the contents of the generated task file.
//!
//! No LLM and no real agent: the worker pane is a `cat` stub whose PTY
//! echoes whatever the daemon injects, so the snapshot reflects the
//! delivered bytes. Runs in the fast tier (no `e2e` feature gate).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{BroadcastMsg, DelegateSignal};
use dot_agent_deck::state::AppState;

mod common;

const ORCH_PANE: &str = "orchestrator-pane";
const WORKER_PANE: &str = "worker-pane";
const WORKER_ROLE: &str = "coder";

/// Poll the agent's PTY snapshot until `needle` appears or `timeout`
/// elapses, returning the final snapshot either way so the caller can
/// assert (and print it on failure).
async fn wait_for_snapshot_needle(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &[u8],
    timeout: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(snap) = registry.snapshot(agent_id)
            && snap.windows(needle.len()).any(|w| w == needle)
        {
            return snap;
        }
        if tokio::time::Instant::now() >= deadline {
            return registry.snapshot(agent_id).unwrap_or_default();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Scenario: Register a worker pane (a `cat` stub) and an orchestrator
/// pane in the same orchestration directly in `AppState`, exactly as a
/// real orchestration tab would at StartAgent time, then call the daemon's
/// real `handle_delegate` for a `coder` task. Assert the worker pane's PTY
/// received the single-line file pointer and NOT the multi-line
/// `## When done` footer, and that the generated
/// `.dot-agent-deck/worker-task-coder.md` carries the footer plus the task
/// body. This is the wiring guard for #187: the footer lives in the file,
/// the injected pane prompt stays one line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegate_injects_single_line_pointer_and_keeps_footer_in_task_file() {
    common::init_test_env();

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("tempdir path is UTF-8")
        .to_string();

    let registry = Arc::new(AgentPtyRegistry::new());

    // Worker pane backed by `cat`: the PTY echoes whatever the daemon
    // injects, so the registry snapshot reflects the delivered bytes.
    let worker_agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(cwd_str.as_str()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn worker stub");

    let (event_tx, _event_rx) = broadcast::channel::<BroadcastMsg>(64);

    // Populate the maps `handle_delegate` reads, mirroring what the
    // StartAgent path records for a live orchestration tab: an
    // orchestrator pane (the only valid delegate source) and a worker
    // pane in the SAME orchestration.
    let orchestration = ("test-orchestration".to_string(), cwd_str.clone());
    let mut state = AppState::default();
    state
        .pane_role_map
        .insert(ORCH_PANE.to_string(), "orchestrator".to_string());
    state
        .pane_role_map
        .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
    state.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
    state
        .pane_orchestration_map
        .insert(ORCH_PANE.to_string(), orchestration.clone());
    state
        .pane_orchestration_map
        .insert(WORKER_PANE.to_string(), orchestration.clone());
    state
        .pane_cwd_map
        .insert(WORKER_PANE.to_string(), cwd_str.clone());

    let task = "List the files in the current directory.";
    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: task.to_string(),
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };

    // `handle_delegate` fans the dispatch out onto a `tokio::spawn`d task
    // and returns immediately; we poll its observable effects below.
    state.handle_delegate(signal, &registry, &event_tx).await;

    // 1) The injected pane prompt must be the single-line file pointer.
    let pointer = b"Read .dot-agent-deck/worker-task-coder.md for your task.";
    let snap =
        wait_for_snapshot_needle(&registry, &worker_agent_id, pointer, Duration::from_secs(5))
            .await;
    let snap_str = String::from_utf8_lossy(&snap);
    assert!(
        snap.windows(pointer.len()).any(|w| w == pointer),
        "worker pane never received the single-line file pointer; snapshot = {snap_str:?}"
    );

    // 2) The footer must NOT have been injected into the pane. Pre-#187
    //    the prompt carried the multi-line `## When done` block, which is
    //    exactly what forced the bracketed-paste path. `## When done` is
    //    plain ASCII, so PTY echo would surface it verbatim if it were
    //    present — its absence is the fix.
    assert!(
        !snap
            .windows(b"## When done".len())
            .any(|w| w == b"## When done"),
        "worker pane prompt still contains the `## When done` footer (#187 regression); \
         the footer belongs in the task file, not the injected prompt. snapshot = {snap_str:?}"
    );

    // 3) The footer (and the task body) must live in the worker task file.
    let task_file = cwd
        .path()
        .join(".dot-agent-deck")
        .join("worker-task-coder.md");
    let mut file_body = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(&task_file) {
            file_body = s;
            if file_body.contains("## When done") {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        file_body.contains("## When done") && file_body.contains("dot-agent-deck work-done --task"),
        "worker task file must carry the work-done footer; got: {file_body:?}"
    );
    assert!(
        file_body.contains(task),
        "worker task file must contain the delegated task body; got: {file_body:?}"
    );

    registry.shutdown_all();
}
