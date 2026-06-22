//! Daemon auto-start for the desktop GUI (PRD #176).
//!
//! The TUI ensures the always-external daemon (PRD #93) is up before it
//! connects: `src/daemon_attach.rs::ensure_external_daemon_or_die` checks the
//! attach socket, and if it is missing, fork-execs `<binary> daemon serve`
//! detached (setsid, stdin `/dev/null`, stdout/stderr → `<state>/daemon.log`),
//! serialized by an `flock(2)` on `<state>/spawn.lock`, then polls the socket
//! every 50 ms for up to 5 s. The GUI is just a *fourth client* of the daemon
//! (Design Decision #1), so it must bring the daemon up the *same way* rather
//! than asking the user to start it by hand.
//!
//! This module mirrors that mechanism faithfully — `src/daemon_attach.rs` is
//! the source of truth and the comments here point back at it — with ONE
//! difference that is the whole reason it can't just call the TUI's code:
//!
//! **Locating the daemon binary.** The TUI finds it via
//! [`std::env::current_exe`] because the TUI *is* `dot-agent-deck`. The GUI's
//! `current_exe()` is the GUI app, not the daemon, so [`resolve_daemon_binary`]
//! resolves it in this order:
//!
//! 1. **`DOT_AGENT_DECK_BIN`** — explicit override; honored verbatim if set
//!    (an unset/empty value is ignored). The escape hatch for a non-standard
//!    install or a test.
//! 2. **`dot-agent-deck` on `PATH`** — the normal case: the user has the CLI
//!    installed, so a plain `PATH` lookup finds it.
//! 3. **Workspace dev build** — `<workspace>/target/debug/dot-agent-deck` then
//!    `…/target/release/dot-agent-deck`, resolved from this crate's
//!    `CARGO_MANIFEST_DIR` so `npm run dev` works from a checkout that has run
//!    `cargo build` but hasn't installed the CLI on `PATH`.
//!
//! If none resolve, [`EnsureDaemonError::BinaryNotFound`] carries an actionable
//! reason; the connect path turns it into a connect/retry state.

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

/// Explicit override for the daemon binary location (resolution step 1).
pub const DAEMON_BIN_ENV: &str = "DOT_AGENT_DECK_BIN";

/// The daemon binary name looked up on `PATH` and under the dev build dirs.
const DAEMON_BIN_NAME: &str = "dot-agent-deck";

/// Poll cadence + ceiling for the attach socket to appear after a spawn —
/// identical to the TUI's `ensure_external_daemon_or_die` (50 ms / 5 s).
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from the ensure-or-spawn path. The connect layer wraps these into a
/// connect/retry state carrying the `Display` text as the reason, so every
/// variant's message is written to read well to a user staring at "couldn't
/// connect".
#[derive(Debug, Error)]
pub enum EnsureDaemonError {
    #[error(
        "could not locate the dot-agent-deck daemon binary: set DOT_AGENT_DECK_BIN to its path, \
         or install dot-agent-deck on your PATH"
    )]
    BinaryNotFound,
    #[error("failed to prepare the daemon state directory: {0}")]
    StateSetup(#[source] io::Error),
    #[error("failed to spawn the daemon ({}): {source}", bin.display())]
    SpawnFailed {
        bin: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "daemon did not start: attach socket {path} never appeared within {timeout_ms}ms. \
         Check {log} for daemon stderr."
    )]
    StartTimeout {
        path: PathBuf,
        log: PathBuf,
        timeout_ms: u128,
    },
    #[error("refusing to use daemon attach socket {path}: {reason}")]
    SocketUntrusted { path: PathBuf, reason: String },
}

