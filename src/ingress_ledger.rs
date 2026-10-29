//! PRD #1666 part 3, round 5-wake (plan §2.1′): every human operation on an
//! agent's input, from the moment it arrives to the moment its bytes are done
//! with the PTY.
//!
//! The doorbell must know whether anyone else's bytes can reach the agent's
//! input while it types its own line. A count of frames in flight could not
//! tell "nothing arrived" from "a frame arrived and was written" (the ABA
//! case), and it stopped counting a frame when its caller stopped waiting,
//! while the PTY job carrying its bytes could still be running. So each
//! operation takes a **ticket** from its generation's ledger when it arrives,
//! before any await, and the ticket leaves the outstanding set exactly once,
//! when it is settled:
//!
//! - until the operation hands its bytes to a PTY job, a caller-side
//!   [`TicketGuard`] owns it: dropping the guard settles `Cancelled`, and a
//!   refusal settles `Refused`;
//! - from the moment the job is queued, the job owns it ([`JobTicket`]): the
//!   writer thread settles it when the job ends (`Written`, `Partial`,
//!   `Failed`), or it settles `Cancelled` exactly when the job is taken back
//!   before it started. A job that started and outlives its caller keeps its
//!   ticket outstanding until it ends.
//!
//! Two counters are read by the doorbell's admission. The **watermark** moves
//! with every ticket admitted, so a watermark unchanged since admission means
//! no human operation arrived at all. The **job mark** moves when a ticket's
//! ownership moves into a job, in the same hold of the ledger as the job is
//! queued, so a job mark unchanged since admission means no human byte was
//! queued for the PTY. The deck's own writes take no ticket and move neither.
//!
//! The ledger belongs to one generation (agent id). When the generation is
//! retired its ledger is closed: what was outstanding leaves the outstanding
//! set and is recorded `Retired`, and nothing in a successor's ledger moves.
//!
//! The history ([`IngressLedger::settled`]) is a log, not the outstanding
//! set: it records each settlement as it happens, so a ticket retired while
//! outstanding is recorded twice, `Retired` and then however its guard or
//! job ended, and a ticket admitted to a closed ledger, never outstanding,
//! is recorded once, when it ends.
//!
//! **Physical outcomes, exactly once (PRD #1666 part 3, Q35-15, I-4).** Every
//! ticket is made by [`IngressLedger::admit`] and reports one *physical*
//! outcome, once: written in full or in part, failed, refused, cancelled
//! before a byte could be written (its guard dropped, or its job taken back
//! before it started), or dropped unrun with the writer's queue. A guard's
//! paths each disarm it; a job's three (the writer's report, its take-back,
//! its drop unrun) share one flag, so whichever comes first is the only one.
//! `Retired` is not one of them: it is the generation's *logical* event, and
//! a job already queued when its generation retires stays unreported
//! ([`IngressLedger::jobs_unreported`]) until the writer, its take-back or
//! its disposal reports it, while it leaves the outstanding set the doorbell
//! reads.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// How a ticket ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// The PTY took every byte.
    Written { bytes: usize },
    /// The PTY took `accepted` bytes, then the write failed.
    Partial { accepted: usize },
    /// The write failed before any byte was taken.
    Failed(std::io::ErrorKind),
    /// The operation was given up before any of its bytes could be written:
    /// its caller dropped it, or its queued job was taken back.
    Cancelled,
    /// The operation was refused (`exited`, `history-only`, `stale`, …).
    Refused(&'static str),
    /// The generation retired while the ticket was outstanding.
    Retired,
}

#[derive(Debug, Default)]
struct LedgerInner {
    watermark: u64,
    job_mark: u64,
    next_ticket: u64,
    /// Tickets admitted and not yet settled, with whether their job was
    /// queued.
    outstanding: BTreeMap<u64, bool>,
    /// Tickets whose job was queued and hasn't reported its physical outcome,
    /// kept across the close.
    unreported: std::collections::BTreeSet<u64>,
    closed: bool,
    /// The most recent settlements, for the log and for tests.
    settled: Vec<(u64, Settlement)>,
}

