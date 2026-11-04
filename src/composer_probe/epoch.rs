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
//! **Where it moves.** At acceptance (Q35-15): for an op the PTY writer
//! takes at once, at its enqueue, before the job is handed to the writer
//! thread, so the writer can't start a job whose move isn't visible (R3
//! §3.2); for an op a closed fence holds (`super::gate`), when it enters the
//! held queue, before anything can release it, and not again when it does.
//! A deck write names its job: a probe's `0x07` and a ring transaction's
//! line, Enter and take-back are their jobs' ([`Writer::Deck`],
//! [`DeckJobId::ring`], Q35-21). One with no job named
//! ([`Writer::UnnamedDeck`], which only tests write) counts as foreign. Exempt reports
//! don't move it: a whole focus or SGR mouse report while the agent has that
//! report mode on (D3), and a whole DA1 or OSC 10/11 reply to a query the
//! ledger keeps (D3′). A CPR reply is never exempt (OQ3). Whether an op is
//! one whole report is read from the input framing at the op's start, kept
//! in enqueue order: a withdrawn job leaves that framing ahead of what the
//! PTY took, which Q35-15/16's physical tracker corrects.
//!
//! **What reads it.** The epoch's numbers: probe admission
//! ([`GenerationInput::admit_probe`], Q35-19), which the probe request and
//! the doorbell tick ask for an `Armable` generation (Q35-20), for a lapse
//! retry's "nothing since the lapsed byte", and the schedule's
//! epoch-moved wait. A probe's settlement applies to `observed` here (R3
//! §3.4), and the second `ExternalEdit` producer moves it
//! ([`GenerationInput::on_output_bytes`]). From Q35-21 the doorbell reads
//! them too: the card's hold reason and the ring's admission frame read
//! [`InputEpoch::touched`], and the ring's `Submitted` reconcile moves
//! `observed` ([`GenerationInput::reconcile_ring`], R3 §3.5). The ledger is read by a closed fence's
//! acceptance, for whether a reply may pass ahead
//! ([`GenerationInput::pass_ahead`]).
//!
//! **The planner (Q35-16).** The input framing kept here is the logical
//! planner of `super::planner`: [`GenerationInput::enqueue_planned`] reads a
//! person's op once, at its acceptance, against the ledger as it stands,
//! and in the same hold either finds its first cut (a segment is open, or
//! the records hold a liability) or numbers its native Ctrl+G candidates in
//! the hazard FIFO's sequence. The planner reads the records' count for
//! that, and nothing in production raises it before activation, so there it
//! only numbers. The FIFO's entries come later, one per candidate the PTY
//! took ([`GenerationInput::enter_native`]).
//!
//! **The lock.** One mutex per generation at rank [`crate::lock_rank::R6`],
//! above every other ranked lock, so the debug checker refuses any ranked
//! lock taken under it. The daemon takes it under the registry's record
//! table when a generation retires, inside the ingress ledger's lock at a
//! ticketed enqueue and inside the output lock at the output hook. Every
//! method takes it, does its arithmetic and releases it, calling only the
//! framing, ledger and record code of this module's siblings while it is
//! held, none of which takes a lock, does I/O or calls back out. The one
//! callback is the arm's publication ([`GenerationInput::arm_probe`],
//! [`GenerationInput::raise_published`]), which runs under this lock: its
//! contract is to take no lock and do no I/O, and the production page
//! (`GenerationFiles`) keeps it by storing to the mapped page's atomics, so
//! a page fault on that mapping is the one wait it can add. The output and
//! ingress locks aren't ranked, so the checker sees neither of those edges:
//! that neither is taken under this lock rests on that leaf code and that
//! callback's contract, and a change that adds a callback here, or a page
//! whose publication breaks the contract, reviews those edges again.
//!
//! **The invocation records** ([`super::record::Records`], Q35-07) live under
//! the same lock once attached ([`GenerationInput::attach_records`]), so a
//! record's transition, its hazard pop and the quiet clock's pause or resume
//! are one critical section, ordered against the output hook's latch and the
//! tracker's candidates by the lock alone. The daemon attaches them to an
//! installed generation at its spawn (Q35-14b, `super::observe`).
//!
//! **What it costs today.** A person's op is read as a report and every op
//! is tokenized under this lock, inside the ingress lock when ticketed, and an output push that
//! carries notes waits for it while holding the output lock. Q35-16's
//! planner decides under it: whether a person's op is cut at a candidate or
//! its candidates are numbered native is read from the liability state here
//! ([`GenerationInput::enqueue_planned`], [`GenerationInput::would_cut`]),
//! which no production path sets before activation. The op is still read as
//! a report and tokenized once, its candidates found in that same pass, now
//! with that bookkeeping and a clone of the framing when it numbers, all
//! under this lock. Native numbers are published to the arm page with this
//! lock released. Outside it, a cut op's rest is read again, from the cut's
//! saved state, when it is released, and the writer thread replays an op
//! with native candidates over what the PTY took. What the planner costs
//! here was measured in one A/B harness (Q35-16's `r6-contention.md`); those
//! numbers bound what that harness observed, and are no claim that it costs
//! nothing in timing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use super::admission;
use super::framing::{Csi, Event, Framing, Report, Str, StringKind, Token, whole_report};
use super::frontier::{FrontierQueue, Request};
use super::planner::InputGrammar;
use super::planner::{Cut, Natives, first_cut, is_candidate};
use super::probe::Schedule;
use super::query_ledger::{
    Backlog, CprWaitEnd, EntryView, HazardFifo, HazardPop, Pop, QueryHistory, QueryKind,
    QueryLedger,
};
use super::query_wait::{QueryWaitEstimate, RedrawTiming};
use super::record::{
    self, Attribution, Channel, Decision, Drained, ExitFact, FirstStop, FrontierOutcome, Hold,
    NotOwned, Record, RecordKey, Recorded, Records, Refusal, Reported, Route, WriterScope,
};
use super::segments::{self, Front, Marker, Seen, Segments};
use crate::lock_rank::{R6, RankedMutex};

/// A deck job the epoch can name: a probe's, or a ring transaction's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeckJobId(pub u64);

impl DeckJobId {
    /// The high bit marks a ring transaction's job, so it never equals a
    /// probe's, whose number is its segment's (counted from 1).
    const RING: u64 = 1 << 63;

    /// PRD #1666 part 3 (Q35-21, R3 §3.5): the job of a ring transaction's
    /// attempt `attempt`, unique within its generation since attempt
    /// numbers are.
    pub fn ring(attempt: u32) -> Self {
        Self(Self::RING | u64::from(attempt))
    }
}

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

    /// PRD #1666 part 3 (Q35-21, R3 §3.5): the ring's `Submitted` (or
    /// placeholder-confirmed `Withdrawn`) reconcile. The box becomes empty
    /// through the ring's last op iff that
    /// op is `ring`'s and nothing foreign entered since its admission at
    /// `admitted_at`: the agent took the deck's line alone from a box the
    /// epoch said was empty at admission. Whether it applied.
    pub fn reconcile_ring(&mut self, ring: DeckJobId, admitted_at: u64) -> bool {
        let applies = !self.exhausted
            && self.owner == Some(ring)
            && self.last_foreign <= admitted_at
            && self.observed == admitted_at;
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
/// guard from its caller. The segments' E1F and EC read it as it is
/// (`super::segments::return_marker`).
pub(super) fn sync_begin(token: &Token<'_>, csi: &Csi<'_>) -> bool {
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
    /// When the generation's redraw waits are expected to have run out
    /// (Q35-18a). Availability advice: nothing here reads it.
    estimate: QueryWaitEstimate,
    /// The generation's invocation records, once attached.
    records: Option<Records>,
    /// Its segments and what they read, attached with the records (Q35-20).
    live: Option<Live>,
    retired: bool,
}

/// The most explanations [`Live::explained`] banks, and the most unexplained
/// pair endings [`Live::unexplained`] remembers: each `Owned` record
/// accounts for one bracketed-paste pair, and more than a few outstanding
/// would only let a later unreported editor go unnoticed. Past the cap the
/// accounting fails closed: a credit not banked, or a pair's ending
/// forgotten into [`Live::forgotten`], can only make a later pair move the
/// epoch once more, never once less (R-B-U2-01).
pub(crate) const EXPLAINED_CAP: u8 = 4;

/// How long a record's credit outlives its drain (the audit's U3-4): a
/// policy, not a bound on output delivery. The agent re-enables bracketed
/// paste once its editor command, the helper, has exited, so a session's
/// pair usually ends soon after its helper drains; but the helper's drain
/// and the agent's output reach the daemon on independent paths, and a
/// paused or backlogged output reader can deliver a session's own pair
/// later than this. The grace only stops a credit whose pair has not come
/// (the agent exited, the pair was lost, or it is merely late) from being
/// lent to a later, unrelated pair indefinitely. Expiring is conservative
/// for that session: its own pair, if it comes after the grace, moves the
/// epoch as an external edit, one more epoch and never one less. That extra
/// epoch is not invisible: it can withhold an in-flight empty observation
/// or cost one more check.
pub(crate) const CREDIT_AFTER_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);

/// The daemon's read of an `Armed` export, as the epoch's settlement reads
/// it ([`Live::reads`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoxRead {
    Empty,
    NonEmpty,
    /// The export couldn't be read: the check saw nothing of the box.
    Unreadable,
}

/// The most daemon reads of an `Armed` export kept for the epoch's
/// settlement ([`Live::reads`]).
const READS_KEPT: usize = record::RECORD_CAP;

/// PRD #1666 part 3 (Q35-20): an installed generation's segments and what
/// they read, under the epoch's lock with its records, so a record step, the
/// segments' match, release and completion, and the epoch's settlement are
/// one critical section (O-20.3).
#[derive(Debug)]
struct Live {
    segments: Segments,
    /// `ExternalEdit`'s second producer (PR U3-11): a `?2004l` was read and
    /// no `?2004h` has followed it yet.
    pair_open: bool,
    /// Records made since the last pair that may account for one, oldest
    /// first: an editor session the deck saw, so its pair isn't an
    /// unreported one. Each is kept by its record, and expires
    /// [`CREDIT_AFTER_DRAIN`] after that record drained (or went).
    explained: std::collections::VecDeque<Credit>,
    /// When each pair that completed with no record to account for it (so
    /// it moved the epoch) ended, oldest first (R-U2-04). A record made
    /// later whose helper may have been born by then may be that pair's
    /// session: it accounts for that pair and banks nothing, so it can't
    /// lend an explanation to a later, unrelated pair.
    unexplained: std::collections::VecDeque<Instant>,
    /// The latest ending of a pair [`Self::unexplained`] had no room to keep
    /// (R-B-U2-01). A record whose helper may have been born by then may be
    /// that pair's session, whose debt the queue no longer shows: it banks
    /// nothing. A forgotten debt never becomes credit.
    forgotten: Option<Instant>,
    /// When the deck may try a probe again (R2 §6.8, R5's continuing
    /// retries). Its `broken` bit is presentation only (O-20.6).
    schedule: Schedule,
    /// The epoch the last op entering the input other than a deck job's
    /// moved it to (a person's key, an answer, an unnamed deck write): not
    /// an inferred edit (an unexplained pair or an external one), which can
    /// be a probe's own round trip read before its record. Q147b's
    /// settlement reads it.
    last_foreign_op: u64,
    /// Each open probe: its segment number, its job, and `E0`, the epoch
    /// just before its own byte moved it (set at its enqueue).
    probes: Vec<Probe>,
    /// The lapsed Claude probe whose lapse the schedule counted as a failure
    /// (U3-02): each lapse is counted once.
    lapse_counted: Option<u64>,
    /// The daemon's read of an `Armed` invocation's export: whether it was
    /// empty, and whether it read the box at all, for the epoch's settlement
    /// of the probe it matches.
    reads: std::collections::VecDeque<(RecordKey, BoxRead)>,
    /// Q35-21 (A-20d-1a, O-20.7b): what wakes the off-lock keymap re-read an
    /// empty read's settlement waits for ([`GenerationInput::recheck_keymap_at_settlement`]).
    /// `None` applies an empty read at once: a generation no observer
    /// re-reads for, which only the tests that model no observer make.
    recheck: Option<Recheck>,
    /// Empty reads whose settlement waits for that re-read, oldest first.
    parked: Vec<Parked>,
    /// The ticket the next parked read gets: tickets only grow, so a
    /// re-read that started before a read parked can't settle it.
    next_ticket: u64,
}

