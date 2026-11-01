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
//! report it refused or couldn't parse, left evidence unread ([`read_line`]):
//! its record, one made after the loss included, never settles clean, only
//! uncertain. That mark is kept while the helper lives (Q87), within
//! [`LOST_PER_GENERATION`] and [`LOST_TOTAL`]; a loss past them settles every
//! record of its generation uncertain instead. A marker read that overflows or fails is no drain: nothing
//! settles, and no marker is taken as deleted. Until both hold the drain is
//! rescheduled; after [`DRAIN_RETRIES`] passes (a count, not a time) it
//! gives up to the uncertain outcome
//! (`NotConsumed(Unproven)` for an open record, no fallback pop), never to
//! the clean one, and keeps its record and markers. A decision that arrives
//! after the exit drained is logged and changes nothing: the helper is told
//! the settled outcome (core §2.4's table). Work is bounded per
//! generation and in total ([`Pool`]): composer connections open at once,
//! the bytes of the one report read after an invocation, attestations at
//! once and per pass, marker names read per pass, and lost-connection marks
//! kept. Past a bound a scan or a marker waits for the next pass; a
//! connection gets no capability; a loss with no mark left latches the
//! generation's overflow; an attestation not run is an identity refusal
//! (`record_refused`, with the request's claim on IPC and none for a scan, an
//! unread connection or a marker).
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
//! helper's, one a connection that lost bytes left, Q85), each helper's one hold
//! shared by its connections, its watches and its pin (Q87), and the `close` of
//! a hold whose last holder goes. No path is opened, read, renamed,
//! unlinked or written under it.
//! Retirement: the pane's successor retires the input (records and scope
//! dropped, M-FIRST taken in the same section) and stops the observer; the
//! observer's thread holds it weakly and ends at its next wake, and the arm
//! page is unmapped when the record goes.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::Permission;
use super::admission::{Outside, Refusal as AdmissionRefusal};
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

/// Helpers kept as having lost a connection's bytes (Q87), per generation
/// and in total: each holds its helper's pidfd until the mark may go. One
/// more than a pool allows latches its generation's loss overflow instead,
/// and every record of that generation then drains only uncertain.
pub const LOST_PER_GENERATION: u32 = 8;
pub const LOST_TOTAL: u32 = 64;

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

/// Lost-connection marks kept in the daemon (Q87).
pub static LOST: Pool = Pool::new(LOST_TOTAL);

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
/// takes no lock and wakes every waiter. Its clock is tokio's, which is the
/// system's monotonic clock outside a runtime whose clock a test paused.
#[derive(Debug, Default)]
pub struct Departure {
    at: OnceLock<tokio::time::Instant>,
    notify: tokio::sync::Notify,
}

impl Departure {
    /// The helper's exit was observed. A second mark moves nothing.
    pub fn depart(&self) {
        self.at.get_or_init(tokio::time::Instant::now);
        self.notify.notify_waiters();
    }

    pub fn departed(&self) -> bool {
        self.at.get().is_some()
    }

    /// When the reading ends: [`READ_DEADLINE`] after the first mark.
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.at.get().map(|at| *at + READ_DEADLINE)
    }

    /// Whether the read deadline has passed.
    pub fn expired(&self) -> bool {
        self.deadline()
            .is_some_and(|end| tokio::time::Instant::now() >= end)
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
            tokio::time::sleep_until(end).await;
        }
    }
}