/// How many settlements a ledger keeps for the log.
const SETTLED_KEPT: usize = 64;

/// One generation's ledger.
#[derive(Debug, Default)]
pub struct IngressLedger {
    inner: Mutex<LedgerInner>,
}

/// A snapshot of a ledger's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerMarks {
    pub watermark: u64,
    pub job_mark: u64,
    pub outstanding: usize,
}

impl IngressLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Admit one human operation: a new ticket, the watermark moved. Called
    /// at the operation's entry, before any await.
    pub fn admit(self: &Arc<Self>) -> TicketGuard {
        let mut inner = self.inner.lock().unwrap();
        inner.next_ticket += 1;
        let ticket = inner.next_ticket;
        inner.watermark += 1;
        if !inner.closed {
            inner.outstanding.insert(ticket, false);
        }
        TicketGuard {
            ledger: Arc::clone(self),
            ticket,
            armed: true,
        }
    }

    pub fn marks(&self) -> LedgerMarks {
        let inner = self.inner.lock().unwrap();
        LedgerMarks {
            watermark: inner.watermark,
            job_mark: inner.job_mark,
            outstanding: inner.outstanding.len(),
        }
    }

    /// The ledger's lock by `try_lock`, for the doorbell's final seam (plan
    /// §2.4): held across the seam's checks and its enqueue, so no human
    /// operation is admitted between the check of the watermark and the
    /// deck's job. `None` when it is contended.
    pub fn try_hold(&self) -> Option<LedgerHold<'_>> {
        self.inner.try_lock().ok().map(|inner| LedgerHold { inner })
    }

    /// Whether any human operation is outstanding.
    pub fn any_outstanding(&self) -> bool {
        !self.inner.lock().unwrap().outstanding.is_empty()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    /// How many queued jobs haven't reported their physical outcome. Unlike
    /// the outstanding set, retirement doesn't empty it.
    pub fn jobs_unreported(&self) -> usize {
        self.inner.lock().unwrap().unreported.len()
    }

    /// The most recent settlements, oldest first: a log in which a ticket
    /// retired while outstanding appears again when it ends (see the module
    /// documentation).
    pub fn settled(&self) -> Vec<(u64, Settlement)> {
        self.inner.lock().unwrap().settled.clone()
    }

    /// Retire the generation: what is outstanding is marked `Retired` and
    /// later settlements change nothing a successor reads.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        inner.closed = true;
        let outstanding: Vec<u64> = inner.outstanding.keys().copied().collect();
        inner.outstanding.clear();
        for ticket in outstanding {
            push_settled(&mut inner, ticket, Settlement::Retired);
        }
    }

    fn settle(&self, ticket: u64, how: Settlement) {
        let mut inner = self.inner.lock().unwrap();
        inner.unreported.remove(&ticket);
        if inner.outstanding.remove(&ticket).is_some() || inner.closed {
            push_settled(&mut inner, ticket, how);
        }
    }

    /// Move `ticket`'s ownership into a job and queue it, in one hold of the
    /// ledger: the job mark and the job's place in the PTY writer's queue are
    /// seen together. `enqueue` returns whether the job was queued; when it
    /// was not, the ticket settles as `Failed(BrokenPipe)` (the writer is
    /// gone) and the job mark does not move. `armed` is set, still under the
    /// hold, once the job is queued: a job ticket dropped before then (inside
    /// a failed `enqueue`) settles nothing itself, since this hold settles it.
    fn queue_with<T>(
        &self,
        ticket: u64,
        armed: &AtomicU8,
        enqueue: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        let mut inner = self.inner.lock().unwrap();
        let queued = enqueue();
        if queued.is_some() {
            // A job the writer already finished stays reported.
            let _ = armed.compare_exchange(UNQUEUED, QUEUED, Ordering::SeqCst, Ordering::SeqCst);
            inner.job_mark += 1;
            inner.unreported.insert(ticket);
            if let Some(job) = inner.outstanding.get_mut(&ticket) {
                *job = true;
            }
        } else if inner.outstanding.remove(&ticket).is_some() || inner.closed {
            push_settled(
                &mut inner,
                ticket,
                Settlement::Failed(std::io::ErrorKind::BrokenPipe),
            );
        }
        queued
    }
}

