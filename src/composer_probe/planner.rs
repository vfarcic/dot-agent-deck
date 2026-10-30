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
use std::time::{Duration, Instant};

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
    ///
    /// **An accepted exception, with a residual (the Q35-16 review fix;
    /// both reviewers and the orchestrator accepted it).** A lone ESC isn't
    /// certainly the Escape key. It can be the first half of a key whose
    /// terminal sends ESC and the rest separately, an Alt+key (ESC then the
    /// key's byte, a multibyte character's included) or a sequence split
    /// across frames; and before the agent's own ESC timeout runs out, its
    /// reader can take the deck's first byte as Alt+that byte. So a deck line
    /// started here can lose its first byte to such a key, or turn it into
    /// one. That is the behaviour before Q35-16, kept for doorbell lines
    /// only: a probe never starts here, and Q35-19's probes use the strict
    /// ground predicate ([`Physical::until_ground`], [`Framing::at_ground`]),
    /// never [`Physical::until_line`] or [`Self::line_may_start`], which are
    /// no permission to probe.
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
    /// When the sequence the stream ends in counts as flushed by the agent's
    /// own reader, if its grammar flushes one (Q83, [`InputGrammar`]): from
    /// then the boundary reads ground. Set with the boundary, by the writer
    /// thread.
    flush_at: std::sync::Mutex<Option<Instant>>,
}

impl Default for Physical {
    fn default() -> Self {
        Self {
            boundary: AtomicU8::new(Boundary::Ground as u8),
            wake: Notify::new(),
            flush_at: std::sync::Mutex::new(None),
        }
    }
}

impl Physical {
    /// Where the written stream is now: inside a sequence the agent's own
    /// reader has flushed by now reads as ground (Q83).
    pub fn boundary(&self) -> Boundary {
        match self.boundary.load(Ordering::SeqCst) {
            b if b == Boundary::Ground as u8 => Boundary::Ground,
            b if b == Boundary::Escape as u8 => Boundary::Escape,
            _ if self.flushed(Instant::now()) => Boundary::Ground,
            _ => Boundary::Inside,
        }
    }

    /// When the sequence the stream ends in counts as flushed, if it ends in
    /// one the agent's grammar flushes.
    fn flush_at(&self) -> Option<Instant> {
        *self.flush_at.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the sequence the stream ends in counts as flushed at `now`.
    pub fn flushed(&self, now: Instant) -> bool {
        self.flush_at().is_some_and(|at| now >= at)
    }

    /// Whether the written stream is in ground.
    pub fn at_ground(&self) -> bool {
        self.boundary() == Boundary::Ground
    }

    /// The writer thread's report of where the stream is after a write
    /// returned. A move to a more settled boundary wakes every waiter
    /// registered before it.
    pub fn set(&self, boundary: Boundary) {
        self.set_flushing(boundary, None);
    }

    /// [`Self::set`], with when the sequence `boundary` ends in counts as
    /// flushed by the agent's reader (Q83), `None` for never.
    pub fn set_flushing(&self, boundary: Boundary, flush_at: Option<Instant>) {
        *self.flush_at.lock().unwrap_or_else(|e| e.into_inner()) =
            flush_at.filter(|_| boundary == Boundary::Inside);
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
            // A flush is a move to ground no write announces: wake at it.
            match self.flush_at() {
                Some(at) => {
                    tokio::select! {
                        () = notified => {}
                        () = tokio::time::sleep_until(at.into()) => {}
                    }
                }
                None => notified.await,
            }
        }
    }
}

