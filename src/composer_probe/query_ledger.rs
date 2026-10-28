//! PRD #1666 part 3 (Q35-06; amendments 1–4 of Q35-core): what a
//! generation's output asked its terminal, and which Ctrl+G presses the agent
//! may not have read yet. Three structures, kept per generation under the
//! input epoch's leaf lock (`super::epoch::GenerationInput`):
//!
//! - [`QueryLedger`], D3′'s exemption ledger: the queries of the reply forms
//!   (CPR, DA1, OSC 10/11) the output emitted and nothing has answered yet.
//!   At most [`LEDGER_SLOTS`], never evicted. A query past them saturates it
//!   for the generation's life: from then on no reply is exempt and none can
//!   pass ahead, and the ledger counts what it dropped. Saturation refuses
//!   nothing by itself (A1 §3.2).
//! - [`Backlog`], `B`: whether the agent may be waiting on a cursor position
//!   report, read from its own state and never from the ledger's saturation
//!   (A2 §4.2). Each screen profile declares how a wait ends
//!   ([`CprWaitEnd`]).
//! - [`HazardFifo`]: the native Ctrl+G candidates the agent may not have
//!   processed yet, numbered by `native_seq` (A2 §4.4), `Latched` when `B`
//!   held at acceptance and `Quiet` otherwise, with a quiet clock that pauses
//!   while an invocation record is incomplete (A2 §4.5). Popped once per
//!   invocation record, exactly by the `native_seen` the helper reported, or
//!   at the record's exit drain by its birth floor (A3 §6.1). No candidate is
//!   discarded on a liveness reading (A3 §5).
//!
//! Each bound fails toward the conservative side: a lost exemption makes a
//! reply touch; `B` turns false only through a CPR reply or, on a profile
//! that declares it, the next frame, never through time or a reply to
//! another kind of query; an overflowed or exhausted FIFO delays eligibility
//! and never clears early. Nothing in this module reads a clock or a process: time,
//! record facts and `native_seen` arrive as inputs.
//!
//! Who feeds and reads what, as of this commit: the output hook notes queries,
//! `?2026h` and the D3 report modes, and the shared enqueue settles replies
//! (both through `GenerationInput`), and an installed generation's records
//! feed the pending fact and their pops (Q35-14b); the physical tracker's
//! native candidate (Q35-16) arrives later, as does the first reader, probe
//! admission (Q35-19).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::framing::Report;

/// How many outstanding queries the exemption ledger keeps.
pub const LEDGER_SLOTS: usize = 8;

/// How many native candidates the hazard FIFO keeps one by one; the rest are
/// counted (A1 §3.2).
pub const HAZARD_SLOTS: usize = 16;

/// D5's quiet period: how long a `Quiet` entry's running clock keeps it.
pub const T_TRIGGER: Duration = Duration::from_secs(10);

/// A query whose reply the input side can recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// `CSI 6 n` or `CSI ? 6 n`. Its reply is never exempt (OQ3).
    Cpr,
    /// `CSI c` or `CSI 0 c`.
    Da1,
    /// `OSC 10 ; ?` or `OSC 11 ; ?`.
    OscColour { slot: u8 },
}

impl QueryKind {
    /// The query `report` answers, if it answers one.
    pub fn answered_by(report: Report) -> Option<Self> {
        match report {
            Report::Cpr { .. } => Some(Self::Cpr),
            Report::Da1 => Some(Self::Da1),
            Report::OscColour { slot } => Some(Self::OscColour { slot }),
            Report::FocusIn | Report::FocusOut | Report::SgrMouse => None,
        }
    }
}

/// Whether the ledger has kept every query the output emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryHistory {
    Exact,
    /// A query was dropped at saturation. Never turns back into `Exact`.
    Unknown,
}

/// D3′'s outstanding queries (A1 §3.2 (a)). See the module documentation.
#[derive(Debug, Clone, Default)]
pub struct QueryLedger {
    outstanding: VecDeque<QueryKind>,
    saturated: bool,
    dropped: u32,
}

impl QueryLedger {
    /// The output emitted a query of `kind`.
    pub fn note(&mut self, kind: QueryKind) {
        if self.saturated || self.outstanding.len() == LEDGER_SLOTS {
            self.saturated = true;
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.outstanding.push_back(kind);
        }
    }

    /// Whether a reply to `kind` answers a query the ledger keeps. After
    /// saturation the kept entries still match (A2 §5); a query dropped at
    /// saturation doesn't.
    pub fn solicited(&self, kind: QueryKind) -> bool {
        self.outstanding.contains(&kind)
    }

    /// Whether one whole reply to `kind` is exempt from touching the box:
    /// solicited, not a CPR, and the ledger not saturated.
    pub fn exempts(&self, kind: QueryKind) -> bool {
        !self.saturated && kind != QueryKind::Cpr && self.solicited(kind)
    }

