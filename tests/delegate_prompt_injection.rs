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

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, GuardedSend, SpawnOptions, TabMembership,
};
use dot_agent_deck::event::{
    AgentEvent, AgentType, BroadcastMsg, DelegateSignal, EventType, WorkDoneSignal,
};
use dot_agent_deck::state::{AppState, OrchestrationIdentity};
#[cfg(unix)]
use spec::spec;

mod common;

const ORCH_PANE: &str = "orchestrator-pane";
const WORKER_PANE: &str = "worker-pane";
const WORKER_ROLE: &str = "coder";
const POINTER: &[u8] = b"Read .dot-agent-deck/worker-task-coder.md for your task.";
const SESSION_START_ORIGIN_METADATA_KEY: &str = "session_start_origin";
const WRAPPER_FORK_SESSION_START_ORIGIN: &str = "wrapper_fork";
const DELEGATE_READINESS_BUFFER_ENV: &str = "DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS";
const DELEGATE_NO_EVENT_WINDOW_ENV: &str = "DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS";
const SESSION_START_WAIT_ENV: &str = "DOT_AGENT_DECK_SESSION_START_WAIT_MS";
const WORKER_RESPONSE_TIMEOUT_ENV: &str = "DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS";
const DELEGATE_READINESS_BUFFER_MS: u64 = 1000;
const SLOW_STUB_NOT_READY_MS: u64 = 650;

/// Serializes process-environment changes when this integration-test binary is
/// run through plain `cargo test`; nextest already gives each test a process.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let mut previous = Vec::with_capacity(values.len());
        for (key, value) in values {
            previous.push((*key, std::env::var_os(key)));
            // SAFETY: every env-mutating test in this integration-test binary
            // holds ENV_LOCK for the guard's full lifetime.
            unsafe { std::env::set_var(key, value) };
        }
        Self { previous }
    }

    fn repoint(&self, key: &'static str, value: &str) {
        assert!(
            self.previous.iter().any(|(saved, _)| *saved == key),
            "cannot repoint an environment key this guard does not own: {key}"
        );
        // SAFETY: the caller still holds ENV_LOCK while this guard is alive.
        unsafe { std::env::set_var(key, value) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.previous.drain(..).rev() {
            // SAFETY: the caller still holds ENV_LOCK while this guard drops.
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

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

fn snapshot_contains(snapshot: &[u8], needle: &[u8]) -> bool {
    snapshot
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Poll a condition on REAL wall-clock time after a paused Tokio clock has
/// been advanced past a virtual deadline (#346).
///
/// The write that deadline unblocks still needs a genuine round trip through
/// a real child process and the detached `pump_reader` OS thread (see
/// `pump_reader` in `agent_pty.rs`) before it is observable in a snapshot —
/// neither of which is affected by `tokio::time::pause`/`advance`, since both
/// live entirely outside the Tokio runtime. A single fixed `std::thread::sleep`
/// guess for that round trip (the pattern this replaces) is tight enough to
/// lose the race under nextest's full-parallel load, where dozens of other
/// test processes are contending for the same CPU. Poll with a generous real
/// budget instead, yielding to the runtime each iteration so any pending
/// async steps (like the notice/pointer write itself) keep making progress.
async fn poll_until_after_time_advance(
    real_timeout: Duration,
    mut condition: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + real_timeout;
    loop {
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return condition();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Extra virtual time an `advance` needs in order to CROSS a timer that was
/// armed exactly `nominal` ago on a paused clock (#402).
///
/// Tokio's timer wheel counts whole milliseconds: a deadline is rounded UP to
/// the next tick (`deadline_to_tick` adds 999_999 ns before truncating) while
/// `now` is truncated DOWN to one. A timer armed at real offset `a` therefore
/// expires at tick `ceil(a) + nominal_ms`, but `advance(nominal)` only moves
/// `now` to tick `floor(a + delta + nominal)`, where `delta` is the REAL time
/// that elapsed between the daemon task arming the timer and this test calling
/// `tokio::time::pause()`. On the delegate path `delta` is "whatever is left of
/// a 20 ms `wait_for_replacement_agent` poll tick once the respawn finished" —
/// a load-dependent coin flip. When it lands under a millisecond,
/// `advance(nominal)` stops one tick SHORT and the timer does not fire.
///
/// The resulting failure is silent and total rather than off-by-a-little: the
/// fallback then fires on the NEXT advance instead, so the readiness buffer is
/// armed a full advance late, its deadline sits beyond every remaining advance,
/// nothing is ever written to the worker PTY, and the final assertion reports an
/// EMPTY snapshot. No amount of real-clock polling recovers it, because the
/// paused clock never moves again. One whole tick of overshoot makes the
/// crossing a property of the advance instead of host scheduling; two is
/// used so the margin does not itself depend on where `a` fell inside its tick.
#[cfg(unix)]
const TIMER_TICK_SLACK: Duration = Duration::from_millis(2);

/// `tokio::time::advance`, then drain the runtime so the task woken by the
/// crossed deadline reaches ITS next await before the caller advances again.
///
/// `advance` yields exactly once. Every later advance in the
/// `orchestration/delegate/011` scenarios is measured from the instant the
/// delegate task arms the readiness-buffer sleep, so "the woken task actually
/// ran" is a precondition of those advances, not a nicety — if it slips to the
/// following advance the buffer deadline moves with it and the pointer is never
/// delivered inside the test's virtual budget. Yields only: no real sleep, so
/// the paused clock stays exactly where the caller put it.
#[cfg(unix)]
async fn advance_and_run(duration: Duration) {
    tokio::time::advance(duration).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
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
fn clear_true_config(command: &str) -> String {
    format!(
        "[[orchestrations]]\nname = \"test-orchestration\"\n\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"true\"\nstart = true\n\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = \"{command}\"\nclear = true\n"
    )
}

#[cfg(unix)]
fn write_slow_readiness_stub(path: &std::path::Path) {
    let script = format!(
        r#"#!/usr/bin/env python3
import os
import sys
import termios
import time

fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
new = list(old)
new[0] &= ~(termios.IGNBRK | termios.BRKINT | termios.PARMRK
            | termios.ISTRIP | termios.INLCR | termios.IGNCR
            | termios.ICRNL | termios.IXON)
new[1] &= ~termios.OPOST
new[3] &= ~(termios.ECHO | termios.ECHONL | termios.ICANON
            | termios.ISIG | termios.IEXTEN)
termios.tcsetattr(fd, termios.TCSANOW, new)

os.write(1, b'DELEGATE-STUB-RAW-READY')
os.set_blocking(fd, False)
deadline = time.monotonic() + {seconds}
while time.monotonic() < deadline:
    try:
        os.read(fd, 4096)
    except BlockingIOError:
        pass
    time.sleep(0.005)
os.set_blocking(fd, True)

os.write(1, b'DELEGATE-STUB-CAT-READY')
while True:
    data = os.read(fd, 4096)
    if not data:
        break
    os.write(1, data)
"#,
        seconds = SLOW_STUB_NOT_READY_MS as f64 / 1000.0,
    );
    write_executable(path, &script);
}

#[cfg(unix)]
fn snapshot_has_silence_notice(snapshot: &[u8]) -> bool {
    let text = String::from_utf8_lossy(snapshot);
    text.contains("delegated worker went quiet (dot-agent-deck daemon report)")
        && text.contains("emitted no agent event")
}

#[cfg(unix)]
fn silence_notice_terminator(snapshot: &[u8]) -> Option<u8> {
    let marker = b"delegated worker went quiet (dot-agent-deck daemon report)";
    let start = snapshot
        .windows(marker.len())
        .position(|window| window == marker)?;
    snapshot[start..]
        .iter()
        .copied()
        .find(|byte| matches!(byte, b'\r' | b'\n'))
}

#[cfg(unix)]
async fn wait_for_silence_notice(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    timeout: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = registry.snapshot(agent_id).unwrap_or_default();
        if (snapshot_has_silence_notice(&snapshot)
            && silence_notice_terminator(&snapshot).is_some())
            || tokio::time::Instant::now() >= deadline
        {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
struct SlowReadinessResult {
    snapshot: Vec<u8>,
    measured_readiness_window: Duration,
}

#[cfg(unix)]
async fn run_slow_readiness_delegate(buffer_ms: u64) -> SlowReadinessResult {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let stub = cwd.path().join("slow-readiness-agent.py");
    write_slow_readiness_stub(&stub);
    let command = stub.to_string_lossy().into_owned();
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config(&command),
    )
    .expect("write slow-readiness orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let old_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn initial slow-readiness stand-in");
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
    let raw_ready = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        b"DELEGATE-STUB-RAW-READY",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        snapshot_contains(&raw_ready, b"DELEGATE-STUB-RAW-READY"),
        "replacement slow-readiness stub never entered raw discard mode; snapshot = {:?}",
        String::from_utf8_lossy(&raw_ready)
    );

    let session_start_at = Instant::now();
    let event = session_start_event(AgentType::None, WORKER_PANE, &new_agent_id, false);
    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&event).expect("serialize slow-stub SessionStart"),
    )
    .expect("write slow-stub SessionStart");
    let cat_ready = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        b"DELEGATE-STUB-CAT-READY",
        Duration::from_secs(2),
    )
    .await;
    let measured_readiness_window = session_start_at.elapsed();
    assert!(
        snapshot_contains(&cat_ready, b"DELEGATE-STUB-CAT-READY"),
        "slow-readiness stub did not become input-aware; snapshot = {:?}",
        String::from_utf8_lossy(&cat_ready)
    );

    let mut submitted_pointer = POINTER.to_vec();
    submitted_pointer.push(b'\r');
    let snapshot = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        &submitted_pointer,
        Duration::from_millis(buffer_ms + 1200),
    )
    .await;
    SlowReadinessResult {
        snapshot,
        measured_readiness_window,
    }
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
    let orchestration = OrchestrationIdentity::NameCwd {
        name: "test-orchestration".to_string(),
        cwd: cwd.to_string(),
    };
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
    let orchestration = OrchestrationIdentity::NameCwd {
        name: "test-orchestration".to_string(),
        cwd: cwd_str.clone(),
    };
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
    let bin = dot_agent_deck::platform::paths::binary_name();
    assert_ne!(
        bin, "dot-agent-deck",
        "this assertion only proves anything when the test binary's own file name differs \
         from the literal the pre-fix code always emitted"
    );
    assert!(
        file_body.contains("## When done")
            && file_body.contains(&format!("{bin} work-done --task")),
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
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (
            DELEGATE_READINESS_BUFFER_ENV,
            &DELEGATE_READINESS_BUFFER_MS.to_string(),
        ),
        (SESSION_START_WAIT_ENV, "5000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
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
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (
            DELEGATE_READINESS_BUFFER_ENV,
            &DELEGATE_READINESS_BUFFER_MS.to_string(),
        ),
        (SESSION_START_WAIT_ENV, "5000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
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

    let released_at = Instant::now();
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
    assert!(
        released_at.elapsed() < Duration::from_secs(2),
        "the 1000 ms delegate readiness buffer consumed the full two-second prompt-release budget"
    );
    registry.shutdown_all();
}

/// Scenario: Delegate with `clear = true`, emit the replacement worker's matching `SessionStart`, and force a 1000 ms readiness buffer. The task pointer must remain absent early in that interval and appear after the buffer elapses.
#[spec("orchestration/delegate/010")]
#[test]
#[cfg(unix)]
fn delegate_010_observed_session_start_waits_for_readiness_buffer() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (
            DELEGATE_READINESS_BUFFER_ENV,
            &DELEGATE_READINESS_BUFFER_MS.to_string(),
        ),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build observed readiness runtime")
        .block_on(delegate_010_observed_session_start_waits_for_readiness_buffer_inner());
}

#[cfg(unix)]
async fn delegate_010_observed_session_start_waits_for_readiness_buffer_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config("cat"),
    )
    .expect("write observed-readiness orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let old_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn initial observed-readiness worker");
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

    let session_start_at = Instant::now();
    let event = session_start_event(AgentType::None, WORKER_PANE, &new_agent_id, false);
    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&event).expect("serialize matching SessionStart"),
    )
    .expect("write matching SessionStart");
    tokio::time::sleep(Duration::from_millis(350)).await;
    let early = daemon.registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        !snapshot_contains(&early, POINTER),
        "matching SessionStart released delegate delivery before the configured 1000 ms readiness buffer elapsed; elapsed = {:?}, snapshot = {:?}",
        session_start_at.elapsed(),
        String::from_utf8_lossy(&early)
    );

    let delivered = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        POINTER,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        snapshot_contains(&delivered, POINTER),
        "delegate pointer was not delivered after the observed-branch readiness buffer elapsed; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
}

