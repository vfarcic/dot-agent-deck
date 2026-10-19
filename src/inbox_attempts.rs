//! PRD #1666 design v3.1 §2, §4, §6, §7, §9: the pure state model of a deck
//! message's delivery attempts — stages, the one finalizer, the nine
//! transition rows, receiver slots and incarnations, possession, the attempt
//! and evidence budgets, and task-bound work-done.
//!
//! The hub's receipt engine ([`crate::inbox_engine`]) drives the OpenCode
//! receivers from this model; the other receivers keep the hub's ack-wait path
//! until their own §13 slice moves them onto it. Time is passed in as an
//! `Instant` on every operation, so every deadline is exercised without a
//! sleep.
//!
//! # The nine rows
//!
//! Every row runs under the caller's lock and ends in [`AttemptModel::finalize`]
//! for the messages it touched. `finalize` changes a message's state, with two
//! named exceptions: T5 Retry moves `Uncertain` to `Attempting` with a manual
//! token (the user's decision, not a rule), and T9 exit makes every live
//! message `Undeliverable(exited)` (the generation is gone, so no rule runs
//! after it).
//!
//! | # | Event | Method |
//! |---|---|---|
//! | T1 | a hand-out (`Queued`, or a revalidated repeat token) | [`AttemptModel::issue`] |
//! | T2 | a stage record for the current assignment | [`AttemptModel::stage`] |
//! | T3 | a stage record for an earlier assignment | [`AttemptModel::stage`] |
//! | T4 | lapse (no fresh possession, and a spool check found no receipt) | [`AttemptModel::tick_cleared`] |
//! | T5 | user Retry | [`AttemptModel::retry`] |
//! | T6 | user Cancel | [`AttemptModel::cancel`] |
//! | T7 | a revoke answer (`revoked_ack` / `already_handed`) | [`AttemptModel::stage`] |
//! | T8 | registration, resume answer or its silence, a closed stream nobody replaced | [`AttemptModel::register`], [`AttemptModel::resume_answer`], [`AttemptModel::tick_cleared`] |
//! | T9 | generation exit | [`AttemptModel::exit`] |

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Hand-outs one message may have, across every issuance path (§7).
pub const MAX_ATTEMPTS: u32 = 6;
/// Automatic marked repeats one message may have, within [`MAX_ATTEMPTS`] (§6).
pub const MAX_AUTO_REPEATS: u32 = 2;
/// Spool files one attempt reserves before its content is handed out (§7):
/// two caps, the `Handed` and `Committed` records and a revoke marker.
pub const EVIDENCE_PER_ATTEMPT: usize = 5;
/// Evidence entries (spool files) one agent generation may hold (§7).
pub const DEFAULT_EVIDENCE_LIMIT: usize = 1024;
/// How long possession without a commit lasts before the one held_long
/// attention notice (§6.3).
pub const HELD_LONG_AFTER: Duration = Duration::from_secs(60 * 60);
/// PRD #1666 G3: how long a deck message handed to a Pi agent may go with no
/// turn starting, the agent idle all that time, before both clients say it
/// may not have reached the agent. Attention only: nothing is sent again.
pub const NOT_STARTED_AFTER: Duration = Duration::from_secs(3 * 60);
/// Keepalives in one `POSSESS_TTL` (§4).
pub const KEEPALIVES_PER_POSSESS_TTL: u32 = 3;
/// `POSSESS_TTL`s a message may wait unpossessed for something that would
/// let it go on — a spool check that can tell, or a receiver to take its
/// scheduled repeat — before it is `Uncertain` and the user decides (§3, §6).
pub const WAIT_TTLS_BEFORE_UNCERTAIN: u32 = 2;
/// `POSSESS_TTL`s a stream incarnation whose connection closed stays its
/// slot's, for its context to reconnect and resume what it holds.
pub const CLOSED_RETIRE_TTLS: u32 = 3;

/// The model's bounds and clocks.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_attempts: u32,
    pub max_auto_repeats: u32,
    pub max_deliver: u32,
    pub evidence_limit: usize,
    /// No snapshot for this long lapses a stream incarnation's attempts.
    pub possess_ttl: Duration,
    /// A resume answer's window, and a once-waiter's idle grace.
    pub ack_wait: Duration,
    pub held_long_after: Duration,
    /// PRD #1666 G3: [`NOT_STARTED_AFTER`], or its override.
    pub not_started_after: Duration,
}

impl Limits {
    /// How long a message waits, unpossessed, for a spool check that can
    /// tell or for a receiver to take its scheduled repeat, before it is
    /// `Uncertain`.
    pub fn uncertain_after(&self) -> Duration {
        self.possess_ttl * WAIT_TTLS_BEFORE_UNCERTAIN
    }

    /// How long a stream incarnation whose connection closed stays its
    /// slot's, waiting for its context to reconnect and resume what it
    /// holds, before it is retired with no successor.
    pub fn closed_retire_after(&self) -> Duration {
        self.possess_ttl * CLOSED_RETIRE_TTLS
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_attempts: MAX_ATTEMPTS,
            max_auto_repeats: MAX_AUTO_REPEATS,
            max_deliver: crate::inbox::DEFAULT_MAX_DELIVER,
            evidence_limit: DEFAULT_EVIDENCE_LIMIT,
            possess_ttl: crate::inbox_hub::DEFAULT_KEEPALIVE * KEEPALIVES_PER_POSSESS_TTL,
            ack_wait: crate::inbox::DEFAULT_ACK_WAIT,
            held_long_after: HELD_LONG_AFTER,
            not_started_after: NOT_STARTED_AFTER,
        }
    }
}

/// PRD #1666 G3: what the deck last saw of the generation's turns, from its
/// own hook reports: whether a turn is under way, and since when it has been
/// idle. `idle_since` moves only when a turn under way ends, so a repeated
/// Idle, a rebind or a keepalive cannot postpone the not-started window.
/// The default, idle and never seen busy, is a generation that has reported
/// no turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnView {
    pub busy: bool,
    pub idle_since: Option<Instant>,
}

/// A daemon-minted receiver incarnation (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rx(pub u64);

/// A stable receiver slot, distinct from [`Rx`] and from the binary or
/// integration pin (§4, auditor C4). A registration retires only the previous
/// incarnation of its own slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    OpencodeServer,
    OpencodeTui,
    Pi,
    OnceWaiter,
    Hook,
}

/// An attempt's stage (§2). `Committed`, `NotHanded`, `HandedInert` and
/// `Fenced` are final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Issued,
    Handed,
    Committed,
    NotHanded,
    HandedInert,
    Fenced,
}

impl Stage {
    pub fn is_final(self) -> bool {
        !matches!(self, Stage::Issued | Stage::Handed)
    }

    fn rank(self) -> u8 {
        match self {
            Stage::Issued => 0,
            Stage::Handed => 1,
            _ => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UncertainReason {
    /// Possibly handed, nobody possesses it, automatic repeats spent.
    Lapsed,
    /// No further automatic or manual hand-out fits the attempt budget.
    OutOfAttempts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndeliverableReason {
    Cancelled,
    Exited,
    MaxDeliver,
}

/// A message's state (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageState {
    Queued,
    Attempting,
    Uncertain(UncertainReason),
    CancelHeld,
    Settled,
    Undeliverable(UndeliverableReason),
}

impl MessageState {
    pub fn is_terminal(self) -> bool {
        matches!(self, MessageState::Settled | MessageState::Undeliverable(_))
    }
}

/// Which issuance path produced an attempt; every one draws on one budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind {
    First,
    Requeue,
    AutoRepeat,
    ManualRetry,
}

impl IssueKind {
    /// Whether the deck-written frame carries the possible-repeat line (§6.1).
    pub fn marked(self) -> bool {
        matches!(self, IssueKind::AutoRepeat | IssueKind::ManualRetry)
    }
}

/// One hand-out under an assignment (§2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub a: u32,
    pub e: u32,
    pub rx: Rx,
    /// Per-receiver delivery number.
    pub d: u64,
    pub stage: Stage,
    pub kind: IssueKind,
    /// A spool check since the attempt last held possession, and since the
    /// assignment's last hand-out, found no receipt for it (§3): only then
    /// does it count toward an automatic repeat. Possession regained, a
    /// rebinding, a fence or another hand-out of its assignment clears it, so
    /// each repeat rests on a check made after the one before it.
    pub cleared: bool,
    /// Bumped whenever possession is regained, so a check begun before is
    /// not taken as covering what was published after.
    mark: u64,
    /// Its incarnation was retired and the resume answer is still owed.
    pub resume_pending: bool,
    pub revoked: bool,
    /// When a revoke of this attempt was last sent (r8k R8G-04): the line can
    /// be lost — no link, or a full queue — so until the attempt is final
    /// [`AttemptModel::revokes_due`] sends it again one ack-wait later.
    revoke_sent: Option<Instant>,
    /// Kept for an attempt whose late commit can still correct the sender
    /// (§7): a `HandedInert` one, and a `Fenced` one until a spool check
    /// after its message ended found no commit for it — one begun while no
    /// live incarnation held its hand-back ([`Self::correcting`]).
    pub verifier: bool,
    /// The incarnation this final attempt's commit authority was last handed
    /// back to ([`Registration::correctable`], r8k B02). While it is live its
    /// replay of a saved commit can still be outstanding, so no spool check
    /// may take the verifier away: [`AttemptModel::verify_candidates`] leaves
    /// the attempt out, and the hand-back makes a check already under way
    /// stale (the mark).
    correcting: Option<Rx>,
    pub reclaimed: bool,
    issued_at: Instant,
    resume_deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Auto,
    Manual,
}

/// An attempt the caller must look up in the spool before it may count as
/// receipt-less (§3), with the possession mark the answer is valid for: the
/// caller hands the checks it could answer back to
/// [`AttemptModel::tick_cleared`], which revalidates each under its lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LapseCheck {
    pub id: String,
    pub a: u32,
    pub e: u32,
    mark: u64,
}

/// A scheduled repeat, revalidated at hand-out (§2, auditor C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RepeatToken {
    a: u32,
    revision: u64,
    kind: TokenKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Retry,
    Cancel,
}

/// The answer to a user action (§2 "Idempotent actions").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Applied { revision: u64 },
    Stale { state: MessageState, revision: u64 },
    NotApplicable { state: MessageState },
    AlreadySettled,
    RetryLimit,
    WrongGeneration,
    Unknown,
}

/// One message's delivery record.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub offer: bool,
    /// The current assignment.
    pub a: u32,
    pub state: MessageState,
    pub revision: u64,
    pub attempts: Vec<Attempt>,
    pub latch: bool,
    pub auto_repeats: u32,
    pub issued: u32,
    pub requeues: u32,
    token: Option<RepeatToken>,
    possessed_since: Option<Instant>,
    /// Since when rule 5 has found the message unpossessed and unable to go
    /// on: waiting for a spool check that can tell, or for a receiver to
    /// take its repeat.
    waiting_since: Option<Instant>,
    held_long_sent: Option<u64>,
    /// PRD #1666 G3: when the receiver first reported holding an attempt of
    /// the current assignment issued since the last Retry (a possession
    /// snapshot listing it, or its hand-over), never its issue grace. Unlike
    /// `possessed_since` a watcher gap does not clear it, so a respawn cannot
    /// postpone or re-arm the not-started window; a commit, the latch, any
    /// state but Attempting, a Retry or a new assignment does.
    not_started_anchor: Option<Instant>,
    /// PRD #1666 G3: the attempts a Retry left behind (every `e` up to this
    /// one). Their possession does not anchor the window of the repeat it
    /// asked for, which starts at the repeat's own.
    not_started_after_e: u32,
    /// PRD #1666 G3: the revision at which the not-started attention stands,
    /// recomputed from the turn clock on every tick ([`AttemptModel::note_turn`]).
    not_started: Option<u64>,
    last_action: Option<(u64, Verb, ActionOutcome)>,
    corrected: bool,
}

impl Delivery {
    fn new(offer: bool) -> Self {
        Delivery {
            offer,
            a: 1,
            state: MessageState::Queued,
            revision: 0,
            attempts: Vec::new(),
            latch: false,
            auto_repeats: 0,
            issued: 0,
            requeues: 0,
            token: None,
            possessed_since: None,
            waiting_since: None,
            held_long_sent: None,
            not_started_anchor: None,
            not_started_after_e: 0,
            not_started: None,
            last_action: None,
            corrected: false,
        }
    }

    pub fn attempt(&self, a: u32, e: u32) -> Option<&Attempt> {
        self.attempts.iter().find(|t| t.a == a && t.e == e)
    }

    fn attempt_mut(&mut self, a: u32, e: u32) -> Option<&mut Attempt> {
        self.attempts.iter_mut().find(|t| t.a == a && t.e == e)
    }

    /// Whether an automatic or manual repeat is scheduled and not yet handed
    /// out.
    pub fn repeat_scheduled(&self) -> bool {
        self.token.is_some()
    }

    /// Whether the one held_long attention (§6.3) was raised at the current
    /// revision and still stands.
    pub fn held_long(&self) -> bool {
        self.state == MessageState::Attempting && self.held_long_sent == Some(self.revision)
    }

    /// PRD #1666 G3: whether the not-started attention stands at the current
    /// revision: a deck message a Pi agent holds uncommitted while the deck's
    /// turn clock has read it idle for [`Limits::not_started_after`]. It
    /// yields to [`Self::held_long`], so a message is one row.
    pub fn not_started(&self) -> bool {
        self.state == MessageState::Attempting
            && self.not_started == Some(self.revision)
            && !self.held_long()
    }

    fn set_state(&mut self, state: MessageState) {
        if self.state != state {
            self.state = state;
            self.revision += 1;
        }
    }
}

/// What the model tells the hub to do or say, drained by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Revoke an open attempt (rule 1, Cancel).
    Revoke { id: String, a: u32, e: u32 },
    /// Tell the sender the message was delivered after all.
    DeliveredAfterAll { id: String, despite_cancel: bool },
    /// A positive not-handed witness was contradicted by a commit.
    WitnessViolation { id: String, a: u32, e: u32 },
    /// An earlier assignment's worker also received the task (T3).
    AlsoReachedOldWorker { id: String, a: u32 },
    /// An offer returns to its pool, excluding this worker.
    ReturnToPool { id: String, a: u32 },
    /// One-shot held_long attention (§6.3).
    HeldLong { id: String, revision: u64 },
    /// Hand-outs are paused for room in the evidence store (§7).
    EvidencePaused { reserved: usize },
    /// An attempt's evidence reservation was returned: its spool files go
    /// now (§7). Exit (T9) emits none, since the generation's whole spool
    /// goes with it.
    Reclaimed { id: String, a: u32, e: u32 },
}

/// Why a hand-out was refused (T1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueRefusal {
    UnknownMessage,
    RetiredReceiver,
    Terminal,
    Cancelled,
    NothingToIssue,
    StaleRepeat,
    OutOfAttempts,
    EvidencePaused,
    Exited,
}

/// A successful hand-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issued {
    pub a: u32,
    pub e: u32,
    pub d: u64,
    pub kind: IssueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    Applied,
    /// Not above the current stage: a duplicate or a late lower stage.
    NoOp,
    /// Recorded for an earlier assignment; nothing settles (T3).
    Recorded,
    WrongReceiver,
    Unknown,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotOutcome {
    Applied,
    /// A stale or replayed `seq`, or a retired incarnation.
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAnswer {
    Retained,
    NotHanded,
    Unknown,
}

/// A registration's answer: the new incarnation, the attempts it is asked
/// to resume (§4), and the final attempts whose late commit it may still
/// publish (r8k R8G-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub rx: Rx,
    pub resume: Vec<(String, u32, u32)>,
    /// Final attempts bound to a retired incarnation of the same slot that
    /// keep their late-commit verifier (§7) and are not reclaimed: a
    /// `HandedInert` one, and a `Fenced` one still verifying. Their late
    /// commit still corrects the sender, and the context that holds such a
    /// commit, its watcher having died with it, now answers on this
    /// incarnation. The caller hands each one's commit authority, and only
    /// that, to the new watcher: no stage, approval or callback comes with it.
    /// Each keeps its verifier while this incarnation lives (r8k B02), so a
    /// replay it has not published yet is not overtaken by an absence.
    pub correctable: Vec<(String, u32, u32)>,
}

/// A `may_hand_over` approval, valid only under the epoch it was granted in
/// (§2 "Revoke proof and callback quiescence").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub a: u32,
    pub e: u32,
    pub rx: Rx,
    pub epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandOverRefusal {
    Unknown,
    WrongReceiver,
    Committed,
    Revoked,
}

/// The component side of the final hand-over check: its synchronous local
/// revoked set and its incarnation epoch. Every native call checks it with no
/// await between the check and the call.
#[derive(Debug, Clone, Default)]
pub struct CallbackGuard {
    epoch: u64,
    revoked: BTreeSet<(String, u32, u32)>,
}

impl CallbackGuard {
    pub fn new(epoch: u64) -> Self {
        CallbackGuard {
            epoch,
            revoked: BTreeSet::new(),
        }
    }

    /// The context was rebound to a new incarnation: every earlier approval
    /// is void.
    pub fn rebind(&mut self, epoch: u64) {
        self.epoch = epoch;
    }

    pub fn revoke(&mut self, id: &str, a: u32, e: u32) {
        self.revoked.insert((id.to_string(), a, e));
    }

    /// Whether the callback holding `grant` may reach the native API now.
    pub fn may_call(&self, grant: &Grant) -> bool {
        grant.epoch == self.epoch && !self.revoked.contains(&(grant.id.clone(), grant.a, grant.e))
    }
}

/// The contiguous observed prefix a stream receiver reports as `seen` (§4,
/// tester C3): the highest `d` such that every `d' <= d` was observed.
#[derive(Debug, Clone, Default)]
pub struct ContiguousWatermark {
    seen: u64,
    above: BTreeSet<u64>,
}

impl ContiguousWatermark {
    pub fn observe(&mut self, d: u64) {
        if d <= self.seen {
            return;
        }
        self.above.insert(d);
        while self.above.remove(&(self.seen + 1)) {
            self.seen += 1;
        }
    }

    pub fn seen(&self) -> u64 {
        self.seen
    }
}

