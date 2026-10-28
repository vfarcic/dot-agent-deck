//! PRD #1666 part 3 (Q35-06; R3 §3, A1–A4 of Q35-core): the input epoch, a
//! generation's count of what entered its agent's input box, and the
//! structures kept with it under one leaf lock ([`GenerationInput`]).
//!
//! **The numbers (R3 §3.1).** `epoch` counts the ops enqueued for the PTY
//! that carry bytes, plus synthetic `ExternalEdit` moves; `observed` is the
//! epoch through which a probe proved the box empty; `last_foreign` is the
//! epoch of the newest op no deck job owns; `owner` is the deck job owning the
//! op at `epoch`. The box is *touched* while `epoch != observed`. Only
//! [`InputEpoch::apply`] moves `observed`, and only for a probe whose own
//! byte is the last thing that entered and which read the box empty with
//! nothing foreign since its admission (R3 §3.4). A prompt or turn hook moves
//! none of the numbers.
//!
//! **Where it moves, as of this commit.** At the PTY writer's shared enqueue,
//! before the job is handed to the writer thread, so the writer can't start
//! a job whose move isn't visible (R3 §3.2); Q35-15 moves it to acceptance.
//! A deck write with no job named ([`Writer::UnnamedDeck`], which every deck
//! write in the daemon is as of this commit) counts as foreign. Exempt reports
//! don't move it: a whole focus or SGR mouse report while the agent has that
//! report mode on (D3), and a whole DA1 or OSC 10/11 reply to a query the
//! ledger keeps (D3′). A CPR reply is never exempt (OQ3). Whether an op is
//! one whole report is read from the input framing at the op's start, kept
//! in enqueue order: a withdrawn job leaves that framing ahead of what the
//! PTY took, which Q35-15/16's physical tracker corrects.
//!
//! **What reads it, as of this commit: nothing.** The card, admission and
//! the ring's reconcile read it from Q35-19 and Q35-21 on; existing doorbell
//! behaviour reads `PaneInputState` as before.
//!
//! **The lock.** One mutex per generation at rank [`crate::lock_rank::R6`],
//! above every other ranked lock, so the debug checker refuses any ranked
//! lock taken under it. The daemon takes it under the registry's record
//! table when a generation retires, inside the ingress ledger's lock at a
//! ticketed enqueue and inside the output lock at the output hook. Every
//! method takes it, does its arithmetic and releases it, calling only the
//! framing, ledger and record code of this module's siblings while it is
//! held, none of which takes a lock, does I/O or calls back out. The output
//! and ingress locks aren't ranked, so the checker sees neither of those
//! edges: that neither is taken under this lock rests on that leaf code, and
//! a change that adds a callback here reviews those edges again.
//!
//! **The invocation records** ([`super::record::Records`], Q35-07) live under
//! the same lock once attached ([`GenerationInput::attach_records`]), so a
//! record's transition, its hazard pop and the quiet clock's pause or resume
//! are one critical section, ordered against the output hook's latch and the
//! tracker's candidates by the lock alone. No production caller attaches them
//! yet (Q35-14b does).
//!
//! **What it costs today.** Nothing reads it to decide, but it isn't free: a
//! person's op is read as a report and every op is tokenized under this
//! lock, inside the ingress lock when ticketed, and an output push that
//! carries notes waits for it while holding the output lock.

use std::time::Instant;

use super::framing::{Csi, Event, Framing, Report, Str, StringKind, Token, whole_report};
use super::query_ledger::{
    Backlog, CprWaitEnd, EntryView, HazardFifo, HazardPop, Pop, QueryHistory, QueryKind,
    QueryLedger,
};
use super::record::{
    self, Channel, Decision, Drained, ExitFact, FirstStop, FrontierOutcome, Hold, Record,
    RecordKey, Recorded, Records, Refusal, Reported, Route, WriterScope,
};
use crate::lock_rank::{R6, RankedMutex};

/// A deck job the epoch can name: a probe's, or a ring transaction's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeckJobId(pub u64);

/// Who wrote an op's bytes, as the epoch reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// A deck job named by its caller: the op is the job's, not foreign.
    Deck(DeckJobId),
    /// The deck's own bytes with no job named: foreign to every job.
    UnnamedDeck,
    /// A person's keys, or a report a client forwards as input: the only
    /// writer whose op can be an exempt report.
    Human,
    /// A voice answer's keys.
    Answer,
}

