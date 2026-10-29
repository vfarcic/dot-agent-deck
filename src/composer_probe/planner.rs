//! PRD #1666 part 3 (Q35-16; core §4.2–§4.3 and §4.8, A1 §3.3, A2 §4.4 and
//! §5, A3 §5, C1 rev 9 I-6 and I-9): the two framings of a pane's input, and
//! what each decides.
//!
//! **The logical planner** reads every op at acceptance, in acceptance order,
//! with the input epoch's framing (`super::epoch::GenerationInput`), and
//! decides there, once, what each Ctrl+G candidate in it is. That decision is
//! carried with the op's bytes and never made again from what the ledger
//! says later (A2 §5):
//! - while a segment is open, or a liability is out, a candidate **cuts** the
//!   op: the bytes through it go forward, and the rest is held behind the
//!   segment it opens ([`Cut`]). Only the first cut is found at acceptance;
//!   the rest is read lazily, from the planner's state at the cut, when what
//!   follows it is next released ([`next_cut`]), so a frame full of
//!   candidates costs one node at a time, not one per candidate;
//! - otherwise it is **native**: written as typed, numbered at acceptance,
//!   and the number published before the op reaches the writer ([`Natives`]);
//! - a BEL ending an OSC that is one whole colour reply to a query the ledger
//!   keeps is no candidate at all (D3′, recognised after saturation too).
//!
//! With no liability and no segment, which is every production generation
//! before activation, nothing is ever cut: the planner only numbers.
//!
//! **The physical tracker** ([`Physical`]) reads what the PTY took, one
//! `write(2)` return at a time, on the writer thread, and answers where the
//! written stream is ([`Boundary`], `physical_boundary` in core §12.1): in
//! ground, outside any paste, string or escape sequence; right after a lone
//! ESC; or inside one. A deck line starts only in ground or after a lone ESC
//! (the Escape key: see [`Boundary::Escape`]). It also reports each
//! native candidate to the hazard FIFO as the PTY takes it, by replaying the
//! planner's reading of that op ([`NativeRun`]), not by classifying it again.
//!
//! **The ground wake** ([`Physical::until_line`] for a deck line,
//! [`Physical::until_ground`] for ground itself; I-9) is check, register,
//! recheck, await: the [`Notified`](tokio::sync::futures::Notified) future is
//! pinned and enabled before the recheck, so a return to ground that lands
//! after the recheck and before the first poll still wakes it.
//!
//! Neither tracker is permission for anything: the tracker's agreement with
//! `vt100` on paste mode is this repository's own reading, not how xterm,
//! xterm.js or an agent's input reader treats the bytes (P-PASTE is
//! unmeasured), and this commit lets paste mode suppress no candidate.

use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::Notify;

use super::framing::{CandidateForm, Framing, Token};

/// Whether `token` is a Ctrl+G candidate, given whether its op was
/// classified at acceptance as one whole colour reply to a query the ledger
/// keeps (D3′): that op's ending BEL is no candidate, every other form
/// everywhere is (core §4.4's safe default).
pub fn is_candidate(token: &Token<'_>, solicited_reply: bool) -> bool {
    token
        .candidate()
        .is_some_and(|c| !(solicited_reply && c.form == CandidateForm::BelEndsOsc))
}

/// Where the planner cut an op: right after the candidate whose last byte is
/// at `at - 1`, with the planner's state there, from which the rest of the op
/// is read when it is next released.
#[derive(Debug, Clone)]
pub struct Cut {
    pub at: usize,
    pub resume: Framing,
}

/// The first cut in `bytes`, read from `start`, a state after a cut: every
/// candidate in the rest of a cut op cuts again (the op's whole-reply
/// classification was made for the op, and its rest is never one).
pub fn next_cut(start: &Framing, bytes: &[u8]) -> Option<Cut> {
    first_cut(start, bytes, false)
}

/// The first cut in `bytes` read from `start`, with `solicited_reply` the
/// op's classification at acceptance.
pub fn first_cut(start: &Framing, bytes: &[u8], solicited_reply: bool) -> Option<Cut> {
    let mut framing = start.clone();
    for (i, &byte) in bytes.iter().enumerate() {
        let mut cut = false;
        framing.feed(byte, |token| cut |= is_candidate(&token, solicited_reply));
        if cut {
            return Some(Cut {
                at: i + 1,
                resume: framing,
            });
        }
    }
    None
}

/// The native candidates one op carried, numbered at acceptance from
/// `first` to `last`, with the planner's state at the op's start: what the
/// writer thread replays to report each one to the hazard FIFO as the PTY
/// takes it (A1 §3.3, A2 §4.4).
#[derive(Debug, Clone)]
pub struct Natives {
    pub first: u64,
    pub last: u64,
    pub start: Framing,
}

impl Natives {
    /// The run the writer feeds this op's written bytes to.
    pub fn run(self) -> NativeRun {
        NativeRun {
            next: (self.first <= self.last).then_some(self.first),
            last: self.last,
            framing: self.start,
        }
    }
}

/// [`Natives`] being written: the planner's reading of the op, replayed over
/// what the PTY takes.
#[derive(Debug, Clone)]
pub struct NativeRun {
    /// The next candidate's number, `None` once `last` was reported.
    next: Option<u64>,
    last: u64,
    framing: Framing,
}

