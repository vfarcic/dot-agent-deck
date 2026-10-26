//! PRD #1666 part 3, round 5 (plan D6): the daemon's ranked runtime locks.
//!
//! Five `std::sync::Mutex`es are taken in a fixed order, outermost first:
//!
//! | rank | lock |
//! | --- | --- |
//! | [`R1`] | the registry's delegation tracker |
//! | [`R2`] | the registry's dispatch return routes |
//! | [`R3`] | the inbox hub's state |
//! | [`R4`] | the registry's record table (`inner`) |
//! | [`R5`] | the retained-report store |
//! | [`LEAF`] | the registry's dispatched units, taken with none of the above held |
//!
//! A thread holding rank *n* may take only a rank above *n*. `AppState`'s
//! `tokio` `RwLock` (R0) is outermost by convention and is **not** seen here:
//! a thread-local stack is not sound for a guard held across an `.await`, which
//! R0's callers do. What keeps R0 outermost is that no ranked guard is held
//! across an `.await` (`clippy.toml`'s `await-holding-invalid-types` names
//! [`RankedGuard`]) and the audits recorded with the commit that introduced
//! this module. A green checker is no evidence about R0 or any other lock.
//!
//! # Five ways to take a ranked lock
//!
//! Each existing acquisition site keeps the poison policy it had before it
//! was routed through here (plan D15 rule 1), by using the form equal to it:
//!
//! - [`RankedMutex::lock`] panics on poison, as `.lock().unwrap()` does;
//! - [`RankedMutex::lock_recovering`] continues on the poisoned state, as
//!   `unwrap_or_else(PoisonError::into_inner)` does;
//! - [`RankedMutex::try_lock`] never panics: it answers [`Unavailable`] on
//!   poison or a rank violation, writes nothing through a poisoned guard and
//!   never clears poison. It **blocks** until the lock is free, like
//!   [`RankedMutex::lock`]; "try" names the fallible answer, not a
//!   non-blocking acquisition;
//! - [`RankedMutex::try_lock_now`] is the non-blocking one: it answers
//!   [`NotNow::WouldBlock`] at once while another thread holds the lock, and
//!   otherwise behaves as `try_lock` does, except that its caller picks the
//!   poison policy ([`OnPoison`]). It is for a reader that must not wait,
//!   such as the doorbell's admission check while it holds a pane's writer;
//! - [`RankedMutex::lock_in_drop`] is for a destructor, and for a helper only
//!   a destructor calls. It never panics and never asserts, recovers or skips
//!   poison as its caller asks, and makes no `tracing` call while the thread is
//!   panicking: a skip during an unwind is counted in [`SKIPPED_IN_UNWIND`],
//!   which the next ordinary acquisition on any thread reports and resets.
//!
//! # The checker
//!
//! In a debug build every acquisition checks the ranks the thread already
//! holds **before** it calls `Mutex::lock`, so a re-entrant acquisition is a
//! reported violation instead of a deadlock. [`RankedMutex::lock`] and
//! [`RankedMutex::lock_recovering`] panic on a violation; the other two forms
//! answer it. A guard removes its own entry from the stack by the token its
//! acquisition pushed, through `LocalKey::try_with` and
//! `RefCell::try_borrow_mut`, and does nothing if it cannot reach the stack:
//! its `Drop` asserts nothing, logs nothing and calls nothing that can panic.
//! The worst a stack it could not reach leaves is a later false violation in a
//! debug build.
//!
//! In a release build there is no stack, a rank violation cannot occur, and
//! only the poison policies apply.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The registry's delegation tracker.
pub const R1: u8 = 1;
/// The registry's dispatch return routes.
pub const R2: u8 = 2;
/// The inbox hub's state.
pub const R3: u8 = 3;
/// The registry's record table.
pub const R4: u8 = 4;
/// The retained-report store.
pub const R5: u8 = 5;
/// The registry's dispatched units: taken with no other ranked lock held, and
/// nothing ranked is taken under it.
pub const LEAF: u8 = u8::MAX;

