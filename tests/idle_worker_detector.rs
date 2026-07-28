//! Fast-tier behavioral coverage for the daemon's idle-worker detector.
//!
//! These tests exercise the real `AppState::handle_delegate` and
//! `AppState::handle_work_done` paths with daemon-owned PTYs. The role maps are
//! populated exactly as `StartAgent` would populate them. Worker panes use
//! `cat`; the orchestrator uses a raw, no-echo `cat`, making each daemon prompt
//! appear exactly once in its observable PTY snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::{RwLock, broadcast};

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, bind_attach_listener, serve_attach_with_counter,
};
use dot_agent_deck::event::{BroadcastMsg, DelegateSignal, WorkDoneSignal};
use dot_agent_deck::project_config::load_project_config;
use dot_agent_deck::state::{AppState, SharedState};
use spec::spec;

mod common;

const ORCH_PANE: &str = "idle-orchestrator-pane";
const ORCH_ROLE: &str = "orchestrator";
const ORCHESTRATION: &str = "idle-test-orchestration";
const TIMEOUT_ENV: &str = "DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS";
const IDLE_NEEDLE: &str = "has not responded";

/// Serializes process-environment changes when these tests are run with plain
/// `cargo test`; nextest already runs each test in its own process.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    previous: Option<String>,
}

impl EnvGuard {
    fn set(value: Option<&str>) -> Self {
        let previous = std::env::var(TIMEOUT_ENV).ok();
        // SAFETY: every test in this integration-test binary holds ENV_LOCK for
        // the guard's full lifetime, so this environment mutation is serialized.
        unsafe {
            match value {
                Some(value) => std::env::set_var(TIMEOUT_ENV, value),
                None => std::env::remove_var(TIMEOUT_ENV),
            }
        }
        Self { previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: the caller still holds ENV_LOCK while this guard is dropped.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(TIMEOUT_ENV, value),
                None => std::env::remove_var(TIMEOUT_ENV),
            }
        }
    }
}

struct IdleHarness {
    cwd: TempDir,
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    orchestrator_agent_id: String,
    worker_agent_ids: HashMap<String, String>,
}

