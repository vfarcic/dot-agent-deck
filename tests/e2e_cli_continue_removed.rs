#![cfg(feature = "e2e")]

//! PRD #89 Phase 3 — the `--continue` flag is removed from the CLI surface
//! (M3.1) and invoking it is rejected with a guiding message (M3.4).
//!
//! Thin real-binary spawn test — no PTY drive. It runs `dot-agent-deck --help`
//! and `dot-agent-deck --continue` as plain subprocesses and asserts the flag
//! is gone from `--help` and rejected (non-zero exit) on use. RED today:
//! `--continue` is still a declared `clap` argument, so `--help` advertises it
//! and `dot-agent-deck --continue` is accepted (it exits 0 — the flag parses,
//! then terminal init fails in a worker without a TTY, but the process still
//! exits successfully).
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use spec::spec;

/// Path to the freshly-built binary under test (Cargo sets this at
/// integration-test build time).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dot-agent-deck")
}

/// Run the binary with `args` under an isolated, scrubbed environment so the
/// spawn can neither read the developer's real session/config nor leak a
/// long-lived daemon: a per-call tempdir HOME + sockets + state dir, a short
/// idle-shutdown, and the test max-lifetime backstop. stdin is `/dev/null`;
/// stdout/stderr are captured. Returns the captured `Output` (combined stdout +
/// stderr text and the exit status).
fn run_isolated(args: &[&str]) -> (std::process::Output, String) {
    let home = common::race_safe_tempdir();
    let h: &Path = home.path();
    let mut cmd = Command::new(bin());
    cmd.args(args);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", h);
    cmd.env("TERM", "xterm-256color");
    cmd.env("DOT_AGENT_DECK_SOCKET", h.join("hook.sock"));
    cmd.env("DOT_AGENT_DECK_ATTACH_SOCKET", h.join("attach.sock"));
    cmd.env("DOT_AGENT_DECK_STATE_DIR", h.join("state"));
    // Reap any daemon this spawn lazily starts quickly (no clients/agents) and
    // cap its lifetime as a backstop.
    cmd.env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "1");
    cmd.env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().expect("spawn dot-agent-deck");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Keep `home` alive until after the child has fully exited.
    drop(home);
    (out, combined)
}

/// Scenario: Run `dot-agent-deck --help` and assert its output no longer
/// advertises `--continue`, then run `dot-agent-deck --continue` and assert the
/// process exits non-zero with a message that references the flag (guiding the
/// user toward the new auto-restore default). RED today on both counts:
/// `--help` still lists `--continue` ("Restore pane session from last exit"),
/// and `dot-agent-deck --continue` is an accepted flag so the process exits 0
/// rather than being rejected.
#[spec("cli/continue-removed/001")]
#[test]
fn continue_removed_001_flag_gone_from_cli_surface() {
    // --- `--help` must no longer advertise the removed flag. ---
    let (help_out, help_text) = run_isolated(&["--help"]);
    assert!(
        help_out.status.success(),
        "`dot-agent-deck --help` should exit 0, got {:?}.\noutput:\n{help_text}",
        help_out.status.code()
    );
    assert!(
        !help_text.contains("--continue"),
        "PRD #89 M3.1: `--continue` must be removed from the CLI surface, but \
         `dot-agent-deck --help` still lists it. RED until the `clap` argument is deleted \
         from `Cli`.\n--help output:\n{help_text}"
    );

    // --- Invoking the removed flag must be rejected, not accepted. ---
    let (cont_out, cont_text) = run_isolated(&["--continue"]);
    assert!(
        !cont_out.status.success(),
        "PRD #89 M3.4: `dot-agent-deck --continue` must exit non-zero once the flag is \
         removed (clap rejects the unknown argument). RED today: the flag is still accepted, \
         so it parses and the process exits successfully (exit {:?}).\noutput:\n{cont_text}",
        cont_out.status.code()
    );
    assert!(
        cont_text.to_lowercase().contains("continue"),
        "the rejection should reference the removed `--continue` flag so the user is guided \
         toward the new auto-restore default, but the error did not mention it.\noutput:\n{cont_text}"
    );
}
