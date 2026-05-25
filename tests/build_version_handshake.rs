//! PRD #103 M4.2 — integration tests for the local-attach build-version
//! handshake (`build_version_handshake::ensure_compatible_daemon_or_die`).
//!
//! Each test spawns:
//!   * a real `dot-agent-deck daemon serve` subprocess (the same binary
//!     pattern as `tests/daemon_stop.rs`), so peer-credential PID
//!     lookups and SIGTERM-driven recovery exercise the production code
//!     paths rather than an in-process tokio task; and
//!   * a real `dot-agent-deck` TUI subprocess, either under a piped
//!     stdio (non-TTY scenarios 4 + 5) or a portable-pty PTY pair (TTY
//!     scenarios 1–3).
//!
//! The build_id under comparison is injected via the test-only
//! `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` env var. Both
//! `AttachResponse::hello` (daemon side, via
//! [`dot_agent_deck::build_id::local_build_id`]) and
//! `ensure_compatible_daemon_or_die` (TUI side, same helper) honour the
//! same variable, so setting it independently on the daemon and TUI
//! subprocesses simulates the same-tag / different-commit skew without
//! rebuilding the binary at a synthetic `DAD_BUILD_ID`.
//!
//! The TUI subprocess additionally sets `DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE`
//! to bail out cleanly after the handshake + lazy-respawn succeed,
//! instead of entering the full ratatui session (which would never exit
//! on its own inside `cargo test`).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tempfile::TempDir;
use tokio::net::UnixStream;

use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};

mod common;

/// Build-ids sharing the `0.25.0-` tag prefix but differing in the
/// `-g<sha>` suffix — the same-tag / different-commit case that the
/// PRD explicitly calls out. Used in the "no agents, S press" test
/// since the recovery flow there is the most thorough exercise of the
/// mismatch path.
const DAEMON_BUILD: &str = "0.25.0-gdaemon00";
const TUI_BUILD: &str = "0.25.0-gclient00";

/// Subprocess daemon scoped to a tempdir. On drop, kills the
/// originally-spawned daemon and additionally runs `daemon stop
/// --force` against the socket path so any lazy-respawn that replaced
/// our child (after the TUI's SIGTERM in the recovery flow) is also
/// cleaned up. Without that second step a successful recovery test
/// would leak a detached daemon between cargo test invocations.
struct TestDaemon {
    _dir: TempDir,
    attach_path: PathBuf,
    hook_path: PathBuf,
    state_dir: PathBuf,
    lock_dir: PathBuf,
    child: Option<Child>,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Best-effort cleanup of any lazy-respawned daemon. Ignored
        // failures are fine — if nothing is listening, this is a no-op.
        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let _ = Command::new(bin)
            .args(["daemon", "stop", "--force"])
            .env("DOT_AGENT_DECK_SOCKET", &self.hook_path)
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &self.attach_path)
            .env("DOT_AGENT_DECK_STATE_DIR", &self.state_dir)
            .env("DOT_AGENT_DECK_LOCK_DIR", &self.lock_dir)
            .env("RUST_LOG", "error")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Spawn the daemon subprocess and wait for the attach socket to be
/// accepting connections. `build_id_override` is set on the daemon's
/// env iff `Some(_)` — `None` lets the daemon fall back to its
/// compile-time `DAD_BUILD_ID`.
async fn spawn_daemon(build_id_override: Option<&str>) -> TestDaemon {
    // PRD #103 / CodeRabbit finding #5: bare `tempfile::tempdir()`
    // races with the daemon's umask flip during socket bind and lands
    // at 0o600, which then trips EACCES on subsequent `bind(2)`. The
    // race-safe helper re-chmods to 0o700 immediately after creation.
    let dir = common::race_safe_tempdir();
    let attach_path = dir.path().join("attach.sock");
    let hook_path = dir.path().join("hook.sock");
    let state_dir = dir.path().join("state");
    let lock_dir = dir.path().join("lock");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::create_dir_all(&lock_dir).unwrap();

    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"])
        .env("DOT_AGENT_DECK_SOCKET", &hook_path)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &attach_path)
        .env("DOT_AGENT_DECK_STATE_DIR", &state_dir)
        .env("DOT_AGENT_DECK_LOCK_DIR", &lock_dir)
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(bid) = build_id_override {
        cmd.env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", bid);
    } else {
        // Defensive: ensure no leaked override from the test runner's
        // own env reaches the daemon. We never set it on the test
        // process, but a future change could.
        cmd.env_remove("DOT_AGENT_DECK_BUILD_ID_OVERRIDE");
    }
    let mut child = cmd
        .spawn()
        .expect("spawn dot-agent-deck daemon serve subprocess");

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if attach_path.exists() && UnixStream::connect(&attach_path).await.is_ok() {
            return TestDaemon {
                _dir: dir,
                attach_path,
                hook_path,
                state_dir,
                lock_dir,
                child: Some(child),
            };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!(
        "subprocess daemon failed to bind {} within 15s",
        attach_path.display()
    );
}