/// Scenario: Delegate with `clear = true` to workers that never emit `SessionStart` and advance a paused Tokio clock across the fallback timeout. A 1000 ms buffer must hold through its boundary, and a separate 1 ms case must still wait rather than collapsing to `sleep(0)`.
#[spec("orchestration/delegate/011")]
#[test]
#[cfg(unix)]
fn delegate_011_timeout_fallback_also_waits_for_readiness_buffer() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let env = EnvGuard::set(&[
        (
            DELEGATE_READINESS_BUFFER_ENV,
            &DELEGATE_READINESS_BUFFER_MS.to_string(),
        ),
        (SESSION_START_WAIT_ENV, "30000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build timeout-fallback readiness runtime")
        .block_on(async {
            delegate_011_timeout_fallback_also_waits_for_readiness_buffer_inner().await;
            env.repoint(DELEGATE_READINESS_BUFFER_ENV, "1");
            delegate_011_one_millisecond_buffer_is_a_real_wait_inner().await;
            env.repoint(DELEGATE_READINESS_BUFFER_ENV, " 1 \t");
            delegate_011_one_millisecond_buffer_is_a_real_wait_inner().await;
            env.repoint(DELEGATE_READINESS_BUFFER_ENV, "18446744073709551616");
            delegate_011_overflow_buffer_clamps_to_thirty_seconds_inner().await;
        });
}

#[cfg(unix)]
async fn delegate_011_timeout_fallback_also_waits_for_readiness_buffer_inner() {
    common::init_test_env();
    let cwd = common::race_safe_tempdir();
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config("cat"),
    )
    .expect("write timeout-fallback orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    let old_agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn initial timeout-fallback worker");
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

    tokio::time::pause();
    // Cross the 30 s `SessionStart` fallback, with a tick of overshoot so the
    // crossing does not depend on how much real time happened to pass between
    // the daemon task arming that timeout and the `pause()` above — see
    // `TIMER_TICK_SLACK` (#402). Every advance below is relative to the instant
    // the readiness-buffer sleep is armed, which is this one.
    advance_and_run(Duration::from_secs(30) + TIMER_TICK_SLACK).await;
    std::thread::sleep(Duration::from_millis(100));
    let after_timeout = registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        !snapshot_contains(&after_timeout, POINTER),
        "timeout fallback wrote the delegate pointer immediately after its SessionStart wait instead of honoring the additional 1000 ms readiness buffer; snapshot = {:?}",
        String::from_utf8_lossy(&after_timeout)
    );

    advance_and_run(Duration::from_millis(DELEGATE_READINESS_BUFFER_MS - 2)).await;
    std::thread::sleep(Duration::from_millis(100));
    let just_before_buffer = registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        !snapshot_contains(&just_before_buffer, POINTER),
        "timeout fallback released delegate delivery just short of the configured 1000 ms readiness buffer; snapshot = {:?}",
        String::from_utf8_lossy(&just_before_buffer)
    );

    tokio::time::advance(Duration::from_millis(3)).await;
    poll_until_after_time_advance(Duration::from_secs(2), || {
        snapshot_contains(
            &registry.snapshot(&new_agent_id).unwrap_or_default(),
            POINTER,
        )
    })
    .await;
    let delivered = registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        snapshot_contains(&delivered, POINTER),
        "delegate pointer was not delivered after the timeout-fallback readiness buffer elapsed; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    registry.shutdown_all();
}