/// How an agent's own input reader frames what it reads, as far as the
/// physical tracker follows it (Q83).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputGrammar {
    /// The tracker's own framing: a sequence stays open until a byte ends it.
    /// Every agent and release whose reader wasn't read from source.
    #[default]
    Strict,
    /// Claude Code 2.1.310 and 2.1.312's key reader, read from their bundled
    /// source (the Q83 source study: the tokenizer and `xy`, `nd`, `ll`):
    /// - a C0 other than ESC inside a CSI aborts it to ground and is read
    ///   from ground (an SS3 already ends at one in the strict framing);
    /// - an unfinished sequence is flushed once 50 ms pass with no input,
    ///   except one its `ll` recogniser keeps for 2 s
    ///   ([`Framing::response_like_prefix`]). A paste is kept strict: what
    ///   the reader leaves after its 2 s paste flush wasn't established, and
    ///   a lone ESC isn't flushed here either, so it stays
    ///   [`Boundary::Escape`] and admits no probe.
    ClaudeMeasured,
}

impl InputGrammar {
    /// The grammar of `agent`'s `release`, latched from its start-up header:
    /// the measured one only for the releases read from source.
    pub fn of(agent: &crate::event::AgentType, release: &semver::Version) -> Self {
        let measured = [
            semver::Version::new(2, 1, 310),
            semver::Version::new(2, 1, 312),
        ];
        if *agent == crate::event::AgentType::ClaudeCode && measured.contains(release) {
            Self::ClaudeMeasured
        } else {
            Self::Strict
        }
    }
}

/// The agent's quiet wait before it flushes an unfinished sequence (`Yn`).
pub const FLUSH_QUIET: Duration = Duration::from_millis(50);
/// Its wait for a sequence that may be a terminal's response (`$c`).
pub const FLUSH_RESPONSE: Duration = Duration::from_millis(2000);
/// What the tracker adds to either, since the agent's wait runs from when
/// its reader took the last byte, which is after this thread's write
/// returned, and its timer can fire late: a reader or timer delayed longer
/// than this behind the write is the residual, and the deck's next byte could
/// then still reach it inside the sequence. Generous on purpose: it costs a
/// ring under half a second after a sequence the user abandoned.
pub const FLUSH_MARGIN: Duration = Duration::from_millis(400);

/// The writer thread's own tracker: the input framing over the bytes the PTY
/// took, publishing its boundary to [`Physical`].
#[derive(Debug, Default)]
pub struct PhysicalTracker {
    framing: Framing,
}

impl PhysicalTracker {
    /// The PTY took `written`, read with the strict grammar.
    pub fn written(&mut self, written: &[u8], physical: &Physical) {
        self.written_with(written, physical, InputGrammar::Strict, Instant::now());
    }

    /// The PTY took `written` at `now`, read as `grammar` reads it (Q83).
    pub fn written_with(
        &mut self,
        written: &[u8],
        physical: &Physical,
        grammar: InputGrammar,
        now: Instant,
    ) {
        if written.is_empty() {
            return;
        }
        let measured = grammar == InputGrammar::ClaudeMeasured;
        // The agent's reader flushed what the stream ended in before these
        // bytes came.
        if measured && physical.flushed(now) {
            self.framing.abandon();
        }
        for &byte in written {
            if measured && byte < 0x20 && byte != 0x1b && self.framing.in_csi() {
                self.framing.abandon();
            }
            self.framing.feed(byte, |_| {});
        }
        let boundary = Boundary::of(&self.framing);
        let flush_at =
            (measured && boundary == Boundary::Inside && !self.framing.in_paste()).then(|| {
                let quiet = if self.framing.response_like_prefix() {
                    FLUSH_RESPONSE
                } else {
                    FLUSH_QUIET
                };
                now + quiet + FLUSH_MARGIN
            });
        physical.set_flushing(boundary, flush_at);
    }

    /// Where the stream is at `now`, applying a flush of the agent's that
    /// has come due (Q83): what this thread's start conditions read, so a job
    /// starts on the same boundary the waiters were woken by.
    pub fn settle(&mut self, physical: &Physical, now: Instant) -> Boundary {
        if physical.flushed(now) {
            self.framing.abandon();
            physical.set(Boundary::of(&self.framing));
        }
        Boundary::of(&self.framing)
    }

    /// Where the bytes taken so far end.
    pub fn boundary(&self) -> Boundary {
        Boundary::of(&self.framing)
    }
}
