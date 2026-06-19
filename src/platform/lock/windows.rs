//! Windows spawn lock (PRD #42 M1 compiling skeleton; real implementation is
//! PRD #163).
//!
//! The idiomatic Windows primitive for cross-process mutual exclusion of the
//! spawn is a **named mutex** (`CreateMutexW(Global\dot-agent-deck-spawn-{user})`
//! plus `WaitForSingleObject`), which also doubles as the singleton-daemon
//! guard and so replaces the Unix stale-inode bind race entirely.
//! `WAIT_ABANDONED`
//! (owner crashed while holding it) is treated as "acquired, prior owner
//! crashed". An alternative is `LockFileEx` on the lock-file handle.
//!
//! At M1 this is a skeleton: it opens/holds the lock file so the RAII
//! [`SpawnLock`] shape and the `acquire_spawn_lock(path)` signature match the
//! Unix backend, but does not yet provide real cross-process exclusion. That
//! lands in #163 and is only exercised once the Windows IPC transport (M2)
//! makes the daemon path compile on Windows.

use std::path::Path;

/// RAII guard mirroring the Unix [`crate::platform::lock::SpawnLock`]. Holds
/// the lock file open for its lifetime; #163 will additionally own the named
/// mutex / `LockFileEx` handle and release it on drop.
pub struct SpawnLock {
    #[allow(dead_code)]
    file: std::fs::File,
}

/// Windows counterpart to the Unix `acquire_spawn_lock`. See the module docs:
/// at M1 this opens the lock file but does not yet enforce exclusion (#163).
pub async fn acquire_spawn_lock(path: &Path) -> std::io::Result<SpawnLock> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        Ok(SpawnLock { file })
    })
    .await
    .map_err(std::io::Error::other)?
}
