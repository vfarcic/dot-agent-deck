//! PRD #128 Direction B-1 regression: the orchestrator spawn-time role
//! prompt must wait `SPAWN_TIME_READINESS_BUFFER` after SessionStart is
//! observed before the daemon writes the prompt + CR. Claude Code's
//! `SessionStart` hook fires very early in its boot, before its TUI
//! input enters submit-CR-aware mode; on slower environments (remote
//! daemon, weak VM, scheduler jitter) the gap between SessionStart and
//! input-readiness is wide enough that a CR firing immediately on
//! SessionStart is treated as a newline in the input buffer instead of
//! a submit. The user sees the role prompt land in the orchestrator's
//! input box, followed by a blank line — the prompt was never
//! dispatched.
//!
//! This test pins the gate. An agent stub simulates the
//! "input-not-ready-yet" window by silently consuming any stdin (CR
//! included) for the first `STUB_NOT_READY_MS` ms after spawn, then
//! switching to `cat -u` so any subsequent bytes are echoed back to
//! the PTY master (and therefore visible in the scrollback). With the
//! fix in place, the test drives the same gating helper the TUI loop
//! uses (`dot_agent_deck::ui::should_inject_spawn_time_prompt`) — it
//! waits past `SPAWN_TIME_READINESS_BUFFER` (500 ms) before calling
//! `write_and_submit_to_pane`, so the role prompt arrives during the
//! stub's `cat -u` phase and surfaces in scrollback. Without the fix
//! (toggle-verify), the write fires while the stub is still in its
//! discard phase and the role prompt is invisible in scrollback —
//! exactly the production failure mode.
//!
//! `STUB_NOT_READY_MS` (300 ms) is deliberately less than the fix's
//! `SPAWN_TIME_READINESS_BUFFER` (500 ms) AND greater than the legacy
//! zero-buffer behavior. That gives a clean toggle: with the fix the
//! write lands after the stub becomes ready; without it the write
//! lands inside the discard window.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::{AgentSpawnOptions, PaneController};
use dot_agent_deck::state::{AppState, SharedState};
use dot_agent_deck::ui::{SPAWN_TIME_READINESS_BUFFER, should_inject_spawn_time_prompt};

mod common;

/// Stub's not-ready window. Chosen smaller than
/// `SPAWN_TIME_READINESS_BUFFER` so the fix waits past it, and larger
/// than zero so the un-fixed path lands inside it. See the file-level
/// comment for the rationale.
const STUB_NOT_READY_MS: u64 = 300;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct DaemonHandle {
    _dir: TempDir,
    attach_path: PathBuf,
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
        let dir = common::race_safe_tempdir();
        let hook = dir.path().join("hook.sock");
        let attach = dir.path().join("attach.sock");
        (dir, hook, attach)
    };

    let state: SharedState = Arc::new(RwLock::new(AppState::default()));
    let daemon = Daemon::with_attach(state, attach_path.clone())
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
        attach_path,
        pty_registry,
        handle,
    }
}

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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    registry.snapshot(agent_id).unwrap_or_default()
}

/// Build an executable shell script that simulates Claude Code's
/// SessionStart-fires-before-input-readiness behavior:
///
/// 1. For the first `STUB_NOT_READY_MS` ms after launch, drain stdin
///    into `/dev/null` (background `dd`). Any bytes — payload AND
///    CR — written during this window are silently dropped.
/// 2. After the discard window, exec `cat -u`. Subsequent bytes are
///    echoed to stdout and surface in the PTY scrollback.
///
/// Using `dd` (POSIX) keeps the stub portable across the test
/// environments dot-agent-deck supports.
fn write_slow_readiness_stub(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("slow-readiness-stub.py");
    // PRD #128 regression: simulate Claude Code's TUI input being
    // unavailable until `STUB_NOT_READY_MS` after SessionStart.
    //
    // Python 3 lets us flip the PTY into full raw mode AND
    // deterministically discard bytes byte-by-byte during the
    // not-ready window. Pure-POSIX-sh stubs failed here for two
    // reasons:
    //
    // 1. `stty raw -echo` runs as a child process — race with the
    //    first daemon write.
    // 2. `cat`/`dd` drainers backgrounded with `&` may have their
    //    stdin reopened on /dev/null by the shell's job-control
    //    behavior (POSIX sh / dash) — they never see the bytes.
    //
    // Python flips raw mode in-process before any write, then loops
    // explicit `os.read` calls until the not-ready deadline passes.
    // Bytes consumed during this window are silently dropped. After
    // the window, the loop switches to echoing every byte read —
    // the CR surviving to this phase is the proof of submission.
    let script = format!(
        r#"#!/usr/bin/env python3
import os
import sys
import termios
import time

fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
new = list(old)
# Raw mode: clear canonical, echo, CR/LF translations, signal generation,
# and the OPOST output post-processing. Mirrors `stty raw -echo`.
new[0] &= ~(termios.IGNBRK | termios.BRKINT | termios.PARMRK
            | termios.ISTRIP | termios.INLCR | termios.IGNCR
            | termios.ICRNL | termios.IXON)
new[1] &= ~termios.OPOST
new[3] &= ~(termios.ECHO | termios.ECHONL | termios.ICANON
            | termios.ISIG | termios.IEXTEN)
termios.tcsetattr(fd, termios.TCSANOW, new)

os.write(1, b'STUB-RAW-READY')

# Discard window. Non-blocking read so we don't get stuck waiting on
# more bytes after the deadline passes.
os.set_blocking(fd, False)
deadline = time.monotonic() + {seconds}
while time.monotonic() < deadline:
    try:
        os.read(fd, 4096)
    except BlockingIOError:
        pass
    time.sleep(0.005)
os.set_blocking(fd, True)

os.write(1, b'STUB-CAT-READY')

# Echo phase: every byte read from stdin is echoed to stdout. The
# role-prompt CR appearing here proves it landed AFTER the
# discard window — the fix's readiness buffer held the write long
# enough.
while True:
    b = os.read(fd, 4096)
    if not b:
        break
    os.write(1, b)
"#,
        seconds = STUB_NOT_READY_MS as f64 / 1000.0,
    );
    std::fs::write(&path, script).expect("write stub script");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod stub");
    path
}

