#![cfg(feature = "e2e")]

//! L2 real-agent chain test for #187 / PR #188.
//!
//! The bug: when the orchestrator delegates, the daemon injects a prompt
//! into the worker pane. Before the fix that prompt was multi-line (it
//! carried the `## When done` footer), so `encode_pane_payload`
//! bracketed-paste-wrapped it and Claude Code parked it as a compacted
//! block awaiting a manual Enter — the worker never started unattended.
//! The fix keeps the injected prompt to a single-line pointer and moves
//! the footer into the worker task file.
//!
//! This test proves the empirical end of that fix that unit/integration
//! tests cannot: a real, long-running interactive worker agent
//! AUTO-SUBMITS the daemon-injected single-line prompt (no human Enter).
//!
//! Two arms, each skipped when its CLI/credentials are absent (Decision
//! 26 runtime-skip), each a cheap single invocation well under Decision
//! 23's <$0.05/run bound. Local-only (Decision 8): gated behind the `e2e`
//! feature so CI (`cargo test-fast`) never compiles it.
//!
//! - **Claude (Haiku)** runs the FULL loop: real `handle_delegate` →
//!   single-line pointer injected → Claude auto-submits, reads its task
//!   file, performs a trivial task, and runs `dot-agent-deck work-done`;
//!   the observable is the daemon-written `.dot-agent-deck/work-done-*.md`.
//!
//! - **OpenCode** confirms only the AUTO-SUBMIT half. OpenCode's own
//!   permission sandbox gates `.dot-agent-deck` reads / shell runs
//!   ("Access external directory …"), which is orthogonal to #187 — so
//!   rather than configure that sandbox, the OpenCode arm injects a
//!   purely conversational prompt through the SAME
//!   `write_to_pane_and_submit` primitive the delegate dispatch uses and
//!   asserts the model's reply renders. (Verified 2026-06-22: OpenCode
//!   does auto-submit; the only thing that blocked the full loop was its
//!   tool-permission sandbox, not #187, not the account, not PRD #79.)

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::event::{BroadcastMsg, DelegateSignal};
use dot_agent_deck::state::{AppState, SharedState};

mod common;

/// Serializes the socket-bind window across the harness (mirrors the
/// pattern in `spawn_time_role_prompt_submit_after_session_start.rs`).
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

const ORCH_PANE: &str = "orchestrator-pane";
const WORKER_PANE: &str = "worker-pane";
const WORKER_ROLE: &str = "coder";
const PINNED_CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";

struct DaemonHandle {
    _dir: TempDir,
    state: SharedState,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    hook_path: PathBuf,
    handle: JoinHandle<()>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.handle.abort();
        self.registry.shutdown_all();
    }
}

/// Bring up an in-process daemon whose hook loop will dispatch our
/// `Delegate` and ingest the worker's `WorkDone`. Returns handles to the
/// shared state (so the test can populate the orchestration maps), the PTY
/// registry (to spawn the worker), the broadcast sender, and the hook
/// socket path (handed to the worker via `DOT_AGENT_DECK_SOCKET`).
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
    let daemon = Daemon::with_attach(state.clone(), attach_path.clone())
        .with_idle_shutdown(None)
        .with_lock_dir_override(common::lock_dir_path());
    let registry = daemon.pty_registry.clone();
    let event_tx = daemon.event_tx.clone();

    let hook_for_daemon = hook_path.clone();
    let handle = tokio::spawn(async move {
        let _ = run_daemon_with(&hook_for_daemon, daemon).await;
    });

    // Wait until the hook socket accepts connections so the worker's
    // `work-done` call can't race daemon startup.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while tokio::time::Instant::now() < deadline {
        if hook_path.exists() && UnixStream::connect(&hook_path).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        ready,
        "daemon hook socket was not accepting connections within 5s"
    );

    DaemonHandle {
        _dir: dir,
        state,
        registry,
        event_tx,
        hook_path,
        handle,
    }
}

