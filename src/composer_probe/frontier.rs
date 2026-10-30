//! PRD #1666 part 3 (Q35-20a; amendment 4 §7.2, amendment 5 §3.2,
//! amendment 6 §3): a generation's drain-frontier requests, and the wake that
//! tells its PTY reader one is waiting.
//!
//! A frontier is the output offset `o` the reader takes after a zero-timeout
//! poll of the master finds nothing left to read: every byte written to the
//! slave before that poll and not discarded is behind `o` (P-DRN, A4 §7.1).
//! The reader (`agent_pty`'s Linux reader) takes it; this module holds what
//! the reader and its requesters share:
//! - [`FrontierQueue`], one per generation, kept in its
//!   [`super::epoch::GenerationInput`]: requests waiting for the reader, at
//!   most [`MAX_OUTSTANDING`] unsettled at once and one per record;
//! - [`Request`], whose outcome is set once, `Taken{o}` or `Missing`, by
//!   whichever of the reader's publication and a retirement gets there
//!   first (A6). The one that set it settles it; the other does nothing;
//! - the eventfd [`EventFd`] (Linux) a requester rings and the reader polls.
//!
//! **Fail-closed.** Every path that can't take a frontier answers `Missing`,
//! which holds: a queue no reader attached (macOS, Windows, a reader whose
//! setup failed), a retired wake, a request for another generation, a queue
//! at its cap, and the reader's end. Nothing here makes `Taken` authoritative:
//! under `EcPolicy::Hold` it authorises nothing (A5 §1.4).
//!
//! **Locks.** The queue's mutex is a leaf. It is never taken under the bus
//! lock: the reader snapshots the queue before its zero poll and publishes
//! with no queue lock held, and a requester rings the wake after releasing
//! it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::record::{FrontierOutcome, RecordKey};

/// How many requests can be unsettled at once in one generation: the
/// record cap (A4 §7.2).
pub const MAX_OUTSTANDING: usize = 64;

/// What a requester rings to wake the generation's reader.
pub trait Wake: Send + Sync {
    /// Tell the reader a request is queued. A wake already pending counts as
    /// rung.
    fn ring(&self) -> std::io::Result<()>;
}

/// One frontier request.
#[derive(Debug)]
pub struct Request {
    id: u64,
    generation: u64,
    record: Option<RecordKey>,
    outcome: OnceLock<FrontierOutcome>,
}

impl Request {
    /// Its number in its queue, from 1.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The generation it was asked for.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The record that learns its outcome, if any.
    pub fn record(&self) -> Option<RecordKey> {
        self.record
    }

    /// Its outcome, once set.
    pub fn outcome(&self) -> Option<FrontierOutcome> {
        self.outcome.get().copied()
    }

    /// Set its outcome, if nothing has: `true` when this call set it, so the
    /// caller settles it (A6's compare-and-set).
    pub fn settle(&self, outcome: FrontierOutcome) -> bool {
        self.outcome.set(outcome).is_ok()
    }
}

/// Why a queue stopped taking requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retired {
    /// Its wake failed (A5 §3.2): polled with an error, a read or a ring
    /// that failed other than with `EAGAIN`.
    Wake,
    /// Its reader ended: the generation is retiring.
    Reader,
}

#[derive(Debug, Default)]
struct Shared {
    /// The reader's generation, once one attached.
    generation: Option<u64>,
    retired: Option<Retired>,
    /// Not yet handed to the reader.
    queued: Vec<Arc<Request>>,
    /// Every request handed out and not known settled, queued or in the
    /// reader's pending set: what a retirement settles.
    outstanding: Vec<Arc<Request>>,
    /// The `e2e` build's `frontier-status` answers from here.
    #[cfg(feature = "e2e")]
    by_id: std::collections::HashMap<u64, Arc<Request>>,
}

/// A generation's frontier requests. See the module documentation.
#[derive(Default)]
pub struct FrontierQueue {
    shared: Mutex<Shared>,
    wake: OnceLock<Arc<dyn Wake>>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for FrontierQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontierQueue")
            .field("shared", &self.shared)
            .field("wake", &self.wake.get().is_some())
            .finish()
    }
}