#[derive(Debug, Clone)]
struct Receiver {
    slot: Slot,
    retired: bool,
    last_seq: Option<u64>,
    seen: u64,
    fresh_at: Instant,
    held: BTreeSet<(String, u32, u32)>,
    /// A once-waiter's busy interval: busy from hand-out until Idle or an
    /// error stop, then one ack-wait of grace from the first idle report.
    busy: bool,
    idle_since: Option<Instant>,
    /// A hook or `inbox fetch` receiver's open connection.
    connected: bool,
    /// A stream receiver's connection closed at this instant; with no
    /// successor registered, it is retired [`Limits::closed_retire_after`]
    /// later.
    closed_at: Option<Instant>,
    queue_survived: bool,
    next_d: u64,
}

/// One agent generation's delivery model.
#[derive(Debug)]
pub struct AttemptModel {
    limits: Limits,
    generation: u64,
    receivers: BTreeMap<Rx, Receiver>,
    slots: BTreeMap<Slot, Rx>,
    next_rx: u64,
    messages: BTreeMap<String, Delivery>,
    evidence_reserved: usize,
    effects: Vec<Effect>,
    exited: bool,
}

impl AttemptModel {
    pub fn new(limits: Limits, generation: u64) -> Self {
        AttemptModel {
            limits,
            generation,
            receivers: BTreeMap::new(),
            slots: BTreeMap::new(),
            next_rx: 1,
            messages: BTreeMap::new(),
            evidence_reserved: 0,
            effects: Vec::new(),
            exited: false,
        }
    }

    pub fn post(&mut self, id: &str, offer: bool) {
        self.messages
            .entry(id.to_string())
            .or_insert_with(|| Delivery::new(offer));
    }

    pub fn delivery(&self, id: &str) -> Option<&Delivery> {
        self.messages.get(id)
    }

    pub fn state(&self, id: &str) -> Option<MessageState> {
        self.messages.get(id).map(|d| d.state)
    }

    /// Evidence entries (spool files) reserved by attempts not yet reclaimed.
    pub fn evidence_reserved(&self) -> usize {
        self.evidence_reserved
    }

    pub fn drain_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// T8: a receiver registers in `slot`. The previous incarnation of the
    /// same slot, and only it, is retired: its controls and approvals are
    /// refused at once, and its open attempts wait for this registration's
    /// resume answer.
    pub fn register(&mut self, now: Instant, slot: Slot, queue_survived: bool) -> Registration {
        let rx = Rx(self.next_rx);
        self.next_rx += 1;
        self.receivers.insert(
            rx,
            Receiver {
                slot,
                retired: false,
                last_seq: None,
                seen: 0,
                fresh_at: now,
                held: BTreeSet::new(),
                busy: slot == Slot::OnceWaiter,
                idle_since: None,
                connected: slot == Slot::Hook,
                closed_at: None,
                queue_survived,
                next_d: 0,
            },
        );
        let mut resume = Vec::new();
        let mut correctable = Vec::new();
        if let Some(old) = self.slots.insert(slot, rx) {
            if let Some(r) = self.receivers.get_mut(&old) {
                r.retired = true;
            }
            let deadline = now + self.limits.ack_wait;
            for (id, msg) in self.messages.iter_mut() {
                for at in msg.attempts.iter_mut() {
                    let in_slot = self
                        .receivers
                        .get(&at.rx)
                        .is_some_and(|r| r.slot == slot && r.retired);
                    if !in_slot {
                        continue;
                    }
                    if !at.stage.is_final() {
                        at.resume_pending = true;
                        at.resume_deadline = Some(deadline);
                        resume.push((id.clone(), at.a, at.e));
                    }
                }
            }
        }
        // Every retired incarnation of the slot counts, the one just replaced
        // and any the clock retired with no successor (which frees the slot).
        // r8k B02: the hand-back holds the verifier for as long as this
        // incarnation lives, and a spool check begun before it covers nothing.
        for (id, msg) in self.messages.iter_mut() {
            for at in msg.attempts.iter_mut() {
                let in_slot = self
                    .receivers
                    .get(&at.rx)
                    .is_some_and(|r| r.slot == slot && r.retired);
                if in_slot && at.stage.is_final() && at.verifier && !at.reclaimed {
                    at.correcting = Some(rx);
                    at.recheck();
                    correctable.push((id.clone(), at.a, at.e));
                }
            }
        }
        self.finalize_all(now);
        Registration {
            rx,
            resume,
            correctable,
        }
    }

    /// Whether a receiver on `slot` ever registered with this generation.
    pub fn has_slot(&self, slot: Slot) -> bool {
        self.receivers.values().any(|r| r.slot == slot)
    }

    /// T1: hand `id` out to `rx`, if it is `Queued` or carries a repeat token
    /// that still holds under the lock right now.
    pub fn issue(&mut self, now: Instant, id: &str, rx: Rx) -> Result<Issued, IssueRefusal> {
        if self.exited {
            return Err(IssueRefusal::Exited);
        }
        if !self.receivers.get(&rx).is_some_and(|r| !r.retired) {
            return Err(IssueRefusal::RetiredReceiver);
        }
        let Some(msg) = self.messages.get(id) else {
            return Err(IssueRefusal::UnknownMessage);
        };
        if msg.state.is_terminal() {
            return Err(IssueRefusal::Terminal);
        }
        if msg.latch {
            return Err(IssueRefusal::Cancelled);
        }
        let kind = if let Some(token) = msg.token {
            let held = msg.attempts.iter().filter(|at| at.a == msg.a).any(|at| {
                at.resume_pending || possessed(&self.receivers, &self.limits, id, at, now)
            });
            // §3: an automatic repeat only once every possibly handed attempt
            // of the assignment was checked in the spool and found no receipt.
            let cleared = msg
                .attempts
                .iter()
                .filter(|at| at.a == msg.a)
                .all(|at| !needs_clearance(at) || at.cleared);
            let valid = token.a == msg.a
                && token.revision == msg.revision
                && (token.kind == TokenKind::Manual || (!held && cleared));
            if !valid {
                if let Some(msg) = self.messages.get_mut(id) {
                    msg.token = None;
                }
                self.finalize(now, id);
                return Err(IssueRefusal::StaleRepeat);
            }
            match token.kind {
                TokenKind::Auto => IssueKind::AutoRepeat,
                TokenKind::Manual => IssueKind::ManualRetry,
            }
        } else if msg.state == MessageState::Queued {
            if msg.attempts.is_empty() {
                IssueKind::First
            } else {
                IssueKind::Requeue
            }
        } else {
            return Err(IssueRefusal::NothingToIssue);
        };
        if msg.issued >= self.limits.max_attempts {
            self.finalize(now, id);
            return Err(IssueRefusal::OutOfAttempts);
        }
        if self.evidence_reserved + EVIDENCE_PER_ATTEMPT > self.limits.evidence_limit {
            self.effects.push(Effect::EvidencePaused {
                reserved: self.evidence_reserved,
            });
            return Err(IssueRefusal::EvidencePaused);
        }
        let Some(receiver) = self.receivers.get_mut(&rx) else {
            return Err(IssueRefusal::RetiredReceiver);
        };
        receiver.next_d += 1;
        let d = receiver.next_d;
        let Some(msg) = self.messages.get_mut(id) else {
            return Err(IssueRefusal::UnknownMessage);
        };
        let a = msg.a;
        let e = msg.attempts.len() as u32 + 1;
        // §3: a check covers one repeat decision. One begun before this
        // hand-out and answered after it is stale too (the mark).
        for at in msg.attempts.iter_mut().filter(|at| at.a == a) {
            at.recheck();
        }
        msg.attempts.push(Attempt {
            a,
            e,
            rx,
            d,
            stage: Stage::Issued,
            kind,
            cleared: false,
            mark: 0,
            resume_pending: false,
            revoked: false,
            revoke_sent: None,
            verifier: false,
            correcting: None,
            reclaimed: false,
            issued_at: now,
            resume_deadline: None,
        });
        msg.issued += 1;
        if kind == IssueKind::AutoRepeat {
            msg.auto_repeats += 1;
        }
        msg.token = None;
        self.evidence_reserved += EVIDENCE_PER_ATTEMPT;
        self.finalize(now, id);
        Ok(Issued { a, e, d, kind })
    }

    /// T2, T3 and T7: a stage record for `(a, e)`. `from` is the reporting
    /// incarnation; a `Committed` record is admitted on its cap alone and
    /// passes `None`. `Committed` dominates every other final stage; every
    /// other record is applied only from the attempt's live incarnation and
    /// only when it ranks above the current stage.
    pub fn stage(
        &mut self,
        now: Instant,
        id: &str,
        a: u32,
        e: u32,
        stage: Stage,
        from: Option<Rx>,
    ) -> StageOutcome {
        if self.exited {
            return StageOutcome::Exited;
        }
        let from_receiver =
            from.and_then(|rx| self.receivers.get(&rx).map(|r| (rx, r.slot, r.retired)));
        let Some(msg) = self.messages.get_mut(id) else {
            return StageOutcome::Unknown;
        };
        let current = msg.a;
        let Some(at) = msg.attempt_mut(a, e) else {
            return StageOutcome::Unknown;
        };
        if stage == Stage::Committed {
            if at.stage == Stage::Committed {
                return StageOutcome::NoOp;
            }
            let contradicted = at.stage == Stage::NotHanded || a != current;
            at.stage = Stage::Committed;
            at.resume_pending = false;
            at.resume_deadline = None;
            at.verifier = false;
            if contradicted {
                self.effects.push(Effect::WitnessViolation {
                    id: id.to_string(),
                    a,
                    e,
                });
            }
            if a != current {
                self.effects.push(Effect::AlsoReachedOldWorker {
                    id: id.to_string(),
                    a,
                });
                self.reclaim(id);
                return StageOutcome::Recorded;
            }
            self.finalize(now, id);
            return StageOutcome::Applied;
        }
        if matches!(stage, Stage::Issued | Stage::Fenced) {
            return StageOutcome::WrongReceiver;
        }
        let Some((rx, slot, false)) = from_receiver else {
            return StageOutcome::WrongReceiver;
        };
        if rx != at.rx || at.resume_pending {
            return StageOutcome::WrongReceiver;
        }
        if stage.rank() <= at.stage.rank() {
            return StageOutcome::NoOp;
        }
        let stage = if stage == Stage::Handed && slot == Slot::OnceWaiter {
            Stage::HandedInert
        } else {
            stage
        };
        at.stage = stage;
        if stage == Stage::HandedInert {
            at.verifier = true;
        }
        self.finalize(now, id);
        StageOutcome::Applied
    }

    /// A stream receiver's possession snapshot (§4). `seen` is its contiguous
    /// observed prefix; a stale or replayed `seq`, or a retired incarnation's
    /// snapshot, is dropped.
    pub fn snapshot(
        &mut self,
        now: Instant,
        rx: Rx,
        seq: u64,
        seen: u64,
        held: &[(&str, u32, u32)],
    ) -> SnapshotOutcome {
        let outcome = self.apply_snapshot(now, rx, seq, seen, held);
        if outcome == SnapshotOutcome::Applied {
            self.finalize_bound_to(now, rx);
        }
        outcome
    }

    /// Record a snapshot's possession without running the finalizer.
    fn apply_snapshot(
        &mut self,
        now: Instant,
        rx: Rx,
        seq: u64,
        seen: u64,
        held: &[(&str, u32, u32)],
    ) -> SnapshotOutcome {
        let Some(r) = self.receivers.get_mut(&rx) else {
            return SnapshotOutcome::Dropped;
        };
        if r.retired || r.last_seq.is_some_and(|last| seq <= last) {
            return SnapshotOutcome::Dropped;
        }
        r.last_seq = Some(seq);
        // Monotone at admission: a lower watermark never revives the issue
        // grace an earlier covering omission ended.
        r.seen = r.seen.max(seen);
        r.fresh_at = now;
        r.held = held
            .iter()
            .map(|(id, a, e)| (id.to_string(), *a, *e))
            .collect();
        for (id, a, e) in held {
            if let Some(at) = self
                .messages
                .get_mut(*id)
                .and_then(|msg| msg.attempt_mut(*a, *e))
                .filter(|at| at.rx == rx)
            {
                at.regained();
            }
        }
        SnapshotOutcome::Applied
    }

    /// A once-waiter's busy interval (§4): busy from `Handed`; Idle or an
    /// error stop starts one ack-wait of grace. A repeated idle report keeps
    /// the first idle time, and only a resumed busy interval restarts it.
    /// Other slots ignore it: a hook's possession is its connection.
    pub fn busy(&mut self, now: Instant, rx: Rx, busy: bool) {
        if let Some(r) = self.receivers.get_mut(&rx)
            && r.slot == Slot::OnceWaiter
        {
            if busy {
                r.busy = true;
                r.idle_since = None;
                for msg in self.messages.values_mut() {
                    for at in msg.attempts.iter_mut().filter(|at| at.rx == rx) {
                        at.regained();
                    }
                }
            } else if r.busy || r.idle_since.is_none() {
                r.busy = false;
                r.idle_since = Some(now);
            }
        }
        self.finalize_bound_to(now, rx);
    }

    /// Whether `rx` is a live incarnation.
    pub fn live(&self, rx: Rx) -> bool {
        self.receivers.get(&rx).is_some_and(|r| !r.retired)
    }

    /// The live incarnation of `slot`, if one is registered.
    pub fn slot_rx(&self, slot: Slot) -> Option<Rx> {
        self.slots.get(&slot).copied().filter(|rx| self.live(*rx))
    }

    /// The attempts a spool check must clear before rule 5 may repeat their
    /// message (§3): possibly handed, of a live message's current
    /// assignment, not resuming, not possessed and not cleared since they
    /// last were. Each carries its possession mark, for
    /// [`Self::tick_cleared`] to revalidate.
    pub fn lapse_candidates(&self, now: Instant) -> Vec<LapseCheck> {
        let mut out = Vec::new();
        if self.exited {
            return out;
        }
        for (id, msg) in &self.messages {
            if msg.state.is_terminal() {
                continue;
            }
            for at in msg.attempts.iter().filter(|at| at.a == msg.a) {
                if needs_clearance(at)
                    && !at.cleared
                    && !possessed(&self.receivers, &self.limits, id, at, now)
                {
                    out.push(LapseCheck {
                        id: id.clone(),
                        a: at.a,
                        e: at.e,
                        mark: at.mark,
                    });
                }
            }
        }
        out
    }

    /// The fenced attempts of ended messages that still keep their late-commit
    /// verifier (§7): each is looked up in the spool, and only a lookup that
    /// finds no commit lets [`Self::tick_verified`] return its evidence, so a
    /// receipt its incarnation published before a Cancel ended the message
    /// still corrects the sender.
    ///
    /// An attempt whose commit authority was handed back to an incarnation
    /// that is still live is left out (r8k B02): that incarnation may yet
    /// publish the commit its predecessor died with, and an absence now says
    /// nothing about it. Verification resumes once that incarnation is
    /// retired — by a successor's registration, which hands the attempt back
    /// to the successor in turn, or by the clock with no successor.
    pub fn verify_candidates(&self) -> Vec<LapseCheck> {
        let mut out = Vec::new();
        if self.exited {
            return out;
        }
        for (id, msg) in &self.messages {
            if !msg.state.is_terminal() {
                continue;
            }
            for at in &msg.attempts {
                let replaying = at.correcting.is_some_and(|rx| self.live(rx));
                if at.stage == Stage::Fenced && at.verifier && !at.reclaimed && !replaying {
                    out.push(LapseCheck {
                        id: id.clone(),
                        a: at.a,
                        e: at.e,
                        mark: at.mark,
                    });
                }
            }
        }
        out
    }

    /// The verify candidates whose spool lookup found no commit: each one
    /// still fenced, unchanged since, drops its verifier, and its evidence is
    /// returned.
    pub fn tick_verified(&mut self, verified: &[LapseCheck]) {
        if self.exited {
            return;
        }
        for check in verified {
            let Some(at) = self
                .messages
                .get_mut(&check.id)
                .and_then(|msg| msg.attempt_mut(check.a, check.e))
            else {
                continue;
            };
            if at.mark == check.mark && at.stage == Stage::Fenced {
                at.verifier = false;
            }
            self.reclaim(&check.id);
        }
    }

    /// Whether any attempt could still have a commit receipt the deck has
    /// not seen: not reclaimed and not committed. While one does, the spool
    /// is worth scanning, so a commit published for any issued attempt —
    /// fenced, lapsed, an earlier assignment's or a late-correction
    /// verifier's — reaches T2 or T3.
    pub fn watching_receipts(&self) -> bool {
        !self.exited
            && self.messages.values().any(|msg| {
                msg.attempts
                    .iter()
                    .any(|at| !at.reclaimed && at.stage != Stage::Committed)
            })
    }

    /// The message ids this model holds.
    pub fn ids(&self) -> Vec<String> {
        self.messages.keys().cloned().collect()
    }

    /// The incarnation an attempt is bound to.
    pub fn attempt_rx(&self, id: &str, a: u32, e: u32) -> Option<Rx> {
        self.messages.get(id)?.attempt(a, e).map(|at| at.rx)
    }

    /// Messages whose delivered attempts still hold evidence (§7 "N
    /// delivered messages still wait").
    pub fn holding_evidence(&self) -> usize {
        self.messages
            .values()
            .filter(|msg| msg.attempts.iter().any(|at| !at.reclaimed))
            .count()
    }

    /// A hook or `inbox fetch` receiver's connection closed (§4): its
    /// possession ends.
    pub fn connection_closed(&mut self, now: Instant, rx: Rx) {
        if let Some(r) = self.receivers.get_mut(&rx) {
            r.connected = false;
        }
        self.finalize_bound_to(now, rx);
    }

    /// A stream receiver's connection closed (§4). It decides nothing at
    /// once: the incarnation stays its slot's so a reconnecting context can
    /// resume what it holds, and its attempts lapse when `POSSESS_TTL` runs
    /// out. With no successor registered within
    /// [`Limits::closed_retire_after`], the clock retires it and fences its
    /// open attempts: possibly handed, with no one left to answer for them.
    pub fn stream_closed(&mut self, now: Instant, rx: Rx) {
        if let Some(r) = self.receivers.get_mut(&rx)
            && !r.retired
            && matches!(r.slot, Slot::OpencodeServer | Slot::OpencodeTui | Slot::Pi)
        {
            r.closed_at.get_or_insert(now);
        }
        self.finalize_bound_to(now, rx);
    }