/// Poll the worker's PTY snapshot until its length stops growing for
/// `quiet`, i.e. the agent has finished rendering its initial UI and is
/// waiting for input. Capped at `timeout`. Delegating only after the
/// worker is input-ready mirrors production, where delegates target a
/// long-running worker rather than one mid-boot.
async fn wait_until_worker_ready(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    quiet: Duration,
    timeout: Duration,
) {
    let start = tokio::time::Instant::now();
    let deadline = start + timeout;
    // Don't treat an early boot lull as "ready": a TUI agent can pause
    // briefly mid-init before its input is interactive, and injecting then
    // gets the bytes dropped. Require a minimum settle since first output
    // in addition to the quiet window.
    let min_since_first_output = Duration::from_secs(6);
    let mut last_len = 0usize;
    let mut first_output_at: Option<tokio::time::Instant> = None;
    let mut stable_since = start;
    loop {
        let len = registry.snapshot(agent_id).map(|s| s.len()).unwrap_or(0);
        if len > 0 && first_output_at.is_none() {
            first_output_at = Some(tokio::time::Instant::now());
        }
        if len != last_len {
            last_len = len;
            stable_since = tokio::time::Instant::now();
        } else if let Some(first) = first_output_at
            && stable_since.elapsed() >= quiet
            && first.elapsed() >= min_since_first_output
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll for `path` to exist, up to `timeout`.
async fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll the worker pane until its rendered screen contains `needle`,
/// returning `(found, rendered_screen)`. The PTY byte stream is replayed
/// through a `vt100` grid so a streamed/redrawn reply (the agent prints
/// token-by-token with cursor moves) is matched on its final rendered
/// state rather than on raw, escape-interleaved bytes.
async fn wait_for_rendered_text(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &str,
    timeout: Duration,
) -> (bool, String) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snap = registry.snapshot(agent_id).unwrap_or_default();
        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(&snap);
        let screen = parser.screen().contents();
        if screen.contains(needle) {
            return (true, screen);
        }
        if tokio::time::Instant::now() >= deadline {
            return (false, screen);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Build an isolated `HOME` for a Claude Code worker that (a) carries the
/// host credentials/onboarding so auth works without a fresh login flow,
/// and (b) pre-marks `worker_cwd` as a trusted folder so Claude does NOT
/// show its first-run "Is this a project you trust?" dialog. In production
/// a worker pane runs in the user's already-trusted repo, so the dialog
/// never appears; a fresh tempdir cwd would otherwise trip it and swallow
/// the injected delegate prompt. The returned TempDir must be kept alive
/// for the worker's lifetime.
fn prepare_claude_home(worker_cwd: &str) -> TempDir {
    let host_home = std::env::var("HOME").expect("HOME is set");
    let home = common::race_safe_tempdir();

    // Carry the host credentials so the worker authenticates as the host
    // user (mirrors the harness's `with_imported_claude_credentials`).
    std::fs::create_dir_all(home.path().join(".claude")).expect("mk .claude");
    std::fs::copy(
        Path::new(&host_home)
            .join(".claude")
            .join(".credentials.json"),
        home.path().join(".claude").join(".credentials.json"),
    )
    .expect("copy claude credentials");

    // Start from the host's global config (preserves oauthAccount +
    // hasCompletedOnboarding so the worker skips the global onboarding
    // flow), then add this cwd as a trusted project.
    let host_cfg_path = Path::new(&host_home).join(".claude.json");
    let mut cfg: serde_json::Value = std::fs::read_to_string(&host_cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "hasCompletedOnboarding": true }));
    if !cfg["projects"].is_object() {
        cfg["projects"] = serde_json::json!({});
    }
    cfg["projects"][worker_cwd] = serde_json::json!({
        "hasTrustDialogAccepted": true,
        "hasCompletedProjectOnboarding": true,
        "projectOnboardingSeenCount": 1,
    });
    std::fs::write(
        home.path().join(".claude.json"),
        serde_json::to_string(&cfg).expect("serialize .claude.json"),
    )
    .expect("write isolated .claude.json");

    home
}

/// Shared body for both arms: spawn `worker_command` as a long-running
/// interactive worker, delegate a trivial task to it, and assert the
/// daemon writes the work-done file — proving the worker auto-submitted
/// the single-line prompt and followed the task-file footer.
async fn run_delegate_work_done_loop(worker_command: &str, seed_claude_trust: bool) {
    let daemon = spawn_daemon().await;

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("worker cwd is UTF-8")
        .to_string();

    // The worker runs `dot-agent-deck work-done` from the footer, so the
    // freshly built test binary must be on its PATH, and it needs the hook
    // socket via DOT_AGENT_DECK_SOCKET. The PTY child inherits the rest of
    // the environment (HOME → agent credentials), so we only overlay these.
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("bin dir is UTF-8");
    let path_env = format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default());

    let mut worker_env = vec![
        (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
        (
            "DOT_AGENT_DECK_SOCKET".to_string(),
            daemon.hook_path.display().to_string(),
        ),
        ("PATH".to_string(), path_env),
    ];

    // For Claude: point the worker at an isolated HOME that pre-trusts the
    // tempdir cwd, so the first-run trust dialog never appears and the
    // injected delegate prompt lands in the input box rather than being
    // consumed answering the dialog. Held alive until the worker exits.
    let _claude_home = if seed_claude_trust {
        let home = prepare_claude_home(&cwd_str);
        worker_env.push((
            "HOME".to_string(),
            home.path().to_str().expect("home path UTF-8").to_string(),
        ));
        Some(home)
    } else {
        None
    };

    let worker_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(worker_command),
            cwd: Some(cwd_str.as_str()),
            rows: 40,
            cols: 120,
            env: worker_env,
            ..SpawnOptions::default()
        })
        .expect("spawn worker agent");

    // Register the orchestration maps `handle_delegate`/`handle_work_done`
    // read, exactly as StartAgent would for a live orchestration tab.
    {
        let mut st = daemon.state.write().await;
        st.pane_role_map
            .insert(ORCH_PANE.to_string(), "orchestrator".to_string());
        st.pane_role_map
            .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
        st.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        let orch = ("test-orchestration".to_string(), cwd_str.clone());
        st.pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orch.clone());
        st.pane_orchestration_map
            .insert(WORKER_PANE.to_string(), orch);
        st.pane_cwd_map
            .insert(WORKER_PANE.to_string(), cwd_str.clone());
    }

    // Let the interactive agent reach input-readiness before delegating.
    wait_until_worker_ready(
        &daemon.registry,
        &worker_agent_id,
        Duration::from_millis(1500),
        Duration::from_secs(30),
    )
    .await;

    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: "List the files in the current directory using the Bash tool (for example `ls -a`). \
               That is the entire task — do not do anything else."
            .to_string(),
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };
    daemon
        .state
        .read()
        .await
        .handle_delegate(signal, &daemon.registry, &daemon.event_tx)
        .await;

    // The work-done file is written by `handle_work_done` only after the
    // worker auto-submitted the injected prompt and ran the work-done CLI.
    let work_done = cwd
        .path()
        .join(".dot-agent-deck")
        .join("work-done-coder.md");
    let ok = wait_for_path(&work_done, Duration::from_secs(120)).await;
    let snap = daemon
        .registry
        .snapshot(&worker_agent_id)
        .unwrap_or_default();
    let pane = String::from_utf8_lossy(&snap);
    // Self-diagnosing failure signal: did the injected prompt reach the
    // pane, and did the agent surface an API/account error (e.g. quota)?
    // Distinguishes "fix the test" from "fix the account/credentials".
    let prompt_reached = pane.contains("worker-task-coder.md");
    let lower = pane.to_lowercase();
    let agent_errored = ["quota", "exceeded", "billing", "unauthorized", "rate limit"]
        .iter()
        .any(|k| lower.contains(k));
    assert!(
        ok,
        "worker never produced {work_done:?}. prompt_reached_pane={prompt_reached} \
         agent_api_error_in_pane={agent_errored}. If prompt_reached=true and \
         agent_api_error=true, the agent's account/quota is the blocker, not the delegate \
         path.\n=== worker pane AFTER delegate (full) ===\n{pane}\n=== end after ==="
    );
}

