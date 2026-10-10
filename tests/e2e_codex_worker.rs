#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! L2 real-Codex orchestration-worker proof for PRD #20.

use std::path::Path;
use std::time::Duration;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{AgentType, DelegateSignal};
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
    runtime.block_on(codex_worker_inner(WorkerLaunch::Settled));
}

/// Scenario: Start a real cheap-model Codex as a `clear = false` `coder` role
/// behind a launcher script that takes a few seconds before it runs Codex, as
/// `devbox run codex-big` does, and delegate to it at once, while it is still
/// starting, with the deck's re-send turned off. The single write of the task
/// pointer must reach Codex whole: Codex does the task (the sentinel with exact
/// contents) and signals work-done.
#[spec("codex/worker/002")]
#[test]
fn codex_worker_002_clear_false_delegate_to_a_starting_worker_arrives_whole() {
    // The pointer's in-place re-send off: this test is about the FIRST write.
    // With it on, a pointer lost in a starting Codex is pressed in again 20 s
    // later, and the test would pass whether or not the deck waited for Codex.
    // SAFETY: a stated residual, not a proof (issue #1516). Set at the top of
    // the sync entry point, before the availability probe and the tokio
    // runtime exist, and never written again; nextest runs each test in its
    // own process, so the change cannot reach another test.
    unsafe {
        std::env::set_var("DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS", "0");
    }
    skip_unless!(common::check_codex_available());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    runtime.block_on(codex_worker_inner(WorkerLaunch::StillStarting));
}

/// When the delegate goes out relative to the worker's start.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerLaunch {
    /// Bare `codex`, delegated once its output has settled.
    Settled,
    /// Through a launcher that waits before it runs Codex, delegated as soon as
    /// the worker is spawned (issue #1650).
    StillStarting,
}

async fn codex_worker_inner(launch: WorkerLaunch) {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("worker cwd is UTF-8")
        .to_string();
    let codex_args = format!(
        "--model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model(),
    );
    let command = match launch {
        WorkerLaunch::Settled => format!("codex {codex_args}"),
        WorkerLaunch::StillStarting => {
            // A launcher whose name hides the agent, like `devbox run …`; the
            // role declares `agent = "codex"` as this repository's own config
            // does, so the deck still wraps it.
            let launcher = cwd.path().join("start-codex.sh");
            std::fs::write(&launcher, "sleep 4\nexec codex \"$@\"\n")
                .expect("write the Codex launcher");
            // Through `sh`, so the script needs no executable bit — and this
            // file no Unix-only permissions API (Qodo, PR #1659).
            format!("sh {} {codex_args}", launcher.display())
        }
    };

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
             agent = \"codex\"\n\
             clear = false\n"
        ),
    )
    .expect("write Codex worker orchestration config");

    let codex_home = cwd.path().join("codex-home");
    // Issue #502/#785: the importer registers what it copied for DIAGNOSTIC
    // redaction itself, so this in-process worker path is covered without a
    // `TuiDeck` to hold a recording set — which it has none of, and never will.
    let _codex_redactions = common::import_codex_credentials(&codex_home)
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
            agent_type: Some(AgentType::Codex),
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
        let orchestration = dot_agent_deck::state::OrchestrationIdentity {
            id: "orch-test-0".to_string(),
            name: "codex-worker-orchestration".to_string(),
        };
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

    if launch == WorkerLaunch::Settled {
        common::wait_until_agent_output_settled(
            &daemon.registry,
            &worker_agent_id,
            Duration::from_secs(2),
            Duration::from_secs(45),
        )
        .await;
    }

    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: format!(
            "First create {SENTINEL_NAME} in the current working directory with the exact contents {SENTINEL_CONTENT} and no trailing newline. Then run the dot-agent-deck work-done command from the completion instructions below. Do not stop before both steps are complete."
        ),
        to: vec![WORKER_ROLE.to_string()],
        supersede: false,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    daemon
        .state
        .read()
        .await
        .handle_delegate(signal, &daemon.registry, &daemon.event_tx)
        .await;

    let sentinel = cwd.path().join(SENTINEL_NAME);
    // Poll the CONTENT, not existence: a shell redirect creates the file before
    // the write lands, so an existence wait can read it empty (issue #244).
    // `codex/worker/002` runs inside nextest's 180 s terminate-after, so its
    // wait ends early enough for the panic below to show the pane.
    let sentinel_wait = match launch {
        WorkerLaunch::Settled => Duration::from_secs(240),
        WorkerLaunch::StillStarting => Duration::from_secs(120),
    };
    let sentinel_result =
        common::wait_for_file_trimmed_eq_async(&sentinel, SENTINEL_CONTENT, sentinel_wait).await;
    let worker_pane = || {
        daemon
            .registry
            .snapshot(&worker_agent_id)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };
    if let Err(observed) = sentinel_result {
        let pane = worker_pane();
        panic!(
            "wrapped Codex never wrote {SENTINEL_CONTENT:?} into {SENTINEL_NAME:?} within {sentinel_wait:?}; \
             observed: {observed}. pointer_reached_pane={}\n=== Codex worker pane ===\n{pane}\n=== end ===",
            pane.contains("worker-task-coder.md")
        );
    }

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
