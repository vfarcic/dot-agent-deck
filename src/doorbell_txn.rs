//! PRD #1666 part 3, round 5-wake (plan §1, §2.1′, §2.4, §3): the doorbell's
//! transaction as a pure model, with no PTY, lock, task or clock of its own.
//!
//! One transaction types the deck's token-led line (`L_t`, plan §1.2) into an
//! agent's empty composer, proves the agent echoed exactly that line, presses
//! Enter only after a synchronous re-check, and ends in exactly one of three
//! states: `Submitted` (the agent's prompt hook reported the line's token, or
//! the transcript shows it as a user message), `Withdrawn` (the deck deleted
//! its own line under the strict recogniser and saw the composer's
//! placeholder again), or `Unresolved` with one cause. Before its first
//! possible byte it can instead end with nothing written.
//!
//! The pieces it needs from the daemon come in as values: the frames the
//! evidence state recognised ([`Frame`]), the ledger's marks and the mode
//! evidence read at the final seam ([`Seam`]), job outcomes, hook evidence and
//! the time ([`Event`]). What it asks of the daemon goes out as [`Action`]s.
//! The latch is a [`LatchCell`] word moved forward by compare-and-swap, so
//! the hook arm can resolve an attempt on receipt while the transaction is
//! between events, and the re-projection model ([`Publisher`]) only ever
//! reads the cell, never installs a result.
//!
//! Inert: nothing in the daemon drives this module until activation (plan
//! §10, commit 6). The doorbell's live admission still reads the
//! caller-lifetime counter (decision Q19).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::inbox::MessageId;

/// The writer is held for at most this long per ring (plan §2.3, Q3′).
pub const TXN_MAX: Duration = Duration::from_secs(3);
/// The echo proof's own cap, inside [`TXN_MAX`] (Q3).
pub const ECHO_WAIT: Duration = Duration::from_millis(1500);
/// How long the deck waits for evidence that its Enter submitted the line.
pub const SUBMIT_CONFIRM_WAIT: Duration = Duration::from_secs(1);
/// The floor between the line's write completing and the Enter, the one the
/// pane input's payload writes use. It overlaps the echo wait.
pub const SUBMIT_DELAY: Duration = crate::pane_input::SUBMIT_DELAY;
/// A contended lock at the seam is tried again after this long, or at the
/// deadline, whichever is first (plan §2.4).
pub const SEAM_RETRY: Duration = Duration::from_millis(1);
/// The number of recent attempts the ring correlates (plan §3.4′).
pub const RING_SLOTS: usize = 64;

/// The six token letters' alphabet (plan §1.2): letters classed inert in every
/// measured dialog, no digits and none of `y n q j k h l e s u`.
pub const TOKEN_ALPHABET: &[u8; 16] = b"abcdfgimoprtvwxz";

// ------------------------------------------------------------------------
// The ring token

/// A six-letter ring token, packed five bits per letter (`a` is 0) into the
/// low 30 bits, so one attempt-ring entry holds it beside its attempt number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RingToken(u32);

impl RingToken {
    /// The token spelled by six lowercase letters; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 6 || !bytes.iter().all(u8::is_ascii_lowercase) {
            return None;
        }
        Some(Self(
            bytes
                .iter()
                .fold(0u32, |packed, b| (packed << 5) | u32::from(b - b'a')),
        ))
    }

    /// Six letters from [`TOKEN_ALPHABET`], each picked by `draw` (`0..16`).
    pub fn from_draws(mut draw: impl FnMut() -> u8) -> Self {
        let letters: String = (0..6)
            .map(|_| char::from(TOKEN_ALPHABET[usize::from(draw() & 0x0f)]))
            .collect();
        Self::parse(&letters).expect("alphabet letters are lowercase")
    }

    /// The packed value, below 2³⁰.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// The six letters.
    pub fn letters(self) -> String {
        (0..6)
            .rev()
            .map(|i| char::from(b'a' + ((self.0 >> (5 * i)) & 0x1f) as u8))
            .collect()
    }

    /// The line the deck types for message `id`: `(token)` and the doorbell
    /// line, `L_t` (plan §1.2). A message id is ASCII with no control
    /// character, so the line is too.
    pub fn line(self, id: &MessageId) -> String {
        format!(
            "({}){}",
            self.letters(),
            crate::doorbell::doorbell_line(id.as_str())
        )
    }
}

/// How many candidates [`pick_token`] draws before it gives up. Admission
/// runs inside the ring's one deadline, so the redraw is bounded work. The
/// bound counts collisions, whatever makes them: one candidate drawn again
/// and again, distinct candidates each on the screen or recent, or a fair
/// draw that collides that many times in a row.
pub const TOKEN_DRAWS: usize = 32;

/// Pick the attempt's token (plan §1.2): draws from `draw` until a candidate
/// appears neither in `screen` (the admission frame's text) nor among the
/// ring's recent tokens, at most [`TOKEN_DRAWS`] times. Two checked facts, not
/// probabilities: the token was not on the pane's screen when the attempt
/// began, and is no recent attempt's. `None` when every candidate collided.
pub fn pick_token(
    mut draw: impl FnMut() -> u8,
    screen: &str,
    ring: &AttemptRing,
) -> Option<RingToken> {
    (0..TOKEN_DRAWS)
        .map(|_| RingToken::from_draws(&mut draw))
        .find(|candidate| {
            !screen.contains(&candidate.letters()) && ring.lookup(*candidate).is_none()
        })
}

// ------------------------------------------------------------------------
// The attempt counter and the attempt ring

/// The reserved attempt number: a generation whose next number would be this
/// one admits no more attempts (`attempt_exhausted`, plan §3.4′).
pub const ATTEMPT_RESERVED: u32 = u32::MAX;

/// A generation's attempt numbers: a checked increment from 1, never reused,
/// never wrapping or saturating.
#[derive(Debug)]
pub struct AttemptCounter {
    next: AtomicU32,
}

impl Default for AttemptCounter {
    fn default() -> Self {
        Self::starting_at(1)
    }
}

impl AttemptCounter {
    /// A counter whose next number is `next` (tests start near the reserved
    /// number).
    pub fn starting_at(next: u32) -> Self {
        Self {
            next: AtomicU32::new(next.max(1)),
        }
    }

    /// Test seam: make `next` the next number this counter allocates.
    #[cfg(test)]
    pub fn set_next_for_test(&self, next: u32) {
        self.next.store(next.max(1), Ordering::Release);
    }

    /// Whether the next number would be [`ATTEMPT_RESERVED`]: the generation
    /// admits no more attempts (`attempt_exhausted`).
    pub fn exhausted(&self) -> bool {
        self.next.load(Ordering::Acquire) == ATTEMPT_RESERVED
    }

    /// The next attempt number, or `None` once it would be
    /// [`ATTEMPT_RESERVED`].
    pub fn allocate(&self) -> Option<u32> {
        let mut n = self.next.load(Ordering::Acquire);
        loop {
            if n == ATTEMPT_RESERVED {
                return None;
            }
            match self
                .next
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(n),
                Err(seen) => n = seen,
            }
        }
    }
}

/// The last [`RING_SLOTS`] attempts' tokens, each entry `(attempt, token)`
/// written with one store so no reader sees one write's attempt beside
/// another's token. Zero is empty. It correlates a token with its attempt; a
/// token evicted from it is a miss.
#[derive(Debug)]
pub struct AttemptRing {
    slots: [AtomicU64; RING_SLOTS],
}

impl Default for AttemptRing {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// One ring entry, packed: the attempt in the high 32 bits, the token below.
pub fn ring_entry(attempt: u32, token: RingToken) -> u64 {
    (u64::from(attempt) << 32) | u64::from(token.bits())
}

impl AttemptRing {
    /// Write attempt `attempt`'s entry into slot `attempt mod 64`, with one
    /// store. Called before the attempt's cell opens and before any byte.
    pub fn record(&self, attempt: u32, token: RingToken) {
        self.slots[attempt as usize % RING_SLOTS]
            .store(ring_entry(attempt, token), Ordering::Release);
    }