    /// A reply to `kind` reached the agent: it answers the oldest kept query
    /// of that kind. `false` when none was outstanding (a duplicate or an
    /// unsolicited reply).
    pub fn settle(&mut self, kind: QueryKind) -> bool {
        match self.outstanding.iter().position(|&k| k == kind) {
            Some(at) => {
                self.outstanding.remove(at);
                true
            }
            None => false,
        }
    }

    pub fn history(&self) -> QueryHistory {
        if self.saturated {
            QueryHistory::Unknown
        } else {
            QueryHistory::Exact
        }
    }

    pub fn saturated(&self) -> bool {
        self.saturated
    }

    /// Queries noted at or after saturation, which the ledger didn't keep.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }
}

/// How a screen profile's agent ends a wait for a cursor position report
/// (A2 §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CprWaitEnd {
    /// Every wait ends before the agent's next `?2026h` (Codex, P-WE):
    /// `B` holds while the last whole CPR query is later than the last whole
    /// `?2026h`.
    SyncBegin,
    /// Nothing declared: `B` holds while CPR queries outnumber whole CPR
    /// replies written to the agent, and for good once that count saturates.
    Counted,
}

/// `B`: whether the agent may be blocked on a CPR wait (A2 §4.2).
#[derive(Debug, Clone)]
pub struct Backlog {
    end: CprWaitEnd,
    /// [`CprWaitEnd::SyncBegin`]: a CPR query came after the last `?2026h`.
    after_sync: bool,
    /// [`CprWaitEnd::Counted`]: queries less replies, floored at zero.
    outstanding: u32,
    /// [`CprWaitEnd::Counted`]: the count reached `u32::MAX`.
    saturated: bool,
}

impl Backlog {
    pub fn new(end: CprWaitEnd) -> Self {
        Self {
            end,
            after_sync: false,
            outstanding: 0,
            saturated: false,
        }
    }

    /// The output emitted a whole CPR query.
    pub fn cpr_query(&mut self) {
        match self.end {
            CprWaitEnd::SyncBegin => self.after_sync = true,
            CprWaitEnd::Counted => match self.outstanding.checked_add(1) {
                Some(n) if n < u32::MAX => self.outstanding = n,
                _ => {
                    self.outstanding = u32::MAX;
                    self.saturated = true;
                }
            },
        }
    }

    /// The output emitted a whole `?2026h`.
    pub fn sync_begin(&mut self) {
        if self.end == CprWaitEnd::SyncBegin {
            self.after_sync = false;
        }
    }

    /// A whole CPR reply was written to the agent, whoever sent it: the
    /// agent's waiter takes it as its reply either way.
    pub fn cpr_reply(&mut self) {
        if self.end == CprWaitEnd::Counted && !self.saturated {
            self.outstanding = self.outstanding.saturating_sub(1);
        }
    }

    /// `B`.
    pub fn holds(&self) -> bool {
        match self.end {
            CprWaitEnd::SyncBegin => self.after_sync,
            CprWaitEnd::Counted => self.saturated || self.outstanding > 0,
        }
    }
}

/// What a record's one pop took from the FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pop {
    /// The entry with this `native_seq`.
    Entry(u64),
    /// One of the counted entries past [`HAZARD_SLOTS`].
    Extra,
    /// Nothing qualified.
    Nothing,
}

/// An invocation record's receipt for its one pop (A2 §4.4): a record pops
/// once, whichever of its observations comes first, and never again. Kept in
/// the record (Q35-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HazardPop {
    #[default]
    Unspent,
    Spent(Pop),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Latched,
    /// The FIFO's running clock when the entry was accepted.
    Quiet {
        run_at_entry: Duration,
    },
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    seq: u64,
    accepted: Instant,
    state: EntryState,
}

/// The state of one native candidate, for a reader that wants the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryView {
    Latched { seq: u64 },
    Quiet { seq: u64, paused: bool },
}

/// The native-trigger hazard FIFO (A1 §3.3, A2 §4.4–§4.5, A3 §5–§6.1). See
/// the module documentation.
///
/// Its quiet clock is one clock for the FIFO: it runs while no record of the
/// generation is pending, and a `Quiet` entry has been quiet for the running
/// time since its acceptance. The instant a `Quiet` entry has run exactly
/// [`T_TRIGGER`] still counts as quiet, so a CPR noted at that instant
/// latches it: the tie goes to the hazard.
#[derive(Debug, Clone)]
pub struct HazardFifo {
    entries: VecDeque<Entry>,
    /// Entries past [`HAZARD_SLOTS`], all `Latched`.
    extra: u32,
    /// The highest `native_seq` among them.
    newest_extra: u64,
    /// When the newest of them was accepted.
    newest_extra_accept: Option<Instant>,
    /// The last number handed out; `0` before the first.
    native_seq: u64,
    /// `native_seq` or `extra` couldn't advance. Sticky.
    exhausted: bool,
    /// The running time accrued while no record was pending.
    run: Duration,
    /// When `run` was last brought up to date, while running.
    mark: Option<Instant>,
    /// The latest time any call has brought the clock to. A caller's
    /// timestamp earlier than it, taken before a racing caller's, is read as
    /// this one, so no interval is counted twice.
    latest: Option<Instant>,
    paused: bool,
}

