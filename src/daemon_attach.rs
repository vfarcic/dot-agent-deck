//! Lazy-spawn machinery for the on-remote daemon (PRD #76, M4.3 + M2.8).
//!
//! After the 2026-05-09 architectural pivot the laptop-side bridge that
//! ssh-exec'd `dot-agent-deck daemon attach` on the remote was removed
//! (M2.7). What survives in this module is the pieces that lazy-spawn the
//! daemon on the remote when something else needs it (the on-remote TUI
//! bootstrap from M2.8, or M2.9's `ssh -t` connect):
//!
//! - [`ensure_daemon_running`] — `flock(2)`-serialized "is the attach socket
//!   present? if not, run `spawn_fn` and poll for it" loop. Concurrent
//!   first-attaches serialize on `<state_dir>/spawn.lock` so only one races
//!   the bind. The loser re-checks after acquiring the lock and short-circuits.
//! - endpoint-trust verification — checks the path is a Unix socket owned by
//!   the current uid at mode 0o600 before any caller connects, defending
//!   against a same-uid attacker who pre-binds at the attach path. The check
//!   itself lives in [`crate::platform::fsperm::verify_endpoint_trusted`]
//!   (PRD #42 M1); this module maps its failure to [`AttachError::SocketUntrusted`].
//! - [`spawn_daemon_serve_detached`] — detach-spawn of `dot-agent-deck daemon
//!   serve` so the daemon survives the parent's exit. The platform-specific
//!   spawn (`setsid` + `O_NOFOLLOW` + 0o600 on Unix) lives in
//!   [`crate::platform::detach`].
//! - [`ensure_external_daemon_or_die`] — M2.8 entry point used by the TUI
//!   when `DOT_AGENT_DECK_VIA_DAEMON=1`: glues the three primitives above
//!   together against the production `state_dir()` and the production
//!   `dot-agent-deck daemon serve` binary.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::agent_pty::DOT_AGENT_DECK_VIA_DAEMON;
use crate::config::state_dir;

/// Errors surfaced by the lazy-spawn machinery. The CLI handler renders
/// these to stderr before exiting nonzero; tests match on the variant.
#[derive(Debug, Error)]
pub enum AttachError {
    #[error("failed to spawn detached daemon: {source}")]
    DaemonSpawnFailed {
        #[source]
        source: std::io::Error,
    },
    #[error(
        "daemon failed to start within {timeout_ms}ms: socket {path} never appeared. Check {log_path} for daemon stderr."
    )]
    DaemonStartTimeout {
        path: PathBuf,
        log_path: PathBuf,
        timeout_ms: u128,
    },
    #[error(
        "refusing to connect to daemon attach socket {path}: {reason}. \
         Another user (or a hostile same-uid process) may have placed this file."
    )]
    SocketUntrusted { path: PathBuf, reason: String },
}