    /// The attempt a token belongs to, while its entry is still in the ring.
    pub fn lookup(&self, token: RingToken) -> Option<u32> {
        self.slots
            .iter()
            .map(|slot| slot.load(Ordering::Acquire))
            .filter(|&entry| entry != 0 && entry as u32 == token.bits())
            .map(|entry| (entry >> 32) as u32)
            .max()
    }
}

// ------------------------------------------------------------------------
// The latch cell

/// Why an attempt ended `Unresolved` (plan §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cause {
    Deadline = 1,
    EchoMismatch,
    PartialWrite,
    WriteStalled,
    ModeChanged,
    Layout,
    NoSubmitEvidence,
    WithdrawalRefused,
    WithdrawalStalled,
    DialogAfterTyping,
    DialogBeforeSubmit,
    DialogAfterSubmit,
}

impl Cause {
    const ALL: [Cause; 12] = [
        Cause::Deadline,
        Cause::EchoMismatch,
        Cause::PartialWrite,
        Cause::WriteStalled,
        Cause::ModeChanged,
        Cause::Layout,
        Cause::NoSubmitEvidence,
        Cause::WithdrawalRefused,
        Cause::WithdrawalStalled,
        Cause::DialogAfterTyping,
        Cause::DialogBeforeSubmit,
        Cause::DialogAfterSubmit,
    ];

    /// A dialog took the deck's keys, or may have (`dialog_during_ring` on
    /// the card, plan §5.1).
    pub fn is_dialog(self) -> bool {
        matches!(
            self,
            Cause::DialogAfterTyping | Cause::DialogBeforeSubmit | Cause::DialogAfterSubmit
        )
    }

    fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|cause| *cause as u8 == code)
    }
}

/// How a resolved attempt ended (plan §3.4′).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resolution {
    NotTyped = 1,
    Submitted,
    Withdrawn,
    LateExact,
    User,
}

impl Resolution {
    fn from_code(code: u8) -> Option<Self> {
        [
            Resolution::NotTyped,
            Resolution::Submitted,
            Resolution::Withdrawn,
            Resolution::LateExact,
            Resolution::User,
        ]
        .into_iter()
        .find(|r| *r as u8 == code)
    }
}

/// A latch cell's state, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latch {
    Open(u32),
    Suspended(u32, Cause),
    Resolved(u32, Resolution),
    /// Absorbing: the generation retired.
    Retired,
}

const PHASE_OPEN: u64 = 0;
const PHASE_SUSPENDED: u64 = 1;
const PHASE_RESOLVED: u64 = 2;
/// The reserved all-ones word.
const RETIRED: u64 = u64::MAX;

fn word(attempt: u32, phase: u64, detail: u8) -> u64 {
    (u64::from(attempt) << 32) | (phase << 8) | u64::from(detail)
}

/// The `(attempt, phase)` key writes compare lexicographically.
fn key(word: u64) -> u64 {
    word >> 8
}

impl Latch {
    fn decode(word: u64) -> Self {
        if word == RETIRED {
            return Latch::Retired;
        }
        let attempt = (word >> 32) as u32;
        let detail = word as u8;
        match (word >> 8) & 0xff_ffff {
            PHASE_OPEN => Latch::Open(attempt),
            PHASE_SUSPENDED => Latch::Suspended(
                attempt,
                Cause::from_code(detail).expect("a suspended word carries a cause"),
            ),
            _ => Latch::Resolved(
                attempt,
                Resolution::from_code(detail).expect("a resolved word carries how it ended"),
            ),
        }
    }

    /// Whether the doorbell is suspended for the generation.
    pub fn suspended(self) -> bool {
        matches!(self, Latch::Suspended(..))
    }
}

/// One generation's latch (plan §3.4′): an `AtomicU64` holding
/// `(attempt, phase, detail)`, phases `Open < Suspended < Resolved`, plus the
/// absorbing `Retired`. Every write is a compare-and-swap that needs both a
/// greater `(attempt, phase)` key than the current one and the exact
/// from-state its transition names, attempt number included; a write that
/// meets interference retries and gives up once either condition fails, and
/// none writes over `Retired`. "Clear" is how a resolved cell projects, never
/// a write of a lower phase.
pub struct LatchCell {
    word: AtomicU64,
    /// Test seam: run once, just before the next suspension's
    /// compare-and-swap (`inbox/doorbell/050` (n)).
    #[cfg(test)]
    before_suspend: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for LatchCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LatchCell")
            .field("word", &self.load())
            .finish()
    }
}

impl Default for LatchCell {
    fn default() -> Self {
        Self {
            word: AtomicU64::new(word(0, PHASE_RESOLVED, Resolution::NotTyped as u8)),
            #[cfg(test)]
            before_suspend: std::sync::Mutex::new(None),
        }
    }
}

impl LatchCell {
    /// The cell's state now.
    pub fn load(&self) -> Latch {
        Latch::decode(self.word.load(Ordering::Acquire))
    }

    /// The raw word, for the `doorbell latch` log line.
    pub fn raw(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    /// Move to `to` if the current state satisfies `from` and `to`'s key is
    /// greater. `Ok` with the state moved from, `Err` with the state that
    /// refused the move.
    fn advance(&self, from: impl Fn(Latch) -> bool, to: u64) -> Result<Latch, Latch> {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let state = Latch::decode(current);
            if current == RETIRED || !from(state) || key(to) <= key(current) {
                return Err(state);
            }
            match self
                .word
                .compare_exchange_weak(current, to, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(state),
                Err(seen) => current = seen,
            }
        }
    }

    /// Step 1 admits attempt `attempt`: any earlier attempt's state, to
    /// `Open(attempt)`. Refused once the cell is `Retired`.
    pub fn open(&self, attempt: u32) -> Result<Latch, Latch> {
        self.advance(
            |state| match state {
                Latch::Open(a) | Latch::Suspended(a, _) | Latch::Resolved(a, _) => a < attempt,
                Latch::Retired => false,
            },
            word(attempt, PHASE_OPEN, 0),
        )
    }

    /// The attempt ended with nothing written: `Open(a)` to
    /// `Resolved(a, not_typed)`.
    pub fn not_typed(&self, attempt: u32) -> Result<Latch, Latch> {
        self.advance(
            |state| state == Latch::Open(attempt),
            word(attempt, PHASE_RESOLVED, Resolution::NotTyped as u8),
        )
    }

    /// The attempt ended `Unresolved(cause)`, expiry included: `Open(a)` to
    /// `Suspended(a, cause)`. Refused when exact evidence resolved it first.
    pub fn suspend(&self, attempt: u32, cause: Cause) -> Result<Latch, Latch> {
        #[cfg(test)]
        if let Some(hook) = self.before_suspend.lock().unwrap().take() {
            hook();
        }
        self.advance(
            |state| state == Latch::Open(attempt),
            word(attempt, PHASE_SUSPENDED, cause as u8),
        )
    }

    /// Exact hook evidence the ring maps to `attempt`, at any time: `Open(a)`
    /// to `Resolved(a, submitted)`, or `Suspended(a)` to
    /// `Resolved(a, late_exact)`.
    pub fn exact_evidence(&self, attempt: u32) -> Result<Latch, Latch> {
        let open = self.advance(
            |state| state == Latch::Open(attempt),
            word(attempt, PHASE_RESOLVED, Resolution::Submitted as u8),
        );
        if open.is_ok() {
            return open;
        }
        self.advance(
            |state| matches!(state, Latch::Suspended(a, _) if a == attempt),
            word(attempt, PHASE_RESOLVED, Resolution::LateExact as u8),
        )
    }

    /// A transcript echo the transaction saw (`Submitted`), or a completed
    /// strict withdrawal (`Withdrawn`): `Open(a)` or `Suspended(a)` to
    /// `Resolved(a, how)`.
    pub fn resolve(&self, attempt: u32, how: Resolution) -> Result<Latch, Latch> {
        debug_assert!(matches!(how, Resolution::Submitted | Resolution::Withdrawn));
        self.advance(
            |state| matches!(state, Latch::Open(a) | Latch::Suspended(a, _) if a == attempt),
            word(attempt, PHASE_RESOLVED, how as u8),
        )
    }

