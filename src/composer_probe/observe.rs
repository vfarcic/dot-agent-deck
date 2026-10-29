//! PRD #1666 part 3 (Q35-14b; core §2.3–§2.4, §2.6 and §5.4, R5 §3.3,
//! amendments 1–6 of Q35-core): the daemon's side of an installed
//! generation's editor helper. What its helper reports and what the daemon
//! sees of it become the generation's invocation records
//! ([`super::record`]), and a liability the deck publishes on the arm page
//! is consumed only through them.
//!
//! **The channels.** A helper is seen four ways, each attested by the
//! daemon's own reads ([`super::attest`]), never by what the helper says:
//! - **IPC**, on the hook socket: a helper asks the hook capabilities on its
//!   connection, and only an installed generation's helper copy, named by
//!   the connection's `SO_PEERPIDFD`, is told the daemon serves
//!   `composer_invocation` ([`Observer::connect`]). Where the socket gives no
//!   peer pidfd the capability isn't advertised, so the IPC path gives no
//!   authority (Q81) and the helper's failed exchange leaves the marker. The
//!   invocation is attested before it is answered ([`Observer::invocation`]).
//! - **Markers** in `inv/<G>/`, read and renamed `.seen` once applied (core
//!   §2.3's handshake).
//! - **The child scan**: every [`SCAN_PERIOD`] while a liability is
//!   published, and once when the agent's output turns bracketed paste off
//!   (`?2004l`, which an agent writes as it runs its editor), the helper
//!   copies that are `A`'s own children. A sighting carries no decision,
//!   claim or numbers ([`scan_observation`]): `Records::observe` refuses
//!   none of those on a scan, so this adapter is what keeps them out.
//! - **The exit**, by the pidfd each record's helper is held by.
//!
//! **Settlement** is the records' (core §2.4). An `Armed` decision consumes
//! one liability only with the daemon's attribution `Owned`, whose pre-read
//! and post-read classes were both `Owned` with the pins live (Q35-14a's
//! revalidation, run for every channel), so a nested agent, a tool or an
//! orphan never spends `A`'s arm. Every non-`Armed` invocation moves the
//! input epoch as an external edit before it is answered (producer 1 of
//! `ExternalEdit`, PR U3-11). An `Owned` invocation whose export can't be
//! read is answered `Unreadable`, never `Empty`, and still consumes: the
//! decision and the attribution are what consume, the read is only what the
//! helper reports to its agent.
//!
//! **The drain** (core §2.4, AU7 §2, Q84). A record whose exit is observed
//! is settled by its exit only once its decision evidence is drained: every
//! composer connection of its helper has finished, and its markers have been
//! read again completely. A connection is registered when its capability
//! question is answered, which is before a helper can send a decision on it,
//! so a decision on IPC is always on a registered connection. When the exit
//! is observed each of the helper's connections is told ([`Departure`]),
//! whether or not the helper has a record (Q85); its reader applies what it
//! reads, and ends the connection at its end or at its read deadline,
//! [`READ_DEADLINE`] after the exit in total, however many lines arrive
//! meanwhile from whatever process still holds the helper's descriptor
//! (Q85), so a line read before then is applied before the drain, and the
//! connection's slot comes back. A connection that ends holding a partial
//! line, or with bytes still pending at its deadline, or on a line or a
//! report it refused, left evidence unread ([`read_line`]): its record never
//! settles clean, only uncertain. A marker read that overflows or fails is no drain: nothing
//! settles, and no marker is taken as deleted. Until both hold the drain is
//! rescheduled; after [`DRAIN_RETRIES`] passes (a count, not a time) it
//! gives up to the uncertain outcome
//! (`NotConsumed(Unproven)` for an open record, no fallback pop), never to
//! the clean one, and keeps its record and markers. A decision that arrives
//! after the exit drained is logged and changes nothing: the helper is told
//! the settled outcome (core §2.4's table). Work is bounded per
//! generation and in total ([`Pool`]): composer connections open at once,
//! the bytes of the one report read after an invocation, attestations at
//! once and per pass, and marker names read per pass. Past a bound a scan or
//! a marker waits for the next pass; a connection gets no capability; an
//! attestation not run is an identity refusal (`record_refused`, with the
//! request's claim on IPC and none for a scan, an unread connection or a
//! marker).
//!
//! **The kill** (core §5.4, P-E2, Q84). A record whose helper declared
//! `Armed` and is still alive [`E2_AFTER`] after the first `Armed`
//! observation of it is killed through its pidfd, only when that pidfd was
//! pinned for a helper the daemon attested as this instance's helper copy
//! (its connection's peer pidfd, or a child scan with its start ticks) and
//! the process still runs that copy. A marker's name never makes a process
//! one E2 signals. The clock starts at the first observation of `Armed` and
//! a duplicate doesn't move it ([`e2_due`]). A helper stopped before its arm
//! load, or on an editor path, is never killed.
//!
//! **Permission** (plan §1.2, Q84). A generation is armable once its header
//! latched an exact release with a verified `editor_probe` record and `A`
//! is bound; then `gen-G` is written. `A` is bound only for a direct Claude,
//! from the deck's own launch record ([`bound_agent`]): the PTY child the
//! deck spawned, or the one child of the deck's own `sh -c`, running the
//! agent's executable with the argv the launch gives it. A user's launcher
//! script binds nothing (Q35-22 binds below a launcher; a direct Codex runs
//! below the deck's wrapper, so its `A` isn't the PTY child either). One
//! settler at a time ([`SettleClaim`]) computes it, and only a settlement
//! that moves it out of `Pending` publishes `A` with it, so `gen-G` is
//! written once. Nothing in production publishes a liability: installation
//! rolls out to no class until activation, and the only publisher is the
//! `e2e` build's control verb ([`Seam::control`]).
//!
//! **Lock order.** No lock is held while a file is read or written:
//! ```text
//! registry (R4) ─ clone Arc<Observer>, released ─┐
//! hook task │ observer thread │ attach handler ──┴─> attest, bind, /proc and
//!                                                     inv/G reads, marker renames
//!                                                     and deletion, gen-G write
//!                                                     (no lock)
//!                                                     └─> seam (Mutex) ─> input (R6)
//!                                                                  └─> arm page stores
//! settle claim (atomic, held across bind and the gen-G write, no lock) ─> seam
//! output lock (bus) ─> Bell (atomics, unpark)
//! observer exit observed ─> Departure (once-set instant, notify) ─> hook task's read deadline
//! retirement: R4 ─> R6 (retire_taking_first_stop); Observer::stop (atomics)
//! ```
//! The seam lock isn't ranked; nothing takes it under R4, R6 or the output
//! lock. Under it are the input lock's leaf steps, memory stores to the arm
//! page, and two kinds of system call on descriptors already held: `poll` of
//! each pidfd it keeps for a helper's exit (a record's pin, a connected
//! helper's, one a connection that lost bytes left, Q85), and the `dup` of a pidfd E2 is about to check. No path is opened, read, renamed, unlinked or written under it.
//! Retirement: the pane's successor retires the input (records and scope
//! dropped, M-FIRST taken in the same section) and stops the observer; the
//! observer's thread holds it weakly and ends at its next wake, and the arm
//! page is unmapped when the record goes.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::Permission;
use super::epoch::GenerationInput;
use super::record::{
    self, Attribution, Channel, Claim, Decision, Observation, RecordKey, Recorded, Refusal, Report,
    Settlement, Tree,
};
use crate::event::{
    ComposerClaim, ComposerDecision, ComposerInvocationRequest, ComposerOutcome, EditorReport,
    EditorTree, ExportRead,
};

/// The child scan's period while a liability is published (R5 §3.3).
pub const SCAN_PERIOD: Duration = Duration::from_millis(50);

/// How often an idle generation's markers are read.
pub const IDLE_SWEEP: Duration = Duration::from_secs(1);

/// How long after a helper's exit its open connections may still deliver
/// (core §2.4): a connection with nothing pending once it passes has said
/// nothing more.
pub const READ_DEADLINE: Duration = Duration::from_millis(100);

/// How many passes an exited record's drain is rescheduled while its
/// decision evidence isn't all read (a connection not finished, a marker
/// read that overflowed or failed) before it gives up to the uncertain
/// outcome (Q84). A count of passes, not a time: a pass waits at most
/// [`SCAN_PERIOD`] while a liability is published, so 100 passes take about
/// 5 s when nothing wakes them, but a bell wakes a pass early and slow work
/// delays one, so the give-up can come sooner or later than that. It is an
/// availability allowance, not a safety bound: giving up consumes nothing.
pub const DRAIN_RETRIES: u32 = 100;

/// How long an `Armed` helper may live after its decision was first seen
/// (core §5.4).
pub const E2_AFTER: Duration = Duration::from_secs(5);

/// Composer connections open at once, per generation and in total.
pub const CONNECTIONS_PER_GENERATION: u32 = 8;
pub const CONNECTIONS_TOTAL: u32 = 64;

/// The most bytes of the report line read after an invocation.
pub const REPORT_BYTES: usize = 4 << 10;

/// Attestations per generation per pass, and at once in total.
pub const ATTESTATIONS_PER_PASS: u32 = 16;
pub const ATTESTATIONS_TOTAL: u32 = 8;

/// Marker names read per generation per pass (core §2.6).
pub const MARKER_NAMES_PER_PASS: usize = 256;

/// How long the control verb waits for a pending permission to settle.
pub const CONTROL_PERMISSION_WAIT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Budgets.
// ---------------------------------------------------------------------------