/// R3 §3.1's numbers. See the module documentation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputEpoch {
    epoch: u64,
    observed: u64,
    last_foreign: u64,
    owner: Option<DeckJobId>,
    /// `epoch` couldn't advance. Sticky: touched, and nothing applies.
    exhausted: bool,
}

/// What a probe saw, for [`InputEpoch::apply`] (R3 §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub probe: DeckJobId,
    /// `E0`: the epoch just before the probe's own byte moved it.
    pub admitted_at: u64,
    /// The probe's claim was `Mine` and the daemon's read was empty.
    pub read_empty: bool,
    /// The generation retired before settlement.
    pub retired: bool,
}

impl InputEpoch {
    pub fn touched(&self) -> bool {
        self.exhausted || self.epoch != self.observed
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn observed(&self) -> u64 {
        self.observed
    }

    pub fn last_foreign(&self) -> u64 {
        self.last_foreign
    }

    pub fn owner(&self) -> Option<DeckJobId> {
        self.owner
    }

    /// One op entered, `owner`'s when it is a deck job's, foreign otherwise.
    /// The epoch it moved to.
    pub fn record(&mut self, owner: Option<DeckJobId>) -> u64 {
        match self.epoch.checked_add(1) {
            Some(epoch) => self.epoch = epoch,
            None => self.exhausted = true,
        }
        self.owner = owner;
        if owner.is_none() {
            self.last_foreign = self.epoch;
        }
        self.epoch
    }

    /// Apply a probe's observation: the box becomes empty through the
    /// probe's byte iff that byte is the last op, it is this probe's, nothing
    /// foreign entered since admission, the probe read empty and the
    /// generation hasn't retired. Whether it applied.
    pub fn apply(&mut self, obs: &Observation) -> bool {
        let applies = !self.exhausted
            && obs.admitted_at.checked_add(1) == Some(self.epoch)
            && self.owner == Some(obs.probe)
            && self.last_foreign <= obs.admitted_at
            && obs.read_empty
            && !obs.retired;
        if applies {
            self.observed = self.epoch;
        }
        applies
    }
}

/// The report modes an agent turned on in its output (D3): focus reporting
/// (1004), and the mouse tracking and encoding fields as `vt100` models them,
/// one value each, a reset clearing a field only when it names the value in
/// force.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportModes {
    focus: bool,
    /// 9, 1000, 1002 or 1003.
    tracking: Option<u32>,
    /// 1005, 1006 or 1015.
    encoding: Option<u32>,
}

impl ReportModes {
    fn set(&mut self, mode: u32, on: bool) {
        let field = match mode {
            1004 => {
                self.focus = on;
                return;
            }
            9 | 1000 | 1002 | 1003 => &mut self.tracking,
            1005 | 1006 | 1015 => &mut self.encoding,
            _ => return,
        };
        if on {
            *field = Some(mode);
        } else if *field == Some(mode) {
            *field = None;
        }
    }

    /// Whether `report` is one the agent asked for.
    pub fn enabled(&self, report: Report) -> bool {
        match report {
            Report::FocusIn | Report::FocusOut => self.focus,
            Report::SgrMouse => self.tracking.is_some() && self.encoding == Some(1006),
            Report::Cpr { .. } | Report::Da1 | Report::OscColour { .. } => false,
        }
    }
}

/// What the output hook hands the epoch's structures from one push, in
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputNote {
    Query(QueryKind),
    /// A1 §2.2's frame marker, the bytes `ESC [ ? 2026 h` exactly and
    /// contiguously, as crossterm spells a synchronized-update begin,
    /// outside any string or paste. Any other spelling, a combined
    /// `?1049;2026h`, an empty or sub-parameter, a leading zero, an
    /// intermediate, or a control or skipped byte inside or between the ESC
    /// and the `[`, is no frame: it notes nothing for `B`, which holds rather than guess,
    /// while the report modes it carries are still noted as [`Self::Mode`].
    SyncBegin,
    /// `CSI ? <mode> h` or `l`, for each mode `vte` keeps.
    Mode {
        mode: u32,
        on: bool,
    },
    /// RIS.
    Reset,
}

