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
    /// The lock was not obtained before the wait ran out. This says nothing
    /// about who had it: on Unix the wait samples the lock between pauses, so
    /// several short holders taking turns, or a wait whose attempt could not
    /// run in time, end here as well as one holder that kept it throughout.
    Busy,
    /// It could not be taken for another reason: the lock file could not be
    /// opened or created (a directory at its path, say), or locking it failed.
    Failed(std::io::Error),
}

impl std::fmt::Display for LockWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockWaitError::Busy => f.write_str("could not get the lock in time"),
            LockWaitError::Failed(error) => error.fmt(f),
        }
    }
}

/// Shortest and longest pause between two attempts of
/// [`acquire_spawn_lock_within`]; the pause doubles from the first to the
/// second.
#[cfg(unix)]
const LOCK_RETRY_FIRST_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);
#[cfg(unix)]
const LOCK_RETRY_LONGEST_PAUSE: std::time::Duration = std::time::Duration::from_millis(200);

/// [`acquire_spawn_lock`], waiting at most `wait` for it, and telling a lock
/// not obtained in time ([`LockWaitError::Busy`]) from a lock that could not
/// be taken at all ([`LockWaitError::Failed`]).
///
/// On Unix it tries without waiting for a holder
/// ([`unix::try_acquire_spawn_lock`], each attempt a blocking-pool job) and
/// sleeps between attempts, so giving up leaves no `flock` waiting for the
/// lock. That is the point of it: [`acquire_spawn_lock`] under a
/// `tokio::time::timeout` stops the caller waiting but leaves its blocking
/// `flock` holding a blocking-pool thread until the holder lets go. The wait
/// answers by `wait` even when an attempt cannot run in time (auditor N7):
/// an attempt still queued then is cancelled, one that starts after it does
/// not try, and one that takes the lock after it lets go at once rather than
/// being handed back. An attempt already inside the `open` cannot be
/// cancelled, since `O_NONBLOCK` does not bound a slow filesystem; it lets go
/// of what it took when it returns, and only the caller stops waiting for it.
///
/// **Polling is not fair** (auditor N8). A waiter sleeping between attempts
/// is not queued in the kernel, so a newcomer can take the lock while it
/// sleeps, and steady short holders can make it miss every release and run
/// out. Callers live with that: worktree creation goes on without the lock
/// after its 60 s wait, the attach rebind binds without it, and a reclaim or
/// an abandoned-spawn removal keeps the worktree.
///
/// Off Unix it is that timeout over [`acquire_spawn_lock`], whose named
/// mutex owner thread is left waiting for the mutex when the wait runs out;
/// the no-waiter-left property above is Unix's alone.
pub async fn acquire_spawn_lock_within(
    path: &Path,
    wait: std::time::Duration,
) -> Result<SpawnLock, LockWaitError> {
    #[cfg(unix)]
    {
        acquire_within_using(
            path,
            ClockDeadline::after(wait),
            unix::try_acquire_spawn_lock,
        )
        .await
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

/// When a bounded wait runs out, readable both from the wait itself and from
/// a blocking-pool attempt. The wait uses [`ClockDeadline`]; a test stands in
/// one it moves itself, so its setup decides when the wait runs out rather than
/// the scheduler (auditor N10).
#[cfg(unix)]
trait WaitDeadline: Clone + Send + Sync + 'static {
    /// Whether the wait has run out.
    fn passed(&self) -> bool;
    /// Resolves once the wait has run out.
    fn reached(&self) -> impl std::future::Future<Output = ()> + Send + 'static;
}

/// The deadline of a real wait: a point on the clock, or none for a wait too
/// long to represent, which then never runs out.
#[cfg(unix)]
#[derive(Clone, Copy)]
struct ClockDeadline(Option<std::time::Instant>);

#[cfg(unix)]
impl ClockDeadline {
    fn after(wait: std::time::Duration) -> Self {
        Self(std::time::Instant::now().checked_add(wait))
    }
}

#[cfg(unix)]
impl WaitDeadline for ClockDeadline {
    fn passed(&self) -> bool {
        self.0.is_some_and(|at| std::time::Instant::now() >= at)
    }

    fn reached(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let at = self.0;
        async move {
            match at {
                Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                None => std::future::pending().await,
            }
        }
    }
}

/// The Unix wait of [`acquire_spawn_lock_within`], with the single attempt
/// and the deadline passed in so a test can stand in for them.
#[cfg(unix)]
async fn acquire_within_using<F, D>(
    path: &Path,
    deadline: D,
    attempt: F,
) -> Result<SpawnLock, LockWaitError>
where
    F: Fn(&Path) -> std::io::Result<Option<SpawnLock>> + Clone + Send + 'static,
    D: WaitDeadline,
{
    let reached = deadline.reached();
    tokio::pin!(reached);
    let mut pause = LOCK_RETRY_FIRST_PAUSE;
    loop {
        let attempt_path = path.to_path_buf();
        let try_once = attempt.clone();
        let expired = deadline.clone();
        let mut job = tokio::task::spawn_blocking(move || {
            if expired.passed() {
                return Ok(None);
            }
            // A lock taken after the deadline is dropped here, which lets go
            // of it, rather than handed to a caller that has stopped waiting.
            Ok(try_once(&attempt_path)?.filter(|_| !expired.passed()))
        });
        let cancel = job.abort_handle();
        // `biased`: an attempt that has finished is read before the deadline,
        // as `tokio::time::timeout` reads its future before its timer.
        let joined = tokio::select! {
            biased;
            joined = &mut job => joined,
            () = &mut reached => {
                // Cancels the attempt if it is still queued; one already
                // running finishes, and its lock, if any, is dropped with its
                // unclaimed result.
                cancel.abort();
                return Err(LockWaitError::Busy);
            }
        };
        match joined {
            Ok(Ok(Some(lock))) if !deadline.passed() => return Ok(lock),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(LockWaitError::Failed(error)),
            Err(join) => return Err(LockWaitError::Failed(std::io::Error::other(join))),
        }
        if deadline.passed() {
            return Err(LockWaitError::Busy);
        }
        tokio::select! {
            () = &mut reached => return Err(LockWaitError::Busy),
            () = tokio::time::sleep(pause) => {}
        }
        pause = (pause * 2).min(LOCK_RETRY_LONGEST_PAUSE);
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

    /// A runtime with one blocking thread, for the waits below.
    #[cfg(unix)]
    fn one_blocking_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
    }

    /// Whether nothing holds the lock at `path`: this thread takes it at
    /// once, and lets go again.
    #[cfg(unix)]
    fn lock_is_free(path: &Path) -> bool {
        matches!(unix::try_acquire_spawn_lock(path), Ok(Some(_)))
    }

    /// Auditor N7: the wait is bounded even when its attempt cannot run. On a
    /// runtime with ONE blocking thread, kept busy by an unrelated job that
    /// waits for the test, a bounded wait for a FREE lock answers within its
    /// budget, before that job is let go; and once it is, the attempt the wait
    /// gave up on leaves the lock free.
    #[cfg(unix)]
    #[test]
    fn a_wait_whose_attempt_is_still_queued_answers_within_its_budget() {
        let rt = one_blocking_thread();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("worktree.lock");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let unrelated = rt.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            let _ = gate_rx.recv();
        });
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the unrelated job holds the blocking thread");
        let began = std::time::Instant::now();
        let wait = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                acquire_spawn_lock_within(&path, Duration::from_millis(50)),
            )
            .await
        });
        let took = began.elapsed();
        // Let go before asserting, so a failure cannot leave the runtime's
        // only blocking thread waiting at shutdown.
        drop(gate_tx);
        let _ = rt.block_on(unrelated);
        rt.shutdown_timeout(Duration::from_secs(10));
        assert!(
            matches!(wait, Ok(Err(LockWaitError::Busy))),
            "answered {:?} after {took:?}",
            wait.as_ref().map(|r| r.as_ref().map(|_| "the lock"))
        );
        assert!(took < Duration::from_secs(1), "answered after {took:?}");
        assert!(
            lock_is_free(&path),
            "the abandoned attempt left the lock held"
        );
    }

    /// A deadline the test moves itself (auditor N10): it runs out when the
    /// test says so, after the setup it is waiting for has been seen to
    /// happen, so the scheduler cannot spend the budget before it does.
    #[cfg(unix)]
    #[derive(Clone)]
    struct TestDeadline(tokio::sync::watch::Receiver<bool>);

    #[cfg(unix)]
    impl TestDeadline {
        /// The deadline, and the sender that makes it run out.
        fn new() -> (tokio::sync::watch::Sender<bool>, Self) {
            let (expire, passed) = tokio::sync::watch::channel(false);
            (expire, Self(passed))
        }
    }

    #[cfg(unix)]
    impl WaitDeadline for TestDeadline {
        fn passed(&self) -> bool {
            *self.0.borrow()
        }

        fn reached(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
            let mut passed = self.0.clone();
            async move {
                // A dropped sender leaves the deadline where it is; a test
                // always sends before it lets go.
                if passed.wait_for(|passed| *passed).await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    /// Auditor N7: an attempt that takes the lock only after the wait has run
    /// out is not handed back, and does not keep it. The attempt here waits
    /// for the test before taking a free lock. The test runs the wait out only
    /// once the attempt has started (auditor N10), and the wait answers `Busy`
    /// while the attempt is still held; once it has taken the lock and
    /// finished, the lock is free again.
    #[cfg(unix)]
    #[test]
    fn an_attempt_that_succeeds_after_the_deadline_is_not_returned_and_lets_go() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let rt = one_blocking_thread();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("worktree.lock");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let entered_tx = Arc::new(Mutex::new(entered_tx));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let gate_rx = Arc::new(Mutex::new(gate_rx));
        let took_it = Arc::new(AtomicBool::new(false));
        let attempt = {
            let took_it = Arc::clone(&took_it);
            move |path: &Path| {
                let _ = entered_tx.lock().unwrap().send(());
                let _ = gate_rx.lock().unwrap().recv();
                let taken = unix::try_acquire_spawn_lock(path)?;
                took_it.store(taken.is_some(), Ordering::SeqCst);
                Ok(taken)
            }
        };
        let (expire, deadline) = TestDeadline::new();
        let wait_path = path.clone();
        let wait =
            rt.spawn(async move { acquire_within_using(&wait_path, deadline, attempt).await });
        // The deadline runs out only once the attempt is inside it, however
        // long the blocking thread took to start it.
        let entered = entered_rx.recv_timeout(Duration::from_secs(30));
        expire.send_replace(true);
        // The attempt is still held at the gate, so the wait has to answer
        // without it. The watchdog is generous: it only bounds a wait that
        // never answers.
        let answer =
            rt.block_on(async { tokio::time::timeout(Duration::from_secs(30), wait).await });
        // Let go before asserting, so a failure cannot leave the runtime's
        // only blocking thread waiting at shutdown.
        drop(gate_tx);
        rt.shutdown_timeout(Duration::from_secs(30));
        assert!(entered.is_ok(), "the attempt never started");
        let answer = match answer {
            Ok(Ok(answer)) => answer,
            Ok(Err(join)) => panic!("the wait failed: {join}"),
            Err(_) => panic!("the wait did not answer while its attempt was held"),
        };
        assert!(
            matches!(answer, Err(LockWaitError::Busy)),
            "answered {:?}",
            answer.as_ref().map(|_| "the lock")
        );
        assert!(
            took_it.load(Ordering::SeqCst),
            "the attempt took the lock, late"
        );
        assert!(lock_is_free(&path), "a late success kept the lock");
    }

    /// Auditor N8: polling promises no turn. Every attempt here meets a
    /// DIFFERENT holder, the previous one having let go in between, and the
    /// wait still runs out; its error says it could not get the lock in time,
    /// not that one holder kept it for the whole wait. The test runs the wait
    /// out only after the second holder has taken its turn (auditor N10).
    #[cfg(unix)]
    #[test]
    fn holders_that_take_turns_can_outlast_a_wait_which_says_only_that_it_ran_out() {
        use std::sync::{Arc, Mutex};
        let rt = one_blocking_thread();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("worktree.lock");
        let holders: Arc<Mutex<(Option<std::fs::File>, usize)>> = Arc::new(Mutex::new((None, 0)));
        let (turn_tx, turn_rx) = std::sync::mpsc::channel::<usize>();
        let turn_tx = Arc::new(Mutex::new(turn_tx));
        let attempt = {
            let holders = Arc::clone(&holders);
            move |path: &Path| {
                let mut turn = holders.lock().unwrap();
                // The last holder lets go and a fresh one takes the lock,
                // between this attempt and the one before it.
                turn.0 = None;
                let next = std::fs::OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .truncate(false)
                    .open(path)?;
                next.lock()?;
                turn.0 = Some(next);
                turn.1 += 1;
                let taken = unix::try_acquire_spawn_lock(path);
                let _ = turn_tx.lock().unwrap().send(turn.1);
                taken
            }
        };
        let (expire, deadline) = TestDeadline::new();
        let wait_path = path.clone();
        let wait =
            rt.spawn(async move { acquire_within_using(&wait_path, deadline, attempt).await });
        // Two attempts have met two holders before the deadline runs out,
        // however long the scheduler took to run them.
        let second_turn = loop {
            match turn_rx.recv_timeout(Duration::from_secs(30)) {
                Ok(turn) if turn >= 2 => break true,
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        expire.send_replace(true);
        let answer =
            rt.block_on(async { tokio::time::timeout(Duration::from_secs(30), wait).await });
        // An attempt already running when the deadline ran out finishes
        // first, so the holder it leaves is the one let go of here.
        rt.shutdown_timeout(Duration::from_secs(30));
        let (last, turns) = {
            let mut turn = holders.lock().unwrap();
            (turn.0.take(), turn.1)
        };
        drop(last);
        assert!(second_turn, "{turns} holder(s): no turnover was exercised");
        let error = match answer {
            Ok(Ok(Err(error))) => error,
            Ok(Ok(Ok(_))) => panic!("took a lock that every attempt found held"),
            Ok(Err(join)) => panic!("the wait failed: {join}"),
            Err(_) => panic!("the wait did not answer once it had run out"),
        };
        assert!(matches!(error, LockWaitError::Busy), "{error:?}");
        assert!(turns >= 2, "{turns} holder(s): no turnover was exercised");
        let said = error.to_string();
        assert_eq!(said, "could not get the lock in time");
        assert!(lock_is_free(&path));
    }
}
