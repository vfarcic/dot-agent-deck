//! Shared helpers for integration tests.
//!
//! ## Why this module exists
//!
//! `dot_agent_deck::daemon::run_daemon_with` acquires an exclusive
//! `flock(2)` over a per-socket `.lock` file before binding. The lock
//! file lives in a user-owned directory resolved by `lock_root` in this
//! order:
//!
//! 1. `Daemon::lock_dir_override` (set by `with_lock_dir_override`)
//! 2. `DOT_AGENT_DECK_LOCK_DIR` env var
//! 3. `$XDG_RUNTIME_DIR/dot-agent-deck`
//! 4. `$HOME/.cache/dot-agent-deck`
//!
//! On a CI runner whose `XDG_RUNTIME_DIR` points at a path the test
//! process can't `mkdir` / `set_permissions` on — or whose
//! `~/.cache/dot-agent-deck` was pre-created at restrictive perms by an
//! earlier user — `ensure_lock_root` returns `PermissionDenied` and the
//! daemon fails to start. Every test that spawns a daemon hits this.
//!
//! The fix: each test redirects `lock_root` to a per-binary tempdir by
//! constructing its daemon with
//! `Daemon::new(state).with_lock_dir_override(common::lock_dir_path())`
//! (or the equivalent on `Daemon::with_attach`). The tempdir is shared
//! by every test in the binary; lock files are suffixed with a stable
//! hash of the full socket path, so distinct tests' locks don't
//! collide.
//!
//! Round-11 reviewer #B: the override is a per-`Daemon` builder field,
//! not a process-wide static. Production builds compile without any
//! "set the lock dir from anywhere in the process" surface, and the
//! daemon's lock root cannot be steered by code other than the
//! constructor.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use tempfile::TempDir;

/// Holds the per-binary tempdir for the entire test run.
static LOCK_DIR_GUARD: OnceLock<TempDir> = OnceLock::new();

/// Idempotent setup hook. Call once before constructing a daemon in a
/// test — typically at the start of a `spawn_daemon` helper. Creates
/// the per-binary tempdir on first call; subsequent calls are no-ops.
pub fn init_test_env() {
    LOCK_DIR_GUARD.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("dot-agent-deck-test-lock-")
            .tempdir()
            .expect("create per-binary lock-dir tempdir")
    });
}

/// Path to the per-binary lock-dir tempdir, for passing to
/// [`dot_agent_deck::daemon::Daemon::with_lock_dir_override`] (in-process
/// tests) or to `Command::env` for subprocess-based tests. Returns
/// `None` if [`init_test_env`] was never called.
pub fn lock_dir_path() -> Option<PathBuf> {
    LOCK_DIR_GUARD.get().map(|d| d.path().to_path_buf())
}

/// `tempfile::tempdir()` calls `mkdir(2)` with mode `0o700 & ~umask`. The
/// crate's `bind_socket` (src/daemon.rs) briefly flips the process-global
/// umask to `0o177`, and any concurrent `mkdir` (in another test) lands
/// during that window with mode `0o700 & ~0o177 = 0o600` — no execute
/// bit, so the directory is no longer traversable and any subsequent
/// `bind(2)` of a socket inside it fails with `EACCES`. Re-apply 0o700
/// after creation so concurrent integration tests are robust to the race.
///
/// This is the integration-test counterpart of the same-named helper in
/// `src/daemon_attach.rs` tests module; promoted here so every test
/// binary that spawns daemons (orchestration_delegate, daemon_protocol,
/// etc.) gets the fix without duplicating the workaround.
#[allow(dead_code)]
pub fn race_safe_tempdir() -> TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("chmod tempdir to 0o700");
    dir
}
