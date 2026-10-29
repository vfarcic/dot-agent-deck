//! PRD #1666 part 3 (Q35-15; R5 §4, R6 §5, core §8.1–§8.3): the fence
//! gate, the per-generation stage in front of the PTY writer that a closed
//! fence holds a person's input in.
//!
//! **States.** `Open`: nothing is held, and an op is written as it was
//! before this stage existed, its caller awaiting the PTY. `Fenced`: from a
//! trigger at an output offset until a release, an accepted op is held in
//! acceptance order and its caller reads on. `Retired`: the generation left
//! its pane; nothing is accepted, and what was held is handed back once to be
//! refused.
//!
//! **Acceptance is a handoff, not a wait** ([`Gate::admit`]). While fenced, an
//! op is one of:
//! - **pass-ahead** (I-5): a person's op that is one whole D3′-exempt reply
//!   to a query noted after the fence's trigger
//!   (`super::epoch::GenerationInput::passes_ahead`). It is written at once,
//!   ahead of what is held. It never enters the held queue, doesn't count
//!   against [`FENCE_OPS_CAP`] or [`FENCE_CAP`], doesn't move the epoch (it is
//!   exempt) and counts in the writer's in-flight count, as any write does.
//!   The same holds for every job that doesn't come through acceptance at
//!   all: the deck's own writes are submitted to the writer straight, and a
//!   closed fence neither holds nor counts them (Q35-20 brings the deck's
//!   lines under it).
//! - **settled**: a zero-byte op has nothing to order against the PTY; it
//!   settles at acceptance and adds no node (R6 §5.1).
//! - **held**: it fits, so the gate owns it, ticket and all, from now until
//!   a release submits it or retirement refuses it. Its epoch move happens
//!   here, before it is visible in the queue, and not again at release.
//! - **full**: holding it would pass [`FENCE_OPS_CAP`] ops or [`FENCE_CAP`]
//!   bytes (one op larger than the cap is held alone in an empty queue). The
//!   op is handed back with a [`Waiter`] in a FIFO; its caller stops reading
//!   and waits for the gate to change, then tries again, keeping its place.
//!
//! **Fairness (core §8.2).** While any waiter is queued, an op that isn't the
//! head waiter's joins the queue rather than taking freed space, fenced or
//! open; a waiter that goes away unlinks itself and lets the next try.
//! Pass-ahead and zero-byte ops take no space and wait for nobody.
//!
//! **Three facts, kept apart (C1 rev 9 I-4).** An op's *physical* outcome
//! (written in full or in part, failed, taken back unstarted, or refused
//! without a byte written) is reported once, through its ticket. A *logical*
//! cancellation is its caller giving up: a held op has no caller to give up
//! (no withdraw guard is armed for it), and a full op's caller still owns its
//! ticket. *Retirement* is the gate's own state: it stops acceptance and hands
//! back what is held, and it ends nothing already submitted: a released job
//! that has started stays in flight until the writer reports it.
//!
//! **The lock.** One mutex, a leaf: nothing calls out of it but the two
//! closures [`Gate::admit`] is given, which classify against and move the
//! epoch (`R6`, the top rank). Acceptance and release run under the pane
//! writer's own lock, so they are serialised with each other; retirement and
//! the status read take only this one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

/// The most bytes a fence holds, unless one op alone is larger (R6 §5.1).
pub const FENCE_CAP: usize = 1024 * 1024;

/// The most ops a fence holds (R6 §5.1).
pub const FENCE_OPS_CAP: usize = 1024;

/// What [`Gate::admit`] did with an op.
#[derive(Debug)]
pub enum Admission<T> {
    /// No fence: the caller writes it, awaiting the PTY, as before.
    Open(T),
    /// Fenced, and a reply that passes ahead: the caller writes it now.
    PassAhead(T),
    /// Fenced, and the gate owns it until a release or retirement.
    Held,
    /// Fenced, and it carries no byte: settled at acceptance.
    Settled(T),
    /// No room: handed back, with the caller's place in the queue.
    Full(T, Waiter<T>),
    /// The generation retired.
    Retired(T),
}

/// A fence's counts, for the `e2e` build's status read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub fenced: bool,
    pub held_ops: usize,
    pub held_bytes: usize,
    pub full_waiters: usize,
}

#[derive(Debug)]
struct Fence<T> {
    /// The output offset it closed at: a reply passes ahead only for a query
    /// noted after it.
    trigger: u64,
    held: VecDeque<(T, usize)>,
    held_bytes: usize,
}

#[derive(Debug)]
struct Inner<T> {
    fence: Option<Fence<T>>,
    retired: bool,
    /// Full waiters, oldest first.
    waiters: VecDeque<u64>,
    next_waiter: u64,
    /// Moves on every change a waiter may be waiting for.
    version: u64,
}

/// One generation's fence gate. See the module documentation.
#[derive(Debug)]
pub struct Gate<T> {
    inner: Mutex<Inner<T>>,
    changed: Notify,
}

impl<T> Default for Gate<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                fence: None,
                retired: false,
                waiters: VecDeque::new(),
                next_waiter: 0,
                version: 0,
            }),
            changed: Notify::new(),
        }
    }
}