impl NativeRun {
    /// The PTY took `written`, the op's next bytes: each candidate they
    /// complete, in order, by its number. A candidate past `last` (none,
    /// unless the planner's numbering ran out) isn't reported.
    pub fn written(&mut self, written: &[u8], mut taken: impl FnMut(u64)) {
        for &byte in written {
            let mut completed = 0usize;
            self.framing.feed(byte, |token| {
                completed += usize::from(is_candidate(&token, false))
            });
            for _ in 0..completed {
                if let Some(seq) = self.next {
                    taken(seq);
                    self.next = seq.checked_add(1).filter(|&n| n <= self.last);
                }
            }
        }
    }
}

/// Where [`Physical::until_pausing`] has got to, for a test that moves the
/// tracker between its steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeAt {
    /// The first check found the stream short of what was waited for;
    /// nothing is registered.
    Checked,
    /// The waiter is registered (enabled); the recheck is next.
    Registered,
    /// The recheck found the stream still short; the first poll is next.
    Rechecked,
}

/// Where the bytes the PTY took leave a pane's input (`physical_boundary`,
/// core §12.1), from least to most settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Boundary {
    /// Inside an escape sequence, a string or a paste.
    Inside,
    /// Right after a lone ESC: the Escape key, which an agent's reader takes
    /// as such once its own wait for a following byte runs out, or the start
    /// of a sequence the user hasn't finished. Not ground, so it admits no
    /// probe (core §4.8). A deck line may start here, as one could before
    /// Q35-16: users press Escape alone to interrupt an agent, and holding
    /// every ring until their next key would make the doorbell wait after
    /// each interrupt, not rarely (`inbox/doorbell/019`).
    Escape,
    /// In ground, outside any paste, string or escape sequence.
    Ground,
}

impl Boundary {
    /// Where `framing`'s stream ends.
    pub fn of(framing: &Framing) -> Self {
        if framing.at_ground() {
            Self::Ground
        } else if framing.escape_pending() {
            Self::Escape
        } else {
            Self::Inside
        }
    }

    /// Whether one of the deck's own lines may start here (core §4.8, with
    /// the lone-ESC exception above).
    pub fn line_may_start(self) -> bool {
        self >= Self::Escape
    }
}

/// The physical boundary a pane's writer thread keeps: where the bytes the
/// PTY took so far leave the stream. Set only by the writer thread; read and
/// waited on by anyone.
#[derive(Debug)]
pub struct Physical {
    boundary: AtomicU8,
    wake: Notify,
}

impl Default for Physical {
    fn default() -> Self {
        Self {
            boundary: AtomicU8::new(Boundary::Ground as u8),
            wake: Notify::new(),
        }
    }
}

impl Physical {
    /// Where the written stream is now.
    pub fn boundary(&self) -> Boundary {
        match self.boundary.load(Ordering::SeqCst) {
            b if b == Boundary::Ground as u8 => Boundary::Ground,
            b if b == Boundary::Escape as u8 => Boundary::Escape,
            _ => Boundary::Inside,
        }
    }

    /// Whether the written stream is in ground.
    pub fn at_ground(&self) -> bool {
        self.boundary() == Boundary::Ground
    }

    /// The writer thread's report of where the stream is after a write
    /// returned. A move to a more settled boundary wakes every waiter
    /// registered before it.
    pub fn set(&self, boundary: Boundary) {
        let was = self.boundary.swap(boundary as u8, Ordering::SeqCst);
        if (boundary as u8) > was {
            self.wake.notify_waiters();
        }
    }

    /// Resolve once the written stream is in ground: at once when it is
    /// already (I-9).
    pub async fn until_ground(&self) {
        self.until_pausing(|b| b == Boundary::Ground, |_| {}).await
    }

    /// Resolve once a deck line may start ([`Boundary::line_may_start`]): at
    /// once when it may already (I-9).
    pub async fn until_line(&self) {
        self.until_pausing(Boundary::line_may_start, |_| {}).await
    }

    /// Wait until `ok` holds of the boundary: check, register (the
    /// `Notified` future pinned and enabled), recheck, then await; calling
    /// `at` at each step, for tests.
    pub async fn until_pausing(&self, ok: impl Fn(Boundary) -> bool, mut at: impl FnMut(WakeAt)) {
        loop {
            if ok(self.boundary()) {
                return;
            }
            at(WakeAt::Checked);
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            at(WakeAt::Registered);
            if ok(self.boundary()) {
                return;
            }
            at(WakeAt::Rechecked);
            notified.await;
        }
    }
}

/// The writer thread's own tracker: the input framing over the bytes the PTY
/// took, publishing its boundary to [`Physical`].
#[derive(Debug, Default)]
pub struct PhysicalTracker {
    framing: Framing,
}

impl PhysicalTracker {
    /// The PTY took `written`.
    pub fn written(&mut self, written: &[u8], physical: &Physical) {
        if written.is_empty() {
            return;
        }
        for &byte in written {
            self.framing.feed(byte, |_| {});
        }
        physical.set(Boundary::of(&self.framing));
    }

    /// Where the bytes taken so far end.
    pub fn boundary(&self) -> Boundary {
        Boundary::of(&self.framing)
    }
}