/// If `socket_path` doesn't exist, run `spawn_fn` to start a detached
/// daemon, then poll for the socket file at `poll_interval` until either
/// it appears (Ok) or `poll_timeout` elapses (`DaemonStartTimeout`).
///
/// Concurrent callers serialize on an exclusive `flock(2)` over
/// `<state_dir>/spawn.lock` so only one of them runs `spawn_fn`; losers see
/// the socket present after acquiring the lock and short-circuit. This is
/// the only correct way to dedupe — a sleep-and-poll without the lock just
/// shrinks the race.
///
/// Pure of process-spawning details so tests can drive the loop by passing
/// a closure that synchronously creates the socket file (or doesn't). The
/// real production callsite uses [`spawn_daemon_serve_detached`].
///
/// Both the pre-existing-socket and freshly-spawned-socket branches run
/// [`verify_socket_trusted`] before returning so the caller knows the
/// socket is owned by the current uid at mode 0o600.
pub async fn ensure_daemon_running<F>(
    socket_path: &Path,
    state_dir: &Path,
    spawn_fn: F,
    poll_interval: Duration,
    poll_timeout: Duration,
) -> Result<(), AttachError>
where
    F: FnOnce() -> std::io::Result<()>,
{
    // Lock file lives inside the state dir, so we have to make sure the dir
    // exists first. `ensure_owner_only_dir` creates idempotently AND enforces
    // mode 0o700 unconditionally — including repairing a pre-existing dir
    // that was left at looser permissions by a stale install.
    crate::platform::fsperm::ensure_owner_only_dir(state_dir)
        .map_err(|source| AttachError::DaemonSpawnFailed { source })?;

    // Acquire the spawn mutex. flock(2) blocks until granted, so this also
    // serves as the "wait for the in-flight spawn to finish" barrier.
    let lock_path = state_dir.join("spawn.lock");
    let _lock = crate::platform::lock::acquire_spawn_lock(&lock_path)
        .await
        .map_err(|source| AttachError::DaemonSpawnFailed { source })?;

    // First check happens INSIDE the lock so a waiter that lost the race sees
    // the socket the winner created and skips the spawn.
    if socket_path.exists() {
        verify_socket_trusted(socket_path)?;
        // Trust check only validates the inode (type, owner, mode) — it
        // doesn't know whether anyone is listening. A daemon that died
        // without unlinking (crash, SIGKILL, host reboot mid-write) leaves
        // a stale socket on disk that would otherwise short-circuit lazy-
        // spawn forever, with every subsequent client connect failing
        // `ECONNREFUSED`. Probe-connect here so we can distinguish a live
        // daemon from a leftover file; on failure, the inode is ours
        // (trust check just validated uid + mode) so unlinking and
        // falling through to the spawn branch is safe.
        if probe_socket_alive(socket_path).await {
            return Ok(());
        }
        let _ = std::fs::remove_file(socket_path);
    }

    spawn_fn().map_err(|source| AttachError::DaemonSpawnFailed { source })?;

    let log_path = state_dir.join("daemon.log");
    let start = Instant::now();
    loop {
        if socket_path.exists() {
            verify_socket_trusted(socket_path)?;
            return Ok(());
        }
        if start.elapsed() >= poll_timeout {
            return Err(AttachError::DaemonStartTimeout {
                path: socket_path.to_path_buf(),
                log_path,
                timeout_ms: poll_timeout.as_millis(),
            });
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Verify `path` is a trusted endpoint (a Unix socket owned by the current uid
/// at mode 0o600) before any caller connects.
///
/// Defends against a same-uid attacker pre-creating a socket at the attach
/// path before the real daemon binds: in that scenario `bind(2)` fails with
/// `EADDRINUSE` for the daemon and `connect(2)` succeeds for us against the
/// attacker's socket. Validating ownership and mode out-of-band closes the
/// gap. Stat is not racy here because we never re-stat after this check —
/// the FD we then connect to is anchored to the inode the kernel resolves
/// during this single call (and any swap underneath us produces an obvious
/// connection error from `UnixStream::connect`).
///
/// PRD #42 M1: the platform check lives in
/// [`crate::platform::fsperm::verify_endpoint_trusted`]; here we just map its
/// failure reason onto [`AttachError::SocketUntrusted`].
fn verify_socket_trusted(path: &Path) -> Result<(), AttachError> {
    crate::platform::fsperm::verify_endpoint_trusted(path).map_err(|reason| {
        AttachError::SocketUntrusted {
            path: path.to_path_buf(),
            reason,
        }
    })
}

/// Best-effort liveness probe for a Unix domain socket. Returns true iff
/// `connect(2)` succeeds; any error (typically `ECONNREFUSED` from a stale
/// inode whose binder is dead) returns false. The connection is dropped
/// immediately — this is only a "is anything listening" signal.
///
/// Used inside [`ensure_daemon_running`] to differentiate a live daemon
/// from a leftover socket file: file existence is not sufficient evidence
/// that the daemon is up, since `bind(2)` doesn't auto-unlink on the
/// binder's death.
///
/// PRD #42 M2: the transport is abstracted behind
/// [`crate::platform::ipc::IpcStream`]; on Unix this is the same
/// `UnixStream::connect` liveness probe, unchanged.
async fn probe_socket_alive(path: &Path) -> bool {
    crate::platform::ipc::IpcStream::connect(path).await.is_ok()
}

/// Spawn `dot-agent-deck daemon serve` as a detached background process
/// that survives the parent's exit. Used by lazy-spawn-on-attach (M4.3).
///
/// - The binary is located via [`std::env::current_exe`] rather than `$PATH`
///   because non-interactive ssh shells routinely skip `~/.local/bin` (we
///   hit this exact bug three times — commits 493248b, bbf2236, ea8c748).
/// - `setsid(2)` runs in the child via `pre_exec` so the daemon becomes
///   its own session leader and won't receive `SIGHUP` when the parent
///   shell exits.
/// - stdin is `/dev/null` and stdout/stderr append to `<state_dir>/daemon.log`.
///   The log is opened with `O_NOFOLLOW` and mode 0o600 so a same-uid
///   attacker can't pre-place a symlink to redirect daemon output (which
///   contains hook payloads and agent task strings) and the log file
///   itself isn't world-readable on the default umask.
///
/// Note: we do not wait for the child here — the spawned daemon stays up
/// after this function returns. Callers should poll the attach socket
/// (see [`ensure_daemon_running`]) to know when the daemon is ready.
pub fn spawn_daemon_serve_detached(state_dir: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    spawn_daemon_serve_detached_with_exe(state_dir, &exe).map(|_| ())
}

/// Same as [`spawn_daemon_serve_detached`] but takes an explicit `exe`
/// path and returns the spawned daemon's pid on success. Production callers
/// should always go through [`spawn_daemon_serve_detached`] (which discards
/// the pid); this variant exists so integration tests in `tests/` can
/// fork-exec the cargo-built `dot-agent-deck` binary (the test runner's
/// `current_exe()` is the test harness, not our binary) and recover the
/// pid for `kill(2)`-based cleanup.
///
/// PRD #42 M1: the platform-specific detach (Unix `setsid` + `O_NOFOLLOW` +
/// 0o600 log + `/dev/null`; Windows `DETACHED_PROCESS` + `NUL`) lives in
/// [`crate::platform::detach`]. This is a thin, signature-stable delegator.
pub fn spawn_daemon_serve_detached_with_exe(state_dir: &Path, exe: &Path) -> std::io::Result<u32> {
    crate::platform::detach::spawn_daemon_serve_detached_with_exe(state_dir, exe)
}

/// Returns true if `DOT_AGENT_DECK_VIA_DAEMON` is set to a truthy value
/// (`1`, `true`, `yes`, case-insensitive). Pre-PRD-93 this controlled the
/// in-process-vs-external decision directly; now (PRD #93 M1.1) the default
/// is "external," and this helper exists only to recognize the historical
/// opt-in spelling. Kept because `connect.rs` still injects
/// `DOT_AGENT_DECK_VIA_DAEMON=1` over `ssh -t` for the remote bootstrap (a
/// no-op against the new default but harmless), and a few tests still drive
/// the parser directly.
pub fn via_daemon_enabled() -> bool {
    std::env::var(DOT_AGENT_DECK_VIA_DAEMON)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// M2.8 TUI bootstrap entry point. When the dashboard is invoked with
/// `DOT_AGENT_DECK_VIA_DAEMON=1`, the TUI calls this before the first
/// [`crate::daemon_client::DaemonClient`] connect:
///
/// - If the attach socket is missing, fork-execs `dot-agent-deck daemon
///   serve` detached so it outlives the TUI (agents must survive TUI exit
///   per PRD #76 line 199), under [`flock`]-serialized
///   `<state_dir>/spawn.lock` so concurrent first-attaches don't double-spawn.
/// - Either way, runs [`verify_socket_trusted`] on the resulting socket
///   before returning Ok. A pre-existing regular file or wrong-mode socket
///   at the path is rejected with [`AttachError::SocketUntrusted`] — the
///   TUI must not silently fall back to the in-process daemon, since that
///   would mask a same-uid attacker.
///
/// Errors are surfaced as-is to the caller (the dashboard renders them to
/// stderr and exits nonzero — there is no in-process fallback). Polling
/// timeout is 5s with 50ms intervals; that's enough headroom for the
/// daemon's bind path on a loaded host without making error output feel
/// hung.
pub async fn ensure_external_daemon_or_die(attach_path: &Path) -> Result<(), AttachError> {
    let state = state_dir();
    let state_for_spawn = state.clone();
    ensure_daemon_running(
        attach_path,
        &state,
        move || spawn_daemon_serve_detached(&state_for_spawn),
        Duration::from_millis(50),
        Duration::from_secs(5),
    )
    .await
}
