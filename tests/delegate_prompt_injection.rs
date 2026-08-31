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
#[cfg(unix)]
use dot_agent_deck::event::{
    WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN, WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN,
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

/// Issue #709: how long past its configured readiness buffer the slow-readiness
/// fixture keeps looking for the delegate pointer.
///
/// Extracted from the `buffer_ms + 1200` this used to inline, because the two
/// arms of `orchestration/delegate/012` now spend it differently and the
/// difference is the point: the zero-buffer control pays it in full as a
/// negative window and must NOT have it widened, while the buffered arm scales
/// it with machine load because its wait ends the moment the pointer lands.
#[cfg(unix)]
const POINTER_DELIVERY_SLACK: Duration = Duration::from_millis(1200);

/// How long `orchestration/delegate/010` waits for the pointer AFTER the
/// replacement worker's matching `SessionStart` before calling it undelivered.
///
/// Deliberately far above the 1000 ms buffer it is measuring, because it is not
/// the assertion — the buffer's lower bound is (see the test). It only has to
/// distinguish "released by the SessionStart" from "released by nothing", and
/// the delegate path's fallback is the bare 30 s `SESSION_START_WAIT_TIMEOUT`
/// constant with no env override, so anything arriving inside this window is
/// attributable to the observed event and nothing else. Ten seconds of
/// unused ceiling costs zero on the happy path (the poll returns the instant the
/// needle appears) and buys the test immunity to a loaded runner, which is the
/// whole point of the #243 rework.
#[cfg(unix)]
const OBSERVED_READINESS_DELIVERY_CEILING: Duration = Duration::from_secs(10);

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

    /// Issue #243: guarantee a variable is UNSET for the guard's lifetime, and
    /// restore whatever it was afterwards.
    ///
    /// `set` cannot express this, and "unset" is a distinct third state here
    /// rather than a synonym for `0`: the buffer skip floors at an EXPLICIT
    /// setting, so a test that means "the operator configured nothing" has to
    /// remove the variable. Under nextest each test owns its process and it is
    /// already absent; under plain `cargo test` a sibling test in this binary
    /// has almost certainly left one behind, which would silently floor the very
    /// skip the caller is measuring.
    #[cfg(unix)]
    fn unset(keys: &[&'static str]) -> Self {
        let mut previous = Vec::with_capacity(keys.len());
        for key in keys {
            previous.push((*key, std::env::var_os(key)));
            // SAFETY: the caller holds ENV_LOCK for the guard's full lifetime.
            unsafe { std::env::remove_var(key) };
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

/// Issue #702: the byte the pane delivered immediately after `anchor`, or
/// `None` while the anchor — or the byte that follows it — has not arrived.
///
/// The whole discrimination this file performs rests on reading ONE exact byte
/// rather than searching for a class of bytes, so it is worth having in one
/// place: both the notice's submit terminator and the observer's own raw-mode
/// proof below are "the byte after a known anchor".
#[cfg(unix)]
fn byte_after(snapshot: &[u8], anchor: &[u8]) -> Option<u8> {
    let end = snapshot
        .windows(anchor.len())
        .position(|window| window == anchor)?
        + anchor.len();
    snapshot.get(end).copied()
}

/// Issue #702: the silence notice's stable FINAL clause, and therefore the last
/// bytes of the payload the daemon writes before its terminator.
///
/// Present in BOTH branches of `compose_delegate_silence_notice` (the fenced
/// pane-text one and the "rendered nothing" one), and `encode_pane_payload`'s
/// `trim_end_matches` cannot eat it because it ends in `.` — so the byte that
/// follows it in the pane is exactly the terminator the daemon chose.
#[cfg(unix)]
const SILENCE_NOTICE_TAIL: &str = "(RUST_LOG=pane_write=trace also has the delivered bytes).";

/// Issue #702: the terminator the daemon wrote after the silence notice — CR if
/// it SUBMITTED the report, LF if it left it as deferred scrollback.
///
/// Anchored to the END of the payload ([`SILENCE_NOTICE_TAIL`]) rather than to
/// the first line break at or after the notice's opening clause, which is what
/// the assertion used to search for. That search asked a weaker question than
/// the one under test: it accepted the first `\r`-or-`\n` ANYWHERE after the
/// notice began, so any unrelated line break landing in the orchestrator's pane
/// between the payload and its submit CR would have been read as "the
/// terminator" — a false LF verdict on a report that was in fact submitted.
/// Reading the single byte that follows the payload cannot be fooled in either
/// direction: it is the daemon's own choice of tail, and nothing else.
#[cfg(unix)]
fn silence_notice_terminator(snapshot: &[u8]) -> Option<u8> {
    byte_after(snapshot, SILENCE_NOTICE_TAIL.as_bytes())
}

/// Poll an agent's snapshot until `ready` holds or `timeout` elapses, returning
/// the final snapshot either way so the caller can assert on (and print) it.
///
/// Every wait in the silence-notice fixtures is on an OBSERVABLE CONDITION
/// rather than on a duration: nothing here may be tuned against `SUBMIT_DELAY`,
/// because a sleep that is long enough today is a silent pass tomorrow.
#[cfg(unix)]
async fn wait_for_snapshot_where(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    timeout: Duration,
    ready: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = registry.snapshot(agent_id).unwrap_or_default();
        if ready(&snapshot) || tokio::time::Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until the notice's payload AND the terminator byte that follows it are
/// both in the pane — the condition the `#702` assertion reads, so the wait can
/// never end one byte early and report a terminator that simply had not landed.
#[cfg(unix)]
async fn wait_for_silence_notice(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    timeout: Duration,
) -> Vec<u8> {
    wait_for_snapshot_where(registry, agent_id, timeout, |snapshot| {
        snapshot_has_silence_notice(snapshot) && silence_notice_terminator(snapshot).is_some()
    })
    .await
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
    // Issue #709: THE wait this issue measured failing — a flat 2 s for a
    // freshly spawned Python stand-in's very first byte, which on a 16-core box
    // at load average 44 expired before the interpreter had been scheduled at
    // all, so the assertion below reported `snapshot = ""` and read as a
    // delivery defect rather than as starvation. `wait_for_child_first_output`
    // keeps the condition and replaces the clock: a load-scaled ceiling, and an
    // early return if the stand-in dies rather than prints.
    let raw_ready = common::wait_for_child_first_output(
        &daemon.registry,
        &new_agent_id,
        b"DELEGATE-STUB-RAW-READY",
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
    // Issue #709: NOT a boot wait — the stub has already printed — but still a
    // wait on something that must happen, so it gets the same load scaling. The
    // band assertion on `measured_readiness_window` below is the real bound
    // here; widening this ceiling only decides WHICH assertion reports a slow
    // observation, and the band prints the measured figure while this one would
    // print a misleading "never became input-aware".
    let cat_ready = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        b"DELEGATE-STUB-CAT-READY",
        common::load_scaled(Duration::from_secs(2)),
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
    // Issue #709: the delegate's own delivery, and the one wait in this fixture
    // whose length is part of what the caller asserts — so the two arms are
    // budgeted differently ON PURPOSE.
    //
    // The zero-buffer CONTROL arm expects NOTHING to arrive, which makes this a
    // NEGATIVE window: it is always paid in full, and stretching it would buy
    // runtime rather than confidence. It keeps its fixed
    // `POINTER_DELIVERY_SLACK` exactly as before. The buffered arm expects the
    // pointer, so its wait returns the instant the needle lands and a
    // load-scaled ceiling costs an idle box nothing while giving a contended one
    // room to actually get there.
    let pointer_wait = if buffer_ms == 0 {
        POINTER_DELIVERY_SLACK
    } else {
        Duration::from_millis(buffer_ms) + common::load_scaled(POINTER_DELIVERY_SLACK)
    };
    let snapshot = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        &submitted_pointer,
        pointer_wait,
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
    // Issue #709: a boot wait — the respawn has to fork and exec before the
    // record exists — so it takes the load-scaled ceiling every boot leg in this
    // file now takes. Condition-driven either way: it returns the instant the
    // new record appears, so an idle box is not slowed by the wider ceiling.
    let deadline = tokio::time::Instant::now() + common::child_boot_budget();
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
    session_start_event_with_origin(
        agent_type,
        pane_id,
        agent_id,
        wrapper_fork.then_some(WRAPPER_FORK_SESSION_START_ORIGIN),
    )
}

/// A `SessionStart` carrying an arbitrary `session_start_origin` value, or none.
///
/// Issue #243 gave the marker three values instead of one, and the daemon prices
/// them differently, so `wrapper_fork: bool` stopped being able to say what a
/// test means. The forgery test (`orchestration/delegate/028`) in particular
/// needs to post a value NO honest producer would emit for its pane, which is a
/// thing only a free-form origin can express.
#[cfg(unix)]
fn session_start_event_with_origin(
    agent_type: AgentType,
    pane_id: &str,
    agent_id: &str,
    origin: Option<&str>,
) -> AgentEvent {
    let mut metadata = std::collections::HashMap::new();
    if let Some(origin) = origin {
        metadata.insert(
            SESSION_START_ORIGIN_METADATA_KEY.to_string(),
            origin.to_string(),
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

/// Scenario: Delegate with `clear = true`, emit the replacement worker's matching `SessionStart`, and force a 1000 ms readiness buffer. The task pointer must arrive, and the measured delay from that `SessionStart` to its arrival must be at least the whole configured buffer — a lower bound, so a loaded machine can only overshoot it, never turn it red.
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
    // MEASURE the hold rather than racing it (issue #243).
    //
    // This used to sleep 350 ms of WALL CLOCK, assert the pointer was still
    // absent, and then give delivery a 2 s ceiling — a two-sided wall-clock
    // constraint around a 1000 ms timer the daemon owns, with 650 ms of slack
    // below and ~1 s above. It failed once in three full-tier runs for #243's
    // implementer while passing every time in isolation, and #243 took the tier
    // from ~44 s to ~30 s, so more tests are concurrent at peak. Re-deriving the
    // two numbers would not fix the shape: every early-check instant is the same
    // race with a different margin, and a wall-clock race that only bites under
    // load is worse than a slow test.
    //
    // What replaces it is one-sided in the direction load actually moves.
    // `held` is measured from BEFORE the hook line is even written, so it can
    // only exceed the buffer the daemon applied — hook-socket latency, runtime
    // jitter and the 20 ms snapshot poll all push it up and none of them push it
    // down. The one thing that pushes it below the buffer is the buffer being
    // bypassed, which is exactly the regression this test exists to catch:
    // re-running it with the buffer env set to `0` delivers in 24.7 ms against
    // this 1000 ms floor, a 40x margin. So the property is now pinned across the
    // WHOLE buffer rather than its first 350 ms, and no amount of load can turn
    // it red.
    let delivered = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        POINTER,
        OBSERVED_READINESS_DELIVERY_CEILING,
    )
    .await;
    let held = session_start_at.elapsed();
    assert!(
        snapshot_contains(&delivered, POINTER),
        "delegate pointer was not delivered within {OBSERVED_READINESS_DELIVERY_CEILING:?} of the \
         replacement worker's matching SessionStart, so the observed-readiness branch released \
         nothing at all; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    assert!(
        held >= Duration::from_millis(DELEGATE_READINESS_BUFFER_MS),
        "the matching SessionStart released delegate delivery after only {held:?}, which is less \
         than the configured {DELEGATE_READINESS_BUFFER_MS} ms readiness buffer could possibly \
         have taken — the observed branch bypassed the buffer (PRD #249 M1)"
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

/// Issue #243: how long a delegated worker that is DEMONSTRABLY up may take to
/// see its task pointer before the delay is a defect rather than a boot cost.
///
/// **Ten seconds, and the derivation is the old six with one term moved.** This
/// was 6 s against a 1000 ms buffer — 5 s of slack over everything the deck does
/// on this leg, sized so the slowest healthy delegate in the issue's own daemon
/// log (a ClaudeCode worker at 3.80 / 3.85 / 3.96 / 4.39 s end to end) cleared it
/// with headroom. Round 3 replaced the buffer this fixture pays: the stand-in now
/// reaches its interface the way a real Codex does (it clears `ICANON`/`ECHO`,
/// see [`write_wrapped_ready_agent`]), so the gate releases on the strong fact and
/// pays the 5000 ms `WRAPPER_INTERFACE_READINESS_BUFFER` rather than the ordinary
/// 1000 ms. The SLACK is unchanged at 5 s; only the constant inside it moved,
/// which is why this is 10 and not a fresh guess.
///
/// *Below:* three times under `SESSION_START_WAIT_TIMEOUT`, and three times under
/// the ~31 s that every unreleased path now costs — the Codex workers in the same
/// log sit at 31.2 / 31.2 / 31.7 / 31.7 / 32.3 s, constant to the tenth of a
/// second, which is the tell that it is the constant and not load. Nothing that
/// waits the timeout out can slip past this as a false green, which is the entire
/// reason the number is bounded above at all.
///
/// **Raising it to ~34 s to accommodate the OLD fixture was the wrong repair, and
/// is why the fixture changed instead.** A `printf …; exec cat` stand-in never
/// leaves cooked mode, so since `46ccca1` it releases on the window-expiry
/// fallback BY DESIGN and lands at a measured 30.98 s. A budget wide enough for
/// that would make this test pass on precisely the behaviour it was written to
/// catch. Measured on this branch with the raw-input fixture: 5.044 s.
#[cfg(unix)]
const READY_TO_POINTER_BUDGET: Duration = Duration::from_secs(10);

/// Issue #243: how far past [`READY_TO_POINTER_BUDGET`] the two latency tests
/// keep looking, once they already know they have failed, purely to MEASURE what
/// the delay actually is.
///
/// A test that reports "not delivered within 6 s" pins the defect but hands the
/// fix no before-number to beat; one that reports "delivered after 31.4 s" is the
/// issue's evidence. Set above the 30 s fallback plus the 1000 ms buffer so the
/// real figure lands inside it rather than at the ceiling. It is paid ONLY on the
/// failing path — a delivery inside budget returns immediately — so it costs the
/// fast tier nothing once the readiness signal exists.
#[cfg(unix)]
const MEASURED_LATENCY_CEILING: Duration = Duration::from_secs(34);

/// Issue #243: the ready prompt a wrapped Codex stand-in paints once its
/// interface exists, carrying a nonce so a snapshot match cannot be anything but
/// this fixture's own banner.
#[cfg(unix)]
const WRAPPED_READY_BANNER: &str = "Ask Codex to do anything (ready-7c1e)";

/// Issue #243: a stand-in for codex-cli's measured behaviour — it takes the
/// terminal out of cooked mode, paints its ready interface and then accepts
/// input, and it emits NO native `SessionStart` until a prompt actually arrives.
///
/// That last part is the defect, and it is why this is a `cat` stand-in rather
/// than a hook-emitting one: codex-cli posts its native `SessionStart` when the
/// first TURN starts, so the signal the readiness gate waits for is caused by the
/// very prompt the gate is withholding. Wrapping is not stubbed — the deck's
/// common spawn boundary rewrites a `codex` command into a real
/// `dot-agent-deck wrap --agent codex -- codex`, so the fork-time
/// card-surfacing `SessionStart` under test here is emitted by the real wrapper.
///
/// **The `stty raw -echo` is round 3's repair and it is load-bearing.** This was
/// a bare `printf …; exec cat` for two rounds, which never leaves cooked mode —
/// so once `46ccca1` made the wrapper's WEAK fact provisional for the whole
/// `SESSION_START_WAIT_TIMEOUT`, this fixture stopped modelling "a worker sitting
/// at its ready prompt" and started modelling the one shape that waits the
/// timeout out on purpose: a line-oriented REPL. The test went red at 30.98 s
/// against its 6 s budget, and the tempting repair — widen the budget past 31 s —
/// would have made it green on exactly the regression it exists to catch. A real
/// Codex clears `ICANON`/`ECHO`, so the fixture does too, and the sentence the
/// test asserts ("a worker visibly at its prompt gets its task promptly") is
/// falsifiable again.
///
/// `stty` BEFORE `printf`, for [`raw_input_agent_script`]'s reason: with no
/// output written yet the settle branch returns early, so fact 1 is the only fact
/// that can fire and which one wins is not left to a race between the wrapper's
/// 50 ms supervisory poll and its 750 ms settle window.
#[cfg(unix)]
fn write_wrapped_ready_agent(path: &std::path::Path) {
    write_executable(
        path,
        &format!("#!/bin/sh\nstty raw -echo\nprintf '{WRAPPED_READY_BANNER}\\r\\n'\nexec cat\n"),
    );
}

/// Scenario: Delegate with `clear = true` to a wrapped Codex stand-in that takes its terminal out of cooked mode the way a real Codex TUI does, paints its ready prompt, and then — like real codex-cli — emits no native `SessionStart` until a prompt arrives. Once that ready prompt is visibly on the replacement pane, the task pointer must reach it within ten seconds (the wrapper's strong interface fact plus the 5000 ms interface buffer it is priced at) instead of waiting out the 30 s `SessionStart` fallback (issue #243).
#[spec("orchestration/delegate/029")]
#[test]
#[cfg(unix)]
fn delegate_029_wrapped_worker_without_native_session_start_is_delivered_promptly() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        // The delegate path reads `SESSION_START_WAIT_TIMEOUT` as a bare
        // constant with no override (`src/state.rs`), so this pins nothing here
        // — it is set to the production value so the scheduler mirror of the
        // same wait cannot quietly shorten what this test is measuring.
        (SESSION_START_WAIT_ENV, "30000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
    // The buffer env is REMOVED, and round 3 is when that started to matter.
    // This test used to pin it to 1000 ms for determinism, which was harmless
    // while the strong fact SKIPPED the buffer. It is not harmless now: guard 3
    // makes an explicitly-set value win over BOTH defaults, so a pin here would
    // quietly buy this fixture the ordinary 1000 ms and the test would report
    // "delivered promptly" about a configuration the deck does not ship. Left
    // unset, the run pays the real 5000 ms `WRAPPER_INTERFACE_READINESS_BUFFER`
    // a wrapped Codex delegate pays in production, which is what
    // `READY_TO_POINTER_BUDGET` is now derived against.
    let _unset = EnvGuard::unset(&[DELEGATE_READINESS_BUFFER_ENV]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build wrapped prompt-latency runtime")
        .block_on(
            delegate_029_wrapped_worker_without_native_session_start_is_delivered_promptly_inner(),
        );
}

#[cfg(unix)]
async fn delegate_029_wrapped_worker_without_native_session_start_is_delivered_promptly_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let bin_dir = cwd.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create wrapped-agent bin dir");
    write_wrapped_ready_agent(&bin_dir.join("codex"));
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config("codex"),
    )
    .expect("write wrapped prompt-latency orchestration config");
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
        .expect("spawn initial wrapped ready stand-in");
    {
        let mut state = daemon.state.write().await;
        register_orchestration(&mut state, &cwd_str);
    }

    daemon
        .state
        .read()
        .await
        .handle_delegate(
            DelegateSignal {
                pane_id: ORCH_PANE.to_string(),
                task: "List the files in the current directory.".to_string(),
                to: vec![WORKER_ROLE.to_string()],
                timestamp: chrono::Utc::now(),
            },
            &daemon.registry,
            &daemon.event_tx,
        )
        .await;
    let new_agent_id =
        wait_for_replacement_agent(&daemon.registry, WORKER_PANE, &old_agent_id).await;

    // CONTROL. Everything below is about a worker that is up and waiting, so
    // this run means nothing unless the replacement genuinely reached its ready
    // interface. Its banner on the pane is the user-visible form of "the agent
    // is booted and healthy at its prompt" that this issue reports the deck
    // ignoring.
    let banner = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        WRAPPED_READY_BANNER.as_bytes(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        snapshot_contains(&banner, WRAPPED_READY_BANNER.as_bytes()),
        "control: the wrapped replacement never painted its ready interface, so this run proves \
         nothing about the readiness gate; snapshot = {:?}",
        String::from_utf8_lossy(&banner)
    );
    let ready_at = Instant::now();

    let delivered = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        POINTER,
        READY_TO_POINTER_BUDGET,
    )
    .await;
    if !snapshot_contains(&delivered, POINTER) {
        // Failed already; keep looking only to turn "missed the budget" into a
        // number the fix can be measured against. See `MEASURED_LATENCY_CEILING`.
        let eventual = wait_for_snapshot_needle(
            &daemon.registry,
            &new_agent_id,
            POINTER,
            MEASURED_LATENCY_CEILING,
        )
        .await;
        let measured = ready_at.elapsed();
        let arrived = snapshot_contains(&eventual, POINTER);
        panic!(
            "a wrapped worker sitting VISIBLY at its ready prompt did not receive its delegated \
             task pointer within {READY_TO_POINTER_BUDGET:?}: it {} after {measured:?} measured \
             from the instant that prompt appeared. This fixture clears ICANON/ECHO before it \
             paints, so the wrapper's STRONG interface fact is on the wire and the gate has \
             something to release on; a figure near 31 s means it released on nothing instead and \
             paid the full SESSION_START_WAIT_TIMEOUT, which is issue #243's regression. The \
             wrapper's fork-time SessionStart is skipped as boot provenance and codex-cli emits \
             no native one until a prompt starts a turn, so that fallback is all there would be \
             left. snapshot = {:?}",
            if arrived {
                "eventually arrived"
            } else {
                "still had not arrived"
            },
            String::from_utf8_lossy(&eventual)
        );
    }
}

// ---------------------------------------------------------------------------
// Issue #243: the THREE guards on the wrapper's interface buffer.
//
// **Round 3 changed what they guard.** For two rounds `src/state.rs`'s delegate
// seam dropped the post-readiness buffer to ZERO when all three held, and these
// tests pinned the skip. Measurement retracted the premise: a full-screen TUI
// takes raw mode at INIT, before it will accept a submit, so writing on that
// instant is the worst moment available rather than the safest. There is no skip
// any more. What the three guards now decide is WHICH buffer is owed —
// `WRAPPER_INTERFACE_READINESS_BUFFER` (5000 ms, measured against codex-cli's
// initialisation) or the ordinary `DELEGATE_READINESS_BUFFER` (1000 ms):
//
//   1. the event carried the STRONG interface fact (`wrapper_interface_ready`,
//      the child cleared `ICANON`/`ECHO`) — and, a level up in the gate, that
//      fact is also the only one a Wrapper-strategy agent may be RELEASED on
//      before the upgrade window expires;
//   2. the daemon's own frozen launch record says it spawned that agent as a
//      wrapper host, so a producer-written marker cannot select the pricing;
//   3. the operator pinned no `…_DELEGATE_READINESS_BUFFER_MS` of their own,
//      which wins over both defaults when they did.
//
// `/026`, `/027` and `/028` below pin one guard each. **Every one of them needed
// a two-sided bound to survive the change**, because a dropped guard no longer
// produces a near-zero delivery — it produces the OTHER buffer, which clears any
// floor. `/026` and `/028` were both measured green with their guard deleted
// before this round re-founded them.
//
// Between them they also pin that the interface path still WORKS, which is the
// failure `/029` cannot see: guard 2 is fail-closed, so a refactor that made it
// refuse every honest agent would drop every wrapped agent to the ordinary
// buffer — 3601 ms short of what codex-cli needed under measured load — with
// every other test still green.
// ---------------------------------------------------------------------------

/// Issue #243: every `AgentEvent` the daemon broadcast since the collector
/// started, so a test can ask WHICH readiness fact released its gate.
///
/// A collector rather than an inline `subscribe()` + `recv()`: the interface
/// event can arrive before the test's next await point (the wrapper emits it
/// from its own 50 ms supervisory poll, off any clock the test controls), and a
/// broadcast receiver that is not being drained lags. Started before the
/// delegate is issued, drained continuously, and queried afterwards.
#[cfg(unix)]
struct EventCollector {
    events: Arc<Mutex<Vec<AgentEvent>>>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl EventCollector {
    fn start(event_tx: &broadcast::Sender<BroadcastMsg>) -> Self {
        let mut rx = event_tx.subscribe();
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(BroadcastMsg::Event(event)) => {
                        sink.lock().unwrap_or_else(|e| e.into_inner()).push(event);
                    }
                    Ok(BroadcastMsg::OrchestrationSurface(_) | BroadcastMsg::WorktreeKept(_)) => {}
                    // A lagged receiver has lost events it will never see again;
                    // the queries below report an empty result rather than
                    // asserting, so the caller fails with its own message.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self { events, task }
    }

    /// Every wrapper INTERFACE `SessionStart` seen for `agent_id`, in arrival
    /// order. Fork-time provenance events are excluded: they say a child was
    /// forked, not that its interface exists.
    fn interface_session_starts(&self, agent_id: &str) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|event| {
                event.agent_id.as_deref() == Some(agent_id)
                    && event.is_wrapper_interface_session_start()
            })
            .cloned()
            .collect()
    }

    /// Block until the wrapper announces `agent_id`'s interface, and return that
    /// event. Panics with the whole captured stream on timeout, because every
    /// caller's run means nothing without it.
    async fn wait_for_interface_session_start(
        &self,
        agent_id: &str,
        timeout: Duration,
    ) -> AgentEvent {
        self.wait_for_interface_fact(agent_id, None, timeout).await
    }

    /// Block until the wrapper announces a SPECIFIC interface fact for
    /// `agent_id` — `None` for "whichever fact comes first".
    ///
    /// Issue #243 round 3: the by-origin form is what lets a test measure the
    /// UPGRADE. The wrapper's two facts can both fire on one session, in the
    /// order fact 2 then fact 1 and never the reverse
    /// (`InterfaceWatch::claim` latches per fact), so a caller that only ever
    /// sees the first one cannot tell "the gate waited for the strong fact"
    /// from "the gate released on the weak one". `orchestration/delegate/026`
    /// needs both events to anchor its bound against.
    async fn wait_for_interface_fact(
        &self,
        agent_id: &str,
        origin: Option<&str>,
        timeout: Duration,
    ) -> AgentEvent {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(event) = self
                .interface_session_starts(agent_id)
                .into_iter()
                .find(|event| {
                    origin.is_none_or(|want| {
                        event
                            .metadata
                            .get(SESSION_START_ORIGIN_METADATA_KEY)
                            .map(String::as_str)
                            == Some(want)
                    })
                })
            {
                return event;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the wrapper never announced agent {agent_id:?}'s {} interface fact within \
                 {timeout:?}, so this run establishes nothing about how the readiness gate \
                 prices the two of them; captured events = {:?}",
                origin.unwrap_or("(either)"),
                self.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .map(|event| (
                        event.agent_id.clone(),
                        format!("{:?}", event.event_type),
                        event.metadata.clone()
                    ))
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(unix)]
impl Drop for EventCollector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Issue #243: a wrapped-agent delegate run, held open so the caller can measure
/// what happens AFTER the wrapper announces the replacement's interface.
///
/// Keeps the daemon and the working directory alive — dropping either kills the
/// pane the caller is still polling.
#[cfg(unix)]
struct WrappedInterfaceRun {
    daemon: common::InProcDaemon,
    _cwd: tempfile::TempDir,
    /// Kept alive so its draining task outlives the run, and QUERIED after it:
    /// `orchestration/delegate/026`'s fixture produces a second interface fact
    /// some seconds after the first, and the upgrade between them is what that
    /// test measures. See [`WrappedInterfaceRun::wait_for_interface_fact`].
    collector: EventCollector,
    new_agent_id: String,
    /// The interface `SessionStart` the wrapper emitted for the REPLACEMENT
    /// worker — the event the buffer seam prices.
    interface_event: AgentEvent,
    /// When the replacement's banner became visible on its pane. Not the timing
    /// anchor (that is `interface_event.timestamp`); the user-visible control
    /// that a worker really was sitting at its interface.
    banner_at: Instant,
}

/// Drive one `clear = true` delegate to a `codex`-named stand-in that the deck
/// really wraps, and return once the wrapper has announced the replacement's
/// interface.
///
/// `script` is the stand-in's whole body and is what selects WHICH interface
/// fact fires — a child that never leaves cooked mode can only ever produce the
/// settled guess, and one that clears `ICANON`/`ECHO` produces the raw-input
/// observation. Everything else is `orchestration/delegate/029`'s setup: the
/// command is the bare name `codex` on a `$PATH` carrying both the fixture and
/// the built deck binary, so `AgentType::from_command` resolves it to a
/// Wrapper-strategy agent and the common spawn boundary rewrites it into a REAL
/// `dot-agent-deck wrap --agent codex -- codex`.
#[cfg(unix)]
async fn run_wrapped_interface_delegate(script: &str, banner: &str) -> WrappedInterfaceRun {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let bin_dir = cwd.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create wrapped-agent bin dir");
    write_executable(&bin_dir.join("codex"), script);
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config("codex"),
    )
    .expect("write wrapped interface-fact orchestration config");
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
        .expect("spawn initial wrapped interface-fact stand-in");
    {
        let mut state = daemon.state.write().await;
        register_orchestration(&mut state, &cwd_str);
    }

    // Started BEFORE the delegate: the replacement's interface event can land
    // before the first poll below returns.
    let collector = EventCollector::start(&daemon.event_tx);

    daemon
        .state
        .read()
        .await
        .handle_delegate(
            DelegateSignal {
                pane_id: ORCH_PANE.to_string(),
                task: "List the files in the current directory.".to_string(),
                to: vec![WORKER_ROLE.to_string()],
                timestamp: chrono::Utc::now(),
            },
            &daemon.registry,
            &daemon.event_tx,
        )
        .await;
    let new_agent_id =
        wait_for_replacement_agent(&daemon.registry, WORKER_PANE, &old_agent_id).await;

    // CONTROL, the same one `/029` opens with: nothing below means anything
    // unless the replacement genuinely reached its interface and painted it.
    let painted = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        banner.as_bytes(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        snapshot_contains(&painted, banner.as_bytes()),
        "control: the wrapped replacement never painted its ready interface, so this run proves \
         nothing about how the readiness buffer prices the wrapper's two facts; snapshot = {:?}",
        String::from_utf8_lossy(&painted)
    );
    let banner_at = Instant::now();

    let interface_event = collector
        .wait_for_interface_session_start(&new_agent_id, WRAPPER_INTERFACE_ANNOUNCE_CEILING)
        .await;
    WrappedInterfaceRun {
        daemon,
        _cwd: cwd,
        collector,
        new_agent_id,
        interface_event,
        banner_at,
    }
}

#[cfg(unix)]
impl WrappedInterfaceRun {
    /// Block until the wrapper announces the interface fact carrying `origin`
    /// for THIS run's replacement, and return that event.
    ///
    /// `interface_event` is whichever fact arrived FIRST; this is how a caller
    /// reaches a later one. Panics with the captured stream on timeout.
    async fn wait_for_interface_fact(&self, origin: &str, timeout: Duration) -> AgentEvent {
        self.collector
            .wait_for_interface_fact(&self.new_agent_id, Some(origin), timeout)
            .await
    }
}

/// Issue #243: how long the wrapper's interface announcement is waited for
/// before a run is called inconclusive.
///
/// Not an assertion about latency — the wrapper's supervisory poll is 50 ms and
/// its settle window 750 ms, so both facts announce inside a second on any
/// machine. It is a ceiling on a precondition: if no interface event arrives at
/// all, every timing measurement below is measuring the 30 s fallback instead
/// and the failure must say so rather than reporting a mysterious late pointer.
#[cfg(unix)]
const WRAPPER_INTERFACE_ANNOUNCE_CEILING: Duration = Duration::from_secs(15);

/// Issue #243: how long the pointer is waited for after the interface event a
/// test anchors on, before the run is called "released by nothing at all".
///
/// Same role as `orchestration/delegate/010`'s ceiling: the load-bearing
/// assertion is always a separate bound, and this only separates "released by
/// the fact we anchored on" from "released by the timeout". Comfortably above
/// the longest buffer the deck can owe (5000 ms) plus any plausible scheduling,
/// and comfortably below the 30 s `SESSION_START_WAIT_TIMEOUT` the fallback
/// would otherwise supply.
///
/// **Round 3 shrank what this can prove, and the shrinkage is why `/026` and
/// `/028` no longer rest on it.** `46ccca1` made the upgrade window equal to
/// `SESSION_START_WAIT_TIMEOUT`, so "released on the weak fact when the window
/// expired" and "released by nothing at all" now land in the SAME instant —
/// both ~31 s. No ceiling can tell those two apart any more. It still works
/// where the anchor is a fact that releases the gate IMMEDIATELY (the strong
/// fact in `/027`, the forged marker in `/028`), which is every remaining use.
#[cfg(unix)]
const HELD_POINTER_DELIVERY_CEILING: Duration = Duration::from_secs(10);

/// The deck's OWN `WRAPPER_INTERFACE_READINESS_BUFFER`, mirrored here for the
/// same reason [`PRODUCTION_READINESS_BUFFER_MS`] is: it is `pub(crate)` and an
/// integration test cannot name it.
///
/// Issue #243 round 3 replaced a SKIP with this second buffer, and the two
/// defaults being DIFFERENT is what every guard test below now measures against.
/// While the strong fact suppressed the buffer, "did guard N hold?" was
/// answerable by a single bound near zero; now every path pays something, and the
/// only question left is WHICH something — 5000 ms for a fact-1 release on a pane
/// the deck itself spawned as a wrapper host, 1000 ms for everything else. If
/// either production default moves, this and `PRODUCTION_READINESS_BUFFER_MS` are
/// what to re-derive.
#[cfg(unix)]
const PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS: u64 = 5000;

/// Issue #243 round 3: an upper bound that says "whatever this run paid, it was
/// NOT the 5000 ms interface buffer".
///
/// The guard tests that must observe the ORDINARY buffer
/// (`orchestration/delegate/028`) or the OPERATOR's
/// (`orchestration/delegate/027` arm 2) can no longer make their point with a
/// lower bound alone, because dropping the guard they exist to pin now yields
/// the LONGER buffer, which clears any floor they could set. Measured: `/028`
/// stayed green with guard 2 deleted, and `/027` arm 2 stayed green with guard 3
/// deleted. The bound had to become two-sided.
///
/// **3000 ms, from both ends.** Above the values it must accommodate: 1000 ms
/// (`/028`) and 1500 ms (`/027` arm 2), leaving 2.0 s and 1.5 s of slack for a
/// socket hop, a broadcast, a PTY write and a 20 ms snapshot poll on a loaded
/// runner. Below the value it must exclude: a full 2.0 s under the 5000 ms
/// interface buffer, so a dropped guard cannot pass itself off as scheduling
/// noise. The gap is symmetric on purpose — there is no reason to favour a false
/// red over a false green here, since both hide the same defect.
#[cfg(unix)]
const SHORT_BUFFER_ATTRIBUTION_CEILING: Duration = Duration::from_millis(3000);

/// The deck's OWN `DELEGATE_READINESS_BUFFER` default, mirrored here because it
/// is `pub(crate)` and an integration test cannot name it.
///
/// The two guard tests that must observe a buffer NOT being skipped
/// (`orchestration/delegate/026`, `/028`) deliberately leave
/// `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` unset and assert against this
/// instead — and that is load-bearing rather than tidiness. Guard 3 floors the
/// skip at an explicitly-set value, so with the variable pinned to anything at
/// all the buffer survives whether or not guards 1 and 2 exist: both tests would
/// stay green with the guard they exist to pin deleted. Measured, not reasoned —
/// `/026` passed with guard 1 reverted until this changed. If the production
/// default ever moves, this constant is the one thing to re-derive.
#[cfg(unix)]
const PRODUCTION_READINESS_BUFFER_MS: u64 = 1000;

/// Issue #243: an operator-pinned buffer for `orchestration/delegate/027`'s
/// second arm, deliberately NOT the 1000 ms default.
///
/// A value equal to the default would leave the arm unable to tell "the skip
/// floored at the operator's setting" from "the skip never happened", since both
/// produce the same hold. 1500 ms is far enough above 1000 ms to be attributable
/// and far enough below the 30 s clamp to keep the test fast.
#[cfg(unix)]
const OPERATOR_PINNED_BUFFER_MS: u64 = 1500;

/// Issue #243 round 3: how long `orchestration/delegate/026`'s stand-in stays in
/// COOKED mode after painting, before it clears `ICANON`/`ECHO`.
///
/// This models the production launch shape in miniature. `devbox run codex-big`
/// prints one banner at ~0.1 s and then computes its shellenv in silence for a
/// measured 2750–4132 ms before `codex` is exec'd at all, so the wrapper's weak
/// fact fires while a LAUNCHER still owns the line discipline and the strong one
/// follows 2005–3370 ms later — 21 times out of 21.
///
/// **Two seconds, and both ends are tight for a reason.** It must exceed the
/// wrapper's 750 ms settle window by enough that fact 2 has certainly fired and
/// been reported before fact 1 becomes observable — otherwise the run silently
/// degrades into `/027`'s single-fact case and stops testing the upgrade at all,
/// which the ordering control below is there to catch. And it is kept small
/// because it is dead time in the fast tier: the whole test is this dwell plus
/// the 5000 ms buffer.
///
/// The separation it buys is what makes the assertion falsifiable. Fact 2 lands
/// at ~0.85 s and fact 1 at ~2.05 s, so a gate that wrongly released on the weak
/// fact would deliver at ~1.85 s — BEFORE the strong fact exists — and the bound
/// below, which is measured from the strong fact, reads that as a negative
/// interval clamped to zero.
#[cfg(unix)]
const UPGRADE_FIXTURE_COOKED_DWELL_SECS: u64 = 2;

/// The nonce-carrying banner the settle-then-raw stand-in paints while it is
/// still in cooked mode.
#[cfg(unix)]
const UPGRADED_READY_BANNER: &str = "Ask Codex to do anything (upgrade-2e77)";

/// The nonce-carrying banner the raw-input stand-in paints after taking the
/// terminal out of cooked mode.
#[cfg(unix)]
const RAW_INPUT_READY_BANNER: &str = "Ask Codex to do anything (raw-9f52)";

/// A wrapped stand-in that paints its prompt, stays in COOKED mode long enough
/// for its output to settle, and only THEN clears `ICANON`/`ECHO` — so its
/// wrapper reports fact 2 first and fact 1 some seconds behind it.
///
/// This is the production launch shape in miniature and the only fixture in the
/// suite that exercises the UPGRADE. See [`UPGRADE_FIXTURE_COOKED_DWELL_SECS`]
/// for why the dwell is what it is, and `InterfaceWatch::claim` for why the
/// wrapper can report a second fact at all (it latches per fact, not per
/// session, which is exactly what round 3's regression fix restored).
#[cfg(unix)]
fn cooked_then_raw_agent_script() -> String {
    format!(
        "#!/bin/sh\nprintf '{UPGRADED_READY_BANNER}\\r\\n'\nsleep          {UPGRADE_FIXTURE_COOKED_DWELL_SECS}\nstty raw -echo\nexec cat\n"
    )
}

/// A wrapped stand-in that takes its terminal OUT of cooked mode before painting
/// anything — the observable signature of a TUI that is reading keystrokes, and
/// the only thing that produces the strong `wrapper_interface_ready` fact.
///
/// `stty` before `printf` on purpose. The watch checks the line discipline
/// first and the settle window only as a fallback, so clearing `ICANON`/`ECHO`
/// while the child has written nothing at all makes fact 1 the only fact that
/// CAN fire: with no output yet, the settle branch returns early. Painting first
/// would leave which fact wins to a race between the 50 ms supervisory poll and
/// the 750 ms settle window.
#[cfg(unix)]
fn raw_input_agent_script() -> String {
    format!("#!/bin/sh\nstty raw -echo\nprintf '{RAW_INPUT_READY_BANNER}\\r\\n'\nexec cat\n")
}

/// Scenario: Delegate with `clear = true` to a wrapped stand-in that paints its prompt, goes quiet in COOKED mode long enough for its wrapper to report the weak output-settled guess, and only two seconds later clears `ICANON`/`ECHO` so the same wrapper reports the strong raw-input observation. Assert the two facts arrived in that order, and that the pointer was held until the STRONG one — landing a full 5000 ms interface buffer after it, rather than 1000 ms after the guess that beat it to the daemon.
#[spec("orchestration/delegate/026")]
#[test]
#[cfg(unix)]
fn delegate_026_settled_interface_fact_is_upgraded_before_the_pointer_is_released() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (SESSION_START_WAIT_ENV, "30000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
    // The buffer env is REMOVED, and that is the assertion's whole load-bearing
    // half — see `PRODUCTION_READINESS_BUFFER_MS`. Pinning it would let guard 3
    // hold the buffer up on its own, and this test would keep passing with the
    // guard it exists to pin deleted.
    let _unset = EnvGuard::unset(&[DELEGATE_READINESS_BUFFER_ENV]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build settled-fact readiness runtime")
        .block_on(
            delegate_026_settled_interface_fact_is_upgraded_before_the_pointer_is_released_inner(),
        );
}

#[cfg(unix)]
async fn delegate_026_settled_interface_fact_is_upgraded_before_the_pointer_is_released_inner() {
    let run =
        run_wrapped_interface_delegate(&cooked_then_raw_agent_script(), UPGRADED_READY_BANNER)
            .await;

    // CONTROL 1: the fact that arrived FIRST must be the weak one, or there is
    // no upgrade here to measure. A fixture that cleared `ICANON`/`ECHO` sooner
    // than intended — or a wrapper whose settle window moved — would silently
    // degrade this run into `orchestration/delegate/027`'s single-fact case,
    // where the bound below holds for a completely different reason.
    assert_eq!(
        run.interface_event
            .metadata
            .get(SESSION_START_ORIGIN_METADATA_KEY)
            .map(String::as_str),
        Some(WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN),
        "control: this fixture spends its first {UPGRADE_FIXTURE_COOKED_DWELL_SECS}s in cooked \
         mode, so the FIRST interface fact must be the settled guess; metadata = {:?}",
        run.interface_event.metadata
    );
    assert!(
        !run.interface_event
            .is_wrapper_interface_ready_session_start(),
        "control: the settled marker must not satisfy the strong predicate the gate releases on"
    );

    // The upgrade itself: the same wrapper session reports the STRONG fact once
    // the child finally takes the terminal. `InterfaceWatch::claim` latches per
    // FACT rather than per session precisely so this second event can exist;
    // before that fix it was computed on the next 50 ms tick and thrown away.
    let ready_event = run
        .wait_for_interface_fact(
            WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN,
            WRAPPER_INTERFACE_ANNOUNCE_CEILING,
        )
        .await;

    // CONTROL 2: and it must genuinely have arrived AFTER the guess. Equal or
    // reversed timestamps mean the two facts raced rather than being ordered by
    // the fixture, and the whole point of the dwell is that they do not.
    assert!(
        ready_event.timestamp > run.interface_event.timestamp,
        "control: the strong fact was stamped {:?}, at or before the weak one at {:?} — the \
         fixture's cooked dwell did not separate them, so this run is not measuring an upgrade",
        ready_event.timestamp,
        run.interface_event.timestamp
    );

    let delivered = wait_for_snapshot_needle(
        &run.daemon.registry,
        &run.new_agent_id,
        POINTER,
        HELD_POINTER_DELIVERY_CEILING,
    )
    .await;
    // Measured against each event's OWN timestamp, stamped by the wrapper before
    // the line ever reached the daemon (`Emitter::build_event`). Both instants
    // are necessarily at or before the moment the daemon acted on them, so
    // socket latency, scheduling and this test's 20 ms poll can only push these
    // figures UP — the same one-sided shape `orchestration/delegate/010` uses,
    // and the reason load cannot turn this red.
    let held_from_strong = (chrono::Utc::now() - ready_event.timestamp)
        .to_std()
        .unwrap_or(Duration::ZERO);
    let held_from_weak = (chrono::Utc::now() - run.interface_event.timestamp)
        .to_std()
        .unwrap_or(Duration::ZERO);
    assert!(
        snapshot_contains(&delivered, POINTER),
        "the delegate pointer never arrived within {HELD_POINTER_DELIVERY_CEILING:?} of the \
         wrapper's STRONG interface event, so nothing released it at all; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    // THE ASSERTION, and it is one bound doing two jobs. Reaching 5000 ms past
    // the strong fact means (a) the gate did not release on the weak fact that
    // beat it here by {UPGRADE_FIXTURE_COOKED_DWELL_SECS}s, and (b) what it did
    // release on was priced as an interface observation rather than as an
    // ordinary readiness fact.
    //
    // **A ceiling cannot do this job any more, which is why this test was
    // re-founded rather than re-tuned** (issue #243 round 3). It used to assert
    // a 1000 ms LOWER bound under a 10 s ceiling, on the theory that the weak
    // fact releases the gate and pays the ordinary buffer. Since `46ccca1` the
    // upgrade window IS `SESSION_START_WAIT_TIMEOUT`, so a weak fact that never
    // upgrades is released by window-expiry at ~30 s and then pays 1000 ms —
    // landing in the same instant as "released by nothing at all", which no
    // ceiling can separate from it. Worse, the old lower bound of 1000 ms was
    // then satisfied by a ~30.2 s value whether or not any guard existed: the
    // test passed with the entire buffer deleted. Anchoring on WHICH FACT
    // released the gate, rather than on how long the release took, is what
    // makes it falsifiable again.
    assert!(
        held_from_strong >= Duration::from_millis(PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS),
        "the delegate pointer landed {held_from_strong:?} after the wrapper's STRONG interface \
         fact ({held_from_weak:?} after its weak one), short of the \
         {PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS} ms interface buffer that fact is priced at. \
         The gate released on the output-settled GUESS instead of holding for the observation \
         behind it. Settling is not evidence of an interface — a launcher stalled mid-boot \
         settles exactly like a REPL at its prompt, and `devbox run codex-big` does it for a \
         measured 2750–4132 ms while the pty is still canonical — so a pointer written then goes \
         into the LAUNCHER's line discipline, is drained fused when the agent finally takes raw \
         mode, and parks unsubmitted in the composer (issue #243, `INTERFACE_UPGRADE_WINDOW`)"
    );
    // The user-visible half of the same statement, and the one a reader can
    // check by eye: a worker whose banner has been on the pane this whole time
    // still waited for the fact that says the AGENT owns the terminal.
    assert!(
        run.banner_at.elapsed() >= Duration::from_millis(PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS),
        "the pointer arrived {:?} after the replacement's prompt appeared, which is inside the \
         interface buffer alone — never mind the cooked dwell before it",
        run.banner_at.elapsed()
    );
    run.daemon.registry.shutdown_all();
}

/// Scenario: Delegate with `clear = true` to a wrapped stand-in that clears `ICANON`/`ECHO` on its terminal, so its wrapper reports the strong raw-input-mode observation and the gate releases on it. With no operator buffer configured the pointer must then be held for the full 5000 ms interface buffer that fact is priced at — not the ordinary 1000 ms every other readiness fact gets; with `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS=1500` set, the very same release must instead hold it for that 1500 ms and no longer.
#[spec("orchestration/delegate/027")]
#[test]
#[cfg(unix)]
fn delegate_027_raw_input_fact_pays_the_interface_buffer_never_the_operators() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build raw-input readiness runtime");
    let script = raw_input_agent_script();

    {
        // ARM 1 — the FEATURE. The variable is REMOVED, not set to `0`: guard 3
        // makes an explicitly-set value win over BOTH defaults, so leaving a
        // sibling test's setting in place would decide the very number this arm
        // measures. Only an unset variable reaches
        // `WRAPPER_INTERFACE_READINESS_BUFFER`.
        let _base = EnvGuard::set(&[
            (SESSION_START_WAIT_ENV, "30000"),
            (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
            (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
        ]);
        let _unset = EnvGuard::unset(&[DELEGATE_READINESS_BUFFER_ENV]);
        runtime.block_on(delegate_027_raw_input_fact_pays_the_interface_buffer_inner(
            &script,
        ));
    }

    {
        // ARM 2 — guard 3 in isolation, on the SAME fixture and the same fact.
        let _env = EnvGuard::set(&[
            (
                DELEGATE_READINESS_BUFFER_ENV,
                &OPERATOR_PINNED_BUFFER_MS.to_string(),
            ),
            (SESSION_START_WAIT_ENV, "30000"),
            (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
            (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
        ]);
        runtime.block_on(
            delegate_027_operator_pinned_buffer_replaces_the_interface_buffer_inner(&script),
        );
    }
}

/// Assert the run really did exercise the STRONG fact, and hand back the run.
///
/// Both arms depend on it identically: an assertion about what the skip does is
/// vacuous if the fixture quietly stopped producing the only fact that skips.
#[cfg(unix)]
fn assert_raw_input_fact(run: &WrappedInterfaceRun) {
    assert_eq!(
        run.interface_event
            .metadata
            .get(SESSION_START_ORIGIN_METADATA_KEY)
            .map(String::as_str),
        Some(WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN),
        "control: a stand-in that cleared ICANON/ECHO before writing a byte must announce the \
         RAW-INPUT observation — with no output yet, the settle branch cannot fire at all. \
         Getting the settled value here means the wrapper stopped observing the child's line \
         discipline, and every wrapped agent in production silently dropped to the ordinary \
         buffer — or, since `46ccca1`, to waiting the readiness timeout out first; \
         metadata = {:?}",
        run.interface_event.metadata
    );
    // THE FAIL-CLOSED ALARM, and round 3 did not weaken it. Guard 2 refuses
    // toward the SHORTER buffer, so a version of it that turned down every
    // honest agent would leave the deck writing into a codex-cli that is still
    // initialising — silently, with the payload parked in the composer — and
    // every other assertion in this suite would stay green. This is still the
    // only thing in the suite that would notice.
    assert!(
        run.daemon
            .registry
            .agent_spawned_as_wrapper_host(&run.new_agent_id),
        "control: guard 2 must admit this replacement — the deck resolved the bare command \
         `codex` to a Wrapper-strategy agent and exec'd it under `dot-agent-deck wrap` itself. If \
         this is false the interface buffer is unreachable for every honest agent, which is the \
         one regression no other test in this suite can see"
    );
}

#[cfg(unix)]
async fn delegate_027_raw_input_fact_pays_the_interface_buffer_inner(script: &str) {
    let run = run_wrapped_interface_delegate(script, RAW_INPUT_READY_BANNER).await;
    assert_raw_input_fact(&run);

    let delivered = wait_for_snapshot_needle(
        &run.daemon.registry,
        &run.new_agent_id,
        POINTER,
        HELD_POINTER_DELIVERY_CEILING,
    )
    .await;
    let held = (chrono::Utc::now() - run.interface_event.timestamp)
        .to_std()
        .unwrap_or(Duration::ZERO);
    // The UPPER bound, and it is this `wait_for_snapshot_needle` that carries
    // it: 10 s is a third of `SESSION_START_WAIT_TIMEOUT`, so a pointer that
    // shows up here at all was released by the fact this run anchored on and not
    // by the gate giving up. That separation still works for this test — unlike
    // `orchestration/delegate/026`'s — because the strong fact releases the gate
    // in the instant it arrives rather than at window expiry.
    assert!(
        snapshot_contains(&delivered, POINTER),
        "the delegate pointer never arrived at all within {HELD_POINTER_DELIVERY_CEILING:?} of \
         the wrapper's raw-input interface event; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    // THE LOWER BOUND, and round 3 inverted it. This asserted `held <= 700 ms`
    // for two rounds — that the strong fact SKIPPED the buffer outright — and
    // the premise was measured false: a full-screen TUI enables raw mode at
    // INIT, real codex-cli 85 ms after exec and `orchestration/delegate/009` at
    // fork + 100 ms, so writing on that instant is the earliest and worst moment
    // available and `/009` lost the pointer into an unsubmitted composer exactly
    // as production did. There is no skip to pin. What fact 1 buys now is a
    // DIFFERENT buffer, sized against how long that initialisation goes on
    // eating input, and this bound is what tells the two defaults apart.
    //
    // It is one bound over both surviving guards, which is why the message names
    // both. Guard 1 mis-priced (the strong fact treated as an ordinary readiness
    // fact) and guard 2 fail-closed (`agent_spawned_as_wrapper_host` refusing an
    // honest agent) both land on `delegate_readiness_buffer()` and both show up
    // here as ~1 s instead of ~5 s.
    assert!(
        held >= Duration::from_millis(PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS),
        "a wrapper-hosted agent whose interface the wrapper OBSERVED (raw input mode) received \
         its pointer {held:?} after that observation, short of the \
         {PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS} ms \
         `WRAPPER_INTERFACE_READINESS_BUFFER` the strong fact is priced at. A figure near \
         {PRODUCTION_READINESS_BUFFER_MS} ms is the ORDINARY buffer being paid instead, and there \
         are exactly two ways to get there: guard 1 stopped telling the wrapper's two facts \
         apart, or guard 2 is refusing an honest agent. The second is fail-closed and silent — \
         every other test in this suite stays green through it — and what it costs is the \
         measured 3601 ms worst case codex-cli's TUI initialisation needs under load, i.e. the \
         prompt parks unsubmitted in the composer and no turn ever starts"
    );
}

#[cfg(unix)]
async fn delegate_027_operator_pinned_buffer_replaces_the_interface_buffer_inner(script: &str) {
    let run = run_wrapped_interface_delegate(script, RAW_INPUT_READY_BANNER).await;
    assert_raw_input_fact(&run);

    let delivered = wait_for_snapshot_needle(
        &run.daemon.registry,
        &run.new_agent_id,
        POINTER,
        HELD_POINTER_DELIVERY_CEILING,
    )
    .await;
    let held = (chrono::Utc::now() - run.interface_event.timestamp)
        .to_std()
        .unwrap_or(Duration::ZERO);
    assert!(
        snapshot_contains(&delivered, POINTER),
        "the delegate pointer never arrived within {HELD_POINTER_DELIVERY_CEILING:?} of the \
         wrapper's raw-input interface event while an operator buffer was pinned; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    assert!(
        held >= Duration::from_millis(OPERATOR_PINNED_BUFFER_MS),
        "the interface observation released the pointer after {held:?}, short of the \
         {OPERATOR_PINNED_BUFFER_MS} ms the operator pinned in \
         {DELEGATE_READINESS_BUFFER_ENV}. The deck may choose its OWN default off this fact; it \
         must never choose the operator's setting off, which is #199's escape hatch and the one \
         knob someone whose prompts go missing on a slow machine can reach for — a marker on an \
         unauthenticated socket must not be able to shorten it (issue #243 review, guard 3)"
    );
    // THE OTHER HALF, added in round 3, and without it this arm pins nothing.
    // Guard 3 used to be the only thing standing between the operator's interval
    // and a SKIP, so a lower bound alone said everything: no guard meant ~0 ms.
    // Now no guard means `WRAPPER_INTERFACE_READINESS_BUFFER`, and 5000 ms
    // clears a 1500 ms floor comfortably — measured, by deleting the operator
    // branch and watching this arm stay green. What guard 3 actually promises is
    // that an explicitly-set value OVERRIDES both defaults rather than being
    // max()-ed against them, so that "what the operator set" and "what the
    // operator gets" stay the same sentence; a value ABOVE the pin is as much a
    // violation of that as one below it, and only a two-sided bound says so.
    assert!(
        held <= SHORT_BUFFER_ATTRIBUTION_CEILING,
        "the operator pinned {OPERATOR_PINNED_BUFFER_MS} ms in \
         {DELEGATE_READINESS_BUFFER_ENV} and the pointer was held {held:?} instead, past \
         {SHORT_BUFFER_ATTRIBUTION_CEILING:?}. That is the \
         {PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS} ms interface default being applied over the \
         operator's own interval — a max() rather than an override. The escape hatch has to be \
         able to shorten this buffer as well as lengthen it: it is how the e2e harness pins 0, \
         and an operator who measured their own machine is not overruled by a default measured \
         on someone else's"
    );
}

/// Scenario: Delegate with `clear = true` to a plain `cat` worker the daemon never spawned as a wrapper host, then post a `SessionStart` for it carrying the wrapper's strong `wrapper_interface_ready` marker — the forgery #243's audit reproduced from a bare `python3`. The marker must release the gate and be priced as an ORDINARY readiness fact: the pointer is held for the deck's 1000 ms default, and specifically not for the 5000 ms interface buffer a genuine wrapper host's observation would have bought.
#[spec("orchestration/delegate/028")]
#[test]
#[cfg(unix)]
fn delegate_028_forged_interface_marker_is_priced_as_an_ordinary_fact() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (SESSION_START_WAIT_ENV, "30000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
    // Unset because guard 3 makes an explicitly-set value win over BOTH
    // defaults: with one pinned, the forged and the honest paths resolve to the
    // same number and this test could not tell them apart at all.
    let _unset = EnvGuard::unset(&[DELEGATE_READINESS_BUFFER_ENV]);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build forged-marker readiness runtime")
        .block_on(delegate_028_forged_interface_marker_is_priced_as_an_ordinary_fact_inner());
}

#[cfg(unix)]
async fn delegate_028_forged_interface_marker_is_priced_as_an_ordinary_fact_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config("cat"),
    )
    .expect("write forged-marker orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let old_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn initial forged-marker worker");
    {
        let mut state = daemon.state.write().await;
        register_orchestration(&mut state, &cwd_str);
    }
    daemon
        .state
        .read()
        .await
        .handle_delegate(
            DelegateSignal {
                pane_id: ORCH_PANE.to_string(),
                task: "List the files in the current directory.".to_string(),
                to: vec![WORKER_ROLE.to_string()],
                timestamp: chrono::Utc::now(),
            },
            &daemon.registry,
            &daemon.event_tx,
        )
        .await;
    let new_agent_id =
        wait_for_replacement_agent(&daemon.registry, WORKER_PANE, &old_agent_id).await;

    // CONTROL: the daemon's own frozen launch record must NOT call this pane a
    // wrapper host, or the forgery is not a forgery. `cat` is a command
    // `AgentType::from_command` cannot resolve, so `spawn_agent_type` is `None`
    // — the ordinary shape of every pane running something the deck did not
    // choose an agent for.
    assert!(
        !daemon.registry.agent_spawned_as_wrapper_host(&new_agent_id),
        "control: this pane must not be a wrapper host, or the marker below is honest rather \
         than forged; spawn_agent_type = {:?}",
        daemon.registry.spawn_agent_type(&new_agent_id)
    );

    let posted_at = Instant::now();
    // THE FORGERY. One JSON line on the daemon's hook socket, carrying the
    // wrapper's strong interface marker for a pane no wrapper is running on.
    // #243's audit reproduced exactly this from a bare `python3` with no deck
    // environment at all: `metadata` is free-form by contract and the socket
    // authenticates nobody, so the marker is producer-writable and the daemon
    // must not grant a privilege on it alone.
    let forged = session_start_event_with_origin(
        AgentType::None,
        WORKER_PANE,
        &new_agent_id,
        Some(WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN),
    );
    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&forged).expect("serialize forged interface SessionStart"),
    )
    .expect("write forged interface SessionStart");

    let delivered = wait_for_snapshot_needle(
        &daemon.registry,
        &new_agent_id,
        POINTER,
        HELD_POINTER_DELIVERY_CEILING,
    )
    .await;
    let held = posted_at.elapsed();
    // Being precise about the delta this pins, because round 3 moved it.
    // Releasing the GATE was already forgeable before #243 — a bare unmarked
    // `SessionStart` does it — and still is, which is why the pointer is
    // expected to ARRIVE rather than be withheld.
    assert!(
        snapshot_contains(&delivered, POINTER),
        "the forged marker should still RELEASE the gate — that was always forgeable and is not \
         what guard 2 defends — but no pointer arrived within \
         {HELD_POINTER_DELIVERY_CEILING:?}; snapshot = {:?}",
        String::from_utf8_lossy(&delivered)
    );
    assert!(
        held >= Duration::from_millis(PRODUCTION_READINESS_BUFFER_MS),
        "the forged marker released the pointer {held:?} after the line hit the socket, inside \
         the deck's own {PRODUCTION_READINESS_BUFFER_MS} ms readiness buffer. Every readiness \
         fact the deck has owes a buffer — a session existing is not the TUI interpreting `\r` \
         as submit (#199/#249/#663) — so no marker of any kind may deliver faster than this"
    );
    // THE GUARD-2 ASSERTION, and it is this one, not the floor above. The
    // catalog entry for this test claimed "dropping guard 2 delivers in 21.1 ms"
    // and that proof expired with `56c10dd`: with the SKIP replaced by a second
    // buffer, dropping guard 2 no longer delivers instantly — it delivers after
    // `WRAPPER_INTERFACE_READINESS_BUFFER`, which sails over the 1000 ms floor.
    // Measured: this test stayed green with `agent_spawned_as_wrapper_host`
    // deleted from the seam. What guard 2 is worth is now ATTRIBUTION rather
    // than privilege — whether a claimed interface fact is priced as a real
    // TUI's initialisation or as an ordinary readiness fact — and telling
    // 1000 ms from 5000 ms is the only way to observe it.
    assert!(
        held <= SHORT_BUFFER_ATTRIBUTION_CEILING,
        "a forged `wrapper_interface_ready` marker was priced as a real wrapper's OBSERVATION: \
         the pointer was held {held:?}, past {SHORT_BUFFER_ATTRIBUTION_CEILING:?} and toward the \
         {PRODUCTION_WRAPPER_INTERFACE_BUFFER_MS} ms interface buffer. The daemon never spawned \
         this pane as a wrapper host, so nothing is observing that child's interface and the \
         claim is unbacked — the deck's own frozen launch record, which no hook path can write, \
         is the only thing that may select that buffer (issue #243 audit F1, guard 2)"
    );
    daemon.registry.shutdown_all();
}

/// Issue #243 round 4: the floor `orchestration/delegate/030` holds the
/// declared-no-signal path to, and the reason the buffer variable is UNSET there.
///
/// **It pins the CALL SITE, which nothing else in the suite does.** `/030`'s
/// upper bound proves the 30 s dead wait is gone; it says nothing about which of
/// the deck's three buffer defaults the skip then reaches for. With the variable
/// pinned — as this test pinned it for three rounds — every default collapses to
/// the pinned number and the test stays green even if the seam silently reverts
/// to `state::delegate_readiness_buffer()`. Its real-agent sibling
/// `orchestration/delegate/015` pins the variable too, for its own reasons, so it
/// cannot catch that either. Unset, the run resolves
/// `state::no_signal_readiness_buffer()` for real and this floor is what reads it
/// back.
///
/// **Seven seconds, chosen to reject the two wrong answers by a whole step.** The
/// loop below walks virtual time in 1 s steps, so a delivery attributable to the
/// ordinary 1000 ms `DELEGATE_READINESS_BUFFER` lands at 1 s and one attributable
/// to the 5000 ms `WRAPPER_INTERFACE_READINESS_BUFFER` lands at 5 s; the shipped
/// 8000 ms `NO_SIGNAL_READINESS_BUFFER` lands at 8 s. A floor at 7 s separates
/// the right answer from both wrong ones with a full step of margin either side,
/// rather than pinning 8 s exactly and going red on a re-derivation that moves
/// the constant by a second. **Verified load-bearing:** reverting the call site
/// in `src/state.rs` to `delegate_readiness_buffer()` delivers at 1 s of virtual
/// time and turns this red.
///
/// Deliberately a floor and not an equality: what is under test is that the
/// no-signal path is priced as a whole cold agent start rather than as the gap
/// after an announcement, and [`NO_SIGNAL_POINTER_CEILING`] bounds the other end.
#[cfg(unix)]
const NO_SIGNAL_BUFFER_FLOOR: Duration = Duration::from_secs(7);

/// Issue #243 round 4: `orchestration/delegate/030`'s upper bound, split out from
/// [`READY_TO_POINTER_BUDGET`] because the two tests no longer pay the same
/// buffer.
///
/// `/029`'s 10 s is derived as "the 5000 ms interface buffer plus 5 s of slack".
/// This path pays the 8000 ms no-signal buffer instead, so borrowing that number
/// would leave 2 s of headroom under a derivation that never mentioned this test
/// — and would couple `/030` to a constant that moves whenever the WRAPPER's
/// buffer is re-derived. Twelve is the same shape recomputed: 8 s of buffer plus
/// 4 s, on a paused clock where the only thing between the release and the write
/// is that one timer.
///
/// *Below:* still 2.6x under the ~31 s (`SESSION_START_WAIT_TIMEOUT` + a buffer)
/// that a run which regressed to the dead wait costs, which is the one thing this
/// bound must not be able to accommodate. Do not raise it toward that figure.
#[cfg(unix)]
const NO_SIGNAL_POINTER_CEILING: Duration = Duration::from_secs(12);

/// Scenario: Delegate with `clear = true` to a worker whose agent emits no readiness event of any kind before its first prompt (OpenCode's measured behaviour), with no operator buffer configured, then walk a paused Tokio clock forward a second at a time. The task pointer must reach the pane inside twelve virtual seconds rather than after the 30 s dead wait, and no sooner than seven — the shipped 8000 ms no-signal buffer, which is how the run proves the skip resolved THAT buffer rather than the ordinary 1000 ms one (issue #243).
#[spec("orchestration/delegate/030")]
#[test]
#[cfg(unix)]
fn delegate_030_agent_with_no_pre_prompt_signal_skips_the_dead_wait() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _env = EnvGuard::set(&[
        (SESSION_START_WAIT_ENV, "30000"),
        (WORKER_RESPONSE_TIMEOUT_ENV, "0"),
        (DELEGATE_NO_EVENT_WINDOW_ENV, "0"),
    ]);
    // The buffer env is REMOVED rather than pinned, for `orchestration/delegate/029`'s
    // reason applied to the other path: guard 3 makes an explicitly-set value
    // win over ALL THREE defaults, so a pin here buys this fixture whatever
    // number the pin names and the bounds below stop being able to tell the
    // no-signal buffer from the ordinary one. Left unset, the run pays the real
    // `NO_SIGNAL_READINESS_BUFFER` a declared-`NoSignal` delegate pays in
    // production — see `NO_SIGNAL_BUFFER_FLOOR`.
    let _unset = EnvGuard::unset(&[DELEGATE_READINESS_BUFFER_ENV]);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build no-signal readiness runtime")
        .block_on(delegate_030_agent_with_no_pre_prompt_signal_skips_the_dead_wait_inner());
}

#[cfg(unix)]
async fn delegate_030_agent_with_no_pre_prompt_signal_skips_the_dead_wait_inner() {
    common::init_test_env();
    let cwd = common::race_safe_tempdir();
    let bin_dir = cwd.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create no-signal agent bin dir");
    // Named `opencode` on purpose: `AgentType::from_command` keys on the
    // basename, so this is the OpenCode CONFIGURATION rather than an anonymous
    // stand-in — a Plugin-strategy agent the deck does not wrap, whose plugin
    // bus carries no pre-prompt event at all (`session.created` was measured
    // arriving 16 ms AFTER the prompt was accepted, #146). Nothing in this test
    // ever emits an event, which is precisely that agent's cold-boot stream.
    let agent = bin_dir.join("opencode");
    write_executable(&agent, "#!/bin/sh\nexec cat\n");
    let command = agent.to_string_lossy().into_owned();
    assert_eq!(
        AgentType::from_command(Some(&command)),
        Some(AgentType::OpenCode),
        "control: the fixture must resolve to the OpenCode agent, or this measures nothing about \
         an agent with no pre-prompt readiness signal"
    );
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        clear_true_config(&command),
    )
    .expect("write no-signal orchestration config");
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    let old_agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn initial no-signal worker");
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

    tokio::time::pause();
    // Walk virtual time forward a second at a time rather than jumping to the
    // budget, for two reasons. It MEASURES: the failure can report how long the
    // pointer really took instead of only that it missed. And it is robust to a
    // fix that arms a bounded buffer only after a shortened wait resolves — a
    // single `advance` cannot cross a timer armed part-way through itself, so
    // one jump would report a false red for a correct two-stage fix. Each step
    // overshoots by `TIMER_TICK_SLACK` (#402) so crossing a deadline never
    // depends on where its arming instant fell inside a millisecond.
    let step = Duration::from_secs(1);
    let mut virtual_elapsed = Duration::ZERO;
    let delivered = loop {
        if poll_until_after_time_advance(Duration::from_millis(60), || {
            snapshot_contains(
                &registry.snapshot(&new_agent_id).unwrap_or_default(),
                POINTER,
            )
        })
        .await
        {
            break true;
        }
        if virtual_elapsed >= MEASURED_LATENCY_CEILING {
            break false;
        }
        advance_and_run(step + TIMER_TICK_SLACK).await;
        virtual_elapsed += step;
    };
    let snapshot = registry.snapshot(&new_agent_id).unwrap_or_default();
    registry.shutdown_all();

    assert!(
        delivered,
        "a worker whose agent emits no readiness event ever received its delegated task pointer \
         at all, within {MEASURED_LATENCY_CEILING:?} of virtual time; snapshot = {:?}",
        String::from_utf8_lossy(&snapshot)
    );
    assert!(
        virtual_elapsed <= NO_SIGNAL_POINTER_CEILING,
        "a worker whose agent has NO pre-prompt readiness signal still sat through the dead wait: \
         the task pointer arrived after {virtual_elapsed:?} of virtual time, against a budget of \
         {NO_SIGNAL_POINTER_CEILING:?}. There is no signal for the gate to fast-path on, so \
         `hook_install.is_some()` sends it into the full SESSION_START_WAIT_TIMEOUT and only the \
         fallback delivers (issue #243). snapshot = {:?}",
        String::from_utf8_lossy(&snapshot)
    );
    // Round 4's addition, and the only assertion anywhere that reads WHICH
    // buffer the declared-no-signal skip resolves. Everything above is satisfied
    // by a seam that skipped the wait and then paid the ordinary 1000 ms — which
    // is the shape this issue shipped and `orchestration/delegate/015` found red
    // against a real OpenCode. See `NO_SIGNAL_BUFFER_FLOOR`.
    assert!(
        virtual_elapsed >= NO_SIGNAL_BUFFER_FLOOR,
        "the declared-no-signal skip delivered the task pointer after only {virtual_elapsed:?} of \
         virtual time, under the {NO_SIGNAL_BUFFER_FLOOR:?} floor. With no operator buffer \
         configured this path must resolve `no_signal_readiness_buffer()` \
         (`NO_SIGNAL_READINESS_BUFFER`, 8000 ms, sized in issue #243 round 4 against a real \
         OpenCode's composer paint across 176 runs); a figure at or near 1 s means the call site \
         reverted to the ordinary `delegate_readiness_buffer()`, and one near 5 s means it \
         reached for the wrapper's interface buffer. Neither covers a whole cold agent start, \
         and the cost of getting it wrong is a SILENTLY swallowed prompt. snapshot = {:?}",
        String::from_utf8_lossy(&snapshot)
    );
}

/// Scenario: Delegate to a worker that receives the pointer and then emits no agent event before the short no-event window expires, in two arms. When its pane is sitting at a booted agent's ready prompt, the orchestrator's notice must QUOTE the lines that pane is actually rendering, framed as untrusted pane text. When its pane has rendered nothing at all, the notice must say so instead — proving the text is read from that pane rather than canned. Both arms stay LF-terminated with no role-name interpolation in the daemon prose.
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

/// Issue #702: what the orchestrator observer prints once `stty raw -echo` has
/// returned. It is followed by a bare LF, and the byte the pane delivers after
/// it is read as PROOF that the line discipline really is raw — see
/// `SilentWorkerArm::new`.
#[cfg(unix)]
const ORCHESTRATOR_READY_MARKER: &str = "ORCHESTRATOR-NOTICE-READY";

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
        // Issue #702: the readiness marker is terminated with a bare LF ON
        // PURPOSE — see the raw-mode proof below, which reads the byte that
        // follows it. Under `-opost` that LF reaches the master unchanged;
        // under a cooked line discipline ONLCR rewrites it to CRLF, so the
        // marker's own tail measures the very translation that would otherwise
        // make this whole fixture unable to tell a submit from a deferral.
        write_executable(
            &observer,
            "#!/bin/sh\nstty raw -echo\nprintf 'ORCHESTRATOR-NOTICE-READY\\n'\nexec cat -u\n",
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
        let observer_ready = wait_for_snapshot_where(
            &registry,
            &orchestrator_agent_id,
            Duration::from_secs(2),
            |snapshot| byte_after(snapshot, ORCHESTRATOR_READY_MARKER.as_bytes()).is_some(),
        )
        .await;
        assert!(
            snapshot_contains(&observer_ready, ORCHESTRATOR_READY_MARKER.as_bytes()),
            "orchestrator notice observer never printed its readiness marker; snapshot = {:?}",
            String::from_utf8_lossy(&observer_ready)
        );
        // Issue #702, and the reason `/013` can claim "submitted" at all: PROVE
        // the observer's line discipline instead of assuming it.
        //
        // The marker alone only proves `stty` RAN. If it ran and failed — or has
        // not been applied yet — the pane is cooked, and then OPOST/ONLCR
        // rewrites every LF the daemon writes into CRLF. A DEFERRED,
        // LF-terminated notice would land in this scrollback as `...\r\n` and be
        // read as a CR, so the assertion downstream would go GREEN on exactly
        // the evidence it exists to reject. Reading the byte after the marker
        // measures that translation directly: LF means OPOST is off, and since
        // `stty raw` applies its whole flag set in one `tcsetattr`, `-opost`
        // being in effect is also proof that `-icrnl` is — which closes the
        // input-side CR->LF translation in the same stroke.
        let observed_tail = byte_after(&observer_ready, ORCHESTRATOR_READY_MARKER.as_bytes());
        assert_eq!(
            observed_tail,
            Some(b'\n'),
            "the orchestrator observer's PTY is NOT in raw mode: its readiness marker was printed \
             with a bare LF but the pane delivered {observed_tail:?} after it. With OPOST/ONLCR \
             still on, a deferred LF-terminated notice is rewritten to CRLF and would be observed \
             as a submit CR, so this fixture could not tell #702's submitted report from the \
             deferred one it replaced; snapshot = {:?}",
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
         option (keep waiting, re-delegate, reassign, notify the user). The terminator is the \
         single byte the pane delivered after the payload's final clause, so Some(10) means the \
         report was DEFERRED as LF scrollback rather than submitted as a turn; observed \
         terminator = {terminator:?}, missing options = {missing_options:?}, snapshot = \
         {notice_text:?}"
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
