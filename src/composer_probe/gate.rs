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
//!   A job that doesn't come through acceptance at all, the deck's own
//!   line, is submitted to the writer straight, and a closed fence doesn't
//!   count it: the writer refuses to start it while the fence is closed
//!   (Q35-20, CX-H), so it writes nothing past a check.
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
//! **Segments (Q35-20).** A probe's arm section runs under this lock
//! ([`Gate::arm_section`]), so a person's op is accepted either before it,
//! taking the probe's queued job back, or after it, held behind the fence it
//! closed. A segment's release goes as far as the last ground point before
//! the next held candidate ([`Gate::release_until`]), and a trigger through
//! the next cut ([`Gate::release_cut`]).
//!
//! **The lock.** One mutex, a leaf: nothing calls out of it but the
//! closures [`Gate::admit`] is given, which classify against and move the
//! epoch (`R6`, the top rank), and the arm section's, which takes the seam's
//! lock and the epoch's. Acceptance and release run under the pane
//! writer's own lock, so they are serialised with each other; retirement and
//! the status read take only this one. It is a plain mutex, not one
//! [`crate::lock_rank`] ranks: its order (below the pane writer's lock,
//! above the seam's and `R6`) is written in `agent_pty_segments.rs`'s lock
//! order, and no debug build checks it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::doorbell_txn::{ClaimState, JobClaim};

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
    /// Fenced, and the caller's `hold` refused it under the gate's lock
    /// (Q16-1): handed back, not accepted, nothing held.
    Refused(T),
}

/// One held op as [`Gate::release_until`] reads it (Q35-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measure {
    /// The last offset in the op, from 0 to its length, at which the input
    /// is in ground before any candidate in it; `None` when there is none.
    pub ground: Option<usize>,
    /// A Ctrl+G candidate starts in it, or was begun by an earlier op and
    /// ends in it: the release stops here.
    pub candidate: bool,
}

/// A fence's counts, for the `e2e` build's status read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub fenced: bool,
    pub held_ops: usize,
    pub held_bytes: usize,
    pub full_waiters: usize,
    /// Since when something has been held without a break (Q35-20's
    /// display timers); `None` while nothing is.
    pub held_since: Option<std::time::Instant>,
}

#[derive(Debug)]
struct Fence<T> {
    /// The output offset it closed at: a reply passes ahead only for a query
    /// noted after it.
    trigger: u64,
    held: VecDeque<(T, usize)>,
    held_bytes: usize,
    /// When its held queue last went from empty to holding.
    since: Option<std::time::Instant>,
    /// A probe's arm section closed it (Q35-20): the segments' release
    /// driver owns its opening. A fence closed any other way isn't the
    /// driver's to open.
    segmented: bool,
}

#[derive(Debug)]
struct Inner<T> {
    fence: Option<Fence<T>>,
    retired: bool,
    /// A probe's job queued and not yet at its arm section (Q35-20).
    probe: Option<Arc<JobClaim>>,
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
    /// PRD #1666 part 3 (Q35-20e): told whenever what a card shows of this
    /// gate may have changed: a hold starts, a full op starts waiting, or
    /// anything a waiter waits for moves. Set once, by the registry that
    /// publishes the generation ([`Self::watch_display`]).
    display: std::sync::OnceLock<Arc<Notify>>,
}

