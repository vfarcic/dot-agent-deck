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
use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, DelegateSignal, EventType};
use dot_agent_deck::state::AppState;
#[cfg(unix)]
use spec::spec;

mod common;

const ORCH_PANE: &str = "orchestrator-pane";
const WORKER_PANE: &str = "worker-pane";
const WORKER_ROLE: &str = "coder";
const POINTER: &[u8] = b"Read .dot-agent-deck/worker-task-coder.md for your task.";
const SESSION_START_ORIGIN_METADATA_KEY: &str = "session_start_origin";
const WRAPPER_FORK_SESSION_START_ORIGIN: &str = "wrapper_fork";

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

#[cfg(unix)]
fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write synthetic agent executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod synthetic agent executable");
}

#[cfg(unix)]
fn path_with_built_deck(bin_dir: &std::path::Path) -> String {
    let deck_dir = std::path::Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .parent()
        .expect("built deck binary has a parent directory");
    format!(
        "{}:{}:{}",
        bin_dir.display(),
        deck_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[cfg(unix)]
async fn wait_for_replacement_agent(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    old_agent_id: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(record) = registry.agent_records().into_iter().find(|record| {
            record.pane_id_env.as_deref() == Some(pane_id) && record.id != old_agent_id
        }) {
            return record.id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "delegate never replaced agent {old_agent_id:?} for pane {pane_id:?}; records = {:?}",
            registry.agent_records()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
fn register_orchestration(state: &mut AppState, cwd: &str) {
    let orchestration = ("test-orchestration".to_string(), cwd.to_string());
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
        .insert(WORKER_PANE.to_string(), orchestration);
    state
        .pane_cwd_map
        .insert(WORKER_PANE.to_string(), cwd.to_string());
}

#[cfg(unix)]
fn session_start_event(
    agent_type: AgentType,
    pane_id: &str,
    agent_id: &str,
    wrapper_fork: bool,
) -> AgentEvent {
    let mut metadata = std::collections::HashMap::new();
    if wrapper_fork {
        metadata.insert(
            SESSION_START_ORIGIN_METADATA_KEY.to_string(),
            WRAPPER_FORK_SESSION_START_ORIGIN.to_string(),
        );
    }
    AgentEvent {
        session_id: format!("session-{agent_id}"),
        agent_type,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata,
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
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
    let snap =
        wait_for_snapshot_needle(&registry, &worker_agent_id, POINTER, Duration::from_secs(5))
            .await;
    let snap_str = String::from_utf8_lossy(&snap);
    assert!(
        snap.windows(POINTER.len()).any(|w| w == POINTER),
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

/// Scenario: Delegate with `clear = true` to a wrapped Codex stand-in whose wrapper surfaces a fork-time `SessionStart` before the child is genuinely ready. The prompt must remain absent after that card-surfacing event and appear only after a native Codex `SessionStart` for the replacement agent arrives.
#[spec("orchestration/delegate/007")]
#[test]
#[cfg(unix)]
fn delegate_007_wrapper_fork_start_does_not_release_native_hook_agent() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build wrapper readiness runtime")
        .block_on(delegate_007_wrapper_fork_start_does_not_release_native_hook_agent_inner());
}

#[cfg(unix)]
async fn delegate_007_wrapper_fork_start_does_not_release_native_hook_agent_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let bin_dir = cwd.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create synthetic Codex bin dir");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\nexec cat\n");
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        "[[orchestrations]]\nname = \"test-orchestration\"\n\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"true\"\nstart = true\n\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = \"codex\"\nclear = true\n",
    )
    .expect("write wrapped worker orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let old_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("codex"),
            cwd: Some(&cwd_str),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
                (
                    "DOT_AGENT_DECK_SOCKET".to_string(),
                    daemon.hook_path.display().to_string(),
                ),
                ("PATH".to_string(), path_with_built_deck(&bin_dir)),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn initial wrapped Codex stand-in");
    {
        let mut state = daemon.state.write().await;
        register_orchestration(&mut state, &cwd_str);
    }

    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: "List the files in the current directory.".to_string(),
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };
    daemon
        .state
        .read()
        .await
        .handle_delegate(signal, &daemon.registry, &daemon.event_tx)
        .await;
    let new_agent_id =
        wait_for_replacement_agent(&daemon.registry, WORKER_PANE, &old_agent_id).await;

    let before_native = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        POINTER,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        !before_native.windows(POINTER.len()).any(|w| w == POINTER),
        "wrapper fork-time SessionStart released the readiness gate before native Codex was ready; prompt reached replacement PTY early: {:?}",
        String::from_utf8_lossy(&before_native)
    );

    let native = session_start_event(AgentType::Codex, WORKER_PANE, &new_agent_id, false);
    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&native).expect("serialize native Codex SessionStart"),
    )
    .expect("write native Codex SessionStart");
    let after_native = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        POINTER,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        after_native.windows(POINTER.len()).any(|w| w == POINTER),
        "prompt was not delivered after native Codex SessionStart; snapshot = {:?}",
        String::from_utf8_lossy(&after_native)
    );
}

/// Scenario: Delegate with `clear = true` to a hookless wrapper-like stand-in and emit its marked fork-time `SessionStart`, the only readiness event it will produce. The replacement PTY must receive the prompt promptly instead of waiting for the timeout fallback.
#[spec("orchestration/delegate/008")]
#[test]
#[cfg(unix)]
fn delegate_008_hookless_wrapper_fork_start_still_releases_prompt() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build hookless wrapper runtime")
        .block_on(delegate_008_hookless_wrapper_fork_start_still_releases_prompt_inner());
}

#[cfg(unix)]
async fn delegate_008_hookless_wrapper_fork_start_still_releases_prompt_inner() {
    common::init_test_env();
    let cwd = common::race_safe_tempdir();
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        "[[orchestrations]]\nname = \"test-orchestration\"\n\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"true\"\nstart = true\n\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = \"cat\"\nclear = true\n",
    )
    .expect("write hookless wrapper-like orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    let old_agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn hookless wrapper-like stand-in");
    let (event_tx, _rx) = broadcast::channel::<BroadcastMsg>(64);
    let mut state = AppState::default();
    register_orchestration(&mut state, &cwd_str);
    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: "List the files in the current directory.".to_string(),
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };
    state.handle_delegate(signal, &registry, &event_tx).await;
    let new_agent_id = wait_for_replacement_agent(&registry, WORKER_PANE, &old_agent_id).await;

    event_tx
        .send(BroadcastMsg::Event(session_start_event(
            AgentType::None,
            WORKER_PANE,
            &new_agent_id,
            true,
        )))
        .expect("dispatch task subscribes before respawn");
    let snapshot =
        wait_for_snapshot_needle(&registry, &new_agent_id, POINTER, Duration::from_secs(2)).await;
    assert!(
        snapshot.windows(POINTER.len()).any(|w| w == POINTER),
        "hookless wrapper's sole fork-time SessionStart must release prompt delivery promptly; snapshot = {:?}",
        String::from_utf8_lossy(&snapshot)
    );
    registry.shutdown_all();
}
