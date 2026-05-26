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
//! - [`verify_socket_trusted`] — checks the path is a Unix socket owned by
//!   the current uid at mode 0o600 before any caller connects, defending
//!   against a same-uid attacker who pre-binds at the attach path.
//! - [`spawn_daemon_serve_detached`] — `setsid(2)` + `O_NOFOLLOW` + 0o600
//!   detach-spawn of `dot-agent-deck daemon serve` so the daemon survives
//!   the parent's exit.
//! - [`ensure_external_daemon_or_die`] — M2.8 entry point used by the TUI
//!   when `DOT_AGENT_DECK_VIA_DAEMON=1`: glues the three primitives above
//!   together against the production `state_dir()` and the production
//!   `dot-agent-deck daemon serve` binary.

use std::os::unix::io::AsRawFd;
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
    // exists first. `prepare_state_dir` creates idempotently AND enforces
    // mode 0o700 unconditionally — including repairing a pre-existing dir
    // that was left at looser permissions by a stale install.
    prepare_state_dir(state_dir).map_err(|source| AttachError::DaemonSpawnFailed { source })?;

    // Acquire the spawn mutex. flock(2) blocks until granted, so this also
    // serves as the "wait for the in-flight spawn to finish" barrier.
    let lock_path = state_dir.join("spawn.lock");
    let _lock = acquire_spawn_lock(&lock_path)
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