/// A bounded count of something held at once.
#[derive(Debug)]
pub struct Pool {
    max: u32,
    used: AtomicU32,
}

impl Pool {
    pub const fn new(max: u32) -> Self {
        Self {
            max,
            used: AtomicU32::new(0),
        }
    }

    /// One more, unless the pool is full.
    pub fn take(&self) -> bool {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.max).then_some(used + 1)
            })
            .is_ok()
    }

    /// Give one back. Never below zero.
    pub fn give(&self) {
        let _ = self
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_sub(1)
            });
    }

    pub fn used(&self) -> u32 {
        self.used.load(Ordering::Acquire)
    }
}

/// One unit of a generation's pool and of the total, given back when
/// dropped.
#[derive(Debug)]
pub struct Slot {
    generation: Arc<Pool>,
    total: &'static Pool,
}

impl Slot {
    /// A unit of both, or none: a full total gives back the generation's.
    pub fn take(generation: &Arc<Pool>, total: &'static Pool) -> Option<Slot> {
        if !generation.take() {
            return None;
        }
        if !total.take() {
            generation.give();
            return None;
        }
        Some(Slot {
            generation: Arc::clone(generation),
            total,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.generation.give();
        self.total.give();
    }
}

/// Composer connections open in the daemon.
pub static CONNECTIONS: Pool = Pool::new(CONNECTIONS_TOTAL);

/// Attestations running in the daemon.
pub static ATTESTATIONS: Pool = Pool::new(ATTESTATIONS_TOTAL);

// ---------------------------------------------------------------------------
// The pure parts.
// ---------------------------------------------------------------------------

/// The record's decision for a request's.
pub fn decision(decision: ComposerDecision) -> Decision {
    match decision {
        ComposerDecision::Armed => Decision::Armed,
        ComposerDecision::Unarmed => Decision::Unarmed,
        ComposerDecision::Tool => Decision::Tool,
        ComposerDecision::Unknown => Decision::Unknown,
    }
}

/// The record's claim for a request's.
pub fn claim(claim: ComposerClaim) -> Claim {
    match claim {
        ComposerClaim::Owned => Claim::Owned,
        ComposerClaim::Tool => Claim::Tool,
        ComposerClaim::Unknown => Claim::Unknown,
    }
}

/// The record's report for a helper's.
pub fn report(report: EditorReport) -> Report {
    match report {
        EditorReport::EditorDone { tree } => Report::Done(match tree {
            EditorTree::Clean => Tree::Clean,
            EditorTree::Survivors => Tree::Survivors,
            EditorTree::Unknown => Tree::Unknown,
        }),
        EditorReport::SpawnFailed { errno } => Report::SpawnFailed { errno },
        EditorReport::Unknown => Report::Unrecognised,
    }
}

/// What the helper is told (core §2.4's last column): `Mine` only for the
/// record's consumption, with the daemon's own read, `Unreadable` when it
/// has none; the user's editor for every editor-bearing decision; otherwise
/// `Uncertain`, which consumes nothing and opens no editor.
pub fn outcome(
    decision: Decision,
    recorded: Result<Recorded, Refusal>,
    read: Option<ExportRead>,
) -> ComposerOutcome {
    match decision {
        Decision::Armed => match recorded {
            Ok(Recorded::Kept {
                settlement: Settlement::Consumed,
                ..
            }) => ComposerOutcome::Mine {
                read: read.unwrap_or(ExportRead::Unreadable),
            },
            _ => ComposerOutcome::Uncertain,
        },
        Decision::Unarmed | Decision::Tool => ComposerOutcome::Unarmed,
        Decision::Unknown => ComposerOutcome::Uncertain,
    }
}

/// A scan's observation of helper `key` (R5 §3.3): the daemon's own
/// attribution and the helper's birth floor, and no decision, claim or
/// numbers, which only the helper's channels carry.
pub fn scan_observation(
    generation: u64,
    key: RecordKey,
    attribution: Attribution,
    birth_floor: Instant,
) -> Observation {
    Observation {
        generation,
        key,
        channel: Channel::Scan,
        attribution,
        claim: None,
        decision: None,
        seg_seen: None,
        native_seen: None,
        birth_floor,
    }
}

/// A marker's observation: its decision and numbers, and no claim, which
/// only a request carries.
pub fn marker_observation(
    generation: u64,
    marker: &record::Marker,
    attribution: Attribution,
    birth_floor: Instant,
) -> Observation {
    Observation {
        generation,
        key: marker.key,
        channel: Channel::Marker,
        attribution,
        claim: None,
        decision: Some(marker.decision),
        seg_seen: marker.seen.map(|(seg, _)| seg),
        native_seen: marker.seen.map(|(_, native)| native),
        birth_floor,
    }
}

/// Whether record `r`'s helper is due E2's kill at `now` (core §5.4, P-E2):
/// it declared `Armed`, and [`E2_AFTER`] has passed since that decision was
/// first observed. Its exit not yet observed is the caller's to check.
pub fn e2_due(r: &record::Record, now: Instant) -> bool {
    r.decision == Some(Decision::Armed)
        && r.exit_observed.is_none()
        && r.first_armed_seen
            .is_some_and(|first| now.saturating_duration_since(first) >= E2_AFTER)
}

/// What a pass does with an exited record's drain (core §2.4, Q84).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainStep {
    /// Its evidence is all read: settle by the exit.
    Settle,
    /// Reschedule: nothing settles.
    Wait,
    /// Evidence that will never be read (a connection ended with it unread),
    /// or the retries spent with evidence still unread: the uncertain
    /// outcome ([`super::record::Records::drained_uncertain`]), never the
    /// clean one.
    GiveUp,
}

/// The drain's step for an exited record (core §2.4, Q84, Q85): `open`, its
/// helper's composer connections not finished yet (one finishes at its end,
/// or at its read deadline, [`READ_DEADLINE`] after its helper's exit was
/// observed, after the lines it read before then were applied);
/// `markers_read`, whether this pass read the record's markers completely
/// (no overflow, no error, every one applied within the budget); `unread`,
/// whether a finished connection ended with bytes it didn't read (a partial
/// line, bytes still pending at its deadline, a line it refused); `retries`,
/// the passes already rescheduled. No timer settles an open connection:
/// only its finishing does, or the retries running out to the uncertain
/// outcome. Evidence a connection left unread is never read later, so once
/// nothing else is outstanding the record gives up at once.
pub fn drain_step(open: u32, markers_read: bool, unread: bool, retries: u32) -> DrainStep {
    if open == 0 && markers_read {
        if unread {
            DrainStep::GiveUp
        } else {
            DrainStep::Settle
        }
    } else if retries >= DRAIN_RETRIES {
        DrainStep::GiveUp
    } else {
        DrainStep::Wait
    }
}

/// A composer connection's view of its helper's exit (core §2.4): the
/// observer marks it when it observes the exit, and the connection's reader
/// then reads for [`READ_DEADLINE`] more, in total (Q85): the deadline is
/// anchored at the first mark, so nothing the peer sends renews it. Marking
/// takes no lock and wakes every waiter.
#[derive(Debug, Default)]
pub struct Departure {
    at: OnceLock<Instant>,
    notify: tokio::sync::Notify,
}

impl Departure {
    /// The helper's exit was observed. A second mark moves nothing.
    pub fn depart(&self) {
        self.at.get_or_init(Instant::now);
        self.notify.notify_waiters();
    }

    pub fn departed(&self) -> bool {
        self.at.get().is_some()
    }

    /// When the reading ends: [`READ_DEADLINE`] after the first mark.
    pub fn deadline(&self) -> Option<Instant> {
        self.at.get().map(|at| *at + READ_DEADLINE)
    }

    /// Whether the read deadline has passed.
    pub fn expired(&self) -> bool {
        self.deadline().is_some_and(|end| Instant::now() >= end)
    }

    /// Resolves once [`Departure::depart`] has been called, before or after
    /// this.
    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.departed() {
                return;
            }
            notified.await;
        }
    }

    /// Resolves at [`Departure::deadline`]: the same instant for every call,
    /// however many lines were read since the mark.
    pub async fn read_deadline(&self) {
        self.wait().await;
        if let Some(end) = self.deadline() {
            tokio::time::sleep_until(tokio::time::Instant::from_std(end)).await;
        }
    }
}

/// The one settler of a generation's permission at a time (Q84): the
/// observer's pass and the control verb both settle it, and whichever takes
/// this claim first computes, writes `gen-G` and publishes; the other
/// returns, and finds the permission settled or settles it later itself.
#[derive(Debug, Default)]
pub struct SettleClaim {
    held: AtomicBool,
}

/// A held [`SettleClaim`], released when dropped.
#[derive(Debug)]
pub struct Settling<'a> {
    claim: &'a SettleClaim,
}

impl SettleClaim {
    /// The claim, unless another settler holds it.
    pub fn claim(&self) -> Option<Settling<'_>> {
        self.held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            // Lazily: a guard built for a failed claim would release the
            // holder's when dropped.
            .then(|| Settling { claim: self })
    }
}

impl Drop for Settling<'_> {
    fn drop(&mut self) {
        self.claim.held.store(false, Ordering::Release);
    }
}

