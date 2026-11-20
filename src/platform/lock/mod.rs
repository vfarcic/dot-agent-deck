//! Spawn-serialization lock (PRD #42 M1, lifted from `daemon_attach.rs`).
//!
//! Serializes concurrent first-attaches / `daemon serve` starts so only one
//! process races the bind. Unix uses an exclusive `flock(2)` on a lock file;
//! Windows (PRD #163 M2) uses a named mutex in the `Global\` namespace, keyed on
//! the current user's SID **and** the lock path — see [`spawn_mutex_name`]. The
//! RAII [`SpawnLock`] guard and the `acquire_spawn_lock(path)` signature are
//! preserved across both backends so callers don't churn.

use std::path::Path;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{SpawnLock, acquire_spawn_lock};
#[cfg(windows)]
pub use windows::{SpawnLock, acquire_spawn_lock};

/// Why [`acquire_spawn_lock_within`] gave up without the lock.
#[derive(Debug)]
pub enum LockWaitError {
    /// Another holder kept it for the whole wait.
    Busy,
    /// It could not be taken for another reason: the lock file could not be
    /// opened or created (a directory at its path, say), or locking it failed.
    Failed(std::io::Error),
}

impl std::fmt::Display for LockWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockWaitError::Busy => f.write_str("another holder kept the lock for the whole wait"),
            LockWaitError::Failed(error) => error.fmt(f),
        }
    }
}

/// Shortest and longest pause between two attempts of
/// [`acquire_spawn_lock_within`]; the pause doubles from the first to the
/// second.
const LOCK_RETRY_FIRST_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);
const LOCK_RETRY_LONGEST_PAUSE: std::time::Duration = std::time::Duration::from_millis(200);

/// [`acquire_spawn_lock`], waiting at most `wait` for another holder to let
/// go, and telling a holder that kept it ([`LockWaitError::Busy`]) from a lock
/// that could not be taken at all ([`LockWaitError::Failed`]).
///
/// On Unix it tries without waiting ([`unix::try_acquire_spawn_lock`], each
/// attempt a short blocking-pool job that returns at once) and sleeps
/// between attempts, so giving up leaves nothing waiting for the lock. That
/// is the point of it: [`acquire_spawn_lock`] under a `tokio::time::timeout`
/// stops the caller waiting but leaves its blocking `flock` holding a
/// blocking-pool thread until the holder lets go. Off Unix it is that
/// timeout over [`acquire_spawn_lock`], whose owner thread is left waiting
/// for the mutex when the wait runs out.
pub async fn acquire_spawn_lock_within(
    path: &Path,
    wait: std::time::Duration,
) -> Result<SpawnLock, LockWaitError> {
    #[cfg(unix)]
    {
        let deadline = tokio::time::Instant::now() + wait;
        let mut pause = LOCK_RETRY_FIRST_PAUSE;
        loop {
            let attempt = path.to_path_buf();
            match tokio::task::spawn_blocking(move || unix::try_acquire_spawn_lock(&attempt)).await
            {
                Ok(Ok(Some(lock))) => return Ok(lock),
                Ok(Ok(None)) => {}
                Ok(Err(error)) => return Err(LockWaitError::Failed(error)),
                Err(join) => return Err(LockWaitError::Failed(std::io::Error::other(join))),
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(LockWaitError::Busy);
            }
            tokio::time::sleep(pause.min(deadline - now)).await;
            pause = (pause * 2).min(LOCK_RETRY_LONGEST_PAUSE);
        }
    }
    #[cfg(not(unix))]
    {
        match tokio::time::timeout(wait, acquire_spawn_lock(path)).await {
            Ok(Ok(lock)) => Ok(lock),
            Ok(Err(error)) => Err(LockWaitError::Failed(error)),
            Err(_) => Err(LockWaitError::Busy),
        }
    }
}