/// The notes one output token makes, appended to `out`.
pub(super) fn output_notes(token: &Token<'_>, modes: &[u32], out: &mut Vec<OutputNote>) {
    match token.event {
        Event::Csi(csi) if csi.private == Some(b'?') && matches!(csi.final_byte, b'h' | b'l') => {
            let on = csi.final_byte == b'h';
            if on && sync_begin(token, &csi) {
                out.push(OutputNote::SyncBegin);
            }
            for &mode in modes {
                if matches!(mode, 9 | 1000 | 1002 | 1003 | 1004 | 1005 | 1006 | 1015) {
                    out.push(OutputNote::Mode { mode, on });
                }
            }
        }
        Event::Csi(csi) => {
            if let Some(kind) = csi_query(&csi) {
                out.push(OutputNote::Query(kind));
            }
        }
        Event::Str(Str {
            kind: StringKind::Osc,
            payload: Some(payload),
            ..
        }) => match payload {
            b"10;?" => out.push(OutputNote::Query(QueryKind::OscColour { slot: 10 })),
            b"11;?" => out.push(OutputNote::Query(QueryKind::OscColour { slot: 11 })),
            _ => {}
        },
        Event::Escape(b'c') => out.push(OutputNote::Reset),
        _ => {}
    }
}

/// Whether a CSI is [`OutputNote::SyncBegin`]'s exact spelling, the bytes
/// `ESC [ ? 2026 h` with nothing between them, outside a paste. It needs no
/// guard from its caller.
fn sync_begin(token: &Token<'_>, csi: &Csi<'_>) -> bool {
    csi.private == Some(b'?')
        && csi.params == b"2026"
        && csi.intermediates.is_empty()
        && csi.final_byte == b'h'
        && csi.literal
        && !token.in_paste
}

/// A whole CPR or DA1 query.
fn csi_query(csi: &Csi<'_>) -> Option<QueryKind> {
    if !csi.intermediates.is_empty() {
        return None;
    }
    let mut params = csi.params();
    let first = params.next().map(|p| (p.value(), p.subparams().count()));
    if params.next().is_some() {
        return None;
    }
    match (csi.private, csi.final_byte, first) {
        (None | Some(b'?'), b'n', Some((Some(6), 1))) => Some(QueryKind::Cpr),
        (None, b'c', None | Some((Some(0), 1))) => Some(QueryKind::Da1),
        _ => None,
    }
}

/// What [`GenerationInput::enqueue`] did with one op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// The epoch moved to this value.
    Moved(u64),
    /// One whole report the agent asked for: no move.
    Exempt(Report),
}

#[derive(Debug)]
struct State {
    epoch: InputEpoch,
    /// The input side's framing, in enqueue order.
    framing: Framing,
    modes: ReportModes,
    ledger: QueryLedger,
    backlog: Backlog,
    hazards: HazardFifo,
    /// The generation's invocation records, once attached.
    records: Option<Records>,
    retired: bool,
}

/// One generation's input epoch, query ledger, `B` and hazard FIFO, under
/// one leaf lock. See the module documentation.
#[derive(Debug)]
pub struct GenerationInput {
    state: RankedMutex<{ R6 }, State>,
}

impl Default for GenerationInput {
    fn default() -> Self {
        Self::new(CprWaitEnd::Counted)
    }
}

impl GenerationInput {
    /// A generation whose screen profile ends a CPR wait as `cpr_wait_end`
    /// says.
    pub fn new(cpr_wait_end: CprWaitEnd) -> Self {
        Self {
            state: RankedMutex::new(State {
                epoch: InputEpoch::default(),
                framing: Framing::default(),
                modes: ReportModes::default(),
                ledger: QueryLedger::default(),
                backlog: Backlog::new(cpr_wait_end),
                hazards: HazardFifo::default(),
                records: None,
                retired: false,
            }),
        }
    }

