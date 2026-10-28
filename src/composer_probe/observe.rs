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
//! **The drain** (core §2.4, AU7 §2). A record whose exit is observed is
//! settled by its exit only once its decision evidence is drained: no
//! composer connection of its helper is still open, or [`READ_DEADLINE`]
//! has passed since its exit, and its markers have been read again. Until
//! then the drain is rescheduled, never settled. A connection is registered
//! when its capability question is answered, which is before a helper can
//! send a decision on it, so a decision on IPC is always on a registered
//! connection. Work is bounded per generation and in total ([`Pool`]):
//! composer connections open at once, the bytes of the one report read after
//! an invocation, attestations at once and per pass, and marker names read
//! per pass. Past a bound a scan or a marker waits for the next pass; a
//! connection gets no capability; an attestation not run is an identity
//! refusal (`record_refused`, with the request's claim on IPC and none for a
//! scan, an unread connection or a marker).
//!
//! **The kill** (core §5.4, P-E2). A record whose helper declared `Armed`
//! and is still alive [`E2_AFTER`] after the first `Armed` observation of it
//! is killed through its pidfd. The clock starts at the first observation of
//! `Armed` and a duplicate doesn't move it ([`e2_due`]). A helper stopped
//! before its arm load, or on an editor path, is never killed.
//!
//! **Permission** (plan §1.2). A generation is armable once its header
//! latched an exact release with a verified `editor_probe` record and `A`
//! is bound; then `gen-G` is written. As of this commit `A` is bound only
//! for a direct Claude, as the PTY child (Q35-22 binds it below a launcher;
//! a direct Codex runs below the deck's wrapper, so its `A` isn't the PTY
//! child either). Nothing in production publishes a liability: installation
//! rolls out to no class until activation, and the only publisher is the
//! `e2e` build's control verb ([`Seam::control`]).
//!
//! **Lock order.** No lock is held while a process or a file is read:
//! ```text
//! registry (R4) ─ clone Arc<Observer>, released ─┐
//! hook task │ observer thread │ attach handler ──┴─> attest (no lock)
//!                                                     └─> seam (Mutex) ─> input (R6)
//!                                                                  └─> arm page stores
//! output lock (bus) ─> Bell (atomics, unpark)
//! retirement: R4 ─> R6 (retire_taking_first_stop); Observer::stop (atomics)
//! ```
//! The seam lock isn't ranked; nothing takes it under R4, R6 or the output
//! lock, and nothing under it but the input lock's leaf steps and memory
//! stores to the arm page. Retirement: the pane's successor retires the
//! input (records and scope dropped, M-FIRST taken in the same section) and
//! stops the observer; the observer's thread holds it weakly and ends at its
//! next wake, and the arm page is unmapped when the record goes.

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
/// (core §2.4).
pub const READ_DEADLINE: Duration = Duration::from_millis(100);

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