#[cfg(unix)]
async fn delegate_011_one_millisecond_buffer_is_a_real_wait_inner() {
    common::init_test_env();
    let cwd = common::race_safe_tempdir();
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config("cat"),
    )
    .expect("write one-millisecond orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    let old_agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn initial one-millisecond worker");
    let (event_tx, _rx) = broadcast::channel::<BroadcastMsg>(64);
    let mut state = AppState::default();
    register_orchestration(&mut state, &cwd_str);
    state
        .handle_delegate(
            DelegateSignal {
                pane_id: ORCH_PANE.to_string(),
                task: "List the files in the current directory.".to_string(),
                to: vec![WORKER_ROLE.to_string()],
                timestamp: chrono::Utc::now(),
            },
            &registry,
            &event_tx,
        )
        .await;
    let new_agent_id = wait_for_replacement_agent(&registry, WORKER_PANE, &old_agent_id).await;

    // `TIMER_TICK_SLACK` (#402): same fallback crossing as the scenario above,
    // and the 1 ms buffer armed here leaves even less room to absorb a miss.
    advance_and_run(Duration::from_secs(30) + TIMER_TICK_SLACK).await;
    std::thread::sleep(Duration::from_millis(50));
    let at_fallback = registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        !snapshot_contains(&at_fallback, POINTER),
        "BUFFER_MS=1 collapsed to a zero wait at the timeout fallback; snapshot = {:?}",
        String::from_utf8_lossy(&at_fallback)
    );

    tokio::time::advance(Duration::from_millis(2)).await;
    poll_until_after_time_advance(Duration::from_secs(2), || {
        snapshot_contains(
            &registry.snapshot(&new_agent_id).unwrap_or_default(),
            POINTER,
        )
    })
    .await;
    let delivered = registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        snapshot_contains(&delivered, POINTER),
        "the one-millisecond readiness buffer never released after virtual time crossed its rounded deadline; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    registry.shutdown_all();
}

#[cfg(unix)]
async fn delegate_011_overflow_buffer_clamps_to_thirty_seconds_inner() {
    common::init_test_env();
    let cwd = common::race_safe_tempdir();
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config("cat"),
    )
    .expect("write overflow-buffer orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    let old_agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn initial overflow-buffer worker");
    let (event_tx, _rx) = broadcast::channel::<BroadcastMsg>(64);
    let mut state = AppState::default();
    register_orchestration(&mut state, &cwd_str);
    state
        .handle_delegate(
            DelegateSignal {
                pane_id: ORCH_PANE.to_string(),
                task: "List the files in the current directory.".to_string(),
                to: vec![WORKER_ROLE.to_string()],
                timestamp: chrono::Utc::now(),
            },
            &registry,
            &event_tx,
        )
        .await;
    let new_agent_id = wait_for_replacement_agent(&registry, WORKER_PANE, &old_agent_id).await;

    // `TIMER_TICK_SLACK` (#402): the fallback crossing again. The 1001 ms step
    // below still straddles the 1000 ms default it must NOT have fallen back
    // to, because it is measured from the instant this advance arms the buffer.
    advance_and_run(Duration::from_secs(30) + TIMER_TICK_SLACK).await;
    advance_and_run(Duration::from_millis(1001)).await;
    std::thread::sleep(Duration::from_millis(50));
    let after_default = registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        !snapshot_contains(&after_default, POINTER),
        "an above-u64 readiness value fell back to the 1000 ms default instead of clamping to 30 s; snapshot = {:?}",
        String::from_utf8_lossy(&after_default)
    );

    tokio::time::advance(Duration::from_millis(29_001)).await;
    poll_until_after_time_advance(Duration::from_secs(2), || {
        snapshot_contains(
            &registry.snapshot(&new_agent_id).unwrap_or_default(),
            POINTER,
        )
    })
    .await;
    let delivered = registry.snapshot(&new_agent_id).unwrap_or_default();
    assert!(
        snapshot_contains(&delivered, POINTER),
        "the clamped 30-second readiness buffer never released after its rounded deadline; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    registry.shutdown_all();
}

/// Scenario: Toggle only the delegate readiness buffer around a slow raw-mode worker that emits `SessionStart` 650 ms before accepting input. A zero buffer must lose the pointer, while 1000 ms must deliver the pointer and its submit CR after the stub becomes ready.
#[spec("orchestration/delegate/012")]
#[test]
#[cfg(unix)]
fn delegate_012_slow_agent_toggle_proves_delivery_and_submission() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build slow-readiness toggle runtime")
        .block_on(async {
            let zero = run_slow_readiness_delegate(0).await;
            assert!(
                !snapshot_contains(&zero.snapshot, POINTER),
                "the zero-buffer control unexpectedly delivered the pointer outside the stub's discard window; snapshot = {:?}",
                String::from_utf8_lossy(&zero.snapshot)
            );

            env.repoint(
                DELEGATE_READINESS_BUFFER_ENV,
                &DELEGATE_READINESS_BUFFER_MS.to_string(),
            );
            let buffered = run_slow_readiness_delegate(DELEGATE_READINESS_BUFFER_MS).await;
            eprintln!(
                "delegate slow-readiness window measured from SessionStart: zero arm {:?}, buffered arm {:?}; configured buffer: {} ms",
                zero.measured_readiness_window,
                buffered.measured_readiness_window,
                DELEGATE_READINESS_BUFFER_MS
            );
            assert!(
                buffered.measured_readiness_window >= Duration::from_millis(500)
                    && buffered.measured_readiness_window <= Duration::from_millis(900),
                "the synthetic readiness window drifted outside its intended measurement band: {:?}",
                buffered.measured_readiness_window
            );
            let mut submitted_pointer = POINTER.to_vec();
            submitted_pointer.push(b'\r');
            assert!(
                snapshot_contains(&buffered.snapshot, POINTER),
                "the 1000 ms readiness buffer did not deliver the delegate pointer after the measured {:?} input-readiness window; snapshot = {:?}",
                buffered.measured_readiness_window,
                String::from_utf8_lossy(&buffered.snapshot)
            );
            assert!(
                snapshot_contains(&buffered.snapshot, &submitted_pointer),
                "the delegate pointer was not followed by its submit CR after the readiness buffer; snapshot = {:?}",
                String::from_utf8_lossy(&buffered.snapshot)
            );
        });
}

/// Scenario: Delegate to a worker that receives the pointer and then emits no agent event before the short no-event window expires, in two arms. A ready worker's notice must quote its nonce-bearing pane text inside the untrusted frame, while a blank worker's notice must report a blank pane. Both notices must be submitted with CR and tell the orchestrator it can keep waiting, re-delegate, reassign, or notify the user, without interpolating the role name.
#[spec("orchestration/delegate/013")]
#[test]
#[cfg(unix)]
fn delegate_013_silent_worker_surfaces_notice_in_orchestrator_pane() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "600"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build delegate failure-visibility runtime")
        .block_on(async {
            delegate_013_silent_worker_surfaces_notice_in_orchestrator_pane_inner().await;
            delegate_013_blank_worker_pane_is_reported_as_blank_inner().await;
        });
}

/// Issue #686: the line the ready-prompt stand-in draws, carrying a nonce so the
/// assertion proves the notice quoted THIS pane rather than any canned string.
#[cfg(unix)]
const READY_PROMPT_LINE: &str = "Ask the agent to do anything \u{b7} ready-prompt-9f21c4";

