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
//! PATH once and set it into the daemon's own environment. The capture runs the
//! user's `$SHELL` as an *interactive login* shell (`-ilc`) — the same kind of
//! shell an SSH session gets — so it sources `~/.bashrc` exactly as an
//! interactive login does. That matters: many CLI installers (e.g. `opencode`)
//! append their `PATH` line to `~/.bashrc`, *after* the standard
//! `case $- in *i*) ;; *) return;; esac` guard that bails out of a
//! non-interactive shell, so a login-*only* (`-lc`) capture never sees them.
//! Every pane the daemon subsequently spawns inherits the captured PATH
//! automatically, with NO change to the hot spawn path. On capture failure (no
//! `$SHELL`, non-zero exit, timeout, or unusable output) the daemon keeps its
//! inherited PATH, so behavior never regresses.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Upper bound on the login-shell capture. An interactive login shell sourcing
/// the user's profile *and* `~/.bashrc` is normally well under a second, but a
/// heavy interactive setup (nvm / conda / rbenv init, version-manager hooks)
/// can take longer, so we allow generous headroom. The bound only ever bites a
/// hung shell — a healthy one exits in milliseconds — and on timeout we fall
/// back to the inherited PATH (no regression).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the capture polls the child for completion while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Marker tokens that wrap the PATH in the probe's stdout. We extract the PATH
/// from *between* them rather than reading raw stdout, because an **interactive**
/// startup file (`~/.bashrc` & friends) may print a banner, MOTD, version line,
/// or other noise to stdout before our `printf` ever runs. A real PATH never
/// contains these literal tokens, so the slice between them is exactly the PATH.
const PATH_MARK_BEGIN: &str = "__DOT_AGENT_DECK_PATH_BEGIN__";
const PATH_MARK_END: &str = "__DOT_AGENT_DECK_PATH_END__";

/// Capture the login-shell PATH by running the user's `$SHELL` as an
/// **interactive login** shell and printing `$PATH`.
///
/// Returns `Some(path)` only on a clean, non-empty result. Returns `None` when
/// `$SHELL` is unset/empty, the shell fails to spawn, exits non-zero, times out,
/// or prints no usable PATH.
pub fn capture_login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok()?;
    capture_path_via_shell(&shell)
}

