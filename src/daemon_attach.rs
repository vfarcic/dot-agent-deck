//! Lazy-spawn machinery for the on-remote daemon (PRD #76, M4.3).
//!
//! After the 2026-05-09 architectural pivot the laptop-side bridge that
//! ssh-exec'd `dot-agent-deck daemon attach` on the remote was removed
//! (M2.7). What survives in this module is the pieces that lazy-spawn the
//! daemon on the remote when something else needs it (the on-remote TUI, a
//! future M2.8 caller, or M2.9's `ssh -t` connect):
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

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

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
    // exists first. Mode 0o700 on freshly-created dirs only — we deliberately
    // leave pre-existing dirs alone (the user may have looser perms there for
    // other tooling).
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
        return Ok(());
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
async fn acquire_spawn_lock(path: &Path) -> std::io::Result<SpawnLock> {
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

/// Create `state_dir` with mode 0o700 if missing. Pre-existing dirs are
/// left untouched (the user may have looser perms there from other tooling
/// — chmod'ing them down without consent could break things). The 0o700 on
/// fresh creation matches the auditor's "freshly-created dirs should be
/// 0o700" requirement so per-uid state isn't world-readable on a clean
/// remote.
fn prepare_state_dir(state_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if state_dir.exists() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(state_dir)
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

    let exe = std::env::current_exe()?;
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
    // don't wait — the caller will poll the attach socket.
    let _child = cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `tempfile::tempdir()` calls `mkdir(2)` with mode `0o700 & ~umask`. The
    /// crate's `bind_socket` (src/daemon.rs) briefly flips the process-global
    /// umask to `0o177`, and any concurrent `mkdir` (in another test) lands
    /// during that window with mode `0o700 & ~0o177 = 0o600` — no execute
    /// bit, so files inside the dir become unreachable. Re-apply 0o700 after
    /// creation so our tests are robust to the race.
    fn race_safe_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    /// Bind a real Unix listener at `path` with mode 0o600 so it passes the
    /// trust check. Returns the listener; the caller keeps it alive for the
    /// duration of the test (drop unbinds nothing — the inode persists, so
    /// only the test's tempdir cleanup removes it).
    fn bind_trusted_socket(path: &Path) -> std::os::unix::net::UnixListener {
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener
    }

    #[tokio::test]
    async fn ensure_returns_immediately_when_socket_present() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");
        let _listener = bind_trusted_socket(&sock);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        ensure_daemon_running(
            &sock,
            dir.path(),
            move || {
                calls_inner.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect("should short-circuit when socket already exists");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "spawn_fn must not run when socket is already present"
        );
    }

    #[tokio::test]
    async fn ensure_spawns_then_succeeds_when_socket_appears() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");
        let sock_for_spawn = sock.clone();
        // Hold the listener handle outside so it lives past the closure.
        let listener_slot: Arc<std::sync::Mutex<Option<std::os::unix::net::UnixListener>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot = listener_slot.clone();

        ensure_daemon_running(
            &sock,
            dir.path(),
            move || {
                // Simulate a daemon that binds the socket synchronously.
                let l = bind_trusted_socket(&sock_for_spawn);
                *slot.lock().unwrap() = Some(l);
                Ok(())
            },
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect("should observe the spawned daemon's socket");
    }

    #[tokio::test]
    async fn ensure_polls_until_socket_appears() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");
        let sock_async = sock.clone();
        let listener_slot: Arc<std::sync::Mutex<Option<std::os::unix::net::UnixListener>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot = listener_slot.clone();

        // Background task creates the socket after a short delay, so
        // ensure_daemon_running must actually iterate the poll loop.
        let creator = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let l = bind_trusted_socket(&sock_async);
            *slot.lock().unwrap() = Some(l);
        });

        ensure_daemon_running(
            &sock,
            dir.path(),
            || Ok(()), // pretend the spawn succeeded; the task above mimics binding.
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect("should succeed once the background task creates the socket");
        creator.await.unwrap();
    }

    #[tokio::test]
    async fn ensure_times_out_when_socket_never_appears() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");

        let err = ensure_daemon_running(
            &sock,
            dir.path(),
            || Ok(()),
            Duration::from_millis(10),
            Duration::from_millis(50),
        )
        .await
        .expect_err("should time out");
        match err {
            AttachError::DaemonStartTimeout {
                path,
                log_path,
                timeout_ms,
            } => {
                assert_eq!(path, sock);
                assert_eq!(log_path, dir.path().join("daemon.log"));
                assert_eq!(timeout_ms, 50);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ensure_propagates_spawn_failure() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");

        let err = ensure_daemon_running(
            &sock,
            dir.path(),
            || Err(std::io::Error::other("boom")),
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect_err("spawn failure should bubble up");
        match err {
            AttachError::DaemonSpawnFailed { source } => {
                assert_eq!(source.to_string(), "boom");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn trust_check_rejects_regular_file() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");
        // A regular file at the socket path — wrong type. Created at 0o600
        // and same-uid so type is the only failing check.
        std::fs::write(&sock, b"").unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();

        let err = ensure_daemon_running(
            &sock,
            dir.path(),
            || Ok(()),
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect_err("non-socket file at attach path must be rejected");
        match err {
            AttachError::SocketUntrusted { path, reason } => {
                assert_eq!(path, sock);
                assert!(
                    reason.contains("not a Unix domain socket"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn trust_check_rejects_wrong_mode_socket() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        // Deliberately wrong mode — too permissive.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = ensure_daemon_running(
            &sock,
            dir.path(),
            || Ok(()),
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect_err("non-0600 socket must be rejected");
        match err {
            AttachError::SocketUntrusted { path, reason } => {
                assert_eq!(path, sock);
                assert!(reason.contains("mode"), "unexpected reason: {reason}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn trust_check_rejects_wrong_owner_socket() {
        // A non-root process can't chown to another uid, so we simulate
        // wrong-owner by chown'ing to a uid that almost certainly differs
        // from ours when we ARE root, and skipping the test otherwise.
        // SAFETY: getuid is async-signal-safe and infallible.
        let our_uid = unsafe { libc::getuid() };
        if our_uid != 0 {
            // Same-uid is the only realistic threat model — verifying the
            // negative path requires root to call chown, so skip otherwise.
            eprintln!("skipping: requires root to set foreign uid on test socket");
            return;
        }
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Chown to nobody-ish uid 65534 (only works as root).
        let cpath = std::ffi::CString::new(sock.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid path, valid uid_t, valid gid_t (-1 means "leave gid").
        let res = unsafe { libc::chown(cpath.as_ptr(), 65534, u32::MAX) };
        assert_eq!(res, 0, "chown should succeed when running as root");

        let err = ensure_daemon_running(
            &sock,
            dir.path(),
            || Ok(()),
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect_err("foreign-owned socket must be rejected");
        match err {
            AttachError::SocketUntrusted { path, reason } => {
                assert_eq!(path, sock);
                assert!(
                    reason.contains("owned by uid"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn flock_serializes_concurrent_ensure_calls() {
        // Two ensure_daemon_running calls hit the same state_dir at the
        // same time. The flock must serialize them so only the first runs
        // its spawn closure; the second sees the socket present after
        // acquiring the lock and short-circuits.
        let dir = race_safe_tempdir();
        let state_dir = dir.path().to_path_buf();
        let sock = state_dir.join("attach.sock");

        let calls = Arc::new(AtomicUsize::new(0));
        let listener_slot: Arc<std::sync::Mutex<Option<std::os::unix::net::UnixListener>>> =
            Arc::new(std::sync::Mutex::new(None));

        let s1 = sock.clone();
        let s1_for_spawn = sock.clone();
        let sd1 = state_dir.clone();
        let c1 = calls.clone();
        let slot1 = listener_slot.clone();
        let h1 = tokio::spawn(async move {
            ensure_daemon_running(
                &s1,
                &sd1,
                move || {
                    c1.fetch_add(1, Ordering::SeqCst);
                    // Sleep WHILE holding the spawn lock so the second
                    // task is parked on flock acquire — that's the race
                    // we want to demonstrate the lock fixes.
                    std::thread::sleep(Duration::from_millis(150));
                    let l = bind_trusted_socket(&s1_for_spawn);
                    *slot1.lock().unwrap() = Some(l);
                    Ok(())
                },
                Duration::from_millis(10),
                Duration::from_secs(2),
            )
            .await
        });

        // Give the first task time to enter spawn_fn.
        tokio::time::sleep(Duration::from_millis(40)).await;

        let s2 = sock.clone();
        let sd2 = state_dir.clone();
        let c2 = calls.clone();
        let h2 = tokio::spawn(async move {
            ensure_daemon_running(
                &s2,
                &sd2,
                move || {
                    c2.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                Duration::from_millis(10),
                Duration::from_secs(2),
            )
            .await
        });

        h1.await.unwrap().expect("first ensure should succeed");
        h2.await
            .unwrap()
            .expect("second ensure should short-circuit on the socket the first one bound");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "spawn_fn must run exactly once — flock should serialize the two callers"
        );
    }
}
