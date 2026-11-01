//! PRD #1666 part 3 (Q35-08; core §4.5–§4.7 and §4.9, amendments 1–6 of
//! Q35-core): a generation's segments, what releases the bytes held behind
//! each, and when each is complete ([`Segments`]).
//!
//! **A segment** is one Ctrl+G the deck writes and waits for the agent to
//! act on: a [`Kind::Probe`], the deck's own check, or a [`Kind::HumanG`], a
//! person's Ctrl+G that arrived while the gate held. It has a number (the
//! arm page's `seg_seq`, handed out here when it is published), the output
//! offset and instant at which the writer reported its trigger accepted, and
//! at most one matched invocation record ([`super::record`]). A `HumanG`
//! waits, unnumbered, until every segment ahead of it has ended; the
//! caller then publishes its number and writes its trigger alone
//! ([`Segments::next_trigger`]). So at most one trigger is outstanding
//! (core §4.5's strict serialization), apart from a lapsed probe's, which
//! its retry supersedes and D5 says the agent never reads (core §4.7).
//!
//! **What the number shows, and what it doesn't.** The number is published
//! before the trigger is submitted. A helper that read `S` read it after
//! `S` was published; that says nothing about whether `S`'s trigger had been
//! written, so a match by number is not evidence that the agent read the
//! deck's byte. A match rests on serialization (no other trigger was
//! outstanding when `S` was published), on P-E′ (an `Owned`, export-shaped
//! invocation is the composer acting on a key from its own input) and on
//! D5, whose residual (a native Ctrl+G written before the probe and read
//! after it) can match a probe's segment: core §4.9 states that effect (CL-19).
//!
//! **Matching (core §4.5).** The outstanding written segment matches the
//! one `Owned` record that read its number, or, failing that, the one
//! `Owned` record with no decision and no number on any channel whose birth
//! floor is at or after the trigger's written instant (the birth frontier,
//! which refuses a floor in the same tick). Two candidates of either kind
//! match nothing. A record matched by its birth that later names another
//! number holds its segment for good ([`Stage::Mismatched`]).
//!
//! **The return markers.** The output hook (`super::paste_mode::OutputHook`)
//! reads them in its own framing, every byte from the generation's birth, so
//! a marker's offset is in the coordinate its triggers and frontiers are in
//! (R-U2-02); [`ReturnMarkers`] is the same reading on its own, for a caller
//! that holds no hook. Either reads the output through the
//! output grammar and reports `ESC [ ? 2004 l`, `ESC [ ? 2004 h` and
//! `ESC [ ? 2026 h`, each spelled exactly, with the offset of its ESC: the
//! frame is [`super::epoch`]'s marker predicate itself, and the two paste
//! markers are held to the same spelling (amendment 1 §2.2's "exact grammar
//! for both"). A combined `?1049;2026h` or a gap between the ESC and the `[`
//! is no marker, which holds. [`ReturnWatch`] keeps a segment's progress
//! past a lower bound:
//! - **E1** (core §4.5): a `?2004l` at or after the trigger's written offset,
//!   then a `?2004h`; a later `?2004l` before completion starts the pair over.
//!   **E1F** (amendment 1 §2.2): E1, then the first `?2026h` after its
//!   `?2004h`. E1 alone never completes a Codex segment.
//! - **EC** (amendment 3 §3.4, amendment 4 §7.2): a `?2004h` whose ESC is at
//!   or after the drain frontier `o`, and on Codex a `?2026h` after it.
//!
//! **Release and completion (core §4.6, amendments 1, 2 and 5).** Release
//! lets the bytes behind a segment go, up to the next held candidate;
//! completion lets the next segment's trigger be written. Every completion
//! here is a record completion through [`Records::complete`], so its
//! refusals are the record's: nothing completes by markers while the writer
//! scope is `Pending` or `Open`, EC completes nothing under
//! [`super::record::EcPolicy::Hold`] (the production policy, which also
//! covers a native `SpawnFailed`), and E1/E1F needs a matched, helper-only
//! record. A segment completes only when its own matched record's route is
//! accepted, and only once that record's exit has been drained: a record
//! subsumed by a later invocation doesn't complete its segment, which then
//! holds until retirement. The rows:
//! - Claude `Probe`: released at its record's `Armed` or `Unarmed` decision;
//!   complete at E1 (helper-only) or EC.
//! - Codex `Probe`, and Codex `HumanG` stage 2 `Armed`: released at
//!   completion, E1F.
//! - Claude `HumanG` stage 2 `Armed`: released at the decision; complete at
//!   E1.
//! - `HumanG` stage 2 `Unarmed`: released once the decision is `Unarmed` and
//!   the editor child was observed running; complete at EC. When its record
//!   then drains `WaitUnknown`, that release ends for what is still held
//!   (R-U2-06): only retirement ends it.
//! - A segment's return markers read while its trigger is in flight (after
//!   its number is published, before the writer's report reaches here) are
//!   kept, and read once the trigger is reported written; a failed or
//!   withdrawn trigger discards them (R-U2-01).
//! - A record whose report is `SpawnFailed` before its release: released at
//!   completion (E1 on Claude, E1F on Codex). `WaitUnknown` (an
//!   editor-bearing decision with no report at the drain) opens the writer
//!   scope, so only retirement ends its segment.
//!
//! A `Tool` or unknown decision on a matched record releases nothing before
//! completion. Nothing here releases or completes on a timer: the lapse
//! ([`Stage::Lapsed`], a Claude `Probe` unmatched past [`T_TRIGGER`]) only
//! lets one retry probe replace it while it holds nothing, and
//! [`NOT_RETURNED`] only changes what the view says (E5, E5′).
//!
//! **Endings** ([`Outcome`]) are truthful: `Written` only for a completed
//! segment, `TakenBack` and `Cancelled` only for a trigger never submitted
//! or withdrawn before it was reported written (the caller withdraws only a
//! job the writer hasn't started), `WriteFailed` for a trigger whose write failed and `Gone` for
//! the segments its op still carried, `Superseded` for a lapsed probe its
//! retry replaced, and `Retired` for everything at retirement.
//!
//! **The view** ([`Segments::view`]) publishes `AwaitingPostFlush` and
//! `AwaitingReturn` only while the segment holds: once a segment has
//! released, it reads `Released` until it completes. Each view carries the
//! writer scope's state, so an `EditorLeftWriter` (or other) cause is
//! published with the stage.
//!
//! **What reads it** (Q35-20): an installed generation's input keeps its
//! segments beside its records (`super::epoch::GenerationInput`), feeds them
//! the output hook's markers and every record step, and its release driver
//! (`crate::agent_pty`) does what [`Segments::front`] allows to the held
//! input. No production generation is `Armable` before activation, so none
//! opens a segment there. M-PF1 (Q77) hasn't run, so this is observation
//! code: the premises of E1F's rows are unverified on real agents.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::epoch::sync_begin;
use super::framing::{Csi, Event, Framing, Token};
use super::query_ledger::{HazardFifo, T_TRIGGER};
use super::record::{
    self, Attribution, Completion, Decision, ExitFact, Frontier, Record, RecordKey, Records,
    Report, Route, ScopeState, born_at_or_after,
};

