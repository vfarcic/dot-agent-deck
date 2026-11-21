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
    /// line, `L_t` (plan §1.2), naming the running deck
    /// ([`crate::doorbell::doorbell_line`]). A message id is ASCII with no
    /// control character, and the command word holds none either
    /// ([`crate::platform::paths::binary_name`]), so the line has none.
    pub fn line(self, id: &MessageId) -> String {
        self.line_naming(id, &crate::platform::paths::binary_name())
    }

    /// [`Self::line`] with `command` as the doorbell line's command word.
    pub fn line_naming(self, id: &MessageId, command: &str) -> String {
        format!(
            "({}){}",
            self.letters(),
            crate::doorbell::doorbell_line_naming(id.as_str(), command)
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
    /// The composer's region holds something its text does not show
    /// ([`crate::doorbell_screen::Composer::hidden`]).
    pub hidden: bool,
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
    /// PRD #1666 part 3 (Q35-21, R3 §3.6): something entered the input box
    /// since the last observation that proved it empty (the generation's
    /// input epoch, read with the frame). The screen cannot show blanks the
    /// agent does not draw, so a blank composer is empty only while nothing
    /// touched the box; a touched one is empty only once a check reads it so.
    pub touched: bool,
}

impl Frame {
    /// The empty ready composer and no dialog (step 2).
    ///
    /// The screen and the box's input epoch both have to say so (R6-4,
    /// Q35-21). A composer counts while its placeholder shows, or while it
    /// is blank with the cursor where an empty one keeps it and nothing
    /// hidden in its region, and only while the epoch says nothing touched
    /// the box since it was last observed empty: spaces with the cursor
    /// moved back to the start draw exactly the empty composer, and a
    /// placeholder on screen can predate the keys that replaced it. Three
    /// things make a touched box untouched again: a check that reads it
    /// empty, the deck's own submitted line, and the deck's own withdrawn
    /// line, which ends only once the agent drew its placeholder again over
    /// an empty box. Both lines go through the ring's reconcile
    /// (`GenerationInput::reconcile_ring`: R3 §3.5 for the submitted line,
    /// the Q35-21 ruling for the withdrawn one).
    pub fn empty_and_ready(&self) -> bool {
        !self.touched
            && self.dialog.is_none()
            && self.composer.as_ref().is_some_and(|c| {
                c.placeholder || (!c.hidden && c.text.is_empty() && c.cursor_at_end)
            })
    }

    /// The composer holds exactly `text`, the cursor right after it.
    pub fn holds_exactly(&self, text: &str) -> bool {
        self.composer
            .as_ref()
            .is_some_and(|c| !c.placeholder && !c.hidden && c.text == text && c.cursor_at_end)
    }

    /// The composer holds a strict prefix of `line` of `chars` characters,
    /// cursor at its end: still arriving.
    fn holds_prefix(&self, line: &str, chars: usize) -> bool {
        self.composer.as_ref().is_some_and(|c| {
            !c.placeholder
                && !c.hidden
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
    /// The pane's input was inside an escape sequence, a string or a paste
    /// the user hadn't finished when the line was to start (PRD #1666 part
    /// 3, Q35-16): nothing was written, and the doorbell tries again later.
    OffGround,
}

/// What step 1 is decided from (plan §1.1).
#[derive(Debug, Clone)]
pub struct Admission {
    /// The activation verdict: `Ok` for Go, else the clause.
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
    /// The command word the line names for `inbox fetch`: the deck the daemon
    /// runs from ([`crate::platform::paths::binary_name`], issue #1730).
    pub command: String,
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
    /// This many presses of the delete key, in one job.
    Deletes(usize),
    /// PRD #211: this many presses of the agent's clear key, in one job,
    /// where a withdrawal's final seam chose it ([`Txn::clear_lines`],
    /// [`WithdrawKey::Clear`]). [`Txn::serve_keys`] sets that key only
    /// where it is Ctrl+U, which empties one logical line per press.
    Clears(usize),
}

const JOB_QUEUED: u8 = 0;
const JOB_STARTED: u8 = 1;
const JOB_WITHDRAWN: u8 = 2;
const JOB_REFUSED: u8 = 3;

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
    /// The writer came to it and its start condition refused it
    /// ([`JobClaim::start_if`]): it is never written, and the writer, not a
    /// withdrawer, accounts for it.
    Refused,
}

/// What the PTY writer thread knows when it comes to a job (PRD #1666 part
/// 3, Q35-16, I-8), built there from its own state, for a job's start
/// condition: where the bytes the PTY took so far leave the stream, whether
/// its generation retired (a writer thread is one generation's, and a
/// successor published on its pane retires it), and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartCtx {
    pub boundary: crate::composer_probe::planner::Boundary,
    pub retired: bool,
    pub now: Instant,
}

/// What [`JobClaim::start_if`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start<R> {
    /// Started: the writer writes it.
    Started,
    /// Its condition refused it, for this reason: it is never written.
    Refused(R),
    /// It was no longer queued (taken back, or already started): the
    /// condition wasn't asked.
    NotQueued,
}

impl JobClaim {
    /// Where the claim stands now. A `Queued` answer can be stale the moment
    /// it is read; `Started` and `Withdrawn` are final.
    pub fn state(&self) -> ClaimState {
        match self.state.load(Ordering::Acquire) {
            JOB_QUEUED => ClaimState::Queued,
            JOB_STARTED => ClaimState::Started,
            JOB_REFUSED => ClaimState::Refused,
            _ => ClaimState::Withdrawn,
        }
    }

    /// The writer, before the job's first byte, for a job with a start
    /// condition (PRD #1666 part 3, Q35-16, I-8): `pred` is asked, on the
    /// writer thread with `ctx` the writer built, only while the job is still
    /// queued. `Ok` starts it as [`Self::start`] does; `Err(reason)` moves it
    /// to refused and hands `reason` back unchanged, whatever its type. Either
    /// move is one compare-and-swap from queued, so a take-back that lands
    /// while `pred` runs wins and the job is [`Start::NotQueued`]: a refused
    /// job is never also withdrawn, nor a withdrawn one refused.
    pub fn start_if<R>(
        &self,
        ctx: &StartCtx,
        pred: impl FnOnce(&StartCtx) -> Result<(), R>,
    ) -> Start<R> {
        if self.state() != ClaimState::Queued {
            return Start::NotQueued;
        }
        let (to, verdict) = match pred(ctx) {
            Ok(()) => (JOB_STARTED, Start::Started),
            Err(reason) => (JOB_REFUSED, Start::Refused(reason)),
        };
        match self
            .state
            .compare_exchange(JOB_QUEUED, to, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => verdict,
            Err(_) => Start::NotQueued,
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
    /// The writer came to the job and its start condition refused it
    /// ([`JobClaim::start_if`]): none of it was written (PRD #1666 part 3,
    /// Q35-16: a deck line starts only where the bytes the pane took leave
    /// its input at a boundary).
    JobNotStarted,
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
    /// PRD #211: a paced withdrawal's next deletes wait until then
    /// ([`Txn::pace_deletes`]).
    DeletePace(Instant),
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
    /// A withdrawal's presses, of the delete key or the clear key.
    Deletes,
}

/// PRD #211: the key a withdrawal presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawKey {
    /// One delete per character the agent took.
    Delete,
    /// The agent's clear key, which empties one logical line per press
    /// ([`Txn::clear_lines`]).
    Clear,
}

/// How a withdrawal's presses go out: `total` presses of `key`, at most
/// `per_write` to a job, `gap` apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Presses {
    key: WithdrawKey,
    total: usize,
    per_write: usize,
    gap: Duration,
}

impl Presses {
    /// Whether every job of these presses can start before `deadline`,
    /// the first at `now`.
    fn fit(&self, now: Instant, deadline: Instant) -> bool {
        let writes = self.total.div_ceil(self.per_write);
        let needs = self
            .gap
            .saturating_mul(u32::try_from(writes.saturating_sub(1)).unwrap_or(u32::MAX));
        now + needs < deadline
    }
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
    /// Diagnosis only: the step and check that last unresolved the attempt,
    /// with the length of the composer text it read then. Never the text:
    /// it is logged at the default level, and the box can hold whatever the
    /// user typed or pasted.
    unresolved_at: Option<String>,
    /// The driver allowed this attempt the misrendered-echo withdrawal
    /// ([`Self::allow_gap_withdrawal`]).
    gap_allowed: bool,
    /// The current withdrawal is that one: its recogniser is
    /// [`Self::gap_view`], not [`Self::strict`].
    gap: bool,
    /// PRD #211: the agent's served delete pacing — at most this many
    /// deletes per write, this long apart ([`Self::pace_deletes`]); `None`
    /// sends a withdrawal's deletes in one job.
    delete_pace: Option<(usize, Duration)>,
    /// PRD #211: the agent's served clear key, set by [`Self::clear_lines`]:
    /// presses per logical line, at most this many to a write, this long
    /// apart. `None` withdraws with deletes only.
    clear: Option<(usize, usize, Duration)>,
    /// The current withdrawal's presses, once its final seam chose them.
    presses: Option<Presses>,
    /// The presses of the current withdrawal the agent has taken so far, and
    /// how many the job in flight carries.
    deleted: usize,
    delete_job: usize,
}

/// PRD #211: `shown` is `typed` with only spaces inserted into it — every
/// character of `typed` in order, nothing missing, nothing else added, and
/// nothing before or after it (`shown` starts and ends with `typed`'s first
/// and last characters) — and is not `typed` itself, which is the exact
/// echo. This is how the composer draws a line whose every byte the agent
/// took while its render fell out of step with its buffer (Antigravity's
/// measured seven-column gap). A newline, a tab or any other character is
/// not such a gap.
pub fn whitespace_gap(shown: &str, typed: &str) -> bool {
    if shown == typed || typed.is_empty() {
        return false;
    }
    if shown.chars().next() != typed.chars().next()
        || shown.chars().next_back() != typed.chars().next_back()
    {
        return false;
    }
    let mut rest = typed.chars().peekable();
    for c in shown.chars() {
        if rest.peek() == Some(&c) {
            rest.next();
        } else if c != ' ' {
            return false;
        }
    }
    rest.peek().is_none()
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
        line: token.line_naming(id, &admission.command),
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
        unresolved_at: None,
        gap_allowed: false,
        gap: false,
        delete_pace: None,
        clear: None,
        presses: None,
        deleted: 0,
        delete_job: 0,
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

    /// Why a withdrawal started, if one did (diagnosis: the attempt's log
    /// line names it).
    pub fn withdrawal_cause(&self) -> Option<Cause> {
        self.withdrawal.map(|(_, cause)| cause)
    }

    /// Whether the caller's admission re-check said no after the line was
    /// written (diagnosis, as above).
    pub fn admission_was_lost(&self) -> bool {
        self.admission_lost
    }

    /// PRD #211: allow this attempt one recovery from a misrendered echo.
    /// When the echo fails and the composer is drawn as the deck's own
    /// line with only spaces inserted ([`whitespace_gap`]), the line is
    /// withdrawn, with one delete per character the deck typed or with the
    /// agent's clear key ([`Self::clear_lines`]), instead of
    /// the attempt ending `Unresolved(EchoMismatch)`; the doorbell then
    /// rings again. The driver allows it for an agent whose render is
    /// measured to fall out of step with a buffer that took every byte, and
    /// never for the ring right after one that used it, so a second
    /// misrendered echo stays suspended.
    pub fn allow_gap_withdrawal(&mut self) {
        self.gap_allowed = true;
    }

    /// PRD #211: send a withdrawal's deletes at most `per_write` to a write,
    /// `gap` apart, as the agent's served keys ask (Antigravity drops some of
    /// a burst: [`crate::agent_registry::DeleteCharKey::max_presses_per_write`]).
    /// Each write is a job of its own on the writer, under this transaction's
    /// ownership and its seam's checks, and none starts after the deadline. A
    /// paced withdrawal whose writes cannot all start before the deadline is
    /// not started at all, so the line stays whole and the attempt
    /// unresolved; one the deadline still cuts short ends unresolved too.
    pub fn pace_deletes(&mut self, per_write: usize, gap: Duration) {
        self.delete_pace = Some((per_write.max(1), gap));
    }

    /// PRD #211: withdraw a line the composer holds as ONE logical line,
    /// and nothing else, with the agent's clear key instead of deletes:
    /// `per_line` presses, at most `per_write` to a write (`None`: all in
    /// one), `gap` apart. Both recognisers already require the rendered
    /// composer to show only the deck's own text, and the seam's job mark
    /// that no person's job reached the writer since admission, so emptying
    /// the line removes what the screen and that ordering say is only the
    /// deck's. Neither reads the agent's buffer itself, and the clear has
    /// the same window between its last check and the agent's handling of
    /// it as the Enter (`docs/develop/doorbell-admission.md`, accepted
    /// residuals). Measured on `agy` 1.5.9
    /// (`tests/fixtures/antigravity/composer-after-clear.bin`): one Ctrl+U
    /// emptied a 166-character line wrapped onto two rows.
    /// A withdrawal whose presses the deadline cannot fit falls back to the
    /// deletes, paced or not, when those fit. A withdrawal with a kept
    /// newline is two logical lines, a count not measured, so it deletes.
    pub fn clear_lines(&mut self, per_line: usize, per_write: Option<usize>, gap: Duration) {
        let per_line = per_line.max(1);
        self.clear = Some((per_line, per_write.unwrap_or(per_line).max(1), gap));
    }

    /// PRD #211: withdraw with the agent's served keys (`keys`, from
    /// [`crate::agent_registry`]). An agent whose deletes must be paced
    /// (Antigravity drops some of a burst) has them paced as served
    /// ([`Self::pace_deletes`]); paced, a doorbell line's deletes outlast the
    /// deadline, so such an agent also withdraws a one-line composer with
    /// its clear key ([`Self::clear_lines`]), one press, where that key is
    /// Ctrl+U and empties a logical line per press. The other agents'
    /// deletes are measured exact in one burst, and they keep them.
    pub fn serve_keys(&mut self, keys: &crate::agent_registry::PromptKeys) {
        let ms = |pause: Option<u32>| Duration::from_millis(u64::from(pause.unwrap_or(0)));
        let Some(per_write) = keys.delete_char.max_presses_per_write else {
            return;
        };
        self.pace_deletes(
            per_write as usize,
            ms(keys.delete_char.pause_between_writes_ms),
        );
        if keys.clear.presses == crate::agent_registry::ClearPresses::PerLine
            && keys.clear.bytes == "\x15"
        {
            self.clear_lines(
                1,
                keys.clear.max_presses_per_write.map(|n| n as usize),
                ms(keys.clear.pause_between_writes_ms),
            );
        }
    }

    /// The key the current withdrawal's final seam chose, the delete key
    /// until one did: its jobs are [`Job::Deletes`] or [`Job::Clears`].
    pub fn withdraw_key(&self) -> WithdrawKey {
        self.presses.map_or(WithdrawKey::Delete, |p| p.key)
    }

    /// The attempt started the misrendered-echo withdrawal
    /// ([`Self::allow_gap_withdrawal`]), whatever it ended in.
    pub fn gap_withdrew(&self) -> bool {
        self.gap
    }

    /// Which step and check last unresolved the attempt (diagnosis only).
    pub fn unresolved_at(&self) -> Option<&str> {
        self.unresolved_at.as_deref()
    }

    /// Diagnosis only: note `what` with the length of the composer text as
    /// last read (in characters; none when no composer was on screen).
    fn note_unresolved(&mut self, what: &str) {
        let length = self.frame.composer.as_ref().map(|c| c.text.chars().count());
        self.unresolved_at = Some(format!("{what}; composer text length {length:?}"));
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
            // Refused at the writer: nothing of this job was written. The
            // line's first stage leaves the attempt with nothing written; a
            // later job of a line already in the pane can't be finished.
            (Phase::LineJob(0), Event::JobNotStarted) if !self.wrote => {
                self.nothing_written(NotTyped::OffGround, now)
            }
            (Phase::LineJob(_) | Phase::CrJob, Event::JobNotStarted) => {
                self.unresolve(Cause::WriteStalled, seam, now)
            }
            (Phase::DeleteJob, Event::JobNotStarted) => {
                self.suspend_and_pause(Cause::WithdrawalStalled, now)
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
                self.deletes_ended(accepted, ok, now)
            }
            (Phase::DeleteJob, Event::JobStalled) => {
                self.suspend_and_pause(Cause::WithdrawalStalled, now)
            }
            (Phase::DeletePace(at), _) => self.next_deletes(at, seam, now),
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
            (Event::JobNotStarted, Owned::Queued(_)) => {
                self.job = Owned::Idle;
                self.claim = None;
            }
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
        // A job its start condition refused was never written, as one taken
        // back wasn't: the writer accounted for it.
        if claim.withdraw() || claim.state() == ClaimState::Refused {
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
            _ => {
                self.note_unresolved("try_line: later stage's seam");
                self.unresolve(Cause::EchoMismatch, seam, now)
            }
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
            self.note_unresolved("head_echo: wait ended");
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
            self.note_unresolved("cancel_after_head: admission lost");
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
            self.note_unresolved("echo: wait ended");
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
                    self.note_unresolved("pre_cr: the frame does not hold exactly the line");
                    return self.unresolve(Cause::EchoMismatch, seam, now);
                }
                self.enqueue(JobKind::Cr, Job::Cr, Phase::CrJob, now)
            }
            SeamVerdict::Layout => self.edge(now),
            SeamVerdict::Mode(_) => self.unresolve(Cause::ModeChanged, seam, now),
            SeamVerdict::Closed => self.retired(now),
            SeamVerdict::Expired => self.end(End::Expire, now),
            SeamVerdict::Human => {
                self.note_unresolved("pre_cr: seam watermark moved");
                self.unresolve(Cause::EchoMismatch, seam, now)
            }
            SeamVerdict::JobMark => {
                self.note_unresolved("pre_cr: seam job mark moved");
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
                    self.note_unresolved("cancel_after_line: admission lost");
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

    /// PRD #211, the misrendered echo's recogniser: the composer shows the
    /// whole line the deck typed with only spaces inserted
    /// ([`whitespace_gap`]), its token head drawn intact, cursor after it,
    /// nothing highlighted or hidden, no placeholder, no dialog, the layout
    /// epoch unchanged, and no Enter sent. Returns the deletes to send: one
    /// per character the deck typed, as the PTY accepted every one of them
    /// and the screen draws them all, with only the gap added. That is the
    /// rendered screen's word for what the agent's buffer holds, not a read
    /// of the buffer (the same-uid trust boundary of
    /// `docs/develop/hook-provenance.md`). A person's keys since
    /// admission are refused before this is read (the seam's job mark), and
    /// any other difference, a missing or foreign character included, is no
    /// such gap.
    fn gap_view(&self) -> Option<usize> {
        if !self.gap_allowed
            || self.after_cr
            || self.accepted != self.line.chars().count()
            || !self.frame.clean
            || self.frame.epoch != self.epoch
            || self.frame.dialog.is_some()
        {
            return None;
        }
        let composer = self.frame.composer.as_ref()?;
        if composer.selection || composer.placeholder || composer.hidden || !composer.cursor_at_end
        {
            return None;
        }
        let head: String = self
            .line
            .chars()
            .take(crate::doorbell_screen::TOKEN_HEAD_CHARS)
            .collect();
        if !composer.text.starts_with(&head) || !whitespace_gap(&composer.text, &self.line) {
            return None;
        }
        Some(self.line.chars().count())
    }

    /// The misrendered-echo withdrawal may start now: the profile has a
    /// delete key, the transaction owns no job on the writer and no Enter
    /// was ever queued. Its confirmation does not need a measured
    /// placeholder ([`Self::confirm_withdrawn`]).
    fn may_gap_withdraw(&self) -> bool {
        self.caps.delete_key && self.job == Owned::Idle && self.cr_ended_at.is_none()
    }

    /// Start the misrendered-echo withdrawal, when it applies to an echo
    /// mismatch on the current frame.
    fn try_gap_withdrawal(
        &mut self,
        cause: Cause,
        seam: &mut dyn Seam,
        now: Instant,
    ) -> Option<Vec<Action>> {
        if cause != Cause::EchoMismatch || self.withdrawal.is_some() || !self.may_gap_withdraw() {
            return None;
        }
        let deletes = self.gap_view()?;
        self.gap = true;
        self.withdrawal = Some((deletes, cause));
        self.enter(Phase::Withdrawing, now);
        Some(self.try_deletes(seam, now))
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
        if let Some(actions) = self.try_gap_withdrawal(cause, seam, now) {
            return actions;
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
                // A fresh frame, immediately before the deletes, read by the
                // recogniser that started the withdrawal.
                let view = if self.gap {
                    self.gap_view()
                } else {
                    self.strict()
                };
                if view != Some(deletes) {
                    return self.suspend_and_pause(cause, now);
                }
                // A withdrawal the deadline would cut short is not started:
                // it would leave part of the line in the box.
                let Some(presses) = self.choose_presses(deletes, now) else {
                    self.note_unresolved("try_deletes: no withdrawal fits the deadline");
                    // Nothing was withdrawn: the next ring may try again.
                    self.gap = false;
                    return self.suspend_and_pause(cause, now);
                };
                self.presses = Some(presses);
                self.deleted = 0;
                self.enqueue_deletes(now)
            }
            // The misrendered echo was recognised at the transaction's
            // epoch: a Clean frame at another one is a relayout the gap was
            // not read on, so the gap withdrawal is refused rather than
            // re-read there. A frame that is not Clean is waited out.
            SeamVerdict::Layout if self.gap && self.frame.clean => {
                self.note_unresolved("try_deletes: the layout moved before the gap withdrawal");
                self.suspend_and_pause(cause, now)
            }
            SeamVerdict::Layout => self.edge(now),
            SeamVerdict::Closed => self.retired(now),
            SeamVerdict::Expired => self.end(End::Expire, now),
            SeamVerdict::Mode(_) => self.suspend_and_pause(Cause::ModeChanged, now),
            SeamVerdict::Human | SeamVerdict::JobMark => self.suspend_and_pause(cause, now),
        }
    }

    /// The presses that withdraw the recognised `deletes` characters, the
    /// first job at `now`: the clear key's, when one is set
    /// ([`Self::clear_lines`]), the composer holds one logical line and
    /// they fit the deadline; else the deletes, paced as served
    /// ([`Self::pace_deletes`]), when they fit; else none.
    fn choose_presses(&self, deletes: usize, now: Instant) -> Option<Presses> {
        let one_line = self
            .frame
            .composer
            .as_ref()
            .is_some_and(|c| !c.text.contains('\n'));
        let clear = self
            .clear
            .filter(|_| one_line)
            .map(|(per_line, per_write, gap)| Presses {
                key: WithdrawKey::Clear,
                total: per_line,
                per_write,
                gap,
            });
        let (per_write, gap) = self.delete_pace.unwrap_or((deletes.max(1), Duration::ZERO));
        let delete = Presses {
            key: WithdrawKey::Delete,
            total: deletes,
            per_write,
            gap,
        };
        clear
            .into_iter()
            .chain(std::iter::once(delete))
            .find(|p| p.fit(now, self.deadline))
    }

    /// Queue the next of the withdrawal's presses that are left: all of them
    /// in one job, or, paced, the key's per-write share.
    fn enqueue_deletes(&mut self, now: Instant) -> Vec<Action> {
        let presses = self.presses.expect("a withdrawal chose its presses");
        let left = presses.total - self.deleted;
        self.delete_job = presses.per_write.min(left);
        let job = match presses.key {
            WithdrawKey::Delete => Job::Deletes(self.delete_job),
            WithdrawKey::Clear => Job::Clears(self.delete_job),
        };
        self.enqueue(JobKind::Deletes, job, Phase::DeleteJob, now)
    }

    /// A delete job ended with `accepted` of its deletes taken: on to the
    /// next, once the pace allows, or, all taken, to the confirmation.
    fn deletes_ended(&mut self, accepted: usize, ok: bool, now: Instant) -> Vec<Action> {
        let presses = self.presses.expect("a delete job has its presses");
        if !ok || accepted != self.delete_job {
            return self.suspend_and_pause(Cause::WithdrawalStalled, now);
        }
        self.deleted += accepted;
        if self.deleted == presses.total {
            self.enter(Phase::ConfirmWithdrawn, now);
            return self.confirm_withdrawn(now);
        }
        let at = now + presses.gap;
        self.enter(Phase::DeletePace(at), now);
        vec![Action::WakeAt(at.min(self.deadline))]
    }

    /// A paced withdrawal's next deletes, once their moment came: checked by
    /// the seam as every job is, but not by the recogniser that started the
    /// withdrawal, which reads the whole line the deletes are now removing.
    fn next_deletes(&mut self, at: Instant, seam: &mut dyn Seam, now: Instant) -> Vec<Action> {
        if now < at && now < self.deadline {
            return vec![Action::WakeAt(at.min(self.deadline))];
        }
        let (_, cause) = self.withdrawal.expect("a paced withdrawal has its deletes");
        match self.seam(seam, false, now) {
            SeamVerdict::Contended(at) => vec![Action::WakeAt(at)],
            SeamVerdict::Pass(_) => self.enqueue_deletes(now),
            SeamVerdict::Layout => vec![Action::WakeAt(self.next_wake(now))],
            SeamVerdict::Closed => self.retired(now),
            SeamVerdict::Expired => self.end(End::Expire, now),
            SeamVerdict::Mode(_) => self.suspend_and_pause(Cause::ModeChanged, now),
            SeamVerdict::Human | SeamVerdict::JobMark => self.suspend_and_pause(cause, now),
        }
    }

    /// `Withdrawn` needs the delete job done in full and then a Clean frame
    /// showing the placeholder. After a layout edge that is the next Clean
    /// frame, at the new epoch (plan §2.2).
    ///
    /// The misrendered-echo withdrawal of a profile with no measured
    /// placeholder (Antigravity's hint is not redrawn by every clear) is
    /// confirmed by a blank composer instead, cursor at its start, nothing
    /// hidden: it deleted one character per byte the agent took, or cleared
    /// the one logical line the agent holds, so anything a press the agent
    /// did not act on left would be the line's own start, `(` and its
    /// token, which the composer draws. A composer still holding text is
    /// never confirmed: the attempt ends unresolved at the deadline, the
    /// cell suspended.
    fn confirm_withdrawn(&mut self, now: Instant) -> Vec<Action> {
        if self.frame.clean {
            self.epoch = self.frame.epoch;
            let blank = self.gap
                && !self.caps.placeholder
                && self.frame.dialog.is_none()
                && self.frame.composer.as_ref().is_some_and(|c| {
                    !c.hidden && !c.selection && c.text.is_empty() && c.cursor_at_end
                });
            if self.frame.placeholder_empty() || blank {
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
        if let Some(actions) = self.try_gap_withdrawal(cause, seam, now) {
            return actions;
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

#[cfg(test)]
mod gap_withdrawal_tests {
    //! PRD #211: the misrendered-echo withdrawal ([`Txn::allow_gap_withdrawal`]),
    //! driven step by step against a seam that shows one frame.

    use super::*;
    use crate::doorbell_generation::GenerationDoorbell;

    const ID: &str = "m-k3xzq2ab-0f3c9a71d2e4b850";

    /// The seam: the frame it shows (`.0`), and the rest of what its next
    /// read reports (`.1`), which a test can move between the read that
    /// recognised the echo and the final read before a job.
    struct OneFrame(Frame, SeamState);

    /// What a [`OneFrame`] read reports besides its frame.
    struct SeamState {
        job_mark: u64,
        mode: Result<(), &'static str>,
    }

    impl OneFrame {
        fn new(frame: Frame) -> Self {
            Self(
                frame,
                SeamState {
                    job_mark: 0,
                    mode: Ok(()),
                },
            )
        }
    }

    impl Seam for OneFrame {
        fn read(&mut self) -> Option<SeamRead> {
            Some(SeamRead {
                watermark: 0,
                job_mark: self.1.job_mark,
                frame: self.0.clone(),
                mode: self.1.mode,
                open: true,
            })
        }
    }

    fn composer(text: &str, placeholder: bool) -> Frame {
        Frame {
            epoch: 0,
            clean: true,
            composer: Some(ComposerView {
                text: text.to_string(),
                cursor_at_end: true,
                placeholder,
                selection: false,
                hidden: false,
            }),
            ..Frame::default()
        }
    }

    fn caps(placeholder: bool) -> Caps {
        Caps {
            delete_key: true,
            placeholder,
            hook_route: true,
            transcript_route: false,
            staged_head: None,
        }
    }

    fn generation() -> GenerationDoorbell {
        GenerationDoorbell::new(
            false,
            #[cfg(unix)]
            None,
        )
    }

    /// Admit one attempt on `bell`, allowing the gap withdrawal as the driver
    /// does: only when the last ring that wrote did not use it.
    fn admit_on(bell: &GenerationDoorbell, caps: Caps, now: Instant) -> Txn<'_> {
        admit_naming(bell, caps, now, "dot-agent-deck")
    }

    /// [`admit_on`], the line naming `command` for `inbox fetch`.
    fn admit_naming<'b>(
        bell: &'b GenerationDoorbell,
        caps: Caps,
        now: Instant,
        command: &str,
    ) -> Txn<'b> {
        let admission = Admission {
            activation: Ok(()),
            mode: Ok(()),
            ledger_idle: true,
            watermark: 0,
            job_mark: 0,
            frame: composer("", true),
            nudge_in_flight: false,
            settling: false,
            submit_route: true,
            still_admitted: true,
            command: command.to_string(),
        };
        let mut draws = [3u8, 1, 4, 1, 5, 9, 2, 6].into_iter().cycle();
        let mut txn = admit(
            &admission,
            bell.counter(),
            bell.ring(),
            bell.cell(),
            || draws.next().unwrap(),
            &MessageId::parse(ID).unwrap(),
            caps,
            now,
        )
        .expect("admitted");
        if bell.gap_withdrawal_allowed() {
            txn.allow_gap_withdrawal();
        }
        txn
    }

    /// `line` with seven spaces inserted after its 35th character, as `agy`
    /// drew it.
    fn gapped(line: &str) -> String {
        let at = line.char_indices().nth(35).unwrap().0;
        format!("{}       {}", &line[..at], &line[at..])
    }

    fn enqueued(actions: &[Action]) -> Vec<Job> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Enqueue(job, _) => Some(job.clone()),
                _ => None,
            })
            .collect()
    }

    fn released(actions: &[Action]) -> Option<Outcome> {
        actions.iter().find_map(|a| match a {
            Action::Release(outcome) => Some(*outcome),
            _ => None,
        })
    }

    /// Type the whole line, then show `shown` once the echo wait is over.
    /// Returns what that step asked for and every job queued.
    fn type_then_show(
        txn: &mut Txn<'_>,
        seam: &mut OneFrame,
        shown: Frame,
        t0: Instant,
    ) -> (Vec<Action>, Vec<Job>) {
        let mut jobs = enqueued(&txn.start(seam, t0));
        assert_eq!(jobs, vec![Job::Line(txn.line().to_string())]);
        txn.on(Event::JobStarted, seam, t0);
        let n = txn.line().len();
        txn.on(
            Event::JobEnded {
                accepted: n,
                ok: true,
            },
            seam,
            t0,
        );
        seam.0 = shown.clone();
        let actions = txn.on(Event::Frame(shown), seam, t0 + ECHO_WAIT);
        jobs.extend(enqueued(&actions));
        (actions, jobs)
    }

    /// Run the transaction to its end on `seam`'s frame, reporting each queued
    /// job as written in full; returns the outcome and the jobs.
    fn run_out(
        txn: &mut Txn<'_>,
        seam: &mut OneFrame,
        actions: Vec<Action>,
        now: Instant,
    ) -> (Outcome, Vec<Job>) {
        let after = withdrawn_frame(txn);
        run_out_to(txn, seam, actions, now, after)
    }

    /// [`run_out`], with `after` the frame the agent draws once it took
    /// every delete.
    fn run_out_to(
        txn: &mut Txn<'_>,
        seam: &mut OneFrame,
        mut actions: Vec<Action>,
        mut now: Instant,
        after: Frame,
    ) -> (Outcome, Vec<Job>) {
        let mut jobs = Vec::new();
        loop {
            if let Some(outcome) = released(&actions) {
                return (outcome, jobs);
            }
            let queued = enqueued(&actions);
            jobs.extend(queued.iter().cloned());
            now += Duration::from_millis(50);
            actions = match queued.first() {
                Some(Job::Deletes(n) | Job::Clears(n)) => {
                    txn.on(Event::JobStarted, seam, now);
                    // The agent took every press: its composer is empty.
                    seam.0 = after.clone();
                    txn.on(
                        Event::JobEnded {
                            accepted: *n,
                            ok: true,
                        },
                        seam,
                        now,
                    )
                }
                Some(job) => panic!("unexpected job {job:?}"),
                None => txn.on(Event::Frame(seam.0.clone()), seam, now),
            };
        }
    }

    /// The composer once the deletes went in: its placeholder for a profile
    /// with one, else a blank box.
    fn withdrawn_frame(txn: &Txn<'_>) -> Frame {
        composer("", txn.caps.placeholder)
    }

    #[test]
    fn whitespace_gap_accepts_only_inserted_spaces_inside_the_line() {
        let typed = "(abcdef) dot-agent-deck: message m-1";
        assert!(whitespace_gap(&gapped(typed), typed));
        assert!(whitespace_gap(
            "(abcdef)  dot-agent-deck: message m-1",
            typed
        ));
        assert!(!whitespace_gap(typed, typed), "the exact echo is no gap");
        assert!(!whitespace_gap(&format!(" {typed}"), typed), "before it");
        assert!(!whitespace_gap(&format!("{typed} "), typed), "after it");
        assert!(
            !whitespace_gap(&typed.replace("m-1", "m1"), typed),
            "missing"
        );
        assert!(!whitespace_gap(
            &typed.replace("message", "messagex"),
            typed
        ));
        assert!(!whitespace_gap(&typed.replace(' ', "\n"), typed), "newline");
        assert!(!whitespace_gap(&typed.replace(": ", ":\t "), typed), "tab");
        assert!(!whitespace_gap("", typed));
    }

    #[test]
    fn a_whitespace_gap_is_withdrawn_by_the_typed_count_and_the_next_ring_submits() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(true), t0);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, mut jobs) =
            type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(
            enqueued(&actions),
            vec![Job::Deletes(line.chars().count())],
            "one delete per character the deck typed, at once"
        );
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        jobs.extend(more);
        assert_eq!(outcome, Outcome::Withdrawn);
        assert!(txn.gap_withdrew());
        assert!(
            !jobs.contains(&Job::Cr),
            "a misrendered echo is never submitted"
        );
        assert!(matches!(
            bell.cell().load(),
            Latch::Resolved(_, Resolution::Withdrawn)
        ));
        bell.ring_wrote(txn.gap_withdrew());

        // The retry: the agent now draws the line exactly, and it is
        // submitted once.
        let t1 = t0 + TXN_MAX;
        let mut retry = admit_on(&bell, caps(true), t1);
        let line = retry.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (_, jobs) = type_then_show(&mut retry, &mut seam, composer(&line, false), t1);
        assert_eq!(
            jobs,
            vec![Job::Line(line), Job::Cr],
            "typed once, then Enter"
        );
        assert!(!retry.gap_withdrew());
        bell.ring_wrote(retry.gap_withdrew());
        assert!(
            bell.gap_withdrawal_allowed(),
            "a ring without it re-arms it"
        );
    }

    #[test]
    fn without_a_measured_placeholder_a_blank_composer_confirms_the_gap_withdrawal() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(false), t0);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, _) = type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(enqueued(&actions), vec![Job::Deletes(line.chars().count())]);
        let (outcome, _) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        assert_eq!(outcome, Outcome::Withdrawn);
    }

    /// PRD #211: a paced withdrawal ([`Txn::pace_deletes`]) sends one delete
    /// per job, the next only once the pace allows and its seam passes, and
    /// is confirmed once every delete is in. Here at a 5 ms pace, which the
    /// deadline leaves room for.
    #[test]
    fn a_paced_gap_withdrawal_sends_one_delete_per_job_at_its_pace() {
        const PACE: Duration = Duration::from_millis(5);
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(false), t0);
        txn.pace_deletes(1, PACE);
        let line = txn.line().to_string();
        let count = line.chars().count();
        let mut seam = OneFrame::new(composer("", true));
        let (mut actions, _) =
            type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        let mut now = t0 + ECHO_WAIT;
        let mut deleted = 0;
        loop {
            assert_eq!(
                enqueued(&actions),
                vec![Job::Deletes(1)],
                "one delete per job ({deleted} in)"
            );
            txn.on(Event::JobStarted, &mut seam, now);
            deleted += 1;
            let left: String = line.chars().take(count - deleted).collect();
            seam.0 = composer(&left, false);
            actions = txn.on(
                Event::JobEnded {
                    accepted: 1,
                    ok: true,
                },
                &mut seam,
                now,
            );
            if deleted == count {
                break;
            }
            assert_eq!(enqueued(&actions), vec![], "the next waits for its pace");
            let early = txn.on(Event::Frame(seam.0.clone()), &mut seam, now + PACE / 2);
            assert_eq!(enqueued(&early), vec![], "not before its pace");
            now += PACE;
            actions = txn.on(Event::Frame(seam.0.clone()), &mut seam, now);
        }
        assert_eq!(released(&actions), None);
        seam.0 = composer("", false);
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, now);
        assert_eq!(outcome, Outcome::Withdrawn);
        assert!(more.is_empty(), "nothing after the last delete: {more:?}");
    }

    /// PRD #211: the agent drops a delete of a paced withdrawal (its job
    /// ends with nothing taken): the withdrawal stops there, unresolved,
    /// and nothing more is sent.
    #[test]
    fn a_paced_delete_the_agent_did_not_take_stops_the_withdrawal() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(false), t0);
        txn.pace_deletes(1, Duration::from_millis(5));
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, _) = type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(enqueued(&actions), vec![Job::Deletes(1)]);
        let now = t0 + ECHO_WAIT;
        txn.on(Event::JobStarted, &mut seam, now);
        let actions = txn.on(
            Event::JobEnded {
                accepted: 0,
                ok: true,
            },
            &mut seam,
            now,
        );
        assert_eq!(enqueued(&actions), vec![]);
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, now);
        assert_eq!(outcome, Outcome::Unresolved(Cause::WithdrawalStalled));
        assert!(more.is_empty(), "{more:?}");
    }

    /// PRD #211: at Antigravity's served pace, 20 ms per delete, the deck's
    /// whole line takes longer to delete than the deadline leaves once the
    /// echo wait is over, so the withdrawal is not started: no delete is
    /// sent, the line stays whole, and the attempt ends unresolved.
    #[test]
    fn a_paced_withdrawal_the_deadline_cannot_fit_is_not_started() {
        let pace = crate::agent_registry::spec(&crate::event::AgentType::Antigravity)
            .prompt_keys
            .as_ref()
            .map(|keys| &keys.delete_char)
            .expect("Antigravity's keys");
        let (per_write, gap) = (
            pace.max_presses_per_write.expect("paced") as usize,
            Duration::from_millis(u64::from(pace.pause_between_writes_ms.unwrap())),
        );
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(false), t0);
        txn.pace_deletes(per_write, gap);
        let line = txn.line().to_string();
        let writes = u32::try_from(line.chars().count().div_ceil(per_write)).unwrap();
        assert!(
            ECHO_WAIT + gap * (writes - 1) >= TXN_MAX,
            "the line's paced deletes would fit: the case this test is about is gone"
        );
        let mut seam = OneFrame::new(composer("", true));
        let (actions, jobs) =
            type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(jobs, vec![Job::Line(line)], "no delete was sent");
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        assert_eq!(outcome, Outcome::Unresolved(Cause::EchoMismatch));
        assert!(more.is_empty(), "{more:?}");
    }

    /// PRD #211: what real `agy` 1.5.9 drew after paced deletes of the line
    /// (`doorbell_screen`'s `PACED_DELETES` capture), read by its own profile,
    /// confirms the gap withdrawal: Antigravity has no measured placeholder,
    /// and the composer it leaves is blank with the cursor at its start.
    #[test]
    fn real_agy_after_paced_deletes_confirms_the_gap_withdrawal() {
        use crate::doorbell_screen::{ANTIGRAVITY_PROFILE, Grid, frame};
        let grid = Grid::parse(
            crate::doorbell_screen::antigravity_tests::PACED_DELETES,
            40,
            143,
        );
        let after = frame(&ANTIGRAVITY_PROFILE, &grid, 0, true, None);
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(false), t0);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, _) = type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(enqueued(&actions), vec![Job::Deletes(line.chars().count())]);
        let (outcome, _) = run_out_to(&mut txn, &mut seam, actions, t0 + ECHO_WAIT, after);
        assert_eq!(outcome, Outcome::Withdrawn);
    }

    #[test]
    fn a_foreign_character_stays_suspended_and_nothing_is_deleted() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(true), t0);
        let line = txn.line().to_string();
        let foreign = gapped(&line).replacen("       ", "   x   ", 1);
        let mut seam = OneFrame::new(composer("", true));
        let (actions, mut jobs) =
            type_then_show(&mut txn, &mut seam, composer(&foreign, false), t0);
        assert_eq!(txn.phase(), Phase::Pause(Cause::EchoMismatch));
        assert!(bell.cell().load().suspended());
        // The pause runs on to the deadline, deleting nothing.
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        jobs.extend(more);
        assert_eq!(outcome, Outcome::Unresolved(Cause::EchoMismatch));
        assert!(!txn.gap_withdrew());
        assert_eq!(jobs, vec![Job::Line(line)], "no delete, no Enter");
    }

    #[test]
    fn a_retry_that_mismatches_again_stays_suspended() {
        let bell = generation();
        let t0 = Instant::now();
        let mut first = admit_on(&bell, caps(true), t0);
        let line = first.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, _) =
            type_then_show(&mut first, &mut seam, composer(&gapped(&line), false), t0);
        let (outcome, _) = run_out(&mut first, &mut seam, actions, t0 + ECHO_WAIT);
        assert_eq!(outcome, Outcome::Withdrawn);
        bell.ring_wrote(first.gap_withdrew());
        assert!(!bell.gap_withdrawal_allowed());

        let t1 = t0 + TXN_MAX;
        let mut retry = admit_on(&bell, caps(true), t1);
        let line = retry.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (_, jobs) = type_then_show(&mut retry, &mut seam, composer(&gapped(&line), false), t1);
        assert_eq!(retry.phase(), Phase::Pause(Cause::EchoMismatch));
        assert!(bell.cell().load().suspended());
        assert_eq!(jobs, vec![Job::Line(line)], "no second withdrawal");
        let outcome = released(&retry.on(Event::Tick, &mut seam, retry.deadline()));
        assert_eq!(outcome, Some(Outcome::Unresolved(Cause::EchoMismatch)));
        assert!(!retry.gap_withdrew());
    }

    /// Admit an attempt whose line is `chars` characters long, its command
    /// word a deep install path, with Antigravity's served keys as the
    /// driver serves them ([`Txn::serve_keys`]).
    fn admit_antigravity(bell: &GenerationDoorbell, now: Instant, chars: usize) -> Txn<'_> {
        let short = admit_naming(&generation(), caps(false), now, "d")
            .line()
            .chars()
            .count();
        let command = format!("/{}", "v".repeat(chars - short - 1) + "d");
        let mut txn = admit_naming(bell, caps(false), now, &command);
        assert_eq!(txn.line().chars().count(), chars);
        txn.serve_keys(
            crate::agent_registry::spec(&crate::event::AgentType::Antigravity)
                .prompt_keys
                .as_ref()
                .expect("Antigravity's keys"),
        );
        txn
    }

    /// The composer real `agy` 1.5.9 drew after one Ctrl+U of a wrapped
    /// doorbell line (`doorbell_screen`'s `CLEAR` capture), read by its own
    /// profile.
    fn real_agy_after_clear() -> Frame {
        use crate::doorbell_screen::{ANTIGRAVITY_PROFILE, Grid, frame};
        let grid = Grid::parse(crate::doorbell_screen::antigravity_tests::CLEAR, 40, 143);
        frame(&ANTIGRAVITY_PROFILE, &grid, 0, true, None)
    }

    /// PRD #211: with Antigravity's served keys, the misrendered echo of a
    /// 170-character line, whose deletes at the served 20 ms pace could not
    /// start before the deadline, is withdrawn with ONE Ctrl+U, in one job,
    /// and confirmed on the blank composer real `agy` drew after it.
    /// Nothing is deleted and nothing is submitted.
    #[test]
    fn a_clear_withdrawal_of_a_long_gap_line_fits_the_deadline_on_the_real_frame() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_antigravity(&bell, t0, 170);
        let line = txn.line().to_string();
        let (_, gap) = txn.delete_pace.expect("the served deletes are paced");
        let writes = u32::try_from(line.chars().count()).unwrap();
        assert!(
            ECHO_WAIT + gap * (writes - 1) >= TXN_MAX,
            "the paced deletes would fit: the case this test is about is gone"
        );
        let mut seam = OneFrame::new(composer("", true));
        let (actions, jobs) =
            type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(enqueued(&actions), vec![Job::Clears(1)], "one press");
        assert_eq!(txn.withdraw_key(), WithdrawKey::Clear, "of the clear key");
        let (outcome, more) = run_out_to(
            &mut txn,
            &mut seam,
            actions,
            t0 + ECHO_WAIT,
            real_agy_after_clear(),
        );
        assert_eq!(outcome, Outcome::Withdrawn);
        assert!(txn.gap_withdrew());
        assert_eq!(jobs, vec![Job::Line(line), Job::Clears(1)]);
        assert_eq!(more, vec![Job::Clears(1)], "one clear, then no Enter");
        assert!(matches!(
            bell.cell().load(),
            Latch::Resolved(_, Resolution::Withdrawn)
        ));
    }

    /// PRD #211: the agent leaves text in the composer after the measured
    /// press (here the line's second row): the withdrawal is not confirmed,
    /// no second press or delete is sent, and at the deadline the attempt
    /// ends unresolved with the cell suspended.
    #[test]
    fn a_leftover_after_the_clear_is_not_confirmed() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_antigravity(&bell, t0, 170);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, jobs) =
            type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(enqueued(&actions), vec![Job::Clears(1)]);
        let leftover: String = line.chars().skip(140).collect();
        let (outcome, more) = run_out_to(
            &mut txn,
            &mut seam,
            actions,
            t0 + ECHO_WAIT,
            composer(&leftover, false),
        );
        assert_eq!(outcome, Outcome::Unresolved(Cause::Deadline));
        assert!(bell.cell().load().suspended());
        assert_eq!(jobs, vec![Job::Line(line), Job::Clears(1)]);
        assert_eq!(more, vec![Job::Clears(1)], "nothing after the one press");
    }

    /// PRD #211: with the clear key served, a composer holding anything but
    /// the deck's own line (a foreign character in the gap) still gets no
    /// press of any key and stays suspended.
    #[test]
    fn with_the_clear_key_foreign_text_still_gets_nothing() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_antigravity(&bell, t0, 170);
        let line = txn.line().to_string();
        let foreign = gapped(&line).replacen("       ", "   x   ", 1);
        let mut seam = OneFrame::new(composer("", true));
        let (actions, mut jobs) =
            type_then_show(&mut txn, &mut seam, composer(&foreign, false), t0);
        assert_eq!(txn.phase(), Phase::Pause(Cause::EchoMismatch));
        assert!(bell.cell().load().suspended());
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        jobs.extend(more);
        assert_eq!(outcome, Outcome::Unresolved(Cause::EchoMismatch));
        assert!(!txn.gap_withdrew());
        assert_eq!(jobs, vec![Job::Line(line)], "no press, no Enter");
    }

    /// PRD #211: a clear the deadline cannot fit (here paced so slowly that
    /// its two presses cannot both start) falls back to the deletes when
    /// those fit; when neither fits, nothing is sent.
    #[test]
    fn a_clear_that_cannot_fit_falls_back_to_deletes_that_do() {
        let slow = Duration::from_secs(5);
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(false), t0);
        txn.clear_lines(2, Some(1), slow);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, _) = type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(enqueued(&actions), vec![Job::Deletes(line.chars().count())]);
        assert_eq!(txn.withdraw_key(), WithdrawKey::Delete);
        let (outcome, _) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        assert_eq!(outcome, Outcome::Withdrawn);

        let bell = generation();
        let mut txn = admit_on(&bell, caps(false), t0);
        txn.clear_lines(2, Some(1), slow);
        txn.pace_deletes(1, slow);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, jobs) =
            type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(jobs, vec![Job::Line(line)], "no press");
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        assert_eq!(outcome, Outcome::Unresolved(Cause::EchoMismatch));
        assert!(more.is_empty(), "{more:?}");
    }

    /// The misrendered echo of a 170-character line on Antigravity's
    /// served keys (so a withdrawal would be one Ctrl+U) is recognised on
    /// the frame event, but `change` moves what the seam's final read
    /// before the clear reports; the attempt then runs to its end on what
    /// the seam shows. Returns the outcome, every job queued, and whether
    /// the cell was left suspended.
    fn final_read_changed(change: impl FnOnce(&mut OneFrame, &str)) -> (Outcome, Vec<Job>, bool) {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_antigravity(&bell, t0, 170);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let mut jobs = enqueued(&txn.start(&mut seam, t0));
        txn.on(Event::JobStarted, &mut seam, t0);
        txn.on(
            Event::JobEnded {
                accepted: line.len(),
                ok: true,
            },
            &mut seam,
            t0,
        );
        let shown = composer(&gapped(&line), false);
        seam.0 = shown.clone();
        change(&mut seam, &line);
        let actions = txn.on(Event::Frame(shown), &mut seam, t0 + ECHO_WAIT);
        // `run_out` reports the jobs `actions` queued as well.
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, t0 + ECHO_WAIT);
        jobs.extend(more);
        let suspended = bell.cell().load().suspended();
        (outcome, jobs, suspended)
    }

    /// Assert what [`final_read_changed`] reported is a refused clear: the
    /// line and nothing after it, no Ctrl+U, delete or Enter, the attempt
    /// unresolved by its deadline and the cell suspended.
    fn assert_refused(what: &str, (outcome, jobs, suspended): (Outcome, Vec<Job>, bool)) {
        assert!(
            matches!(jobs.as_slice(), [Job::Line(_)]),
            "{what}: no press and no Enter after the line: {jobs:?}"
        );
        assert!(
            matches!(outcome, Outcome::Unresolved(_)),
            "{what}: released unresolved by the deadline: {outcome:?}"
        );
        assert!(suspended, "{what}: the cell is suspended");
    }

    /// PRD #211 (review: the final seam of the recovery): the echo was
    /// recognised as the deck's line misrendered, and then, on the seam's
    /// final read before the Ctrl+U, a person's job is on the writer (J0
    /// moved), the composer's text no longer satisfies the gap recogniser
    /// or its cursor is no longer at its end, the input mode is no longer
    /// usable, or the layout moved to a new epoch. Each one refuses
    /// the clear: nothing is pressed, nothing is submitted, and the attempt
    /// ends unresolved at its deadline with the cell suspended.
    #[test]
    fn a_change_on_the_final_read_refuses_the_clear() {
        assert_refused(
            "job mark moved",
            final_read_changed(|seam, _| seam.1.job_mark = 1),
        );
        assert_refused(
            "composer text changed",
            final_read_changed(|seam, line| {
                seam.0 = composer(&format!("{}x", gapped(line)), false);
            }),
        );
        assert_refused(
            "cursor moved",
            final_read_changed(|seam, _| {
                seam.0.composer.as_mut().unwrap().cursor_at_end = false;
            }),
        );
        assert_refused(
            "mode changed",
            final_read_changed(|seam, _| seam.1.mode = Err("the agent left raw mode")),
        );
        assert_refused(
            "layout moved",
            final_read_changed(|seam, _| seam.0.epoch = 1),
        );
        let (outcome, jobs, _) = final_read_changed(|_, _| {});
        assert_eq!(outcome, Outcome::Withdrawn, "unchanged, it clears");
        assert!(matches!(jobs.as_slice(), [Job::Line(_), Job::Clears(1)]));
    }

    /// PRD #211 (review: the final seam of the recovery): a paced gap
    /// withdrawal's first delete went in, and a person's job reached the
    /// writer before the next (J0 moved): the next delete's seam refuses
    /// it, no delete or Enter follows, and the attempt ends unresolved at
    /// its deadline with the cell suspended.
    #[test]
    fn a_paced_withdrawal_stops_when_the_job_mark_moves_between_deletes() {
        const PACE: Duration = Duration::from_millis(5);
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(false), t0);
        txn.pace_deletes(1, PACE);
        let line = txn.line().to_string();
        let mut seam = OneFrame::new(composer("", true));
        let (actions, mut jobs) =
            type_then_show(&mut txn, &mut seam, composer(&gapped(&line), false), t0);
        assert_eq!(enqueued(&actions), vec![Job::Deletes(1)]);
        let now = t0 + ECHO_WAIT;
        txn.on(Event::JobStarted, &mut seam, now);
        let left: String = line.chars().take(line.chars().count() - 1).collect();
        seam.0 = composer(&left, false);
        seam.1.job_mark = 1;
        let actions = txn.on(
            Event::JobEnded {
                accepted: 1,
                ok: true,
            },
            &mut seam,
            now,
        );
        assert_eq!(enqueued(&actions), vec![], "the next waits for its pace");
        let actions = txn.on(Event::Frame(seam.0.clone()), &mut seam, now + PACE);
        let (outcome, more) = run_out(&mut txn, &mut seam, actions, now + PACE);
        jobs.extend(more);
        assert_eq!(
            jobs,
            vec![Job::Line(line), Job::Deletes(1)],
            "no delete and no Enter after the job mark moved"
        );
        assert!(matches!(outcome, Outcome::Unresolved(_)), "{outcome:?}");
        assert!(bell.cell().load().suspended());
    }

    /// PRD #211: a strict withdrawal of a composer holding exactly the
    /// deck's line, with the clear key served, presses it once too; a
    /// composer that kept a newline is two logical lines, a press count not
    /// measured, and is deleted character by character.
    #[test]
    fn a_strict_withdrawal_clears_one_line_and_deletes_a_kept_newline() {
        let bell = generation();
        let t0 = Instant::now();
        let mut txn = admit_on(&bell, caps(true), t0);
        txn.clear_lines(1, None, Duration::ZERO);
        let line = txn.line().to_string();
        txn.accepted = line.chars().count();
        txn.frame = composer(&line, false);
        let mut seam = OneFrame::new(composer(&line, false));
        let actions = txn.unresolve(Cause::EchoMismatch, &mut seam, t0);
        assert_eq!(enqueued(&actions), vec![Job::Clears(1)]);
        assert_eq!(txn.withdraw_key(), WithdrawKey::Clear);

        let bell = generation();
        let mut txn = admit_on(&bell, caps(true), t0);
        txn.clear_lines(1, None, Duration::ZERO);
        let line = txn.line().to_string();
        txn.accepted = line.chars().count();
        txn.after_cr = true;
        let kept = format!("{line}\n");
        txn.frame = composer(&kept, false);
        let mut seam = OneFrame::new(composer(&kept, false));
        let actions = txn.unresolve(Cause::NoSubmitEvidence, &mut seam, t0);
        assert_eq!(
            enqueued(&actions),
            vec![Job::Deletes(line.chars().count() + 1)]
        );
        assert_eq!(txn.withdraw_key(), WithdrawKey::Delete);
    }
}