/// Ensure a daemon is reachable at `socket_path`, auto-starting one if not.
///
/// Production entry point: resolves the daemon binary up front (so a
/// missing-binary failure surfaces before any locking/spawning), then defers to
/// [`ensure_daemon_running`] with a spawn closure that fork-execs the resolved
/// binary detached. The binary resolution order is documented on
/// [`resolve_daemon_binary`].
pub async fn ensure_daemon(socket_path: &Path) -> Result<(), EnsureDaemonError> {
    let state = state_dir();
    let bin = resolve_daemon_binary()?;
    let state_for_spawn = state.clone();
    ensure_daemon_running(
        socket_path,
        &state,
        move || {
            spawn_daemon_detached(&bin, &state_for_spawn)
                .map_err(|source| EnsureDaemonError::SpawnFailed { bin, source })
        },
        POLL_INTERVAL,
        POLL_TIMEOUT,
    )
    .await
}

/// The lock-serialized "is the socket live? if not, `spawn_fn` and poll" loop.
///
/// Mirrors `src/daemon_attach.rs::ensure_daemon_running`: pure of the
/// spawn details (the closure does the fork-exec) so tests can drive the loop
/// by passing a closure that synchronously creates the socket — or one that
/// does nothing, to exercise the timeout. Concurrent first-attaches (a GUI and
/// a TUI launched together) serialize on `<state>/spawn.lock` so only one races
/// the daemon's `bind(2)`; the loser re-checks under the lock and short-circuits.
///
/// Both the pre-existing and freshly-spawned socket paths run
/// [`verify_socket_trusted`] before returning, so a same-uid attacker who
/// pre-binds at the attach path is rejected rather than connected to.
pub async fn ensure_daemon_running<F>(
    socket_path: &Path,
    state_dir: &Path,
    spawn_fn: F,
    poll_interval: Duration,
    poll_timeout: Duration,
) -> Result<(), EnsureDaemonError>
where
    F: FnOnce() -> Result<(), EnsureDaemonError>,
{
    prepare_state_dir(state_dir).map_err(EnsureDaemonError::StateSetup)?;

    // Acquire the spawn mutex. flock(2) blocks until granted, so this doubles
    // as the "wait for an in-flight spawn to finish" barrier.
    let lock_path = state_dir.join("spawn.lock");
    let _lock = acquire_spawn_lock(&lock_path)
        .await
        .map_err(EnsureDaemonError::StateSetup)?;

    // First check INSIDE the lock so a waiter that lost the race sees the
    // socket the winner created and skips the spawn.
    if socket_path.exists() {
        verify_socket_trusted(socket_path)?;
        // Existence ≠ liveness: a daemon that died without unlinking (crash,
        // SIGKILL, reboot mid-write) leaves a stale socket that would otherwise
        // short-circuit forever with every connect getting ECONNREFUSED. Probe;
        // on failure the inode is ours (trust check validated uid + mode), so
        // unlinking and falling through to spawn is safe.
        if probe_socket_alive(socket_path).await {
            return Ok(());
        }
        let _ = std::fs::remove_file(socket_path);
    }

    spawn_fn()?;

    let log = state_dir.join("daemon.log");
    let start = Instant::now();
    loop {
        if socket_path.exists() {
            verify_socket_trusted(socket_path)?;
            return Ok(());
        }
        if start.elapsed() >= poll_timeout {
            return Err(EnsureDaemonError::StartTimeout {
                path: socket_path.to_path_buf(),
                log,
                timeout_ms: poll_timeout.as_millis(),
            });
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Resolve the `dot-agent-deck` daemon binary. See the module docs for the
/// order: `DOT_AGENT_DECK_BIN` → `PATH` → workspace dev build.
fn resolve_daemon_binary() -> Result<PathBuf, EnsureDaemonError> {
    let override_bin = std::env::var(DAEMON_BIN_ENV).ok();
    let path_dirs = path_dirs();
    let dev_candidates = dev_candidates();
    resolve_daemon_binary_from(override_bin, &path_dirs, &dev_candidates, &|p| p.is_file())
        .ok_or(EnsureDaemonError::BinaryNotFound)
}

/// Pure resolver behind [`resolve_daemon_binary`] — all the I/O (env, PATH
/// split, filesystem existence) is injected so the precedence is unit-testable
/// without touching the real environment.
///
/// - `override_bin` (step 1) is honored verbatim when present and non-empty,
///   *without* an existence check: an explicit override is the user's
///   deliberate choice, and if it can't be exec'd the spawn error names it.
/// - `path_dirs` (step 2) and `dev_candidates` (step 3) are gated by `is_file`,
///   so only an actually-present binary is chosen.
fn resolve_daemon_binary_from(
    override_bin: Option<String>,
    path_dirs: &[PathBuf],
    dev_candidates: &[PathBuf],
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(bin) = override_bin.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(bin));
    }
    for dir in path_dirs {
        let candidate = dir.join(DAEMON_BIN_NAME);
        if is_file(&candidate) {
            return Some(candidate);
        }
    }
    for candidate in dev_candidates {
        if is_file(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

/// `PATH` split into its component directories (empty when `PATH` is unset).
fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Workspace dev-build candidates, resolved from this crate's compile-time
/// `CARGO_MANIFEST_DIR` (`<workspace>/gui/core`) so the path is independent of
/// the GUI's runtime working directory — `npm run dev` can launch from anywhere
/// in the checkout. Debug is preferred over release (a dev workflow rebuilds
/// debug). On an installed GUI these paths simply don't exist and step 1/2 win.
fn dev_candidates() -> Vec<PathBuf> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    vec![
        workspace_root
            .join("target")
            .join("debug")
            .join(DAEMON_BIN_NAME),
        workspace_root
            .join("target")
            .join("release")
            .join(DAEMON_BIN_NAME),
    ]
}

/// Spawn `<bin> daemon serve` as a detached background process that survives
/// the GUI's exit. Mirrors `src/daemon_attach.rs::spawn_daemon_serve_detached_with_exe`:
///
/// - `setsid(2)` in the child (`pre_exec`) makes the daemon its own session
///   leader so it won't get `SIGHUP` when the GUI exits.
/// - stdin is `/dev/null`; stdout/stderr append to `<state>/daemon.log`, opened
///   `O_NOFOLLOW` + mode 0o600 so a same-uid attacker can't pre-plant a symlink
///   to redirect daemon output (which carries hook payloads / task strings).
/// - We do not wait on the child — the caller polls the attach socket for
///   readiness ([`ensure_daemon_running`]).
fn spawn_daemon_detached(bin: &Path, state_dir: &Path) -> io::Result<()> {
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
            return Err(io::Error::other(format!(
                "daemon log path {} is a symlink — refusing to follow (someone may have planted it to redirect daemon output)",
                log_path.display()
            )));
        }
        Err(e) => return Err(e),
    };
    let stderr = stdout.try_clone()?;
    let stdin = std::fs::File::open("/dev/null")?;

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("daemon")
        .arg("serve")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);

    // SAFETY: `pre_exec` runs in the child between fork and exec; only
    // async-signal-safe libc calls are permitted there. `setsid(2)` is on
    // POSIX's async-signal-safe list and we do nothing else in the closure.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Spawn and drop the handle: the child is its own session leader, so init
    // reaps it once the GUI exits. The caller polls the socket for readiness.
    cmd.spawn().map(|_child| ())
}