/// How long a held segment waits past its helper's drained exit (E5), or
/// past E1 for its frame (E5′), before the view says it hasn't returned
/// (core §7). Display only: it releases nothing.
pub const NOT_RETURNED: Duration = Duration::from_secs(5);

/// The most segments a generation keeps; the planner refuses an op that
/// would open more.
pub const SEGMENT_CAP: usize = 64;

/// The most return markers a segment keeps while its trigger is in flight
/// (R-U2-01): the latest are kept. An E1 needs only the last disable and
/// the enable after it, so dropping older ones can only hold, never
/// release.
pub const IN_FLIGHT_MARKERS: usize = 64;

/// Whose return path a generation's segments wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// Returns with `?2004h` (M43b).
    Claude,
    /// Returns with `?2004h`, flushes its input, then draws (`?2026h`).
    Codex,
}

/// A segment's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The deck's own check.
    Probe,
    /// A person's Ctrl+G held behind an earlier segment.
    HumanG,
}

/// A return marker in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// `ESC [ ? 2004 l`.
    PasteOff,
    /// `ESC [ ? 2004 h`.
    PasteOn,
    /// `ESC [ ? 2026 h`.
    Frame,
}

/// A marker and the output offset of its ESC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    pub marker: Marker,
    pub start: u64,
}

/// The return marker a whole output CSI is, if any. The frame is
/// [`super::epoch`]'s marker predicate; the paste markers are spelled as
/// exactly: the `?` marker, the one parameter `2004`, no intermediate, the
/// bytes contiguous from the ESC, outside a paste.
pub fn return_marker(token: &Token<'_>, csi: &Csi<'_>) -> Option<Marker> {
    if sync_begin(token, csi) {
        return Some(Marker::Frame);
    }
    let exact = csi.private == Some(b'?')
        && csi.params == b"2004"
        && csi.intermediates.is_empty()
        && csi.literal
        && !token.in_paste;
    match csi.final_byte {
        b'h' if exact => Some(Marker::PasteOn),
        b'l' if exact => Some(Marker::PasteOff),
        _ => None,
    }
}

/// Reads a generation's output for return markers, however it is split,
/// counting the bytes it was fed.
#[derive(Debug, Clone)]
pub struct ReturnMarkers {
    framing: Framing,
    fed: u64,
    /// The count can't advance: nothing more is reported.
    exhausted: bool,
}

impl Default for ReturnMarkers {
    fn default() -> Self {
        Self {
            framing: Framing::output(),
            fed: 0,
            exhausted: false,
        }
    }
}

