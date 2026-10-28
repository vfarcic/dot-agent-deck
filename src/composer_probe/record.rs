//! PRD #1666 part 3 (Q35-07; core §2.2–§2.4 and §2.6, amendments 1–6 of
//! Q35-core): the invocation records of one generation, what each one
//! settles, and the generation's writer scope ([`Records`]).
//!
//! **A record** ([`Record`]) is one helper process the daemon observed, keyed
//! by `(pid, start ticks)`, so a reused pid is another record. It keeps what
//! the channels told the daemon (an IPC request, a marker, a child scan, the
//! exit): the daemon's own attribution ([`Attribution`], computed by Q35-14a
//! while the helper was alive, never taken from the helper), the helper's
//! claim (recorded only), its decision, the segment and native numbers it
//! read, its birth floor, the first time an `Armed` decision was seen (A2
//! §3.3, P-E2's clock origin, never reset), the helper's report after its
//! editor ([`Report`]), the drain frontier's outcome ([`Frontier`], A3), its
//! settlement and its completion. Nothing here reads a process, a file or a
//! clock: every fact and time arrives as an input.
//!
//! **Settlement (core §2.4)** happens once per record. A liability is
//! consumed only when both D and O hold: D, an `Armed` decision on the IPC or
//! marker channel, or no decision on any channel with the interval proof at
//! the exit drain; O, the daemon's attribution `Owned`, with the helper's
//! claim `Owned` or absent. Everything else leaves the count alone. A second
//! decision, or one after the exit drain, changes nothing. A marker is an
//! observation of a decision; the export's read is the daemon's own (Q35-14a)
//! and nothing here carries it.
//!
//! **The interval proof** reads the count history ([`CountHistory`]): the
//! count was at least 1 at the birth floor and no recorded zero interval meets
//! `[birth floor, exit observed]`. A store to 0 is stamped with a clock read
//! before the store, a store from 0 with one read after it, so each recorded
//! zero interval contains the true one; every bound widens toward refusal. A
//! birth floor is the start ticks floored to their tick ([`birth_floor_ns`]),
//! so a helper born in the same tick as something it must follow can't be
//! shown to follow it ([`born_at_or_after`]): at tick granularity the birth
//! frontier refuses (RV7 §2).
//!
//! **Completion (A2 §3.1, A3 §3.4, A5, A6)** is what probe admission waits
//! for: a record attributed `NotOwned` needs none; any other is `Pending`
//! until a marker route completes it or a later `Owned` invocation subsumes
//! it. The marker routes are the caller's to recognise (E1/E1F for a
//! helper-only record, EC for an editor-bearing one, Q35-08/20); this module
//! refuses them while the writer scope isn't closed, refuses EC under
//! [`EcPolicy::Hold`], and refuses E1/E1F for a record that isn't helper-only
//! or isn't matched to a segment. Subsumption needs no marker: an `Owned`
//! record born after a record's exit, whose editor is known to have ended
//! (a helper-only decision, or a `Clean` or `Survivors` report), completes it.
//!
//! **The writer scope (A3 §4.3, A5 §1.4, A6 §1.3)** is `{open, undecided}`:
//! `undecided` counts the records with no decision yet, and `open` is a
//! sticky cause that only retirement clears. It is closed when `open` is unset
//! and nothing is undecided, pending while something is undecided, and open
//! once a cause is set; pending and open refuse the same things. A record
//! leaves `undecided` once, at its first decision or at its exit drain with
//! none, and when the count returns to zero with no cause set the step says so
//! (`rescan`), once, for the caller to re-run what pending refused. The
//! highest cause by precedence is kept ([`Cause`]).
//!
//! **Retention (core §2.6, A2 §3.3).** At most [`RECORD_CAP`] records; an
//! observation past it creates none ([`Recorded::Dropped`]). A record is
//! deleted only once it is settled, its exit drained, its completion no
//! longer pending, and its markers deleted. Its exit facts ([`ExitFact`]) are
//! handed out at its drain, before it can be deleted, so a segment keeps them
//! after the record is gone (AU7 §2). The count history keeps an entry while
//! an unsettled record's birth floor could still read it, compacts older ones
//! into a baseline, and holds at most [`HISTORY_CAP`] entries; at either cap
//! no probe is admitted ([`Records::admits`]).
//!
//! **The quiet clock (Q35-06's condition (b)).** Every step that can change
//! whether a record is pending takes the generation's hazard FIFO and feeds it
//! the pending fact with the step's own time, and a record's one hazard pop is
//! made from the same step, so the transition and the pause are one event. In
//! the daemon both live under the generation's input lock
//! (`super::epoch::GenerationInput`), which also orders them against the
//! output hook's latch and the physical tracker's candidates.
//!
//! **What reads it, as of this commit: nothing.** The handler, the drain and
//! the exit observation (Q35-14b), the segments (Q35-08, Q35-20) and
//! admission (Q35-19) call it later.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::query_ledger::{HazardFifo, HazardPop};

/// The most records a generation keeps (core §2.6).
pub const RECORD_CAP: usize = 64;

/// The most count-history entries a generation keeps (core §2.6).
pub const HISTORY_CAP: usize = 256;

/// The most writer-scope pending episodes a generation keeps one by one for
/// M-FIRST; later ones are counted.
pub const EPISODE_CAP: usize = 64;