/// Take an exclusive lock on an open file without waiting (PRD #1666 design
/// §3): `Ok(true)` when this handle now holds it, `Ok(false)` when another
/// handle does. A wrapper over std's `File::try_lock`, which the inbox's spill
/// inventory already uses; [`acquire_spawn_lock`] is the blocking lock.
pub fn try_lock_file(file: &std::fs::File) -> std::io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Name of the Windows named mutex that stands in for the Unix `flock(2)` on
/// `lock_path` (PRD #163 M2). `user_token` is
/// [`crate::platform::paths::endpoint_user_suffix`] — the current user's SID.
///
/// Shape: `Global\dot-agent-deck-spawn-{user}-{basename}-{hash}`.
///
/// Three properties, each load-bearing:
///
/// - **`Global\`** — machine-wide, not per-session. Named pipes are not
///   session-scoped, so a daemon in an RDP session and one on the console
///   contend for the *same* endpoint; a `Local\` (per-session) mutex would let
///   both bind-race unserialized. This is also what makes the mutex usable as
///   the singleton-daemon guard (below).
/// - **`{user}` = the SID** — the deck is per-user and the endpoints are
///   per-user, so the serialization must be too: two users starting their own
///   daemons must not block each other. The SID is the non-colliding uid
///   analogue (see `endpoint_user_suffix`); `%USERNAME%` would collide across
///   domains.
/// - **`{basename}-{hash}` = the lock path** — the *deviation from the PRD's
///   literal `Global\dot-agent-deck-spawn-{user}`*, and it is required for
///   correctness. `flock(2)` serializes per lock **file**, and the two callers
///   use two different files: `daemon_attach::ensure_daemon_running` locks
///   `<state_dir>/spawn.lock` and holds it across spawn **and** the poll-for-
///   endpoint loop, while the daemon it spawns locks
///   `daemon::lock_path_for(endpoint)` across its own probe→bind. Collapsing
///   both onto one per-user name would deadlock every Windows lazy-spawn: the
///   launcher waits for an endpoint that the child cannot create until the
///   launcher's mutex is released. Keying on the path reproduces flock's
///   per-file granularity exactly. It also keeps the test isolation the
///   `DOT_AGENT_DECK_STATE_DIR` / `DOT_AGENT_DECK_LOCK_DIR` overrides buy on
///   Unix: a pinned lock dir yields a distinct mutex, so concurrent tests do not
///   cross-serialize.
///
/// **Doubling as the singleton-daemon guard** (PRD mapping table): because the
/// key is derived from the lock path and `daemon::lock_path_for` is a pure
/// function of the endpoint, every `daemon serve` for a given endpoint computes
/// the *same* mutex name and therefore serializes its probe→bind behind this one
/// object — the role the Unix stale-inode dance plays. The loser's probe then
/// sees the live endpoint and returns `AddrInUse` (with `first_pipe_instance`
/// as the kernel-level backstop).
///
/// The hash is FNV-1a over the **lowercased** full path (Windows paths are
/// case-insensitive, so two spellings of one path must map to one mutex) and is
/// deliberately *not* `DefaultHasher`: SipHash keys are not stable across Rust
/// versions, and an older TUI must compute the same name as a newer daemon
/// (CLAUDE.md rule 12). A hash collision would over-serialize two unrelated
/// paths — the safe direction — never under-serialize.
///
/// Compiled on every platform: it is pure data, so the naming rule stays
/// unit-testable on Linux CI where the `#[cfg(windows)]` caller is absent.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn spawn_mutex_name(user_token: &str, lock_path: &Path) -> String {
    let full = lock_path.to_string_lossy().to_lowercase();
    // Split on BOTH separators rather than using `Path::file_name`: on a Linux
    // host (where this function's tests run) `\` is an ordinary character, so
    // `file_name` would hand back the whole `C:\…\spawn.lock` string and the
    // derived name would differ from the Windows one. The name must be pure data,
    // identical everywhere.
    let mut basename: String = full
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        // `\` separates the object-namespace segments and would escape
        // `Global\`; everything outside the safe set collapses to `_`. The
        // basename is only there to make the name legible in Process
        // Explorer — the hash carries the identity.
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    if basename.is_empty() {
        basename.push_str("spawn");
    }
    let hash = fnv1a64(full.as_bytes());
    // Longest realistic name: 7 (`Global\`) + 21 + 45 (SID) + 33 + 17 ≈ 123,
    // comfortably inside the 260-character kernel-object-name limit.
    format!(r"Global\dot-agent-deck-spawn-{user_token}-{basename}-{hash:016x}")
}