/// The return marker a whole output CSI is, with the offset of its ESC,
/// when the byte that completed it is at output offset `end`.
pub fn seen_ending_at(token: &Token<'_>, csi: &Csi<'_>, end: u64) -> Option<Seen> {
    let marker = return_marker(token, csi)?;
    let start = end.checked_sub(MARKER_LEN - 1)?;
    Some(Seen { marker, start })
}

impl ReturnMarkers {
    /// Read the next output bytes, appending each marker they complete.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Seen>) {
        for &byte in bytes {
            if self.exhausted {
                return;
            }
            let at = self.fed;
            self.framing.feed(byte, |token| {
                if let Event::Csi(csi) = token.event
                    && let Some(seen) = seen_ending_at(&token, &csi, at)
                {
                    out.push(seen);
                }
            });
            match self.fed.checked_add(1) {
                Some(fed) => self.fed = fed,
                None => self.exhausted = true,
            }
        }
    }

    /// The bytes read so far: the next byte's offset.
    pub fn fed(&self) -> u64 {
        self.fed
    }
}

/// Every return marker's length, `ESC [ ? <4 digits> <final>`: its bytes
/// are contiguous from its ESC, so its start is this far behind its final.
const MARKER_LEN: u64 = 8;

/// The return markers past a lower bound: E1 and E1F after a trigger, or EC
/// after a drain frontier. A marker whose ESC is before the bound doesn't
/// count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReturnWatch {
    from: u64,
    /// E1's: a `?2004l` comes first.
    disable: bool,
    off: Option<u64>,
    on: Option<u64>,
    frame: Option<u64>,
}

impl ReturnWatch {
    /// E1 for a trigger written at output offset `from`.
    pub fn e1(from: u64) -> Self {
        Self {
            from,
            disable: true,
            off: None,
            on: None,
            frame: None,
        }
    }

    /// EC after the drain frontier `o`.
    pub fn ec(o: u64) -> Self {
        Self {
            from: o,
            disable: false,
            off: None,
            on: None,
            frame: None,
        }
    }

    /// The next marker, in output order.
    pub fn feed(&mut self, seen: Seen) {
        if seen.start < self.from {
            return;
        }
        match seen.marker {
            Marker::PasteOff if self.disable => {
                self.off = Some(seen.start);
                self.on = None;
                self.frame = None;
            }
            Marker::PasteOn
                if self.on.is_none()
                    && (!self.disable || self.off.is_some_and(|off| seen.start > off)) =>
            {
                self.on = Some(seen.start);
            }
            Marker::Frame if self.frame.is_none() && self.on.is_some_and(|on| seen.start > on) => {
                self.frame = Some(seen.start);
            }
            _ => {}
        }
    }

    /// The enable has followed: E1's pair, or EC's `?2004h`.
    pub fn enabled(&self) -> bool {
        self.on.is_some()
    }

    /// The markers `agent`'s return needs: the enable on Claude, the enable
    /// then a frame on Codex.
    pub fn returned(&self, agent: Agent) -> bool {
        match agent {
            Agent::Claude => self.on.is_some(),
            Agent::Codex => self.frame.is_some(),
        }
    }
}

/// What the front segment lets the held input do ([`Segments::front`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Front {
    /// No segment is open.
    None,
    /// The front segment holds everything behind it.
    Held,
    /// The front segment released: what is held may go as far as the next
    /// held candidate, which waits for the segment to complete.
    Released,
    /// The front is a `HumanG` waiting for its trigger.
    Trigger,
}

/// How a segment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Its trigger was written and its matched invocation's return
    /// completed it: what was behind it is released.
    Written,
    /// A probe's trigger was withdrawn before the writer started it.
    TakenBack,
    /// A person's Ctrl+G whose op was cancelled before its trigger was
    /// started.
    Cancelled,
    /// A lapsed probe its retry replaced.
    Superseded,
    /// The generation retired.
    Retired,
    /// Its trigger's write failed.
    WriteFailed,
    /// Its op's write failed at an earlier trigger.
    Gone,
}

/// One segment's ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub number: Option<u64>,
    pub kind: Kind,
    pub op: Option<u64>,
    pub outcome: Outcome,
}

/// Where a segment stands, as the view publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Behind an earlier segment, unnumbered.
    Waiting,
    /// Numbered and submitted; the writer hasn't reported it.
    Submitted,
    /// Written, with no matched record.
    Unmatched,
    /// A Claude probe unmatched past [`T_TRIGGER`]: one retry may replace
    /// it while it holds nothing.
    Lapsed,
    /// Matched; its record hasn't drained, or its return markers haven't
    /// followed.
    Matched,
    /// A matched record named another segment's number after its match.
    Mismatched,
    /// Codex, helper-only: E1 followed, its frame hasn't, or wasn't
    /// accepted.
    AwaitingPostFlush,
    /// An editor-bearing record drained; EC hasn't completed it.
    AwaitingReturn,
    /// What was behind it has gone; it isn't complete.
    Released,
}

