//! Daemon socket-path discovery (PRD #93 always-external-daemon).
//!
//! Extracted into the `protocol` crate (PRD #176 M1.2) so every client — the
//! TUI binary's `config`, the CLI daemon commands, and the desktop GUI core —
//! resolves the daemon's Unix-socket paths through ONE definition instead of
//! each re-deriving the env/XDG/`/tmp` scheme and risking drift. The binary's
//! `config::socket_path` / `config::attach_socket_path` now delegate here, and
//! the GUI core connects through [`attach_socket_path`] so a second front-end
//! can never invent a divergent path.
//!
//! Two sockets, disjoint wire formats:
//! - [`socket_path`] — the line-delimited-JSON hook-ingestion socket.
//! - [`attach_socket_path`] — the binary frame attach socket whose codec lives
//!   in [`crate::wire`] (what the GUI core connects to).
//!
//! Resolution order (identical for both): an explicit env override, then
//! `$XDG_RUNTIME_DIR/<name>`, then a per-user `/tmp/<name>-<uid>.sock`
//! fallback. The uid is embedded in the `/tmp` fallback so two users on one
//! host get disjoint paths (PRD #93 reviewer REV-2): the daemon is per-user
//! and the `0o600` mode lives on the inode, but the *path* still has to be
//! unique or the loser's `bind(2)` sees `EADDRINUSE` against the winner's
//! inode.

use std::path::PathBuf;

/// Explicit override for [`socket_path`] (the hook-ingestion socket).
pub const SOCKET_ENV: &str = "DOT_AGENT_DECK_SOCKET";
/// Explicit override for [`attach_socket_path`] (the streaming-attach socket).
pub const ATTACH_SOCKET_ENV: &str = "DOT_AGENT_DECK_ATTACH_SOCKET";

/// Resolve the hook-ingestion socket path: [`SOCKET_ENV`] →
/// `$XDG_RUNTIME_DIR/dot-agent-deck.sock` → `/tmp/dot-agent-deck-<uid>.sock`.
pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var(SOCKET_ENV) {
        return PathBuf::from(path);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("dot-agent-deck.sock");
    }
    PathBuf::from(format!("/tmp/dot-agent-deck-{}.sock", current_uid()))
}

/// Resolve the streaming-attach socket path: [`ATTACH_SOCKET_ENV`] →
/// `$XDG_RUNTIME_DIR/dot-agent-deck-attach.sock` →
/// `/tmp/dot-agent-deck-attach-<uid>.sock`.
pub fn attach_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var(ATTACH_SOCKET_ENV) {
        return PathBuf::from(path);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("dot-agent-deck-attach.sock");
    }
    PathBuf::from(format!("/tmp/dot-agent-deck-attach-{}.sock", current_uid()))
}

/// Current OS uid, used to namespace the `/tmp` fallback sockets per user.
/// Wraps `libc::getuid` so the unsafe is centralized in one place.
pub fn current_uid() -> u32 {
    // SAFETY: `getuid(2)` is async-signal-safe and has no failure mode; it
    // simply returns the calling process's real uid.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize the process-global env mutation against any future
    // env-sensitive test in this crate.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// PRD #93 reviewer REV-2: with every override and `XDG_RUNTIME_DIR`
    /// unset, both fallbacks must land under `/tmp` and embed the uid so two
    /// users on one host can't collide on a single socket path.
    #[test]
    fn tmp_fallback_is_per_user() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_attach = std::env::var(ATTACH_SOCKET_ENV).ok();
        let prev_sock = std::env::var(SOCKET_ENV).ok();
        let prev_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: env lock held; restored before the asserts run.
        unsafe {
            std::env::remove_var(ATTACH_SOCKET_ENV);
            std::env::remove_var(SOCKET_ENV);
            std::env::remove_var("XDG_RUNTIME_DIR");
        }

        let uid = current_uid();
        let attach = attach_socket_path();
        let hook = socket_path();

        // SAFETY: env lock held; restore the prior environment.
        unsafe {
            restore("XDG_RUNTIME_DIR", prev_xdg);
            restore(SOCKET_ENV, prev_sock);
            restore(ATTACH_SOCKET_ENV, prev_attach);
        }

        assert_eq!(
            attach,
            PathBuf::from(format!("/tmp/dot-agent-deck-attach-{uid}.sock"))
        );
        assert_eq!(
            hook,
            PathBuf::from(format!("/tmp/dot-agent-deck-{uid}.sock"))
        );
    }

    /// Restore an env var to a saved value (`None` → remove). Caller holds
    /// [`ENV_LOCK`].
    ///
    /// # Safety
    /// Mutates the process-global environment; only sound while no other
    /// thread reads or writes the environment concurrently.
    unsafe fn restore(key: &str, val: Option<String>) {
        match val {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