/// Scenario: Start a real Claude Code (Haiku) worker as a long-running
/// interactive agent under an in-process daemon, register it as a `coder`
/// role in an orchestration, then call the daemon's real `handle_delegate`
/// with a trivial "list the files" task. The single-line file-pointer
/// prompt is injected into the worker's PTY; the worker must auto-submit
/// it (no manual Enter), read its task file, list the files, and run
/// `dot-agent-deck work-done`. Assert the daemon writes
/// `.dot-agent-deck/work-done-coder.md`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegate_work_done_chain_claude() {
    skip_unless!(common::check_claude_available());
    let command = format!("claude --model {PINNED_CLAUDE_MODEL} --allowedTools Bash Read");
    run_delegate_work_done_loop(&command, true).await;
}

/// Scenario: Confirm a real OpenCode worker AUTO-SUBMITS a daemon-injected
/// single-line prompt — the exact #187 mechanism, for a second non-Claude
/// agent (the case PR #188 did not claim). Unlike the Claude arm this does
/// NOT run the full work-done loop: OpenCode's permission sandbox gates
/// `.dot-agent-deck` reads and shell runs (it prompts "Access external
/// directory …" / tool approval), which is orthogonal to #187. Instead we
/// inject a purely conversational prompt via the SAME
/// `write_to_pane_and_submit` primitive the delegate dispatch uses, and
/// assert the model's reply renders — proving the single-line prompt was
/// submitted without a manual Enter, with no tool permissions involved.
///
/// The answer token (`4444`) is absent from the prompt, so finding it in
/// the rendered pane proves the prompt was submitted and answered, not
/// merely echoed into the input box.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opencode_auto_submits_daemon_injected_prompt() {
    skip_unless!(common::check_opencode_available());

    let registry = Arc::new(AgentPtyRegistry::new());
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("worker cwd is UTF-8")
        .to_string();

    // OpenCode model ids are provider-qualified (`provider/model`); a bare
    // `gpt-4o-mini` is rejected as "Invalid model format". A small model is
    // plenty for a one-line arithmetic reply.
    let worker_agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("opencode --model openrouter/openai/gpt-4o-mini"),
            cwd: Some(cwd_str.as_str()),
            rows: 40,
            cols: 120,
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn opencode worker");

    wait_until_worker_ready(
        &registry,
        &worker_agent_id,
        Duration::from_millis(1500),
        Duration::from_secs(30),
    )
    .await;

    registry
        .write_to_pane_and_submit(
            WORKER_PANE,
            "Reply with only the number equal to 4000 plus 444.",
        )
        .await
        .expect("inject prompt into opencode worker");

    let (ok, screen) =
        wait_for_rendered_text(&registry, &worker_agent_id, "4444", Duration::from_secs(90)).await;
    assert!(
        ok,
        "OpenCode did not auto-submit the daemon-injected single-line prompt \
         (no '4444' reply rendered). Rendered screen:\n{screen}"
    );

    registry.shutdown_all();
}