/// Q35-21 (A-20d-1a): the wake [`Live::recheck`] holds. It takes no lock
/// (the observer's is an unpark), so it is called under R6.
#[derive(Clone)]
struct Recheck(Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for Recheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Recheck")
    }
}

/// Q35-21 (A-20d-1a): an `Armed` empty read's settlement, decided under R6
/// and waiting for a keymap read taken after it ([`Live::parked`]).
#[derive(Debug, Clone, Copy)]
struct Parked {
    ticket: u64,
    probe: Probe,
}

/// [`GenerationInput::pair_accounting`]'s answer.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairAccounting {
    pub credits: Vec<(RecordKey, Option<Instant>)>,
    pub unexplained: Vec<Instant>,
    pub forgotten: Option<Instant>,
}

/// One record's explanation of a pair still to come ([`Live::explained`]).
#[derive(Debug, Clone, Copy)]
struct Credit {
    key: RecordKey,
    /// When the deck first saw its record drained or gone.
    drained: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Probe {
    number: u64,
    job: DeckJobId,
    e0: Option<u64>,
}

/// What a probe's settlement did to the schedule and the epoch (Q35-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeRead {
    /// Its matched helper read the box empty, and the epoch's apply rule
    /// (R3 §3.4) moved `observed` when it says so.
    Empty { applied: bool },
    /// The box held something, or the read couldn't be trusted.
    NonEmpty,
    /// No liability was read: an uncertain check.
    Uncertain,
}

/// How the planner reads an op at its acceptance (Q35-16, core §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// A closed fence holds it: a segment is open, so its first Ctrl+G
    /// candidate cuts it, and nothing in it is native.
    Held,
    /// Written now through the gate: its first candidate cuts it while a
    /// liability is out; with none out, every candidate is native.
    Open,
    /// Written now by a path that doesn't pass the gate: nothing can be cut,
    /// and its candidates are native while no liability is out.
    Bypass,
}

/// What the planner decided for one op, once, at its acceptance.
#[derive(Debug, Clone)]
pub struct Planned {
    pub enqueued: Enqueued,
    /// The first cut, when its candidates cut.
    pub cut: Option<Cut>,
    /// The native candidates it carries, numbered.
    pub natives: Option<Natives>,
    /// The input framing at its first byte, when its candidates cut: what a
    /// release reads it from (Q35-20).
    pub framing: Option<Framing>,
}

/// [`GenerationInput::enqueue`] under a hold the caller already has.
fn enqueue_locked(state: &mut State, writer: Writer, bytes: &[u8]) -> Enqueued {
    let enqueued = plan_locked(state, writer, bytes, None).enqueued;
    // Q35-20 (R3 §3.4): a probe's own byte; `E0` is the epoch just before
    // it, read in the same hold that moved it.
    if let (Writer::Deck(job), Enqueued::Moved(to)) = (writer, enqueued)
        && let Some(live) = state.live.as_mut()
        && let Some(probe) = live.probes.iter_mut().find(|p| p.job == job)
    {
        probe.e0 = to.checked_sub(1);
    }
    enqueued
}

/// Q35-20: settle what a step allows the segments (match, release,
/// completion), then the epoch's settlement of each probe whose matched
/// record decided (R3 §3.4), in the caller's hold of the lock. A probe its
/// completion removed before its settlement ran is settled here too, before
/// its ending deletes its entry (R-U2-05).
fn settle_live(state: &mut State, now: Instant) {
    let (Some(live), Some(records), false) =
        (state.live.as_mut(), state.records.as_mut(), state.retired)
    else {
        return;
    };
    live.segments.settle(records, &mut state.hazards, now);
    for (number, key, decision) in live.segments.take_decided_probes(records) {
        let read = live.read_of(key);
        // An `Armed` decision can reach the records on a marker before the
        // helper's own invocation, which carries the daemon's read: wait for
        // the read, until the record drained or the segment completed.
        if decision == Decision::Armed
            && read.is_none()
            && records.get(key).is_some_and(|r| !r.drained)
        {
            // revact28: logged each time, so a probe stuck here (its
            // record never draining) shows in the log.
            tracing::debug!(
                target: "composer_probe",
                number,
                ?decision,
                "probe segment matched; waiting for the daemon's read"
            );
            live.segments.undecide(number);
            continue;
        }
        tracing::debug!(
            target: "composer_probe",
            number,
            ?decision,
            read = read.is_some(),
            "probe segment matched"
        );
        live.settle_probe(
            &mut state.epoch,
            records,
            (number, key, Some(decision)),
            now,
        );
    }
    for probe in live.segments.take_completed_probes() {
        live.settle_probe(&mut state.epoch, records, probe, now);
    }
    for ended in live.segments.take_ended() {
        live.probes.retain(|p| Some(p.number) != ended.number);
        tracing::debug!(target: "composer_probe", ?ended, "segment ended");
    }
}

impl Live {
    /// A pair that ended at `ended` moved the epoch with no record to account
    /// for it: remember it, and when the queue is full, fold its oldest entry
    /// into [`Self::forgotten`] rather than dropping it (R-B-U2-01).
    fn owe(&mut self, ended: Instant) {
        if self.unexplained.len() == usize::from(EXPLAINED_CAP)
            && let Some(oldest) = self.unexplained.pop_front()
        {
            self.forgotten = Some(self.forgotten.map_or(oldest, |f| f.max(oldest)));
        }
        self.unexplained.push_back(ended);
    }

    /// A new record of a helper born no earlier than `birth_floor` may
    /// account for one pair. When a forgotten pair may have been its
    /// session's, it does nothing: it neither repays a remembered pair nor
    /// banks. Else it repays the oldest unexplained pair that ended once it
    /// may have been born; else it banks a credit for its pair still to
    /// come, while there is room.
    ///
    /// **The invariant (R-B-U2-01).** Compare an accounting that forgets
    /// nothing, in which each record repays the oldest unrepaid pair that
    /// ended at or after its floor, or else banks. Every pair that
    /// accounting still owes and that ended after [`Self::forgotten`] is in
    /// [`Self::unexplained`], and every credit banked here is one it banks
    /// too. So a pair moves the epoch here whenever it moves it there.
    ///
    /// It holds for any interleaving, step by step. A pair consumes the
    /// oldest credit or moves the epoch and is owed, in both accountings;
    /// with fewer credits here, a pair can only move the epoch here and not
    /// there, which adds a debt here only. Forgetting raises the watermark
    /// past every entry it drops. Drains and the grace drop the same credits
    /// from both. A record whose floor is at or before the watermark does
    /// nothing here, while the other repays one debt or banks: either keeps
    /// the relation. A record whose floor is after the watermark sees here
    /// every debt the other can use, all remembered; it repays the oldest
    /// of them here, at or before the one the other repays, and the one
    /// taken here, when older, is one the other no longer owes; when there
    /// is none here there is none there, and both bank. Checking the
    /// watermark first is what makes this hold: repaying a remembered pair
    /// for a record that may be a forgotten pair's session shifts that debt
    /// onto a later record, which then finds none and banks a credit for a
    /// pair that already moved the epoch.
    ///
    /// **What the argument assumes** (the audit's U3-2 and U3-3), none of it
    /// established here:
    ///
    /// 1. `birth_floor` is at or before the helper's real birth, on the
    ///    clock a pair's ending is read on. The observer derives it
    ///    (`observe::birth_floor`, called at attestation) from the helper's
    ///    start ticks floored to their tick (`record::birth_floor_ns`), and
    ///    maps that boot-clock time onto [`Instant`] by reading `Instant`
    ///    first and the boot clock after it, so the mapping errs early; a
    ///    suspend between the birth and the mapping moves it earlier still,
    ///    since the boot clock counts suspend and `Instant` does not. The
    ///    output hook reads a pair's `now` after it fed the push that held
    ///    the `?2004h` (`paste_mode`), so at or after the pair's real ending.
    ///    `probe_069_the_floor_the_refusal_the_prune_and_the_bell`
    ///    (tests/editor_observer.rs: ticks to an `Instant`, erring early) and
    ///    `probe_008_the_birth_floor_refuses_within_a_tick`
    ///    (tests/invocation_record.rs: ticks to boot-clock nanoseconds,
    ///    floored) check the two conversions; nothing checks a live helper's floor
    ///    against its birth. A floor later than the birth would let a record
    ///    miss its own pair read earlier and bank a credit for it.
    /// 2. [`Self::unexplained`] is sorted by ending: pairs reach
    ///    [`GenerationInput::on_return_markers`] in output order, from the
    ///    output hook under the output lock, each push's `now` read after
    ///    the last one's, so [`Self::owe`] pushes in order and its fold drops
    ///    the oldest.
    /// 3. Credits are consumed first in, first out, which preserves the
    ///    count but not whose pair a credit explained: the next paragraph.
    ///
    /// The accounting that forgets nothing is the reference, not proof that
    /// a credit is the record's own: credits are consumed oldest first, so a
    /// pair no record accounts for, read while another session's credit is
    /// live, takes that credit, and the epoch moves at that session's own
    /// pair instead (the audit's U3-1).
    fn explain(&mut self, key: RecordKey, birth_floor: Instant) {
        let outcome = if self.forgotten.is_some_and(|f| f >= birth_floor) {
            // Fail closed: it may repay a debt the queue forgot, so it
            // spends no debt a later record may need.
            "fail-closed"
        } else if let Some(at) = self
            .unexplained
            .iter()
            .position(|&ended| ended >= birth_floor)
        {
            self.unexplained.remove(at);
            "repaid a pair that already moved the epoch"
        } else if self.explained.len() < usize::from(EXPLAINED_CAP) {
            self.explained.push_back(Credit { key, drained: None });
            "banked a credit"
        } else {
            "no room to bank"
        };
        tracing::debug!(
            target: "composer_probe",
            ?key,
            outcome,
            unexplained = self.unexplained.len(),
            credits = self.explained.len(),
            "record explains a pair"
        );
    }

    /// Note when each credit's record was first seen drained or gone.
    fn note_drained(&mut self, records: &Records, now: Instant) {
        for credit in &mut self.explained {
            if credit.drained.is_none() && records.get(credit.key).is_none_or(|r| r.drained) {
                credit.drained = Some(now);
            }
        }
    }

    /// Drop each credit whose record drained [`CREDIT_AFTER_DRAIN`] or more
    /// before `now` (the audit's U3-4): its pair has not come in the grace,
    /// and a late one moves the epoch.
    fn expire_credits(&mut self, now: Instant) {
        self.explained.retain(|c| {
            c.drained
                .is_none_or(|d| now.saturating_duration_since(d) < CREDIT_AFTER_DRAIN)
        });
    }

    /// The daemon's latest read of record `key`'s export, if any.
    /// The daemon's read of `key`'s export: whether it was empty, an
    /// unreadable one counting as not empty, as the settlement has always
    /// read it.
    fn read_of(&self, key: RecordKey) -> Option<bool> {
        self.box_read_of(key).map(|read| read == BoxRead::Empty)
    }