    /// One byte-carrying op is being handed to the PTY writer: classify it
    /// against the framing at its start, the report modes and the ledger;
    /// settle the queries its replies answer; move the epoch unless it is an
    /// exempt report.
    pub fn enqueue(&self, writer: Writer, bytes: &[u8]) -> Enqueued {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        // Only a person's op can be exempt, so no other is read as a report.
        let exempt = match writer {
            Writer::Human => whole_report(&state.framing, bytes).filter(|&report| {
                state.modes.enabled(report)
                    || QueryKind::answered_by(report).is_some_and(|k| state.ledger.exempts(k))
            }),
            _ => None,
        };
        for &byte in bytes {
            state.framing.feed(byte, |token| {
                let answered = token
                    .report()
                    .filter(|_| !token.in_paste)
                    .and_then(QueryKind::answered_by);
                if let Some(kind) = answered
                    && !state.retired
                {
                    state.ledger.settle(kind);
                    if kind == QueryKind::Cpr {
                        state.backlog.cpr_reply();
                    }
                }
            });
        }
        match exempt {
            Some(report) => Enqueued::Exempt(report),
            None => {
                let owner = match writer {
                    Writer::Deck(job) => Some(job),
                    Writer::UnnamedDeck | Writer::Human | Writer::Answer => None,
                };
                Enqueued::Moved(state.epoch.record(owner))
            }
        }
    }

    /// An edit the deck learned of from outside the PTY (an editor session
    /// that isn't the deck's): a foreign move with no op.
    pub fn external_edit(&self) -> u64 {
        self.state.lock_recovering().epoch.record(None)
    }

    /// Apply a probe's observation (R3 §3.4); refused once retired.
    pub fn apply(&self, obs: &Observation) -> bool {
        let mut state = self.state.lock_recovering();
        let obs = Observation {
            retired: obs.retired || state.retired,
            ..*obs
        };
        state.epoch.apply(&obs)
    }

    /// The output hook's notes from one push, in order, at `now`.
    pub fn on_output(&self, notes: &[OutputNote], now: Instant) {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        if state.retired {
            return;
        }
        for &note in notes {
            let before = state.backlog.holds();
            match note {
                OutputNote::Query(kind) => {
                    state.ledger.note(kind);
                    if kind == QueryKind::Cpr {
                        state.backlog.cpr_query();
                    }
                }
                OutputNote::SyncBegin => state.backlog.sync_begin(),
                OutputNote::Mode { mode, on } => state.modes.set(mode, on),
                OutputNote::Reset => state.modes = ReportModes::default(),
            }
            if !before && state.backlog.holds() {
                state.hazards.latch(now);
            }
        }
    }

    /// A native Ctrl+G candidate was physically accepted at `now` (Q35-16's
    /// feeder): its `native_seq`, `None` once numbering is exhausted or the
    /// generation retired.
    pub fn accept_native(&self, now: Instant) -> Option<u64> {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        if state.retired {
            return None;
        }
        let backlog = state.backlog.holds();
        state.hazards.accept(now, backlog)
    }

    /// Whether any invocation record of the generation is incomplete
    /// (Q35-14b's feeder).
    pub fn records_pending(&self, now: Instant, pending: bool) {
        self.state
            .lock_recovering()
            .hazards
            .records_pending(now, pending);
    }

    /// See [`HazardFifo::pop_reported`].
    pub fn pop_reported(&self, receipt: &mut HazardPop, native_seen: u64, now: Instant) -> Pop {
        self.state
            .lock_recovering()
            .hazards
            .pop_reported(receipt, native_seen, now)
    }

    /// See [`HazardFifo::pop_at_drain`].
    pub fn pop_at_drain(&self, receipt: &mut HazardPop, birth_floor: Instant, now: Instant) -> Pop {
        self.state
            .lock_recovering()
            .hazards
            .pop_at_drain(receipt, birth_floor, now)
    }

    /// The hazard FIFO's clear conjunct, as of `now`. Never clear once
    /// retired.
    pub fn hazards_clear(&self, now: Instant) -> bool {
        let mut state = self.state.lock_recovering();
        !state.retired && state.hazards.clear(now)
    }

    /// The live hazard entries, oldest first, and how many are counted past
    /// them, as of `now`.
    pub fn hazards(&self, now: Instant) -> (Vec<EntryView>, u32) {
        let mut state = self.state.lock_recovering();
        (state.hazards.entries(now), state.hazards.extra())
    }

    /// `B`.
    pub fn backlog(&self) -> bool {
        self.state.lock_recovering().backlog.holds()
    }