impl FrontierQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A reader for `generation` takes this queue's requests, woken by
    /// `wake`. Called once, before the reader starts; a second call changes
    /// nothing.
    pub fn attach(&self, generation: u64, wake: Arc<dyn Wake>) {
        let mut shared = self.lock();
        if shared.generation.is_none() && self.wake.set(wake).is_ok() {
            shared.generation = Some(generation);
        }
    }

    /// Ask for a frontier for `generation`, for `record` when one learns it.
    /// Returns the request and the requests this call settled `Missing`
    /// (itself when nothing can take it, or every outstanding one when the
    /// ring failed), which the caller settles. A record with an unsettled
    /// request gets that request back, and nothing is queued (A4 §7.2).
    pub fn request(
        &self,
        generation: u64,
        record: Option<RecordKey>,
    ) -> (Arc<Request>, Vec<Arc<Request>>) {
        let mut shared = self.lock();
        if let Some(key) = record
            && let Some(existing) = shared
                .outstanding
                .iter()
                .find(|r| r.record == Some(key) && r.outcome().is_none())
        {
            return (Arc::clone(existing), Vec::new());
        }
        shared.outstanding.retain(|r| r.outcome().is_none());
        let request = Arc::new(Request {
            id: self.next_id.fetch_add(1, Ordering::Relaxed) + 1,
            generation,
            record,
            outcome: OnceLock::new(),
        });
        #[cfg(feature = "e2e")]
        shared.by_id.insert(request.id, Arc::clone(&request));
        let refused = shared.retired.is_some()
            || shared.generation != Some(generation)
            || shared.outstanding.len() >= MAX_OUTSTANDING;
        if refused {
            drop(shared);
            let settled = if request.settle(FrontierOutcome::Missing) {
                vec![Arc::clone(&request)]
            } else {
                Vec::new()
            };
            return (request, settled);
        }
        shared.queued.push(Arc::clone(&request));
        shared.outstanding.push(Arc::clone(&request));
        drop(shared);
        let rung = self.wake.get().map_or(Ok(()), |wake| wake.ring());
        match rung {
            Ok(()) => (request, Vec::new()),
            Err(error) => {
                tracing::warn!(
                    target: "composer_frontier",
                    generation,
                    error = %error,
                    "frontier wake failed; the generation's frontier requests are missing"
                );
                (request, self.retire(Retired::Wake))
            }
        }
    }

    /// The reader's snapshot: every queued request, moved to its pending set.
    pub fn take_queued(&self) -> Vec<Arc<Request>> {
        std::mem::take(&mut self.lock().queued)
    }

    /// Stop taking requests: every outstanding request this call settles
    /// `Missing` is returned for the caller to settle, and every later one
    /// is `Missing` at once. The first cause is kept.
    pub fn retire(&self, why: Retired) -> Vec<Arc<Request>> {
        let mut shared = self.lock();
        shared.retired.get_or_insert(why);
        shared.queued.clear();
        std::mem::take(&mut shared.outstanding)
            .into_iter()
            .filter(|r| r.settle(FrontierOutcome::Missing))
            .collect()
    }

    /// Why it stopped taking requests, if it has.
    pub fn retired(&self) -> Option<Retired> {
        self.lock().retired
    }

    /// The `e2e` build's `frontier-status`: request `id`, if it was asked.
    #[cfg(feature = "e2e")]
    pub fn get(&self, id: u64) -> Option<Arc<Request>> {
        self.lock().by_id.get(&id).cloned()
    }
}

/// A nonblocking, close-on-exec eventfd: the reader's wake (A4 §7.2).
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct EventFd(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl EventFd {
    pub fn new() -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: eventfd(2) takes no pointers; a non-negative return is a
        // descriptor this process now owns.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned to us and nothing else owns it.
        Ok(Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }))
    }

    /// Read the counter back to zero. Nothing pending (`EAGAIN`) counts as
    /// drained.
    pub fn drain(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut value = 0u64;
        // SAFETY: an 8-byte read into a `u64` we own.
        let n = unsafe {
            libc::read(
                self.0.as_raw_fd(),
                std::ptr::from_mut(&mut value).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        if n >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(target_os = "linux")]
impl std::os::fd::AsRawFd for EventFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
impl Wake for EventFd {
    /// Add one to the counter without blocking: `EAGAIN` means a wake is
    /// already pending, which counts as rung (A4 §7.2).
    fn ring(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let value = 1u64;
        // SAFETY: an 8-byte write from a `u64` we own.
        let n = unsafe {
            libc::write(
                self.0.as_raw_fd(),
                std::ptr::from_ref(&value).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        if n >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(())
        } else {
            Err(error)
        }
    }
}