/// One segment as published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    pub number: Option<u64>,
    pub kind: Kind,
    pub stage: Stage,
    pub record: Option<RecordKey>,
    /// The matched record's exit facts, kept after the record is deleted.
    pub fact: Option<ExitFact>,
    /// The writer scope as the view was taken.
    pub scope: ScopeState,
    /// EC's markers followed the frontier: computed, and logged by the
    /// caller, whether or not the policy let it complete.
    pub ec: bool,
    /// E5 or E5′: [`NOT_RETURNED`] has passed in this stage.
    pub not_returned: bool,
}

/// Why a step was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The generation retired.
    Retired,
    /// Another generation's fact.
    Stale,
    /// A segment is outstanding, so no probe.
    Busy,
    /// No segment holds, so a person's Ctrl+G is native.
    NotHeld,
    /// [`SEGMENT_CAP`].
    Full,
    /// The segment number can't advance.
    Exhausted,
    /// No segment has that number.
    NoSegment,
    /// The trigger isn't submitted, or was already reported.
    NotSubmitted,
}

#[derive(Debug, Clone)]
struct Segment {
    kind: Kind,
    op: Option<u64>,
    number: Option<u64>,
    /// The output offset and instant its trigger was reported accepted.
    written: Option<(u64, Instant)>,
    /// Return markers read while its trigger was in flight: numbered, its
    /// write not yet reported (R-U2-01). The agent can read the trigger and
    /// answer before the writer's report reaches here, so they are kept,
    /// and read once it is reported written, against its offset; a trigger
    /// that fails or is withdrawn takes them with it. The latest
    /// [`IN_FLIGHT_MARKERS`] are kept.
    in_flight: VecDeque<Seen>,
    record: Option<RecordKey>,
    mismatched: bool,
    fact: Option<ExitFact>,
    editor_running: bool,
    e1: Option<ReturnWatch>,
    ec: Option<ReturnWatch>,
    /// When E1's enable followed (E5′'s origin).
    enabled_at: Option<Instant>,
    /// When its record was first seen drained (E5's origin).
    drained_at: Option<Instant>,
    released: bool,
    /// Its release has been withdrawn for what is still held (R-U2-06): it
    /// released to the user's editor while that ran, and its record then
    /// drained `WaitUnknown`, an editor-bearing decision with no report.
    /// What went before stays the editor's; what is still held waits for
    /// retirement (Q104b's OQ5).
    cut_off: bool,
    /// A probe whose matched decision was handed to the epoch's settlement
    /// ([`Segments::take_decided_probes`]).
    decided: bool,
}

impl Segment {
    fn new(kind: Kind, op: Option<u64>, number: Option<u64>) -> Self {
        Self {
            kind,
            op,
            number,
            written: None,
            in_flight: VecDeque::new(),
            record: None,
            mismatched: false,
            fact: None,
            editor_running: false,
            e1: None,
            ec: None,
            enabled_at: None,
            drained_at: None,
            released: false,
            cut_off: false,
            decided: false,
        }
    }

    fn end(&self, outcome: Outcome) -> Ended {
        Ended {
            number: self.number,
            kind: self.kind,
            op: self.op,
            outcome,
        }
    }
}

/// One generation's segments, in order. See the module documentation.
#[derive(Debug, Clone)]
pub struct Segments {
    generation: u64,
    agent: Agent,
    segments: VecDeque<Segment>,
    /// The last number published; 0 before the first.
    last: u64,
    ended: Vec<Ended>,
    /// Probes that completed before their decision was handed to the epoch's
    /// settlement, or after it was handed back ([`Self::undecide`]): their
    /// number, record and decision, for [`Self::take_completed_probes`]
    /// (R-U2-05). A completion removes the segment, so it would otherwise
    /// take the settlement with it.
    completed: Vec<(u64, RecordKey, Option<Decision>)>,
    /// Records whose editor child was seen running before a segment matched
    /// them (R-U2-01's kind: the observer reports it once, and a trigger's
    /// written report can still be on its way): a segment that matches one
    /// takes it then. The latest [`super::record::RECORD_CAP`] are kept.
    editors: VecDeque<RecordKey>,
    retired: bool,
}

impl Segments {
    pub fn new(generation: u64, agent: Agent) -> Self {
        Self {
            generation,
            agent,
            segments: VecDeque::new(),
            last: 0,
            ended: Vec::new(),
            completed: Vec::new(),
            editors: VecDeque::new(),
            retired: false,
        }
    }

    fn live(&self) -> Result<(), Refusal> {
        if self.retired {
            return Err(Refusal::Retired);
        }
        Ok(())
    }

    fn next_number(&self) -> Result<u64, Refusal> {
        self.last.checked_add(1).ok_or(Refusal::Exhausted)
    }