/// Skips made by [`RankedMutex::lock_in_drop`] while the thread was
/// panicking, not yet reported. The next ordinary acquisition on any thread
/// logs the count at `error!` and resets it.
pub static SKIPPED_IN_UNWIND: AtomicU64 = AtomicU64::new(0);

/// Every skip [`SKIPPED_IN_UNWIND`] ever counted, never reset: a test reads
/// it, since any thread's next acquisition resets the reported count.
#[cfg(test)]
pub static SKIPPED_IN_UNWIND_TOTAL: AtomicU64 = AtomicU64::new(0);

fn count_skip_in_unwind() {
    SKIPPED_IN_UNWIND.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    SKIPPED_IN_UNWIND_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Why [`RankedMutex::try_lock`] or [`RankedMutex::lock_in_drop`] did not
/// hand out a guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// A thread panicked while it held the lock.
    Poisoned,
    /// This thread already holds a rank at or above this one (debug builds
    /// only).
    RankViolation,
}

/// Why [`RankedMutex::try_lock_now`] did not hand out a guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotNow {
    /// Another thread holds the lock.
    WouldBlock,
    /// Poisoned (with [`OnPoison::Skip`]), or a rank violation.
    Unavailable(Unavailable),
}

/// What [`RankedMutex::lock_in_drop`] and [`RankedMutex::try_lock_now`] do
/// with a poisoned lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnPoison {
    /// Continue on the poisoned state, as `PoisonError::into_inner` does.
    Recover,
    /// Do nothing.
    Skip,
}

/// A `std::sync::Mutex` with a rank. See the module documentation.
#[derive(Debug, Default)]
pub struct RankedMutex<const RANK: u8, T> {
    mutex: Mutex<T>,
}

/// A held [`RankedMutex`]. Dereferences to the protected value.
#[must_use = "dropping a guard releases the lock"]
pub struct RankedGuard<'a, const RANK: u8, T> {
    guard: MutexGuard<'a, T>,
    #[cfg(debug_assertions)]
    token: u64,
}

impl<const RANK: u8, T> RankedMutex<RANK, T> {
    pub const fn new(value: T) -> Self {
        Self {
            mutex: Mutex::new(value),
        }
    }