/// A helper process: its pid and its start ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordKey {
    pub pid: u32,
    pub start: u64,
}

/// The channel an observation came by. The exit is its own step
/// ([`Records::exit`], [`Records::drained`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// A connection on the composer socket, read or not.
    Ipc,
    /// A marker file in `inv/<G>/`.
    Marker,
    /// A child scan's sighting.
    Scan,
}

/// The daemon's own attribution of an invocation (core §2.3), computed once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// Not computed yet.
    Pending,
    /// This generation's agent root `A` made it, with the export shape.
    Owned,
    NotOwned(NotOwned),
    /// The reads may describe another process: the helper exited during
    /// them, or its time namespace isn't the daemon's.
    Unattributable,
}

/// Why an invocation isn't `A`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotOwned {
    Unknown,
    Tool,
    /// The export path isn't the agent's export shape, or isn't the argv the
    /// request named.
    Shape,
}

/// The helper's own class for an invocation: recorded, never authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Owned,
    Tool,
    Unknown,
}

/// What the helper chose after its arm load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No editor.
    Armed,
    /// The user's editor.
    Unarmed,
    /// A tool's editor.
    Tool,
    /// A decision a newer helper names: not known to be helper-only.
    Unknown,
}

impl Decision {
    /// Whether this decision runs no editor.
    pub fn helper_only(self) -> bool {
        self == Decision::Armed
    }
}

/// What the helper's reap loop found of its editor's tree (amendment 4
/// §7.5), as its report carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tree {
    Clean,
    Survivors,
    Unknown,
}

/// The helper's report after its editor phase (A3 §3.2, A4 §7.5, A6 §2.3).
/// The editor's exit status or signal isn't on the wire: an editor that
/// exited and one that was signalled both report `Done`. A helper whose wait
/// failed (`WaitUnknown`), was killed, or lost its connection reports
/// nothing, which the exit drain reads as [`Report::None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    None,
    /// The editor ran and was reaped.
    Done(Tree),
    /// No editor ran: it couldn't be started (`errno`), or none was
    /// configured.
    SpawnFailed {
        errno: Option<i32>,
    },
    /// A report a newer helper names: read as no report.
    Unrecognised,
}

/// The drain frontier (A3 §3.3, A6 §3): requested on a `Clean` report, then
/// taken at offset `o` or found missing, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontier {
    NotApplicable,
    Requested,
    Taken { offset: u64 },
    Missing,
}

/// What became of a frontier request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontierOutcome {
    Taken { offset: u64 },
    Missing,
}

/// A record's settlement (core §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    Open,
    /// A liability was consumed: the count went down by one.
    Consumed,
    NotConsumed(NotConsumed),
}

/// Why a record consumed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotConsumed {
    /// `Armed` and `Owned`, with the count already 0.
    AlreadyZero,
    /// `Armed`, but not attributed `Owned`, or the claim disagreed: the
    /// liability stays.
    Unattributed,
    /// The user's or a tool's editor.
    Unarmed,
    /// A decision a newer helper names.
    UnknownDecision,
    /// No decision, and the exit didn't prove it read the arm.
    Unproven,
}

/// Whether probe admission still waits on a record (A2 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Pending,
    Complete {
        at: Instant,
    },
    Subsumed {
        by: RecordKey,
    },
    /// Attributed `NotOwned`: not the composer's.
    NotRequired,
}

/// A completion the caller recognised from markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// E1 (Claude) or E1F (Codex) after a matched helper-only record's
    /// trigger.
    HelperOnly,
    /// EC: the drain frontier, then the return markers after it.
    EditorBearing,
}

/// Whether EC is authority (amendment 4 §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcPolicy {
    /// EC completes nothing, and every editor-bearing invocation opens the
    /// writer scope.
    Hold,
    Authority,
}

/// The policy in production until the user decides R-XW′ (amendment 5).
pub const EC_POLICY: EcPolicy = EcPolicy::Hold;

/// Why the writer scope is open, lowest precedence first: the card shows the
/// highest that applies (amendment 6 §1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cause {
    /// An exit drained with no decision, or a sighting dropped at the cap.
    EditorUnconfirmed,
    /// An `Unarmed` or `Tool` decision, dropped or not (under `Hold`).
    EditorReturnUnchecked,
    /// No report, a tree the helper couldn't account for, or an unknown
    /// report after an editor-bearing decision.
    EditorEndUnknown,
    /// A `Survivors` report.
    EditorLeftWriter,
}

/// The writer scope's three states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeState {
    Closed,
    /// A seen helper hasn't decided yet.
    Pending,
    Open(Cause),
}

/// The generation's writer scope (amendment 6 §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriterScope {
    open: Option<Cause>,
    undecided: u32,
}

impl WriterScope {
    pub fn is_closed(&self) -> bool {
        self.open.is_none() && self.undecided == 0
    }

    pub fn state(&self) -> ScopeState {
        match (self.open, self.undecided) {
            (Some(cause), _) => ScopeState::Open(cause),
            (None, 0) => ScopeState::Closed,
            (None, _) => ScopeState::Pending,
        }
    }

    pub fn undecided(&self) -> u32 {
        self.undecided
    }

    fn set(&mut self, cause: Cause) {
        self.open = Some(self.open.map_or(cause, |c| c.max(cause)));
    }
}

/// Which channels have seen a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Channels {
    pub ipc: bool,
    pub marker: bool,
    pub scan: bool,
}