/// The argv a POSIX shell's `-c` gives the simple command `line`, or `None`
/// when the shell would expand, redirect or run anything else in it (Q84's
/// launch provenance): blanks separate words, single quotes keep everything,
/// double quotes keep everything but `$`, `` ` `` and `\` (refused), and an
/// unquoted `\` keeps the next character. Any unquoted expansion, glob,
/// control or comment character, a leading assignment, a line break or an
/// unterminated quote refuses.
pub fn shell_words(line: &str) -> Option<Vec<String>> {
    const SPECIAL: &[char] = &[
        '$', '`', ';', '&', '|', '(', ')', '<', '>', '*', '?', '[', '{', '}', '~', '#', '\n', '\r',
        '!',
    ];
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' => return None,
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next()? {
                    '\n' => return None,
                    c => word.push(c),
                }
            }
            '=' if words.is_empty() => return None,
            c if SPECIAL.contains(&c) => return None,
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    (!words.is_empty()).then_some(words)
}

/// What binding reads of one process: its executable's `(dev, ino)` and its
/// argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcFacts {
    pub exe: super::classifier::FileId,
    pub argv: Vec<std::ffi::OsString>,
}

/// Which process [`bound_agent`] binds as `A`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// The PTY child itself.
    Pty,
    /// The PTY child's one child, by pid.
    Child(u32),
}

/// Whether `program`'s basename is a shell's ([`super::install::SHELLS`]).
fn is_shell(program: &std::ffi::OsStr) -> bool {
    std::path::Path::new(program)
        .file_name()
        .is_some_and(|name| super::install::SHELLS.iter().any(|shell| name == *shell))
}

/// `A` for a direct Claude from the deck's own launch record (Q84): `launch`
/// is the argv the deck executed as the PTY child, `pty` that process now
/// (the caller has pinned it by the pid and start ticks the deck spawned),
/// `children` its children, and `resolve` a program word's executable as
/// the agent's `PATH` finds it.
///
/// The agent's argv is `launch` itself, or, when `launch` is the deck's
/// shell running one line (`<shell> -c <line>`), the words the shell gives
/// that line ([`shell_words`]); a program that is itself a shell binds
/// nothing. The PTY child is `A` when it runs that program's executable with
/// exactly that argv (the deck executed it, or its shell `exec`ed it). Its
/// one child is `A` only when the PTY child is still the deck's shell, its
/// executable and argv the launch's, and that child runs the program with
/// the argv. Anything else, a user's launcher script among it (its process
/// runs an interpreter, not the program's own executable), binds nothing:
/// launchers are Q35-22's.
pub fn bound_agent(
    launch: &[std::ffi::OsString],
    pty: &ProcFacts,
    children: &[(u32, ProcFacts)],
    resolve: impl Fn(&std::ffi::OsStr) -> Option<super::classifier::FileId>,
) -> Option<Bound> {
    let first = launch.first()?;
    let shell_line = match launch {
        [shell, flag, line] if is_shell(shell) && flag == "-c" => Some(line.to_str()?),
        _ => None,
    };
    let expected: Vec<std::ffi::OsString> = match shell_line {
        Some(line) => shell_words(line)?.into_iter().map(Into::into).collect(),
        None => launch.to_vec(),
    };
    let program = expected.first()?;
    if is_shell(program) {
        return None;
    }
    let agent_exe = resolve(program)?;
    let runs_agent = |p: &ProcFacts| p.exe == agent_exe && p.argv == expected;
    if runs_agent(pty) {
        return Some(Bound::Pty);
    }
    if shell_line.is_some()
        && resolve(first) == Some(pty.exe)
        && pty.argv == launch
        && let [(pid, child)] = children
        && runs_agent(child)
    {
        return Some(Bound::Child(*pid));
    }
    None
}

/// A start in clock ticks since boot as an [`Instant`] at or before the
/// process's birth: `now` read first, then `boot_now_ns`, the boot clock, so
/// the floor errs early. `None` when it can't be represented.
pub fn birth_floor(
    start_ticks: u64,
    ticks_per_sec: u64,
    now: Instant,
    boot_now_ns: u64,
) -> Option<Instant> {
    let floor = record::birth_floor_ns(start_ticks, ticks_per_sec)?;
    let back = boot_now_ns.checked_sub(floor)?;
    now.checked_sub(Duration::from_nanos(back))
}

/// The daemon's attestation of one helper, as a record takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Evidence {
    pub key: RecordKey,
    pub birth_floor: Instant,
    pub attribution: Attribution,
    /// The daemon's read of the export, for an `Owned` helper.
    pub read: Option<ExportRead>,
}

/// The e2e control verb's operation (OQ10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOp {
    Publish,
    Withdraw,
}

/// What a control operation does to the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlStep {
    Raise,
    Lower,
    Unchanged,
}

/// The control verb's one test liability per generation (PA2 U3-05): at
/// most one outstanding, publish while published and withdraw while
/// withdrawn are no-ops, and it keeps no history. It is outstanding while it
/// was published and the count is above zero; a consumption takes it, since
/// a liability counts as any other and nothing else publishes one in a
/// build that has the verb.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TestLiability {
    published: bool,
}

impl TestLiability {
    /// The step for `op` with the count at `count`.
    pub fn step(&mut self, op: ControlOp, count: u32) -> ControlStep {
        let outstanding = self.published && count > 0;
        match op {
            ControlOp::Publish if outstanding => ControlStep::Unchanged,
            ControlOp::Publish => {
                self.published = true;
                ControlStep::Raise
            }
            ControlOp::Withdraw => {
                self.published = false;
                if outstanding {
                    ControlStep::Lower
                } else {
                    ControlStep::Unchanged
                }
            }
        }
    }
}

/// Why a control operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRefusal {
    /// It named another generation than the one it was addressed to.
    Generation,
    /// The generation may not be armed.
    Permission(Permission),
    /// The records refused the step (retired, or the count can't move).
    Records(Refusal),
}

/// The page a [`Seam`] publishes to.
pub trait Page {
    fn publish(&mut self, count: u32);
}

/// One generation's state that changes the count, under the seam lock: the
/// records (through the input lock), the arm page and the control verb's
/// liability. Every step that can move the count publishes what the records
/// hold before the lock is released, so the page's count is the records'
/// between steps: a store to zero is stamped before it, one from zero after.
pub struct Seam<P> {
    generation: u64,
    input: Arc<GenerationInput>,
    page: P,
    published: u32,
    test: TestLiability,
    permission: Permission,
}

impl<P: Page> Seam<P> {
    /// Generation `generation`'s seam, its records already attached to
    /// `input`.
    pub fn new(generation: u64, input: Arc<GenerationInput>, page: P) -> Self {
        Self {
            generation,
            input,
            page,
            published: 0,
            test: TestLiability::default(),
            permission: Permission::Pending,
        }
    }

    pub fn input(&self) -> &Arc<GenerationInput> {
        &self.input
    }

    pub fn permission(&self) -> Permission {
        self.permission
    }

    /// Settle the permission. Once out of `Pending` it doesn't move: whether
    /// this call moved it, so only its winner publishes what goes with it.
    pub fn set_permission(&mut self, permission: Permission) -> bool {
        let moved = self.permission == Permission::Pending && permission != Permission::Pending;
        if moved {
            self.permission = permission;
        }
        moved
    }

    pub fn page(&mut self) -> &mut P {
        &mut self.page
    }

    /// Publish the records' count if the page doesn't show it.
    fn sync(&mut self) {
        if let Some(count) = self.input.liability_count()
            && count != self.published
        {
            self.page.publish(count);
            self.published = count;
        }
    }

    /// The control verb (OQ10), addressed to generation `addressed`: publish
    /// or withdraw the one test liability through the same records and page
    /// a probe uses, writing no byte to the agent. The new count.
    pub fn control(&mut self, addressed: u64, op: ControlOp) -> Result<u32, ControlRefusal> {
        if addressed != self.generation {
            return Err(ControlRefusal::Generation);
        }
        if self.permission != Permission::Armable {
            return Err(ControlRefusal::Permission(self.permission));
        }
        let count = self
            .input
            .liability_count()
            .ok_or(ControlRefusal::Records(Refusal::Stale))?;
        let mut test = self.test;
        let step = test.step(op, count);
        let now = match step {
            ControlStep::Unchanged => {
                self.test = test;
                return Ok(count);
            }
            ControlStep::Raise => {
                let raised = count
                    .checked_add(1)
                    .ok_or(ControlRefusal::Records(Refusal::Count))?;
                // A store from zero is stamped after it.
                self.page.publish(raised);
                self.published = raised;
                self.input.liability_raise(self.generation, Instant::now())
            }
            ControlStep::Lower => {
                // A store to zero is stamped before it.
                let stamp = Instant::now();
                self.input.liability_lower(self.generation, stamp)
            }
        };
        let result = now.map_err(ControlRefusal::Records);
        self.sync();
        if result.is_ok() {
            self.test = test;
        }
        result
    }

    /// An invocation on IPC, its helper attested as `evidence` (`None`: an
    /// identity refusal), at `now`. The epoch moves for a decision that may
    /// open an editor before the outcome is returned for the reply.
    pub fn invocation(
        &mut self,
        request: &ComposerInvocationRequest,
        evidence: Option<Evidence>,
        now: Instant,
    ) -> ComposerOutcome {
        let decided = decision(request.decision);
        if request.generation != self.generation {
            return ComposerOutcome::Uncertain;
        }
        let claimed = request.claim.map(claim);
        let read = evidence.and_then(|e| e.read);
        let recorded = match evidence {
            Some(e) => self.input.record_observe(
                &Observation {
                    generation: request.generation,
                    key: e.key,
                    channel: Channel::Ipc,
                    attribution: e.attribution,
                    claim: claimed,
                    decision: Some(decided),
                    seg_seen: request.seg_seen,
                    native_seen: request.native_seen,
                    birth_floor: e.birth_floor,
                },
                now,
            ),
            None => self
                .input
                .record_refused(
                    request.generation,
                    Channel::Ipc,
                    Some(decided),
                    claimed,
                    now,
                )
                .and(Err(Refusal::NoRecord)),
        };
        self.sync();
        if decided != Decision::Armed {
            // ExternalEdit's first producer (PR U3-11): an invocation that
            // isn't the deck's may edit the box, and the epoch says so
            // before the helper is told to run its editor.
            self.input.external_edit();
        }
        outcome(decided, recorded, read)
    }

