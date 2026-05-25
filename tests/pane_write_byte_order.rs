//! Byte-level reproducer harness for PRD #100 (Enter sometimes inserts a
//! newline into the orchestrator's input instead of submitting).
//!
//! Phase 1 / M1.2 asks for a deterministic reproducer along the PRD's
//! axes: very long messages, embedded newlines, rapid back-to-back
//! sends, sends right after orchestrator mount, sends while another
//! role pane animates. Three of those axes are already pinned in
//! `tests/orchestration_delegate.rs` (single-line CR, multi-line
//! bracketed paste, concurrent same-pane serialization). This file
//! covers the two PRD axes the existing tests don't:
//!
//! * **Very long single-line messages** — the size axis. The
//!   single-line/no-paste contract in `encode_pane_payload` must hold
//!   regardless of payload size; a regression to "wrap above N bytes"
//!   would silently introduce hypothesis-#1 surface on long prompts.
//! * **Notice-then-submit byte order** — the orchestration error path
//!   writes a notice (`\n` terminator) before a subsequent submit
//!   (`\r` terminator). The combined byte stream
//!   `notice\npayload\r` is exactly the surface PRD hypothesis #2
//!   predicts: claude-style agents read the LF as newline-in-input,
//!   so the CR submits `notice\npayload` together. We pin the bytes
//!   here so Phase 2 has a stable before/after.
//!
//! The bug itself lives in the receiving agent (Claude Code /
//! similar) and depends on that agent's bracketed-paste + Enter
//! interpretation, which we cannot run in CI. What CI *can* pin is
//! the framing the deck produces — the receiving agent only ever
//! sees these bytes, so a contract on the framing is a contract on
//! the bug surface.
//!
//! Each agent runs `cat -u`. The host PTY applies line discipline on
//! the way out: every `\n` written by the daemon-side writer surfaces
//! in the scrollback as `\r\n` (ONLCR), and each input is echoed
//! twice (once by the kernel's PTY echo, once by `cat` reading and
//! writing back). Assertions below avoid raw `\r`/`\n` counting and
//! instead pin facts that survive line discipline: presence and
//! relative order of `\x1b[200~` / `\x1b[201~` markers and message
//! bodies.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, TabMembership, is_valid_pane_id_env,
};
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::state::{AppState, SharedState};

mod common;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct DaemonHandle {
    _dir: TempDir,
    #[allow(dead_code)]
    hook_path: PathBuf,
    attach_path: PathBuf,
    #[allow(dead_code)]
    state: SharedState,
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
    let daemon = Daemon::with_attach(state.clone(), attach_path.clone())
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
        hook_path,
        attach_path,
        state,
        pty_registry,
        handle,
    }
}

async fn start_pane(daemon: &DaemonHandle, pane_id: &str) -> String {
    assert!(is_valid_pane_id_env(pane_id), "pane_id must be valid");
    let client = DaemonClient::new(daemon.attach_path.clone());
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    client
        .start_agent(StartAgentOptions {
            command: Some("cat -u".to_string()),
            cwd: Some(cwd),
            display_name: Some(pane_id.to_string()),
            rows: 24,
            cols: 80,
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: "byte-order".to_string(),
                role_index: 0,
                role_name: pane_id.to_string(),
                is_start_role: false,
                orchestration_cwd: None,
            }),
            agent_type: None,
        })
        .await
        .expect("start_agent")
}

/// Poll the agent's scrollback for `needle` and return the snapshot
/// captured at the moment the needle first appears. Panics on timeout.
async fn wait_for_in_snapshot(
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
    let last = registry.snapshot(agent_id).unwrap_or_default();
    panic!(
        "needle {:?} not found in agent {} scrollback within {:?}; \
         last snapshot: {:?}",
        String::from_utf8_lossy(needle),
        agent_id,
        timeout,
        String::from_utf8_lossy(&last)
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// PRD axis: very long single-line message. The single-line policy in
/// `encode_pane_payload` (no bracketed-paste wrap unless the payload
/// contains an embedded `\n`) must hold regardless of payload size.
/// A "wrap above N bytes" regression would introduce hypothesis #1
/// surface — Enter inside the paste read as newline-in-input — for
/// any user prompt longer than the threshold.
#[tokio::test]
async fn daemon_very_long_single_line_message_stays_unwrapped() {
    let daemon = spawn_daemon().await;
    let agent_id = start_pane(&daemon, "long-line-pane").await;

    // 8 KB single-line payload (well past typical PTY chunking and
    // any plausible buffer-size threshold a future regression might
    // introduce).
    let big = "x".repeat(8 * 1024);
    daemon
        .pty_registry
        .write_to_pane_and_submit("long-line-pane", &big)
        .await
        .expect("write_to_pane_and_submit");

    // Wait for at least the first KB of x's to surface so the write
    // has certainly flushed.
    let needle = "x".repeat(1024).into_bytes();
    let snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &agent_id,
        &needle,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        !contains(&snap, b"\x1b[200~"),
        "long single-line payload must not be bracketed-paste wrapped"
    );
    assert!(
        !contains(&snap, b"\x1b[201~"),
        "long single-line payload must not carry a bracketed-paste end marker"
    );
}

/// PRD axis: a notice (LF terminator, no submit) followed by a submit
/// (CR terminator) on the same pane. This is the orchestration error
/// path: `handle_delegate` writes a respawn-failure notice to the
/// orchestrator pane via `write_to_pane_notice`, and a subsequent
/// `handle_work_done` writes feedback via `write_to_pane_and_submit`.
///
/// Pin: notice precedes prompt in the byte stream, and neither single-
/// line payload introduces bracketed-paste framing. Whether the
/// notice's LF then fuses into the next submit is a property of the
/// receiving agent's input box — claude-style agents would interpret
/// `notice\npayload\r` as a two-line input submitted on the CR. We
/// don't assert that here (no live agent in CI), but the byte order
/// pinned by this test is the bug surface for PRD hypothesis #2 (LF
/// vs CR framing inconsistency).
#[tokio::test]
async fn daemon_notice_then_submit_preserves_order_and_avoids_paste_framing() {
    let daemon = spawn_daemon().await;
    let agent_id = start_pane(&daemon, "notice-then-submit").await;

    daemon
        .pty_registry
        .write_to_pane_notice("notice-then-submit", "NOTICE-MARKER")
        .await
        .expect("write_to_pane_notice");
    daemon
        .pty_registry
        .write_to_pane_and_submit("notice-then-submit", "USER-PROMPT")
        .await
        .expect("write_to_pane_and_submit");

    let snap = wait_for_in_snapshot(
        &daemon.pty_registry,
        &agent_id,
        b"USER-PROMPT",
        Duration::from_secs(5),
    )
    .await;

    let text = String::from_utf8_lossy(&snap);
    let notice_pos = text.find("NOTICE-MARKER").expect("notice must surface");
    let prompt_pos = text.find("USER-PROMPT").expect("prompt must surface");
    assert!(
        notice_pos < prompt_pos,
        "notice must precede prompt in the byte stream; got {text:?}"
    );
    assert!(
        !contains(&snap, b"\x1b[200~"),
        "single-line notice + single-line prompt must not introduce \
         bracketed-paste framing; got {text:?}"
    );
    assert!(
        !contains(&snap, b"\x1b[201~"),
        "single-line notice + single-line prompt must not introduce \
         bracketed-paste framing; got {text:?}"
    );
}