/// One invocation record. Read-only outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub key: RecordKey,
    pub observed: Channels,
    pub attribution: Attribution,
    pub claim: Option<Claim>,
    pub decision: Option<Decision>,
    pub seg_seen: Option<u64>,
    pub native_seen: Option<u64>,
    pub birth_floor: Instant,
    /// P-E2's origin: the first `Armed` observation's time.
    pub first_armed_seen: Option<Instant>,
    pub report: Report,
    pub frontier: Frontier,
    /// The segment this record matched, once.
    pub matched: Option<u64>,
    pub exit_observed: Option<Instant>,
    pub drained: bool,
    pub settlement: Settlement,
    pub completion: Completion,
    pub hazard_pop: HazardPop,
    /// Whether it still counts in the scope's `undecided`.
    undecided: bool,
}

impl Record {
    /// The helper ran no editor: an `Armed` decision or a failed start.
    pub fn helper_only(&self) -> bool {
        self.decision.is_some_and(Decision::helper_only)
            || matches!(self.report, Report::SpawnFailed { .. })
    }

    /// The editor is known to have ended (A3 §2 #7): no editor ran, or the
    /// helper reaped it and accounted for its tree.
    fn editor_ended(&self) -> bool {
        self.helper_only() || matches!(self.report, Report::Done(Tree::Clean | Tree::Survivors))
    }

    fn fact(&self, generation: u64) -> Option<ExitFact> {
        Some(ExitFact {
            generation,
            key: self.key,
            seg_seen: self.seg_seen,
            native_seen: self.native_seen,
            matched: self.matched,
            decision: self.decision,
            report: self.report,
            frontier: self.frontier,
            first_armed_seen: self.first_armed_seen,
            exit_observed: self.exit_observed?,
            settlement: self.settlement,
        })
    }
}

/// A record's facts at its exit drain, for a segment to keep after the record
/// is deleted (AU7 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitFact {
    pub generation: u64,
    pub key: RecordKey,
    pub seg_seen: Option<u64>,
    pub native_seen: Option<u64>,
    pub matched: Option<u64>,
    pub decision: Option<Decision>,
    pub report: Report,
    pub frontier: Frontier,
    pub first_armed_seen: Option<Instant>,
    pub exit_observed: Instant,
    pub settlement: Settlement,
}

/// One observation of a helper on a channel. A scan carries no decision,
/// claim or numbers; an IPC connection whose request hasn't been read
/// carries none yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// The generation the observation names (its `inv/<G>` directory, or the
    /// request's).
    pub generation: u64,
    pub key: RecordKey,
    pub channel: Channel,
    pub attribution: Attribution,
    pub claim: Option<Claim>,
    pub decision: Option<Decision>,
    pub seg_seen: Option<u64>,
    pub native_seen: Option<u64>,
    pub birth_floor: Instant,
}

/// What an observation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// It reached its record. `settlement` is the record's, whether this
    /// observation settled it or an earlier one did.
    Kept {
        settlement: Settlement,
        /// The writer scope just closed: re-run what pending refused, once.
        rescan: bool,
    },
    /// The record cap: no record was made, nothing consumed.
    Dropped,
}

/// What the exit drain settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drained {
    pub settlement: Settlement,
    pub fact: ExitFact,
    /// The writer scope just closed.
    pub rescan: bool,
}

/// What a report did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reported {
    /// A `Clean` report: take a drain frontier for the record.
    RequestFrontier,
    Recorded,
    /// The record already had a report: nothing changed.
    Duplicate,
}

/// Why a step was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Another generation's input, or a retired one's.
    Stale,
    NoRecord,
    /// The record's exit hasn't been observed.
    NotExited,
    /// A completion for a record that needs none, or has one.
    NotPending(Completion),
    /// The writer scope isn't closed.
    Scope(ScopeState),
    /// EC under [`EcPolicy::Hold`].
    Policy,
    /// E1/E1F for a record that ran an editor, or may have.
    NotHelperOnly,
    /// E1/E1F for a record matched to no segment.
    Unmatched,
    /// EC without its exit drained.
    NotDrained,
    /// EC without a `Clean` report or a failed start.
    NotClean,
    /// EC without a taken frontier.
    NoFrontier,
    /// A match for a record not attributed `Owned`.
    NotOwned,
    /// The record already matched this other segment.
    Matched(u64),
    /// A step the record's state no longer takes.
    Settled,
    /// The count can't move that way.
    Count,
}

/// Why no probe may be admitted, as far as the records go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    Retired,
    Scope(ScopeState),
    Incomplete(RecordKey),
    RecordCap,
    HistoryCap,
}

/// How a writer-scope pending episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Decided(Decision),
    ExitWithoutDecision,
    /// A cause was set while it was pending.
    Opened(Cause),
    Retired,
}

/// One pending episode, for M-FIRST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Episode {
    pub duration: Duration,
    pub resolved: Resolution,
}

/// M-FIRST's per-generation event (amendment 5 §3.6, amendment 6 §3.6): when
/// probing first stopped and why, and the pending episodes. No path or
/// message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstStop {
    /// The cause, and how long after the generation started.
    pub stopped: Option<(Cause, Duration)>,
    /// How long the generation has been observed (a censored generation has
    /// no stop).
    pub observed: Duration,
    pub episodes: Vec<Episode>,
    /// Episodes past [`EPISODE_CAP`].
    pub episodes_uncounted: u32,
}