fn push_settled(inner: &mut LedgerInner, ticket: u64, how: Settlement) {
    inner.settled.push((ticket, how));
    if inner.settled.len() > SETTLED_KEPT {
        let excess = inner.settled.len() - SETTLED_KEPT;
        inner.settled.drain(..excess);
    }
}

/// A hold of a ledger's lock ([`IngressLedger::try_hold`]).
pub struct LedgerHold<'a> {
    inner: std::sync::MutexGuard<'a, LedgerInner>,
}

impl LedgerHold<'_> {
    pub fn marks(&self) -> LedgerMarks {
        LedgerMarks {
            watermark: self.inner.watermark,
            job_mark: self.inner.job_mark,
            outstanding: self.inner.outstanding.len(),
        }
    }
}

/// The caller's ownership of a ticket, until the operation's bytes are handed
/// to a job. Dropping it settles the ticket `Cancelled`.
#[derive(Debug)]
pub struct TicketGuard {
    ledger: Arc<IngressLedger>,
    ticket: u64,
    armed: bool,
}

impl TicketGuard {
    pub fn ticket(&self) -> u64 {
        self.ticket
    }

    /// The operation was refused.
    pub fn refuse(mut self, reason: &'static str) {
        self.armed = false;
        self.ledger.settle(self.ticket, Settlement::Refused(reason));
    }

    /// The operation carried no byte and was accepted: done, with nothing to
    /// write (Q35-15, a zero-byte frame while a fence holds input).
    pub fn settle_empty(mut self) {
        self.armed = false;
        self.ledger
            .settle(self.ticket, Settlement::Written { bytes: 0 });
    }

    /// Hand the ticket to a job: `enqueue` queues the job carrying the
    /// returned [`JobTicket`]; from then on the job settles it. Returns
    /// whatever `enqueue` returned, `None` when the job could not be queued
    /// (the ticket is then settled).
    pub(crate) fn into_job<T>(mut self, enqueue: impl FnOnce(JobTicket) -> Option<T>) -> Option<T> {
        self.armed = false;
        let ledger = Arc::clone(&self.ledger);
        let ticket = self.ticket;
        let armed = Arc::new(AtomicU8::new(UNQUEUED));
        let job = JobTicket {
            ledger: Arc::clone(&ledger),
            ticket,
            armed: Arc::clone(&armed),
        };
        ledger.queue_with(ticket, &armed, move || enqueue(job))
    }
}

impl Drop for TicketGuard {
    fn drop(&mut self) {
        if self.armed {
            self.ledger.settle(self.ticket, Settlement::Cancelled);
        }
    }
}

/// A job's ownership of a ticket. The writer thread settles it when the job
/// ends; a job taken back before it started settles it `Cancelled` through
/// its [`TicketHandle`]; a job dropped unrun (its writer thread gone) settles
/// it `Failed(BrokenPipe)`.
#[derive(Debug)]
pub struct JobTicket {
    ledger: Arc<IngressLedger>,
    ticket: u64,
    /// Set once the job is queued, and cleared when the job settles the
    /// ticket itself; a ticket dropped while armed settles `Failed`.
    /// [`UNQUEUED`], [`QUEUED`] or [`REPORTED`].
    armed: Arc<AtomicU8>,
}

impl JobTicket {
    /// A handle that can settle the ticket `Cancelled` from the caller's side,
    /// at the moment the job is taken back. It shares the job's flag, so the
    /// take-back and the job's own report are one report between them.
    pub(crate) fn handle(&self) -> TicketHandle {
        TicketHandle {
            ledger: Arc::clone(&self.ledger),
            ticket: self.ticket,
            armed: Arc::clone(&self.armed),
        }
    }

    /// Report `how` unless another path of this job already reported.
    fn report(&self, how: Settlement) {
        if self.armed.swap(REPORTED, Ordering::SeqCst) != REPORTED {
            self.ledger.settle(self.ticket, how);
        }
    }