/// The next line on a composer connection whose helper's exit `departure`
/// marks (core §2.4, Q85), or `None` once the connection is to end. `line`
/// is the connection's accumulator, kept by the caller across calls. A read
/// races the read deadline and the deadline wins a tie, so a peer that keeps
/// sending can't extend it: a call made after the deadline reads nothing,
/// and one made before it ends at the deadline itself, however recently the
/// last line came. At the deadline a partial line in `line`, or bytes still
/// to read by `pending`, call `lose`. A line returned before the deadline is
/// the caller's to apply.
pub async fn read_before<R>(
    departure: &Departure,
    line: &mut crate::bounded_read::CappedLine,
    reader: &mut tokio::io::BufReader<R>,
    max_bytes: usize,
    pending: impl FnOnce(&tokio::io::BufReader<R>) -> bool,
    lose: impl FnOnce(),
) -> Option<Result<Option<String>, crate::bounded_read::CappedLineError>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    if !departure.expired() {
        tokio::select! {
            biased;
            () = departure.read_deadline() => {}
            read = line.read(reader, max_bytes) => return Some(read),
        }
    }
    if line.partial() || pending(reader) {
        lose();
    }
    None
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
    /// Publish `count`, leaving the segment and native numbers as the page
    /// last held them.
    fn publish(&mut self, count: u32);

    /// Publish `count` with `seg_seq`, the probe's segment number, in one
    /// publication (Q35-19, R5 §4.2): the helper reads both in the same
    /// seqlock. A page with no segment word keeps only the count. The arm
    /// calls it under the generation's input lock
    /// ([`super::epoch::GenerationInput::arm_probe`]), so it must take no
    /// lock and do no I/O.
    fn publish_probe(&mut self, count: u32, seg_seq: u64) {
        let _ = seg_seq;
        self.publish(count);
    }

    /// Publish `count` with `native_seq`, the last native Ctrl+G candidate
    /// the planner numbered (Q35-16, A2 §4.4). A page with no native word
    /// keeps nothing of it.
    fn publish_native(&mut self, count: u32, native_seq: u64) {
        let _ = (count, native_seq);
    }
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
    /// The segment number the page shows.
    seg_seq: u64,
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
            seg_seq: 0,
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

    /// Publish `native_seq`, the last number of a native Ctrl+G candidate
    /// about to be submitted (Q35-16, A2 §4.4), with the count the page
    /// already shows: the helper reads it in the same seqlock as the count.
    pub fn publish_native(&mut self, native_seq: u64) {
        self.page.publish_native(self.published, native_seq);
    }

    /// Probe admission at `now` with this seam's permission (Q35-19, A4 §4):
    /// what each of the deck's check writes asks when it is scheduled, the
    /// first probe, a scheduled retry and Claude's lapse retry alike
    /// (`outside.attempt` names which). Writes nothing.
    pub fn admits(&self, outside: &Outside, now: Instant) -> Result<(), AdmissionRefusal> {
        let outside = Outside {
            permission: self.permission,
            ..*outside
        };
        self.input.admit_probe(&outside, now)
    }

    /// **The arm increment** (Q35-19, A4 §4, A5 §1.5): admission asked again
    /// with `outside` as the writer thread reads it when it comes to the
    /// probe's job, then, only on `Ok`, `count + 1` and `seg_seq` published
    /// in one publication and the raise recorded, stamped after the store.
    /// Run under the seam lock, which every record step that can open the
    /// writer scope or leave a record incomplete holds too, so none lands
    /// between the check and the publication; a native candidate enters the
    /// hazard FIFO only from the writer thread, which is the one running
    /// this. The check, the publication and the record are one critical
    /// section of the generation's input lock ([`GenerationInput::arm_probe`]),
    /// which retirement takes as well (the Q35-19 review's U2), so a
    /// retirement lands before the arm, which then refuses, or after it.
    /// The output hook takes that lock too, so it can't change `B` or latch
    /// a `Quiet` entry between admission and the publication; what it
    /// changes after the section and before the byte could as well land
    /// just after the byte. **A refusal
    /// publishes nothing and records nothing**: every refusal, retirement's
    /// included, comes before the store. The new count.
    pub fn arm(
        &mut self,
        seg_seq: u64,
        outside: &Outside,
        now: Instant,
    ) -> Result<u32, AdmissionRefusal> {
        let outside = Outside {
            permission: self.permission,
            ..*outside
        };
        let input = Arc::clone(&self.input);
        let page = &mut self.page;
        let raised = input.arm_probe(self.generation, &outside, now, |raised| {
            page.publish_probe(raised, seg_seq);
        })?;
        self.published = raised;
        self.seg_seq = seg_seq;
        Ok(raised)
    }

    /// The take-back of an arm whose job a withdrawal won after
    /// [`Self::arm`] published it (`super::probe::start`): the liability
    /// leaves with the job, stamped before the store. The new count.
    pub fn disarm(&mut self) -> Result<u32, Refusal> {
        // A store to zero is stamped before it.
        let stamp = Instant::now();
        let lowered = self.input.liability_lower(self.generation, stamp);
        self.sync();
        lowered
    }

    /// One more liability on the page with `seg_seq`, a store from zero
    /// stamped after it, for the `e2e` build's control verb
    /// ([`Self::control`]): the same critical section as [`Self::arm`]'s,
    /// with no admission. The new count.
    fn raise(&mut self, seg_seq: u64) -> Result<u32, Refusal> {
        let input = Arc::clone(&self.input);
        let page = &mut self.page;
        let raised = input.raise_published(self.generation, |raised| {
            page.publish_probe(raised, seg_seq);
        })?;
        self.published = raised;
        self.seg_seq = seg_seq;
        Ok(raised)
    }

    /// Publish `seg_seq`, a `HumanG`'s segment number, with the count the
    /// page shows (Q35-20, core §4.5): the helper the agent runs for it reads
    /// the number in the same seqlock as the count.
    pub fn publish_segment(&mut self, seg_seq: u64) {
        self.seg_seq = seg_seq;
        self.page.publish_probe(self.published, seg_seq);
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
            // The page keeps the segment number it shows.
            ControlStep::Raise => self.raise(self.seg_seq),
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
        // Q35-20 (R3 §3.4): the daemon's read of an `Armed` export, kept for
        // the epoch's settlement of the probe its record matches.
        if decided == Decision::Armed
            && let Some(e) = evidence
        {
            self.input
                .note_read(e.key, matches!(read, Some(ExportRead::Empty)));
        }
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
    /// report asks the generation's reader for a drain frontier (Q35-20a),
    /// which the record learns when the reader takes it, or as missing at
    /// once where no reader takes frontiers.
    pub fn report(&mut self, key: RecordKey, got: EditorReport, now: Instant) {
        if let Ok(record::Reported::RequestFrontier) =
            self.input
                .record_report(self.generation, key, report(got), now)
        {
            self.input.request_frontier(self.generation, Some(key));
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

#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux::HeldForTest;
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
        IDLE_SWEEP, LOST, LOST_PER_GENERATION, MARKER_NAMES_PER_PASS, Page, Pool, ProcFacts,
        SCAN_PERIOD, Seam, SettleClaim, Slot, bound_agent, drain_step, e2_due, marker_observation,
        read_before, scan_observation,
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
            let mut publisher = self.arm().publisher();
            let seg_seq = publisher.seg_seq();
            publisher.publish(count, seg_seq);
        }

        fn publish_probe(&mut self, count: u32, seg_seq: u64) {
            self.arm().publisher().publish(count, seg_seq);
        }

        fn publish_native(&mut self, count: u32, native_seq: u64) {
            let mut publisher = self.arm().publisher();
            let seg_seq = publisher.seg_seq();
            publisher.publish_native(count, seg_seq, native_seq);
        }
    }

    /// The most exit facts an observer keeps after their records go.
    const FACTS_KEPT: usize = record::RECORD_CAP;

    /// The most bytes of a process's argv binding reads.
    const ARGV_BYTES: u64 = 128 << 10;

    /// A helper held by its pidfd (Q87): one per helper, shared by its
    /// connections, its connected watch (`peers`), its lost mark (`unread`)
    /// and its pin, so a watch never depends on duplicating a descriptor.
    type Held = Arc<Pinned<OwnedFd>>;

    /// A record's helper, held by its pidfd for its exit.
    struct Pin {
        fd: Held,
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
        /// Each helper in `open` (Q85), held for its exit when no record
        /// pins it: its connections' read deadline then runs from that exit
        /// too, so a helper that never got a record leaves no connection
        /// open behind it. Admission puts the connection's own hold here
        /// (Q87), so no connection is open unwatched. Removed with its
        /// `open` entry.
        peers: HashMap<RecordKey, Held>,
        /// Helpers one of whose connections ended with bytes it didn't read
        /// (Q85), held for the helper's exit: their drain settles only
        /// uncertain, and a record made for one later inherits that. Kept
        /// until the record drains, or, for a helper with no record, until
        /// it exited with no connection left open ([`Observer::exits`]);
        /// never while it lives. Each takes a unit of
        /// [`super::LOST_PER_GENERATION`] and [`super::LOST_TOTAL`].
        unread: HashMap<RecordKey, Lost>,
        /// A loss no mark could be kept for (Q87): the lost pools were
        /// full. Until the generation retires every record of it drains only
        /// uncertain, since any of them might be the one whose loss went
        /// unmarked.
        lost_overflow: bool,
        /// The last exit facts of records that went: no segment holds them
        /// yet (Q35-20).
        facts: std::collections::VecDeque<ExitFact>,
        /// `A`, once bound.
        agent: Option<Identity>,
        /// More marker names than a pass reads.
        marker_overflow: bool,
        /// Records whose editor child was reported running (Q35-20).
        editors: Vec<RecordKey>,
    }

    /// A lost mark's hold on its helper, and its unit of the lost pools.
    struct Lost {
        helper: Held,
        _slot: Slot,
    }

    impl Watch {
        /// The hold already kept for `key`'s helper, if any (Q87): its
        /// connections', its lost mark's or its pin's.
        fn held(&self, key: RecordKey) -> Option<Held> {
            self.peers
                .get(&key)
                .or_else(|| self.unread.get(&key).map(|lost| &lost.helper))
                .or_else(|| self.pins.get(&key).map(|pin| &pin.fd))
                .cloned()
        }
    }

    struct Inner {
        seam: Seam<GenerationFiles>,
        watch: Watch,
    }

    /// Q35-20 (O-20.7b): a generation's keymap sources as read when its
    /// permission settled, and what an admission compares a fresh read with.
    enum KeymapEvidence {
        Claude(crate::composer_probe::keymap::ClaudeKeymap),
        Codex {
            sources: crate::composer_probe::keymap::CodexSources,
            before: Box<crate::composer_probe::keymap::CodexRead>,
            precedence: &'static [crate::doorbell_activation::KeymapSource],
        },
        Held(crate::composer_probe::keymap::Hold),
    }

    impl KeymapEvidence {
        fn check(&self) -> Result<(), crate::composer_probe::keymap::Hold> {
            match self {
                KeymapEvidence::Claude(keymap) => keymap.check(),
                KeymapEvidence::Codex {
                    sources,
                    before,
                    precedence,
                } => {
                    crate::composer_probe::keymap::codex_keymap(before, &sources.read(), precedence)
                }
                KeymapEvidence::Held(hold) => Err(*hold),
            }
        }
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
        lost: Arc<Pool>,
        bell: Arc<Bell>,
        stopped: std::sync::atomic::AtomicBool,
        /// Q35-20 (O-20.7b): the keymap sources as read when the
        /// permission settled `Armable`.
        keymap: std::sync::OnceLock<KeymapEvidence>,
        /// A test's park in the drain (Q91), called with each exited
        /// record's key between the drain's snapshot and its settlement,
        /// with no lock held.
        #[cfg(test)]
        drain_park: std::sync::Mutex<Option<DrainPark>>,
        /// The test and `e2e` builds' pass barrier (Q105b.2): while set,
        /// the thread runs no pass, so no scan, marker read, exit or drain.
        #[cfg(any(test, feature = "e2e"))]
        passes_held: std::sync::atomic::AtomicBool,
        /// Passes the thread has finished.
        #[cfg(any(test, feature = "e2e"))]
        passes: std::sync::atomic::AtomicU64,
    }

    #[cfg(test)]
    type DrainPark = Arc<dyn Fn(RecordKey) + Send + Sync>;

    /// A composer connection: its helper, held by the pidfd its observer
    /// watches for the helper's exit (the connection's peer pidfd, or the
    /// hold the observer already kept for that helper, Q87), and its budget
    /// slot. It is open for the drain until it is dropped, which its reader
    /// does at its end, or at [`Connection::read_deadline`] ([`read_line`]),
    /// after applying the lines it read before then. Dropping it gives its
    /// slot back.
    pub struct Connection {
        observer: Arc<Observer>,
        helper: Held,
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

        /// The reader is ending this connection with bytes it didn't read
        /// (Q85), or read a line it couldn't use (Q87): a partial line,
        /// bytes pending at the read deadline, a line it refused, or one it
        /// couldn't parse. What they said is unknown, so the helper's record,
        /// one made later included, drains only uncertain. Recorded unless
        /// the record already drained (a late connection's loss changes
        /// nothing settled), as a mark holding the helper for its exit, or,
        /// when the lost pools are full, as the generation's loss overflow;
        /// logged either way.
        pub fn lose(&self) {
            let key = key_of(&self.helper);
            let mut inner = self.observer.lock();
            let settled = inner.seam.input().record(key).is_some_and(|r| r.drained);
            let mut overflow = false;
            if !settled && !inner.watch.unread.contains_key(&key) {
                match Slot::take(&self.observer.lost, &LOST) {
                    Some(slot) => {
                        inner.watch.unread.insert(
                            key,
                            Lost {
                                helper: Arc::clone(&self.helper),
                                _slot: slot,
                            },
                        );
                    }
                    None => {
                        overflow = !std::mem::replace(&mut inner.watch.lost_overflow, true);
                    }
                }
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
            if overflow {
                tracing::warn!(
                    target: "composer_probe",
                    agent_id = %self.observer.agent_id,
                    "more editor helpers lost a connection's bytes than are kept; every \
                     exit of this generation now settles only as uncertain"
                );
            }
        }
    }

    /// The next line on a composer connection (core §2.4, Q85), or `None`
    /// once it is to end. `line` is the connection's accumulator, kept by the
    /// caller across calls; `socket` the connection's own descriptor. With no
    /// connection (`conn` is `None`) it is [`CappedLine::read`]. With one it
    /// is [`read_before`] its read deadline: a partial line in `line`, or
    /// bytes still buffered in `reader` or waiting on `socket`, at the
    /// deadline make the connection [`Connection::lose`] them, and `None`
    /// tells the caller to drop it. A line returned before the deadline is
    /// the caller's to apply before it drops the connection.
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
        read_before(
            &conn.departure,
            line,
            reader,
            max_bytes,
            |reader| pending(reader, socket),
            || conn.lose(),
        )
        .await
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

    /// What the observer holds of a connection's helper, for the daemon's
    /// tests (Q87): `None` where it holds nothing, else whether it is this
    /// connection's own hold.
    #[cfg(test)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct HeldForTest {
        pub peer: Option<bool>,
        pub lost: Option<bool>,
        pub pin: Option<bool>,
    }

    #[cfg(test)]
    impl Connection {
        pub(crate) fn held_for_test(&self) -> HeldForTest {
            let key = key_of(&self.helper);
            let inner = self.observer.lock();
            let mine = |held: &Held| Arc::ptr_eq(held, &self.helper);
            HeldForTest {
                peer: inner.watch.peers.get(&key).map(mine),
                lost: inner.watch.unread.get(&key).map(|l| mine(&l.helper)),
                pin: inner.watch.pins.get(&key).map(|p| mine(&p.fd)),
            }
        }
    }

    #[cfg(test)]
    impl Observer {
        /// The lost marks kept, the generation's lost pool in use, and
        /// whether its loss overflowed (Q87).
        pub(crate) fn lost_for_test(&self) -> (usize, u32, bool) {
            let inner = self.lock();
            (
                inner.watch.unread.len(),
                self.lost.used(),
                inner.watch.lost_overflow,
            )
        }

        /// Whether `key`'s helper has a lost mark kept.
        pub(crate) fn lost_mark_for_test(&self, key: RecordKey) -> bool {
            self.lock().watch.unread.contains_key(&key)
        }

        /// Call `park` in every drain between its snapshot and its
        /// settlement (Q91), with the record's key and no lock held: a test
        /// blocks there to change what the settlement then reads.
        pub(crate) fn park_drains_for_test(
            &self,
            park: impl Fn(RecordKey) + Send + Sync + 'static,
        ) {
            *self
                .drain_park
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(park));
        }

        fn parked(&self, key: RecordKey) {
            let park = self
                .drain_park
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(park) = park {
                park(key);
            }
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
            // Q35-20: its segments, for the agent whose return path they
            // wait on. A generation whose agent isn't known gets none, and
            // no probe can open one.
            let segments_agent = match agent.as_ref() {
                Some(AgentType::ClaudeCode) => Some(crate::composer_probe::segments::Agent::Claude),
                Some(AgentType::Codex) => Some(crate::composer_probe::segments::Agent::Codex),
                _ => None,
            };
            if let Some(segments_agent) = segments_agent {
                spawned.input.attach_segments(generation, segments_agent);
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
                lost: Arc::new(Pool::new(LOST_PER_GENERATION)),
                bell: Arc::clone(&bell),
                stopped: std::sync::atomic::AtomicBool::new(false),
                keymap: std::sync::OnceLock::new(),
                #[cfg(test)]
                drain_park: std::sync::Mutex::new(None),
                #[cfg(any(test, feature = "e2e"))]
                passes_held: std::sync::atomic::AtomicBool::new(false),
                #[cfg(any(test, feature = "e2e"))]
                passes: std::sync::atomic::AtomicU64::new(0),
            });
            let weak = Arc::downgrade(&observer);
            // Q35-16: the planner's native numbers go to this generation's
            // page, through the seam lock, which takes nothing of the
            // writer's or the ingress ledger's.
            let page = Weak::clone(&weak);
            observer.input().set_native_page(Box::new(move |seq| {
                if let Some(observer) = page.upgrade() {
                    observer.lock().seam.publish_native(seq);
                }
            }));
            let thread = std::thread::Builder::new()
                .name(format!("composer-observer-{generation}"))
                .spawn(move || run(weak))
                .ok()?;
            bell.bind(thread.thread().clone());
            let _ = slot.set(bell);
            Some(observer)
        }

        /// The test and `e2e` builds' pass barrier (Q105b.2): `true` holds
        /// the thread before its next pass, `false` lets it run one now.
        #[cfg(any(test, feature = "e2e"))]
        pub fn hold_passes_for_e2e(&self, hold: bool) {
            self.passes_held
                .store(hold, std::sync::atomic::Ordering::SeqCst);
            self.bell.ring();
        }

        /// Whether passes are held, and how many the thread has finished.
        #[cfg(any(test, feature = "e2e"))]
        pub fn passes_for_e2e(&self) -> (bool, u64) {
            use std::sync::atomic::Ordering::SeqCst;
            (self.passes_held.load(SeqCst), self.passes.load(SeqCst))
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

        /// Probe admission at `now` with this generation's permission
        /// (Q35-19's predicate), writing nothing.
        pub fn admits(
            &self,
            outside: &super::Outside,
            now: Instant,
        ) -> Result<(), super::AdmissionRefusal> {
            self.lock().seam.admits(outside, now)
        }

        /// The arm increment for segment `seg_seq` (Q35-19's
        /// [`Seam::arm`]), on the writer thread in the probe's arm section.
        pub fn arm_segment(
            &self,
            seg_seq: u64,
            outside: &super::Outside,
            now: Instant,
        ) -> Result<u32, super::AdmissionRefusal> {
            self.lock().seam.arm(seg_seq, outside, now)
        }

        /// Settle the generation's permission now, if it can be (plan
        /// §1.2): a probe request asks rather than wait for the next sweep.
        /// Reads `/proc` and writes `gen-G` with no lock held.
        pub fn settle(&self) {
            self.settle_permission();
        }

        /// The take-back of an arm a withdrawal won (`probe::start`).
        pub fn disarm(&self) {
            let _ = self.lock().seam.disarm();
        }

        /// Publish a `HumanG`'s segment number with the count the page
        /// shows, before its trigger is submitted (core §4.5).
        pub fn publish_segment(&self, seg_seq: u64) {
            self.lock().seam.publish_segment(seg_seq);
        }

        /// The writer scope's state, `None` once retired.
        pub fn scope(&self) -> Option<record::ScopeState> {
            self.input().writer_scope().map(|s| s.state())
        }

        /// Whether the keymap sources still leave Ctrl+G on the external
        /// editor (O-20.7b), read again now. File I/O: call it with no lock
        /// held. No evidence (the permission never settled `Armable`) is
        /// [`crate::composer_probe::keymap::Hold::Unmeasured`].
        pub fn keymap(&self) -> Result<(), crate::composer_probe::keymap::Hold> {
            debug_assert!(crate::lock_rank::held_ranks().is_empty());
            match self.keymap.get() {
                Some(evidence) => evidence.check(),
                None => Err(crate::composer_probe::keymap::Hold::Unmeasured),
            }
        }

        /// The keymap evidence for `agent` under `probe` (O-20.7b): Claude's
        /// `keybindings.json` read now; Codex's launch sources (its config,
        /// profile and the overrides on `args`, the bound agent's own
        /// arguments) read now. Codex's trusted project files and its
        /// pre-spawn read aren't read here.
        fn keymap_evidence(
            &self,
            agent: &AgentType,
            probe: &EditorProbe<'static>,
            args: &[OsString],
        ) -> KeymapEvidence {
            use crate::composer_probe::keymap;
            let env = |name: &str| self.env_value(name);
            match agent {
                AgentType::ClaudeCode => match keymap::claude_keybindings_path(env) {
                    Ok(path) => KeymapEvidence::Claude(keymap::ClaudeKeymap::at_spawn(path, probe)),
                    Err(hold) => KeymapEvidence::Held(hold),
                },
                AgentType::Codex => {
                    let home = self
                        .env_value("CODEX_HOME")
                        .map(std::path::PathBuf::from)
                        .or_else(|| {
                            self.env_value("HOME")
                                .map(|h| std::path::PathBuf::from(h).join(".codex"))
                        });
                    let Some(home) = home else {
                        return KeymapEvidence::Held(keymap::Hold::Unmeasured);
                    };
                    match keymap::CodexSources::from_launch(&home, args, Vec::new()) {
                        Ok(sources) => {
                            let before = Box::new(sources.read());
                            KeymapEvidence::Codex {
                                sources,
                                before,
                                precedence: probe.keymap_sources,
                            }
                        }
                        Err(hold) => KeymapEvidence::Held(hold),
                    }
                }
                _ => KeymapEvidence::Held(keymap::Hold::Unmeasured),
            }
        }

        /// A helper record whose decision opened the user's editor (core
        /// §4.6's `HumanG/Unarmed` row): its editor child, seen running an
        /// executable other than the helper copy, is reported to the
        /// segments once. Run each pass, with no lock held for the reads.
        fn editors(&self) {
            let waiting: Vec<RecordKey> = {
                let inner = self.lock();
                inner
                    .watch
                    .pins
                    .keys()
                    .copied()
                    .filter(|key| {
                        inner.seam.input().record(*key).is_some_and(|r| {
                            r.decision == Some(record::Decision::Unarmed)
                                && r.exit_observed.is_none()
                        })
                    })
                    .filter(|key| !inner.watch.editors.contains(key))
                    .collect()
            };
            for key in waiting {
                let running = children(key.pid)
                    .into_iter()
                    .any(|pid| exe_id(pid).is_some_and(|id| id != self.helper_id));
                if !running {
                    continue;
                }
                let mut inner = self.lock();
                inner.watch.editors.push(key);
                if inner.watch.editors.len() > record::RECORD_CAP {
                    inner.watch.editors.remove(0);
                }
                let input = Arc::clone(inner.seam.input());
                drop(inner);
                input.segment_editor_running(key, Instant::now());
            }
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
        /// capability. A registered connection is always watched (Q87): it
        /// and the observer's `peers` share one hold on the helper, the one
        /// the observer already kept for that helper if there is one,
        /// otherwise this pin; nothing is duplicated, so nothing can fail
        /// between taking the slot and watching. A helper whose exit was
        /// already observed is departed from the start.
        pub fn connect(
            self: &Arc<Self>,
            peer: std::io::Result<Option<OwnedFd>>,
        ) -> Option<Connection> {
            let mut kernel = LinuxKernel::default();
            let pinned = attest::pin(&mut kernel, Sighting::Peer(peer)).ok()?;
            if exe_id(pinned.pid) != Some(self.helper_id) {
                return None;
            }
            let slot = Slot::take(&self.connections, &CONNECTIONS)?;
            let key = key_of(&pinned);
            let fresh: Held = Arc::new(pinned);
            let departure = Arc::new(Departure::default());
            let helper = {
                let mut inner = self.lock();
                if inner
                    .seam
                    .input()
                    .record(key)
                    .is_some_and(|r| r.exit_observed.is_some())
                {
                    departure.depart();
                }
                let helper = inner.watch.held(key).unwrap_or_else(|| Arc::clone(&fresh));
                inner
                    .watch
                    .open
                    .entry(key)
                    .or_default()
                    .push(Arc::clone(&departure));
                inner
                    .watch
                    .peers
                    .entry(key)
                    .or_insert_with(|| Arc::clone(&helper));
                helper
            };
            // An unused pin is closed with no lock held.
            drop(fresh);
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
                if evidence.is_some() && unexited(&inner, key) {
                    hold(&mut inner.watch, key, Arc::clone(&conn.helper), true);
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
                // Q89: the `e2e` build's assumption may bind a direct native
                // Codex stand-in as `A`; production binds Codex below the
                // deck's wrapper or a launcher only at Q35-22.
                (PaneClass::Direct(ProbeAgent::Codex), Some(child))
                    if cfg!(feature = "e2e") && !self.assumed.is_empty() =>
                {
                    child
                }
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
            // Q89 (`e2e` only): a direct Codex runs below the deck's own
            // `wrap --agent codex -- <program> <args>`; its `A` is the
            // wrapper's one child running that program with those args.
            let (pid, start, facts) = if matches!(self.class, PaneClass::Direct(ProbeAgent::Codex))
                && cfg!(feature = "e2e")
            {
                self.below_deck_wrapper(&mut kernel, pid, start, facts)?
            } else {
                (pid, start, facts)
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

        /// Q89 (`e2e` only): when `pid` runs this daemon's own executable as
        /// `wrap --agent codex -- <program> <args>`, the wrapper's one child
        /// running `<program>`'s executable with exactly `<program> <args>`;
        /// otherwise `pid` itself. Anything else below a wrapper binds
        /// nothing (`Err(false)`): launchers are Q35-22's.
        fn below_deck_wrapper(
            &self,
            kernel: &mut LinuxKernel,
            pid: u32,
            start: u64,
            facts: ProcFacts,
        ) -> Result<(u32, u64, ProcFacts), bool> {
            let own = std::env::current_exe()
                .ok()
                .and_then(|exe| std::fs::metadata(exe).ok())
                .map(|m| FileId {
                    dev: m.dev(),
                    ino: m.ino(),
                });
            let argv: Vec<&OsStr> = facts.argv.iter().map(OsString::as_os_str).collect();
            let wrapped = match argv.as_slice() {
                [_, wrap, flag, agent, dashes, rest @ ..]
                    if *wrap == "wrap"
                        && *flag == "--agent"
                        && *agent == "codex"
                        && *dashes == "--"
                        && !rest.is_empty()
                        && own == Some(facts.exe) =>
                {
                    rest.iter().map(|a| a.to_os_string()).collect::<Vec<_>>()
                }
                _ => return Ok((pid, start, facts)),
            };
            let path = self.env_value("PATH");
            let program = resolve_program(&wrapped[0], path.as_deref()).ok_or(false)?;
            let below: Vec<(u32, ProcFacts)> = children(pid)
                .into_iter()
                .filter(|c| kernel.stat(*c).is_ok_and(|stat| stat.ppid == pid))
                .filter_map(|c| Some((c, proc_facts(c)?)))
                .collect();
            let [(child, child_facts)] = below.as_slice() else {
                return Err(false);
            };
            if child_facts.exe != program || child_facts.argv != wrapped {
                return Err(false);
            }
            let child_start = kernel.stat(*child).map_err(|_| false)?.start;
            Ok((*child, child_start, child_facts.clone()))
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
            // Q35-20 (O-20.7b): the generation's keymap sources, read now,
            // with no lock held, for its release's record: what each
            // admission compares a fresh read with.
            if permission == Permission::Armable
                && let Some(LatchedRelease::Known(release)) = header.as_ref()
                && let Some(probe) = records
                    .iter()
                    .find(|r| r.agent == *agent && r.release == *release)
                    .and_then(|r| match r.editor_probe {
                        crate::doorbell_activation::Measurement::Verified(probe) => Some(probe),
                        _ => None,
                    })
            {
                // The bound agent's own arguments: a direct Codex's `-c`
                // overrides and profile are on its argv, not on the deck's
                // launch line.
                let args = identity
                    .as_ref()
                    .and_then(|a| proc_facts(a.pid))
                    .map(|f| f.argv.get(1..).unwrap_or_default().to_vec())
                    .unwrap_or_default();
                let _ = self.keymap.set(self.keymap_evidence(agent, &probe, &args));
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
            self.editors();
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
                            hold(&mut inner.watch, key, Arc::new(helper), true);
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
                            hold(&mut inner.watch, key, Arc::new(helper), true);
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
                            hold(&mut inner.watch, marker.key, Arc::new(helper), false);
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
                .filter(|(_, pin)| exited(pin.fd.pidfd()))
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
                if !waiting.is_empty() && exited(peer.pidfd()) {
                    for departure in waiting {
                        departure.depart();
                    }
                }
            }
            // A lost connection's mark goes only once nothing can still
            // need it (Q85, Q87): its record drained, or, with no record,
            // its helper exited with no connection open, so no channel can
            // make the record that would inherit it. Never while the helper
            // lives.
            watch
                .unread
                .retain(|key, lost| match seam.input().record(*key) {
                    Some(record) => !record.drained,
                    None => watch.open.contains_key(key) || !exited(lost.helper.pidfd()),
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
        /// (Q85), or any of a generation whose loss overflowed (Q87), gives
        /// up to the uncertain outcome and keeps its markers and its record.
        /// The step is taken from the record's open connections, its loss
        /// and the generation's overflow as they are under the lock the
        /// settlement is committed under (Q91), not as the snapshot before
        /// the marker reads found them: another helper's loss can latch the
        /// overflow while those reads run with no lock held. A settled
        /// one's markers are deleted with no lock held, and it is pruned,
        /// its facts kept, once it may go.
        fn drains(&self, budget: &mut u32) {
            let name = self.name();
            let waiting: Vec<(RecordKey, u32, u32)> = {
                let inner = self.lock();
                inner
                    .watch
                    .exited
                    .iter()
                    .map(|(key, retries)| (*key, open_of(&inner.watch, *key), *retries))
                    .collect()
            };
            if waiting.is_empty() {
                return;
            }
            // The markers again, before any settles by its exit.
            let listing = waiting
                .iter()
                .any(|(_, open, _)| *open == 0)
                .then(|| self.reader.markers(&name, MARKER_NAMES_PER_PASS));
            for (key, open, retries) in waiting {
                let read = match &listing {
                    Some(listing) if open == 0 => self.drain_markers(listing, key, budget),
                    _ => MarkerRead {
                        mine: Vec::new(),
                        complete: false,
                    },
                };
                #[cfg(test)]
                self.parked(key);
                let now = Instant::now();
                let mut inner = self.lock();
                let open = open_of(&inner.watch, key);
                let unread = inner.watch.lost_overflow || inner.watch.unread.contains_key(&key);
                match drain_step(open, read.complete, unread, retries) {
                    DrainStep::Wait => {
                        if let Some(retries) = inner.watch.exited.get_mut(&key) {
                            *retries += 1;
                        }
                    }
                    DrainStep::GiveUp => {
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
                        inner.watch.exited.remove(&key);
                        inner.watch.unread.remove(&key);
                        let drained = inner.seam.drained(key, now);
                        drop(inner);
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
            let due: Vec<(RecordKey, Held)> = {
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
                    .map(|(key, pin)| (*key, Arc::clone(&pin.fd)))
                    .collect()
            };
            for (key, fd) in due {
                if exe_id(key.pid) != Some(self.helper_id) {
                    if exited(fd.pidfd()) {
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
                if exited(fd.pidfd()) {
                    continue;
                }
                let killed = kill(fd.pidfd());
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

    /// `key`'s helper's composer connections not finished yet.
    fn open_of(watch: &Watch, key: RecordKey) -> u32 {
        u32::try_from(watch.open.get(&key).map_or(0, Vec::len)).unwrap_or(u32::MAX)
    }

    /// Pin `key`'s helper: a pin E2 may signal replaces one it may not, and
    /// nothing else replaces a pin. The pin is the hold already kept for the
    /// helper (Q87, Q91), its connections', its lost mark's or the pin it
    /// replaces, so a helper is held once however its pin is made; only a
    /// helper held by nothing yet is held by `fresh`.
    fn hold(watch: &mut Watch, key: RecordKey, fresh: Held, killable: bool) {
        match watch.pins.get(&key) {
            Some(pin) if pin.killable || !killable => {}
            _ => {
                let fd = watch.held(key).unwrap_or(fresh);
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
            #[cfg(any(test, feature = "e2e"))]
            if observer
                .passes_held
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                drop(observer);
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            }
            observer.pass();
            #[cfg(any(test, feature = "e2e"))]
            observer
                .passes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
