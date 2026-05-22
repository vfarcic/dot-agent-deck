//! Shared helpers for integration tests.
//!
//! ## Why this module exists
//!
//! `dot_agent_deck::daemon::run_daemon` (and `run_daemon_with`) acquire
//! an exclusive `flock(2)` over a per-socket `.lock` file *before*
//! binding. The lock file lives in a user-owned directory resolved via
//! `lock_root()`:
//!
//! 1. `dot_agent_deck::daemon::LOCK_DIR_OVERRIDE` — in-process override (preferred for tests)
//! 2. `DOT_AGENT_DECK_LOCK_DIR` — env-var override (subprocess tests)
//! 3. `$XDG_RUNTIME_DIR/dot-agent-deck`
//! 4. `$HOME/.cache/dot-agent-deck`
//!
//! On a CI runner whose `XDG_RUNTIME_DIR` points at a path the test
//! process can't `mkdir` / `set_permissions` on — or whose
//! `~/.cache/dot-agent-deck` was pre-created at restrictive perms by an
//! earlier user — `ensure_lock_root` returns `PermissionDenied` and the
//! daemon fails to start. Every test that spawns a daemon hits this.
//!
//! The fix: redirect `lock_root()` into a per-binary tempdir owned by
//! the test process. The tempdir is shared by every test in the binary
//! (each lock file is suffixed with a stable hash of its socket path,
//! so distinct tests' locks don't collide).
//!
//! Round-10 auditor #4: we use `LOCK_DIR_OVERRIDE` (a `OnceLock<PathBuf>`)
//! instead of mutating `std::env::var("DOT_AGENT_DECK_LOCK_DIR")`.
//! `std::env::set_var` is `unsafe` because it races against any
//! concurrent `getenv` — including libc-internal reads of unrelated
//! variables on the same `environ` array. Inside a test binary,
//! `libtest` has already spawned worker threads by the time any
//! `#[test]` function runs, so the prior `OnceLock::get_or_init`
//! pattern still had a window where another worker could be reading
//! env state while ours was writing. `OnceLock::set` on the daemon's
//! in-process override has well-defined memory ordering and touches
//! no global env state, eliminating the race entirely for in-process
//! tests. Subprocess tests (`tests/external_daemon.rs`) still need
//! `DOT_AGENT_DECK_LOCK_DIR` set on the child's environment —
//! they handle that locally, alongside the other env vars they
//! already set the same way.

use std::path::PathBuf;
use std::sync::OnceLock;
use tempfile::TempDir;

/// Holds the per-binary tempdir for the entire test run. Kept in a
/// `OnceLock` so the closure that creates the tempdir runs at most
/// once.
static LOCK_DIR_GUARD: OnceLock<TempDir> = OnceLock::new();

/// Idempotent setup hook. Call once before any code path that touches
/// `lock_root()` — typically at the start of a test that spawns a
/// daemon, or inside a `spawn_daemon` helper. Safe to call from every
/// test; the underlying initialization runs at most once per test
/// binary and uses only race-free primitives.
pub fn init_test_env() {
    LOCK_DIR_GUARD.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("dot-agent-deck-test-lock-")
            .tempdir()
            .expect("create per-binary lock-dir tempdir");
        // `OnceLock::set` returns Err if already set (e.g. a different
        // test in the same binary also called init_test_env via a
        // different path). Both cases are fine — the override either
        // becomes our tempdir, or stays whatever the first caller
        // installed. Either way subsequent `lock_root()` calls see a
        // valid override pointing at a writable per-binary tempdir.
        let _ = dot_agent_deck::daemon::LOCK_DIR_OVERRIDE.set(dir.path().to_path_buf());
        dir
    });
}

/// Path to the per-binary lock-dir tempdir, for subprocess tests
/// that need to forward it on the child's environment via
/// `Command::env`. In-process tests should not need this — they go
/// through `lock_root()`'s `LOCK_DIR_OVERRIDE` check directly. Returns
/// `None` if `init_test_env` was never called.
#[allow(dead_code)] // only used by tests/external_daemon.rs
pub fn lock_dir_path() -> Option<PathBuf> {
    LOCK_DIR_GUARD.get().map(|d| d.path().to_path_buf())
}