    /// The user's own submit or clear: whatever attempt is suspended, to
    /// `Resolved(a, user)`. Nothing suspended: nothing.
    pub fn user_cleared(&self) -> Result<Latch, Latch> {
        let Latch::Suspended(attempt, _) = self.load() else {
            return Err(self.load());
        };
        self.advance(
            |state| matches!(state, Latch::Suspended(a, _) if a == attempt),
            word(attempt, PHASE_RESOLVED, Resolution::User as u8),
        )
    }

    /// The generation retired.
    pub fn retire(&self) {
        self.word.store(RETIRED, Ordering::Release);
    }

    /// Test seam: run `hook` once, just before the next suspension's
    /// compare-and-swap.
    #[cfg(test)]
    pub(crate) fn before_next_suspend(&self, hook: impl FnOnce() + Send + 'static) {
        *self.before_suspend.lock().unwrap() = Some(Box::new(hook));
    }
}

/// The hook arm's move on receipt of exact evidence (plan §3.4′): the ring
/// maps the token to its attempt and the cell's exact-attempt guard decides.
/// An evicted token, or one whose attempt the cell is no longer on, moves
/// nothing. `exact` is the prompt's shape; `with_other_text` moves nothing.
pub fn hook_evidence(
    cell: &LatchCell,
    ring: &AttemptRing,
    token: RingToken,
    exact: bool,
) -> Option<Latch> {
    if !exact {
        return None;
    }
    let attempt = ring.lookup(token)?;
    cell.exact_evidence(attempt).ok().map(|_| cell.load())
}

// ------------------------------------------------------------------------
// What the transaction reads

/// The composer a profile located on a frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposerView {
    /// Its text, rows joined; a newline the agent kept is `\n`.
    pub text: String,
    /// The cursor sits right after the text.
    pub cursor_at_end: bool,
    /// The agent's placeholder is drawn (its input is empty).
    pub placeholder: bool,
    /// Part of the composer is highlighted.
    pub selection: bool,
}

/// What the evidence state and the profile's recognisers show of one frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// The layout epoch it was parsed at.
    pub epoch: u64,
    /// The evidence state is Clean.
    pub clean: bool,
    /// A measured dialog marker, by name.
    pub dialog: Option<&'static str>,
    /// The composer, when one is on screen.
    pub composer: Option<ComposerView>,
    /// Complete user messages the transcript shows (the profile's user-role
    /// cells; an assistant's quote is not one).
    pub user_cells: Vec<String>,
    /// The frame's cells as text, for the token's redraw rule.
    pub text: String,
}

impl Frame {
    /// The empty ready composer and no dialog (step 2).
    ///
    /// A blank composer counts only with the cursor where an empty one keeps
    /// it: the text drops trailing blanks, so spaces the user typed, or a row
    /// before blank continuation rows, read as no text with the cursor past
    /// it.
    pub fn empty_and_ready(&self) -> bool {
        self.dialog.is_none()
            && self
                .composer
                .as_ref()
                .is_some_and(|c| c.placeholder || (c.text.is_empty() && c.cursor_at_end))
    }

    /// The composer holds exactly `text`, the cursor right after it.
    pub fn holds_exactly(&self, text: &str) -> bool {
        self.composer
            .as_ref()
            .is_some_and(|c| !c.placeholder && c.text == text && c.cursor_at_end)
    }

    /// The composer holds a strict prefix of `line` of `chars` characters,
    /// cursor at its end: still arriving.
    fn holds_prefix(&self, line: &str, chars: usize) -> bool {
        self.composer.as_ref().is_some_and(|c| {
            !c.placeholder
                && c.cursor_at_end
                && c.text.chars().count() == chars
                && line.starts_with(c.text.as_str())
        })
    }

    fn placeholder_empty(&self) -> bool {
        self.dialog.is_none() && self.composer.as_ref().is_some_and(|c| c.placeholder)
    }
}

/// What the profile measured for the agent's release, as far as the
/// transaction needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// A measured delete key that commits in no dialog of the release.
    pub delete_key: bool,
    /// A measured placeholder, which confirms a withdrawal.
    pub placeholder: bool,
    /// The prompt hook reports the ring token.
    pub hook_route: bool,
    /// The transcript shows the submitted line as a user message.
    pub transcript_route: bool,
    /// Typed in two stages, the first this many characters: the token head
    /// (plan §4.3 as amended by Q27).
    pub staged_head: Option<usize>,
}

/// What the final seam reads under its `try_lock`s (plan §2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeamRead {
    pub watermark: u64,
    pub job_mark: u64,
    pub frame: Frame,
    /// The input-mode evidence is usable at `now`, or the clause that says why
    /// not.
    pub mode: Result<(), &'static str>,
    /// The generation is open and the writer's `pane_retired` flag clear.
    pub open: bool,
}

/// The final seam's view of the daemon: one synchronous read under
/// `try_lock`s, `None` when any of them is contended.
pub trait Seam {
    fn read(&mut self) -> Option<SeamRead>;
}

// ------------------------------------------------------------------------
// Admission

/// Why an attempt ended with nothing written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotTyped {
    /// The activation table or the input-mode evidence refused it, or there
    /// is no submit route (`cannot_type` with this clause).
    CannotType(&'static str),
    /// The admission frame did not show the empty ready composer.
    AwaitingPrompt,
    /// The admission frame showed a dialog.
    Dialog(&'static str),
    /// A human ticket is outstanding or was admitted before the first byte.
    HumanInput,
    /// A layout edge, or no Clean frame.
    Layout,
    /// Inside the first-ring settle window.
    Settling,
    /// The generation stopped being fit for a ring before the first byte.
    NotAdmitted,
    /// The generation retired.
    Retired,
    /// No attempt number left (`attempt_exhausted`).
    Exhausted,
    /// Every token drawn was on the admission screen or recent
    /// ([`TOKEN_DRAWS`]). Not an attempt: no number was allocated, nothing
    /// was recorded or typed and the cell did not move, so the hold carries
    /// on as though this ring had not been tried.
    NoToken,
    /// The deadline passed before the first byte.
    Deadline,
    /// The line's write failed before the pane took any of it.
    WriteFailed,
}

/// What step 1 is decided from (plan §1.1).
#[derive(Debug, Clone)]
pub struct Admission {
    /// The activation verdict: `Ok` for Go or Inherited, else the clause.
    pub activation: Result<(), &'static str>,
    /// The input-mode evidence: usable with both bits clear, or the clause.
    pub mode: Result<(), &'static str>,
    /// The ledger's outstanding set is empty and `until_idle` resolved.
    pub ledger_idle: bool,
    pub watermark: u64,
    pub job_mark: u64,
    /// The current frame; Clean is required.
    pub frame: Frame,
    pub nudge_in_flight: bool,
    pub settling: bool,
    pub submit_route: bool,
    pub still_admitted: bool,
}

/// How a transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    NothingWritten(NotTyped),
    Submitted,
    Withdrawn,
    Unresolved(Cause),
    /// The generation retired after the first possible byte: nothing more was
    /// enqueued.
    Retired,
}

/// A job the transaction hands the writer thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// One stage of the line.
    Line(String),
    /// The Enter.
    Cr,
    /// This many delete keys, in one job.
    Deletes(usize),
}

const JOB_QUEUED: u8 = 0;
const JOB_STARTED: u8 = 1;
const JOB_WITHDRAWN: u8 = 2;

/// One queued job's claim word (plan §2.3, Q24): the one word the PTY writer
/// thread's job (`PtyJob` in `src/agent_pty.rs`), the writer, every
/// withdrawer and the transaction read and move. Queued until the writer
/// claims it to start it or a withdrawer takes it back, and whichever
/// compare-and-swap wins decides whether any byte of the job is ever
/// written. The take-back's result is known in the step that makes it, so
/// ending a transaction waits on no answer.
#[derive(Debug, Default)]
pub struct JobClaim {
    state: AtomicU8,
}

/// Where a [`JobClaim`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    Queued,
    /// The writer claimed it: some, all or none of its bytes may be in.
    Started,
    /// Taken back before the writer came to it: it is never written.
    Withdrawn,
}

