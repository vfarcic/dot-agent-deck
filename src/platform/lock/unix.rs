//! Unix spawn lock: exclusive `flock(2)` on a lock file. Behavior-preserving
//! lift of the former `daemon_attach::{acquire_spawn_lock, SpawnLock}`.

use std::os::unix::io::AsRawFd;
use std::path::Path;

/// RAII guard for the `spawn.lock` flock. Drop releases the lock by closing the
/// file descriptor (and explicitly `LOCK_UN`'ing for clarity).
pub struct SpawnLock {
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
/// blocking, so we run the syscall on `spawn_blocking` to avoid stalling other
/// tasks scheduled on the same tokio worker when contention is real (i.e.,
/// another caller on this host is mid-spawn).
///
/// Reused by both the lazy-spawn machinery and the daemon's own
/// `run_daemon_with` to serialize its probe-remove-bind sequence against
/// concurrent `daemon serve` starts (PRD #93 auditor BLOCKER — two daemons
/// probing a stale socket would otherwise both `remove_file` and both `bind`,
/// clobbering each other's clients).
pub async fn acquire_spawn_lock(path: &Path) -> std::io::Result<SpawnLock> {
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

/// Try once to take the exclusive `flock(2)` on `path` (opened or created as
/// [`acquire_spawn_lock`] opens it), without waiting: `Ok(None)` when another
/// open file description holds it. Neither the open (`O_NONBLOCK`, which
/// changes nothing for a regular file) nor the lock (`LOCK_NB`) waits for a
/// holder, so a caller that gives up leaves nothing behind
/// ([`super::acquire_spawn_lock_within`]).
pub fn try_acquire_spawn_lock(path: &Path) -> std::io::Result<Option<SpawnLock>> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    // SAFETY: as in `acquire_spawn_lock`; `LOCK_NB` makes it return at once.
    let res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if res != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(SpawnLock { file }))
}
