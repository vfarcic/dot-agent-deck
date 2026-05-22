//! Shared helpers for integration tests.
//!
//! ## Why this module exists
//!
//! `dot_agent_deck::daemon::run_daemon` (and `run_daemon_with`) acquire
//! an exclusive `flock(2)` over a per-socket `.lock` file *before*
//! binding. The lock file lives in a user-owned directory resolved via
//! `lock_root()`:
//!
//! 1. `DOT_AGENT_DECK_LOCK_DIR`  — test override (preferred)
//! 2. `$XDG_RUNTIME_DIR/dot-agent-deck`
//! 3. `$HOME/.cache/dot-agent-deck`
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
//! so distinct tests' locks don't collide), so we set
//! `DOT_AGENT_DECK_LOCK_DIR` exactly once at first use and let the
//! `TempDir` outlive every test in the binary.

use std::sync::OnceLock;
use tempfile::TempDir;

/// Holds the per-binary tempdir for the entire test run. Kept in a
/// `OnceLock` so the closure that calls `set_var` runs at most once and
/// completes-before any thread reads `DOT_AGENT_DECK_LOCK_DIR`.
static LOCK_DIR_GUARD: OnceLock<TempDir> = OnceLock::new();

/// Idempotent setup hook. Call once before any code path that touches
/// `lock_root()` — typically at the start of a test that spawns a
/// daemon, or inside a `spawn_daemon` helper. Safe to call from every
/// test; the underlying mutation runs at most once per test binary.
pub fn init_test_env() {
    LOCK_DIR_GUARD.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("dot-agent-deck-test-lock-")
            .tempdir()
            .expect("create per-binary lock-dir tempdir");
        // SAFETY: `OnceLock::get_or_init` serializes this closure so
        // exactly one thread runs `set_var`. Every test that reads
        // `DOT_AGENT_DECK_LOCK_DIR` via the daemon's `lock_root()`
        // routes through `init_test_env()` first, so all reads happen
        // after the single completed write. No reader can observe a
        // torn / pre-init state.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_LOCK_DIR", dir.path());
        }
        dir
    });
}
