#![cfg(feature = "e2e")]

//! L2 real-Codex orchestration-worker proof for PRD #20.

use std::path::Path;
use std::time::Duration;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::DelegateSignal;
use spec::spec;

mod common;

const ORCH_PANE: &str = "synthetic-orchestrator-pane";
const WORKER_PANE: &str = "codex-worker-pane";
const WORKER_ROLE: &str = "coder";
const SENTINEL_NAME: &str = "codex_worker_sentinel_c81f2a.txt";
const SENTINEL_CONTENT: &str = "CODEX_WORKER_SENTINEL_OK";

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Scenario: Start a real cheap-model Codex as the `coder` role through the
/// normal wrapper spawn seam, then have a synthetic orchestrator delegate a
/// task. Codex must auto-submit the injected task pointer, create the requested
/// sentinel with exact contents, and signal work-done through the daemon socket.
#[spec("codex/worker/001")]
#[test]
fn codex_worker_001_real_worker_receives_delegate_and_signals_work_done() {
    skip_unless!(common::check_codex_available());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    runtime.block_on(codex_worker_001_inner());
}

async fn codex_worker_001_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("worker cwd is UTF-8")
        .to_string();
    let command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'model_reasoning_effort=\"low\"'",
        common::CODEX_TEST_MODEL
    );

    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\n\
             name = \"codex-worker-orchestration\"\n\n\
             [[orchestrations.roles]]\n\
             name = \"orchestrator\"\n\
             command = \"true\"\n\
             start = true\n\n\
             [[orchestrations.roles]]\n\
             name = \"{WORKER_ROLE}\"\n\
             command = {command:?}\n\
             clear = false\n"
        ),
    )
    .expect("write Codex worker orchestration config");

    let codex_home = cwd.path().join("codex-home");
    common::import_codex_credentials(&codex_home)
        .expect("copy Codex credentials and trust worker cwd");
    let worker_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(&cwd_str),
            rows: 40,
            cols: 120,
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
                (
                    "DOT_AGENT_DECK_SOCKET".to_string(),
                    daemon.hook_path.display().to_string(),
                ),
                ("PATH".to_string(), path_with_binary_dir()),
                (
                    "HOME".to_string(),
                    codex_home
                        .to_str()
                        .expect("Codex test HOME is UTF-8")
                        .to_string(),
                ),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn wrapped real Codex worker");

    {
        let mut state = daemon.state.write().await;
        state
            .pane_role_map
            .insert(ORCH_PANE.to_string(), "orchestrator".to_string());
        state
            .pane_role_map
            .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
        state.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        let orchestration = ("codex-worker-orchestration".to_string(), cwd_str.clone());
        state
            .pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orchestration.clone());
        state
            .pane_orchestration_map
            .insert(WORKER_PANE.to_string(), orchestration);
        state
            .pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());
        state
            .pane_cwd_map
            .insert(WORKER_PANE.to_string(), cwd_str.clone());
    }

    common::wait_until_agent_output_settled(
        &daemon.registry,
        &worker_agent_id,
        Duration::from_secs(2),
        Duration::from_secs(45),
    )
    .await;

    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: format!(
            "First create {SENTINEL_NAME} in the current working directory with the exact contents {SENTINEL_CONTENT} and no trailing newline. Then run the dot-agent-deck work-done command from the completion instructions below. Do not stop before both steps are complete."
        ),
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };
    daemon
        .state
        .read()
        .await
        .handle_delegate(signal, &daemon.registry, &daemon.event_tx)
        .await;

    let sentinel = cwd.path().join(SENTINEL_NAME);
    let sentinel_ok = common::wait_for_path_async(&sentinel, Duration::from_secs(240)).await;
    let worker_pane = || {
        daemon
            .registry
            .snapshot(&worker_agent_id)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };
    let pane = worker_pane();
    assert!(
        sentinel_ok,
        "wrapped Codex never created {SENTINEL_NAME:?}. pointer_reached_pane={}\n=== Codex worker pane ===\n{pane}\n=== end ===",
        pane.contains("worker-task-coder.md")
    );
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("read Codex worker sentinel"),
        SENTINEL_CONTENT,
        "Codex worker created the sentinel with unexpected contents"
    );

    let work_done = cwd
        .path()
        .join(".dot-agent-deck")
        .join("work-done-coder.md");
    assert!(
        common::wait_for_path_async(&work_done, Duration::from_secs(120)).await,
        "wrapped Codex created the sentinel but never signalled work-done through the hook socket (missing {work_done:?})\n=== Codex worker pane ===\n{}\n=== end ===",
        worker_pane()
    );
}
