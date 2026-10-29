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
//!   (`super::epoch::GenerationInput::pass_ahead`), decided and accepted in
//!   one hold of the epoch's lock, so the query it answers and its exemption
//!   can't change between the two. It is written at once,
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
//! **Cuts (Q35-16, core §4.3).** The planner reads each op at acceptance
//! and, while a segment is open or a liability is out, cuts it after its
//! first Ctrl+G candidate. A release then goes only as far as the first cut
//! ([`Gate::release_cut`]): the prefix through it is written, and the
//! candidate opens the next segment, the fence staying closed behind it with
//! the rest held in order. An op the gate lets through with no fence, cut
//! because a liability is out, closes one at its cut ([`Gate::cut_open`]).
//! Each cut makes one more sub-op, held in the queue's place of the op, so
//! sub-ops count against [`FENCE_OPS_CAP`] as the op did, and a frame full of
//! candidates is split one cut at a time.
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

/// What a release does with one held op (Q35-16, core §4.3), as its caller's
/// `split` decides from the cut the planner made at its acceptance.
#[derive(Debug)]
pub enum Cutting<T> {
    /// No cut: it goes, and the release goes on to the next op.
    Whole,
    /// Its cut is at its last byte: it goes whole, and its Ctrl+G candidate
    /// opens a new segment, behind which everything after it stays held.
    Through,
    /// Its prefix through its cut goes, as a sub-op of this many bytes; the
    /// op, now its rest, stays first in the queue behind the segment its
    /// candidate opens.
    Split(T, usize),
}

/// What [`Gate::admit`] did with an op.
#[derive(Debug)]
pub enum Admission<T> {
    /// No fence: the caller writes it, awaiting the PTY, as before. (A
    /// retired gate answers [`Self::Retired`] instead, fence or none.)
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
    /// handed back before. `passes(op, trigger)` decides whether it is a
    /// reply that passes ahead of a fence closed at `trigger`, and accepts it
    /// when it is, so the caller submits it with nothing left to decide;
    /// `hold(op)` runs just before the gate takes it, to move the epoch, plan
    /// the op's cuts into it (Q35-16) and count it in flight. Both run under
    /// the gate's lock. A zero-byte op the gate settles isn't held, so `hold`
    /// doesn't run for it.
    pub fn admit(
        self: &Arc<Self>,
        mut op: T,
        len: usize,
        waiter: Option<Waiter<T>>,
        passes: impl FnOnce(&T, u64) -> bool,
        hold: impl FnOnce(&mut T),
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
        hold(&mut op);
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

    /// Open the fence as far as the first cut (Q35-16, core §4.3, §4.6): what
    /// it held, in acceptance order, up to and including the first op `split`
    /// cuts, of which only the prefix through its cut comes back. The
    /// candidate there opens the next segment: the fence stays closed, its
    /// trigger moved to `trigger`, holding the rest in order. With no cut the
    /// fence opens, as [`Self::release`] opens it. `split` runs under the
    /// gate's lock and must take no lock. Empty when no fence was closed, or
    /// once retired.
    pub fn release_cut(&self, trigger: u64, split: impl FnMut(&mut T) -> Cutting<T>) -> Vec<T> {
        let mut inner = self.lock();
        let Some(fence) = inner.fence.as_mut() else {
            return Vec::new();
        };
        let (out, cut) = Self::through_cut(fence, split);
        if cut {
            fence.trigger = trigger;
        } else {
            inner.fence = None;
        }
        self.moved(&mut inner);
        out
    }

    /// An op the gate let through with no fence closed, whose planner cut it
    /// (Q35-16, core §4.3): a liability is out, so its Ctrl+G candidate opens
    /// a segment. The fence closes at `trigger`, holding the op, and opens as
    /// far as its cut ([`Self::release_cut`]): the prefix through the cut
    /// comes back for the caller to submit, and the rest stays held. If a
    /// fence was closed since the op was let through, the op joins it, held
    /// whole, and nothing comes back. `Err(op)` once retired.
    pub fn cut_open(
        &self,
        op: T,
        len: usize,
        trigger: u64,
        split: impl FnMut(&mut T) -> Cutting<T>,
    ) -> Result<Vec<T>, T> {
        let mut inner = self.lock();
        if inner.retired {
            return Err(op);
        }
        if let Some(fence) = inner.fence.as_mut() {
            fence.held_bytes += len;
            fence.held.push_back((op, len));
            self.moved(&mut inner);
            return Ok(Vec::new());
        }
        let fence = inner.fence.insert(Fence {
            trigger,
            held: VecDeque::from([(op, len)]),
            held_bytes: len,
        });
        let (out, cut) = Self::through_cut(fence, split);
        if !cut {
            inner.fence = None;
        }
        self.moved(&mut inner);
        Ok(out)
    }

    /// Take `fence`'s held ops, in order, through the first one `split`
    /// cuts; whether one did.
    fn through_cut(
        fence: &mut Fence<T>,
        mut split: impl FnMut(&mut T) -> Cutting<T>,
    ) -> (Vec<T>, bool) {
        let mut out = Vec::new();
        while let Some((mut op, len)) = fence.held.pop_front() {
            match split(&mut op) {
                Cutting::Whole => {
                    fence.held_bytes -= len;
                    out.push(op);
                }
                Cutting::Through => {
                    fence.held_bytes -= len;
                    out.push(op);
                    return (out, true);
                }
                Cutting::Split(prefix, taken) => {
                    let taken = taken.min(len);
                    fence.held_bytes -= taken;
                    out.push(prefix);
                    fence.held.push_front((op, len - taken));
                    return (out, true);
                }
            }
        }
        (out, false)
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