    /// Whether `seg` is a Claude probe still unmatched past [`T_TRIGGER`]
    /// since its trigger was written (the tie isn't a lapse).
    fn lapsed(&self, seg: &Segment, now: Instant) -> bool {
        self.agent == Agent::Claude
            && seg.kind == Kind::Probe
            && seg.record.is_none()
            && !seg.mismatched
            && seg
                .written
                .is_some_and(|(_, at)| now.saturating_duration_since(at) > T_TRIGGER)
    }

    /// Open a probe, returning the number to publish before its trigger is
    /// submitted. Refused while a segment is outstanding, except one lapsed
    /// Claude probe while nothing is held behind it (`held`, the gate's),
    /// which its retry supersedes (core §4.7). The rest of admission is the
    /// caller's.
    pub fn open_probe(&mut self, held: bool, now: Instant) -> Result<u64, Refusal> {
        self.live()?;
        let retry = match self.segments.front() {
            None => false,
            Some(seg) if self.segments.len() == 1 && !held && self.lapsed(seg, now) => true,
            Some(_) => return Err(Refusal::Busy),
        };
        let number = self.next_number()?;
        if retry && let Some(old) = self.segments.pop_front() {
            self.ended.push(old.end(Outcome::Superseded));
        }
        self.last = number;
        self.segments
            .push_back(Segment::new(Kind::Probe, None, Some(number)));
        Ok(number)
    }

    /// A person's Ctrl+G the planner cut from op `op` behind the open
    /// segments: it waits, unnumbered. With nothing open it isn't held.
    pub fn open_human_g(&mut self, op: u64) -> Result<(), Refusal> {
        self.live()?;
        if self.segments.is_empty() {
            return Err(Refusal::NotHeld);
        }
        if self.segments.len() >= SEGMENT_CAP {
            return Err(Refusal::Full);
        }
        self.segments
            .push_back(Segment::new(Kind::HumanG, Some(op), None));
        Ok(())
    }

    /// A person's Ctrl+G a release reached at the front of the held input
    /// with no segment open (Q35-20): every segment ahead of it has ended,
    /// so it is a `HumanG` whose trigger may be written now, numbered here.
    /// The caller publishes the number, then submits the trigger alone. With
    /// a segment open it waits behind it instead ([`Self::open_human_g`]),
    /// and this refuses [`Refusal::Busy`].
    pub fn open_held_trigger(&mut self) -> Result<u64, Refusal> {
        self.live()?;
        if !self.segments.is_empty() {
            return Err(Refusal::Busy);
        }
        let number = self.next_number()?;
        self.last = number;
        self.segments
            .push_back(Segment::new(Kind::HumanG, None, Some(number)));
        Ok(number)
    }

    /// What the front segment lets the held input do now (Q35-20): nothing
    /// while it holds, go as far as the next held candidate once it has
    /// released, and with no segment open, the same, the candidate there
    /// then opening the next segment. A front `HumanG` still unnumbered
    /// asks for its trigger.
    pub fn front(&self) -> Front {
        match self.segments.front() {
            None => Front::None,
            Some(seg) if seg.number.is_none() => Front::Trigger,
            Some(seg) if seg.released && !seg.cut_off => Front::Released,
            Some(_) => Front::Held,
        }
    }

    /// Whether the only open segment is a Claude probe that lapsed (core
    /// §4.7), so admission may read the gate as [`super::admission::Fence::Lapsed`]
    /// when nothing is held behind it. The writer epoch of its own byte is
    /// the caller's.
    pub fn lapsed_alone(&self, now: Instant) -> bool {
        self.segments.len() == 1 && self.segments.front().is_some_and(|s| self.lapsed(s, now))
    }

    /// The open segments' count.
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// The probes whose matched record has decided, each once (Q35-20, R3
    /// §3.4's apply rule): the caller settles the epoch from each.
    pub fn take_decided_probes(&mut self, records: &Records) -> Vec<(u64, RecordKey, Decision)> {
        let mut decided = Vec::new();
        for seg in &mut self.segments {
            if seg.kind != Kind::Probe || seg.decided {
                continue;
            }
            let (Some(number), Some(key)) = (seg.number, seg.record) else {
                continue;
            };
            let decision = records
                .get(key)
                .and_then(|r| r.decision)
                .or_else(|| seg.fact.and_then(|f| f.decision));
            if let Some(decision) = decision {
                seg.decided = true;
                decided.push((number, key, decision));
            }
        }
        decided
    }

    /// The probes that completed with their settlement still to do, each
    /// once (R-U2-05): its number, matched record and decision, `None` when
    /// none was ever known. Their records have drained, so nothing they wait
    /// for is still coming.
    pub fn take_completed_probes(&mut self) -> Vec<(u64, RecordKey, Option<Decision>)> {
        std::mem::take(&mut self.completed)
    }

