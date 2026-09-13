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
use crate::daemon_client::LocalEndpoint;

/// How long [`ensure_external_daemon_or_die`] polls for the freshly-spawned
/// daemon's endpoint before giving up.
///
/// **Derived from [`crate::login_shell::CAPTURE_TIMEOUT`] rather than picked as
/// a round number, and that derivation is the point.** The daemon we spawn does
/// real work *before* it binds: `main`'s `DaemonCmd::Serve` arm runs
/// `apply_login_shell_path` (an interactive-login `$SHELL`, up to
/// `CAPTURE_TIMEOUT`), then materializes the pi extension and the Codex hooks —
/// all ahead of `IpcListener::bind`. So the launcher's budget must cover the
/// daemon's worst-case *pre-bind* budget, or it is guaranteed to time out while
/// the daemon is still healthy.
///
/// This previously read `Duration::from_secs(5)` against a `CAPTURE_TIMEOUT` of
/// 10s — half the time the daemon was explicitly permitted to take before
/// binding. Any machine whose `$SHELL -ilc` exceeded 5s (a `compinit` + plugin
/// manager + generated-completions zsh measures ~6s) failed *every* first
/// attach: the launcher errored at 5s, the daemon bound at ~6s and stayed up,
/// and the retry then succeeded — so the bug presented as flaky startup rather
/// than a fixed misconfiguration.
///
/// The slack on top absorbs the post-capture pre-bind work and a loaded host.
/// The cost of a longer bound is only paid when a daemon is genuinely absent or
/// dead — the healthy path returns as soon as the endpoint appears, typically in
/// milliseconds.
pub const DAEMON_START_POLL_TIMEOUT: Duration =
    crate::login_shell::CAPTURE_TIMEOUT.saturating_add(Duration::from_secs(5));