    /// The job ended: `accepted` of `len` bytes taken, `error` the error that
    /// stopped it, if one did.
    pub(crate) fn settle_written(
        self,
        len: usize,
        accepted: usize,
        error: Option<std::io::ErrorKind>,
    ) {
        let how = match error {
            None if accepted == len => Settlement::Written { bytes: accepted },
            Some(kind) if accepted == 0 => Settlement::Failed(kind),
            _ => Settlement::Partial { accepted },
        };
        self.report(how);
    }

    /// The job ended having written nothing on purpose (a voice answer whose
    /// question changed before its keys were reached).
    pub(crate) fn settle_cancelled(self) {
        self.report(Settlement::Cancelled);
    }

    /// The job was refused before it wrote anything: a released op whose
    /// generation retired before the writer reached it (Q35-15, core §8.1).
    pub(crate) fn settle_refused(self, reason: &'static str) {
        self.report(Settlement::Refused(reason));
    }
}

impl Drop for JobTicket {
    /// Unrun with a queued job: the writer is gone. A drop inside a failed
    /// enqueue, still unqueued, reports nothing: the queueing hold does.
    fn drop(&mut self) {
        if self
            .armed
            .compare_exchange(QUEUED, REPORTED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.ledger.settle(
                self.ticket,
                Settlement::Failed(std::io::ErrorKind::BrokenPipe),
            );
        }
    }
}

/// A job ticket not yet in a queue.
const UNQUEUED: u8 = 0;
/// A job ticket whose job is queued and hasn't reported.
const QUEUED: u8 = 1;
/// A job ticket that reported its physical outcome.
const REPORTED: u8 = 2;

/// Settles a queued job's ticket `Cancelled` when the job is taken back
/// before it started — the one case its caller, not the writer thread, ends
/// it. A writer that later drops the taken-back job reports nothing more.
#[derive(Debug, Clone)]
pub(crate) struct TicketHandle {
    ledger: Arc<IngressLedger>,
    ticket: u64,
    armed: Arc<AtomicU8>,
}

impl TicketHandle {
    pub(crate) fn settle_cancelled(&self) {
        if self.armed.swap(REPORTED, Ordering::SeqCst) != REPORTED {
            self.ledger.settle(self.ticket, Settlement::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// Scenario: Queue a ticket's job where the queue refuses it, drop a job's ticket unrun, and settle a ticket after its ledger was closed. The refused queueing settles the ticket as failed without moving the job mark, the unrun job's ticket settles as failed, and a late settlement in a closed ledger leaves nothing outstanding.
    #[spec("inbox/doorbell/035")]
    #[test]
    fn doorbell_035_failed_queueing_unrun_jobs_and_closed_ledgers() {
        let ledger = IngressLedger::new();
        let guard = ledger.admit();
        let ticket = guard.ticket();
        let queued: Option<()> = guard.into_job(|_job| None);
        assert!(queued.is_none());
        assert_eq!(ledger.marks().job_mark, 0);
        assert_eq!(
            ledger.settled().last(),
            Some(&(ticket, Settlement::Failed(std::io::ErrorKind::BrokenPipe)))
        );

        let guard = ledger.admit();
        let ticket = guard.ticket();
        let job = guard.into_job(Some).expect("queued");
        assert_eq!(ledger.marks().job_mark, 1);
        assert_eq!(ledger.marks().outstanding, 1);
        drop(job);
        assert_eq!(ledger.marks().outstanding, 0);
        assert_eq!(
            ledger.settled().last(),
            Some(&(ticket, Settlement::Failed(std::io::ErrorKind::BrokenPipe)))
        );

        let guard = ledger.admit();
        let job = guard.into_job(Some).expect("queued");
        ledger.close();
        job.settle_written(4, 4, None);
        assert_eq!(ledger.marks().outstanding, 0);
        assert!(ledger.is_closed());
        let after_close = ledger.admit();
        assert_eq!(
            ledger.marks().outstanding,
            0,
            "a closed ledger admits nothing outstanding"
        );
        drop(after_close);
    }
}