    /// Take the lock; panic if it is poisoned, and (debug builds) on a rank
    /// violation.
    #[track_caller]
    pub fn lock(&self) -> RankedGuard<'_, RANK, T> {
        report_skipped_in_unwind();
        let token = checked_or_panic(RANK);
        match self.mutex.lock() {
            Ok(guard) => RankedGuard::new(guard, token),
            Err(_) => panic!("ranked lock {RANK} is poisoned: a thread panicked while it held it"),
        }
    }

    /// Take the lock, continuing on its state if it is poisoned; panic
    /// (debug builds) on a rank violation.
    #[track_caller]
    pub fn lock_recovering(&self) -> RankedGuard<'_, RANK, T> {
        report_skipped_in_unwind();
        let token = checked_or_panic(RANK);
        let guard = self
            .mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        RankedGuard::new(guard, token)
    }

    /// Take the lock, or answer why not. Never panics, never writes through a
    /// poisoned guard and never clears poison. Blocks until the lock is free.
    pub fn try_lock(&self) -> Result<RankedGuard<'_, RANK, T>, Unavailable> {
        report_skipped_in_unwind();
        let token = check(RANK).map_err(|()| {
            if !std::thread::panicking() {
                tracing::error!(
                    rank = RANK,
                    held = ?held_ranks(),
                    "lock ranks: an acquisition out of rank order was refused"
                );
            }
            Unavailable::RankViolation
        })?;
        match self.mutex.lock() {
            Ok(guard) => Ok(RankedGuard::new(guard, token)),
            // Dropped unused: dropping a guard never clears poison.
            Err(poisoned) => {
                drop(poisoned);
                Err(Unavailable::Poisoned)
            }
        }
    }

    /// Take the lock if no other thread holds it, without waiting: never
    /// blocks and never panics. A rank violation answers
    /// [`Unavailable::RankViolation`] (debug builds) and a poisoned lock is
    /// recovered or refused as `on_poison` says; poison is never cleared. A
    /// refusal pushes nothing onto the thread's rank stack.
    pub fn try_lock_now(&self, on_poison: OnPoison) -> Result<RankedGuard<'_, RANK, T>, NotNow> {
        report_skipped_in_unwind();
        let token = check(RANK).map_err(|()| {
            if !std::thread::panicking() {
                tracing::error!(
                    rank = RANK,
                    held = ?held_ranks(),
                    "lock ranks: an acquisition out of rank order was refused"
                );
            }
            NotNow::Unavailable(Unavailable::RankViolation)
        })?;
        match self.mutex.try_lock() {
            Ok(guard) => Ok(RankedGuard::new(guard, token)),
            Err(std::sync::TryLockError::WouldBlock) => Err(NotNow::WouldBlock),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => match on_poison {
                OnPoison::Recover => Ok(RankedGuard::new(poisoned.into_inner(), token)),
                OnPoison::Skip => {
                    // Dropped unused: dropping a guard never clears poison.
                    drop(poisoned);
                    Err(NotNow::Unavailable(Unavailable::Poisoned))
                }
            },
        }
    }

    /// Take the lock from a destructor, or from a helper only a destructor
    /// calls. Never panics or asserts; makes no `tracing` call while the
    /// thread is panicking. `None` on a rank violation (debug builds), or on
    /// poison with [`OnPoison::Skip`].
    pub fn lock_in_drop(&self, on_poison: OnPoison) -> Option<RankedGuard<'_, RANK, T>> {
        let token = match check(RANK) {
            Ok(token) => token,
            Err(()) => {
                skipped(RANK, "a rank at or above it is already held");
                return None;
            }
        };
        match self.mutex.lock() {
            Ok(guard) => Some(RankedGuard::new(guard, token)),
            Err(poisoned) => match on_poison {
                OnPoison::Recover => Some(RankedGuard::new(poisoned.into_inner(), token)),
                OnPoison::Skip => {
                    drop(poisoned);
                    if std::thread::panicking() {
                        count_skip_in_unwind();
                    }
                    None
                }
            },
        }
    }

    /// Whether a thread panicked while it held this lock.
    pub fn is_poisoned(&self) -> bool {
        self.mutex.is_poisoned()
    }
}

impl<'a, const RANK: u8, T> RankedGuard<'a, RANK, T> {
    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    fn new(guard: MutexGuard<'a, T>, token: Token) -> Self {
        #[cfg(debug_assertions)]
        {
            push(RANK, token);
            Self { guard, token }
        }
        #[cfg(not(debug_assertions))]
        {
            Self { guard }
        }
    }
}

impl<const RANK: u8, T> Deref for RankedGuard<'_, RANK, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<const RANK: u8, T> DerefMut for RankedGuard<'_, RANK, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<const RANK: u8, T> Drop for RankedGuard<'_, RANK, T> {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        pop(self.token);
    }
}

/// Log, outside an unwind, how many destructor skips an unwind counted.
fn report_skipped_in_unwind() {
    if SKIPPED_IN_UNWIND.load(Ordering::Relaxed) == 0 || std::thread::panicking() {
        return;
    }
    let skipped = SKIPPED_IN_UNWIND.swap(0, Ordering::Relaxed);
    if skipped > 0 {
        tracing::error!(
            skipped,
            "lock ranks: destructors skipped a ranked lock while their thread was panicking; \
             what they would have released stays held until the daemon restarts"
        );
    }
}

/// A destructor's acquisition was skipped.
fn skipped(rank: u8, why: &str) {
    if std::thread::panicking() {
        count_skip_in_unwind();
    } else {
        tracing::error!(rank, why, "lock ranks: a destructor skipped a ranked lock");
    }
}

#[cfg(debug_assertions)]
type Token = u64;
#[cfg(not(debug_assertions))]
type Token = ();