/// The arm word's count and its history of transitions (core §2.4).
#[derive(Debug, Clone, Default)]
pub struct CountHistory {
    /// The count before the first entry.
    baseline: u32,
    /// Before this, compaction dropped entries: no proof can start earlier.
    exact_from: Option<Instant>,
    entries: VecDeque<(Instant, u32)>,
    latest: Option<Instant>,
}

impl CountHistory {
    pub fn count(&self) -> u32 {
        self.entries.back().map_or(self.baseline, |&(_, c)| c)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn count_at(&self, at: Instant) -> u32 {
        self.entries
            .iter()
            .rev()
            .find(|&&(stamp, _)| stamp <= at)
            .map_or(self.baseline, |&(_, c)| c)
    }

    /// The interval proof: the count was at least 1 at `birth_floor` and no
    /// recorded zero interval meets `[birth_floor, exit]`.
    pub fn proves(&self, birth_floor: Instant, exit: Instant) -> bool {
        exit >= birth_floor
            && self.exact_from.is_none_or(|from| birth_floor >= from)
            && self.count_at(birth_floor) >= 1
            && !self
                .entries
                .iter()
                .any(|&(stamp, c)| c == 0 && stamp >= birth_floor && stamp <= exit)
    }

    /// Record the count `count` at `stamp`: read before a store to 0, after a
    /// store from 0. A stamp earlier than one already recorded is read as
    /// that one (the stores follow their calls in order).
    fn set(&mut self, stamp: Instant, count: u32) {
        let stamp = self.latest.map_or(stamp, |l| l.max(stamp));
        self.latest = Some(stamp);
        if self.entries.len() >= HISTORY_CAP {
            // Fail closed: the dropped entry's interval can't be proved over.
            self.drop_front();
        }
        self.entries.push_back((stamp, count));
    }

    fn drop_front(&mut self) {
        if let Some((stamp, count)) = self.entries.pop_front() {
            self.baseline = count;
            self.exact_from = Some(stamp);
        }
    }

    /// Fold every entry no proof from `keep_from` on can read into the
    /// baseline: those followed by another entry stamped before it. With no
    /// `keep_from`, every entry.
    fn compact(&mut self, keep_from: Option<Instant>) {
        match keep_from {
            Some(from) => {
                // An entry at `from` itself is kept: a zero stamped at a
                // birth floor meets its interval.
                while self.entries.get(1).is_some_and(|&(next, _)| next < from) {
                    self.drop_front();
                }
            }
            None => {
                while !self.entries.is_empty() {
                    self.drop_front();
                }
            }
        }
    }
}

/// The start ticks of a process as a birth floor in nanoseconds of the boot
/// clock: floored to the tick, so at or before the true birth. `None` when it
/// doesn't fit or `ticks_per_sec` is 0.
pub fn birth_floor_ns(start_ticks: u64, ticks_per_sec: u64) -> Option<u64> {
    if ticks_per_sec == 0 {
        return None;
    }
    let ns = u128::from(start_ticks) * 1_000_000_000 / u128::from(ticks_per_sec);
    u64::try_from(ns).ok()
}

/// The birth frontier (core §4.5): a helper with no decision follows a
/// segment written at `written` only when its floor is at or after it. A
/// floor before it, the same tick included, refuses (RV7 §2).
pub fn born_at_or_after<T: Ord>(birth_floor: T, written: T) -> bool {
    birth_floor >= written
}

/// A marker file's name (core §2.6, as the helper writes it): its key,
/// decision, the numbers it read, and whether the daemon renamed it seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    pub key: RecordKey,
    pub decision: Decision,
    pub seen: Option<(u64, u64)>,
    pub renamed: bool,
}

