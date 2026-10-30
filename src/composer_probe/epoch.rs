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
//! A deck write with no job named ([`Writer::UnnamedDeck`], which every deck
//! write in the daemon is as of this commit) counts as foreign. Exempt reports
//! don't move it: a whole focus or SGR mouse report while the agent has that
//! report mode on (D3), and a whole DA1 or OSC 10/11 reply to a query the
//! ledger keeps (D3′). A CPR reply is never exempt (OQ3). Whether an op is
//! one whole report is read from the input framing at the op's start, kept
//! in enqueue order: a withdrawn job leaves that framing ahead of what the
//! PTY took, which Q35-15/16's physical tracker corrects.
//!
//! **What reads it, as of this commit.** The epoch's numbers: probe
//! admission ([`GenerationInput::admit_probe`], Q35-19), which nothing calls
//! in production before Q35-20 wires it, for a lapse retry's "nothing since
//! the lapsed byte". The card and the ring's reconcile read them from Q35-21
//! on; existing doorbell behaviour reads `PaneInputState` as before. The
//! ledger is read by a closed fence's acceptance, for whether a reply may
//! pass ahead ([`GenerationInput::pass_ahead`]), and a fence closes only
//! through the `e2e` build's control verb until a probe opens one (Q35-20).
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
//! held, none of which takes a lock, does I/O or calls back out. The output
//! and ingress locks aren't ranked, so the checker sees neither of those
//! edges: that neither is taken under this lock rests on that leaf code, and
//! a change that adds a callback here reviews those edges again.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::admission;
use super::framing::{Csi, Event, Framing, Report, Str, StringKind, Token, whole_report};
use super::frontier::{FrontierQueue, Request};
use super::planner::{Cut, Natives, first_cut, is_candidate};
use super::query_ledger::{
    Backlog, CprWaitEnd, EntryView, HazardFifo, HazardPop, Pop, QueryHistory, QueryKind,
    QueryLedger,
};
use super::query_wait::{QueryWaitEstimate, RedrawTiming};
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
    retired: bool,
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
}

/// [`GenerationInput::enqueue`] under a hold the caller already has.
fn enqueue_locked(state: &mut State, writer: Writer, bytes: &[u8]) -> Enqueued {
    plan_locked(state, writer, bytes, None).enqueued
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
    let mut cut = None;
    let mut numbered: Option<(u64, u64)> = None;
    for (i, &byte) in bytes.iter().enumerate() {
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
    let enqueued = match exempt {
        Some(report) => Enqueued::Exempt(report),
        None => {
            let owner = match writer {
                Writer::Deck(job) => Some(job),
                Writer::UnnamedDeck | Writer::Human | Writer::Answer => None,
            };
            Enqueued::Moved(state.epoch.record(owner))
        }
    };
    Planned {
        enqueued,
        cut,
        natives: numbered
            .zip(start)
            .map(|((first, last), start)| Natives { first, last, start }),
    }
}

/// One generation's input epoch, query ledger, `B`, hazard FIFO and redraw
/// estimate, under one leaf lock. See the module documentation.
#[derive(Debug)]
pub struct GenerationInput {
    state: RankedMutex<{ R6 }, State>,
    /// How many bytes the output hook has read, advanced by the hook before
    /// it hands over a push's notes, outside the lock: the offset a query is
    /// noted at, and a fence's trigger (Q35-15).
    output_offset: AtomicU64,
    /// Where native numbers are published (Q35-16): called with no lock of
    /// this module held.
    native_page: NativePage,
    /// The drain frontier's requests and its reader's wake (Q35-20a).
    frontier: FrontierQueue,
    /// Test seam: parks an acceptance between the epoch's move and the job
    /// becoming visible, so a test can see the one before the other.
    #[cfg(test)]
    accept_park: std::sync::Mutex<Option<AcceptPark>>,
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
                retired: false,
            }),
            output_offset: AtomicU64::new(0),
            native_page: NativePage::default(),
            frontier: FrontierQueue::default(),
            #[cfg(test)]
            accept_park: std::sync::Mutex::new(None),
        }
    }

    /// The output hook read `len` more bytes; called before it hands over
    /// that push's notes, so a query is noted at the end of its push.
    pub fn advance_output(&self, len: usize) {
        self.output_offset.fetch_add(len as u64, Ordering::SeqCst);
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
        stop
    }

    fn retire_locked(state: &mut State) {
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