    /// Hand probe `number`'s decision to [`Self::take_decided_probes`] again
    /// later: its settlement waits for something its record doesn't hold yet.
    pub fn undecide(&mut self, number: u64) {
        if let Some(seg) = self
            .segments
            .iter_mut()
            .find(|s| s.kind == Kind::Probe && s.number == Some(number))
        {
            seg.decided = false;
        }
    }

    /// The number of the next `HumanG` whose trigger may be written, once
    /// every segment ahead of it has ended: publish it, then submit the
    /// trigger alone.
    pub fn next_trigger(&mut self) -> Option<u64> {
        if self.retired {
            return None;
        }
        let number = self.next_number().ok()?;
        let seg = self.segments.front_mut()?;
        if seg.number.is_some() {
            return None;
        }
        seg.number = Some(number);
        self.last = number;
        Some(number)
    }

    fn submitted(&mut self, number: u64) -> Result<usize, Refusal> {
        self.live()?;
        let i = self
            .segments
            .iter()
            .position(|s| s.number == Some(number))
            .ok_or(Refusal::NoSegment)?;
        if self.segments[i].written.is_some() {
            return Err(Refusal::NotSubmitted);
        }
        Ok(i)
    }

    /// The writer took segment `number`'s trigger, with the output at
    /// `offset`, at `at`.
    pub fn written(
        &mut self,
        records: &mut Records,
        fifo: &mut HazardFifo,
        number: u64,
        offset: u64,
        at: Instant,
        now: Instant,
    ) -> Result<(), Refusal> {
        let i = self.submitted(number)?;
        let seg = &mut self.segments[i];
        seg.written = Some((offset, at));
        seg.e1 = Some(ReturnWatch::e1(offset));
        // What the agent wrote while the report was on its way: read now,
        // the watch's own bound deciding what counts (R-U2-01).
        let kept = std::mem::take(&mut seg.in_flight);
        if i == 0 {
            for s in kept {
                Self::feed_front(seg, records, s, now);
            }
        }
        self.evaluate(records, fifo, now);
        Ok(())
    }

    /// Segment `number`'s trigger write failed: it ends `WriteFailed`, and
    /// the segments its op still carried end `Gone`.
    pub fn write_failed(&mut self, number: u64) -> Result<(), Refusal> {
        let i = self.submitted(number)?;
        let Some(seg) = self.segments.remove(i) else {
            return Err(Refusal::NoSegment);
        };
        self.ended.push(seg.end(Outcome::WriteFailed));
        if let Some(op) = seg.op {
            self.end_where(|s| s.op == Some(op) && s.number.is_none(), Outcome::Gone);
        }
        Ok(())
    }

    /// Segment `number`'s trigger was withdrawn before the writer started
    /// it (its job's claim): a probe ends `TakenBack`, a person's Ctrl+G
    /// `Cancelled`.
    pub fn withdrawn(&mut self, number: u64) -> Result<(), Refusal> {
        let i = self.submitted(number)?;
        let Some(seg) = self.segments.remove(i) else {
            return Err(Refusal::NoSegment);
        };
        let outcome = match seg.kind {
            Kind::Probe => Outcome::TakenBack,
            Kind::HumanG => Outcome::Cancelled,
        };
        self.ended.push(seg.end(outcome));
        Ok(())
    }

    /// Op `op` was cancelled: its waiting Ctrl+G presses end `Cancelled`.
    /// How many.
    pub fn cancelled(&mut self, op: u64) -> usize {
        self.end_where(
            |s| s.op == Some(op) && s.number.is_none(),
            Outcome::Cancelled,
        )
    }

    fn end_where(&mut self, which: impl Fn(&Segment) -> bool, outcome: Outcome) -> usize {
        let before = self.segments.len();
        let mut kept = VecDeque::with_capacity(before);
        for seg in self.segments.drain(..) {
            if which(&seg) {
                self.ended.push(seg.end(outcome));
            } else {
                kept.push_back(seg);
            }
        }
        self.segments = kept;
        before - self.segments.len()
    }

    /// Return markers the output hook read, in output order.
    pub fn on_output(
        &mut self,
        records: &mut Records,
        fifo: &mut HazardFifo,
        seen: &[Seen],
        now: Instant,
    ) {
        if self.retired {
            return;
        }
        for &s in seen {
            if let Some(seg) = self.segments.front_mut() {
                if seg.written.is_some() {
                    Self::feed_front(seg, records, s, now);
                } else if seg.number.is_some() {
                    // Its trigger is in flight: kept until it is reported.
                    if seg.in_flight.len() == IN_FLIGHT_MARKERS {
                        seg.in_flight.pop_front();
                    }
                    seg.in_flight.push_back(s);
                }
            }
            self.evaluate(records, fifo, now);
        }
    }