/// Run `<shell> -ilc '<probe>'` with a timeout and return the captured PATH.
///
/// The shell is invoked **interactive** (`-i`) **and** login (`-l`) — the same
/// combination an SSH session gets — so it sources the identical startup files,
/// including `~/.bashrc`. That is the crux of the fix: most CLI installers (e.g.
/// `opencode`) append their `PATH` line to `~/.bashrc`, *after* the standard
/// `case $- in *i*) ;; *) return;; esac` guard that bails out of a
/// non-interactive shell. A login-only (`-lc`) capture hits that guard and never
/// sees those dirs, so a bare `opencode` fails to resolve even though it works
/// over SSH. Going interactive makes the captured PATH match "what works when you
/// SSH in" — the invariant users expect.
///
/// Split out from [`capture_login_shell_path`] so the parse branches are
/// unit-testable with a fake `$SHELL` (a temp script) without mutating the
/// process environment.
fn capture_path_via_shell(shell: &str) -> Option<String> {
    if shell.trim().is_empty() {
        return None;
    }

    // `-ilc <probe>`: interactive (`-i`) login (`-l`) shell running a single
    // command string (`-c`). The probe prints `$PATH` wrapped in unique markers
    // so we can recover it even when an interactive rc writes a banner to stdout
    // first (see `PATH_MARK_BEGIN`). `-c` makes the shell run the probe and exit,
    // so `-i` does not leave it waiting on (null) stdin.
    let probe = format!(r#"printf '{PATH_MARK_BEGIN}%s{PATH_MARK_END}' "$PATH""#);
    let mut child = Command::new(shell)
        .arg("-ilc")
        .arg(&probe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Drain stdout on a separate thread *while* we poll for exit. An interactive
    // rc may print a banner / MOTD to stdout before our `printf` runs; if that
    // exceeds the OS pipe buffer (~64 KB on Linux) the child would block on its
    // write and `try_wait` would never complete — a deadlock that hangs until
    // CAPTURE_TIMEOUT and then falls back, defeating the capture for exactly the
    // verbose-profile users this targets. Concurrent draining keeps the pipe
    // empty; the reader returns when the child's stdout closes.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });

    // Poll for completion so we can enforce the timeout AND kill a hung shell.
    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    // On a clean exit the child's stdout is closed, so the drainer has finished
    // (or will momentarily) — join it to collect the output. On timeout/error we
    // already killed the child; drop the handle instead of joining so we never
    // block on a drainer an exotic backgrounded grandchild could keep open. On
    // the clean path the join completes before this returns, so the process is
    // single-threaded again before `apply_login_shell_path` calls `set_var`.
    match status {
        Some(status) if status.success() => {
            let out = reader.join().ok()?;
            extract_marked_path(&out)
        }
        _ => None,
    }
}

/// Pull the PATH out from between [`PATH_MARK_BEGIN`] and [`PATH_MARK_END`] and
/// validate it. Returns `None` when the markers are absent (the probe never ran
/// — e.g. an rc that hard-exits — or produced nothing), the captured value is
/// empty, or it carries a NUL / other control byte.
fn extract_marked_path(out: &str) -> Option<String> {
    let start = out.find(PATH_MARK_BEGIN)? + PATH_MARK_BEGIN.len();
    let end_rel = out[start..].find(PATH_MARK_END)?;
    let path = out[start..start + end_rel].trim();
    if path.is_empty() {
        return None;
    }
    // PRD #170 round 2 (auditor finding 1): reject a value carrying a NUL or any
    // other control byte BEFORE it can reach `std::env::set_var`. `set_var`
    // PANICS on a NUL (U+0000 is valid UTF-8 and survives `trim`), so a
    // pathological / malicious `$SHELL` that printed one would crash daemon
    // startup. A real shell printing a real PATH never contains a control byte,
    // so a clean capture is unaffected; a tainted one falls back to the inherited
    // PATH (no regression).
    if path.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(path.to_string())
}

/// PRD #170 round 2 (auditor finding 2): debug-only run-once latch. The
/// `set_var` in [`apply_login_shell_path`] is sound ONLY while the process is
/// still single-threaded — before the tokio runtime or any worker thread
/// starts. The invariant is enforced by call placement (the synchronous `main`
/// dispatch, as the first statement of the `daemon serve` arm); this latch is a
/// cheap debug-build backstop that trips an assert if a future refactor ever
/// calls this twice or relocates it after a thread/runtime has spawned, which
/// would silently reintroduce a `getenv`/`setenv` data race (UB on glibc).
#[cfg(debug_assertions)]
static LOGIN_SHELL_PATH_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Apply the captured login-shell PATH to the daemon's OWN process environment.
///
/// # Invariant (MUST hold — `set_var` soundness)
///
/// Call this EXACTLY ONCE at daemon-process startup, as early as possible and
/// BEFORE the async runtime or any worker thread starts. `set_var` mutates the
/// process-global environment; on glibc a concurrent `getenv` from another
/// thread is a data race (undefined behavior). The sole call site
/// (`src/main.rs`, the `DaemonCmd::Serve` arm) runs in the synchronous `fn main`
/// — `run_daemon_serve_cli` is `#[tokio::main]`, so it builds its runtime only
/// when *called*, after this returns — keeping this in the single-threaded
/// startup window. Do NOT add a thread/`tokio::spawn` before this call, and do
/// NOT move it later. A debug-build latch ([`LOGIN_SHELL_PATH_APPLIED`]) trips
/// if it is ever called twice.
///
/// On `Some(path)` it sets `PATH` (so every pane the daemon later spawns
/// inherits it); on `None` it leaves the inherited PATH untouched. Either
/// outcome is logged.
pub fn apply_login_shell_path() {
    // PRD #170 round 2 (auditor finding 2): debug-build run-once backstop for
    // the single-threaded-pre-runtime invariant documented above.
    #[cfg(debug_assertions)]
    debug_assert!(
        !LOGIN_SHELL_PATH_APPLIED.swap(true, std::sync::atomic::Ordering::SeqCst),
        "apply_login_shell_path must run exactly once, in the single-threaded startup window \
         before any tokio runtime / thread spawn (set_var soundness)"
    );

    match capture_login_shell_path() {
        Some(path) => {
            // SAFETY: called from the synchronous `main` dispatch at daemon
            // startup, BEFORE the tokio runtime and any worker threads exist
            // (`run_daemon_serve_cli` is `#[tokio::main]`, so it builds its
            // runtime only when *called*, after this returns). The process is
            // single-threaded here, so no concurrent `getenv` can race this
            // `setenv` — the PRD's stated soundness condition for `set_var`. The
            // captured value is rejected if it carries a NUL/control byte (see
            // `capture_path_via_shell`), so `set_var` cannot panic here.
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

    /// Wrap `path` in the probe markers exactly as the real `printf` probe does,
    /// so [`extract_marked_path`] sees the same framing it must parse in
    /// production.
    fn marked(path: &str) -> String {
        format!("{PATH_MARK_BEGIN}{path}{PATH_MARK_END}")
    }

    /// Write `body` to a fresh executable script under `dir` and return its path.
    fn write_shell(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write fake shell");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake shell");
        path
    }

    /// A fake `$SHELL` that exports `path_value` as PATH, drops the leading flag
    /// bundle (`-ilc`, `-lc`, …), then runs the requested `-c` command (our
    /// probe) so the real probe string + marker framing are exercised end to end.
    /// Exits with `exit_code` so the status-rejection branch is testable.
    fn write_probe_shell(
        dir: &std::path::Path,
        name: &str,
        path_value: &str,
        exit_code: i32,
    ) -> std::path::PathBuf {
        let body = format!(
            "#!/bin/sh\n\
             export PATH=\"{path_value}\"\n\
             while [ \"$#\" -gt 0 ]; do case \"$1\" in -*) shift ;; *) break ;; esac; done\n\
             if [ \"$#\" -gt 0 ]; then /bin/sh -c \"$1\"; fi\n\
             exit {exit_code}\n"
        );
        write_shell(dir, name, &body)
    }

    #[test]
    fn extract_marked_path_returns_inner_value() {
        assert_eq!(
            extract_marked_path(&marked("/opt/login/bin:/usr/bin")),
            Some("/opt/login/bin:/usr/bin".to_string()),
        );
    }

    #[test]
    fn extract_marked_path_ignores_surrounding_noise() {
        // An interactive rc printed a banner before, and a newline after, the
        // markers — the inner PATH must still come out clean. This is why we
        // delimit instead of reading raw stdout.
        let out = format!("Welcome to the machine!\n{}\n", marked("/opt/login/bin"));
        assert_eq!(
            extract_marked_path(&out),
            Some("/opt/login/bin".to_string())
        );
    }

    #[test]
    fn extract_marked_path_none_when_markers_absent() {
        // No probe output at all (rc hard-exited before printf) → no markers.
        assert_eq!(extract_marked_path("just a banner, no markers"), None);
        // Begin marker but no end marker → still None.
        assert_eq!(
            extract_marked_path(&format!("{PATH_MARK_BEGIN}/opt/bin")),
            None
        );
    }

    #[test]
    fn extract_marked_path_none_when_inner_empty() {
        assert_eq!(extract_marked_path(&marked("")), None);
        assert_eq!(extract_marked_path(&marked("   ")), None);
    }

    #[test]
    fn extract_marked_path_none_on_control_byte() {
        // PRD #170 round 2 (auditor finding 1): a NUL inside the markers would
        // make `set_var` panic, and any other control byte is equally bogus — a
        // real PATH never contains one, so both are rejected and the daemon keeps
        // its inherited PATH instead of crashing.
        assert_eq!(extract_marked_path(&marked("/opt/bin\u{0}/usr/bin")), None);
        assert_eq!(extract_marked_path(&marked("/opt/bin\u{1b}/usr/bin")), None);
    }

    #[test]
    fn captures_path_from_interactive_login_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = write_probe_shell(dir.path(), "ok-shell.sh", "/opt/login/bin:/usr/bin", 0);
        assert_eq!(
            capture_path_via_shell(&shell.to_string_lossy()),
            Some("/opt/login/bin:/usr/bin".to_string()),
        );
    }

    #[test]
    fn captures_path_despite_large_stdout_banner() {
        // Regression guard: an interactive rc that prints a banner bigger than
        // the OS pipe buffer (~64 KB) to stdout BEFORE our probe must not
        // deadlock the capture. stdout is drained on a thread concurrently with
        // the poll loop, so the child never blocks on its write and the marked
        // PATH (printed last) still parses out. Without concurrent draining this
        // would hang until CAPTURE_TIMEOUT and then return None.
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = write_shell(
            dir.path(),
            "verbose-shell.sh",
            "#!/bin/sh\n\
             export PATH=\"/opt/login/bin:/usr/bin\"\n\
             i=0\n\
             while [ \"$i\" -lt 2000 ]; do \
             printf 'banner line %s padding padding padding padding padding\\n' \"$i\"; \
             i=$((i + 1)); done\n\
             while [ \"$#\" -gt 0 ]; do case \"$1\" in -*) shift ;; *) break ;; esac; done\n\
             if [ \"$#\" -gt 0 ]; then /bin/sh -c \"$1\"; fi\n",
        );
        assert_eq!(
            capture_path_via_shell(&shell.to_string_lossy()),
            Some("/opt/login/bin:/usr/bin".to_string()),
        );
    }

    #[test]
    fn capture_requests_an_interactive_shell() {
        // The crux of the fix: the capture MUST invoke an *interactive* shell
        // (`-i`), or dirs added behind `~/.bashrc`'s `case $- in *i*` guard
        // (where installers like opencode put their PATH line) are missed. This
        // fake `$SHELL` exports the extra dir ONLY when it sees an `-i`-bearing
        // flag, so a login-only capture would yield the bare `/usr/bin` and this
        // assertion would fail.
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = write_shell(
            dir.path(),
            "interactive-aware-shell.sh",
            "#!/bin/sh\n\
             interactive=0\n\
             for a in \"$@\"; do case \"$a\" in -*i*) interactive=1 ;; esac; done\n\
             if [ \"$interactive\" = 1 ]; then export PATH=\"/opt/interactive/bin:/usr/bin\"; \
             else export PATH=\"/usr/bin\"; fi\n\
             while [ \"$#\" -gt 0 ]; do case \"$1\" in -*) shift ;; *) break ;; esac; done\n\
             if [ \"$#\" -gt 0 ]; then /bin/sh -c \"$1\"; fi\n",
        );
        assert_eq!(
            capture_path_via_shell(&shell.to_string_lossy()),
            Some("/opt/interactive/bin:/usr/bin".to_string()),
            "capture must run the shell interactively (`-i`) so ~/.bashrc-added dirs are seen"
        );
    }

    #[test]
    fn none_on_non_zero_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Prints a valid marked PATH but exits non-zero — must be rejected.
        let shell = write_probe_shell(dir.path(), "fail-shell.sh", "/opt/login/bin", 1);
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