#[cfg(debug_assertions)]
std::thread_local! {
    /// The ranks this thread holds, each with the token its guard removes.
    static HELD: std::cell::RefCell<Vec<(u8, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
    static NEXT_TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Check that `rank` may be taken now, and reserve the token its guard will
/// push. `Err(())` on a violation, or when the stack cannot be read.
#[cfg(debug_assertions)]
fn check(rank: u8) -> Result<Token, ()> {
    HELD.try_with(|held| {
        let held = held.try_borrow().map_err(|_| ())?;
        let allowed = if rank == LEAF {
            held.is_empty()
        } else {
            held.iter().all(|&(h, _)| h < rank)
        };
        if !allowed {
            return Err(());
        }
        Ok(NEXT_TOKEN
            .try_with(|next| {
                let token = next.get();
                next.set(token.wrapping_add(1));
                token
            })
            .unwrap_or(0))
    })
    .unwrap_or(Err(()))
}

#[cfg(not(debug_assertions))]
fn check(_rank: u8) -> Result<Token, ()> {
    Ok(())
}

#[track_caller]
fn checked_or_panic(rank: u8) -> Token {
    match check(rank) {
        Ok(token) => token,
        Err(()) => panic!(
            "lock rank violation: taking rank {rank} while this thread holds {:?}",
            held_ranks()
        ),
    }
}

/// The ranks this thread holds, for a violation's message.
pub fn held_ranks() -> Vec<u8> {
    #[cfg(debug_assertions)]
    {
        HELD.try_with(|held| {
            held.try_borrow()
                .map(|held| held.iter().map(|&(rank, _)| rank).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }
    #[cfg(not(debug_assertions))]
    {
        Vec::new()
    }
}

#[cfg(debug_assertions)]
fn push(rank: u8, token: Token) {
    let _ = HELD.try_with(|held| {
        if let Ok(mut held) = held.try_borrow_mut() {
            held.push((rank, token));
        }
    });
}

/// Remove the entry `token` pushed. Cannot panic: an unreachable stack or a
/// missing entry leaves it as it is.
#[cfg(debug_assertions)]
fn pop(token: Token) {
    let _ = HELD.try_with(|held| {
        if let Ok(mut held) = held.try_borrow_mut()
            && let Some(at) = held.iter().rposition(|&(_, t)| t == token)
        {
            held.remove(at);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spec::spec;

    use super::*;

    fn poison<const RANK: u8, T: Send + 'static>(mutex: &Arc<RankedMutex<RANK, T>>)
    where
        RankedMutex<RANK, T>: Sync + Send,
    {
        let mutex = Arc::clone(mutex);
        let _ = std::thread::spawn(move || {
            let _held = mutex.lock();
            panic!("poison the lock on purpose");
        })
        .join();
    }

    /// Scenario: Take ranked locks inside out on one thread, and the leaf with a ranked guard held. Each `lock()` panics with the held ranks in its message instead of deadlocking, and the stack is consistent again for the thread's next acquisition.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_an_out_of_order_lock_panics_before_it_blocks() {
        let hub = RankedMutex::<R3, u32>::new(0);
        let registry = RankedMutex::<R4, u32>::new(0);
        let leaf = RankedMutex::<LEAF, u32>::new(0);

        let held = registry.lock();
        let violation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _hub = hub.lock();
        }))
        .expect_err("R3 under R4 is a violation");
        let message = violation
            .downcast_ref::<String>()
            .expect("a formatted message");
        assert!(
            message.contains("taking rank 3") && message.contains("[4]"),
            "{message}"
        );
        drop(held);

        let held = hub.lock();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _leaf = leaf.lock_recovering();
            }))
            .is_err(),
            "the leaf with a ranked guard held is a violation"
        );
        // Re-entering the held rank is reported, not a deadlock.
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _again = hub.lock();
            }))
            .is_err()
        );
        drop(held);

        // The order allowed, and the stack is empty afterwards.
        {
            let _hub = hub.lock();
            let _registry = registry.lock();
        }
        assert!(held_ranks().is_empty());
        let _leaf = leaf.lock_recovering();
    }

    /// Scenario: A panic while a guard is held, then a fresh acquisition on the same thread. The unwind removes the guard's stack entry, so the next acquisition of a lower rank is not a violation.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_a_panic_under_a_guard_leaves_the_stack_consistent() {
        let hub = Arc::new(RankedMutex::<R3, u32>::new(0));
        let tracker = RankedMutex::<R1, u32>::new(0);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = hub.lock_recovering();
            panic!("under the guard");
        }));
        assert!(unwound.is_err());
        assert!(held_ranks().is_empty());
        let _tracker = tracker.lock();
        let _hub = hub.lock_recovering();
    }

    /// Scenario: Poison a ranked lock, then take it in each of the four forms. `try_lock` answers `Poisoned` and leaves the data and the poison as they were; `lock_recovering` and `lock_in_drop(Recover)` hand out the recovered guard; `lock_in_drop(Skip)` answers `None`; `lock` panics; and the lock is still poisoned at the end.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_no_form_clears_poison() {
        let mutex = Arc::new(RankedMutex::<R2, u32>::new(7));
        poison(&mutex);
        assert!(mutex.is_poisoned());

        assert_eq!(mutex.try_lock().err(), Some(Unavailable::Poisoned));
        assert!(mutex.is_poisoned());
        assert!(mutex.lock_in_drop(OnPoison::Skip).is_none());
        {
            let mut recovered = mutex.lock_recovering();
            assert_eq!(*recovered, 7);
            *recovered = 8;
        }
        assert_eq!(
            *mutex.lock_in_drop(OnPoison::Recover).expect("recovered"),
            8
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = mutex.lock();
            }))
            .is_err()
        );
        assert!(mutex.is_poisoned(), "no form clears poison");
        assert!(held_ranks().is_empty());
    }

    /// Scenario: Take a ranked lock out of order with the two non-panicking forms. `try_lock` answers `RankViolation` and `lock_in_drop` answers `None`, neither panics, and the data is unchanged.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_the_non_panicking_forms_answer_a_rank_violation() {
        let returns = RankedMutex::<R2, u32>::new(1);
        let registry = RankedMutex::<R4, u32>::new(2);
        let _held = registry.lock();
        assert_eq!(returns.try_lock().err(), Some(Unavailable::RankViolation));
        assert!(returns.lock_in_drop(OnPoison::Recover).is_none());
        assert!(returns.lock_in_drop(OnPoison::Skip).is_none());
        assert_eq!(held_ranks(), vec![R4]);
    }

    /// Scenario: A guard is dropped while its thread's rank stack is already borrowed (a seam that holds the borrow). Its `Drop` does nothing and does not panic; the entry it could not remove is the only trace.
    #[spec("orchestration/work-done/032")]
    #[test]
    // The rank stack exists only with debug assertions; a release-profile
    // test build (the scanner budget measurement) has none to borrow.
    #[cfg(debug_assertions)]
    fn work_done_032_a_guard_drop_with_the_stack_borrowed_does_not_panic() {
        let hub = RankedMutex::<R3, u32>::new(0);
        let guard = hub.lock();
        HELD.with(|held| {
            let _borrowed = held.borrow_mut();
            drop(guard);
        });
        // The entry it could not reach is still there: the documented cost.
        assert_eq!(held_ranks(), vec![R3]);
        HELD.with(|held| held.borrow_mut().clear());
    }

    /// Scenario: Another OS thread holds a ranked lock while this thread asks for it with `try_lock_now`. It answers `WouldBlock` at once instead of waiting, leaves nothing on this thread's rank stack, and hands out the guard once the holder lets go.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_try_lock_now_does_not_wait_for_another_thread() {
        let mutex = Arc::new(RankedMutex::<R3, u32>::new(5));
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let mutex = Arc::clone(&mutex);
            std::thread::spawn(move || {
                let _guard = mutex.lock();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
            })
        };
        held_rx.recv().unwrap();
        let asked = std::time::Instant::now();
        assert_eq!(
            mutex.try_lock_now(OnPoison::Recover).err(),
            Some(NotNow::WouldBlock)
        );
        assert_eq!(
            mutex.try_lock_now(OnPoison::Skip).err(),
            Some(NotNow::WouldBlock)
        );
        assert!(
            asked.elapsed() < std::time::Duration::from_secs(1),
            "answered without waiting: {:?}",
            asked.elapsed()
        );
        assert!(held_ranks().is_empty(), "a refusal pushes no rank");
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let guard = mutex.try_lock_now(OnPoison::Skip).expect("free now");
        assert_eq!(*guard, 5);
        assert_eq!(held_ranks(), vec![R3]);
        drop(guard);
        assert!(held_ranks().is_empty());
    }

    /// Scenario: Poison a ranked lock, then take it with `try_lock_now` under each poison policy, and take one out of rank order. `Recover` hands out the recovered guard, `Skip` answers `Poisoned`, the out-of-order one answers `RankViolation` without panicking, and the lock is still poisoned at the end.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_try_lock_now_answers_poison_and_rank_as_asked() {
        let mutex = Arc::new(RankedMutex::<R2, u32>::new(7));
        poison(&mutex);
        assert_eq!(
            mutex.try_lock_now(OnPoison::Skip).err(),
            Some(NotNow::Unavailable(Unavailable::Poisoned))
        );
        assert!(held_ranks().is_empty());
        assert_eq!(
            *mutex.try_lock_now(OnPoison::Recover).expect("recovered"),
            7
        );
        assert!(mutex.is_poisoned(), "no form clears poison");
        let registry = RankedMutex::<R4, u32>::new(0);
        let _held = registry.lock();
        assert_eq!(
            mutex.try_lock_now(OnPoison::Recover).err(),
            Some(NotNow::Unavailable(Unavailable::RankViolation))
        );
        assert_eq!(held_ranks(), vec![R4]);
    }

    /// A subscriber that panics on any event, so a `tracing` call made where
    /// none may be is a second panic the test sees.
    struct PanicOnEvent;

    impl tracing::Subscriber for PanicOnEvent {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            panic!(
                "a tracing event during an unwind: {:?}",
                event.metadata().name()
            );
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct SkipsInDrop<'a> {
        violated: &'a RankedMutex<R2, u32>,
        poisoned: &'a RankedMutex<R5, u32>,
    }

    impl Drop for SkipsInDrop<'_> {
        fn drop(&mut self) {
            assert!(self.violated.lock_in_drop(OnPoison::Recover).is_none());
            assert!(self.poisoned.lock_in_drop(OnPoison::Skip).is_none());
        }
    }

    /// Scenario: A destructor runs during an unwind with a higher rank held and a poisoned lock, under a tracing subscriber that panics on any event. `lock_in_drop` skips both without a tracing call, counts both in `SKIPPED_IN_UNWIND`, and the next ordinary acquisition reports and resets the count.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_lock_in_drop_during_an_unwind_counts_and_does_not_log() {
        let returns = RankedMutex::<R2, u32>::new(0);
        let registry = RankedMutex::<R4, u32>::new(0);
        let poisoned = Arc::new(RankedMutex::<R5, u32>::new(0));
        poison(&poisoned);
        let before = SKIPPED_IN_UNWIND.load(Ordering::Relaxed);

        let capture = crate::test_isolation::capture_tracing_on_this_thread(PanicOnEvent);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = registry.lock();
            let _armed = SkipsInDrop {
                violated: &returns,
                poisoned: &poisoned,
            };
            std::panic::panic_any("injected-unwind-3b9e");
        }));
        drop(capture);
        let payload = unwound.expect_err("the injected panic");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"injected-unwind-3b9e"),
            "the injected payload, not a second panic from a tracing call"
        );
        assert!(SKIPPED_IN_UNWIND.load(Ordering::Relaxed) >= before + 2);
        let _ordinary = returns.lock();
        assert_eq!(
            SKIPPED_IN_UNWIND.load(Ordering::Relaxed),
            0,
            "the next ordinary acquisition reports and resets the count"
        );
    }
}