/// Issue #686: where a silent stand-in diverts everything written into its PTY.
/// It must NOT echo to stdout — a real agent TUI puts a typed prompt into its
/// own input widget, not into the scrollback — so the delivery control reads
/// this file while the pane keeps rendering only what the agent itself drew.
#[cfg(unix)]
const SILENT_WORKER_DELIVERY_LOG: &str = "silent-worker-delivered-bytes.log";

/// Issue #686: touched by a silent stand-in once `stty raw -echo` has taken
/// effect. Waiting on a FILE rather than on pane output is what lets the blank
/// arm stay blank: without this handshake the delegate can land before the line
/// discipline is set, the PTY driver echoes the pointer, and the "blank" pane is
/// not blank at all — measured, not hypothetical.
#[cfg(unix)]
const SILENT_WORKER_RAW_MARKER: &str = "silent-worker-raw-mode.marker";

/// Issue #686: a stand-in for a worker that is booted, healthy and idle at its
/// own ready prompt while emitting no agent event whatsoever — the measured
/// shape of the report. It draws a ready prompt, swallows everything written to
/// it without echoing, and never emits a hook event of any kind.
///
/// It stands in for the agents that emit nothing until their first prompt
/// arrives (Codex and OpenCode, measured; Claude and Pi emit at boot). A
/// stand-in is used rather than a real agent because what is under test is what
/// the deck reports about a silent pane, which needs no LLM: the pane's bytes
/// and the absence of events are both established by the fixture.
#[cfg(unix)]
fn write_ready_prompt_worker(path: &std::path::Path, dir: &std::path::Path) {
    write_executable(
        path,
        &format!(
            "#!/bin/sh\n\
             stty raw -echo\n\
             printf '{banner}\\r\\n'\n\
             printf '{prompt}\\r\\n'\n\
             : > '{marker}'\n\
             exec cat -u > '{log}'\n",
            banner = "stand-in agent v0 \u{b7} type a message and press enter",
            prompt = format_args!("\u{258c} {READY_PROMPT_LINE}"),
            marker = dir.join(SILENT_WORKER_RAW_MARKER).display(),
            log = dir.join(SILENT_WORKER_DELIVERY_LOG).display(),
        ),
    );
}

/// Issue #686 control: the same silent worker with a pane that has rendered
/// NOTHING. Everything written to it is swallowed, exactly as above, so the two
/// arms differ only in whether the pane has any text to report.
#[cfg(unix)]
fn write_blank_pane_worker(path: &std::path::Path, dir: &std::path::Path) {
    write_executable(
        path,
        &format!(
            "#!/bin/sh\nstty raw -echo\n: > '{marker}'\nexec cat -u > '{log}'\n",
            marker = dir.join(SILENT_WORKER_RAW_MARKER).display(),
            log = dir.join(SILENT_WORKER_DELIVERY_LOG).display(),
        ),
    );
}