impl JobClaim {
    /// Where the claim stands now. A `Queued` answer can be stale the moment
    /// it is read; `Started` and `Withdrawn` are final.
    pub fn state(&self) -> ClaimState {
        match self.state.load(Ordering::Acquire) {
            JOB_QUEUED => ClaimState::Queued,
            JOB_STARTED => ClaimState::Started,
            _ => ClaimState::Withdrawn,
        }
    }

    /// The writer, before the job's first byte: `true` when this call moved
    /// the job from queued to started, so the writer may write it; `false`
    /// when it was no longer queued, because it was taken back (drop it
    /// unwritten) or an earlier `start` already started it.
    pub fn start(&self) -> bool {
        self.state
            .compare_exchange(JOB_QUEUED, JOB_STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The take-back: `true` when this call moved the job from queued to
    /// withdrawn, so it will never be written; `false` when it was no longer
    /// queued, because the writer started it or an earlier `withdraw`
    /// already took it back.
    ///
    /// `false` names the writer only when this is the job's sole withdrawer
    /// and its first call, which is how [`Txn`] uses it: a fresh claim per
    /// enqueue, one `withdraw` consuming it. The daemon keeps that premise by
    /// giving a transaction's job no other withdrawer while the transaction
    /// runs: it is queued without the writer's timeout or drop guard, and
    /// the doorbell's driver, which owns the transaction, takes the claim
    /// back only once the transaction can no longer step (Q24).
    pub fn withdraw(&self) -> bool {
        self.state
            .compare_exchange(
                JOB_QUEUED,
                JOB_WITHDRAWN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

/// Two claims are equal only when they are the same job's.
impl PartialEq for JobClaim {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for JobClaim {}

/// What the transaction asks of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Queue this job on the writer thread (the seam passed). The writer
    /// starts it only through its claim ([`JobClaim::start`]), which the
    /// transaction can take back first.
    Enqueue(Job, Arc<JobClaim>),
    /// Call again at this time (a contended seam, a wait).
    WakeAt(Instant),
    /// Request a re-projection of the generation (plan §3.4′).
    Reproject,
    /// Release the writer: the transaction is over.
    Release(Outcome),
}

/// What the daemon tells the transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A frame the evidence state produced.
    Frame(Frame),
    /// The queued job started: its bytes may reach the pane.
    JobStarted,
    /// The job ended, `accepted` bytes taken, `ok` when the write and its
    /// flush succeeded.
    JobEnded { accepted: usize, ok: bool },
    /// The job's write did not finish within its bound. It is still the
    /// transaction's until it ends.
    JobStalled,
    /// Exact or other hook evidence forwarded by the hook arm, which has
    /// already moved the cell.
    Evidence { token: RingToken, exact: bool },
    /// Time passed.
    Tick,
    /// The input-mode evidence stopped being usable.
    ModeChanged,
    /// The message was taken, the status epoch moved, a receiver armed, or
    /// the admission was lost.
    AdmissionLost,
    /// The generation retired.
    Retired,
}

/// Where the transaction is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The next line stage waits for its seam.
    Typing(usize),
    /// A line stage is queued or running.
    LineJob(usize),
    /// The staged head is written; its echo is awaited.
    HeadEcho,
    /// The whole line is written; its echo is awaited.
    Echo,
    /// The echo held; the Enter waits for its floor and its seam.
    PreCr,
    /// The Enter is queued or running.
    CrJob,
    /// The Enter went in; submit evidence is awaited.
    Confirm,
    /// Deletes wait for their seam, then the placeholder.
    Withdrawing,
    DeleteJob,
    ConfirmWithdrawn,
    /// `Unresolved`, the cell suspended; late evidence or a strict
    /// withdrawal can still resolve it until the deadline.
    Pause(Cause),
    Done(Outcome),
}

/// What a job the transaction queued does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Line,
    Cr,
    Deletes,
}

/// The transaction's one job on the writer (plan §2.3): its own from the
/// enqueue until the job ends or is taken back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owned {
    Idle,
    /// Queued and not started: a take-back can still withdraw it.
    Queued(JobKind),
    /// Started, or its take-back lost: its bytes may reach the pane until it
    /// ends, stalled or not.
    Running(JobKind),
}

/// Why the transaction is ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// Nothing written, if the line's job never started.
    NotTyped(NotTyped),
    /// The deadline.
    Expire,
    /// Exact evidence resolved the attempt.
    Submitted,
    /// The generation retired.
    Retired,
}

/// One doorbell transaction (plan §1.1), from admission to release.
#[derive(Debug)]
pub struct Txn<'c> {
    cell: &'c LatchCell,
    attempt: u32,
    token: RingToken,
    line: String,
    caps: Caps,
    deadline: Instant,
    w0: u64,
    j0: u64,
    epoch: u64,
    phase: Phase,
    frame: Frame,
    /// The job the transaction owns, from its enqueue until it ends.
    job: Owned,
    /// The queued job's claim, while the writer has not reported it started.
    claim: Option<Arc<JobClaim>>,
    /// When the Enter's job last ended: no withdrawal starts within
    /// [`SUBMIT_CONFIRM_WAIT`] of it, as the agent may still take it.
    cr_ended_at: Option<Instant>,
    /// The first possible byte: a line job started (or could have).
    wrote: bool,
    /// The line's bytes the agent accepted.
    accepted: usize,
    line_done_at: Option<Instant>,
    phase_since: Instant,
    /// Deletes for the current withdrawal, and the cause to report if it is
    /// refused or stalls.
    withdrawal: Option<(usize, Cause)>,
    /// An Enter reached the pane, so a newline the agent kept after the line
    /// is the deck's (the measured not-taken path), whether the Enter's job
    /// ended at once or late, after a stall.
    after_cr: bool,
    /// The generation stopped being fit for the ring after the first byte;
    /// acted on once the echo can say whether the line is the deck's alone.
    admission_lost: bool,
}

/// Step 1 and step 2 (plan §1.1): admit an attempt or say why not. On
/// admission the token is picked, then the attempt number allocated, its ring
/// entry written and the cell moved to `Open`, in that order, before any byte.
///
/// A suspended cell does not refuse admission here: whether a suspended
/// generation is rung at all is the doorbell's decision, which reads the cell
/// before it admits (plan §3.4′, "Suspension"), and activation (commit 6)
/// must pin that its driver never admits while the cell is suspended.
#[allow(clippy::too_many_arguments)]
pub fn admit<'c>(
    admission: &Admission,
    counter: &AttemptCounter,
    ring: &AttemptRing,
    cell: &'c LatchCell,
    draw: impl FnMut() -> u8,
    id: &MessageId,
    caps: Caps,
    now: Instant,
) -> Result<Txn<'c>, NotTyped> {
    admission.activation.map_err(NotTyped::CannotType)?;
    admission.mode.map_err(NotTyped::CannotType)?;
    if !admission.submit_route {
        return Err(NotTyped::CannotType("no_submit_route"));
    }
    if !admission.ledger_idle {
        return Err(NotTyped::HumanInput);
    }
    if !admission.frame.clean || admission.nudge_in_flight {
        return Err(NotTyped::Layout);
    }
    if admission.settling {
        return Err(NotTyped::Settling);
    }
    if !admission.still_admitted {
        return Err(NotTyped::NotAdmitted);
    }
    if let Some(dialog) = admission.frame.dialog {
        return Err(NotTyped::Dialog(dialog));
    }
    if !admission.frame.empty_and_ready() {
        return Err(NotTyped::AwaitingPrompt);
    }
    if cell.load() == Latch::Retired {
        return Err(NotTyped::Retired);
    }
    let token = pick_token(draw, &admission.frame.text, ring).ok_or(NotTyped::NoToken)?;
    let attempt = counter.allocate().ok_or(NotTyped::Exhausted)?;
    ring.record(attempt, token);
    cell.open(attempt).map_err(|_| NotTyped::Retired)?;
    Ok(Txn {
        cell,
        attempt,
        token,
        line: token.line(id),
        caps,
        deadline: now + TXN_MAX,
        w0: admission.watermark,
        j0: admission.job_mark,
        epoch: admission.frame.epoch,
        phase: Phase::Typing(0),
        frame: admission.frame.clone(),
        job: Owned::Idle,
        claim: None,
        cr_ended_at: None,
        wrote: false,
        accepted: 0,
        line_done_at: None,
        phase_since: now,
        withdrawal: None,
        after_cr: false,
        admission_lost: false,
    })
}