    fn box_read_of(&self, key: RecordKey) -> Option<BoxRead> {
        self.reads
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, read)| *read)
    }
}

impl Live {
    /// The epoch's settlement of probe `number` from its matched record
    /// `key`'s `decision` and the daemon's read of its export (R3 §3.4),
    /// once: an `Armed` empty read that is the deck's own claim applies the
    /// observation and the schedule's empty, or, where the keymap is
    /// re-read at settlement ([`Self::recheck`], A-20d-1a), is parked for a
    /// keymap read taken after this decision; another `Armed` is non-empty;
    /// anything else, no decision included, is a failed check, counted
    /// unless the probe's lapse was counted already (R-D-U2-02).
    fn settle_probe(
        &mut self,
        epoch: &mut InputEpoch,
        records: &Records,
        (number, key, decision): (u64, RecordKey, Option<Decision>),
        now: Instant,
    ) {
        let read = self.read_of(key);
        let Some(at) = self.probes.iter().position(|p| p.number == number) else {
            return;
        };
        let probe = self.probes.remove(at);
        // A record already pruned can't show whose claim it read: not applied.
        let claim_mine = records
            .get(key)
            .is_some_and(|r| r.claim.is_none_or(|c| c == record::Claim::Owned));
        tracing::debug!(
            target: "composer_probe",
            number,
            ?decision,
            ?read,
            claim_mine,
            e0 = ?probe.e0,
            "probe settled"
        );
        match (decision, read) {
            // Q35-21 (A-20d-1a): parked for a keymap read taken after it,
            // the observer's, which settles it under this lock.
            (Some(Decision::Armed), Some(true)) if claim_mine && self.recheck.is_some() => {
                self.next_ticket = self.next_ticket.wrapping_add(1);
                self.parked.push(Parked {
                    ticket: self.next_ticket,
                    probe,
                });
                if let Some(Recheck(wake)) = &self.recheck {
                    wake();
                }
            }
            (Some(Decision::Armed), Some(true)) if claim_mine => self.apply_empty(epoch, probe),
            // Q147b: the read is of the box as the probe's own byte found
            // it. An op entering the input after that byte (a person's key
            // held behind the check, or typed after it) isn't in it, so the
            // next check waits for nothing more: it is due at once.
            // Otherwise it waits for the epoch to move past where it stands
            // now, as before. So does a check whose export the daemon
            // couldn't read (Q151b): it saw nothing of the box, and the next
            // check would see as little (an export on a filesystem the check
            // doesn't support stays so), so it is a timed recheck, as before
            // Q147b.
            (Some(Decision::Armed), _) => {
                let saw_box = self.box_read_of(key) != Some(BoxRead::Unreadable);
                let from = match probe.e0 {
                    Some(e0) if saw_box && self.last_foreign_op > e0.saturating_add(1) => {
                        e0.saturating_add(1)
                    }
                    _ => epoch.epoch(),
                };
                self.schedule.non_empty(now, from);
            }
            // R-D-U2-02: a probe whose lapse the schedule already counted
            // (`lapse_due`) was one attempt; its late failed check is that
            // same failure, not a second one. A late read still settles it
            // (the arms above).
            _ if self.lapse_counted == Some(number) => {}
            _ => self.schedule.failed(now),
        }
    }
}

impl Live {
    /// An empty read the deck may apply: the epoch's apply rule (R3 §3.4),
    /// which refuses a read the epoch has moved past, then the schedule's
    /// empty.
    fn apply_empty(&mut self, epoch: &mut InputEpoch, probe: Probe) {
        let applied = probe.e0.map(|e0| {
            epoch.apply(&Observation {
                probe: probe.job,
                admitted_at: e0,
                read_empty: true,
                retired: false,
            })
        });
        tracing::debug!(
            target: "composer_probe",
            number = probe.number,
            e0 = ?probe.e0,
            ?applied,
            epoch = epoch.epoch(),
            observed = epoch.observed(),
            last_foreign = epoch.last_foreign(),
            owner = ?epoch.owner(),
            "empty read applied"
        );
        self.schedule.empty();
    }
}

/// Whether a liability is out on the generation: the count its records hold,
/// which is what the arm page publishes.
fn liability_out(state: &State) -> bool {
    !state.retired && state.records.as_ref().is_some_and(|r| r.count() > 0)
}

/// Whether `bytes` from `writer`, read from the framing as it stands, are one
/// whole colour reply to a query the ledger keeps: the one op whose ending
/// BEL is no candidate (D3′, A2 §5).
fn solicited_reply(state: &State, writer: Writer, bytes: &[u8]) -> bool {
    writer == Writer::Human && solicited_whole(state, whole_report(&state.framing, bytes))
}

/// [`solicited_reply`] for an op whose whole report was read already.
fn solicited_whole(state: &State, whole: Option<Report>) -> bool {
    matches!(
        whole,
        Some(report @ Report::OscColour { .. })
            if QueryKind::answered_by(report).is_some_and(|k| state.ledger.solicited(k))
    )
}

/// [`GenerationInput::enqueue_planned`] under a hold the caller already has;
/// `plan` `None` plans nothing.
fn plan_locked(state: &mut State, writer: Writer, bytes: &[u8], plan: Option<Plan>) -> Planned {
    // Only a person's op can be exempt, so no other is read as a report.
    let whole = match writer {
        Writer::Human => whole_report(&state.framing, bytes),
        _ => None,
    };
    let exempt = whole.filter(|&report| {
        state.modes.enabled(report)
            || QueryKind::answered_by(report).is_some_and(|k| state.ledger.exempts(k))
    });
    // A person's keys and a voice answer's are the planner's; the deck's own
    // bytes open no segment and are never native.
    let person = matches!(writer, Writer::Human | Writer::Answer);
    let liability = liability_out(state);
    let (cutting, numbering) = match plan.filter(|_| person) {
        None => (false, false),
        Some(Plan::Held) => (true, false),
        Some(Plan::Open) => (liability, !liability),
        Some(Plan::Bypass) => (false, !liability),
    };
    // From ground, an op with neither a BEL nor an ESC holds no candidate,
    // so it has nothing to number and its start state needn't be kept.
    let numbering = numbering
        && !state.retired
        && !(state.framing.at_ground() && !bytes.iter().any(|&b| b == 0x07 || b == 0x1b));
    let solicited = (cutting || numbering) && solicited_whole(state, whole);
    let start = numbering.then(|| state.framing.clone());
    let at_start = cutting.then(|| state.framing.clone());
    let mut cut = None;
    let mut numbered: Option<(u64, u64)> = None;
    let mut ground = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        if state.framing.at_ground() {
            ground = i;
        }
        let mut candidates = 0usize;
        state.framing.feed(byte, |token| {
            if is_candidate(&token, solicited) {
                candidates += 1;
            }
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
        if candidates == 0 {
            continue;
        }
        if cutting && cut.is_none() {
            cut = Some(Cut {
                at: i + 1,
                start: ground,
                resume: state.framing.clone(),
            });
        }
        if numbering {
            for _ in 0..candidates {
                if let Some(seq) = state.hazards.number() {
                    numbered = Some(numbered.map_or((seq, seq), |(first, _)| (first, seq)));
                }
            }
        }
    }
    // R2 §6.8 (U3-02): a person's op gives a broken observer one fresh
    // attempt, due at once. A report the deck exempts (an enabled mode's, or
    // a whole reply to a query it saw asked) isn't typing; a terminal reply
    // it can't exempt (unsolicited, a duplicate, split) is, as the planner
    // reads it. Every such op makes the attempt due again, so a person typing
    // while the observer stays broken gets at most one attempt per doorbell
    // tick (the tick asks one request at a time), each failure backing off.
    if person
        && exempt.is_none()
        && !bytes.is_empty()
        && !state.retired
        && let Some(live) = state.live.as_mut()
    {
        live.schedule.person();
    }
    let enqueued = match exempt {
        Some(report) => Enqueued::Exempt(report),
        None => {
            let owner = match writer {
                Writer::Deck(job) => Some(job),
                Writer::UnnamedDeck | Writer::Human | Writer::Answer => None,
            };
            let moved_to = state.epoch.record(owner);
            if owner.is_none()
                && let Some(live) = state.live.as_mut()
            {
                live.last_foreign_op = moved_to;
            }
            tracing::debug!(
                target: "composer_probe",
                ?writer,
                len = bytes.len(),
                moved_to,
                "an op moved the epoch"
            );
            Enqueued::Moved(moved_to)
        }
    };
    Planned {
        enqueued,
        cut,
        natives: numbered
            .zip(start)
            .map(|((first, last), start)| Natives { first, last, start }),
        framing: at_start,
    }
}

/// [`GenerationInput::records_for_e2e`]'s answer.
#[cfg(any(test, feature = "e2e"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordsForE2e {
    pub scope: record::ScopeState,
    pub policy: record::EcPolicy,
    pub cap: usize,
    pub retained: usize,
    pub witness: record::E2eWitness,
}

/// One generation's input epoch, query ledger, `B`, hazard FIFO and redraw
/// estimate, under one leaf lock. See the module documentation.
#[derive(Debug)]
pub struct GenerationInput {
    state: RankedMutex<{ R6 }, State>,
    /// Woken after any step that may have moved the segments (Q35-20): the
    /// generation's release driver waits on it. Notified with this lock
    /// released.
    moved: tokio::sync::Notify,
    /// How many bytes the output hook has read, advanced by the hook before
    /// it hands over a push's notes, outside the lock: the offset a query is
    /// noted at, and a fence's trigger (Q35-15).
    output_offset: AtomicU64,
    /// Where native numbers are published (Q35-16): called with no lock of
    /// this module held.
    native_page: NativePage,
    /// A probe request is on its way (U3-02): the doorbell tick starts no
    /// other until it answers ([`Self::begin_probe_request`]).
    probe_request: std::sync::atomic::AtomicBool,
    /// The drain frontier's requests and its reader's wake (Q35-20a).
    frontier: FrontierQueue,
    /// How the agent's input reader frames what it reads, as its release
    /// capture holds it for the generation's own agent (Q83,
    /// [`InputGrammar::latched`]); strict until a release is known.
    input_grammar: AtomicU8,
    /// Test seam: parks an acceptance between the epoch's move and the job
    /// becoming visible, so a test can see the one before the other.
    #[cfg(test)]
    accept_park: std::sync::Mutex<Option<AcceptPark>>,
    /// Test seam (R-C-U2-01): the clock a records step stamps its drains
    /// and settles the segments at, in place of the wall clock, so a test on
    /// a virtual clock orders a credit's grace against the pairs it passes to
    /// [`Self::on_return_markers`] exactly. `None`, the wall clock, unless a
    /// test set it; a release build has no field and reads the wall clock.
    #[cfg(test)]
    step_clock: std::sync::Mutex<Option<Instant>>,
    /// Test seam (Q84-3): where the output hook parks between taking its
    /// stamp and publishing it ([`Self::park_feeder_for_test`]).
    #[cfg(test)]
    stamp_park: super::stamp_park::StampPark,
    /// The test and `e2e` builds' writer-thread controls (Q105b.2): a hold
    /// and a fault plan, reached without the pane writer's lock.
    #[cfg(any(test, feature = "e2e"))]
    writer_e2e: Arc<crate::e2e_controls::WriterE2e>,
}

/// A probe request on its way ([`GenerationInput::begin_probe_request`]).
pub struct ProbeRequest(Arc<GenerationInput>);

impl Drop for ProbeRequest {
    fn drop(&mut self) {
        self.0.probe_request.store(false, Ordering::SeqCst);
    }
}