impl IdleHarness {
    async fn new(worker_roles: &[&str], project_config: Option<&str>) -> Self {
        common::init_test_env();
        let cwd = common::race_safe_tempdir();
        if let Some(contents) = project_config {
            std::fs::write(cwd.path().join(".dot-agent-deck.toml"), contents)
                .expect("write project config");
        }
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let registry = Arc::new(AgentPtyRegistry::new());

        // Raw no-echo cat gives one observable copy per injected prompt. The
        // readiness marker ensures termios has changed before a timer can fire.
        let orchestrator_agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some(
                    "stty -echo -icanon -icrnl -opost min 1 time 0 && \
                     printf ORCH-READY && exec cat -u",
                ),
                cwd: Some(&cwd_str),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn orchestrator stub");

        let mut state = AppState::default();
        let orchestration = (ORCHESTRATION.to_string(), cwd_str.clone());
        state
            .pane_role_map
            .insert(ORCH_PANE.to_string(), ORCH_ROLE.to_string());
        state.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        state
            .pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orchestration.clone());
        state
            .pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());

        let mut worker_agent_ids = HashMap::new();
        for role in worker_roles {
            let pane_id = worker_pane(role);
            let agent_id = registry
                .spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    cwd: Some(&cwd_str),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.clone())],
                    ..SpawnOptions::default()
                })
                .unwrap_or_else(|error| panic!("spawn {role} worker stub: {error}"));
            state
                .pane_role_map
                .insert(pane_id.clone(), role.to_string());
            state
                .pane_orchestration_map
                .insert(pane_id.clone(), orchestration.clone());
            state.pane_cwd_map.insert(pane_id, cwd_str.clone());
            worker_agent_ids.insert((*role).to_string(), agent_id);
        }

        let (event_tx, _event_rx) = broadcast::channel(64);
        let harness = Self {
            cwd,
            registry,
            state: Arc::new(RwLock::new(state)),
            event_tx,
            orchestrator_agent_id,
            worker_agent_ids,
        };
        let ready = harness
            .wait_for_snapshot(
                |snapshot| snapshot.contains("ORCH-READY"),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            ready.contains("ORCH-READY"),
            "orchestrator raw-cat stub never became ready; snapshot = {ready:?}"
        );
        harness
    }

    async fn delegate(&self, roles: &[&str]) {
        let signal = DelegateSignal {
            pane_id: ORCH_PANE.to_string(),
            task: "Perform the delegated test task.".to_string(),
            to: roles.iter().map(|role| (*role).to_string()).collect(),
            timestamp: chrono::Utc::now(),
        };
        self.state
            .read()
            .await
            .handle_delegate(signal, &self.registry, &self.event_tx)
            .await;
    }

    async fn work_done(&self, role: &str) {
        self.state
            .read()
            .await
            .handle_work_done(
                WorkDoneSignal {
                    pane_id: worker_pane(role),
                    task: "The delegated test task is complete.".to_string(),
                    done: false,
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
            )
            .await;
    }

    async fn wait_for_snapshot(
        &self,
        predicate: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> String {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = String::from_utf8_lossy(
                &self
                    .registry
                    .snapshot(&self.orchestrator_agent_id)
                    .unwrap_or_default(),
            )
            .into_owned();
            if predicate(&snapshot) || tokio::time::Instant::now() >= deadline {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_idle_role(&self, role: &str, timeout: Duration) -> String {
        self.wait_for_snapshot(|snapshot| idle_mentions_role(snapshot, role), timeout)
            .await
    }
}

impl Drop for IdleHarness {
    fn drop(&mut self) {
        self.registry.shutdown_all();
    }
}

struct AttachServer {
    path: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AttachServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_attach_server(harness: &IdleHarness) -> AttachServer {
    let path = harness.cwd.path().join("attach.sock");
    let listener = bind_attach_listener(&path).expect("bind attach listener");
    let registry = Arc::clone(&harness.registry);
    let state = Arc::clone(&harness.state);
    let event_tx = harness.event_tx.clone();
    let task = tokio::spawn(async move {
        let _ = serve_attach_with_counter(
            listener,
            registry,
            event_tx,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            state,
            None,
            Arc::new(dot_agent_deck::scheduler::Scheduler::with_stderr_notifier()),
            dot_agent_deck::spawn::new_reuse_registry(),
            dot_agent_deck::issue_dispatch_run::new_worktree_registry(),
        )
        .await;
    });
    AttachServer { path, task }
}

fn worker_pane(role: &str) -> String {
    format!("idle-{role}-pane")
}

fn idle_mentions_role(snapshot: &str, role: &str) -> bool {
    snapshot
        .split(['\r', '\n'])
        .any(|line| line.contains(IDLE_NEEDLE) && line.contains(role))
}

fn idle_count(snapshot: &str) -> usize {
    snapshot.match_indices(IDLE_NEEDLE).count()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime")
}

/// Scenario: Register an orchestrator and a `coder` worker in one orchestration, then delegate to the worker with a tiny timeout and never send work-done. The orchestrator pane must receive one self-describing idle prompt containing both "has not responded" and the `coder` role name.
#[spec("scheduler/idle-worker/001")]
#[test]
fn idle_worker_001_silent_worker_prompts_orchestrator() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("1200"));
    runtime().block_on(async {
        let harness = IdleHarness::new(&["coder"], None).await;
        harness.delegate(&["coder"]).await;

        let snapshot = harness
            .wait_for_idle_role("coder", Duration::from_secs(4))
            .await;
        assert!(
            idle_mentions_role(&snapshot, "coder"),
            "silent coder did not produce a self-describing idle prompt; snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: Delegate concurrently to a silent control worker and a responsive worker, then send work-done from the responsive worker before the tiny timeout. The control must prove the detector fired while the responsive role must never receive an idle prompt after the timeout.
#[spec("scheduler/idle-worker/002")]
#[test]
fn idle_worker_002_work_done_cancels_idle_prompt() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("1500"));
    runtime().block_on(async {
        let harness = IdleHarness::new(&["silent-control", "responsive-worker"], None).await;
        harness
            .delegate(&["silent-control", "responsive-worker"])
            .await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        harness.work_done("responsive-worker").await;

        let _ = harness
            .wait_for_idle_role("silent-control", Duration::from_secs(4))
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = harness.wait_for_snapshot(|_| true, Duration::ZERO).await;
        assert!(
            idle_mentions_role(&snapshot, "silent-control"),
            "silent control worker did not prove the detector fired; snapshot = {snapshot:?}"
        );
        assert!(
            !idle_mentions_role(&snapshot, "responsive-worker"),
            "work-done did not cancel the responsive worker's idle prompt; snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: With the timeout environment override unset, first parse an otherwise empty project config and require the default timeout to be 120 minutes. Then place `worker_response_timeout_minutes = 0` before any table header, delegate to a silent worker, and require that configured timeout to fire immediately.
#[spec("scheduler/idle-worker/003")]
#[test]
fn idle_worker_003_timeout_config_and_default_are_honored() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(None);
    runtime().block_on(async {
        let default_dir = common::race_safe_tempdir();
        std::fs::write(default_dir.path().join(".dot-agent-deck.toml"), "")
            .expect("write empty project config");
        let default_config = load_project_config(default_dir.path())
            .expect("parse default project config")
            .expect("project config exists");
        let default_debug = format!("{default_config:?}");
        let default_is_120 =
            default_debug.contains("worker_response_timeout_minutes: 120");

        let harness = IdleHarness::new(
            &["config-worker"],
            Some("worker_response_timeout_minutes = 0\n\n[[orchestrations]]\nname = \"unused\"\nroles = []\n"),
        )
        .await;
        harness.delegate(&["config-worker"]).await;
        let snapshot = harness
            .wait_for_idle_role("config-worker", Duration::from_secs(3))
            .await;
        let configured_timeout_fired = idle_mentions_role(&snapshot, "config-worker");

        assert!(
            default_is_120 && configured_timeout_fired,
            "timeout contract not honored: default_debug = {default_debug:?}, configured snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: Delegate once to a silent worker with a tiny timeout, wait for its idle prompt, then keep the worker open for another timeout window. The orchestrator snapshot must contain exactly one "has not responded" prompt, proving the detector does not re-nag.
#[spec("scheduler/idle-worker/004")]
#[test]
fn idle_worker_004_idle_prompt_is_one_shot() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("1000"));
    runtime().block_on(async {
        let harness = IdleHarness::new(&["one-shot-worker"], None).await;
        harness.delegate(&["one-shot-worker"]).await;
        let first = harness
            .wait_for_idle_role("one-shot-worker", Duration::from_secs(4))
            .await;
        assert!(
            idle_mentions_role(&first, "one-shot-worker"),
            "the first idle prompt never fired; snapshot = {first:?}"
        );

        tokio::time::sleep(Duration::from_millis(1700)).await;
        let final_snapshot = harness.wait_for_snapshot(|_| true, Duration::ZERO).await;
        assert_eq!(
            idle_count(&final_snapshot),
            1,
            "one delegation must produce exactly one idle prompt; snapshot = {final_snapshot:?}"
        );
    });
}

/// Scenario: Delegate to one worker, then re-delegate to that same pane before the first timer expires. Wait past delegation one's deadline and require no premature prompt, then wait for delegation two's deadline and require exactly one role-bearing prompt.
#[spec("scheduler/idle-worker/005")]
#[test]
fn idle_worker_005_redelegation_replaces_first_timer() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("2000"));
    runtime().block_on(async {
        let harness = IdleHarness::new(&["redelegated-worker"], None).await;
        harness.delegate(&["redelegated-worker"]).await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        harness.delegate(&["redelegated-worker"]).await;

        tokio::time::sleep(Duration::from_millis(1200)).await;
        let premature = harness.wait_for_snapshot(|_| true, Duration::ZERO).await;
        assert!(
            !idle_mentions_role(&premature, "redelegated-worker"),
            "delegation one's stale timer fired against delegation two; snapshot = {premature:?}"
        );

        let final_snapshot = harness
            .wait_for_idle_role("redelegated-worker", Duration::from_secs(3))
            .await;
        assert!(
            idle_mentions_role(&final_snapshot, "redelegated-worker"),
            "delegation two's idle timer never fired; snapshot = {final_snapshot:?}"
        );
        assert_eq!(
            idle_count(&final_snapshot),
            1,
            "re-delegation must leave exactly one active idle timer; snapshot = {final_snapshot:?}"
        );
    });
}

/// Scenario: Delegate concurrently to a silent control worker and a worker that is immediately closed through the real StopAgent attach request. The control must produce an idle prompt while the stopped role remains absent after the timeout, proving pane closure cancels its timer.
#[spec("scheduler/idle-worker/006")]
#[test]
fn idle_worker_006_stop_agent_cancels_idle_prompt() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("2500"));
    runtime().block_on(async {
        let harness = IdleHarness::new(&["silent-control", "stopped-worker"], None).await;
        let server = start_attach_server(&harness).await;
        harness
            .delegate(&["silent-control", "stopped-worker"])
            .await;

        let stopped_id = harness
            .worker_agent_ids
            .get("stopped-worker")
            .expect("stopped worker registry id");
        let response = common::attach_request_on(
            &server.path,
            &AttachRequest::StopAgent {
                id: stopped_id.clone(),
            },
        )
        .expect("StopAgent over attach socket");
        assert!(response.ok, "StopAgent failed: {:?}", response.error);

        let _ = harness
            .wait_for_idle_role("silent-control", Duration::from_secs(6))
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = harness.wait_for_snapshot(|_| true, Duration::ZERO).await;
        assert!(
            idle_mentions_role(&snapshot, "silent-control"),
            "silent control worker did not prove the detector fired; snapshot = {snapshot:?}"
        );
        assert!(
            !idle_mentions_role(&snapshot, "stopped-worker"),
            "StopAgent did not cancel the stopped worker's idle prompt; snapshot = {snapshot:?}"
        );
    });
}
