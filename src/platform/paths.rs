//! Home / runtime / state directory and IPC-endpoint path resolution
//! (PRD #42 M1, lifted from `config.rs`).
//!
//! The Unix branch preserves today's behavior byte-for-byte: `$HOME`,
//! `$XDG_RUNTIME_DIR`, the per-uid `/tmp` socket fallback, and `getuid(2)`
//! namespacing. The Windows branch resolves `%USERPROFILE%`/`%LOCALAPPDATA%`
//! and returns named-pipe endpoint strings (`\\.\pipe\dot-agent-deck-{user}-…`).
//! The `DOT_AGENT_DECK_*` env overrides stay authoritative on both platforms.
//!
//! Note: only the **path computation** lives here. The socket binding / I/O
//! that consumes these paths stays in `daemon*`/`hook`/`ui` until M2 abstracts
//! the transport.

use std::path::PathBuf;

/// Home directory used to anchor config/state/cache paths.
///
/// Unix: `$HOME`, falling back to `/` (matches the historical
/// `config::dirs_home`). Windows: `%USERPROFILE%`, falling back to `C:\`.
pub fn home_dir() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"))
    }
    #[cfg(windows)]
    {
        // `dirs::home_dir()` resolves `%USERPROFILE%` (via the known-folder API
        // — more robust than reading the env var directly).
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(r"C:\"))
    }
}

/// Current real uid, used to namespace the `/tmp` fallback sockets per user.
/// Wraps `getuid(2)` so the single `unsafe` lives in one place.
///
/// Unix-only: Windows has no uid concept and namespaces its named-pipe
/// endpoints by username instead (see [`endpoint_user_suffix`]).
#[cfg(unix)]
pub fn current_uid() -> u32 {
    // SAFETY: `getuid(2)` is async-signal-safe and has no failure mode; it
    // simply returns the calling process's real uid.
    unsafe { libc::getuid() }
}

/// Per-user namespacing suffix for the Windows named-pipe endpoints — the
/// Win32 analogue of the per-uid `/tmp` socket suffix.
#[cfg(windows)]
pub fn endpoint_user_suffix() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string())
}

/// Hook-ingestion endpoint. Unix: a Unix-domain-socket path
/// (`$XDG_RUNTIME_DIR/dot-agent-deck.sock` else `/tmp/dot-agent-deck-{uid}.sock`).
/// Windows: the named-pipe `\\.\pipe\dot-agent-deck-{user}-hook`.
///
/// `DOT_AGENT_DECK_SOCKET` overrides on both platforms.
pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_SOCKET") {
        return PathBuf::from(path);
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("dot-agent-deck.sock");
        }

        // PRD #93 reviewer REV-2: the `/tmp` fallback must include the uid so
        // two users on the same host can't collide on the same socket path
        // (the daemon is per-user; the 0o600 mode is on the socket inode, but
        // the *path* still has to be unique, otherwise the loser's `bind(2)`
        // sees `EADDRINUSE` against the winner's inode). Same rationale as
        // `attach_socket_path` below.
        PathBuf::from(format!("/tmp/dot-agent-deck-{}.sock", current_uid()))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\dot-agent-deck-{}-hook",
            endpoint_user_suffix()
        ))
    }
}

/// Streaming-attach endpoint (separate from the hook endpoint so the two
/// protocols have disjoint wire formats — hook is line-delimited JSON, attach
/// is a binary frame protocol). Unix: `$XDG_RUNTIME_DIR/dot-agent-deck-attach.sock`
/// else `/tmp/dot-agent-deck-attach-{uid}.sock`. Windows: the named pipe
/// `\\.\pipe\dot-agent-deck-{user}-attach`.
///
/// `DOT_AGENT_DECK_ATTACH_SOCKET` overrides on both platforms.
pub fn attach_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_ATTACH_SOCKET") {
        return PathBuf::from(path);
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("dot-agent-deck-attach.sock");
        }

        // PRD #93 reviewer REV-2: include the uid in the `/tmp` fallback path so
        // two users on the same host get disjoint sockets (each daemon's
        // `bind(2)` would otherwise collide with the other user's inode), and
        // so the path itself can't be observed by another user to figure out
        // *which* deck process to target. The 0o600 mode on the inode is
        // already enforced; the per-user path is the missing half.
        PathBuf::from(format!("/tmp/dot-agent-deck-attach-{}.sock", current_uid()))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\dot-agent-deck-{}-attach",
            endpoint_user_suffix()
        ))
    }
}

/// Per-user state directory (detached-daemon log, spawn mutex). Resolution
/// order on Unix:
///
/// 1. `DOT_AGENT_DECK_STATE_DIR` — explicit override (tests use this).
/// 2. `$XDG_STATE_HOME/dot-agent-deck` — freedesktop spec default.
/// 3. `$HOME/.local/state/dot-agent-deck` — XDG fallback.
///
/// Windows: the override first, then `%LOCALAPPDATA%\dot-agent-deck` (already
/// per-user ACL'd by default).
pub fn state_dir() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_STATE_DIR") {
        return PathBuf::from(path);
    }

    #[cfg(unix)]
    {
        match std::env::var("XDG_STATE_HOME") {
            Ok(state_home) if !state_home.is_empty() => {
                PathBuf::from(state_home).join("dot-agent-deck")
            }
            _ => home_dir().join(".local/state/dot-agent-deck"),
        }
    }
    #[cfg(windows)]
    {
        // `dirs::data_local_dir()` resolves `%LOCALAPPDATA%` (already per-user
        // ACL'd by default).
        match dirs::data_local_dir() {
            Some(local) => local.join("dot-agent-deck"),
            None => home_dir().join("AppData/Local/dot-agent-deck"),
        }
    }
}