/// Verify `path` is a Unix socket owned by the current uid at mode 0o600.
///
/// Defends against a same-uid attacker pre-creating a socket at the attach
/// path before the real daemon binds: in that scenario `bind(2)` fails with
/// `EADDRINUSE` for the daemon and `connect(2)` succeeds for us against the
/// attacker's socket. Validating ownership and mode out-of-band closes the
/// gap. Stat is not racy here because we never re-stat after this check —
/// the FD we then connect to is anchored to the inode the kernel resolves
/// during this single call (and any swap underneath us produces an obvious
/// connection error from `UnixStream::connect`).
pub(crate) fn verify_socket_trusted(path: &Path) -> Result<(), AttachError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = std::fs::metadata(path).map_err(|source| AttachError::SocketUntrusted {
        path: path.to_path_buf(),
        reason: format!("stat failed: {source}"),
    })?;

    if !metadata.file_type().is_socket() {
        return Err(AttachError::SocketUntrusted {
            path: path.to_path_buf(),
            reason: "not a Unix domain socket".to_string(),
        });
    }

    // SAFETY: getuid(2) is async-signal-safe, has no failure mode, and
    // returns the calling process's real uid.
    let our_uid = unsafe { libc::getuid() };
    if metadata.uid() != our_uid {
        return Err(AttachError::SocketUntrusted {
            path: path.to_path_buf(),
            reason: format!("owned by uid {} (expected {})", metadata.uid(), our_uid),
        });
    }

    let mode = metadata.mode() & 0o777;
    if mode != 0o600 {
        return Err(AttachError::SocketUntrusted {
            path: path.to_path_buf(),
            reason: format!("mode is 0o{mode:o} (expected 0o600)"),
        });
    }

    Ok(())
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
async fn probe_socket_alive(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// RAII guard for the `spawn.lock` flock. Drop releases the lock by
/// closing the file descriptor (and explicitly LOCK_UN'ing for clarity).
pub(crate) struct SpawnLock {
    file: std::fs::File,
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        // SAFETY: fd is valid for the lifetime of self.file; flock(LOCK_UN)
        // on a held lock is safe and reverses the LOCK_EX taken in
        // acquire_spawn_lock. Closing the file (next, via File::Drop) would
        // also release the lock — the explicit unlock just keeps the
        // semantics readable.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Open or create `path` and acquire an exclusive `flock(2)` on it. flock is
/// blocking, so we run the syscall on `spawn_blocking` to avoid stalling
/// other tasks scheduled on the same tokio worker when contention is real
/// (i.e., another caller on this host is mid-spawn).
///
/// `pub(crate)` so the daemon's `run_daemon_with` can reuse the same
/// primitive to serialize its own probe-remove-bind sequence against
/// concurrent `daemon serve` starts (PRD #93 auditor BLOCKER — two
/// daemons probing a stale socket would otherwise both `remove_file` and
/// both `bind`, clobbering each other's clients).
pub(crate) async fn acquire_spawn_lock(path: &Path) -> std::io::Result<SpawnLock> {
    use std::os::unix::fs::OpenOptionsExt;

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        // SAFETY: passing a valid fd and a valid op constant; flock(2) does
        // not retain any reference to the address space, so the unsafe is a
        // formality of the libc binding.
        let res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if res != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(SpawnLock { file })
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Create `state_dir` with mode 0o700, repairing the mode on pre-existing
/// directories. `DirBuilder::mode(0o700)` only applies to a directory
/// freshly created by the call — an existing `state_dir` at 0o755 (stale
/// install, prior misconfigured run) would slip through and leave the
/// per-uid socket dir world-readable. We follow up with an unconditional
/// `set_permissions(0o700)`, matching the chmod-after-bind pattern used
/// for the daemon socket in `src/daemon.rs`.
///
/// Race-safety: `prepare_state_dir` runs before `spawn.lock` is acquired,
/// so two first-time callers (twin TUI launches, or TUI + `daemon serve`)
/// can both observe "missing" and try to create. `DirBuilder::recursive(true)`
/// makes the mkdir idempotent — the stdlib internally converts
/// `AlreadyExists` to `Ok(())` when the existing path is a directory — so
/// the loser's create call succeeds against whatever the winner left behind.
/// Real I/O errors (permission denied, ENOSPC) still surface unchanged.
fn prepare_state_dir(state_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(state_dir)?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
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
pub fn spawn_daemon_serve_detached_with_exe(state_dir: &Path, exe: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;

    prepare_state_dir(state_dir)?;
    let log_path = state_dir.join("daemon.log");
    let stdout = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            // O_NOFOLLOW + open of a symlink fails with ELOOP. A symlink at
            // the daemon log path means someone placed it there ahead of us;
            // refuse to write through it rather than silently following.
            return Err(std::io::Error::other(format!(
                "daemon log path {} is a symlink — refusing to follow (someone may have planted it to redirect daemon output)",
                log_path.display()
            )));
        }
        Err(e) => return Err(e),
    };
    let stderr = stdout.try_clone()?;
    let stdin = std::fs::File::open("/dev/null")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("serve")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);

    // SAFETY: `pre_exec` runs in the child between fork and exec. Only
    // async-signal-safe libc calls are permitted here; `setsid(2)` is on
    // POSIX's async-signal-safe list. We do nothing else in the closure.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Spawn and immediately drop the handle. The child is now its own
    // session leader; when this process exits, init reaps the child. We
    // don't wait — the caller will poll the attach socket. The pid is
    // returned for tests that need to clean up the spawned daemon; the
    // production wrapper [`spawn_daemon_serve_detached`] discards it.
    let child = cmd.spawn()?;
    Ok(child.id())
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

// ---------------------------------------------------------------------------
// PRD #103 M1.5 — peer-credential PID discovery on a connected attach socket.
// ---------------------------------------------------------------------------

/// Return the PID of the process holding the other end of a connected
/// Unix-domain stream.
///
/// Uses `libc::getsockopt` directly (`SO_PEERCRED` on Linux,
/// `LOCAL_PEERPID` on macOS). The PRD considered
/// `std::os::unix::net::UnixStream::peer_cred()` and rejected it — on
/// rustc 1.94 stable that API is still nightly-only behind the
/// `peer_credentials_unix_socket` feature, so depending on it would not
/// compile.
///
/// **Crucially: this helper exchanges zero protocol bytes with the
/// peer.** That's the load-bearing property: the entire point of having
/// it is to drive `dot-agent-deck daemon stop` against a *stale* daemon
/// (PRD #103 Phase 3), and a stale daemon by definition does not
/// implement any new protocol surface we add. PID discovery via
/// `getsockopt` is an OS-level facility and works against any daemon
/// version, including the v0.24.x daemon that motivated the PRD.
///
/// Generic over `AsRawFd` so the same helper covers both
/// `std::os::unix::net::UnixStream` and `tokio::net::UnixStream` — the
/// `getsockopt` syscall doesn't care which runtime owns the fd.
#[cfg(target_os = "linux")]
pub fn peer_pid<S: AsRawFd>(stream: &S) -> std::io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a freshly-zeroed `libc::ucred` allocated on the
    // stack and outlives the syscall; `len` tracks its size by value.
    // `getsockopt` writes at most `len` bytes into the pointee, which is
    // exactly the layout libc guarantees for `ucred`. The fd comes from
    // `AsRawFd` so it's owned by the caller for the duration of this
    // call. No unwinding can leak resources because there are no Drop
    // types involved.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.pid as u32)
}

/// macOS variant — uses `LOCAL_PEERPID` (not `LOCAL_PEERCRED`, which
/// returns a `struct xucred` without a PID). `nix` does not yet ship a
/// typed wrapper for `LOCAL_PEERPID`, so a small `libc::getsockopt` call
/// is fine; the unsafe surface is one syscall with a stack-allocated
/// output.
#[cfg(target_os = "macos")]
pub fn peer_pid<S: AsRawFd>(stream: &S) -> std::io::Result<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid` is a stack-allocated `pid_t` that outlives the call;
    // `len` matches its size by value. `getsockopt(LOCAL_PEERPID)`
    // writes at most `len` bytes into the pointee, which is exactly
    // `sizeof(pid_t)`. The fd is owned by the caller for the duration
    // of this call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pid as u32)
}