impl<T> Default for Gate<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                fence: None,
                retired: false,
                probe: None,
                waiters: VecDeque::new(),
                next_waiter: 0,
                version: 0,
            }),
            changed: Notify::new(),
            display: std::sync::OnceLock::new(),
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
        self.display_changed();
    }

    /// PRD #1666 part 3 (Q35-20e): have `notify` told whenever what a card
    /// shows of this gate may have changed. The first call wins: a
    /// generation is published by one registry.
    pub fn watch_display(&self, notify: Arc<Notify>) {
        let _ = self.display.set(notify);
    }

    /// Tell the display watcher, if one is set. `notify_one` keeps a permit,
    /// so a change made while the watcher is busy is seen on its next wait.
    fn display_changed(&self) {
        if let Some(notify) = self.display.get() {
            notify.notify_one();
        }
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
            since: None,
            segmented: false,
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
    /// doesn't run for it. It never answers [`Admission::Refused`]: an op that
    /// may be refused at its acceptance is offered by [`Self::admit_unless`].
    pub fn admit(
        self: &Arc<Self>,
        op: T,
        len: usize,
        waiter: Option<Waiter<T>>,
        passes: impl FnOnce(&T, u64) -> bool,
        hold: impl FnOnce(&mut T),
    ) -> Admission<T> {
        self.admit_unless(op, len, waiter, passes, |op| {
            hold(op);
            true
        })
    }

    /// [`Self::admit`], with `hold` also able to refuse the op (Q16-1):
    /// `false` hands it back [`Admission::Refused`], in which case `hold` must
    /// have changed nothing. `hold` runs under the gate's lock, so what it
    /// decides holds for the fence as it stands when the op is taken: no arm
    /// section can close or reuse the fence in between.
    pub fn admit_unless(
        self: &Arc<Self>,
        mut op: T,
        len: usize,
        waiter: Option<Waiter<T>>,
        passes: impl FnOnce(&T, u64) -> bool,
        hold: impl FnOnce(&mut T) -> bool,
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
        if inner.fence.is_none() {
            // R5 §4.2 (Q35-20): a person's op accepted while a probe's job
            // is still queued takes it back, by the claim's swap, before the
            // op is written; the arm section, which takes this lock, then
            // finds it taken back and publishes nothing.
            if len > 0
                && let Some(claim) = inner.probe.take()
            {
                claim.withdraw();
            }
            drop(inner);
            drop(waiter);
            return Admission::Open(op);
        }
        let Some(fence) = inner.fence.as_mut() else {
            unreachable!("checked above");
        };
        let fits = fence.held.is_empty()
            || (fence.held.len() < FENCE_OPS_CAP && fence.held_bytes + len <= FENCE_CAP);
        if !fits {
            let waiter = waiter.unwrap_or_else(|| self.wait_in(&mut inner));
            return Admission::Full(op, waiter);
        }
        if !hold(&mut op) {
            drop(inner);
            drop(waiter);
            return Admission::Refused(op);
        }
        let starts = fence.held.is_empty();
        if starts {
            fence.since = Some(std::time::Instant::now());
        }
        fence.held_bytes += len;
        fence.held.push_back((op, len));
        drop(inner);
        if starts {
            self.display_changed();
        }
        drop(waiter);
        Admission::Held
    }

    fn wait_in(self: &Arc<Self>, inner: &mut Inner<T>) -> Waiter<T> {
        let id = inner.next_waiter;
        inner.next_waiter = inner.next_waiter.wrapping_add(1);
        inner.waiters.push_back(id);
        self.display_changed();
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

    /// Release what a released or ended segment lets go (Q35-20, core §4.6):
    /// the held ops, in acceptance order, as far as the last point before
    /// the next held Ctrl+G candidate at which the input is in ground, so no
    /// byte of the candidate, nor of a sequence that may still become one,
    /// goes ahead of its segment. `measure` reads each held op in order (it
    /// carries the input's state from one to the next) until one holds a
    /// candidate; `split(op, n)` takes an op's first `n` bytes off as a
    /// sub-op, leaving the rest held first. With no candidate held and every
    /// op taken, the fence opens when `open`, which the caller passes only
    /// when no segment is outstanding; otherwise it stays closed, holding the
    /// rest. Both closures run under the gate's lock and must take no lock.
    /// Empty when no fence was closed.
    ///
    /// It asks no authority: production releases through
    /// [`Self::release_until_if`] with the segments' check, and this
    /// always-allowed form is for callers that model no segments (the gate's
    /// own tests).
    pub fn release_until(
        &self,
        measure: impl FnMut(&T) -> Measure,
        split: impl FnMut(&mut T, usize) -> T,
        open: bool,
    ) -> Vec<T> {
        self.release_until_if(|| true, measure, split, open)
    }

    /// [`Self::release_until`] under `authority`, asked first, under the
    /// gate's lock, whether the segments still allow the release
    /// (R-B-U2-02).
    ///
    /// **The linearization point is that read.** A successful authority read
    /// commits the release logically: what it releases is decided by the
    /// held set as it stands at the read, and the gate's lock, held from
    /// before the read until the ops are taken, keeps every acceptance and
    /// retirement from changing that set in between. A fact that withdraws
    /// the release (an unknown wait's cut-off, a retirement of the input) is
    /// published under R6, which the read also takes, so it is ordered
    /// either before the read, which then refuses and nothing is taken, or
    /// after it, and so after the logical release. The ops are physically
    /// taken after the read, and R6 is not held through that; a withdrawal
    /// published in that window is still ordered after the release, which is
    /// what makes taking them correct, not the physical timing.
    ///
    /// `authority` may take R6, which the arm section takes under this lock
    /// too; nothing takes this lock under R6.
    pub fn release_until_if(
        &self,
        authority: impl FnOnce() -> bool,
        mut measure: impl FnMut(&T) -> Measure,
        mut split: impl FnMut(&mut T, usize) -> T,
        open: bool,
    ) -> Vec<T> {
        let mut inner = self.lock();
        let Some(fence) = inner.fence.as_mut() else {
            return Vec::new();
        };
        if !authority() {
            return Vec::new();
        }
        // The last ground point before the first candidate: (op, offset).
        let mut stop = (0usize, 0usize);
        let mut candidate = false;
        for (i, (op, _)) in fence.held.iter().enumerate() {
            let m = measure(op);
            if let Some(ground) = m.ground {
                stop = (i, ground);
            }
            if m.candidate {
                candidate = true;
                break;
            }
        }
        let (whole, part) = match stop {
            // With no segment outstanding and no candidate held, nothing is
            // left to hold for: an unfinished sequence at the end (a lone
            // Escape) goes too.
            _ if open && !candidate => (fence.held.len(), 0),
            (i, off) if fence.held.get(i).is_some_and(|(_, len)| off >= *len) => (i + 1, 0),
            (i, off) => (i, off),
        };
        let mut out = Vec::new();
        for _ in 0..whole {
            if let Some((op, len)) = fence.held.pop_front() {
                fence.held_bytes -= len;
                out.push(op);
            }
        }
        if part > 0
            && let Some((op, len)) = fence.held.front_mut()
        {
            let prefix = split(op, part);
            *len -= part;
            fence.held_bytes -= part;
            out.push(prefix);
        }
        if open && !candidate && fence.held.is_empty() {
            inner.fence = None;
        }
        if !out.is_empty() || inner.fence.is_none() {
            self.moved(&mut inner);
        }
        out
    }

    /// Whether a probe's arm section closed the fence that is closed now
    /// (Q35-20): only such a fence is the segments' to open.
    pub fn segment_fenced(&self) -> bool {
        self.lock().fence.as_ref().is_some_and(|f| f.segmented)
    }

    /// Whether a fence is closed with nothing held behind it.
    pub fn fenced_empty(&self) -> bool {
        self.lock()
            .fence
            .as_ref()
            .is_some_and(|f| f.held.is_empty())
    }

    /// Whether any held op holds a Ctrl+G candidate (Q35-20), `cut` saying
    /// whether an op does.
    pub fn holds_any(&self, cut: impl Fn(&T) -> bool) -> bool {
        self.lock()
            .fence
            .as_ref()
            .is_some_and(|f| f.held.iter().any(|(op, _)| cut(op)))
    }

    /// Open the fence when nothing is held behind it (Q35-20: the last
    /// segment ended with no input waiting). Whether it opened.
    pub fn open_if_empty(&self) -> bool {
        let mut inner = self.lock();
        if inner.fence.as_ref().is_some_and(|f| f.held.is_empty()) {
            inner.fence = None;
            self.moved(&mut inner);
            return true;
        }
        false
    }

    /// The probe's arm section (Q35-20, O-20.2): under the gate's lock, so
    /// that a person's op is either accepted before it, finding the probe's
    /// job still queued and taking it back ([`Self::admit`]), or after it,
    /// finding the fence closed and being held. `section` runs only while
    /// the gate isn't retired and the job wasn't taken back, with the gate's
    /// counts as they stand under the lock; on its `Ok` the
    /// fence closes at the output offset `trigger` reads then, which is
    /// before the probe's byte. `section` may take the seam's lock and the
    /// epoch's (R6) under this one; nothing takes this lock under either.
    pub fn arm_section<R>(
        &self,
        claim: &Arc<JobClaim>,
        retired: R,
        taken_back: R,
        section: impl FnOnce(Status) -> Result<(), R>,
        trigger: impl FnOnce() -> u64,
    ) -> Result<(), R> {
        let mut inner = self.lock();
        let pending = inner.probe.take();
        if inner.retired {
            return Err(retired);
        }
        if claim.state() != ClaimState::Queued
            || pending.as_ref().is_some_and(|p| !Arc::ptr_eq(p, claim))
        {
            inner.probe = pending;
            return Err(taken_back);
        }
        let status = Status {
            fenced: inner.fence.is_some(),
            held_ops: inner.fence.as_ref().map_or(0, |f| f.held.len()),
            held_bytes: inner.fence.as_ref().map_or(0, |f| f.held_bytes),
            full_waiters: inner.waiters.len(),
            held_since: None,
        };
        section(status)?;
        let trigger = trigger();
        inner
            .fence
            .get_or_insert(Fence {
                trigger,
                held: VecDeque::new(),
                held_bytes: 0,
                since: None,
                segmented: true,
            })
            .segmented = true;
        self.moved(&mut inner);
        Ok(())
    }

    /// A probe's job was queued under `claim` (Q35-20, R5 §4.2): until its
    /// arm section runs, a person's op the gate accepts with no fence closed
    /// takes it back first.
    pub fn probe_queued(&self, claim: Arc<JobClaim>) {
        self.lock().probe = Some(claim);
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
            if fence.held.is_empty() {
                fence.since = Some(std::time::Instant::now());
            }
            fence.held_bytes += len;
            fence.held.push_back((op, len));
            self.moved(&mut inner);
            return Ok(Vec::new());
        }
        let fence = inner.fence.insert(Fence {
            trigger,
            held: VecDeque::from([(op, len)]),
            held_bytes: len,
            since: Some(std::time::Instant::now()),
            segmented: false,
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
        self.retire_counted()
            .map_or_else(Vec::new, |(held, _)| held)
    }

    /// [`Self::retire`], with the bytes of what was held (Q35-20e: the
    /// successor's card says typing was not delivered). `None` when the gate
    /// had already retired: this call retired nothing, so it has nothing to
    /// report, not "held nothing" (R-E-U2-01).
    pub fn retire_counted(&self) -> Option<(Vec<T>, usize)> {
        let mut inner = self.lock();
        if inner.retired {
            return None;
        }
        inner.retired = true;
        inner.probe = None;
        let (held, bytes) = inner.fence.take().map_or((Vec::new(), 0), |fence| {
            let bytes = fence.held_bytes;
            (fence.held.into_iter().map(|(op, _)| op).collect(), bytes)
        });
        self.moved(&mut inner);
        Some((held, bytes))
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
            held_since: inner
                .fence
                .as_ref()
                .filter(|f| !f.held.is_empty())
                .and_then(|f| f.since),
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
