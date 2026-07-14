#![cfg(feature = "e2e")]

//! PRD #201 M3.2 — `dot-agent-deck orchestrator setup` CLI smoke.
//!
//! Thin real-binary spawn test (no PTY, no daemon, no LLM). It runs the real
//! `dot-agent-deck orchestrator setup` subprocess against a per-call temp HOME
//! and a fully-controlled PATH, so it deterministically exercises every branch
//! end to end through the actual clap wiring — which the pure-core unit tests in
//! `src/orchestrator_ext.rs` (test-plan rows 11-12) cannot reach:
//!
//!   - `pi` ABSENT (PATH has no `pi`)   → non-zero exit + the exact install hint.
//!   - `pi` PRESENT (a fake `pi` file on PATH) → exit 0 + the bundled extension
//!     materialized into `$HOME/.pi/agent/extensions/dot-agent-deck/`.
//!   - HOME UNSET / EMPTY (even with `pi` present) → non-zero exit + an error
//!     naming HOME, and NO materialization (Greptile P1: the explicit path must
//!     refuse rather than guess a `/tmp`/`./` location Pi never discovers).
//!
//! A *fake* `pi` (an empty but executable file named `pi` on PATH) keeps the
//! present-branch hermetic and deterministic whether or not real Pi is installed
//! on the runner: detection only asks "is there a regular *executable* file
//! named `pi` on PATH?" (c8110eb) — it never executes it. Real-Pi behavior (does
//! the materialized extension actually load and orchestrate) is M4.1's real-agent
//! e2e, out of scope here.
//!
//! Gated behind the `e2e` feature so `cargo test-fast` never compiles it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Path to the freshly-built binary under test (Cargo sets this at
/// integration-test build time).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dot-agent-deck")
}

/// The exact install hint the absent branch must print (kept in lockstep with
/// `orchestrator_ext::PI_INSTALL_HINT`).
const PI_INSTALL_HINT: &str = "npm install -g @earendil-works/pi-coding-agent";

/// Run `dot-agent-deck orchestrator setup` under a scrubbed environment: an
/// isolated `home` and an exact `path` (colon-joined dirs) so neither the real
/// `~/.pi` nor the developer's real PATH leaks in. Returns the captured output
/// plus combined stdout+stderr text for assertions.
fn run_setup(home: &Path, path_dirs: &[&Path]) -> (Output, String) {
    run_setup_home(Some(home.as_os_str()), path_dirs)
}

/// Like [`run_setup`] but with explicit control over the child's `HOME`:
/// `Some(value)` sets it (a real dir, or an empty string to exercise the
/// empty-HOME refusal), `None` leaves it entirely UNSET after `env_clear`. The
/// rest of the environment stays scrubbed so only HOME/PATH vary.
fn run_setup_home(home: Option<&std::ffi::OsStr>, path_dirs: &[&Path]) -> (Output, String) {
    let path = std::env::join_paths(path_dirs).expect("join PATH dirs");
    let mut cmd = Command::new(bin());
    cmd.args(["orchestrator", "setup"]);
    cmd.env_clear();
    if let Some(home) = home {
        cmd.env("HOME", home);
    }
    cmd.env("PATH", path);
    cmd.env("TERM", "xterm-256color");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd
        .output()
        .expect("spawn dot-agent-deck orchestrator setup");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out, combined)
}

fn ext_dir(home: &Path) -> PathBuf {
    home.join(".pi/agent/extensions/dot-agent-deck")
}

/// Give a fixture file the exec bit so it passes `is_executable_file` (c8110eb):
/// setup detection requires a regular *executable* file named `pi`, so a fake
/// `pi` created via `fs::write` must be `chmod +x`'d or it takes the not-present
/// branch. Unix-only; a no-op elsewhere, where the check accepts any regular
/// file.
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[test]
fn orchestrator_setup_pi_absent_prints_hint_and_fails() {
    let home = tempfile::tempdir().unwrap();
    // PATH points only at an empty dir → no `pi` discoverable.
    let empty = tempfile::tempdir().unwrap();

    let (out, text) = run_setup(home.path(), &[empty.path()]);

    assert!(
        !out.status.success(),
        "pi absent must exit non-zero, got {:?}.\noutput:\n{text}",
        out.status.code()
    );
    assert!(
        text.lines().any(|l| l == PI_INSTALL_HINT),
        "absent branch must print the exact install hint on its own line.\noutput:\n{text}"
    );
    assert!(
        !ext_dir(home.path()).exists(),
        "absent branch must NOT materialize anything under ~/.pi"
    );
}

