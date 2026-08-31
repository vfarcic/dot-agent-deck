// PRD #126: this suite is Unix-only at the source level, for the two reasons the
// repo already gates on. (1) Every test's harness spawns a real PTY running a
// POSIX-shell stub — `stty -echo -icanon`, `printf`, `exec cat -u`, `trap ''
// TERM` under a pinned `SHELL=/bin/sh` — none of which exists on Windows, the
// same rationale as `src/agent_pty.rs`'s `#[cfg(all(test, unix))] mod
// spawn_tests`. (2) `006`/`008`/`009`/`010` additionally drive the REAL
// `StopAgent` attach request over a Unix-domain socket via
// `common::attach_request_on`, which is itself `#[cfg(unix)]`.
//
// Unlike the `e2e_*.rs` suites, this file is FAST tier, so CI's Windows job
// (`cargo nextest run`, no `--features e2e`) does compile it — which is how the
// missing gate surfaced there as an `attach_request_on` E0425 and nowhere else.
// `#![cfg(unix)]` makes the crate empty on Windows so that build compiles and
// does not panic; on Linux and macOS every test still runs and asserts exactly
// as before, matching the `Platform coverage: mac+linux` already documented for
// all 14 of these specs in `tests/CATALOG.md`. A named-pipe + ConPTY port of the
// harness is tracked by #164 (M10).
#![cfg(unix)]
//! Fast-tier behavioral coverage for the daemon's idle-worker detector.
//!
//! These tests exercise the real `AppState::handle_delegate` and
//! `AppState::handle_work_done` paths with daemon-owned PTYs. The role maps are
//! populated exactly as `StartAgent` would populate them. Worker panes use
//! `cat`; the orchestrator uses a raw, no-echo `cat`, making each daemon prompt
//! appear exactly once in its observable PTY snapshot — except where a test
//! needs the orchestrator to go away on its own, which is what
//! [`OrchestratorStub::ExitsOnFlag`] is for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::{RwLock, broadcast};

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership,
};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, bind_attach_listener, serve_attach_with_counter,
};
use dot_agent_deck::event::{BroadcastMsg, DelegateSignal, WorkDoneSignal};
use dot_agent_deck::state::{
    AppState, OrchestrationIdentity, SharedState, worker_response_timeout,
};
use spec::spec;

mod common;

const ORCH_PANE: &str = "idle-orchestrator-pane";
const ORCH_ROLE: &str = "orchestrator";
const ORCHESTRATION: &str = "idle-test-orchestration";
/// PRD #140's per-tab instance token. The harness stamps it on the daemon's
/// routing identity AND on the orchestrator pane's registry `TabMembership`,
/// exactly as `StartAgent` does for a current client, so the idle watch's
/// revalidation compares two *present* identities instead of short-circuiting on
/// an absent one — the case that decides whether a live orchestration's nudge is
/// delivered at all.
const ORCHESTRATION_INSTANCE: &str = "idle-test-orchestration-instance-1";
const TIMEOUT_ENV: &str = "DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS";

/// The daemon-authored opening clause of `compose_idle_worker_prompt`, spelled
/// out here rather than imported from `src/` so a silent rewording of the
/// injected prompt fails these tests instead of following them. The
/// parenthetical is the part that establishes DAEMON PROVENANCE: an LLM
/// orchestrator can easily emit "has not responded" while explaining why it is
/// waiting, but not a verbatim self-identification as a daemon report.
const IDLE_NEEDLE: &str = "has not responded with work-done (dot-agent-deck daemon report, not a \
                           message from a person or an agent)";

/// Ordinary `cat` worker stub: stays alive, never answers, dies on SIGTERM.
const WORKER_COMMAND: &str = "cat";

/// A worker that IGNORES SIGTERM, so `close_agent` spends its full
/// `AGENT_TERMINATE_GRACE` (3 s) in the grace loop before escalating to
/// SIGKILL. That grace window is the interval PRD #126 M1 review finding 1
/// cared about: the pane is being closed, the agent is still alive, and a
/// timer firing inside it used to inject the very nudge the close exists to
/// suppress. `SHELL` is pinned so the `trap` builtin is POSIX `sh`'s (it is
/// consumed as a wrapper choice and never exported into the child).
const TERM_RESISTANT_WORKER_COMMAND: &str = "trap '' TERM; exec cat";

/// A worker that stays alive just long enough to receive its
/// delegated task pointer, then ENDS ITS OWN PROCESS — no SIGTERM, no
/// `StopAgent`, no explicit close of any kind. This is the whole scenario the
/// EOF-triggered sweep exists for: a worker that genuinely finished (its
/// session simply ended) without ever calling `work-done`, so the daemon's
/// only signal that anything happened is `pump_reader` observing PTY EOF.
///
/// The fixed sleep's margin does NOT measure the delegate's guarded write
/// landing — the property the EOF-triggered sweep actually depends on is
/// earlier and cheaper than that: `dispatch_one_owned` calls
/// `AgentPtyRegistry::bind_delegation_worker_agent_id` before it ever
/// attempts the write, and it is the BIND, not the write, that lets the
/// sweep's worker-side identity match find this record at all (an unbound
/// record is left for its own timer instead — see
/// `OutstandingDelegation::worker_agent_id`'s doc comment). If the worker
/// exits before the bind lands, the test does not fail loudly at the delegate
/// — it fails later at the notice-appears assertion, which reads as a
/// product regression rather than a harness timing loss. This sleep is sized
/// well above the ordinary poll-latency-plus-task-scheduling cost of the bind
/// as a safety margin against a loaded CI runner; it is not a signal-driven
/// wait because nothing in the current registry API lets a test observe the
/// bind directly without a source change out of this fix's scope.
const WORKER_EXITS_ON_ITS_OWN_COMMAND: &str =
    "stty -echo -icanon -icrnl -opost min 1 time 0 && printf WORKER-READY && sleep 2.0";