    /// The final hand-over check (§3), the daemon's half. It only answers: a
    /// refusal changes no stage, since an earlier approval of the same
    /// attempt may still be live in the context. `NotHanded` comes from the
    /// receiver itself, once it has dropped the callback and installed its
    /// local revoke. An approval carries the incarnation epoch it was granted
    /// under.
    pub fn may_hand_over(
        &self,
        id: &str,
        a: u32,
        e: u32,
        rx: Rx,
    ) -> Result<Grant, HandOverRefusal> {
        let live = self.receivers.get(&rx).is_some_and(|r| !r.retired);
        let Some(msg) = self.messages.get(id) else {
            return Err(HandOverRefusal::Unknown);
        };
        let Some(at) = msg.attempt(a, e) else {
            return Err(HandOverRefusal::Unknown);
        };
        if at.rx != rx || !live || at.resume_pending {
            return Err(HandOverRefusal::WrongReceiver);
        }
        if at.stage.is_final() {
            return Err(HandOverRefusal::Revoked);
        }
        let committed = msg
            .attempts
            .iter()
            .any(|t| t.a == a && t.stage == Stage::Committed);
        if committed {
            return Err(HandOverRefusal::Committed);
        }
        if at.revoked || msg.latch || a != msg.a {
            return Err(HandOverRefusal::Revoked);
        }
        Ok(Grant {
            id: id.to_string(),
            a,
            e,
            rx,
            epoch: rx.0,
        })
    }

    /// T8: the new incarnation's answer for one resumed attempt. `Retained`
    /// rebinds the same attempt, stage unchanged; `NotHanded` counts only
    /// from a context whose callback queue survived; anything else fences it.
    pub fn resume_answer(
        &mut self,
        now: Instant,
        rx: Rx,
        id: &str,
        a: u32,
        e: u32,
        answer: ResumeAnswer,
    ) -> StageOutcome {
        let Some(r) = self.receivers.get(&rx) else {
            return StageOutcome::Unknown;
        };
        if r.retired {
            return StageOutcome::WrongReceiver;
        }
        let (slot, survived) = (r.slot, r.queue_survived);
        let Some(msg) = self.messages.get_mut(id) else {
            return StageOutcome::Unknown;
        };
        let Some(at) = msg.attempt_mut(a, e) else {
            return StageOutcome::Unknown;
        };
        if !at.resume_pending {
            return StageOutcome::NoOp;
        }
        if self.receivers.get(&at.rx).map(|old| old.slot) != Some(slot) {
            return StageOutcome::WrongReceiver;
        }
        at.resume_pending = false;
        at.resume_deadline = None;
        match answer {
            ResumeAnswer::Retained => {
                let Some(r) = self.receivers.get_mut(&rx) else {
                    return StageOutcome::Unknown;
                };
                r.next_d += 1;
                at.rx = rx;
                at.d = r.next_d;
                at.issued_at = now;
                at.regained();
                // r8k R8G-04: a revoke sent to the retired incarnation may
                // never have reached the context, which now answers on this
                // one; it is sent again here, to the new incarnation.
                if at.revoked {
                    at.revoke_sent = Some(now);
                    self.effects.push(Effect::Revoke {
                        id: id.to_string(),
                        a,
                        e,
                    });
                }
            }
            ResumeAnswer::NotHanded if survived => at.stage = Stage::NotHanded,
            _ => at.fence(),
        }
        self.finalize(now, id);
        StageOutcome::Applied
    }

    /// T4 (lapse), T8 (resume silence) and held_long attention, for a
    /// generation with no receipt store to check: the declared degraded
    /// policy (§3), under which no suppression is claimed, so every lapse
    /// candidate counts as checked.
    pub fn tick(&mut self, now: Instant) {
        let checks = self.lapse_candidates(now);
        self.tick_cleared(now, &checks);
    }

    /// T4 (lapse), T8 (resume silence) and held_long attention. `cleared`
    /// are the lapse candidates whose spool check found no receipt (§3 "Slow
    /// and failed storage": only a completed, error-free scan and an exact
    /// lookup that finds neither record name is absence). Each is
    /// revalidated here, under the caller's lock: one that regained
    /// possession, was rebound or fenced, or reached a final stage while
    /// the check ran is not cleared by it. A candidate left out stays
    /// uncleared, so it cannot lead to an automatic repeat.
    pub fn tick_cleared(&mut self, now: Instant, cleared: &[LapseCheck]) {
        if self.exited {
            return;
        }
        for check in cleared {
            let Some(msg) = self.messages.get(&check.id) else {
                continue;
            };
            let Some(at) = msg.attempt(check.a, check.e) else {
                continue;
            };
            let valid = at.mark == check.mark
                && at.a == msg.a
                && needs_clearance(at)
                && !possessed(&self.receivers, &self.limits, &check.id, at, now);
            if valid
                && let Some(at) = self
                    .messages
                    .get_mut(&check.id)
                    .and_then(|msg| msg.attempt_mut(check.a, check.e))
            {
                at.cleared = true;
            }
        }
        self.retire_closed(now);
        let ids: Vec<String> = self.messages.keys().cloned().collect();
        for id in &ids {
            if let Some(msg) = self.messages.get_mut(id) {
                for at in msg.attempts.iter_mut() {
                    if at.resume_pending && at.resume_deadline.is_some_and(|d| now >= d) {
                        at.resume_pending = false;
                        at.resume_deadline = None;
                        at.fence();
                    }
                }
            }
            self.finalize(now, id);
            let held_long_after = self.limits.held_long_after;
            if let Some(msg) = self.messages.get_mut(id) {
                let due = msg
                    .possessed_since
                    .is_some_and(|since| now >= since + held_long_after);
                if msg.state == MessageState::Attempting
                    && !msg.latch
                    && due
                    && msg.held_long_sent != Some(msg.revision)
                {
                    msg.held_long_sent = Some(msg.revision);
                    self.effects.push(Effect::HeldLong {
                        id: id.clone(),
                        revision: msg.revision,
                    });
                }
            }
        }
        self.revokes_due(now);
    }

    /// [`Self::tick_cleared`], then G3's not-started attention against the
    /// generation's turn clock ([`Self::note_turn`]).
    pub fn tick_turn(&mut self, now: Instant, cleared: &[LapseCheck], turn: TurnView) {
        self.tick_cleared(now, cleared);
        self.note_turn(now, turn);
    }

    /// PRD #1666 G3: recompute every message's not-started attention against
    /// `turn`, the generation's turn clock. It stands for a message handed to
    /// a Pi receiver, still attempting and uncommitted, once the agent has
    /// been idle for [`Limits::not_started_after`] since both the message's
    /// first reported possession or hand-over and the end of its last turn. A turn
    /// under way, a commit or ineligibility withdraws it, and only a turn that
    /// really ended re-arms it. Attention only: it changes no state, schedules
    /// no repeat and issues nothing.
    pub fn note_turn(&mut self, now: Instant, turn: TurnView) {
        if self.exited {
            return;
        }
        let ids: Vec<String> = self.messages.keys().cloned().collect();
        for id in &ids {
            let due = self.not_started_due(id, turn);
            if let Some(msg) = self.messages.get_mut(id) {
                msg.not_started = due.filter(|due| now >= *due).map(|_| msg.revision);
            }
        }
    }

    /// PRD #1666 G3: when the next not-started attention not yet standing
    /// falls due under `turn`, so the clock wakes for it.
    pub fn not_started_due_at(&self, turn: TurnView) -> Option<Instant> {
        self.messages
            .iter()
            .filter(|(_, msg)| msg.not_started.is_none())
            .filter_map(|(id, _)| self.not_started_due(id, turn))
            .min()
    }

    fn not_started_due(&self, id: &str, turn: TurnView) -> Option<Instant> {
        if self.exited || turn.busy {
            return None;
        }
        let msg = self.messages.get(id)?;
        // Attempting excludes a commit (rule 1 settles the message) and the
        // cancel latch (rule 2 holds or ends it).
        if msg.state != MessageState::Attempting || msg.latch {
            return None;
        }
        let anchor = msg.not_started_anchor?;
        let on_pi = msg.attempts.iter().filter(|at| at.a == msg.a).any(|at| {
            !at.stage.is_final()
                && self
                    .receivers
                    .get(&at.rx)
                    .is_some_and(|r| r.slot == Slot::Pi)
        });
        if !on_pi {
            return None;
        }
        Some(anchor.max(turn.idle_since.unwrap_or(anchor)) + self.limits.not_started_after)
    }

    /// r8k R8G-04: send again every revoke still owed an answer one ack-wait
    /// after it was last sent. A revoke line can be lost on its way — the
    /// attempt's incarnation had no link, or its queue was full — and nothing
    /// else would send it again: a terminal message is no lapse candidate, and
    /// a stream is retired only by a replacement registration (which answers
    /// through [`Self::resume_answer`]) or by the clock after it closed.
    ///
    /// Owed while the attempt is revoked and not final, not waiting for a
    /// resume answer (the answer re-sends it, [`Self::resume_answer`]) and not
    /// reclaimed. It stops only when the attempt is final: the receiver's
    /// answer (`revoked_ack`, `already_handed`), a commit, a resume answer or
    /// its silence, the stream's retirement, or exit. A send proves nothing
    /// and neither does an absence, so neither makes it final.
    ///
    /// That bounds the RATE of this periodic path — one line per owed attempt
    /// per ack-wait — and, through the evidence budget, how many attempts can
    /// be owed at once. A retained rebind sends at once and restamps the
    /// attempt (see [`Self::resume_answer`]), so repeated registrations can
    /// send more than one line within an ack-wait. It does not bound the
    /// number of re-sends: a live receiver that never
    /// answers is sent one per ack-wait for as long as it lives, and its
    /// attempt keeps its evidence reserved all that time.
    pub fn revokes_due(&mut self, now: Instant) {
        if self.exited {
            return;
        }
        let ack_wait = self.limits.ack_wait;
        for (id, msg) in self.messages.iter_mut() {
            for at in msg.attempts.iter_mut().filter(|at| owes_revoke(at)) {
                if at.revoke_sent.is_none_or(|sent| now >= sent + ack_wait) {
                    at.revoke_sent = Some(now);
                    self.effects.push(Effect::Revoke {
                        id: id.clone(),
                        a: at.a,
                        e: at.e,
                    });
                }
            }
        }
    }

    /// r8k R8G-04: when the next owed revoke is due to be sent again
    /// ([`Self::revokes_due`]), so the clock runs while one is owed even when
    /// its message is terminal.
    pub fn revoke_due_at(&self) -> Option<Instant> {
        if self.exited {
            return None;
        }
        let ack_wait = self.limits.ack_wait;
        self.messages
            .values()
            .flat_map(|msg| msg.attempts.iter())
            .filter(|at| owes_revoke(at))
            .map(|at| at.revoke_sent.map_or(at.issued_at, |sent| sent + ack_wait))
            .min()
    }

    /// Retire every stream incarnation whose connection closed
    /// [`Limits::closed_retire_after`] ago with no successor: its slot is
    /// free, and its open attempts are fenced — possibly handed, with no
    /// incarnation left to resume or revoke them.
    fn retire_closed(&mut self, now: Instant) {
        let after = self.limits.closed_retire_after();
        let gone: Vec<Rx> = self
            .receivers
            .iter()
            .filter(|(_, r)| !r.retired && r.closed_at.is_some_and(|at| now >= at + after))
            .map(|(rx, _)| *rx)
            .collect();
        for rx in gone {
            if let Some(r) = self.receivers.get_mut(&rx) {
                r.retired = true;
                if self.slots.get(&r.slot) == Some(&rx) {
                    self.slots.remove(&r.slot);
                }
            }
            for msg in self.messages.values_mut() {
                for at in msg.attempts.iter_mut().filter(|at| at.rx == rx) {
                    if !at.stage.is_final() && !at.resume_pending {
                        at.fence();
                    }
                }
            }
        }
    }

    /// T5: user Retry on `Uncertain`, held_long or not-started attention: a
    /// manual repeat token.
    pub fn retry(
        &mut self,
        now: Instant,
        id: &str,
        generation: u64,
        revision: u64,
    ) -> ActionOutcome {
        self.action(now, id, generation, revision, Verb::Retry)
    }

    /// T6: user Cancel: the latch, and a revoke of every open attempt.
    pub fn cancel(
        &mut self,
        now: Instant,
        id: &str,
        generation: u64,
        revision: u64,
    ) -> ActionOutcome {
        self.action(now, id, generation, revision, Verb::Cancel)
    }

    fn action(
        &mut self,
        now: Instant,
        id: &str,
        generation: u64,
        revision: u64,
        verb: Verb,
    ) -> ActionOutcome {
        if generation != self.generation {
            return ActionOutcome::WrongGeneration;
        }
        let max_attempts = self.limits.max_attempts;
        let Some(msg) = self.messages.get_mut(id) else {
            return ActionOutcome::Unknown;
        };
        if let Some((r, v, outcome)) = msg.last_action
            && r == revision
            && v == verb
        {
            return outcome;
        }
        if revision != msg.revision {
            return ActionOutcome::Stale {
                state: msg.state,
                revision: msg.revision,
            };
        }
        let state = msg.state;
        let mut outcome = match verb {
            _ if state == MessageState::Settled => ActionOutcome::AlreadySettled,
            _ if state.is_terminal() => ActionOutcome::NotApplicable { state },
            Verb::Retry => {
                let held_long = state == MessageState::Attempting && msg.held_long_sent.is_some();
                // PRD #1666 G3: the not-started attention the user saw, still
                // standing at the revision they saw it at.
                let not_started =
                    state == MessageState::Attempting && msg.not_started == Some(msg.revision);
                if !(matches!(state, MessageState::Uncertain(_)) || held_long || not_started) {
                    ActionOutcome::NotApplicable { state }
                } else if msg.issued >= max_attempts {
                    ActionOutcome::RetryLimit
                } else {
                    if state == MessageState::Attempting {
                        msg.revision += 1;
                    } else {
                        msg.set_state(MessageState::Attempting);
                    }
                    msg.token = Some(RepeatToken {
                        a: msg.a,
                        revision: msg.revision,
                        kind: TokenKind::Manual,
                    });
                    msg.possessed_since = None;
                    msg.waiting_since = None;
                    msg.held_long_sent = None;
                    msg.not_started = None;
                    msg.not_started_anchor = None;
                    msg.not_started_after_e = msg.attempts.len() as u32;
                    ActionOutcome::Applied { revision: 0 }
                }
            }
            Verb::Cancel => {
                msg.latch = true;
                msg.token = None;
                msg.revision += 1;
                let current = msg.a;
                for at in msg.attempts.iter_mut() {
                    if at.a == current && !at.stage.is_final() && !at.revoked {
                        at.revoked = true;
                        at.revoke_sent = Some(now);
                        self.effects.push(Effect::Revoke {
                            id: id.to_string(),
                            a: at.a,
                            e: at.e,
                        });
                    }
                }
                ActionOutcome::Applied { revision: 0 }
            }
        };
        if matches!(outcome, ActionOutcome::Applied { .. }) {
            self.finalize(now, id);
        }
        let Some(msg) = self.messages.get_mut(id) else {
            return ActionOutcome::Unknown;
        };
        if let ActionOutcome::Applied { revision: r } = &mut outcome {
            *r = msg.revision;
            // Only an applied action is remembered: an unsuccessful one at a
            // newer revision must not turn a retransmission of the applied
            // one into `Stale`.
            msg.last_action = Some((revision, verb, outcome));
        }
        outcome
    }

    /// T9: the generation exited. Open attempts are fenced, live messages are
    /// undeliverable (an offer returns to its pool), and all evidence goes.
    pub fn exit(&mut self, now: Instant) {
        let _ = now;
        self.exited = true;
        for (id, msg) in self.messages.iter_mut() {
            for at in msg.attempts.iter_mut() {
                if !at.stage.is_final() || at.resume_pending {
                    at.stage = Stage::Fenced;
                    at.resume_pending = false;
                    at.resume_deadline = None;
                }
                at.verifier = false;
                at.reclaimed = true;
            }
            msg.token = None;
            if !msg.state.is_terminal() {
                if msg.offer {
                    self.effects.push(Effect::ReturnToPool {
                        id: id.clone(),
                        a: msg.a,
                    });
                }
                msg.set_state(MessageState::Undeliverable(UndeliverableReason::Exited));
            }
        }
        self.evidence_reserved = 0;
    }