    pub fn history(&self) -> QueryHistory {
        self.state.lock_recovering().ledger.history()
    }

    /// Whether a whole reply to `kind` would be recognised as solicited, and
    /// whether it would be exempt (and so eligible to pass ahead, I-5).
    pub fn reply_class(&self, kind: QueryKind) -> (bool, bool) {
        let state = self.state.lock_recovering();
        (state.ledger.solicited(kind), state.ledger.exempts(kind))
    }

    pub fn epoch(&self) -> InputEpoch {
        self.state.lock_recovering().epoch.clone()
    }

    pub fn touched(&self) -> bool {
        self.state.lock_recovering().epoch.touched()
    }

    /// Attach the generation's invocation records (Q35-07). Once: a second
    /// table, or one after retirement, is refused.
    pub fn attach_records(&self, records: Records) -> bool {
        let mut state = self.state.lock_recovering();
        if state.retired || state.records.is_some() {
            return false;
        }
        state.records = Some(records);
        true
    }

    /// Run `step` on the records and the hazard FIFO in one critical
    /// section, so a record's transition, its pop and the quiet clock's
    /// pause or resume are one event against the output hook's latch and the
    /// tracker's candidates (Q35-07's condition (b)). Private: each step is
    /// a method of [`Records`], which takes no lock, calls nothing outside
    /// its module and the FIFO, and does no I/O.
    fn with_records<T>(
        &self,
        step: impl FnOnce(&mut Records, &mut HazardFifo) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        match (state.retired, state.records.as_mut()) {
            (false, Some(records)) => step(records, &mut state.hazards),
            _ => Err(Refusal::Stale),
        }
    }

    /// See [`Records::observe`].
    pub fn record_observe(
        &self,
        obs: &record::Observation,
        now: Instant,
    ) -> Result<Recorded, Refusal> {
        self.with_records(|r, fifo| r.observe(fifo, obs, now))
    }

    /// See [`Records::refused_identity`].
    pub fn record_refused(
        &self,
        generation: u64,
        channel: Channel,
        decision: Option<Decision>,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.with_records(|r, _| r.refused_identity(generation, channel, decision, now))
    }

    /// See [`Records::report`].
    pub fn record_report(
        &self,
        generation: u64,
        key: RecordKey,
        report: record::Report,
        now: Instant,
    ) -> Result<Reported, Refusal> {
        self.with_records(|r, fifo| r.report(fifo, generation, key, report, now))
    }

    /// See [`Records::frontier`].
    pub fn record_frontier(
        &self,
        generation: u64,
        key: RecordKey,
        outcome: FrontierOutcome,
    ) -> Result<bool, Refusal> {
        self.with_records(|r, _| r.frontier(generation, key, outcome))
    }

    /// See [`Records::exit`].
    pub fn record_exit(
        &self,
        generation: u64,
        key: RecordKey,
        at: Instant,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.with_records(|r, fifo| r.exit(fifo, generation, key, at, now))
    }

    /// See [`Records::drained`].
    pub fn record_drained(
        &self,
        generation: u64,
        key: RecordKey,
        now: Instant,
    ) -> Result<Drained, Refusal> {
        self.with_records(|r, fifo| r.drained(fifo, generation, key, now))
    }

    /// See [`Records::match_segment`].
    pub fn record_match(&self, generation: u64, key: RecordKey, seg: u64) -> Result<(), Refusal> {
        self.with_records(|r, _| r.match_segment(generation, key, seg))
    }

    /// See [`Records::complete`].
    pub fn record_complete(
        &self,
        generation: u64,
        key: RecordKey,
        route: Route,
        at: Instant,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.with_records(|r, fifo| r.complete(fifo, generation, key, route, at, now))
    }

    /// See [`Records::prune`].
    pub fn record_prune(
        &self,
        generation: u64,
        key: RecordKey,
        markers_deleted: bool,
        now: Instant,
    ) -> Result<Option<ExitFact>, Refusal> {
        self.with_records(|r, fifo| r.prune(fifo, generation, key, markers_deleted, now))
    }

    /// See [`Records::raise`].
    pub fn liability_raise(&self, generation: u64, stamp: Instant) -> Result<u32, Refusal> {
        self.with_records(|r, _| r.raise(generation, stamp))
    }