/// The daemon-authored opening clause of `compose_worker_exited_notice` — the
/// EOF-triggered notice, distinct from both
/// `IDLE_NEEDLE` (the timeout-based idle prompt) and the delegate-possibly-
/// not-delivered silence notice. Spelled out here, not imported from `src/`,
/// so a silent rewording of the notice fails this test instead of following
/// it.
const WORKER_EXITED_NEEDLE: &str =
    "delegated worker exited without work-done (dot-agent-deck daemon report)";

/// The daemon-authored opening clause of `compose_delegate_silence_notice` —
/// the OLDER, timeout-based went-quiet notice the EOF-triggered sweep must also
/// retire promptly rather than leaving it to run out its own window.
const SILENCE_NEEDLE: &str = "delegated worker went quiet (dot-agent-deck daemon report)";

/// File the [`OrchestratorStub::ExitsOnFlag`] stub polls for. Its appearance in
/// the harness cwd is the test's remote control for a NATURAL orchestrator exit.
const NATURAL_EXIT_FLAG: &str = "orchestrator-exit.flag";

/// Which orchestrator stub a harness spawns onto [`ORCH_PANE`].
enum OrchestratorStub {
    /// Raw, no-echo `cat`: stays alive for the whole test, so every daemon
    /// submission into the pane lands in an observable scrollback.
    Persistent,
    /// Same observable shape, but the process ENDS BY ITSELF the moment
    /// [`NATURAL_EXIT_FLAG`] appears in its cwd. That is the path
    /// `scheduler/idle-worker/008` cannot reach: `StopAgent` runs
    /// `begin_pane_close`, whose sweep drops every record pointing at the pane
    /// *before* any timer can wake, whereas a process that simply exits
    /// triggers no close transition and therefore no sweep at all. On that path
    /// the `write_and_submit_guarded` agent-id gate is the ONLY thing between a
    /// still-armed timer and whichever agent inherits the freed pane id.
    ExitsOnFlag,
}

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

    /// Re-point the seam mid-test. Used by `003`, whose whole contract is that
    /// the SAME harness and cwd behave differently for `0` and a positive
    /// value — the timeout is resolved per delegate, so flipping it between
    /// delegates is the decisive comparison.
    fn repoint(&self, value: Option<&str>) {
        // SAFETY: the caller still holds ENV_LOCK.
        unsafe {
            match value {
                Some(value) => std::env::set_var(TIMEOUT_ENV, value),
                None => std::env::remove_var(TIMEOUT_ENV),
            }
        }
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
        let workers: Vec<(&str, &str)> = worker_roles
            .iter()
            .map(|role| (*role, WORKER_COMMAND))
            .collect();
        Self::with_workers(&workers, project_config).await
    }

    /// Same as [`IdleHarness::new`] but each worker names its own stub command,
    /// so a test can put a SIGTERM-ignoring worker next to an ordinary one.
    async fn with_workers(workers: &[(&str, &str)], project_config: Option<&str>) -> Self {
        Self::with_orchestrator_stub(OrchestratorStub::Persistent, workers, project_config).await
    }

    /// Same as [`IdleHarness::with_workers`] but the orchestrator stub is
    /// chosen too — see [`OrchestratorStub`].
    async fn with_orchestrator_stub(
        orchestrator: OrchestratorStub,
        workers: &[(&str, &str)],
        project_config: Option<&str>,
    ) -> Self {
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
        let orchestrator_agent_id = match orchestrator {
            OrchestratorStub::Persistent => spawn_raw_cat_observer_with_membership(
                &registry,
                ORCH_PANE,
                "ORCH-READY",
                &cwd_str,
                Some(orchestrator_membership(&cwd_str)),
            ),
            OrchestratorStub::ExitsOnFlag => spawn_exit_on_flag_observer(
                &registry,
                ORCH_PANE,
                "ORCH-READY",
                &cwd_str,
                Some(orchestrator_membership(&cwd_str)),
            ),
        };

        let mut state = AppState::default();
        // PRD #140: the daemon's routing identity. `Instance` is what a current
        // client produces — two tabs of one orchestration in one directory are
        // told apart by this token, not by `(name, cwd)`.
        let orchestration = OrchestrationIdentity::Instance {
            id: ORCHESTRATION_INSTANCE.to_string(),
            name: ORCHESTRATION.to_string(),
        };
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
        for (role, command) in workers {
            let pane_id = worker_pane(role);
            let agent_id = registry
                .spawn_agent(SpawnOptions {
                    command: Some(command),
                    cwd: Some(&cwd_str),
                    env: vec![
                        (DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.clone()),
                        ("SHELL".to_string(), "/bin/sh".to_string()),
                    ],
                    ..SpawnOptions::default()
                })
                .unwrap_or_else(|error| panic!("spawn {role} worker stub: {error}"));
            state
                .pane_role_map
                .insert(pane_id.clone(), (*role).to_string());
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
        // Issue #709: every test in this file boots through here, so the flat
        // 5 s this used to give a freshly spawned `sh` its first `printf` was a
        // second fixed deadline on `scheduler/idle-worker/008`'s path — and the
        // one that fires FIRST. Same treatment as the successor wait in `008`:
        // still condition-driven, but with a ceiling scaled by how contended
        // the machine is and an early return if the stub dies rather than
        // prints.
        let ready = String::from_utf8_lossy(
            &common::wait_for_child_first_output(
                &harness.registry,
                &harness.orchestrator_agent_id,
                b"ORCH-READY",
            )
            .await,
        )
        .into_owned();
        assert!(
            ready.contains("ORCH-READY"),
            "orchestrator raw-cat stub never became ready; snapshot = {ready:?}"
        );
        harness
    }

    fn cwd_str(&self) -> String {
        self.cwd.path().to_string_lossy().to_string()
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

    fn snapshot_of(&self, agent_id: &str) -> String {
        String::from_utf8_lossy(&self.registry.snapshot(agent_id).unwrap_or_default()).into_owned()
    }

    async fn wait_for_snapshot_of(
        &self,
        agent_id: &str,
        predicate: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> String {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot_of(agent_id);
            if predicate(&snapshot) || tokio::time::Instant::now() >= deadline {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_snapshot(
        &self,
        predicate: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> String {
        let agent_id = self.orchestrator_agent_id.clone();
        self.wait_for_snapshot_of(&agent_id, predicate, timeout)
            .await
    }

    async fn wait_for_idle_role(&self, role: &str, timeout: Duration) -> String {
        self.wait_for_snapshot(|snapshot| idle_mentions_role(snapshot, role), timeout)
            .await
    }

    /// Let an [`OrchestratorStub::ExitsOnFlag`] orchestrator end its own
    /// process, then wait until the pane genuinely has no live owner so the
    /// caller can hand the freed pane id to a successor.
    ///
    /// Deliberately NOT a `StopAgent`: nothing in this path calls
    /// `begin_pane_close`, so no record sweep runs and the armed delegation
    /// survives its orchestrator. The caller asserts `is_pane_closing` is false
    /// afterwards to pin that down.
    async fn end_orchestrator_process(&self) {
        std::fs::write(self.cwd.path().join(NATURAL_EXIT_FLAG), b"exit\n")
            .expect("write the orchestrator stub's exit flag");
        let freed = tokio::time::timeout(Duration::from_secs(5), async {
            while self.registry.pane_current_agent_id(ORCH_PANE).is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            freed.is_ok(),
            "the orchestrator stub never exited on its own, so the pane id was never freed for \
             reuse and the scenario under test could not occur"
        );
    }

    /// Stop an agent through the REAL `StopAgent` attach request, off the async
    /// runtime, and report how long the close took. A SIGTERM-ignoring child
    /// keeps the pane marked closing for the whole `AGENT_TERMINATE_GRACE`
    /// window, which is exactly the interval `009`/`010` need to hold open.
    async fn stop_agent_timed(
        socket: std::path::PathBuf,
        agent_id: String,
    ) -> (tokio::time::Instant, tokio::time::Instant) {
        let started = tokio::time::Instant::now();
        let response = tokio::task::spawn_blocking(move || {
            common::attach_request_on(&socket, &AttachRequest::StopAgent { id: agent_id })
        })
        .await
        .expect("StopAgent blocking task")
        .expect("StopAgent over attach socket");
        assert!(response.ok, "StopAgent failed: {:?}", response.error);
        (started, tokio::time::Instant::now())
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

/// Spawn a raw, no-echo `cat` bound to `pane_id`: every byte the daemon submits
/// into that pane appears exactly once in the agent's scrollback and nothing
/// else does, so "no bytes were submitted" is directly observable.
fn spawn_raw_cat_observer(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    marker: &str,
    cwd: &str,
) -> String {
    spawn_raw_cat_observer_with_membership(registry, pane_id, marker, cwd, None)
}

/// [`spawn_raw_cat_observer`] plus an optional registry `TabMembership`, which is
/// what the harness gives its ORCHESTRATOR stub: the idle watch reads the
/// orchestrator pane's live membership back out of the registry both to recover
/// the orchestration cwd at arm time and to revalidate the orchestration identity
/// immediately before submitting. Successor stubs that merely inherit a freed
/// pane id pass `None` — an unrelated agent genuinely has no membership, and
/// keeping it absent is what leaves the `write_and_submit_guarded` agent-id gate
/// as the only guard in `008`/`014`.
fn spawn_raw_cat_observer_with_membership(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    marker: &str,
    cwd: &str,
    membership: Option<TabMembership>,
) -> String {
    let command =
        format!("stty -echo -icanon -icrnl -opost min 1 time 0 && printf {marker} && exec cat -u");
    registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(cwd),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ],
            tab_membership: membership,
            ..SpawnOptions::default()
        })
        .unwrap_or_else(|error| panic!("spawn raw-cat observer on {pane_id}: {error}"))
}

/// The registry membership `StartAgent` would store for this harness's
/// orchestrator pane: PRD #140's per-tab instance token plus the shared
/// orchestration cwd.
fn orchestrator_membership(cwd: &str) -> TabMembership {
    TabMembership::Orchestration {
        name: ORCHESTRATION.to_string(),
        role_index: 0,
        role_name: ORCH_ROLE.to_string(),
        is_start_role: true,
        orchestration_cwd: Some(cwd.to_string()),
        display_title: None,
        orchestration_id: Some(ORCHESTRATION_INSTANCE.to_string()),
    }
}

/// An orchestrator stub whose own shell owns the pane and leaves as soon as
/// [`NATURAL_EXIT_FLAG`] appears in `cwd`: the process ends, the PTY reaches
/// EOF, and the registry marks the agent exited WITHOUT any close transition
/// having run. Nothing is `exec`d and no child is backgrounded, so the polling
/// shell is the single process holding the slave fd — when it leaves, the pane
/// is genuinely free (a surviving `cat` would hold the fd open and keep the
/// pane "live" forever, silently defeating the test).
///
/// Termios is pinned like [`spawn_raw_cat_observer`]'s for consistency, but this
/// pane is never an observation target: the successor that inherits its pane id
/// is.
fn spawn_exit_on_flag_observer(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    marker: &str,
    cwd: &str,
    membership: Option<TabMembership>,
) -> String {
    let flag = std::path::Path::new(cwd).join(NATURAL_EXIT_FLAG);
    let flag = flag.to_string_lossy();
    let command = format!(
        "stty -echo -icanon -icrnl -opost min 1 time 0; printf {marker}; \
         while [ ! -f '{flag}' ]; do sleep 0.05; done"
    );
    registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(cwd),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ],
            tab_membership: membership,
            ..SpawnOptions::default()
        })
        .unwrap_or_else(|error| panic!("spawn exit-on-flag observer on {pane_id}: {error}"))
}

fn worker_pane(role: &str) -> String {
    format!("idle-{role}-pane")
}

/// The daemon wraps the role name in unforgeable untrusted-data markers (PRD
/// #126 M1 audit finding 1). Matching the WRAPPED form — not the bare role
/// name — is what makes the assertion prove the text came from the daemon's
/// template rather than from anything else that happens to say the role name.
fn idle_role_needle(role: &str) -> String {
    format!("[UNTRUSTED-ROLE-LABEL: {role} :END-UNTRUSTED-ROLE-LABEL]")
}

fn idle_mentions_role(snapshot: &str, role: &str) -> bool {
    let role_needle = idle_role_needle(role);
    snapshot
        .split(['\r', '\n'])
        .any(|line| line.contains(IDLE_NEEDLE) && line.contains(&role_needle))
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

/// Write a `.dot-agent-deck.toml` carrying only the timeout key into a fresh
/// tempdir and return it. The key is placed above any table header — appending
/// it after one would silently make it a key OF that table.
fn config_dir_with_timeout(minutes: u64) -> TempDir {
    let dir = common::race_safe_tempdir();
    std::fs::write(
        dir.path().join(".dot-agent-deck.toml"),
        format!("worker_response_timeout_minutes = {minutes}\n"),
    )
    .expect("write project config");
    dir
}

fn dir_str(dir: &TempDir) -> String {
    dir.path().to_string_lossy().to_string()
}

/// Scenario: Register an orchestrator and a `coder` worker in one orchestration, then delegate to the worker with a tiny timeout and never send work-done. The orchestrator pane must receive one self-describing idle prompt carrying the daemon-report clause and the `coder` role inside the untrusted-role-label markers.
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

/// Scenario: In one orchestration whose config sets `worker_response_timeout_minutes = 0`, delegate three times while re-pointing the millisecond seam: a positive value must produce an idle prompt, the same harness with the seam at `0` must produce none, and with the seam unset the config's own `0` must produce none either. Exactly one prompt may exist at the end.
#[spec("scheduler/idle-worker/003")]
#[test]
fn idle_worker_003_zero_disables_the_detector_from_either_source() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let env = EnvGuard::set(Some("500"));
    runtime().block_on(async {
        let harness = IdleHarness::with_workers(
            &[
                ("env-positive-control", WORKER_COMMAND),
                ("env-zero-worker", WORKER_COMMAND),
                ("file-zero-worker", WORKER_COMMAND),
            ],
            Some("worker_response_timeout_minutes = 0\n\n[[orchestrations]]\nname = \"unused\"\nroles = []\n"),
        )
        .await;

        // Positive control. It also proves the env seam OVERRIDES the file:
        // this cwd's config says 0 (disabled), yet the seam's 500 ms fires.
        harness.delegate(&["env-positive-control"]).await;
        let control = harness
            .wait_for_idle_role("env-positive-control", Duration::from_secs(4))
            .await;
        assert!(
            idle_mentions_role(&control, "env-positive-control"),
            "a positive millisecond seam value was not honored, so the two zero cases below \
             would prove nothing; snapshot = {control:?}"
        );

        // Same harness, same cwd, same worker shape — only the seam changes.
        // A prompt here would therefore be attributable to nothing but the 0.
        env.repoint(Some("0"));
        harness.delegate(&["env-zero-worker"]).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let after_env_zero = harness.wait_for_snapshot(|_| true, Duration::ZERO).await;
        assert!(
            !idle_mentions_role(&after_env_zero, "env-zero-worker"),
            "the millisecond seam set to 0 must DISABLE the detector (no record, no timer), \
             not fire immediately; snapshot = {after_env_zero:?}"
        );

        // Seam unset: resolution now reads the config's own 0.
        env.repoint(None);
        harness.delegate(&["file-zero-worker"]).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let after_file_zero = harness.wait_for_snapshot(|_| true, Duration::ZERO).await;
        assert!(
            !idle_mentions_role(&after_file_zero, "file-zero-worker"),
            "worker_response_timeout_minutes = 0 must DISABLE the detector; \
             snapshot = {after_file_zero:?}"
        );
        assert_eq!(
            idle_count(&after_file_zero),
            1,
            "only the positive control may have produced a prompt; \
             snapshot = {after_file_zero:?}"
        );
    });
}

/// Scenario: Call the timeout resolver directly against purpose-built config directories with the millisecond seam set and unset. It must default to 120 minutes when the key is absent, prefer the env seam over the file, prefer the orchestration cwd over the worker cwd, and reject out-of-range values from either source by falling back rather than clamping.
#[spec("scheduler/idle-worker/007")]
#[test]
fn idle_worker_007_timeout_resolution_precedence_and_bounds() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let env = EnvGuard::set(None);

    let keyless = common::race_safe_tempdir();
    std::fs::write(keyless.path().join(".dot-agent-deck.toml"), "")
        .expect("write keyless project config");
    let five = config_dir_with_timeout(5);
    let nine = config_dir_with_timeout(9);
    // 20000 minutes is past the 10080-minute (seven-day) ceiling.
    let huge = config_dir_with_timeout(20_000);
    let no_config = common::race_safe_tempdir();
    let default_120 = Duration::from_secs(120 * 60);

    // 1. Default when the key is absent — the documented 120 minutes.
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&keyless)), None),
        Some(default_120),
        "an absent worker_response_timeout_minutes must resolve to 120 minutes"
    );
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&no_config)), None),
        Some(default_120),
        "a cwd with no .dot-agent-deck.toml at all must also resolve to 120 minutes"
    );

    // 2. The orchestration cwd is consulted BEFORE the worker cwd — they
    //    diverge for PRD #120 issue-dispatch clones, whose worker panes get
    //    their own checkout.
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&five)), Some(&dir_str(&nine))),
        Some(Duration::from_secs(5 * 60)),
        "the orchestration cwd's value must win over the worker cwd's"
    );
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&no_config)), Some(&dir_str(&nine))),
        Some(Duration::from_secs(9 * 60)),
        "with no config in the orchestration cwd, the worker cwd is the fallback"
    );

    // 3. An out-of-range FILE value falls back to the default. Clamping would
    //    have produced the 10080-minute ceiling instead.
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&huge)), None),
        Some(default_120),
        "an out-of-range worker_response_timeout_minutes must fall back to the default, \
         not be clamped to the ceiling"
    );

    // 4. The env seam wins over the file when it is in range.
    env.repoint(Some("700"));
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&five)), None),
        Some(Duration::from_millis(700)),
        "the millisecond seam must override the project config"
    );

    // 5. An out-of-range env value is IGNORED — resolution continues to the
    //    file/default instead of clamping to the nearest bound.
    env.repoint(Some("50")); // below the 100 ms floor
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&five)), None),
        Some(Duration::from_secs(5 * 60)),
        "a below-floor seam value must fall through to the file, not clamp to 100 ms"
    );
    env.repoint(Some("604800001")); // one millisecond past seven days
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&five)), None),
        Some(Duration::from_secs(5 * 60)),
        "an above-ceiling seam value must fall through to the file, not clamp to seven days"
    );
    env.repoint(Some("604800001"));
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&huge)), None),
        Some(default_120),
        "out of range from BOTH sources must land on the default, not on either bound"
    );

    // 6. Zero disables outright, from either source.
    env.repoint(Some("0"));
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&five)), None),
        None,
        "a zero seam value must disable the detector even when the file enables it"
    );
    env.repoint(None);
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&config_dir_with_timeout(0))), None),
        None,
        "worker_response_timeout_minutes = 0 must disable the detector"
    );
    // Boundary values are honored, so the rejection above is about RANGE.
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&config_dir_with_timeout(1))), None),
        Some(Duration::from_secs(60)),
        "the one-minute floor must be honored, not treated as out of range"
    );
    assert_eq!(
        worker_response_timeout(Some(&dir_str(&config_dir_with_timeout(10_080))), None),
        Some(Duration::from_secs(10_080 * 60)),
        "the seven-day ceiling must be honored, not treated as out of range"
    );
}