    /// The one finalizer (§2): every message state change except T5 Retry
    /// and T9 exit (module docs). First matching rule wins.
    pub fn finalize(&mut self, now: Instant, id: &str) {
        if self.exited {
            return;
        }
        let Some(msg) = self.messages.get(id) else {
            return;
        };
        let current: Vec<&Attempt> = msg.attempts.iter().filter(|at| at.a == msg.a).collect();
        let committed = current.iter().any(|at| at.stage == Stage::Committed);
        let held = current
            .iter()
            .any(|at| possessed(&self.receivers, &self.limits, id, at, now));
        let resuming = current.iter().any(|at| at.resume_pending);
        let all_final = current
            .iter()
            .all(|at| at.stage.is_final() && !at.resume_pending);
        let all_not_handed =
            !current.is_empty() && current.iter().all(|at| at.stage == Stage::NotHanded);
        let none_issued = current.is_empty();
        // G3's anchor: a receiver's own report that it holds an attempt of
        // this delivery cycle, not the issue grace `held` also counts.
        let witnessed = current
            .iter()
            .filter(|at| at.e > msg.not_started_after_e)
            .any(|at| {
                at.stage == Stage::Handed
                    || (possessed(&self.receivers, &self.limits, id, at, now)
                        && snapshot_holds(&self.receivers, &self.limits, id, at, now))
            });
        let cleared = current.iter().all(|at| !needs_clearance(at) || at.cleared);
        let limits = self.limits;
        let Some(msg) = self.messages.get_mut(id) else {
            return;
        };
        let effects = &mut self.effects;
        'rules: {
            // Rule 1: any attempt of the assignment committed.
            if committed {
                // A message that ran out of attempts on not-handed reports a
                // commit now contradicts (PR #1735 review 4298669219) reached
                // the agent after all: it settles, and whoever was told it was
                // undeliverable is corrected through the hub's late-ack record
                // when the inbox copy follows (reviewer R9T-03). An offer went
                // back to its pool and stays out of attempts.
                if msg.state == MessageState::Undeliverable(UndeliverableReason::MaxDeliver)
                    && !msg.offer
                {
                    msg.set_state(MessageState::Settled);
                    if !msg.corrected {
                        msg.corrected = true;
                        effects.push(Effect::DeliveredAfterAll {
                            id: id.to_string(),
                            despite_cancel: false,
                        });
                    }
                    break 'rules;
                }
                if msg.state.is_terminal() {
                    // A message ended as cancelled reached the agent after
                    // all, and whoever was told otherwise is told so; it
                    // stays cancelled.
                    let despite_cancel = match msg.state {
                        MessageState::Undeliverable(UndeliverableReason::Cancelled) => Some(true),
                        MessageState::Undeliverable(UndeliverableReason::MaxDeliver) => Some(false),
                        _ => None,
                    };
                    if let Some(despite_cancel) = despite_cancel
                        && !msg.corrected
                    {
                        msg.corrected = true;
                        effects.push(Effect::DeliveredAfterAll {
                            id: id.to_string(),
                            despite_cancel,
                        });
                    }
                    break 'rules;
                }
                let told_otherwise = msg.latch || matches!(msg.state, MessageState::Uncertain(_));
                msg.set_state(MessageState::Settled);
                msg.token = None;
                msg.possessed_since = None;
                msg.waiting_since = None;
                msg.held_long_sent = None;
                for at in msg.attempts.iter_mut() {
                    if !at.stage.is_final() && !at.revoked {
                        at.revoked = true;
                        at.revoke_sent = Some(now);
                        effects.push(Effect::Revoke {
                            id: id.to_string(),
                            a: at.a,
                            e: at.e,
                        });
                    }
                }
                if told_otherwise && !msg.corrected {
                    msg.corrected = true;
                    effects.push(Effect::DeliveredAfterAll {
                        id: id.to_string(),
                        despite_cancel: msg.latch,
                    });
                }
                break 'rules;
            }
            if msg.state.is_terminal() {
                break 'rules;
            }
            // Rule 2: the cancel latch.
            if msg.latch {
                msg.token = None;
                msg.possessed_since = None;
                msg.waiting_since = None;
                if all_final {
                    if msg.offer && all_not_handed {
                        effects.push(Effect::ReturnToPool {
                            id: id.to_string(),
                            a: msg.a,
                        });
                    }
                    msg.set_state(MessageState::Undeliverable(UndeliverableReason::Cancelled));
                } else {
                    msg.set_state(MessageState::CancelHeld);
                }
                break 'rules;
            }
            if none_issued {
                msg.set_state(MessageState::Queued);
                break 'rules;
            }
            // Rule 3: every attempt positively not handed.
            if all_not_handed {
                msg.token = None;
                msg.possessed_since = None;
                msg.waiting_since = None;
                if msg.state != MessageState::Queued {
                    msg.requeues += 1;
                    if msg.requeues >= limits.max_deliver || msg.issued >= limits.max_attempts {
                        msg.set_state(MessageState::Undeliverable(UndeliverableReason::MaxDeliver));
                        break 'rules;
                    }
                    if msg.offer {
                        effects.push(Effect::ReturnToPool {
                            id: id.to_string(),
                            a: msg.a,
                        });
                        msg.a += 1;
                    }
                    msg.set_state(MessageState::Queued);
                }
                break 'rules;
            }
            // Rule 4: fresh possession, issue grace or a pending resume answer.
            if held || resuming {
                if msg.token.is_some_and(|t| t.kind == TokenKind::Auto) {
                    msg.token = None;
                }
                msg.set_state(MessageState::Attempting);
                msg.waiting_since = None;
                if held {
                    msg.possessed_since.get_or_insert(now);
                } else {
                    msg.possessed_since = None;
                }
                break 'rules;
            }
            // Rule 5: possibly handed and nobody possesses it. An automatic
            // repeat needs every such attempt cleared by a spool check (§3);
            // a message that cannot go on — no check can tell, or no
            // receiver takes its repeat — waits a bounded time and is then
            // `Uncertain`, for the user to decide. Once uncertain it stays
            // so: only the user, a receipt or regained possession moves it.
            msg.possessed_since = None;
            let waiting = *msg.waiting_since.get_or_insert(now);
            let waited = now >= waiting + limits.uncertain_after();
            if msg.token.is_some() {
                if waited {
                    msg.token = None;
                    msg.set_state(MessageState::Uncertain(UncertainReason::Lapsed));
                } else {
                    msg.set_state(MessageState::Attempting);
                }
            } else if matches!(msg.state, MessageState::Uncertain(_)) {
            } else if msg.issued >= limits.max_attempts {
                msg.set_state(MessageState::Uncertain(UncertainReason::OutOfAttempts));
            } else if !cleared {
                if waited {
                    msg.set_state(MessageState::Uncertain(UncertainReason::Lapsed));
                } else {
                    msg.set_state(MessageState::Attempting);
                }
            } else if msg.auto_repeats < limits.max_auto_repeats {
                msg.set_state(MessageState::Attempting);
                msg.token = Some(RepeatToken {
                    a: msg.a,
                    revision: msg.revision,
                    kind: TokenKind::Auto,
                });
            } else {
                msg.set_state(MessageState::Uncertain(UncertainReason::Lapsed));
            }
        }
        // PRD #1666 G3: the not-started window runs from the first reported
        // possession or hand-over of this delivery cycle; anything that ends
        // the attempting ends it.
        if msg.state == MessageState::Attempting && !msg.latch {
            if witnessed {
                msg.not_started_anchor.get_or_insert(now);
            }
        } else {
            msg.not_started_anchor = None;
            msg.not_started = None;
        }
        self.reclaim(id);
    }

    fn finalize_all(&mut self, now: Instant) {
        let ids: Vec<String> = self.messages.keys().cloned().collect();
        for id in &ids {
            self.finalize(now, id);
        }
    }

    fn finalize_bound_to(&mut self, now: Instant, rx: Rx) {
        let ids: Vec<String> = self
            .messages
            .iter()
            .filter(|(_, m)| m.attempts.iter().any(|at| at.rx == rx))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            self.finalize(now, id);
        }
    }

    /// Return an attempt's evidence reservation once it is quiescent (§7):
    /// its message is terminal, it is final with no resume answer owed, and
    /// it keeps no late-commit verifier (a `HandedInert` one's is kept while
    /// the message is not settled, a `Fenced` one's until a spool check after
    /// the message ended).
    ///
    /// Finality already implies that no approval under a current epoch is
    /// outstanding, so the model carries no separate approval predicate:
    /// every path to a final stage is either the approved incarnation
    /// reporting what its callback did (the approval is spent, or revoked
    /// locally before a `NotHanded`), a resume answer or its silence (the
    /// incarnation was retired, which bumps the epoch), the retirement of a
    /// stream that closed (no sooner than an approval it sent has expired),
    /// or exit. A refused [`Self::may_hand_over`] is none of these, so it
    /// stages nothing. That a surviving old JS context honours the epoch bump
    /// and the expiry before its approved callback resumes is the adapter's
    /// obligation (inbox/opencode/031, inbox/opencode/046), not this model's.
    fn reclaim(&mut self, id: &str) {
        let Some(msg) = self.messages.get_mut(id) else {
            return;
        };
        let terminal = msg.state.is_terminal();
        let settled = msg.state == MessageState::Settled;
        for at in msg.attempts.iter_mut().filter(|at| !at.reclaimed) {
            if settled {
                at.verifier = false;
            }
            if terminal && at.stage.is_final() && !at.resume_pending && !at.verifier {
                at.reclaimed = true;
                self.evidence_reserved =
                    self.evidence_reserved.saturating_sub(EVIDENCE_PER_ATTEMPT);
                self.effects.push(Effect::Reclaimed {
                    id: id.to_string(),
                    a: at.a,
                    e: at.e,
                });
            }
        }
    }
}

impl Attempt {
    /// An earlier spool check covers nothing from here on: possession was
    /// regained, or its assignment was handed out again.
    fn recheck(&mut self) {
        self.cleared = false;
        self.mark += 1;
    }

    /// Possession was regained: an earlier spool check covers nothing
    /// published from here on.
    fn regained(&mut self) {
        self.recheck();
    }

    /// Fenced: possibly handed, and checked in the spool again before it
    /// leads to a repeat (§3), since its incarnation may have published a
    /// receipt it never reported. For the same reason it keeps its
    /// late-commit verifier until a check after its message ended
    /// ([`AttemptModel::verify_candidates`]).
    fn fence(&mut self) {
        self.stage = Stage::Fenced;
        self.verifier = true;
        self.regained();
    }
}

/// Whether a revoke of `at` is still owed an answer ([`AttemptModel::revokes_due`]).
fn owes_revoke(at: &Attempt) -> bool {
    at.revoked && !at.stage.is_final() && !at.resume_pending && !at.reclaimed
}

/// Whether `at` must be cleared by a spool check before rule 5 may repeat
/// its message: it may have been handed and may have a receipt the deck has
/// not seen (§3).
fn needs_clearance(at: &Attempt) -> bool {
    !at.resume_pending
        && matches!(
            at.stage,
            Stage::Issued | Stage::Handed | Stage::Fenced | Stage::HandedInert
        )
}

/// Whether a fresh possession snapshot of `at`'s stream receiver lists it:
/// [`possessed`] without the issue grace.
fn snapshot_holds(
    receivers: &BTreeMap<Rx, Receiver>,
    limits: &Limits,
    id: &str,
    at: &Attempt,
    now: Instant,
) -> bool {
    receivers.get(&at.rx).is_some_and(|r| {
        matches!(r.slot, Slot::OpencodeServer | Slot::OpencodeTui | Slot::Pi)
            && now < r.fresh_at + limits.possess_ttl
            && r.held.contains(&(id.to_string(), at.a, at.e))
    })
}

/// Whether `at` is possessed right now (§4). A stream attempt is possessed
/// while its live incarnation's fresh snapshot holds it, or while it is in
/// issue grace (`d` above the watermark, less than one TTL since issue). A
/// once-waiter attempt is possessed while its receiver is busy, or within one
/// ack-wait of going idle; a hook attempt while its connection is open.
fn possessed(
    receivers: &BTreeMap<Rx, Receiver>,
    limits: &Limits,
    id: &str,
    at: &Attempt,
    now: Instant,
) -> bool {
    if at.resume_pending
        || matches!(
            at.stage,
            Stage::Committed | Stage::NotHanded | Stage::Fenced
        )
    {
        return false;
    }
    let Some(r) = receivers.get(&at.rx) else {
        return false;
    };
    if r.retired {
        return false;
    }
    match r.slot {
        Slot::OpencodeServer | Slot::OpencodeTui | Slot::Pi => {
            snapshot_holds(receivers, limits, id, at, now)
                || (at.d > r.seen && now < at.issued_at + limits.possess_ttl)
        }
        Slot::OnceWaiter => {
            r.busy
                || r.idle_since
                    .is_some_and(|since| now < since + limits.ack_wait)
        }
        Slot::Hook => r.connected,
    }
}

/// How a work-done report resolved against the task index (§9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOutcome {
    Retired(String),
    AlreadyReported,
    UnknownTask,
    /// No task id: posted visibly and uncorrelated, retiring nothing.
    Uncorrelated,
}

#[derive(Debug, Clone)]
struct TaskEntry {
    worker_pane: String,
    worker_generation: u64,
    reported: bool,
}

/// `tasks: id → worker pane + generation` (§9): a no-inference prototype
/// until §13 step 5, which adds the instance and orchestration binding, the
/// commission/notice transaction, worker-exit cleanup and the bound on
/// reported tombstones. It has none of those yet and evicts nothing.
#[derive(Debug, Clone, Default)]
pub struct TaskIndex {
    tasks: BTreeMap<String, TaskEntry>,
}

impl TaskIndex {
    pub fn bind(&mut self, task_id: &str, worker_pane: &str, worker_generation: u64) {
        self.tasks.insert(
            task_id.to_string(),
            TaskEntry {
                worker_pane: worker_pane.to_string(),
                worker_generation,
                reported: false,
            },
        );
    }

    /// A report names its task, or retires nothing (§9): no inference from
    /// whichever task happens to be open.
    pub fn report(&mut self, task_id: Option<&str>, pane: &str, generation: u64) -> ReportOutcome {
        let Some(id) = task_id else {
            return ReportOutcome::Uncorrelated;
        };
        match self.tasks.get_mut(id) {
            Some(t) if t.worker_pane == pane && t.worker_generation == generation => {
                if t.reported {
                    ReportOutcome::AlreadyReported
                } else {
                    t.reported = true;
                    ReportOutcome::Retired(id.to_string())
                }
            }
            _ => ReportOutcome::UnknownTask,
        }
    }

    pub fn open(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|(_, t)| !t.reported)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    const GEN: u64 = 7;

    fn model_with(limits: Limits) -> (AttemptModel, Instant) {
        (AttemptModel::new(limits, GEN), Instant::now())
    }

    fn model() -> (AttemptModel, Instant) {
        model_with(Limits::default())
    }

    fn ttl() -> Duration {
        Limits::default().possess_ttl
    }

    fn past_ttl(t: Instant) -> Instant {
        t + ttl() + Duration::from_secs(1)
    }

    /// Issue `id` to `rx` and record a snapshot holding it, so it starts
    /// possessed.
    fn issue_held(m: &mut AttemptModel, now: Instant, id: &str, rx: Rx, seq: u64) -> Issued {
        let issued = m.issue(now, id, rx).expect("issue");
        m.snapshot(now, rx, seq, issued.d, &[(id, issued.a, issued.e)]);
        issued
    }

    /// Lapse every open attempt twice over until automatic repeats are spent:
    /// `id` ends `Uncertain(Lapsed)` with three attempts.
    fn drive_to_uncertain(m: &mut AttemptModel, t: Instant, id: &str, rx: Rx) -> Instant {
        issue_held(m, t, id, rx, 1);
        let t1 = past_ttl(t);
        m.tick(t1);
        m.issue(t1, id, rx).expect("first automatic repeat");
        let t2 = past_ttl(t1);
        m.tick(t2);
        m.issue(t2, id, rx).expect("second automatic repeat");
        let t3 = past_ttl(t2);
        m.tick(t3);
        t3
    }

    /// Scenario: An offer's first attempt is handed over and its context
    /// reloads, so the attempt is fenced; the marked repeat refuses safely;
    /// then the first attempt's commit arrives late. The offer never leaves
    /// the worker, the late commit settles it, and later stage records below
    /// or equal to a final stage change nothing.
    #[spec("inbox/attempts/001")]
    #[test]
    fn attempts_001_stages_are_monotonic_and_an_unknown_attempt_blocks_reassignment() {
        let (mut m, t) = model();
        m.post("m1", true);
        let rx1 = m.register(t, Slot::Pi, false).rx;
        let e1 = issue_held(&mut m, t, "m1", rx1, 1);
        assert_eq!(m.state("m1"), Some(MessageState::Attempting));

        let reg = m.register(t, Slot::Pi, false);
        assert_eq!(reg.resume, vec![("m1".to_string(), 1, e1.e)]);
        m.resume_answer(t, reg.rx, "m1", 1, e1.e, ResumeAnswer::Unknown);
        assert_eq!(
            m.delivery("m1").unwrap().attempt(1, e1.e).unwrap().stage,
            Stage::Fenced
        );
        assert_eq!(
            m.issue(t, "m1", reg.rx),
            Err(IssueRefusal::NothingToIssue),
            "a fenced attempt is checked in the spool before it repeats (§3)"
        );
        m.tick(t);
        let e2 = m.issue(t, "m1", reg.rx).expect("marked repeat");
        assert_eq!(e2.kind, IssueKind::AutoRepeat);
        assert!(e2.kind.marked());
        assert_eq!(
            m.stage(t, "m1", 1, e2.e, Stage::NotHanded, Some(reg.rx)),
            StageOutcome::Applied
        );
        let d = m.delivery("m1").unwrap();
        assert_eq!(d.a, 1, "a fenced sibling keeps the offer with its worker");
        assert!(
            !m.drain_effects()
                .iter()
                .any(|f| matches!(f, Effect::ReturnToPool { .. }))
        );

        assert_eq!(
            m.stage(t, "m1", 1, e1.e, Stage::Committed, None),
            StageOutcome::Applied
        );
        assert_eq!(m.state("m1"), Some(MessageState::Settled));

        m.post("m2", false);
        let x = m.issue(t, "m2", reg.rx).unwrap();
        assert_eq!(
            m.stage(t, "m2", 1, x.e, Stage::Committed, None),
            StageOutcome::Applied
        );
        let r = m.delivery("m2").unwrap().revision;
        assert_eq!(
            m.stage(t, "m2", 1, x.e, Stage::Handed, Some(reg.rx)),
            StageOutcome::NoOp
        );
        assert_eq!(
            m.stage(t, "m2", 1, x.e, Stage::Committed, None),
            StageOutcome::NoOp
        );
        assert_eq!(m.delivery("m2").unwrap().revision, r);
        assert_eq!(m.state("m2"), Some(MessageState::Settled));
    }