/// Poll `path` until it contains `needle` or `timeout` elapses, returning the
/// final contents either way so the caller can assert on (and print) them.
#[cfg(unix)]
async fn wait_for_file_needle(path: &std::path::Path, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let contents = std::fs::read(path).unwrap_or_default();
        if contents.windows(needle.len()).any(|w| w == needle)
            || tokio::time::Instant::now() >= deadline
        {
            return contents;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll until `path` exists or `timeout` elapses, reporting whether it appeared.
#[cfg(unix)]
async fn wait_for_file(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The two panes and the delegate wiring both `delegate/013` arms share: a raw
/// no-echo orchestrator observer whose scrollback is exactly what the daemon
/// wrote into it, plus a caller-supplied silent worker.
#[cfg(unix)]
struct SilentWorkerArm {
    _cwd: tempfile::TempDir,
    registry: Arc<AgentPtyRegistry>,
    state: AppState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    orchestrator_agent_id: String,
    worker_agent_id: String,
    delivery_log: std::path::PathBuf,
}

#[cfg(unix)]
impl SilentWorkerArm {
    async fn new(
        write_worker: impl Fn(&std::path::Path, &std::path::Path),
        worker_file_name: &str,
    ) -> Self {
        common::init_test_env();
        let cwd = common::race_safe_tempdir();
        let observer = cwd.path().join("orchestrator-observer");
        write_executable(
            &observer,
            "#!/bin/sh\nstty raw -echo\nprintf ORCHESTRATOR-NOTICE-READY\nexec cat -u\n",
        );
        let cwd_str = cwd.path().to_string_lossy().into_owned();
        let registry = Arc::new(AgentPtyRegistry::new());
        let orchestrator_agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some(&observer.to_string_lossy()),
                cwd: Some(&cwd_str),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn raw orchestrator notice observer");
        let observer_ready = wait_for_snapshot_needle(
            &registry,
            &orchestrator_agent_id,
            b"ORCHESTRATOR-NOTICE-READY",
            Duration::from_secs(2),
        )
        .await;
        assert!(
            snapshot_contains(&observer_ready, b"ORCHESTRATOR-NOTICE-READY"),
            "orchestrator notice observer never entered raw no-echo mode; snapshot = {:?}",
            String::from_utf8_lossy(&observer_ready)
        );
        let delivery_log = cwd.path().join(SILENT_WORKER_DELIVERY_LOG);
        let raw_marker = cwd.path().join(SILENT_WORKER_RAW_MARKER);
        let worker = cwd.path().join(worker_file_name);
        write_worker(&worker, cwd.path());
        let worker_agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some(&worker.to_string_lossy()),
                cwd: Some(&cwd_str),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn silent delegated worker");
        // Both arms wait for raw no-echo mode BEFORE delegating. Without it the
        // PTY line discipline echoes the pointer into the pane and the blank arm
        // silently stops testing a blank pane.
        assert!(
            wait_for_file(&raw_marker, Duration::from_secs(2)).await,
            "silent worker never reached raw no-echo mode, so the PTY driver would echo the \
             delegate pointer into a pane that is supposed to show only what the agent drew"
        );
        let (event_tx, _rx) = broadcast::channel::<BroadcastMsg>(64);
        let mut state = AppState::default();
        register_orchestration(&mut state, &cwd_str);
        state
            .pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());
        Self {
            _cwd: cwd,
            registry,
            state,
            event_tx,
            orchestrator_agent_id,
            worker_agent_id,
            delivery_log,
        }
    }

    /// Delegate, then prove the pointer physically reached the worker's PTY.
    /// The control reads the stand-in's delivery log rather than its scrollback
    /// precisely because a real agent TUI does not echo — the pane still shows
    /// only whatever the agent itself drew.
    async fn delegate_and_confirm_delivery(&mut self) {
        self.state
            .handle_delegate(
                DelegateSignal {
                    pane_id: ORCH_PANE.to_string(),
                    task: "List the files in the current directory.".to_string(),
                    to: vec![WORKER_ROLE.to_string()],
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
                &self.event_tx,
            )
            .await;
        let delivered =
            wait_for_file_needle(&self.delivery_log, POINTER, Duration::from_secs(2)).await;
        assert!(
            delivered.windows(POINTER.len()).any(|w| w == POINTER),
            "silent-worker visibility control failed: the worker never received the delegate \
             pointer; delivered = {:?}",
            String::from_utf8_lossy(&delivered)
        );
    }

    async fn wait_for_notice(&self) -> Vec<u8> {
        let notice = wait_for_silence_notice(
            &self.registry,
            &self.orchestrator_agent_id,
            Duration::from_secs(3),
        )
        .await;
        assert!(
            snapshot_has_silence_notice(&notice),
            "a worker that received its delegate pointer and emitted no agent event produced no \
             daemon notice in the orchestrator pane; snapshot = {:?}",
            String::from_utf8_lossy(&notice)
        );
        notice
    }
}

#[cfg(unix)]
async fn delegate_013_silent_worker_surfaces_notice_in_orchestrator_pane_inner() {
    let mut arm = SilentWorkerArm::new(write_ready_prompt_worker, "ready-prompt-worker").await;
    let drawn = wait_for_snapshot_needle(
        &arm.registry,
        &arm.worker_agent_id,
        READY_PROMPT_LINE.as_bytes(),
        Duration::from_secs(2),
    )
    .await;
    assert!(
        snapshot_contains(&drawn, READY_PROMPT_LINE.as_bytes()),
        "precondition failed: the worker pane never rendered its ready prompt, so there is \
         nothing for the notice to report; snapshot = {:?}",
        String::from_utf8_lossy(&drawn)
    );
    arm.delegate_and_confirm_delivery().await;
    let notice = arm.wait_for_notice().await;
    let notice_text = String::from_utf8_lossy(&notice);

    let terminator = silence_notice_terminator(&notice);
    let remediation_options = ["keep waiting", "re-delegat", "reassign", "notify the user"];
    let missing_options: Vec<&str> = remediation_options
        .iter()
        .copied()
        .filter(|option| !notice_text.to_ascii_lowercase().contains(option))
        .collect();
    assert!(
        terminator == Some(b'\r') && missing_options.is_empty(),
        "issue #702: the silence notice must be submitted with CR and name every remediation \
         option (keep waiting, re-delegate, reassign, notify the user); observed terminator = \
         {terminator:?}, missing options = {missing_options:?}, snapshot = {notice_text:?}"
    );

    assert!(
        notice_text.contains(READY_PROMPT_LINE),
        "issue #686: the notice must report what the worker's pane is ACTUALLY rendering — here a \
         booted agent sitting at its ready prompt — instead of sending the reader after a \
         delivery bug that does not exist; snapshot = {notice_text:?}"
    );
    assert!(
        notice_text.contains("[UNTRUSTED-PANE-TEXT:")
            && notice_text.contains(":END-UNTRUSTED-PANE-TEXT]"),
        "pane text is agent-authored and must reach the orchestrator inside the untrusted-data \
         frame, never as bare daemon prose; snapshot = {notice_text:?}"
    );
    assert!(
        !notice_text.contains("It may never have received the prompt"),
        "issue #686: with the pane's own contents in hand the notice must stop ASSERTING a \
         delivery failure; snapshot = {notice_text:?}"
    );
    assert!(
        !notice_text.contains(WORKER_ROLE),
        "the daemon-authored prose must still not interpolate the untrusted delegate role (the \
         framed pane text carries none in this fixture, so this pins the prose); snapshot = \
         {notice_text:?}"
    );
    arm.registry.shutdown_all();
}

/// Issue #686 control: the same silence, the same delivery, a pane with nothing
/// on it. Without this arm a notice that simply always claimed a ready prompt
/// would pass the first arm; with it, the reported text has to come from the
/// pane.
#[cfg(unix)]
async fn delegate_013_blank_worker_pane_is_reported_as_blank_inner() {
    let mut arm = SilentWorkerArm::new(write_blank_pane_worker, "blank-pane-worker").await;
    arm.delegate_and_confirm_delivery().await;
    let notice = arm.wait_for_notice().await;
    let notice_text = String::from_utf8_lossy(&notice);

    assert!(
        !notice_text.contains(READY_PROMPT_LINE),
        "the notice reported a ready prompt for a pane that never drew one, so its text is canned \
         rather than read from the pane; snapshot = {notice_text:?}"
    );
    assert!(
        notice_text.contains("rendered nothing"),
        "issue #686: a pane with no text of its own must be reported as such — that is the fact \
         that makes 'the prompt may never have arrived' a reasonable reading; snapshot = \
         {notice_text:?}"
    );
    arm.registry.shutdown_all();
}

#[cfg(unix)]
const SUPERSEDED_GENERATION_A_SENTINEL: &str = "SILENCE-WATCH-GENERATION-A-7c4e91";
#[cfg(unix)]
const LIVE_GENERATION_B_SENTINEL: &str = "SILENCE-WATCH-GENERATION-B-2a8f65";

#[cfg(unix)]
fn write_generation_sentinel_worker(path: &std::path::Path, generation_marker: &std::path::Path) {
    write_executable(
        path,
        &format!(
            "#!/bin/sh\n\
             stty raw -echo\n\
             if [ -e '{marker}' ]; then\n\
               printf '{generation_b}\\r\\n'\n\
             else\n\
               : > '{marker}'\n\
               printf '{generation_a}\\r\\n'\n\
             fi\n\
             exec cat -u\n",
            marker = generation_marker.display(),
            generation_a = SUPERSEDED_GENERATION_A_SENTINEL,
            generation_b = LIVE_GENERATION_B_SENTINEL,
        ),
    );
}

/// Scenario: Let generation A receive a delegate and arm its silence watch, then issue a `clear = true` replacement delegate so generation B owns the same pane before A's window expires and while B is still waiting to receive its payload. No notice may quote A's pane sentinel, while B must later receive its own payload and produce a notice quoting B's distinct sentinel when B's own silence window expires.
#[spec("orchestration/delegate/025")]
#[test]
#[cfg(unix)]
fn delegate_025_superseded_generation_is_silent_while_new_watch_stays_armed() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "500"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build superseded-generation silence-watch runtime")
        .block_on(async {
            common::init_test_env();
            let cwd = common::race_safe_tempdir();
            let cwd_str = cwd.path().to_string_lossy().into_owned();
            let worker = cwd.path().join("generation-sentinel-worker");
            let generation_marker = cwd.path().join("generation-a-started.marker");
            write_generation_sentinel_worker(&worker, &generation_marker);
            std::fs::write(
                cwd.path().join(".dot-agent-deck.toml"),
                clear_true_config(&worker.to_string_lossy()),
            )
            .expect("write generation-sentinel orchestration config");

            let observer = cwd.path().join("generation-silence-orchestrator");
            write_executable(
                &observer,
                "#!/bin/sh\nstty raw -echo\nprintf GENERATION-ORCHESTRATOR-READY\nexec cat -u\n",
            );
            let registry = Arc::new(AgentPtyRegistry::new());
            let orchestrator_agent_id = registry
                .spawn_agent(SpawnOptions {
                    command: Some(&observer.to_string_lossy()),
                    cwd: Some(&cwd_str),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn generation-silence orchestrator observer");
            let observer_ready = wait_for_snapshot_needle(
                &registry,
                &orchestrator_agent_id,
                b"GENERATION-ORCHESTRATOR-READY",
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_contains(&observer_ready, b"GENERATION-ORCHESTRATOR-READY"),
                "orchestrator observer never entered raw no-echo mode; snapshot = {:?}",
                String::from_utf8_lossy(&observer_ready)
            );

            let initial_agent_id = registry
                .spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    cwd: Some(&cwd_str),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn initial worker occupant");
            let (event_tx, _rx) = broadcast::channel::<BroadcastMsg>(64);
            let mut state = AppState::default();
            register_orchestration(&mut state, &cwd_str);
            state
                .pane_cwd_map
                .insert(ORCH_PANE.to_string(), cwd_str.clone());

            state
                .handle_delegate(
                    DelegateSignal {
                        pane_id: ORCH_PANE.to_string(),
                        task: "Generation A must remain silent.".to_string(),
                        to: vec![WORKER_ROLE.to_string()],
                        timestamp: chrono::Utc::now(),
                    },
                    &registry,
                    &event_tx,
                )
                .await;
            let generation_a =
                wait_for_replacement_agent(&registry, WORKER_PANE, &initial_agent_id).await;
            let generation_a_started = wait_for_snapshot_needle(
                &registry,
                &generation_a,
                SUPERSEDED_GENERATION_A_SENTINEL.as_bytes(),
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_contains(
                    &generation_a_started,
                    SUPERSEDED_GENERATION_A_SENTINEL.as_bytes(),
                ),
                "generation A did not render its sentinel; snapshot = {:?}",
                String::from_utf8_lossy(&generation_a_started)
            );
            event_tx
                .send(BroadcastMsg::Event(session_start_event(
                    AgentType::None,
                    WORKER_PANE,
                    &generation_a,
                    false,
                )))
                .expect("generation A dispatch subscribes before respawn");
            let generation_a_delivered = wait_for_snapshot_needle(
                &registry,
                &generation_a,
                POINTER,
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_contains(&generation_a_delivered, POINTER),
                "generation A did not receive its payload, so its silence watch was not \
                 observably armed; snapshot = {:?}",
                String::from_utf8_lossy(&generation_a_delivered)
            );

            env.repoint(DELEGATE_READINESS_BUFFER_ENV, "1400");
            state
                .handle_delegate(
                    DelegateSignal {
                        pane_id: ORCH_PANE.to_string(),
                        task: "Generation B must supersede A and remain silent.".to_string(),
                        to: vec![WORKER_ROLE.to_string()],
                        timestamp: chrono::Utc::now(),
                    },
                    &registry,
                    &event_tx,
                )
                .await;
            let generation_b =
                wait_for_replacement_agent(&registry, WORKER_PANE, &generation_a).await;
            event_tx
                .send(BroadcastMsg::Event(session_start_event(
                    AgentType::None,
                    WORKER_PANE,
                    &generation_b,
                    false,
                )))
                .expect("generation B dispatch subscribes before respawn");
            let generation_b_pane = wait_for_snapshot_needle(
                &registry,
                &generation_b,
                LIVE_GENERATION_B_SENTINEL.as_bytes(),
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_contains(&generation_b_pane, LIVE_GENERATION_B_SENTINEL.as_bytes()),
                "generation B never visibly took ownership of the worker pane; snapshot = {:?}",
                String::from_utf8_lossy(&generation_b_pane)
            );
            assert!(
                !snapshot_contains(&generation_b_pane, POINTER),
                "generation B's payload arrived before the test could exercise A's expiry \
                 inside B's readiness wait; snapshot = {:?}",
                String::from_utf8_lossy(&generation_b_pane)
            );

            let during_b_readiness = wait_for_silence_notice(
                &registry,
                &orchestrator_agent_id,
                Duration::from_millis(800),
            )
            .await;
            let generation_b_still_waiting = registry.snapshot(&generation_b).unwrap_or_default();
            assert!(
                !snapshot_contains(&generation_b_still_waiting, POINTER),
                "generation B's payload arrived before A's 500 ms window expired, so the \
                 supersession race was not reproduced; snapshot = {:?}",
                String::from_utf8_lossy(&generation_b_still_waiting)
            );
            assert!(
                !snapshot_has_silence_notice(&during_b_readiness)
                    && !snapshot_contains(
                        &during_b_readiness,
                        SUPERSEDED_GENERATION_A_SENTINEL.as_bytes(),
                    ),
                "issue #687: generation A's silence notice reached the orchestrator after \
                 generation B had already taken over the pane but before B received its own \
                 payload; orchestrator snapshot = {:?}",
                String::from_utf8_lossy(&during_b_readiness)
            );

            let generation_b_delivered = wait_for_snapshot_needle(
                &registry,
                &generation_b,
                POINTER,
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_contains(&generation_b_delivered, POINTER),
                "generation B never received its own payload after the readiness wait; snapshot = {:?}",
                String::from_utf8_lossy(&generation_b_delivered)
            );
            let generation_b_notice = wait_for_snapshot_needle(
                &registry,
                &orchestrator_agent_id,
                LIVE_GENERATION_B_SENTINEL.as_bytes(),
                Duration::from_secs(2),
            )
            .await;
            let notice_count = String::from_utf8_lossy(&generation_b_notice)
                .matches("delegated worker went quiet (dot-agent-deck daemon report)")
                .count();
            assert!(
                snapshot_has_silence_notice(&generation_b_notice)
                    && snapshot_contains(
                        &generation_b_notice,
                        LIVE_GENERATION_B_SENTINEL.as_bytes(),
                    )
                    && !snapshot_contains(
                        &generation_b_notice,
                        SUPERSEDED_GENERATION_A_SENTINEL.as_bytes(),
                    )
                    && notice_count == 1,
                "generation B's own silence watch did not stay armed and fire exactly once with \
                 B's pane evidence; notice_count = {notice_count}, orchestrator snapshot = {:?}",
                String::from_utf8_lossy(&generation_b_notice)
            );
            registry.shutdown_all();
        });
}

#[cfg(unix)]
struct SilenceHarness {
    _cwd: tempfile::TempDir,
    cwd_str: String,
    registry: Arc<AgentPtyRegistry>,
    state: AppState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    orchestrator_agent_id: String,
    worker_agent_id: String,
}

#[cfg(unix)]
impl SilenceHarness {
    async fn new(channel_capacity: usize) -> Self {
        common::init_test_env();
        let cwd = common::race_safe_tempdir();
        let observer = cwd.path().join("silence-test-orchestrator");
        write_executable(
            &observer,
            "#!/bin/sh\nstty raw -echo\nprintf SILENCE-ORCHESTRATOR-READY\nexec cat -u\n",
        );
        let cwd_str = cwd.path().to_string_lossy().into_owned();
        let registry = Arc::new(AgentPtyRegistry::new());
        let orchestrator_agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some(&observer.to_string_lossy()),
                cwd: Some(&cwd_str),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn silence-test orchestrator");
        let ready = wait_for_snapshot_needle(
            &registry,
            &orchestrator_agent_id,
            b"SILENCE-ORCHESTRATOR-READY",
            Duration::from_secs(2),
        )
        .await;
        assert!(
            snapshot_contains(&ready, b"SILENCE-ORCHESTRATOR-READY"),
            "silence-test orchestrator never became observable; snapshot = {:?}",
            String::from_utf8_lossy(&ready)
        );
        let worker_agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                cwd: Some(&cwd_str),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn silence-test worker");
        let mut state = AppState::default();
        register_orchestration(&mut state, &cwd_str);
        state
            .pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());
        let (event_tx, _rx) = broadcast::channel(channel_capacity);
        Self {
            _cwd: cwd,
            cwd_str,
            registry,
            state,
            event_tx,
            orchestrator_agent_id,
            worker_agent_id,
        }
    }

    async fn delegate_and_wait_for_pointer(&self) {
        self.state
            .handle_delegate(
                DelegateSignal {
                    pane_id: ORCH_PANE.to_string(),
                    task: "Perform the delegated silence-watch task.".to_string(),
                    to: vec![WORKER_ROLE.to_string()],
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
                &self.event_tx,
            )
            .await;
        let delivered = wait_for_snapshot_needle(
            &self.registry,
            &self.worker_agent_id,
            POINTER,
            Duration::from_secs(2),
        )
        .await;
        assert!(
            snapshot_contains(&delivered, POINTER),
            "silence-watch precondition failed: worker never received pointer; snapshot = {:?}",
            String::from_utf8_lossy(&delivered)
        );
    }

    async fn redelegate_and_wait_for_another_pointer(&self) {
        let before = self
            .registry
            .snapshot(&self.worker_agent_id)
            .unwrap_or_default();
        let previous_count = before
            .windows(POINTER.len())
            .filter(|w| *w == POINTER)
            .count();
        assert!(
            previous_count > 0,
            "re-delegation precondition failed: first pointer was absent; snapshot = {:?}",
            String::from_utf8_lossy(&before)
        );

        self.state
            .handle_delegate(
                DelegateSignal {
                    pane_id: ORCH_PANE.to_string(),
                    task: "Perform the newer delegated silence-watch task.".to_string(),
                    to: vec![WORKER_ROLE.to_string()],
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
                &self.event_tx,
            )
            .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = self
                .registry
                .snapshot(&self.worker_agent_id)
                .unwrap_or_default();
            let current_count = snapshot
                .windows(POINTER.len())
                .filter(|w| *w == POINTER)
                .count();
            if current_count > previous_count {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "second delegation never produced another observable task pointer; previous_count={previous_count}, current_count={current_count}, snapshot={:?}",
                String::from_utf8_lossy(&snapshot)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn type_unsent_worker_draft(&self, draft: &str) {
        use std::io::Write as _;

        let handle = self
            .registry
            .subscribe(&self.worker_agent_id)
            .expect("attach silence-test worker");
        let mut writer = handle.writer.lock().await;
        writer
            .write_all(draft.as_bytes())
            .expect("write unsent worker draft");
        writer.flush().expect("flush unsent worker draft");
        drop(writer);
        let observed = wait_for_snapshot_needle(
            &self.registry,
            &self.worker_agent_id,
            draft.as_bytes(),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            snapshot_contains(&observed, draft.as_bytes()),
            "unsent worker draft did not physically reach the PTY; snapshot={:?}",
            String::from_utf8_lossy(&observed)
        );
    }

    fn orchestrator_snapshot(&self) -> Vec<u8> {
        self.registry
            .snapshot(&self.orchestrator_agent_id)
            .unwrap_or_default()
    }
}

#[cfg(unix)]
impl Drop for SilenceHarness {
    fn drop(&mut self) {
        self.registry.shutdown_all();
    }
}

#[cfg(unix)]
fn turn_event(pane_id: &str, agent_id: &str, event_type: EventType) -> AgentEvent {
    let mut event = session_start_event(AgentType::None, pane_id, agent_id, false);
    event.event_type = event_type;
    event
}

/// Scenario: Attempt direct guarded notice writes with a wrong expected agent,
/// a pane re-homed into another orchestration, and a pane mid-close. Each attempt
/// must be refused and none of its marker bytes may enter the observable PTY.
#[test]
#[cfg(unix)]
fn delegate_notice_guard_rejects_wrong_agent_rehome_and_closing() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build guarded-notice runtime")
        .block_on(async {
            common::init_test_env();
            let cwd = common::race_safe_tempdir();
            let observer = cwd.path().join("guarded-notice-observer");
            write_executable(
                &observer,
                "#!/bin/sh\nstty raw -echo\nprintf GUARDED-NOTICE-READY\nexec cat -u\n",
            );
            let cwd_str = cwd.path().to_string_lossy().into_owned();
            let registry = Arc::new(AgentPtyRegistry::new());
            let agent_id = registry
                .spawn_agent(SpawnOptions {
                    command: Some(&observer.to_string_lossy()),
                    cwd: Some(&cwd_str),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
                    tab_membership: Some(TabMembership::Orchestration {
                        name: "successor-orchestration".to_string(),
                        role_index: 0,
                        role_name: "orchestrator".to_string(),
                        is_start_role: true,
                        orchestration_cwd: Some(cwd_str.clone()),
                        display_title: None,
                        orchestration_id: Some("successor-instance".to_string()),
                    }),
                    ..SpawnOptions::default()
                })
                .expect("spawn guarded-notice observer");
            let ready = wait_for_snapshot_needle(
                &registry,
                &agent_id,
                b"GUARDED-NOTICE-READY",
                Duration::from_secs(2),
            )
            .await;
            assert!(snapshot_contains(&ready, b"GUARDED-NOTICE-READY"));

            let wrong_agent = registry
                .write_notice_guarded(
                    ORCH_PANE,
                    "WRONG-AGENT-NOTICE",
                    Some("stale-agent-id"),
                    || async { true },
                )
                .await
                .expect("wrong-agent guarded notice result");
            assert_eq!(wrong_agent, GuardedSend::WrongSession);

            let rehome_registry = Arc::clone(&registry);
            let rehomed = registry
                .write_notice_guarded(
                    ORCH_PANE,
                    "REHOMED-NOTICE",
                    Some(&agent_id),
                    || async move {
                        rehome_registry
                            .pane_orchestration(ORCH_PANE)
                            .is_some_and(|membership| membership.name == "original-orchestration")
                    },
                )
                .await
                .expect("re-homed guarded notice result");
            assert_eq!(rehomed, GuardedSend::Stale);

            registry.begin_pane_close(ORCH_PANE);
            let closing_registry = Arc::clone(&registry);
            let closing = registry
                .write_notice_guarded(
                    ORCH_PANE,
                    "CLOSING-NOTICE",
                    Some(&agent_id),
                    || async move { !closing_registry.is_pane_closing(ORCH_PANE) },
                )
                .await
                .expect("closing guarded notice result");
            assert_eq!(closing, GuardedSend::Stale);
            registry.finish_pane_close(ORCH_PANE, false);

            std::thread::sleep(Duration::from_millis(100));
            let snapshot = registry.snapshot(&agent_id).unwrap_or_default();
            for marker in [
                b"WRONG-AGENT-NOTICE".as_slice(),
                b"REHOMED-NOTICE".as_slice(),
                b"CLOSING-NOTICE".as_slice(),
            ] {
                assert!(
                    !snapshot_contains(&snapshot, marker),
                    "refused notice bytes reached the pane: marker={:?}, snapshot={:?}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&snapshot)
                );
            }
            registry.shutdown_all();
        });
}

/// Scenario: Arm a timed silence notice, let the original orchestrator exit, and
/// place an unrelated successor on the same pane id before the deadline. The
/// dead orchestration's notice must not enter the successor's PTY.
#[test]
#[cfg(unix)]
fn delegate_silence_notice_does_not_reach_successor_orchestrator() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "700"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build successor-notice runtime")
        .block_on(async {
            let harness = SilenceHarness::new(64).await;
            harness.delegate_and_wait_for_pointer().await;
            harness
                .registry
                .close_agent(&harness.orchestrator_agent_id)
                .expect("let original orchestrator exit");

            let successor_script = harness._cwd.path().join("successor-orchestrator");
            write_executable(
                &successor_script,
                "#!/bin/sh\nstty raw -echo\nprintf SUCCESSOR-ORCHESTRATOR-READY\nexec cat -u\n",
            );
            let successor = harness
                .registry
                .spawn_agent(SpawnOptions {
                    command: Some(&successor_script.to_string_lossy()),
                    cwd: Some(&harness.cwd_str),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn successor orchestrator");
            let ready = wait_for_snapshot_needle(
                &harness.registry,
                &successor,
                b"SUCCESSOR-ORCHESTRATOR-READY",
                Duration::from_secs(2),
            )
            .await;
            assert!(snapshot_contains(&ready, b"SUCCESSOR-ORCHESTRATOR-READY"));
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let snapshot = harness.registry.snapshot(&successor).unwrap_or_default();
            assert!(
                !snapshot_has_silence_notice(&snapshot),
                "a timed notice entered a successor orchestrator pane: {:?}",
                String::from_utf8_lossy(&snapshot)
            );
        });
}

/// Scenario: Complete a hookless delegate through the real work-done handler, type an unsent draft, and delegate the same fixed pointer again; the second pointer must still reach the worker. Independently, report only an older task done after two delegates and require the newer silent task's no-event notice to remain armed.
#[spec("orchestration/delegate/021")]
#[test]
#[cfg(unix)]
fn delegate_021_work_done_releases_only_its_own_delivery_state() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "600"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build work-done cancellation runtime")
        .block_on(async {
            {
                let harness = SilenceHarness::new(64).await;
                harness.delegate_and_wait_for_pointer().await;
                harness
                    .state
                    .handle_work_done(
                        WorkDoneSignal {
                            pane_id: WORKER_PANE.to_string(),
                            task: "Completed without hook activity.".to_string(),
                            done: false,
                            timestamp: chrono::Utc::now(),
                        },
                        &harness.registry,
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(900)).await;
                let snapshot = harness.orchestrator_snapshot();
                assert!(
                    !snapshot_has_silence_notice(&snapshot),
                    "timely work-done failed to cancel its no-event watch: {:?}",
                    String::from_utf8_lossy(&snapshot)
                );
                harness
                    .type_unsent_worker_draft("worker draft deliberately left unsent")
                    .await;
                harness.redelegate_and_wait_for_another_pointer().await;
            }

            let harness = SilenceHarness::new(64).await;
            harness.delegate_and_wait_for_pointer().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            harness.redelegate_and_wait_for_another_pointer().await;
            harness
                .state
                .handle_work_done(
                    WorkDoneSignal {
                        pane_id: WORKER_PANE.to_string(),
                        task: "The superseded task completed late.".to_string(),
                        done: false,
                        timestamp: chrono::Utc::now(),
                    },
                    &harness.registry,
                )
                .await;
            let notice = wait_for_silence_notice(
                &harness.registry,
                &harness.orchestrator_agent_id,
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_has_silence_notice(&notice),
                "stale work-done cancelled the newer delegation's no-event watch: {:?}",
                String::from_utf8_lossy(&notice)
            );
        });
}

/// Scenario: Overflow the silence watch's tiny broadcast receiver after pointer
/// delivery. Because a proof event may have been dropped, the daemon must stay
/// conservative and emit no unprovable silence notice.
#[test]
#[cfg(unix)]
fn delegate_lagged_event_bus_suppresses_unprovable_silence_notice() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "400"),
    ]);
    // #346: this scenario overflows a capacity-4 broadcast channel with a tight,
    // non-yielding burst of 32 sends so the silence watch's OWN receiver lags
    // and observes `RecvError::Lagged` (proving the "unprovable, so suppress"
    // path). On a `multi_thread` runtime the watch task (spawned via
    // `tokio::spawn`) can run truly concurrently on a second OS thread and
    // drain the channel as it fills, which — depending on real host
    // scheduling under nextest's full-parallel load — can race the burst
    // enough to avoid ever lagging, making the intended overflow non-
    // deterministic. `current_thread` makes it deterministic: the burst loop
    // below has no `.await` inside it, so on a single-threaded, cooperative
    // executor it always runs to completion before the watch task gets a
    // chance to poll even once, guaranteeing the overflow regardless of
    // machine load.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build lagged-channel runtime")
        .block_on(async {
            let harness = SilenceHarness::new(4).await;
            harness.delegate_and_wait_for_pointer().await;
            for index in 0..32 {
                let event =
                    turn_event("unrelated-pane", &format!("agent-{index}"), EventType::Idle);
                harness
                    .event_tx
                    .send(BroadcastMsg::Event(event))
                    .expect("silence watch is subscribed before pointer delivery");
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
            let snapshot = harness.orchestrator_snapshot();
            assert!(
                !snapshot_has_silence_notice(&snapshot),
                "a lagged receiver was treated as proof of silence: {:?}",
                String::from_utf8_lossy(&snapshot)
            );
        });
}

/// Scenario: Send a turn-shaped event for the correct worker pane but an old
/// agent generation. The stale event must not suppress the current worker's
/// silence notice.
#[test]
#[cfg(unix)]
fn delegate_wrong_generation_event_does_not_suppress_silence_notice() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "400"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build wrong-generation runtime")
        .block_on(async {
            let harness = SilenceHarness::new(64).await;
            harness.delegate_and_wait_for_pointer().await;
            harness
                .event_tx
                .send(BroadcastMsg::Event(turn_event(
                    WORKER_PANE,
                    "stale-worker-generation",
                    EventType::Thinking,
                )))
                .expect("silence watch receiver");
            let notice = wait_for_silence_notice(
                &harness.registry,
                &harness.orchestrator_agent_id,
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_has_silence_notice(&notice),
                "a stale generation event suppressed the current worker's notice: {:?}",
                String::from_utf8_lossy(&notice)
            );
        });
}

/// Scenario: Replace a delegated worker with a successor on the same pane and
/// send a turn event from that successor. Pane-id reuse must not let the rebound
/// agent answer the original generation's silence watch.
#[test]
#[cfg(unix)]
fn delegate_rebound_worker_event_does_not_suppress_original_watch() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "1200"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build rebound-worker runtime")
        .block_on(async {
            let harness = SilenceHarness::new(64).await;
            harness.delegate_and_wait_for_pointer().await;
            harness
                .registry
                .close_agent(&harness.worker_agent_id)
                .expect("close original worker without a pane-close sweep");
            let successor = harness
                .registry
                .spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    cwd: Some(&harness.cwd_str),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn rebound worker");
            harness
                .event_tx
                .send(BroadcastMsg::Event(turn_event(
                    WORKER_PANE,
                    &successor,
                    EventType::Thinking,
                )))
                .expect("silence watch receiver");
            let notice = wait_for_silence_notice(
                &harness.registry,
                &harness.orchestrator_agent_id,
                Duration::from_secs(3),
            )
            .await;
            assert!(
                snapshot_has_silence_notice(&notice),
                "a rebound successor event suppressed the original generation's notice: {:?}",
                String::from_utf8_lossy(&notice)
            );
        });
}

/// Scenario: Emit only a matching startup `Idle` event after delegate delivery.
/// Startup status is not proof that a turn consumed the pointer, so the silence
/// notice must still appear.
#[test]
#[cfg(unix)]
fn delegate_startup_idle_does_not_suppress_silence_notice() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "400"),
    ]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build startup-idle runtime")
        .block_on(async {
            let harness = SilenceHarness::new(64).await;
            harness.delegate_and_wait_for_pointer().await;
            harness
                .event_tx
                .send(BroadcastMsg::Event(turn_event(
                    WORKER_PANE,
                    &harness.worker_agent_id,
                    EventType::Idle,
                )))
                .expect("silence watch receiver");
            let notice = wait_for_silence_notice(
                &harness.registry,
                &harness.orchestrator_agent_id,
                Duration::from_secs(2),
            )
            .await;
            assert!(
                snapshot_has_silence_notice(&notice),
                "a startup Idle event incorrectly proved task delivery: {:?}",
                String::from_utf8_lossy(&notice)
            );
        });
}