/// Scenario: Delegate once to a silent worker with a tiny timeout, wait for its idle prompt, then keep the worker open for another timeout window. The orchestrator snapshot must contain exactly one daemon-report prompt, proving the detector does not re-nag.
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

/// Scenario: Delegate to a silent worker, close the ORCHESTRATOR through the real StopAgent request, then spawn a brand-new unrelated agent that inherits the freed orchestrator pane id. After two full timeout windows the new occupant's PTY must still hold only its own readiness marker — the dead orchestration's idle prompt must never be auto-submitted into a stranger's session.
#[spec("scheduler/idle-worker/008")]
#[test]
fn idle_worker_008_closed_orchestrator_pane_id_reuse_receives_nothing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("1200"));
    runtime().block_on(async {
        let harness = IdleHarness::new(&["orphaned-worker"], None).await;
        let server = start_attach_server(&harness).await;
        harness.delegate(&["orphaned-worker"]).await;

        // Close the ORCHESTRATOR. Its pane id is now free for reuse.
        IdleHarness::stop_agent_timed(server.path.clone(), harness.orchestrator_agent_id.clone())
            .await;

        // A different agent — no orchestration membership, no relationship to
        // the delegation — takes the freed pane id, exactly as a fresh spawn
        // reusing a recycled `pane_id_env` would.
        let successor = spawn_raw_cat_observer(
            &harness.registry,
            ORCH_PANE,
            "SUCCESSOR-READY",
            &harness.cwd_str(),
        );
        // Issue #709: a flat 5 s for a freshly spawned `sh` to reach its
        // `printf` is an idle-box number, and this test failed on it once in a
        // 12-run streak on a 16-core box at load average 44. The ceiling is now
        // scaled by that contention and the wait still ends the instant the
        // marker lands, so the assertion underneath is unchanged and an idle box
        // is no slower. The successor's pane id was claimed synchronously by the
        // spawn above, well before the delegation's deadline — readiness only
        // has to precede the SNAPSHOT below, which is what makes absence there
        // evidence.
        let ready = String::from_utf8_lossy(
            &common::wait_for_child_first_output(&harness.registry, &successor, b"SUCCESSOR-READY")
                .await,
        )
        .into_owned();
        assert!(
            ready.contains("SUCCESSOR-READY"),
            "the successor agent never became ready, so it could not have observed a stray \
             submit either; snapshot = {ready:?}"
        );

        // Two full timeout windows: the original delegation's deadline passes
        // while the successor owns the pane and is fully observable.
        tokio::time::sleep(Duration::from_millis(2600)).await;
        let snapshot = harness.snapshot_of(&successor);
        assert!(
            snapshot.contains("SUCCESSOR-READY"),
            "the successor's own output vanished, so absence below proves nothing; \
             snapshot = {snapshot:?}"
        );
        assert_eq!(
            idle_count(&snapshot),
            0,
            "the dead orchestration's idle prompt was auto-submitted into an unrelated agent \
             that merely inherited the pane id; snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains("orphaned-worker"),
            "no fragment of the dead orchestration's delegation may reach the successor; \
             snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: Delegate to a silent worker, then let the ORCHESTRATOR's own process end — no StopAgent, so no close transition and no record sweep ever runs. A brand-new unrelated agent inherits the freed orchestrator pane id before the deadline, and after two further timeout windows its PTY must still hold nothing but its own readiness marker.
#[spec("scheduler/idle-worker/014")]
#[test]
fn idle_worker_014_natural_orchestrator_exit_pane_id_reuse_receives_nothing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("1500"));
    let timeout = Duration::from_millis(1500);
    runtime().block_on(async {
        let harness = IdleHarness::with_orchestrator_stub(
            OrchestratorStub::ExitsOnFlag,
            &[("orphaned-worker", WORKER_COMMAND)],
            None,
        )
        .await;

        let delegated_at = tokio::time::Instant::now();
        harness.delegate(&["orphaned-worker"]).await;

        // The orchestrator simply ENDS. Unlike `008`'s StopAgent, this path
        // never enters `begin_pane_close`, so nothing sweeps the armed record
        // and the identity gate is the only guard left.
        harness.end_orchestrator_process().await;
        assert!(
            !harness.registry.is_pane_closing(ORCH_PANE),
            "the orchestrator pane is in a CLOSE transition, so the close-time record sweep — \
             not the identity gate — would be what suppresses the prompt, and this test would \
             stop covering the natural-exit path"
        );

        // An unrelated agent takes the freed pane id, as any fresh spawn
        // reusing a recycled `pane_id_env` would.
        let successor = spawn_raw_cat_observer(
            &harness.registry,
            ORCH_PANE,
            "SUCCESSOR-READY",
            &harness.cwd_str(),
        );
        let ready = harness
            .wait_for_snapshot_of(
                &successor,
                |snapshot| snapshot.contains("SUCCESSOR-READY"),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            ready.contains("SUCCESSOR-READY"),
            "the successor agent never became ready, so it could not have observed a stray \
             submit either; snapshot = {ready:?}"
        );
        assert!(
            tokio::time::Instant::now() < delegated_at + timeout,
            "the successor only took the pane AFTER the delegation's deadline had already \
             passed, so the timer had nobody to mis-deliver to and this test would pass for the \
             wrong reason: it became ready {:?} after the delegate (timeout {timeout:?})",
            tokio::time::Instant::now() - delegated_at
        );

        // Two further timeout windows: the deadline passes while the successor
        // owns the pane and is fully observable.
        tokio::time::sleep(timeout * 2).await;
        let snapshot = harness.snapshot_of(&successor);
        assert!(
            snapshot.contains("SUCCESSOR-READY"),
            "the successor's own output vanished, so absence below proves nothing; \
             snapshot = {snapshot:?}"
        );
        assert_eq!(
            idle_count(&snapshot),
            0,
            "the dead orchestration's idle prompt was auto-submitted into an unrelated agent \
             that merely inherited the pane id of an orchestrator which exited on its own; \
             snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains("orphaned-worker"),
            "no fragment of the dead orchestration's delegation may reach the successor; \
             snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: Delegate to a worker, let it receive the task pointer and then end its own process on its own — no SIGTERM, no StopAgent, no explicit close of any kind. The daemon's new EOF-triggered notice must appear in the orchestrator's pane well within the (much longer) idle-timeout and silence-window, with neither of the two OLDER timeout-based notices firing instead.
#[spec("scheduler/idle-worker/016")]
#[test]
fn idle_worker_016_natural_worker_exit_retires_records_and_reports_promptly() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    // Both timeout watches resolve from this: the idle-timeout window is set
    // directly, and the silence window is `min(this, 30s)` — 30s here. Both
    // are far longer than the few hundred ms this test actually takes, so a
    // notice appearing before either elapses can only have come from the
    // EOF-triggered sweep, never from either timer running out.
    let _env = EnvGuard::set(Some("60000"));
    runtime().block_on(async {
        let harness = IdleHarness::with_workers(
            &[("vanishing-worker", WORKER_EXITS_ON_ITS_OWN_COMMAND)],
            None,
        )
        .await;

        let worker_pane_id = worker_pane("vanishing-worker");
        let worker_agent_id = harness.worker_agent_ids["vanishing-worker"].clone();

        // Wait for the worker's own readiness marker before delegating — the
        // same precondition every other harness test in this file relies on
        // (a delegate landing before termios is raw could be swallowed).
        let ready = harness
            .wait_for_snapshot_of(
                &worker_agent_id,
                |snapshot| snapshot.contains("WORKER-READY"),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            ready.contains("WORKER-READY"),
            "the vanishing-worker stub never became ready; snapshot = {ready:?}"
        );

        harness.delegate(&["vanishing-worker"]).await;

        // The worker's own script exits on its own shortly after — no
        // StopAgent, no explicit close of any kind. Wait until the registry
        // genuinely has no live owner for its pane, mirroring
        // `end_orchestrator_process`'s freed-pane wait.
        let freed = tokio::time::timeout(Duration::from_secs(5), async {
            while harness
                .registry
                .pane_current_agent_id(&worker_pane_id)
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            freed.is_ok(),
            "the vanishing-worker stub never exited on its own, so the scenario under test \
             could not occur"
        );
        assert!(
            !harness.registry.is_pane_closing(&worker_pane_id),
            "the worker pane is in a CLOSE transition, so a deliberate close's own record sweep \
             — not the EOF-triggered sweep — would be what retired the records, and this test \
             would stop covering the natural-exit path"
        );

        // The notice must land promptly — well before either timeout watch's
        // (60s / 30s) window could have fired it instead.
        let snapshot = harness
            .wait_for_snapshot(
                |snapshot| snapshot.contains(WORKER_EXITED_NEEDLE),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            snapshot.contains(WORKER_EXITED_NEEDLE),
            "no EOF-triggered 'worker exited without work-done' notice appeared in the \
             orchestrator's pane within 5s of the worker's natural exit; snapshot = {snapshot:?}"
        );
        assert!(
            snapshot.contains(&worker_pane_id),
            "the notice must name the exited worker's pane so the orchestrator knows which \
             worker to check; snapshot = {snapshot:?}"
        );
        assert_eq!(
            idle_count(&snapshot),
            0,
            "the OLDER timeout-based idle prompt fired instead of (or alongside) the new \
             EOF-triggered notice, meaning the sweep did not retire the OutstandingDelegation \
             record before its own timer ran out; snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains(SILENCE_NEEDLE),
            "the OLDER timeout-based silence notice fired instead of (or alongside) the new \
             EOF-triggered notice, meaning the sweep did not retire the SilenceWatchRecord \
             before its own timer ran out; snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: Delegate to a silent control worker and to a worker that ignores SIGTERM, then StopAgent the TERM-resistant one so its three-second grace window brackets the detector deadline. The test asserts the overlap actually happened, then requires a prompt for the control and none for the worker whose close was in flight.
#[spec("scheduler/idle-worker/009")]
#[test]
fn idle_worker_009_close_grace_window_suppresses_the_timeout() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("2000"));
    let timeout = Duration::from_millis(2000);
    runtime().block_on(async {
        let harness = IdleHarness::with_workers(
            &[
                ("silent-control", WORKER_COMMAND),
                ("term-resistant-worker", TERM_RESISTANT_WORKER_COMMAND),
            ],
            None,
        )
        .await;
        let server = start_attach_server(&harness).await;

        let delegated_at = tokio::time::Instant::now();
        harness
            .delegate(&["silent-control", "term-resistant-worker"])
            .await;

        // Start the close well inside the timeout window; the SIGTERM grace
        // then keeps the pane closing until well past the deadline.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let stopped_id = harness
            .worker_agent_ids
            .get("term-resistant-worker")
            .expect("term-resistant worker registry id")
            .clone();
        let (close_started, close_finished) =
            IdleHarness::stop_agent_timed(server.path.clone(), stopped_id).await;

        let deadline = delegated_at + timeout;
        assert!(
            close_started < deadline && close_finished > deadline,
            "the close window did not bracket the detector deadline, so this test would pass \
             for the wrong reason: close ran for {:?} starting {:?} after the delegate, \
             timeout {timeout:?}",
            close_finished - close_started,
            close_started - delegated_at
        );

        let snapshot = harness
            .wait_for_idle_role("silent-control", Duration::from_secs(4))
            .await;
        assert!(
            idle_mentions_role(&snapshot, "silent-control"),
            "silent control worker did not prove the detector fired during the close; \
             snapshot = {snapshot:?}"
        );
        assert!(
            !idle_mentions_role(&snapshot, "term-resistant-worker"),
            "a timer fired inside the SIGTERM grace window and nudged the orchestrator about a \
             worker the operator had deliberately closed; snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: Begin closing a SIGTERM-ignoring worker and, while the close is provably still in flight, delegate to that same worker alongside a control. Arming must be refused for the closing pane, so after the timeout the control has a prompt and the closing worker has none.
#[spec("scheduler/idle-worker/010")]
#[test]
fn idle_worker_010_delegate_during_close_refuses_to_arm() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("1500"));
    runtime().block_on(async {
        let harness = IdleHarness::with_workers(
            &[
                ("silent-control", WORKER_COMMAND),
                ("closing-worker", TERM_RESISTANT_WORKER_COMMAND),
            ],
            None,
        )
        .await;
        let server = start_attach_server(&harness).await;

        let stopped_id = harness
            .worker_agent_ids
            .get("closing-worker")
            .expect("closing worker registry id")
            .clone();
        let close = tokio::spawn(IdleHarness::stop_agent_timed(
            server.path.clone(),
            stopped_id,
        ));

        // Barrier: the delegate below must land strictly INSIDE the close
        // transition, which the SIGTERM-ignoring child holds open for the full
        // three-second grace.
        let closing_pane = worker_pane("closing-worker");
        let entered = tokio::time::timeout(Duration::from_secs(5), async {
            while !harness.registry.is_pane_closing(&closing_pane) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(entered.is_ok(), "the pane never entered the closing state");

        harness
            .delegate(&["closing-worker", "silent-control"])
            .await;
        assert!(
            harness.registry.is_pane_closing(&closing_pane),
            "the close transition ended before the delegate landed, so the race this test \
             exists for was never actually held open"
        );

        close.await.expect("StopAgent task");

        let snapshot = harness
            .wait_for_idle_role("silent-control", Duration::from_secs(4))
            .await;
        assert!(
            idle_mentions_role(&snapshot, "silent-control"),
            "silent control worker did not prove the detector fired; snapshot = {snapshot:?}"
        );
        assert!(
            !idle_mentions_role(&snapshot, "closing-worker"),
            "a delegate that landed inside the close transition armed a record the close had \
             already swept past; snapshot = {snapshot:?}"
        );
    });
}

/// Scenario: Delegate twice to each of two worker panes on the same clock, then send ONE late work-done for the first worker (standing in for delegation one's belated completion) and TWO for the second. The first worker's delegation two must still be reported — on its own deadline, once — while the second worker, whose remaining delegation the second completion retired, must produce nothing at all.
#[spec("scheduler/idle-worker/013")]
#[test]
fn idle_worker_013_late_first_completion_leaves_the_second_watch_armed() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(Some("1500"));
    let timeout = Duration::from_millis(1500);
    runtime().block_on(async {
        // `fully-completed-worker` is the BEHAVIORAL CONTROL for the retirement
        // itself. Asserting only that delegation two survives a late completion
        // would pass just as happily if `work-done` retired NOTHING — the
        // surviving watch proves nothing about which record was consumed. Its
        // second completion must consume the record the first one left armed, so
        // a `work-done` that did nothing shows up here as an extra prompt.
        // Listed FIRST in both delegate calls so its (slightly earlier) deadline
        // could not hide behind the other worker's.
        let harness =
            IdleHarness::new(&["fully-completed-worker", "twice-delegated-worker"], None).await;

        harness
            .delegate(&["fully-completed-worker", "twice-delegated-worker"])
            .await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        let second_delegate_at = tokio::time::Instant::now();
        harness
            .delegate(&["fully-completed-worker", "twice-delegated-worker"])
            .await;

        // Each worker's delegation #1 finally reports, 300 ms after it was
        // superseded. It owes exactly one retirement — and it must be #1's.
        tokio::time::sleep(Duration::from_millis(300)).await;
        harness.work_done("fully-completed-worker").await;
        harness.work_done("twice-delegated-worker").await;
        // Only the control's delegation #2 also reports.
        harness.work_done("fully-completed-worker").await;

        let observed = harness
            .wait_for_idle_role("twice-delegated-worker", Duration::from_secs(4))
            .await;
        let observed_at = tokio::time::Instant::now();
        assert!(
            idle_mentions_role(&observed, "twice-delegated-worker"),
            "delegation one's late work-done disarmed delegation two, so a re-delegated worker \
             that then went silent was never reported; snapshot = {observed:?}"
        );
        assert!(
            observed_at >= second_delegate_at + timeout - Duration::from_millis(250),
            "the prompt arrived on delegation ONE's clock rather than delegation two's, \
             {:?} after the second delegate (timeout {timeout:?})",
            observed_at - second_delegate_at
        );

        // Settle before the negative assertions: the control's deadline is a
        // hair EARLIER than the reported worker's, so anything it was going to
        // emit has had its window and then some.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = harness.wait_for_snapshot(|_| true, Duration::ZERO).await;
        assert!(
            !idle_mentions_role(&snapshot, "fully-completed-worker"),
            "the second work-done did not retire the delegation the first one deliberately left \
             armed — a work-done that retired NOTHING at all would look exactly like this; \
             snapshot = {snapshot:?}"
        );
        assert_eq!(
            idle_count(&snapshot),
            1,
            "exactly one of the four delegations may report; snapshot = {snapshot:?}"
        );
    });
}