    /// One return marker for the front segment, whose trigger was reported
    /// written.
    fn feed_front(seg: &mut Segment, records: &Records, s: Seen, now: Instant) {
        // The caller publishes the frontier before it feeds the output after
        // it (amendment 4 §7.2). One learned later counts only the markers
        // fed after it, which holds.
        if seg.ec.is_none()
            && let Some(r) = seg.record.and_then(|k| records.get(k))
            && let Frontier::Taken { offset } = r.frontier
        {
            seg.ec = Some(ReturnWatch::ec(offset));
        }
        if let Some(w) = seg.e1.as_mut() {
            w.feed(s);
            seg.enabled_at = match (w.enabled(), seg.enabled_at) {
                (true, None) => Some(now),
                (true, at) => at,
                (false, _) => None,
            };
        }
        if let Some(w) = seg.ec.as_mut() {
            w.feed(s);
        }
    }

    /// A record step happened (an observation, a report, a frontier, an
    /// exit, a subsumption, or the writer scope closing): match, release
    /// and complete what it now allows.
    pub fn settle(&mut self, records: &mut Records, fifo: &mut HazardFifo, now: Instant) {
        if !self.retired {
            self.evaluate(records, fifo, now);
        }
    }

    /// A record's exit drain finished: its facts are kept by the segment it
    /// matched, for after the record is deleted (AU7 §2).
    pub fn drained(
        &mut self,
        records: &mut Records,
        fifo: &mut HazardFifo,
        fact: ExitFact,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.live()?;
        if fact.generation != self.generation {
            return Err(Refusal::Stale);
        }
        self.evaluate(records, fifo, now);
        if let Some(seg) = self
            .segments
            .iter_mut()
            .find(|s| s.record == Some(fact.key))
        {
            seg.fact = Some(fact);
        }
        self.evaluate(records, fifo, now);
        Ok(())
    }

    /// Record `key`'s editor child was observed running an executable other
    /// than the helper. Whether a segment matched to it took it.
    pub fn editor_running(
        &mut self,
        records: &mut Records,
        fifo: &mut HazardFifo,
        key: RecordKey,
        now: Instant,
    ) -> bool {
        if self.retired {
            return false;
        }
        self.evaluate(records, fifo, now);
        let Some(seg) = self.segments.iter_mut().find(|s| s.record == Some(key)) else {
            if !self.editors.contains(&key) {
                if self.editors.len() == record::RECORD_CAP {
                    self.editors.pop_front();
                }
                self.editors.push_back(key);
            }
            return false;
        };
        seg.editor_running = true;
        self.evaluate(records, fifo, now);
        true
    }

    /// The generation retired: every segment ends `Retired`, and every
    /// later step is refused.
    pub fn retire(&mut self) {
        if self.retired {
            return;
        }
        self.end_where(|_| true, Outcome::Retired);
        self.retired = true;
    }

    /// The endings since the last call, in order.
    pub fn take_ended(&mut self) -> Vec<Ended> {
        std::mem::take(&mut self.ended)
    }

    /// The open segments, in order, as of `now`.
    pub fn view(&self, records: &Records, now: Instant) -> Vec<View> {
        let scope = records.scope().state();
        self.segments
            .iter()
            .map(|seg| {
                let record = seg.record.and_then(|k| records.get(k));
                let (stage, since) = self.stage(seg, record.as_ref(), now);
                View {
                    number: seg.number,
                    kind: seg.kind,
                    stage,
                    record: seg.record,
                    fact: seg.fact,
                    scope,
                    ec: seg.ec.is_some_and(|w| w.returned(self.agent)),
                    not_returned: since
                        .is_some_and(|at| now.saturating_duration_since(at) >= NOT_RETURNED),
                }
            })
            .collect()
    }

    /// A segment's stage, and since when a held, returning one has waited.
    fn stage(
        &self,
        seg: &Segment,
        record: Option<&Record>,
        now: Instant,
    ) -> (Stage, Option<Instant>) {
        if seg.number.is_none() {
            return (Stage::Waiting, None);
        }
        if seg.written.is_none() {
            return (Stage::Submitted, None);
        }
        if seg.mismatched {
            return (Stage::Mismatched, None);
        }
        if seg.released && !seg.cut_off {
            return (Stage::Released, None);
        }
        if seg.record.is_none() {
            let stage = if self.lapsed(seg, now) {
                Stage::Lapsed
            } else {
                Stage::Unmatched
            };
            return (stage, None);
        }
        let (drained, helper_only) = match (record, seg.fact) {
            (Some(r), _) => (r.drained, r.helper_only()),
            (None, Some(f)) => (true, fact_helper_only(&f)),
            (None, None) => (false, false),
        };
        if !drained {
            return (Stage::Matched, None);
        }
        if !helper_only {
            return (Stage::AwaitingReturn, seg.drained_at);
        }
        match (self.agent, seg.enabled_at) {
            (Agent::Codex, Some(at)) => (Stage::AwaitingPostFlush, Some(at)),
            _ => (Stage::Matched, seg.drained_at),
        }
    }