    /// Whether a decision for `key` now would come after its record's exit
    /// drained, which the records ignore (core §2.4: the helper is told the
    /// settled outcome). The caller logs it with the seam lock released, so
    /// it isn't lost silently (Q84).
    pub fn late(&self, key: RecordKey) -> bool {
        self.input.record(key).is_some_and(|r| r.drained)
    }

    /// A scan's or a marker's observation.
    pub fn observe(&mut self, obs: &Observation, now: Instant) -> Result<Recorded, Refusal> {
        let recorded = self.input.record_observe(obs, now);
        self.sync();
        recorded
    }

    /// An observation the identity refusal dropped: no record.
    pub fn refused(
        &mut self,
        channel: Channel,
        decided: Option<Decision>,
        claimed: Option<Claim>,
        now: Instant,
    ) -> Result<(), Refusal> {
        self.input
            .record_refused(self.generation, channel, decided, claimed, now)
    }

    /// A helper's report after its editor, on its connection. A `Clean`
    /// report asks for a drain frontier, which no reader takes yet
    /// (Q35-20a), so it is recorded missing at once.
    pub fn report(&mut self, key: RecordKey, got: EditorReport, now: Instant) {
        if let Ok(record::Reported::RequestFrontier) =
            self.input
                .record_report(self.generation, key, report(got), now)
        {
            let _ =
                self.input
                    .record_frontier(self.generation, key, record::FrontierOutcome::Missing);
        }
    }

    /// The helper's exit observed at `at`.
    pub fn exit(&mut self, key: RecordKey, at: Instant, now: Instant) -> Result<(), Refusal> {
        self.input.record_exit(self.generation, key, at, now)
    }

    /// The exit drain, once the evidence is drained: a decisionless exit can
    /// consume, so the page follows.
    pub fn drained(&mut self, key: RecordKey, now: Instant) -> Result<record::Drained, Refusal> {
        let drained = self.input.record_drained(self.generation, key, now);
        self.sync();
        drained
    }

    /// The exit drain given up with evidence unread: nothing consumes.
    pub fn drained_uncertain(
        &mut self,
        key: RecordKey,
        now: Instant,
    ) -> Result<record::Drained, Refusal> {
        let drained = self
            .input
            .record_drained_uncertain(self.generation, key, now);
        self.sync();
        drained
    }

    /// Delete the record if it may go, its markers deleted or not.
    pub fn prune(
        &mut self,
        key: RecordKey,
        markers_deleted: bool,
        now: Instant,
    ) -> Result<Option<record::ExitFact>, Refusal> {
        self.input
            .record_prune(self.generation, key, markers_deleted, now)
    }
}

/// What rings an observer: a header that latched, the agent's output
/// turning bracketed paste off, or activity on a channel. Ringing takes no
/// lock: the output hook rings it under the output lock.
#[derive(Debug, Default)]
pub struct Bell {
    scan: AtomicBool,
    thread: OnceLock<std::thread::Thread>,
}

impl Bell {
    /// Wake the observer.
    pub fn ring(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    /// Wake the observer for a child scan.
    pub fn ring_scan(&self) {
        self.scan.store(true, Ordering::Release);
        self.ring();
    }

    /// Whether a scan was asked for since the last call.
    pub fn take_scan(&self) -> bool {
        self.scan.swap(false, Ordering::AcqRel)
    }

    /// The thread to wake.
    pub fn bind(&self, thread: std::thread::Thread) {
        let _ = self.thread.set(thread);
    }
}

/// The slot an agent's output hook reads its observer's bell from: set once,
/// when an installed generation's observer starts.
pub type BellSlot = Arc<OnceLock<Arc<Bell>>>;

#[cfg(target_os = "linux")]
pub use linux::{Connection, Observer, Spawned, read_line};

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::io::Read;
    use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::{Arc, Mutex, Weak};
    use std::time::{Duration, Instant};

    use super::{
        ATTESTATIONS, ATTESTATIONS_PER_PASS, Bell, Bound, CONNECTIONS, CONNECTIONS_PER_GENERATION,
        CONTROL_PERMISSION_WAIT, ControlOp, ControlRefusal, Departure, DrainStep, Evidence,
        IDLE_SWEEP, MARKER_NAMES_PER_PASS, Page, Pool, ProcFacts, SCAN_PERIOD, Seam, SettleClaim,
        Slot, bound_agent, drain_step, e2_due, marker_observation, scan_observation,
    };
    use crate::composer_probe::attest::{
        self, Agent, Kernel, LinuxKernel, Pinned, Sighting, attribute, read_export,
    };
    use crate::composer_probe::classifier::{FileId, Identity, ProcView};
    use crate::composer_probe::epoch::GenerationInput;
    use crate::composer_probe::install::{Installation, PaneClass, ProbeAgent};
    use crate::composer_probe::instance::{GenerationFiles, InstanceReader, Kind};
    use crate::composer_probe::record::{
        self, Attribution, Channel, EC_POLICY, ExitFact, RecordKey, Records,
    };
    use crate::composer_probe::{Binding, InertCause, Permission};
    use crate::doorbell_activation::{EditorProbe, LatchedRelease};
    use crate::event::{AgentType, ComposerInvocationRequest, ComposerOutcome, EditorReport};

    impl Page for GenerationFiles {
        fn publish(&mut self, count: u32) {
            // No segment numbers until probes exist (Q35-19/20).
            self.arm().publisher().publish(count, 0);
        }
    }

    /// The most exit facts an observer keeps after their records go.
    const FACTS_KEPT: usize = record::RECORD_CAP;

    /// The most bytes of a process's argv binding reads.
    const ARGV_BYTES: u64 = 128 << 10;

    /// A record's helper, held by its pidfd for its exit.
    struct Pin {
        fd: OwnedFd,
        /// Whether E2 may signal it (Q84): only a helper the daemon attested
        /// as this instance's helper copy, by its connection's peer pidfd or
        /// by a child scan with its start ticks. A pin from a marker's name
        /// watches the exit and is never signalled.
        killable: bool,
    }

    /// What a pass watches, under the seam lock with the seam.
    #[derive(Default)]
    struct Watch {
        /// Helpers with a record whose exit isn't observed yet.
        pins: HashMap<RecordKey, Pin>,
        /// Exits observed, drain pending, with the passes it was rescheduled.
        exited: HashMap<RecordKey, u32>,
        /// Composer connections not finished yet, by the helper on the other
        /// end.
        open: HashMap<RecordKey, Vec<Arc<Departure>>>,
        /// A pidfd of each helper in `open` (Q85), for its exit when no
        /// record pins it: its connections' read deadline then runs from
        /// that exit too, so a helper that never got a record leaves no
        /// connection open behind it. Removed with its `open` entry.
        peers: HashMap<RecordKey, OwnedFd>,
        /// Helpers one of whose connections ended with bytes it didn't read
        /// (Q85), each with a pidfd of its own for the helper's exit (`None`
        /// when the dup failed): their drain settles only uncertain. Removed
        /// when the record drains, or, for a helper with no record, once it
        /// exited with no connection left open ([`Observer::exits`]), so the
        /// map holds no more than the live helpers and the records.
        unread: HashMap<RecordKey, Option<OwnedFd>>,
        /// The last exit facts of records that went: no segment holds them
        /// yet (Q35-20).
        facts: std::collections::VecDeque<ExitFact>,
        /// `A`, once bound.
        agent: Option<Identity>,
        /// More marker names than a pass reads.
        marker_overflow: bool,
    }

    struct Inner {
        seam: Seam<GenerationFiles>,
        watch: Watch,
    }