/// Verify `path` is a Unix socket owned by the current uid at mode 0o600.
/// Mirrors `src/daemon_attach.rs::verify_socket_trusted` — defends against a
/// same-uid process pre-binding at the attach path before the real daemon does.
fn verify_socket_trusted(path: &Path) -> Result<(), EnsureDaemonError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata =
        std::fs::metadata(path).map_err(|source| EnsureDaemonError::SocketUntrusted {
            path: path.to_path_buf(),
            reason: format!("stat failed: {source}"),
        })?;

    if !metadata.file_type().is_socket() {
        return Err(EnsureDaemonError::SocketUntrusted {
            path: path.to_path_buf(),
            reason: "not a Unix domain socket".to_string(),
        });
    }

    // SAFETY: getuid(2) is async-signal-safe, has no failure mode, and returns
    // the calling process's real uid.
    let our_uid = unsafe { libc::getuid() };
    if metadata.uid() != our_uid {
        return Err(EnsureDaemonError::SocketUntrusted {
            path: path.to_path_buf(),
            reason: format!("owned by uid {} (expected {})", metadata.uid(), our_uid),
        });
    }

    let mode = metadata.mode() & 0o777;
    if mode != 0o600 {
        return Err(EnsureDaemonError::SocketUntrusted {
            path: path.to_path_buf(),
            reason: format!("mode is 0o{mode:o} (expected 0o600)"),
        });
    }

    Ok(())
}