/// FNV-1a (64-bit). Fixed constants, so the digest is identical across Rust
/// versions, builds and platforms — the property [`spawn_mutex_name`] needs and
/// `DefaultHasher` does not provide.
#[cfg_attr(not(windows), allow(dead_code))]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    const SID: &str = "S-1-5-21-3623811015-3361044348-30300820-1013";

    /// The contract BOTH backends must satisfy, and the one the callers depend on:
    /// while a guard is alive, a second acquisition of the same lock **blocks**;
    /// dropping the guard releases it and the waiter is granted the lock.
    ///
    /// Deliberately not `cfg`-gated. On Unix it pins the `flock(LOCK_EX)` +
    /// `Drop`-unlocks behavior (previously untested — the only callers are the
    /// daemon-start and lazy-spawn paths). On Windows it is the *live* check of the
    /// PRD #163 M2 named mutex on the `windows-latest` CI job, which runs
    /// `cargo nextest run`: two guards in one process sit on two owner threads, so
    /// this exercises real creation (with its owner-only DACL), the blocking wait,
    /// and the cross-thread release — the part a Linux `cargo check --target`
    /// cannot reach.
    #[tokio::test]
    async fn spawn_lock_is_exclusive_while_held_and_released_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Unique per run, so the derived Windows mutex name is unique too and
        // concurrent test binaries never contend.
        let path = dir.path().join("spawn.lock");

        let first = acquire_spawn_lock(&path).await.expect("first acquire");

        let waiter_path = path.clone();
        let waiter = tokio::spawn(async move { acquire_spawn_lock(&waiter_path).await });
        // Long enough for the waiter to reach the blocking wait. Not a race: while
        // the lock is held it can NEVER complete, so a slow waiter also passes.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !waiter.is_finished(),
            "a second acquire must block while the first guard is alive"
        );

        drop(first);

        let second = tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("dropping the guard must release the lock within 10s")
            .expect("the waiting task must not panic")
            .expect("the waiter must be granted the lock");
        // And the second guard releases cleanly in turn (on Windows this is the
        // ReleaseMutex-on-the-owning-thread path).
        drop(second);
    }

    /// The Windows spawn-mutex name is scoped `Global\`, carries the per-user
    /// SID, and is a namespace-safe token (no stray `\`, bounded length) —
    /// pure data, so the rule is checked on Linux CI too (PRD #163 M2).
    #[test]
    fn spawn_mutex_name_is_global_per_user_and_namespace_safe() {
        let name = spawn_mutex_name(
            SID,
            &PathBuf::from(r"C:\Users\alice\AppData\Local\dot-agent-deck\spawn.lock"),
        );

        assert!(name.starts_with(r"Global\dot-agent-deck-spawn-"), "{name}");
        assert!(name.contains(SID), "{name}");
        // Exactly one separator: the `Global\` prefix. A second one would let
        // the derived segment escape into another namespace.
        assert_eq!(name.matches('\\').count(), 1, "{name}");
        assert!(name.len() <= 260, "{} chars: {name}", name.len());
        // The basename survives for legibility, sanitized and lowercased.
        assert!(name.contains("spawn.lock-"), "{name}");
    }

    /// Distinct lock paths must yield distinct mutexes — this is what keeps
    /// `<state_dir>/spawn.lock` (held across the launcher's poll loop) from
    /// deadlocking against the spawned daemon's per-endpoint lock, and what
    /// preserves per-`DOT_AGENT_DECK_LOCK_DIR` test isolation.
    #[test]
    fn spawn_mutex_name_differs_per_lock_path_and_per_user() {
        let launcher = spawn_mutex_name(SID, &PathBuf::from(r"C:\s\dot-agent-deck\spawn.lock"));
        let daemon = spawn_mutex_name(
            SID,
            &PathBuf::from(r"C:\s\dot-agent-deck\locks\hook-0123456789abcdef.lock"),
        );
        assert_ne!(launcher, daemon);

        // Same path, different user → different mutex (per-user daemons must
        // not block each other).
        let other_user = spawn_mutex_name(
            "S-1-5-21-1-2-3-4",
            &PathBuf::from(r"C:\s\dot-agent-deck\spawn.lock"),
        );
        assert_ne!(launcher, other_user);
    }

    /// Same path, two spellings: Windows paths are case-insensitive, so both
    /// must map onto ONE mutex or the exclusion silently disappears. And the
    /// derivation is deterministic (FNV-1a, not `DefaultHasher`) so an older
    /// TUI and a newer daemon agree on the name.
    #[test]
    fn spawn_mutex_name_is_case_insensitive_and_deterministic() {
        let lower = spawn_mutex_name(SID, &PathBuf::from(r"C:\Users\alice\spawn.lock"));
        let upper = spawn_mutex_name(SID, &PathBuf::from(r"c:\USERS\ALICE\SPAWN.LOCK"));
        assert_eq!(lower, upper);

        assert_eq!(
            lower,
            spawn_mutex_name(SID, &PathBuf::from(r"C:\Users\alice\spawn.lock"))
        );
        // Pinned digest: a change here means older and newer builds would stop
        // agreeing on the mutex name (CLAUDE.md rule 12).
        assert_eq!(fnv1a64(b"dot-agent-deck"), 0x7e10_96f6_8933_bf16);
    }

    /// Auditor N5 / reviewer P2: a bounded wait that runs out leaves nothing
    /// waiting for the lock. On a runtime with ONE blocking thread, with the
    /// lock held by another open file description throughout, three bounded
    /// waits each give up as `Busy`, and a blocking job queued after them
    /// still runs; a directory at the lock's path is `Failed`, not `Busy`;
    /// once the holder lets go the lock is taken. Unix only: off Unix the
    /// wait is a timeout over the named mutex's owner thread.
    #[cfg(unix)]
    #[test]
    fn a_bounded_wait_that_runs_out_leaves_no_waiter_and_tells_busy_from_failed() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("worktree.lock");
        let held = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        held.lock().unwrap();
        let blocked_dir = dir.path().join("a-directory.lock");
        std::fs::create_dir(&blocked_dir).unwrap();
        let (busy, heartbeat, failed) = rt.block_on(async {
            let mut busy = Vec::new();
            for _ in 0..3 {
                let wait = tokio::time::timeout(
                    Duration::from_secs(10),
                    acquire_spawn_lock_within(&path, Duration::from_millis(50)),
                )
                .await;
                busy.push(matches!(wait, Ok(Err(LockWaitError::Busy))));
            }
            let heartbeat =
                tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(|| 7))
                    .await
                    .ok()
                    .and_then(Result::ok);
            let failed = tokio::time::timeout(
                Duration::from_secs(5),
                acquire_spawn_lock_within(&blocked_dir, Duration::from_secs(2)),
            )
            .await;
            let failed = matches!(failed, Ok(Err(LockWaitError::Failed(_))));
            (busy, heartbeat, failed)
        });
        // Released before asserting, so a failure cannot leave the runtime
        // waiting on a blocked `flock` at shutdown.
        drop(held);
        let taken = rt.block_on(acquire_spawn_lock_within(&path, Duration::from_secs(5)));
        rt.shutdown_timeout(Duration::from_secs(10));
        assert_eq!(busy, vec![true; 3]);
        assert_eq!(heartbeat, Some(7), "a blocking job queued after them runs");
        assert!(
            failed,
            "a lock file that cannot be opened is Failed, not Busy"
        );
        assert!(taken.is_ok(), "{:?}", taken.err());
    }
}