/// The generation's native-number publisher, once its observer runs.
#[derive(Default)]
struct NativePage(std::sync::OnceLock<Box<dyn Fn(u64) + Send + Sync>>);

impl std::fmt::Debug for NativePage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NativePage")
            .field(&self.0.get().is_some())
            .finish()
    }
}

/// Test seam: one armed park at acceptance ([`GenerationInput::park_next_acceptance`]).
#[cfg(test)]
#[derive(Debug)]
struct AcceptPark {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Default for GenerationInput {
    fn default() -> Self {
        Self::new(CprWaitEnd::Counted)
    }
}

impl GenerationInput {
    /// A generation whose screen profile ends a CPR wait as `cpr_wait_end`
    /// says, with no redraw timing.
    pub fn new(cpr_wait_end: CprWaitEnd) -> Self {
        Self::profiled(cpr_wait_end, None, Instant::now())
    }

    /// A generation spawned at `spawn` whose screen profile ends a CPR wait
    /// as `cpr_wait_end` says and redraws as `redraw` says, `None` while it
    /// has no profile (Q35-18a).
    pub fn profiled(
        cpr_wait_end: CprWaitEnd,
        redraw: Option<&'static RedrawTiming>,
        spawn: Instant,
    ) -> Self {
        Self {
            state: RankedMutex::new(State {
                epoch: InputEpoch::default(),
                framing: Framing::default(),
                modes: ReportModes::default(),
                ledger: QueryLedger::default(),
                backlog: Backlog::new(cpr_wait_end),
                hazards: HazardFifo::default(),
                estimate: QueryWaitEstimate::new(redraw, spawn),
                records: None,
                live: None,
                retired: false,
            }),
            moved: tokio::sync::Notify::new(),
            output_offset: AtomicU64::new(0),
            native_page: NativePage::default(),
            probe_request: std::sync::atomic::AtomicBool::new(false),
            frontier: FrontierQueue::default(),
            input_grammar: AtomicU8::new(0),
            #[cfg(test)]
            accept_park: std::sync::Mutex::new(None),
            #[cfg(test)]
            step_clock: std::sync::Mutex::new(None),
            #[cfg(test)]
            stamp_park: Default::default(),
            #[cfg(any(test, feature = "e2e"))]
            writer_e2e: Arc::default(),
        }
    }

    /// The writer thread's test and `e2e` controls (Q105b.2).
    #[cfg(any(test, feature = "e2e"))]
    pub fn writer_e2e(&self) -> &Arc<crate::e2e_controls::WriterE2e> {
        &self.writer_e2e
    }

    /// The output hook read `len` more bytes; called before it hands over
    /// that push's notes, so a query is noted at the end of its push.
    pub fn advance_output(&self, len: usize) {
        self.output_offset.fetch_add(len as u64, Ordering::SeqCst);
    }

    /// Test seam (Q84-3): park the output hook at each
    /// [`StampPoint`](super::stamp_park::StampPoint) it reaches.
    #[cfg(test)]
    pub(crate) fn park_feeder_for_test(
        &self,
        park: impl Fn(super::stamp_park::StampPoint) + Send + Sync + 'static,
    ) {
        self.stamp_park.set(park);
    }

    /// The output hook's side of [`Self::park_feeder_for_test`].
    #[cfg(test)]
    pub(crate) fn feeder_at(&self, point: super::stamp_park::StampPoint) {
        self.stamp_park.at(point);
    }

    /// The output offset as of now: a fence closed now has this trigger,
    /// and a query noted later is after it.
    pub fn output_offset(&self) -> u64 {
        self.output_offset.load(Ordering::SeqCst)
    }

    /// Whether `bytes` from `writer` pass ahead of a fence whose trigger is
    /// at output offset `trigger` (I-5), and, when they do, their acceptance,
    /// in one hold of the lock: a person's op that is one whole D3′-exempt
    /// reply, read against the framing at its start, answering a query noted
    /// after `trigger`, is fed to the framing and settles that query here,
    /// classified exempt, so it moves no epoch. The decision, the query it
    /// consumes and its exemption are one decision: an output push that
    /// saturates the ledger lands before it, and the reply is held, or after
    /// it, and changes nothing about it. Its job is then submitted with no
    /// second [`Self::enqueue`] (Q35-15 fix, R-U2-04). `false`, changing
    /// nothing, for anything else.
    pub fn pass_ahead(&self, writer: Writer, bytes: &[u8], trigger: u64) -> bool {
        if writer != Writer::Human {
            return false;
        }
        // A retired generation's ledger is empty, so nothing passes.
        let mut guard = self.state.lock_recovering();
        let passes = whole_report(&guard.framing, bytes)
            .and_then(QueryKind::answered_by)
            .is_some_and(|kind| guard.ledger.passes_ahead(kind, trigger));
        if passes {
            let accepted = enqueue_locked(&mut guard, writer, bytes);
            debug_assert!(
                matches!(accepted, Enqueued::Exempt(_)),
                "a reply that passes ahead is exempt under the same hold"
            );
        }
        passes
    }

    /// Test seam: the next acceptance parks after it moved the epoch and
    /// before its job is visible to the writer or the held queue, until the
    /// returned sender sends.
    #[cfg(test)]
    pub(crate) fn park_next_acceptance(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *self.accept_park.lock().unwrap() = Some(AcceptPark {
            reached: reached_tx,
            release: release_rx,
        });
        (reached, release)
    }

    /// Test seam: where an acceptance parks when a test asked.
    pub(crate) fn accepted_parks(&self) {
        #[cfg(test)]
        if let Some(park) = self.accept_park.lock().unwrap().take() {
            let _ = park.reached.send(());
            let _ = park.release.recv();
        }
    }

    /// One byte-carrying op is accepted, for the PTY writer or a fence's
    /// held queue: classify it against the framing at its start, the report
    /// modes and the ledger; settle the queries its replies answer; move the
    /// epoch unless it is an exempt report.
    pub fn enqueue(&self, writer: Writer, bytes: &[u8]) -> Enqueued {
        enqueue_locked(&mut self.state.lock_recovering(), writer, bytes)
    }

    /// [`Self::enqueue`], with the planner's decision for the op's Ctrl+G
    /// candidates made in the same hold (Q35-16, core §4.3): the classification
    /// is the ledger's as it stands now, and nothing reads it again for this
    /// op. Native candidates are numbered here, in acceptance order; the
    /// caller publishes the last number before it submits the op
    /// ([`Self::publish_native`]).
    pub fn enqueue_planned(&self, writer: Writer, bytes: &[u8], plan: Plan) -> Planned {
        plan_locked(&mut self.state.lock_recovering(), writer, bytes, Some(plan))
    }

    /// [`Self::enqueue_planned`], unless the planner would cut the op
    /// (Q16-1): then `Err` with where, and nothing changed. The decision and
    /// the acceptance are one hold of this lock, so no liability raised or
    /// segment opened between a separate read and the acceptance can give
    /// the op a cut its caller refused to take (a voice answer, whose keys
    /// are never split away from their gate).
    pub fn enqueue_planned_uncut(
        &self,
        writer: Writer,
        bytes: &[u8],
        plan: Plan,
    ) -> Result<Planned, usize> {
        let mut state = self.state.lock_recovering();
        let cutting = matches!(writer, Writer::Human | Writer::Answer)
            && match plan {
                Plan::Held => true,
                Plan::Open => liability_out(&state),
                Plan::Bypass => false,
            };
        if cutting {
            let solicited = solicited_reply(&state, writer, bytes);
            if let Some(cut) = first_cut(&state.framing, bytes, solicited) {
                return Err(cut.at);
            }
        }
        Ok(plan_locked(&mut state, writer, bytes, Some(plan)))
    }

    /// Where `bytes` from `writer` would be cut, read from the framing as it
    /// stands, while a segment is open (`segment_open`) or a liability is
    /// out: the end of their first candidate. `None` when nothing cuts.
    /// Changes nothing: a caller that writes only up to the cut enqueues
    /// those bytes alone, so the rest is read once, when it comes again.
    pub fn would_cut(&self, writer: Writer, bytes: &[u8], segment_open: bool) -> Option<usize> {
        let state = self.state.lock_recovering();
        if !matches!(writer, Writer::Human | Writer::Answer)
            || !(segment_open || liability_out(&state))
        {
            return None;
        }
        let solicited = solicited_reply(&state, writer, bytes);
        first_cut(&state.framing, bytes, solicited).map(|cut| cut.at)
    }

    /// Native candidate `seq` was physically accepted at `now` (the physical
    /// tracker's report, A1 §3.3): it enters the hazard FIFO, `Latched` when
    /// `B` holds now. Nothing once retired.
    pub fn enter_native(&self, seq: u64, now: Instant) {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        if state.retired {
            return;
        }
        let backlog = state.backlog.holds();
        state.hazards.enter(seq, now, backlog);
    }

    /// Where native numbers are published: the generation's arm page, once
    /// its observer runs. Set once; `false` when one was set already.
    pub fn set_native_page(&self, page: Box<dyn Fn(u64) + Send + Sync>) -> bool {
        self.native_page.0.set(page).is_ok()
    }

    /// Publish `seq`, the last native number of an op about to be submitted
    /// (A2 §4.4), with no lock of this module held. Nothing with no page.
    pub fn publish_native(&self, seq: u64) {
        if let Some(page) = self.native_page.0.get() {
            page(seq);
        }
    }

    /// An edit the deck learned of from outside the PTY (an editor session
    /// that isn't the deck's): a foreign move with no op.
    pub fn external_edit(&self) -> u64 {
        let moved_to = self.state.lock_recovering().epoch.record(None);
        tracing::debug!(target: "composer_probe", moved_to, "an external edit moved the epoch");
        moved_to
    }