/// Best-effort liveness probe: true iff `connect(2)` succeeds. Any error
/// (typically ECONNREFUSED from a stale inode whose binder is dead) is false.
async fn probe_socket_alive(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// RAII guard for the `spawn.lock` flock; drop releases it.
struct SpawnLock {
    file: std::fs::File,
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        // SAFETY: fd is valid for the lifetime of self.file; LOCK_UN on a held
        // lock reverses the LOCK_EX taken in acquire_spawn_lock.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Open-or-create `path` and take an exclusive `flock(2)`. flock is blocking,
/// so the syscall runs on `spawn_blocking` to avoid stalling other tokio tasks
/// when another launcher on this host is mid-spawn. Mirrors
/// `src/daemon_attach.rs::acquire_spawn_lock`.
async fn acquire_spawn_lock(path: &Path) -> io::Result<SpawnLock> {
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
        // SAFETY: valid fd + valid op constant; flock(2) retains no reference to
        // our address space, so the unsafe is a formality of the libc binding.
        let res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if res != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SpawnLock { file })
    })
    .await
    .map_err(io::Error::other)?
}

/// Create `state_dir` (recursive) at mode 0o700, repairing the mode on a
/// pre-existing dir. Mirrors `src/daemon_attach.rs::prepare_state_dir`.
fn prepare_state_dir(state_dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(state_dir)?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
}

/// Per-user state directory for the daemon log and spawn lock. Replicates
/// `src/config.rs::state_dir` (env override → `$XDG_STATE_HOME/dot-agent-deck`
/// → `$HOME/.local/state/dot-agent-deck`) so the GUI's lazy-spawn writes to the
/// same place the TUI's does. The socket path itself comes from the shared
/// `protocol::attach_socket_path`, so connect correctness never depends on this
/// matching exactly — only the log/lock location does.
fn state_dir() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_STATE_DIR") {
        return PathBuf::from(path);
    }
    match std::env::var("XDG_STATE_HOME") {
        Ok(state_home) if !state_home.is_empty() => {
            PathBuf::from(state_home).join("dot-agent-deck")
        }
        _ => home_dir().join(".local/state/dot-agent-deck"),
    }
}