/// PRD #128 regression: with the fix, the spawn-time role prompt is
/// held until `SPAWN_TIME_READINESS_BUFFER` has elapsed past the
/// simulated SessionStart — which is after the agent stub's
/// "input-not-ready" window closes — so the CR arrives during the
/// stub's `cat -u` phase and is echoed back to the PTY master. The
/// scrollback contains the prompt followed by `\r\n`, proving the CR
/// was honored as a submit.
///
/// Toggle-verify (run manually before final commit):
/// - Set `SPAWN_TIME_READINESS_BUFFER` to `Duration::ZERO` in
///   `src/ui.rs`. Run this test — assertion fails: the write fires
///   immediately on simulated SessionStart, lands inside the stub's
///   discard window, and the role prompt never surfaces in scrollback.
/// - Restore `SPAWN_TIME_READINESS_BUFFER` to 500 ms. Test passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_time_role_prompt_submits_after_input_readiness_buffer() {
    let daemon = spawn_daemon().await;
    let stub_dir = common::race_safe_tempdir();
    let stub_path = write_slow_readiness_stub(&stub_dir);

    let controller = Arc::new(EmbeddedPaneController::new(
        daemon.attach_path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let controller_for_spawn = controller.clone();
    let stub_cmd = stub_path.display().to_string();
    let (pane_id, _resolved) = tokio::task::spawn_blocking(move || {
        controller_for_spawn
            .create_pane_with_options(Some(stub_cmd.as_str()), None, AgentSpawnOptions::default())
            .expect("create_pane_with_options")
    })
    .await
    .expect("join spawn task");

    let agent_id = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let records = daemon.pty_registry.agent_records();
            if let Some(rec) = records
                .iter()
                .find(|r| r.pane_id_env.as_deref() == Some(pane_id.as_str()))
            {
                break rec.id.clone();
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("daemon registry never surfaced agent for pane_id {pane_id}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    // Synchronize on `STUB-RAW-READY`: the stub emits this only after
    // `stty raw -echo` has taken effect on the PTY. Until then the
    // default line discipline would echo any write back to the master
    // and surface in scrollback regardless of whether the discard
    // window dropped the bytes — which would defeat the toggle.
    let raw_ready = wait_for_bytes_in_snapshot(
        &daemon.pty_registry,
        &agent_id,
        b"STUB-RAW-READY",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        raw_ready
            .windows(b"STUB-RAW-READY".len())
            .any(|w| w == b"STUB-RAW-READY"),
        "stub never reached raw-mode-ready state; snapshot = {:?}",
        String::from_utf8_lossy(&raw_ready)
    );

    // Simulated SessionStart: in production this is set by the
    // broadcast SessionStart event the agent's hook emits during
    // boot. Here, we stamp it at the moment the stub finishes raw-mode
    // setup — that's the moment its discard window begins, mirroring
    // production where the input-ready gap is measured from
    // SessionStart.
    let ready_since = Instant::now();

    // Drive the same gate the TUI loop drives. Without the fix, this
    // returns `true` immediately and the write lands inside the stub's
    // discard window. With the fix, this returns `false` until
    // `SPAWN_TIME_READINESS_BUFFER` has elapsed — by which time the
    // stub has switched to `cat -u`.
    let poll_deadline = Instant::now() + SPAWN_TIME_READINESS_BUFFER + Duration::from_millis(200);
    loop {
        if should_inject_spawn_time_prompt(Some(ready_since), Instant::now()) {
            break;
        }
        if Instant::now() > poll_deadline {
            panic!(
                "gate never opened within {:?} past SessionStart",
                SPAWN_TIME_READINESS_BUFFER
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let role_prompt = "ROLE-PROMPT-MARKER";
    let controller_for_write = controller.clone();
    let pane_for_write = pane_id.clone();
    let prompt_for_write = role_prompt.to_string();
    tokio::task::spawn_blocking(move || {
        controller_for_write
            .write_and_submit_to_pane(&pane_for_write, &prompt_for_write)
            .expect("write_and_submit_to_pane")
    })
    .await
    .expect("join write task");

    // The fix's success criterion: the CR was honored as a submit,
    // which surfaces in `cat -u`'s echo as the marker followed by
    // `\r\n` (terminal output processing turns the bare CR into
    // CRLF). Failure mode: the snapshot never contains the marker
    // because the bytes were dropped by the stub's `dd` discard
    // phase.
    // With the stub in raw mode (`stty raw -echo` disables OPOST),
    // `cat -u` outputs the bytes verbatim — no LF appended. The CR
    // surviving to the `cat -u` phase is the proof of submission:
    // it landed AFTER the discard window closed.
    let needle = b"ROLE-PROMPT-MARKER\r";
    let snap = wait_for_bytes_in_snapshot(
        &daemon.pty_registry,
        &agent_id,
        needle,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        snap.windows(needle.len()).any(|w| w == needle),
        "role prompt + CR was not echoed back by the stub's `cat -u` phase — the write \
         landed inside the stub's input-not-ready window and was discarded by the drainer. \
         Either the readiness buffer is too short, or the gate fired early. \
         snapshot = {:?}",
        String::from_utf8_lossy(&snap)
    );
}