impl Default for HazardFifo {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            extra: 0,
            newest_extra: 0,
            newest_extra_accept: None,
            native_seq: 0,
            exhausted: false,
            run: Duration::ZERO,
            mark: None,
            latest: None,
            paused: false,
        }
    }
}

impl HazardFifo {
    /// Bring the running clock up to `now`, or to the latest time already
    /// seen when `now` is earlier, and let every `Quiet` entry that has run
    /// past [`T_TRIGGER`] leave (D5). The time it brought the clock to.
    fn advance(&mut self, now: Instant) -> Instant {
        let now = self.latest.map_or(now, |latest| latest.max(now));
        self.latest = Some(now);
        if !self.paused {
            if let Some(mark) = self.mark {
                self.run += now.saturating_duration_since(mark);
            }
            self.mark = Some(now);
        }
        let run = self.run;
        self.entries.retain(|e| match e.state {
            EntryState::Latched => true,
            EntryState::Quiet { run_at_entry } => run - run_at_entry <= T_TRIGGER,
        });
        now
    }

    /// A native candidate was physically accepted at `now`, with `B` as
    /// `backlog` says. Its number, or `None` once numbering is exhausted (then
    /// no candidate is numbered, and the FIFO stays non-clear until
    /// retirement).
    pub fn accept(&mut self, now: Instant, backlog: bool) -> Option<u64> {
        self.advance(now);
        if self.exhausted {
            return None;
        }
        let Some(seq) = self.native_seq.checked_add(1) else {
            self.exhausted = true;
            return None;
        };
        self.native_seq = seq;
        if self.extra > 0 || self.entries.len() == HAZARD_SLOTS {
            // While anything is counted, every new entry is counted too, so
            // every counted entry is newer than every kept one.
            match self.extra.checked_add(1) {
                Some(extra) => self.extra = extra,
                None => self.exhausted = true,
            }
            self.newest_extra = seq;
            self.newest_extra_accept = Some(now);
        } else {
            let state = if backlog {
                EntryState::Latched
            } else {
                EntryState::Quiet {
                    run_at_entry: self.run,
                }
            };
            self.entries.push_back(Entry {
                seq,
                accepted: now,
                state,
            });
        }
        Some(seq)
    }

    /// `B` turned true at `now`: every `Quiet` entry still quiet latches,
    /// paused or running (the agent may now be waiting before it reads the
    /// candidate).
    pub fn latch(&mut self, now: Instant) {
        self.advance(now);
        for entry in &mut self.entries {
            entry.state = EntryState::Latched;
        }
    }

    /// Whether any record of the generation is incomplete, as of `now`:
    /// while one is, the quiet clock stops (P-DQ, A2 §4.3).
    pub fn records_pending(&mut self, now: Instant, pending: bool) {
        let now = self.advance(now);
        if pending != self.paused {
            self.paused = pending;
            self.mark = (!pending).then_some(now);
        }
    }

    /// The pop of a record that reported `native_seen`, at its first such
    /// observation: the oldest live entry numbered at most `native_seen`, or,
    /// with no kept entry live, one counted entry once `native_seen` reaches
    /// the newest of them. Nothing when `receipt` is already spent.
    pub fn pop_reported(&mut self, receipt: &mut HazardPop, native_seen: u64, now: Instant) -> Pop {
        self.advance(now);
        if *receipt != HazardPop::Unspent {
            return Pop::Nothing;
        }
        let pop = match self.entries.front() {
            Some(e) if e.seq <= native_seen => {
                let seq = e.seq;
                self.entries.pop_front();
                Pop::Entry(seq)
            }
            Some(_) => Pop::Nothing,
            None if self.extra > 0 && native_seen >= self.newest_extra => self.pop_extra(),
            None => Pop::Nothing,
        };
        *receipt = HazardPop::Spent(pop);
        pop
    }

