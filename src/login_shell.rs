//! PRD #170 M1.1 / M1.2 — login-shell PATH parity for daemon-spawned panes.
//!
//! The daemon spawns every pane command (`agent_pty`'s `CommandBuilder` →
//! `portable-pty`) by resolving a *bare* command against the daemon's OWN
//! process PATH. When the daemon is launched without the user's login profile
//! — over SSH non-interactively, or any non-login context — its PATH lacks the
//! dir where `claude`/`opencode` live (`~/.local/bin`), so a bare command fails
//! to spawn.
//!
//! The fix is a single daemon-startup block: capture the user's login-shell
//! PATH once (`$SHELL -lc 'printf %s "$PATH"'`, bounded by a timeout) and set it
//! into the daemon's own environment. Every pane the daemon subsequently spawns
//! inherits that PATH automatically, with NO change to the hot spawn path. On
//! capture failure (no `$SHELL`, non-zero exit, timeout, or empty output) the
//! daemon keeps its inherited PATH, so behavior never regresses.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Upper bound on the login-shell capture. A login shell sourcing the user's
/// profile is normally sub-second; on timeout we fall back to the inherited
/// PATH (no regression).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the capture polls the child for completion while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Capture the login-shell PATH by running `$SHELL -lc 'printf %s "$PATH"'`.
///
/// Returns `Some(path)` only on a clean, non-empty result. Returns `None` when
/// `$SHELL` is unset/empty, the shell fails to spawn, exits non-zero, times out,
/// or prints nothing but whitespace.
pub fn capture_login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok()?;
    capture_path_via_shell(&shell)
}

/// Run `<shell> -lc 'printf %s "$PATH"'` with a timeout and return the trimmed
/// stdout. Split out from [`capture_login_shell_path`] so the parse/fallback
/// branches are unit-testable with a fake `$SHELL` (a temp script) without
/// mutating the process environment.
fn capture_path_via_shell(shell: &str) -> Option<String> {
    if shell.trim().is_empty() {
        return None;
    }

    // `-lc <command>`: a login shell (`-l`) running a single command string
    // (`-c`), exactly as the PRD specifies. `printf %s` emits the PATH with no
    // trailing newline; we trim anyway to be robust to shells that add one.
    let mut child = Command::new(shell)
        .arg("-lc")
        .arg(r#"printf %s "$PATH""#)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Poll for completion so we can enforce the timeout AND kill a hung shell.
    // The captured output (PATH) is far smaller than the pipe buffer, so it is
    // safe to read it only after exit; a shell that floods stdout instead would
    // block on write and simply hit the timeout below.
    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => return None,
        }
    };

    if !status.success() {
        return None;
    }

    let mut out = String::new();
    child.stdout.as_mut()?.read_to_string(&mut out).ok()?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Apply the captured login-shell PATH to the daemon's OWN process environment.
///
/// Call this ONCE at daemon-process startup, before the async runtime and any
/// worker threads start. On `Some(path)` it sets `PATH` (so every pane the
/// daemon later spawns inherits it); on `None` it leaves the inherited PATH
/// untouched. Either outcome is logged.
pub fn apply_login_shell_path() {
    match capture_login_shell_path() {
        Some(path) => {
            // SAFETY: called from the synchronous `main` dispatch at daemon
            // startup, BEFORE the tokio runtime and any worker threads exist
            // (`run_daemon_serve_cli` is `#[tokio::main]`, so it builds its
            // runtime only when *called*, after this returns). The process is
            // single-threaded here, so no concurrent `getenv` can race this
            // `setenv` — the PRD's stated soundness condition for `set_var`.
            unsafe {
                std::env::set_var("PATH", &path);
            }
            tracing::info!(
                path = %path,
                "PRD #170: applied login-shell PATH to the daemon environment"
            );
        }
        None => {
            tracing::info!(
                "PRD #170: no login-shell PATH captured; keeping the daemon's inherited PATH"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Write `body` to a fresh executable script under `dir` and return its path.
    fn write_shell(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write fake shell");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake shell");
        path
    }

    #[test]
    fn captures_path_from_login_shell_stdout() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A fake `$SHELL` that, invoked with `-lc <cmd>`, prints a known PATH.
        let shell = write_shell(
            dir.path(),
            "ok-shell.sh",
            "#!/bin/sh\nprintf %s \"/opt/login/bin:/usr/bin\"\n",
        );
        assert_eq!(
            capture_path_via_shell(&shell.to_string_lossy()),
            Some("/opt/login/bin:/usr/bin".to_string()),
        );
    }

    #[test]
    fn trims_trailing_whitespace_from_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = write_shell(
            dir.path(),
            "trailing-shell.sh",
            "#!/bin/sh\nprintf '%s\\n' \"/opt/login/bin\"\n",
        );
        assert_eq!(
            capture_path_via_shell(&shell.to_string_lossy()),
            Some("/opt/login/bin".to_string()),
        );
    }

    #[test]
    fn none_on_non_zero_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Prints a plausible PATH but exits non-zero — must be rejected.
        let shell = write_shell(
            dir.path(),
            "fail-shell.sh",
            "#!/bin/sh\nprintf %s \"/opt/login/bin\"\nexit 1\n",
        );
        assert_eq!(capture_path_via_shell(&shell.to_string_lossy()), None);
    }

    #[test]
    fn none_on_empty_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Exits cleanly but prints only whitespace — must fall back to None.
        let shell = write_shell(dir.path(), "blank-shell.sh", "#!/bin/sh\nprintf '   \\n'\n");
        assert_eq!(capture_path_via_shell(&shell.to_string_lossy()), None);
    }

    #[test]
    fn none_on_empty_shell_path() {
        // The empty-`$SHELL` branch (same as a missing `$SHELL`, which makes
        // `capture_login_shell_path` short-circuit on the `?`).
        assert_eq!(capture_path_via_shell(""), None);
        assert_eq!(capture_path_via_shell("   "), None);
    }

    #[test]
    fn none_when_shell_binary_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist-shell");
        assert_eq!(capture_path_via_shell(&missing.to_string_lossy()), None);
    }
}
