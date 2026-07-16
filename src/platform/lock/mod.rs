//! Spawn-serialization lock (PRD #42 M1, lifted from `daemon_attach.rs`).
//!
//! Serializes concurrent first-attaches / `daemon serve` starts so only one
//! process races the bind. Unix uses an exclusive `flock(2)` on a lock file;
//! Windows will use a named mutex (`Global\dot-agent-deck-spawn-{user}`) — a
//! compiling stub at M1, real implementation in PRD #163. The RAII
//! [`SpawnLock`] guard and the `acquire_spawn_lock(path)` signature are
//! preserved across both backends so callers don't churn.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{SpawnLock, acquire_spawn_lock};
#[cfg(windows)]
pub use windows::{SpawnLock, acquire_spawn_lock};