/// `$HOME`, falling back to `/` (matches `src/config.rs::dirs_home`).
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Step 1 wins: an explicit `DOT_AGENT_DECK_BIN` override is returned
    /// verbatim and short-circuits PATH + dev fallback (even when those would
    /// also resolve).
    #[test]
    fn binary_resolution_prefers_override() {
        let chosen = resolve_daemon_binary_from(
            Some("/custom/dot-agent-deck".to_string()),
            &[PathBuf::from("/usr/bin")],
            &[PathBuf::from("/ws/target/debug/dot-agent-deck")],
            &|_| true,
        );
        assert_eq!(chosen, Some(PathBuf::from("/custom/dot-agent-deck")));
    }

    /// An empty override is ignored (treated as unset) so it can't shadow a
    /// real PATH hit.
    #[test]
    fn binary_resolution_ignores_empty_override() {
        let chosen = resolve_daemon_binary_from(
            Some(String::new()),
            &[PathBuf::from("/usr/bin")],
            &[],
            &|p| p == Path::new("/usr/bin/dot-agent-deck"),
        );
        assert_eq!(chosen, Some(PathBuf::from("/usr/bin/dot-agent-deck")));
    }

    /// Step 2: with no override, the first PATH directory containing the binary
    /// wins, in PATH order.
    #[test]
    fn binary_resolution_finds_on_path_in_order() {
        let dirs = [
            PathBuf::from("/empty"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/bin"),
        ];
        let chosen = resolve_daemon_binary_from(None, &dirs, &[], &|p| {
            p == Path::new("/usr/local/bin/dot-agent-deck")
                || p == Path::new("/usr/bin/dot-agent-deck")
        });
        assert_eq!(
            chosen,
            Some(PathBuf::from("/usr/local/bin/dot-agent-deck")),
            "earliest matching PATH dir wins"
        );
    }

    /// Step 3: override absent and nothing on PATH → the dev build candidates
    /// are tried, debug before release.
    #[test]
    fn binary_resolution_falls_back_to_dev_build() {
        let dirs = [PathBuf::from("/usr/bin")];
        let dev = [
            PathBuf::from("/ws/target/debug/dot-agent-deck"),
            PathBuf::from("/ws/target/release/dot-agent-deck"),
        ];
        let chosen = resolve_daemon_binary_from(None, &dirs, &dev, &|p| {
            // Nothing on PATH; both dev builds present → debug preferred.
            p.starts_with("/ws/target/")
        });
        assert_eq!(
            chosen,
            Some(PathBuf::from("/ws/target/debug/dot-agent-deck"))
        );
    }

    /// Nothing resolves → `None`, which the caller turns into a clear
    /// `BinaryNotFound` reason naming the env override and PATH.
    #[test]
    fn binary_resolution_none_when_nothing_found() {
        let chosen = resolve_daemon_binary_from(
            None,
            &[PathBuf::from("/usr/bin")],
            &[PathBuf::from("/ws/target/debug/dot-agent-deck")],
            &|_| false,
        );
        assert_eq!(chosen, None);
        let reason = EnsureDaemonError::BinaryNotFound.to_string();
        assert!(reason.contains("DOT_AGENT_DECK_BIN"), "got: {reason}");
        assert!(reason.contains("PATH"), "got: {reason}");
    }

    /// Happy path: when the spawn closure makes a trusted (0o600) socket appear
    /// before the timeout, `ensure_daemon_running` returns Ok.
    #[tokio::test]
    async fn socket_appears_within_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("attach.sock");
        let state = dir.path().join("state");

        let sock_for_spawn = sock.clone();
        let spawn = move || {
            // Bind a real socket so verify_socket_trusted sees a socket inode
            // owned by us, then chmod to the 0o600 the daemon sets post-bind.
            let listener = std::os::unix::net::UnixListener::bind(&sock_for_spawn)
                .map_err(EnsureDaemonError::StateSetup)?;
            std::fs::set_permissions(&sock_for_spawn, std::fs::Permissions::from_mode(0o600))
                .map_err(EnsureDaemonError::StateSetup)?;
            // Keep the bound socket file in place for the poll/verify; leaking
            // the listener avoids any drop-time unlink ambiguity in the test.
            std::mem::forget(listener);
            Ok(())
        };

        ensure_daemon_running(
            &sock,
            &state,
            spawn,
            Duration::from_millis(5),
            Duration::from_secs(2),
        )
        .await
        .expect("socket appeared and is trusted");
    }

    /// Failure reason: a spawn that never produces the socket times out with a
    /// `StartTimeout` whose message names the missing socket.
    #[tokio::test]
    async fn timeout_when_socket_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("attach.sock");
        let state = dir.path().join("state");

        let err = ensure_daemon_running(
            &sock,
            &state,
            || Ok(()), // "spawned" but nothing ever binds
            Duration::from_millis(5),
            Duration::from_millis(60),
        )
        .await
        .expect_err("must time out when the socket never appears");

        match &err {
            EnsureDaemonError::StartTimeout { path, .. } => assert_eq!(path, &sock),
            other => panic!("expected StartTimeout, got {other:?}"),
        }
        assert!(
            err.to_string().contains("never appeared"),
            "reason should explain the failure: {err}"
        );
    }
}