    /// The ring's `Submitted` reconcile ([`InputEpoch::reconcile_ring`]);
    /// refused once retired. The caller holds the generation's ingress
    /// ledger across this call (R3 §3.5), so no person's op is admitted
    /// between its check and this update.
    pub fn reconcile_ring(&self, ring: DeckJobId, admitted_at: u64) -> bool {
        let mut state = self.state.lock_recovering();
        !state.retired && state.epoch.reconcile_ring(ring, admitted_at)
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

    /// The output hook's notes from one push, in order, at `now`. Its
    /// queries are noted at the output offset as of now, which the hook has
    /// advanced past the push ([`Self::advance_output`]).
    pub fn on_output(&self, notes: &[OutputNote], now: Instant) {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        if state.retired {
            return;
        }
        let offset = self.output_offset();
        for &note in notes {
            let before = state.backlog.holds();
            match note {
                OutputNote::Query(kind) => {
                    state.ledger.note(kind, offset);
                    state.estimate.query(kind, now);
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

    /// The daemon applied a size change to the PTY at `now`: a dimension
    /// change or a nudge edge (Q35-18a). Nothing once retired.
    pub fn size_change(&self, now: Instant) {
        let mut state = self.state.lock_recovering();
        if !state.retired {
            state.estimate.size_change(now);
        }
    }

    /// The generation's screen profile was learned at `now`, after a spawn
    /// with none (Q35-18a): the redraw estimate takes `redraw` and counts the
    /// last size change before it, and `B` takes `cpr_wait_end` keeping its
    /// value ([`Backlog::adopt`]). The ledger, the FIFO and the records are
    /// untouched. `false`, changing nothing, once retired or for a generation
    /// that already has a redraw timing.
    pub fn adopt_profile(
        &self,
        cpr_wait_end: CprWaitEnd,
        redraw: &'static RedrawTiming,
        now: Instant,
    ) -> bool {
        let mut state = self.state.lock_recovering();
        if state.retired || !state.estimate.adopt(redraw, now) {
            return false;
        }
        state.backlog.adopt(cpr_wait_end);
        true
    }

    /// When the generation's redraw waits are expected to have run out, or
    /// `None` for no estimate (see
    /// [`QueryWaitEstimate::estimate`]). Writes nothing.
    pub fn query_wait_estimate(&self) -> Option<Instant> {
        self.state.lock_recovering().estimate.estimate()
    }

    /// Test seam: every fact the lock keeps, as text, so a test can tell
    /// whether a call changed any of them.
    #[cfg(test)]
    pub(crate) fn state_text(&self) -> String {
        format!("{:?}", *self.state.lock_recovering())
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
    /// whether it would be D3′-exempt. Exemption isn't pass-ahead
    /// eligibility: that also needs the answered query to be after a fence's
    /// trigger ([`Self::pass_ahead`], I-5).
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

    /// PRD #1666 part 3 (Q35-20): attach the generation's segments, which
    /// wait for `agent`'s return path, beside its records. Once, after the
    /// records, and never after retirement.
    pub fn attach_segments(&self, generation: u64, agent: segments::Agent) -> bool {
        let mut state = self.state.lock_recovering();
        if state.retired || state.records.is_none() || state.live.is_some() {
            return false;
        }
        state.live = Some(Live {
            segments: Segments::new(generation, agent),
            pair_open: false,
            explained: std::collections::VecDeque::new(),
            unexplained: std::collections::VecDeque::new(),
            forgotten: None,
            schedule: Schedule::default(),
            last_foreign_op: 0,
            probes: Vec::new(),
            reads: std::collections::VecDeque::new(),
            lapse_counted: None,
            recheck: None,
            parked: Vec::new(),
            next_ticket: 0,
        });
        true
    }

    /// Q35-21 (A-20d-1a, O-20.7b): from now on an `Armed` empty read's
    /// settlement doesn't apply the read at once. It is decided under the
    /// lock, as before, and parked with a ticket, and `wake` is called (under
    /// the lock: it must take none) so the observer reads the keymap again
    /// with no lock held and hands the verdict to
    /// [`Self::settle_keymap_recheck`]. `false`, changing nothing, with no
    /// segments attached or once retired.
    pub fn recheck_keymap_at_settlement(&self, wake: Arc<dyn Fn() + Send + Sync>) -> bool {
        let mut state = self.state.lock_recovering();
        if state.retired {
            return false;
        }
        match state.live.as_mut() {
            Some(live) => {
                live.recheck = Some(Recheck(wake));
                true
            }
            None => false,
        }
    }

    /// Q35-21 (A-20d-1a): the newest ticket of an empty read waiting for a
    /// keymap re-read, if any. A re-read taken after this answer settles
    /// every read parked through that ticket and none parked after it.
    /// `None` once retired.
    pub fn keymap_recheck_due(&self) -> Option<u64> {
        let state = self.state.lock_recovering();
        if state.retired {
            return None;
        }
        state
            .live
            .as_ref()
            .and_then(|live| live.parked.last())
            .map(|parked| parked.ticket)
    }

    /// Whether an empty read's settlement is waiting for its keymap
    /// re-read: a check still outstanding, so no new attempt is due.
    pub fn settling(&self) -> bool {
        let state = self.state.lock_recovering();
        !state.retired && state.live.as_ref().is_some_and(|l| !l.parked.is_empty())
    }

    /// Q35-21 (A-20d-1a, O-20.7b): the keymap re-read's verdict, `keymap`,
    /// read with no lock held after [`Self::keymap_recheck_due`] answered
    /// `through`. Under the lock, and only while the generation that parked
    /// them hasn't retired, each read parked through that ticket settles:
    /// with the keymap allowed it applies as an empty read does (the epoch's
    /// apply rule, then the schedule's empty); held, it is a failed check,
    /// counted unless the probe's lapse was counted already (R-D-U2-02).
    /// A read parked after `through` waits for a re-read of its own. How
    /// many settled.
    pub fn settle_keymap_recheck(
        &self,
        through: u64,
        keymap: Result<(), super::keymap::Hold>,
        now: Instant,
    ) -> usize {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        if state.retired {
            return 0;
        }
        let Some(live) = state.live.as_mut() else {
            return 0;
        };
        let (due, waiting): (Vec<Parked>, Vec<Parked>) =
            live.parked.drain(..).partition(|p| p.ticket <= through);
        live.parked = waiting;
        for parked in &due {
            let probe = parked.probe;
            tracing::debug!(
                target: "composer_probe",
                number = probe.number,
                ticket = parked.ticket,
                ?keymap,
                "empty read settled after its keymap re-read"
            );
            match keymap {
                Ok(()) => live.apply_empty(&mut state.epoch, probe),
                Err(_) if live.lapse_counted == Some(probe.number) => {}
                Err(_) => live.schedule.failed(now),
            }
        }
        due.len()
    }

    /// Whether the generation has segments attached (and hasn't retired).
    pub fn has_segments(&self) -> bool {
        let state = self.state.lock_recovering();
        !state.retired && state.live.is_some()
    }

    /// What the release driver waits on (Q35-20).
    pub fn segments_moved(&self) -> &tokio::sync::Notify {
        &self.moved
    }

    /// The gate held one more op (R-U2-03): the release driver is woken, so
    /// a segment that has already released passes it on as far as the next
    /// held candidate, rather than leaving it held until something else
    /// moves the segments. Called with the gate's lock and this one released.
    pub fn held_accepted(&self) {
        if self.has_segments() {
            self.moved.notify_one();
        }
    }

    /// The return markers the output hook read in one push, in output order
    /// and at offsets counted from the generation's birth, for the segments
    /// and the second `ExternalEdit` producer (Q35-20, PR U3-11), at `now`.
    /// Nothing without segments attached: a marker read before attachment
    /// can belong to no segment, and no record yet accounts for a pair.
    ///
    /// **The second producer.** A complete `?2004l` … `?2004h` pair, the
    /// shape of an editor session, that no record the deck made accounts for
    /// moves the epoch as an external edit when its `?2004h` is read: an
    /// editor the agent ran without the deck's helper may have changed the
    /// box. A record accounts for one pair whichever is read first, since
    /// the helper's report and the agent's output reach the daemon on
    /// different paths. A mode observation alone, or the tracker's boolean,
    /// releases and grants nothing (the Q35-02 condition): only the
    /// segments' own watch, after a trigger's written offset, is E1.
    pub fn on_return_markers(&self, seen: &[Seen], now: Instant) {
        if seen.is_empty() {
            return;
        }
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        let (Some(live), false) = (state.live.as_mut(), state.retired) else {
            return;
        };
        tracing::debug!(target: "composer_probe", ?seen, "return markers");
        for s in seen {
            match s.marker {
                Marker::PasteOff => live.pair_open = true,
                Marker::PasteOn if live.pair_open => {
                    live.pair_open = false;
                    live.expire_credits(now);
                    if live.explained.pop_front().is_none() {
                        let moved_to = state.epoch.record(None);
                        live.owe(now);
                        tracing::debug!(
                            target: "composer_probe",
                            moved_to,
                            unexplained = live.unexplained.len(),
                            "an unexplained pair moved the epoch"
                        );
                    } else {
                        tracing::debug!(
                            target: "composer_probe",
                            "a pair spent a record's credit"
                        );
                    }
                }
                _ => {}
            }
        }
        if let Some(records) = state.records.as_mut() {
            live.segments
                .on_output(records, &mut state.hazards, seen, now);
        }
        settle_live(state, now);
        drop(guard);
        self.moved.notify_one();
    }

    /// The daemon read an `Armed` invocation's export (Q35-20): whether it
    /// was empty, for the epoch's settlement of the probe it matches.
    pub fn note_read(&self, key: RecordKey, empty: bool) {
        self.note_box_read(
            key,
            if empty {
                BoxRead::Empty
            } else {
                BoxRead::NonEmpty
            },
        );
    }

    /// The daemon couldn't read an `Armed` invocation's export (Q151b): it
    /// settles as one read not empty did before Q147b, a timed recheck,
    /// whatever was typed during its check.
    pub fn note_unreadable(&self, key: RecordKey) {
        self.note_box_read(key, BoxRead::Unreadable);
    }

    fn note_box_read(&self, key: RecordKey, read: BoxRead) {
        let mut state = self.state.lock_recovering();
        if let Some(live) = state.live.as_mut() {
            if live.reads.len() == READS_KEPT {
                live.reads.pop_front();
            }
            live.reads.push_back((key, read));
        }
    }

    /// Whether a probe attempt is due at `now` (R2 §6.8).
    pub fn probe_due(&self, now: Instant) -> bool {
        let state = self.state.lock_recovering();
        let epoch = state.epoch.epoch();
        state
            .live
            .as_ref()
            .is_some_and(|live| live.parked.is_empty() && live.schedule.due(now, epoch))
    }

    /// A due attempt failed at `now`: refused by admission, or its job
    /// refused or failed at the writer.
    pub fn probe_failed(&self, now: Instant) {
        if let Some(live) = self.state.lock_recovering().live.as_mut() {
            live.schedule.failed(now);
        }
    }

    /// The writer refused a due attempt at `now` only because the screen
    /// moved since its scheduling ([`super::probe::Schedule::moved`]): a
    /// failure only for every [`super::probe::MOVED_PER_FAILURE`]th in a
    /// row.
    pub fn probe_moved(&self, now: Instant) {
        if let Some(live) = self.state.lock_recovering().live.as_mut() {
            live.schedule.moved(now);
        }
    }

    /// The test and `e2e` builds' schedule clock offset (Q105b.2): the
    /// pending wait ends `by` earlier. Answers the schedule as it then is,
    /// `(due, failures, broken)` at `now`; `None` with no live schedule.
    #[cfg(any(test, feature = "e2e"))]
    pub fn advance_schedule_for_e2e(
        &self,
        by: std::time::Duration,
        now: Instant,
    ) -> Option<(bool, u32, bool)> {
        let mut state = self.state.lock_recovering();
        let epoch = state.epoch.epoch();
        let live = state.live.as_mut()?;
        live.schedule.advance(by);
        Some((
            live.schedule.due(now, epoch),
            live.schedule.failures(),
            live.schedule.broken(),
        ))
    }

    /// The test and `e2e` builds' generation-bound records override
    /// (Q105b.2, [`Records::configure_for_e2e`]).
    #[cfg(any(test, feature = "e2e"))]
    pub fn configure_records_for_e2e(
        &self,
        policy: Option<record::EcPolicy>,
        cap: Option<usize>,
    ) -> Result<(), &'static str> {
        match self.state.lock_recovering().records.as_mut() {
            Some(records) => records.configure_for_e2e(policy, cap),
            None => Err("no records attached"),
        }
    }

    /// The test and `e2e` builds' read-only view of this generation's
    /// records (Q105b.2), read in one hold of the lock and moving nothing:
    /// the writer scope, the policy and cap, how many records are retained,
    /// and the witness. `None` with no records attached or once retired.
    #[cfg(any(test, feature = "e2e"))]
    pub fn records_for_e2e(&self) -> Option<RecordsForE2e> {
        let state = self.state.lock_recovering();
        if state.retired {
            return None;
        }
        state.records.as_ref().map(|r| RecordsForE2e {
            scope: r.scope().state(),
            policy: r.policy(),
            cap: r.cap(),
            retained: r.len(),
            witness: r.witness(),
        })
    }

    /// The doorbell tick's own lapsed probe (U3-02, core §4.7): whether its
    /// retry is due at `now`. A lone Claude probe that lapsed with nothing
    /// held behind it (`held_ops`, the gate's) is a failed attempt (R2
    /// §6.8): the schedule counts it once, so its retry waits the backoff a
    /// failure waits, and is then due when the schedule is and no empty
    /// read is parked for its keymap re-read. `false` for anything else,
    /// Codex included (only a Claude probe lapses).
    pub fn lapse_due(&self, held_ops: usize, now: Instant) -> bool {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        if state.retired || held_ops > 0 {
            return false;
        }
        let epoch = state.epoch.epoch();
        let Some(live) = state.live.as_mut() else {
            return false;
        };
        let Some(number) = live.segments.lapsed_number(now) else {
            return false;
        };
        if live.lapse_counted != Some(number) {
            live.lapse_counted = Some(number);
            live.schedule.failed(now);
            tracing::debug!(
                target: "composer_probe",
                number,
                "probe segment lapsed unmatched; a retry may replace it"
            );
        }
        // An empty read parked for its keymap re-read is a check still
        // outstanding, as `probe_due` reads it: a lapse retry started under
        // it could lapse in turn and leave the parked read's held verdict
        // counting a second failure (the reva21 review's U3-1).
        live.parked.is_empty() && live.schedule.due(now, epoch)
    }

    /// Start a probe request (U3-02): `None` while another is on its way.
    /// The request is over when the guard drops.
    pub fn begin_probe_request(self: &Arc<Self>) -> Option<ProbeRequest> {
        self.probe_request
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| ProbeRequest(Arc::clone(self)))
    }

    /// Whether the last attempt failed, so a due one is the schedule's
    /// retry.
    pub fn probe_retrying(&self) -> bool {
        self.state
            .lock_recovering()
            .live
            .as_ref()
            .is_some_and(|live| live.schedule.failures() > 0)
    }

    /// Open a probe's segment at `now` (core §4.5): its number and job, to
    /// publish and queue. `held`: the gate holds input, which forbids a
    /// lapse retry's supersession (core §4.7).
    pub fn segment_open_probe(
        &self,
        held: bool,
        now: Instant,
    ) -> Result<(u64, DeckJobId), segments::Refusal> {
        let mut state = self.state.lock_recovering();
        let live = match (state.retired, state.live.as_mut()) {
            (false, Some(live)) => live,
            _ => return Err(segments::Refusal::Retired),
        };
        let number = live.segments.open_probe(held, now)?;
        let job = DeckJobId(number);
        live.probes.push(Probe {
            number,
            job,
            e0: None,
        });
        // A superseded lapsed probe's ending.
        for ended in live.segments.take_ended() {
            live.probes.retain(|p| Some(p.number) != ended.number);
        }
        Ok((number, job))
    }

    /// Segment `number`'s trigger: the writer took it, the output at
    /// `offset` and the instant `at` as its job started (a lower bound, so
    /// no marker the agent writes after reading it can come before).
    pub fn segment_written(
        &self,
        number: u64,
        offset: u64,
        at: Instant,
        now: Instant,
    ) -> Result<(), segments::Refusal> {
        self.segment_step(now, |seg, records, fifo| {
            seg.written(records, fifo, number, offset, at, now)
        })
    }

    /// Segment `number`'s trigger was withdrawn or refused before the writer
    /// started it.
    pub fn segment_withdrawn(&self, number: u64) -> Result<(), segments::Refusal> {
        self.segment_step(Instant::now(), |seg, _, _| seg.withdrawn(number))
    }

    /// Segment `number`'s trigger write failed.
    pub fn segment_write_failed(&self, number: u64) -> Result<(), segments::Refusal> {
        self.segment_step(Instant::now(), |seg, _, _| seg.write_failed(number))
    }

    /// Record `key`'s editor child was observed running an executable other
    /// than the helper (core §4.6's `HumanG/Unarmed` row), with no check of
    /// whether it still runs: tests only (the revb21 review's U3-4).
    #[cfg(test)]
    pub fn segment_editor_running(&self, key: RecordKey, now: Instant) -> bool {
        self.segment_editor_pinned(key, None, now)
    }

    /// [`Self::segment_editor_running`] with the editor's identity as the
    /// observer pinned it, `(pid, start)` (A-20d-6): kept with the segment
    /// its record matches, and shown in its [`segments::View`]. Tests
    /// only, with no check of whether it still runs; the observer reports
    /// through [`Self::segment_editor_pinned_with`].
    #[cfg(test)]
    pub fn segment_editor_pinned(
        &self,
        key: RecordKey,
        editor: Option<RecordKey>,
        now: Instant,
    ) -> bool {
        self.segment_step(now, |seg, records, fifo| {
            Ok(seg.editor_pinned_for_test(records, fifo, key, editor, now))
        })
        .unwrap_or(false)
    }

    /// Record `key`'s editor child was observed running an executable other
    /// than the helper (core §4.6's `HumanG/Unarmed` row), with its identity
    /// as the observer pinned it, `(pid, start)`, kept with the segment its
    /// record matches and shown in its [`segments::View`], and the
    /// observer's check of whether the pinned editor still runs, which an
    /// early release to it asks under this input's lock (A-20d-6): the
    /// check must not block.
    pub fn segment_editor_pinned_with(
        &self,
        key: RecordKey,
        editor: segments::EditorPin,
        now: Instant,
    ) -> bool {
        self.segment_step(now, |seg, records, fifo| {
            Ok(seg.editor_pinned_with(records, fifo, key, editor, now))
        })
        .unwrap_or(false)
    }

    /// A held Ctrl+G at the front of the held input, with no segment open:
    /// its number, to publish before its trigger is written alone.
    pub fn segment_trigger(&self) -> Result<u64, segments::Refusal> {
        self.segment_step(Instant::now(), |seg, _, _| seg.open_held_trigger())
    }

    /// A person's Ctrl+G the planner cut from an op the gate let through
    /// with no fence closed, a liability being out (A-20d-5): it opens a
    /// `HumanG`, unnumbered until [`Self::segment_next_trigger`]
    /// ([`Segments::open_cut`]).
    pub fn segment_cut(&self) -> Result<(), segments::Refusal> {
        self.segment_step(Instant::now(), |seg, _, _| seg.open_cut())
    }

    /// The front `HumanG`'s number, once every segment ahead of it has
    /// ended: to publish before its trigger is submitted
    /// ([`Segments::next_trigger`]). `None` when the front isn't an
    /// unnumbered `HumanG`, or once retired.
    pub fn segment_next_trigger(&self) -> Option<u64> {
        self.segment_step(Instant::now(), |seg, _, _| {
            seg.next_trigger().ok_or(segments::Refusal::NoSegment)
        })
        .ok()
    }

    fn segment_step<T>(
        &self,
        now: Instant,
        step: impl FnOnce(&mut Segments, &mut Records, &mut HazardFifo) -> Result<T, segments::Refusal>,
    ) -> Result<T, segments::Refusal> {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        let (Some(live), Some(records), false) =
            (state.live.as_mut(), state.records.as_mut(), state.retired)
        else {
            return Err(segments::Refusal::Retired);
        };
        let result = step(&mut live.segments, records, &mut state.hazards);
        if let Err(refusal) = &result {
            tracing::debug!(target: "composer_probe", ?refusal, "segment step refused");
        }
        settle_live(state, now);
        drop(guard);
        self.moved.notify_one();
        result
    }

    /// What the front segment lets the held input do, `None` with no
    /// segments attached or once retired.
    pub fn segments_front(&self) -> Option<Front> {
        let state = self.state.lock_recovering();
        match (state.retired, state.live.as_ref()) {
            (false, Some(live)) => Some(live.segments.front()),
            _ => None,
        }
    }

    /// The front segment's release as the driver acts on it: what it lets
    /// the held input do ([`Self::segments_front`]) and where it stops short
    /// of the next held candidate ([`Segments::front_stop`], Q147b, Q151b),
    /// read in one hold of this lock.
    pub fn segments_release(&self) -> Option<(Front, segments::Stop)> {
        let state = self.state.lock_recovering();
        match (state.retired, state.live.as_ref()) {
            (false, Some(live)) => Some((
                live.segments.front(),
                state
                    .records
                    .as_ref()
                    .map_or(segments::Stop::Candidate, |records| {
                        live.segments.front_stop(records)
                    }),
            )),
            _ => None,
        }
    }

    /// Q152b: the front's number and the line with no end its release has
    /// left in Claude's cooked terminal ([`Segments::cooked_line`]); `None`
    /// unless its release stops at [`segments::Stop::Cooked`], with no
    /// segments attached, or once retired.
    pub fn segments_cooked_line(&self) -> Option<(u64, usize)> {
        let state = self.state.lock_recovering();
        match (state.retired, state.live.as_ref(), state.records.as_ref()) {
            (false, Some(live), Some(records)) => live.segments.cooked_line(records),
            _ => None,
        }
    }

    /// Q152b: what the release driver wrote into segment `number`'s cooked
    /// terminal left `run` bytes of a line with no end there
    /// ([`Segments::note_cooked_line`]).
    pub fn segment_note_cooked_line(&self, number: u64, run: usize) {
        let mut state = self.state.lock_recovering();
        if state.retired {
            return;
        }
        if let Some(live) = state.live.as_mut() {
            live.segments.note_cooked_line(number, run);
        }
    }

    /// Whether the front segment can never complete
    /// ([`Segments::stranded`]); `false` with no segments attached or once
    /// retired.
    pub fn segments_stranded(&self) -> bool {
        let state = self.state.lock_recovering();
        match (state.retired, state.live.as_ref(), state.records.as_ref()) {
            (false, Some(live), Some(records)) => live.segments.stranded(records),
            _ => false,
        }
    }

    /// The release driver's authority to refuse a person's Ctrl+G held
    /// behind a front segment that can never complete, with what is held
    /// after it ([`Segments::refuse_behind`]): whether the front is
    /// stranded, read and acted on in one hold of this lock, the segments
    /// waiting behind it ended `Refused`. Asked under the gate's lock, as a
    /// release's authority is.
    pub fn segment_refuse_behind(&self) -> bool {
        self.segment_step(Instant::now(), |seg, records, _| {
            Ok(seg.refuse_behind(records))
        })
        .unwrap_or(false)
    }

    /// The release driver's authority to refuse a held Ctrl+C, Ctrl+\ or
    /// Ctrl+Z, with what is held after it, behind a front whose release
    /// stops at one ([`Segments::refuse_signal_behind`], Q151b): whether it
    /// does, read and acted on in one hold of this lock, the segments
    /// waiting behind it ended `Refused`. Asked under the gate's lock, as a
    /// release's authority is.
    pub fn segment_refuse_signal(&self) -> bool {
        self.segment_step(Instant::now(), |seg, records, _| {
            Ok(seg.refuse_signal_behind(records))
        })
        .unwrap_or(false)
    }

    /// The open segments, as of `now`.
    pub fn segments_view(&self, now: Instant) -> Vec<segments::View> {
        let state = self.state.lock_recovering();
        match (state.retired, state.live.as_ref(), state.records.as_ref()) {
            (false, Some(live), Some(records)) => live.segments.view(records, now),
            _ => Vec::new(),
        }
    }

    /// The gate as probe admission reads it (core §4.7–§4.8): open with no
    /// fence and no segment; lapsed when its only segment is a Claude probe
    /// that lapsed with nothing held behind it (`held_ops` from the gate),
    /// with that probe's `E0`; closed otherwise.
    pub fn admission_fence(
        &self,
        fenced: bool,
        held_ops: usize,
        now: Instant,
    ) -> (admission::Fence, Option<u64>) {
        let state = self.state.lock_recovering();
        let Some(live) = state.live.as_ref().filter(|_| !state.retired) else {
            return (admission::Fence::Closed, None);
        };
        if live.segments.is_empty() {
            let fence = if fenced {
                admission::Fence::Closed
            } else {
                admission::Fence::Open
            };
            return (fence, None);
        }
        if held_ops == 0 && live.segments.lapsed_alone(now) {
            let e0 = live.probes.first().and_then(|p| p.e0);
            return (admission::Fence::Lapsed, e0);
        }
        (admission::Fence::Closed, None)
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
        self.with_records_live(|records, fifo, _| step(records, fifo))
    }

    /// [`Self::with_records`], the step also handed the generation's
    /// segments and what they read, when attached: one hold for a record's
    /// transition and what it means to them.
    fn with_records_live<T>(
        &self,
        step: impl FnOnce(&mut Records, &mut HazardFifo, Option<&mut Live>) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let mut guard = self.state.lock_recovering();
        let state = &mut *guard;
        let result = match (state.retired, state.records.as_mut()) {
            (false, Some(records)) => step(records, &mut state.hazards, state.live.as_mut()),
            _ => return Err(Refusal::Stale),
        };
        let now = self.step_now();
        // The audit's U3-4: a credit's life after its record drained or went
        // starts at the step that did it.
        if let (Some(live), Some(records)) = (state.live.as_mut(), state.records.as_ref()) {
            live.note_drained(records, now);
        }
        // Q35-20 (O-20.3): what the step allows the segments, in the same
        // hold.
        let live = state.live.is_some();
        if live {
            settle_live(state, now);
        }
        drop(guard);
        if live {
            self.moved.notify_one();
        }
        result
    }

    /// When a records step happened: the wall clock, or in a test the clock
    /// it set ([`Self::set_step_clock`]).
    fn step_now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = *self
            .step_clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return now;
        }
        Instant::now()
    }

    /// Test seam (Q35-21): the box observed empty through the epoch as it
    /// stands, as a probe that read it empty settles it (R3 §3.4), for a
    /// test about something other than how a box is observed: a ring's
    /// ticket, framing or ordering on a generation with no observer.
    #[cfg(test)]
    pub(crate) fn observe_empty_for_test(&self) {
        let mut state = self.state.lock_recovering();
        state.epoch.observed = state.epoch.epoch;
    }

    /// Test seam (R-C-U2-01): stamp each later records step at `now` rather
    /// than the wall clock, `None` to go back to it.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn set_step_clock(&self, now: Option<Instant>) {
        *self
            .step_clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
    }

    /// Test read of the pair accounting (R-C-U2-01, the audit's U3-1): the
    /// banked credits, oldest first, with when each one's record was first
    /// seen drained or gone; the remembered unexplained pair endings, oldest
    /// first; and the watermark of the forgotten ones. `None` without
    /// segments attached.
    #[cfg(test)]
    pub(crate) fn pair_accounting(&self) -> Option<PairAccounting> {
        let state = self.state.lock_recovering();
        state.live.as_ref().map(|live| PairAccounting {
            credits: live.explained.iter().map(|c| (c.key, c.drained)).collect(),
            unexplained: live.unexplained.iter().copied().collect(),
            forgotten: live.forgotten,
        })
    }

    /// See [`Records::observe`].
    pub fn record_observe(
        &self,
        obs: &record::Observation,
        now: Instant,
    ) -> Result<Recorded, Refusal> {
        // An editor session the deck saw accounts for one bracketed-paste
        // pair (PR U3-11's second producer), in the same hold that made its
        // record, so no pair can be read between the two (R-U2-04). A pair
        // that ended once its helper may have been born, read before the
        // record, already moved the epoch: the record accounts for that one
        // and banks nothing for a later pair. A tool's helper isn't the
        // composer's, so it explains none.
        let explains = obs.attribution != Attribution::NotOwned(NotOwned::Tool);
        self.with_records_live(|r, fifo, live| {
            let before = r.get(obs.key).is_some();
            let recorded = r.observe(fifo, obs, now);
            if explains
                && !before
                && r.get(obs.key).is_some()
                && let Some(live) = live
            {
                // Its helper was born at or after its floor: a pair that
                // ended before that floor wasn't its session.
                live.explain(obs.key, obs.birth_floor);
            }
            recorded
        })
    }

    /// See [`Records::refused_identity`].
    pub fn record_refused(
        &self,
        generation: u64,
        channel: Channel,
        decision: Option<Decision>,
        claim: Option<record::Claim>,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.with_records(|r, _| r.refused_identity(generation, channel, decision, claim, now))
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

    /// The agent's input grammar (Q83): strict until [`Self::set_input_grammar`].
    pub fn input_grammar(&self) -> InputGrammar {
        match self.input_grammar.load(Ordering::SeqCst) {
            1 => InputGrammar::ClaudeMeasured,
            _ => InputGrammar::Strict,
        }
    }

    /// The generation's release capture moved, and holds `grammar` for its
    /// agent: strict again after a conflict.
    pub fn set_input_grammar(&self, grammar: InputGrammar) {
        let value = match grammar {
            InputGrammar::Strict => 0,
            InputGrammar::ClaudeMeasured => 1,
        };
        self.input_grammar.store(value, Ordering::SeqCst);
    }

    /// The generation's drain-frontier queue (Q35-20a).
    pub fn frontier(&self) -> &FrontierQueue {
        &self.frontier
    }

    /// Ask for a drain frontier for `generation`, which `record` learns when
    /// it has one (Q35-20a). What can't be taken is settled `Missing` here.
    pub fn request_frontier(&self, generation: u64, record: Option<RecordKey>) -> Arc<Request> {
        let (request, settled) = self.frontier.request(generation, record);
        self.settle_frontier(&settled);
        request
    }

    /// The requests in `settled` had their outcome set by the caller: each
    /// one's record learns it. Called with no bus lock held, so this
    /// module's lock is never taken under it from the reader (A5 §3.2).
    pub fn settle_frontier(&self, settled: &[Arc<Request>]) {
        for request in settled {
            if let (Some(key), Some(outcome)) = (request.record(), request.outcome()) {
                let _ = self.record_frontier(request.generation(), key, outcome);
            }
        }
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

    /// See [`Records::drained_uncertain`].
    pub fn record_drained_uncertain(
        &self,
        generation: u64,
        key: RecordKey,
        now: Instant,
    ) -> Result<Drained, Refusal> {
        self.with_records(|r, fifo| r.drained_uncertain(fifo, generation, key, now))
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
        let pruned =
            self.with_records(|r, fifo| r.prune(fifo, generation, key, markers_deleted, now));
        // AU7 §2: the segment it matched keeps its facts after it goes.
        if let Ok(Some(fact)) = pruned {
            let mut guard = self.state.lock_recovering();
            let state = &mut *guard;
            if let (Some(live), Some(records)) = (state.live.as_mut(), state.records.as_mut()) {
                let _ = live
                    .segments
                    .drained(records, &mut state.hazards, fact, now);
            }
        }
        pruned
    }

    /// See [`Records::raise`].
    pub fn liability_raise(&self, generation: u64, stamp: Instant) -> Result<u32, Refusal> {
        self.with_records(|r, _| r.raise(generation, stamp))
    }

    /// See [`Records::lower`].
    pub fn liability_lower(&self, generation: u64, stamp: Instant) -> Result<u32, Refusal> {
        self.with_records(|r, _| r.lower(generation, stamp))
    }

    /// The liability count the records hold: what the arm page publishes.
    /// `None` with no records attached or once retired.
    pub fn liability_count(&self) -> Option<u32> {
        self.with_records(|r, _| Ok(r.count())).ok()
    }

    /// See [`Records::fact`].
    pub fn record_fact(&self, key: RecordKey) -> Option<ExitFact> {
        self.with_records(|r, _| Ok(r.fact(key))).ok().flatten()
    }

    /// Whether the generation retired.
    pub fn retired(&self) -> bool {
        self.state.lock_recovering().retired
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

    /// Probe admission for this generation at `now` (Q35-19, A4 §4): its own
    /// half read in one hold of the lock, then [`admission::admit`] with the
    /// caller's `outside`. Writes nothing: the hazard FIFO is read as of `now`
    /// without bringing its clock up to date, and O5 reads only the estimate.
    pub fn admit_probe(
        &self,
        outside: &admission::Outside,
        now: Instant,
    ) -> Result<(), admission::Refusal> {
        Self::admit_locked(&self.state.lock_recovering(), outside, now)
    }

    /// Admission over `state`, read in the caller's hold of the lock.
    fn admit_locked(
        state: &State,
        outside: &admission::Outside,
        now: Instant,
    ) -> Result<(), admission::Refusal> {
        let records = match (state.retired, state.records.as_ref()) {
            (false, Some(records)) => Some(records),
            _ => None,
        };
        let inside = admission::Inside {
            retired: state.retired,
            records: records.map_or(Err(Hold::Retired), Records::admits),
            exhausted: state.hazards.exhausted(),
            hazards_clear: state.hazards.clear_at(now),
            count: records.map_or(0, Records::count),
            epoch: state.epoch.epoch(),
            backlog: state.backlog.holds(),
            estimate: state.estimate.estimate(),
        };
        admission::admit(&inside, outside, now)
    }

    /// **The arm increment's records half** (Q35-19, A4 §4; the Q35-19
    /// review's U2): in one hold of this lock, ask admission with `outside`
    /// at `now`, check that the raise can be recorded, hand `publish` the
    /// raised count to store, and record the raise stamped after the store.
    /// Retirement takes this lock too, so it lands wholly before the arm,
    /// which then refuses `Retired` and publishes nothing, or wholly after
    /// it, when the arm stands complete and the records it raised go with
    /// the generation. A refusal calls `publish` not at all. `publish` runs
    /// under this lock: it may store to the page, which takes no lock, and
    /// must not take one.
    pub fn arm_probe(
        &self,
        generation: u64,
        outside: &admission::Outside,
        now: Instant,
        publish: impl FnOnce(u32),
    ) -> Result<u32, admission::Refusal> {
        let mut state = self.state.lock_recovering();
        Self::admit_locked(&state, outside, now)?;
        Self::raise_locked(&mut state, generation, publish).map_err(admission::Refusal::Raise)
    }

    /// The raise with no admission, for the `e2e` build's control verb: the
    /// same single critical section as [`Self::arm_probe`]'s, so retirement
    /// can't land between the store and the record either.
    pub fn raise_published(
        &self,
        generation: u64,
        publish: impl FnOnce(u32),
    ) -> Result<u32, Refusal> {
        Self::raise_locked(&mut self.state.lock_recovering(), generation, publish)
    }

    /// Check the raise, store it with `publish`, then record it, stamped
    /// after the store: nothing refuses once `publish` ran.
    fn raise_locked(
        state: &mut State,
        generation: u64,
        publish: impl FnOnce(u32),
    ) -> Result<u32, Refusal> {
        let records = match (state.retired, state.records.as_mut()) {
            (false, Some(records)) => records,
            _ => return Err(Refusal::Stale),
        };
        let raised = records.can_raise(generation)?;
        publish(raised);
        records.raise(generation, Instant::now())
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
        Self::retire_locked(&mut state);
        drop(state);
        self.moved.notify_one();
    }

    /// [`Self::retire`], first ending the records' pending episode at `now`
    /// and taking M-FIRST's event, in the same critical section: read before
    /// it the event misses that episode, after it there are no records.
    /// `None` with no records attached or once retired.
    pub fn retire_taking_first_stop(&self, now: Instant) -> Option<FirstStop> {
        let mut state = self.state.lock_recovering();
        let stop = match (state.retired, state.records.as_mut()) {
            (false, Some(records)) => {
                records.retire(now);
                Some(records.first_stop(now))
            }
            _ => None,
        };
        Self::retire_locked(&mut state);
        drop(state);
        self.moved.notify_one();
        stop
    }

    fn retire_locked(state: &mut State) {
        if let Some(live) = state.live.as_mut() {
            live.segments.retire();
            let _ = live.segments.take_ended();
            live.probes.clear();
            live.parked.clear();
        }
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

    /// Scenario: A Claude probe is written with no helper seen and lapses, and the doorbell tick counts the lapse as one failed attempt. The probe then completes with no decision ever known (the deferred settlement a completion hands over), and its settlement adds no second failure: the schedule keeps one failure and the first backoff step's deadline. A probe that never lapsed, settled the same way, counts its failure (the control).
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_a_lapsed_probes_completion_with_no_decision_counts_its_attempt_once() {
        use crate::composer_probe::probe::BACKOFF;
        use crate::composer_probe::query_ledger::T_TRIGGER;
        use crate::composer_probe::record::{EcPolicy, RecordKey, Records};
        let now = Instant::now();
        let Some(at) = now.checked_sub(T_TRIGGER + std::time::Duration::from_secs(1)) else {
            return;
        };
        let key = RecordKey {
            pid: 4242,
            start: 1,
        };
        let settle_undecided = |input: &GenerationInput, number: u64| {
            let mut guard = input.state.lock_recovering();
            let State {
                live,
                records,
                epoch,
                ..
            } = &mut *guard;
            live.as_mut().unwrap().settle_probe(
                epoch,
                records.as_ref().unwrap(),
                (number, key, None),
                now,
            );
        };
        let failures = |input: &GenerationInput| {
            input
                .advance_schedule_for_e2e(std::time::Duration::ZERO, now)
                .unwrap()
                .1
        };
        let probe = |written: Instant| {
            let input = GenerationInput::default();
            assert!(input.attach_records(Records::new(5, EcPolicy::Hold, now)));
            assert!(input.attach_segments(5, segments::Agent::Claude));
            let (number, _) = input.segment_open_probe(false, now).unwrap();
            input.segment_written(number, 0, written, now).unwrap();
            (input, number)
        };

        let (input, number) = probe(at);
        assert!(!input.lapse_due(0, now), "the lapse is a failure first");
        assert_eq!(failures(&input), 1);
        settle_undecided(&input, number);
        assert_eq!(
            failures(&input),
            1,
            "its no-decision completion is that failure"
        );
        assert!(!input.probe_due(now + BACKOFF[0] - std::time::Duration::from_millis(1)));
        assert!(input.probe_due(now + BACKOFF[0]));

        let (control, number) = probe(now);
        settle_undecided(&control, number);
        assert_eq!(failures(&control), 1, "an unlapsed probe's failure counts");
    }

    /// Scenario: A Claude probe lapses with nothing held behind it, its lapse is counted, and its backoff passes, so its lapse retry is due. An earlier probe's `Armed` empty read is then parked for its keymap re-read: while it is parked the lapse retry is not due, as a first attempt isn't, so one failure can't be counted twice (the reva21 review's U3-1); once the re-read settles it, the retry is due again.
    #[cfg(target_os = "linux")]
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_a_lapse_retry_waits_while_an_empty_read_is_parked() {
        use crate::composer_probe::probe::BACKOFF;
        use crate::composer_probe::query_ledger::T_TRIGGER;
        use crate::composer_probe::record::{EcPolicy, Records};
        let now = Instant::now();
        let Some(at) = now.checked_sub(T_TRIGGER + std::time::Duration::from_secs(1)) else {
            return;
        };
        let input = GenerationInput::default();
        assert!(input.attach_records(Records::new(5, EcPolicy::Hold, now)));
        assert!(input.attach_segments(5, segments::Agent::Claude));
        assert!(input.recheck_keymap_at_settlement(Arc::new(|| {})));
        let (number, _) = input.segment_open_probe(false, now).unwrap();
        input.segment_written(number, 0, at, now).unwrap();
        assert!(!input.lapse_due(0, now), "the lapse is a failure first");
        let later = now + BACKOFF[0];
        assert!(
            input.lapse_due(0, later),
            "its retry is due after the backoff"
        );
        {
            let mut state = input.state.lock_recovering();
            let live = state.live.as_mut().unwrap();
            let ticket = live.next_ticket;
            live.next_ticket += 1;
            live.parked.push(Parked {
                ticket,
                probe: Probe {
                    number: 0,
                    job: DeckJobId(0),
                    e0: None,
                },
            });
        }
        assert!(input.settling());
        assert!(
            !input.lapse_due(0, later),
            "no lapse retry while an empty read is parked"
        );
        let through = input.keymap_recheck_due().expect("parked");
        input.settle_keymap_recheck(through, Ok(()), later);
        assert!(!input.settling());
        assert!(input.lapse_due(0, later), "due again once it settled");
    }

    /// Scenario: A probe's helper decides `Armed` and the daemon reads its export empty. When the helper's claim on the composer is the deck's own, or the record has no claim, the empty read settles the box empty and the next attempt is due at once; when the claim is a tool's or `Claim::Unknown`, the read is not the deck's to apply, so the box reads non-empty and the next attempt waits.
    #[spec("inbox/probe/030")]
    #[test]
    fn probe_030_an_armed_empty_read_settles_empty_only_on_the_decks_own_claim() {
        use crate::composer_probe::query_ledger::HazardFifo;
        use crate::composer_probe::record::{
            Attribution, Channel, Claim, EcPolicy, Observation, RecordKey, Records,
        };
        let now = Instant::now();
        let key = RecordKey {
            pid: 4343,
            start: 1,
        };
        let settled = |claim: Option<Claim>| {
            let input = GenerationInput::default();
            assert!(input.attach_records(Records::new(5, EcPolicy::Hold, now)));
            assert!(input.attach_segments(5, segments::Agent::Claude));
            let (number, _) = input.segment_open_probe(false, now).unwrap();
            input.segment_written(number, 0, now, now).unwrap();
            {
                let mut guard = input.state.lock_recovering();
                let records = guard.records.as_mut().unwrap();
                records.raise(5, now).unwrap();
                records
                    .observe(
                        &mut HazardFifo::default(),
                        &Observation {
                            generation: 5,
                            key,
                            channel: Channel::Ipc,
                            attribution: Attribution::Owned,
                            claim,
                            decision: Some(Decision::Armed),
                            seg_seen: Some(number),
                            native_seen: None,
                            birth_floor: now,
                        },
                        now,
                    )
                    .unwrap();
                assert_eq!(records.get(key).unwrap().claim, claim, "{claim:?}");
            }
            input.note_read(key, true);
            // A failure first, so that only an empty settlement makes the
            // next attempt due at once.
            input.probe_failed(now);
            assert!(!input.probe_due(now));
            let mut guard = input.state.lock_recovering();
            let State {
                live,
                records,
                epoch,
                ..
            } = &mut *guard;
            live.as_mut().unwrap().settle_probe(
                epoch,
                records.as_ref().unwrap(),
                (number, key, Some(Decision::Armed)),
                now,
            );
            drop(guard);
            input.probe_due(now)
        };
        assert!(
            settled(Some(Claim::Owned)),
            "the deck's own claim reads empty"
        );
        assert!(settled(None), "no claim known reads empty");
        assert!(
            !settled(Some(Claim::Tool)),
            "a tool's claim reads non-empty"
        );
        assert!(
            !settled(Some(Claim::Unknown)),
            "an unknown claim reads non-empty"
        );
    }

    /// Scenario: A generation whose empty reads wait for a keymap re-read. Two probes' `Armed` empty reads are settled one after the other; each is parked with a newer ticket, wakes the observer once, and while any is parked no new attempt is due. A re-read whose tickets were read before the second parked settles only the first; the second waits for a re-read of its own, which, holding, counts one failed check. A read parked on a generation that then retires is settled by nothing (A-20d-1a's ownership across the I/O).
    #[spec("inbox/probe/014")]
    #[test]
    fn probe_014_a_parked_empty_read_settles_only_by_a_reread_after_it() {
        use crate::composer_probe::keymap::Hold as KeymapHold;
        use crate::composer_probe::query_ledger::HazardFifo;
        use crate::composer_probe::record::{Channel, Claim, EcPolicy, Observation, Records};
        use std::sync::atomic::AtomicUsize;
        let now = Instant::now();
        let generation = || {
            let input = GenerationInput::default();
            assert!(input.attach_records(Records::new(5, EcPolicy::Hold, now)));
            assert!(input.attach_segments(5, segments::Agent::Claude));
            let wakes = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&wakes);
            assert!(input.recheck_keymap_at_settlement(Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })));
            input
                .state
                .lock_recovering()
                .records
                .as_mut()
                .unwrap()
                .raise(5, now)
                .unwrap();
            (input, wakes)
        };
        // Probe `number`'s helper decided `Armed` and its export read empty:
        // its settlement, as `settle_live` runs it under the lock.
        let settle_empty = |input: &GenerationInput, number: u64| {
            let key = RecordKey {
                pid: 4000 + u32::try_from(number).unwrap(),
                start: 1,
            };
            let mut guard = input.state.lock_recovering();
            let State {
                live,
                records,
                epoch,
                ..
            } = &mut *guard;
            let records = records.as_mut().unwrap();
            records
                .observe(
                    &mut HazardFifo::default(),
                    &Observation {
                        generation: 5,
                        key,
                        channel: Channel::Ipc,
                        attribution: Attribution::Owned,
                        claim: Some(Claim::Owned),
                        decision: Some(Decision::Armed),
                        seg_seen: Some(number),
                        native_seen: None,
                        birth_floor: now,
                    },
                    now,
                )
                .unwrap();
            let live = live.as_mut().unwrap();
            live.probes.push(Probe {
                number,
                job: DeckJobId(number),
                e0: None,
            });
            live.reads.push_back((key, BoxRead::Empty));
            live.settle_probe(epoch, records, (number, key, Some(Decision::Armed)), now);
        };
        let failures = |input: &GenerationInput| {
            input
                .advance_schedule_for_e2e(std::time::Duration::ZERO, now)
                .unwrap()
                .1
        };

        let (input, wakes) = generation();
        input.probe_failed(now);
        input.advance_schedule_for_e2e(crate::composer_probe::probe::BACKOFF[0], now);
        assert!(input.probe_due(now), "due before anything parks");
        settle_empty(&input, 1);
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "the observer is woken");
        assert!(input.settling());
        assert!(
            !input.probe_due(now),
            "a parked read is a check outstanding"
        );
        assert_eq!(failures(&input), 1, "parked, not yet applied");
        let through = input.keymap_recheck_due().expect("one parked");
        // The second parks while the first re-read is reading the keymap.
        settle_empty(&input, 2);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert_eq!(input.settle_keymap_recheck(through, Ok(()), now), 1);
        assert_eq!(failures(&input), 0, "the first applied");
        assert!(input.settling(), "the second waits for its own re-read");
        assert!(!input.probe_due(now));
        let second = input.keymap_recheck_due().expect("the second");
        assert!(second > through);
        assert_eq!(
            input.settle_keymap_recheck(second, Err(KeymapHold::Changed), now),
            1
        );
        assert_eq!(failures(&input), 1, "a held keymap is a failed check");
        assert!(!input.settling());
        assert_eq!(input.keymap_recheck_due(), None);

        let (input, _) = generation();
        settle_empty(&input, 1);
        let through = input.keymap_recheck_due().expect("parked");
        input.retire();
        assert_eq!(
            input.settle_keymap_recheck(through, Ok(()), now),
            0,
            "a retired generation's parked read settles nothing"
        );
        assert!(!input.settling());
    }
}