/// Whether an exited helper's drain may settle at `now` (core §2.4): none of
/// its composer connections is open (`open`, how many are), or
/// [`READ_DEADLINE`] has passed since its exit was observed at `exited`, after
/// which a connection still open is taken to have said nothing more.
/// Otherwise the drain is rescheduled, and nothing settles.
pub fn drain_ready(open: u32, exited: Instant, now: Instant) -> bool {
    open == 0 || now.saturating_duration_since(exited) >= READ_DEADLINE
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

    /// Settle the permission. Once out of `Pending` it doesn't move.
    pub fn set_permission(&mut self, permission: Permission) {
        if self.permission == Permission::Pending {
            self.permission = permission;
        }
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
pub use linux::{Connection, Observer, Spawned};

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Mutex, Weak};
    use std::time::{Duration, Instant};

    use super::{
        ATTESTATIONS, ATTESTATIONS_PER_PASS, Bell, CONNECTIONS, CONNECTIONS_PER_GENERATION,
        CONTROL_PERMISSION_WAIT, ControlOp, ControlRefusal, Evidence, IDLE_SWEEP,
        MARKER_NAMES_PER_PASS, Page, Pool, SCAN_PERIOD, Seam, Slot, drain_ready, e2_due,
        marker_observation, scan_observation,
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
    use crate::doorbell_activation::LatchedRelease;
    use crate::event::{AgentType, ComposerInvocationRequest, ComposerOutcome, EditorReport};

    impl Page for GenerationFiles {
        fn publish(&mut self, count: u32) {
            // No segment numbers until probes exist (Q35-19/20).
            self.arm().publisher().publish(count, 0);
        }
    }

    /// The most exit facts an observer keeps after their records go.
    const FACTS_KEPT: usize = record::RECORD_CAP;

    /// What a pass watches, under the seam lock with the seam.
    #[derive(Default)]
    struct Watch {
        /// Helpers with a record whose exit isn't observed yet.
        pins: HashMap<RecordKey, OwnedFd>,
        /// Exits observed, drain pending, with when.
        exited: HashMap<RecordKey, Instant>,
        /// Composer connections open, by the helper on the other end.
        open: HashMap<RecordKey, u32>,
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
        reader: InstanceReader,
        helper_id: FileId,
        latch: Box<dyn Fn(&AgentType) -> LatchedRelease + Send + Sync>,
        inner: Mutex<Inner>,
        connections: Arc<Pool>,
        bell: Arc<Bell>,
        stopped: std::sync::atomic::AtomicBool,
    }

    /// A composer connection: its helper, pinned by the connection's peer
    /// pidfd, and its budget slot. Dropping it closes the registration.
    pub struct Connection {
        observer: Arc<Observer>,
        helper: Pinned<OwnedFd>,
        _slot: Slot,
    }

    impl Connection {
        pub fn observer(&self) -> &Arc<Observer> {
            &self.observer
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            let key = key_of(&self.helper);
            let mut inner = self.observer.lock();
            if let Some(n) = inner.watch.open.get_mut(&key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    inner.watch.open.remove(&key);
                }
            }
            drop(inner);
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

    /// `A`'s children, from every thread's `children` file.
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
                reader,
                helper_id,
                latch: spawned.latch,
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
        /// capability.
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
            *self.lock().watch.open.entry(key_of(&helper)).or_default() += 1;
            Some(Connection {
                observer: Arc::clone(self),
                helper,
                _slot: slot,
            })
        }

        /// An invocation on `conn` (core §2.3–§2.4): attested off the lock,
        /// then recorded and settled under it, the epoch moved for a
        /// non-`Armed` decision. The helper's pidfd is kept for its exit.
        pub fn invocation(
            &self,
            conn: &Connection,
            request: &ComposerInvocationRequest,
        ) -> ComposerOutcome {
            if request.generation != self.generation {
                return ComposerOutcome::Uncertain;
            }
            let evidence = self.attest(&conn.helper, Some(&request.argv));
            let outcome = {
                let mut inner = self.lock();
                let outcome = inner.seam.invocation(request, evidence, Instant::now());
                let key = key_of(&conn.helper);
                if evidence.is_some()
                    && inner.seam.input().record(key).is_some()
                    && !inner.watch.pins.contains_key(&key)
                    && let Ok(pidfd) = conn.helper.pidfd().try_clone()
                {
                    inner.watch.pins.insert(key, pidfd);
                }
                outcome
            };
            self.bell.ring();
            outcome
        }

        /// The report a helper sent after its editor, on `conn`.
        pub fn report(&self, conn: &Connection, report: EditorReport) {
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

        /// `A` for a direct Claude: the PTY child, or, when the deck ran the
        /// command under its shell's `-c` and that shell didn't `exec` it,
        /// the shell's one child; pinned, with its family read from its
        /// executable. `Err(true)` when it exited.
        fn bind(&self) -> Result<Identity, bool> {
            let (pid, start) = match (self.class, self.child) {
                (PaneClass::Direct(ProbeAgent::Claude), Some(child)) => child,
                _ => return Err(false),
            };
            let shell = std::fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .and_then(|exe| exe.file_name().map(|n| n.to_os_string()))
                .is_some_and(|name| {
                    crate::composer_probe::install::SHELLS
                        .iter()
                        .any(|shell| name == *shell)
                });
            let (pid, start) = if shell {
                let below = children(pid);
                let [only] = below.as_slice() else {
                    return Err(false);
                };
                let mut kernel = LinuxKernel::default();
                let stat = kernel.stat(*only).map_err(|_| false)?;
                if stat.ppid != pid {
                    return Err(false);
                }
                (*only, stat.start)
            } else {
                (pid, start)
            };
            let mut kernel = LinuxKernel::default();
            let pinned = attest::pin(
                &mut kernel,
                Sighting::Pid {
                    pid,
                    start: Some(start),
                },
            )
            .map_err(|why| why == attest::Unpinned::Exited)?;
            let exe = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|_| false)?;
            let exe_file = exe_id(pid).ok_or(false)?;
            let root = exe.parent().ok_or(false)?.to_path_buf();
            let root_meta = std::fs::metadata(&root).map_err(|_| false)?;
            let identity = Identity {
                pid,
                start,
                exe: exe_file,
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
        /// gets `gen-G`, and one whose file can't be written is inert.
        fn settle_permission(&self) {
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
                crate::composer_probe::install::captured_assumption(),
            );
            let mut permission =
                crate::composer_probe::permission(&records, agent, header.as_ref(), binding);
            if permission == Permission::Armable
                && let Some(identity) = &identity
                && let Err(cause) =
                    crate::composer_probe::install::write_identity(&self.name(), identity)
            {
                tracing::warn!(
                    target: "composer_probe",
                    agent_id = %self.agent_id,
                    ?cause,
                    "editor probe identity not written; the generation stays inert"
                );
                permission = Permission::Inert(InertCause::Unbound);
            }
            if permission != Permission::Pending {
                tracing::info!(
                    target: "composer_probe",
                    agent_id = %self.agent_id,
                    ?permission,
                    "editor probe permission settled"
                );
            }
            let mut inner = self.lock();
            inner.seam.set_permission(permission);
            if inner.seam.permission() == Permission::Armable {
                inner.watch.agent = identity;
            }
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
            self.drains();
            self.kills();
        }

        /// The child scan: this instance's helper copies among `A`'s
        /// children, each attested and observed once.
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
                if self.lock().seam.input().record(key).is_some() {
                    continue;
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
                        if inner.seam.observe(&obs, now).is_ok()
                            && inner.seam.input().record(key).is_some()
                        {
                            inner.watch.pins.insert(key, helper.into_pidfd());
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
                let mut inner = self.lock();
                if !inner.watch.marker_overflow {
                    tracing::warn!(
                        target: "composer_probe",
                        agent_id = %self.agent_id,
                        "more editor helper markers than a pass reads"
                    );
                }
                inner.watch.marker_overflow = true;
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
            {
                let mut inner = self.lock();
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
                            && inner.seam.input().record(marker.key).is_some()
                            && !inner.watch.pins.contains_key(&marker.key)
                            && !inner.watch.exited.contains_key(&marker.key)
                        {
                            inner.watch.pins.insert(marker.key, helper.into_pidfd());
                        }
                    }
                    None => {
                        let _ =
                            inner
                                .seam
                                .refused(Channel::Marker, Some(marker.decision), None, now);
                    }
                }
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

        /// Exits: a pinned helper whose pidfd reads ready.
        fn exits(&self) {
            let mut inner = self.lock();
            let gone: Vec<RecordKey> = inner
                .watch
                .pins
                .iter()
                .filter(|(_, fd)| exited(fd))
                .map(|(k, _)| *k)
                .collect();
            for key in gone {
                inner.watch.pins.remove(&key);
                let at = Instant::now();
                if inner.seam.exit(key, at, at).is_ok() {
                    inner.watch.exited.insert(key, at);
                }
            }
        }

        /// Drains: an exited record with its evidence drained is settled by
        /// its exit, its facts kept, and pruned once it may go. One whose
        /// connection is still open within the read deadline is
        /// rescheduled.
        fn drains(&self) {
            let name = self.name();
            let ready: Vec<RecordKey> = {
                let inner = self.lock();
                let now = Instant::now();
                inner
                    .watch
                    .exited
                    .iter()
                    .filter(|(key, at)| {
                        drain_ready(inner.watch.open.get(key).copied().unwrap_or(0), **at, now)
                    })
                    .map(|(k, _)| *k)
                    .collect()
            };
            if ready.is_empty() {
                return;
            }
            // The markers again, before any settles by its exit.
            let names = self
                .reader
                .markers(&name, MARKER_NAMES_PER_PASS)
                .map(|(names, _)| names)
                .unwrap_or_default();
            for key in ready {
                let mine: Vec<(String, record::Marker)> = names
                    .iter()
                    .filter_map(|n| record::parse_marker(n).map(|m| (n.clone(), m)))
                    .filter(|(_, m)| m.key == key)
                    .collect();
                let mut budget = u32::MAX;
                for (file, marker) in mine.iter().filter(|(_, m)| !m.renamed) {
                    self.apply_marker(&name, file, marker, &mut budget);
                }
                let mut inner = self.lock();
                let now = Instant::now();
                let Ok(drained) = inner.seam.drained(key, now) else {
                    inner.watch.exited.remove(&key);
                    continue;
                };
                inner.watch.exited.remove(&key);
                let deleted = mine.iter().all(|(file, m)| {
                    let seen = if m.renamed {
                        file.clone()
                    } else {
                        format!("{file}.seen")
                    };
                    self.reader.remove_marker(&name, file).is_ok()
                        && self.reader.remove_marker(&name, &seen).is_ok()
                });
                let fact = match inner.seam.prune(key, deleted, now) {
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

        /// E2 (core §5.4): an `Armed` helper alive past its bound.
        fn kills(&self) {
            let inner = self.lock();
            let now = Instant::now();
            for (key, pidfd) in &inner.watch.pins {
                let Some(r) = inner.seam.input().record(*key) else {
                    continue;
                };
                if e2_due(&r, now) && !exited(pidfd) {
                    let killed = kill(pidfd);
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