    /// The fallback pop, at the exit drain of a record no observation of
    /// which carried `native_seen` (A3 §6.1): the oldest live entry accepted
    /// strictly before `birth_floor`, or, with no kept entry live, one counted
    /// entry once the newest of them was. A floor equal to an acceptance pops
    /// nothing. Nothing when `receipt` is already spent.
    pub fn pop_at_drain(
        &mut self,
        receipt: &mut HazardPop,
        birth_floor: Instant,
        now: Instant,
    ) -> Pop {
        self.advance(now);
        if *receipt != HazardPop::Unspent {
            return Pop::Nothing;
        }
        let pop = match self.entries.front() {
            Some(e) if e.accepted < birth_floor => {
                let seq = e.seq;
                self.entries.pop_front();
                Pop::Entry(seq)
            }
            Some(_) => Pop::Nothing,
            None if self.extra > 0 && self.newest_extra_accept.is_some_and(|a| a < birth_floor) => {
                self.pop_extra()
            }
            None => Pop::Nothing,
        };
        *receipt = HazardPop::Spent(pop);
        pop
    }

    fn pop_extra(&mut self) -> Pop {
        self.extra -= 1;
        if self.extra == 0 {
            self.newest_extra = 0;
            self.newest_extra_accept = None;
        }
        Pop::Extra
    }

    /// No live entry, nothing counted, and numbering not exhausted, as of
    /// `now`: O4's hazard conjunct (A1 §3.4).
    pub fn clear(&mut self, now: Instant) -> bool {
        self.advance(now);
        !self.exhausted && self.entries.is_empty() && self.extra == 0
    }

    /// The live kept entries, oldest first, as of `now`.
    pub fn entries(&mut self, now: Instant) -> Vec<EntryView> {
        self.advance(now);
        let paused = self.paused;
        self.entries
            .iter()
            .map(|e| match e.state {
                EntryState::Latched => EntryView::Latched { seq: e.seq },
                EntryState::Quiet { .. } => EntryView::Quiet { seq: e.seq, paused },
            })
            .collect()
    }

    /// How many entries are counted past [`HAZARD_SLOTS`].
    pub fn extra(&self) -> u32 {
        self.extra
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// Scenario: Bring the CPR count one below its cap and note two more CPR queries. The second saturates it, and `B` then stays true through any number of replies.
    #[spec("inbox/probe/060")]
    #[test]
    fn probe_060_the_counter_saturates_sticky() {
        let mut b = Backlog::new(CprWaitEnd::Counted);
        b.outstanding = u32::MAX - 2;
        b.cpr_query();
        assert!(!b.saturated);
        assert_eq!(b.outstanding, u32::MAX - 1);
        b.cpr_query();
        assert!(b.saturated && b.holds());
        for _ in 0..3 {
            b.cpr_reply();
        }
        assert!(b.holds(), "a saturated count never counts down");
        assert_eq!(b.outstanding, u32::MAX);
    }

    /// Scenario: Bring the FIFO's `native_seq` to its last value and accept two more native candidates. The first is numbered `u64::MAX`, the second isn't, and the FIFO stays non-clear, refusing further numbers, however it is popped.
    #[spec("inbox/probe/052")]
    #[test]
    fn probe_052_native_seq_exhaustion_is_sticky() {
        let t0 = Instant::now();
        let mut f = HazardFifo {
            native_seq: u64::MAX - 1,
            ..HazardFifo::default()
        };
        assert_eq!(f.accept(t0, false), Some(u64::MAX));
        assert!(!f.exhausted);
        assert_eq!(f.accept(t0, false), None);
        assert!(f.exhausted);
        assert_eq!(f.native_seq, u64::MAX, "nothing wraps");
        let mut receipt = HazardPop::Unspent;
        assert_eq!(
            f.pop_reported(&mut receipt, u64::MAX, t0),
            Pop::Entry(u64::MAX)
        );
        assert!(
            !f.clear(t0),
            "exhausted stays non-clear with every entry popped"
        );
        assert_eq!(f.accept(t0 + T_TRIGGER * 3, false), None);
        assert!(!f.clear(t0 + T_TRIGGER * 3), "no clock clears exhaustion");
    }

    /// Scenario: Bring the FIFO's counted entries to their cap and accept one more native candidate. The count stays at its cap, the FIFO is exhausted, and popping counted entries never makes it clear.
    #[spec("inbox/probe/052")]
    #[test]
    fn probe_052_extra_exhaustion_is_sticky() {
        let t0 = Instant::now();
        let mut f = HazardFifo::default();
        for _ in 0..HAZARD_SLOTS {
            f.accept(t0, true);
        }
        f.accept(t0, true);
        assert_eq!(f.extra, 1);
        f.extra = u32::MAX;
        let seq = f.accept(t0, true).expect("still numbered");
        assert!(f.exhausted);
        assert_eq!(f.extra, u32::MAX, "the count doesn't wrap");
        assert_eq!(f.newest_extra, seq);
        f.entries.clear();
        let mut receipt = HazardPop::Unspent;
        assert_eq!(f.pop_reported(&mut receipt, seq, t0), Pop::Extra);
        assert!(!f.clear(t0));
    }
}