/// Parse a marker name: `<pid>-<start>-<armed|unarmed|tool>`, optionally
/// `-<seg>-<native>`, optionally `.seen`, every number decimal digits only
/// (1–10 for the pid, 1–20 for the others) and in range. Anything else is
/// `None`, never partly read.
pub fn parse_marker(name: &str) -> Option<Marker> {
    let (body, renamed) = match name.strip_suffix(".seen") {
        Some(body) => (body, true),
        None => (name, false),
    };
    let mut parts = body.split('-');
    let pid = digits(parts.next()?, 10)?;
    let start = digits(parts.next()?, 20)?;
    let decision = match parts.next()? {
        "armed" => Decision::Armed,
        "unarmed" => Decision::Unarmed,
        "tool" => Decision::Tool,
        _ => return None,
    };
    let seen = match (parts.next(), parts.next()) {
        (None, _) => None,
        (Some(seg), Some(native)) => Some((digits(seg, 20)?, digits(native, 20)?)),
        (Some(_), None) => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(Marker {
        key: RecordKey {
            pid: u32::try_from(pid).ok()?,
            start,
        },
        decision,
        seen,
        renamed,
    })
}

fn digits(s: &str, max: usize) -> Option<u64> {
    // An empty field fails the parse.
    if s.len() > max || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// One generation's invocation records, count history and writer scope. See
/// the module documentation.
#[derive(Debug, Clone)]
pub struct Records {
    generation: u64,
    policy: EcPolicy,
    started: Instant,
    records: Vec<Record>,
    scope: WriterScope,
    history: CountHistory,
    retired: bool,
    pending_since: Option<Instant>,
    stopped: Option<(Cause, Duration)>,
    episodes: Vec<Episode>,
    episodes_uncounted: u32,
}

impl Records {
    /// Generation `generation`'s table, under `policy`, from `started`, with
    /// the count at 0.
    pub fn new(generation: u64, policy: EcPolicy, started: Instant) -> Self {
        Self {
            generation,
            policy,
            started,
            records: Vec::new(),
            scope: WriterScope::default(),
            history: CountHistory::default(),
            retired: false,
            pending_since: None,
            stopped: None,
            episodes: Vec::new(),
            episodes_uncounted: 0,
        }
    }

    pub fn scope(&self) -> WriterScope {
        self.scope
    }

    pub fn count(&self) -> u32 {
        self.history.count()
    }

    pub fn history(&self) -> &CountHistory {
        &self.history
    }

    pub fn get(&self, key: RecordKey) -> Option<Record> {
        self.records.iter().find(|r| r.key == key).copied()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Whether any record's completion is pending: the hazard FIFO's quiet
    /// clocks stop while one is (A2 §4.5).
    pub fn any_pending(&self) -> bool {
        self.records
            .iter()
            .any(|r| r.completion == Completion::Pending)
    }

    fn check(&self, generation: u64) -> Result<(), Refusal> {
        if self.retired || generation != self.generation {
            return Err(Refusal::Stale);
        }
        Ok(())
    }

    fn index(&self, key: RecordKey) -> Result<usize, Refusal> {
        self.records
            .iter()
            .position(|r| r.key == key)
            .ok_or(Refusal::NoRecord)
    }

    /// An observation of a helper. It makes the record if there is none (at
    /// the cap it makes none, and opens the writer scope unless it proves the
    /// helper helper-only), takes the first attribution, claim and numbers it
    /// carries, and settles the record on its first decision. A record seen
    /// with no decision counts as undecided until it decides or its exit is
    /// drained. Feeds `fifo` the pending fact and the record's one pop.
    pub fn observe(
        &mut self,
        fifo: &mut HazardFifo,
        obs: &Observation,
        now: Instant,
    ) -> Result<Recorded, Refusal> {
        self.check(obs.generation)?;
        let before = self.scope.state();
        let i = match self.index(obs.key) {
            Ok(i) => i,
            Err(_) if self.records.len() >= RECORD_CAP => {
                self.dropped(obs.decision);
                self.transition(before, now, None);
                return Ok(Recorded::Dropped);
            }
            Err(_) => {
                self.records.push(Record {
                    key: obs.key,
                    observed: Channels::default(),
                    attribution: Attribution::Pending,
                    claim: None,
                    decision: None,
                    seg_seen: None,
                    native_seen: None,
                    birth_floor: obs.birth_floor,
                    first_armed_seen: None,
                    report: Report::None,
                    frontier: Frontier::NotApplicable,
                    matched: None,
                    exit_observed: None,
                    drained: false,
                    settlement: Settlement::Open,
                    completion: Completion::Pending,
                    hazard_pop: HazardPop::Unspent,
                    undecided: true,
                });
                self.scope.undecided += 1;
                self.records.len() - 1
            }
        };
        let count = self.history.count();
        let r = &mut self.records[i];
        match obs.channel {
            Channel::Ipc => r.observed.ipc = true,
            Channel::Marker => r.observed.marker = true,
            Channel::Scan => r.observed.scan = true,
        }
        if r.attribution == Attribution::Pending {
            r.attribution = obs.attribution;
            if matches!(r.attribution, Attribution::NotOwned(_))
                && r.completion == Completion::Pending
            {
                r.completion = Completion::NotRequired;
            }
        }
        r.claim = r.claim.or(obs.claim);
        r.seg_seen = r.seg_seen.or(obs.seg_seen);
        r.native_seen = r.native_seen.or(obs.native_seen);
        let mut resolved = None;
        let mut consumed = false;
        // A decision after the exit drain changes nothing (core §2.4).
        if let Some(decision) = obs.decision.filter(|_| !r.drained) {
            if decision == Decision::Armed && r.first_armed_seen.is_none() {
                r.first_armed_seen = Some(now);
            }
            if r.decision.is_none() {
                r.decision = Some(decision);
            }
            if r.settlement == Settlement::Open {
                r.settlement = settle_on_decision(r, decision, count);
                consumed = r.settlement == Settlement::Consumed;
            }
            // Every decision observed counts for the scope, the first only
            // for `undecided`; the cause is set before the count goes down.
            if let Some(cause) = decision_cause(self.policy, decision) {
                self.scope.set(cause);
            }
            let r = &mut self.records[i];
            if r.undecided {
                r.undecided = false;
                self.scope.undecided -= 1;
                resolved = Some(Resolution::Decided(decision));
            }
        }
        if consumed {
            self.history.set(now, count - 1);
        }
        self.maybe_pop(fifo, i, now);
        self.subsume();
        let rescan = self.transition(before, now, resolved);
        self.after(fifo, now);
        Ok(Recorded::Kept {
            settlement: self.records[i].settlement,
            rescan,
        })
    }

    /// An observation dropped by the identity refusal (core §2.3): no record.
    /// It opens the writer scope only when it names a decision that isn't
    /// helper-only; a scan sighting, or a connection whose request wasn't
    /// read, names none, and changes nothing (A6 §1.3).
    pub fn refused_identity(
        &mut self,
        generation: u64,
        channel: Channel,
        decision: Option<Decision>,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.check(generation)?;
        let before = self.scope.state();
        if let Some(decision) = decision.filter(|d| !d.helper_only())
            && channel != Channel::Scan
        {
            self.scope.set(dropped_cause(decision));
        }
        self.transition(before, now, None);
        Ok(())
    }

    /// The cap's drop (A5 §1.4, A6 §1.3): an `Armed` observation opens
    /// nothing; anything else, a decisionless sighting or connection
    /// included, can't be shown helper-only.
    fn dropped(&mut self, decision: Option<Decision>) {
        match decision {
            Some(Decision::Armed) => {}
            Some(decision) => self.scope.set(dropped_cause(decision)),
            None => self.scope.set(Cause::EditorUnconfirmed),
        }
    }

    /// The helper's report after its editor, on the connection that carried
    /// its decision. The first report is kept. A `Clean` one asks for a drain
    /// frontier; one that leaves the editor's end unknown, or survivors,
    /// opens the writer scope under either policy.
    pub fn report(
        &mut self,
        fifo: &mut HazardFifo,
        generation: u64,
        key: RecordKey,
        report: Report,
        now: Instant,
    ) -> Result<Reported, Refusal> {
        self.check(generation)?;
        let i = self.index(key)?;
        let before = self.scope.state();
        let r = &mut self.records[i];
        if r.report != Report::None {
            return Ok(Reported::Duplicate);
        }
        r.report = report;
        let mut out = Reported::Recorded;
        match report {
            Report::Done(Tree::Clean) => {
                if r.frontier == Frontier::NotApplicable {
                    r.frontier = Frontier::Requested;
                    out = Reported::RequestFrontier;
                }
            }
            Report::Done(Tree::Survivors) => self.scope.set(Cause::EditorLeftWriter),
            Report::Done(Tree::Unknown) => self.scope.set(Cause::EditorEndUnknown),
            Report::SpawnFailed { .. } | Report::None | Report::Unrecognised => {}
        }
        self.subsume();
        self.transition(before, now, None);
        self.after(fifo, now);
        Ok(out)
    }

    /// The outcome of a frontier request, set once.
    pub fn frontier(
        &mut self,
        generation: u64,
        key: RecordKey,
        outcome: FrontierOutcome,
    ) -> Result<bool, Refusal> {
        self.check(generation)?;
        let i = self.index(key)?;
        let r = &mut self.records[i];
        if r.frontier != Frontier::Requested {
            return Ok(false);
        }
        r.frontier = match outcome {
            FrontierOutcome::Taken { offset } => Frontier::Taken { offset },
            FrontierOutcome::Missing => Frontier::Missing,
        };
        Ok(true)
    }

    /// The helper's exit was observed at `at` (the clock read after its pidfd
    /// turned readable). The first observation is kept.
    pub fn exit(
        &mut self,
        fifo: &mut HazardFifo,
        generation: u64,
        key: RecordKey,
        at: Instant,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.check(generation)?;
        let i = self.index(key)?;
        let r = &mut self.records[i];
        if r.exit_observed.is_none() {
            r.exit_observed = Some(at);
        }
        self.subsume();
        self.after(fifo, now);
        Ok(())
    }

    /// The record's exit drain finished: every channel has said what it will.
    /// Once: a record with no decision leaves `undecided` and is settled by
    /// its exit, and the writer scope opens for an ending that can't be shown
    /// clean. The record's exit facts are returned for its segment.
    pub fn drained(
        &mut self,
        fifo: &mut HazardFifo,
        generation: u64,
        key: RecordKey,
        now: Instant,
    ) -> Result<Drained, Refusal> {
        self.check(generation)?;
        let i = self.index(key)?;
        let before = self.scope.state();
        let count = self.history.count();
        let r = &mut self.records[i];
        let Some(exit) = r.exit_observed else {
            return Err(Refusal::NotExited);
        };
        let mut resolved = None;
        let mut consumed = false;
        if !r.drained {
            r.drained = true;
            if r.decision != Some(Decision::Armed) && r.frontier == Frontier::NotApplicable {
                r.frontier = Frontier::Missing;
            }
            if r.settlement == Settlement::Open {
                let proved = r.attribution == Attribution::Owned
                    && matches!(r.claim, None | Some(Claim::Owned))
                    && count > 0
                    && self.history.proves(r.birth_floor, exit);
                r.settlement = if proved {
                    consumed = true;
                    Settlement::Consumed
                } else {
                    Settlement::NotConsumed(NotConsumed::Unproven)
                };
            }
            if let Some(cause) = drain_cause(r) {
                self.scope.set(cause);
            }
            let r = &mut self.records[i];
            if r.undecided {
                r.undecided = false;
                self.scope.undecided -= 1;
                resolved = Some(Resolution::ExitWithoutDecision);
            }
            if consumed {
                self.history.set(now, count - 1);
            }
            if self.records[i].attribution == Attribution::Owned {
                let r = &mut self.records[i];
                fifo.pop_at_drain(&mut r.hazard_pop, r.birth_floor, now);
            }
        }
        self.subsume();
        let rescan = self.transition(before, now, resolved);
        self.after(fifo, now);
        let r = &self.records[i];
        Ok(Drained {
            settlement: r.settlement,
            fact: r.fact(self.generation).ok_or(Refusal::NotExited)?,
            rescan,
        })
    }

    /// Match the record to segment `seg` (core §4.5's last condition): only
    /// an `Owned` record, and only one segment.
    pub fn match_segment(
        &mut self,
        generation: u64,
        key: RecordKey,
        seg: u64,
    ) -> Result<(), Refusal> {
        self.check(generation)?;
        let i = self.index(key)?;
        let r = &mut self.records[i];
        if r.attribution != Attribution::Owned {
            return Err(Refusal::NotOwned);
        }
        match r.matched {
            Some(other) if other != seg => Err(Refusal::Matched(other)),
            _ => {
                r.matched = Some(seg);
                Ok(())
            }
        }
    }

    /// A completion the caller recognised by `route`, at `at`. Refused while
    /// the writer scope isn't closed, read in this same step.
    pub fn complete(
        &mut self,
        fifo: &mut HazardFifo,
        generation: u64,
        key: RecordKey,
        route: Route,
        at: Instant,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.check(generation)?;
        let i = self.index(key)?;
        let r = &self.records[i];
        if r.completion != Completion::Pending {
            return Err(Refusal::NotPending(r.completion));
        }
        if !self.scope.is_closed() {
            return Err(Refusal::Scope(self.scope.state()));
        }
        match route {
            Route::HelperOnly => {
                if !r.helper_only() {
                    return Err(Refusal::NotHelperOnly);
                }
                if r.exit_observed.is_none() {
                    return Err(Refusal::NotExited);
                }
                if r.matched.is_none() {
                    return Err(Refusal::Unmatched);
                }
            }
            Route::EditorBearing => {
                if self.policy == EcPolicy::Hold {
                    return Err(Refusal::Policy);
                }
                if !r.drained {
                    return Err(Refusal::NotDrained);
                }
                if !matches!(
                    r.report,
                    Report::Done(Tree::Clean) | Report::SpawnFailed { .. }
                ) {
                    return Err(Refusal::NotClean);
                }
                if !matches!(r.frontier, Frontier::Taken { .. }) {
                    return Err(Refusal::NoFrontier);
                }
            }
        }
        self.records[i].completion = Completion::Complete { at };
        self.after(fifo, now);
        Ok(())
    }

    /// Delete the record if it may go: settled, its exit drained, its
    /// completion not pending, and its markers deleted (`markers_deleted`,
    /// the caller's result; a failed deletion keeps the record). Its exit
    /// facts, or `None` when it stays.
    pub fn prune(
        &mut self,
        fifo: &mut HazardFifo,
        generation: u64,
        key: RecordKey,
        markers_deleted: bool,
        now: Instant,
    ) -> Result<Option<ExitFact>, Refusal> {
        self.check(generation)?;
        let i = self.index(key)?;
        let r = &self.records[i];
        if !markers_deleted
            || r.settlement == Settlement::Open
            || !r.drained
            || r.completion == Completion::Pending
        {
            return Ok(None);
        }
        let fact = r.fact(self.generation);
        self.records.remove(i);
        self.after(fifo, now);
        Ok(fact)
    }

    /// A probe published one more liability; `stamp` read after the store.
    pub fn raise(&mut self, generation: u64, stamp: Instant) -> Result<u32, Refusal> {
        self.check(generation)?;
        let count = self.history.count().checked_add(1).ok_or(Refusal::Count)?;
        self.history.set(stamp, count);
        self.compact();
        Ok(count)
    }

    /// A liability was withdrawn without a consumption (a take-back);
    /// `stamp` read before the store.
    pub fn lower(&mut self, generation: u64, stamp: Instant) -> Result<u32, Refusal> {
        self.check(generation)?;
        let count = self.history.count().checked_sub(1).ok_or(Refusal::Count)?;
        self.history.set(stamp, count);
        self.compact();
        Ok(count)
    }

    /// What the records say about admitting a probe (A2 §3.4, A4 §4's
    /// conjuncts 1 and 2, core §2.6's caps).
    pub fn admits(&self) -> Result<(), Hold> {
        if self.retired {
            return Err(Hold::Retired);
        }
        if self.records.len() >= RECORD_CAP {
            return Err(Hold::RecordCap);
        }
        if self.history.len() >= HISTORY_CAP {
            return Err(Hold::HistoryCap);
        }
        if !self.scope.is_closed() {
            return Err(Hold::Scope(self.scope.state()));
        }
        if let Some(r) = self
            .records
            .iter()
            .find(|r| r.completion == Completion::Pending)
        {
            return Err(Hold::Incomplete(r.key));
        }
        Ok(())
    }

    /// M-FIRST's event, as of `now`.
    pub fn first_stop(&self, now: Instant) -> FirstStop {
        FirstStop {
            stopped: self.stopped,
            observed: now.saturating_duration_since(self.started),
            episodes: self.episodes.clone(),
            episodes_uncounted: self.episodes_uncounted,
        }
    }

    /// The generation left its pane: every record, the scope and the history
    /// are dropped, and every later step is refused.
    pub fn retire(&mut self, now: Instant) {
        if self.retired {
            return;
        }
        if let Some(since) = self.pending_since.take() {
            self.episode(now.saturating_duration_since(since), Resolution::Retired);
        }
        self.retired = true;
        self.records.clear();
        self.scope = WriterScope::default();
        self.history = CountHistory::default();
    }

    /// The record's one pop by the `native_seen` it reported (A2 §4.4), once
    /// it is attributed `Owned`.
    fn maybe_pop(&mut self, fifo: &mut HazardFifo, i: usize, now: Instant) {
        let r = &mut self.records[i];
        if r.attribution == Attribution::Owned
            && r.hazard_pop == HazardPop::Unspent
            && let Some(seen) = r.native_seen
        {
            fifo.pop_reported(&mut r.hazard_pop, seen, now);
        }
    }

    /// A2 §3.2: a pending record whose editor is known to have ended is
    /// subsumed by an `Owned` record born after its exit.
    fn subsume(&mut self) {
        for i in 0..self.records.len() {
            let r = self.records[i];
            let Some(exit) = r.exit_observed else {
                continue;
            };
            if r.completion != Completion::Pending || !r.editor_ended() {
                continue;
            }
            if let Some(by) = self.records.iter().find(|s| {
                s.key != r.key && s.attribution == Attribution::Owned && s.birth_floor > exit
            }) {
                self.records[i].completion = Completion::Subsumed { by: by.key };
            }
        }
    }

    /// After a step: compact the history and feed the quiet clock.
    fn after(&mut self, fifo: &mut HazardFifo, now: Instant) {
        self.compact();
        fifo.records_pending(now, self.any_pending());
    }

    fn compact(&mut self) {
        let keep_from = self
            .records
            .iter()
            .filter(|r| r.settlement == Settlement::Open)
            .map(|r| r.birth_floor)
            .min();
        self.history.compact(keep_from);
    }

    /// Book the scope's move from `before`: an episode that ended, a stop, and
    /// whether the scope just closed.
    fn transition(
        &mut self,
        before: ScopeState,
        now: Instant,
        resolved: Option<Resolution>,
    ) -> bool {
        let after = self.scope.state();
        if after == ScopeState::Pending && self.pending_since.is_none() {
            self.pending_since = Some(now);
        }
        if after != ScopeState::Pending
            && let Some(since) = self.pending_since.take()
        {
            let resolved = match after {
                ScopeState::Open(cause) => resolved.unwrap_or(Resolution::Opened(cause)),
                _ => resolved.unwrap_or(Resolution::Retired),
            };
            self.episode(now.saturating_duration_since(since), resolved);
        }
        if let (ScopeState::Open(cause), None) = (after, self.stopped) {
            self.stopped = Some((cause, now.saturating_duration_since(self.started)));
        }
        before == ScopeState::Pending && after == ScopeState::Closed
    }

    fn episode(&mut self, duration: Duration, resolved: Resolution) {
        if self.episodes.len() < EPISODE_CAP {
            self.episodes.push(Episode { duration, resolved });
        } else {
            self.episodes_uncounted = self.episodes_uncounted.saturating_add(1);
        }
    }
}

/// Core §2.4's table, for a record's first decision with the count at
/// `count`.
fn settle_on_decision(r: &Record, decision: Decision, count: u32) -> Settlement {
    match decision {
        Decision::Armed => {
            let owned =
                r.attribution == Attribution::Owned && matches!(r.claim, None | Some(Claim::Owned));
            match (owned, count) {
                (false, _) => Settlement::NotConsumed(NotConsumed::Unattributed),
                (true, 0) => Settlement::NotConsumed(NotConsumed::AlreadyZero),
                (true, _) => Settlement::Consumed,
            }
        }
        Decision::Unarmed | Decision::Tool => Settlement::NotConsumed(NotConsumed::Unarmed),
        Decision::Unknown => Settlement::NotConsumed(NotConsumed::UnknownDecision),
    }
}

/// The cause a recorded decision sets: under `Hold`, any that isn't
/// helper-only (A5 §1.4, A6 §1.3); under `Authority`, none (the report
/// decides).
fn decision_cause(policy: EcPolicy, decision: Decision) -> Option<Cause> {
    match (policy, decision) {
        (_, Decision::Armed) | (EcPolicy::Authority, _) => None,
        (EcPolicy::Hold, Decision::Unarmed | Decision::Tool) => Some(Cause::EditorReturnUnchecked),
        (EcPolicy::Hold, Decision::Unknown) => Some(Cause::EditorUnconfirmed),
    }
}

/// The cause a dropped observation naming `decision` sets, under either
/// policy: no record is left to learn how it ended, so what it may have
/// opened is held (`Armed` never reaches here).
fn dropped_cause(decision: Decision) -> Cause {
    match decision {
        Decision::Unarmed | Decision::Tool => Cause::EditorReturnUnchecked,
        Decision::Armed | Decision::Unknown => Cause::EditorUnconfirmed,
    }
}

/// The cause a record's exit drain sets, under either policy (A3 §4.3, A4
/// §7.4, A6 §1.3): no decision at all, or an editor-bearing decision whose
/// helper didn't report its tree clean.
fn drain_cause(r: &Record) -> Option<Cause> {
    match (r.decision, r.report) {
        (None, _) => Some(Cause::EditorUnconfirmed),
        (Some(Decision::Armed), _) => None,
        (_, Report::Done(Tree::Clean) | Report::SpawnFailed { .. }) => None,
        (_, Report::Done(Tree::Survivors)) => Some(Cause::EditorLeftWriter),
        (_, Report::Done(Tree::Unknown) | Report::None | Report::Unrecognised) => {
            Some(Cause::EditorEndUnknown)
        }
    }
}