/// How often [`ensure_external_daemon_or_die`] re-checks for the endpoint.
const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Errors surfaced by the lazy-spawn machinery. The CLI handler renders
/// these to stderr before exiting nonzero; tests match on the variant.
#[derive(Debug, Error)]
pub enum AttachError {
    #[error("failed to spawn detached daemon: {source}")]
    DaemonSpawnFailed {
        #[source]
        source: std::io::Error,
    },
    // "never became available" rather than "never appeared": on Windows the
    // endpoint is a named pipe with no filesystem presence, so what timed out is
    // the connect-probe, not a file materializing (PRD #163 M4).
    // The log pointer has to disclaim itself: `daemon.log` only receives
    // anything when the daemon was started with `DOT_AGENT_DECK_LOG` set, so on
    // a default install this path is a 0-byte file. The bare "check the log"
    // wording sent you looking at an empty file and implied the daemon never
    // started, when the actual failure mode is a daemon that started fine and
    // bound *after* the deadline.
    #[error(
        "daemon failed to start within {timeout_ms}ms: endpoint {path} never became available. \
         For daemon stderr see {log_path} — but note it stays empty unless the daemon was started \
         with DOT_AGENT_DECK_LOG set, so an empty log is not evidence the daemon never ran."
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
///
/// PRD #163 M4: on a platform whose endpoint is not a filesystem path (Windows
/// named pipes), the presence test is a connect-probe instead of `exists()`, and
/// trust verification happens inside that connect. See the body for why an
/// unconditional `exists()` would make Windows lazy-spawn always time out.
pub async fn ensure_daemon_running<F>(
    endpoint: &LocalEndpoint,
    state_dir: &Path,
    spawn_fn: F,
    poll_interval: Duration,
    poll_timeout: Duration,
) -> Result<(), AttachError>
where
    F: FnOnce() -> std::io::Result<()>,
{
    // PRD #741 M2: a `&LocalEndpoint`, not a `&Path`. Two of the things this
    // function does are meaningful only against a daemon on this machine — it
    // `remove_file`s the inode at the address when the probe says nothing is
    // listening, and it starts a daemon process HERE. Neither is something to do
    // on behalf of a deck that lives somewhere else, and the unlink in
    // particular is safe only because `verify_socket_trusted` just proved the
    // inode is ours. `Endpoint::as_local()` is the only route to this type.
    let socket_path = endpoint.path();
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
    //
    // PRD #163 M4: "is the endpoint there yet?" is platform-shaped, and getting
    // it wrong breaks lazy-spawn outright on Windows. A `\\.\pipe\` name has no
    // filesystem presence, so `exists()` is permanently false there — every
    // attach would spawn a second daemon and then time out waiting for a socket
    // file that can never appear. Where the endpoint is not a path, the
    // connect-probe *is* the presence test, and it doubles as the trust check
    // (`IpcStream::connect` verifies the pipe server's owner SID before handing
    // back a stream), so there is nothing to verify out of band and no inode to
    // unlink. The Unix branch below is byte-for-byte what it always was.
    //
    // PRD #741 M3: the boolean became three-valued. This function is
    // `LocalEndpoint`-only by type since M2 — it unlinks an inode and starts a
    // process HERE — so the presence it asks is the local one, and the third
    // answer (a deck on another machine) is unreachable rather than handled.
    if crate::platform::ipc::LOCAL_ENDPOINT_PRESENCE.is_daemon_owned_inode() {
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
    } else if probe_socket_alive(socket_path).await {
        return Ok(());
    }

    spawn_fn().map_err(|source| AttachError::DaemonSpawnFailed { source })?;

    let log_path = state_dir.join("daemon.log");
    let start = Instant::now();
    loop {
        if crate::platform::ipc::LOCAL_ENDPOINT_PRESENCE.is_daemon_owned_inode() {
            if socket_path.exists() {
                verify_socket_trusted(socket_path)?;
                return Ok(());
            }
        } else if probe_socket_alive(socket_path).await {
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
/// stderr and exits nonzero — there is no in-process fallback). Polling uses
/// [`DAEMON_START_POLL_TIMEOUT`] at [`DAEMON_START_POLL_INTERVAL`]; see the
/// former's docs for why the budget is derived from the daemon's own pre-bind
/// work rather than hardcoded.
pub async fn ensure_external_daemon_or_die(endpoint: &LocalEndpoint) -> Result<(), AttachError> {
    let state = state_dir();
    let state_for_spawn = state.clone();
    ensure_daemon_running(
        endpoint,
        &state,
        move || spawn_daemon_serve_detached(&state_for_spawn),
        DAEMON_START_POLL_INTERVAL,
        DAEMON_START_POLL_TIMEOUT,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher's poll budget must strictly exceed the daemon's own
    /// worst-case *pre-bind* budget, because the daemon runs the login-shell
    /// PATH capture before it binds the endpoint the launcher is polling for.
    ///
    /// This is the regression guard for the flaky-startup bug: the two
    /// constants live in different modules, both look locally reasonable, and
    /// nothing but this assertion couples them. A timing test would be flaky
    /// and would only fail on a machine with a slow interactive shell — the
    /// ordering is the real invariant, so assert the ordering.
    #[test]
    fn attach_poll_timeout_exceeds_daemon_pre_bind_budget() {
        assert!(
            DAEMON_START_POLL_TIMEOUT > crate::login_shell::CAPTURE_TIMEOUT,
            "lazy-spawn would time out on a healthy but slow daemon: launcher waits \
             {DAEMON_START_POLL_TIMEOUT:?} but the daemon may spend up to {:?} in the \
             login-shell PATH capture before it even calls bind",
            crate::login_shell::CAPTURE_TIMEOUT,
        );
    }

    /// The endpoint poll must actually get to poll. An interval at or above the
    /// total budget collapses the loop into a single check, which would
    /// reintroduce the failure for any daemon that is not already bound.
    #[test]
    fn attach_poll_interval_allows_repeated_checks() {
        assert!(
            DAEMON_START_POLL_INTERVAL * 10 < DAEMON_START_POLL_TIMEOUT,
            "poll interval {DAEMON_START_POLL_INTERVAL:?} is too coarse for a \
             {DAEMON_START_POLL_TIMEOUT:?} budget",
        );
    }
}