impl<'c> Txn<'c> {
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn token(&self) -> RingToken {
        self.token
    }

    /// `L_t`.
    pub fn line(&self) -> &str {
        &self.line
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The job mark admission recorded (`J0`, plan §2.1′).
    pub fn j0(&self) -> u64 {
        self.j0
    }

    /// An Enter reached the pane: a withdrawal after it is of a line the
    /// agent did not take.
    pub fn cr_reached(&self) -> bool {
        self.after_cr
    }

    /// The dialog the latest frame showed, by its measured name, or
    /// `unrecognised` for one of the profile's shape with an unknown title.
    pub fn last_dialog(&self) -> Option<&'static str> {
        self.frame.dialog
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    fn stages(&self) -> Vec<String> {
        match self.caps.staged_head {
            Some(head) if head > 0 && head < self.line.chars().count() => {
                let split = self
                    .line
                    .char_indices()
                    .nth(head)
                    .map_or(self.line.len(), |(at, _)| at);
                vec![
                    self.line[..split].to_string(),
                    self.line[split..].to_string(),
                ]
            }
            _ => vec![self.line.clone()],
        }
    }

    /// The text the deck owns in the composer: the accepted part of the line,
    /// and after a not-taken Enter the newline the agent kept.
    fn owned(&self) -> String {
        self.line.chars().take(self.accepted).collect()
    }

    fn enter(&mut self, phase: Phase, now: Instant) {
        self.phase = phase;
        self.phase_since = now;
    }

    /// Start the transaction: try the first line stage's seam.
    pub fn start(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        self.on(Event::Tick, seam, now)
    }

    /// Feed one event; returns what the daemon must do. After `Release` the
    /// transaction takes nothing more.
    pub fn on(&mut self, event: Event, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if let Phase::Done(_) = self.phase {
            return Vec::new();
        }
        if let Event::Frame(frame) = &event {
            self.frame = frame.clone();
        }
        self.track_job(&event, now);
        match event {
            Event::Retired => return self.retired(now),
            Event::Evidence { token, exact } => {
                if let Some(actions) = self.evidence(token, exact, now) {
                    return actions;
                }
            }
            Event::AdmissionLost if !self.wrote => {
                return self.nothing_written(NotTyped::NotAdmitted, now);
            }
            Event::AdmissionLost => self.admission_lost = true,
            _ => {}
        }
        if now >= self.deadline {
            return self.end(End::Expire, now);
        }
        match (self.phase, event) {
            // The rest's seam is being retried: a cancellation since the
            // head's echo is handled as one during it.
            (Phase::Typing(stage), _) if stage > 0 && self.cancels_at_head() => {
                self.cancel_after_head(seam, now)
            }
            (Phase::Typing(stage), _) => self.try_line(stage, seam, now),
            (Phase::LineJob(stage), Event::JobEnded { accepted, ok }) => {
                self.line_ended(stage, accepted, ok, seam, now)
            }
            (Phase::LineJob(_), Event::JobStalled) => {
                self.unresolve(Cause::WriteStalled, seam, now)
            }
            (Phase::HeadEcho | Phase::Echo, Event::ModeChanged) if self.wrote => {
                self.unresolve(Cause::ModeChanged, seam, now)
            }
            (Phase::HeadEcho, _) => self.after_head(seam, now),
            (Phase::Echo | Phase::PreCr, _) if self.admission_lost => {
                self.admission_lost = false;
                self.cancel_after_line(seam, now)
            }
            (Phase::Echo, _) => self.echo(seam, now),
            (Phase::PreCr, Event::ModeChanged) => self.unresolve(Cause::ModeChanged, seam, now),
            (Phase::PreCr, _) => self.pre_cr(seam, now),
            (Phase::CrJob, Event::JobEnded { accepted, ok }) => {
                if ok && accepted == 1 {
                    self.enter(Phase::Confirm, now);
                    self.confirm(seam, now)
                } else {
                    self.unresolve(Cause::WriteStalled, seam, now)
                }
            }
            (Phase::CrJob, Event::JobStalled) => self.unresolve(Cause::WriteStalled, seam, now),
            (Phase::Confirm, _) => self.confirm(seam, now),
            (Phase::Withdrawing, _) => self.try_deletes(seam, now),
            (Phase::DeleteJob, Event::JobEnded { accepted, ok }) => {
                let (deletes, cause) = self.withdrawal.expect("a delete job has its withdrawal");
                let _ = cause;
                if ok && accepted == deletes {
                    self.enter(Phase::ConfirmWithdrawn, now);
                    self.confirm_withdrawn(now)
                } else {
                    self.suspend_and_pause(Cause::WithdrawalStalled, now)
                }
            }
            (Phase::DeleteJob, Event::JobStalled) => {
                self.suspend_and_pause(Cause::WithdrawalStalled, now)
            }
            (Phase::ConfirmWithdrawn, _) => self.confirm_withdrawn(now),
            (Phase::Pause(cause), _) => self.pause(cause, seam, now),
            _ => vec![Action::WakeAt(self.next_wake(now))],
        }
    }

    fn next_wake(&self, now: Instant) -> Instant {
        (now + Duration::from_millis(10)).min(self.deadline)
    }

    fn release(&mut self, outcome: Outcome) -> Vec<Action> {
        debug_assert!(
            !matches!(self.job, Owned::Queued(_)),
            "a queued job is taken back before the release"
        );
        self.phase = Phase::Done(outcome);
        vec![Action::Reproject, Action::Release(outcome)]
    }

    /// Keep the owned job in step with the writer's reports.
    fn track_job(&mut self, event: &Event, now: Instant) {
        match (event, self.job) {
            (Event::JobStarted, Owned::Queued(kind)) => self.started(kind),
            (Event::JobEnded { accepted, .. }, Owned::Queued(kind) | Owned::Running(kind)) => {
                self.job = Owned::Idle;
                self.claim = None;
                // An ended job took its bytes whatever order its reports came
                // in: a line the pane accepted is written even when its start
                // was never seen, so the deadline cannot call it nothing.
                if kind == JobKind::Line {
                    self.wrote |= *accepted > 0;
                }
                if kind == JobKind::Cr {
                    self.cr_ended_at = Some(now);
                    self.after_cr |= *accepted > 0;
                }
            }
            _ => {}
        }
    }

    /// The owned job started: a line's first byte may be in the pane.
    fn started(&mut self, kind: JobKind) {
        self.job = Owned::Running(kind);
        self.claim = None;
        if kind == JobKind::Line {
            self.wrote = true;
        }
    }

    /// Queue `job` on the writer (plan §2.3: one deck job in flight); the
    /// transaction owns it until it ends or is taken back.
    fn enqueue(&mut self, kind: JobKind, job: Job, phase: Phase, now: Instant) -> Vec<Action> {
        debug_assert_eq!(self.job, Owned::Idle, "one deck job in flight");
        let claim = Arc::new(JobClaim::default());
        self.job = Owned::Queued(kind);
        self.claim = Some(Arc::clone(&claim));
        self.enter(phase, now);
        vec![Action::Enqueue(job, claim)]
    }

    /// End the transaction (plan §2.3). A queued job is taken back first, in
    /// this same step, and the ending is decided on the result: only a job
    /// withdrawn before it started cannot run after the release. A job
    /// already started runs on after it, the release's one residual.
    fn end(&mut self, end: End, now: Instant) -> Vec<Action> {
        self.take_back();
        self.finish(end, now)
    }

    /// Take back the queued job, if the writer has not reported it started:
    /// a compare-and-swap on its claim, settled here. Withdrawn, it can never
    /// be written; claimed by the writer first, it is a started job, and a
    /// line's first byte may be in. Reading a lost withdrawal as the writer's
    /// claim holds because this is the claim's one withdrawer and it calls
    /// once ([`JobClaim::withdraw`]).
    fn take_back(&mut self) {
        let Owned::Queued(kind) = self.job else {
            return;
        };
        let claim = self.claim.take().expect("a queued job has its claim");
        if claim.withdraw() {
            self.job = Owned::Idle;
        } else {
            self.started(kind);
        }
    }

    fn finish(&mut self, end: End, now: Instant) -> Vec<Action> {
        match end {
            // The same, reached at or after the deadline (a lost admission
            // is handled before the deadline check): nothing is left to wait
            // for, so the deadline's ending runs in this step.
            End::NotTyped(_) if self.wrote && now >= self.deadline => self.expire(now),
            // The line's job started before its take-back: this is a
            // cancellation after the first byte (plan §3.2), acted on once
            // the echo can say whether the line is the deck's alone.
            End::NotTyped(_) if self.wrote => {
                self.admission_lost = true;
                vec![Action::WakeAt(self.next_wake(now))]
            }
            End::NotTyped(why) => self.not_typed(why),
            End::Expire => self.expire(now),
            End::Submitted => self.release(Outcome::Submitted),
            End::Retired if self.wrote => self.release(Outcome::Retired),
            End::Retired => self.not_typed(NotTyped::Retired),
        }
    }

    /// The attempt ends with nothing written, a queued line taken back first.
    fn nothing_written(&mut self, why: NotTyped, now: Instant) -> Vec<Action> {
        self.end(End::NotTyped(why), now)
    }

    /// Nothing was written and nothing can be: resolve and release.
    fn not_typed(&mut self, why: NotTyped) -> Vec<Action> {
        let _ = self.cell.not_typed(self.attempt);
        self.release(Outcome::NothingWritten(why))
    }

    fn retired(&mut self, now: Instant) -> Vec<Action> {
        self.cell.retire();
        self.end(End::Retired, now)
    }

    /// Evidence forwarded by the hook arm. The arm moved the cell on receipt,
    /// so the cell decides: an attempt it resolved as submitted is over.
    fn evidence(&mut self, token: RingToken, exact: bool, now: Instant) -> Option<Vec<Action>> {
        if token != self.token || !exact || !self.wrote {
            return None;
        }
        match self.cell.load() {
            Latch::Resolved(a, Resolution::Submitted | Resolution::LateExact)
                if a == self.attempt =>
            {
                Some(self.end(End::Submitted, now))
            }
            _ => None,
        }
    }

    /// The deadline (plan §2.3), once no queued job is left: before the first
    /// possible byte the attempt ends with nothing written; after it, the
    /// cell is suspended (unless evidence resolved it first) and the writer
    /// is released, a started job running on.
    fn expire(&mut self, now: Instant) -> Vec<Action> {
        if !self.wrote {
            return self.not_typed(NotTyped::Deadline);
        }
        let _ = now;
        match self.cell.load() {
            Latch::Suspended(a, cause) if a == self.attempt => {
                return self.release(Outcome::Unresolved(cause));
            }
            Latch::Resolved(a, Resolution::Withdrawn) if a == self.attempt => {
                return self.release(Outcome::Withdrawn);
            }
            Latch::Resolved(a, Resolution::Submitted | Resolution::LateExact)
                if a == self.attempt =>
            {
                return self.release(Outcome::Submitted);
            }
            _ => {}
        }
        let cause = if !self.frame.clean || self.frame.epoch != self.epoch {
            Cause::Layout
        } else {
            Cause::Deadline
        };
        match self.cell.suspend(self.attempt, cause) {
            Err(Latch::Resolved(a, Resolution::Submitted | Resolution::LateExact))
                if a == self.attempt =>
            {
                self.release(Outcome::Submitted)
            }
            _ => self.release(Outcome::Unresolved(cause)),
        }
    }

    /// The final seam (plan §2.4) for the next job: one synchronous read
    /// under `try_lock`s, every check, then the enqueue. The first job that
    /// can write a byte checks the watermark against `W0`; every later one
    /// checks the job mark against `J0`.
    fn seam(&mut self, seam: &mut dyn Seam, first_byte: bool, now: Instant) -> SeamVerdict {
        let Some(read) = seam.read() else {
            return SeamVerdict::Contended((now + SEAM_RETRY).min(self.deadline));
        };
        self.frame = read.frame.clone();
        if !read.open {
            return SeamVerdict::Closed;
        }
        if now >= self.deadline {
            return SeamVerdict::Expired;
        }
        if first_byte && read.watermark != self.w0 {
            return SeamVerdict::Human;
        }
        if !first_byte && read.job_mark != self.j0 {
            return SeamVerdict::JobMark;
        }
        if let Err(clause) = read.mode {
            return SeamVerdict::Mode(clause);
        }
        if !read.frame.clean || read.frame.epoch != self.epoch {
            return SeamVerdict::Layout;
        }
        SeamVerdict::Pass(read.frame)
    }

    fn try_line(&mut self, stage: usize, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        let first = stage == 0;
        match self.seam(seam, first, now) {
            SeamVerdict::Contended(at) => vec![Action::WakeAt(at)],
            SeamVerdict::Pass(frame) => {
                let ready = if first {
                    frame.empty_and_ready()
                } else {
                    frame.dialog.is_none() && frame.holds_prefix(&self.line, self.accepted)
                };
                if !ready {
                    return if first {
                        match frame.dialog {
                            Some(dialog) => self.nothing_written(NotTyped::Dialog(dialog), now),
                            None => self.nothing_written(NotTyped::AwaitingPrompt, now),
                        }
                    } else {
                        self.unresolve(Cause::DialogAfterTyping, seam, now)
                    };
                }
                let text = self.stages()[stage].clone();
                self.enqueue(JobKind::Line, Job::Line(text), Phase::LineJob(stage), now)
            }
            SeamVerdict::Human if first => self.nothing_written(NotTyped::HumanInput, now),
            SeamVerdict::Layout if first => self.nothing_written(NotTyped::Layout, now),
            SeamVerdict::Mode(clause) if first => {
                self.nothing_written(NotTyped::CannotType(clause), now)
            }
            SeamVerdict::Closed if first => self.nothing_written(NotTyped::Retired, now),
            SeamVerdict::Expired if first => self.nothing_written(NotTyped::Deadline, now),
            SeamVerdict::Layout => vec![Action::WakeAt(self.next_wake(now))],
            SeamVerdict::Mode(_) => self.unresolve(Cause::ModeChanged, seam, now),
            SeamVerdict::Closed => self.retired(now),
            _ => self.unresolve(Cause::EchoMismatch, seam, now),
        }
    }

    fn line_ended(
        &mut self,
        stage: usize,
        accepted: usize,
        ok: bool,
        seam: &mut dyn Seam,
        now: Instant,
    ) -> Vec<Action> {
        let stages = self.stages();
        let wanted = stages[stage].len();
        let before: usize = stages[..stage].iter().map(|s| s.chars().count()).sum();
        let taken = stages[stage][..accepted.min(wanted)].chars().count();
        self.accepted = before + taken;
        if accepted == 0 && stage == 0 {
            // Nothing reached the pane.
            self.wrote = false;
            return self.nothing_written(NotTyped::WriteFailed, now);
        }
        self.wrote = true;
        if !ok || accepted < wanted {
            return self.unresolve(Cause::PartialWrite, seam, now);
        }
        if stage + 1 < stages.len() {
            self.enter(Phase::HeadEcho, now);
            return self.after_head(seam, now);
        }
        self.line_done_at = Some(now);
        self.enter(Phase::Echo, now);
        self.echo(seam, now)
    }

    /// The staged head is written. A cancellation while its echo is awaited
    /// means the rest is never typed and the head is withdrawn once its echo
    /// shows it is the deck's alone; a profile that cannot withdraw finishes
    /// the line and handles the cancellation after it, as for an unstaged
    /// line.
    fn after_head(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if self.cancels_at_head() {
            self.cancel_after_head(seam, now)
        } else {
            self.head_echo(seam, now)
        }
    }

    fn cancels_at_head(&self) -> bool {
        self.admission_lost && self.caps.delete_key && self.caps.placeholder
    }

    /// Waits for the staged head's echo, then types the rest.
    fn head_echo(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if !self.frame.clean || self.frame.epoch != self.epoch {
            return self.edge(now);
        }
        if let Some(_dialog) = self.frame.dialog {
            return self.unresolve(Cause::DialogAfterTyping, seam, now);
        }
        if self.frame.holds_prefix(&self.line, self.accepted) {
            self.enter(Phase::Typing(1), now);
            return self.try_line(1, seam, now);
        }
        if now >= self.phase_since + ECHO_WAIT {
            return self.unresolve(Cause::EchoMismatch, seam, now);
        }
        vec![Action::WakeAt(self.next_wake(now))]
    }

    /// [`Self::head_echo`] after a cancellation: on the head's exact echo the
    /// head is withdrawn under the strict recogniser instead of the rest
    /// being typed; with no such echo within [`ECHO_WAIT`], `Unresolved`.
    fn cancel_after_head(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if !self.frame.clean || self.frame.epoch != self.epoch {
            return self.edge(now);
        }
        if self.frame.dialog.is_some() {
            return self.unresolve(Cause::DialogAfterTyping, seam, now);
        }
        if self.frame.holds_prefix(&self.line, self.accepted) {
            self.admission_lost = false;
            return self.unresolve(Cause::EchoMismatch, seam, now);
        }
        if now >= self.phase_since + ECHO_WAIT {
            return self.unresolve(Cause::EchoMismatch, seam, now);
        }
        vec![Action::WakeAt(self.next_wake(now))]
    }

    /// A layout edge after the first byte (plan §2.2): wait for a Clean frame,
    /// then take its epoch and evaluate the step on it again.
    fn edge(&mut self, now: Instant) -> Vec<Action> {
        if self.frame.clean {
            self.epoch = self.frame.epoch;
            return vec![Action::WakeAt(now)];
        }
        vec![Action::WakeAt(self.next_wake(now))]
    }

    /// Step 4: a Clean frame at the epoch shows exactly `L_t`, cursor at its
    /// end; a prefix keeps the wait going.
    fn echo(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if !self.frame.clean || self.frame.epoch != self.epoch {
            return self.edge(now);
        }
        if self.frame.dialog.is_some() {
            return self.unresolve(Cause::DialogAfterTyping, seam, now);
        }
        if self.frame.holds_exactly(&self.line) {
            self.enter(Phase::PreCr, now);
            return self.pre_cr(seam, now);
        }
        let done = self.line_done_at.unwrap_or(self.phase_since);
        if now >= done + ECHO_WAIT {
            return self.unresolve(Cause::EchoMismatch, seam, now);
        }
        vec![Action::WakeAt(self.next_wake(now))]
    }

    /// Step 5, the final seam for the Enter: no await between its verdict and
    /// its enqueue. `SUBMIT_DELAY` is a floor from the line's write
    /// completion.
    fn pre_cr(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        let floor = self.line_done_at.unwrap_or(now) + SUBMIT_DELAY;
        if now < floor {
            return vec![Action::WakeAt(floor.min(self.deadline))];
        }
        match self.seam(seam, false, now) {
            SeamVerdict::Contended(at) => vec![Action::WakeAt(at)],
            SeamVerdict::Pass(frame) => {
                if frame.dialog.is_some() {
                    return self.unresolve(Cause::DialogBeforeSubmit, seam, now);
                }
                if !frame.holds_exactly(&self.line) {
                    return self.unresolve(Cause::EchoMismatch, seam, now);
                }
                self.enqueue(JobKind::Cr, Job::Cr, Phase::CrJob, now)
            }
            SeamVerdict::Layout => self.edge(now),
            SeamVerdict::Mode(_) => self.unresolve(Cause::ModeChanged, seam, now),
            SeamVerdict::Closed => self.retired(now),
            SeamVerdict::Expired => self.end(End::Expire, now),
            SeamVerdict::Human | SeamVerdict::JobMark => {
                self.unresolve(Cause::EchoMismatch, seam, now)
            }
        }
    }

    /// Step 6: submit evidence, within [`SUBMIT_CONFIRM_WAIT`]. A transcript
    /// echo of `L_t` while the composer no longer holds it is `Submitted`
    /// (hook evidence arrives as [`Event::Evidence`]); an empty composer alone
    /// proves nothing. At the window's end, a measured not-taken state (the
    /// line still there, or with a newline kept) is withdrawn strictly, or
    /// refused for a profile with no placeholder to confirm it; anything else
    /// is `Unresolved(no_submit_evidence)`.
    fn confirm(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        let current = self.frame.clean && self.frame.epoch == self.epoch;
        if !self.frame.clean {
            let _ = self.edge(now);
        } else if !current {
            self.epoch = self.frame.epoch;
        }
        if self.frame.clean {
            if self.frame.dialog.is_some() {
                return self.unresolve(Cause::DialogAfterSubmit, seam, now);
            }
            if self.transcript_echo() {
                let _ = self.cell.resolve(self.attempt, Resolution::Submitted);
                return self.release(Outcome::Submitted);
            }
        }
        if now < self.phase_since + SUBMIT_CONFIRM_WAIT {
            return vec![Action::WakeAt(
                (self.phase_since + SUBMIT_CONFIRM_WAIT).min(self.next_wake(now)),
            )];
        }
        if self.strict().is_some() && !self.caps.placeholder {
            return self.suspend_and_pause(Cause::WithdrawalRefused, now);
        }
        self.unresolve(Cause::NoSubmitEvidence, seam, now)
    }

    /// The transcript shows `L_t` as a user message and the composer no
    /// longer holds it.
    fn transcript_echo(&self) -> bool {
        self.caps.transcript_route
            && self.frame.clean
            && self.frame.user_cells.contains(&self.line)
            && !self
                .frame
                .composer
                .as_ref()
                .is_some_and(|c| c.text.starts_with(self.line.as_str()))
    }

    /// A mid-transaction cancellation after the line (plan §3.2): with the
    /// echo proof holding, withdraw strictly, or submit for a profile with no
    /// delete key (a stale line's fetch finds nothing); without it,
    /// `Unresolved`.
    fn cancel_after_line(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if self.frame.clean
            && self.frame.epoch == self.epoch
            && self.frame.holds_exactly(&self.line)
        {
            if self.caps.delete_key && self.caps.placeholder {
                if let Some(deletes) = self.strict() {
                    self.withdrawal = Some((deletes, Cause::EchoMismatch));
                    self.enter(Phase::Withdrawing, now);
                    return self.try_deletes(seam, now);
                }
            } else if !self.caps.delete_key {
                self.enter(Phase::PreCr, now);
                return self.pre_cr(seam, now);
            }
        }
        self.unresolve(Cause::EchoMismatch, seam, now)
    }

    /// The strict recogniser (plan §3.3) on the current frame: the composer
    /// holds exactly the owned text (after a not-taken Enter, with its kept
    /// newline), starting with a whole token, cursor right after it, nothing
    /// highlighted, no dialog, the layout epoch unchanged. Returns the
    /// deletes to send.
    fn strict(&self) -> Option<usize> {
        let owned = self.owned();
        let head: Vec<char> = owned
            .chars()
            .take(crate::doorbell_screen::TOKEN_HEAD_CHARS)
            .collect();
        let whole_token = head.len() == crate::doorbell_screen::TOKEN_HEAD_CHARS
            && head[0] == '('
            && head[7] == ')'
            && head[1..7].iter().all(char::is_ascii_lowercase);
        if !whole_token
            || !self.frame.clean
            || self.frame.epoch != self.epoch
            || self.frame.dialog.is_some()
        {
            return None;
        }
        let composer = self.frame.composer.as_ref()?;
        if composer.selection || composer.placeholder || !composer.cursor_at_end {
            return None;
        }
        let newline = composer.text == format!("{owned}\n");
        if !(composer.text == owned || newline) || (newline && !self.after_cr) {
            return None;
        }
        Some(owned.chars().count() + usize::from(newline))
    }

    /// A withdrawal may start now: the profile can delete and confirm, the
    /// transaction owns no job on the writer, stalled or queued (plan §3.3:
    /// no deck job in flight), and any Enter ended at least
    /// [`SUBMIT_CONFIRM_WAIT`] ago, as the agent may still take it.
    fn may_withdraw(&self, now: Instant) -> bool {
        self.caps.delete_key
            && self.caps.placeholder
            && self.job == Owned::Idle
            && self
                .cr_ended_at
                .is_none_or(|ended| now >= ended + SUBMIT_CONFIRM_WAIT)
    }

    /// A failure after the first byte: withdraw strictly when it applies,
    /// else `Unresolved(cause)` (plan §1.1, step 4 and 5's failures). The
    /// pause can still withdraw once its job has ended.
    fn unresolve(&mut self, cause: Cause, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if self.may_withdraw(now)
            && let Some(deletes) = self.strict()
        {
            self.withdrawal = Some((deletes, cause));
            self.enter(Phase::Withdrawing, now);
            return self.try_deletes(seam, now);
        }
        self.unresolve_without_withdrawal(cause, now)
    }

    fn unresolve_without_withdrawal(&mut self, cause: Cause, now: Instant) -> Vec<Action> {
        self.suspend_and_pause(cause, now)
    }

    /// Suspend the cell, unless exact evidence resolved the attempt first, in
    /// which case it was submitted and nothing is shown; then pause until the
    /// deadline for late evidence or a strict withdrawal.
    fn suspend_and_pause(&mut self, cause: Cause, now: Instant) -> Vec<Action> {
        match self.cell.suspend(self.attempt, cause) {
            Ok(_) => {
                self.enter(Phase::Pause(cause), now);
                vec![Action::Reproject, Action::WakeAt(self.next_wake(now))]
            }
            Err(Latch::Resolved(a, Resolution::Submitted | Resolution::LateExact))
                if a == self.attempt =>
            {
                self.release(Outcome::Submitted)
            }
            Err(Latch::Resolved(a, Resolution::Withdrawn)) if a == self.attempt => {
                self.release(Outcome::Withdrawn)
            }
            Err(_) => {
                // Already suspended (a second cause in the pause) or retired.
                self.enter(Phase::Pause(cause), now);
                vec![Action::WakeAt(self.next_wake(now))]
            }
        }
    }

    fn try_deletes(&mut self, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        let (deletes, cause) = self.withdrawal.expect("a withdrawal has its deletes");
        match self.seam(seam, false, now) {
            SeamVerdict::Contended(at) => vec![Action::WakeAt(at)],
            SeamVerdict::Pass(_) => {
                // A fresh frame, immediately before the deletes.
                if self.strict() != Some(deletes) {
                    return self.suspend_and_pause(cause, now);
                }
                self.enqueue(
                    JobKind::Deletes,
                    Job::Deletes(deletes),
                    Phase::DeleteJob,
                    now,
                )
            }
            SeamVerdict::Layout => self.edge(now),
            SeamVerdict::Closed => self.retired(now),
            SeamVerdict::Expired => self.end(End::Expire, now),
            SeamVerdict::Mode(_) => self.suspend_and_pause(Cause::ModeChanged, now),
            SeamVerdict::Human | SeamVerdict::JobMark => self.suspend_and_pause(cause, now),
        }
    }

    /// `Withdrawn` needs the delete job done in full and then a Clean frame
    /// showing the placeholder. After a layout edge that is the next Clean
    /// frame, at the new epoch (plan §2.2).
    fn confirm_withdrawn(&mut self, now: Instant) -> Vec<Action> {
        if self.frame.clean {
            self.epoch = self.frame.epoch;
            if self.frame.placeholder_empty() {
                let _ = self.cell.resolve(self.attempt, Resolution::Withdrawn);
                return self.release(Outcome::Withdrawn);
            }
        }
        vec![Action::WakeAt(self.next_wake(now))]
    }

    /// `UNRESOLVED_PAUSE`: the rest of the deadline. A transcript echo, or a
    /// strict withdrawal that becomes possible, still resolves the attempt.
    fn pause(&mut self, cause: Cause, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        // A Clean frame after an edge is the one evaluated from here on.
        if self.frame.clean && self.frame.epoch != self.epoch {
            self.epoch = self.frame.epoch;
        }
        if let Latch::Resolved(a, how) = self.cell.load()
            && a == self.attempt
        {
            return match how {
                Resolution::Withdrawn => self.release(Outcome::Withdrawn),
                Resolution::User => self.release(Outcome::Unresolved(cause)),
                _ => self.release(Outcome::Submitted),
            };
        }
        if self.transcript_echo()
            && self
                .cell
                .resolve(self.attempt, Resolution::Submitted)
                .is_ok()
        {
            return self.release(Outcome::Submitted);
        }
        if self.withdrawal.is_none()
            && self.may_withdraw(now)
            && let Some(deletes) = self.strict()
        {
            self.withdrawal = Some((deletes, cause));
            self.enter(Phase::Withdrawing, now);
            return self.try_deletes(seam, now);
        }
        vec![Action::WakeAt(self.next_wake(now))]
    }
}

/// The seam's verdict before one job.
enum SeamVerdict {
    Contended(Instant),
    Pass(Frame),
    Human,
    JobMark,
    Mode(&'static str),
    Layout,
    Closed,
    Expired,
}

// ------------------------------------------------------------------------
// Re-projection (plan §3.4′)

/// What a generation's card shows from its cell (plan §5.1, row 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// `uncertain_input`.
    UncertainInput,
    /// `dialog_during_ring`.
    DialogDuringRing,
}

/// The cell's projection: `Suspended` shows, every other state is clear.
pub fn project(latch: Latch) -> Option<Shown> {
    match latch {
        Latch::Suspended(_, cause) if cause.is_dialog() => Some(Shown::DialogDuringRing),
        Latch::Suspended(..) => Some(Shown::UncertainInput),
        _ => None,
    }
}

/// The re-projection publisher's model: requests name `(pane, agent)`, run
/// one at a time in order, a generation's pending requests coalesce, and a run
/// reads the cell at that moment and installs nothing of its own.
#[derive(Debug, Default)]
pub struct Publisher {
    pending: VecDeque<(String, String)>,
    /// The snapshot's projection per pane.
    snapshot: HashMap<String, Option<Shown>>,
    /// Every broadcast sent, in order.
    pub broadcasts: Vec<(String, Option<Shown>)>,
}

impl Publisher {
    /// Ask for a re-projection; never blocks, and coalesces with one already
    /// pending for the same generation.
    pub fn request(&mut self, pane: &str, agent: &str) {
        let key = (pane.to_string(), agent.to_string());
        if !self.pending.contains(&key) {
            self.pending.push_back(key);
        }
    }