#[test]
fn orchestrator_setup_pi_present_materializes_and_succeeds() {
    let home = tempfile::tempdir().unwrap();
    // A fake `pi` on PATH: detection checks for a regular *executable* file
    // named `pi` (it never runs it), so an empty file with the exec bit set is
    // enough to drive the present branch without a real Pi. The exec bit is
    // required since c8110eb (`is_executable_file`) — a non-executable candidate
    // takes the not-present branch.
    let bin_dir = tempfile::tempdir().unwrap();
    let pi = bin_dir.path().join("pi");
    std::fs::write(&pi, b"").unwrap();
    make_executable(&pi);

    let (out, text) = run_setup(home.path(), &[bin_dir.path()]);

    assert!(
        out.status.success(),
        "pi present must exit 0, got {:?}.\noutput:\n{text}",
        out.status.code()
    );
    let dir = ext_dir(home.path());
    assert!(
        dir.join("index.ts").is_file(),
        "present branch must materialize index.ts into {}",
        dir.display()
    );
    assert!(
        dir.join("orchestrator.ts").is_file(),
        "present branch must materialize orchestrator.ts into {}",
        dir.display()
    );
    assert!(
        text.contains("index.ts"),
        "success message should name what it wrote.\noutput:\n{text}"
    );
}

/// Scenario: run the real `orchestrator setup` with a fake (present) `pi` on
/// PATH but HOME entirely UNSET. It must REFUSE — exit non-zero with an error
/// naming HOME — instead of guessing a `/tmp` location and falsely reporting
/// success (Greptile P1). The present `pi` proves the ONLY reason to fail is the
/// unset HOME, and the absence of the "Enabled" banner and the install hint
/// confirms it neither materialized nor took the pi-absent branch.
#[test]
fn orchestrator_setup_home_unset_errors_without_materializing() {
    // A fake *present* `pi`, so the only reason to fail is the unset HOME.
    let bin_dir = tempfile::tempdir().unwrap();
    let pi = bin_dir.path().join("pi");
    std::fs::write(&pi, b"").unwrap();
    make_executable(&pi);

    let (out, text) = run_setup_home(None, &[bin_dir.path()]);

    assert!(
        !out.status.success(),
        "unset HOME must exit non-zero, got {:?}.\noutput:\n{text}",
        out.status.code()
    );
    assert!(
        text.contains("HOME"),
        "error must name HOME.\noutput:\n{text}"
    );
    assert!(
        !text.contains("Enabled"),
        "unset HOME must NOT report setup success.\noutput:\n{text}"
    );
    assert!(
        !text.lines().any(|l| l == PI_INSTALL_HINT),
        "unset HOME errors BEFORE the pi-presence check, so no install hint.\noutput:\n{text}"
    );
}

/// Scenario: run the real `orchestrator setup` with a fake (present) `pi` on
/// PATH but HOME set to the EMPTY string. Like the unset case it must refuse —
/// exit non-zero naming HOME — never resolving to a relative `./.pi/...` base
/// and never reporting success.
#[test]
fn orchestrator_setup_home_empty_errors_without_materializing() {
    let bin_dir = tempfile::tempdir().unwrap();
    let pi = bin_dir.path().join("pi");
    std::fs::write(&pi, b"").unwrap();
    make_executable(&pi);

    let (out, text) = run_setup_home(Some(std::ffi::OsStr::new("")), &[bin_dir.path()]);

    assert!(
        !out.status.success(),
        "empty HOME must exit non-zero, got {:?}.\noutput:\n{text}",
        out.status.code()
    );
    assert!(
        text.contains("HOME"),
        "error must name HOME.\noutput:\n{text}"
    );
    assert!(
        !text.contains("Enabled"),
        "empty HOME must NOT report setup success.\noutput:\n{text}"
    );
    // A relative `.pi/...` base must never be created in the CWD.
    assert!(
        !Path::new(".pi/agent/extensions/dot-agent-deck").exists(),
        "empty HOME must NOT materialize into a relative ./.pi base"
    );
}