    /// Match, release and complete the outstanding segment, then any that a
    /// completion brings to the front.
    fn evaluate(&mut self, records: &mut Records, fifo: &mut HazardFifo, now: Instant) {
        // A drained record can still learn its frontier, match or report:
        // the kept facts follow it until it is deleted.
        for seg in &mut self.segments {
            if let Some(fact) = seg.record.and_then(|k| records.fact(k)) {
                seg.fact = Some(fact);
            }
            // A release before an unknown ending was the running editor's;
            // once the ending is known unknown, it releases nothing more.
            if seg.released
                && let Some(fact) = seg.fact
                && wait_unknown(fact.decision, fact.report)
            {
                seg.cut_off = true;
            }
        }
        while let Some(seg) = self.segments.front_mut() {
            let (Some(number), Some((_, written_at))) = (seg.number, seg.written) else {
                return;
            };
            if seg.record.is_none()
                && !seg.mismatched
                && let Some(key) = find_match(records, number, written_at)
                && records.match_segment(self.generation, key, number).is_ok()
            {
                seg.record = Some(key);
                if let Some(at) = self.editors.iter().position(|k| *k == key) {
                    self.editors.remove(at);
                    seg.editor_running = true;
                }
            }
            let Some(key) = seg.record else {
                return;
            };
            // A matched record that is gone was deleted after a completion
            // that wasn't this segment's (a subsumption): it holds.
            let Some(r) = records.get(key) else {
                return;
            };
            if r.seg_seen.is_some_and(|s| s != number) {
                seg.mismatched = true;
            }
            if seg.mismatched {
                return;
            }
            if r.drained && seg.drained_at.is_none() {
                seg.drained_at = Some(now);
            }
            if !seg.released && early_release(self.agent, seg, &r) {
                seg.released = true;
            }
            let (route, watch) = if r.helper_only() {
                (Route::HelperOnly, seg.e1)
            } else {
                (Route::EditorBearing, seg.ec)
            };
            if !r.drained || !watch.is_some_and(|w| w.returned(self.agent)) {
                return;
            }
            match records.complete(fifo, self.generation, key, route, now, now) {
                Ok(()) | Err(record::Refusal::NotPending(Completion::Complete { .. })) => {}
                Err(_) => return,
            }
            if let Some(seg) = self.segments.pop_front() {
                if seg.kind == Kind::Probe
                    && !seg.decided
                    && let (Some(number), Some(key)) = (seg.number, seg.record)
                {
                    let decision = records
                        .get(key)
                        .and_then(|r| r.decision)
                        .or_else(|| seg.fact.and_then(|f| f.decision));
                    self.completed.push((number, key, decision));
                }
                self.ended.push(seg.end(Outcome::Written));
            }
        }
    }
}

/// Core §4.5's match for segment `number`, written at `written`: the one
/// unmatched `Owned` record that read the number, else the one with no
/// decision and no number whose birth floor is at or after `written`.
fn find_match(records: &Records, number: u64, written: Instant) -> Option<RecordKey> {
    let free = |r: &&Record| r.attribution == Attribution::Owned && r.matched.is_none();
    let mut by_number = records
        .iter()
        .filter(free)
        .filter(|r| r.seg_seen == Some(number));
    if let Some(r) = by_number.next() {
        return by_number.next().is_none().then_some(r.key);
    }
    let mut by_birth = records.iter().filter(free).filter(|r| {
        r.decision.is_none() && r.seg_seen.is_none() && born_at_or_after(r.birth_floor, written)
    });
    let r = by_birth.next()?;
    by_birth.next().is_none().then_some(r.key)
}

/// The releases that come before completion (core §4.6). A failed start
/// releases only at completion: no editor took the bytes.
fn early_release(agent: Agent, seg: &Segment, r: &Record) -> bool {
    if matches!(r.report, Report::SpawnFailed { .. }) {
        return false;
    }
    match (agent, seg.kind, r.decision) {
        (Agent::Claude, Kind::Probe, Some(Decision::Armed | Decision::Unarmed)) => true,
        (Agent::Claude, Kind::HumanG, Some(Decision::Armed)) => true,
        (_, Kind::HumanG, Some(Decision::Unarmed)) => seg.editor_running,
        _ => false,
    }
}

/// Whether a drained record's ending is `WaitUnknown`: an editor-bearing
/// decision (or none) and no report the helper recognisably sent, so
/// nothing says when, or whether, its editor stopped reading.
fn wait_unknown(decision: Option<Decision>, report: Report) -> bool {
    !decision.is_some_and(Decision::helper_only)
        && matches!(report, Report::None | Report::Unrecognised)
}

/// [`Record::helper_only`], from the facts kept at the drain.
fn fact_helper_only(f: &ExitFact) -> bool {
    f.decision.is_some_and(Decision::helper_only) || matches!(f.report, Report::SpawnFailed { .. })
}