    /// How an installed generation was spawned: what the observer needs of
    /// it.
    pub struct Spawned {
        pub agent_id: String,
        pub agent: Option<AgentType>,
        /// The spawn's own variables, which the agent sees over the
        /// daemon's.
        pub env: Vec<(String, String)>,
        /// The PTY child and its start ticks.
        pub child: Option<(u32, u64)>,
        pub input: Arc<GenerationInput>,
        /// The generation's release as latched for an agent, read with no
        /// seam lock held.
        pub latch: Box<dyn Fn(&AgentType) -> LatchedRelease + Send + Sync>,
        /// The `e2e` build's assumed editor-probe records (the daemon's
        /// [`crate::composer_probe::install::captured_assumption`]): nothing
        /// in production.
        pub assumed: &'static [(AgentType, semver::Version, EditorProbe<'static>)],
    }

    /// One installed generation's observer. See the module documentation.
    pub struct Observer {
        agent_id: String,
        generation: u64,
        agent: Option<AgentType>,
        class: PaneClass,
        kind: Kind,
        env: Vec<(String, String)>,
        child: Option<(u32, u64)>,
        /// The argv the deck executed as the PTY child.
        launch: Vec<OsString>,
        reader: InstanceReader,
        helper_id: FileId,
        latch: Box<dyn Fn(&AgentType) -> LatchedRelease + Send + Sync>,
        assumed: &'static [(AgentType, semver::Version, EditorProbe<'static>)],
        settling: SettleClaim,
        inner: Mutex<Inner>,
        connections: Arc<Pool>,
        bell: Arc<Bell>,
        stopped: std::sync::atomic::AtomicBool,
    }

    /// A composer connection: its helper, pinned by the connection's peer
    /// pidfd, and its budget slot. It is open for the drain until it is
    /// dropped, which its reader does at its end, or at
    /// [`Connection::read_deadline`] ([`read_line`]), after applying the
    /// lines it read before then. Dropping it gives its slot back.
    pub struct Connection {
        observer: Arc<Observer>,
        helper: Pinned<OwnedFd>,
        departure: Arc<Departure>,
        _slot: Slot,
    }

    impl Connection {
        pub fn observer(&self) -> &Arc<Observer> {
            &self.observer
        }

        /// Resolves [`super::READ_DEADLINE`] after the observer observed
        /// this connection's helper exit, at the same instant however often
        /// it is awaited: the reader then ends the connection
        /// ([`read_line`]).
        pub async fn read_deadline(&self) {
            self.departure.read_deadline().await;
        }

        /// Whether the read deadline has passed.
        pub fn expired(&self) -> bool {
            self.departure.expired()
        }

        /// The reader is ending this connection with bytes it didn't read
        /// (Q85): a partial line, bytes pending at the read deadline, or a
        /// line it refused. What they said is unknown, so the helper's
        /// record drains only uncertain. Recorded unless the record already
        /// drained (a late connection's loss changes nothing settled) and
        /// logged either way.
        pub fn lose(&self) {
            let key = key_of(&self.helper);
            let pidfd = self.helper.pidfd().try_clone().ok();
            let mut inner = self.observer.lock();
            let settled = inner.seam.input().record(key).is_some_and(|r| r.drained);
            if !settled {
                inner.watch.unread.entry(key).or_insert(pidfd);
            }
            drop(inner);
            tracing::warn!(
                target: "composer_probe",
                agent_id = %self.observer.agent_id,
                pid = key.pid,
                settled,
                "editor helper's connection ended with bytes unread; its exit settles \
                 only as uncertain"
            );
        }
    }

    /// The next line on a composer connection (core §2.4, Q85), or `None`
    /// once it is to end. `line` is the connection's accumulator, kept by the
    /// caller across calls; `socket` the connection's own descriptor. With no
    /// connection (`conn` is `None`) it is [`CappedLine::read`]. With one, a
    /// read races its read deadline and the deadline wins a tie, so a peer
    /// that keeps sending can't extend it: at the deadline a partial line in
    /// `line`, or bytes still buffered in `reader` or waiting on `socket`,
    /// make the connection [`Connection::lose`] them, and `None` tells the
    /// caller to drop it. A line returned before the deadline is the
    /// caller's to apply before it drops the connection.
    ///
    /// [`CappedLine::read`]: crate::bounded_read::CappedLine::read
    pub async fn read_line<R>(
        conn: Option<&Connection>,
        line: &mut crate::bounded_read::CappedLine,
        reader: &mut tokio::io::BufReader<R>,
        socket: BorrowedFd<'_>,
        max_bytes: usize,
    ) -> Option<Result<Option<String>, crate::bounded_read::CappedLineError>>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let Some(conn) = conn else {
            return Some(line.read(reader, max_bytes).await);
        };
        if !conn.expired() {
            tokio::select! {
                biased;
                () = conn.read_deadline() => {}
                read = line.read(reader, max_bytes) => return Some(read),
            }
        }
        if line.partial() || pending(reader, socket) {
            conn.lose();
        }
        None
    }

    /// Whether a connection still has bytes to read: buffered by `reader`,
    /// or waiting on `socket`.
    fn pending<R: tokio::io::AsyncRead>(
        reader: &tokio::io::BufReader<R>,
        socket: BorrowedFd<'_>,
    ) -> bool {
        if !reader.buffer().is_empty() {
            return true;
        }
        let mut pending: libc::c_int = 0;
        // SAFETY: `FIONREAD` writes one `int`; `socket` is a borrowed, open
        // descriptor.
        unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut pending) == 0 && pending > 0 }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            let key = key_of(&self.helper);
            let mut inner = self.observer.lock();
            let mut peer = None;
            if let Some(open) = inner.watch.open.get_mut(&key) {
                open.retain(|d| !Arc::ptr_eq(d, &self.departure));
                if open.is_empty() {
                    inner.watch.open.remove(&key);
                    peer = inner.watch.peers.remove(&key);
                }
            }
            drop(inner);
            // Closed with no lock held.
            drop(peer);
            self.observer.bell.ring();
        }
    }

    fn key_of<P>(pinned: &Pinned<P>) -> RecordKey {
        RecordKey {
            pid: pinned.pid,
            start: pinned.start(),
        }
    }

    /// The daemon's time namespace, read once (Q81).
    fn daemon_time_ns() -> Result<FileId, Option<i32>> {
        static NS: std::sync::OnceLock<Result<FileId, Option<i32>>> = std::sync::OnceLock::new();
        *NS.get_or_init(|| {
            std::fs::metadata("/proc/self/ns/time")
                .map(|m| FileId {
                    dev: m.dev(),
                    ino: m.ino(),
                })
                .map_err(|e| e.raw_os_error())
        })
    }

    fn ticks_per_sec() -> u64 {
        // SAFETY: `sysconf` has no memory effects.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        u64::try_from(ticks).unwrap_or(0)
    }

    fn boot_now_ns() -> Option<u64> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a valid timespec to write.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
            return None;
        }
        u64::try_from(ts.tv_sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(ts.tv_nsec).ok()?)
    }

    /// `start` as a birth floor, as of now.
    fn floor_of(start: u64) -> Option<Instant> {
        let now = Instant::now();
        super::birth_floor(start, ticks_per_sec(), now, boot_now_ns()?)
    }

    fn exe_id(pid: u32) -> Option<FileId> {
        std::fs::metadata(format!("/proc/{pid}/exe"))
            .ok()
            .map(|m| FileId {
                dev: m.dev(),
                ino: m.ino(),
            })
    }

    /// `pid`'s executable and argv, `None` when either can't be read or the
    /// argv is longer than [`ARGV_BYTES`].
    fn proc_facts(pid: u32) -> Option<ProcFacts> {
        let exe = exe_id(pid)?;
        let mut bytes = Vec::new();
        std::fs::File::open(format!("/proc/{pid}/cmdline"))
            .ok()?
            .take(ARGV_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > ARGV_BYTES {
            return None;
        }
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        let argv = bytes
            .split(|b| *b == 0)
            .map(|arg| OsStr::from_bytes(arg).to_os_string())
            .collect();
        Some(ProcFacts { exe, argv })
    }

    /// The executable a shell (or the deck's spawn) runs for `program` with
    /// `path` as its `PATH`: an absolute path as it is, a bare name by the
    /// first directory holding an executable file of that name. A relative
    /// path, or a relative or empty `PATH` entry reached before a match,
    /// resolves nothing: what it names depends on a directory binding
    /// doesn't read.
    fn resolve_program(program: &OsStr, path: Option<&OsStr>) -> Option<FileId> {
        let file = |candidate: &std::path::Path| {
            std::fs::metadata(candidate)
                .ok()
                .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .map(|m| FileId {
                    dev: m.dev(),
                    ino: m.ino(),
                })
        };
        let named = std::path::Path::new(program);
        if program.as_bytes().contains(&b'/') {
            return named.is_absolute().then(|| file(named)).flatten();
        }
        for dir in std::env::split_paths(path?) {
            if !dir.is_absolute() {
                return None;
            }
            if let Some(found) = file(&dir.join(program)) {
                return Some(found);
            }
        }
        None
    }

    fn exited(pidfd: &OwnedFd) -> bool {
        LinuxKernel::default().exited(pidfd).unwrap_or(true)
    }

    fn kill(pidfd: &OwnedFd) -> bool {
        // SAFETY: `pidfd_send_signal(fd, SIGKILL, NULL, 0)` on a pidfd we
        // hold: it signals exactly the process the pidfd names.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            ) == 0
        }
    }

    /// A process's children, from every thread's `children` file.
    fn children(pid: u32) -> Vec<u32> {
        let mut found = Vec::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return found;
        };
        for task in tasks.flatten() {
            if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
                found.extend(
                    text.split_whitespace()
                        .filter_map(|c| c.parse::<u32>().ok()),
                );
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    /// A marker read of an exited record's helper: its marker names, and
    /// whether the read was complete.
    struct MarkerRead {
        mine: Vec<(String, record::Marker)>,
        complete: bool,
    }

    impl Observer {
        /// Start generation `spawned`'s observer from its installation: its
        /// records attached, its arm page held, its thread running, and its
        /// bell in `slot`. `None` when its name isn't a number, which every
        /// generation the daemon spawns has, or its instance can't be read.
        pub fn start(
            installation: Installation,
            spawned: Spawned,
            slot: &super::BellSlot,
        ) -> Option<Arc<Observer>> {
            let generation: u64 = spawned.agent_id.parse().ok()?;
            let class = installation.class();
            let (kind, agent) = match class {
                PaneClass::Direct(agent) | PaneClass::Launcher(agent) => (
                    Kind::Agent(agent),
                    Some(match agent {
                        ProbeAgent::Claude => AgentType::ClaudeCode,
                        ProbeAgent::Codex => AgentType::Codex,
                    }),
                ),
                _ => (Kind::Learned, spawned.agent),
            };
            let reader = InstanceReader::open(installation.instance_dir()).ok()?;
            let helper_id = installation.helper_id();
            let launch = installation.launch().to_vec();
            if !spawned
                .input
                .attach_records(Records::new(generation, EC_POLICY, Instant::now()))
            {
                return None;
            }
            let bell = Arc::new(Bell::default());
            let observer = Arc::new(Observer {
                agent_id: spawned.agent_id,
                generation,
                agent,
                class,
                kind,
                env: spawned.env,
                child: spawned.child,
                launch,
                reader,
                helper_id,
                latch: spawned.latch,
                assumed: spawned.assumed,
                settling: SettleClaim::default(),
                inner: Mutex::new(Inner {
                    seam: Seam::new(generation, spawned.input, installation.into_files()),
                    watch: Watch::default(),
                }),
                connections: Arc::new(Pool::new(CONNECTIONS_PER_GENERATION)),
                bell: Arc::clone(&bell),
                stopped: std::sync::atomic::AtomicBool::new(false),
            });
            let weak = Arc::downgrade(&observer);
            let thread = std::thread::Builder::new()
                .name(format!("composer-observer-{generation}"))
                .spawn(move || run(weak))
                .ok()?;
            bell.bind(thread.thread().clone());
            let _ = slot.set(bell);
            Some(observer)
        }

        /// The generation left its pane: the thread ends at its next wake.
        pub fn stop(&self) {
            self.stopped
                .store(true, std::sync::atomic::Ordering::Release);
            self.bell.ring();
        }

        /// The generation's input, its records attached.
        pub fn input(&self) -> Arc<GenerationInput> {
            Arc::clone(self.lock().seam.input())
        }

        /// The generation's permission as settled so far.
        pub fn permission(&self) -> Permission {
            self.lock().seam.permission()
        }

        /// `A`, once the permission settled `Armable` with it bound.
        pub fn bound_agent(&self) -> Option<Identity> {
            self.lock().watch.agent.clone()
        }

        /// `key`'s exit facts once its record drained: the record's own while
        /// it stays, the kept copy once it went.
        pub fn exit_fact(&self, key: RecordKey) -> Option<ExitFact> {
            let inner = self.lock();
            inner.seam.input().record_fact(key).or_else(|| {
                inner
                    .watch
                    .facts
                    .iter()
                    .rev()
                    .find(|f| f.key == key)
                    .copied()
            })
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn name(&self) -> String {
            self.generation.to_string()
        }

        /// A variable as the agent sees it: the spawn's own, else the
        /// daemon's.
        fn env_value(&self, name: &str) -> Option<OsString> {
            match self.env.iter().rev().find(|(k, _)| k == name) {
                Some((_, v)) => Some(v.into()),
                None => std::env::var_os(name),
            }
        }

        /// Attest `helper` (core §2.3) with no lock held: one `gen-G` read,
        /// the attribution and, for an `Owned` helper, the read with its
        /// revalidation, both with the same pin and the same agent. `None`
        /// when the attestation budget is spent or no birth floor can be
        /// given: an identity refusal.
        fn attest(&self, helper: &Pinned<OwnedFd>, argv: Option<&[String]>) -> Option<Evidence> {
            debug_assert!(
                crate::lock_rank::held_ranks().is_empty(),
                "attestation runs with no ranked lock held"
            );
            if !ATTESTATIONS.take() {
                return None;
            }
            struct Give;
            impl Drop for Give {
                fn drop(&mut self) {
                    ATTESTATIONS.give();
                }
            }
            let _give = Give;
            let birth_floor = floor_of(helper.start())?;
            let identity = self.reader.identity(&self.name());
            let env = |name: &str| self.env_value(name);
            let agent = Agent {
                identity: identity.as_ref(),
                kind: self.kind,
                env: &env,
                // SAFETY: `getuid(2)` has no failure mode.
                uid: unsafe { libc::getuid() },
                time_ns: daemon_time_ns(),
            };
            let mut kernel = LinuxKernel::default();
            let attributed = attribute(&mut kernel, &agent, helper, argv);
            let attested = read_export(
                &mut kernel,
                &mut crate::platform::fsperm::fs_view::LinuxFsView,
                &agent,
                helper,
                attributed,
            );
            Some(Evidence {
                key: key_of(helper),
                birth_floor,
                attribution: attested.attribution,
                read: attested.export.as_ref().map(attest::Export::read),
            })
        }

        /// A composer connection's capability question (Q81): pin its peer by
        /// `SO_PEERPIDFD`, and register it when the peer runs this
        /// instance's helper copy and the budgets allow. `None` gets no
        /// capability. A helper whose exit was already observed is departed
        /// from the start.
        pub fn connect(
            self: &Arc<Self>,
            peer: std::io::Result<Option<OwnedFd>>,
        ) -> Option<Connection> {
            let mut kernel = LinuxKernel::default();
            let helper = attest::pin(&mut kernel, Sighting::Peer(peer)).ok()?;
            if exe_id(helper.pid) != Some(self.helper_id) {
                return None;
            }
            let slot = Slot::take(&self.connections, &CONNECTIONS)?;
            let key = key_of(&helper);
            let departure = Arc::new(Departure::default());
            let peer = helper.pidfd().try_clone().ok();
            {
                let mut inner = self.lock();
                if inner
                    .seam
                    .input()
                    .record(key)
                    .is_some_and(|r| r.exit_observed.is_some())
                {
                    departure.depart();
                }
                inner
                    .watch
                    .open
                    .entry(key)
                    .or_default()
                    .push(Arc::clone(&departure));
                if let Some(peer) = peer {
                    inner.watch.peers.entry(key).or_insert(peer);
                }
            }
            // Watched from the next pass on, at the scan's period rather
            // than the idle sweep's (Q85).
            self.bell.ring();
            Some(Connection {
                observer: Arc::clone(self),
                helper,
                departure,
                _slot: slot,
            })
        }

        /// An invocation on `conn` (core §2.3–§2.4): attested off the lock,
        /// then recorded and settled under it, the epoch moved for a
        /// non-`Armed` decision. The helper's pidfd is kept for its exit, and
        /// E2 may signal it: the connection's peer pidfd named this
        /// instance's helper copy.
        pub fn invocation(
            &self,
            conn: &Connection,
            request: &ComposerInvocationRequest,
        ) -> ComposerOutcome {
            if request.generation != self.generation {
                return ComposerOutcome::Uncertain;
            }
            let evidence = self.attest(&conn.helper, Some(&request.argv));
            let key = key_of(&conn.helper);
            let (outcome, late) = {
                let mut inner = self.lock();
                let late = evidence.is_some() && inner.seam.late(key);
                let outcome = inner.seam.invocation(request, evidence, Instant::now());
                if evidence.is_some()
                    && unexited(&inner, key)
                    && let Ok(fd) = conn.helper.pidfd().try_clone()
                {
                    hold(&mut inner.watch, key, fd, true);
                }
                (outcome, late)
            };
            if late {
                self.log_late(key, Channel::Ipc, super::decision(request.decision));
            }
            self.bell.ring();
            outcome
        }

        /// The report a helper sent after its editor, on `conn`. A failed
        /// start is logged with its errno and no path (amendment 6 §2.2).
        pub fn report(&self, conn: &Connection, report: EditorReport) {
            if let EditorReport::SpawnFailed { errno } = report {
                tracing::info!(
                    target: "composer_probe",
                    agent_id = %self.agent_id,
                    pid = conn.helper.pid,
                    errno,
                    "editor helper couldn't start the editor"
                );
            }
            self.lock()
                .seam
                .report(key_of(&conn.helper), report, Instant::now());
        }

        /// The e2e control verb (OQ10): addressed to `addressed`, refused for
        /// any other generation, and while the permission is still pending
        /// it waits up to [`CONTROL_PERMISSION_WAIT`] for it to settle.
        pub fn control(&self, addressed: u64, op: ControlOp) -> Result<u32, ControlRefusal> {
            if addressed != self.generation {
                return Err(ControlRefusal::Generation);
            }
            let waited = Instant::now();
            while self.lock().seam.permission() == Permission::Pending
                && waited.elapsed() < CONTROL_PERMISSION_WAIT
            {
                self.settle_permission();
                if self.lock().seam.permission() == Permission::Pending {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let result = self.lock().seam.control(addressed, op);
            self.bell.ring();
            result
        }

        /// `A` for a direct Claude, from the deck's own launch record
        /// ([`bound_agent`], Q84): the PTY child, pinned by the pid and start
        /// ticks the deck spawned, or its one child when the deck's shell
        /// runs it; pinned, its facts read again after the pin and its
        /// family read from its executable. `Err(true)` when it, or the PTY
        /// child, exited.
        fn bind(&self) -> Result<Identity, bool> {
            let (pty_pid, pty_start) = match (self.class, self.child) {
                (PaneClass::Direct(ProbeAgent::Claude), Some(child)) => child,
                _ => return Err(false),
            };
            let mut kernel = LinuxKernel::default();
            let pty_pin = attest::pin(
                &mut kernel,
                Sighting::Pid {
                    pid: pty_pid,
                    start: Some(pty_start),
                },
            )
            .map_err(|why| why == attest::Unpinned::Exited)?;
            let pty = proc_facts(pty_pid).ok_or(false)?;
            let below: Vec<(u32, ProcFacts)> = children(pty_pid)
                .into_iter()
                .filter(|c| kernel.stat(*c).is_ok_and(|stat| stat.ppid == pty_pid))
                .filter_map(|c| Some((c, proc_facts(c)?)))
                .collect();
            let path = self.env_value("PATH");
            let chosen = bound_agent(&self.launch, &pty, &below, |program| {
                resolve_program(program, path.as_deref())
            });
            // The reads were the PTY child's: it was alive after them.
            if kernel.exited(pty_pin.pidfd()).unwrap_or(true) {
                return Err(true);
            }
            let (pid, start, facts) = match chosen.ok_or(false)? {
                Bound::Pty => (pty_pid, pty_start, pty),
                Bound::Child(pid) => {
                    let facts = below
                        .into_iter()
                        .find_map(|(c, facts)| (c == pid).then_some(facts))
                        .ok_or(false)?;
                    (pid, kernel.stat(pid).map_err(|_| false)?.start, facts)
                }
            };
            let pinned = attest::pin(
                &mut kernel,
                Sighting::Pid {
                    pid,
                    start: Some(start),
                },
            )
            .map_err(|why| why == attest::Unpinned::Exited)?;
            // What was bound is what is pinned.
            if proc_facts(pid).as_ref() != Some(&facts) {
                return Err(false);
            }
            let exe = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|_| false)?;
            let root = exe.parent().ok_or(false)?.to_path_buf();
            let root_meta = std::fs::metadata(&root).map_err(|_| false)?;
            let identity = Identity {
                pid,
                start,
                exe: facts.exe,
                install_root: root,
                install_root_id: FileId {
                    dev: root_meta.dev(),
                    ino: root_meta.ino(),
                },
                basename: exe.file_name().ok_or(false)?.to_os_string(),
            };
            // Read again after the pin is polled: the reads are `A`'s.
            if kernel.exited(pinned.pidfd()).unwrap_or(true) {
                return Err(true);
            }
            if exe_id(pid) != Some(identity.exe) {
                return Err(false);
            }
            Ok(identity)
        }

        /// Plan §1.2's permission, once the header latched: with the e2e
        /// assumption's records in the `e2e` build. An armable generation
        /// gets `gen-G`, and one whose file can't be written is inert. One
        /// settler at a time ([`SettleClaim`], Q84): it computes and writes
        /// with no lock held, and publishes the permission with `A` under
        /// the seam lock only if the permission was still pending, so the
        /// identity file is written once, by the one settlement that wins,
        /// and nothing replaces what it published.
        fn settle_permission(&self) {
            let Some(_settling) = self.settling.claim() else {
                return;
            };
            if self.lock().seam.permission() != Permission::Pending {
                return;
            }
            // A wrapper's agent isn't learned yet (Q35-22): pending.
            let Some(agent) = self.agent.as_ref() else {
                return;
            };
            let latch = (self.latch)(agent);
            let header = (latch != LatchedRelease::Unseen).then_some(latch);
            let (binding, identity) = match self.bind() {
                Ok(identity) => (Binding::Bound, Some(identity)),
                Err(true) => (Binding::Never, None),
                Err(false) => (Binding::Waiting, None),
            };
            let records = crate::doorbell_activation::with_assumed_editor_probes(
                crate::doorbell_activation::RELEASE_RECORDS.to_vec(),
                self.assumed,
            );
            let mut permission =
                crate::composer_probe::permission(&records, agent, header.as_ref(), binding);
            if permission == Permission::Armable
                && let Some(identity) = &identity
                && let Err(cause) = self.reader.write_identity(&self.name(), identity)
            {
                tracing::warn!(
                    target: "composer_probe",
                    agent_id = %self.agent_id,
                    ?cause,
                    "editor probe identity not written; the generation stays inert"
                );
                permission = Permission::Inert(InertCause::Unbound);
            }
            let won = {
                let mut inner = self.lock();
                let won = inner.seam.set_permission(permission);
                if won && permission == Permission::Armable {
                    inner.watch.agent = identity;
                }
                won
            };
            if won {
                tracing::info!(
                    target: "composer_probe",
                    agent_id = %self.agent_id,
                    ?permission,
                    "editor probe permission settled"
                );
            }
        }

        /// A decision that came after its record's exit drained: ignored by
        /// the records, logged here with no lock held.
        fn log_late(&self, key: RecordKey, channel: Channel, decided: record::Decision) {
            tracing::warn!(
                target: "composer_probe",
                agent_id = %self.agent_id,
                pid = key.pid,
                ?channel,
                ?decided,
                "editor helper decision after its exit settled; ignored"
            );
        }

        /// Whether the next wait is a scan's period.
        fn active(&self) -> bool {
            let inner = self.lock();
            let count = inner.seam.input().liability_count().unwrap_or(0);
            count > 0
                || !inner.watch.pins.is_empty()
                || !inner.watch.exited.is_empty()
                || !inner.watch.open.is_empty()
        }

        /// One pass: permission, scans, markers, exits, drains and kills.
        fn pass(&self) {
            self.settle_permission();
            let (count, agent) = {
                let inner = self.lock();
                (
                    inner.seam.input().liability_count().unwrap_or(0),
                    inner.watch.agent.clone(),
                )
            };
            let mut budget = ATTESTATIONS_PER_PASS;
            if (count > 0 || self.bell.take_scan())
                && let Some(agent) = &agent
            {
                self.scan(agent.pid, &mut budget);
            }
            self.markers(&mut budget);
            self.exits();
            self.drains(&mut budget);
            self.kills();
        }

        /// The child scan: this instance's helper copies among `A`'s
        /// children, each attested and observed once. A pin a marker made
        /// for one of them is replaced by the scan's, which E2 may signal.
        fn scan(&self, a: u32, budget: &mut u32) {
            for pid in children(a) {
                if exe_id(pid) != Some(self.helper_id) {
                    continue;
                }
                let mut kernel = LinuxKernel::default();
                let Ok(helper) = attest::pin(&mut kernel, Sighting::Pid { pid, start: None })
                else {
                    continue;
                };
                // Pinned, it is still this instance's helper below `A`.
                if helper.stat.ppid != a || exe_id(pid) != Some(self.helper_id) {
                    continue;
                }
                let key = key_of(&helper);
                {
                    let mut inner = self.lock();
                    if inner.seam.input().record(key).is_some() {
                        if inner.watch.pins.get(&key).is_some_and(|p| !p.killable) {
                            hold(&mut inner.watch, key, helper.into_pidfd(), true);
                        }
                        continue;
                    }
                }
                if *budget == 0 {
                    return;
                }
                *budget -= 1;
                let evidence = self.attest(&helper, None);
                let mut inner = self.lock();
                let now = Instant::now();
                match evidence {
                    Some(e) => {
                        let obs =
                            scan_observation(self.generation, e.key, e.attribution, e.birth_floor);
                        if inner.seam.observe(&obs, now).is_ok() && unexited(&inner, key) {
                            hold(&mut inner.watch, key, helper.into_pidfd(), true);
                        }
                    }
                    None => {
                        let _ = inner.seam.refused(Channel::Scan, None, None, now);
                    }
                }
            }
        }

        /// The markers in `inv/G/`: each one not yet seen is applied and
        /// renamed `.seen`; a name that isn't a marker is left alone.
        fn markers(&self, budget: &mut u32) {
            let name = self.name();
            let Ok((names, more)) = self.reader.markers(&name, MARKER_NAMES_PER_PASS) else {
                return;
            };
            if more {
                let first = !std::mem::replace(&mut self.lock().watch.marker_overflow, true);
                if first {
                    tracing::warn!(
                        target: "composer_probe",
                        agent_id = %self.agent_id,
                        "more editor helper markers than a pass reads"
                    );
                }
            }
            for file in names {
                let Some(marker) = record::parse_marker(&file) else {
                    continue;
                };
                if marker.renamed {
                    continue;
                }
                if *budget == 0 {
                    return;
                }
                self.apply_marker(&name, &file, &marker, budget);
            }
        }

        /// Apply one marker. A marker reports a decision for the process its
        /// name gives; it never makes that process one E2 may signal (Q84):
        /// a pin taken from its name only watches the exit.
        fn apply_marker(&self, name: &str, file: &str, marker: &record::Marker, budget: &mut u32) {
            let known = self.lock().seam.input().record(marker.key);
            let mut kernel = LinuxKernel::default();
            let pinned = attest::pin(
                &mut kernel,
                Sighting::Pid {
                    pid: marker.key.pid,
                    start: Some(marker.key.start),
                },
            );
            let evidence = match (&pinned, &known) {
                // An attributed record takes no second attribution.
                (_, Some(r)) if r.attribution != Attribution::Pending => Some(Evidence {
                    key: marker.key,
                    birth_floor: r.birth_floor,
                    attribution: r.attribution,
                    read: None,
                }),
                (Ok(helper), _) => {
                    *budget = budget.saturating_sub(1);
                    self.attest(helper, None)
                }
                // Exited, with a record: nothing attributed it while alive.
                (Err(_), Some(r)) => Some(Evidence {
                    key: marker.key,
                    birth_floor: r.birth_floor,
                    attribution: Attribution::Unattributable,
                    read: None,
                }),
                (Err(_), None) => None,
            };
            let now = Instant::now();
            let late = {
                let mut inner = self.lock();
                let late = evidence.is_some() && inner.seam.late(marker.key);
                match evidence {
                    Some(e) => {
                        let obs = marker_observation(
                            self.generation,
                            marker,
                            e.attribution,
                            e.birth_floor,
                        );
                        let _ = inner.seam.observe(&obs, now);
                        if let Ok(helper) = pinned
                            && unexited(&inner, marker.key)
                        {
                            hold(&mut inner.watch, marker.key, helper.into_pidfd(), false);
                        }
                    }
                    None => {
                        let _ =
                            inner
                                .seam
                                .refused(Channel::Marker, Some(marker.decision), None, now);
                    }
                }
                late
            };
            if late {
                self.log_late(marker.key, Channel::Marker, marker.decision);
            }
            if let Err(error) = self.reader.mark_seen(name, file) {
                tracing::debug!(
                    target: "composer_probe",
                    agent_id = %self.agent_id,
                    %error,
                    "editor helper marker not renamed"
                );
            }
        }

        /// Exits: a pinned helper whose pidfd reads ready. Its connections
        /// are told, so each reads until its read deadline; so are those of
        /// a helper with no pin whose own pidfd reads ready (Q85).
        fn exits(&self) {
            let mut inner = self.lock();
            let gone: Vec<RecordKey> = inner
                .watch
                .pins
                .iter()
                .filter(|(_, pin)| exited(&pin.fd))
                .map(|(k, _)| *k)
                .collect();
            for key in gone {
                inner.watch.pins.remove(&key);
                let at = Instant::now();
                if inner.seam.exit(key, at, at).is_ok() {
                    inner.watch.exited.insert(key, 0);
                    for departure in inner.watch.open.get(&key).into_iter().flatten() {
                        departure.depart();
                    }
                }
            }
            let Inner { seam, watch } = &mut *inner;
            for (key, peer) in &watch.peers {
                if watch.pins.contains_key(key) {
                    continue;
                }
                let departures = watch.open.get(key).into_iter().flatten();
                let waiting: Vec<&Arc<Departure>> = departures.filter(|d| !d.departed()).collect();
                if !waiting.is_empty() && exited(peer) {
                    for departure in waiting {
                        departure.depart();
                    }
                }
            }
            // A lost connection's mark outlives nothing it could still
            // matter to (Q85): a drained record, or a helper with no record
            // that exited with no connection open, which no exit drain can
            // reach any more.
            watch
                .unread
                .retain(|key, pidfd| match seam.input().record(*key) {
                    Some(record) => !record.drained,
                    None => {
                        watch.open.contains_key(key) || pidfd.as_ref().is_some_and(|fd| !exited(fd))
                    }
                });
        }

        /// This pass's read of `key`'s markers for its drain: complete only
        /// when the directory read neither failed nor overflowed and every
        /// marker of `key`'s not yet seen was applied within `budget`.
        fn drain_markers(
            &self,
            listing: &std::io::Result<(Vec<String>, bool)>,
            key: RecordKey,
            budget: &mut u32,
        ) -> MarkerRead {
            let name = self.name();
            let Ok((names, more)) = listing else {
                return MarkerRead {
                    mine: Vec::new(),
                    complete: false,
                };
            };
            let mine: Vec<(String, record::Marker)> = names
                .iter()
                .filter_map(|n| record::parse_marker(n).map(|m| (n.clone(), m)))
                .filter(|(_, m)| m.key == key)
                .collect();
            let mut complete = !more;
            for (file, marker) in mine.iter().filter(|(_, m)| !m.renamed) {
                if *budget == 0 {
                    complete = false;
                    break;
                }
                self.apply_marker(&name, file, marker, budget);
            }
            MarkerRead { mine, complete }
        }

        /// Drains (core §2.4, Q84): an exited record settles by its exit only
        /// once every composer connection of its helper finished and this
        /// pass read its markers completely. Otherwise it is rescheduled; a
        /// record still unread after [`super::DRAIN_RETRIES`] passes, or one
        /// a connection ended with bytes unread once its markers are read
        /// (Q85), gives up to the uncertain outcome and keeps its markers and
        /// its record. A
        /// settled one's markers are deleted with no lock held, and it is
        /// pruned, its facts kept, once it may go.
        fn drains(&self, budget: &mut u32) {
            let name = self.name();
            let waiting: Vec<(RecordKey, u32, bool, u32)> = {
                let inner = self.lock();
                inner
                    .watch
                    .exited
                    .iter()
                    .map(|(key, retries)| {
                        let open = inner.watch.open.get(key).map_or(0, Vec::len);
                        let unread = inner.watch.unread.contains_key(key);
                        (
                            *key,
                            u32::try_from(open).unwrap_or(u32::MAX),
                            unread,
                            *retries,
                        )
                    })
                    .collect()
            };
            if waiting.is_empty() {
                return;
            }
            // The markers again, before any settles by its exit.
            let listing = waiting
                .iter()
                .any(|(_, open, _, _)| *open == 0)
                .then(|| self.reader.markers(&name, MARKER_NAMES_PER_PASS));
            for (key, open, unread, retries) in waiting {
                let read = match &listing {
                    Some(listing) if open == 0 => self.drain_markers(listing, key, budget),
                    _ => MarkerRead {
                        mine: Vec::new(),
                        complete: false,
                    },
                };
                let now = Instant::now();
                match drain_step(open, read.complete, unread, retries) {
                    DrainStep::Wait => {
                        if let Some(retries) = self.lock().watch.exited.get_mut(&key) {
                            *retries += 1;
                        }
                    }
                    DrainStep::GiveUp => {
                        let mut inner = self.lock();
                        inner.watch.exited.remove(&key);
                        inner.watch.unread.remove(&key);
                        let _ = inner.seam.drained_uncertain(key, now);
                        drop(inner);
                        tracing::warn!(
                            target: "composer_probe",
                            agent_id = %self.agent_id,
                            pid = key.pid,
                            open,
                            unread,
                            "editor helper's decision evidence not all read after its exit; \
                             settled as uncertain"
                        );
                    }
                    DrainStep::Settle => {
                        let drained = {
                            let mut inner = self.lock();
                            inner.watch.exited.remove(&key);
                            inner.watch.unread.remove(&key);
                            inner.seam.drained(key, now)
                        };
                        let Ok(drained) = drained else {
                            continue;
                        };
                        let deleted = read.mine.iter().all(|(file, m)| {
                            let seen = if m.renamed {
                                file.clone()
                            } else {
                                format!("{file}.seen")
                            };
                            self.reader.remove_marker(&name, file).is_ok()
                                && self.reader.remove_marker(&name, &seen).is_ok()
                        });
                        let mut inner = self.lock();
                        let fact = match inner.seam.prune(key, deleted, Instant::now()) {
                            Ok(Some(last)) => last,
                            _ => inner.seam.input().record_fact(key).unwrap_or(drained.fact),
                        };
                        let facts = &mut inner.watch.facts;
                        facts.retain(|f| f.key != key);
                        if facts.len() == FACTS_KEPT {
                            facts.pop_front();
                        }
                        facts.push_back(fact);
                    }
                }
            }
        }

        /// E2 (core §5.4, Q84): an `Armed` helper alive past its bound, held
        /// by a pin E2 may signal, and still running this instance's helper
        /// copy (its executable read first, its liveness after, so the read
        /// was the pinned process's). Each check after the first is made
        /// with no lock held; one that no longer runs the helper copy is
        /// never signalled.
        fn kills(&self) {
            let due: Vec<(RecordKey, OwnedFd)> = {
                let inner = self.lock();
                let now = Instant::now();
                inner
                    .watch
                    .pins
                    .iter()
                    .filter(|(key, pin)| {
                        pin.killable
                            && inner
                                .seam
                                .input()
                                .record(**key)
                                .is_some_and(|r| e2_due(&r, now))
                    })
                    .filter_map(|(key, pin)| Some((*key, pin.fd.try_clone().ok()?)))
                    .collect()
            };
            for (key, fd) in due {
                if exe_id(key.pid) != Some(self.helper_id) {
                    if exited(&fd) {
                        continue;
                    }
                    if let Some(pin) = self.lock().watch.pins.get_mut(&key) {
                        pin.killable = false;
                    }
                    tracing::warn!(
                        target: "composer_probe",
                        agent_id = %self.agent_id,
                        pid = key.pid,
                        "editor helper alive after its armed bound no longer runs the helper; \
                         not killed"
                    );
                    continue;
                }
                if exited(&fd) {
                    continue;
                }
                let killed = kill(&fd);
                tracing::warn!(
                    target: "composer_probe",
                    agent_id = %self.agent_id,
                    pid = key.pid,
                    killed,
                    "editor helper still alive after its armed bound; killed"
                );
            }
        }
    }

    /// Whether `key` has a record whose exit isn't observed yet: only such a
    /// helper is pinned, so an exit is observed, and drained, once.
    fn unexited(inner: &Inner, key: RecordKey) -> bool {
        inner
            .seam
            .input()
            .record(key)
            .is_some_and(|r| r.exit_observed.is_none())
    }

    /// Hold `key`'s helper by `fd`: a pin E2 may signal replaces one it may
    /// not, and nothing else replaces a pin.
    fn hold(watch: &mut Watch, key: RecordKey, fd: OwnedFd, killable: bool) {
        match watch.pins.get(&key) {
            Some(pin) if pin.killable || !killable => {}
            _ => {
                watch.pins.insert(key, Pin { fd, killable });
            }
        }
    }

    /// The observer's thread: a pass, then a wait, until the observer goes
    /// or stops, or the generation retires.
    fn run(weak: Weak<Observer>) {
        loop {
            let Some(observer) = weak.upgrade() else {
                return;
            };
            if observer.stopped.load(std::sync::atomic::Ordering::Acquire)
                || observer.lock().seam.input().retired()
            {
                return;
            }
            observer.pass();
            let wait = if observer.active() {
                SCAN_PERIOD
            } else {
                IDLE_SWEEP
            };
            drop(observer);
            std::thread::park_timeout(wait);
        }
    }
}