/// Build the env-var block applied to a TUI subprocess. Pinned to the
/// same socket / state paths the daemon was started with.
fn tui_env_pairs(d: &TestDaemon) -> Vec<(&'static str, String)> {
    vec![
        (
            "DOT_AGENT_DECK_SOCKET",
            d.hook_path.to_string_lossy().into_owned(),
        ),
        (
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            d.attach_path.to_string_lossy().into_owned(),
        ),
        (
            "DOT_AGENT_DECK_STATE_DIR",
            d.state_dir.to_string_lossy().into_owned(),
        ),
        (
            "DOT_AGENT_DECK_LOCK_DIR",
            d.lock_dir.to_string_lossy().into_owned(),
        ),
        // Bail out after the handshake (+ optional lazy-respawn)
        // instead of entering the full TUI. PRD #103 M4.2.
        ("DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE", "1".to_string()),
        ("RUST_LOG", "error".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Scenario 4 — non-TTY / CI fallback. stdout piped → `is_terminal()` is
// false → handshake prints the plain stderr message and exits non-zero.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_tty_mismatch_prints_stderr_and_exits_nonzero() {
    let daemon = spawn_daemon(Some(DAEMON_BUILD)).await;
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");

    let mut cmd = Command::new(bin);
    cmd.env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", TUI_BUILD)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in tui_env_pairs(&daemon) {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn TUI subprocess");

    assert!(
        !output.status.success(),
        "non-TTY mismatch must exit non-zero, got status {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // PRD #103 M2.1 / `render_non_tty_error` pin this text exactly:
    // both build-ids on one line, recovery hint on the next.
    let expected = format!(
        "error: local daemon is build {DAEMON_BUILD} but this TUI is build {TUI_BUILD}\n\
         recover with: dot-agent-deck daemon stop\n"
    );
    assert!(
        stderr.contains(&expected),
        "stderr must contain the canonical mismatch error.\n\
         expected:\n{expected}\n\
         actual:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 5 — match. Daemon and TUI advertise the same build_id; the
// handshake returns silently and the TUI proceeds (we early-exit via
// `DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE`).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn match_path_is_silent_and_exits_zero() {
    // Both daemon and TUI use the same override: a pinned synthetic
    // value, so the test does not depend on the value of the
    // compile-time `DAD_BUILD_ID`.
    let daemon = spawn_daemon(Some(DAEMON_BUILD)).await;
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");

    let mut cmd = Command::new(bin);
    cmd.env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", DAEMON_BUILD)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in tui_env_pairs(&daemon) {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn TUI subprocess");

    assert!(
        output.status.success(),
        "match path must exit 0, got {:?}; stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Daemon version mismatch"),
        "match path must not render the mismatch prompt on stderr: {stderr}"
    );
    assert!(
        !stderr.contains("error: local daemon is build"),
        "match path must not print the non-TTY mismatch error: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// PTY harness — used by scenarios 1, 2, 3. `portable-pty` is already a
// regular dependency of the crate (for agent PTYs), so we reuse it
// here rather than introducing a new dev-dep.
// ---------------------------------------------------------------------------

struct PtyChild {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// PTY output captured by a background reader thread. Tests poll
    /// this for the prompt text; in raw mode `\n` is rendered as
    /// `\r\n` so callers must normalise before strict comparisons.
    output: Arc<Mutex<Vec<u8>>>,
}

impl PtyChild {
    fn spawn(daemon: &TestDaemon, build_id_override: &str) -> Self {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let mut cmd = CommandBuilder::new(bin);
        cmd.env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", build_id_override);
        for (k, v) in tui_env_pairs(daemon) {
            cmd.env(k, v);
        }
        // `TERM` would normally be inherited from the cargo test env;
        // pin it to a safe value so crossterm's raw-mode probe doesn't
        // drift across CI runners.
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).expect("spawn pty TUI");
        drop(pair.slave);

        let writer = pair.master.take_writer().expect("take_writer");
        let mut reader = pair.master.try_clone_reader().expect("clone reader");

        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let output_for_thread = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut guard = output_for_thread.lock().unwrap();
                        guard.extend_from_slice(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            master: pair.master,
            writer,
            output,
        }
    }

    /// Wait until the captured PTY output contains `needle`, or the
    /// deadline elapses. Returns the normalised (`\r\n` → `\n`)
    /// snapshot of the output at the moment the needle was found.
    fn wait_for(&self, needle: &str, timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let normalised = {
                let guard = self.output.lock().unwrap();
                String::from_utf8_lossy(&guard).replace("\r\n", "\n")
            };
            if normalised.contains(needle) {
                return Ok(normalised);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for {needle:?} in PTY output.\n\
                     captured (normalised):\n{normalised}"
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    /// Block on the child exiting (`portable_pty::Child::wait` is
    /// synchronous). Drops the master afterwards so the PTY backing
    /// store is released.
    fn wait_exit(mut self, timeout: Duration) -> Result<portable_pty::ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    drop(self.master);
                    return Ok(status);
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return Err(format!(
                            "child did not exit within {timeout:?}; killed.\n\
                             captured:\n{}",
                            String::from_utf8_lossy(&self.output.lock().unwrap())
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("wait error: {e}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario 1 — TTY, no live agents, user presses S. Daemon stops, TUI
// proceeds normally. Also the same-tag / different-commit case (both
// build-ids share the `0.25.0-` prefix but differ in `-g<sha>`).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tty_no_agents_press_s_stops_daemon_and_exits_zero() {
    let mut daemon = spawn_daemon(Some(DAEMON_BUILD)).await;

    let mut pty = PtyChild::spawn(&daemon, TUI_BUILD);

    // Wait for the prompt body to render. The PRD pins this text
    // character-for-character; we anchor on the closing keybind line
    // because the leading `⚠` glyph is multi-byte and any partial-utf8
    // capture would still contain the keybind line in full.
    let snapshot = pty
        .wait_for("[S] stop daemon and continue", Duration::from_secs(15))
        .expect("prompt must render");

    // Assert the PRD-pinned text in full — every byte must be present,
    // proving both build-ids surfaced correctly and the no-agents
    // branch was taken.
    let expected_block = format!(
        "⚠  Daemon version mismatch\n\
         \x20  running daemon:  {DAEMON_BUILD}\n\
         \x20  this binary:     {TUI_BUILD}\n\
         \n\
         \x20  [S] stop daemon and continue   [Q] quit\n"
    );
    assert!(
        snapshot.contains(&expected_block),
        "prompt body must match the no-agents PRD form exactly.\n\
         expected:\n{expected_block}\n\
         got:\n{snapshot}"
    );

    // Press S. The handshake SIGTERMs the daemon and the TUI re-runs
    // `ensure_external_daemon_or_die`, which lazy-spawns a fresh
    // daemon at the (overridden) TUI build_id. Then the
    // `DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE` escape hatch fires and
    // the TUI exits 0.
    pty.send(b"S");

    let status = pty
        .wait_exit(Duration::from_secs(15))
        .expect("TUI must exit after S");
    assert!(
        status.success(),
        "TUI must exit 0 after stopping the daemon and lazy-respawning, got {status:?}"
    );

    // The OG daemon process must be gone — confirms peer_pid +
    // SIGTERM actually ran end-to-end against the right subprocess.
    // `wait()` returns once the kernel reaps the child; if SIGTERM
    // did nothing, this would hang past the spawn_blocking budget.
    let mut og_child = daemon.child.take().expect("og child handle");
    let _exit = tokio::task::spawn_blocking(move || og_child.wait().expect("wait og daemon"))
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Scenario 2 — TTY, no live agents, user presses Q. Daemon untouched,
// process exits non-zero.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tty_no_agents_press_q_aborts_and_leaves_daemon_running() {
    let daemon = spawn_daemon(Some(DAEMON_BUILD)).await;
    let mut pty = PtyChild::spawn(&daemon, TUI_BUILD);

    pty.wait_for("[Q] quit", Duration::from_secs(15))
        .expect("prompt must render");

    pty.send(b"Q");

    let status = pty
        .wait_exit(Duration::from_secs(15))
        .expect("TUI must exit after Q");
    assert!(
        !status.success(),
        "Q press must exit non-zero, got {status:?}"
    );

    // Daemon must still be accepting connections — the Q branch must
    // not SIGTERM anything.
    let connect = UnixStream::connect(&daemon.attach_path).await;
    assert!(
        connect.is_ok(),
        "daemon must still be alive after Q: connect result = {connect:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3 — TTY, live agent(s) present, user presses S. First S
// re-renders the prompt naming the agents; second S stops the daemon.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tty_with_live_agents_requires_two_s_presses_and_names_agents() {
    let daemon = spawn_daemon(Some(DAEMON_BUILD)).await;

    // Start a long-lived agent so the handshake's ListAgents round
    // -trip returns at least one entry — this is what triggers the
    // data-loss-guard branch of the prompt.
    let client = DaemonClient::new(daemon.attach_path.clone());
    let agent_id = client
        .start_agent(StartAgentOptions {
            command: Some("sleep 60".into()),
            ..StartAgentOptions::default()
        })
        .await
        .expect("StartAgent must succeed");
    assert!(!agent_id.is_empty(), "agent_id must be populated");

    let mut pty = PtyChild::spawn(&daemon, TUI_BUILD);

    // Wait for the with-agents prompt. The PRD pins the header at
    // `(N managed agent(s) running)` and the agent IDs as indented
    // lines beneath; the data-loss warning sits between the list and
    // the keybinds.
    let snapshot = pty
        .wait_for("[S] stop daemon and continue", Duration::from_secs(15))
        .expect("with-agents prompt must render");
    assert!(
        snapshot.contains("(1 managed agent(s) running)"),
        "header must use the PRD-pinned plural form '(N managed agent(s) running)': {snapshot}"
    );
    assert!(
        snapshot.contains(&agent_id),
        "prompt must name the live agent id ({agent_id}): {snapshot}"
    );
    assert!(
        snapshot.contains("Stopping the daemon will end these agents."),
        "prompt must warn about data loss: {snapshot}"
    );
    // PRD #103 M4.2 / CodeRabbit finding #3: the live-agent prompt
    // must surface the same `running daemon:` / `this binary:` build
    // IDs as the no-agent prompt — without them the user can't tell
    // which side of the mismatch is newer.
    assert!(
        snapshot.contains(&format!("running daemon:  {DAEMON_BUILD}")),
        "live-agent prompt must name the daemon build id: {snapshot}"
    );
    assert!(
        snapshot.contains(&format!("this binary:     {TUI_BUILD}")),
        "live-agent prompt must name the TUI build id: {snapshot}"
    );

    // First S — must NOT terminate yet. The implementation re-renders
    // the prompt and waits for a second S. We can't reliably observe
    // the re-render (raw mode doesn't acknowledge the keystroke
    // out-of-band), so we just confirm the daemon is still up after a
    // brief delay.
    pty.send(b"S");
    std::thread::sleep(Duration::from_millis(300));
    let still_alive = UnixStream::connect(&daemon.attach_path).await;
    assert!(
        still_alive.is_ok(),
        "first S with live agents must not SIGTERM the daemon: {still_alive:?}"
    );

    // Second S — now the handshake SIGTERMs and the TUI lazy-respawns
    // before early-exiting.
    pty.send(b"S");

    let status = pty
        .wait_exit(Duration::from_secs(15))
        .expect("TUI must exit after second S");
    assert!(
        status.success(),
        "TUI must exit 0 after second S, got {status:?}"
    );
}