    /// Requests waiting.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Run the oldest request: nothing when the pane's current agent is
    /// another; otherwise the cell as it is now, into the snapshot, broadcast
    /// only when it changed.
    pub fn run_next<'a>(
        &mut self,
        current_agent: impl Fn(&str) -> Option<String>,
        cell_of: impl Fn(&str) -> Option<&'a LatchCell>,
    ) -> bool {
        let Some((pane, agent)) = self.pending.pop_front() else {
            return false;
        };
        if current_agent(&pane).as_deref() != Some(agent.as_str()) {
            return true;
        }
        let Some(cell) = cell_of(&agent) else {
            return true;
        };
        let shown = project(cell.load());
        if self.snapshot.insert(pane.clone(), shown).flatten() != shown {
            self.broadcasts.push((pane, shown));
        }
        true
    }

    /// The snapshot's projection for `pane`.
    pub fn shown(&self, pane: &str) -> Option<Shown> {
        self.snapshot.get(pane).copied().flatten()
    }
}

// ------------------------------------------------------------------------
// The seam's lock model (plan §2.4)

/// The seam's retry loop over virtual time: `try_seam` is called at `start`
/// and every [`SEAM_RETRY`] after, until it passes or the deadline comes.
/// Returns when it passed, or `None` when the deadline came first; no call is
/// made at or after the deadline.
pub fn seam_until(
    start: Instant,
    deadline: Instant,
    mut try_seam: impl FnMut(Instant) -> bool,
) -> Option<Instant> {
    let mut now = start;
    while now < deadline {
        if try_seam(now) {
            return Some(now);
        }
        now = (now + SEAM_RETRY).min(deadline);
    }
    None
}