impl<T> Gate<T> {
    fn lock(&self) -> MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn moved(&self, inner: &mut Inner<T>) {
        inner.version = inner.version.wrapping_add(1);
        self.changed.notify_waiters();
    }

    /// Close the fence at output offset `trigger`. `false` once retired; a
    /// fence already closed keeps its own trigger.
    pub fn fence(&self, trigger: u64) -> bool {
        let mut inner = self.lock();
        if inner.retired {
            return false;
        }
        inner.fence.get_or_insert(Fence {
            trigger,
            held: VecDeque::new(),
            held_bytes: 0,
        });
        true
    }

    /// Offer `op`, `len` bytes long, from the holder of `waiter` if it was
    /// handed back before. `passes(op, trigger)` says whether it is a reply
    /// that may pass ahead of a fence closed at `trigger`; `hold(op)` runs
    /// just before the gate takes it, to move the epoch and count it in
    /// flight. Both run under the gate's lock. A zero-byte op the gate
    /// settles isn't held, so `hold` doesn't run for it.
    pub fn admit(
        self: &Arc<Self>,
        op: T,
        len: usize,
        waiter: Option<Waiter<T>>,
        passes: impl FnOnce(&T, u64) -> bool,
        hold: impl FnOnce(&T),
    ) -> Admission<T> {
        let mut inner = self.lock();
        if inner.retired {
            return Admission::Retired(op);
        }
        if let Some(fence) = &inner.fence {
            if passes(&op, fence.trigger) {
                return Admission::PassAhead(op);
            }
            if len == 0 {
                return Admission::Settled(op);
            }
        }
        let id = waiter.as_ref().map(Waiter::id);
        if inner.waiters.front().is_some_and(|&head| Some(head) != id) {
            let waiter = waiter.unwrap_or_else(|| self.wait_in(&mut inner));
            return Admission::Full(op, waiter);
        }
        let Some(fence) = inner.fence.as_mut() else {
            drop(inner);
            drop(waiter);
            return Admission::Open(op);
        };
        let fits = fence.held.is_empty()
            || (fence.held.len() < FENCE_OPS_CAP && fence.held_bytes + len <= FENCE_CAP);
        if !fits {
            let waiter = waiter.unwrap_or_else(|| self.wait_in(&mut inner));
            return Admission::Full(op, waiter);
        }
        hold(&op);
        fence.held_bytes += len;
        fence.held.push_back((op, len));
        drop(inner);
        drop(waiter);
        Admission::Held
    }

    fn wait_in(self: &Arc<Self>, inner: &mut Inner<T>) -> Waiter<T> {
        let id = inner.next_waiter;
        inner.next_waiter = inner.next_waiter.wrapping_add(1);
        inner.waiters.push_back(id);
        Waiter {
            gate: Arc::clone(self),
            id,
            seen: inner.version,
        }
    }

    /// Open the fence: what it held, in acceptance order, for the caller to
    /// submit before anything else can be accepted. Empty when no fence was
    /// closed, or once retired.
    pub fn release(&self) -> Vec<T> {
        let mut inner = self.lock();
        let held = match inner.fence.take() {
            Some(fence) => fence.held.into_iter().map(|(op, _)| op).collect(),
            None => return Vec::new(),
        };
        self.moved(&mut inner);
        held
    }

    /// The generation retired: from now on nothing is accepted, and what was
    /// held comes back, once, to be refused. Waiters are woken to see it.
    pub fn retire(&self) -> Vec<T> {
        let mut inner = self.lock();
        if inner.retired {
            return Vec::new();
        }
        inner.retired = true;
        let held = inner
            .fence
            .take()
            .map(|fence| fence.held.into_iter().map(|(op, _)| op).collect())
            .unwrap_or_default();
        self.moved(&mut inner);
        held
    }

    pub fn retired(&self) -> bool {
        self.lock().retired
    }

    pub fn status(&self) -> Status {
        let inner = self.lock();
        Status {
            fenced: inner.fence.is_some(),
            held_ops: inner.fence.as_ref().map_or(0, |f| f.held.len()),
            held_bytes: inner.fence.as_ref().map_or(0, |f| f.held_bytes),
            full_waiters: inner.waiters.len(),
        }
    }
}

/// A full op's place in its gate's FIFO. Dropped, it leaves the queue and
/// lets the next waiter try.
#[derive(Debug)]
pub struct Waiter<T> {
    gate: Arc<Gate<T>>,
    id: u64,
    /// The gate's version when this waiter last looked.
    seen: u64,
}

impl<T> Waiter<T> {
    fn id(&self) -> u64 {
        self.id
    }

    /// Wait until the gate has changed since this waiter last looked: space
    /// freed, the fence opened, another waiter left, or retirement. The
    /// caller then offers its op again.
    pub async fn changed(&mut self) {
        loop {
            let notified = self.gate.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let version = self.gate.lock().version;
            if version != self.seen {
                self.seen = version;
                return;
            }
            notified.await;
        }
    }
}

impl<T> Drop for Waiter<T> {
    fn drop(&mut self) {
        let mut inner = self.gate.lock();
        if let Some(at) = inner.waiters.iter().position(|&w| w == self.id) {
            inner.waiters.remove(at);
            self.gate.moved(&mut inner);
        }
    }
}