    /// See [`Records::lower`].
    pub fn liability_lower(&self, generation: u64, stamp: Instant) -> Result<u32, Refusal> {
        self.with_records(|r, _| r.lower(generation, stamp))
    }

    /// A copy of one record.
    pub fn record(&self, key: RecordKey) -> Option<Record> {
        self.with_records(|r, _| Ok(r.get(key))).ok().flatten()
    }

    /// The writer scope, `None` with no records attached or once retired.
    pub fn writer_scope(&self) -> Option<WriterScope> {
        self.with_records(|r, _| Ok(r.scope())).ok()
    }

    /// See [`Records::admits`]; [`Hold::Retired`] with no records attached
    /// or once retired.
    pub fn records_admit(&self) -> Result<(), Hold> {
        self.with_records(|r, _| Ok(r.admits()))
            .unwrap_or(Err(Hold::Retired))
    }

    /// See [`Records::first_stop`].
    pub fn first_stop(&self, now: Instant) -> Option<FirstStop> {
        self.with_records(|r, _| Ok(r.first_stop(now))).ok()
    }

    /// The generation left its pane: the ledger, `B`, the hazard FIFO and the
    /// invocation records are dropped, nothing is noted or accepted any more,
    /// and no observation applies. A successor starts with its own.
    pub fn retire(&self) {
        let mut state = self.state.lock_recovering();
        state.retired = true;
        state.ledger = QueryLedger::default();
        state.backlog = Backlog::new(CprWaitEnd::Counted);
        state.hazards = HazardFifo::default();
        state.records = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// Scenario: Bring the epoch to its last value, record one more op, and offer the probe that owns it an observation. The epoch stays at its last value, the box stays touched, and the observation doesn't apply.
    #[spec("inbox/probe/002")]
    #[test]
    fn probe_002_an_exhausted_epoch_stays_touched() {
        let probe = DeckJobId(1);
        let mut e = InputEpoch {
            epoch: u64::MAX - 1,
            observed: u64::MAX - 1,
            last_foreign: 0,
            owner: None,
            exhausted: false,
        };
        assert_eq!(e.record(Some(probe)), u64::MAX);
        assert!(!e.exhausted);
        assert_eq!(e.record(Some(probe)), u64::MAX);
        assert!(e.exhausted && e.touched());
        assert!(!e.apply(&Observation {
            probe,
            admitted_at: u64::MAX - 1,
            read_empty: true,
            retired: false,
        }));
    }

    /// Scenario: Hand the output classifier the exact `?2026h` token read outside and inside a bracketed paste, which the output grammar never reports but the marker grammar excludes. Only the token outside a paste is a frame.
    #[spec("inbox/probe/060")]
    #[test]
    fn probe_060_a_pasted_marker_is_no_frame() {
        let notes = |in_paste| {
            let token = Token {
                event: Event::Csi(Csi {
                    private: Some(b'?'),
                    params: b"2026",
                    intermediates: b"",
                    final_byte: b'h',
                    literal: true,
                }),
                in_paste,
            };
            let mut out = Vec::new();
            output_notes(&token, &[2026], &mut out);
            out
        };
        assert_eq!(notes(false), [OutputNote::SyncBegin]);
        assert_eq!(notes(true), []);
    }

    /// Scenario: Ask the marker predicate itself, with no caller's guard in front of it, about literal `2026` CSIs with the `?` marker and `h`, with no or another private marker, and with another final byte. Only the first is the marker.
    #[spec("inbox/probe/060")]
    #[test]
    fn probe_060_the_marker_predicate_needs_no_guard() {
        let is_marker = |private, final_byte| {
            let csi = Csi {
                private,
                params: b"2026",
                intermediates: b"",
                final_byte,
                literal: true,
            };
            let token = Token {
                event: Event::Csi(csi),
                in_paste: false,
            };
            sync_begin(&token, &csi)
        };
        assert!(is_marker(Some(b'?'), b'h'));
        assert!(!is_marker(None, b'h'), "no private marker");
        assert!(!is_marker(Some(b'>'), b'h'), "another private marker");
        assert!(!is_marker(Some(b'?'), b'l'), "another final byte");
    }
}