/// Scenario: Resolve the no-event knob behaviorally at `1`, whitespace-padded
/// `1`, and an integer above `u64::MAX` while the idle detector is disabled. The
/// short values must report, and the overflow value must report only at its 30 s
/// cap rather than falling through to disabled.
#[test]
#[cfg(unix)]
fn delegate_no_event_window_parses_one_whitespace_and_overflow() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let env = EnvGuard::set(&[
        (DELEGATE_READINESS_BUFFER_ENV, "0"),
        (SESSION_START_WAIT_ENV, "2000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "1"),
    ]);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build no-event parser runtime")
        .block_on(async {
            for raw in ["1", " 1 \t"] {
                env.repoint(DELEGATE_NO_EVENT_WINDOW_ENV, raw);
                let harness = SilenceHarness::new(64).await;
                harness.delegate_and_wait_for_pointer().await;
                let notice = wait_for_silence_notice(
                    &harness.registry,
                    &harness.orchestrator_agent_id,
                    Duration::from_secs(1),
                )
                .await;
                assert!(
                    snapshot_has_silence_notice(&notice),
                    "no-event override {raw:?} did not resolve to an enabled one-millisecond window: {:?}",
                    String::from_utf8_lossy(&notice)
                );
            }

            env.repoint(DELEGATE_NO_EVENT_WINDOW_ENV, "18446744073709551616");
            let harness = SilenceHarness::new(64).await;
            harness.delegate_and_wait_for_pointer().await;
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            std::thread::sleep(Duration::from_millis(50));
            let early = harness.orchestrator_snapshot();
            assert!(
                !snapshot_has_silence_notice(&early),
                "overflow no-event value reported before the 30-second cap: {:?}",
                String::from_utf8_lossy(&early)
            );

            tokio::time::advance(Duration::from_millis(30_001)).await;
            poll_until_after_time_advance(Duration::from_secs(2), || {
                snapshot_has_silence_notice(&harness.orchestrator_snapshot())
            })
            .await;
            let capped = harness.orchestrator_snapshot();
            assert!(
                snapshot_has_silence_notice(&capped),
                "above-u64 no-event value fell through to disabled instead of the 30-second cap: {:?}",
                String::from_utf8_lossy(&capped)
            );
        });
}