    /// Scenario: Deliver four messages whose attempts end Fenced,
    /// HandedInert (after Cancel), NotHanded, and NotHanded under an earlier
    /// offer assignment, then deliver each attempt's genuine commit late. The
    /// commit overrides every non-commit final stage; a cancelled message
    /// stays cancelled and its sender is corrected once; a commit over a
    /// not-handed witness is reported as a witness violation.
    #[spec("inbox/attempts/002")]
    #[test]
    fn attempts_002_a_late_commit_dominates_every_other_final_stage() {
        let (mut m, t) = model();
        let rx1 = m.register(t, Slot::Pi, false).rx;

        m.post("fenced", false);
        let f1 = issue_held(&mut m, t, "fenced", rx1, 1);
        let rx2 = m.register(t, Slot::Pi, false).rx;
        m.resume_answer(t, rx2, "fenced", 1, f1.e, ResumeAnswer::Unknown);
        m.tick(t);
        let f2 = m.issue(t, "fenced", rx2).unwrap();
        m.drain_effects();
        assert_eq!(
            m.stage(t, "fenced", 1, f1.e, Stage::Committed, None),
            StageOutcome::Applied
        );
        let d = m.delivery("fenced").unwrap();
        assert_eq!(d.attempt(1, f1.e).unwrap().stage, Stage::Committed);
        assert_eq!(d.state, MessageState::Settled);
        assert!(m.drain_effects().contains(&Effect::Revoke {
            id: "fenced".into(),
            a: 1,
            e: f2.e
        }));

        m.post("inert", false);
        let i1 = m.issue(t, "inert", rx2).unwrap();
        let r = m.delivery("inert").unwrap().revision;
        assert!(matches!(
            m.cancel(t, "inert", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        assert_eq!(m.state("inert"), Some(MessageState::CancelHeld));
        m.stage(t, "inert", 1, i1.e, Stage::HandedInert, Some(rx2));
        assert_eq!(
            m.state("inert"),
            Some(MessageState::Undeliverable(UndeliverableReason::Cancelled))
        );
        m.drain_effects();
        assert_eq!(
            m.stage(t, "inert", 1, i1.e, Stage::Committed, None),
            StageOutcome::Applied
        );
        assert_eq!(
            m.state("inert"),
            Some(MessageState::Undeliverable(UndeliverableReason::Cancelled)),
            "terminal absorption is applied separately from recording the commit"
        );
        assert_eq!(
            m.drain_effects(),
            vec![
                Effect::DeliveredAfterAll {
                    id: "inert".into(),
                    despite_cancel: true
                },
                Effect::Reclaimed {
                    id: "inert".into(),
                    a: 1,
                    e: i1.e
                }
            ]
        );
        assert_eq!(
            m.stage(t, "inert", 1, i1.e, Stage::Committed, None),
            StageOutcome::NoOp
        );
        assert!(
            m.drain_effects().is_empty(),
            "a duplicate commit corrects nothing twice"
        );
        assert_eq!(m.issue(t, "inert", rx2), Err(IssueRefusal::Terminal));

        m.post("witness", false);
        let w1 = m.issue(t, "witness", rx2).unwrap();
        m.stage(t, "witness", 1, w1.e, Stage::NotHanded, Some(rx2));
        assert_eq!(m.state("witness"), Some(MessageState::Queued));
        m.drain_effects();
        assert_eq!(
            m.stage(t, "witness", 1, w1.e, Stage::Committed, None),
            StageOutcome::Applied
        );
        assert_eq!(m.state("witness"), Some(MessageState::Settled));
        assert!(m.drain_effects().contains(&Effect::WitnessViolation {
            id: "witness".into(),
            a: 1,
            e: w1.e
        }));

        m.post("offer", true);
        let o1 = m.issue(t, "offer", rx2).unwrap();
        m.stage(t, "offer", 1, o1.e, Stage::NotHanded, Some(rx2));
        assert_eq!(m.delivery("offer").unwrap().a, 2);
        let o2 = m.issue(t, "offer", rx2).unwrap();
        assert_eq!(o2.a, 2);
        m.drain_effects();
        assert_eq!(
            m.stage(t, "offer", 1, o1.e, Stage::Committed, None),
            StageOutcome::Recorded
        );
        let effects = m.drain_effects();
        assert!(effects.contains(&Effect::AlsoReachedOldWorker {
            id: "offer".into(),
            a: 1
        }));
        assert!(effects.contains(&Effect::WitnessViolation {
            id: "offer".into(),
            a: 1,
            e: o1.e
        }));
        assert_eq!(m.state("offer"), Some(MessageState::Attempting));
    }

    /// Scenario: Cancel a possessed message, then deliver its commit; commit
    /// another and then Cancel it; Cancel a third whose revoke answer is lost
    /// across several lapse ticks and only later arrives. The first settles
    /// with a despite-Cancel correction, the second answers already settled,
    /// and the third stays cancel-held with no repeat until its revoked_ack
    /// closes it.
    #[spec("inbox/attempts/003")]
    #[test]
    fn attempts_003_cancel_holds_until_every_attempt_is_final() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;

        m.post("c1", false);
        let a = issue_held(&mut m, t, "c1", rx, 1);
        let r = m.delivery("c1").unwrap().revision;
        assert!(matches!(
            m.cancel(t, "c1", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        assert_eq!(m.state("c1"), Some(MessageState::CancelHeld));
        assert!(m.drain_effects().contains(&Effect::Revoke {
            id: "c1".into(),
            a: 1,
            e: a.e
        }));
        m.stage(t, "c1", 1, a.e, Stage::Committed, None);
        assert_eq!(m.state("c1"), Some(MessageState::Settled));
        assert!(m.drain_effects().contains(&Effect::DeliveredAfterAll {
            id: "c1".into(),
            despite_cancel: true
        }));

        m.post("c2", false);
        let b = m.issue(t, "c2", rx).unwrap();
        m.stage(t, "c2", 1, b.e, Stage::Committed, None);
        let r = m.delivery("c2").unwrap().revision;
        assert_eq!(m.cancel(t, "c2", GEN, r), ActionOutcome::AlreadySettled);
        assert_eq!(m.state("c2"), Some(MessageState::Settled));

        m.post("c3", false);
        let c = issue_held(&mut m, t, "c3", rx, 2);
        let r = m.delivery("c3").unwrap().revision;
        m.cancel(t, "c3", GEN, r);
        let mut now = t;
        for _ in 0..3 {
            now = past_ttl(now);
            m.tick(now);
            assert_eq!(m.state("c3"), Some(MessageState::CancelHeld));
            assert!(!m.delivery("c3").unwrap().repeat_scheduled());
            assert_eq!(m.issue(now, "c3", rx), Err(IssueRefusal::Cancelled));
        }
        m.stage(now, "c3", 1, c.e, Stage::NotHanded, Some(rx));
        assert_eq!(
            m.state("c3"),
            Some(MessageState::Undeliverable(UndeliverableReason::Cancelled))
        );
    }

    /// Scenario: Let a possessed attempt's snapshots stop until it lapses, so
    /// a marked repeat is scheduled; then either commit it, let fresh
    /// possession return before the hand-out, or run the finalizer several
    /// times before the hand-out. The repeat is refused after settlement,
    /// withdrawn by recovered possession, and issued and counted once.
    #[spec("inbox/attempts/004")]
    #[test]
    fn attempts_004_a_scheduled_repeat_is_revalidated_at_hand_out() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;

        m.post("settled", false);
        let s = issue_held(&mut m, t, "settled", rx, 1);
        let t1 = past_ttl(t);
        m.tick(t1);
        assert!(m.delivery("settled").unwrap().repeat_scheduled());
        m.stage(t1, "settled", 1, s.e, Stage::Committed, None);
        assert_eq!(m.issue(t1, "settled", rx), Err(IssueRefusal::Terminal));
        assert!(!m.delivery("settled").unwrap().repeat_scheduled());

        let rx2 = m.register(t1, Slot::OpencodeServer, false).rx;
        m.post("recovered", false);
        let r1 = issue_held(&mut m, t1, "recovered", rx2, 1);
        let t2 = past_ttl(t1);
        m.tick(t2);
        assert!(m.delivery("recovered").unwrap().repeat_scheduled());
        m.snapshot(t2, rx2, 2, r1.d, &[("recovered", 1, r1.e)]);
        assert!(!m.delivery("recovered").unwrap().repeat_scheduled());
        assert_eq!(
            m.issue(t2, "recovered", rx2),
            Err(IssueRefusal::NothingToIssue)
        );
        assert_eq!(m.delivery("recovered").unwrap().auto_repeats, 0);

        let rx3 = m.register(t2, Slot::OpencodeTui, false).rx;
        m.post("twice", false);
        issue_held(&mut m, t2, "twice", rx3, 1);
        let t3 = past_ttl(t2);
        m.tick(t3);
        m.finalize(t3, "twice");
        m.finalize(t3, "twice");
        m.tick(t3);
        let rep = m.issue(t3, "twice", rx3).expect("one repeat");
        assert_eq!(rep.kind, IssueKind::AutoRepeat);
        let d = m.delivery("twice").unwrap();
        assert_eq!((d.auto_repeats, d.issued), (1, 2));
        assert_eq!(m.issue(t3, "twice", rx3), Err(IssueRefusal::NothingToIssue));

        // The hand-out's own check, reached with a token whose admissibility
        // changed while nothing withdrew it: possession recorded without the
        // finalizer, the revision moved, the assignment moved.
        let (mut m, t) = model();
        let rx = m.register(t, Slot::OpencodeServer, false).rx;
        for id in ["held", "revised", "moved"] {
            m.post(id, false);
        }
        let h = m.issue(t, "held", rx).unwrap();
        let v = m.issue(t, "revised", rx).unwrap();
        let mv = m.issue(t, "moved", rx).unwrap();
        m.snapshot(
            t,
            rx,
            1,
            mv.d,
            &[("held", 1, h.e), ("revised", 1, v.e), ("moved", 1, mv.e)],
        );
        let t1 = past_ttl(t);
        m.tick(t1);
        for id in ["held", "revised", "moved"] {
            assert!(m.delivery(id).unwrap().repeat_scheduled(), "{id}");
        }
        assert_eq!(
            m.apply_snapshot(t1, rx, 2, mv.d, &[("held", 1, h.e)]),
            SnapshotOutcome::Applied
        );
        assert!(
            m.delivery("held").unwrap().repeat_scheduled(),
            "not withdrawn yet"
        );
        m.messages.get_mut("revised").unwrap().revision += 1;
        m.messages.get_mut("moved").unwrap().a += 1;
        for id in ["held", "revised", "moved"] {
            assert_eq!(m.issue(t1, id, rx), Err(IssueRefusal::StaleRepeat), "{id}");
            let d = m.delivery(id).unwrap();
            assert_eq!((d.issued, d.auto_repeats), (1, 0), "{id}");
        }
        assert!(!m.delivery("held").unwrap().repeat_scheduled());
        assert_eq!(m.state("moved"), Some(MessageState::Queued));
        let again = m
            .issue(t1, "revised", rx)
            .expect("rescheduled at the new revision");
        assert_eq!(again.kind, IssueKind::AutoRepeat);
    }

    /// Scenario: Feed a stream receiver stale and replayed snapshots, a
    /// snapshot whose watermark is below a newly issued attempt, fresh
    /// snapshots that keep omitting it past the TTL, and a pause past the TTL
    /// followed by recovery. Stale and replayed snapshots are dropped, issue
    /// grace holds the uncovered attempt until the TTL and then ends, and a
    /// paused receiver's attempt takes one marked repeat whose recovery
    /// settles on the first commit.
    #[spec("inbox/attempts/005")]
    #[test]
    fn attempts_005_snapshots_watermarks_issue_grace_and_ttl_expiry() {
        assert_eq!(
            Limits::default().possess_ttl,
            Duration::from_secs(90),
            "three default 30 s keepalives"
        );
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("m1", false);
        let e1 = m.issue(t, "m1", rx).unwrap();
        assert_eq!(
            m.snapshot(t, rx, 5, e1.d, &[("m1", 1, e1.e)]),
            SnapshotOutcome::Applied
        );
        assert_eq!(m.snapshot(t, rx, 5, e1.d, &[]), SnapshotOutcome::Dropped);
        assert_eq!(m.snapshot(t, rx, 4, e1.d, &[]), SnapshotOutcome::Dropped);
        assert_eq!(m.state("m1"), Some(MessageState::Attempting));

        m.post("m2", false);
        let e2 = m.issue(t, "m2", rx).unwrap();
        m.snapshot(t, rx, 6, e1.d, &[("m1", 1, e1.e)]);
        assert_eq!(
            m.state("m2"),
            Some(MessageState::Attempting),
            "d > seen is in issue grace"
        );
        assert!(!m.delivery("m2").unwrap().repeat_scheduled());

        m.post("m3", false);
        let e3 = m.issue(t, "m3", rx).unwrap();
        m.snapshot(t, rx, 7, e3.d, &[("m1", 1, e1.e), ("m2", 1, e2.e)]);
        assert!(
            !m.delivery("m3").unwrap().repeat_scheduled(),
            "an omission alone is no repeat: the spool is checked first (§3)"
        );
        m.tick(t);
        assert!(
            m.delivery("m3").unwrap().repeat_scheduled(),
            "omitted with d <= seen is not possessed, and checked it repeats"
        );

        let late = past_ttl(t);
        m.snapshot(late, rx, 8, e1.d, &[("m1", 1, e1.e)]);
        m.tick(late);
        let d2 = m.delivery("m2").unwrap();
        assert!(
            d2.attempt(1, e2.e).unwrap().cleared,
            "issue grace ended at the TTL"
        );
        assert!(d2.repeat_scheduled());

        let paused = past_ttl(late);
        m.tick(paused);
        assert!(m.delivery("m1").unwrap().attempt(1, e1.e).unwrap().cleared);
        let rep = m
            .issue(paused, "m1", rx)
            .expect("marked repeat after a pause");
        assert!(rep.kind.marked());
        m.snapshot(paused, rx, 9, rep.d, &[("m1", 1, e1.e), ("m1", 1, rep.e)]);
        assert_eq!(m.state("m1"), Some(MessageState::Attempting));
        m.drain_effects();
        m.stage(paused, "m1", 1, e1.e, Stage::Committed, None);
        assert_eq!(m.state("m1"), Some(MessageState::Settled));
        assert!(m.drain_effects().contains(&Effect::Revoke {
            id: "m1".into(),
            a: 1,
            e: rep.e
        }));

        // A later snapshot reporting a lower watermark does not revive the
        // issue grace an earlier covering omission ended.
        let rx2 = m.register(paused, Slot::OpencodeServer, false).rx;
        m.post("w1", false);
        m.post("w2", false);
        let w1 = m.issue(paused, "w1", rx2).unwrap();
        let w2 = m.issue(paused, "w2", rx2).unwrap();
        m.snapshot(paused, rx2, 1, w2.d, &[("w1", 1, w1.e)]);
        m.tick(paused);
        assert!(m.delivery("w2").unwrap().repeat_scheduled());
        m.snapshot(paused, rx2, 2, 0, &[("w1", 1, w1.e)]);
        assert!(
            m.delivery("w2").unwrap().repeat_scheduled(),
            "a regressed watermark does not revive issue grace"
        );
    }

    /// Scenario: A receiver observes delivery numbers out of order (2, then
    /// 1, then 4, then 3) and reports its watermark after each. The watermark
    /// only advances over a contiguous observed prefix, so an unobserved
    /// predecessor never reads as omitted.
    #[spec("inbox/attempts/006")]
    #[test]
    fn attempts_006_the_watermark_is_a_contiguous_observed_prefix() {
        let mut w = ContiguousWatermark::default();
        w.observe(2);
        assert_eq!(w.seen(), 0);
        w.observe(1);
        assert_eq!(w.seen(), 2);
        w.observe(4);
        assert_eq!(w.seen(), 2);
        w.observe(3);
        assert_eq!(w.seen(), 4);
        w.observe(2);
        assert_eq!(w.seen(), 4);
    }

    /// Scenario: The OpenCode server and TUI contexts register side by side
    /// and each takes a message; then the TUI context alone re-registers. Only
    /// the TUI's previous incarnation is retired and asked to resume; the
    /// server context keeps its attempt, snapshots and hand-over approval.
    #[spec("inbox/attempts/007")]
    #[test]
    fn attempts_007_a_registration_retires_only_its_own_slot() {
        let (mut m, t) = model();
        let server2 = m.register(t, Slot::OpencodeServer, false).rx;
        let tui2 = m.register(t, Slot::OpencodeTui, false).rx;
        m.post("s", false);
        m.post("u", false);
        let s = issue_held(&mut m, t, "s", server2, 1);
        let u = issue_held(&mut m, t, "u", tui2, 1);
        let reg = m.register(t, Slot::OpencodeTui, false);
        assert_eq!(reg.resume, vec![("u".to_string(), 1, u.e)]);
        assert_eq!(
            m.snapshot(t, server2, 2, s.d, &[("s", 1, s.e)]),
            SnapshotOutcome::Applied
        );
        assert_eq!(
            m.snapshot(t, tui2, 2, u.d, &[("u", 1, u.e)]),
            SnapshotOutcome::Dropped
        );
        assert!(m.may_hand_over("s", 1, s.e, server2).is_ok());
        assert_eq!(
            m.may_hand_over("u", 1, u.e, tui2),
            Err(HandOverRefusal::WrongReceiver)
        );
        assert!(
            !m.delivery("s")
                .unwrap()
                .attempt(1, s.e)
                .unwrap()
                .resume_pending
        );
    }

    /// Scenario: Retire a Pi context four times: its watcher reconnects with
    /// the callback queue intact and answers retained; a reloaded context
    /// answers unknown; one stays silent; one without a surviving queue
    /// answers not_handed. Retained rebinds the same attempt with no repeat
    /// and the old incarnation cannot release it; unknown and silence fence
    /// it and take a marked repeat; not_handed counts only from a surviving
    /// queue.
    #[spec("inbox/attempts/008")]
    #[test]
    fn attempts_008_retirement_waits_for_the_resume_answer() {
        let (mut m, t) = model();
        let rx1 = m.register(t, Slot::Pi, true).rx;
        m.post("kept", false);
        let k = issue_held(&mut m, t, "kept", rx1, 1);
        let reg = m.register(t, Slot::Pi, true);
        let rx2 = reg.rx;
        assert_eq!(m.state("kept"), Some(MessageState::Attempting));
        assert!(
            m.delivery("kept")
                .unwrap()
                .attempt(1, k.e)
                .unwrap()
                .resume_pending
        );
        assert_eq!(
            m.stage(t, "kept", 1, k.e, Stage::NotHanded, Some(rx1)),
            StageOutcome::WrongReceiver
        );
        assert_eq!(
            m.resume_answer(t, rx2, "kept", 1, k.e, ResumeAnswer::Retained),
            StageOutcome::Applied
        );
        let at = m.delivery("kept").unwrap().attempt(1, k.e).unwrap().clone();
        assert_eq!(
            (at.rx, at.stage, at.resume_pending),
            (rx2, Stage::Issued, false)
        );
        m.snapshot(t, rx2, 1, at.d, &[("kept", 1, k.e)]);
        m.tick(t + Duration::from_secs(5));
        assert!(!m.delivery("kept").unwrap().repeat_scheduled());
        assert_eq!(m.issue(t, "kept", rx2), Err(IssueRefusal::NothingToIssue));
        assert_eq!(
            m.stage(t, "kept", 1, k.e, Stage::NotHanded, Some(rx1)),
            StageOutcome::WrongReceiver,
            "the old incarnation cannot release the rebound attempt"
        );

        m.post("reloaded", false);
        let r = issue_held(&mut m, t, "reloaded", rx2, 2);
        let rx3 = m.register(t, Slot::Pi, false).rx;
        m.resume_answer(t, rx3, "reloaded", 1, r.e, ResumeAnswer::Unknown);
        assert_eq!(
            m.delivery("reloaded")
                .unwrap()
                .attempt(1, r.e)
                .unwrap()
                .stage,
            Stage::Fenced
        );
        m.tick(t);
        assert!(m.issue(t, "reloaded", rx3).unwrap().kind.marked());

        m.post("silent", false);
        let s = issue_held(&mut m, t, "silent", rx3, 1);
        let rx4 = m.register(t, Slot::Pi, false).rx;
        m.tick(t + Limits::default().ack_wait - Duration::from_secs(1));
        assert!(
            m.delivery("silent")
                .unwrap()
                .attempt(1, s.e)
                .unwrap()
                .resume_pending
        );
        m.tick(t + Limits::default().ack_wait + Duration::from_secs(1));
        assert_eq!(
            m.delivery("silent").unwrap().attempt(1, s.e).unwrap().stage,
            Stage::Fenced
        );

        m.post("lost-queue", false);
        let q = issue_held(&mut m, t, "lost-queue", rx4, 1);
        let rx5 = m.register(t, Slot::Pi, false).rx;
        m.resume_answer(t, rx5, "lost-queue", 1, q.e, ResumeAnswer::NotHanded);
        assert_eq!(
            m.delivery("lost-queue")
                .unwrap()
                .attempt(1, q.e)
                .unwrap()
                .stage,
            Stage::Fenced,
            "not_handed from a context whose queue did not survive is unknown"
        );
        m.post("kept-queue", false);
        let kq = issue_held(&mut m, t, "kept-queue", rx5, 2);
        let rx6 = m.register(t, Slot::Pi, true).rx;
        m.resume_answer(t, rx6, "kept-queue", 1, kq.e, ResumeAnswer::NotHanded);
        assert_eq!(m.state("kept-queue"), Some(MessageState::Queued));
    }

    /// Scenario: Approve a Pi attempt's hand-over, retire the context and
    /// rebind its callback guard; separately settle a message by a newer
    /// attempt while an older, approved attempt still owes its revoke answer,
    /// and commit an older attempt after a newer one was issued. The rebound
    /// guard refuses the earlier approval, the older attempt keeps its
    /// evidence until it is final, and the late commit lands on exactly the
    /// attempt that made it. That retirement reaches a surviving old context
    /// is the adapters' to prove (step 4), not this model's.
    #[spec("inbox/attempts/009")]
    #[test]
    fn attempts_009_reclamation_waits_for_finality_and_a_rebound_guard_refuses_old_approvals() {
        let (mut m, t) = model();
        let rx1 = m.register(t, Slot::Pi, true).rx;
        m.post("g", false);
        let g = issue_held(&mut m, t, "g", rx1, 1);
        let grant = m.may_hand_over("g", 1, g.e, rx1).expect("approved");
        let mut guard = CallbackGuard::new(grant.epoch);
        assert!(guard.may_call(&grant));
        let rx2 = m.register(t, Slot::Pi, true).rx;
        m.resume_answer(t, rx2, "g", 1, g.e, ResumeAnswer::Retained);
        guard.rebind(rx2.0);
        assert!(
            !guard.may_call(&grant),
            "rebinding voids the outstanding approval"
        );
        assert_eq!(
            m.may_hand_over("g", 1, g.e, rx1),
            Err(HandOverRefusal::WrongReceiver)
        );
        let fresh = m.may_hand_over("g", 1, g.e, rx2).expect("approved again");
        assert!(guard.may_call(&fresh));
        guard.revoke("g", 1, g.e);
        assert!(!guard.may_call(&fresh));

        let server = m.register(t, Slot::OpencodeServer, true).rx;
        let tui = m.register(t, Slot::OpencodeTui, true).rx;
        m.post("q", false);
        let before = m.evidence_reserved();
        let q1 = issue_held(&mut m, t, "q", server, 1);
        m.may_hand_over("q", 1, q1.e, server).expect("approved");
        let t1 = past_ttl(t);
        m.snapshot(t1, tui, 1, 0, &[]);
        m.tick(t1);
        let q2 = m.issue(t1, "q", tui).expect("repeat on the other context");
        m.stage(t1, "q", 1, q2.e, Stage::Committed, None);
        assert_eq!(m.state("q"), Some(MessageState::Settled));
        assert!(
            !m.delivery("q")
                .unwrap()
                .attempt(1, q1.e)
                .unwrap()
                .stage
                .is_final()
        );
        assert_eq!(
            m.evidence_reserved(),
            before + EVIDENCE_PER_ATTEMPT,
            "an attempt that is not final keeps its evidence after settlement"
        );
        let server2 = m.register(t1, Slot::OpencodeServer, false).rx;
        m.resume_answer(t1, server2, "q", 1, q1.e, ResumeAnswer::Unknown);
        assert_eq!(
            m.evidence_reserved(),
            before,
            "fenced by an unknown resume, it is final and quiescent"
        );

        m.post("old", false);
        let o1 = issue_held(&mut m, t1, "old", tui, 2);
        let t2 = past_ttl(t1);
        m.tick(t2);
        let o2 = m.issue(t2, "old", tui).unwrap();
        m.stage(t2, "old", 1, o1.e, Stage::Committed, None);
        let d = m.delivery("old").unwrap();
        assert_eq!(d.attempt(1, o1.e).unwrap().stage, Stage::Committed);
        assert_ne!(d.attempt(1, o2.e).unwrap().stage, Stage::Committed);
    }

    /// Scenario: A Claude once-waiter writes a message and exits, the user
    /// cancels it, time passes, and then the prompt hook's genuine commit
    /// arrives. The message stays cancelled with no repeat, its late-commit
    /// verifier keeps its evidence until then, and the commit corrects the
    /// sender once and releases the evidence.
    #[spec("inbox/attempts/010")]
    #[test]
    fn attempts_010_a_handed_inert_attempt_keeps_its_late_commit_verifier() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::OnceWaiter, false).rx;
        m.post("w", false);
        let w = m.issue(t, "w", rx).unwrap();
        m.stage(t, "w", 1, w.e, Stage::Handed, Some(rx));
        let at = m.delivery("w").unwrap().attempt(1, w.e).unwrap().clone();
        assert_eq!(
            at.stage,
            Stage::HandedInert,
            "the waiter exits after writing"
        );
        assert_eq!(m.state("w"), Some(MessageState::Attempting));
        let r = m.delivery("w").unwrap().revision;
        m.cancel(t, "w", GEN, r);
        assert_eq!(
            m.state("w"),
            Some(MessageState::Undeliverable(UndeliverableReason::Cancelled))
        );
        let later = t + Duration::from_secs(4 * 3600);
        m.tick(later);
        assert_eq!(m.evidence_reserved(), EVIDENCE_PER_ATTEMPT);
        assert!(m.delivery("w").unwrap().attempt(1, w.e).unwrap().verifier);
        assert_eq!(m.issue(later, "w", rx), Err(IssueRefusal::Terminal));
        m.drain_effects();
        assert_eq!(
            m.stage(later, "w", 1, w.e, Stage::Committed, None),
            StageOutcome::Applied
        );
        assert_eq!(
            m.drain_effects(),
            vec![
                Effect::DeliveredAfterAll {
                    id: "w".into(),
                    despite_cancel: true
                },
                Effect::Reclaimed {
                    id: "w".into(),
                    a: 1,
                    e: w.e
                }
            ]
        );
        assert_eq!(m.evidence_reserved(), 0);
    }

    /// Scenario: Two clients act on one uncertain message at the same
    /// revision: Retry twice, then Cancel at the stale revision, then Cancel
    /// twice at the current one, and an action naming another generation.
    /// The repeated verb gets the same answer, the stale one is told the
    /// current state, and the other generation is refused.
    #[spec("inbox/attempts/011")]
    #[test]
    fn attempts_011_actions_compare_on_the_revision_and_are_idempotent() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("u", false);
        let now = drive_to_uncertain(&mut m, t, "u", rx);
        assert_eq!(
            m.state("u"),
            Some(MessageState::Uncertain(UncertainReason::Lapsed))
        );
        let r = m.delivery("u").unwrap().revision;
        let first = m.retry(now, "u", GEN, r);
        assert!(matches!(first, ActionOutcome::Applied { .. }));
        assert_eq!(m.retry(now, "u", GEN, r), first);
        let cur = m.delivery("u").unwrap().revision;
        assert_eq!(
            m.cancel(now, "u", GEN, r),
            ActionOutcome::Stale {
                state: MessageState::Attempting,
                revision: cur
            }
        );
        let c = m.cancel(now, "u", GEN, cur);
        assert!(matches!(c, ActionOutcome::Applied { .. }));
        assert_eq!(m.cancel(now, "u", GEN, cur), c);
        assert_eq!(
            m.retry(now, "u", GEN + 1, cur),
            ActionOutcome::WrongGeneration
        );

        // An unsuccessful action at the new revision does not replace the
        // last applied one: the original client's retransmitted Retry still
        // gets its own answer.
        let rx2 = m.register(now, Slot::OpencodeServer, false).rx;
        m.post("v", false);
        let now = drive_to_uncertain(&mut m, now, "v", rx2);
        let r = m.delivery("v").unwrap().revision;
        let applied = m.retry(now, "v", GEN, r);
        assert!(matches!(applied, ActionOutcome::Applied { .. }));
        let r2 = m.delivery("v").unwrap().revision;
        assert_eq!(
            m.retry(now, "v", GEN, r2),
            ActionOutcome::NotApplicable {
                state: MessageState::Attempting
            }
        );
        assert_eq!(
            m.retry(now, "v", GEN, r),
            applied,
            "the retransmitted Retry gets its original answer"
        );
    }

    /// Scenario: One message has four positively refused hand-outs and then
    /// two that lapse; another is retried by the user while held for an hour;
    /// a third is refused five times. Every issuance path draws on one
    /// six-attempt budget: the first ends uncertain as out of attempts (no
    /// seventh), refuses Retry, still accepts Cancel and a late commit; the
    /// held_long Retry counts; the third becomes undeliverable at max_deliver.
    #[spec("inbox/attempts/012")]
    #[test]
    fn attempts_012_one_attempt_budget_spans_every_issuance_path() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("mix", false);
        for _ in 0..4 {
            let x = m.issue(t, "mix", rx).unwrap();
            m.stage(t, "mix", 1, x.e, Stage::NotHanded, Some(rx));
        }
        assert_eq!(m.delivery("mix").unwrap().requeues, 4);
        let e5 = issue_held(&mut m, t, "mix", rx, 1);
        let t1 = past_ttl(t);
        m.tick(t1);
        let e6 = m.issue(t1, "mix", rx).unwrap();
        assert_eq!(e6.kind, IssueKind::AutoRepeat);
        let t2 = past_ttl(t1);
        m.tick(t2);
        let d = m.delivery("mix").unwrap();
        assert_eq!(d.issued, 6);
        assert_eq!(
            d.state,
            MessageState::Uncertain(UncertainReason::OutOfAttempts)
        );
        assert!(!d.repeat_scheduled());
        assert_eq!(m.issue(t2, "mix", rx), Err(IssueRefusal::NothingToIssue));
        let r = m.delivery("mix").unwrap().revision;
        assert_eq!(m.retry(t2, "mix", GEN, r), ActionOutcome::RetryLimit);
        assert!(matches!(
            m.cancel(t2, "mix", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        assert_eq!(m.state("mix"), Some(MessageState::CancelHeld));
        m.stage(t2, "mix", 1, e5.e, Stage::Committed, None);
        assert_eq!(m.state("mix"), Some(MessageState::Settled));

        m.post("held", false);
        let h = issue_held(&mut m, t, "held", rx, 2);
        let mut now = t;
        let mut seq = 3;
        while now < t + HELD_LONG_AFTER {
            now += Duration::from_secs(30);
            seq += 1;
            m.snapshot(now, rx, seq, h.d, &[("held", 1, h.e)]);
            m.tick(now);
        }
        let r = m.delivery("held").unwrap().revision;
        assert!(matches!(
            m.retry(now, "held", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        let manual = m
            .issue(now, "held", rx)
            .expect("manual repeat while possessed");
        assert_eq!(manual.kind, IssueKind::ManualRetry);
        assert_eq!(m.delivery("held").unwrap().issued, 2);

        m.post("refused", false);
        for _ in 0..crate::inbox::DEFAULT_MAX_DELIVER {
            let x = m.issue(now, "refused", rx).unwrap();
            m.stage(now, "refused", 1, x.e, Stage::NotHanded, Some(rx));
        }
        assert_eq!(
            m.state("refused"),
            Some(MessageState::Undeliverable(UndeliverableReason::MaxDeliver))
        );
    }

    /// PR #1735 review (4298669219): a message that ran out of attempts on
    /// not-handed reports, whose last attempt then commits after all — the
    /// receiver contradicting its own report — is corrected as delivered
    /// after all, once, without being cancelled.
    #[test]
    fn a_commit_after_running_out_of_attempts_corrects_the_message() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("refused", false);
        let mut issued = Vec::new();
        for _ in 0..crate::inbox::DEFAULT_MAX_DELIVER {
            let x = m.issue(t, "refused", rx).unwrap();
            m.stage(t, "refused", 1, x.e, Stage::NotHanded, Some(rx));
            issued.push(x.e);
        }
        assert_eq!(
            m.state("refused"),
            Some(MessageState::Undeliverable(UndeliverableReason::MaxDeliver))
        );
        m.drain_effects();
        m.stage(
            t,
            "refused",
            1,
            issued[issued.len() - 1],
            Stage::Committed,
            None,
        );
        let effects = m.drain_effects();
        assert!(effects.contains(&Effect::DeliveredAfterAll {
            id: "refused".into(),
            despite_cancel: false
        }));
        assert_eq!(
            m.state("refused"),
            Some(MessageState::Settled),
            "the commit settles the message it contradicts"
        );
        m.stage(t, "refused", 1, issued[0], Stage::Committed, None);
        assert!(
            !m.drain_effects()
                .iter()
                .any(|effect| matches!(effect, Effect::DeliveredAfterAll { .. })),
            "corrected once"
        );
    }

    /// Scenario: Keep one attempt freshly possessed for over two hours with
    /// no commit, ticking every 30 seconds. One held_long attention notice is
    /// raised for the revision, no automatic repeat is scheduled and nothing
    /// is handed out again.
    #[spec("inbox/attempts/013")]
    #[test]
    fn attempts_013_held_long_is_attention_once_per_revision_without_replay() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("long", false);
        let l = issue_held(&mut m, t, "long", rx, 1);
        let mut now = t;
        let mut seq = 1;
        let mut notices = 0;
        while now < t + 2 * HELD_LONG_AFTER + Duration::from_secs(60) {
            now += Duration::from_secs(30);
            seq += 1;
            m.snapshot(now, rx, seq, l.d, &[("long", 1, l.e)]);
            m.tick(now);
            notices += m
                .drain_effects()
                .iter()
                .filter(|f| matches!(f, Effect::HeldLong { .. }))
                .count();
            assert!(!m.delivery("long").unwrap().repeat_scheduled());
        }
        assert_eq!(notices, 1);
        assert_eq!(m.state("long"), Some(MessageState::Attempting));
        assert_eq!(m.issue(now, "long", rx), Err(IssueRefusal::NothingToIssue));
        assert_eq!(m.delivery("long").unwrap().issued, 1);
    }

    /// Scenario: With room for two attempts' evidence, settle one message
    /// normally, settle another while its repeat's revoke answer is still
    /// owed, take a third, and queue a fourth; then answer the revoke and
    /// commit the third. A normal success reclaims its evidence, the fourth
    /// waits Queued while hand-outs are paused, and commit and revoke
    /// processing still run and make room for it.
    #[spec("inbox/attempts/014")]
    #[test]
    fn attempts_014_the_evidence_budget_pauses_hand_outs_not_cleanup() {
        let limits = Limits {
            evidence_limit: 2 * EVIDENCE_PER_ATTEMPT,
            ..Limits::default()
        };
        let (mut m, t) = model_with(limits);
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("ok", false);
        let ok = m.issue(t, "ok", rx).unwrap();
        assert_eq!(m.evidence_reserved(), EVIDENCE_PER_ATTEMPT);
        m.stage(t, "ok", 1, ok.e, Stage::Committed, None);
        assert_eq!(
            m.evidence_reserved(),
            0,
            "a normal success reclaims its evidence"
        );

        m.post("owed", false);
        let o1 = issue_held(&mut m, t, "owed", rx, 1);
        let t1 = past_ttl(t);
        m.tick(t1);
        let o2 = m.issue(t1, "owed", rx).unwrap();
        m.stage(t1, "owed", 1, o1.e, Stage::Committed, None);
        assert_eq!(m.state("owed"), Some(MessageState::Settled));
        assert_eq!(m.evidence_reserved(), EVIDENCE_PER_ATTEMPT);

        m.post("taken", false);
        let tk = m.issue(t1, "taken", rx).unwrap();
        m.post("waiting", false);
        m.drain_effects();
        assert_eq!(
            m.issue(t1, "waiting", rx),
            Err(IssueRefusal::EvidencePaused)
        );
        assert_eq!(m.state("waiting"), Some(MessageState::Queued));
        assert!(m.drain_effects().contains(&Effect::EvidencePaused {
            reserved: 2 * EVIDENCE_PER_ATTEMPT
        }));

        m.stage(t1, "owed", 1, o2.e, Stage::NotHanded, Some(rx));
        assert_eq!(m.evidence_reserved(), EVIDENCE_PER_ATTEMPT);
        m.stage(t1, "taken", 1, tk.e, Stage::Committed, None);
        assert_eq!(m.evidence_reserved(), 0);
        assert!(m.issue(t1, "waiting", rx).is_ok());
    }

    /// Scenario: An orchestrator delegates task t1, the worker reports it by
    /// id, the orchestrator delegates t2, and the worker then sends two
    /// reports with no task id, one naming t1 again, one naming an unknown
    /// task and one from another generation of the pane. Reports without an
    /// id retire nothing and t2 stays open.
    #[spec("inbox/attempts/015")]
    #[test]
    fn attempts_015_a_report_without_a_task_id_retires_nothing() {
        let mut tasks = TaskIndex::default();
        tasks.bind("t1", "2", 1);
        assert_eq!(
            tasks.report(Some("t1"), "2", 1),
            ReportOutcome::Retired("t1".into())
        );
        tasks.bind("t2", "2", 1);
        assert_eq!(tasks.report(None, "2", 1), ReportOutcome::Uncorrelated);
        assert_eq!(tasks.report(None, "2", 1), ReportOutcome::Uncorrelated);
        assert_eq!(tasks.open(), vec!["t2".to_string()]);
        assert_eq!(
            tasks.report(Some("t1"), "2", 1),
            ReportOutcome::AlreadyReported
        );
        assert_eq!(tasks.report(Some("t9"), "2", 1), ReportOutcome::UnknownTask);
        assert_eq!(tasks.report(Some("t2"), "2", 2), ReportOutcome::UnknownTask);
        assert_eq!(tasks.report(Some("t2"), "3", 1), ReportOutcome::UnknownTask);
        assert_eq!(tasks.open(), vec!["t2".to_string()]);
    }

    /// Scenario: A generation exits holding a possessed direct message, a
    /// queued one and an assigned offer. Open attempts are fenced, the live
    /// messages become undeliverable for exit, the offer returns to its pool,
    /// all evidence is released, and nothing is handed out or committed
    /// afterwards.
    #[spec("inbox/attempts/016")]
    #[test]
    fn attempts_016_generation_exit_fences_and_releases_everything() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("live", false);
        let l = issue_held(&mut m, t, "live", rx, 1);
        m.post("queued", false);
        m.post("offer", true);
        m.issue(t, "offer", rx).unwrap();
        m.drain_effects();
        m.exit(t);
        let d = m.delivery("live").unwrap();
        assert_eq!(d.attempt(1, l.e).unwrap().stage, Stage::Fenced);
        for id in ["live", "queued", "offer"] {
            assert_eq!(
                m.state(id),
                Some(MessageState::Undeliverable(UndeliverableReason::Exited))
            );
        }
        assert!(m.drain_effects().contains(&Effect::ReturnToPool {
            id: "offer".into(),
            a: 1
        }));
        assert_eq!(m.evidence_reserved(), 0);
        assert_eq!(m.issue(t, "queued", rx), Err(IssueRefusal::Exited));
        assert_eq!(
            m.stage(t, "live", 1, l.e, Stage::Committed, None),
            StageOutcome::Exited
        );
    }

    /// Scenario: A Claude once-waiter's attempt is handed over and Claude
    /// reports an error stop and then Idle within one ack-wait; separately it
    /// goes busy, idle, busy again and idle; and a hook receiver's fetch
    /// connection closes while it is reported busy. A repeated idle report
    /// does not extend the one idle grace, a resumed busy interval restarts
    /// it, and a hook's possession is its open connection, not a busy
    /// interval.
    #[spec("inbox/attempts/017")]
    #[test]
    fn attempts_017_one_idle_grace_and_the_hook_connection() {
        let (mut m, t) = model();
        let wait = Limits::default().ack_wait;
        let s = Duration::from_secs(1);
        let rx = m.register(t, Slot::OnceWaiter, false).rx;
        m.post("dup", false);
        let d = m.issue(t, "dup", rx).unwrap();
        m.stage(t, "dup", 1, d.e, Stage::Handed, Some(rx));
        m.busy(t, rx, false);
        m.busy(t + wait - s, rx, false);
        m.tick(t + wait - s);
        assert!(!m.delivery("dup").unwrap().repeat_scheduled());
        let t0 = t + wait + s;
        m.tick(t0);
        assert!(
            m.delivery("dup").unwrap().repeat_scheduled(),
            "a repeated idle report does not extend the grace"
        );

        let rx2 = m.register(t0, Slot::OnceWaiter, false).rx;
        m.post("cycle", false);
        let c = m.issue(t0, "cycle", rx2).unwrap();
        m.stage(t0, "cycle", 1, c.e, Stage::Handed, Some(rx2));
        m.busy(t0, rx2, true);
        m.busy(t0 + s, rx2, false);
        m.busy(t0 + 3 * s, rx2, true);
        m.busy(t0 + 5 * s, rx2, false);
        m.tick(t0 + 5 * s + wait - s);
        assert!(
            !m.delivery("cycle").unwrap().repeat_scheduled(),
            "a resumed busy interval restarts the grace"
        );
        m.tick(t0 + 5 * s + wait + s);
        assert!(m.delivery("cycle").unwrap().repeat_scheduled());

        let hook = m.register(t0, Slot::Hook, false).rx;
        m.post("hook", false);
        m.issue(t0, "hook", hook).unwrap();
        let later = t0 + 4 * wait;
        m.tick(later);
        assert_eq!(m.state("hook"), Some(MessageState::Attempting));
        assert!(!m.delivery("hook").unwrap().repeat_scheduled());
        m.busy(later, hook, true);
        m.connection_closed(later, hook);
        m.tick(later);
        assert!(
            m.delivery("hook").unwrap().repeat_scheduled(),
            "a closed fetch connection holds nothing, whatever the status"
        );
    }

    /// Scenario: On an OpenCode and on a Pi receiver alike, a stream holds an attempt in a fresh snapshot and closes at
    /// once. No clock pass runs until two TTLs later; that pass finds no
    /// receipt, clears the attempt and schedules an automatic repeat. At three
    /// TTLs the next pass retires the closed stream and fences the attempt,
    /// which withdraws its clearance, while the repeat token, only one TTL
    /// old, stays. A new receiver then registers in the same slot. The issue
    /// is refused as a stale repeat, because the fenced attempt has not been
    /// checked again; this is the one guard that refuses it (R8F-02). The
    /// next pass checks it, and only then is the repeat handed out.
    #[spec("inbox/boundary/037")]
    #[test]
    fn boundary_037_a_late_pass_fence_leaves_only_the_issue_time_clearance_check() {
        for slot in [Slot::OpencodeTui, Slot::Pi] {
            let (mut m, t) = model();
            let ttl = ttl();
            m.post("m", false);
            let rx = m.register(t, slot, false).rx;
            issue_held(&mut m, t, "m", rx, 1);
            m.stream_closed(t, rx);

            m.tick(t + ttl * 2);
            let msg = m.delivery("m").unwrap();
            assert!(
                msg.attempt(1, 1).unwrap().cleared,
                "the late pass clears it"
            );
            assert!(msg.repeat_scheduled(), "and schedules the repeat");

            let retired = t + Limits::default().closed_retire_after();
            m.tick_cleared(retired, &[]);
            let msg = m.delivery("m").unwrap();
            let at = msg.attempt(1, 1).unwrap();
            assert_eq!(at.stage, Stage::Fenced, "the closed stream was retired");
            assert!(!at.cleared, "the fence withdrew the clearance");
            assert!(
                msg.repeat_scheduled(),
                "the token is kept, not yet uncertain"
            );

            let successor = m.register(retired, slot, false);
            assert!(successor.resume.is_empty(), "retired: nothing to resume");
            assert!(m.delivery("m").unwrap().repeat_scheduled());
            assert_eq!(
                m.issue(retired, "m", successor.rx),
                Err(IssueRefusal::StaleRepeat),
                "a repeat on a check the fence withdrew"
            );

            m.tick(retired);
            let repeat = m.issue(retired, "m", successor.rx).expect("checked again");
            assert_eq!(repeat.kind, IssueKind::AutoRepeat);
        }
    }

    /// Scenario: A Pi attempt is approved and that approval is kept live;
    /// the user cancels, and a second check of the same attempt is refused
    /// before the revoke reaches the context. Likewise a check refused
    /// because a sibling attempt committed. Neither refusal makes the attempt
    /// final: Cancel stays held and the evidence stays reserved until the
    /// receiver revokes locally and reports `NotHanded` itself.
    #[spec("inbox/attempts/018")]
    #[test]
    fn attempts_018_a_refused_check_is_no_not_handed_witness() {
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("m", false);
        let at = issue_held(&mut m, t, "m", rx, 1);
        let grant = m.may_hand_over("m", 1, at.e, rx).expect("approved");
        let mut guard = CallbackGuard::new(grant.epoch);
        let r = m.delivery("m").unwrap().revision;
        assert!(matches!(
            m.cancel(t, "m", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        m.drain_effects();
        assert_eq!(
            m.may_hand_over("m", 1, at.e, rx),
            Err(HandOverRefusal::Revoked)
        );
        assert_eq!(
            m.delivery("m").unwrap().attempt(1, at.e).unwrap().stage,
            Stage::Issued,
            "a refusal is not a witness"
        );
        assert_eq!(m.state("m"), Some(MessageState::CancelHeld));
        assert_eq!(m.evidence_reserved(), EVIDENCE_PER_ATTEMPT);
        assert!(
            !m.drain_effects()
                .iter()
                .any(|e| matches!(e, Effect::Reclaimed { .. })),
            "no reclamation while the earlier approval can still call"
        );
        assert!(guard.may_call(&grant), "the earlier approval is still live");
        let past = past_ttl(t);
        m.tick(past);
        assert_eq!(m.state("m"), Some(MessageState::CancelHeld));
        assert_eq!(m.evidence_reserved(), EVIDENCE_PER_ATTEMPT);

        guard.revoke("m", 1, at.e);
        assert!(!guard.may_call(&grant));
        m.stage(past, "m", 1, at.e, Stage::NotHanded, Some(rx));
        assert_eq!(
            m.state("m"),
            Some(MessageState::Undeliverable(UndeliverableReason::Cancelled))
        );
        assert_eq!(m.evidence_reserved(), 0);
        assert!(m.drain_effects().contains(&Effect::Reclaimed {
            id: "m".into(),
            a: 1,
            e: at.e
        }));

        let server = m.register(past, Slot::OpencodeServer, false).rx;
        let tui = m.register(past, Slot::OpencodeTui, false).rx;
        m.post("c", false);
        let first = issue_held(&mut m, past, "c", server, 1);
        let later = past_ttl(past);
        m.snapshot(later, server, 2, first.d, &[]);
        m.snapshot(later, tui, 1, 0, &[]);
        m.tick(later);
        let second = m.issue(later, "c", tui).expect("marked repeat");
        m.stage(later, "c", 1, second.e, Stage::Committed, None);
        let reserved = m.evidence_reserved();
        assert_eq!(
            m.may_hand_over("c", 1, first.e, server),
            Err(HandOverRefusal::Committed)
        );
        assert!(
            !m.delivery("c")
                .unwrap()
                .attempt(1, first.e)
                .unwrap()
                .stage
                .is_final(),
            "a refusal for a committed assignment is not a witness either"
        );
        assert_eq!(m.evidence_reserved(), reserved);
    }

    /// Scenario: On an OpenCode and on a Pi receiver alike, a watcher is handed a message and dies, its
    /// replacement dies before answering, so the attempt is fenced, and the
    /// user cancels the message; a spool check for its commit is taken,
    /// another replacement registers and is handed the attempt's commit
    /// authority, and that check, like one taken after the registration,
    /// finds nothing. That replacement dies before publishing, the next one
    /// is handed the attempt again, and the commit it publishes tells the
    /// sender the message was delivered after all and returns the evidence
    /// once. Once a replacement holding the hand-back is retired with no
    /// successor, a check that finds nothing returns the evidence.
    #[spec("inbox/attempts/019")]
    #[test]
    fn attempts_019_a_handed_back_fenced_commit_keeps_its_verifier_while_its_replay_is_outstanding()
    {
        for slot in [Slot::OpencodeTui, Slot::Pi] {
            let fenced_and_cancelled = || {
                let (mut m, t) = model();
                let first = m.register(t, slot, false).rx;
                m.post("m", false);
                let at = issue_held(&mut m, t, "m", first, 1);
                m.register(t, slot, false);
                let t = t + Limits::default().ack_wait;
                m.tick(t);
                assert_eq!(
                    m.delivery("m").unwrap().attempt(1, at.e).unwrap().stage,
                    Stage::Fenced,
                    "precondition: the unanswered resume fenced the attempt"
                );
                let r = m.delivery("m").unwrap().revision;
                assert!(matches!(
                    m.cancel(t, "m", GEN, r),
                    ActionOutcome::Applied { .. }
                ));
                assert!(m.state("m").unwrap().is_terminal());
                assert_eq!(m.verify_candidates().len(), 1, "precondition: verifying");
                m.drain_effects();
                (m, t, at)
            };
            let reclaimed = |effects: &[Effect]| {
                effects
                    .iter()
                    .filter(|effect| matches!(effect, Effect::Reclaimed { .. }))
                    .count()
            };
            let handed_back = |e: u32| vec![("m".to_string(), 1, e)];

            let (mut m, t, at) = fenced_and_cancelled();
            let before = m.verify_candidates();
            let replacement = m.register(t, slot, false);
            assert_eq!(replacement.correctable, handed_back(at.e));
            m.tick_verified(&before);
            let after = m.verify_candidates();
            m.tick_verified(&after);
            assert_eq!(
                reclaimed(&m.drain_effects()),
                0,
                "an absence check reclaimed a commit handed back for publication"
            );
            assert_eq!(m.evidence_reserved(), EVIDENCE_PER_ATTEMPT);
            m.stream_closed(t, replacement.rx);
            let next = m.register(t, slot, false);
            assert_eq!(
                next.correctable,
                handed_back(at.e),
                "the replay was lost with the replacement that died before publishing it"
            );
            assert!(
                m.may_hand_over("m", 1, at.e, next.rx).is_err(),
                "a final attempt is not callable"
            );
            assert_eq!(
                m.stage(t, "m", 1, at.e, Stage::Committed, None),
                StageOutcome::Applied
            );
            assert_eq!(
                m.drain_effects(),
                vec![
                    Effect::DeliveredAfterAll {
                        id: "m".into(),
                        despite_cancel: true
                    },
                    Effect::Reclaimed {
                        id: "m".into(),
                        a: 1,
                        e: at.e
                    }
                ]
            );
            assert_eq!(m.evidence_reserved(), 0);
            assert_eq!(
                m.stage(t, "m", 1, at.e, Stage::Committed, None),
                StageOutcome::NoOp
            );
            assert!(m.drain_effects().is_empty());

            let (mut m, t, at) = fenced_and_cancelled();
            let replacement = m.register(t, slot, false);
            assert_eq!(replacement.correctable, handed_back(at.e));
            m.stream_closed(t, replacement.rx);
            let gone = t + Limits::default().closed_retire_after();
            m.tick(gone);
            let checks = m.verify_candidates();
            assert_eq!(checks.len(), 1, "verification resumes once no one holds it");
            m.tick_verified(&checks);
            assert_eq!(reclaimed(&m.drain_effects()), 1);
            assert_eq!(m.evidence_reserved(), 0);
            assert!(
                m.register(gone, slot, false).correctable.is_empty(),
                "a reclaimed attempt is not handed back"
            );
        }
    }

    /// The revokes in `effects`, as `(a, e)` pairs of message `id`.
    fn revokes(effects: &[Effect], id: &str) -> Vec<(u32, u32)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Revoke { id: of, a, e } if of == id => Some((*a, *e)),
                _ => None,
            })
            .collect()
    }

    /// A message handed to a live receiver and cancelled, its revoke sent
    /// and never answered. Possession lasts far longer than the ack-wait, so
    /// nothing else moves while the clock runs.
    fn cancelled_unanswered(slot: Slot) -> (AttemptModel, Instant, Rx, Issued) {
        let limits = Limits {
            possess_ttl: Duration::from_secs(24 * 60 * 60),
            ..Limits::default()
        };
        let (mut m, t) = model_with(limits);
        m.post("m1", false);
        let rx = m.register(t, slot, false).rx;
        let issued = issue_held(&mut m, t, "m1", rx, 1);
        let r = m.delivery("m1").unwrap().revision;
        assert!(matches!(
            m.cancel(t, "m1", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        assert_eq!(revokes(&m.drain_effects(), "m1"), vec![(1, issued.e)]);
        (m, t, rx, issued)
    }

    /// Scenario: On an OpenCode and on a Pi receiver alike, a cancelled message's revoke is never answered by its live
    /// receiver. The clock runs through several ack-waits, ticking twice in
    /// each: the revoke is sent again exactly once per ack-wait, the attempt
    /// stays open and keeps its evidence reserved, nothing is reclaimed, and
    /// the message stays held for the answer.
    #[spec("inbox/boundary/041")]
    #[test]
    fn boundary_041_an_unanswered_revoke_is_resent_once_per_ack_wait_with_nothing_made_final() {
        for slot in [Slot::OpencodeTui, Slot::Pi] {
            let (mut m, t, _rx, issued) = cancelled_unanswered(slot);
            let ack_wait = Limits::default().ack_wait;
            let reserved = m.evidence_reserved();
            for round in 1..=4u32 {
                let due = t + ack_wait * round;
                m.tick(due);
                m.tick(due + ack_wait / 2);
                let effects = m.drain_effects();
                assert_eq!(
                    revokes(&effects, "m1"),
                    vec![(1, issued.e)],
                    "round {round}: one re-send per ack-wait"
                );
                assert!(
                    !effects
                        .iter()
                        .any(|effect| matches!(effect, Effect::Reclaimed { .. })),
                    "round {round}: a re-send reclaims nothing"
                );
                let at = m.delivery("m1").unwrap().attempt(1, issued.e).unwrap();
                assert!(!at.stage.is_final(), "round {round}: a send is no answer");
                assert!(!at.reclaimed, "round {round}");
                assert_eq!(m.evidence_reserved(), reserved, "round {round}");
                assert_eq!(m.state("m1"), Some(MessageState::CancelHeld));
            }
        }
    }

    /// Scenario: On an OpenCode and on a Pi receiver alike, a cancelled message's revoke is sent and the clock ticks
    /// just short of one ack-wait later: nothing is sent again. At the
    /// ack-wait, it is.
    #[spec("inbox/boundary/042")]
    #[test]
    fn boundary_042_no_revoke_is_resent_before_its_ack_wait() {
        for slot in [Slot::OpencodeTui, Slot::Pi] {
            let (mut m, t, _rx, issued) = cancelled_unanswered(slot);
            let ack_wait = Limits::default().ack_wait;
            m.tick(t);
            m.tick(t + ack_wait - Duration::from_millis(1));
            assert!(
                revokes(&m.drain_effects(), "m1").is_empty(),
                "re-sent before its ack-wait"
            );
            m.tick(t + ack_wait);
            assert_eq!(revokes(&m.drain_effects(), "m1"), vec![(1, issued.e)]);
        }
    }

    /// Scenario: On an OpenCode and on a Pi receiver alike, a cancelled message's revoke is sent again once, and the
    /// receiver then answers it — `already_handed` for one message,
    /// `revoked_ack` for another — or the attempt's own commit arrives for a
    /// third. Each attempt is final, the clock runs on through several
    /// ack-waits, and no revoke is sent again.
    #[spec("inbox/boundary/043")]
    #[test]
    fn boundary_043_revokes_stop_once_the_attempt_is_final() {
        for slot in [Slot::OpencodeTui, Slot::Pi] {
            let ack_wait = Limits::default().ack_wait;
            for answer in [Stage::HandedInert, Stage::NotHanded, Stage::Committed] {
                let (mut m, t, rx, issued) = cancelled_unanswered(slot);
                m.tick(t + ack_wait);
                assert_eq!(revokes(&m.drain_effects(), "m1"), vec![(1, issued.e)]);
                let from = (answer != Stage::Committed).then_some(rx);
                assert_eq!(
                    m.stage(t + ack_wait, "m1", 1, issued.e, answer, from),
                    StageOutcome::Applied,
                    "{answer:?}"
                );
                for round in 2..=5u32 {
                    m.tick(t + ack_wait * round);
                }
                assert!(
                    revokes(&m.drain_effects(), "m1").is_empty(),
                    "{answer:?}: a final attempt was revoked again"
                );
                assert_eq!(
                    m.delivery("m1")
                        .unwrap()
                        .attempt(1, issued.e)
                        .unwrap()
                        .stage,
                    answer
                );
            }
        }
    }

    const STEP: Duration = Duration::from_secs(30);

    fn idle() -> TurnView {
        TurnView::default()
    }

    /// Register `slot`, post `id`, hand it out possessed and staged Handed.
    fn handed(m: &mut AttemptModel, t: Instant, slot: Slot, id: &str) -> (Rx, Issued) {
        let rx = m.register(t, slot, false).rx;
        m.post(id, false);
        let issued = issue_held(m, t, id, rx, 1);
        m.stage(t, id, issued.a, issued.e, Stage::Handed, Some(rx));
        (rx, issued)
    }

    /// Keep `held` possessed by `rx` at `now` and tick against `turn`.
    fn step(
        m: &mut AttemptModel,
        now: Instant,
        rx: Rx,
        seq: &mut u64,
        held: &[(&str, u32, u32)],
        turn: TurnView,
    ) {
        *seq += 1;
        m.snapshot(now, rx, *seq, 0, held);
        m.tick_turn(now, &[], turn);
    }

    /// Scenario: A deck message is handed to a Pi receiver whose agent stays
    /// idle, and the clock ticks every 30 seconds with the receiver keeping
    /// it possessed. The not-started attention appears once the window has
    /// passed since the hand-over (or since a later turn end), the clock is
    /// told when it falls due, and nothing is issued, scheduled or repeated at
    /// any point; the same message on an OpenCode receiver never raises it.
    #[spec("inbox/attempts/020")]
    #[test]
    fn attempts_020_a_pi_message_with_no_turn_started_raises_attention_and_nothing_else() {
        let window = Limits::default().not_started_after;
        let (mut m, t) = model();
        let (rx, i) = handed(&mut m, t, Slot::Pi, "m");
        let held = [("m", i.a, i.e)];
        assert_eq!(m.not_started_due_at(idle()), Some(t + window));
        let later = TurnView {
            busy: false,
            idle_since: Some(t + Duration::from_secs(60)),
        };
        assert_eq!(
            m.not_started_due_at(later),
            Some(t + Duration::from_secs(60) + window),
            "a turn that ended after the hand-over starts the window again"
        );
        let mut now = t;
        let mut seq = 1;
        while now < t + 2 * window {
            now += STEP;
            step(&mut m, now, rx, &mut seq, &held, idle());
            let d = m.delivery("m").unwrap();
            assert_eq!(d.not_started(), now >= t + window, "at {:?}", now - t);
            assert!(!d.repeat_scheduled(), "the window schedules no repeat");
            assert_eq!(d.issued, 1);
            assert_eq!(d.state, MessageState::Attempting);
            assert_eq!(m.drain_effects(), vec![], "the window has no effect");
            assert_eq!(m.issue(now, "m", rx), Err(IssueRefusal::NothingToIssue));
        }
        assert_eq!(
            m.not_started_due_at(idle()),
            None,
            "raised, nothing more due"
        );

        let (mut m, t) = model();
        let (rx, i) = handed(&mut m, t, Slot::OpencodeTui, "m");
        assert_eq!(m.not_started_due_at(idle()), None);
        let mut now = t;
        let mut seq = 1;
        while now < t + 2 * window {
            now += STEP;
            step(&mut m, now, rx, &mut seq, &[("m", i.a, i.e)], idle());
            assert!(!m.delivery("m").unwrap().not_started(), "OpenCode never");
        }
    }

    /// Raise the not-started attention on a Pi message; returns the model,
    /// its receiver, the attempt, the instant and the revision shown.
    fn raised(limits: Limits) -> (AttemptModel, Rx, Issued, Instant, u64) {
        let (mut m, t) = model_with(limits);
        let (rx, i) = handed(&mut m, t, Slot::Pi, "m");
        let now = t + limits.not_started_after;
        m.snapshot(now, rx, 2, 0, &[("m", i.a, i.e)]);
        m.tick_turn(now, &[], idle());
        let d = m.delivery("m").unwrap();
        assert!(d.not_started());
        let revision = d.revision;
        (m, rx, i, now, revision)
    }

    /// Scenario: The user presses Retry or Cancel on a deck message the
    /// card says may not have reached a Pi agent. Retry at the shown revision
    /// schedules one marked repeat and starts a new window, an older revision
    /// is stale and an exhausted budget is refused; Cancel answers by what
    /// the receiver says it did, and a Retry pressed after a turn started
    /// withdrew the row is refused with nothing issued.
    #[spec("inbox/attempts/021")]
    #[test]
    fn attempts_021_retry_and_cancel_on_a_message_that_may_not_have_reached_the_agent() {
        let limits = Limits::default();
        let window = limits.not_started_after;
        // Retry at the shown revision: a marked repeat, a new window.
        let (mut m, rx, i, now, r) = raised(limits);
        assert!(matches!(
            m.retry(now, "m", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        let d = m.delivery("m").unwrap();
        assert!(!d.not_started(), "Retry clears the row");
        assert!(d.repeat_scheduled());
        let repeat = m.issue(now, "m", rx).expect("the manual repeat");
        assert_eq!(repeat.kind, IssueKind::ManualRetry);
        assert!(repeat.kind.marked());
        // The repeat's window starts at its first possession (attempts/025).
        let held = [("m", i.a, i.e), ("m", repeat.a, repeat.e)];
        let mut seq = 2;
        let first = now + STEP;
        let mut at = now;
        while at < first + window - STEP {
            at += STEP;
            step(&mut m, at, rx, &mut seq, &held, idle());
            assert!(
                !m.delivery("m").unwrap().not_started(),
                "the repeat gets its own window, at {:?}",
                at - now
            );
        }
        step(&mut m, first + window, rx, &mut seq, &held, idle());
        assert!(m.delivery("m").unwrap().not_started());

        // An older revision is stale.
        let (mut m, _, _, now, r) = raised(limits);
        assert!(matches!(
            m.retry(now, "m", GEN, r - 1),
            ActionOutcome::Stale { .. }
        ));
        // Past the budget, Retry is refused.
        let (mut m, _, _, now, r) = raised(Limits {
            max_attempts: 1,
            ..limits
        });
        assert_eq!(m.retry(now, "m", GEN, r), ActionOutcome::RetryLimit);

        // Cancel of a handed attempt: the receiver says it already has it.
        let (mut m, rx, i, now, r) = raised(limits);
        assert!(matches!(
            m.cancel(now, "m", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        assert_eq!(revokes(&m.drain_effects(), "m"), vec![(i.a, i.e)]);
        m.stage(now, "m", i.a, i.e, Stage::HandedInert, Some(rx));
        let d = m.delivery("m").unwrap();
        assert_eq!(
            d.state,
            MessageState::Undeliverable(UndeliverableReason::Cancelled)
        );
        assert!(d.attempt(i.a, i.e).unwrap().verifier);
        // Cancel of an attempt only possessed: the receiver revokes it.
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("m", false);
        let i = issue_held(&mut m, t, "m", rx, 1);
        let now = t + window;
        m.snapshot(now, rx, 2, 0, &[("m", i.a, i.e)]);
        m.tick_turn(now, &[], idle());
        let r = m.delivery("m").unwrap().revision;
        assert!(m.delivery("m").unwrap().not_started());
        m.cancel(now, "m", GEN, r);
        m.stage(now, "m", i.a, i.e, Stage::NotHanded, Some(rx));
        assert_eq!(
            m.state("m"),
            Some(MessageState::Undeliverable(UndeliverableReason::Cancelled))
        );

        // A turn started after the picker showed the row: Retry at the shown
        // revision is refused and nothing is issued.
        let (mut m, rx, _, now, r) = raised(limits);
        m.note_turn(
            now,
            TurnView {
                busy: true,
                idle_since: None,
            },
        );
        let d = m.delivery("m").unwrap();
        assert!(!d.not_started());
        assert_eq!(d.revision, r, "the withdrawal moves no revision");
        assert!(matches!(
            m.retry(now, "m", GEN, r),
            ActionOutcome::NotApplicable { .. }
        ));
        assert!(!m.delivery("m").unwrap().repeat_scheduled());
        assert_eq!(m.issue(now, "m", rx), Err(IssueRefusal::NothingToIssue));
        assert_eq!(m.delivery("m").unwrap().issued, 1);
    }

    /// Scenario: The not-started attention is raised on a Pi message, then a
    /// turn starts, the agent stays busy past twice the window, and the turn
    /// ends. The row goes when the turn starts and comes back only one window
    /// after the turn ended; a commit, a Cancel before the window or a
    /// message with no Pi attempt never shows it.
    #[spec("inbox/attempts/023")]
    #[test]
    fn attempts_023_only_an_attempting_uncommitted_pi_message_with_no_turn_shows_the_row() {
        let limits = Limits::default();
        let window = limits.not_started_after;
        let busy = TurnView {
            busy: true,
            idle_since: None,
        };
        // A turn starting withdraws it; only a turn that ended re-arms it.
        let (mut m, rx, i, now, _) = raised(limits);
        let held = [("m", i.a, i.e)];
        let mut seq = 2;
        let mut at = now;
        while at < now + 2 * window {
            at += STEP;
            step(&mut m, at, rx, &mut seq, &held, busy);
            assert!(!m.delivery("m").unwrap().not_started(), "busy");
        }
        let ended = TurnView {
            busy: false,
            idle_since: Some(at),
        };
        let end = at;
        while at < end + window - STEP {
            at += STEP;
            step(&mut m, at, rx, &mut seq, &held, ended);
            assert!(
                !m.delivery("m").unwrap().not_started(),
                "the window restarts at the turn's end, at {:?}",
                at - end
            );
        }
        step(&mut m, end + window, rx, &mut seq, &held, ended);
        assert!(m.delivery("m").unwrap().not_started());

        // A commit clears it for good.
        let (mut m, _, i, now, _) = raised(limits);
        m.stage(now, "m", i.a, i.e, Stage::Committed, None);
        m.tick_turn(now + window, &[], idle());
        assert!(!m.delivery("m").unwrap().not_started());
        assert_eq!(m.not_started_due_at(idle()), None);

        // Cancelled before the window: held for its revoke answer, then
        // HandedInert. Neither raises it.
        let (mut m, t) = model();
        let (rx, i) = handed(&mut m, t, Slot::Pi, "m");
        let r = m.delivery("m").unwrap().revision;
        m.cancel(t, "m", GEN, r);
        let mut seq = 1;
        let mut at = t;
        while at < t + 2 * window {
            at += STEP;
            step(&mut m, at, rx, &mut seq, &[("m", i.a, i.e)], idle());
            assert!(!m.delivery("m").unwrap().not_started(), "cancel-held");
        }
        assert_eq!(m.state("m"), Some(MessageState::CancelHeld));
        m.stage(at, "m", i.a, i.e, Stage::HandedInert, Some(rx));
        m.tick_turn(at + window, &[], idle());
        assert!(!m.delivery("m").unwrap().not_started(), "handed inert");
        assert_eq!(m.not_started_due_at(idle()), None);
    }

    /// Scenario: A deck message is issued to a Pi receiver that reports
    /// holding it only a minute later, and the agent stays idle throughout.
    /// The not-started window runs from that report, not from the issue, so
    /// the row appears four minutes after the issue rather than three; with a
    /// window shorter than the issue grace, an issued message nobody reported
    /// holding never shows it.
    #[spec("inbox/attempts/024")]
    #[test]
    fn attempts_024_the_window_starts_at_the_first_reported_possession_not_the_issue() {
        let limits = Limits::default();
        let window = limits.not_started_after;
        let (mut m, t) = model();
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("m", false);
        let i = m.issue(t, "m", rx).expect("issue");
        let taken = t + Duration::from_secs(60);
        for at in [t + Duration::from_secs(30), taken - Duration::from_secs(1)] {
            m.tick_turn(at, &[], idle());
            assert_eq!(
                m.not_started_due_at(idle()),
                None,
                "issue grace is not possession, at {:?}",
                at - t
            );
        }
        m.snapshot(taken, rx, 1, i.d, &[("m", i.a, i.e)]);
        assert_eq!(m.not_started_due_at(idle()), Some(taken + window));
        let mut seq = 1;
        let mut at = taken;
        while at < taken + window - STEP {
            at += STEP;
            step(&mut m, at, rx, &mut seq, &[("m", i.a, i.e)], idle());
            assert!(
                !m.delivery("m").unwrap().not_started(),
                "at {:?} after the issue",
                at - t
            );
        }
        step(
            &mut m,
            taken + window,
            rx,
            &mut seq,
            &[("m", i.a, i.e)],
            idle(),
        );
        assert!(m.delivery("m").unwrap().not_started());

        // A window shorter than the issue grace: the issue alone arms nothing.
        let short = Limits {
            not_started_after: Duration::from_secs(30),
            ..limits
        };
        let (mut m, t) = model_with(short);
        let rx = m.register(t, Slot::Pi, false).rx;
        m.post("m", false);
        m.issue(t, "m", rx).expect("issue");
        m.tick_turn(t + Duration::from_secs(60), &[], idle());
        let d = m.delivery("m").unwrap();
        assert_eq!(d.state, MessageState::Attempting, "still in issue grace");
        assert!(!d.not_started());
        assert_eq!(m.not_started_due_at(idle()), None);
    }

    /// Scenario: The user presses Retry on a Pi message the card says may
    /// not have reached the agent, while the receiver still reports holding
    /// the earlier copy. The repeat is issued a minute later and first
    /// reported held thirty seconds after that, and the new window runs from
    /// that report: the earlier copy's possession does not start it at the
    /// press.
    #[spec("inbox/attempts/025")]
    #[test]
    fn attempts_025_after_retry_the_window_starts_at_the_repeats_own_possession() {
        let limits = Limits::default();
        let window = limits.not_started_after;
        let (mut m, rx, i, now, r) = raised(limits);
        assert!(matches!(
            m.retry(now, "m", GEN, r),
            ActionOutcome::Applied { .. }
        ));
        let old = [("m", i.a, i.e)];
        let mut seq = 2;
        step(&mut m, now, rx, &mut seq, &old, idle());
        assert_eq!(
            m.not_started_due_at(idle()),
            None,
            "the earlier copy's possession does not anchor the repeat"
        );
        let issued_at = now + Duration::from_secs(60);
        step(&mut m, issued_at, rx, &mut seq, &old, idle());
        let repeat = m.issue(issued_at, "m", rx).expect("the manual repeat");
        assert_eq!(repeat.kind, IssueKind::ManualRetry);
        m.tick_turn(issued_at, &[], idle());
        assert_eq!(m.not_started_due_at(idle()), None, "issued, not yet held");
        let taken = issued_at + STEP;
        let both = [("m", i.a, i.e), ("m", repeat.a, repeat.e)];
        seq += 1;
        m.snapshot(taken, rx, seq, repeat.d, &both);
        assert_eq!(m.not_started_due_at(idle()), Some(taken + window));
        let mut at = taken;
        while at < taken + window - STEP {
            at += STEP;
            step(&mut m, at, rx, &mut seq, &both, idle());
            assert!(
                !m.delivery("m").unwrap().not_started(),
                "at {:?} after the press",
                at - now
            );
        }
        step(&mut m, taken + window, rx, &mut seq, &both, idle());
        assert!(m.delivery("m").unwrap().not_started());
    }

    /// Scenario: A Pi message's copy drops out of the receiver's possession
    /// reports and, with no spool check able to clear it, the message becomes
    /// uncertain; minutes later the receiver reports holding that same copy
    /// again. The message is attempting again and its not-started window
    /// starts at that report, so the row does not appear at once from the
    /// first hand-over's anchor.
    #[spec("inbox/attempts/026")]
    #[test]
    fn attempts_026_possession_regained_after_uncertain_starts_a_new_window() {
        let limits = Limits::default();
        let window = limits.not_started_after;
        let (mut m, t) = model();
        let (rx, i) = handed(&mut m, t, Slot::Pi, "m");
        assert_eq!(m.not_started_due_at(idle()), Some(t + window));
        let gone = t + STEP;
        m.snapshot(gone, rx, 2, i.d, &[]);
        let lapsed = gone + limits.uncertain_after() + window;
        m.tick_turn(lapsed, &[], idle());
        let d = m.delivery("m").unwrap();
        assert_eq!(d.state, MessageState::Uncertain(UncertainReason::Lapsed));
        assert!(!d.repeat_scheduled(), "no check cleared it, so no repeat");
        let back = lapsed + STEP;
        m.snapshot(back, rx, 3, i.d, &[("m", i.a, i.e)]);
        m.tick_turn(back, &[], idle());
        let d = m.delivery("m").unwrap();
        assert_eq!(d.state, MessageState::Attempting, "possession regained");
        assert!(!d.not_started(), "the old anchor is gone");
        assert_eq!(m.not_started_due_at(idle()), Some(back + window));
        let mut seq = 3;
        step(
            &mut m,
            back + window,
            rx,
            &mut seq,
            &[("m", i.a, i.e)],
            idle(),
        );
        assert!(m.delivery("m").unwrap().not_started());
    }
}
